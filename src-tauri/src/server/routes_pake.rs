//! 配对码的 PAKE 两条路由（`/pair/pake/start`、`/pair/pake/finish`）。
//!
//! 与 `/pair` 的分工：**新客户端只走 PAKE**，旧客户端继续走 `/pair`（那条路仍是明文
//! 口令比较）。PAKE 的 6 位码不上网；`start` 返回应答方的 PAKE 报文与确认值，`finish`
//! 校验发起方的确认值，确认通过才铸 token，并用 PAKE 会话密钥密封回执。
//!
//! 两条都在公开路由上（未绑定设备没有 token 可带），认人靠：客户端证书 + 配对码口令。

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Instant;

use axum::extract::{ConnectInfo, Extension, State};
use axum::http::StatusCode;
use axum::Json;
use base64::Engine as _;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};
use zeroize::Zeroizing;

use super::routes_pair::peer_endpoint;
use crate::net;
use crate::pair;
use crate::pake::{self, PendingPake, TokenPayload};
use crate::state::AppState;

const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::STANDARD;

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PakeStartRequest {
    /// 发起方生成的会话 id（UUID 字符串）：两条路由靠它配对，重复即拒。
    pub session: String,
    pub peer_id: String,
    #[serde(default)]
    pub peer_name: String,
    /// 发起方的服务端口（TCP 源端口是一次性的）。
    pub peer_port: u16,
    /// 发起方自报的证书指纹：必须就是这条 TLS 连接出示的客户端证书。
    #[serde(default)]
    pub cert_fingerprint: String,
    /// SPAKE2 第一步报文（base64）。
    pub pa: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PakeStartResponse {
    /// SPAKE2 第二步报文（base64）。
    pub pb: String,
    /// 应答方的确认值（base64）。
    pub confirm: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PakeFinishRequest {
    pub session: String,
    /// 发起方的确认值（base64）。
    pub confirm: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PakeSealedResponse {
    pub nonce: String,
    pub sealed: String,
}

/// session 只允许 UUID 那种字符；长度也收住，别让无认证来源往表里塞大键。
fn valid_session(session: &str) -> bool {
    (8..=64).contains(&session.len())
        && session
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// POST /pair/pake/start
///
/// 应答方（显示配对码的那台）：用**本机的**码派生口令标量，回一条 PAKE 报文与自己的
/// 确认值。此时还不铸 token —— 发起方得先证明它也知道这个码。
pub async fn pake_start_handler(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    client_cert: Option<Extension<net::ClientCertFingerprint>>,
    Json(req): Json<PakeStartRequest>,
) -> Result<Json<PakeStartResponse>, (StatusCode, String)> {
    if !valid_session(&req.session) {
        return Err((StatusCode::BAD_REQUEST, "session 格式错误".into()));
    }
    if req.peer_id.is_empty() || req.peer_id.len() > 128 {
        return Err((StatusCode::BAD_REQUEST, "缺少 peerId".into()));
    }
    if req.peer_name.len() > 128 {
        return Err((StatusCode::BAD_REQUEST, "peerName 过长".into()));
    }
    if req.pa.len() > 512 {
        return Err((StatusCode::BAD_REQUEST, "PAKE 报文过长".into()));
    }

    // PAKE 没有客户端证书就没法把证书指纹绑进 AAD —— 这一条比 `/pair` 严：老客户端
    // 走不了 PAKE，继续用 `/pair`（那条路保留给兼容）。
    let observed = client_cert
        .as_ref()
        .and_then(|Extension(fp)| fp.0.as_deref())
        .ok_or((StatusCode::UNAUTHORIZED, "PAKE 需要客户端证书".to_string()))?;
    if req.cert_fingerprint.is_empty() || !req.cert_fingerprint.eq_ignore_ascii_case(observed) {
        warn!(
            claimed = %net::short_fingerprint(&req.cert_fingerprint),
            seen = %net::short_fingerprint(observed),
            "PAKE start：自报证书指纹与连接出示的不一致"
        );
        return Err((StatusCode::UNAUTHORIZED, "证书指纹与本次连接不一致".into()));
    }

    let from = addr.ip();
    if !state.pair.pake_start_allowed(from) {
        return Err((
            StatusCode::TOO_MANY_REQUESTS,
            "配对请求过于频繁，请稍后再试".into(),
        ));
    }
    state.pair.note_pake_start(from);
    if !state.pair.code_attempt_allowed(from) {
        return Err((
            StatusCode::TOO_MANY_REQUESTS,
            "配对码尝试次数过多，请稍后再试".into(),
        ));
    }

    let code = state
        .pair
        .current_code()
        .ok_or((StatusCode::UNAUTHORIZED, "配对码错误或已过期".to_string()))?;
    let own_id = state.config.lock().device_id.clone();
    let own_fp = net::local_fingerprint().map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("本机身份读取失败: {e}"),
        )
    })?;

    // AAD 的角色固定：peer_id/fp 是发起方（A），own_id/fp 是应答方（B）。
    let aad = pake::aad(&req.session, &req.peer_id, &own_id, observed, &own_fp);
    let pa = B64
        .decode(&req.pa)
        .map_err(|_| (StatusCode::BAD_REQUEST, "PAKE 报文格式错误".to_string()))?;
    let (pb, output) = pake::respond(&code, &req.peer_id, &own_id, &aad, &pa)
        .map_err(|e| (StatusCode::BAD_REQUEST, e))?;

    let (ip, port) = peer_endpoint(&state, addr, &req.peer_id, req.peer_port);
    let peer = pair::PairedPeer {
        id: req.peer_id.clone(),
        name: if req.peer_name.is_empty() {
            req.peer_id.clone()
        } else {
            req.peer_name
        },
        ip,
        port,
        // 连接上出示的那一枚，不是请求体里的那一句。
        fingerprint: observed.to_string(),
    };
    let confirm = B64.encode(&output.confirmation_mac);

    if !state.pair.insert_pake(PendingPake {
        session: req.session,
        from: addr.ip(),
        peer,
        output,
        code: Zeroizing::new(code),
        created_at: Instant::now(),
    }) {
        return Err((
            StatusCode::TOO_MANY_REQUESTS,
            "待完成的配对请求过多，请稍后再试".into(),
        ));
    }

    info!(peer = %req.peer_id, "inbound pairing pake start");
    Ok(Json(PakeStartResponse {
        pb: B64.encode(pb),
        confirm,
    }))
}

/// POST /pair/pake/finish
///
/// 应答方：校验发起方的确认值（它证明对方也知道这个码、且双方看到的证书一致），
/// 通过后消费配对码、落库，并把 token 用 PAKE 会话密钥密封回去。
pub async fn pake_finish_handler(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(req): Json<PakeFinishRequest>,
) -> Result<Json<PakeSealedResponse>, (StatusCode, String)> {
    if !valid_session(&req.session) {
        return Err((StatusCode::BAD_REQUEST, "session 格式错误".into()));
    }
    let pending = state.pair.take_pake(&req.session).ok_or((
        StatusCode::UNAUTHORIZED,
        "配对会话不存在或已过期".to_string(),
    ))?;

    let confirm = B64
        .decode(&req.confirm)
        .map_err(|_| (StatusCode::BAD_REQUEST, "确认值格式错误".to_string()))?;
    if pending.output.verify_peer_confirmation(&confirm).is_err() {
        // 码错、或者有人在中间换了证书：两种都落到同一句话，不告诉对方是哪一种。
        state.pair.note_code_failure(addr.ip());
        warn!(peer = %pending.peer.id, "pairing pake confirmation failed");
        return Err((StatusCode::UNAUTHORIZED, "配对码错误或已过期".into()));
    }

    // 单次使用：确认通过才消费，失败/过期则什么都不动。
    if !state.pair.verify_code(&pending.code) {
        return Err((StatusCode::UNAUTHORIZED, "配对码错误或已过期".into()));
    }
    state.pair.clear_code_failures(addr.ip());

    let grant = pair::mint_grant(&state);
    // PAKE 已经把"这条连接出示的客户端证书"认证过了：直接钉它，不再让全局
    // `observed_at(peer.ip)` 覆盖（那可能是本机之前拨对方时留下的另一枚）。
    pair::commit_pairing_with_fingerprint(
        &state,
        pending.peer.clone(),
        &grant.token,
        None,
        &pending.peer.fingerprint,
    )
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;

    let own_id = state.config.lock().device_id.clone();
    let own_fp = net::local_fingerprint().map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("本机身份读取失败: {e}"),
        )
    })?;
    let aad = pake::aad(
        &req.session,
        &pending.peer.id,
        &own_id,
        &pending.peer.fingerprint,
        &own_fp,
    );
    let payload = TokenPayload {
        token: grant.token,
        peer_id: grant.peer_id,
        peer_name: grant.peer_name,
        cert_fingerprint: grant.fingerprint,
    };
    let plain = serde_json::to_vec(&payload).map_err(|e| {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("配对结果序列化失败: {e}"),
        )
    })?;
    let (nonce, sealed) = pake::seal(pending.output.session_key.as_bytes(), &aad, &plain)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;

    info!(peer = %pending.peer.id, "pairing completed (pake)");
    Ok(Json(PakeSealedResponse { nonce, sealed }))
}

#[cfg(test)]
mod tests {
    use crate::state::AppState;
    use std::sync::Arc;

    /// 真 TLS 上跑完整 PAKE：错码不落库，对码落库并消费配对码。
    ///
    /// 两台“设备”共用同一份测试身份（`cfg(test)` 下身份文件按线程分），所以对端证书
    /// 指纹就是本线程的 `local_fingerprint()`；测试断言的是协议路径，不是两把不同证书。
    #[tokio::test]
    async fn pake_pairs_over_tls_and_rejects_a_wrong_code() {
        let dir = std::env::temp_dir().join(format!("orbdrop_pake_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let state = Arc::new(AppState::new(
            crate::config::test_config(),
            dir.join("config.toml"),
        ));
        let port = crate::server::start_server_loopback(state.clone(), 4323)
            .await
            .expect("server start");
        let base = format!("https://127.0.0.1:{port}");
        // 独立信任表：不碰进程级 pin（并行用例在 127.0.0.1 上互相看见对方是常态）。
        let client = crate::net::client_builder_with_trust(Arc::new(crate::net::Trust::new()))
            .build()
            .expect("client");
        let fp = crate::net::local_fingerprint().expect("identity");
        let responder_id = state.config.lock().device_id.clone();
        // 凭据库是进程级的（测试并行）：用唯一 id，别和别的用例里的 "peer-a" 撞。
        let initiator_id = format!("peer-pake-{}", uuid::Uuid::new_v4());

        // 错码：确认值对不上，服务端不落库、不发 token。
        let code = state.pair.create_code();
        let wrong = if code == "000000" { "111111" } else { "000000" };
        let err = crate::pake::run_client(
            &client,
            &base,
            "sess-wrong-0001",
            &initiator_id,
            "PC-A",
            3999,
            &responder_id,
            wrong,
            &fp,
            &fp,
        )
        .await
        .unwrap_err();
        assert!(err.contains("配对码"), "错码要报配对码错误，实际: {err}");
        assert!(
            state.get_peer(&initiator_id).is_none(),
            "错码不能把关系落库"
        );

        // 对码：落库、钉住这条连接上的证书、配对码被消费。
        let code = state.pair.create_code();
        let payload = crate::pake::run_client(
            &client,
            &base,
            "sess-ok-0001",
            &initiator_id,
            "PC-A",
            3999,
            &responder_id,
            &code,
            &fp,
            &fp,
        )
        .await
        .expect("正确配对码应当成功");
        assert_eq!(payload.peer_id, responder_id);
        assert_eq!(payload.cert_fingerprint, fp);
        assert_eq!(payload.token.len(), 64);
        let row = state.get_peer(&initiator_id).expect("服务端要落库");
        assert!(row.paired);
        assert_eq!(
            crate::credential::get_peer_fingerprint(&initiator_id)
                .unwrap()
                .as_deref(),
            Some(fp.as_str())
        );
        assert!(
            state.pair.current_code().is_none(),
            "成功后配对码必须被消费"
        );

        crate::server::stop_server(&state).await;
        let _ = std::fs::remove_dir_all(&dir);
    }
}
