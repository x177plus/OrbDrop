//! F-16 的**同意制**（`crate::link`）集成测试：真 HTTP + 真 TLS + 真 WS，不是 mock。
//!
//! 这一条要钉住的是"谁说了算"：本机点「连接」不再自己拨号 —— 请求送到对端、挂在对端的
//! 通知里等它点头，点头之后**才**建链；拒绝/撤回就到此为止。所以三个用例分别对应三种结局：
//!
//! 1. 没有凭据的人发来的请求一律 401（公开面上的第一道关）；
//! 2. 对方**接受** → 本机拨号、链路真的建起来（连"对端点过「断开」"这一档也验证：接受
//!    就是那次暂停的撤销）；
//! 3. 对方**拒绝** → 本机不拨号，设备行停在「对方拒绝了连接请求」；
//! 4. 本机**取消** → 对方那条通知当场作废，谁都不拨。
//!
//! 走真 HTTPS 的取舍与 `p1_integration` / `relay_memory` 一致：token 是两侧共用的一枚
//! （真实配对换来的就是这一枚），地址簿里互相标 `paired`。

use std::sync::Arc;
use std::time::{Duration, Instant};

use orbdrop_lib::config::{Config, PeerDevice};
use orbdrop_lib::state::AppState;
use orbdrop_lib::{credential, link, server};

fn isolate_data_dir() {
    static INIT: std::sync::LazyLock<()> = std::sync::LazyLock::new(|| {
        let dir = std::env::temp_dir().join(format!("orbdrop_it_link_{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("建临时数据目录");
        std::env::set_var("ORBDROP_DATA_DIR", &dir);
    });
    std::sync::LazyLock::force(&INIT);
}

fn make_state(port: u16, device_id: &str, device_name: &str) -> Arc<AppState> {
    isolate_data_dir();
    let dir = std::env::temp_dir().join(format!("orbdrop_link_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut config = Config::default();
    config.port = port;
    config.device_id = device_id.to_string();
    config.device_name = device_name.to_string();
    config.download_dir = dir.clone();
    config.bc_exe_path = None;
    Arc::new(AppState::new(config, dir.join("config.toml")))
}

fn two_free_ports() -> (u16, u16) {
    let a = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let b = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    (
        a.local_addr().unwrap().port(),
        b.local_addr().unwrap().port(),
    )
}

async fn wait_until<F: Fn() -> bool>(what: &str, ok: F) {
    let deadline = Instant::now() + Duration::from_secs(20);
    while Instant::now() < deadline {
        if ok() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("等超时：{what}");
}

fn peer(id: &str, name: &str, ip: &str, port: u16) -> PeerDevice {
    PeerDevice {
        id: id.to_string(),
        name: name.to_string(),
        ip: ip.to_string(),
        port,
        online: true,
        paired: true,
        auto_connect: false,
        paused: false,
        relay: None,
    }
}

/// 两边互相已配对（同一枚 token），互指地址簿，服务器都起好。
async fn paired_pair() -> (Arc<AppState>, Arc<AppState>) {
    let (port_a, port_b) = two_free_ports();
    let state_a = make_state(port_a, "device-link-a", "PC-LINK-A");
    let state_b = make_state(port_b, "device-link-b", "PC-LINK-B");
    let port_a = server::start_server_loopback(state_a.clone(), port_a)
        .await
        .expect("A server");
    let port_b = server::start_server_loopback(state_b.clone(), port_b)
        .await
        .expect("B server");

    const TOKEN: &str = "0f0f0f0f1e1e1e1e2d2d2d2d3c3c3c3c";
    credential::set_peer("device-link-a", TOKEN).unwrap();
    credential::set_peer("device-link-b", TOKEN).unwrap();
    state_a.add_peer_book(peer("device-link-b", "PC-LINK-B", "127.0.0.1", port_b));
    state_b.add_peer_book(peer("device-link-a", "PC-LINK-A", "127.0.0.1", port_a));
    assert!(
        !state_a.ws.has_connection("device-link-b"),
        "前提：还没有长连接"
    );
    (state_a, state_b)
}

/// 公开面上的第一道关：**没有凭据的请求一律 401**（拒绝的是"谁都能来敲一扇门"）。
#[tokio::test]
async fn a_link_request_without_a_valid_token_is_refused() {
    let (_a, b) = paired_pair().await;
    let port_b = b.listening_port();
    let url = format!("http://127.0.0.1:{port_b}/link/request");
    let body = serde_json::json!({ "requestId": "r-1", "peerName": "NOPE" });

    let no_token = reqwest::Client::new()
        .post(&url)
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(
        no_token.status(),
        reqwest::StatusCode::UNAUTHORIZED,
        "不带 token"
    );
    let bogus = reqwest::Client::new()
        .post(&url)
        .bearer_auth("not-a-real-token")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(
        bogus.status(),
        reqwest::StatusCode::UNAUTHORIZED,
        "token 不认"
    );
    assert!(!b.link.has_incoming("r-1"), "被拒的请求不该挂到界面上");
    server::stop_server(&b).await;
}

/// 对方点「接受」→ 本机拨号、链路建起来。
///
/// 同一用例还验证最难的那一步：**对端点过「断开」时，接受就是那次暂停的撤销** ——
/// 少了它，"我同意了它却还被 403 挡着"就是一条说不通的死路。
#[tokio::test]
async fn an_accepted_request_ends_the_pause_and_the_link_comes_up() {
    let (a, b) = paired_pair().await;

    // 对端（B）此刻正暂停着本机（A 之前点过「断开」）：这一次接受必须把它撤掉。
    b.ws.pause("device-link-a");
    b.set_link_intent("device-link-a", orbdrop_lib::state::LinkIntent::Paused);
    assert!(b.ws.is_paused("device-link-a"), "前提：对端在暂停本机");

    let request_id = link::request(&a, "device-link-b").expect("发起请求");
    wait_until("请求挂到对端界面上", || {
        b.link.has_incoming(&request_id)
    })
    .await;

    let name = link::accept(&b, &request_id).expect("接受");
    assert_eq!(name, "PC-LINK-A", "回的是本机在对端眼里的名字");
    assert!(!b.ws.is_paused("device-link-a"), "接受即撤销那次暂停");
    assert!(!b.link.has_incoming(&request_id), "处理过的请求不再挂着");

    // 请求的回执（200）一到，发起方就拨号 —— 两侧的长连接真的建起来。
    wait_until("发起方建起长连接", || {
        a.ws.has_connection("device-link-b")
    })
    .await;
    wait_until("对端也认这条链路", || {
        b.ws.has_connection("device-link-a")
    })
    .await;
    assert!(
        !a.link.has_outgoing_for("device-link-b"),
        "结果已收，外发表清干净"
    );

    server::stop_server(&a).await;
    server::stop_server(&b).await;
}

/// 对方点「拒绝」→ 本机**不拨号**，设备行停在"对方拒绝了连接请求"。
#[tokio::test]
async fn a_rejected_request_leaves_the_link_down_and_says_why() {
    let (a, b) = paired_pair().await;

    let request_id = link::request(&a, "device-link-b").expect("发起请求");
    wait_until("请求挂到对端界面上", || {
        b.link.has_incoming(&request_id)
    })
    .await;
    link::reject(&b, &request_id);

    // 本机把回执（403）落成"停手"：理由必须是"对方拒绝"，而不是网络那类可重试的说法。
    wait_until("本机停在对方拒绝这一档", || {
        matches!(
            a.ws.link_state("device-link-b"),
            Some(orbdrop_lib::ws::LinkState::Stopped {
                reason: orbdrop_lib::ws::StopReason::PeerRejected
            })
        )
    })
    .await;
    assert!(!a.ws.has_connection("device-link-b"), "拒绝就是没有链路");
    assert!(!a.link.has_outgoing_for("device-link-b"), "外发表清干净");

    server::stop_server(&a).await;
    server::stop_server(&b).await;
}

/// 本机点「取消」→ 对方那条通知当场作废（`POST /link/request/withdraw`），谁都不拨。
#[tokio::test]
async fn cancelling_the_request_closes_the_peers_notification() {
    let (a, b) = paired_pair().await;

    let request_id = link::request(&a, "device-link-b").expect("发起请求");
    wait_until("请求挂到对端界面上", || {
        b.link.has_incoming(&request_id)
    })
    .await;
    link::cancel(&a, &request_id).expect("取消");

    wait_until("对端那条通知被撤掉", || {
        !b.link.has_incoming(&request_id)
    })
    .await;
    // 取消之后本机不再等这次结果：不给它拨号，也不该留下"对方拒绝"的停手理由
    // （那是对方的决定，而这次是本机自己收的手）。
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(!a.ws.has_connection("device-link-b"), "取消就是没有链路");
    assert!(!a.link.has_outgoing_for("device-link-b"), "外发表清干净");

    server::stop_server(&a).await;
    server::stop_server(&b).await;
}
