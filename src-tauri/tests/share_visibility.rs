//! 共享项可见性（F-22 / F-25）：对端设备标签里到底能看到什么。
//!
//! 规则只有一条，本机和三个受保护端点都走它：
//!   可见 = 该共享项开着广播 && 该对端**此刻**处于长连接。
//!
//! 测试直接调用 handler 并注入 `Caller`：不依赖 keyring，也不用从非回环地址发请求，
//! 测的就是规则本身；回环路径另用真实 HTTP 覆盖（本机必须永远全量可见）。

use std::collections::HashMap;
use std::sync::Arc;

use axum::extract::{Extension, Query, State};
use axum::response::Json;
use orbdrop_lib::config::{Config, ShareItem};
use orbdrop_lib::credential;
use orbdrop_lib::server::{file_handler, shares_handler, tree_handler, Caller};
use orbdrop_lib::state::AppState;

fn temp_share_dir() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("orbdrop_vis_{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("hello.txt"), b"hello world").unwrap();
    dir
}

/// 一个共享项、开播开关关着；对端 `peer-a` 有一条（假的）长连接。
fn make_state(root: std::path::PathBuf, peer_connected: bool) -> Arc<AppState> {
    let mut config = Config::default();
    config.shares.push(ShareItem::new(root));
    let config_path = std::env::temp_dir().join(format!("orbdrop_vis_cfg_{}.toml", uuid::Uuid::new_v4()));
    let state = Arc::new(AppState::new(config, config_path));
    if peer_connected {
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        state.ws.register("peer-a", tx);
    }
    state
}

fn peer_a() -> Caller {
    Caller::Peer("peer-a".to_string())
}

fn tree_params(share_id: &str) -> Query<HashMap<String, String>> {
    let mut params = HashMap::new();
    params.insert("share".to_string(), share_id.to_string());
    params.insert("path".to_string(), String::new());
    Query(params)
}

fn file_params(share_id: &str) -> Query<HashMap<String, String>> {
    let mut params = HashMap::new();
    params.insert("share".to_string(), share_id.to_string());
    params.insert("path".to_string(), "hello.txt".to_string());
    Query(params)
}

async fn shares_seen_by(state: &Arc<AppState>, caller: Caller) -> Vec<serde_json::Value> {
    let Json(value) = shares_handler(State(state.clone()), Extension(caller)).await;
    value["shares"].as_array().cloned().unwrap_or_default()
}

#[tokio::test]
async fn peer_sees_only_broadcast_shares_while_connected() {
    let root = temp_share_dir();
    let state = make_state(root.clone(), true);
    let share_id = state.config.lock().shares[0].id.clone();

    // 未开播：长连接在也不可见
    assert!(shares_seen_by(&state, peer_a()).await.is_empty());

    // 开播：可见，且只带对端用得上的字段
    state.config.lock().shares[0].broadcast_enabled = true;
    let seen = shares_seen_by(&state, peer_a()).await;
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0]["id"], share_id.as_str());
    assert_eq!(seen[0]["isDir"], true, "目录共享项要带 isDir，对端才知道能不能展开");
    assert!(seen[0].get("path").is_none(), "本机绝对路径不该发给对端");

    // 开播，但对方此刻没有长连接：看不见
    assert!(shares_seen_by(&state, Caller::Peer("peer-b".into())).await.is_empty());

    // 本机（回环）永远全量
    assert_eq!(shares_seen_by(&state, Caller::Local).await.len(), 1);

    // 断开长连接 → 可见性立刻消失，不需要谁去回填目标列表（F-25）
    state.ws.drop_peer("peer-a");
    assert!(shares_seen_by(&state, peer_a()).await.is_empty());

    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn peer_cannot_read_an_unbroadcast_share() {
    let root = temp_share_dir();
    let state = make_state(root.clone(), true);
    let share_id = state.config.lock().shares[0].id.clone();

    // 长连接但没开播：/tree、/file 都不可读
    let denied = tree_handler(State(state.clone()), Extension(peer_a()), tree_params(&share_id)).await;
    assert_eq!(denied.unwrap_err().0, axum::http::StatusCode::NOT_FOUND);
    let denied = file_handler(State(state.clone()), Extension(peer_a()), file_params(&share_id)).await;
    let status = match denied {
        Err((status, _)) => status,
        Ok(_) => panic!("没开播就不该读到文件"),
    };
    assert_eq!(status, axum::http::StatusCode::NOT_FOUND);

    // 开播后能列目录、能读文件
    state.config.lock().shares[0].broadcast_enabled = true;
    let listed = tree_handler(State(state.clone()), Extension(peer_a()), tree_params(&share_id)).await;
    assert!(listed.is_ok(), "开播且长连接时对端应当能列出目录");
    let read = file_handler(State(state.clone()), Extension(peer_a()), file_params(&share_id)).await;
    assert!(read.is_ok(), "开播且长连接时对端应当能读文件");

    // 再关播 → 立刻又不可读（关播撤销可见性，F-22）
    state.config.lock().shares[0].broadcast_enabled = false;
    let denied = tree_handler(State(state.clone()), Extension(peer_a()), tree_params(&share_id)).await;
    assert_eq!(denied.unwrap_err().0, axum::http::StatusCode::NOT_FOUND);

    // 本机自己的共享项不受开播开关影响
    let listed = tree_handler(State(state.clone()), Extension(Caller::Local), tree_params(&share_id)).await;
    assert!(listed.is_ok());

    let _ = std::fs::remove_dir_all(root);
}

/// 未配对的来源不得读写：`Caller` 只由 token 反解而来，没有身份就没有可见范围。
#[tokio::test]
async fn unpaired_source_is_rejected() {
    let root = temp_share_dir();
    let state = make_state(root.clone(), false);
    let port = orbdrop_lib::server::start_server(state.clone(), 4050)
        .await
        .expect("server start");
    state.config.lock().shares[0].broadcast_enabled = true;

    // 非回环来源（本机 LAN 地址）不给 token：401
    let lan_ip = local_lan_ip().expect("需要一张可用网卡来模拟外部来源");
    let url = format!("http://{lan_ip}:{port}/shares");
    let resp = reqwest::get(&url).await.expect("request reaches server");
    assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);

    // 回环（本机 UI）仍然畅通
    let resp = reqwest::get(format!("http://127.0.0.1:{port}/shares")).await.unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);

    // 已被删掉的 peer 可改共享项权限的端点不该复活
    let resp = reqwest::Client::new()
        .post(format!("http://127.0.0.1:{port}/share/permissions"))
        .json(&serde_json::json!({ "shareId": "x", "targets": ["peer-a"] }))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);

    orbdrop_lib::server::stop_server(&state).await;
    let _ = std::fs::remove_dir_all(root);
}

/// 真实 HTTP + 真实中间件：token 反解出对端身份 → 可见性按「谁在看」生效。
///
/// 这是「另一台设备请求本机」的最小可复现版本（同机用局域网地址当非回环来源）。
/// 用真 keyring（与 p1_integration 同款做法），固定假设备 id 并在前后各清一次，
/// 避免上一次失败残留把别的测试带红。
#[tokio::test]
async fn peer_identity_from_token_scopes_the_share_list() {
    const PEER_ID: &str = "orbdrop-test-peer-visibility";
    const TOKEN: &str = "orbdrop-test-token-visibility";
    credential::remove_peer(PEER_ID).ok();

    let shared = temp_share_dir();
    let hidden = temp_share_dir();
    let state = make_state(shared.clone(), false);
    {
        let mut config = state.config.lock();
        config.shares[0].broadcast_enabled = true;                 // 开播
        config.shares.push(ShareItem::new(hidden.clone()));        // 未开播
    }
    let hidden_id = state.config.lock().shares[1].id.clone();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    state.ws.register(PEER_ID, tx);                                // 对方处于长连接
    orbdrop_lib::credential::set_peer(PEER_ID, TOKEN).unwrap();

    let port = orbdrop_lib::server::start_server(state.clone(), 4060)
        .await
        .expect("server start");
    let ip = local_lan_ip().expect("需要一张可用网卡来模拟外部来源");
    let client = reqwest::Client::new();

    // 带 token：只看到开播的那一个，且不含本机绝对路径
    let resp = client
        .get(format!("http://{ip}:{port}/shares"))
        .bearer_auth(TOKEN)
        .send().await.unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::OK);
    let json: serde_json::Value = resp.json().await.unwrap();
    let items = json["shares"].as_array().unwrap();
    assert_eq!(items.len(), 1, "只应看到对自己开播的共享项");
    assert!(items[0].get("path").is_none(), "本机绝对路径不下发");

    // 未开播的共享项：握着有效 token 也读不到
    let resp = client
        .get(format!("http://{ip}:{port}/tree?share={hidden_id}&path="))
        .bearer_auth(TOKEN)
        .send().await.unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::NOT_FOUND);

    // 不带 token：连列表都拿不到
    let resp = client.get(format!("http://{ip}:{port}/shares")).send().await.unwrap();
    assert_eq!(resp.status(), reqwest::StatusCode::UNAUTHORIZED);

    // 对方断开长连接：可见性立刻消失
    state.ws.drop_peer(PEER_ID);
    let resp = client
        .get(format!("http://{ip}:{port}/shares"))
        .bearer_auth(TOKEN)
        .send().await.unwrap();
    let json: serde_json::Value = resp.json().await.unwrap();
    assert!(json["shares"].as_array().unwrap().is_empty());

    orbdrop_lib::server::stop_server(&state).await;
    orbdrop_lib::credential::remove_peer(PEER_ID).ok();
    let _ = std::fs::remove_dir_all(shared);
    let _ = std::fs::remove_dir_all(hidden);
}

/// 本机在局域网上的地址：用一次不发包的 UDP connect 让内核挑出出口网卡。
fn local_lan_ip() -> Option<std::net::IpAddr> {
    let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
    socket.connect("192.0.2.1:9").ok()?;
    let ip = socket.local_addr().ok()?.ip();
    if ip.is_loopback() {
        None
    } else {
        Some(ip)
    }
}