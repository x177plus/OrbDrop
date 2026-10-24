//! 群消息/名册签名用的设备身份。
//!
//! **为什么不用 TLS 私钥**：TLS 身份是 ECDSA P-256 的 PKCS#8，验签需要从 X.509 证书里
//! 取 SubjectPublicKeyInfo 再转成 ring 要的非压缩点；而群名册只需要一个可验证的签名者，
//! 用 Ed25519（ring 直接支持 32 字节公钥 + 64 字节签名）更简单、更小、跨平台一致。
//!
//! 这把钥匙与 TLS 身份**相互独立**：
//! - TLS 身份负责"这条连接是不是它"（按 IP/指纹钉扎）；
//! - 群身份负责"这条群消息是不是这个成员发的"（公钥在名册里，跟着名册走）。
//!
//! 首次使用时生成，落在数据目录的 `group-identity.key`（PKCS#8），权限尽量收紧。
//! 公钥会随群消息/名册一起发出去；私钥永不出本机。

use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock};

use base64::Engine as _;
use parking_lot::Mutex;
use ring::rand::SystemRandom;
use ring::signature::{Ed25519KeyPair, KeyPair, UnparsedPublicKey, ED25519};
use tracing::warn;

use crate::config::data_dir;

const KEY_FILE: &str = "group-identity.key";

pub struct GroupIdentity {
    pair: Ed25519KeyPair,
    pubkey_b64: String,
}

static CACHE: LazyLock<Mutex<Option<Arc<GroupIdentity>>>> = LazyLock::new(|| Mutex::new(None));

fn key_path() -> PathBuf {
    data_dir().join(KEY_FILE)
}

/// 读/生成群签名身份。失败时返回可读错误；调用方应把它当成"群功能不可用"而不是
/// 静默退回未签名（那会把 P0 的冒名口子重新打开）。
pub fn load() -> Result<Arc<GroupIdentity>, String> {
    let path = key_path();
    load_at(&path)
}

pub fn load_at(path: &Path) -> Result<Arc<GroupIdentity>, String> {
    if let Some(id) = CACHE.lock().as_ref() {
        return Ok(id.clone());
    }
    let pair = match std::fs::read(path) {
        Ok(bytes) => match Ed25519KeyPair::from_pkcs8(&bytes) {
            Ok(pair) => pair,
            Err(e) => {
                // 群身份是可再生的（不像 TLS 身份那样绑着所有配对）：坏文件留一份
                // `.bad`，重新生成，别让群功能整体不可用。
                let backup = path.with_extension("key.bad");
                let _ = std::fs::rename(path, &backup);
                warn!(%e, backup = %backup.display(), "群签名身份损坏，已备份并重新生成");
                generate_at(path)?
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => generate_at(path)?,
        Err(e) => return Err(format!("群签名身份读取失败（{path:?}）：{e}")),
    };
    let id = Arc::new(GroupIdentity {
        pubkey_b64: b64(pair.public_key().as_ref()),
        pair,
    });
    *CACHE.lock() = Some(id.clone());
    Ok(id)
}

fn generate_at(path: &Path) -> Result<Ed25519KeyPair, String> {
    let rng = SystemRandom::new();
    let pkcs8 =
        Ed25519KeyPair::generate_pkcs8(&rng).map_err(|e| format!("群签名身份生成失败：{e}"))?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("群签名身份目录创建失败（{parent:?}）：{e}"))?;
    }
    crate::config::write_atomic(path, pkcs8.as_ref())
        .map_err(|e| format!("群签名身份写入失败（{path:?}）：{e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).map_err(|e| format!("群签名身份解析失败：{e}"))
}

pub fn public_key_b64() -> Result<String, String> {
    Ok(load()?.pubkey_b64.clone())
}

pub fn sign_b64(data: &[u8]) -> Result<String, String> {
    let id = load()?;
    Ok(b64(id.pair.sign(data).as_ref()))
}

pub fn verify_b64(pubkey_b64: &str, data: &[u8], sig_b64: &str) -> bool {
    let Ok(pubkey) = unb64(pubkey_b64) else {
        return false;
    };
    let Ok(sig) = unb64(sig_b64) else {
        return false;
    };
    if pubkey.len() != 32 || sig.len() != 64 {
        return false;
    }
    UnparsedPublicKey::new(&ED25519, pubkey)
        .verify(data, &sig)
        .is_ok()
}

fn b64(bytes: &[u8]) -> String {
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn unb64(text: &str) -> Result<Vec<u8>, base64::DecodeError> {
    base64::engine::general_purpose::STANDARD.decode(text.trim())
}

/// 用于日志/界面：公钥很短，前 16 个 base64 字符足够认人。
pub fn short_key(pubkey_b64: &str) -> String {
    pubkey_b64.chars().take(16).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sign_and_verify_round_trip() {
        let dir = std::env::temp_dir().join(format!("orbdrop_gid_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("group-identity.key");
        let id = load_at(&path).unwrap();
        let sig = b64(id.pair.sign(b"hello").as_ref());
        assert!(verify_b64(&id.pubkey_b64, b"hello", &sig));
        assert!(!verify_b64(&id.pubkey_b64, b"hello!", &sig));
        // 第二次读同一文件必须得到同一把钥匙。
        let again = load_at(&path).unwrap();
        assert_eq!(id.pubkey_b64, again.pubkey_b64);
        let _ = std::fs::remove_dir_all(dir);
    }
}
