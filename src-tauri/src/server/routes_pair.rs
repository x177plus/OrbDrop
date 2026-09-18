use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{ConnectInfo, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::credential;
use crate::pair;
use crate::state::AppState;
use crate::ws;
use crate::ws::events::ui;

/// GET /pair/info — public endpoint for manual-add probe.
///
/// Reports the port we are **actually listening on**: the server falls forward to
/// port+1..port+10 when the configured port is taken, and a caller that stores the
/// configured value would dial the wrong process (or nothing) forever.
pub async fn pair_info(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
) -> Json<serde_json::Value> {
    // 「你把带这个 token 的调用方认作谁」：对端换过 device id（数据目录被清、凭据库
    // 还在）之后，这是对端唯一能确认"配对关系还活着"的地方 —— 凭据本身不回显，只回
    // 它反解出的 id。
    //
    // 必须在锁 config 之前算：`peer_id_from_token` 自己也要读 config，parking_lot 的
    // Mutex 不可重入。
    let paired_as = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .and_then(|t| crate::ws::session::peer_id_from_token(&state, t));

    let listening = state.server_port.load(std::sync::atomic::Ordering::SeqCst);
    let config = state.config.lock();
    let port = if listening == 0 { config.port } else { listening };
    serde_json::json!({
        "id": config.device_id,
        "name": config.device_name,
        "port": port,
        "ver": 1,
        "pairedAs": paired_as,
    }).into()
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PairRequest {
    /// Confirming device's id.
    pub peer_id: Option<String>,
    /// Confirming device's display name.
    pub peer_name: Option<String>,
    /// Confirming device's own HTTP port — the dial target for the side that
    /// owns the long connection (smaller device id), which may never have seen
    /// this peer through mDNS.
    pub peer_port: Option<u16>,
    /// The 6-digit code this machine issued (F-12).
    pub code: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PairResponse {
    pub token: String,
    pub peer_id: String,
    pub peer_name: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UnpairRequest {
    /// 要解除配对的对象。远端对端由 token 反解身份、不读这个字段；
    /// 本机（回环）调用没有 token 可解，只能显式给出（与回环本来就免鉴权一致）。
    pub peer_id: Option<String>,
}

/// POST /unpair — 解除配对的另一半（F-14）。
///
/// 对端用「解除配对」时它手里还有**本机给它的** token（它随后才删），所以这里带的是
/// 那个 token（Bearer），身份由 auth 中间件解析成 `Caller::Peer(id)`；同机回环调用
/// 会走 `Caller::Local` 免鉴权分支，此时从 body 取 peerId。
///
/// 只清 token 与配对状态、**保留设备行**：本端仍在网络上看得见对方，就应该继续显示为
/// "未绑定"（离线时才从列表消失）。
pub async fn unpair(
    State(state): State<Arc<AppState>>,
    axum::Extension(caller): axum::Extension<crate::server::auth::Caller>,
    body: Option<Json<UnpairRequest>>,
) -> Result<StatusCode, (StatusCode, String)> {
    let peer_id = match caller {
        crate::server::auth::Caller::Peer(id) => id,
        crate::server::auth::Caller::Local => body
            .and_then(|Json(req)| req.peer_id)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| (StatusCode::BAD_REQUEST, "本机调用需要给出 peerId".to_string()))?,
    };

    if let Err(e) = credential::remove_peer(&peer_id) {
        return Err((StatusCode::INTERNAL_SERVER_ERROR, format!("凭据清理失败: {e}")));
    }
    state.ws.drop_peer(&peer_id);
    state.unpair(&peer_id);

    // 同一件收尾：BC 里那条指向对方的 profile 已随 token 作废（只删我们自己写下的条目）。
    crate::bc::provision_profiles(&state);

    ws::emit_ui(&state, ui::DEVICES_CHANGED, &serde_json::json!({}));
    info!(%peer_id, "peer unpaired us via HTTP");
    Ok(StatusCode::NO_CONTENT)
}

/// POST /pair — verifying half of the pairing handshake (F-12).
///
/// The peer that initiated pairing issued its 6-digit code locally (`pair_start`)
/// and displays it; whoever read that code off the peer's screen posts it here.
/// On a match: mint a 32-byte hex token, store it under the caller's device id,
/// mark the caller paired, and return the token plus our own identity. Codes are
/// single-use and expire 2 minutes after issue.
pub async fn pair_handshake(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(req): Json<PairRequest>,
) -> Result<Json<PairResponse>, (StatusCode, String)> {
    let peer_id = req.peer_id.as_deref().unwrap_or_default().to_string();
    if peer_id.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "缺少 peerId".into()));
    }
    if !state.pair.verify_code(&req.code) {
        warn!(%peer_id, "pairing code rejected (unknown or expired)");
        return Err((StatusCode::UNAUTHORIZED, "配对码错误或已过期".into()));
    }

    let token = pair::generate_token();

    // The caller's address: TCP source address plus the port it reports for its
    // own server. This side may be the one that owes the outbound link (smaller
    // device id) and it has usually never seen the caller over mDNS — without
    // the address the link could only ever be dialled from a later discovery.
    let known = state.get_peer(&peer_id);
    let source = addr.ip();
    let ip = if source.is_ipv4() {
        source.to_string()
    } else {
        known.as_ref().map(|p| p.ip.clone()).unwrap_or_default()
    };
    let name = req
        .peer_name
        .clone()
        .or_else(|| known.as_ref().map(|p| p.name.clone()))
        .unwrap_or_else(|| peer_id.clone());
    let port = req
        .peer_port
        .filter(|p| *p != 0)
        .or_else(|| known.as_ref().map(|p| p.port).filter(|p| *p != 0))
        .unwrap_or(0);

    // 落库这一步与邀请那条路共用（写凭据 + 写地址簿 + 预写 BC profile + 广播事件）。
    let grant = pair::mint_grant(&state);
    pair::commit_pairing(
        &state,
        pair::PairedPeer { id: peer_id.clone(), name, ip, port },
        &token,
        None,
    )
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;

    // 配对只到"已配对"为止：不在这里建长连接，等本机用户点「连接」再拨号。
    info!(%peer_id, "pairing completed");
    Ok(Json(PairResponse { token, peer_id: grant.peer_id, peer_name: grant.peer_name }))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PairInviteRequest {
    /// 邀请 id：发起方生成，只用来把两边的这一条邀请对上号。
    pub invite_id: String,
    /// 发起方的身份（自报）——配对成后本机按它登记凭据与设备行。
    pub peer_id: String,
    pub peer_name: String,
    /// 发起方的服务端口（自报）：地址簿要拿它回连/调用本机。
    pub port: u16,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PairInviteWithdraw {
    pub invite_id: String,
}

/// POST /pair/invite —— 免输入配对码那条路（发起方点「发起邀请」，F-12）。
///
/// 与 `POST /push/invite` 同一个形状：这是个**同步**接口，请求一直挂着，直到本机用户在
/// 右下角通知里点「接收/拒绝」、或 60 秒没人处理自动作废 —— 决定一下来就作为响应回给
/// 发起方。响应体与 `POST /pair` 完全一样（`PairResponse`），两条路给发起方的是同一样东西。
///
/// 安全靠两点：① 同意制（没人点「接收」就不会交换 token）；② 若发起方自称的是本机
/// **已配对**的设备，就必须出示那枚 token —— 否则谁都能自称你的老搭档，用户点一次
/// 「接收」就把那台设备的 token 换成了对面手里的那枚（与 `push_invite_handler` 同一道关）。
pub async fn pair_invite_handler(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: axum::http::HeaderMap,
    Json(req): Json<PairInviteRequest>,
) -> Result<Json<PairResponse>, (StatusCode, String)> {
    if req.peer_id.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "缺少 peerId".into()));
    }
    if req.port == 0 {
        return Err((StatusCode::BAD_REQUEST, "缺少对方服务端口".into()));
    }
    let paired = state.get_peer(&req.peer_id).is_some_and(|p| p.paired);
    if paired {
        let claimed = headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|h| h.strip_prefix("Bearer "))
            .and_then(|t| crate::ws::session::peer_id_from_token(&state, t));
        if claimed.as_deref() != Some(req.peer_id.as_str()) {
            info!(peer = %req.peer_id, "配对邀请被拒：自称已配对设备但 token 不认");
            return Err((StatusCode::UNAUTHORIZED, "身份校验失败".into()));
        }
    }

    let invite_id = req.invite_id.clone();
    // 地址只信 TCP 来源：邀请体里的地址谁都能填。
    let peer = pair::PairedPeer {
        id: req.peer_id.clone(),
        name: req.peer_name.clone(),
        ip: addr.ip().to_string(),
        port: req.port,
    };
    info!(%invite_id, peer = %req.peer_name, paired, "inbound pair invite (http)");
    crate::ws::emit_ui(&state, ui::PAIR_INVITE, &serde_json::json!({
        "inviteId": invite_id,
        "peerId": req.peer_id,
        "peerName": req.peer_name,
    }));

    let (tx, rx) = tokio::sync::oneshot::channel();
    state
        .pair
        .insert_invite(pair::PendingInvite::new(invite_id.clone(), peer, tx));
    pair::schedule_invite_timeout(state.clone(), invite_id.clone());

    // 等用户在通知里做决定（到点由超时任务先回一个原因）。
    match tokio::time::timeout(pair::INVITE_TTL + std::time::Duration::from_secs(15), rx).await {
        Ok(Ok(Ok(grant))) => Ok(Json(PairResponse {
            token: grant.token,
            peer_id: grant.peer_id,
            peer_name: grant.peer_name,
        })),
        Ok(Ok(Err(reason))) => Err((StatusCode::FORBIDDEN, reason)),
        // oneshot 断了（本机正在关停之类）：按"没接受"回。
        _ => Err((StatusCode::FORBIDDEN, "配对邀请已作废".into())),
    }
}

/// POST /pair/invite/withdraw —— 发起方点了「取消」，把它挂在本机界面上的邀请撤掉。
///
/// 不认人、也不回错误：邀请 id 本身就是一次性的能力（只在这条邀请里出现过），
/// 撤不成就让 TTL 兜底。
pub async fn pair_invite_withdraw_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<PairInviteWithdraw>,
) -> StatusCode {
    pair::withdraw_incoming_invite(&state, &req.invite_id);
    StatusCode::NO_CONTENT
}