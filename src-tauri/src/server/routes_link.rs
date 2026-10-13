//! 长连接请求的两个端点（F-16 的同意制，见 `crate::link`）。
//!
//! 两条都在**公开面**上（没有长连接可发时也要能送到），身份由**token** 认 —— 与
//! `/pair/invite`、`/push/invite` 同一道关：token 反解出 peer id，反解不出就是 401。

use std::sync::Arc;
use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::Json;
use serde::Deserialize;
use tracing::info;

use crate::link;
use crate::state::AppState;
use crate::ws;
use crate::ws::events::ui;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkRequest {
    pub request_id: String,
    /// 发起方自报的设备名 —— **只用于显示**：本机设备表里那一份优先（那才是本机认识的它）。
    #[serde(default)]
    pub peer_name: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LinkWithdraw {
    pub request_id: String,
}

/// POST /link/request —— 有设备请求与本机建立长连接（F-16）。
///
/// 这条 HTTP 会**挂着**等本机用户在自己的通知里点「接受」/「拒绝」（最长 `REQUEST_TTL`），
/// 回复就是这次请求的结果：200 = 接受，403 + 原因 = 拒绝 / 超时 / 撤回。与配对邀请同一套
/// 取舍：一次请求就是一次一问一答，中间不需要轮询，也不需要先有链路。
pub async fn link_request_handler(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
    Json(req): Json<LinkRequest>,
) -> Result<StatusCode, (StatusCode, String)> {
    if req.request_id.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "缺少 requestId".into()));
    }
    // 身份**只信 token**（不信载荷里自称的设备）：这条会话是 token 换来的。
    let Some(peer_id) = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .and_then(|t| crate::ws::session::peer_id_from_token(&state, t))
    else {
        info!("link request refused: unknown or revoked token");
        return Err((StatusCode::UNAUTHORIZED, "身份校验失败".into()));
    };

    // 显示的用名字：本机设备表里的那一份优先（它是本机自己记着的），载荷里的只是兜底。
    let peer_name = state
        .get_peer(&peer_id)
        .map(|p| p.name)
        .filter(|n| !n.trim().is_empty())
        .unwrap_or(req.peer_name);

    info!(%peer_id, request_id = %req.request_id, "inbound link request");
    ws::emit_ui(&state, ui::LINK_REQUEST, &serde_json::json!({
        "requestId": req.request_id,
        "peerId": peer_id,
        "peerName": peer_name,
    }));

    let (tx, rx) = tokio::sync::oneshot::channel::<Result<(), String>>();
    link::note_incoming(
        &state,
        link::PendingLinkRequest::new(req.request_id.clone(), peer_id, peer_name, tx),
    );

    match tokio::time::timeout(link::REQUEST_TTL + Duration::from_secs(15), rx).await {
        Ok(Ok(Ok(()))) => Ok(StatusCode::NO_CONTENT),
        Ok(Ok(Err(reason))) => Err((StatusCode::FORBIDDEN, reason)),
        // oneshot 断了（本机正在关停之类）：按"没接受"回。
        _ => Err((StatusCode::FORBIDDEN, "连接请求已作废".into())),
    }
}

/// POST /link/request/withdraw —— 发起方点了「取消」，把挂在本机界面上的那条通知撤掉。
///
/// 与配对邀请的撤回同一套：请求 id 本身就是一次性的能力（只在这一次里出现过），
/// 撤不成就让它自己的 TTL 兜底。
pub async fn link_withdraw_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<LinkWithdraw>,
) -> StatusCode {
    link::withdraw_incoming(&state, &req.request_id);
    StatusCode::NO_CONTENT
}