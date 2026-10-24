//! 配对码的 PAKE（SPAKE2 / P-256）。
//!
//! 配对码（6 位数字）从"放在请求体里给对端比较"改成"双方共同的口令"：
//!
//! ```text
//! A（输入码）  -- pa ------------------------>  B（显示码）
//! A            <-- pb + confirm(B) -----------  B
//! A            -- confirm(A) ---------------->  B
//! A            <-- seal(token, fpB) ----------  B
//! ```
//!
//! 码本身**不上网**；中间人只能在线猜一次（猜错就推不出会话密钥），没有离线字典。
//! 确认值把**双方证书指纹**绑进 AAD：即使中间人用自己的证书终止了两端 TLS 并原样
//! 转发 PAKE 报文，两端看到的证书不同 → AAD 不同 → 确认值对不上，配对失败。
//!
//! 端点都是 TLS 上的普通 HTTP；PAKE 不替代传输加密，只负责"第一次配对时谁也冒充不了谁"。

use std::time::Duration;

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use base64::Engine as _;
use hkdf::Hkdf;
use pakery_core::crypto::group::CpaceGroup;
use pakery_crypto::{P256Group, Spake2P256};
use pakery_spake2::{PartyA, PartyAState, PartyB, Spake2Output};
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256, Sha512};
use zeroize::Zeroizing;

use crate::pair::PairedPeer;

/// PAKE 会话在服务端最多留多久（与配对码同寿命）。
pub const PAKE_TTL: Duration = Duration::from_secs(120);

/// 同时挂着的 PAKE 会话上限：未认证来源可以无限 POST start，必须有个盖子。
pub const MAX_PENDING_PAKE: usize = 32;
/// 单个来源地址最多能占几条：挡住"一个人把 32 条全占了，让真正的配对 start 回 429"。
pub const MAX_PENDING_PAKE_PER_SOURCE: usize = 4;

/// 每个来源地址每分钟最多发起几次 start（P-256 运算不便宜）。
pub const PAKE_START_LIMIT: u32 = 20;
pub const PAKE_START_WINDOW: Duration = Duration::from_secs(60);

/// 配对码的 HTTP 超时（局域网；跨段/中转留一点余量）。
const HTTP_TIMEOUT: Duration = Duration::from_secs(15);

const B64: base64::engine::general_purpose::GeneralPurpose =
    base64::engine::general_purpose::STANDARD;

/// 口令派生的域分离标签。码只在两端本地参与计算，不上网。
const CODE_DOMAIN: &[u8] = b"orbdrop-pair-code-v1\0";
/// AAD 的域分离标签（含角色顺序：id_a/fp_a 是**发起方**，id_b/fp_b 是**应答方**）。
const AAD_DOMAIN: &[u8] = b"orbdrop-pair-pake-v1";
/// 配对结果密封的域分离标签。
const SEAL_DOMAIN: &[u8] = b"orbdrop-pair-token-v1";

/// 一端在等服务端确认时持有的 SPAKE2 状态。
pub type ClientState = PartyAState<Spake2P256>;

/// 服务端一条待完成的 PAKE 会话。
pub struct PendingPake {
    pub session: String,
    /// 发起这条会话的来源地址（单来源配额用）。
    pub from: std::net::IpAddr,
    pub peer: PairedPeer,
    /// 已经算好的 SPAKE2 输出（含会话密钥与"等对方确认"的 MAC）。
    pub output: Spake2Output,
    /// 这次会话用的配对码：确认成功时消费它；失败则留到过期。
    pub code: Zeroizing<String>,
    pub created_at: std::time::Instant,
}

/// 配对成功后交给发起方的那份（用 PAKE 会话密钥密封）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TokenPayload {
    pub token: String,
    pub peer_id: String,
    pub peer_name: String,
    pub cert_fingerprint: String,
}

/// 6 位码 → P-256 标量。SHA-512 给 64 字节，`scalar_from_wide_bytes` 按群阶约减。
///
/// 这不是 KSF：码只有 20 bit，慢哈希挡不住离线枚举；PAKE 的意义正是让"离线枚举"
/// 这件事不成立，所以这里只需要一个域分离的均匀映射。
pub fn password_scalar(code: &str) -> Result<<P256Group as CpaceGroup>::Scalar, String> {
    let mut hasher = Sha512::new();
    hasher.update(CODE_DOMAIN);
    hasher.update(code.as_bytes());
    let wide = hasher.finalize();
    P256Group::scalar_from_wide_bytes(&wide).map_err(|e| format!("配对码派生失败: {e:?}"))
}

/// PAKE 的附加认证数据：会话、双方设备 id、双方证书指纹。
///
/// **角色顺序固定**：`id_a`/`fp_a` 永远是发起方（输入码的那台），`id_b`/`fp_b` 永远是
/// 应答方（显示码的那台）。两端各自按自己的角色填，不按连接方向猜。
pub fn aad(session: &str, id_a: &str, id_b: &str, fp_a: &str, fp_b: &str) -> Vec<u8> {
    let mut out = Vec::with_capacity(
        AAD_DOMAIN.len() + session.len() + id_a.len() + id_b.len() + fp_a.len() + fp_b.len() + 5,
    );
    out.extend_from_slice(AAD_DOMAIN);
    for part in [session, id_a, id_b, fp_a, fp_b] {
        out.push(0);
        out.extend_from_slice(part.as_bytes());
    }
    out
}

/// 发起方第一步：生成 `pa` 与本地状态。
pub fn start_a(
    code: &str,
    id_a: &str,
    id_b: &str,
    aad: &[u8],
) -> Result<(Vec<u8>, ClientState), String> {
    let w = password_scalar(code)?;
    PartyA::<Spake2P256>::start(
        &w,
        id_a.as_bytes(),
        id_b.as_bytes(),
        aad,
        &mut rand_core::UnwrapErr(getrandom::SysRng),
    )
    .map_err(|e| format!("PAKE 启动失败: {e}"))
}

/// 发起方第二步：吃下服务端的 `pb`，得到会话密钥与双方确认值。
pub fn finish_a(state: ClientState, pb: &[u8]) -> Result<Spake2Output, String> {
    state.finish(pb).map_err(|e| format!("PAKE 确认失败: {e}"))
}

/// 应答方：用本地配对码回一条 `pb`，并立刻算出确认值（`pa` 已经在手里）。
pub fn respond(
    code: &str,
    id_a: &str,
    id_b: &str,
    aad: &[u8],
    pa: &[u8],
) -> Result<(Vec<u8>, Spake2Output), String> {
    let w = password_scalar(code)?;
    let (pb, state) = PartyB::<Spake2P256>::start(
        &w,
        id_a.as_bytes(),
        id_b.as_bytes(),
        aad,
        &mut rand_core::UnwrapErr(getrandom::SysRng),
    )
    .map_err(|e| format!("PAKE 应答失败: {e}"))?;
    let output = state
        .finish(pa)
        .map_err(|e| format!("PAKE 报文无效: {e}"))?;
    Ok((pb, output))
}

/// 用 PAKE 会话密钥派生 AEAD 密钥（AAD 一起进 HKDF salt，证书/会话变了密钥就不同）。
fn derive_aead_key(session_key: &[u8], aad: &[u8]) -> Result<[u8; 32], String> {
    let hk = Hkdf::<Sha256>::new(Some(aad), session_key);
    let mut key = [0u8; 32];
    hk.expand(SEAL_DOMAIN, &mut key)
        .map_err(|_| "派生配对密钥失败".to_string())?;
    Ok(key)
}

/// 把配对结果密封给发起方：AES-256-GCM，密钥来自 PAKE 会话密钥。
pub fn seal(session_key: &[u8], aad: &[u8], plaintext: &[u8]) -> Result<(String, String), String> {
    let key = derive_aead_key(session_key, aad)?;
    let cipher = Aes256Gcm::new_from_slice(&key).map_err(|_| "AEAD 密钥无效".to_string())?;
    let mut nonce = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut nonce);
    let ciphertext = cipher
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: plaintext,
                aad,
            },
        )
        .map_err(|_| "密封配对结果失败".to_string())?;
    Ok((B64.encode(nonce), B64.encode(ciphertext)))
}

/// 发起方解开密封的配对结果。
pub fn open(session_key: &[u8], aad: &[u8], nonce: &str, sealed: &str) -> Result<Vec<u8>, String> {
    let key = derive_aead_key(session_key, aad)?;
    let cipher = Aes256Gcm::new_from_slice(&key).map_err(|_| "AEAD 密钥无效".to_string())?;
    let nonce = B64
        .decode(nonce)
        .map_err(|_| "nonce 格式错误".to_string())?;
    let ciphertext = B64
        .decode(sealed)
        .map_err(|_| "密封结果格式错误".to_string())?;
    cipher
        .decrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: &ciphertext,
                aad,
            },
        )
        .map_err(|_| "配对结果校验失败".to_string())
}

#[derive(Debug, Deserialize)]
struct StartReply {
    pb: String,
    confirm: String,
}

#[derive(Debug, Deserialize)]
struct SealedReply {
    nonce: String,
    sealed: String,
}

/// 发起方的完整 HTTP 流程（`pair_confirm` 与集成测试共用）。
///
/// `peer_fp` 必须是**这条 TLS 连接上实际看到**的对端证书指纹；它进 AAD，是"中间人
/// 转发 PAKE 报文"也过不了确认的那一半。
#[allow(clippy::too_many_arguments)]
pub async fn run_client(
    client: &reqwest::Client,
    base: &str,
    session: &str,
    own_id: &str,
    own_name: &str,
    own_port: u16,
    peer_id: &str,
    code: &str,
    own_fp: &str,
    peer_fp: &str,
) -> Result<TokenPayload, String> {
    let aad = aad(session, own_id, peer_id, own_fp, peer_fp);
    let (pa, state_a) = start_a(code, own_id, peer_id, &aad)?;

    let start = client
        .post(format!("{base}/pair/pake/start"))
        .timeout(HTTP_TIMEOUT)
        .json(&serde_json::json!({
            "session": session,
            // 这是**发起方自己的 id**：应答方按它登记对端、并把它作为 SPAKE2 的 id_a。
            "peerId": own_id,
            "peerName": own_name,
            "peerPort": own_port,
            "certFingerprint": own_fp,
            "pa": B64.encode(pa),
        }))
        .send()
        .await
        .map_err(|e| format!("联系不上「{peer_id}」：{e}"))?;

    // 不降级：对端没有 PAKE 端点时直接失败，让用户升级，而不是退回明文口令那条路。
    if matches!(
        start.status(),
        reqwest::StatusCode::NOT_FOUND | reqwest::StatusCode::METHOD_NOT_ALLOWED
    ) {
        return Err("对端版本过旧，无法进行安全配对；请两台设备都升级后再试".into());
    }
    if !start.status().is_success() {
        let reason = crate::net::read_text_capped(start, crate::net::CONTROL_BODY_MAX)
            .await
            .unwrap_or_default();
        return Err(if reason.trim().is_empty() {
            "配对失败".into()
        } else {
            reason
        });
    }
    let start: StartReply =
        crate::net::read_json_capped(start, crate::net::CONTROL_BODY_MAX)
            .await
            .map_err(|e| format!("对端回应无法解析：{e}"))?;
    let pb = B64
        .decode(start.pb)
        .map_err(|_| "对端的 PAKE 报文格式错误".to_string())?;
    let peer_confirm = B64
        .decode(start.confirm)
        .map_err(|_| "对端的确认值格式错误".to_string())?;

    let output = finish_a(state_a, &pb)?;
    output
        .verify_peer_confirmation(&peer_confirm)
        .map_err(|_| "配对码错误或已过期".to_string())?;

    let finish = client
        .post(format!("{base}/pair/pake/finish"))
        .timeout(HTTP_TIMEOUT)
        .json(&serde_json::json!({
            "session": session,
            "confirm": B64.encode(&output.confirmation_mac),
        }))
        .send()
        .await
        .map_err(|e| format!("配对确认没送到：{e}"))?;
    if !finish.status().is_success() {
        let reason = crate::net::read_text_capped(finish, crate::net::CONTROL_BODY_MAX)
            .await
            .unwrap_or_default();
        return Err(if reason.trim().is_empty() {
            "配对失败".into()
        } else {
            reason
        });
    }
    let sealed: SealedReply =
        crate::net::read_json_capped(finish, crate::net::CONTROL_BODY_MAX)
            .await
            .map_err(|e| format!("对端回应无法解析：{e}"))?;
    let plain = open(
        output.session_key.as_bytes(),
        &aad,
        &sealed.nonce,
        &sealed.sealed,
    )?;
    let payload: TokenPayload =
        serde_json::from_slice(&plain).map_err(|e| format!("配对结果无法解析：{e}"))?;

    // 密封层已经保证来源；再把"对端 id / 证书指纹"与本次连接对一遍，防止串线。
    if payload.peer_id != peer_id {
        return Err("对端身份与预期不一致".into());
    }
    if !payload.cert_fingerprint.eq_ignore_ascii_case(peer_fp) {
        return Err("对端证书指纹与本次连接不一致".into());
    }
    Ok(payload)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_code_round_trip() {
        let aad = aad("s1", "id-a", "id-b", "fp-a", "fp-b");
        let (pa, state_a) = start_a("483921", "id-a", "id-b", &aad).unwrap();
        let (pb, out_b) = respond("483921", "id-a", "id-b", &aad, &pa).unwrap();
        let out_a = finish_a(state_a, &pb).unwrap();
        assert_eq!(out_a.session_key.as_bytes(), out_b.session_key.as_bytes());
        out_a
            .verify_peer_confirmation(&out_b.confirmation_mac)
            .unwrap();
        out_b
            .verify_peer_confirmation(&out_a.confirmation_mac)
            .unwrap();
    }

    /// 同一个码、但两端看到的证书不同（中间人转发）：会话密钥一样，确认值必须对不上。
    #[test]
    fn a_certificate_substitution_breaks_the_confirmation() {
        let aad_a = aad("s1", "id-a", "id-b", "fp-a", "fp-m");
        let aad_b = aad("s1", "id-a", "id-b", "fp-m", "fp-b");
        let (pa, state_a) = start_a("483921", "id-a", "id-b", &aad_a).unwrap();
        let (pb, out_b) = respond("483921", "id-a", "id-b", &aad_b, &pa).unwrap();
        let out_a = finish_a(state_a, &pb).unwrap();
        assert_eq!(
            out_a.session_key.as_bytes(),
            out_b.session_key.as_bytes(),
            "码相同 → 会话密钥相同"
        );
        assert!(
            out_a
                .verify_peer_confirmation(&out_b.confirmation_mac)
                .is_err(),
            "证书不同 → AAD 不同 → 确认值必须失败"
        );
    }

    /// 码不同：会话密钥不同，确认值对不上。
    #[test]
    fn a_wrong_code_never_confirms() {
        let aad = aad("s1", "id-a", "id-b", "fp-a", "fp-b");
        let (pa, state_a) = start_a("483921", "id-a", "id-b", &aad).unwrap();
        let (pb, out_b) = respond("483929", "id-a", "id-b", &aad, &pa).unwrap();
        let out_a = finish_a(state_a, &pb).unwrap();
        assert_ne!(out_a.session_key.as_bytes(), out_b.session_key.as_bytes());
        assert!(out_a
            .verify_peer_confirmation(&out_b.confirmation_mac)
            .is_err());
    }

    #[test]
    fn token_payload_round_trips_through_seal() {
        let session_key = [7u8; 16];
        let aad_bytes = aad("s1", "id-a", "id-b", "fp-a", "fp-b");
        let payload = TokenPayload {
            token: "tok".into(),
            peer_id: "id-b".into(),
            peer_name: "PC-B".into(),
            cert_fingerprint: "fp-b".into(),
        };
        let plain = serde_json::to_vec(&payload).unwrap();
        let (nonce, sealed) = seal(&session_key, &aad_bytes, &plain).unwrap();
        let opened = open(&session_key, &aad_bytes, &nonce, &sealed).unwrap();
        assert_eq!(opened, plain);

        // 换一个会话密钥或 AAD 都解不开。
        assert!(open(&[8u8; 16], &aad_bytes, &nonce, &sealed).is_err());
        let other_aad = aad("s2", "id-a", "id-b", "fp-a", "fp-b");
        assert!(open(&session_key, &other_aad, &nonce, &sealed).is_err());
    }
}
