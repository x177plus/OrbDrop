//! 共享项更新记录：**共享项设备是唯一真相源**。
//!
//! 其他设备右键「更新记录」时向共享项设备查询（`GET /history`），不在本地另记一份：
//! 只有共享项设备知道谁在写（`/dav` 的 token、`/push/upload` 的 sender、本机 watcher）。
//!
//! 归属靠**短时归属表**，不靠 mtime 猜：
//! - 本机 watcher 看到路径变化时，先查这张表；命中就记成对应设备，没命中就记成本机；
//! - `/dav` 写/删/移动、`/push/upload` 每个文件原子改名之前，由写路径登记归属；
//! - 自动写回在共享项设备看来就是一次 `/push/upload`，因此会记成编辑方设备。
//!
//! 记录按共享项分桶，每桶一个有界环形日志；查询目录时只取每个文件的**最新一条**，
//! 避免大目录把整段历史全吐出来。

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::share::watcher::ChangeEvent;

/// 每个共享项最多保留多少条事件（超出丢最老）。
pub const MAX_ENTRIES_PER_SHARE: usize = 20_000;
/// 一次查询最多返回多少条（目录视图是“每个文件最新一条”）。
pub const MAX_QUERY_LIMIT: usize = 500;
/// 归属表的默认寿命：写路径登记后，watcher 一般 300ms 内就会看到。
pub const ATTRIBUTION_TTL: Duration = Duration::from_secs(15);
/// 上传每个文件在 `rename` 前登记，watcher 几乎立刻看到；短一点，避免紧接着的本机保存被误记。
pub const UPLOAD_ATTRIBUTION_TTL: Duration = Duration::from_secs(3);
/// 同一 `path + actor` 在这段时间内合并成一条。
const COALESCE_MS: i64 = 2_000;

/// 落盘的一条更新记录。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateEntry {
    /// 共享项内相对路径（`/` 分隔）。
    pub path: String,
    /// 事件时刻（epoch 毫秒）。
    pub at: i64,
    /// 写这一项的设备 id。
    pub actor_id: String,
    /// 写这一项时的设备名（展示时优先用当前备注，查不到再用它）。
    pub actor_name: String,
    /// `created` / `modified` / `deleted`。
    pub kind: String,
    /// 这一项是不是目录（目录视图只列文件）。
    #[serde(default)]
    pub is_dir: bool,
    /// `local` / `upload` / `dav`。
    pub source: String,
    #[serde(default)]
    pub size: Option<u64>,
}

/// 返回给界面的记录：多一个 `mine`，相对**查询方**判断“本机/对端”。
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct HistoryView {
    pub path: String,
    pub at: i64,
    pub actor_id: String,
    pub actor_name: String,
    pub kind: String,
    pub is_dir: bool,
    pub source: String,
    pub size: Option<u64>,
    pub mine: bool,
}

impl HistoryView {
    pub fn from_entry(entry: &UpdateEntry, caller_id: &str) -> Self {
        Self {
            path: entry.path.clone(),
            at: entry.at,
            actor_id: entry.actor_id.clone(),
            actor_name: entry.actor_name.clone(),
            kind: entry.kind.clone(),
            is_dir: entry.is_dir,
            source: entry.source.clone(),
            size: entry.size,
            mine: entry.actor_id == caller_id,
        }
    }
}

#[derive(Clone, Debug)]
struct Attribution {
    /// 精确路径或目录前缀（不带尾斜杠）。
    prefix: String,
    actor_id: String,
    actor_name: String,
    source: String,
    until: Instant,
}

#[derive(Default)]
struct ShareLog {
    entries: VecDeque<UpdateEntry>,
    attribution: Vec<Attribution>,
}

#[derive(Default)]
struct Inner {
    shares: HashMap<String, ShareLog>,
    /// 已写入的最大事件时间：同一毫秒内的多条记录也保持先后顺序。
    last_at: i64,
    dirty: bool,
}

#[derive(Default, Serialize, Deserialize)]
struct Disk {
    #[serde(default)]
    shares: HashMap<String, Vec<UpdateEntry>>,
}

/// 共享项更新记录存储。进程内一个，挂在 [`crate::state::AppState::history`]。
pub struct HistoryStore {
    path: PathBuf,
    inner: Mutex<Inner>,
}

impl HistoryStore {
    /// 使用数据目录下的 `share-history.json`。
    pub fn new() -> Self {
        Self::with_path(crate::config::data_dir().join("share-history.json"))
    }

    /// 指定文件（单测用）。
    pub fn with_path(path: PathBuf) -> Self {
        let mut inner = Inner::default();
        if let Ok(bytes) = std::fs::read(&path) {
            if let Ok(disk) = serde_json::from_slice::<Disk>(&bytes) {
                for (share, mut entries) in disk.shares {
                    if entries.len() > MAX_ENTRIES_PER_SHARE {
                        let drop = entries.len() - MAX_ENTRIES_PER_SHARE;
                        entries.drain(..drop);
                    }
                    inner.shares.insert(
                        share,
                        ShareLog {
                            entries: entries.into_iter().collect(),
                            attribution: Vec::new(),
                        },
                    );
                }
            }
        }
        inner.last_at = inner
            .shares
            .values()
            .flat_map(|log| log.entries.iter())
            .map(|entry| entry.at)
            .max()
            .unwrap_or(0);
        Self {
            path,
            inner: Mutex::new(inner),
        }
    }

    /// 登记一次写入归属。`prefix` = 精确文件路径，或目录前缀（不带尾斜杠）；
    /// **空串 = 单文件共享项的根文件**（只对 watcher 报上来的空路径精确命中）。
    pub fn attribute(
        &self,
        share_id: &str,
        prefix: &str,
        actor_id: &str,
        actor_name: &str,
        source: &str,
        ttl: Duration,
    ) {
        let prefix = norm(prefix);
        if !crate::ids::is_segment(share_id) {
            return;
        }
        let mut inner = self.inner.lock().unwrap();
        let log = inner.shares.entry(share_id.to_string()).or_default();
        let now = Instant::now();
        log.attribution.retain(|a| a.until > now);
        // 同一路径的最新一次登记说了算：旧登记（上一次写、上一个设备）不能因为先到而
        // 继续消费下一批 watcher 事件。目录前缀不受影响（前缀不同，各自保留到 TTL）。
        log.attribution.retain(|a| a.prefix != prefix);
        log.attribution.push(Attribution {
            prefix,
            actor_id: actor_id.to_string(),
            actor_name: actor_name.to_string(),
            source: source.to_string(),
            until: now + ttl,
        });
        // 归属表只是短时提示：一次目录上传可能有上万个文件，留个上界防止被异常写路径撑爆。
        if log.attribution.len() > 20_000 {
            log.attribution.drain(..log.attribution.len() - 20_000);
        }
    }

    /// watcher 看到一批变更后调用：逐条记录，actor 由归属表决定，未命中就是本机。
    ///
    /// 同一批里同一路径可能既有 `modified` 又有 `deleted`（原子保存/覆盖）；只记**最后**
    /// 那一档，避免一次写入产生两条互相矛盾的历史，也避免第一条消费掉归属后第二条误记成本机。
    pub fn record_batch(&self, share_id: &str, batch: &[ChangeEvent], local_id: &str, local_name: &str) {
        let mut latest: HashMap<&str, &ChangeEvent> = HashMap::new();
        for event in batch {
            latest.insert(event.path.as_str(), event);
        }
        for event in latest.into_values() {
            self.record_with_dir(
                share_id,
                &event.path,
                &event.kind,
                None,
                event.is_dir,
                local_id,
                local_name,
            );
        }
    }

    /// 记录一条文件变更（`is_dir = false`）。
    pub fn record(
        &self,
        share_id: &str,
        path: &str,
        kind: &str,
        size: Option<u64>,
        local_id: &str,
        local_name: &str,
    ) {
        self.record_with_dir(share_id, path, kind, size, false, local_id, local_name);
    }

    /// 摘掉某条路径的短时归属（本机即将主动删/写它时用）。
    ///
    /// 必须在**落盘动作之前**调用：watcher 的防抖窗口是 300ms，但它与写路径并发，
    /// 先摘掉这条归属，watcher 才不会被上一次对端写入的归属抢走。
    pub fn forget_attribution(&self, share_id: &str, path: &str) {
        let path = norm(path);
        let mut inner = self.inner.lock().unwrap();
        if let Some(log) = inner.shares.get_mut(share_id) {
            log.attribution.retain(|a| a.prefix != path);
        }
    }

    /// 记录一条**本机界面明确发起**的动作（右键删除等）：不查短时归属表。
    ///
    /// `record_with_dir` 会先看归属表：如果对端 15 秒内刚写过这个路径，本机紧接着的删除
    /// 会被误记成"对端删的"。这里先把这一条路径的归属摘掉，再按本机记录；watcher 稍后
    /// 看到同一次删除时会命中同一条（同一 actor、2s 合并窗口）而不会多记一条。
    pub fn record_local(
        &self,
        share_id: &str,
        path: &str,
        kind: &str,
        size: Option<u64>,
        is_dir: bool,
        local_id: &str,
        local_name: &str,
    ) {
        self.forget_attribution(share_id, path);
        self.record_with_dir(share_id, path, kind, size, is_dir, local_id, local_name);
    }

    /// 记录一条变更（含文件/目录标记）。watcher 用这个版本，目录视图据此只列文件。
    /// `actor` 命中归属表时用归属表的设备，否则用 `local_*`。
    pub fn record_with_dir(
        &self,
        share_id: &str,
        path: &str,
        kind: &str,
        size: Option<u64>,
        is_dir: bool,
        local_id: &str,
        local_name: &str,
    ) {
        let path = norm(path);
        if is_internal(&path) || !crate::ids::is_segment(share_id) {
            return;
        }
        let mut inner = self.inner.lock().unwrap();
        let at = (crate::chat::now_ms() as i64).max(inner.last_at.saturating_add(1));
        inner.last_at = at;
        let log = inner.shares.entry(share_id.to_string()).or_default();
        let now = Instant::now();
        log.attribution.retain(|a| a.until > now);
        let (actor_id, actor_name, source) = match match_attribution(&mut log.attribution, &path) {
            Some(attribution) => (
                attribution.actor_id,
                attribution.actor_name,
                attribution.source,
            ),
            None => (
                local_id.to_string(),
                if local_name.is_empty() {
                    local_id.to_string()
                } else {
                    local_name.to_string()
                },
                "local".to_string(),
            ),
        };
        if let Some(last) = log.entries.back_mut() {
            if last.path == path
                && last.actor_id == actor_id
                && at.saturating_sub(last.at) <= COALESCE_MS
            {
                last.at = at;
                last.kind = kind.to_string();
                last.is_dir = is_dir;
                last.source = source;
                last.size = size;
                inner.dirty = true;
                return;
            }
        }
        log.entries.push_back(UpdateEntry {
            path,
            at,
            actor_id,
            actor_name,
            kind: kind.to_string(),
            is_dir,
            source,
            size,
        });
        while log.entries.len() > MAX_ENTRIES_PER_SHARE {
            log.entries.pop_front();
        }
        inner.dirty = true;
    }

    /// 一个文件的完整（有界）时间线，最新在前。
    pub fn query_file(&self, share_id: &str, path: &str, limit: usize) -> Vec<UpdateEntry> {
        let path = norm(path);
        let inner = self.inner.lock().unwrap();
        let Some(log) = inner.shares.get(share_id) else {
            return Vec::new();
        };
        let limit = limit.clamp(1, MAX_QUERY_LIMIT);
        log.entries
            .iter()
            .rev()
            .filter(|entry| entry.path == path)
            .take(limit)
            .cloned()
            .collect()
    }

    /// 目录：前缀下每个文件的**最新一条**，按时间倒序。
    pub fn query_dir(&self, share_id: &str, prefix: &str, limit: usize) -> Vec<UpdateEntry> {
        let prefix = norm(prefix);
        let inner = self.inner.lock().unwrap();
        let Some(log) = inner.shares.get(share_id) else {
            return Vec::new();
        };
        let mut latest: HashMap<&str, &UpdateEntry> = HashMap::new();
        for entry in &log.entries {
            if entry.path.is_empty() {
                // 单文件共享项根的记录只给 `query_file("")` 用；目录视图只列文件。
                continue;
            }
            // 目录自身只在**被删除**时进目录视图：创建/修改一个目录只是"里面可能有文件"的
            // 附带事件；但删除是一条用户要看的记录（尤其是本机/对端右键删掉一整棵目录时）。
            if entry.is_dir && entry.kind != "deleted" {
                continue;
            }
            if !under(&entry.path, &prefix) {
                continue;
            }
            latest
                .entry(entry.path.as_str())
                .and_modify(|old| {
                    if entry.at > old.at {
                        *old = entry;
                    }
                })
                .or_insert(entry);
        }
        let mut entries: Vec<UpdateEntry> = latest.into_values().cloned().collect();
        entries.sort_by(|a, b| b.at.cmp(&a.at));
        entries.truncate(limit.clamp(1, MAX_QUERY_LIMIT));
        entries
    }

    /// 共享项移除时清掉它的历史。
    pub fn remove_share(&self, share_id: &str) {
        let mut inner = self.inner.lock().unwrap();
        if inner.shares.remove(share_id).is_some() {
            inner.dirty = true;
        }
    }

    /// 有变化才落盘。由后台任务每 2s 调一次，退出时再调一次。
    pub fn flush_if_dirty(&self) {
        let mut inner = self.inner.lock().unwrap();
        if !inner.dirty {
            return;
        }
        let disk = Disk {
            shares: inner
                .shares
                .iter()
                .map(|(id, log)| (id.clone(), log.entries.iter().cloned().collect()))
                .collect(),
        };
        let bytes = match serde_json::to_vec(&disk) {
            Ok(bytes) => bytes,
            Err(e) => {
                tracing::warn!(error = %e, "共享项更新记录序列化失败");
                return;
            }
        };
        if let Some(parent) = self.path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        match crate::config::write_atomic(&self.path, &bytes) {
            Ok(()) => inner.dirty = false,
            Err(e) => tracing::warn!(path = %self.path.display(), error = %e, "共享项更新记录落盘失败"),
        }
    }
}

impl Default for HistoryStore {
    fn default() -> Self {
        Self::new()
    }
}

fn match_attribution(attribution: &mut Vec<Attribution>, path: &str) -> Option<Attribution> {
    // 最长前缀优先；精确命中消费掉，目录前缀保留到 TTL（一次目录写入会有很多子事件）。
    let mut best: Option<(usize, bool)> = None;
    for (index, candidate) in attribution.iter().enumerate() {
        let exact = candidate.prefix == path;
        let child = path
            .strip_prefix(&candidate.prefix)
            .is_some_and(|rest| rest.starts_with('/'));
        if !exact && !child {
            continue;
        }
        let len = candidate.prefix.len();
        if best.map_or(true, |(best_len, _)| len > best_len) {
            best = Some((index, exact));
        }
    }
    let (index, exact) = best?;
    let hit = if exact {
        attribution.swap_remove(index)
    } else {
        attribution[index].clone()
    };
    Some(hit)
}

fn under(path: &str, prefix: &str) -> bool {
    if prefix.is_empty() {
        return true;
    }
    // 目录视图只列**它下面的文件**，不把目录自身的一次 created/modified 也算进去。
    path.strip_prefix(prefix)
        .is_some_and(|rest| rest.starts_with('/'))
}

fn is_internal(path: &str) -> bool {
    path.contains(".orbdrop-part") || path.contains(".orbdrop-old-")
}

fn norm(path: &str) -> String {
    path.replace('\\', "/").trim_matches('/').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (HistoryStore, PathBuf) {
        let path = std::env::temp_dir().join(format!(
            "orbdrop_history_{}.json",
            uuid::Uuid::new_v4()
        ));
        (HistoryStore::with_path(path.clone()), path)
    }

    fn ev(path: &str, kind: &str) -> ChangeEvent {
        ChangeEvent {
            share_id: "share-1".into(),
            path: path.into(),
            kind: kind.into(),
            is_dir: false,
        }
    }

    #[test]
    fn local_and_attributed_peer_writes_are_recorded_with_the_right_actor() {
        let (store, path) = store();
        store.record("share-1", "a.txt", "modified", None, "me", "本机");
        store.attribute(
            "share-1",
            "dir",
            "peer-1",
            "对端",
            "upload",
            ATTRIBUTION_TTL,
        );
        store.record_batch(
            "share-1",
            &[ev("dir/b.txt", "created"), ev("other.txt", "modified")],
            "me",
            "本机",
        );
        let entries = store.query_file("share-1", "a.txt", 10);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].actor_id, "me");
        let entries = store.query_dir("share-1", "dir", 10);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].actor_id, "peer-1");
        assert_eq!(entries[0].source, "upload");
        let entries = store.query_file("share-1", "other.txt", 10);
        assert_eq!(entries[0].actor_id, "me");
        store.remove_share("share-1");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn exact_attribution_is_consumed_so_a_later_local_save_is_not_misattributed() {
        let (store, path) = store();
        store.attribute("share-1", "a.txt", "peer-1", "对端", "dav", ATTRIBUTION_TTL);
        store.record("share-1", "a.txt", "modified", None, "me", "本机");
        store.record("share-1", "a.txt", "modified", None, "me", "本机");
        let entries = store.query_file("share-1", "a.txt", 10);
        assert_eq!(entries[0].actor_id, "me", "第二次保存应回归本机");
        let _ = std::fs::remove_file(path);
    }

    /// 单文件共享项根的历史路径是空串：归属也必须能落在这条上，否则写回会被记成本机。
    #[test]
    fn single_file_share_root_can_be_attributed() {
        let (store, path) = store();
        store.attribute("share-1", "", "peer-1", "对端", "upload", ATTRIBUTION_TTL);
        store.record_batch("share-1", &[ev("", "modified")], "me", "本机");
        let entries = store.query_file("share-1", "", 10);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, "");
        assert_eq!(entries[0].actor_id, "peer-1");
        assert_eq!(entries[0].source, "upload");
        let _ = std::fs::remove_file(path);
    }

    /// 本机右键删除不能被"对端 15 秒内刚写过这个路径"的归属抢走：删除记录必须是本机，
    /// 而且 watcher 稍后补的那条 `deleted` 应该并进同一条（不是第二条、也不是对端）。
    #[test]
    fn local_delete_wins_over_a_recent_peer_write() {
        let (store, path) = store();
        store.attribute("share-1", "a.txt", "peer-1", "对端", "dav", ATTRIBUTION_TTL);
        store.record_local("share-1", "a.txt", "deleted", None, false, "me", "本机");
        store.record_batch("share-1", &[ev("a.txt", "deleted")], "me", "本机");
        let entries = store.query_file("share-1", "a.txt", 10);
        assert_eq!(entries.len(), 1, "同一次删除只该有一条记录");
        assert_eq!(entries[0].actor_id, "me");
        assert_eq!(entries[0].kind, "deleted");
        let _ = std::fs::remove_file(path);
    }

    /// 同一路径的归属以**最后一次登记**为准：上一个设备的旧登记不能继续消费下一批事件。
    #[test]
    fn newer_attribution_for_the_same_path_wins() {
        let (store, path) = store();
        store.attribute("share-1", "a.txt", "peer-1", "甲", "dav", ATTRIBUTION_TTL);
        store.attribute("share-1", "a.txt", "peer-2", "乙", "upload", ATTRIBUTION_TTL);
        store.record_batch("share-1", &[ev("a.txt", "modified")], "me", "本机");
        let entries = store.query_file("share-1", "a.txt", 10);
        assert_eq!(entries[0].actor_id, "peer-2");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn directory_query_returns_only_the_latest_entry_per_file() {
        let (store, path) = store();
        store.record("share-1", "dir/a.txt", "created", None, "me", "本机");
        store.record("share-1", "dir/b.txt", "modified", None, "me", "本机");
        store.record("share-1", "dir/a.txt", "modified", None, "peer-1", "对端");
        store.record("share-1", "other.txt", "modified", None, "me", "本机");
        let entries = store.query_dir("share-1", "dir", 10);
        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].path, "dir/a.txt");
        assert_eq!(entries[0].actor_id, "peer-1");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn history_roundtrips_through_disk() {
        let (store, path) = store();
        store.record("share-1", "a.txt", "modified", Some(7), "me", "本机");
        store.flush_if_dirty();
        let loaded = HistoryStore::with_path(path.clone());
        let entries = loaded.query_file("share-1", "a.txt", 10);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].size, Some(7));
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn internal_temp_files_are_ignored() {
        let (store, path) = store();
        store.record("share-1", ".a.orbdrop-part", "modified", None, "me", "本机");
        store.record("share-1", ".a.orbdrop-old-x", "modified", None, "me", "本机");
        assert!(store.query_dir("share-1", "", 10).is_empty());
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn directory_entries_are_not_listed_as_changed_files() {
        let (store, path) = store();
        store.record_with_dir(
            "share-1",
            "dir",
            "created",
            None,
            true,
            "me",
            "本机",
        );
        store.record("share-1", "dir/a.txt", "modified", None, "me", "本机");
        let entries = store.query_dir("share-1", "", 10);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, "dir/a.txt");
        let _ = std::fs::remove_file(path);
    }

    /// 删除目录是一条用户要看的记录：目录视图里要出现它（创建/修改目录仍然不出现）。
    #[test]
    fn deleted_directories_are_listed_in_the_directory_view() {
        let (store, path) = store();
        store.record_with_dir(
            "share-1",
            "gone",
            "deleted",
            None,
            true,
            "me",
            "本机",
        );
        let entries = store.query_dir("share-1", "", 10);
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, "gone");
        assert_eq!(entries[0].kind, "deleted");
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn view_marks_the_callers_own_entries() {
        let (store, path) = store();
        store.record("share-1", "a.txt", "modified", None, "me", "本机");
        store.record("share-1", "b.txt", "modified", None, "peer-1", "对端");
        let entries = store.query_dir("share-1", "", 10);
        let views: Vec<HistoryView> = entries
            .iter()
            .map(|entry| HistoryView::from_entry(entry, "me"))
            .collect();
        assert!(views.iter().any(|view| view.path == "a.txt" && view.mine));
        assert!(views.iter().any(|view| view.path == "b.txt" && !view.mine));
        let _ = std::fs::remove_file(path);
    }
}