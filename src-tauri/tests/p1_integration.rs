//! P1 集成测试：双实例 mDNS 无关（走 HTTP/WS 127.0.0.1）配对 + 推送 + 落盘。
//!
//! 覆盖校验清单：
//! - 配对：A 发起（本机生成 6 位码并显示）→ B 输入该码 → B 向 A 换取 token（F-12）
//! - 配对即自动建链：id 较小的一方自行拨号，无需用户点击"连接"（F-16）
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

    // 4. Pairing already started the link (A initiates: device-a < device-b).
    // Wait for the WS session to register on both sides.
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
    // "device-za" < "device-zb"：本机是"该拨号的一方"，必然走到 spawn 那一步。
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
    orbdrop_lib::ws::ensure_link(&state, "device-zb");
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

/// F-16 回归：配对后长连接由 id 较小的一方自行建立，用户不需要点"连接"。
///
/// 这里让**验证方**（本机生成码、被 POST /pair 的那台）就是 id 较小的一方，
/// 且它从未通过 mDNS/手动添加见过对方 —— 也就是对方地址唯一来源是这次配对请求。
/// 修复前它拿着空地址拨号（`ws://:0`），长连接永远建不起来，界面上只能靠用户点
/// "建立长连接"；修复后配对请求带回来源 IP + 对端自报端口，配对完成即自动建链。
#[tokio::test]
async fn pairing_arms_long_connection_without_a_click() {
    // 凭据库是机器级的，进程结束也不会清：先抹掉上一轮（尤其是跑挂的那轮）留下的
    // token，否则 A 一启动就从凭据库重建出 B 的条目，"从未见过 B" 这个前提不成立。
    credential::remove_peer("device-aa").ok();
    credential::remove_peer("device-ab").ok();

    let inbox = std::env::temp_dir().join(format!("orbdrop_auto_inbox_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&inbox).unwrap();

    // "device-aa" < "device-ab" → A 才是拨号方（确定性规则）。
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

    // A 从配对请求里学到了 B 的地址（来源 IP + B 自报端口）并落库。
    let learned = state_a.get_peer("device-ab").expect("A now knows B");
    assert!(learned.paired, "A marks B paired");
    assert_eq!((learned.ip.as_str(), learned.port), ("127.0.0.1", port_b));

    // 关键断言：双方都没有调用任何"连接"命令，长连接却必须自己起来。
    wait_until("pairing must arm the link by itself", || {
        state_a.ws.has_connection("device-ab") && state_b.ws.has_connection("device-aa")
    }).await;

    // ---- 下面三条是用户实际报的故障（F-16 的两个按钮）----

    // 1) 被拨号的那一台（B，A 拨的号）点"断开"：双方都必须掉线。
    //    修复前只有本机认为断了，对端一直显示"长连接"。
    state_b.ws.pause("device-aa");
    wait_until("断开 must drop the peer's session too", || {
        !state_a.ws.has_connection("device-ab") && !state_b.ws.has_connection("device-aa")
    }).await;
    assert!(state_b.ws.is_paused("device-aa"));

    // 2) 断开方自己点"连接"：它并不是"id 较小的一方"，但显式请求必须能连回来。
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
