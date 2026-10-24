//! 会话数据的**文件封装**：让 `chat.dat` / `groups.dat` / `*.bin` 双击或用记事本打开
//! 时看不到明文。
//!
//! **这不是加密，也不该被当成加密**：
//! - 掩码 [`MASK`] 就写在源码里，任何人拿到程序或仓库都能还原；
//! - 没有密钥派生、没有随机数、没有完整性校验；
//! - 它只满足"随手翻目录/双击看不到聊天内容"，挡不住拷贝、取证或恶意程序。
//!
//! 真要保护静态数据，只能把随机密钥交给系统钥匙串（DPAPI / Keychain / Keystore）并用
//! AEAD；那一套的边界与成本见项目文档，不要在这层上面继续叠"更复杂的假加密"。
//!
//! 文件形状（全部小端）：
//!
//! ```text
//! "ODB1" | version u8 | ext_len u8 | ext | plain_len u32 | payload ^ MASK
//! ```
//!
//! `ext` 只给图片用（png/jpg/...）：图片文件名统一是 `<消息 id>.bin`，预览时靠头里的
//! 扩展名决定 MIME。账本的 `ext` 为空。

use std::io;
use std::path::{Path, PathBuf};

use tracing::{info, warn};

/// 封装文件的魔数。它本身是明文；看到它只说明"这是封装过的文件"，不泄露聊天内容。
pub const MAGIC: [u8; 4] = *b"ODB1";

/// 当前封装版本。
const VERSION: u8 = 1;

/// 固定掩码（32B）。**不是密钥**：源码里能直接读到，只用来让内容不再是可读文本。
const MASK: [u8; 32] = [
    0x9e, 0x37, 0x4b, 0xd2, 0x68, 0x1f, 0xa5, 0x73, 0xc8, 0x2d, 0x5a, 0x91, 0x0c, 0xe4, 0x7f, 0x36,
    0xbd, 0x58, 0x21, 0x8a, 0xf3, 0x6c, 0x14, 0xd7, 0x49, 0xa0, 0x2b, 0x75, 0x8e, 0x63, 0x1a, 0xc5,
];

/// 文件头的最大长度（`ext` 最长 255 字节）。
pub const MAX_OVERHEAD: u64 = 4 + 1 + 1 + 255 + 4;

/// 数据目录下存放封装账本与图片的目录。
pub const VAULT_DIR: &str = "chat-vault";

/// 封装后的会话账本。
pub fn chat_path(data_dir: &Path) -> PathBuf {
    vault_dir(data_dir).join("chat.dat")
}

/// 封装后的群聊账本。
pub fn groups_path(data_dir: &Path) -> PathBuf {
    vault_dir(data_dir).join("groups.dat")
}

/// 图片内容目录（`chat-vault/blobs/`）。
pub fn blobs_dir(data_dir: &Path) -> PathBuf {
    vault_dir(data_dir).join("blobs")
}

/// 数据目录下的封装根目录。
pub fn vault_dir(data_dir: &Path) -> PathBuf {
    data_dir.join(VAULT_DIR)
}

/// 把一段明文封装成文件字节。
pub fn encode(ext: &str, plain: &[u8]) -> Vec<u8> {
    assert!(ext.len() <= u8::MAX as usize, "扩展名太长");
    assert!(ext.is_ascii(), "扩展名必须是 ASCII");
    assert!(
        plain.len() <= u32::MAX as usize,
        "单份会话数据超过 4 GB，封装格式装不下"
    );
    let mut out = Vec::with_capacity(6 + ext.len() + 4 + plain.len());
    out.extend_from_slice(&MAGIC);
    out.push(VERSION);
    out.push(ext.len() as u8);
    out.extend_from_slice(ext.as_bytes());
    out.extend_from_slice(&(plain.len() as u32).to_le_bytes());
    out.extend(
        plain
            .iter()
            .enumerate()
            .map(|(i, b)| b ^ MASK[i % MASK.len()]),
    );
    out
}

/// 这段字节是不是封装文件（只看魔数）。
pub fn is_enveloped(bytes: &[u8]) -> bool {
    bytes.len() >= MAGIC.len() && bytes[..MAGIC.len()] == MAGIC
}

/// 解封装：非封装文件返回 `None`（老版本的明文，迁移/测试时还要读）。
///
/// 魔数对得上但头/长度不对 → `Err`：这是损坏，不能当明文继续解析。
pub fn decode_if(bytes: &[u8]) -> Result<Option<(String, Vec<u8>)>, String> {
    if !is_enveloped(bytes) {
        return Ok(None);
    }
    if bytes.len() < 6 {
        return Err("封装文件头不完整".into());
    }
    let version = bytes[4];
    if version != VERSION {
        return Err(format!("不认识的封装版本：{version}"));
    }
    let ext_len = bytes[5] as usize;
    let header_len = 6 + ext_len + 4;
    if bytes.len() < header_len {
        return Err("封装文件头不完整".into());
    }
    let ext_bytes = &bytes[6..6 + ext_len];
    if !ext_bytes.is_ascii() {
        return Err("封装文件头里的扩展名不是 ASCII".into());
    }
    let ext = String::from_utf8(ext_bytes.to_vec()).map_err(|_| "扩展名不是 UTF-8".to_string())?;
    let len_at = 6 + ext_len;
    let want = u32::from_le_bytes(
        bytes[len_at..len_at + 4]
            .try_into()
            .map_err(|_| "长度字段不完整".to_string())?,
    ) as usize;
    let payload = &bytes[header_len..];
    if payload.len() != want {
        return Err(format!(
            "封装文件长度对不上：头里写 {want}，实际 {}",
            payload.len()
        ));
    }
    let plain = payload
        .iter()
        .enumerate()
        .map(|(i, b)| b ^ MASK[i % MASK.len()])
        .collect();
    Ok(Some((ext, plain)))
}

/// 读一个会话文件并解封装；老版本的明文原样返回。
pub fn read_decoded(path: &Path) -> io::Result<Vec<u8>> {
    let raw = std::fs::read(path)?;
    match decode_if(&raw) {
        Ok(Some((_, plain))) => Ok(plain),
        Ok(None) => Ok(raw),
        Err(e) => Err(io::Error::new(io::ErrorKind::InvalidData, e)),
    }
}

/// 把损坏的文件留一份 `.bad.<uuid>` 再从空库启动。
///
/// **备份也必须是封装后的**：如果原文件是老版本的明文，直接 rename 会留下一份双击就能
/// 读到的聊天记录，正好绕开这层封装。写成功才删原文件；写不成就保留原文件。
pub fn backup_bad(path: &Path, raw: &[u8]) -> Option<PathBuf> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "chat.bad".to_string());
    let backup = path.with_file_name(format!("{name}.bad.{}", uuid::Uuid::new_v4()));
    let out = if is_enveloped(raw) {
        raw.to_vec()
    } else {
        encode("", raw)
    };
    match std::fs::write(&backup, out) {
        Ok(()) => {
            let _ = std::fs::remove_file(path);
            Some(backup)
        }
        Err(_) => None,
    }
}

/// 首启/升级迁移的结果。
#[derive(Debug, Default)]
pub struct Migration {
    /// `chat.dat` 可用（存在且能解开，或本来就没有旧账本）。
    pub chat_ready: bool,
    /// `groups.dat` 可用。
    pub groups_ready: bool,
    /// 成功搬过去的旧文件数（账本 + 图片 + `.bad`/`.bak`）。
    pub migrated: usize,
    pub errors: Vec<String>,
}

/// 把老版本的明文数据搬进 `chat-vault/`。
///
/// 幂等：目标已经可用就只清理旧文件；迁移中途崩溃，下次启动继续搬。任何一步失败都保留
/// 旧文件并记进 `errors`，绝不在没校验新文件的情况下删旧文件。
pub fn migrate(data_dir: &Path) -> Migration {
    let mut out = Migration::default();
    let vault = vault_dir(data_dir);
    let blobs = blobs_dir(data_dir);
    if let Err(e) = std::fs::create_dir_all(&blobs) {
        out.errors.push(format!("建 chat-vault 目录失败：{e}"));
        return out;
    }

    out.chat_ready = migrate_ledger(
        &data_dir.join("chat.json"),
        &chat_path(data_dir),
        &vault,
        "chat",
        &mut out,
    );
    out.groups_ready = migrate_ledger(
        &data_dir.join("groups.json"),
        &groups_path(data_dir),
        &vault,
        "groups",
        &mut out,
    );
    // 账本没全部就绪时，回退会继续用旧目录；这时旧图片一个都不能删（哪怕已经复制成功）。
    let ledgers_ready = out.chat_ready && out.groups_ready;
    migrate_blobs(
        &data_dir.join("chat-blobs"),
        &blobs,
        &vault,
        ledgers_ready,
        &mut out,
    );
    migrate_artifacts(data_dir, &vault, &mut out);

    if out.migrated > 0 || !out.errors.is_empty() {
        info!(
            migrated = out.migrated,
            errors = out.errors.len(),
            "会话数据已切到 chat-vault"
        );
    }
    for e in &out.errors {
        warn!(%e, "会话数据迁移未完成");
    }
    out
}

fn migrate_ledger(src: &Path, dst: &Path, vault: &Path, label: &str, out: &mut Migration) -> bool {
    // 目标已经在：能解开就算完成；旧文件（降级期间写的明文 / 老版本留下的）收进
    // `legacy/`，不静默删除。
    if dst.is_file() {
        match std::fs::read(dst) {
            Ok(raw) => match decode_if(&raw) {
                Ok(Some(_)) => {
                    if src.is_file() {
                        archive_file(src, vault, label, out);
                    }
                    return true;
                }
                Ok(None) => {
                    // 有人把明文直接写在目标路径上：就地封装，别留一份可读文本。
                    let raw = raw;
                    let wrapped = encode("", &raw);
                    if write_verified(dst, &wrapped, &raw) {
                        out.migrated += 1;
                        if src.is_file() {
                            archive_file(src, vault, label, out);
                        }
                        return true;
                    }
                    out.errors
                        .push(format!("{label}：目标文件已存在但不是封装格式，重写失败"));
                    return false;
                }
                Err(e) => {
                    // 目标损坏：先把它收进 legacy，再用旧的明文重建。
                    archive_raw(dst, &raw, vault, &format!("{label}-corrupt"), out);
                    if !src.is_file() {
                        out.errors
                            .push(format!("{label}：封装文件损坏，且没有旧文件可恢复：{e}"));
                        return false;
                    }
                }
            },
            Err(e) => {
                out.errors.push(format!("{label}：读不了目标文件：{e}"));
                return false;
            }
        }
    }

    if !src.is_file() {
        // 没有旧账本：空库也是可用状态。
        return true;
    }
    let raw = match std::fs::read(src) {
        Ok(v) => v,
        Err(e) => {
            out.errors.push(format!("{label}：读不了旧文件：{e}"));
            return false;
        }
    };
    let wrapped = if is_enveloped(&raw) {
        raw.clone()
    } else {
        encode("", &raw)
    };
    let plain = match decode_if(&wrapped) {
        Ok(Some((_, p))) => p,
        _ => {
            out.errors.push(format!("{label}：旧文件无法封装"));
            return false;
        }
    };
    if !write_verified(dst, &wrapped, &plain) {
        out.errors.push(format!("{label}：写新文件失败"));
        return false;
    }
    if let Err(e) = std::fs::remove_file(src) {
        // 新文件已经校验通过；旧文件删不掉只会留下明文，必须报出来。
        out.errors.push(format!(
            "{label}：新文件已就绪，但旧文件删不掉（{}）：{e}",
            src.display()
        ));
        return true;
    }
    out.migrated += 1;
    true
}

fn migrate_blobs(src_dir: &Path, dst_dir: &Path, vault: &Path, cleanup: bool, out: &mut Migration) {
    let Ok(rd) = std::fs::read_dir(src_dir) else {
        return;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        let Some((id, _)) = name.rsplit_once('.') else {
            out.errors
                .push(format!("图片内容文件名不认识，留在原处：{name}"));
            continue;
        };
        let Some(dst) = crate::blobs::path_in(dst_dir, id, &name) else {
            out.errors
                .push(format!("图片内容文件名不认识，留在原处：{name}"));
            continue;
        };
        let raw = match std::fs::read(&path) {
            Ok(v) => v,
            Err(e) => {
                out.errors.push(format!("读不了图片内容 {name}：{e}"));
                continue;
            }
        };
        let ext = crate::paste::image_ext_of(&name).unwrap_or_default();
        let wrapped = if is_enveloped(&raw) {
            raw.clone()
        } else {
            encode(&ext, &raw)
        };
        let plain = match decode_if(&wrapped) {
            Ok(Some((_, p))) => p,
            _ => {
                out.errors.push(format!("图片内容无法封装：{name}"));
                continue;
            }
        };
        if dst.is_file() {
            // 同 id 的已迁过；内容一致才删旧文件，不一致就收进 legacy。
            let same = std::fs::read(&dst)
                .ok()
                .and_then(|b| decode_if(&b).ok().flatten())
                .is_some_and(|(_, p)| p == plain);
            if same {
                if cleanup {
                    let _ = std::fs::remove_file(&path);
                }
            } else {
                archive_file(&path, vault, "blob-conflict", out);
            }
            continue;
        }
        if write_verified(&dst, &wrapped, &plain) {
            out.migrated += 1;
            if cleanup && std::fs::remove_file(&path).is_err() {
                out.errors
                    .push(format!("图片内容已迁移但旧文件删不掉：{name}"));
            }
        } else {
            out.errors.push(format!("图片内容写入失败：{name}"));
        }
    }
    // 只在真的空了的时候删目录；不认识的文件留在原处，别替用户丢。
    if cleanup
        && std::fs::read_dir(src_dir)
            .map(|mut it| it.next().is_none())
            .unwrap_or(false)
    {
        let _ = std::fs::remove_dir(src_dir);
    }
}

/// `.bad` / `.bak` / 残留临时文件：`.bad`、`.bak` 也封装后收进 `legacy/`；临时文件直接删。
fn migrate_artifacts(data_dir: &Path, vault: &Path, out: &mut Migration) {
    let Ok(rd) = std::fs::read_dir(data_dir) else {
        return;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        if !path.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if name.ends_with(".orbdrop-tmp") {
            let _ = std::fs::remove_file(&path);
            continue;
        }
        let legacy_artifact = (name.starts_with("chat.json.") || name.starts_with("groups.json."))
            && !name.ends_with(".orbdrop-tmp");
        if !legacy_artifact {
            continue;
        }
        archive_file(&path, vault, "artifact", out);
    }
}

fn write_verified(path: &Path, wrapped: &[u8], plain: &[u8]) -> bool {
    if let Some(parent) = path.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return false;
        }
    }
    if crate::config::write_atomic(path, wrapped).is_err() {
        return false;
    }
    // 迁移会**删掉旧文件**：新文件至少要先落稳，别让断电把唯一一份数据留在缓存里。
    if let Ok(file) = std::fs::File::open(path) {
        let _ = file.sync_all();
    }
    match std::fs::read(path) {
        Ok(raw) => decode_if(&raw)
            .ok()
            .flatten()
            .is_some_and(|(_, got)| got == plain),
        Err(_) => false,
    }
}

fn archive_file(src: &Path, vault: &Path, label: &str, out: &mut Migration) {
    let raw = match std::fs::read(src) {
        Ok(v) => v,
        Err(e) => {
            out.errors
                .push(format!("读不了旧文件 {}：{e}", src.display()));
            return;
        }
    };
    archive_raw(src, &raw, vault, label, out);
}

fn archive_raw(src: &Path, raw: &[u8], vault: &Path, label: &str, out: &mut Migration) {
    let dir = vault.join("legacy");
    if std::fs::create_dir_all(&dir).is_err() {
        out.errors
            .push(format!("建 legacy 目录失败，保留 {}", src.display()));
        return;
    }
    let name = src
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "legacy".to_string());
    let dst = dir.join(format!(
        "{label}-{}-{}.dat",
        name.replace(['/', '\\'], "_"),
        uuid::Uuid::new_v4()
    ));
    let wrapped = if is_enveloped(raw) {
        raw.to_vec()
    } else {
        encode("", raw)
    };
    // 这里不能要求"能解密成合法数据"：`legacy` 正是收损坏文件的地方。
    // 只校验写出去的字节与源文件一致。
    let copied = crate::config::write_atomic(&dst, &wrapped).is_ok()
        && std::fs::read(&dst).is_ok_and(|got| got == wrapped);
    if copied {
        let _ = std::fs::remove_file(src);
        out.migrated += 1;
    } else {
        out.errors
            .push(format!("旧文件收不进 legacy，保留原处：{}", src.display()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("orbdrop_envelope_{tag}_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn round_trip_keeps_ext_and_payload() {
        let plain = "聊天记录：你好".as_bytes();
        let wrapped = encode("png", plain);
        assert!(is_enveloped(&wrapped));
        let (ext, got) = decode_if(&wrapped).unwrap().unwrap();
        assert_eq!(ext, "png");
        assert_eq!(got, plain);
    }

    #[test]
    fn encoded_bytes_do_not_expose_the_plaintext() {
        let plain = b"hello world, this is a chat message";
        let wrapped = encode("", plain);
        assert!(
            !wrapped.windows(plain.len()).any(|w| w == plain),
            "封装后不应出现整段明文"
        );
    }

    #[test]
    fn plaintext_is_left_alone_for_legacy_reads() {
        assert!(decode_if(b"{\"convos\":{}}").unwrap().is_none());
        assert_eq!(
            read_decoded_from_bytes(b"{\"convos\":{}}"),
            b"{\"convos\":{}}"
        );
    }

    fn read_decoded_from_bytes(bytes: &[u8]) -> Vec<u8> {
        match decode_if(bytes).unwrap() {
            Some((_, p)) => p,
            None => bytes.to_vec(),
        }
    }

    #[test]
    fn a_corrupt_header_is_an_error_not_plaintext() {
        let mut wrapped = encode("", b"secret");
        wrapped[5] = 200; // 扩展名长度越界
        assert!(decode_if(&wrapped).is_err());

        let mut cut = encode("", b"secret");
        cut.truncate(cut.len() - 1);
        assert!(decode_if(&cut).is_err());
    }

    #[test]
    fn migration_moves_ledgers_blobs_and_artifacts() {
        let dir = temp_dir("migrate");
        let chat = br#"{"convos":{}}"#;
        let groups = br#"{"version":2,"groups":[],"convos":{}}"#;
        std::fs::write(dir.join("chat.json"), chat).unwrap();
        std::fs::write(dir.join("groups.json"), groups).unwrap();
        std::fs::create_dir_all(dir.join("chat-blobs")).unwrap();
        std::fs::write(dir.join("chat-blobs").join("m1.png"), b"PNGDATA").unwrap();
        std::fs::write(dir.join("chat.json.bad.old"), b"broken").unwrap();
        std::fs::write(dir.join("chat.json.123.4.orbdrop-tmp"), b"temp").unwrap();

        let report = migrate(&dir);
        assert!(report.chat_ready, "{report:?}");
        assert!(report.groups_ready, "{report:?}");
        assert!(report.errors.is_empty(), "{report:?}");

        let chat_bytes = std::fs::read(chat_path(&dir)).unwrap();
        assert_eq!(decode_if(&chat_bytes).unwrap().unwrap().1, chat);
        assert!(!dir.join("chat.json").exists(), "旧账本应清掉");

        let group_bytes = std::fs::read(groups_path(&dir)).unwrap();
        assert_eq!(decode_if(&group_bytes).unwrap().unwrap().1, groups);

        let blob = blobs_dir(&dir).join("m1.bin");
        let (ext, data) = decode_if(&std::fs::read(&blob).unwrap()).unwrap().unwrap();
        assert_eq!(ext, "png");
        assert_eq!(data, b"PNGDATA");
        assert!(!dir.join("chat-blobs").exists(), "旧图片目录应清掉");

        // .bad 收进 legacy 且也是封装后的；临时文件删掉。
        assert!(!dir.join("chat.json.bad.old").exists());
        assert!(!dir.join("chat.json.123.4.orbdrop-tmp").exists());
        let legacy: Vec<_> = std::fs::read_dir(vault_dir(&dir).join("legacy"))
            .unwrap()
            .flatten()
            .collect();
        assert_eq!(legacy.len(), 1);
        let legacy_bytes = std::fs::read(legacy[0].path()).unwrap();
        assert_eq!(decode_if(&legacy_bytes).unwrap().unwrap().1, b"broken");

        // 再跑一遍：幂等，不报错。
        let again = migrate(&dir);
        assert!(again.chat_ready && again.groups_ready, "{again:?}");
        assert!(again.errors.is_empty(), "{again:?}");
    }

    #[test]
    fn an_existing_vault_wins_and_downgrade_data_is_archived() {
        let dir = temp_dir("downgrade");
        std::fs::create_dir_all(blobs_dir(&dir)).unwrap();
        let old = encode("", br#"{"convos":{"a":{"messages":[]}}}"#);
        std::fs::write(chat_path(&dir), &old).unwrap();
        std::fs::write(
            dir.join("chat.json"),
            br#"{"convos":{"b":{"messages":[]}}}"#,
        )
        .unwrap();

        let report = migrate(&dir);
        assert!(report.chat_ready, "{report:?}");
        assert!(!dir.join("chat.json").exists());
        assert_eq!(
            decode_if(&std::fs::read(chat_path(&dir)).unwrap())
                .unwrap()
                .unwrap()
                .1,
            decode_if(&old).unwrap().unwrap().1,
            "原 vault 不能被降级数据覆盖"
        );
        let legacy = std::fs::read_dir(vault_dir(&dir).join("legacy")).unwrap();
        assert_eq!(legacy.count(), 1);
    }
}
