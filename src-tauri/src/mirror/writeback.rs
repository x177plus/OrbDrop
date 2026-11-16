//! 「打开远端文件」的本地工作副本登记：远端坐标 → **一个稳定本地路径** + 同步基线。
//!
//! 与旧实现的区别（旧版未对外，直接替换）：
//!
//! - 登记**移出镜像根**：Windows 退出会整棵删镜像树，放在里面的登记会跟着丢。现在落在
//!   `<数据目录>/writeback/journal.json`，与镜像树的生命周期解耦。
//! - 一个远端坐标只对应**一个本地路径**，远端版本变化不换路径；macOS/Android 也从
//!   “每次按版本哈希分新槽”改成 `<镜像根>/<peer>/<share>/work/<相对路径>`。
//! - 远端基线用 `rev`（大小 + mtime 纳秒）而不是秒级 mtime；写回条件提交以它为准。
//! - `dirty` 是持久化标记：Windows 退出前会把未写回的副本导出到
//!   `<数据目录>/writeback/pending/…`，重启后即使本机文件已不在镜像里也还能续传。
//!
//! 运行期的状态（是否在传、退避到什么时候）不落盘，由 `mirror::auto` 维护；这里只存
//! “本地副本此刻基于远端哪个版本、本地基线是什么”这两件事实。

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};

use serde::{Deserialize, Serialize};
use tracing::warn;

use super::PlacedItem;

/// 条件写回冲突的统一标记。接收端在原子改名之前拒绝时用它开头，发送端据此把失败
/// 归类成 `Conflict` 而不是可无限重试的 `Failed`。
pub const CONFLICT_MARKER: &str = "写回冲突";

/// 同一远端坐标 → 本地工作副本的登记。
#[derive(Clone, Debug, Serialize, Deserialize)]
struct Entry {
    peer_id: String,
    share_id: String,
    /// 共享项内相对路径（`/` 分隔、无首尾斜杠）。
    path: String,
    /// 本地工作副本的绝对路径（登记时原样记；查找时再 canonicalize）。
    local: String,
    /// 本地副本基于的远端版本；`None` = 打开时问不到。
    #[serde(default)]
    rev: Option<String>,
    /// 打开时对端的大小 / 毫秒时间戳；`None` = 当时问不到。
    #[serde(default)]
    size: Option<u64>,
    #[serde(default)]
    mtime_ms: Option<i64>,
    /// 本地基线：上一次“已同步/已写回”的内容的大小 / 毫秒时间戳。
    #[serde(default)]
    local_size: Option<u64>,
    #[serde(default)]
    local_mtime_ms: Option<i64>,
    /// Windows 退出前导出的待写回副本：文件已经不是编辑器里那份路径，但它确实还没写回。
    #[serde(default)]
    dirty: bool,
}

/// 写回命令/自动任务要用的那一份工作副本。
#[derive(Clone, Debug)]
pub struct OpenedCopy {
    pub peer_id: String,
    pub share_id: String,
    pub path: String,
    pub local: PathBuf,
    pub rev: Option<String>,
    pub size: Option<u64>,
    pub mtime_ms: Option<i64>,
    pub local_size: Option<u64>,
    pub local_mtime_ms: Option<i64>,
    /// 持久化的“待写回”标记（见 [`export_dirty_pending`]）。
    pub dirty: bool,
}

/// 命名的唯一口径：普通项取远端相对路径最后一段；共享项根文件（`path == ""`）
/// 取本地工作副本的文件名。自动写回的通知、刷新日志与手动写回共用这一份。
pub fn display_name(opened: &OpenedCopy) -> String {
    opened
        .path
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .or_else(|| {
            opened
                .local
                .file_name()
                .map(|name| name.to_string_lossy().into_owned())
                .filter(|name| !name.is_empty())
        })
        .unwrap_or_else(|| "文件".to_string())
}

#[derive(Default, Serialize, Deserialize)]
struct Journal {
    #[serde(default)]
    entries: Vec<Entry>,
}

/// 进程内串行化读-改-写：打开、写回、刷新都是低频操作，但可能并发。
static LOCK: LazyLock<Mutex<()>> = LazyLock::new(|| Mutex::new(()));

/// 登记与待写回副本的根：`<数据目录>/writeback`。
pub fn root() -> PathBuf {
    crate::config::data_dir().join("writeback")
}

/// Windows 退出前导出的待写回副本根。
pub fn pending_root() -> PathBuf {
    root().join("pending")
}

/// 写回前拍的稳定快照根（真正上传的是这里的副本，不是编辑器还在写的那份）。
pub fn staging_root() -> PathBuf {
    root().join("staging")
}

fn journal_path(root: &Path) -> PathBuf {
    root.join("journal.json")
}

fn load(root: &Path) -> Vec<Entry> {
    let path = journal_path(root);
    let Ok(bytes) = std::fs::read(&path) else {
        return Vec::new();
    };
    match serde_json::from_slice::<Journal>(&bytes) {
        Ok(journal) => journal.entries,
        Err(e) => {
            // 不自动备份/重建：文件坏了就让用户能看到/手工抢救，按空表继续只会把
            // 后续写入盖上去。日志里留路径已经足够定位。
            warn!(path = %path.display(), error = %e, "写回登记文件损坏，按空表处理");
            Vec::new()
        }
    }
}

fn save(root: &Path, entries: &[Entry]) {
    let path = journal_path(root);
    let bytes = match serde_json::to_vec_pretty(&Journal {
        entries: entries.to_vec(),
    }) {
        Ok(bytes) => bytes,
        Err(e) => {
            warn!(error = %e, "写回登记序列化失败");
            return;
        }
    };
    if let Err(e) = std::fs::create_dir_all(root) {
        warn!(path = %root.display(), error = %e, "写回登记目录建不出来");
        return;
    }
    if let Err(e) = crate::config::write_atomic(&path, &bytes) {
        warn!(path = %path.display(), error = %e, "写回登记落盘失败");
    }
}

/// 记录一次「打开」：同坐标或同本地路径的旧记录先清掉，再写新的。
///
/// 目录不登记（写回面只做文件）；空路径是**单文件共享项**的根，也照常登记 ——
/// 写回目标由 `UploadDest.path == ""` 表达（接收端把共享根当文件本身覆盖）。
/// 登记失败只记日志，不能让“打开”因此失败。
pub fn record_opened(peer_id: &str, share_id: &str, path: &str, placed: &PlacedItem) {
    record_at(
        &root(),
        &super::root(),
        &pending_root(),
        peer_id,
        share_id,
        path,
        placed,
    );
}

/// 找到这个远端项对应的、仍可信的本地工作副本。
pub fn find_opened(peer_id: &str, share_id: &str, path: &str) -> Option<OpenedCopy> {
    find_at(
        &root(),
        &super::root(),
        &pending_root(),
        peer_id,
        share_id,
        path,
    )
}

/// 此刻所有仍可信的工作副本（自动同步轮询用）。
///
/// 失效项不在这里删除：编辑器原子保存会短暂让路径消失，运行期的一拍看不到不能当作
/// “这一项没了”。启动时由 [`prune_missing`] 清理真正缺失的登记。
pub fn list_opened() -> Vec<OpenedCopy> {
    list_at(&root(), &super::root(), &pending_root())
}

/// 写回链路已经接手（或副本失效）：删掉登记，下一次写回必须重新打开。
pub fn forget_opened(peer_id: &str, share_id: &str, path: &str) {
    forget_at(
        &root(),
        &pending_root(),
        peer_id,
        share_id,
        path,
    );
}

/// 删除目录（或文件）后清掉它和它**下面**所有工作副本的登记。
///
/// 远端目录被递归删除时，只按目录路径摘登记不够：目录里那些打开过/改过的文件仍在登记表里，
/// 自动写回会把它们重新上传，等于让删除"复活"。这里按 `path` 前缀一起摘掉；本地工作副本
/// 本身不删（用户可能还开着，想留也能留）。
pub fn forget_opened_under(peer_id: &str, share_id: &str, path: &str) {
    forget_under_at(&root(), &pending_root(), peer_id, share_id, path);
}

/// 本地副本在打开后是否被改过（大小或 mtime_ms 与登记基线不同，或已被标记待写回）。
///
/// 大小/时间戳都读不到时按“没改”处理（不能凭猜测拦人）。
pub fn local_changed(opened: &OpenedCopy) -> bool {
    if opened.dirty {
        return true;
    }
    let (size, mtime) = local_snapshot(&opened.local);
    match (opened.local_size, size) {
        (Some(before), Some(now)) if before != now => return true,
        _ => {}
    }
    match (opened.local_mtime_ms, mtime) {
        (Some(before), Some(now)) if before != now => return true,
        _ => {}
    }
    false
}

/// 本机副本此刻的大小 / 毫秒修改时间。写回开始前拍一张，成功后把它当作
/// “已经写上去的那一份”的基线；传输期间用户又改过文件时，重新打开仍会被识别为有未写回修改。
pub fn local_snapshot(path: &Path) -> (Option<u64>, Option<i64>) {
    local_stat(path)
}

/// 写回成功后更新登记：远端基线换成新值，本地基线换成**上传时**那一份快照。
///
/// 不在这里重新读本地文件：传输过程中用户可能又改了它；把最新状态当基线会让那次编辑
/// 被误认为“已经写回”。调用方传进来的是发起上传时的快照。
///
/// 如果这一份是从 `pending` 接管的（Windows 退出前导出的），成功之后就没有“工作副本”
/// 可言了：删掉 pending 文件并移除登记，用户下次重新打开会得到一份新的镜像副本。
#[allow(clippy::too_many_arguments)]
pub fn settle_opened(
    peer_id: &str,
    share_id: &str,
    path: &str,
    rev: Option<String>,
    size: Option<u64>,
    mtime_ms: Option<i64>,
    local_size: Option<u64>,
    local_mtime_ms: Option<i64>,
) {
    settle_at(
        &root(),
        &pending_root(),
        peer_id,
        share_id,
        path,
        rev,
        size,
        mtime_ms,
        local_size,
        local_mtime_ms,
    );
}

/// 远端刷新成功：同一个本地路径已经换成新的远端版本。
pub fn record_refreshed(peer_id: &str, share_id: &str, path: &str, placed: &PlacedItem) {
    let _guard = LOCK.lock().unwrap();
    let root = root();
    let mut entries = load(&root);
    let path = norm_remote(path);
    let Some(entry) = entries
        .iter_mut()
        .find(|e| e.peer_id == peer_id && e.share_id == share_id && e.path == path)
    else {
        return;
    };
    let (local_size, local_mtime_ms) = local_stat(&placed.path);
    entry.local = placed.path.to_string_lossy().into_owned();
    entry.rev = placed.rev.clone();
    entry.size = placed.size;
    entry.mtime_ms = placed.mtime_ms;
    entry.local_size = local_size;
    entry.local_mtime_ms = local_mtime_ms;
    entry.dirty = false;
    save(&root, &entries);
}

/// 同一个远端项同时只允许跑一条写回：第二次点击直接拒绝，避免两条上传互相覆盖。
static IN_FLIGHT: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

/// 写回互斥的 RAII 票据：命令返回（成功或失败）时自动放出。
pub struct InFlight {
    key: String,
}

impl Drop for InFlight {
    fn drop(&mut self) {
        IN_FLIGHT.lock().unwrap().remove(&self.key);
    }
}

/// 占住这个远端项的写回位。返回 `None` = 已经有一条在跑。
pub fn begin_writeback(peer_id: &str, share_id: &str, path: &str) -> Option<InFlight> {
    let key = format!("{peer_id}\0{share_id}\0{}", norm_remote(path));
    let mut in_flight = IN_FLIGHT.lock().unwrap();
    if !in_flight.insert(key.clone()) {
        return None;
    }
    Some(InFlight { key })
}

/// 拆出写回目标：`(共享项内的父目录, 对端原名)`。
///
/// 普通上传会对名字做跨平台净化；写回用的是对端已存在的原名，调用方（清单构造）也会对
/// 登记过的源跳过净化。这里只按接收侧 [`crate::transfer::resume::safe_join`] 的口径拒绝
/// 真正落不了盘的名字（冒号、保留设备名、结尾点/空格、斜杠），合法的前导点原样保留。
///
/// 空路径是**单文件共享项根**，没有可拆的父目录；调用方（`perform_writeback`）会用
/// `dest.path == ""` 直接表达“覆盖共享根文件”，不经过这里。
pub fn remote_target(path: &str) -> Result<(String, String), String> {
    let path = norm_remote(path);
    if path.is_empty() {
        return Err("空路径不是可拆分的写回目标".into());
    }
    let (parent, name) = match path.rsplit_once('/') {
        Some((parent, name)) => (parent.to_string(), name.to_string()),
        None => (String::new(), path),
    };
    if name.is_empty() || name == "." || name == ".." {
        return Err("写回目标路径不合法".into());
    }
    let valid = !name.contains('/')
        && crate::transfer::resume::safe_join(Path::new("."), &name).is_some();
    if !valid {
        return Err(format!(
            "「{name}」不能作为对端文件名（跨平台限制），暂不支持写回；请用「上传到此文件夹…」手动覆盖"
        ));
    }
    Ok((parent, name))
}

/// 稳定工作副本的本地路径：`<work 根>/<共享项内相对路径>`。
///
/// mac/Android 的“打开”走这里。远端路径逐段清洗（与拖出同一条 [`peer_fs::plain_component`]
/// 口径），最终落点必须仍在 `work_root` 下。
pub fn work_path(work_root: &Path, remote_path: &str, name: &str) -> Result<PathBuf, String> {
    let path = if remote_path.trim_matches('/').is_empty() {
        super::identity::child_path(work_root, name)?
    } else {
        super::identity::join_remote(work_root, remote_path)?
    };
    if !crate::unsendable::is_equal_or_under(&path, work_root) {
        return Err("工作副本落点越界".into());
    }
    Ok(path)
}

/// 打开时用的工作副本路径：同一个远端坐标从登记里取回**上次那个路径**；新坐标若和
/// 别的项清洗后撞到同一个本地路径，就加一个稳定的短哈希后缀，避免两个远端文件共用
/// 一份工作副本（`record_opened` 会把同本地路径的旧登记顶掉，那等于丢编辑）。
pub fn stable_local_path(
    peer_id: &str,
    share_id: &str,
    remote_path: &str,
    name: &str,
) -> Result<PathBuf, String> {
    let journal = root();
    let work_root = super::identity::share_dir(&super::root(), peer_id, share_id)?.join("work");
    stable_local_path_at(&journal, &work_root, peer_id, share_id, remote_path, name)
}

fn stable_local_path_at(
    journal_root: &Path,
    work_root: &Path,
    peer_id: &str,
    share_id: &str,
    remote_path: &str,
    name: &str,
) -> Result<PathBuf, String> {
    let remote_path = norm_remote(remote_path);
    let _guard = LOCK.lock().unwrap();
    let entries = load(journal_root);
    if let Some(entry) = entries
        .iter()
        .find(|e| e.peer_id == peer_id && e.share_id == share_id && e.path == remote_path)
    {
        let local = Path::new(&entry.local);
        // 旧版本（未对外）的暂存槽不是 `work/` 下的稳定路径：不沿用，按新布局重算；
        // 干净内容的旧文件会由 `record_opened` 让位，随后被 sweep 回收。pending 是
        // 退出前抢救出来的普通文件，继续沿用（不能因为路径不在 work/ 下就丢掉）。
        if crate::unsendable::is_equal_or_under(local, work_root)
            || crate::unsendable::is_equal_or_under(local, &pending_root())
        {
            return Ok(local.to_path_buf());
        }
    }
    std::fs::create_dir_all(work_root).map_err(|e| format!("建工作副本目录失败: {e}"))?;
    let base = work_path(work_root, &remote_path, name)?;
    let occupied = entries.iter().any(|entry| {
        let local = Path::new(&entry.local);
        crate::unsendable::is_equal_or_under(local, &base)
            && crate::unsendable::is_equal_or_under(&base, local)
            && !(entry.peer_id == peer_id && entry.share_id == share_id && entry.path == remote_path)
    });
    if !occupied && !base.exists() {
        return Ok(base);
    }
    let hash = short_hash(&format!("{peer_id}\0{share_id}\0{remote_path}"));
    Ok(suffix_path(&base, &hash))
}

fn short_hash(text: &str) -> String {
    // FNV-1a：只求稳定、短，不用于安全判断。跨版本不变，路径不会因为编译器升级而漂移。
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in text.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("{hash:08x}")
}

fn suffix_path(path: &Path, hash: &str) -> PathBuf {
    let stem = path
        .file_stem()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    let ext = path
        .extension()
        .map(|s| s.to_string_lossy().into_owned());
    let name = match ext {
        Some(ext) if !ext.is_empty() => format!("{stem}~{hash}.{ext}"),
        _ => format!("{stem}~{hash}"),
    };
    path.with_file_name(name)
}

/// 把一份本地副本按**普通字节**复制出来。
///
/// 不能用 `std::fs::copy`：Windows 上它走 `CopyFileEx`，面对 CFAPI 占位符可能复制重解析点
/// 元数据/跳过未水合区间，而不是触发按需读取。这里显式 `read` + `write`，保证拿到的是
/// 真实内容（未水合的区间会由 provider 在读取时拉回来）。
pub fn copy_plain(src: &Path, dest: &Path) -> std::io::Result<u64> {
    use std::io::{Read, Write};
    // 同源同目标会把文件截断成 0 字节：`File::create(dest)` 先动手，再 read 就没了。
    if let (Ok(from), Ok(to)) = (std::fs::canonicalize(src), std::fs::canonicalize(dest)) {
        if crate::unsendable::is_equal_or_under(&from, &to)
            && crate::unsendable::is_equal_or_under(&to, &from)
        {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                "source and destination are the same file",
            ));
        }
    }
    let mut input = std::fs::File::open(src)?;
    let mut output = std::fs::File::create(dest)?;
    let mut buf = vec![0u8; 1024 * 1024];
    let mut total = 0u64;
    loop {
        let n = input.read(&mut buf)?;
        if n == 0 {
            break;
        }
        output.write_all(&buf[..n])?;
        total += n as u64;
    }
    output.sync_all()?;
    Ok(total)
}

/// 写回前把本地副本复制成一份**稳定快照**，返回 `(快照路径, 原始本地基线)`。
///
/// 编辑器可能在传输期间继续写；直接读原路径会拼出一份半新半旧的文件。快照放在
/// `<数据目录>/writeback/staging/…`，上传链路允许从这里读字节；调用方结束后用
/// [`cleanup_staging`] 清掉整个槽目录。
pub fn stage_snapshot(local: &Path) -> Result<(PathBuf, (Option<u64>, Option<i64>)), String> {
    let source_meta = std::fs::metadata(local).map_err(|e| format!("读取写回源失败: {e}"))?;
    let baseline = local_stat(local);
    let name = local
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| "writeback".into());
    let slot = staging_root().join(uuid::Uuid::new_v4().to_string());
    std::fs::create_dir_all(&slot).map_err(|e| format!("建写回快照目录失败: {e}"))?;
    let dest = slot.join(name);
    if let Err(e) = copy_plain(local, &dest) {
        let _ = std::fs::remove_dir_all(&slot);
        return Err(format!("写回前复制本地副本失败: {e}"));
    }
    // 复制期间又保存了：这份快照可能是半新半旧，丢掉等下一轮安静窗口再拍。
    if local_stat(local) != baseline {
        let _ = std::fs::remove_dir_all(&slot);
        return Err("本地副本在拍写回快照期间又被修改；稍后重试".into());
    }
    // 快照的 mtime 跟源文件一致：清单里的 `mtime` 是接收端续传键的一部分。复制时间
    // 每次都不同，会让“失败后重试/重启续传”退化成从零重传。
    if let Ok(modified) = source_meta.modified() {
        if let Ok(file) = std::fs::OpenOptions::new().write(true).open(&dest) {
            let _ = file.set_times(std::fs::FileTimes::new().set_modified(modified));
        }
    }
    Ok((dest, baseline))
}

/// 删掉 [`stage_snapshot`] 建出来的槽。
pub fn cleanup_staging(snapshot: &Path) {
    if let Some(slot) = snapshot.parent() {
        if slot.starts_with(staging_root()) {
            let _ = std::fs::remove_dir_all(slot);
        }
    }
}

/// 退出前把**所有未写回**的工作副本导出到 `pending`。
///
/// Windows 的 `mirror::shutdown` 会整棵删镜像树（占位符离开 provider 后不可读），所以必须在
/// 删之前把脏内容读出来变成普通文件。读占位符会触发水合；对端不在线时这一步会失败。
///
/// 返回**未能导出的脏条目数**：大于 0 时调用方必须保留镜像树与同步根注册（见
/// `mirror::shutdown`），否则那部分编辑就真丢了。
pub fn export_dirty_pending() -> usize {
    let journal_root = root();
    let pending = pending_root();
    let mirror = super::root();
    let _guard = LOCK.lock().unwrap();
    let mut entries = load(&journal_root);
    let mut changed = false;
    let mut failed = 0usize;
    for entry in &mut entries {
        let Some(verified) = verify(&mirror, &pending, entry) else {
            // 文件已经不在（或不可信）：没有可导出的内容。
            continue;
        };
        if !entry.dirty && !local_changed_raw(entry, &verified) {
            continue;
        }
        let dest = match pending_path(&pending, entry) {
            Ok(path) => path,
            Err(e) => {
                warn!(peer = %entry.peer_id, path = %entry.path, error = %e, "导出待写回副本失败：落点不合法");
                failed += 1;
                continue;
            }
        };
        if let Some(parent) = dest.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                warn!(path = %parent.display(), error = %e, "建待写回目录失败");
                continue;
            }
        }
        let same_dest = std::fs::canonicalize(&dest).ok().is_some_and(|dest| {
            crate::unsendable::is_equal_or_under(&dest, &verified)
                && crate::unsendable::is_equal_or_under(&verified, &dest)
        });
        if same_dest {
            entry.local = dest.to_string_lossy().into_owned();
            entry.dirty = true;
            changed = true;
            continue;
        }
        match copy_plain(&verified, &dest) {
            Ok(_) => {
                entry.local = dest.to_string_lossy().into_owned();
                entry.dirty = true;
                changed = true;
            }
            Err(e) => {
                warn!(path = %verified.display(), error = %e, "导出待写回副本失败");
                failed += 1;
            }
        }
    }
    if changed {
        save(&journal_root, &entries);
    }
    failed
}

/// 启动时清掉本地副本已经不在的登记。
pub fn prune_missing() {
    prune_staging();
    let _guard = LOCK.lock().unwrap();
    let root = root();
    let pending = pending_root();
    let mirror = super::root();
    let mut entries = load(&root);
    let before = entries.len();
    entries.retain(|entry| verify(&mirror, &pending, entry).is_some());
    if entries.len() != before {
        save(&root, &entries);
    }
}

/// 清掉上次进程崩溃/被杀留下的 staging 槽（正常路径由 `StagingGuard` 清）。
/// 只删超过一天的：正在跑的写回不会在启动这一拍被别人删掉。
fn prune_staging() {
    let root = staging_root();
    let Ok(dir) = std::fs::read_dir(&root) else {
        return;
    };
    let cutoff = std::time::SystemTime::now()
        .checked_sub(std::time::Duration::from_secs(24 * 60 * 60))
        .unwrap_or(std::time::SystemTime::UNIX_EPOCH);
    for entry in dir.flatten() {
        let path = entry.path();
        let stale = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .map(|modified| modified < cutoff)
            .unwrap_or(false);
        if stale {
            let _ = std::fs::remove_dir_all(&path);
        }
    }
}

/// 当前登记里仍可信的本地路径（canonical）。`mirror::sweep` 用它避开工作副本。
pub fn registered_paths() -> HashSet<PathBuf> {
    let _guard = LOCK.lock().unwrap();
    let mut out = HashSet::new();
    for entry in load(&root()) {
        if let Some(path) = verify(&super::root(), &pending_root(), &entry) {
            out.insert(path);
        }
    }
    out
}

fn record_at(
    journal_root: &Path,
    mirror_root: &Path,
    pending: &Path,
    peer_id: &str,
    share_id: &str,
    path: &str,
    placed: &PlacedItem,
) {
    if placed.is_dir {
        return;
    }
    let path = norm_remote(path);
    let local = placed.path.to_string_lossy().into_owned();
    let (local_size, local_mtime_ms) = local_stat(&placed.path);
    let _guard = LOCK.lock().unwrap();
    let _ = (mirror_root, pending);
    let mut entries = load(journal_root);
    let previous = entries
        .iter()
        .find(|e| e.peer_id == peer_id && e.share_id == share_id && e.path == path)
        .cloned();
    entries.retain(|e| {
        !(e.peer_id == peer_id && e.share_id == share_id && e.path == path)
            && e.local != local
    });
    entries.push(Entry {
        peer_id: peer_id.to_string(),
        share_id: share_id.to_string(),
        path,
        local,
        // 探测失败时 `place` 可能返回一份本地已有的副本（macOS 离线复用）；这时不能
        // 把已知的基线清成 None，否则远端变化检测就退化了。
        rev: placed
            .rev
            .clone()
            .or_else(|| previous.as_ref().and_then(|e| e.rev.clone())),
        size: placed
            .size
            .or_else(|| previous.as_ref().and_then(|e| e.size)),
        mtime_ms: placed
            .mtime_ms
            .or_else(|| previous.as_ref().and_then(|e| e.mtime_ms)),
        local_size,
        local_mtime_ms,
        dirty: false,
    });
    save(journal_root, &entries);
}

fn opened_from(entry: &Entry) -> OpenedCopy {
    OpenedCopy {
        peer_id: entry.peer_id.clone(),
        share_id: entry.share_id.clone(),
        path: entry.path.clone(),
        // 用登记时的原路径而不是 canonical：Windows 的 canonical 会带 `\\?\`，
        // 清单那两道目录闸门按原样比较，带前缀反而绕过它们（写回源已另行校验）。
        local: PathBuf::from(&entry.local),
        rev: entry.rev.clone(),
        size: entry.size,
        mtime_ms: entry.mtime_ms,
        local_size: entry.local_size,
        local_mtime_ms: entry.local_mtime_ms,
        dirty: entry.dirty,
    }
}

fn find_at(
    journal_root: &Path,
    mirror_root: &Path,
    pending: &Path,
    peer_id: &str,
    share_id: &str,
    path: &str,
) -> Option<OpenedCopy> {
    let path = norm_remote(path);
    let _guard = LOCK.lock().unwrap();
    let entries = load(journal_root);
    let index = entries.iter().position(|e| {
        e.peer_id == peer_id && e.share_id == share_id && e.path == path
    })?;
    match verify(mirror_root, pending, &entries[index]) {
        Some(_) => Some(opened_from(&entries[index])),
        None => None,
    }
}

fn list_at(journal_root: &Path, mirror_root: &Path, pending: &Path) -> Vec<OpenedCopy> {
    let _guard = LOCK.lock().unwrap();
    let entries = load(journal_root);
    entries
        .iter()
        .filter(|entry| verify(mirror_root, pending, entry).is_some())
        .map(opened_from)
        .collect()
}

fn forget_at(journal_root: &Path, pending: &Path, peer_id: &str, share_id: &str, path: &str) {
    let path = norm_remote(path);
    let _guard = LOCK.lock().unwrap();
    let mut entries = load(journal_root);
    let mut removed: Vec<PathBuf> = Vec::new();
    let before = entries.len();
    entries.retain(|e| {
        let hit = e.peer_id == peer_id && e.share_id == share_id && e.path == path;
        if hit && e.dirty {
            removed.push(PathBuf::from(&e.local));
        }
        !hit
    });
    if entries.len() != before {
        save(journal_root, &entries);
        // pending 副本是我们自己导出的临时物，登记没了就不用留；镜像里的工作副本
        // 不删（用户可能还想从本地拿回内容）。
        for local in removed {
            if is_under(&local, pending) {
                let _ = std::fs::remove_file(&local);
            }
        }
    }
}

/// [`forget_opened_under`] 的实现：命中「同一路径」或「以 `path/` 开头」的全部登记。
fn forget_under_at(
    journal_root: &Path,
    pending: &Path,
    peer_id: &str,
    share_id: &str,
    path: &str,
) {
    let base = norm_remote(path);
    let prefix = format!("{base}/");
    let _guard = LOCK.lock().unwrap();
    let mut entries = load(journal_root);
    let mut removed: Vec<PathBuf> = Vec::new();
    let before = entries.len();
    entries.retain(|e| {
        let hit = e.peer_id == peer_id
            && e.share_id == share_id
            && (e.path == base || e.path.starts_with(&prefix));
        if hit && e.dirty {
            removed.push(PathBuf::from(&e.local));
        }
        !hit
    });
    if entries.len() != before {
        save(journal_root, &entries);
        for local in removed {
            if is_under(&local, pending) {
                let _ = std::fs::remove_file(&local);
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn settle_at(
    journal_root: &Path,
    pending: &Path,
    peer_id: &str,
    share_id: &str,
    path: &str,
    rev: Option<String>,
    size: Option<u64>,
    mtime_ms: Option<i64>,
    local_size: Option<u64>,
    local_mtime_ms: Option<i64>,
) {
    let path = norm_remote(path);
    let _guard = LOCK.lock().unwrap();
    let mut entries = load(journal_root);
    let Some(index) = entries
        .iter()
        .position(|e| e.peer_id == peer_id && e.share_id == share_id && e.path == path)
    else {
        return;
    };
    // pending 是“退出前抢救出来的待写回副本”：写回成功之后它就没有继续存在的意义，
    // 删掉并移除登记；用户下次打开会重新建一份正常的工作副本。
    if entries[index].dirty && is_under(Path::new(&entries[index].local), pending) {
        let local = PathBuf::from(&entries[index].local);
        entries.remove(index);
        save(journal_root, &entries);
        let _ = std::fs::remove_file(local);
        return;
    }
    let entry = &mut entries[index];
    entry.rev = rev;
    entry.size = size;
    entry.mtime_ms = mtime_ms;
    entry.local_size = local_size;
    entry.local_mtime_ms = local_mtime_ms;
    entry.dirty = false;
    save(journal_root, &entries);
}

/// 登记是否还指向一份**仍在镜像根或 pending 根下**的普通文件。
fn verify(mirror_root: &Path, pending: &Path, entry: &Entry) -> Option<PathBuf> {
    let local = PathBuf::from(&entry.local);
    let local = std::fs::canonicalize(&local).ok()?;
    let mirror = std::fs::canonicalize(mirror_root).ok();
    let pending = std::fs::canonicalize(pending).ok();
    let under = |root: Option<&PathBuf>| -> bool {
        root.is_some_and(|root| crate::unsendable::is_equal_or_under(&local, root))
    };
    if !under(mirror.as_ref()) && !under(pending.as_ref()) {
        return None;
    }
    // 不仅要在根下，还要落在**这一项自己的** <peer>/<share> 目录里：登记文件被改坏时，
    // 也不能把另一个共享项的副本当成这一项的写回源。
    let mut expected_ok = false;
    for root in [mirror.as_ref(), pending.as_ref()].into_iter().flatten() {
        if let Ok(expected) = super::identity::share_dir(root, &entry.peer_id, &entry.share_id) {
            if let Ok(expected) = std::fs::canonicalize(expected) {
                if crate::unsendable::is_equal_or_under(&local, &expected) {
                    expected_ok = true;
                    break;
                }
            }
        }
    }
    if !expected_ok {
        return None;
    }
    let meta = std::fs::symlink_metadata(&local).ok()?;
    if !meta.is_file() || crate::unsendable::is_link(&meta.file_type()) {
        return None;
    }
    Some(local)
}

fn pending_path(pending: &Path, entry: &Entry) -> Result<PathBuf, String> {
    let dir = super::identity::share_dir(pending, &entry.peer_id, &entry.share_id)?;
    std::fs::create_dir_all(&dir).map_err(|e| format!("建待写回目录失败: {e}"))?;
    let mut dest = if entry.path.is_empty() {
        let name = Path::new(&entry.local)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .ok_or_else(|| "待写回副本没有文件名".to_string())?;
        super::identity::child_path(&dir, &name)?
    } else {
        super::identity::join_remote(&dir, &entry.path)?
    };
    // 清洗后可能撞名：已经存在且不是这一项自己的旧落点，就加稳定后缀，绝不能把
    // 另一项的待写回内容覆盖掉。
    if dest.exists() && dest != Path::new(&entry.local) {
        let hash = short_hash(&format!(
            "{}\0{}\0{}",
            entry.peer_id, entry.share_id, entry.path
        ));
        dest = suffix_path(&dest, &hash);
    }
    Ok(dest)
}

fn is_under(path: &Path, root: &Path) -> bool {
    let Ok(path) = std::fs::canonicalize(path) else {
        return false;
    };
    let Ok(root) = std::fs::canonicalize(root) else {
        return false;
    };
    crate::unsendable::is_equal_or_under(&path, &root)
}

fn norm_remote(path: &str) -> String {
    path.replace('\\', "/").trim_matches('/').to_string()
}

fn local_stat(path: &Path) -> (Option<u64>, Option<i64>) {
    let meta = std::fs::metadata(path).ok();
    let size = meta.as_ref().map(|meta| meta.len());
    let mtime = meta
        .and_then(|meta| meta.modified().ok())
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|age| age.as_millis() as i64);
    (size, mtime)
}

fn local_changed_raw(entry: &Entry, local: &Path) -> bool {
    let (size, mtime) = local_stat(local);
    match (entry.local_size, size) {
        (Some(before), Some(now)) if before != now => return true,
        _ => {}
    }
    match (entry.local_mtime_ms, mtime) {
        (Some(before), Some(now)) if before != now => return true,
        _ => {}
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "orbdrop_writeback_{tag}_{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn placed(path: &Path) -> PlacedItem {
        PlacedItem {
            path: path.to_path_buf(),
            size: Some(7),
            mtime: Some(1_700_000_000),
            mtime_ms: Some(1_700_000_000_000),
            rev: Some("7-18f".into()),
            is_dir: false,
        }
    }

    fn roots() -> (PathBuf, PathBuf, PathBuf) {
        let data = temp_dir("data");
        let mirror = data.join("mirror");
        let pending = data.join("writeback").join("pending");
        std::fs::create_dir_all(&mirror).unwrap();
        std::fs::create_dir_all(&pending).unwrap();
        (data.join("writeback"), mirror, pending)
    }

    fn file_in(root: &Path, peer: &str, share: &str, name: &str) -> PathBuf {
        let dir = root.join(peer).join(share);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        std::fs::write(&path, b"hello").unwrap();
        path
    }

    #[test]
    fn remote_target_splits_parent_and_original_name() {
        assert_eq!(
            remote_target("dir/a.txt").unwrap(),
            ("dir".to_string(), "a.txt".to_string())
        );
        assert_eq!(
            remote_target("a.txt").unwrap(),
            (String::new(), "a.txt".to_string())
        );
        assert_eq!(
            remote_target("dir\\sub\\a.txt").unwrap(),
            ("dir/sub".to_string(), "a.txt".to_string())
        );
        assert_eq!(
            remote_target(".gitignore").unwrap(),
            (String::new(), ".gitignore".to_string()),
            "对端原名里的前导点要保留（上传清单的净化会吃掉它）"
        );
        assert_eq!(
            remote_target("dir/.env").unwrap(),
            ("dir".to_string(), ".env".to_string())
        );
        assert!(remote_target("").is_err(), "空路径由调用方按共享根处理，不能在这里拆");
        assert!(remote_target("a:b.txt").is_err(), "接收侧 safe_join 会拒冒号");
        assert!(remote_target("dir/CON").is_err(), "Windows 保留名");
        assert!(remote_target("dir/trailing.").is_err(), "结尾点落不了盘");
    }

    #[test]
    fn a_recorded_copy_round_trips_until_forgotten() {
        let (journal, mirror, pending) = roots();
        let local = file_in(&mirror, "peer", "share", "a.txt");

        record_at(&journal, &mirror, &pending, "peer", "share", "dir/a.txt", &placed(&local));
        let opened = find_at(&journal, &mirror, &pending, "peer", "share", "dir/a.txt").unwrap();
        assert_eq!(
            opened.local, local,
            "返回登记时的原路径（不是带 \\\\?\\ 的 canonical）"
        );
        assert_eq!(opened.size, Some(7));
        assert_eq!(opened.mtime_ms, Some(1_700_000_000_000));
        assert_eq!(opened.rev.as_deref(), Some("7-18f"));

        forget_at(&journal, &pending, "peer", "share", "dir/a.txt");
        assert!(find_at(&journal, &mirror, &pending, "peer", "share", "dir/a.txt").is_none());

        std::fs::remove_dir_all(journal.parent().unwrap()).unwrap();
    }

    /// 单文件共享项的根（`path == ""`）也登记；显示名与待写回副本的名字都取工作副本文件名。
    #[test]
    fn single_file_share_root_is_registered_and_named_after_the_copy() {
        let (journal, mirror, pending) = roots();
        let local = file_in(&mirror, "peer", "share", "report.pdf");

        record_at(&journal, &mirror, &pending, "peer", "share", "", &placed(&local));
        let opened = find_at(&journal, &mirror, &pending, "peer", "share", "").unwrap();
        assert_eq!(opened.path, "");
        assert_eq!(display_name(&opened), "report.pdf");

        let entries = load(&journal);
        assert_eq!(
            pending_path(&pending, &entries[0]).unwrap(),
            pending.join("peer").join("share").join("report.pdf"),
            "待写回副本用文件名，不能拼出空路径"
        );

        forget_at(&journal, &pending, "peer", "share", "");
        assert!(find_at(&journal, &mirror, &pending, "peer", "share", "").is_none());
        std::fs::remove_dir_all(journal.parent().unwrap()).unwrap();
    }

    /// 删除一个目录时，目录里打开过/改过的文件登记也要一起摘掉，否则自动写回会把它们
    /// 重新上传，等于让删除"复活"。
    #[test]
    fn forgetting_a_directory_drops_descendant_registrations() {
        let (journal, mirror, pending) = roots();
        let dir = mirror.join("peer").join("share").join("dir").join("sub");
        std::fs::create_dir_all(&dir).unwrap();
        let a = mirror.join("peer").join("share").join("dir").join("a.txt");
        let b = dir.join("b.txt");
        let other = mirror.join("peer").join("share").join("other.txt");
        for file in [&a, &b, &other] {
            std::fs::write(file, b"hello").unwrap();
        }
        record_at(&journal, &mirror, &pending, "peer", "share", "dir/a.txt", &placed(&a));
        record_at(&journal, &mirror, &pending, "peer", "share", "dir/sub/b.txt", &placed(&b));
        record_at(&journal, &mirror, &pending, "peer", "share", "other.txt", &placed(&other));

        forget_under_at(&journal, &pending, "peer", "share", "dir");

        assert!(find_at(&journal, &mirror, &pending, "peer", "share", "dir/a.txt").is_none());
        assert!(find_at(&journal, &mirror, &pending, "peer", "share", "dir/sub/b.txt").is_none());
        assert!(
            find_at(&journal, &mirror, &pending, "peer", "share", "other.txt").is_some(),
            "目录之外的登记不能被误删"
        );

        std::fs::remove_dir_all(journal.parent().unwrap()).unwrap();
    }

    /// 登记表是本地文件：即便被改出一条指向镜像根之外的路径，也不能把任意文件当副本写回。
    #[test]
    fn a_copy_outside_the_trusted_roots_is_not_trusted() {
        let (journal, mirror, pending) = roots();
        let outside = temp_dir("outside_file");
        let local = outside.join("secret.txt");
        std::fs::write(&local, b"secret").unwrap();

        record_at(&journal, &mirror, &pending, "peer", "share", "a.txt", &placed(&local));
        assert!(
            find_at(&journal, &mirror, &pending, "peer", "share", "a.txt").is_none(),
            "镜像根/pending 之外的文件不能通过登记表变成写回源"
        );

        std::fs::remove_dir_all(journal.parent().unwrap()).unwrap();
        std::fs::remove_dir_all(&outside).unwrap();
    }

    /// 同一个本地路径被另一项复用时，旧登记必须让位；否则写回会把别人的内容传上去。
    #[test]
    fn recording_a_new_item_at_the_same_local_path_replaces_the_old_record() {
        let (journal, mirror, pending) = roots();
        let local = file_in(&mirror, "peer", "share", "a.txt");

        record_at(&journal, &mirror, &pending, "peer", "share", "old.txt", &placed(&local));
        record_at(&journal, &mirror, &pending, "peer", "share", "new.txt", &placed(&local));

        assert!(find_at(&journal, &mirror, &pending, "peer", "share", "old.txt").is_none());
        assert!(find_at(&journal, &mirror, &pending, "peer", "share", "new.txt").is_some());

        std::fs::remove_dir_all(journal.parent().unwrap()).unwrap();
    }

    #[test]
    fn listing_returns_verifiable_copies_and_startup_prune_drops_missing_files() {
        let (journal, mirror, pending) = roots();
        let good = file_in(&mirror, "peer", "share", "good.txt");
        let outside = temp_dir("list_outside").join("outside.txt");
        std::fs::write(&outside, b"no").unwrap();

        record_at(&journal, &mirror, &pending, "peer", "share", "good.txt", &placed(&good));
        record_at(&journal, &mirror, &pending, "peer", "share", "outside.txt", &placed(&outside));

        let list = list_at(&journal, &mirror, &pending);
        assert_eq!(list.len(), 1, "只返回能通过可信路径校验的副本");
        assert_eq!(list[0].path, "good.txt");

        std::fs::remove_file(&good).unwrap();
        assert!(list_at(&journal, &mirror, &pending).is_empty());
        assert!(
            find_at(&journal, &mirror, &pending, "peer", "share", "good.txt").is_none(),
            "文件没了就不可信"
        );

        std::fs::remove_dir_all(journal.parent().unwrap()).unwrap();
        std::fs::remove_dir_all(outside.parent().unwrap()).unwrap();
    }

    #[test]
    fn local_edits_are_detected_and_cleared_only_by_settling() {
        let (journal, mirror, pending) = roots();
        let local = file_in(&mirror, "peer", "share", "a.txt");

        record_at(&journal, &mirror, &pending, "peer", "share", "a.txt", &placed(&local));
        let opened = find_at(&journal, &mirror, &pending, "peer", "share", "a.txt").unwrap();
        assert!(!local_changed(&opened), "刚打开时没有本地修改");

        std::fs::write(&local, b"hello world").unwrap();
        let opened = find_at(&journal, &mirror, &pending, "peer", "share", "a.txt").unwrap();
        assert!(local_changed(&opened), "大小变了就是编辑过");

        let uploaded = local_snapshot(&local);
        settle_at(
            &journal,
            &pending,
            "peer",
            "share",
            "a.txt",
            Some("b-1".into()),
            Some(11),
            Some(1_700_000_100_000),
            uploaded.0,
            uploaded.1,
        );
        let opened = find_at(&journal, &mirror, &pending, "peer", "share", "a.txt").unwrap();
        assert!(!local_changed(&opened), "成功写回后基线跟上，不再算未写回");
        assert_eq!(opened.size, Some(11), "远端基线也要换成写回后的值");
        assert_eq!(opened.rev.as_deref(), Some("b-1"));

        std::fs::remove_dir_all(journal.parent().unwrap()).unwrap();
    }

    #[test]
    fn exported_pending_copy_survives_mirror_deletion_and_uploads_after_settle() {
        let (journal, mirror, pending) = roots();
        let local = file_in(&mirror, "peer", "share", "a.txt");
        record_at(&journal, &mirror, &pending, "peer", "share", "a.txt", &placed(&local));
        std::fs::write(&local, b"edited").unwrap();

        // 模拟 Windows 退出前的导出：登记必须指向 pending，且不会再因为镜像树被删而失效。
        let mut entries = load(&journal);
        let entry = entries.iter_mut().find(|e| e.path == "a.txt").unwrap();
        let dest = pending.join("peer").join("share").join("a.txt");
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::copy(&local, &dest).unwrap();
        entry.local = dest.to_string_lossy().into_owned();
        entry.dirty = true;
        save(&journal, &entries);
        std::fs::remove_dir_all(&mirror).unwrap();

        let opened = find_at(&journal, &mirror, &pending, "peer", "share", "a.txt")
            .expect("pending 副本必须仍可信");
        assert!(local_changed(&opened), "dirty 标记不能因为重启丢掉");

        settle_at(
            &journal,
            &pending,
            "peer",
            "share",
            "a.txt",
            Some("c-1".into()),
            Some(6),
            Some(1_700_000_200_000),
            Some(6),
            Some(1_700_000_200_000),
        );
        assert!(
            find_at(&journal, &mirror, &pending, "peer", "share", "a.txt").is_none(),
            "pending 写回成功后登记与临时副本都应清掉"
        );
        assert!(!dest.exists());

        std::fs::remove_dir_all(journal.parent().unwrap()).unwrap();
    }

    #[test]
    fn only_one_writeback_can_run_at_a_time() {
        let tag = uuid::Uuid::new_v4().to_string();
        let first = begin_writeback("peer", "share", &tag).expect("第一条能占位");
        assert!(
            begin_writeback("peer", "share", &tag).is_none(),
            "同项第二条写回必须当场拒绝"
        );
        drop(first);
        assert!(
            begin_writeback("peer", "share", &tag).is_some(),
            "第一条结束后可以再发"
        );
    }

    #[test]
    fn work_paths_mirror_the_remote_layout_and_stay_stable() {
        let root = temp_dir("work_path");
        assert_eq!(
            work_path(&root, "dir/a.txt", "a.txt").unwrap(),
            root.join("dir").join("a.txt")
        );
        assert_eq!(
            work_path(&root, "dir/a.txt", "a.txt").unwrap(),
            work_path(&root, "dir/a.txt", "a.txt").unwrap(),
            "同一个远端路径必须得到同一个本地路径"
        );
        assert_eq!(
            work_path(&root, "", ".env").unwrap(),
            root.join(".env"),
            "单文件共享项根用显示名，前导点保留"
        );
        std::fs::remove_dir_all(&root).unwrap();
    }

    #[test]
    fn sanitized_collisions_get_distinct_stable_paths() {
        let (journal, mirror, pending) = roots();
        let work = mirror.join("peer").join("share").join("work");
        std::fs::create_dir_all(&work).unwrap();
        let first =
            stable_local_path_at(&journal, &work, "peer", "share", "dir/a:b.txt", "a:b.txt")
                .unwrap();
        std::fs::create_dir_all(first.parent().unwrap()).unwrap();
        std::fs::write(&first, b"first").unwrap();
        record_at(
            &journal,
            &mirror,
            &pending,
            "peer",
            "share",
            "dir/a:b.txt",
            &placed(&first),
        );
        let second =
            stable_local_path_at(&journal, &work, "peer", "share", "dir/a_b.txt", "a_b.txt")
                .unwrap();
        assert_ne!(first, second, "清洗后撞名不能共用一份工作副本");
        assert!(
            second.to_string_lossy().contains('~'),
            "第二项应带稳定的短哈希后缀: {}",
            second.display()
        );
        std::fs::remove_dir_all(journal.parent().unwrap()).unwrap();
    }
}