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

use crate::state::{AppState, LinkIntent};
use crate::ws;

mod auth;
pub mod proxy;
mod routes_chat;
mod routes_dav;
mod routes_discover;
mod routes_relay;
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

/// 本机网卡上的全部地址（含回环、含 IPv6）。
///
/// 与 `local_ipv4_addresses` 分开是因为用途不同：那个是"要给用户看/要给对端播"的
/// 内网地址（回环不算），这个是判"一个来源地址是不是我自己"用的 —— 回环必须在内
/// （见 `net::observed_dial_target`）。
pub fn interface_addresses() -> Vec<std::net::IpAddr> {
    if_addrs::get_if_addrs()
        .map(|list| list.into_iter().map(|i| i.addr.ip()).collect())
        .unwrap_or_default()
}

/// 本机在局域网上的 IPv4 地址（非回环），**按"该先报哪个"排好序**。
///
/// 一块网卡一个地址：有线 + 无线 + VPN 同时开着就会有 2~3 个。界面（左下角
/// 「端口 · IP」）只显示第一个，其余在悬停提示里列全 —— 那一条 footer 装不下三四个地址。
/// 排序：私有网段（家用/办公局域网）→ 其他全局地址 → 链路本地（169.254，通常是没拿到
/// DHCP 的残留）；同级按地址升序，保证每次刷新顺序一样、不跳。
pub fn local_ipv4_addresses() -> Vec<String> {
    let mut addrs: Vec<std::net::Ipv4Addr> = if_addrs::get_if_addrs()
        .map(|list| {
            list.into_iter()
                .filter_map(|i| match i.addr {
                    if_addrs::IfAddr::V4(v4) if !v4.ip.is_loopback() => Some(v4.ip),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();
    addrs.sort_by_key(|ip| (ip_rank(ip), u32::from(*ip)));
    addrs.dedup();
    addrs.into_iter().map(|ip| ip.to_string()).collect()
}

/// 显示优先级：私有网段 0、其他全局地址 1、链路本地 2。
fn ip_rank(ip: &std::net::Ipv4Addr) -> u8 {
    if ip.is_link_local() {
        2
    } else if ip.is_private() {
        0
    } else {
        1
    }
}

/// 推送那条路允许的请求体上限（清单/回执）。
///
/// axum 的默认值是 2MB，而一份"推整个目录"的清单按每条 ~70 字节算，2MB 只够 3 万条 ——
/// 更大的目录会在邀请那一步 413。长连接那条路的帧上限本来就是 64MB（tungstenite 默认），
/// 这里与它对齐，两条通道的能力才一致。
const MANIFEST_BODY_LIMIT: usize = 64 << 20;

/// Build the axum Router with all routes, including the WS /events
/// endpoint (outside the auth layer).
fn build_router(state: Arc<AppState>) -> Router {
    // **只允许本机自己的前端来源**，不再是 `*`。
    //
    // 回环那条路是免认证的（本机 UI 自验用，见 `auth_middleware`），而 `*` 意味着**任何**
    // 网页都能读到 /shares、/tree、/file 的响应 —— 用户访问的恶意页面于是能枚举并下载
    // 共享文件（Chrome 的 Private Network Access 会拦，Firefox/Safari 目前不拦）。
    // 白名单只收本机来源：`localhost`（dev 的 vite 在 1420）、`tauri.localhost`（Tauri v2
    // 在 Windows 上的 webview 来源）。别的来源跨域读不到任何东西。
    let cors = CorsLayer::new()
        .allow_origin(tower_http::cors::AllowOrigin::predicate(
            |origin: &axum::http::HeaderValue, _req: &axum::http::request::Parts| {
                is_trusted_origin(origin.to_str().unwrap_or_default())
            },
        ))
        .allow_methods(Any)
        .allow_headers(Any);

    // Public routes (no auth): pairing, info, WS upgrade.
    // 推送的两端也在公开面上：未绑定设备没有 token 可带（F-50），
    // 各自的认人规则写在 handler 里（routes_push::authorize / push_invite_handler）。
    let public_routes = Router::new()
        .route("/pair/info", axum::routing::get(routes_pair::pair_info))
        .route("/pair", axum::routing::post(routes_pair::pair_handshake))
        .route(
            "/pair/invite",
            axum::routing::post(routes_pair::pair_invite_handler),
        )
        .route(
            "/pair/invite/withdraw",
            axum::routing::post(routes_pair::pair_invite_withdraw_handler),
        )
        .route("/events", axum::routing::get(events_ws_handler))
        .route(
            "/push/invite",
            // axum 对 `Json` 提取器有一条 **2MB 默认上限**（`DefaultBodyLimit`），而这条
            // 路由的请求体里装着**整份文件清单**（每条 `{"path":…,"size":…}` 约 70 字节）。
            // 2MB 只够 3 万条：再大一点的目录会在"邀请"这一步直接 413，表现成"推不过去"
            // 且看不出为什么。清单是发送方自己算出来的、有对端同意/取消两道闸，把它放开
            // 到 64MB（与长连接那条路的 tungstenite 帧上限同量级）是安全且必要的。
            axum::routing::post(routes_push::push_invite_handler)
                .layer(axum::extract::DefaultBodyLimit::max(MANIFEST_BODY_LIMIT)),
        )
        .route(
            "/push/report",
            axum::routing::post(routes_push::push_report_handler)
                .layer(axum::extract::DefaultBodyLimit::max(MANIFEST_BODY_LIMIT)),
        )
        .route(
            "/push/download",
            axum::routing::get(routes_push::push_download_handler),
        )
        .route(
            // 新数据面：一批块任务 + 分帧记录流（老对端不会来，见 `InboundInvite::supports_stream`）。
            "/push/stream",
            axum::routing::post(routes_push::push_stream_handler)
                .layer(axum::extract::DefaultBodyLimit::max(MANIFEST_BODY_LIMIT)),
        )
        // 会话（聊天）没有长连接时的那一跳：同样挂在公开路由上、自己认人
        // （未绑定设备没有 token 可带），判断规则与 `/push/invite` 一模一样。
        // 中转（F-106 / S-09）：请本机替它连另一台设备（端点由本机开、本机盲转）。
        //
        // **挂在公开路由上**（F-107 §12.1）：S-09 的语义放宽成"开着就允许，不限已配对设备"，
        // 而未配对设备本来就没有 token 可带 —— 所以认人规则写在 handler 里：凭据优先，
        // 没有凭据就必须自报身份并被核实（见 `routes_relay::relay_open`）。
        .route(
            "/relay/open",
            axum::routing::post(routes_relay::relay_open),
        )
        // 反向路由记录（F-107 §10）：**同样在公开面上**、自己认人 —— 要送达的那一侧
        // （被中转的 B）通常从来没跟锚点配过（见 `routes_relay::relay_route`）。
        .route(
            "/relay/route",
            axum::routing::post(routes_relay::relay_route),
        )
        .route(
            "/chat/message",
            axum::routing::post(routes_chat::chat_message_handler),
        )
        .route(
            "/chat/read",
            axum::routing::post(routes_chat::chat_read_handler),
        )
        // 群聊三条同样走公开路由 + 自己认人：群成员**不需要配对**（D6=b），所以不能挪到
        // `auth_routes` 里（那条路要求 token 反解出身份）。
        .route(
            "/chat/group",
            axum::routing::post(routes_chat::group_text_handler),
        )
        .route(
            "/chat/group-read",
            axum::routing::post(routes_chat::group_read_handler),
        )
        .route(
            "/chat/group-roster",
            axum::routing::post(routes_chat::group_roster_handler),
        )
        .route(
            "/chat/group-sync",
            axum::routing::post(routes_chat::group_sync_ask_handler),
        )
        .route(
            "/chat/group-sync-post",
            axum::routing::post(routes_chat::group_sync_post_handler),
        )
        .route(
            "/chat/group-ack",
            axum::routing::post(routes_chat::group_ack_handler),
        );

    // Beyond Compare 的 WebDAV 入口：让 BC 连**本机**（回环），由这里加密转发到对端。
    // 挂在公开路由上是因为它自己校验凭据（它要的挑战/凭据形状必须与对端 DAV 面一致，
    // 而 `auth_middleware` 对回环是直接免认证的 —— 那是给本机 UI 自验用的，不能用在这里）。
    // handler 内部另有一道"只认回环"的闸门。
    //
    // **不能挂 CORS 层**（和 `/dav` 同一条理由）：tower-http 的 CORS 会把**所有** OPTIONS
    // 抢答成它自己的 preflight 响应（`200` + `Access-Control-Allow-Methods: *`），而 BC 正是
    // 用 `OPTIONS` 协商 DAV 能力（要看到 `DAV: 1,2`）——被抢答就等于"这不是 WebDAV"，
    // 远端目录在 BC 里直接加载不了。profile 的 BaseURL 指的就是这里（`server::proxy::base_url`），
    // 所以这条路是 BC 的**唯一**入口，它被吞掉 BC 就整体不可用。
    let proxy_routes = Router::new()
        .route(
            "/proxy/{peer_id}",
            axum::routing::any(proxy::dav_proxy_root),
        )
        .route(
            "/proxy/{peer_id}/",
            axum::routing::any(proxy::dav_proxy_root),
        )
        .route(
            "/proxy/{peer_id}/{*rest}",
            axum::routing::any(proxy::dav_proxy),
        );

    // Authenticated routes (auth middleware).
    let auth_routes = Router::new()
        .route("/tree", axum::routing::get(routes_tree::tree_handler))
        .route("/stat", axum::routing::get(routes_stat::stat_handler))
        .route("/unpair", axum::routing::post(routes_pair::unpair))
        .route("/file", axum::routing::get(routes_file::file_handler))
        .route("/shares", axum::routing::get(routes_shares::shares_handler))
        // 跨网段发现（F-10 延伸）：已配对对端来取「本机此刻看得见的设备」（整份视野）。
        .route(
            "/discover",
            axum::routing::get(routes_discover::discover),
        )
        // 中转（F-106）：已配对对端来请本机替它连另一台设备（端点由本机开、本机盲转）。
        // F-107 起 `/relay/open` 与 `/relay/route` 都移到公开路由（未配对设备也能借 / 也能收到
        // 记录，认人规则在 handler 里），见 `public_routes`。
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            auth::auth_middleware,
        ));

    let http_routes = Router::new()
        .merge(public_routes)
        .merge(auth_routes)
        .layer(cors);

    // WebDAV 面（F-41 ~ F-43）：Beyond Compare 直连共享项。
    // **不能挂 CORS 层**（同 `proxy_routes` 那条）：tower-http 的 CORS 会把 OPTIONS 直接
    // 抢答成 preflight 响应，而 BC 正是用 `OPTIONS` 协商 DAV 能力（`DAV: 1,2`）。
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

    // CORS 层只盖在"本机 UI 用的那几条 REST 路由"上（见上面对 `proxy_routes` 的说明）：
    // BC 的两个面（`/proxy` 与 `/dav`）都在它之外单挂，OPTIONS 才能原样到对端。
    http_routes
        .merge(proxy_routes)
        .merge(dav_routes)
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            presence_middleware,
        ))
        .with_state(state)
}

/// 这个 `Origin` 是不是**本机自己的界面**发出来的。
///
/// 用途有两处（两处都不能用 `*`）：① CORS 白名单 —— 只把响应交给自己的来源；② 回环免认证
/// 的闸门 —— 带**外来** Origin 的回环请求（来自用户浏览器里的某个网页）不算"本机 UI"。
///
/// 判据只看**主机名**：`localhost` / `*.localhost`（含 `tauri.localhost`）/ `127.0.0.1` /
/// `::1`。端口与协议不管 —— dev 的 vite 在 1420、Tauri 在 Windows 上用 `http://tauri.localhost`、
/// macOS/Linux 上是 `tauri://localhost`，钉死某一个都会在某条路上把自己挡掉。
pub(crate) fn is_trusted_origin(origin: &str) -> bool {
    fn host_of(origin: &str) -> Option<&str> {
        let rest = origin.split_once("://")?.1;
        let authority = rest.split('/').next().unwrap_or(rest);
        // IPv6 是 `[::1]:1420` 这种形状。
        if let Some(bracketed) = authority.strip_prefix('[') {
            return bracketed.split(']').next();
        }
        Some(authority.split(':').next().unwrap_or(authority))
    }
    match host_of(origin) {
        Some(host) => {
            host == "localhost"
                || host.ends_with(".localhost")
                || host == "127.0.0.1"
                || host == "::1"
        }
        None => false,
    }
}

/// 本机切到「离线」（隐身）时，对**一切**入站请求回 503。
///
/// 这是"隐身"的另一半：mDNS 那边已经注销广播（对端发现不到本机），但对端每 5 秒还在
/// 打本机的 `/pair/info` 探活 —— 只要还应答，它就把本机当"在线"（`error_for_status`
/// 之外的任何 200 都算）。所以整条链路一起关：REST、DAV、推送、配对邀请、WS 升级
/// （`/events` 也在这张路由表里，层在最外面，升级之前就被拦下）。
///
/// 服务器**不停**：切回在线要立刻可用，而 503 与"连不上"对调用方是两种信号 ——
/// 非 200 即离线（`state::probe_step`），够用。
async fn presence_middleware(
    State(state): State<Arc<AppState>>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    if !state.is_online() {
        return (StatusCode::SERVICE_UNAVAILABLE, "Offline").into_response();
    }
    next.run(req).await
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
            // 暂停的那一份也要从地址簿里清掉：只清内存的话，本机下次启动又会拒绝它
            // （对端那边已经按了「连接」，双方却对同一件事有两种记忆）。
            state.set_link_intent(&peer_id, LinkIntent::Wanted);
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
    // 监听方向要一份 TLS 配置：没有身份就没有加密，宁可就起不来。
    let identity = crate::net::load_or_create_identity()?;
    let tls = crate::net::server_config(&identity);

    // `saturating_add`：`base_port` 是 u16（配置里手写成 65530 也合法），`+ 10` 会溢出 panic
    // —— 而这条路径是"启动监听"，panic 等于应用起不来。溢出时退化成"只试到 65535"。
    let last = base_port.saturating_add(10);
    for port in base_port..=last {
        let addr: SocketAddr = ([0, 0, 0, 0], port).into();
        match TcpListener::bind(addr).await {
            Ok(listener) => {
                let (tx, rx) = oneshot::channel::<()>();
                *state.server_shutdown.lock() = Some(tx);

                let task_handle: tokio::task::JoinHandle<()> = tokio::spawn(async move {
                    // 自己跑 accept 循环（`net::serve`）：这个端口上要同时接两种连接 ——
                    // 对端走 TLS，明文只留给回环（本机 UI 自验、BC 的 WebDAV 入口）。
                    // `axum::serve` 只能整条监听都是同一种，所以这里换成手写的那个。
                    crate::net::serve(listener, app, tls, async {
                        rx.await.ok();
                    })
                    .await;
                });
                *state.server_task.lock() = Some(task_handle);

                state.server_port.store(port, std::sync::atomic::Ordering::SeqCst);
                info!(port, "HTTP server started");
                return Ok(port);
            }
            Err(e) if port < last => {
                error!(port, "port busy, trying next: {e}");
                continue;
            }
            Err(e) => {
                return Err(format!(
                    "端口 {base_port}~{last} 均被占用: {e}"
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

    if current_port == 0 || current_port < base_port || current_port > base_port.saturating_add(10) {
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
        // 卡片副标题那两个数：目录给**直接**子项数（fixture 是 `hello.txt` + `sub` 两项 ——
        // 递归数会得到 3，这个断言就是为了挡住"顺手递归"），大小则不给（那要遍历整棵树）。
        assert_eq!(items[0]["itemCount"], 2);
        assert_eq!(items[0]["size"], serde_json::Value::Null);
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

    /// 回环免认证**只给本机自己的界面**：带外来 Origin 的回环请求要凭据（P1-3）。
    ///
    /// 否则任何网页都能无凭据读走 /shares、/file（`access-control-allow-origin: *` 还会
    /// 把响应交给它），甚至用 PUT/DELETE /dav 改写共享文件。
    #[tokio::test]
    async fn loopback_needs_no_token_but_a_foreign_origin_does() {
        let (state, port, root) = start_test_server().await;
        let url = format!("http://127.0.0.1:{port}/shares");
        let client = reqwest::Client::new();

        // ① 无 Origin（curl / 本机进程 / Rust 侧 loopback 调用）：照旧免认证。
        let bare = client.get(&url).send().await.unwrap();
        assert_eq!(bare.status(), StatusCode::OK, "本机 UI 的那条路不能变");

        // ② 本机自己的来源：放行，且 CORS 只回给这个来源（不再是 `*`）。
        let ours = client
            .get(&url)
            .header(axum::http::header::ORIGIN, "http://localhost:1420")
            .send()
            .await
            .unwrap();
        assert_eq!(ours.status(), StatusCode::OK);
        assert_eq!(
            ours.headers()
                .get(axum::http::header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .and_then(|v| v.to_str().ok()),
            Some("http://localhost:1420"),
            "只回白名单里的那个来源，不能是 *"
        );

        // ③ 外来网页的来源：不再算"本机 UI" → 要凭据（401），CORS 也不给它放行头。
        let foreign = client
            .get(&url)
            .header(axum::http::header::ORIGIN, "https://evil.example")
            .send()
            .await
            .unwrap();
        assert_eq!(
            foreign.status(),
            StatusCode::UNAUTHORIZED,
            "网页从回环发起的请求必须出示凭据"
        );
        assert!(
            foreign
                .headers()
                .get(axum::http::header::ACCESS_CONTROL_ALLOW_ORIGIN)
                .is_none(),
            "外来来源不该拿到 CORS 放行头"
        );

        stop_server(&state).await;
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn only_local_origins_are_trusted() {
        for ok in [
            "http://localhost:1420",
            "tauri://localhost",
            "http://tauri.localhost",
            "https://tauri.localhost",
            "http://127.0.0.1:3890",
            "http://[::1]:3890",
        ] {
            assert!(is_trusted_origin(ok), "{ok} 应当被信任");
        }
        for bad in [
            "https://evil.example",
            "http://evil.example:1420",
            "https://localhost.evil.example",
            "null",
            "",
            "http://192.168.1.5:1420",
        ] {
            assert!(!is_trusted_origin(bad), "{bad} 不该被信任");
        }
    }
}
