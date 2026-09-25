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

/// Sanitize a relative path so it cannot escape the save root (下载目录 / 推送落盘根).
pub fn safe_join(root: &Path, rel: &str) -> Option<PathBuf> {
    let mut out = root.to_path_buf();
    for part in rel.split('/') {
        if part.is_empty() || part == "." || part == ".." || part.contains('\\') {
            return None;
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