use std::sync::Arc;
use std::net::SocketAddr;

use axum::body::Body;
use axum::extract::{ConnectInfo, Request, State};
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use axum::http::StatusCode;
use parking_lot::Mutex;
use tracing::info;

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
        None => return unauthorized("Missing Authorization header"),
    };

    let mut basic_user: Option<String> = None;
    let token: String = if let Some(t) = auth_header.strip_prefix("Bearer ") {
        t.to_string()
    } else if let Some(t) = auth_header.strip_prefix("Basic ") {
        // Basic auth fallback for WebDAV (BC uses Basic): `"<用户名>:<token>"` base64 编码。
        // 用户名不参与鉴权，只当作"发起方是谁"记录下来（见 `note_dav_caller`）。
        match base64_decode_basic(t) {
            Ok(decoded) => match split_basic(&decoded) {
                Some((user, token)) => {
                    basic_user = Some(user.to_string());
                    token.to_string()
                }
                None => return unauthorized("Invalid Basic auth format"),
            },
            Err(_) => return unauthorized("Invalid Basic auth encoding"),
        }
    } else {
        return unauthorized("Unsupported auth scheme");
    };

    // token → 对端 id：这一步既完成鉴权，也确定了「谁在看」。
    // 未配对（token 不在凭据库）一律 401——没有身份就没有可见范围可言。
    match peer_id_from_token(&state, &token) {
        Some(peer_id) => {
            if let Some(user) = basic_user {
                note_dav_caller(&peer_id, &user);
            }
            next.run(with_caller(request, Caller::Peer(peer_id))).await
        }
        None => unauthorized("Invalid token"),
    }
}

/// 「谁经 DAV 面连进来了」要留在日志里（N-11 排障）：BC 那条路用的是本应用写进 profile 的
/// 账号（`bc::DAV_USER`），所以这一行就是"本应用替用户发起的连接"的凭据。
///
/// 每台设备在本进程只记一行：BC 浏览一个目录就会连发十几个 PROPFIND/GET，逐条记等于把
/// 日志刷爆，而"发起方是谁"这个事实只需要出现一次。
fn note_dav_caller(peer_id: &str, user: &str) {
    static SEEN: Mutex<Vec<String>> = Mutex::new(Vec::new());
    let mut seen = SEEN.lock();
    if seen.iter().any(|id| id == peer_id) {
        return;
    }
    seen.push(peer_id.to_string());
    info!(%peer_id, user, "WebDAV 请求已认证：发起方用的是本应用写下的 BC profile");
}

/// 401 必须带 `WWW-Authenticate: Basic`：Beyond Compare 的 WebDAV 客户端先发一次不带凭据的
/// 请求（实测先 HEAD/OPTIONS 裸探），收到挑战后才用 profile 里存的账号密码重试——
/// 只回 401 不带挑战头，BC 会直接判定连接失败。
fn unauthorized(msg: &str) -> Response {
    (
        StatusCode::UNAUTHORIZED,
        [(
            axum::http::header::WWW_AUTHENTICATE,
            "Basic realm=\"OrbDrop\"",
        )],
        msg.to_string(),
    )
        .into_response()
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

/// 拆开已解码的 Basic 凭据：`<用户名>:<token>`。
///
/// token 取**第一个**冒号之后的部分：用户名是我们自己挑的（BC profile 里的 `OrbDrop`），
/// 万一它含冒号，`split(':').nth(1)` 会取到用户名里的第二段去当 token —— 那是错的。
/// 空 token 直接判格式错误，省得拿空串去查凭据库。
fn split_basic(decoded: &str) -> Option<(&str, &str)> {
    let (user, token) = decoded.split_once(':')?;
    (!token.is_empty()).then_some((user, token))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_credentials_split_on_the_first_colon() {
        assert_eq!(split_basic("x:tok"), Some(("x", "tok")));
        assert_eq!(split_basic("OrbDrop:deadbeef"), Some(("OrbDrop", "deadbeef")));
        assert_eq!(split_basic("weird:user:tok"), Some(("weird", "user:tok")));
        assert_eq!(split_basic("no-colon"), None, "没有分隔符 = 格式错误");
        assert_eq!(split_basic("OrbDrop:"), None, "空 token 不该拿去查凭据库");
    }
}