//! P1 集成测试：双实例 mDNS 无关（走 HTTP/WS 127.0.0.1）配对 + 推送 + 落盘。
//!
//! 覆盖校验清单：
//! - 配对：A 发起（本机生成 6 位码并显示）→ B 输入该码 → B 向 A 换取 token（F-12）
//! - 配对只到"已配对"：配对本身**不**建长连接，拨号由用户点「连接」发起（F-16）
//! - 推送：已绑定（已配对）设备**不经同意**直接收（token 就是那份信任）；未绑定的仍是
//!   "邀请 → 同意 → 流式下载" → 文件落盘下载目录（F-50~F-52；收件目录已并入下载目录）
//! - 推送进度事件（F-54 进度数据）
//! - 多设备推送（F-57）：一份清单分头发给多台、逐台失败隔离、批次快照的实时计数
//!
//! 注：凭据落在数据目录的 `credentials.dat`（P1 时曾是系统凭据库，那条路连同迁移代码
//! 已经删掉）。测试用临时 device id，结束清理，不污染真实配对；**读路径不做迁移**，
//! 所以跑测试不会把本机真实的配对搬走或删掉。需要完全隔离的用例（凭据文件本身的行为）
//! 走 `ORBDROP_DATA_DIR`，见 `tests/credentials_vault.rs`。
//!
//! 到对端那一跳全是 TLS（自签证书 + 指纹 pinning）：测试里凡是打对端地址的请求都必须用
//! `net::client()`（它带着 pinning 校验器），随手 `Client::new()` 只会在自签证书上失败。

use std::sync::Arc;
use std::time::{Duration, Instant};

use orbdrop_lib::credential;
use orbdrop_lib::state::{AppState, LinkIntent};
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
    state_a.config.lock().shares.push(ShareItem::new(root.clone()));

    // Start both servers.
    let port_a = orbdrop_lib::server::start_server(state_a.clone(), 3990).await.expect("A server");
    let port_b = orbdrop_lib::server::start_server(state_b.clone(), 3991).await.expect("B server");
    assert_eq!(port_a, 3990);
    assert_eq!(port_b, 3991);

    let client = orbdrop_lib::net::client();

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
        auto_connect: false, paused: false,
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
        auto_connect: false, paused: false,
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
    // 一个够大的文件：既验证"传输过程中发送方就能看到进度"，也把"按块休眠把速度
    // 按死在几十 KB/s"这个坑钉住（见下面的时限断言）。
    let big = root.join("big.bin");
    std::fs::write(&big, vec![0x5Au8; 32 * 1024 * 1024]).unwrap();

    // A pushes a local directory to B (推送的是用户选的本机路径，与共享项无关）。
    // B 是**已绑定**（已配对）设备 → 这条推送不需要它点同意：整条链路里没有一处调用
    // `accept_invite`，下面"文件真的落了盘"就是"自动收下"的证明（下载循环唯一的入口
    // 就是它 —— 界面点「接收」，或已绑定设备在收到邀请时自动放行）。
    let invite_id = push_invite(state_a.clone(), vec![root.to_string_lossy().to_string()], "device-b").await
        .expect("invite sent over WS")
        .targets[0].invite_id.clone();

    // Wait for completion: file appears under the receiver's save dir, in the
    // <发送方名>/<推送名>/ 子目录.
    // The invite name is the share item name — ShareItem::new uses the dir name.
    let share_name = root.file_name().unwrap().to_string_lossy().to_string();
    let expected = save_dir.join("PC-A").join(&share_name).join("hello.txt");

    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let mut done = false;
    // 传输过程中发送方应该**看得到进度**（不是传完才跳一下）：轮询 job 状态 ——
    // 它由收到对端进度回执的那条路径更新（与推给界面的 push://progress 同一处）。
    let mut saw_progress = false;
    let big_out = save_dir.join("PC-A").join(&share_name).join("big.bin");
    let big_len = std::fs::metadata(&big).unwrap().len();
    let nested = save_dir.join("PC-A").join(&share_name).join("sub").join("nested.txt");
    while std::time::Instant::now() < deadline {
        if let Some(job) = state_a.push.job(&invite_id) {
            if let transfer::push_job::PushStatus::Downloading { received, .. } = job.status {
                if received > 0 {
                    saw_progress = true;
                }
            }
        }
        let hello_ok = std::fs::read(&expected).map(|c| c == b"hello world from A").unwrap_or(false);
        let nested_ok = std::fs::read(&nested).map(|c| c == b"nested content").unwrap_or(false);
        let big_ok = std::fs::metadata(&big_out).map(|m| m.len() == big_len).unwrap_or(false);
        if hello_ok && nested_ok && big_ok {
            done = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(done, "整份目录（含 32MB 大文件）应落到接收方下载目录: {:?}", expected);
    assert!(saw_progress, "发送方应在传输过程中收到进度，而不是只在结束时跳一下");

    // Nested file preserved.
    assert_eq!(std::fs::read(&nested).unwrap(), b"nested content");

    // Sender-side job should be removed after completion.
    // 用等待而不是直接断言：文件落到磁盘 ≠ 完成回执已经走完那条路 —— 后者还要几毫秒，
    // 直接断言等于赌这两件事的先后（实测约 1/5 概率翻车）。
    wait_until("发送方收到完成回执并清掉 job", || {
        state_a.push.jobs.lock().get(&invite_id).is_none()
    })
    .await;

    // ---- Cleanup ----
    orbdrop_lib::server::stop_server(&state_a).await;
    orbdrop_lib::server::stop_server(&state_b).await;
    credential::remove_peer("device-a").ok();
    credential::remove_peer("device-b").ok();
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&save_dir);
}

/// 未绑定（没配对、两边都没有 token）的设备照样能被推送（F-50）：邀请走 HTTP 直达，
/// 接收方同意后从发送方拉取，进度/完成回执也走 HTTP —— 全程**没有** WS 长连接。
///
/// 这条链路是本轮新增的：以前推送只能走长连接，于是"未绑定"和"已配对但没点连接"
/// 这两种设备上的「推送」按钮必然报"对方离线"。
#[tokio::test]
async fn push_to_unpaired_peer_rides_http() {
    let src = temp_share_dir("U");
    let save_dir = std::env::temp_dir().join(format!("orbdrop_it_usave_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&save_dir).unwrap();
    let (port_a, port_b) = two_free_ports();
    let state_a = make_state(port_a, "device-ua", "PC-UA", save_dir.clone());
    let state_b = make_state(port_b, "device-ub", "PC-UB", save_dir.clone());
    let port_a = orbdrop_lib::server::start_server(state_a.clone(), port_a).await.expect("A server");
    let port_b = orbdrop_lib::server::start_server(state_b.clone(), port_b).await.expect("B server");
    assert_eq!(state_a.server_port.load(std::sync::atomic::Ordering::SeqCst), port_a);

    // A 只是"看得见" B（mDNS/手动添加给到地址）：没配对、没有 token、没有长连接。
    state_a.add_peer_book(PeerDevice {
        id: "device-ub".into(),
        name: "PC-UB".into(),
        ip: "127.0.0.1".into(),
        port: port_b,
        online: true,
        paired: false,
        auto_connect: false, paused: false,
    });
    assert!(!state_a.ws.has_connection("device-ub"), "前提：没有长连接");
    assert!(!credential::has_peer("device-ub"), "前提：没有配对 token");

    let invite_id = push_invite(state_a.clone(), vec![src.to_string_lossy().to_string()], "device-ub")
        .await
        .expect("推送应该直接发出去（HTTP 邀请）")
        .targets[0].invite_id.clone();

    // B 的界面上收到了邀请卡片，请求还挂在那儿等它决定。
    wait_until("B 收到推送邀请", || state_b.push.inbound.lock().contains_key(&invite_id)).await;

    // 未绑定设备的推送**必须**等人点同意：邀请到了、没人点，一个字节都不该落盘。
    // （`<下载目录>/<发送方名>` 这层目录是下载循环建的 —— 它存在 = 下载已经开始了。）
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(
        !save_dir.join("PC-UA").exists(),
        "未绑定设备：没点同意就不该开始传"
    );

    // ---- 发送方撤销：HTTP 这条路上也要到得了对端（回执走 POST /push/report）----
    // 此时 B 还没点同意，撤销必须让那张邀请卡消失，而不是等它自己 60s 超时。
    transfer::cancel_push(&state_a, &invite_id).expect("cancel");
    wait_until("撤销后 B 的邀请被撤掉", || {
        !state_b.push.inbound.lock().contains_key(&invite_id)
    })
    .await;

    // ---- 重新推一次（这次真传）----
    let big = src.join("big.bin");
    std::fs::write(&big, vec![0x5Au8; 32 * 1024 * 1024]).unwrap();
    let invite_id = push_invite(state_a.clone(), vec![src.to_string_lossy().to_string()], "device-ub")
        .await
        .expect("再推一次")
        .targets[0].invite_id.clone();
    wait_until("B 收到第二次邀请", || state_b.push.inbound.lock().contains_key(&invite_id)).await;

    transfer::accept_invite(state_b.clone(), &invite_id).expect("B 同意");

    let name = src.file_name().unwrap().to_string_lossy().to_string();
    let expected = save_dir.join("PC-UA").join(&name).join("hello.txt");
    let big_out = save_dir.join("PC-UA").join(&name).join("big.bin");
    let big_len = std::fs::metadata(&big).unwrap().len();
    let nested = save_dir.join("PC-UA").join(&name).join("sub").join("nested.txt");

    // 时限 = 回归闸：以前接收端每块睡 50ms（块只有几 KB），32MB 要几分钟 ——
    // 发送方的进度条就"一直为 0"，接收方那张卡也一直停在 0%。
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let mut saw_progress = false;
    let mut done = false;
    while std::time::Instant::now() < deadline {
        if let Some(job) = state_a.push.job(&invite_id) {
            if let transfer::push_job::PushStatus::Downloading { received, .. } = job.status {
                if received > 0 {
                    saw_progress = true;
                }
            }
        }
        let hello_ok = std::fs::read(&expected).map(|c| c == b"hello world from A").unwrap_or(false);
        let nested_ok = std::fs::read(&nested).map(|c| c == b"nested content").unwrap_or(false);
        let big_ok = std::fs::metadata(&big_out).map(|m| m.len() == big_len).unwrap_or(false);
        if hello_ok && nested_ok && big_ok {
            done = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(done, "HTTP 通道也该把整份目录传完（含 32MB 大文件）");
    assert_eq!(std::fs::read(&expected).unwrap(), b"hello world from A");
    assert!(
        std::fs::read(&nested).unwrap() == b"nested content",
        "目录结构完整保留"
    );
    assert!(saw_progress, "HTTP 通道下发送方也应在传输过程中收到进度");

    // 完成回执也是 HTTP：A 收到后把 job 清掉。
    wait_until("发送方收到完成回执", || {
        state_a.push.jobs.lock().get(&invite_id).is_none()
    })
    .await;

    orbdrop_lib::server::stop_server(&state_a).await;
    orbdrop_lib::server::stop_server(&state_b).await;
    let _ = std::fs::remove_dir_all(&src);
    let _ = std::fs::remove_dir_all(&save_dir);
}

/// 已绑定（已配对）但**没建长连接**的设备：推送走 HTTP 直达，同样不需要接收方同意 ——
/// token 就是那份信任。"自动收下"在长连接那条路和 HTTP 这条路上是两段代码
/// （`ws::session` 的 `push_invite` 分支 / `routes_push::push_invite_handler`），两条都得测。
#[tokio::test]
async fn paired_push_without_a_long_connection_skips_consent_http() {
    let src = temp_share_dir("PH");
    let save_dir = std::env::temp_dir().join(format!("orbdrop_it_phsave_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&save_dir).unwrap();
    let (port_a, port_b) = two_free_ports();
    let state_a = make_state(port_a, "device-pha", "PC-PHA", save_dir.clone());
    let state_b = make_state(port_b, "device-phb", "PC-PHB", save_dir.clone());
    let port_a = orbdrop_lib::server::start_server(state_a.clone(), port_a).await.expect("A server");
    let port_b = orbdrop_lib::server::start_server(state_b.clone(), port_b).await.expect("B server");

    // 两侧互相已配对：地址簿标 paired，凭据里存着**同一枚** token（真实配对换来的就是
    // 这一枚 —— 谁给对方发邀请就用它做 Bearer）。凭据文件是进程内共用的一份，所以这里
    // 用一对独一无二的 device id + token，跑完清掉，别踩到别的用例。
    const SHARED_TOKEN: &str = "0123456789abcdef0123456789abcdef";
    credential::set_peer("device-phb", SHARED_TOKEN).unwrap();
    credential::set_peer("device-pha", SHARED_TOKEN).unwrap();
    state_a.add_peer_book(PeerDevice {
        id: "device-phb".into(),
        name: "PC-PHB".into(),
        ip: "127.0.0.1".into(),
        port: port_b,
        online: true,
        paired: true,
        auto_connect: false, paused: false,
    });
    state_b.add_peer_book(PeerDevice {
        id: "device-pha".into(),
        name: "PC-PHA".into(),
        ip: "127.0.0.1".into(),
        port: port_a,
        online: true,
        paired: true,
        auto_connect: false, paused: false,
    });
    assert!(!state_a.ws.has_connection("device-phb"), "前提：没有长连接（走 HTTP 那条路）");

    let invite_id = push_invite(state_a.clone(), vec![src.to_string_lossy().to_string()], "device-phb")
        .await
        .expect("已绑定设备的邀请应该直接发出去")
        .targets[0].invite_id.clone();
    // 同前：没有任何一处调用 `accept_invite`，落盘即"自动收下"。
    let name = src.file_name().unwrap().to_string_lossy().to_string();
    let expected = save_dir.join("PC-PHA").join(&name).join("hello.txt");
    wait_until("已绑定但未长连接的设备：没点同意也把文件收下", || {
        std::fs::read(&expected).map(|c| c == b"hello world from A").unwrap_or(false)
    })
    .await;

    // 这条路没有 WS：完成回执走 POST /push/report，发送方的 job 由它清掉。
    wait_until("发送方收到完成回执（HTTP）", || {
        state_a.push.jobs.lock().get(&invite_id).is_none()
    })
    .await;
    assert!(
        !state_b.push.inbound.lock().contains_key(&invite_id),
        "传完就该清掉，不留一条僵尸邀请"
    );

    orbdrop_lib::server::stop_server(&state_a).await;
    orbdrop_lib::server::stop_server(&state_b).await;
    credential::remove_peer("device-pha").ok();
    credential::remove_peer("device-phb").ok();
    let _ = std::fs::remove_dir_all(&src);
    let _ = std::fs::remove_dir_all(&save_dir);
}

/// n 个互不相同的空闲端口（每次分配都会立刻放掉）。
fn free_ports(n: usize) -> Vec<u16> {
    let mut out = Vec::new();
    while out.len() < n {
        let p = free_port();
        if !out.contains(&p) {
            out.push(p);
        }
    }
    out
}

/// 多设备推送（F-50）：一次选择、**一份清单**、每台各有一条邀请。
///
/// 与单台推送的区别只有三件事，这里盯的正是它们：
/// - **失败隔离**：一台离线就地失败（F-53，不排队），另外两台照走；
/// - **一份清单**：两台收到的邀请内容一字不差（不是各建一份，落盘路径也就一致）；
/// - **整批的实时状态**：每台的成败/在途都落在同一批快照里，批次活到最后一台结束。
///
/// "还差几台"这件事靠**按住 C 的同意**测：C 未绑定，不点同意就不开始 —— 于是
/// "B 成了、C 还在等"这一刻是确定性的窗口，快照里的 成功/等待/失败 三个数可以逐一对上。
#[tokio::test]
async fn multi_device_push_isolates_targets_and_shares_one_manifest() {
    let src = temp_share_dir("MD");
    let big = src.join("big.bin");
    let big_len: u64 = 32 * 1024 * 1024;
    std::fs::write(&big, vec![0x5Au8; big_len as usize]).unwrap();
    let save_b = std::env::temp_dir().join(format!("orbdrop_it_mdb_{}", uuid::Uuid::new_v4()));
    let save_c = std::env::temp_dir().join(format!("orbdrop_it_mdc_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&save_b).unwrap();
    std::fs::create_dir_all(&save_c).unwrap();

    let ports = free_ports(3);
    let (port_a, port_b, port_c) = (ports[0], ports[1], ports[2]);
    let state_a = make_state(port_a, "device-mda", "PC-MDA", save_b.clone());
    let state_b = make_state(port_b, "device-mdb", "PC-MDB", save_b.clone());
    let state_c = make_state(port_c, "device-mdc", "PC-MDC", save_c.clone());
    let port_a = orbdrop_lib::server::start_server(state_a.clone(), port_a).await.expect("A server");
    let port_b = orbdrop_lib::server::start_server(state_b.clone(), port_b).await.expect("B server");
    let port_c = orbdrop_lib::server::start_server(state_c.clone(), port_c).await.expect("C server");

    // B 已配对（收到就收，不问人）、C 未绑定（要它自己点头）、D 已配对但离线。
    const SHARED_TOKEN: &str = "fedcba9876543210fedcba9876543210";
    credential::set_peer("device-mdb", SHARED_TOKEN).unwrap();
    credential::set_peer("device-mda", SHARED_TOKEN).unwrap();
    let peer = |id: &str, name: &str, port: u16, online: bool, paired: bool| PeerDevice {
        id: id.into(),
        name: name.into(),
        ip: if online { "127.0.0.1".into() } else { String::new() },
        port: if online { port } else { 0 },
        online,
        paired,
        auto_connect: false, paused: false,
    };
    state_a.add_peer_book(peer("device-mdb", "PC-MDB", port_b, true, true));
    state_a.add_peer_book(peer("device-mdc", "PC-MDC", port_c, true, false));
    state_a.add_peer_book(peer("device-mdd", "PC-MDD", 0, false, true));
    state_b.add_peer_book(peer("device-mda", "PC-MDA", port_a, true, true));
    state_c.add_peer_book(peer("device-mda", "PC-MDA", port_a, true, false));
    assert!(!state_a.ws.has_connection("device-mdb"), "前提：走 HTTP 那条路（这条用例不测 WS 通道）");

    // 同一台勾两次只发一条：4 个 ptr 里有一个重复项，落成 3 台。
    let paths = vec![src.to_string_lossy().to_string()];
    let view = transfer::push_paths(
        state_a.clone(),
        paths,
        vec!["device-mdb".into(), "device-mdb".into(), "device-mdc".into(), "device-mdd".into()],
    )
    .await
    .expect("两台在线：整批该发出去");
    let batch_id = view.batch_id.clone();

    // ── 每台各有独立状态：离线那台当场失败（F-53），其余两台在等 ──
    assert_eq!((view.targets.len(), view.waiting, view.failed), (3, 2, 1), "3 台目标：2 台在等、1 台离线失败");
    let offline = view.targets.iter().find(|t| t.peer_id == "device-mdd").expect("离线那一行还在");
    assert_eq!(offline.state, "failed");
    assert!(
        offline.error.as_deref().unwrap_or("").contains("不在网上"),
        "失败原因要说清是离线：{:?}",
        offline.error
    );
    let invite_b = view.targets.iter().find(|t| t.peer_id == "device-mdb").unwrap().invite_id.clone();
    let invite_c = view.targets.iter().find(|t| t.peer_id == "device-mdc").unwrap().invite_id.clone();
    assert!(!invite_b.is_empty() && !invite_c.is_empty() && invite_b != invite_c, "每台各有一条邀请");
    assert_eq!(state_a.push.batch_id_of(&invite_b).as_deref(), Some(batch_id.as_str()), "邀请反查得到自己那一批");
    assert_eq!(state_a.push.batch_id_of(&invite_c).as_deref(), Some(batch_id.as_str()), "两台归在同一批");

    // ── 一份清单：两台收到的邀请内容一字不差（此刻 B 还在传、C 还没点头，两条邀请都在） ──
    wait_until("C 收到邀请", || state_c.push.inbound.lock().contains_key(&invite_c)).await;
    let name = src.file_name().unwrap().to_string_lossy().to_string();
    let manifest = |state: &Arc<AppState>, invite: &str| {
        state
            .push
            .inbound
            .lock()
            .get(invite)
            .map(|inv| (inv.name.clone(), inv.files.len(), inv.total_bytes()))
    };
    let mb = manifest(&state_b, &invite_b);
    let mc = manifest(&state_c, &invite_c);
    assert!(mb.is_some(), "B 那条邀请还在（下载循环拿它当'没被取消'的标志）");
    assert_eq!(mb, mc, "两台收到的清单必须完全一致（清单只建一次）");

    // ── 传输中：整批快照里出现"传输中"那一档（界面那张卡的实时数字就是它） ──
    let mut saw_sending = false;
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    while std::time::Instant::now() < deadline {
        if let Some(shot) = state_a.push.batch_snapshot(&batch_id) {
            if shot.targets.iter().any(|t| t.state == "sending") {
                assert_eq!((shot.done, shot.waiting, shot.failed), (0, 2, 1), "传的过程中：成功 0 · 等待 2 · 失败 1");
                assert!(shot.received > 0 && shot.total > 0, "聚合进度该往前走");
                saw_sending = true;
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(saw_sending, "整批快照里应出现'传输中'（不是传完才跳一下）");

    // ── B 传完、C 仍在等：计数行就是"成功 1 · 等待 1 · 失败 1"（C 不点头它就一直等着） ──
    let expected_b = save_b.join("PC-MDA").join(&name).join("big.bin");
    wait_until("B 自动收下整份目录", || {
        std::fs::metadata(&expected_b).map(|m| m.len() == big_len).unwrap_or(false)
    })
    .await;
    wait_until("快照里 B 已成功、C 还在等", || {
        state_a
            .push
            .batch_snapshot(&batch_id)
            .map(|shot| (shot.done, shot.waiting, shot.failed) == (1, 1, 1))
            .unwrap_or(false)
    })
    .await;

    // ── C 点头（未绑定设备的同意制在批次里照旧）→ 也传完 ──
    transfer::accept_invite(state_c.clone(), &invite_c).expect("C 同意");
    let expected_c = save_c.join("PC-MDA").join(&name).join("hello.txt");
    wait_until("C 也收下整份目录", || {
        std::fs::read(&expected_c).map(|c| c == b"hello world from A").unwrap_or(false)
    })
    .await;
    assert!(
        std::fs::read(save_c.join("PC-MDA").join(&name).join("sub").join("nested.txt")).is_ok(),
        "目录结构照旧完整保留"
    );

    // ── 整批的最后一步：没有一台还在路上，批次就丢掉（界面手里那份快照已经是全量） ──
    wait_until("整批结束后批次被丢掉", || state_a.push.batch_snapshot(&batch_id).is_none()).await;

    // ── 一台都派不出去 = 单台那条"就地失败"（F-53）：整体报错，界面弹提示、不建卡片 ──
    let err = transfer::push_paths(
        state_a.clone(),
        vec![src.to_string_lossy().to_string()],
        vec!["device-mdd".into(), "device-missing".into()],
    )
    .await
    .expect_err("都收不了就不该建卡片");
    assert!(err.contains("都收不了"), "错误要说清是「没一台能收」：{err}");

    orbdrop_lib::server::stop_server(&state_a).await;
    orbdrop_lib::server::stop_server(&state_b).await;
    orbdrop_lib::server::stop_server(&state_c).await;
    credential::remove_peer("device-mda").ok();
    credential::remove_peer("device-mdb").ok();
    let _ = std::fs::remove_dir_all(&src);
    let _ = std::fs::remove_dir_all(&save_b);
    let _ = std::fs::remove_dir_all(&save_c);
}

/// 多选推送（F-50）：选中的几项合并成**一条**推送 —— 对端只收到一张邀请卡、只同意一次，
/// 落盘仍是一棵完整的树（每个源各占一个顶层前缀），同名源自动去重。
#[tokio::test]
async fn multi_select_push_is_one_job_with_one_consent() {
    let base = std::env::temp_dir().join(format!("orbdrop_it_multi_{}", uuid::Uuid::new_v4()));
    let (dir_a, dir_x, dir_y) = (base.join("甲目录"), base.join("x"), base.join("y"));
    for d in [&dir_a, &dir_x, &dir_y] {
        std::fs::create_dir_all(d.join("sub")).unwrap();
    }
    std::fs::write(dir_a.join("报告.txt"), b"report A").unwrap();
    std::fs::write(dir_a.join("sub").join("明细.txt"), b"detail A").unwrap();
    let loose = base.join("散文件.txt");
    std::fs::write(&loose, b"loose file").unwrap();
    let same_x = dir_x.join("同名.txt");
    let same_y = dir_y.join("同名.txt");
    std::fs::write(&same_x, b"same name from x").unwrap();
    std::fs::write(&same_y, b"same name from y").unwrap();

    let save_dir = std::env::temp_dir().join(format!("orbdrop_it_msave_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&save_dir).unwrap();
    let (port_a, port_b) = two_free_ports();
    let state_a = make_state(port_a, "device-ua", "PC-UM", save_dir.clone());
    let state_b = make_state(port_b, "device-ub", "PC-UN", save_dir.clone());
    let _port_a = orbdrop_lib::server::start_server(state_a.clone(), port_a).await.expect("A server");
    let port_b = orbdrop_lib::server::start_server(state_b.clone(), port_b).await.expect("B server");
    state_a.add_peer_book(PeerDevice {
        id: "device-ub".into(),
        name: "PC-UN".into(),
        ip: "127.0.0.1".into(),
        port: port_b,
        online: true,
        paired: false,
        auto_connect: false, paused: false,
    });

    // ---- 第一批：一个目录 + 一个散文件（两个源）—— 各以自己的名字平铺在 <发送方>/ 下 ----
    let picks: Vec<String> = [&dir_a, &loose]
        .iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect();
    let invite_id = push_invite(state_a.clone(), picks, "device-ub")
        .await
        .expect("多选也该直接发出去")
        .targets[0].invite_id.clone();
    wait_until("B 收到邀请", || state_b.push.inbound.lock().contains_key(&invite_id)).await;

    // 多选**不套**"推送名"那一层：直接传多文件就是多文件，不是在目录里。
    let root = save_dir.join("PC-UM");
    let first_batch = [
        (root.join("甲目录").join("报告.txt"), &b"report A"[..]),
        (root.join("甲目录").join("sub").join("明细.txt"), &b"detail A"[..]),
        (root.join("散文件.txt"), &b"loose file"[..]),
    ];
    run_one_push(&state_a, &state_b, &invite_id, &first_batch).await;

    // 一条邀请 = 一次同意：多选不是 N 次推送。
    assert_eq!(
        state_b.push.inbound.lock().len(),
        0,
        "传完就该清掉（不是留了 N 条）"
    );

    // ---- 第二批：两个同名文件、父目录不同 → 平铺 + 同名自动去重 ----
    let picks: Vec<String> = [&same_x, &same_y]
        .iter()
        .map(|p| p.to_string_lossy().to_string())
        .collect();
    let invite_id = push_invite(state_a.clone(), picks, "device-ub")
        .await
        .expect("第二批")
        .targets[0].invite_id.clone();
    wait_until("B 收到第二批邀请", || state_b.push.inbound.lock().contains_key(&invite_id)).await;
    let manifest_len = state_b
        .push
        .inbound
        .lock()
        .get(&invite_id)
        .map(|inv| inv.files.len())
        .unwrap_or(0);
    assert_eq!(manifest_len, 2, "两个源两条清单项");

    let second_batch = [
        (root.join("同名.txt"), &b"same name from x"[..]),
        (root.join("同名 (2).txt"), &b"same name from y"[..]),
    ];
    run_one_push(&state_a, &state_b, &invite_id, &second_batch).await;

    // 整棵落地形状：<下载目录>/<发送方名>/ 下面就是那几项，没有多余的一层。
    assert!(
        !root.join("2 项").exists() && !root.join("甲目录").join("甲目录").exists(),
        "不该有「推送名」那一层目录"
    );

    orbdrop_lib::server::stop_server(&state_a).await;
    orbdrop_lib::server::stop_server(&state_b).await;
    let _ = std::fs::remove_dir_all(&base);
    let _ = std::fs::remove_dir_all(&save_dir);
}

/// 单台推送 = 一批只有一台的推送：测试要的是那条邀请 id（`BatchView.targets[0]`）。
///
/// 多设备推送（一批 N 台）的行为在 `push_paths` 自己那儿由 `BatchView` 全量给出，
/// 这里只是把"一台"这件常见事收敛成一句。
async fn push_invite(
    state: Arc<AppState>,
    paths: Vec<String>,
    peer_id: &str,
) -> Result<transfer::push_job::BatchView, String> {
    transfer::push_paths(state, paths, vec![peer_id.to_string()]).await
}

/// 「同意 → 等这批文件全部落地 → 等发送方收到完成回执」——多选用例的两批都走这一套。
async fn run_one_push(
    state_a: &Arc<AppState>,
    state_b: &Arc<AppState>,
    invite_id: &str,
    expect: &[(std::path::PathBuf, &[u8])],
) {
    transfer::accept_invite(state_b.clone(), invite_id).expect("同意");
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        if expect
            .iter()
            .all(|(p, want)| std::fs::read(p).map(|got| got == *want).unwrap_or(false))
        {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "每个源都该按前缀落到对端：{:?}",
            expect.iter().map(|(p, _)| p).collect::<Vec<_>>()
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    wait_until("发送方收到完成回执", || {
        state_a.push.jobs.lock().get(invite_id).is_none()
    })
    .await;
}

/// 推**单个文件**时对面不套同名目录：`A.txt` 直接落在 `<下载目录>/<发送方名>/A.txt`，
/// 不是 `<下载目录>/<发送方名>/A.txt/A.txt`（那一层子目录只对"目录 / 多项"推送有分组意义）。
#[tokio::test]
async fn a_single_pushed_file_lands_as_a_file() {
    let src_dir = std::env::temp_dir().join(format!("orbdrop_it_one_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&src_dir).unwrap();
    let file = src_dir.join("A.txt");
    std::fs::write(&file, b"single file body").unwrap();

    let save_dir = std::env::temp_dir().join(format!("orbdrop_it_osave_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&save_dir).unwrap();
    let (port_a, port_b) = two_free_ports();
    let state_a = make_state(port_a, "device-ua", "PC-UO", save_dir.clone());
    let state_b = make_state(port_b, "device-ub", "PC-UP", save_dir.clone());
    let _port_a = orbdrop_lib::server::start_server(state_a.clone(), port_a).await.expect("A server");
    let port_b = orbdrop_lib::server::start_server(state_b.clone(), port_b).await.expect("B server");
    state_a.add_peer_book(PeerDevice {
        id: "device-ub".into(),
        name: "PC-UP".into(),
        ip: "127.0.0.1".into(),
        port: port_b,
        online: true,
        paired: false,
        auto_connect: false, paused: false,
    });

    let invite_id = push_invite(state_a.clone(), vec![file.to_string_lossy().to_string()], "device-ub")
        .await
        .expect("邀请发出去")
        .targets[0].invite_id.clone();
    wait_until("B 收到邀请", || state_b.push.inbound.lock().contains_key(&invite_id)).await;
    transfer::accept_invite(state_b.clone(), &invite_id).expect("B 同意");

    let landed = save_dir.join("PC-UO").join("A.txt");
    wait_until("单个文件直接落盘（不套同名目录）", || {
        std::fs::read(&landed).map(|c| c == b"single file body").unwrap_or(false)
    })
    .await;
    assert!(landed.is_file(), "落下来的是一个文件: {landed:?}");

    orbdrop_lib::server::stop_server(&state_a).await;
    orbdrop_lib::server::stop_server(&state_b).await;
    let _ = std::fs::remove_dir_all(&src_dir);
    let _ = std::fs::remove_dir_all(&save_dir);
}

/// **同步上下文**里点「同意接收」也必须真的开始下载。
///
/// Tauri 的同步命令跑在主线程上：那里没有 Tokio reactor。`tokio::spawn` 会 panic（点一下
/// App 就闪退），而"没有运行时就跳过"会让点击**静默失效** —— 用户侧看到的是两边都停在 0%，
/// 日志里只有一行 `no tokio runtime here; task skipped task="push download"`。
#[tokio::test]
async fn accepting_a_push_from_a_sync_context_still_downloads() {
    let src = temp_share_dir("S");
    let save_dir = std::env::temp_dir().join(format!("orbdrop_it_ssave_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&save_dir).unwrap();
    let (port_a, port_b) = two_free_ports();
    let state_a = make_state(port_a, "device-ua", "PC-US", save_dir.clone());
    let state_b = make_state(port_b, "device-ub", "PC-UT", save_dir.clone());
    let _port_a = orbdrop_lib::server::start_server(state_a.clone(), port_a).await.expect("A server");
    let port_b = orbdrop_lib::server::start_server(state_b.clone(), port_b).await.expect("B server");
    state_a.add_peer_book(PeerDevice {
        id: "device-ub".into(),
        name: "PC-UT".into(),
        ip: "127.0.0.1".into(),
        port: port_b,
        online: true,
        paired: false,
        auto_connect: false, paused: false,
    });

    let invite_id = push_invite(state_a.clone(), vec![src.to_string_lossy().to_string()], "device-ub")
        .await
        .expect("邀请发出去")
        .targets[0].invite_id.clone();
    wait_until("B 收到邀请", || state_b.push.inbound.lock().contains_key(&invite_id)).await;

    // 「同意接收」：在一个**没有 reactor** 的普通线程上调用 —— 同步 Tauri 命令就是这个上下文。
    let accept_state = state_b.clone();
    let accept_id = invite_id.clone();
    std::thread::spawn(move || {
        transfer::accept_invite(accept_state, &accept_id).expect("accept");
    })
    .join()
    .expect("accept 线程");

    let name = src.file_name().unwrap().to_string_lossy().to_string();
    let expected = save_dir.join("PC-US").join(&name).join("hello.txt");
    wait_until("同步上下文点同意后，文件也要真的落盘", || {
        std::fs::read(&expected).map(|c| c == b"hello world from A").unwrap_or(false)
    })
    .await;

    orbdrop_lib::server::stop_server(&state_a).await;
    orbdrop_lib::server::stop_server(&state_b).await;
    let _ = std::fs::remove_dir_all(&src);
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
    let resp = orbdrop_lib::net::client()
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
        auto_connect: false, paused: false,
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
        let resp = orbdrop_lib::net::client()
            .get(format!("https://{lan_ip}:{port_a}/shares"))
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
/// （用户侧表现为设备 A 点「连接」时 App 闪退）。现在 `task::spawn` 会退到 Tauri 的全局
/// 运行时 —— 任务真的跑起来，但这里的目标是对端端口 1（没人听），所以会话照样建立不了。
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
        auto_connect: false, paused: false,
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
        count: 1,
        files: vec![],
        callback: Some(("127.0.0.1".into(), 1)),
        responder: None,
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
        auto_connect: false, paused: false,
    });

    let resp = orbdrop_lib::net::client()
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
        auto_connect: false, paused: false,
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
        auto_connect: false, paused: false,
    });
    state_b.mark_paired(&orbdrop_lib::config::PeerDevice {
        id: "device-ra".into(),
        name: "PC-RA".into(),
        ip: "127.0.0.1".into(),
        port: 3995,
        online: true,
        paired: true,
        auto_connect: false, paused: false,
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
    state_a2.set_link_intent("device-rb", LinkIntent::Paused); // 镜像 disconnect_peer 的两步
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
// 「断开」被对端记忆撤销的那条路（重启后）
// ---------------------------------------------------------------------------

/// F-16 回归：本机点过「断开」，**重启后仍要拒绝对端的自动重拨**。
///
/// 用户实际遇到的那条路：本机点「断开」→ 退出 → 重开。本机这边确实不再主动拨
/// （`auto_connect` 已清），但对端**没点过断开**、它还记着这次长连接：它一看见本机
/// 回到网上就拨过来。原来"拒绝对端的重拨"只活在内存里（`WsManager::paused`），重启
/// 把它一起带走了 —— 本机照收那条拨号，双方界面回到「长连接」，而且被拨号的一端顺手
/// 把 `auto_connect` 写回来，从此每次重启都自己连（用户看到的："主动断开、退出重开
/// 又连上了"）。
///
/// 所以这里验三件事：暂停**落盘**（`paused`）、重启后被读回并挡住对端的自动拨号
/// （403），以及"对端的人真的点了「连接」"这条正路照旧通（`manual=1` 结束本机暂停）。
#[tokio::test]
async fn a_disconnect_also_survives_a_restart_against_the_peers_memory() {
    let tag = &uuid::Uuid::new_v4().simple().to_string()[..8];
    let id_a = format!("d-hb-a-{tag}");
    let id_b = format!("d-hb-b-{tag}");

    let save_dir = std::env::temp_dir().join(format!("orbdrop_hb_save_dir_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&save_dir).unwrap();
    let (port_a, port_b) = two_free_ports();
    let state_a = make_state(port_a, &id_a, "PC-HBA", save_dir.clone());
    let state_b = make_state(port_b, &id_b, "PC-HBB", save_dir.clone());
    let config_path_a = state_a.config_path.clone();
    let config_path_b = state_b.config_path.clone();

    credential::remove_peer(&id_a).ok();
    credential::remove_peer(&id_b).ok();
    let token = format!("t-hb-{tag}");
    credential::set_peer(&id_a, &token).unwrap();
    credential::set_peer(&id_b, &token).unwrap();

    // 两边都得起服务：重启后要验的正是**入站闸门**（对端拨过来时被 403 挡掉），
    // 没有服务就没有那个闸门可验。
    let port_a = orbdrop_lib::server::start_server(state_a.clone(), port_a).await.expect("A server");
    let port_b = orbdrop_lib::server::start_server(state_b.clone(), port_b).await.expect("B server");
    state_a.mark_paired(&PeerDevice {
        id: id_b.clone(), name: "PC-HBB".into(), ip: "127.0.0.1".into(), port: port_b,
        online: true, paired: true, auto_connect: false, paused: false,
    });
    state_b.mark_paired(&PeerDevice {
        id: id_a.clone(), name: "PC-HBA".into(), ip: "127.0.0.1".into(), port: port_a,
        online: true, paired: true, auto_connect: false, paused: false,
    });

    // A 点「连接」→ 链路建立，两侧都记下这次长连接。
    orbdrop_lib::ws::ensure_link_now(&state_a, &id_b);
    wait_until("点连接建立链路", || {
        state_a.ws.has_connection(&id_b) && state_b.ws.has_connection(&id_a)
    })
    .await;
    assert!(
        read_config(&config_path_b)
            .peers
            .iter()
            .find(|p| p.id == id_a)
            .expect("B 的地址簿里有 A")
            .auto_connect,
        "被拨号的一端也记着这次长连接 —— 这正是下面「重启后它还会拨过来」的前提"
    );

    // ---- A 点「断开」（镜像 `disconnect_peer`），然后退出 ----
    state_a.ws.pause(&id_b);
    state_a.set_link_intent(&id_b, LinkIntent::Paused);
    wait_until("断开后链路消失", || !state_b.ws.has_connection(&id_a)).await;
    let row = read_config(&config_path_a);
    let row = row.peers.iter().find(|p| p.id == id_b).expect("A 的地址簿里有 B");
    assert!(
        !row.auto_connect && row.paused,
        "「断开」的两个方向都要落盘：本机不拨（auto_connect=false）、也拒绝对端的拨（paused=true）"
    );

    // ---- 重启 A：服务也随进程换掉（新 state 起在同一个端口上）----
    orbdrop_lib::server::stop_server(&state_a).await;
    let state_a2 = Arc::new(AppState::new(read_config(&config_path_a), config_path_a.clone()));
    assert!(
        state_a2.ws.is_paused(&id_b),
        "暂停标记要从地址簿读回来 —— 它要是只活在内存里，重启就等于把「断开」撤了"
    );
    let port_a2 = orbdrop_lib::server::start_server(state_a2.clone(), port_a).await.expect("A server (restarted)");
    assert_eq!(port_a2, port_a, "重启后端口不变，对端存的地址才还能用");
    orbdrop_lib::ws::restore_links_on_startup(&state_a2);
    tokio::time::sleep(Duration::from_millis(600)).await;
    assert!(!state_a2.ws.has_connection(&id_b), "本机不主动拨（这条没变过）");

    // ---- 对端还记着这次长连接：它看见本机回到网上就拨过来（真机上由探活/mDNS 触发）----
    orbdrop_lib::ws::redial_if_remembered(&state_b, &id_a);
    tokio::time::sleep(Duration::from_millis(1500)).await;
    assert!(
        !state_a2.ws.has_connection(&id_b) && !state_b.ws.has_connection(&id_a),
        "重启后「断开」仍然有效：对端的自动重拨必须被拒（403），链路不许自己接回来"
    );
    assert!(
        !read_config(&config_path_a)
            .peers
            .iter()
            .find(|p| p.id == id_b)
            .expect("A 的地址簿里有 B")
            .auto_connect,
        "被拨号的一端不该顺手把「要这条链」写回来"
    );

    // ---- 正路照旧：对端的人点「连接」→ `manual=1` → 本机的暂停结束 ----
    orbdrop_lib::ws::ensure_link_now(&state_b, &id_a);
    wait_until("对端点「连接」→ 恢复链路", || {
        state_a2.ws.has_connection(&id_b) && state_b.ws.has_connection(&id_a)
    })
    .await;
    assert!(!state_a2.ws.is_paused(&id_b), "对端显式请求连接 → 本机的暂停结束");
    let row = read_config(&config_path_a);
    let row = row.peers.iter().find(|p| p.id == id_b).expect("A 的地址簿里有 B");
    assert!(
        row.auto_connect && !row.paused,
        "这条链是有人点「连接」要的：意图落盘，重启后该自动接回来"
    );

    // ---- Cleanup ----
    orbdrop_lib::server::stop_server(&state_a2).await;
    orbdrop_lib::server::stop_server(&state_b).await;
    credential::remove_peer(&id_a).ok();
    credential::remove_peer(&id_b).ok();
    let _ = std::fs::remove_dir_all(&save_dir);
    let _ = std::fs::remove_file(&config_path_a);
    let _ = std::fs::remove_file(&config_path_b);
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
        auto_connect: true, paused: false,
    });
    state_b.mark_paired(&orbdrop_lib::config::PeerDevice {
        id: id_a.clone(),
        name: "PC-SA".into(),
        ip: "127.0.0.1".into(),
        port: 0,
        online: true,
        paired: true,
        auto_connect: false, paused: false,
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
        auto_connect: false, paused: false,
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
        auto_connect: true, paused: false,
    });
    state_b.mark_paired(&orbdrop_lib::config::PeerDevice {
        id: id_a.clone(),
        name: "PC-SA".into(),
        ip: "127.0.0.1".into(),
        port: 0,
        online: true,
        paired: true,
        auto_connect: false, paused: false,
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
        auto_connect: true, paused: false,
    });
    state_b.mark_paired(&orbdrop_lib::config::PeerDevice {
        id: id_a.clone(),
        name: "PC-TA".into(),
        ip: "127.0.0.1".into(),
        port: 0,
        online: true,
        paired: true,
        auto_connect: false, paused: false,
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

    let client = orbdrop_lib::net::client();
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
    let seen = orbdrop_lib::state::fetch_identity(&orbdrop_lib::net::client(), "127.0.0.1", port, Some(&token))
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
        auto_connect: false, paused: false,
    });

    orbdrop_lib::state::probe_peers_once(&state_a).await;

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

/// 对端清过一部分状态、本机手里那条凭据还有效：本机两行（旧 id 已配对 + 新 id 未绑定）
/// 必须收敛成**一行**，而且配对状态跟着新 id 走 —— 用户不用重新配对。
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
        auto_connect: true, paused: false,
    });
    // mDNS 已经发现了它对端的新身份：同一地址、另一个 id、未绑定。
    state.upsert_peer(&PeerDevice {
        id: new_id.clone(),
        name: "PC-B".into(),
        ip: "10.0.0.5".into(),
        port: 3890,
        online: true,
        paired: false,
        auto_connect: false, paused: false,
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
        auto_connect: false, paused: false,
    });
    state.upsert_peer(&PeerDevice {
        id: new_id.clone(),
        name: "PC-B".into(),
        ip: "10.0.0.6".into(),
        port: 3890,
        online: true,
        paired: false,
        auto_connect: false, paused: false,
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
        auto_connect: false, paused: false,
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

/// 配对邀请（F-12 免配对码那条路）：A 点「发起邀请」→ 邀请作为一次 HTTP POST 推到 B，
/// 在 B 的面板右下角挂一条带「接收/拒绝」的通知 → B 点「接收」→ 两边各存下对方铸的 token。
/// **全程没有人输入配对码。**
///
/// 换来的两枚 token 必须真能过对面的认证（不是"写进文件就算配对"），所以最后拿它们各调
/// 一次对端的受保护接口（`/shares`）。另外钉住两道边界：没人点「接收」之前谁都不许变成
/// "已配对"；自称已配对设备却不带 token 的邀请必须被拒。
#[tokio::test]
async fn pair_invite_swaps_tokens_without_typing_a_code() {
    let save_dir = std::env::temp_dir().join(format!("orbdrop_it_invite_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&save_dir).unwrap();
    // 上一次跑到中途失败会留下凭据（AppState::new 会按凭据库里的 id 合成一行：
    // 没有地址、离线）—— 先把这两台清干净，测试才不依赖上一次跑成什么样。
    credential::remove_peer("device-inv-a").ok();
    credential::remove_peer("device-inv-b").ok();
    let (pa, pb) = two_free_ports();
    let state_a = make_state(pa, "device-inv-a", "PC-A", save_dir.clone());
    let state_b = make_state(pb, "device-inv-b", "PC-B", save_dir.clone());
    let port_a = orbdrop_lib::server::start_server(state_a.clone(), pa).await.expect("A server");
    let port_b = orbdrop_lib::server::start_server(state_b.clone(), pb).await.expect("B server");

    // A 知道 B 的地址（mDNS 发现 / 手动添加），但两边**都还没配对** —— 这正是邀请要解决的场合。
    state_a.upsert_peer(&PeerDevice {
        id: "device-inv-b".into(),
        name: "PC-B".into(),
        ip: "127.0.0.1".into(),
        port: port_b,
        online: true,
        paired: false,
        auto_connect: false, paused: false,
    });

    // ---- A 点「发起邀请」----
    let invite_id = orbdrop_lib::pair::invite_peer(state_a.clone(), "device-inv-b")
        .await
        .expect("invite sent");
    wait_until("B 界面上出现这条邀请", || state_b.pair.has_invite(&invite_id)).await;
    assert!(
        !state_b.get_peer("device-inv-a").is_some_and(|p| p.paired),
        "还没人点「接收」，两边都不许变成已配对"
    );

    // ---- B 在通知里点「接收」----
    let name = orbdrop_lib::pair::accept_invite(&state_b, &invite_id).expect("accept invite");
    assert_eq!(name, "PC-A", "接受邀请返回的是发起方设备名");

    wait_until("A 落库这条配对", || {
        state_a.get_peer("device-inv-b").is_some_and(|p| p.paired)
    })
    .await;
    assert!(
        state_b.get_peer("device-inv-a").is_some_and(|p| p.paired),
        "B 也变「已配对」"
    );

    // 一对设备只有**一枚** token（由"收下配对"的那一侧铸出，两边各按对方的 id 存下来）——
    // 与配对码那条路同一个约定：A 手里那枚是 B 铸的，B 手里就是同一枚。
    let token_for_a = credential::get_peer_token("device-inv-b")
        .unwrap()
        .expect("A 存下 B 给的 token");
    let token_for_b = credential::get_peer_token("device-inv-a")
        .unwrap()
        .expect("B 存下 A 给的 token");
    assert_eq!(token_for_a.len(), 64, "32 字节 hex");
    assert_eq!(token_for_a, token_for_b, "一对设备共用一枚 token，两边都存下了它");

    let client = orbdrop_lib::net::client();
    let resp = client
        .get(format!("http://127.0.0.1:{port_b}/shares"))
        .bearer_auth(&token_for_a)
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "A 拿邀请换来的 token 必须能过 B 的认证");
    let resp = client
        .get(format!("http://127.0.0.1:{port_a}/shares"))
        .bearer_auth(&token_for_b)
        .send()
        .await
        .unwrap();
    assert!(resp.status().is_success(), "B 拿邀请换来的 token 必须能过 A 的认证");

    // ---- 冒充已配对设备：不带 token 的邀请必须被拒 ----
    // 没有这道关，谁都能自称你的老搭档；用户点一次「接收」，那台设备的 token 就被换成了
    // 对面手里的那枚（与 `/push/invite` 同一道关）。
    let forged = client
        .post(format!("http://127.0.0.1:{port_b}/pair/invite"))
        .json(&serde_json::json!({
            "inviteId": uuid::Uuid::new_v4().to_string(),
            "peerId": "device-inv-a",
            "peerName": "冒充的 PC-A",
            "port": port_a,
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(forged.status().as_u16(), 401, "自称已配对设备必须出示 token");

    // ---- 重新配对同一条路：这次 B 拒绝，已有的关系一个字节都不该被动 ----
    let invite2 = orbdrop_lib::pair::invite_peer(state_a.clone(), "device-inv-b")
        .await
        .expect("second invite");
    wait_until("B 界面上出现第二条邀请", || state_b.pair.has_invite(&invite2)).await;
    orbdrop_lib::pair::reject_invite(&state_b, &invite2);
    wait_until("A 收到拒绝、这条邀请收尾", || !state_a.pair.has_outbound(&invite2)).await;
    assert!(!state_b.pair.has_invite(&invite2), "拒绝后邀请从本机的表里摘掉");
    assert_eq!(
        credential::get_peer_token("device-inv-b").unwrap().as_deref(),
        Some(token_for_a.as_str()),
        "被拒的邀请不能改动已有的凭据"
    );

    // ---- 撤回（用户点「取消」）：对端界面上那条通知跟着撤掉 ----
    let invite3 = orbdrop_lib::pair::invite_peer(state_a.clone(), "device-inv-b")
        .await
        .expect("third invite");
    wait_until("B 界面上出现第三条邀请", || state_b.pair.has_invite(&invite3)).await;
    orbdrop_lib::pair::withdraw_invite_remote(state_a.clone(), &invite3)
        .await
        .expect("withdraw");
    wait_until("B 那条邀请被撤回后自己消失", || !state_b.pair.has_invite(&invite3)).await;

    credential::remove_peer("device-inv-a").ok();
    credential::remove_peer("device-inv-b").ok();
    let _ = std::fs::remove_dir_all(&save_dir);
    let _ = std::fs::remove_file(&state_a.config_path);
    let _ = std::fs::remove_file(&state_b.config_path);
}

/// 「离线」（隐身）把**一切**入站请求挡在门外，切回在线立刻恢复。
///
/// 这条是隐身能不能成立的关键：对端每 5 秒打一次 `/pair/info` 探活，只要还应答，它就把
/// 本机当"在线"（`state::probe_step` 认的是 200）——只注销 mDNS 瞒得住发现、瞒不住状态。
/// WS 升级（`/events`）同样在公开路由上，也必须在升级之前被拦下。
#[tokio::test]
async fn offline_refuses_every_inbound_request() {
    let save_dir = std::env::temp_dir().join(format!("orbdrop_it_off_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&save_dir).unwrap();
    let state = make_state(4190, "device-off", "PC-Off", save_dir.clone());
    let port = orbdrop_lib::server::start_server(state.clone(), 4190)
        .await
        .expect("server");
    let client = orbdrop_lib::net::client();
    let info = format!("http://127.0.0.1:{port}/pair/info");
    let events = format!("http://127.0.0.1:{port}/events?token=whatever");

    assert_eq!(client.get(&info).send().await.unwrap().status(), 200, "默认在线");

    state.set_local_online(false);
    assert_eq!(
        client.get(&info).send().await.unwrap().status(),
        503,
        "离线：探活不许再回 200，否则对端一直当本机在线"
    );
    assert_eq!(
        client.get(&events).send().await.unwrap().status(),
        503,
        "离线：WS 升级也要在升级之前被拦下"
    );
    assert_eq!(
        state.config.lock().online,
        false,
        "离线要落盘（跨重启有效）"
    );

    state.set_local_online(true);
    assert_eq!(client.get(&info).send().await.unwrap().status(), 200, "切回在线立刻恢复");

    let _ = std::fs::remove_dir_all(&save_dir);
    let _ = std::fs::remove_file(&state.config_path);
}

/// 跨网段发现（F-10 延伸）的服务端契约：已配对的对端来问「你这一段里在线的设备」，
/// 只回**它拨过来的那个地址所在网段**、且此刻在线的；呼叫方自己、本机自己、别网段的、
/// 离线的，一个都不回。
#[tokio::test]
async fn discover_reports_only_the_dialled_segment_and_needs_a_paired_token() {
    let Some(lan_ip) = local_lan_ip() else {
        eprintln!("跳过：本机没有局域网地址，模拟不出「来自另一台设备」的请求");
        return;
    };
    let save_dir = std::env::temp_dir().join(format!("orbdrop_it_disc_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&save_dir).unwrap();

    // 凭据库是**全局共享**的（一个数据目录一份），而测试是并行跑的：id 必须带 uuid，
    // 否则和别的测试互相清键，谁的 token 都活不到被用上那一刻。
    let uniq = uuid::Uuid::new_v4().to_string();
    let tag = &uniq[..8];
    let b_id = format!("disc-b-{tag}");
    let a_id = format!("disc-a-{tag}");
    let c_id = format!("disc-c-{tag}");
    let far_id = format!("disc-far-{tag}");
    let sleep_id = format!("disc-sleep-{tag}");

    let state_b = make_state(4390, &b_id, "PC-B", save_dir.clone());
    let add = |id: &str, ip: String, port: u16, online: bool, paired: bool| {
        state_b.add_peer_book(PeerDevice {
            id: id.into(),
            name: id.to_uppercase(),
            ip,
            port,
            online,
            paired,
            auto_connect: false, paused: false,
        });
    };
    // 本机这一段里在线的设备 —— 这是唯一该出现在表里的东西。
    add(&c_id, lan_ip.to_string(), 4391, true, false);
    // 另一段的设备（本机可能正是它自己内网的那扇门）：A 沿这条路根本到不了它。
    add(&far_id, "198.51.100.7".into(), 4392, true, false);
    // 呼叫方自己：表里不该回显它（A 已经知道自己是谁）。
    add(&a_id, lan_ip.to_string(), 4393, true, true);
    // 离线的同段设备：这一层只回答"此刻有谁在"。
    add(&sleep_id, lan_ip.to_string(), 4394, false, false);

    let port_b = orbdrop_lib::server::start_server(state_b.clone(), 4390)
        .await
        .expect("B server");

    let token = format!("tok-{}", uuid::Uuid::new_v4());
    // B 手里那条：键是 A 的 id —— 它据此才知道"带这个 token 的是 A"。
    credential::set_peer(&a_id, &token).unwrap();

    let client = orbdrop_lib::net::client();
    let url = format!("https://{lan_ip}:{port_b}/discover");

    // 没带 / 带错 token：拿不到表。这就是"匹配上之后才发现"那道门。
    assert_eq!(
        client.get(&url).send().await.unwrap().status(),
        reqwest::StatusCode::UNAUTHORIZED,
        "没配对不许看本机认识谁"
    );
    assert_eq!(
        client.get(&url).bearer_auth("not-a-token").send().await.unwrap().status(),
        reqwest::StatusCode::UNAUTHORIZED
    );

    let body: serde_json::Value = client
        .get(&url)
        .bearer_auth(&token)
        .send().await.unwrap()
        .json().await.unwrap();
    assert_eq!(body["id"], b_id.as_str());
    assert_eq!(
        body["port"].as_u64(),
        Some(u64::from(port_b)),
        "报的必须是实际监听的端口（配置端口被占时会顺延）"
    );

    let ids: Vec<&str> = body["peers"]
        .as_array()
        .expect("peers 数组")
        .iter()
        .filter_map(|p| p["id"].as_str())
        .collect();
    assert!(
        ids.contains(&c_id.as_str()),
        "同段在线的设备要在表里: {ids:?}"
    );
    assert!(!ids.contains(&far_id.as_str()), "别的网段的不许回: {ids:?}");
    assert!(!ids.contains(&a_id.as_str()), "呼叫方自己不许回: {ids:?}");
    assert!(
        !ids.contains(&sleep_id.as_str()),
        "同段但离线的不回: {ids:?}"
    );
    assert!(!ids.contains(&b_id.as_str()), "本机自己不许回: {ids:?}");

    orbdrop_lib::server::stop_server(&state_b).await;
    credential::remove_peer(&a_id).ok();
    let _ = std::fs::remove_dir_all(&save_dir);
}

/// 探活那条 5 秒的循环顺手把「对端那一段在线的设备」学过来：学来的行是临时的
/// （不写地址簿、不被再探活），而且**存活跟着担保人走** —— 担保人不再提到它，超时收走。
#[tokio::test]
async fn probe_learns_the_peers_segment_and_sweeps_it_when_the_voucher_goes_quiet() {
    let Some(lan_ip) = local_lan_ip() else {
        eprintln!("跳过：本机没有局域网地址，模拟不出「来自另一台设备」的请求");
        return;
    };
    let save_dir = std::env::temp_dir().join(format!("orbdrop_it_learn_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&save_dir).unwrap();

    // 凭据库全局共享、测试并行跑：id 带 uuid，别和别的测试互相清键（同上）。
    let uniq = uuid::Uuid::new_v4().to_string();
    let tag = &uniq[..8];
    let b_id = format!("learn-b-{tag}");
    let a_id = format!("learn-a-{tag}");
    let c_id = format!("learn-c-{tag}");

    let state_b = make_state(4395, &b_id, "PC-B", save_dir.clone());
    state_b.add_peer_book(PeerDevice {
        id: c_id.clone(),
        name: "PC-C".into(),
        ip: lan_ip.to_string(),
        port: 4396,
        online: true,
        paired: false,
        auto_connect: false, paused: false,
    });
    let port_b = orbdrop_lib::server::start_server(state_b.clone(), 4395)
        .await
        .expect("B server");

    let state_a = make_state(4397, &a_id, "PC-A", save_dir.clone());
    let token = format!("tok-{}", uuid::Uuid::new_v4());
    credential::set_peer(&a_id, &token).unwrap(); // B 认这个 token 是 A 的
    credential::set_peer(&b_id, &token).unwrap(); // A 手里有 B 的 token
    state_a.add_peer_book(PeerDevice {
        id: b_id.clone(),
        name: "PC-B".into(),
        ip: lan_ip.to_string(),
        port: port_b,
        online: true,
        paired: true,
        auto_connect: false, paused: false,
    });

    orbdrop_lib::state::probe_peers_once(&state_a).await;

    let learned = state_a.get_peer(&c_id).expect("A 学到了 B 那一段的设备");
    assert!(learned.online, "担保人说它在线，就是在线");
    assert!(!learned.paired, "学来的设备跟本机没有任何关系");
    assert!(state_a.is_learned(&c_id));

    assert!(
        !state_a.config.lock().peers.iter().any(|p| p.id == c_id),
        "学来的条目是临时的：不许写进地址簿"
    );

    // 担保人不再报它了。本机**不会**去拨它 —— 那不是本机发现的关系，多拨一次就是多余
    // 的一跳；所以它不该被探活判成离线，而是继续按"担保人最近说过它在"活着。
    state_b.config.lock().peers.retain(|p| p.id != c_id);
    state_b.peers.lock().retain(|p| p.id != c_id);
    orbdrop_lib::state::probe_peers_once(&state_a).await;
    assert!(
        state_a.get_peer(&c_id).map(|p| p.online).unwrap_or(false),
        "学来的行不参与探活：担保人没再提它也仍是在线（只有超时才收走）"
    );

    // 担保人不再提到它（超时）→ 收走。把时间戳拨回去模拟"20 秒没人再担保"。
    state_a
        .learned
        .lock()
        .insert(c_id.clone(), Instant::now() - Duration::from_secs(600));
    state_a.sweep_learned();
    assert!(
        state_a.get_peer(&c_id).is_none(),
        "没人再担保的行必须从列表里消失"
    );
    assert!(!state_a.is_learned(&c_id));

    orbdrop_lib::server::stop_server(&state_b).await;
    credential::remove_peer(&a_id).ok();
    credential::remove_peer(&b_id).ok();
    let _ = std::fs::remove_dir_all(&save_dir);
}

// ---------------------------------------------------------------------------
// 会话（聊天）：文本消息与已读回执走的就是那条长连接
// ---------------------------------------------------------------------------

/// 一台设备的两个"家"：config.toml 与会话记录（chat.json）都落在它自己的目录里。
///
/// 会话记录跟 config.toml 走（见 `AppState::new`），所以两台设备必须各占一个目录 ——
/// 共用一个的话，两边会把同一份 chat.json 互相覆盖（真实部署里这就是两台机器）。
fn chat_state(
    port: u16,
    device_id: &str,
    device_name: &str,
    root: &std::path::Path,
) -> (Arc<AppState>, std::path::PathBuf) {
    let dir = root.join(device_id);
    std::fs::create_dir_all(&dir).unwrap();
    let mut config = Config::default();
    config.port = port;
    config.device_id = device_id.to_string();
    config.device_name = device_name.to_string();
    config.download_dir = dir.clone();
    config.bc_exe_path = Some(hermetic_bc_exe());
    let config_path = dir.join("config.toml");
    (Arc::new(AppState::new(config, config_path)), dir)
}

#[tokio::test]
async fn chat_messages_and_read_receipts_travel_over_the_long_connection() {
    use orbdrop_lib::chat::{self, Delivery};

    let root = std::env::temp_dir().join(format!("orbdrop_it_chat_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&root).unwrap();

    let (port_a, port_b) = two_free_ports();
    let (state_a, dir_a) = chat_state(port_a, "device-chat-a", "PC-CA", &root);
    let (state_b, dir_b) = chat_state(port_b, "device-chat-b", "PC-CB", &root);

    let server_a = orbdrop_lib::server::start_server(state_a.clone(), port_a).await.expect("A server");
    let server_b = orbdrop_lib::server::start_server(state_b.clone(), port_b).await.expect("B server");

    // ---- 配对结果（两侧各存一枚 token）+ 用户点「连接」 ----
    credential::remove_peer("device-chat-a").ok();
    credential::remove_peer("device-chat-b").ok();
    let token = format!("t-chat-{}", &uuid::Uuid::new_v4().simple().to_string()[..8]);
    credential::set_peer("device-chat-a", &token).unwrap();
    credential::set_peer("device-chat-b", &token).unwrap();
    state_a.mark_paired(&PeerDevice {
        id: "device-chat-b".into(), name: "PC-CB".into(), ip: "127.0.0.1".into(), port: port_b,
        online: true, paired: true, auto_connect: false, paused: false,
    });
    state_b.mark_paired(&PeerDevice {
        id: "device-chat-a".into(), name: "PC-CA".into(), ip: "127.0.0.1".into(), port: port_a,
        online: true, paired: true, auto_connect: false, paused: false,
    });
    orbdrop_lib::ws::ensure_link_now(&state_a, "device-chat-b");
    wait_until("长连接建立", || {
        state_a.ws.has_connection("device-chat-b") && state_b.ws.has_connection("device-chat-a")
    })
    .await;

    // ---- A 发一条文本 ----
    let text = "在吗？帮我传一下「Q3 排期表」。";
    let (snap, sent_id) = chat::send_text(&state_a, &["device-chat-b".to_string()], text);
    assert!(!sent_id.is_empty(), "发出去的消息带回它的 id（界面据此逐台读状态）");
    let convo = snap.convos.iter().find(|c| c.peer_id == "device-chat-b").expect("A 侧有一条会话");
    assert_eq!(convo.messages.len(), 1);
    let sent = &convo.messages[0];
    assert!(sent.from_me);
    assert_eq!(sent.text, text);
    assert_eq!(sent.targets, vec!["device-chat-b".to_string()]);
    assert_eq!(sent.delivery, Delivery::Sent, "写进长连接就算送达");
    assert_eq!(snap.unread, 0, "自己发的不算未读");

    // ---- B 收到（未读 +1），发件人名字取自**B 自己的设备表** ----
    wait_until("B 收到那条消息", || state_b.chat.inbound_watermark("device-chat-a") > 0).await;
    let snap_b = state_b.chat.snapshot();
    assert_eq!(snap_b.unread, 1);
    let convo_b = snap_b.convos.iter().find(|c| c.peer_id == "device-chat-a").expect("B 侧有一条会话");
    let got = &convo_b.messages[0];
    assert!(!got.from_me);
    assert_eq!(got.text, text);
    assert_eq!(got.from_name, "PC-CA", "发件人名字以本机设备表为准");
    assert_eq!(got.delivery, Delivery::Received);

    // ---- B 打开会话 → 本地未读清零 + 回执回去 → A 那条变「已读」 ----
    chat::mark_read(&state_b, &["device-chat-a".to_string()]);
    assert_eq!(state_b.chat.snapshot().unread, 0);
    wait_until("A 看到「已读」", || {
        state_a
            .chat
            .snapshot()
            .convos
            .iter()
            .find(|c| c.peer_id == "device-chat-b")
            .and_then(|c| c.messages.first())
            .is_some_and(|m| m.delivery == Delivery::Read)
    })
    .await;

    // ---- 对端断开：消息**不排队**，就地落一条未送达（下次连上也照旧） ----
    state_b.ws.pause("device-chat-a");
    wait_until("B 侧链路断开", || !state_a.ws.has_connection("device-chat-b")).await;
    let (snap, _) = chat::send_text(&state_a, &["device-chat-b".to_string()], "这条送不出去");
    let convo = snap.convos.iter().find(|c| c.peer_id == "device-chat-b").unwrap();
    assert_eq!(convo.messages.last().unwrap().delivery, Delivery::Failed);

    // ---- 群发：每台各一份（同一个 id），**没有群实体** —— 世界里只多出"与每台的两条会话" ----
    // 一台有链路、一台（没配对过的 id）根本没有会话：逐台成败各算各的。
    let (snap, fan_id) = chat::send_text(
        &state_a,
        &["device-chat-b".to_string(), "device-chat-ghost".to_string()],
        "群发一句",
    );
    let fan_b = snap.convos.iter().find(|c| c.peer_id == "device-chat-b").unwrap();
    let fan_ghost = snap.convos.iter().find(|c| c.peer_id == "device-chat-ghost").expect("另一台也有自己那条");
    assert_eq!(fan_b.messages.last().unwrap().id, fan_id);
    assert_eq!(fan_ghost.messages.last().unwrap().id, fan_id, "同一个 id：逐台状态靠它拼");
    assert_eq!(
        fan_b.messages.last().unwrap().delivery,
        Delivery::Failed,
        "链路断着，这条也没送出去"
    );
    assert_eq!(fan_ghost.messages.last().unwrap().delivery, Delivery::Failed);
    assert_eq!(snap.convos.len(), 2, "群发不产生第三条「群会话」，就是两台各一条");

    // ---- 两侧都落了盘：重开一份 store 还能读到（会话记录是用户数据） ----
    let reopened_a = chat::ChatStore::with_path(dir_a.join("chat.json"));
    assert_eq!(reopened_a.snapshot().convos[0].messages.len(), 3);
    let reopened_b = chat::ChatStore::with_path(dir_b.join("chat.json"));
    let kept = &reopened_b.snapshot().convos[0].messages[0];
    assert_eq!(kept.text, text);
    assert_eq!(reopened_b.read_watermark("device-chat-a"), kept.at, "已读水位也落盘");

    orbdrop_lib::server::stop_server(&state_a).await;
    orbdrop_lib::server::stop_server(&state_b).await;
    credential::remove_peer("device-chat-a").ok();
    credential::remove_peer("device-chat-b").ok();
    let _ = std::fs::remove_dir_all(&root);
    let _ = (server_a, server_b);   // 端口号不是句柄：服务器由 `stop_server` 收
}
