use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{Query, State, WebSocketUpgrade};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Router;
use tokio::net::TcpListener;
use tokio::sync::oneshot;
use tower_http::cors::{Any, CorsLayer};
use tracing::{error, info};

use crate::state::AppState;
use crate::ws;

mod auth;
mod routes_dav;
mod routes_file;
mod routes_pair;
mod routes_push;
mod routes_shares;
mod routes_stat;
mod routes_tree;

pub use auth::Caller;
pub use routes_file::file_handler;
pub use routes_shares::shares_handler;
pub use routes_stat::stat_handler;
pub use routes_tree::tree_handler;

/// Build the axum Router with all routes, including the WS /events
/// endpoint (outside the auth layer).
fn build_router(state: Arc<AppState>) -> Router {
    let cors = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    // Public routes (no auth): pairing, info, WS upgrade.
    let public_routes = Router::new()
        .route("/pair/info", axum::routing::get(routes_pair::pair_info))
        .route("/pair", axum::routing::post(routes_pair::pair_handshake))
        .route(
            "/events",
            axum::routing::get(events_ws_handler),
        );

    // Authenticated routes (auth middleware).
    let auth_routes = Router::new()
        .route("/tree", axum::routing::get(routes_tree::tree_handler))
        .route("/stat", axum::routing::get(routes_stat::stat_handler))
        .route("/unpair", axum::routing::post(routes_pair::unpair))
        .route("/file", axum::routing::get(routes_file::file_handler))
        .route("/shares", axum::routing::get(routes_shares::shares_handler))
        .route(
            "/push/download",
            axum::routing::get(routes_push::push_download_handler),
        )
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::auth_middleware,
        ));

    let http_routes = Router::new()
        .merge(public_routes)
        .merge(auth_routes)
        .layer(cors);

    // WebDAV 面（F-41 ~ F-43）：Beyond Compare 直连共享项。
    // **不能挂 CORS 层**：tower-http 的 CORS 会把 OPTIONS 直接抢答（返回它自己的 preflight
    // 响应），而 BC 正是用 `OPTIONS` 协商 DAV 能力（要看到 `DAV: 1,2`）——被抢答就等于
    // "这不是 WebDAV"，远端目录在 BC 里直接加载不了（实测过）。
    // 路径命名空间用 shareId：共享项名字带空格/中文且会和上面的 REST 路由抢前缀。
    let dav_routes = Router::new()
        .route(
            "/dav/{share_id}",
            axum::routing::any(routes_dav::dav_share_root),
        )
        // BC 对目录用带尾斜杠的地址（实测 `PROPFIND /dav/sub/`），根目录同理：
        // `/dav/<shareId>` 与 `/dav/<shareId>/` 都得能路由，否则目录级请求直接 404。
        .route(
            "/dav/{share_id}/",
            axum::routing::any(routes_dav::dav_share_root),
        )
        .route(
            "/dav/{share_id}/{*path}",
            axum::routing::any(routes_dav::dav_share_sub),
        )
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::auth_middleware,
        ));

    http_routes.merge(dav_routes).with_state(state)
}

/// WS upgrade handler — validates the token in the query string before
/// accepting the upgrade.
///
/// `manual=1` marks a dial the peer's user asked for ("连接" button). Our own
/// "断开" is a *pause*: it drops the link and turns away the peer's automatic
/// reconnects (otherwise the peer's dial loop would undo the click a second
/// later), but a human on the other side asking to connect is honoured — and it
/// ends our pause.
async fn events_ws_handler(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
    ws: WebSocketUpgrade,
) -> Response {
    let token = params.get("token").cloned().unwrap_or_default();
    let manual = params.get("manual").map(|v| v == "1").unwrap_or(false);

    // 必须能反解出身份才放行。这里**没有**"本机一台设备都没配对就放行"的 P0 后门：
    // 那会让一个刚被解除配对的对端拿旧 token 连上、还被登记成本机的 "self" ——
    // 对端显示"长连接"，本机却把它当"未绑定"，共享可见性也全乱。
    let peer_id = match ws::session::peer_id_from_token(&state, &token) {
        Some(peer_id) => peer_id,
        None => {
            info!("WS upgrade refused: unknown or revoked token");
            return (StatusCode::UNAUTHORIZED, "Invalid token").into_response();
        }
    };

    if state.ws.is_paused(&peer_id) {
        if manual {
            info!(%peer_id, "peer asked to connect, clearing local pause");
            state.ws.resume(&peer_id);
        } else {
            info!(%peer_id, "link refused: locally paused (断开)");
            return (StatusCode::FORBIDDEN, "Paused").into_response();
        }
    }

    ws.on_upgrade(move |socket| async move {
        ws::session::run_inbound_session(state, peer_id, socket).await;
    })
    .into_response()
}

// The auth module needs `credential` — declared at the top of this file.

// ---------------------------------------------------------------------------
// Server lifecycle (unchanged from P0)
// ---------------------------------------------------------------------------

pub async fn start_server(state: Arc<AppState>, base_port: u16) -> Result<u16, String> {
    let app = build_router(state.clone());

    for port in base_port..=base_port + 10 {
        let addr: SocketAddr = ([0, 0, 0, 0], port).into();
        match TcpListener::bind(addr).await {
            Ok(listener) => {
                let (tx, rx) = oneshot::channel::<()>();
                *state.server_shutdown.lock() = Some(tx);

                let task_handle: tokio::task::JoinHandle<()> = tokio::spawn(async move {
                    // Use into_make_service_with_connect_info so ConnectInfo
                    // extractors work in middleware and handlers.
                    if let Err(e) = axum::serve(
                        listener,
                        app.into_make_service_with_connect_info::<SocketAddr>(),
                    )
                    .with_graceful_shutdown(async { rx.await.ok(); })
                    .await
                    {
                        error!("axum serve error: {e}");
                    }
                });
                *state.server_task.lock() = Some(task_handle);

                state.server_port.store(port, std::sync::atomic::Ordering::SeqCst);
                info!(port, "HTTP server started");
                return Ok(port);
            }
            Err(e) if port < base_port + 10 => {
                error!(port, "port busy, trying next: {e}");
                continue;
            }
            Err(e) => {
                return Err(format!(
                    "端口 {base_port}~{} 均被占用: {e}",
                    base_port + 10
                ));
            }
        }
    }
    Err("unreachable port probe logic".into())
}

pub async fn stop_server(state: &AppState) {
    let tx = state.server_shutdown.lock().take();
    if let Some(tx) = tx {
        let _ = tx.send(());
    }
    let handle = state.server_task.lock().take();
    if let Some(h) = handle {
        let _ = tokio::time::timeout(std::time::Duration::from_secs(3), h).await;
    }
    state.server_port.store(0, std::sync::atomic::Ordering::SeqCst);
    info!("HTTP server stopped");
}

pub async fn ensure_server(state: Arc<AppState>) -> Result<u16, String> {
    let current_port = state.server_port.load(std::sync::atomic::Ordering::SeqCst);
    let base_port = state.config.lock().port;

    if current_port == 0 || current_port < base_port || current_port > base_port + 10 {
        if current_port != 0 {
            stop_server(&state).await;
        }
        start_server(state, base_port).await
    } else {
        Ok(current_port)
    }
}

// Existing test module — kept for reference.
#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, ShareItem};
    use crate::state::AppState;
    use reqwest::StatusCode;

    fn temp_share_dir() -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("orbdrop_test_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("hello.txt"), b"hello world").unwrap();
        std::fs::write(dir.join("sub").join("nested.txt"), b"nested content").unwrap();
        dir
    }

    fn make_state(root: std::path::PathBuf) -> Arc<AppState> {
        let mut config = Config::default();
        let share = ShareItem::new(root);
        config.shares.push(share);
        let config_path = std::env::temp_dir().join(format!("orbdrop_test_config_{}.toml", uuid::Uuid::new_v4()));
        Arc::new(AppState::new(config, config_path))
    }

    async fn start_test_server() -> (Arc<AppState>, u16, std::path::PathBuf) {
        let root = temp_share_dir();
        let state = make_state(root.clone());
        let port = start_server(state.clone(), 3990).await.expect("server start");
        (state, port, root)
    }

    #[tokio::test]
    async fn tree_lists_one_level() {
        let (_state, port, root) = start_test_server().await;
        let share_id = _state.config.lock().shares[0].id.clone();

        let resp = reqwest::get(format!(
            "http://127.0.0.1:{port}/tree?share={share_id}&path="
        ))
        .await
        .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let json: serde_json::Value = resp.json().await.unwrap();
        let entries = json["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 2, "root should contain hello.txt and sub");
        let names: Vec<&str> = entries
            .iter()
            .map(|e| e["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"hello.txt"));
        assert!(names.contains(&"sub"));

        let resp = reqwest::get(format!(
            "http://127.0.0.1:{port}/tree?share={share_id}&path=sub"
        ))
        .await
        .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let json: serde_json::Value = resp.json().await.unwrap();
        let entries = json["entries"].as_array().unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0]["name"], "nested.txt");
        assert_eq!(entries[0]["isDir"], false);

        stop_server(&_state).await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn tree_rejects_path_traversal() {
        let (_state, port, root) = start_test_server().await;
        let share_id = _state.config.lock().shares[0].id.clone();

        let resp = reqwest::get(format!(
            "http://127.0.0.1:{port}/tree?share={share_id}&path=..%2F.."
        ))
        .await
        .unwrap();
        assert_eq!(resp.status(), StatusCode::FORBIDDEN);

        stop_server(&_state).await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn file_reads_with_offset_and_len() {
        let (_state, port, root) = start_test_server().await;
        let share_id = _state.config.lock().shares[0].id.clone();

        let resp = reqwest::get(format!(
            "http://127.0.0.1:{port}/file?share={share_id}&path=hello.txt"
        ))
        .await
        .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.bytes().await.unwrap();
        assert_eq!(body.as_ref(), b"hello world");

        let resp = reqwest::get(format!(
            "http://127.0.0.1:{port}/file?share={share_id}&path=hello.txt&offset=6&len=5"
        ))
        .await
        .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = resp.bytes().await.unwrap();
        assert_eq!(body.as_ref(), b"world");

        let resp = reqwest::get(format!(
            "http://127.0.0.1:{port}/file?share={share_id}&path=hello.txt&offset=999"
        ))
        .await
        .unwrap();
        let body = resp.bytes().await.unwrap();
        assert!(body.is_empty());

        let resp = reqwest::get(format!(
            "http://127.0.0.1:{port}/file?share=nonexistent&path=hello.txt"
        ))
        .await
        .unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        stop_server(&_state).await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn shares_endpoint_lists_shares_over_loopback() {
        let (_state, port, root) = start_test_server().await;

        let resp = reqwest::get(format!("http://127.0.0.1:{port}/shares"))
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let json: serde_json::Value = resp.json().await.unwrap();
        let items = json["shares"].as_array().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["isDir"], true);
        assert!(
            items[0].get("path").is_none(),
            "本机绝对路径不下发：本机 UI 走 list_shares 命令，对端只要 name/isDir"
        );

        stop_server(&_state).await;
        let _ = std::fs::remove_dir_all(root);
    }

    #[tokio::test]
    async fn port_probing_bumps_to_free_port() {
        let root = temp_share_dir();
        let state = make_state(root.clone());

        let occupied = std::net::TcpListener::bind("127.0.0.1:3995").unwrap();
        let port = start_server(state.clone(), 3990).await.expect("server start");
        assert!(
            port >= 3990 && port <= 3995,
            "should probe within range, got {port}"
        );
        let _ = occupied;

        stop_server(&state).await;
        let _ = std::fs::remove_dir_all(root);
    }
}