//! 「保存到手机」之前的准备：把要交给系统共享存储的那一份**变成原样的文件**。
//!
//! 收件里推送落盘的是裸字节，原样转发即可；但**会话图片**不是裸图片 —— 它们以封装格式
//! （[`crate::envelope`]）躺在 `chat-vault/blobs/<消息 id>.bin` 里，真正的扩展名在文件头里
//! （见 [`crate::blobs`]）。把这个路径直接交给安卓那一跳，共享存储里就会出现一个 `<id>.bin`：
//! 二进制、相册不认、双击也打不开 —— 这是真机反馈过的问题。
//!
//! 所以这里分两档：
//!
//! - **封装文件**：解出原始字节，按文件头里的扩展名 + 界面给的显示名写成一份临时文件，再拿去
//!   保存（`Pictures/OrbDrop/图片-1.png` 这种形状）；临时目录随 [`PreparedExport`] 析构删掉。
//! - **普通文件**（推送落盘的收件、暂存里选的图）：原样转发，只把显示名净化一遍。
//!
//! 只被安卓命令 `mobile::save_file_to_phone` 使用；桌面端只在测试里编译
//! （`cfg(any(mobile, test))`，见 `lib.rs`）。

use std::path::{Path, PathBuf};

/// 交给原生那一跳的东西：一条**真实的、未封装的**文件路径 + 共享存储里该用的显示名。
///
/// `cleanup` = 解封装时建的那个临时目录（析构时删）；原样转发的收件文件没有它。
pub struct PreparedExport {
    pub path: PathBuf,
    pub name: String,
    cleanup: Option<PathBuf>,
}

impl Drop for PreparedExport {
    fn drop(&mut self) {
        if let Some(dir) = self.cleanup.take() {
            let _ = std::fs::remove_dir_all(dir);
        }
    }
}

/// 看一眼这份文件该怎么交出去（见模块注释）。
///
/// `requested` = 界面上的显示名（收件项的文件名 / 会话里那张图的名字）；缺省时退回路径自己的
/// 文件名。`temp_root` 是解封装时的落点（生产上是 `<数据目录>/export`）：必须在原生那一跳
/// 认的那棵子树里（`dataDir` 下的 `OrbDrop`），否则 `AppFiles` 会拒绝。
pub fn prepare_export(
    source: &Path,
    requested: Option<&str>,
    temp_root: &Path,
) -> Result<PreparedExport, String> {
    if is_enveloped_file(source)? {
        let raw = std::fs::read(source).map_err(|e| format!("读不到这份文件：{e}"))?;
        let (ext, bytes) = crate::envelope::decode_if(&raw)?
            .ok_or_else(|| "这不是一份可保存的图片内容".to_string())?;
        let name = export_name(source, requested, Some(&ext));
        let dir = temp_root.join(format!("orbdrop-export-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).map_err(|e| format!("建临时目录失败：{e}"))?;
        let path = dir.join(&name);
        if let Err(e) = std::fs::write(&path, &bytes) {
            let _ = std::fs::remove_dir_all(&dir);
            return Err(format!("写临时文件失败：{e}"));
        }
        return Ok(PreparedExport {
            path,
            name,
            cleanup: Some(dir),
        });
    }
    Ok(PreparedExport {
        path: source.to_path_buf(),
        name: export_name(source, requested, None),
        cleanup: None,
    })
}

/// 只看头 4 个字节（`envelope::MAGIC`）：整份读进来再判会把一个 1 GB 的视频白读一遍。
fn is_enveloped_file(path: &Path) -> Result<bool, String> {
    use std::io::Read as _;
    let mut file = std::fs::File::open(path).map_err(|e| format!("读不到这份文件：{e}"))?;
    let mut magic = [0u8; 4];
    match file.read_exact(&mut magic) {
        Ok(()) => Ok(magic == crate::envelope::MAGIC),
        // 比魔数还短的文件不可能是封装文件（空文件也一样）。
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => Ok(false),
        Err(e) => Err(format!("读不到这份文件：{e}")),
    }
}

/// 共享存储里该用的名字：优先界面给的显示名，退回路径的文件名；只留最后一段（显示名可能来自
/// 对端，`../x` 或 `a/b` 不能变成目录），再过一遍落盘那套净化（`sanitize_file_name`）。
///
/// `ext` 有值 = 这是封装内容，**文件头里的扩展名才是真相**：名字里带的是别的扩展名就换掉
/// （否则会出现 `.png` 的文件里装着 JPEG 字节），名字里没有扩展名就补上。
fn export_name(source: &Path, requested: Option<&str>, ext: Option<&str>) -> String {
    let raw = requested
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .or_else(|| {
            source
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
        })
        .unwrap_or_else(|| "文件".to_string());
    let leaf = raw.rsplit(['/', '\\']).next().unwrap_or(&raw);
    let mut name = crate::transfer::resume::sanitize_file_name(leaf);
    if name.is_empty() {
        name = "文件".to_string();
    }
    if let Some(ext) = ext.map(str::trim).filter(|e| !e.is_empty()) {
        let stem = match name.rsplit_once('.') {
            Some((stem, _)) if !stem.is_empty() => stem.to_string(),
            _ => name,
        };
        name = format!("{stem}.{ext}");
    }
    name
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// 每个用例自己的临时根（不碰真实数据目录）。
    fn temp_root(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("orbdrop_export_{tag}_{}", uuid::Uuid::new_v4()))
    }

    fn write_blob(root: &Path, id: &str, ext: &str, bytes: &[u8]) -> PathBuf {
        let path = root.join(format!("{id}.bin"));
        fs::write(&path, crate::envelope::encode(ext, bytes)).unwrap();
        path
    }

    /// 会话图片（封装文件）必须**解出原始字节**、按显示名落成一份临时文件 —— 原样转发的话
    /// 系统相册里就是一个打不开的 `<id>.bin`（真机反馈的那个 bug）。
    #[test]
    fn wrapped_chat_image_is_unwrapped_and_renamed() {
        let root = temp_root("wrapped");
        fs::create_dir_all(&root).unwrap();
        let blob = write_blob(&root, "m1", "png", b"PNG-BYTES");

        let prepared = prepare_export(&blob, Some("图片-1.png"), &root).unwrap();
        assert_eq!(prepared.name, "图片-1.png");
        assert_eq!(fs::read(&prepared.path).unwrap(), b"PNG-BYTES");
        assert_ne!(prepared.path, blob, "不能把封装文件原样交出去");
        assert!(prepared.path.starts_with(&root));

        let dir = prepared.cleanup.clone().expect("封装内容要落一份临时文件");
        drop(prepared);
        assert!(!dir.exists(), "保存完临时目录要清掉");
        fs::remove_dir_all(&root).unwrap();
    }

    /// 文件头里的扩展名才是内容真正的格式：显示名写错了扩展名要换掉。
    #[test]
    fn envelope_ext_wins_over_a_wrong_display_name() {
        let root = temp_root("ext");
        fs::create_dir_all(&root).unwrap();
        let blob = write_blob(&root, "m2", "jpg", b"JPEG-BYTES");

        let prepared = prepare_export(&blob, Some("图片.png"), &root).unwrap();
        assert_eq!(prepared.name, "图片.jpg");
        fs::remove_dir_all(&root).unwrap();
    }

    /// 显示名没有扩展名时补上（否则媒体库会按二进制给它安一个 `.bin`）。
    #[test]
    fn missing_extension_is_filled_from_the_envelope() {
        let root = temp_root("noext");
        fs::create_dir_all(&root).unwrap();
        let blob = write_blob(&root, "m3", "gif", b"GIF-BYTES");

        let prepared = prepare_export(&blob, Some("图片"), &root).unwrap();
        assert_eq!(prepared.name, "图片.gif");
        fs::remove_dir_all(&root).unwrap();
    }

    /// 收件里的普通文件（推送落盘的裸字节）原样转发，不复制、不改名。
    #[test]
    fn plain_inbox_file_is_forwarded_untouched() {
        let root = temp_root("plain");
        fs::create_dir_all(&root).unwrap();
        let raw = root.join("a.jpg");
        fs::write(&raw, b"RAW-JPEG").unwrap();

        let prepared = prepare_export(&raw, Some("a.jpg"), &root).unwrap();
        assert_eq!(prepared.name, "a.jpg");
        assert_eq!(prepared.path, raw);
        assert!(prepared.cleanup.is_none());
        fs::remove_dir_all(&root).unwrap();
    }

    /// 对端给的显示名不能把临时文件带出目录（只留最后一段）。
    #[test]
    fn display_name_cannot_escape_the_temp_dir() {
        let root = temp_root("escape");
        fs::create_dir_all(&root).unwrap();
        let blob = write_blob(&root, "m4", "png", b"X");

        let prepared = prepare_export(&blob, Some("../../evil.png"), &root).unwrap();
        assert_eq!(prepared.name, "evil.png");
        assert_eq!(
            prepared.path.parent(),
            Some(prepared.cleanup.as_deref().unwrap())
        );
        fs::remove_dir_all(&root).unwrap();
    }

    /// 比魔数还短的文件（空文件/小文件）不是封装文件：按普通文件转发。
    #[test]
    fn short_file_is_not_treated_as_enveloped() {
        let root = temp_root("short");
        fs::create_dir_all(&root).unwrap();
        let raw = root.join("tiny");
        fs::write(&raw, b"x").unwrap();

        let prepared = prepare_export(&raw, Some("tiny.bin"), &root).unwrap();
        assert_eq!(prepared.path, raw);
        assert!(prepared.cleanup.is_none());
        fs::remove_dir_all(&root).unwrap();
    }
}