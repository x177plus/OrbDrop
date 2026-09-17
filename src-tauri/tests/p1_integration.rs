//! P1 集成测试：双实例 mDNS 无关（走 HTTP/WS 127.0.0.1）配对 + 推送 + 落盘。
//!
//! 覆盖校验清单：
//! - 配对：A 发起（本机生成 6 位码并显示）→ B 输入该码 → B 向 A 换取 token（F-12）
//! - 配对只到"已配对"：配对本身**不**建长连接，拨号由用户点「连接」发起（F-16）
//! - 推送：邀请 → 同意 → 流式下载 → 文件落盘下载目录（F-50~F-52；收件目录已并入下载目录）
//! - 推送进度事件（F-54 进度数据）
//!
//! 注：凭据落在数据目录的 `credentials.dat`（P1 时曾是系统凭据库）。测试用临时 device id，
//! 结束清理，不污染真实配对；**读路径不做迁移**（迁移只在应用启动时显式发生），
//! 所以跑测试不会把本机真实的配对搬走或删掉。需要完全隔离的用例（凭据文件本身的行为）
//! 走 `ORBDROP_DATA_DIR` + 一次性 keyring 服务名，见 `tests/credentials_vault.rs`。

use std::sync::Arc;
use std::time::Duration;

use orbdrop_lib::credential;
use orbdrop_lib::state::AppState;
use orbdrop_lib::transfer;
use orbdrop_lib::ws::{LinkState, StopReason};
use orbdrop_lib::config::{Config, PeerDevice, ShareItem};

/// 给测试一个"便携版"假 BeyondCompare：预写 profile 只会写进这个临时目录，
/// 不会碰用户真实的 BC 配置（`AppState::new` 之后的配对流程会触发预写）。
fn hermetic_bc_exe() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("orbdrop_it_bc_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("BCompare.exe"), b"stub").unwrap();
    std::fs::write(dir.join("BCState.xml"), b"<BCState/>").unwrap();
    dir.join("BCompare.exe")
}

fn make_state(port: u16, device_id: &str, device_name: &str, save_dir: std::path::PathBuf) -> Arc<AppState> {
    let mut config = Config::default();
    config.port = port;
    config.device_id = device_id.to_string();
    config.device_name = device_name.to_string();
    // 落盘根（手动下载 + 推送接收共用同一个目录）。
    config.download_dir = save_dir.clone();
    config.bc_exe_path = Some(hermetic_bc_exe());
    let config_path = std::env::temp_dir().join(format!("orbdrop_it_config_{}.toml", uuid::Uuid::new_v4()));
    Arc::new(AppState::new(config, config_path))
}

fn temp_share_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("orbdrop_it_{}_{}", name, uuid::Uuid::new_v4()));
    std::fs::create_dir_all(dir.join("sub")).unwrap();
    std::fs::write(dir.join("hello.txt"), b"hello world from A").unwrap();
    std::fs::write(dir.join("sub").join("nested.txt"), b"nested content").unwrap();
    dir
}

/// 等着某个条件成立（轮询间隔 100ms，10s 超时）。
///
/// 只能 async：`#[tokio::test]` 跑在单线程运行时上，`std::thread::sleep` 会把
/// 唯一的 worker 线程焊死，被测的 WS/拨号任务根本没机会推进。
async fn wait_until<F: Fn() -> bool>(what: &str, ok: F) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        if ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    panic!("timeout waiting for: {what}");
}

/// 一个"当下没人监听"的端口：让内核分配后立刻放掉。比硬编码端口安全 ——
/// 别的测试的服务器在端口被占时会顺延（+1..+10），可能正好落到硬编码的那个号上，
/// 于是"死地址"变成了别人家的服务器。
fn free_port() -> u16 {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind ephemeral");
    listener.local_addr().expect("local_addr").port()
}

/// 两个互不相同的空闲端口（每次分配都会立刻放掉）。
fn two_free_ports() -> (u16, u16) {
    let a = free_port();
    let mut b = free_port();
    while b == a {
        b = free_port();
    }
    (a, b)
}

/// 该实例的 BC profile 文件：`make_state` 用的是便携版假 BC（exe 同目录有 `BCState.xml`），
/// 所以 profile 就写在那个临时目录里，不会碰用户真实的 BC 配置。
fn bc_profile_path(state: &Arc<AppState>) -> std::path::PathBuf {
    let exe = state.config.lock().bc_exe_path.clone().expect("bc_exe_path 已配置");
    exe.parent().expect("exe 有父目录").join("BCProfiles.xml")
}

/// 从磁盘读回配置 —— "重启"的入口：重启后能恢复什么，取决于文件里到底写了什么
/// （内存里的状态一律不算）。
fn read_config(path: &std::path::Path) -> Config {
    let content = std::fs::read_to_string(path).expect("config file should exist");
    toml::from_str(&content).expect("config file should parse")
}

#[tokio::test]
async fn pairing_then_push_transfers_file_to_receiver() {
    let root = temp_share_dir("A");
    let save_dir = std::env::temp_dir().join(format!("orbdrop_it_save_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&save_dir).unwrap();

    // Two instances (same process): A and B.
    let state_a = make_state(3990, "device-a", "PC-A", save_dir.clone());
    let state_b = make_state(3991, "device-b", "PC-B", save_dir.clone());

    // Give A a share.
    let share = ShareItem::new(root.clone());
    let share_id = share.id.clone();
    state_a.config.lock().shares.push(share);

    // Start both servers.
    let port_a = orbdrop_lib::server::start_server(state_a.clone(), 3990).await.expect("A server");
    let port_b = orbdrop_lib::server::start_server(state_b.clone(), 3991).await.expect("B server");
    assert_eq!(port_a, 3990);
    assert_eq!(port_b, 3991);

    let client = reqwest::Client::new();

    // ---- Pairing (F-12) ----
    // 1. A initiates: it issues the 6-digit code on itself and displays it.
    //    (It knows B's address from its device list — manual add / mDNS.)
    state_a.add_peer_book(orbdrop_lib::config::PeerDevice {
        id: "device-b".into(),
        name: "PC-B".into(),
        ip: "127.0.0.1".into(),
        port: port_b,
        online: true,
        paired: false,
        auto_connect: false,
    });
    let code = state_a.pair.create_code();
    assert_eq!(code.len(), 6, "6-digit pairing code");

    // 2. B's user reads that code and enters it in B's OrbDrop → B posts it to A.
    let resp = client
        .post(format!("http://127.0.0.1:{port_a}/pair"))
        .json(&serde_json::json!({
            "peerId": "device-b",
            "peerName": "PC-B",
            "code": code,
        }))
        .send().await.unwrap();
    assert_eq!(resp.status(), 200, "valid code accepted");
    let conf: serde_json::Value = resp.json().await.unwrap();
    let token = conf["token"].as_str().expect("token").to_string();
    assert!(!token.is_empty(), "32-byte hex token");
    // A answers with its own identity, for B to store against.
    assert_eq!(conf["peerId"], "device-a");
    assert_eq!(conf["peerName"], "PC-A");

    // A stored the token under B's id (server side) and marked B paired,
    // keeping the address it already had for B.
    assert!(credential::has_peer("device-b"), "A stored token for B");
    let peer_b = state_a.get_peer("device-b").expect("B stays in A's list");
    assert!(peer_b.paired, "A marks B paired");
    assert_eq!((peer_b.ip.as_str(), peer_b.port), ("127.0.0.1", port_b));

    // A wrong code is refused, and a code is consumed on first success.
    let resp = client
        .post(format!("http://127.0.0.1:{port_a}/pair"))
        .json(&serde_json::json!({"peerId": "device-b", "peerName": "PC-B", "code": "000000"}))
        .send().await.unwrap();
    assert_eq!(resp.status(), 401, "wrong code refused");
    let resp = client
        .post(format!("http://127.0.0.1:{port_a}/pair"))
        .json(&serde_json::json!({"peerId": "device-b", "peerName": "PC-B", "code": code}))
        .send().await.unwrap();
    assert_eq!(resp.status(), 401, "replayed code refused");

    // 3. B stores the token under A's id (client side, mirrors pair_confirm).
    credential::set_peer("device-a", &token).unwrap();
    assert!(credential::has_peer("device-a"));

    // B registers A so its WS link + download can resolve A's address.
    state_b.add_peer_book(orbdrop_lib::config::PeerDevice {
        id: "device-a".into(),
        name: "PC-A".into(),
        ip: "127.0.0.1".into(),
        port: port_a,
        online: true,
        paired: true,
        auto_connect: false,
    });

    // 4. Pairing stops at "已配对" — it never dials. A's user hits 「连接」 here
    // (mirrors `connect_peer`) to open the WS link the push rides on.
    orbdrop_lib::ws::ensure_link_now(&state_a, "device-b");
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if state_a.ws.has_connection("device-b") && state_b.ws.has_connection("device-a") {
            break;
        }
        if std::time::Instant::now() > deadline {
            panic!("WS link did not establish in time");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // ---- Push (F-50) ----
    // A pushes the share's root directory to B.
    let invite_id = transfer::invite_push(state_a.clone(), &share_id, "", "device-b").await
        .expect("invite sent over WS");

    // Wait for B to receive the invite.
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        if state_b.push.inbound.lock().contains_key(&invite_id) {
            break;
        }
        if std::time::Instant::now() > deadline {
            panic!("receiver did not get push invite");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    // B accepts → download loop pulls from A and writes into B's save dir (下载目录).
    transfer::accept_invite(state_b.clone(), &invite_id).expect("accept");

    // Wait for completion: file appears under the receiver's save dir, in the
    // <发送方名>/<推送名>/ 子目录.
    // The invite name is the share item name — ShareItem::new uses the dir name.
    let share_name = root.file_name().unwrap().to_string_lossy().to_string();
    let expected = save_dir.join("PC-A").join(&share_name).join("hello.txt");

    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    let mut done = false;
    while std::time::Instant::now() < deadline {
        if expected.exists() {
            let content = std::fs::read(&expected).unwrap();
            if content == b"hello world from A" {
                done = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(done, "file should be transferred into receiver save dir: {:?}", expected);

    // Nested file preserved.
    let nested = save_dir.join("PC-A").join(&share_name).join("sub").join("nested.txt");
    assert_eq!(std::fs::read(&nested).unwrap(), b"nested content");

    // Sender-side job should be removed after completion.
    assert!(state_a.push.jobs.lock().get(&invite_id).is_none(), "job cleaned up");

    // ---- Cleanup ----
    orbdrop_lib::server::stop_server(&state_a).await;
    orbdrop_lib::server::stop_server(&state_b).await;
    credential::remove_peer("device-a").ok();
    credential::remove_peer("device-b").ok();
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&save_dir);
}

/// 端口被占时服务会顺延，`/pair/info` 必须报**实际**监听端口 —— 对端会把这里
/// 报的端口存下来当拨号地址，报错就等于让它去连别的进程（或连不上）。
#[tokio::test]
async fn pair_info_reports_the_port_actually_serving() {
    let squatter = std::net::TcpListener::bind(("0.0.0.0", 3995)).unwrap();
    let state = make_state(3995, "device-pa", "PC-PA", std::env::temp_dir());
    let port = orbdrop_lib::server::start_server(state.clone(), 3995).await.expect("server");
    assert_eq!(port, 3996, "3995 被占 → 顺延");

    let info: serde_json::Value = reqwest::get(format!("http://127.0.0.1:{port}/pair/info"))
        .await.unwrap().json().await.unwrap();
    assert_eq!(info["port"].as_u64(), Some(u64::from(port)), "pair/info 必须报实际端口");
    assert_eq!(info["id"], "device-pa");

    drop(squatter);
    orbdrop_lib::server::stop_server(&state).await;
}

/// 启动/任意时刻的 panic 必须留痕：`record_crash` 同步追加，不经 tracing 的异步刷盘。
#[test]
fn crash_recorder_appends_synchronously() {
    let dir = std::env::temp_dir().join(format!("orbdrop_crash_{}", uuid::Uuid::new_v4()));
    let path = dir.join("nested").join("crash.log");
    orbdrop_lib::log::record_crash(&path, "boom-1");
    orbdrop_lib::log::record_crash(&path, "boom-2");
    let text = std::fs::read_to_string(&path).unwrap();
    assert_eq!(text.lines().count(), 2, "每次崩溃一行、追加而不是覆盖");
    assert!(text.contains("boom-1") && text.contains("boom-2"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// 解除配对（F-14）必须是**双向**的，而且不能把设备行抹掉：对方还在网络上看得见，
/// 就该继续以"未绑定"显示（离线才消失）。
///
/// 特意先把长连接断掉再解除：旧的实现靠 WS 事件通知对端，事件是排队发送的，紧跟着
/// 的 drop_peer 会把它一起丢掉，链路不通时更是必然丢 —— 对端就一直停在"已配对"。
#[tokio::test]
async fn unpair_clears_both_sides_and_keeps_the_device_listed() {
    credential::remove_peer("device-ua").ok();
    credential::remove_peer("device-ub").ok();

    let dir = std::env::temp_dir().join(format!("orbdrop_unpair_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let state_a = make_state(3998, "device-ua", "PC-UA", dir.clone());
    let state_b = make_state(3999, "device-ub", "PC-UB", dir.clone());
    let port_a = orbdrop_lib::server::start_server(state_a.clone(), 3998).await.expect("A server");
    let port_b = orbdrop_lib::server::start_server(state_b.clone(), 3999).await.expect("B server");

    // 配对：B 用 A 显示的码向 A 换取 token（A 从请求里拿到 B 的地址）。
    let code = state_a.pair.create_code();
    let resp = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port_a}/pair"))
        .json(&serde_json::json!({
            "peerId": "device-ub",
            "peerName": "PC-UB",
            "peerPort": port_b,
            "code": code,
        }))
        .send().await.unwrap();
    assert_eq!(resp.status(), 200);
    let token = resp.json::<serde_json::Value>().await.unwrap()["token"].as_str().unwrap().to_string();
    credential::set_peer("device-ua", &token).unwrap();
    state_b.mark_paired(&orbdrop_lib::config::PeerDevice {
        id: "device-ua".into(),
        name: "PC-UA".into(),
        ip: "127.0.0.1".into(),
        port: port_a,
        online: true,
        paired: true,
        auto_connect: false,
    });

    // 配对不再自己建链：A 点「连接」（镜像 connect_peer）才有链路。
    orbdrop_lib::ws::ensure_link_now(&state_a, "device-ub");
    wait_until("点连接后建链", || {
        state_a.ws.has_connection("device-ub") && state_b.ws.has_connection("device-ua")
    }).await;

    // 故意先把长连接断掉：解除配对不允许依赖 WS 通道。
    state_a.ws.pause("device-ub");
    wait_until("断开生效", || !state_b.ws.has_connection("device-ua")).await;

    // 配对时 A 就把 BC profile 预写好了（BC 只在启动那一刻登记这个文件，必须提前落盘），
    // 账号用的是应用名 —— 记录里认得出这条连接是本应用发起的。
    let profile_file = bc_profile_path(&state_a);
    let written = std::fs::read_to_string(&profile_file).expect("配对后应预写好 profile");
    assert!(
        written.contains(&format!("TWebDAVProfile Value=\"127.0.0.1:{port_b}\"")),
        "配对后该有指向 B 的 profile:\n{written}"
    );
    assert!(
        written.contains("Username Value=\"OrbDrop\""),
        "profile 账号是应用名:\n{written}"
    );
    // 用户自己的 profile（用户名不是我们的，名字也不像 `IP:端口`）必须一个字都不动。
    let foreign = "\t<TFtpProfile Value=\"MyFtp\">\n\t\t<Host Value=\"example.com\"/>\n\t</TFtpProfile>";
    let with_foreign = written.replace("</BCProfiles>", &format!("{foreign}\n</BCProfiles>"));
    std::fs::write(&profile_file, &with_foreign).unwrap();

    orbdrop_lib::unpair_peer(&state_a, "device-ub").await.expect("unpair");

    // 关系断了 → 指向它的 profile 一起清掉：token 已作废，留着只会让 BC 拿死凭据去撞；
    // 而用户自己那条必须还在（清理只认"我们写下的"条目）。
    let after = std::fs::read_to_string(&profile_file).unwrap();
    assert!(
        !after.contains(&format!("TWebDAVProfile Value=\"127.0.0.1:{port_b}\"")),
        "解除配对后该清掉那条 profile:\n{after}"
    );
    assert!(after.contains(foreign.trim()), "用户手建的 profile 必须原样保留:\n{after}");

    // A 侧：token 清掉、链路没了，但设备行还在、只是回到"未绑定"。
    assert!(!credential::has_peer("device-ub"), "A 的 token 必须清掉");
    assert!(!state_a.ws.has_connection("device-ub"));
    let on_a = state_a.get_peer("device-ub").expect("A 仍列出该设备（未绑定）");
    assert!(!on_a.paired, "A 侧回到未绑定");

    // B 侧：被通知到，同样清 token、保留设备行。
    assert!(!credential::has_peer("device-ua"), "B 的 token 必须被清掉（解除配对是双向的）");
    let on_b = state_b.get_peer("device-ua").expect("B 仍列出该设备（未绑定）");
    assert!(!on_b.paired, "B 侧回到未绑定");

    // 被解除配对后，对方（尤其**旧版本**对端，它不知道要清 token）拿着旧 token 再来拨号时：
    // 必须被拒，而且**不能**被登记成本机的 "self" —— 否则对端显示"长连接"、本机却把它
    // 当"未绑定"，正是用户遇到的现象。
    credential::set_peer("device-ua", "stale-token-after-unpair").unwrap();
    orbdrop_lib::ws::ensure_link_now(&state_b, "device-ua");
    tokio::time::sleep(Duration::from_millis(800)).await;
    let on_a = state_a.get_peer("device-ub").expect("A 仍列出该设备");
    assert!(!on_a.paired, "被解除配对后不能因为对端强行拨号就变回已配对");
    assert!(
        !state_a.ws.has_connection("device-ub") && !state_a.ws.has_connection("self"),
        "过期 token 的拨号必须被拒（不得以 self 身份混进本机）"
    );
    assert!(!state_b.ws.has_connection("device-ua"), "对端也不该连上");

    // 旧版本对端还握着旧 token 时，连共享项列表也拿不到（Bearer 反解不出身份 → 401）。
    // 来源用本机 LAN 地址，绕开"回环 = 本机 UI"的免鉴权分支。
    if let Some(lan_ip) = local_lan_ip() {
        let resp = reqwest::Client::new()
            .get(format!("http://{lan_ip}:{port_a}/shares"))
            .bearer_auth("stale-token-after-unpair")
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            reqwest::StatusCode::UNAUTHORIZED,
            "被解除配对后，旧 token 不得再读到本机共享项"
        );
    }

    orbdrop_lib::server::stop_server(&state_a).await;
    orbdrop_lib::server::stop_server(&state_b).await;
    credential::remove_peer("device-ua").ok();
    credential::remove_peer("device-ub").ok();
    let _ = std::fs::remove_dir_all(&dir);
}

/// 本机的局域网地址：用来模拟"来自另一台设备"的请求（回环会被当成本机 UI 免鉴权）。
fn local_lan_ip() -> Option<std::net::IpAddr> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:9").ok()?; // 只为挑出出口网卡，不实际发包
    let ip = socket.local_addr().ok()?.ip();
    if ip.is_loopback() { None } else { Some(ip) }
}

/// 同步上下文（没有 Tokio 运行时）里点「连接」/「同意接收推送」不能把进程打崩。
///
/// Tauri 的**同步**命令就跑在这种上下文里：修复前 `tokio::spawn` 直接 panic
/// （用户侧表现为设备 A 点「连接」时 App 闪退；"同意"接收推送是同一类崩溃）。
/// 这条测试跑在普通 `#[test]` 里，正是那个没有 reactor 的场景。
#[test]
fn button_tasks_without_a_tokio_runtime_do_not_panic() {
    credential::remove_peer("device-zb").ok();

    let save_dir = std::env::temp_dir().join(format!("orbdrop_nort_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&save_dir).unwrap();
    // 点「连接」必然走到 spawn 那一步（拨号只由这个入口发起）。
    let state = make_state(3994, "device-za", "PC-ZA", save_dir.clone());
    state.mark_paired(&orbdrop_lib::config::PeerDevice {
        id: "device-zb".into(),
        name: "PC-ZB".into(),
        ip: "127.0.0.1".into(),
        port: 1,
        online: true,
        paired: true,
        auto_connect: false,
    });

    // 「连接」
    orbdrop_lib::ws::ensure_link_now(&state, "device-zb");
    assert!(
        !state.ws.has_connection("device-zb"),
        "没有运行时就不该（也不可能）建立会话"
    );

    // 「同意接收推送」→ spawn_download（同样是同步命令触发的后台任务）
    credential::set_peer("device-zb", "nort-token").unwrap();
    state.push.insert_invite(orbdrop_lib::transfer::push_job::InboundInvite {
        invite_id: "nort-invite".into(),
        sender: orbdrop_lib::ws::events::PeerInfo { id: "device-zb".into(), name: "PC-ZB".into() },
        name: "hello.txt".into(),
        size: 11,
        is_dir: false,
        files: vec![],
    });
    orbdrop_lib::transfer::accept_invite(state.clone(), "nort-invite").expect("accept");

    credential::remove_peer("device-zb").ok();
    let _ = std::fs::remove_dir_all(&save_dir);
}

/// F-16 回归：配对只到"已配对"——**配对本身不建长连接**，谁都不许自己拨号。
///
/// 用户报的现象正是"配对完自己就变成长连接了"：配对只该确立关系。这里让配对双方
/// 都有地址、都握着 token（地址还都是对方自报/来源 IP 给的，等于把自动拨号的所有
/// 条件都摆齐），再给 mDNS 发现 / 5s 探活 / 启动这三条旧的自动路径留出出手的时间窗，
/// 断言一条链路都不许冒出来；随后用户点「连接」，链路才建立。
///
/// 后半段是「连接 / 断开」两个按钮的既有语义（与自动建链无关，仍然必须成立）。
#[tokio::test]
async fn pairing_stops_at_paired_until_the_user_clicks_connect() {
    // 凭据库是机器级的，进程结束也不会清：先抹掉上一轮（尤其是跑挂的那轮）留下的
    // token，否则 A 一启动就从凭据库重建出 B 的条目，"从未见过 B" 这个前提不成立。
    credential::remove_peer("device-aa").ok();
    credential::remove_peer("device-ab").ok();

    let save_dir = std::env::temp_dir().join(format!("orbdrop_auto_save_dir_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&save_dir).unwrap();

    let state_a = make_state(3992, "device-aa", "PC-AA", save_dir.clone());
    let state_b = make_state(3993, "device-ab", "PC-AB", save_dir.clone());
    let port_a = orbdrop_lib::server::start_server(state_a.clone(), 3992).await.expect("A server");
    let port_b = orbdrop_lib::server::start_server(state_b.clone(), 3993).await.expect("B server");

    // A 显示自己的配对码；A 此前对 B 一无所知（无 mDNS、无手动添加、无地址）。
    let code = state_a.pair.create_code();
    assert!(
        state_a.get_peer("device-ab").map_or(true, |p| p.ip.is_empty() && p.port == 0),
        "A must have no address for B before pairing"
    );

    // B 见过 A（mDNS/手动添加），读到 A 的码后向 A 换取 token。
    state_b.add_peer_book(orbdrop_lib::config::PeerDevice {
        id: "device-aa".into(),
        name: "PC-AA".into(),
        ip: "127.0.0.1".into(),
        port: port_a,
        online: true,
        paired: false,
        auto_connect: false,
    });

    let resp = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port_a}/pair"))
        .json(&serde_json::json!({
            "peerId": "device-ab",
            "peerName": "PC-AB",
            "peerPort": port_b,
            "code": code,
        }))
        .send().await.unwrap();
    assert_eq!(resp.status(), 200, "valid code accepted");
    let conf: serde_json::Value = resp.json().await.unwrap();
    let token = conf["token"].as_str().expect("token").to_string();

    // B 存下 A 的 token 并标记已配对（镜像 pair_confirm）。
    credential::set_peer("device-aa", &token).unwrap();
    state_b.mark_paired(&orbdrop_lib::config::PeerDevice {
        id: "device-aa".into(),
        name: "PC-AA".into(),
        ip: "127.0.0.1".into(),
        port: port_a,
        online: true,
        paired: true,
        auto_connect: false,
    });

    // A 从配对请求里学到了 B 的地址（来源 IP + B 自报端口）并落库 —— 有地址也不自己拨。
    let learned = state_a.get_peer("device-ab").expect("A now knows B");
    assert!(learned.paired, "A marks B paired");
    assert_eq!((learned.ip.as_str(), learned.port), ("127.0.0.1", port_b));

    // 关键断言：双方都已配对、地址齐全、token 在手，但没人点「连接」→ 一条链路都不许有。
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(
        !state_a.ws.has_connection("device-ab"),
        "配对不该自己建立长连接（A 侧）"
    );
    assert!(
        !state_b.ws.has_connection("device-aa"),
        "配对不该自己建立长连接（B 侧）"
    );

    // 用户点「连接」（A 侧，镜像 connect_peer）→ 链路这才起来。
    orbdrop_lib::ws::ensure_link_now(&state_a, "device-ab");
    wait_until("点连接必须建立长连接", || {
        state_a.ws.has_connection("device-ab") && state_b.ws.has_connection("device-aa")
    }).await;

    // ---- 下面是「断开 / 连接」两个按钮的既有语义 ----

    // 1) 被拨号的那一台（B，A 拨的号）点"断开"：双方都必须掉线。
    //    修复前只有本机认为断了，对端一直显示"长连接"。
    state_b.ws.pause("device-aa");
    wait_until("断开 must drop the peer's session too", || {
        !state_a.ws.has_connection("device-ab") && !state_b.ws.has_connection("device-aa")
    }).await;
    assert!(state_b.ws.is_paused("device-aa"));

    // 2) 断开方自己点"连接"：显式点击必须能连回来（哪个方向点都算数）。
    state_b.ws.resume("device-aa");
    orbdrop_lib::ws::ensure_link_now(&state_b, "device-aa");
    wait_until("断开方点连接必须能重新建立长连接", || {
        state_a.ws.has_connection("device-ab") && state_b.ws.has_connection("device-aa")
    }).await;

    // 而且必须**稳住**：对端（A）的自动拨号循环此刻仍在跑，如果它无视"已经连上"
    // 再拨一条，两条链接会互相 register/unregister —— 界面就是"一会儿连一会儿断"。
    let mut flips = 0;
    let mut connected = true;
    let mut pattern = String::new();
    for _ in 0..60 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let now = state_a.ws.has_connection("device-ab") && state_b.ws.has_connection("device-aa");
        if now != connected {
            flips += 1;
            pattern.push(if now { '1' } else { '0' });
            connected = now;
        }
    }
    assert_eq!(flips, 0, "重连后链路抖动（连/断交替，pattern 尾段={pattern}）");

    // 实验：两端同时点「连接」（各拨一条），应仍只有一条会话且不抖。
    state_b.ws.pause("device-aa");
    wait_until("断开 must drop", || !state_a.ws.has_connection("device-ab")).await;
    state_b.ws.resume("device-aa");
    orbdrop_lib::ws::ensure_link_now(&state_a, "device-ab");
    orbdrop_lib::ws::ensure_link_now(&state_b, "device-aa");
    wait_until("同时点连接应能连上", || state_a.ws.has_connection("device-ab")).await;
    let mut flips2 = 0;
    let mut connected2 = state_a.ws.has_connection("device-ab") && state_b.ws.has_connection("device-aa");
    let mut pattern2 = String::new();
    for _ in 0..60 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let now = state_a.ws.has_connection("device-ab") && state_b.ws.has_connection("device-aa");
        if now != connected2 {
            flips2 += 1;
            pattern2.push(if now { '1' } else { '0' });
            connected2 = now;
        }
    }
    assert_eq!(flips2, 0, "两端同时连接后抖动（pattern 尾段={pattern2}）");

    // 3) 再断开一次：对端（A）的自动重连要一直被拒，否则"断开"一秒后就被撤销。
    state_b.ws.pause("device-aa");
    wait_until("断开 must drop", || !state_a.ws.has_connection("device-ab")).await;
    tokio::time::sleep(Duration::from_millis(1200)).await; // A 的首次重拨已发生
    assert!(
        !state_a.ws.has_connection("device-ab"),
        "对端自动重连必须被拒（本机保持断开）"
    );

    // 4) 对端点"连接"（manual=1）：放行，并结束本机的暂停。
    orbdrop_lib::ws::ensure_link_now(&state_a, "device-ab");
    wait_until("对端点连接必须能连上", || {
        state_a.ws.has_connection("device-ab") && state_b.ws.has_connection("device-aa")
    }).await;
    assert!(
        !state_b.ws.is_paused("device-aa"),
        "对端显式请求连接 → 本机暂停结束"
    );

    // 5) manual 只算一次：上面 A 是"手动"拨上来的，B 再断开后不能因为 A 的循环
    //    还在跑就替 B 撤销暂停（否则 B 的「断开」又被悄悄推翻）。
    state_b.ws.pause("device-aa");
    wait_until("断开 must drop", || !state_a.ws.has_connection("device-ab")).await;
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert!(
        !state_a.ws.has_connection("device-ab"),
        "手动拨号的循环不能把对端的「断开」一直撤销掉"
    );
    assert!(state_b.ws.is_paused("device-aa"));

    // ---- Cleanup ----
    orbdrop_lib::server::stop_server(&state_a).await;
    orbdrop_lib::server::stop_server(&state_b).await;
    credential::remove_peer("device-aa").ok();
    credential::remove_peer("device-ab").ok();
    let _ = std::fs::remove_dir_all(&save_dir);
}

/// F-16 回归：关机时处于长连接的设备，重启后必须自己把链路接回来。
///
/// 用户的痛点：关掉 App 再启动，设备全回到"已配对"，得一台一台重新点「连接」。
/// 这里的"重启"是从**磁盘**重新读回配置再造一个 `AppState`，然后跑启动恢复
/// （`restore_links_on_startup`）—— 内存里什么都没带过去。
///
/// 反向同样要成立：点过「断开」的设备，重启后不许自己连回来（那个意图也跨重启）。
#[tokio::test]
async fn long_connection_is_restored_after_a_restart() {
    // 凭据库是机器级的：先抹掉上一轮（尤其是跑挂的那轮）留下的 token。
    credential::remove_peer("device-ra").ok();
    credential::remove_peer("device-rb").ok();

    let save_dir = std::env::temp_dir().join(format!("orbdrop_restore_save_dir_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&save_dir).unwrap();

    let state_a = make_state(3995, "device-ra", "PC-RA", save_dir.clone());
    let state_b = make_state(3996, "device-rb", "PC-RB", save_dir.clone());
    let config_path = state_a.config_path.clone();
    let port_b = orbdrop_lib::server::start_server(state_b.clone(), 3996).await.expect("B server");

    // 配对结果：一张 token，两侧各按对端 id 存一份（`PairResponse` 就是这个流向）。
    // token 用短值：Windows 单条凭据只有 2.5KB，测试条目别把上限顶掉。
    let token = format!("t-r-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
    credential::set_peer("device-rb", &token).unwrap();
    credential::set_peer("device-ra", &token).unwrap();
    state_a.mark_paired(&orbdrop_lib::config::PeerDevice {
        id: "device-rb".into(),
        name: "PC-RB".into(),
        ip: "127.0.0.1".into(),
        port: port_b,
        online: true,
        paired: true,
        auto_connect: false,
    });
    state_b.mark_paired(&orbdrop_lib::config::PeerDevice {
        id: "device-ra".into(),
        name: "PC-RA".into(),
        ip: "127.0.0.1".into(),
        port: 3995,
        online: true,
        paired: true,
        auto_connect: false,
    });

    // 用户点「连接」（A 侧）→ 链路建立。
    orbdrop_lib::ws::ensure_link_now(&state_a, "device-rb");
    wait_until("点连接建立链路", || {
        state_a.ws.has_connection("device-rb") && state_b.ws.has_connection("device-ra")
    })
    .await;

    // 记忆写在**链路建立那一刻**（不是退出时），所以强杀进程/断电都不会丢。
    assert!(
        read_config(&config_path)
            .peers
            .iter()
            .find(|p| p.id == "device-rb")
            .expect("A 的地址簿里有 B")
            .auto_connect,
        "链路一建立就要把长连接意图写进地址簿"
    );
    assert!(
        read_config(&state_b.config_path)
            .peers
            .iter()
            .find(|p| p.id == "device-ra")
            .expect("B 的地址簿里有 A")
            .auto_connect,
        "被拨号的一端同样记下这次长连接（重启后这一侧也要接回来）"
    );

    // 模拟 A 退出：链路和拨号循环一起消失（`pause` 在测试里代表"进程不再持有链路"），
    // 磁盘上那句"上次是长连接"留着 —— 用户关掉 App 时就是这个状态。
    state_a.ws.pause("device-rb");
    wait_until("退出后链路消失", || !state_b.ws.has_connection("device-ra")).await;

    // ---- 重启 A：只从磁盘读配置，不点任何按钮 ----
    let state_a2 = Arc::new(AppState::new(read_config(&config_path), config_path.clone()));
    orbdrop_lib::ws::restore_links_on_startup(&state_a2);
    wait_until("重启后自动接回长连接", || {
        state_a2.ws.has_connection("device-rb") && state_b.ws.has_connection("device-ra")
    })
    .await;

    // ---- 反向：点过「断开」的设备，重启后不许自己连回来 ----
    state_a2.ws.pause("device-rb");
    state_a2.set_auto_connect("device-rb", false); // 镜像 disconnect_peer 的两步
    wait_until("断开 must drop", || !state_b.ws.has_connection("device-ra")).await;

    let state_a3 = Arc::new(AppState::new(read_config(&config_path), config_path.clone()));
    orbdrop_lib::ws::restore_links_on_startup(&state_a3);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(
        !state_a3.ws.has_connection("device-rb"),
        "「断开」过的设备重启后不该自动连回来"
    );
    assert!(
        !state_b.ws.has_connection("device-ra"),
        "B 侧也不该出现链路（本机没拨号，B 也没有拨号循环）"
    );

    // ---- Cleanup ----
    orbdrop_lib::server::stop_server(&state_b).await;
    credential::remove_peer("device-ra").ok();
    credential::remove_peer("device-rb").ok();
    let _ = std::fs::remove_dir_all(&save_dir);
    let _ = std::fs::remove_file(&config_path);
}

// ---------------------------------------------------------------------------
// 自动恢复遇到"对端还没上线"
// ---------------------------------------------------------------------------

/// 关机时是长连接、但下次启动时对端还没开机：恢复拨号会先失败（地址没人应答），
/// 等对端真的上线（地址被修正 + 标记在线 —— 真机上这一步是 mDNS / 5s 探活做的），
/// **不需要任何人点「连接」**，它就该自己接上。
///
/// 这也解释了为什么自动拨号只在前几次死磕：对端不在的那些分钟里反复发 TCP 只会
/// 每次都等满超时（10060）；等它出现在网上再拨才准。
#[tokio::test]
async fn auto_restore_connects_once_the_peer_shows_up() {
    // device id 带短后缀：机器级凭据库是所有测试共用的，固定 id 会被别的用例的收尾
    // `remove_peer` 顺手抹掉（表现为"没有 token"这种莫名其妙的失败）。别用整条 uuid ——
    // Windows 单条凭据只有 2.5KB，测试条目越长，并行跑时越容易顶到上限（见 requirements 风险表）。
    let tag = &uuid::Uuid::new_v4().simple().to_string()[..8];
    let id_a = format!("d-on-a-{tag}");
    let id_b = format!("d-on-b-{tag}");

    let save_dir = std::env::temp_dir().join(format!("orbdrop_online_save_dir_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&save_dir).unwrap();
    // 端口也让内核现挑（配置里的端口只在真的起服务时用，这里 A 不起服务）。
    let state_a = make_state(0, &id_a, "PC-SA", save_dir.clone());
    let state_b = make_state(0, &id_b, "PC-SB", save_dir.clone());
    let config_path = state_a.config_path.clone();

    let token = format!("t-on-{tag}");
    credential::set_peer(&id_b, &token).unwrap();
    credential::set_peer(&id_a, &token).unwrap();
    // A 记得这次长连接，但手上的地址是个没人监听的端口（对端没开机 / 地址过期）。
    let (dead_port, base_port) = two_free_ports();
    state_a.mark_paired(&orbdrop_lib::config::PeerDevice {
        id: id_b.clone(),
        name: "PC-SB".into(),
        ip: "127.0.0.1".into(),
        port: dead_port,
        online: false,
        paired: true,
        auto_connect: true,
    });
    state_b.mark_paired(&orbdrop_lib::config::PeerDevice {
        id: id_a.clone(),
        name: "PC-SA".into(),
        ip: "127.0.0.1".into(),
        port: 0,
        online: true,
        paired: true,
        auto_connect: false,
    });

    // 「重启」A：它照记忆自动拨，但对端不在 → 先连不上。
    let state_a2 = Arc::new(AppState::new(read_config(&config_path), config_path.clone()));
    orbdrop_lib::ws::restore_links_on_startup(&state_a2);
    tokio::time::sleep(Duration::from_millis(800)).await;
    assert!(
        !state_a2.ws.has_connection(&id_b),
        "对端还没上线，这一轮当然连不上"
    );

    // 对端上线：起它的服务，并把地址修正 + 标记在线（真机上由 mDNS/探活完成）。
    let port_b = orbdrop_lib::server::start_server(state_b.clone(), base_port)
        .await
        .expect("B server");
    state_a2.upsert_peer(&orbdrop_lib::config::PeerDevice {
        id: id_b.clone(),
        name: "PC-SB".into(),
        ip: "127.0.0.1".into(),
        port: port_b,
        online: true,
        paired: true,
        auto_connect: false,
    });

    wait_until("对端上线后自动接回（无需点「连接」）", || {
        state_a2.ws.has_connection(&id_b) && state_b.ws.has_connection(&id_a)
    })
    .await;

    // ---- Cleanup ----
    orbdrop_lib::server::stop_server(&state_b).await;
    credential::remove_peer(&id_a).ok();
    credential::remove_peer(&id_b).ok();
    let _ = std::fs::remove_dir_all(&save_dir);
    let _ = std::fs::remove_file(&config_path);
}

// ---------------------------------------------------------------------------
// 明确拒绝 = 停手（403 / 401），以及 403 之后的恢复
// ---------------------------------------------------------------------------

/// 对端点过「断开」= **明确拒绝**：本机的自动重拨必须停手（不再每分钟去敲一次 ——
/// 那永远敲不出连接），并把理由留给界面。
///
/// 恢复有两条路：对端点「连接」拨过来，或本机点「连接」（`manual=1` 会替它结束暂停）。
#[tokio::test]
async fn a_peer_that_said_disconnect_stops_the_retry_loop() {
    let tag = &uuid::Uuid::new_v4().simple().to_string()[..8];
    let id_a = format!("d-st-a-{tag}");
    let id_b = format!("d-st-b-{tag}");
    credential::remove_peer(&id_a).ok();
    credential::remove_peer(&id_b).ok();

    let save_dir = std::env::temp_dir().join(format!("orbdrop_stop_save_dir_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&save_dir).unwrap();
    let state_a = make_state(0, &id_a, "PC-SA", save_dir.clone());
    let state_b = make_state(0, &id_b, "PC-SB", save_dir.clone());

    // 一张 token，两侧各按对端 id 存一份（`PairResponse` 的流向）。
    let token = format!("t-st-{tag}");
    credential::set_peer(&id_b, &token).unwrap();
    credential::set_peer(&id_a, &token).unwrap();
    let (base_port, _spare) = two_free_ports();
    let port_b = orbdrop_lib::server::start_server(state_b.clone(), base_port)
        .await
        .expect("B server");
    state_a.mark_paired(&orbdrop_lib::config::PeerDevice {
        id: id_b.clone(),
        name: "PC-SB".into(),
        ip: "127.0.0.1".into(),
        port: port_b,
        online: true,
        paired: true,
        auto_connect: true,
    });
    state_b.mark_paired(&orbdrop_lib::config::PeerDevice {
        id: id_a.clone(),
        name: "PC-SA".into(),
        ip: "127.0.0.1".into(),
        port: 0,
        online: true,
        paired: true,
        auto_connect: false,
    });

    // B 先点「断开」：它的 `events_ws_handler` 会把 A 的重拨一律 403。
    state_b.ws.pause(&id_a);

    // A 重启恢复 → 被明确拒绝 → 停手，并留下理由。
    orbdrop_lib::ws::restore_links_on_startup(&state_a);
    wait_until("对端拒绝后 A 必须停手", || {
        matches!(state_a.ws.link_state(&id_b), Some(LinkState::Stopped { .. }))
    })
    .await;
    assert_eq!(
        state_a.ws.link_state(&id_b),
        Some(LinkState::Stopped { reason: StopReason::PeerDisconnected }),
        "理由要留给界面（对端点过断开）"
    );
    assert!(!state_a.ws.has_connection(&id_b));

    // 停手就是停手：这段时间里不该再冒出会话（旧行为是每分钟敲一次，永远敲不通）。
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(!state_a.ws.has_connection(&id_b));
    assert!(!state_b.ws.has_connection(&id_a));
    assert_eq!(
        state_a.ws.link_state(&id_b),
        Some(LinkState::Stopped { reason: StopReason::PeerDisconnected })
    );

    // 恢复：本机点「连接」（`manual=1` 会替对端结束它自己的暂停）。
    state_a.ws.resume(&id_b);
    orbdrop_lib::ws::ensure_link_now(&state_a, &id_b);
    wait_until("点连接必须能连回来", || {
        state_a.ws.has_connection(&id_b) && state_b.ws.has_connection(&id_a)
    })
    .await;
    assert!(!state_b.ws.is_paused(&id_a), "对端显式请求连接 → 它的暂停结束");
    assert_eq!(state_a.ws.link_state(&id_b), Some(LinkState::Connected));

    // ---- Cleanup ----
    orbdrop_lib::server::stop_server(&state_b).await;
    credential::remove_peer(&id_a).ok();
    credential::remove_peer(&id_b).ok();
    let _ = std::fs::remove_dir_all(&save_dir);
}

/// 对端不认本机的 token（它清过数据 / 已解除配对）= 401：同样必须停手，理由要能
/// 让界面说清"得重新配对"（继续拨到天荒地老也不会变成连接）。
#[tokio::test]
async fn a_peer_that_rejects_our_token_stops_the_retry_loop() {
    let tag = &uuid::Uuid::new_v4().simple().to_string()[..8];
    let id_a = format!("d-tk-a-{tag}");
    let id_b = format!("d-tk-b-{tag}");
    credential::remove_peer(&id_a).ok();
    credential::remove_peer(&id_b).ok();

    let save_dir = std::env::temp_dir().join(format!("orbdrop_tok_save_dir_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&save_dir).unwrap();
    let state_a = make_state(0, &id_a, "PC-TA", save_dir.clone());
    let state_b = make_state(0, &id_b, "PC-TB", save_dir.clone());

    // A 手里有"给 B 的 token"，但 B 的凭据库里没有**别人**的这枚 token（它只能把它
    // 解到自己头上 → 401）。这正是"对端清过数据"之后本机的处境。
    let token = format!("t-tk-{tag}");
    credential::set_peer(&id_b, &token).unwrap();
    let (base_port, _spare) = two_free_ports();
    let port_b = orbdrop_lib::server::start_server(state_b.clone(), base_port)
        .await
        .expect("B server");
    state_a.mark_paired(&orbdrop_lib::config::PeerDevice {
        id: id_b.clone(),
        name: "PC-TB".into(),
        ip: "127.0.0.1".into(),
        port: port_b,
        online: true,
        paired: true,
        auto_connect: true,
    });
    state_b.mark_paired(&orbdrop_lib::config::PeerDevice {
        id: id_a.clone(),
        name: "PC-TA".into(),
        ip: "127.0.0.1".into(),
        port: 0,
        online: true,
        paired: true,
        auto_connect: false,
    });

    orbdrop_lib::ws::restore_links_on_startup(&state_a);
    wait_until("token 被否后 A 必须停手", || {
        matches!(state_a.ws.link_state(&id_b), Some(LinkState::Stopped { .. }))
    })
    .await;
    assert_eq!(
        state_a.ws.link_state(&id_b),
        Some(LinkState::Stopped { reason: StopReason::TokenRejected }),
        "理由要留给界面（需重新配对）"
    );
    assert!(!state_a.ws.has_connection(&id_b));
    assert!(!state_b.ws.has_connection(&id_a));

    // ---- Cleanup ----
    orbdrop_lib::server::stop_server(&state_b).await;
    credential::remove_peer(&id_a).ok();
    credential::remove_peer(&id_b).ok();
    let _ = std::fs::remove_dir_all(&save_dir);
}

#[tokio::test]
async fn pair_info_answers_who_it_recognises_the_caller_as() {
    let save_dir = std::env::temp_dir().join(format!("orbdrop_info_save_dir_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&save_dir).unwrap();
    let state = make_state(3993, "device-info", "PC-INFO", save_dir.clone());
    let port = orbdrop_lib::server::start_server(state.clone(), 3993).await.expect("server");

    // 对端替我们存的那一条（键 = 我们 id，值 = 这张 token）。
    let caller = format!("device-caller-{}", uuid::Uuid::new_v4());
    let token = format!("tok-{}", uuid::Uuid::new_v4());
    credential::set_peer(&caller, &token).unwrap();

    let client = reqwest::Client::new();
    let url = format!("http://127.0.0.1:{port}/pair/info");

    // 带 token：回「你是谁」+「我把你认作谁」。对端换过 device id 之后，本机手里
    // 那条 token 还有没有用，只能靠这一问（`pairedAs` = 本机 id ⇒ 关系还活着）。
    let body: serde_json::Value = client
        .get(&url)
        .bearer_auth(&token)
        .send().await.unwrap()
        .json().await.unwrap();
    assert_eq!(body["id"], "device-info");
    assert_eq!(body["pairedAs"], caller.as_str(), "认得出带 token 的调用方");

    // 不带 token / token 不认识：回 null，而且绝不回显凭据本身。
    let body: serde_json::Value = client.get(&url).send().await.unwrap().json().await.unwrap();
    assert!(body["pairedAs"].is_null(), "没带 token 就没什么可认的");
    let body: serde_json::Value = client
        .get(&url)
        .bearer_auth("not-a-token")
        .send().await.unwrap()
        .json().await.unwrap();
    assert!(body["pairedAs"].is_null(), "认不出的 token 不算数");

    // 探活/发现走的就是这一问：解析出来的身份与"它把本机认作谁"要一模一样。
    let seen = orbdrop_lib::state::fetch_identity(&client, "127.0.0.1", port, Some(&token))
        .await
        .expect("问得到身份");
    assert_eq!((seen.id.as_str(), seen.name.as_str()), ("device-info", "PC-INFO"));
    assert_eq!(seen.paired_as.as_deref(), Some(caller.as_str()));

    orbdrop_lib::server::stop_server(&state).await;
    credential::remove_peer(&caller).ok();
    let _ = std::fs::remove_dir_all(&save_dir);
}

/// 用户在界面上看到的那种"两台同名设备"：地址上已经是对方的新身份，旧行却因为
/// "这个地址有人答话"被判成永远在线。一轮探活之后旧行必须离线、忘掉地址 ——
/// 列表里只剩它真正的新身份，而配对记录与凭据一点都不能丢。
#[tokio::test]
async fn probe_hides_a_row_whose_address_answers_with_another_device_id() {
    let save_dir = std::env::temp_dir().join(format!("orbdrop_probe_save_dir_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&save_dir).unwrap();
    let state_b = make_state(3999, "device-b-new", "PC-B", save_dir.clone());
    let port_b = orbdrop_lib::server::start_server(state_b.clone(), 3999).await.expect("B server");

    let state_a = make_state(3994, "device-own-a", "PC-A", save_dir.clone());
    let token = format!("tok-{}", uuid::Uuid::new_v4());
    credential::set_peer("device-b-old", &token).unwrap();
    // 地址簿里的旧行：对端在被清数据前的身份，地址还指着它现在的机器。
    state_a.add_peer_book(PeerDevice {
        id: "device-b-old".into(),
        name: "PC-B".into(),
        ip: "127.0.0.1".into(),
        port: port_b,
        online: true,
        paired: true,
        auto_connect: false,
    });

    orbdrop_lib::state::probe_peers_once(&state_a, &reqwest::Client::new()).await;

    let stale = state_a.get_peer("device-b-old").expect("旧行还在");
    assert!(!stale.online, "旧行必须离线：界面只显示在线设备，这就是去重");
    assert_eq!(
        (stale.ip.as_str(), stale.port),
        ("", 0),
        "地址作废，否则每轮探活都去问一个已经不属于它的地址"
    );
    assert!(stale.paired, "配对记录留着（对端拿原 id 回来还能用）");
    assert!(credential::has_peer("device-b-old"), "凭据不能因为探活被清掉");

    orbdrop_lib::server::stop_server(&state_b).await;
    credential::remove_peer("device-b-old").ok();
    let _ = std::fs::remove_dir_all(&save_dir);
}

/// 对端清过数据目录、系统凭据库还在：本机两行（旧 id 已配对 + 新 id 未绑定）必须
/// 收敛成**一行**，而且配对状态跟着新 id 走 —— 用户不用重新配对。
#[test]
fn peer_reset_with_surviving_credential_migrates_the_row_to_the_new_id() {
    let save_dir = std::env::temp_dir().join(format!("orbdrop_migrate_save_dir_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&save_dir).unwrap();
    let state = make_state(3997, "own-device-mig", "PC-OWN", save_dir.clone());
    let config_path = state.config_path.clone();

    let old_id = format!("device-old-{}", uuid::Uuid::new_v4());
    let new_id = format!("device-new-{}", uuid::Uuid::new_v4());
    let token = format!("tok-{}", uuid::Uuid::new_v4());
    credential::set_peer(&old_id, &token).unwrap();

    // 地址簿里的旧行：已配对、地址在、关机时是长连接。
    state.add_peer_book(PeerDevice {
        id: old_id.clone(),
        name: "PC-B".into(),
        ip: "10.0.0.5".into(),
        port: 3890,
        online: true,
        paired: true,
        auto_connect: true,
    });
    // mDNS 已经发现了它对端的新身份：同一地址、另一个 id、未绑定。
    state.upsert_peer(&PeerDevice {
        id: new_id.clone(),
        name: "PC-B".into(),
        ip: "10.0.0.5".into(),
        port: 3890,
        online: true,
        paired: false,
        auto_connect: false,
    });

    let seen = orbdrop_lib::state::PeerIdentity {
        id: new_id.clone(),
        name: "PC-B".into(),
        // 对端拿本机带过去的 token 反解出**本机 id**：配对关系还活着。
        paired_as: Some("own-device-mig".into()),
    };
    assert!(
        orbdrop_lib::state::apply_identity_reconcile(&state, &old_id, &seen),
        "认得出凭据时要迁到新 id"
    );

    let peers = state.peers.lock().clone();
    let here: Vec<&PeerDevice> = peers
        .iter()
        .filter(|p| p.ip == "10.0.0.5" && p.port == 3890)
        .collect();
    assert_eq!(here.len(), 1, "同一台设备只该剩一行：{here:?}");
    let row = here[0];
    assert_eq!(row.id, new_id, "留下来的是它现在的身份");
    assert!(row.paired && row.online, "配对状态跟着新 id：仍是已配对");
    assert!(row.auto_connect, "「关机时连着长连接」的记忆一起迁过去");
    assert!(
        !peers.iter().any(|p| p.id == old_id),
        "旧 id 的行必须消失，否则又是两行"
    );

    // 凭据换键：token 值不变，键改成新 id（两边照旧互相认）。
    assert_eq!(
        credential::get_peer_token(&new_id).unwrap().as_deref(),
        Some(token.as_str())
    );
    assert!(
        credential::get_peer_token(&old_id).unwrap().is_none(),
        "旧键不能留：留着就成了死凭据"
    );

    // 落盘：重启后读到的就是这一行（内存里的状态不算）。
    let saved = read_config(&config_path);
    assert_eq!(saved.peers.len(), 1, "地址簿里也只留一行：{:?}", saved.peers);
    assert_eq!(saved.peers[0].id, new_id);
    assert!(saved.peers[0].paired && saved.peers[0].auto_connect);
    assert_eq!(
        (saved.peers[0].ip.as_str(), saved.peers[0].port),
        ("10.0.0.5", 3890)
    );

    credential::remove_peer(&new_id).ok();
    let _ = std::fs::remove_dir_all(&save_dir);
    let _ = std::fs::remove_file(&config_path);
}

/// 对端清数据时把凭据库一起清了：配对关系已经断了。这一行必须**从列表里消失**
/// （离线 + 忘掉地址），剩下对面真正的新身份；但凭据与配对记录不能被销毁 —— 对端
/// 拿原 id 回来时这一行还得能用。
#[test]
fn peer_reset_without_our_credential_hides_the_stale_row() {
    let save_dir = std::env::temp_dir().join(format!("orbdrop_stale_save_dir_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&save_dir).unwrap();
    let state = make_state(3998, "own-device-stale", "PC-OWN", save_dir.clone());
    let config_path = state.config_path.clone();

    let old_id = format!("device-old-{}", uuid::Uuid::new_v4());
    let new_id = format!("device-new-{}", uuid::Uuid::new_v4());
    let token = format!("tok-{}", uuid::Uuid::new_v4());
    credential::set_peer(&old_id, &token).unwrap();
    state.add_peer_book(PeerDevice {
        id: old_id.clone(),
        name: "PC-B".into(),
        ip: "10.0.0.6".into(),
        port: 3890,
        online: true,
        paired: true,
        auto_connect: false,
    });
    state.upsert_peer(&PeerDevice {
        id: new_id.clone(),
        name: "PC-B".into(),
        ip: "10.0.0.6".into(),
        port: 3890,
        online: true,
        paired: false,
        auto_connect: false,
    });

    let seen = orbdrop_lib::state::PeerIdentity {
        id: new_id.clone(),
        name: "PC-B".into(),
        paired_as: Some("some-other-device".into()), // 它不认我们的 token
    };
    assert!(
        !orbdrop_lib::state::apply_identity_reconcile(&state, &old_id, &seen),
        "认不出凭据就不能迁走配对关系"
    );

    let peers = state.peers.lock().clone();
    let stale = peers.iter().find(|p| p.id == old_id).expect("旧行还在");
    assert!(!stale.online, "作废的行必须离线（界面只显示在线设备）");
    assert_eq!(
        (stale.ip.as_str(), stale.port),
        ("", 0),
        "地址要忘掉，否则每轮探活都去问一个不属于它的地址"
    );
    assert!(stale.paired, "配对记录留着：对端拿原 id 回来还能用");
    assert_eq!(
        credential::get_peer_token(&old_id).unwrap().as_deref(),
        Some(token.as_str()),
        "凭据不能被销毁"
    );
    assert_eq!(
        peers.iter().filter(|p| p.ip == "10.0.0.6").count(),
        1,
        "地址上只剩对面真正的新身份"
    );

    // 落盘的地址同样要作废：重启后不再去探它，也不会以"在线"身份冒出来。
    let saved = read_config(&config_path);
    let saved_stale = saved
        .peers
        .iter()
        .find(|p| p.id == old_id)
        .expect("地址簿里旧行还在（只是没有地址了）");
    assert_eq!((saved_stale.ip.as_str(), saved_stale.port), ("", 0));
    assert!(
        !saved_stale.online,
        "重启后不许再以「在线」出现：它没有地址，没有任何东西会把它纠正回来"
    );

    credential::remove_peer(&old_id).ok();
    let _ = std::fs::remove_dir_all(&save_dir);
    let _ = std::fs::remove_file(&config_path);
}

/// 重新配对必须把上一次留下的"停手理由"清掉。
///
/// token 失效（对端清过数据 / 它把我们解除配对了）时，那台设备停在
/// `Stopped{TokenRejected}` —— 界面显示「需配对」、行内给的是「配对」按钮。用户点它
/// 走完配对、换到新 token 之后，这个停手理由就失效了：不清掉，界面会继续显示
/// 「需配对」并继续给「配对」按钮 —— 配对明明成功了，界面却还在叫他重新配对。
#[test]
fn re_pairing_clears_the_stale_stop_reason() {
    let save_dir = temp_share_dir("repair_save_dir");
    let state = make_state(3977, "device-repair", "PC-REPAIR", save_dir.clone());
    let config_path = state.config_path.clone();
    let dead = PeerDevice {
        id: "device-dead-token".to_string(),
        name: "PC-DEAD".to_string(),
        ip: "10.0.0.9".to_string(),
        port: 3890,
        online: true,
        paired: true,
        auto_connect: false,
    };
    state.upsert_peer(&dead);
    state
        .ws
        .set_link_state(&dead.id, LinkState::Stopped { reason: StopReason::TokenRejected });
    assert_eq!(
        state.ws.link_state(&dead.id),
        Some(LinkState::Stopped { reason: StopReason::TokenRejected }),
        "前置条件：这台设备此刻停在「需配对」"
    );

    // 配对成功这一刻（两条路径 —— 对端拿本机的码来换 token、本机输入对端的码 ——
    // 都收敛到 `mark_paired`）。
    state.mark_paired(&dead);

    assert_eq!(
        state.ws.link_state(&dead.id),
        None,
        "换到新 token 之后不该再显示「需配对」（否则界面会一直把他按回配对流程）"
    );
    assert!(state.get_peer(&dead.id).expect("设备行还在").paired);

    credential::remove_peer(&dead.id).ok();
    let _ = std::fs::remove_dir_all(&save_dir);
    let _ = std::fs::remove_file(&config_path);
}
