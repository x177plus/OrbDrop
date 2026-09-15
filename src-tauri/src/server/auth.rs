use std::sync::Arc;
use std::net::SocketAddr;

use axum::body::Body;
use axum::extract::{ConnectInfo, Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::http::StatusCode;

use crate::config::ShareItem;
use crate::state::AppState;
use crate::ws::session::peer_id_from_token;

/// 谁在调用本机的 HTTP 接口。
///
/// 由 `auth_middleware` 解析并塞进 request extensions，供各 handler 取用
/// （`/shares` 过滤、`/tree` `/file` 鉴权都依赖它）。
#[derive(Debug, Clone)]
pub enum Caller {
    /// 本机 UI：回环地址过来的请求。本机走同一套 HTTP 协议浏览自己的共享项，
    /// 因此对本机永远全量可见。
    Local,
    /// 某个已配对对端，值是它的 device id。
    Peer(String),
}

impl Caller {
    /// 共享项可见性（F-22 / F-25）：对端要看到某个共享项，必须
    /// ① 该共享项开着广播，且 ② 该对端**此刻**处于长连接。
    ///
    /// 动态判定，而不是「开播那一刻的长连接设备快照」：对端建立/断开长连接时，
    /// 可见范围立刻跟着变，不需要谁去回填历史共享项。
    pub fn sees_share(&self, state: &AppState, share: &ShareItem) -> bool {
        match self {
            Caller::Local => true,
            Caller::Peer(peer_id) => share.broadcast_enabled && state.ws.has_connection(peer_id),
        }
    }
}

/// Auth middleware: identify the caller and reject unauthenticated remote requests.
///
/// - Loopback → `Caller::Local`（本机 UI 的协议自验路径）。
/// - 其它来源必须带 `Bearer <token>`（或 WebDAV 的 `Basic x:<token>`），
///   且 token 必须能反解成某个已配对对端 → `Caller::Peer(id)`。
pub async fn auth_middleware(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    request: Request<Body>,
    next: Next,
) -> Response {
    // Loopback bypass: local UI fetches /shares /tree /file directly.
    if addr.ip().is_loopback() {
        return next.run(with_caller(request, Caller::Local)).await;
    }

    let headers = request.headers();
    let auth_header = match headers.get("Authorization").and_then(|v| v.to_str().ok()) {
        Some(h) => h,
        None => return (StatusCode::UNAUTHORIZED, "Missing Authorization header").into_response(),
    };

    let token: String = if let Some(t) = auth_header.strip_prefix("Bearer ") {
        t.to_string()
    } else if let Some(t) = auth_header.strip_prefix("Basic ") {
        // Basic auth fallback for WebDAV (BC uses Basic): "x:<token>" base64 encoded
        match base64_decode_basic(t) {
            Ok(decoded) => match decoded.split(':').nth(1) {
                Some(token_part) => token_part.to_string(),
                None => {
                    return (StatusCode::UNAUTHORIZED, "Invalid Basic auth format").into_response()
                }
            },
            Err(_) => {
                return (StatusCode::UNAUTHORIZED, "Invalid Basic auth encoding").into_response()
            }
        }
    } else {
        return (StatusCode::UNAUTHORIZED, "Unsupported auth scheme").into_response();
    };

    // token → 对端 id：这一步既完成鉴权，也确定了「谁在看」。
    // 未配对（token 不在凭据库）一律 401——没有身份就没有可见范围可言。
    match peer_id_from_token(&state, &token) {
        Some(peer_id) => next.run(with_caller(request, Caller::Peer(peer_id))).await,
        None => (StatusCode::UNAUTHORIZED, "Invalid token").into_response(),
    }
}

fn with_caller(mut request: Request<Body>, caller: Caller) -> Request<Body> {
    request.extensions_mut().insert(caller);
    request
}

fn base64_decode_basic(input: &str) -> Result<String, String> {
    use base64::Engine;
    let engine = base64::engine::general_purpose::STANDARD;
    let bytes = engine.decode(input).map_err(|e| e.to_string())?;
    String::from_utf8(bytes).map_err(|e| e.to_string())
}