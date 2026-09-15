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
pub async fn pair_info(State(state): State<Arc<AppState>>) -> Json<serde_json::Value> {
    let listening = state.server_port.load(std::sync::atomic::Ordering::SeqCst);
    let config = state.config.lock();
    let port = if listening == 0 { config.port } else { listening };
    serde_json::json!({
        "id": config.device_id,
        "name": config.device_name,
        "port": port,
        "ver": 1,
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
    });

    // Long connection: the smaller device id initiates (see spawn_outbound_link)
    // and it starts by itself — no user click (F-16).
    ws::ensure_link(&state, &peer_id);

    let own_id = state.config.lock().device_id.clone();
    let own_name = state.config.lock().device_name.clone();
    ws::emit_ui(&state, ui::PAIR_COMPLETED, &serde_json::json!({
        "peerId": peer_id,
        "peerName": req.peer_name,
    }));
    ws::emit_ui(&state, ui::DEVICES_CHANGED, &serde_json::json!({}));

    info!(%peer_id, "pairing completed");
    Ok(Json(PairResponse { token, peer_id: own_id, peer_name: own_name }))
}