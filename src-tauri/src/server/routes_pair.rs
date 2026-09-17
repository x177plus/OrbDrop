use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{ConnectInfo, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::config::PeerDevice;
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

    // Store in our keyring under the remote's peer id.
    if let Err(e) = credential::set_peer(&peer_id, &token) {
        return Err((StatusCode::INTERNAL_SERVER_ERROR, format!("凭据存储失败: {e}")));
    }

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

    // Upsert into the address book (persists, so the address survives a restart)
    // and mark paired.
    state.mark_paired(&PeerDevice {
        id: peer_id.clone(),
        name,
        ip,
        port,
        online: true,
        paired: true,
        // 刚配对：还没长连接，没有要恢复的东西。
        auto_connect: false,
    });

    // 配对只到"已配对"为止：不在这里建长连接，等本机用户点「连接」再拨号。
    let own_id = state.config.lock().device_id.clone();
    let own_name = state.config.lock().device_name.clone();
    // 刚拿到 token：把 BC 的 WebDAV profile 预写好（BC 只在启动时登记 profile 文件，
    // 所以要抢在用户下次启动 BC 之前落盘）。
    let written = crate::bc::provision_profiles(&state);
    info!(peer_id, written, "配对完成，已预写 BC profile");
    ws::emit_ui(&state, ui::PAIR_COMPLETED, &serde_json::json!({
        "peerId": peer_id,
        "peerName": req.peer_name,
    }));
    ws::emit_ui(&state, ui::DEVICES_CHANGED, &serde_json::json!({}));

    info!(%peer_id, "pairing completed");
    Ok(Json(PairResponse { token, peer_id: own_id, peer_name: own_name }))
}