//! 会话里**图片消息的内容**：`<数据目录>/chat-blobs/<消息 id>.<扩展名>`。
//!
//! 为什么图片不塞进 `chat.json`：那份文件是**每改一条消息就整份重写**的（见 `chat.rs::persist`），
//! 一张几 MB 的图进去之后，之后每说一句话都要重写一遍它。所以消息本体里只留元数据
//! （是不是图片、名字、字节数），字节单独躺在一份文件里。
//!
//! 为什么也不走推送那条路（`transfer`）：需求把图片当**消息内容** —— 对端直接就能看，
//! 没有"接收 / 同意"那一步，也不该进"传输中"那本账。所以字节随会话消息一起过去
//! （`WsEvent::ChatImage` 里内联 base64），两端各自在本地留一份：
//!
//! - **发送方**：发之前先把那张图从暂存目录落成这里的一份（之后补发、重启、群里给别人补拉
//!   都还要用它）；
//! - **接收方**：收到就把字节写下来（重投幂等：同一个 id 再进来一次不重写）。
//!
//! 删除的时机只有三处：消息被裁剪掉（`chat::prune` / `group::prune`）、清空某个会话的记录、
//! 以及启动时扫一遍孤儿（`sweep`）—— 除此之外它一直在，聊天记录里那张图才一直在。
//!
//! 这一层不认识"会话"（也不该认识）：目录由调用方给（`dir()`），单元测试因此可以指到
//! 自己的临时目录去 —— 与 `ChatStore::with_path` 同一个路数（不碰真实数据目录）。

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use crate::config::data_dir;

/// 图片消息的最大字节数。
///
/// 为什么有上限：字节是**内联**在一条消息里过去的（base64 之后 1.33 倍），一端的内存、
/// 一条 WS 帧、一个 HTTP 请求体都得装下它。超了就如实拒绝 —— 不要偷偷改走推送那条路
/// （那会弹出"对方要不要接收"，正是这个需求要去掉的东西）。截图、照片都在这一档以下。
pub const MAX_IMAGE_BYTES: u64 = 8 * 1024 * 1024;

/// 消息内容（图片）的目录。
pub fn dir() -> PathBuf {
    data_dir().join("chat-blobs")
}

/// 这个 id 能不能拿来当文件名。
///
/// **收到的 id 是别人给的**：不判一遍，"../../x" 就能让对端往本机任意路径写文件。
/// 消息 id 一直是 uuid（`Uuid::new_v4()`），所以这条闸对真实流量从不拦人。
fn safe_id(id: &str) -> Option<&str> {
    let ok = !id.is_empty()
        && id.len() <= 64
        && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_');
    ok.then_some(id)
}

/// 一条消息的**内容**：一段文字，或一张图。
///
/// 图片是**消息内容**（不是推送文件）：对端直接就能看，没有"接收 / 同意"那一步 —— 所以它
/// 与文字走同一条会话通道（`WsEvent::ChatImage` / `ChatText`）。字节放在 [`dir`] 那个目录里
/// （id 就是消息 id），落盘的账本里只留元数据；1:1 与群聊两个账本共用这一份。
#[derive(Debug, Clone, Default)]
pub struct Content {
    pub text: String,
    /// `Some((显示名, 字节数))` = 图片消息；`None` = 文字消息。
    pub image: Option<(String, u64)>,
}

impl Content {
    /// 一段文字。
    pub fn text(text: impl Into<String>) -> Self {
        Self { text: text.into(), image: None }
    }

    /// 一张图（内容已经落进 [`dir`] 了）。
    pub fn image(name: impl Into<String>, size: u64) -> Self {
        Self { text: String::new(), image: Some((name.into(), size)) }
    }

    /// 这条是不是图片消息。
    pub fn is_image(&self) -> bool {
        self.image.is_some()
    }
}

/// 一份账本（`chat.json` / `groups.json`）旁边那个内容目录：`<它的目录>/chat-blobs`。
///
/// 两份账本在同一个数据目录里，所以推出来的是**同一个**目录（见 `GroupStore` 的类型说明）；
/// 测试用各自临时目录里的账本 → 内容也跟着落到各自的临时目录（不碰真实数据）。
pub fn root_of(ledger: &Path) -> PathBuf {
    ledger
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("chat-blobs")
}

/// 一段 base64 的内容 → 落成这条消息的内容文件。
///
/// 收方那几条路共用它（一对一的 `ChatImage`、群里直发的 `GroupText`、补拉的 `GroupSyncPost`）：
/// **解码、过上限、落盘**三步只有这一处实现。回 `Err` 时调用方**不许记账** —— 宁可漏一条，
/// 也不在本机留一条打不开的空图。
pub fn save_base64(root: &Path, id: &str, name: &str, data: &str) -> Result<PathBuf, String> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data.trim())
        .map_err(|e| format!("图片内容解码失败：{e}"))?;
    if bytes.len() as u64 > MAX_IMAGE_BYTES {
        return Err(format!(
            "这条图片超过了能收的上限（{} MB）",
            MAX_IMAGE_BYTES / (1024 * 1024)
        ));
    }
    save_in(root, id, name, &bytes)
}

/// 一批 `(id, 名字)` 的内容文件一起删掉（裁剪 / 清空 / 丢掉攒着的那几条）。
///
/// 认不出图片扩展名的直接跳过（文字消息本来就没有内容文件）—— 调用方不用自己先过一遍。
pub fn remove_many<'a>(root: &Path, items: impl IntoIterator<Item = (&'a str, &'a str)>) -> usize {
    let mut removed = 0usize;
    for (id, name) in items {
        if let Some(path) = path_in(root, id, name) {
            if std::fs::remove_file(path).is_ok() {
                removed += 1;
            }
        }
    }
    removed
}

impl From<&str> for Content {
    fn from(text: &str) -> Self {
        Self::text(text)
    }
}

impl From<String> for Content {
    fn from(text: String) -> Self {
        Self::text(text)
    }
}

/// 一条图片消息的内容文件路径（`id` 不可用、或名字里判不出图片扩展名时 = `None`）。
///
/// 扩展名取自消息带过来的**显示名**，所以同样要过一遍名单 —— 它是唯一进文件名的那部分。
pub fn path_in(root: &Path, id: &str, name: &str) -> Option<PathBuf> {
    let id = safe_id(id)?;
    let ext = crate::paste::image_ext_of(name)?;
    Some(root.join(format!("{id}.{ext}")))
}

/// 把字节落成这条消息的内容（发送方与接收方都走它）。
pub fn save_in(root: &Path, id: &str, name: &str, bytes: &[u8]) -> Result<PathBuf, String> {
    let path = path_in(root, id, name).ok_or_else(|| format!("这条图片消息的名字不可用：{name}"))?;
    if bytes.is_empty() {
        return Err("图片内容为空".into());
    }
    std::fs::create_dir_all(root).map_err(|e| format!("建图片内容目录失败：{e}"))?;
    // **原子写**（先写同目录的临时名，再 rename）：重投一条已经在界面上的消息时，正在渲染
    // 那张图的那一帧不该看见一份写了一半的文件（`config::write_atomic` 就是为这个备的）。
    crate::config::write_atomic(&path, bytes).map_err(|e| format!("写图片内容失败：{e}"))?;
    Ok(path)
}

/// 读回一条图片消息的内容（补发、群里给别人补拉时用）。
pub fn read_in(root: &Path, id: &str, name: &str) -> Option<Vec<u8>> {
    std::fs::read(path_in(root, id, name)?).ok()
}

/// 那条内容此刻在不在盘上。
pub fn exists_in(root: &Path, id: &str, name: &str) -> bool {
    path_in(root, id, name).is_some_and(|p| p.is_file())
}

/// 丢掉一条消息的内容（裁剪 / 清空记录）。
pub fn remove_in(root: &Path, id: &str, name: &str) {
    if let Some(path) = path_in(root, id, name) {
        let _ = std::fs::remove_file(path);
    }
}

/// 清掉**没有任何消息引用**的内容文件。
///
/// 会留下的孤儿有三种：写了一半进程就没了、裁剪时正好没删干净、以及旧版本留下的。
/// `keep` = 两份账本（`chat.json` 的 1:1 与 `groups.json` 的群聊）里还引用着的那些 id。
pub fn sweep_in(root: &Path, keep: &HashSet<String>) -> usize {
    let Ok(rd) = std::fs::read_dir(root) else {
        return 0;
    };
    let mut removed = 0usize;
    for entry in rd.flatten() {
        let Ok(meta) = entry.metadata() else {
            continue;
        };
        if !meta.is_file() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        // 文件名是 `<id>.<ext>`，引用的判据只看 id 那一半。
        let id = name.rsplit_once('.').map(|(id, _)| id).unwrap_or(&name);
        if keep.contains(id) {
            continue;
        }
        if std::fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("orbdrop_blobs_{tag}_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    // 别人给的 id 与名字都要过闸：路径片段一律拼不出文件路径来。
    #[test]
    fn ids_and_names_cannot_escape_the_blob_dir() {
        let root = Path::new("C:/blobs");
        assert!(path_in(root, "9f1c-abc_2", "图片-1.png").is_some());
        assert!(path_in(root, "../../evil", "a.png").is_none());
        assert!(path_in(root, "a/b", "a.png").is_none());
        assert!(path_in(root, "", "a.png").is_none());
        assert!(path_in(root, "a", "../../x.png").is_some_and(|p| p.parent() == Some(root)));
        assert!(path_in(root, "a", "x.exe").is_none());
        assert!(path_in(root, "a", "没有扩展名").is_none());
    }

    // 名字带路径、带大写后缀：扩展名认得出，但落点仍在我们自己的目录里。
    #[test]
    fn a_hostile_display_name_still_lands_inside_our_dir() {
        let root = Path::new("C:/blobs");
        let p = path_in(root, "abc", "..\\..\\x.PNG").unwrap();
        assert_eq!(p.parent(), Some(root));
        assert_eq!(p.file_name().unwrap().to_string_lossy(), "abc.png");
    }

    // 落盘 / 读回 / 删掉一圈。
    #[test]
    fn save_read_remove_round_trip() {
        let root = temp_dir("rt");
        let path = save_in(&root, "m1", "图片-1.png", b"hello").unwrap();
        assert!(exists_in(&root, "m1", "图片-1.png"));
        assert_eq!(read_in(&root, "m1", "图片-1.png").unwrap(), b"hello");
        remove_in(&root, "m1", "图片-1.png");
        assert!(!exists_in(&root, "m1", "图片-1.png"));
        assert!(!path.exists());
    }

    // 空字节要当场拒（不然会留下一条零字节的"图片"）。
    #[test]
    fn empty_content_is_refused() {
        let root = temp_dir("empty");
        assert!(save_in(&root, "m1", "图片-1.png", b"").is_err());
    }

    // `sweep` 只按 id 认人：还引用着的一个不动，没人引用的删掉。
    #[test]
    fn sweep_keeps_referenced_ids_only() {
        let root = temp_dir("sweep");
        let kept = save_in(&root, "keep-me", "图片.png", b"data").unwrap();
        let orphan = save_in(&root, "drop-me", "图片.png", b"data").unwrap();
        let mut keep = HashSet::new();
        keep.insert("keep-me".to_string());
        assert_eq!(sweep_in(&root, &keep), 1);
        assert!(kept.exists());
        assert!(!orphan.exists());
    }
}