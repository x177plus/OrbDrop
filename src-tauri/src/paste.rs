//! 粘贴进来的图片：剪贴板那串字节 → 本机一个**临时文件**；以及把落盘的图片读回给界面看。
//!
//! **为什么中间要有这一步**：会话里"发图片"走的还是**推送**那条路（`transfer::push_paths`），
//! 而它只认本机**真实路径**（清单要按路径列、服务端要按路径分块取字节）。剪贴板给的是一串
//! 字节，所以中间必须有一个"落成文件"的落点。
//!
//! **落在系统临时目录**（`std::env::temp_dir()/orbdrop-paste`），不是自家数据目录：
//! 数据目录整棵子树都会被 `transfer::manifest_for_paths` 当成"本机凭据"当场拒掉
//! （`unsendable::is_own_data_path`），而粘贴的图片恰恰是要发出去的东西。
//!
//! 这一层不认识剪贴板本身（桌面端那串字节由 WebView 的 `paste` 事件交出来，安卓端由
//! `content://` 那一跳交出来）—— 它只管"字节进、路径出"，两端因此共用同一份落盘与命名。

use base64::Engine;
use std::path::{Path, PathBuf};
use tracing::info;

/// 粘贴图片的暂存目录。
pub fn paste_dir() -> PathBuf {
    std::env::temp_dir().join("orbdrop-paste")
}

/// 暂存文件留多久（超过就清）。
///
/// 为什么是"按龄回收"而不是"发完就删"：草稿只活在内存里（`stores/chat.svelte.ts` 的
/// `draft`），进程一退就没了 —— 一张还躺在草稿里、没发出去的图，除了按龄回收没有第二个
/// 回收它的时机。7 天足够长（没人会把一条草稿留一周），也足够短（不至于把临时目录撑爆）。
const MAX_AGE: std::time::Duration = std::time::Duration::from_secs(7 * 24 * 60 * 60);

/// 会话里预览一张图最多读多少字节。
///
/// 预览是把整份文件 base64 交给 WebView —— 没有缩放（真正的缩略图要引 `image` 那类解码库，
/// 那是另一笔依赖，见本仓对依赖的态度），所以这道上限同时是**内存**与**开不打得开**的闸：
/// 一张 12 MB 的图 base64 出来约 16 MB 字符串，外加 WebView 解码后的位图。超过这一档的
/// 不在会话里预览（那条气泡退回原来的文件样式，点「用系统程序打开」看）。
/// 截图（这个功能最主要的用法）通常不到 3 MB，12 MB 留得足够宽。
const MAX_PREVIEW_BYTES: u64 = 12 * 1024 * 1024;

/// 图片扩展名 → MIME。**认不出来的扩展名 = 不是图片**（`None`）—— 这条判据同时是
/// `read_data_url` 的准入条件：不许拿它当"读任意本机文件"的通用后门。
fn mime_of(ext: &str) -> Option<&'static str> {
    match ext.to_ascii_lowercase().as_str() {
        "png" => Some("image/png"),
        "jpg" | "jpeg" => Some("image/jpeg"),
        "gif" => Some("image/gif"),
        "webp" => Some("image/webp"),
        "bmp" => Some("image/bmp"),
        "avif" => Some("image/avif"),
        _ => None,
    }
}

/// 这条名字（文件名或一段路径）里的图片扩展名：**净化过、小写**；认不出来 = `None`。
///
/// 与 [`mime_of`] 同一份名单 —— 会话里那条图片消息的内容文件按它取名（见 `blobs`），
/// 所以"什么算图片"这件事只有这一处判据。名字是**对端给的**，所以这里只管取的**扩展名**
/// 那一小段：路径片段（`..\\..\\x.PNG`）到不了文件名里，落点仍由调用方那个目录说了算。
pub(crate) fn image_ext_of(name: &str) -> Option<String> {
    let ext = std::path::Path::new(name)
        .extension()?
        .to_string_lossy()
        .to_ascii_lowercase();
    mime_of(&ext).map(|_| ext)
}

/// 规整前端给的扩展名：去掉点、统一小写，再确认它是我们认识的图片。
///
/// 前端那串来自剪贴板的 MIME 或数据 URL 的类型（`clipboard.ts::imageExtOf`），不是用户输入，
/// 但它**跨了进程边界**：写文件后缀之前必须自己判一遍，不然 `../../x` 这种就能拼出路径。
fn normalize_ext(ext: &str) -> Result<String, String> {
    let ext = ext.trim().trim_start_matches('.').to_ascii_lowercase();
    if mime_of(&ext).is_none() {
        return Err(format!("不认识的图片格式：{ext}"));
    }
    Ok(ext)
}

/// 把粘贴进来的字节落成一个暂存文件，返回它的绝对路径。
///
/// 名字是"图片-年月日-时分秒.<扩展名>"：这个名字会**一路带到对端**（推送清单里的条目名就是
/// 它），所以得像个人能读懂的名字，而不是一串 uuid。同一秒里粘贴两张就往后编号。
pub fn save(bytes: &[u8], ext: &str) -> Result<String, String> {
    use std::io::Write;
    let ext = normalize_ext(ext)?;
    if bytes.is_empty() {
        return Err("剪贴板里的图片是空的".into());
    }
    let dir = paste_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("建暂存目录失败：{e}"))?;
    let stamp = chrono::Local::now().format("%Y%m%d-%H%M%S");
    let base = format!("图片-{stamp}");
    // 名字要**原子地**占下来（`create_new`），不能先 `exists()` 再 `write`：那样两次粘贴同时
    // 抢同一个名字时两边都会判定"不存在"，然后两次写落到同一个文件上（一次截断一次覆盖，
    // 最后得到的是一份混起来的内容）。本仓在配置落盘那边踩过同一个坑，见 `config::write_atomic`。
    for n in 1..1000u32 {
        let name = match n {
            1 => format!("{base}.{ext}"),
            _ => format!("{base}-{n}.{ext}"),
        };
        let path = dir.join(name);
        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
        {
            Ok(mut file) => {
                file.write_all(bytes)
                    .map_err(|e| format!("暂存粘贴的图片失败：{e}"))?;
                return Ok(path.to_string_lossy().to_string());
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("暂存粘贴的图片失败：{e}")),
        }
    }
    Err("暂存目录里同名的图片太多了".into())
}

/// 同 [`save`]，只是字节以 base64 字符串过来（`invoke` 的载荷是 JSON，见 `lib.rs` 那条命令）。
pub fn save_base64(data: &str, ext: &str) -> Result<String, String> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(data.trim())
        .map_err(|e| format!("剪贴板里的图片数据坏了：{e}"))?;
    save(&bytes, ext)
}

/// 一张落盘的图片 → `data:` URL（会话里画缩略图、看大图都用它）。
///
/// 为什么用 data URL 而不是 asset 协议：asset 协议要开 `security.assetProtocol` 并给它一段
/// 路径范围（那等于"WebView 能读任意本机文件"），而这里只需要读**某几张图片**。代价是整份
/// 字节进了 IPC（见 [`MAX_PREVIEW_BYTES`]），所以有上限。
pub fn read_data_url(path: &str, max_bytes: u64) -> Result<String, String> {
    let p = Path::new(path);
    let name = p
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| path.to_string());
    let meta = std::fs::metadata(p).map_err(|e| format!("读不到「{name}」：{e}"))?;
    if !meta.is_file() {
        return Err(format!("「{name}」不是文件"));
    }
    // 会话图片的文件名统一是 `<id>.bin`，真正的扩展名在封装头里；暂存图片仍是普通
    // `*.png`。所以上限要按"文件可能带封装头"放宽一点，解码后再按真实字节数判一次。
    if meta.len() > max_bytes.saturating_add(crate::envelope::MAX_OVERHEAD) {
        return Err(format!(
            "「{name}」（{}）太大，不在会话里预览；点「用系统程序打开」看",
            human_size(meta.len())
        ));
    }
    let raw = std::fs::read(p).map_err(|e| format!("读不到「{name}」：{e}"))?;
    let (ext, bytes) = match crate::envelope::decode_if(&raw) {
        Ok(Some((ext, bytes))) => (ext, bytes),
        Ok(None) => {
            let ext = p
                .extension()
                .map(|e| e.to_string_lossy().to_string())
                .unwrap_or_default();
            (ext, raw)
        }
        Err(e) => return Err(format!("「{name}」内容损坏：{e}")),
    };
    let Some(mime) = mime_of(&ext) else {
        return Err(format!("「{name}」不是能预览的图片"));
    };
    if bytes.len() as u64 > max_bytes {
        return Err(format!(
            "「{name}」（{}）太大，不在会话里预览；点「用系统程序打开」看",
            human_size(bytes.len() as u64)
        ));
    }
    let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
    Ok(format!("data:{mime};base64,{b64}"))
}

/// 会话里那张缩略图 / 大图读的就是它（同一档上限，见 [`MAX_PREVIEW_BYTES`]）。
pub fn preview_data_url(path: &str) -> Result<String, String> {
    read_data_url(path, MAX_PREVIEW_BYTES)
}

/// 给错误文案用的大小（与前端 `formatFileSize` 同一套写法，只是这里在 Rust 侧说这句话）。
fn human_size(bytes: u64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    const KB: f64 = 1024.0;
    let b = bytes as f64;
    if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

/// 启动时清一次暂存目录里过期的图片（见 [`MAX_AGE`]）。
///
/// 只删**我们自己的**暂存目录里的文件，且只删过期的；删不掉（被占用 / 没有权限）就算了 ——
/// 这是清理，不是功能，报错没有出口。
pub fn sweep() {
    let removed = sweep_dir(&paste_dir(), MAX_AGE, std::time::SystemTime::now());
    if removed > 0 {
        info!(removed, "清掉过期的粘贴暂存图");
    }
}

/// [`sweep`] 的本体（目录、龄限、"现在"都由调用方给 —— 测试才能构造出"过期"而不去改 mtime）。
fn sweep_dir(dir: &Path, max_age: std::time::Duration, now: std::time::SystemTime) -> usize {
    let Ok(rd) = std::fs::read_dir(dir) else {
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
        let age = meta
            .modified()
            .ok()
            .and_then(|m| now.duration_since(m).ok());
        if age.map(|a| a > max_age).unwrap_or(false) && std::fs::remove_file(entry.path()).is_ok() {
            removed += 1;
        }
    }
    removed
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("orbdrop_paste_{tag}_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    // 落盘与命名：字节进、路径出，名字是人能读懂的「图片-时间戳.扩展名」。
    #[test]
    fn save_writes_a_readable_file_and_keeps_second_apart() {
        let bytes = b"\x89PNG\r\n\x1a\nfake";
        let path = save(bytes, ".PNG").unwrap();
        let p = PathBuf::from(&path);
        assert_eq!(std::fs::read(&p).unwrap(), bytes);
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        assert!(name.starts_with("图片-"), "{name}");
        assert!(name.ends_with(".png"), "{name}");

        // 同一秒里第二张不能盖掉第一张。
        let second = save(bytes, "png").unwrap();
        assert_ne!(path, second);
        assert_eq!(std::fs::read(&second).unwrap(), bytes);
        let _ = std::fs::remove_file(&path);
        let _ = std::fs::remove_file(&second);
    }

    // 扩展名跨进程边界过来，必须自己判：`.png` / `PNG` 收，路径片段与未知格式一律拒。
    #[test]
    fn save_refuses_extensions_that_are_not_images() {
        let wrote = [save(b"x", "png"), save(b"x", ".JPEG")];
        for p in wrote.into_iter().flatten() {
            let _ = std::fs::remove_file(p);
        }
        assert!(save(b"x", "../../evil").is_err());
        assert!(save(b"x", "exe").is_err());
        assert!(save(b"x", "").is_err());
    }

    #[test]
    fn save_rejects_empty_bytes() {
        assert!(save(b"", "png").is_err());
    }

    // 界面那半送过来的就是一个 base64 字符串：解出来要与原字节逐字节相同，坏字符串要报原话。
    #[test]
    fn save_base64_decodes_what_the_ui_sends() {
        let path = save_base64("aGVsbG8=", "png").unwrap();
        assert_eq!(std::fs::read(&path).unwrap(), b"hello");
        let _ = std::fs::remove_file(&path);
        let err = save_base64("这不是 base64!!", "png").unwrap_err();
        assert!(err.contains("坏了"), "{err}");
    }

    // 读回来：data URL 的内容要能原样解出，且只认图片扩展名。
    #[test]
    fn read_data_url_round_trips_and_gates_on_extension() {
        let dir = temp_dir("read");
        let img = dir.join("a.png");
        std::fs::write(&img, b"hello").unwrap();
        let url = read_data_url(&img.to_string_lossy(), 1024).unwrap();
        assert_eq!(url, "data:image/png;base64,aGVsbG8=");

        let txt = dir.join("a.txt");
        std::fs::write(&txt, b"hello").unwrap();
        assert!(read_data_url(&txt.to_string_lossy(), 1024).is_err());
        assert!(read_data_url(&dir.join("nope.png").to_string_lossy(), 1024).is_err());
    }

    // 会话图片落盘是封装格式、文件名是 `<id>.bin`：预览要能识别头里的扩展名并解出原始字节。
    #[test]
    fn read_data_url_decodes_a_chat_blob() {
        let dir = temp_dir("enveloped");
        let blob = dir.join("m1.bin");
        std::fs::write(&blob, crate::envelope::encode("png", b"hello")).unwrap();
        let url = read_data_url(&blob.to_string_lossy(), 1024).unwrap();
        assert_eq!(url, "data:image/png;base64,aGVsbG8=");

        // 封装头不是图片：不能靠它预览。
        let bad = dir.join("m2.bin");
        std::fs::write(&bad, crate::envelope::encode("", b"hello")).unwrap();
        assert!(read_data_url(&bad.to_string_lossy(), 1024).is_err());
    }

    #[test]
    fn read_data_url_refuses_oversized_files() {
        let dir = temp_dir("big");
        let img = dir.join("big.png");
        std::fs::write(&img, vec![0u8; 2048]).unwrap();
        let err = read_data_url(&img.to_string_lossy(), 1024).unwrap_err();
        assert!(err.contains("太大"), "{err}");
    }

    // 清理只认龄：过期的删、没过期的一个不动（目录 / 读不到的项也一个不动）。
    #[test]
    fn sweep_removes_only_expired_files() {
        let dir = temp_dir("sweep");
        let fresh = dir.join("fresh.png");
        std::fs::write(&fresh, b"x").unwrap();
        let sub = dir.join("subdir");
        std::fs::create_dir_all(&sub).unwrap();

        // 「现在」= 真现在：什么都没过期。
        assert_eq!(sweep_dir(&dir, MAX_AGE, std::time::SystemTime::now()), 0);
        assert!(fresh.exists());

        // 「现在」= 8 天以后：同一个文件就过期了，目录还在。
        let later = std::time::SystemTime::now() + std::time::Duration::from_secs(8 * 24 * 60 * 60);
        assert_eq!(sweep_dir(&dir, MAX_AGE, later), 1);
        assert!(!fresh.exists());
        assert!(sub.is_dir());
    }
}
