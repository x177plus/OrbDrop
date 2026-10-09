use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::config::data_dir;

/// 两次落盘之间至少隔这么久（见 `ResumeStore::save`）。
///
/// 为什么是"节流"而不是"每次都写"：记录的粒度是**文件**——一个 2 万文件的目录就是 2 万次
/// `save`，而每次都是"把整份映射序列化 + 覆盖写一个文件"，累计写入量是 O(n²) 的（2 万条
/// 记下来能写到 GB 级），全部堵在下载循环里。250ms 一拍的语义与"崩溃后最多重传 250ms 的
/// 数据"等价 —— 何况真正决定"哪些字节已经可靠落盘"的是**调用方在 `save` 之前做的那次
/// flush/sync**（见 `push_job::spawn_download`），不是这里的频率。
const FLUSH_INTERVAL: Duration = Duration::from_millis(250);

/// Persisted resume offsets (F-60): invite_id → relative path → safe offset.
///
/// The receiving side owns the resume file; it survives restarts so an
/// interrupted transfer can continue from where it left off.
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ResumeFile {
    /// invite_id → (relative path → bytes already flushed)
    ///
    /// 老协议（`GET /push/download`）那条路的记录，**保留**：它是与老版本对端互通的唯一依据，
    /// 而且已经装好的机器上就躺着这样一份文件。
    ///
    /// 字段名保持 `records`：老文件（`{"records":{…}}`）读进来就是它，不迁移、不丢数据。
    #[serde(default)]
    pub records: HashMap<String, HashMap<String, u64>>,
    /// 新协议：文件身份（`stream::resume_key`）→ 记录（块位图 + 临时文件名）。
    #[serde(default)]
    pub files: HashMap<String, FileResume>,
}

struct Inner {
    records: ResumeFile,
    /// 内存里有还没落盘的改动。
    dirty: bool,
    last_flush: Instant,
}

pub struct ResumeStore {
    path: PathBuf,
    inner: Mutex<Inner>,
}

impl ResumeStore {
    pub fn new() -> Self {
        let path = data_dir().join("resume.json");
        let records = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<ResumeFile>(&s).ok())
            .unwrap_or_default();
        Self {
            path,
            inner: Mutex::new(Inner {
                records,
                dirty: false,
                last_flush: Instant::now(),
            }),
        }
    }

    pub fn load(&self, invite_id: &str, file_path: &str) -> u64 {
        self.inner
            .lock()
            .records
            .records
            .get(invite_id)
            .and_then(|m| m.get(file_path))
            .copied()
            .unwrap_or(0)
    }

    /// 记一个新 offset。**按 `FLUSH_INTERVAL` 节流落盘**，终态请用 `flush()`。
    ///
    /// 调它之前，那 `offset` 个字节必须已经真的在盘上（调用方的 flush/sync）；这条纪律是
    /// "记录不撒谎"的全部依据 —— 记录说 8MB，盘上就真有那 8MB，否则续传就会从一堆垃圾
    /// 后面接着写，最后交出一个"长度对、内容是坏的"文件。
    pub fn save(&self, invite_id: &str, file_path: &str, offset: u64) {
        let mut guard = self.inner.lock();
        guard
            .records
            .records
            .entry(invite_id.to_string())
            .or_default()
            .insert(file_path.to_string(), offset);
        guard.dirty = true;
        if guard.last_flush.elapsed() >= FLUSH_INTERVAL {
            persist_locked(&self.path, &mut guard);
        }
    }

    /// Remove an invite's record once the transfer completes.
    ///
    /// 立刻落盘：这是一次**状态跃迁**（"这些记录不再需要"），拖到下一拍才写没有意义，
    /// 而且传完之后进程正好被杀时，留着一条已完成的记录只会让下次重新核对一遍。
    pub fn clear(&self, invite_id: &str) {
        let mut guard = self.inner.lock();
        guard.records.records.remove(invite_id);
        guard.dirty = true;
        persist_locked(&self.path, &mut guard);
    }

    /// 把攒着的改动落盘（传完/失败/取消这类收尾点调用）。
    pub fn flush(&self) {
        let mut guard = self.inner.lock();
        if guard.dirty {
            persist_locked(&self.path, &mut guard);
        }
    }

    // -- 新协议（`POST /push/stream`）：按**文件身份**存的块位图 -------------------------

    /// 取一条文件记录（没有就是 `None`，调用方按"从零开始"处理）。
    pub fn file_record(&self, key: &str) -> Option<FileResume> {
        self.inner.lock().records.files.get(key).cloned()
    }

    /// 记一条文件记录（节流落盘；`updated` 由调用方维护）。
    ///
    /// 与老的 `save` 同一条纪律：**调它之前那些字节必须已经落盘**（调用方 flush + sync），
    /// 位图里的一位就是"这块的字节确实在盘上"的承诺，续传就靠它跳过。
    pub fn save_file(&self, key: &str, rec: FileResume) {
        let mut guard = self.inner.lock();
        guard.records.files.insert(key.to_string(), rec);
        guard.dirty = true;
        if guard.last_flush.elapsed() >= FLUSH_INTERVAL {
            persist_locked(&self.path, &mut guard);
        }
    }

    /// 丢掉一条记录（文件已经原子改名到最终路径了：这份进度不再需要）。
    ///
    /// **按 `FLUSH_INTERVAL` 节流**，不立刻落盘：一个几千文件的推送会一条一条地丢掉记录，
    /// 每次"整份 JSON 序列化 + 原子替换"就是又一次 O(n²)（2 万文件累计 GB 级写入，
    /// 实测把小文件吞吐拖到 0.3 MB/s 的正是这类"每文件一次全量落盘"）。收尾点由调用方
    /// 统一 `flush()`。
    pub fn drop_file(&self, key: &str) {
        let mut guard = self.inner.lock();
        guard.records.files.remove(key);
        guard.dirty = true;
        if guard.last_flush.elapsed() >= FLUSH_INTERVAL {
            persist_locked(&self.path, &mut guard);
        }
    }

    /// 淘汰放太久的记录（连同它指着的 `.part` 一起删）—— 兜底，避免 `.part` 越堆越多。
    ///
    /// `keep_seconds` 之内的记录都留着：用户"明天再推一次"是正常用法，不能当天就清掉。
    /// 返回删掉的条数。
    pub fn sweep_stale(&self, keep_seconds: i64) -> usize {
        let now = now_secs();
        let mut guard = self.inner.lock();
        let stale: Vec<(String, String)> = guard
            .records
            .files
            .iter()
            .filter(|(_, rec)| now.saturating_sub(rec.updated) > keep_seconds)
            .map(|(k, rec)| (k.clone(), rec.part.clone()))
            .collect();
        if stale.is_empty() {
            return 0;
        }
        for (key, part) in &stale {
            guard.records.files.remove(key);
            // `part` 记的是**绝对路径**（本机落盘根下的那个 `.part`）：直接删，不做路径拼装。
            let _ = std::fs::remove_file(part);
        }
        guard.dirty = true;
        persist_locked(&self.path, &mut guard);
        stale.len()
    }
}

impl Default for ResumeStore {
    fn default() -> Self {
        Self::new()
    }
}

/// 落盘：紧凑 JSON（`to_string_pretty` 白多 40% 字节）+ 原子替换（半截文件 = 读不出来 =
/// 续传记录全丢）。失败只记日志：记录是"省流量"的优化，不该因为写不进去就把传输打断。
fn persist_locked(path: &Path, guard: &mut Inner) {
    if let Some(parent) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(parent) {
            warn!("resume dir create failed: {e}");
            return;
        }
    }
    match serde_json::to_string(&guard.records) {
        Ok(json) => {
            if let Err(e) = crate::config::write_atomic(path, json.as_bytes()) {
                warn!("resume write failed: {e}");
            } else {
                guard.dirty = false;
                guard.last_flush = Instant::now();
            }
        }
        Err(e) => warn!("resume serialize failed: {e}"),
    }
    debug!(path = %path.display(), "resume state persisted");
}

/// 对端自报的名字**不能**直接当目录名用。
///
/// 落盘根是 `<下载目录>/<对端自报的名字>`，而那个名字是报文里的自由字符串（HTTP/WS 两条路
/// 都一样），它可以是 `C:\Windows\Temp`、`..\..\..\Users\Public`、`\\?\C:\x` 甚至
/// `run:evil`。`Path::join` 对带盘符前缀的入参会**清空**已累积的基址（`PathBuf::push` 的
/// 内部规则：`path.prefix().is_some()` 就 `clear()`），于是"下载目录"那一层直接被丢掉。
///
/// 这里把名字净化成一个**只可能是单个普通目录名**的串：路径分隔符与 Windows 禁用字符换成
/// `_`、去掉结尾的点和空格（Windows 会在建目录时把它们悄悄吃掉，`".. "` 于是退化成 `..`）、
/// 撞上 Windows 保留设备名时加前缀、限长。净化后为空则退成 `unknown-device`。
///
/// 幂等：`safe_dir_name(safe_dir_name(x)) == safe_dir_name(x)`。
pub fn safe_dir_name(raw: &str) -> String {
    /// 单层目录名的长度上限：设备名再长也没有必要（路径总长才是 Windows 的硬限制）。
    const MAX: usize = 64;
    const FALLBACK: &str = "unknown-device";

    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        // 控制字符与 Windows 禁用字符（含 `:` —— 它既是盘符也是 NTFS 数据流的分隔符）。
        let bad = ch.is_control()
            || matches!(ch, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*');
        out.push(if bad { '_' } else { ch });
    }

    // 首尾空白，以及**结尾的圆点**（Windows 会静默丢掉，`".. "` 这类就危险了）。
    let mut name = out.trim().to_string();
    while name.ends_with('.') {
        name.pop();
    }
    name = name.trim_end().to_string();

    if name.chars().count() > MAX {
        name = name.chars().take(MAX).collect();
    }
    // 截断之后可能又冒出结尾的点/空格。
    while name.ends_with('.') || name.ends_with(' ') {
        name.pop();
    }

    // `CON`、`NUL`、`COM1`… 在 Windows 上是设备名，建不出普通目录（`CON.txt` 同样中招）。
    if name.is_empty() || is_reserved_device_name(&name) {
        return if name.is_empty() {
            FALLBACK.to_string()
        } else {
            format!("_{name}")
        };
    }
    name
}

/// Windows 保留设备名（大小写不敏感，取第一个圆点之前的词干）。
fn is_reserved_device_name(name: &str) -> bool {
    let stem: String = name
        .split('.')
        .next()
        .unwrap_or(name)
        .to_ascii_uppercase();
    if matches!(stem.as_str(), "CON" | "PRN" | "AUX" | "NUL" | "CLOCK$") {
        return true;
    }
    let mut chars = stem.chars();
    let (Some(a), Some(b), Some(c), Some(d), None) =
        (chars.next(), chars.next(), chars.next(), chars.next(), chars.next())
    else {
        return false;
    };
    (a == 'C' && b == 'O' && c == 'M' || a == 'L' && b == 'P' && c == 'T')
        && d.is_ascii_digit()
        && d != '0'
}

/// 推送落盘的根目录：`<下载目录>/<净化过的发送方名>`。
///
/// **唯一**的落盘根算法（新引擎 `download::run` 与老循环 `push_job` 共用）：名字先净化，
/// 再用 `safe_join` 兜一道 —— 净化保证它不含分隔符/盘符，`safe_join` 保证拼出来的东西
/// 仍然钉在下载目录里面。两条路都改了才算堵住。
pub fn landing_root(download_dir: &Path, sender_name: &str) -> PathBuf {
    let name = safe_dir_name(sender_name);
    safe_join(download_dir, &name).unwrap_or_else(|| download_dir.join("unknown-device"))
}

/// Sanitize a relative path so it cannot escape the save root (下载目录 / 推送落盘根).
///
/// 清单里的相对路径也是对方给的，逐段校验：
/// - 空段、`.`、`..`、含反斜杠或 NUL → 拒；
/// - 含 `:` → 拒（Windows 上 `a.txt:evil` 会写进 NTFS 数据流，`C:` 是盘符）；
/// - 以圆点或空格结尾 → 拒（Windows 会把它们吃掉，`".. "` 于是变成 `..`）；
/// - **不是单个普通名字**（`Path::components()` 给出前缀/根/父目录）→ 拒。这一条是真正的闸门：
///   `PathBuf::push` 遇到带前缀的段会清空已累积的基址，`safe_join(root,"C:/Windows/Temp")`
///   原本会得到 `C:Windows\Temp`（基址消失）。
/// - Windows 保留设备名 → 拒。
pub fn safe_join(root: &Path, rel: &str) -> Option<PathBuf> {
    let mut out = root.to_path_buf();
    for part in rel.split('/') {
        if part.is_empty()
            || part == "."
            || part == ".."
            || part.contains('\\')
            || part.contains('\0')
            || part.contains(':')
            || part.ends_with('.')
            || part.ends_with(' ')
            || is_reserved_device_name(part)
        {
            return None;
        }
        // 平台自己的判据：Windows 上 `C:`、`\\?\C:`、`/foo` 都不是普通名。
        let mut components = Path::new(part).components();
        match (components.next(), components.next()) {
            (Some(std::path::Component::Normal(_)), None) => {}
            _ => return None,
        }
        out.push(part);
    }
    Some(out)
}

/// 一个文件的续传记录（新协议 `POST /push/stream` 那条路）。
///
/// 键是**文件身份**（`stream::resume_key`：对端 id + 路径 + 大小 + 时间戳），不是邀请 id ——
/// 用户"传失败了再推一次"拿到的是新邀请，只有身份是稳定的。按邀请存的话，重推一次就是从零
/// 重传一整遍（老实现正是如此）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileResume {
    pub size: u64,
    pub mtime: i64,
    pub block_size: u32,
    /// 已**落盘且校验过**的块：位图，按块序号从最低位起，每字节 8 块，hex 编码。
    pub blocks: String,
    /// 每一块的 SHA-256（hex，按块序号对齐；空串 = 没记到）。
    ///
    /// 收尾时会拿它把 `.part` **再读一遍逐块核对**（见 `download::Session::finalize`）：
    /// 这是"文件被改名到最终路径 ⇒ 它的每一个字节都对"的最后一道闸，也让"跨会话续传"
    /// 那些早先收下的块同样被复核（它们不可能被重新下载来验证）。
    #[serde(default)]
    pub hashes: Vec<String>,
    /// 这份进度对应的临时文件（**绝对路径**）。记录指着哪个 `.part`，续传就从它接着写；
    /// 文件原子改名到最终路径之后，这条记录就作废（`drop_file`）。
    pub part: String,
    /// 最后更新时刻（unix 秒）：用来淘汰放太久的记录与它对应的 `.part`。
    pub updated: i64,
}

impl FileResume {
    pub fn new(size: u64, mtime: i64, block_size: u32, part: String) -> Self {
        Self {
            size,
            mtime,
            block_size,
            blocks: String::new(),
            hashes: Vec::new(),
            part,
            updated: now_secs(),
        }
    }

    /// 记下第 `index` 块的摘要（hex）。
    pub fn set_hash(&mut self, index: u32, digest: &[u8; 32]) {
        let at = index as usize;
        if self.hashes.len() <= at {
            self.hashes.resize(at + 1, String::new());
        }
        let mut hex = String::with_capacity(64);
        for b in digest {
            hex.push_str(&format!("{b:02x}"));
        }
        self.hashes[at] = hex;
    }

    pub fn block_hash(&self, index: u32) -> Option<&str> {
        self.hashes
            .get(index as usize)
            .map(|s| s.as_str())
            .filter(|s| !s.is_empty())
    }

    /// 这个文件已经落好多少字节（按位图算）。
    pub fn done_bytes(&self) -> u64 {
        let count = crate::transfer::stream::block_count(self.size, self.block_size);
        let mut total = 0u64;
        for i in 0..count {
            if bitmap_has(&self.blocks, i) {
                total += crate::transfer::stream::block_range(self.size, self.block_size, i)
                    .map(|(_, len)| len as u64)
                    .unwrap_or(0);
            }
        }
        total
    }

    pub fn is_complete(&self) -> bool {
        let count = crate::transfer::stream::block_count(self.size, self.block_size);
        (0..count).all(|i| bitmap_has(&self.blocks, i))
    }
}

pub fn now_secs() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 位图取位。越界的位读作"没有"（记录被截短过就当没这块，续传会重下它）。
pub fn bitmap_has(bits: &str, index: u32) -> bool {
    let byte = (index / 8) as usize;
    let at = byte * 2;
    if bits.len() < at + 2 {
        return false;
    }
    u8::from_str_radix(&bits[at..at + 2], 16).is_ok_and(|b| b & (1u8 << (index % 8)) != 0)
}

/// 位图置位（需要时把字符串补长）。
pub fn bitmap_set(bits: &mut String, index: u32) {
    let byte = (index / 8) as usize;
    while bits.len() < byte * 2 + 2 {
        bits.push('0');
    }
    let at = byte * 2;
    let cur = u8::from_str_radix(&bits[at..at + 2], 16).unwrap_or(0);
    let next = cur | (1u8 << (index % 8));
    bits.replace_range(at..at + 2, &format!("{next:02x}"));
}

/// 位图的字符串长度（hex 字符数）—— 只是给测试与日志看，不参与判据。
pub fn bitmap_hex_len(bits: &str) -> usize {
    bits.len()
}

/// Track progress bookkeeping: returns true when the offset advanced.
pub fn progress_delta(prev: u64, next: u64) -> u64 {
    next.saturating_sub(prev)
}

#[cfg(test)]
mod path_tests {
    use super::*;

    /// 落盘根必须**永远**在下载目录里面 —— 不管对端把名字报成什么样。
    #[test]
    fn landing_root_cannot_escape_the_download_dir() {
        let dl = PathBuf::from("C:\\Users\\u\\Downloads");
        for raw in [
            "PC-B",
            "C:\\Windows\\Temp",
            "C:foo\\bar",
            "\\\\?\\C:\\Windows",
            "..\\..\\..\\Users\\Public",
            "..",
            ".",
            "",
            "   ",
            "../../etc",
            "/etc",
            "..\\..\\x",
            "设备名（家里那台）",
            "NUL",
            "con.txt",
            "COM1",
            "x. ",
            "x.",
            "run:evil",
            &"长".repeat(300),
        ] {
            let dir = landing_root(&dl, raw);
            assert!(
                dir.starts_with(&dl),
                "名字 {raw:?} 逃出了下载目录：{dir:?}"
            );
            // 且必须只多一层（不是多级嵌套，更不是绝对路径）。
            assert_eq!(
                dir.parent(),
                Some(dl.as_path()),
                "名字 {raw:?} 拼出了非单层的落盘根：{dir:?}"
            );
        }
    }

    #[test]
    fn safe_dir_name_keeps_device_names_readable() {
        assert_eq!(safe_dir_name("PC-B"), "PC-B", "正常名字一个字不动");
        assert_eq!(safe_dir_name("笔记本（客厅）"), "笔记本（客厅）", "中文原样保留");
        assert_eq!(safe_dir_name("  spaced  "), "spaced");
        assert_eq!(safe_dir_name(""), "unknown-device");
        assert_eq!(safe_dir_name(".."), "unknown-device", "全是点 → 退成兜底名");
        // 分隔符与禁用字符换成下划线，绝不留下来。
        let cleaned = safe_dir_name("C:\\Windows\\Temp");
        assert!(!cleaned.contains(['\\', '/', ':']), "净化后不该留分隔符/冒号: {cleaned}");
        // 净化后剩下的 `..` 只是普通字符（分隔符已经没了），真正要守的判据是：
        // 结果必须是**单个普通目录名** —— `safe_join` 会再验一次，落盘根因此钉在下载目录里。
        let dotted = safe_dir_name("..\\..\\..\\Users\\Public");
        assert!(!dotted.contains(['\\', '/', ':']), "分隔符必须消失: {dotted}");
        assert!(
            safe_join(Path::new("C:\\dl"), &dotted).is_some(),
            "净化结果必须是单个普通目录名: {dotted}"
        );
        assert_eq!(safe_dir_name("NUL"), "_NUL");
        assert_eq!(safe_dir_name("com1.TXT"), "_com1.TXT");
        assert_eq!(safe_dir_name("x."), "x", "结尾的点 Windows 会吃掉，先去掉");
        assert!(safe_dir_name(&"长".repeat(300)).chars().count() <= 64);
        // 幂等：再净化一次结果不变。
        for raw in ["PC-B", "C:\\x\\y", "", "..", "NUL", "x. ", "设备名"] {
            let once = safe_dir_name(raw);
            assert_eq!(safe_dir_name(&once), once, "净化不幂等：{raw:?}");
        }
    }

    #[test]
    fn safe_join_rejects_everything_that_is_not_a_plain_relative_path() {
        let root = PathBuf::from("C:\\Users\\u\\Downloads\\PC-B");
        for bad in [
            "",
            ".",
            "..",
            "a/../b",
            "../a.txt",
            "a/..",
            "//server/share/x",
            "C:/Windows/Temp/x.txt",
            "C:foo/x",
            "\\\\?\\C:\\x",
            "a\\b.txt",
            "a.txt:evil",
            "x./y",
            "x /y",
            "CON/x",
            "a//b",
            "nul.txt",
        ] {
            assert!(safe_join(&root, bad).is_none(), "{bad:?} 必须被拒");
        }
        for (ok, want) in [
            ("a.txt", "a.txt"),
            ("目录/子目录/文件.txt", "目录/子目录/文件.txt"),
            (".gitkeep", ".gitkeep"),
            ("a.b.c", "a.b.c"),
            ("..a", "..a"),
            ("a..b", "a..b"),
            ("中文 空格.txt", "中文 空格.txt"),
            ("COM0", "COM0"),
        ] {
            let joined = safe_join(&root, ok).expect(ok);
            assert!(joined.starts_with(&root));
            assert_eq!(
                joined.strip_prefix(&root).unwrap().to_string_lossy().replace('\\', "/"),
                want
            );
        }
    }
}

#[cfg(test)]
mod bitmap_tests {
    use super::*;
    use crate::transfer::stream::{block_count, block_range, DEFAULT_BLOCK_SIZE};

    #[test]
    fn bitmap_set_and_has() {
        let mut bits = String::new();
        assert!(!bitmap_has(&bits, 0), "空位图 = 一块都没有");
        bitmap_set(&mut bits, 0);
        assert!(bitmap_has(&bits, 0));
        assert!(!bitmap_has(&bits, 1));
        bitmap_set(&mut bits, 9);
        assert!(bitmap_has(&bits, 9), "跨字节的位要能置上");
        assert!(!bitmap_has(&bits, 8));
        assert_eq!(bitmap_hex_len(&bits), 4, "9 号位落在第二个字节 → 4 个 hex 字符");
        // 越界/越短一律读作"没有"，不 panic。
        assert!(!bitmap_has("", 1000));
        assert!(!bitmap_has("0f", 17));
    }

    #[test]
    fn done_bytes_counts_only_set_blocks() {
        let size = DEFAULT_BLOCK_SIZE as u64 * 2 + 10;
        let count = block_count(size, DEFAULT_BLOCK_SIZE);
        assert_eq!(count, 3);
        let mut rec = FileResume::new(size, 1, DEFAULT_BLOCK_SIZE, "x.part".into());
        assert_eq!(rec.done_bytes(), 0);
        assert!(!rec.is_complete());
        bitmap_set(&mut rec.blocks, 1);
        assert_eq!(rec.done_bytes(), DEFAULT_BLOCK_SIZE as u64, "只算置位的那一块");
        bitmap_set(&mut rec.blocks, 0);
        bitmap_set(&mut rec.blocks, 2);
        assert!(rec.is_complete());
        assert_eq!(rec.done_bytes(), size, "最后一块是零头，总字节等于文件大小");
        for i in 0..count {
            let (_, len) = block_range(size, DEFAULT_BLOCK_SIZE, i).unwrap();
            assert!(len > 0);
        }
    }
}

#[allow(dead_code)]
pub fn log_done(invite_id: &str, file_path: &str, offset: u64) {
    info!(invite_id, file_path, offset, "resume offset updated");
}