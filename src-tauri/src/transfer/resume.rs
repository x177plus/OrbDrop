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
    /// 还没传完的接收任务（见 [`PendingInbound`]）：进程重启之后接着收的依据。
    ///
    /// 与上面两份**不是一回事**：那两份记的是"哪几个字节已经在盘上"（省流量），
    /// 这份记的是"还有一次接收没走完"（要不要接着收）。所以它们各有各的寿命 ——
    /// 任务走完/放弃就把这份删掉，位图留着（用户手动重推一次仍然省得下那些块）。
    #[serde(default)]
    pub pending: HashMap<String, PendingInbound>,
}

/// 接收端的一条**待续传任务**：进程重启之后还认得"有这么一次接收没走完"。
///
/// 为什么不复用文件级那套记录：那份的键是**文件身份**（`stream::resume_key`）、故意不带
/// 邀请 id（跨邀请复用），它答得了"哪几块不用再要"，答不了"要不要接着收"。要重新发起一次
/// 接收，这四样缺一不可：邀请 id（对端的凭据）、对端身份与地址、清单（要哪些、各自多大、
/// 什么时候的）、落盘根（东西该落哪）。
///
/// **只在"已经放行、开始下载"之后才记**（`transfer::accept_invite`）：没点头的邀请不该在
/// 重启之后自己开始收 —— 那等于替用户点了一次同意。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingInbound {
    pub invite_id: String,
    pub sender_id: String,
    /// 对端自报的名字（落盘目录那一层按它拼，见 `landing_dir`）。
    pub sender_name: String,
    pub name: String,
    pub size: u64,
    pub is_dir: bool,
    /// 发送方这次选了几项（>1 = 多选合并成的一条推送）。
    #[serde(default = "one_item")]
    pub count: u32,
    pub files: Vec<PendingFile>,
    /// 对端声明的能力位：重启后要按**同一套**协议续（`stream1` 走新数据面，否则老那一套）。
    #[serde(default)]
    pub caps: Vec<String>,
    /// 落盘根：只有"每次询问下用户亲自挑过目录"的那条邀请才带（否则用设置里的下载目录）。
    #[serde(default)]
    pub root: Option<String>,
    /// 对端地址（未配对时这是唯一的路；已配对的按地址簿拨，它只当兜底）。
    #[serde(default)]
    pub callback: Option<(String, u16)>,
    /// 有进展的时刻（unix 秒）：接受那一刻，或最后一次**有文件收完**那一刻。
    ///
    /// TTL 按它算（见 [`PENDING_TTL_SECS`]）。**尝试不刷新它**（"对面一直不出现"熬不过窗口），
    /// 块级进度也不刷新（那是每 200ms 一次的事，写盘会被拖垮）。
    pub updated_at: i64,
}

/// 清单里的一项：只留重新发起一次接收要的那三样（`FileEntry` 的投影）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingFile {
    pub path: String,
    pub size: u64,
    #[serde(default)]
    pub mtime: i64,
}

fn one_item() -> u32 {
    1
}

/// 一条待续传任务最多留多久：**从"最后一次有进展"算起 7 天**。
///
/// 到点就收尾（如实说一句"等了 7 天没传完"）—— 留着它没有意义：对端要么不在网上了，
/// 要么那条推送早就没了。位图与 `.part` 不跟着删（用户手动重推一次仍然接着传）。
pub const PENDING_TTL_SECS: i64 = 7 * 24 * 60 * 60;

impl PendingInbound {
    /// 过了 TTL 没有？`now` = unix 秒。
    pub fn expired(&self, now: i64) -> bool {
        now.saturating_sub(self.updated_at) > PENDING_TTL_SECS
    }
}

/// 重试的退避台阶（秒）：够不着就等下一拍，越等越久，封顶 2 小时。
///
/// 台阶都在 TTL（7 天）之内，所以"对面一直不出现"最终会由 TTL 收尾，而不是这一串无限重试。
const RETRY_BACKOFF: [u64; 5] = [30, 120, 600, 1800, 7200];

struct Inner {
    records: ResumeFile,
    /// 内存里有还没落盘的改动。
    dirty: bool,
    last_flush: Instant,
}

pub struct ResumeStore {
    path: PathBuf,
    inner: Mutex<Inner>,
    /// 重试的**运行时**记账（不落盘）：invite_id → (已试几轮, 上一次尝试的时刻)。
    ///
    /// 为什么不落盘：重启一次就该立刻再试一遍（上一趟已经随进程没了），把"已经等了几轮"
    /// 也带过去是最糟的 —— 用户重启往往就是为了让它再试一次。
    attempts: Mutex<HashMap<String, (u32, Instant)>>,
}

impl ResumeStore {
    pub fn new() -> Self {
        Self::with_path(data_dir().join("resume.json"))
    }

    /// 指定文件的构造（单测用：不碰真实数据目录）。
    pub fn with_path(path: PathBuf) -> Self {
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
            attempts: Mutex::new(HashMap::new()),
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

    // -- 待续传任务（进程重启后接着收）--------------------------------------------------

    /// 记下一条待续传任务（`transfer::accept_invite` 放行那一刻）。**立刻落盘**：
    /// 这是一次状态跃迁（"从这一刻起这条接收该被接手"），拖到下一拍就没有意义了 ——
    /// 用户正是在这之后可能把应用关掉。
    pub fn put_pending(&self, rec: PendingInbound) {
        let mut guard = self.inner.lock();
        guard.records.pending.insert(rec.invite_id.clone(), rec);
        guard.dirty = true;
        persist_locked(&self.path, &mut guard);
    }

    /// 这条不用再接着收了：传完 / 终态失败 / 用户取消 / 对端撤了 / 过期放弃。
    ///
    /// **不清**文件级位图：用户手动重推一次仍然省得下那些块（见 [`ResumeFile::pending`] 那段）。
    pub fn drop_pending(&self, invite_id: &str) {
        {
            let mut guard = self.inner.lock();
            if guard.records.pending.remove(invite_id).is_some() {
                guard.dirty = true;
                persist_locked(&self.path, &mut guard);
            }
        }
        self.attempts.lock().remove(invite_id);
    }

    /// 有进展了（**一个文件收完了**）：把 TTL 的锚点往前挪。不是每块都叫 ——
    /// 块级进度是每 200ms 一次的事，那样等于把整份 JSON 按 5 次/秒重写。
    pub fn touch_pending(&self, invite_id: &str) {
        let mut guard = self.inner.lock();
        let Some(rec) = guard.records.pending.get_mut(invite_id) else {
            return;
        };
        rec.updated_at = now_secs();
        guard.dirty = true;
        // 与 `save_file` 同一条节流纪律（一个几千文件的推送会一条一条地收尾）。
        if guard.last_flush.elapsed() >= FLUSH_INTERVAL {
            persist_locked(&self.path, &mut guard);
        }
    }

    /// 全部待续传任务（启动时/每一拍由 `transfer::resume_pending` 逐条接着收）。
    pub fn pending_entries(&self) -> Vec<PendingInbound> {
        self.inner.lock().records.pending.values().cloned().collect()
    }

    /// 还在接手的邀请 id。
    ///
    /// `chat` 的启动扫尾用它：那条"接收中"的记录不能被落成「接收失败」—— 它马上就要接着收了
    /// （见 `chat::requeue_inflight`）。
    pub fn pending_ids(&self) -> std::collections::HashSet<String> {
        self.inner.lock().records.pending.keys().cloned().collect()
    }

    /// 这会儿该不该再试一次这条（退避台阶见 [`RETRY_BACKOFF`]）。没有记账 = 头一次，立刻试。
    pub fn attempt_due(&self, invite_id: &str) -> bool {
        let attempts = self.attempts.lock();
        match attempts.get(invite_id) {
            None => true,
            Some((round, at)) => {
                // 已经试过 `round` 轮 → 等第 `round` 号台阶（第 1 轮试完等 30s，第 2 轮完等 2min…）。
                let step = RETRY_BACKOFF[(*round).clamp(1, RETRY_BACKOFF.len() as u32) as usize - 1];
                at.elapsed().as_secs() >= step
            }
        }
    }

    /// 记一次尝试（调用方刚从 `attempt_due` 拿到 `true`）。
    pub fn note_attempt(&self, invite_id: &str) {
        let mut attempts = self.attempts.lock();
        let entry = attempts.entry(invite_id.to_string()).or_insert((0, Instant::now()));
        entry.0 = entry.0.saturating_add(1);
        entry.1 = Instant::now();
    }

    /// 清掉已经不存在的任务的退避记账（任务删了但记账还在 = 内存里一点点泄漏，
    /// 而且 approve 里那个 id 万一被复用会带着旧台阶）。
    pub fn prune_attempts(&self) {
        let live = self.inner.lock().records.pending.keys().cloned().collect::<Vec<_>>();
        self.attempts.lock().retain(|k, _| live.contains(k));
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

/// 这次接收实际写哪个目录：按设置（S-12，`Config::device_subdir`）决定要不要 `<发送方名>/` 那层。
///
/// 关掉时给它**根目录本身**。围栏不靠那一层：清单里的相对路径照旧逐段过 `safe_join`
/// （每个文件都从 `dir` 往下拼、每段必须是普通名字），所以"关掉"只是把"能碰的范围"从
/// `<根>/<发送方名>` 放大到 `<根>`，**没有**放大到根之外 —— 对端自报的名字这时压根不参与拼路径。
pub fn landing_dir(base: &Path, sender_name: &str, device_subdir: bool) -> PathBuf {
    if device_subdir {
        landing_root(base, sender_name)
    } else {
        base.to_path_buf()
    }
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
/// 键是**文件身份**（`stream::resume_key`：对端 id + **落盘目录** + 路径 + 大小 + 时间戳），不是
/// 邀请 id —— 用户"传失败了再推一次"拿到的是新邀请，只有身份是稳定的。按邀请存的话，重推一次
/// 就是从零重传一整遍（老实现正是如此）。身份里带落盘目录，是为了"同一个文件收在两个目录下"
/// 各算各的（见 `stream::resume_key` 那段）。
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

    /// S-12：那颗开关只决定"要不要多一层发送方目录"，**不决定**能不能出根。
    #[test]
    fn device_subdir_toggle_only_decides_the_layer() {
        let base = PathBuf::from("C:\\Users\\u\\Downloads");
        // 开（默认）：`<根>/<净化过的发送方名>`
        assert_eq!(landing_dir(&base, "PC-B", true), base.join("PC-B"));
        // 关：就是根本身 —— 发送方自报的名字这时**不参与拼路径**，连净化都不需要
        assert_eq!(landing_dir(&base, "PC-B", false), base);
        assert_eq!(landing_dir(&base, "..\\..\\evil", false), base);
        // 两种取值下，清单里的相对路径都钉在根里：围栏是 `safe_join`（逐段校验），不是这一层。
        for subdir in [true, false] {
            let dir = landing_dir(&base, "..\\..\\evil", subdir);
            let target = safe_join(&dir, "a/b.txt").expect("普通相对路径该放行");
            assert!(target.starts_with(&base), "逃出根了：{target:?}");
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

#[cfg(test)]
mod pending_tests {
    use super::*;

    fn record(invite: &str, updated_at: i64) -> PendingInbound {
        PendingInbound {
            invite_id: invite.into(),
            sender_id: "peer-a".into(),
            sender_name: "PC-A".into(),
            name: "报告".into(),
            size: 24,
            is_dir: true,
            count: 2,
            files: vec![
                PendingFile { path: "a.bin".into(), size: 16, mtime: 7 },
                PendingFile { path: "空.txt".into(), size: 0, mtime: 0 },
            ],
            caps: vec!["stream1".into()],
            root: Some("D:/下载".into()),
            callback: Some(("127.0.0.1".into(), 47800)),
            updated_at,
        }
    }

    fn store(name: &str) -> (ResumeStore, PathBuf) {
        let path = std::env::temp_dir().join(format!("orbdrop_pending_{name}_{}", uuid::Uuid::new_v4()));
        (ResumeStore::with_path(path.clone()), path)
    }

    /// 待续传任务**要能活过一次重启**：重开一份 store（同一个文件）之后，那几样
    /// （邀请 id / 清单 / 落盘根 / 对端地址）一个不少 —— 这就是"重启后自己接着收"的全部依据。
    #[test]
    fn a_pending_task_survives_a_restart_field_for_field() {
        let (s, path) = store("survive");
        s.put_pending(record("inv-1", now_secs()));

        let reopened = ResumeStore::with_path(path.clone());
        let got = reopened.pending_entries();
        assert_eq!(got.len(), 1);
        let p = &got[0];
        assert_eq!(p.invite_id, "inv-1");
        assert_eq!(p.sender_id, "peer-a");
        assert_eq!(p.files.len(), 2);
        assert_eq!(p.files[1].path, "空.txt");
        assert_eq!(p.root.as_deref(), Some("D:/下载"));
        assert_eq!(p.callback.clone().unwrap().1, 47800);
        assert_eq!(p.caps, vec!["stream1".to_string()], "按同一套协议续");
        // 重建出来的邀请要能直接跑（清单与落盘根逐字段对上）。
        let inv = p.to_invite();
        assert_eq!(inv.name, "报告");
        assert_eq!(inv.count, 2);
        assert_eq!(inv.files[0].size, 16);
        assert!(inv.root.is_some());
        assert!(reopened.pending_ids().contains("inv-1"));
        let _ = std::fs::remove_file(path);
    }

    /// 传完/终态之后任务要删掉（重启不该再自己收一遍）；文件级位图**不跟着删**
    /// （用户手动重推一次仍然省得下那些块）。
    #[test]
    fn dropping_the_task_keeps_the_block_record() {
        let (s, path) = store("drop");
        s.put_pending(record("inv-1", now_secs()));
        s.save_file("key-1", FileResume::new(16, 7, 4, "x.part".into()));

        s.drop_pending("inv-1");
        assert!(s.pending_entries().is_empty());
        assert!(!s.pending_ids().contains("inv-1"));
        assert!(s.file_record("key-1").is_some(), "位图留着");
        // 删了之后，重建出来的那份里也不该再有它。
        let reopened = ResumeStore::with_path(path.clone());
        assert!(reopened.pending_entries().is_empty());
        assert!(reopened.file_record("key-1").is_some());
        let _ = std::fs::remove_file(path);
    }

    /// 退避：第一次立刻试；试过一轮后要等台阶；**重启之后立刻再试**（记账不落盘）。
    #[test]
    fn retry_backoff_is_runtime_only() {
        let (s, path) = store("backoff");
        assert!(s.attempt_due("inv-1"), "没试过 = 立刻试");
        s.note_attempt("inv-1");
        assert!(!s.attempt_due("inv-1"), "刚试过，要等第一级台阶");

        let reopened = ResumeStore::with_path(path.clone());
        assert!(reopened.attempt_due("inv-1"), "重启就是为了让它再试一次");
        let _ = std::fs::remove_file(path);
    }

    /// TTL 按**最后一次有进展**算（7 天）：这个窗口以内留着，过了一点就收尾。
    #[test]
    fn the_ttl_window_is_measured_from_the_last_progress() {
        let now = now_secs();
        assert!(!record("inv-1", now - 6 * 86400).expired(now));
        assert!(record("inv-1", now - 8 * 86400).expired(now));
        // 有进展就把锚点往前挪（`touch_pending` 在"一个文件收完"时叫）。
        let (s, path) = store("touch");
        s.put_pending(record("inv-1", now - 8 * 86400));
        assert!(s.pending_entries()[0].expired(now));
        s.touch_pending("inv-1");
        assert!(!s.pending_entries()[0].expired(now), "刚有进展就不该过期");
        let _ = std::fs::remove_file(path);
    }
}

#[allow(dead_code)]
pub fn log_done(invite_id: &str, file_path: &str, offset: u64) {
    info!(invite_id, file_path, offset, "resume offset updated");
}