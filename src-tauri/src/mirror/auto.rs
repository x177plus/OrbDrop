//! 「打开」工作副本的双向同步：本地保存自动写回，对端变化自动刷新同一路径。
//!
//! 核心不是“轮询一个副本”，而是维护一组**事实**：
//!
//! ```text
//! dirty_local  = 本地 size/mtime_ms != 登记里的本地基线（或 dirty 标记）
//! dirty_remote = 最近探测到的远端 rev != 本地副本基于的 rev
//! ```
//!
//! - `(false, false)` → `Synced`：等下一次本地保存/远端失效。
//! - `(true, false)`  → `LocalDirty`：安静 2s 后走条件写回。
//! - `(false, true)`  → `RemoteDirty`：本地无修改，原地刷新同一路径。
//! - `(true, true)`   → `Diverged`：冲突，不自动覆盖任何一侧，等用户处理。
//!
//! 远端失效优先走长连接：`ws::session` 收到 `ShareChanged` 后调用
//! [`note_share_changed`]，这里只对**该共享项下的工作副本**做一次 `/stat`。60s 的兜底
//! 轮询仍然保留（老对端/长连接抖动时不会漏），但不再是主路径。
//!
//! 写回/刷新都在独立任务里跑，循环只负责调度与结果收口；同项同一时刻只允许一条写回
//! （`writeback::begin_writeback`），刷新也由 `refreshing` 标志串行化。

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use tauri::{AppHandle, Emitter};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use super::writeback::{self, OpenedCopy};
use crate::state::AppState;
use crate::WritebackFailureKind;

/// 本地元数据轮询节拍。只 stat 登记表里的少量文件，开销可以忽略；CFAPI 水合不会改
/// size/mtime，所以这条判据不会自己触发自己。
const POLL: Duration = Duration::from_secs(1);
/// 最后一次本地元数据变化之后要安静这么久；跨过编辑器的分块写/临时文件替换。
const QUIET: Duration = Duration::from_millis(2_000);
/// 写回失败后的最小退避；每次失败翻倍，封顶。
const MIN_BACKOFF: Duration = Duration::from_secs(5);
const MAX_BACKOFF: Duration = Duration::from_secs(5 * 60);
/// 同项已有写回在跑：不打扰用户，稍后再看。
const BUSY_RETRY: Duration = Duration::from_secs(2);
/// 没有 `ShareChanged` 时的兜底探测间隔（老对端 / 长连接抖动）。
const REMOTE_FALLBACK: Duration = Duration::from_secs(60);

/// 共享项级失效：`ShareChanged` 一批只带最后一条路径，所以按共享项记，探测时把该
/// 共享项下的所有工作副本都问一遍。条目数很小（用户同时打开的文件）。
static INVALIDATED: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

/// 长连接收到 `ShareChanged` 时调用：下一次轮询把该共享项下的工作副本标脏。
pub fn note_share_changed(share_id: &str) {
    INVALIDATED.lock().unwrap().insert(share_id.to_string());
}

fn take_invalidated() -> HashSet<String> {
    std::mem::take(&mut *INVALIDATED.lock().unwrap())
}

struct Track {
    entry: OpenedCopy,
    /// 上一次轮询看到的本地大小/mtime_ms。
    last_seen: (Option<u64>, Option<i64>),
    /// 最后一次看到本地元数据变化（或首次跟踪）的时刻。
    last_change: Instant,
    in_flight: bool,
    refreshing: bool,
    next_attempt: Instant,
    backoff: Duration,
    /// 冲突/副本失效后停下，等用户强制写回/重新打开。
    blocked: bool,
    last_error: Option<String>,
    /// 最近一次从对端读到的 rev；与 `entry.rev` 不同就是 `dirty_remote`。
    remote_rev: Option<String>,
    last_remote_check: Instant,
    remote_checking: bool,
    remote_check_due: bool,
    /// 同一个冲突只提示一次，直到本地/远端又发生变化。
    conflict_notified: bool,
    /// 远端变化但本地文件正被占用、刷新要等关闭：只提示一次。
    remote_notified: bool,
}

/// 启动自动同步。一个进程一个；`start` 只应在 setup 里调用一次。
pub fn start(state: Arc<AppState>, app: AppHandle) {
    crate::task::spawn("auto writeback", async move {
        let (tx, mut rx) = mpsc::unbounded_channel::<(String, Result<(), crate::WritebackFailure>)>();
        let (refresh_tx, mut refresh_rx) =
            mpsc::unbounded_channel::<(String, Result<(), crate::WritebackFailure>)>();
        let (probe_tx, mut probe_rx) =
            mpsc::unbounded_channel::<(String, Option<crate::peer_fs::EntryMeta>)>();
        let mut tracks: HashMap<String, Track> = HashMap::new();
        loop {
            // 先收上一拍发出去的结果（不阻塞：结果最多晚一个 tick 处理）。
            while let Ok((key, result)) = rx.try_recv() {
                apply_upload_result(&mut tracks, &app, &key, result);
            }
            while let Ok((key, result)) = refresh_rx.try_recv() {
                apply_refresh_result(&mut tracks, &app, &key, result);
            }
            while let Ok((key, remote)) = probe_rx.try_recv() {
                apply_probe(&mut tracks, &app, &key, remote);
            }

            let invalidated = take_invalidated();
            let opened = writeback::list_opened();
            let present: HashSet<String> = opened.iter().map(key_of).collect();
            tracks.retain(|key, _| present.contains(key));

            let now = Instant::now();
            for entry in &opened {
                let key = key_of(entry);
                let seen = writeback::local_snapshot(&entry.local);
                let track = tracks.entry(key.clone()).or_insert_with(|| Track {
                    entry: entry.clone(),
                    last_seen: seen,
                    last_change: now,
                    in_flight: false,
                    refreshing: false,
                    next_attempt: now,
                    backoff: MIN_BACKOFF,
                    blocked: false,
                    last_error: None,
                    remote_rev: entry.rev.clone(),
                    last_remote_check: now,
                    remote_checking: false,
                    remote_check_due: false,
                    conflict_notified: false,
                    remote_notified: false,
                });
                // 坐标/基线可能已被手动写回/刷新更新。
                track.entry = entry.clone();
                if invalidated.contains(&entry.share_id) {
                    track.remote_check_due = true;
                }

                if seen != track.last_seen {
                    track.last_seen = seen;
                    track.last_change = now;
                    track.next_attempt = now;
                    // 用户又改了：值得再试一次。冲突档仍由上传前的 rev 核对拦住，
                    // 不在这里无限重置一个已经分叉的项。
                    if track.remote_rev == track.entry.rev {
                        track.blocked = false;
                        track.conflict_notified = false;
                    }
                    continue;
                }

                let dirty_local = writeback::local_changed(entry);
                let remote_changed = remote_differs(&track.remote_rev, &track.entry.rev);

                if !dirty_local {
                    // 本地干净：远端变了就刷新同一路径。刷新期间用户可能保存，刷新任务
                    // 自己会在开始/结束各查一次。
                    track.blocked = false;
                    if !remote_changed {
                        track.remote_notified = false;
                    }
                    if remote_changed
                        && !track.refreshing
                        && !track.in_flight
                        && now >= track.next_attempt
                    {
                        schedule_refresh(&state, &app, track, &refresh_tx, now);
                    }
                } else {
                    if !remote_changed {
                        // 对端又回到了本地副本基于的版本（例如用户手动恢复了文件）：
                        // 之前的冲突阻断可以解除，继续自动写回。
                        track.blocked = false;
                        track.conflict_notified = false;
                    }
                    if remote_changed {
                        // 双方都改：不自动上传，也不自动覆盖本地。等用户选。
                        if !track.conflict_notified {
                            track.conflict_notified = true;
                            track.blocked = true;
                            warn!(
                                peer = %entry.peer_id,
                                path = %entry.path,
                                "本地与对端都改过，自动同步停下等用户处理"
                            );
                            emit(&app, entry, "conflict", Some(
                                "对端文件在打开后已变化；本地也有修改。请选择用本地覆盖、用远端覆盖或保留两份",
                            ));
                        }
                    } else if !track.blocked && !track.in_flight && !track.refreshing {
                        if should_upload(
                            now,
                            track.blocked,
                            track.in_flight,
                            track.last_change,
                            track.next_attempt,
                            peer_ready(&state, &entry.peer_id),
                        ) {
                            schedule_upload(&state, track, &tx, now);
                        }
                    }
                }

                schedule_probe(&state, track, &probe_tx, now);
            }

            tokio::time::sleep(POLL).await;
        }
    });
}

fn schedule_upload(
    state: &Arc<AppState>,
    track: &mut Track,
    tx: &mpsc::UnboundedSender<(String, Result<(), crate::WritebackFailure>)>,
    now: Instant,
) {
    let entry = track.entry.clone();
    track.in_flight = true;
    track.next_attempt = now + track.backoff;
    let state = state.clone();
    let key = key_of(&entry);
    let tx = tx.clone();
    debug!(
        peer = %entry.peer_id,
        path = %entry.path,
        "本地工作副本稳定，开始自动写回"
    );
    crate::task::spawn("auto writeback upload", async move {
        let result = crate::perform_writeback(
            state,
            entry.peer_id.clone(),
            entry.share_id.clone(),
            entry.path.clone(),
            None,
            false,
            true,
        )
        .await;
        let _ = tx.send((key, result));
    });
}

fn schedule_refresh(
    state: &Arc<AppState>,
    app: &AppHandle,
    track: &mut Track,
    tx: &mpsc::UnboundedSender<(String, Result<(), crate::WritebackFailure>)>,
    _now: Instant,
) {
    let entry = track.entry.clone();
    // Windows 上有活动句柄时不能删/重建占位符；等 `note_closed` 之后下一拍再来。
    if super::is_open(&entry.local) {
        if !track.remote_notified {
            track.remote_notified = true;
            emit_remote_changed(
                app,
                &entry,
                false,
                Some("对端已更新；文件正在使用，关闭后会刷新本地副本"),
            );
        }
        return;
    }
    track.remote_notified = true;
    let state = state.clone();
    let app = app.clone();
    let key = key_of(&entry);
    let tx = tx.clone();
    info!(peer = %entry.peer_id, path = %entry.path, "对端版本已变化，刷新本地工作副本");
    crate::task::spawn("auto writeback refresh", async move {
        let result = crate::refresh_opened_copy(state, app, &entry, false).await;
        let _ = tx.send((key, result));
    });
}

fn schedule_probe(
    state: &Arc<AppState>,
    track: &mut Track,
    tx: &mpsc::UnboundedSender<(String, Option<crate::peer_fs::EntryMeta>)>,
    now: Instant,
) {
    if track.remote_checking || track.in_flight || track.refreshing {
        return;
    }
    let due = track.remote_check_due
        || track.remote_rev.is_none()
        || now >= track.last_remote_check + REMOTE_FALLBACK;
    if !due || !peer_ready(state, &track.entry.peer_id) {
        return;
    }
    track.remote_checking = true;
    track.remote_check_due = false;
    track.last_remote_check = now;
    let entry = track.entry.clone();
    let key = key_of(&entry);
    let state = state.clone();
    let tx = tx.clone();
    crate::task::spawn("auto writeback remote check", async move {
        let remote = probe_remote(&state, &entry).await;
        let _ = tx.send((key, remote));
    });
}

fn apply_upload_result(
    tracks: &mut HashMap<String, Track>,
    app: &AppHandle,
    key: &str,
    result: Result<(), crate::WritebackFailure>,
) {
    let Some(track) = tracks.get_mut(key) else {
        return;
    };
    track.in_flight = false;
    match result {
        Ok(()) => {
            track.blocked = false;
            track.last_error = None;
            track.backoff = MIN_BACKOFF;
            track.next_attempt = Instant::now();
            track.conflict_notified = false;
            if let Some(fresh) = writeback::find_opened(
                &track.entry.peer_id,
                &track.entry.share_id,
                &track.entry.path,
            ) {
                track.entry = fresh;
                track.remote_rev = track.entry.rev.clone();
                track.last_seen = writeback::local_snapshot(&track.entry.local);
            }
            info!(peer = %track.entry.peer_id, path = %track.entry.path, "自动写回完成");
        }
        Err(failure) => match failure.kind {
            WritebackFailureKind::Busy => {
                track.next_attempt = Instant::now() + BUSY_RETRY;
            }
            WritebackFailureKind::Conflict => {
                track.blocked = true;
                if !track.conflict_notified {
                    track.conflict_notified = true;
                    warn!(
                        peer = %track.entry.peer_id,
                        path = %track.entry.path,
                        error = %failure.message,
                        "自动写回被对端变化拦下，等用户处理"
                    );
                    emit(app, &track.entry, "conflict", Some(&failure.message));
                }
                track.last_error = Some(failure.message);
            }
            WritebackFailureKind::Gone => {
                track.blocked = true;
                track.next_attempt = Instant::now() + MAX_BACKOFF;
                let already = track.last_error.as_deref() == Some(failure.message.as_str());
                track.last_error = Some(failure.message.clone());
                warn!(
                    peer = %track.entry.peer_id,
                    path = %track.entry.path,
                    error = %failure.message,
                    "自动写回暂停（副本或路径不可用）"
                );
                if !already {
                    emit(app, &track.entry, "blocked", Some(&failure.message));
                }
            }
            WritebackFailureKind::Failed => {
                let delay = track.backoff;
                track.backoff = (track.backoff * 2).min(MAX_BACKOFF);
                track.next_attempt = Instant::now() + delay;
                if track.last_error.as_deref() != Some(failure.message.as_str()) {
                    warn!(
                        peer = %track.entry.peer_id,
                        path = %track.entry.path,
                        error = %failure.message,
                        "自动写回失败，稍后重试"
                    );
                    emit(app, &track.entry, "failed", Some(&failure.message));
                    track.last_error = Some(failure.message);
                }
            }
        },
    }
}

fn apply_refresh_result(
    tracks: &mut HashMap<String, Track>,
    app: &AppHandle,
    key: &str,
    result: Result<(), crate::WritebackFailure>,
) {
    let Some(track) = tracks.get_mut(key) else {
        return;
    };
    track.refreshing = false;
    match result {
        Ok(()) => {
            track.blocked = false;
            track.last_error = None;
            track.conflict_notified = false;
            track.remote_notified = false;
            track.backoff = MIN_BACKOFF;
            track.next_attempt = Instant::now();
            if let Some(fresh) = writeback::find_opened(
                &track.entry.peer_id,
                &track.entry.share_id,
                &track.entry.path,
            ) {
                track.entry = fresh;
                track.remote_rev = track.entry.rev.clone();
                track.last_seen = writeback::local_snapshot(&track.entry.local);
            }
            info!(peer = %track.entry.peer_id, path = %track.entry.path, "本地工作副本已刷新");
            emit(app, &track.entry, "refreshed", None);
        }
Err(failure) => {
            if failure.kind == WritebackFailureKind::Busy {
                track.next_attempt = Instant::now() + BUSY_RETRY;
                return;
            }
            // 刷新失败不阻塞本地写回：本地若仍干净，退避后继续试；本地若变脏，
            // 下一拍会按“双方都改”进入冲突。
            let already = track.last_error.as_deref() == Some(failure.message.as_str());
            track.last_error = Some(failure.message.clone());
            if failure.kind == WritebackFailureKind::Conflict {
                track.blocked = true;
                if !track.conflict_notified {
                    track.conflict_notified = true;
                    emit(&app, &track.entry, "conflict", Some(&failure.message));
                }
            } else {
                let delay = track.backoff;
                track.backoff = (track.backoff * 2).min(MAX_BACKOFF);
                track.next_attempt = Instant::now() + delay;
            }
            warn!(
                peer = %track.entry.peer_id,
                path = %track.entry.path,
                error = %failure.message,
                "刷新本地工作副本失败"
            );
            if !already {
                emit(&app, &track.entry, "refresh-failed", Some(&failure.message));
            }
        }
    }
}

fn apply_probe(
    tracks: &mut HashMap<String, Track>,
    app: &AppHandle,
    key: &str,
    remote: Option<crate::peer_fs::EntryMeta>,
) {
    let Some(track) = tracks.get_mut(key) else {
        return;
    };
    track.remote_checking = false;
    let Some(remote) = remote else {
        return;
    };
    track.remote_rev = remote.rev.clone();
    if !remote_differs(&track.remote_rev, &track.entry.rev) {
        return;
    }
    let dirty_local = writeback::local_changed(&track.entry);
    if dirty_local {
        if !track.conflict_notified {
            track.conflict_notified = true;
            track.blocked = true;
            warn!(
                peer = %track.entry.peer_id,
                path = %track.entry.path,
                "探测到对端变化且本地有修改，自动同步停下"
            );
            emit_remote_changed(
                app,
                &track.entry,
                true,
                Some("对端已更新这一项，本地也有修改；写回会冲突"),
            );
        }
        return;
    }
    // 本地干净：下一拍刷新。这里不直接发下载任务，循环里统一调度更简单。
    track.remote_check_due = false;
}

async fn probe_remote(
    state: &Arc<AppState>,
    entry: &OpenedCopy,
) -> Option<crate::peer_fs::EntryMeta> {
    let (base, token) = crate::peer_fs::coords(state, &entry.peer_id).ok()?;
    let client = crate::peer_fs::client_for(state.as_ref(), &entry.peer_id).ok()?;
    let meta = crate::peer_fs::probe_entry(
        &client,
        &base,
        &token,
        &entry.share_id,
        &entry.path,
    )
    .await
    .ok()
    .flatten()?;
    (!meta.is_dir).then_some(meta)
}

fn remote_differs(remote: &Option<String>, base: &Option<String>) -> bool {
    match (remote.as_deref(), base.as_deref()) {
        (Some(remote), Some(base)) => remote != base,
        // 本地基线没有 rev（打开时没问到）：现在问到了就按“可能已变化”处理。
        // 本地干净时刷新是安全的；本地脏时宁可停下来让用户选，也不猜着上传。
        (Some(_), None) => true,
        // 对端没报 rev：没有任何可靠判据，不猜。
        (None, _) => false,
    }
}

/// 这一拍是否该发写回。调用方已经确认：
/// - 本地元数据自上一拍起没再变（否则只是记录变化、重新计时）；
/// - 当前副本相对登记基线是脏的（否则没有要写的东西）；
/// - 远端 rev 与本地基线一致（否则是冲突，不该上传）。
fn should_upload(
    now: Instant,
    blocked: bool,
    in_flight: bool,
    last_change: Instant,
    next_attempt: Instant,
    eligible: bool,
) -> bool {
    !blocked
        && !in_flight
        && eligible
        && now.saturating_duration_since(last_change) >= QUIET
        && now >= next_attempt
}

/// 上传面前置条件：本机在线、目标已配对、长连接在。共享项是否开播只有对端知道，
/// 只能交给上传邀请去拒。
fn peer_ready(state: &AppState, peer_id: &str) -> bool {
    state.is_online()
        && state.get_peer(peer_id).is_some_and(|peer| peer.paired)
        && state.ws.has_connection(peer_id)
}

fn key_of(entry: &OpenedCopy) -> String {
    format!("{}\0{}\0{}", entry.peer_id, entry.share_id, entry.path)
}

fn emit_remote_changed(
    app: &AppHandle,
    entry: &OpenedCopy,
    local_dirty: bool,
    error: Option<&str>,
) {
    let name = entry
        .path
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or(entry.path.as_str());
    let _ = app.emit(
        crate::ws::events::ui::WRITEBACK_AUTO,
        serde_json::json!({
            "peerId": entry.peer_id,
            "shareId": entry.share_id,
            "path": entry.path,
            "name": name,
            "state": "remote-changed",
            "localDirty": local_dirty,
            "error": error,
        }),
    );
}

fn emit(app: &AppHandle, entry: &OpenedCopy, state: &str, error: Option<&str>) {
    let name = entry
        .path
        .rsplit('/')
        .next()
        .filter(|name| !name.is_empty())
        .unwrap_or(entry.path.as_str());
    let _ = app.emit(
        crate::ws::events::ui::WRITEBACK_AUTO,
        serde_json::json!({
            "peerId": entry.peer_id,
            "shareId": entry.share_id,
            "path": entry.path,
            "name": name,
            "state": state,
            "error": error,
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upload_gate_needs_quiet_time_backoff_and_a_live_peer() {
        let now = Instant::now();
        let quiet_done = now - QUIET - Duration::from_millis(1);
        assert!(should_upload(now, false, false, quiet_done, now, true));

        assert!(
            !should_upload(now, false, false, now, now, true),
            "刚变过 / 还在安静窗口内不能发"
        );
        assert!(
            !should_upload(now, true, false, quiet_done, now, true),
            "冲突阻断后不自动重试"
        );
        assert!(
            !should_upload(now, false, true, quiet_done, now, true),
            "同项已有写在跑"
        );
        assert!(
            !should_upload(now, false, false, quiet_done, now, false),
            "对端不在线/没长连接时先等"
        );
        assert!(
            !should_upload(
                now,
                false,
                false,
                now - Duration::from_secs(10),
                now + Duration::from_secs(1),
                true
            ),
            "退避未到不能发"
        );
    }

    #[test]
    fn remote_revision_comparison_needs_a_known_remote() {
        assert!(!remote_differs(&None, &None));
        assert!(
            remote_differs(&Some("a".into()), &None),
            "现在问到了 rev、本地基线没有：按可能已变化处理"
        );
        assert!(!remote_differs(&None, &Some("a".into())));
        assert!(remote_differs(&Some("a".into()), &Some("b".into())));
        assert!(!remote_differs(&Some("a".into()), &Some("a".into())));
    }

    #[test]
    fn share_invalidation_is_consumed_once() {
        note_share_changed("share-x");
        note_share_changed("share-x");
        note_share_changed("share-y");
        let got = take_invalidated();
        assert!(got.contains("share-x"));
        assert!(got.contains("share-y"));
    }
}