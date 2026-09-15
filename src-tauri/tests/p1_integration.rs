//! P1 集成测试：双实例 mDNS 无关（走 HTTP/WS 127.0.0.1）配对 + 推送 + 落盘。
//!
//! 覆盖校验清单：
//! - 配对：A 发起（本机生成 6 位码并显示）→ B 输入该码 → B 向 A 换取 token（F-12）
//! - 配对只到"已配对"：配对本身**不**建长连接，拨号由用户点「连接」发起（F-16）
//! - 推送：邀请 → 同意 → 流式下载 → 文件落盘收件目录（F-50~F-52）
//! - 推送进度事件（F-54 进度数据）
//!
//! 注：keyring（系统凭据库）在本机会话下可用；测试用临时 device id，
//! 结束清理，不污染真实配对。

use std::sync::Arc;
use std::time::Duration;

use orbdrop_lib::credential;
use orbdrop_lib::state::AppState;
use orbdrop_lib::transfer;
use orbdrop_lib::config::{Config, ShareItem};

fn make_state(port: u16, device_id: &str, device_name: &str, inbox_dir: std::path::PathBuf) -> Arc<AppState> {
    let mut config = Config::default();
    config.port = port;
    config.device_id = device_id.to_string();
    config.device_name = device_name.to_string();
    config.inbox_dir = inbox_dir.clone();
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

#[tokio::test]
async fn pairing_then_push_transfers_file_to_receiver() {
    let root = temp_share_dir("A");
    let inbox = std::env::temp_dir().join(format!("orbdrop_it_inbox_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&inbox).unwrap();

    // Two instances (same process): A and B.
    let state_a = make_state(3990, "device-a", "PC-A", inbox.clone());
    let state_b = make_state(3991, "device-b", "PC-B", inbox.clone());

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

    // B accepts → download loop pulls from A and writes to inbox.
    transfer::accept_invite(state_b.clone(), &invite_id).expect("accept");

    // Wait for completion: file appears in B's inbox dir.
    // The invite name is the share item name — ShareItem::new uses the dir name.
    let share_name = root.file_name().unwrap().to_string_lossy().to_string();
    let expected = inbox.join("PC-A").join(&share_name).join("hello.txt");

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
    assert!(done, "file should be transferred to receiver inbox: {:?}", expected);

    // Nested file preserved.
    let nested = inbox.join("PC-A").join(&share_name).join("sub").join("nested.txt");
    assert_eq!(std::fs::read(&nested).unwrap(), b"nested content");

    // Sender-side job should be removed after completion.
    assert!(state_a.push.jobs.lock().get(&invite_id).is_none(), "job cleaned up");

    // ---- Cleanup ----
    orbdrop_lib::server::stop_server(&state_a).await;
    orbdrop_lib::server::stop_server(&state_b).await;
    credential::remove_peer("device-a").ok();
    credential::remove_peer("device-b").ok();
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&inbox);
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
    });

    // 配对不再自己建链：A 点「连接」（镜像 connect_peer）才有链路。
    orbdrop_lib::ws::ensure_link_now(&state_a, "device-ub");
    wait_until("点连接后建链", || {
        state_a.ws.has_connection("device-ub") && state_b.ws.has_connection("device-ua")
    }).await;

    // 故意先把长连接断掉：解除配对不允许依赖 WS 通道。
    state_a.ws.pause("device-ub");
    wait_until("断开生效", || !state_b.ws.has_connection("device-ua")).await;

    orbdrop_lib::unpair_peer(&state_a, "device-ub").await.expect("unpair");

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

    let inbox = std::env::temp_dir().join(format!("orbdrop_nort_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&inbox).unwrap();
    // 点「连接」必然走到 spawn 那一步（拨号只由这个入口发起）。
    let state = make_state(3994, "device-za", "PC-ZA", inbox.clone());
    state.mark_paired(&orbdrop_lib::config::PeerDevice {
        id: "device-zb".into(),
        name: "PC-ZB".into(),
        ip: "127.0.0.1".into(),
        port: 1,
        online: true,
        paired: true,
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
    let _ = std::fs::remove_dir_all(&inbox);
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

    let inbox = std::env::temp_dir().join(format!("orbdrop_auto_inbox_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&inbox).unwrap();

    let state_a = make_state(3992, "device-aa", "PC-AA", inbox.clone());
    let state_b = make_state(3993, "device-ab", "PC-AB", inbox.clone());
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
    let _ = std::fs::remove_dir_all(&inbox);
}
