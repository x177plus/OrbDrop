use std::sync::Arc;

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;
use tracing::warn;

use crate::state::AppState;
use crate::ws;
use crate::ws::events::WsEvent;

/// A debounced change event from the file watcher.
#[derive(Debug, Clone)]
pub struct ChangeEvent {
    pub share_id: String,
    pub path: String,
    pub kind: String,
}

/// Start watching a directory for changes (called when a share is added).
/// The watcher is registered in AppState.watchers keyed by share_id.
pub fn watch_share(state: &Arc<AppState>, share_id: &str, path: std::path::PathBuf) {
    let state_clone = state.clone();
    let sid = share_id.to_string();
    let base = path.clone();

    let (tx, mut rx) = mpsc::unbounded_channel::<notify::Event>();

    // Callback: push raw notify events into the mpsc channel.
    let callback = move |res: notify::Result<notify::Event>| {
        if let Ok(event) = res {
            let _ = tx.send(event); // best-effort; unbounded so never blocks
        }
    };

    let mut watcher = match RecommendedWatcher::new(callback, notify::Config::default()) {
        Ok(w) => w,
        Err(e) => {
            warn!(%sid, "failed to create watcher: {e}");
            return;
        }
    };

    if let Err(e) = watcher.watch(&base, RecursiveMode::Recursive) {
        warn!(%sid, "failed to watch {base:?}: {e}");
        return;
    }

    state.watchers.lock().insert(sid.clone(), watcher);

    // Background task: read from rx, debounce, and dispatch WsEvent::ShareChanged.
    // We use a simple 300ms debounce: collect events for 300ms, then dispatch unique.
    tokio::spawn(async move {
        use tokio::time::{sleep, Duration};
        let mut pending: Vec<ChangeEvent> = Vec::new();

        loop {
            tokio::select! {
                Some(event) = rx.recv() => {
                    let kind = match event.kind {
                        EventKind::Create(_) => "created",
                        EventKind::Modify(_) => "modified",
                        EventKind::Remove(_) => "deleted",
                        _ => continue,
                    }.to_string();

                    for path in event.paths {
                        let rel = match pathdiff::diff_paths(&path, &base) {
                            Some(r) => r.to_string_lossy().replace('\\', "/"),
                            None => continue,
                        };
                        pending.push(ChangeEvent {
                            share_id: sid.clone(),
                            path: rel,
                            kind: kind.clone(),
                        });
                    }
                }
                _ = sleep(Duration::from_millis(300)) => {
                    if pending.is_empty() {
                        continue;
                    }
                    // Dedupe by (share_id, path, kind) within this batch.
                    let mut seen = std::collections::HashSet::new();
                    let batch: Vec<ChangeEvent> = pending.drain(..)
                        .filter(|e| seen.insert((e.share_id.clone(), e.path.clone(), e.kind.clone())))
                        .collect();
                    drop(seen);

                    // 广播对象 = 此刻处于长连接的对端，与 /shares 的可见性判定同一套规则
                    // （开播开关 + 当前连接），因此对端连接/断开时变更通知范围立刻跟着变。
                    let broadcast_enabled: Vec<bool> = {
                        let config = state_clone.config.lock();
                        batch
                            .iter()
                            .map(|ev| {
                                config
                                    .shares
                                    .iter()
                                    .find(|s| s.id == ev.share_id)
                                    .map(|s| s.broadcast_enabled)
                                    .unwrap_or(false)
                            })
                            .collect()
                    };
                    let peers = state_clone.ws.connected_peers();
                    for (ev, enabled) in batch.iter().zip(broadcast_enabled) {
                        if !enabled {
                            continue;
                        }
                        for target in &peers {
                            ws::send_to_peer(&state_clone, target, WsEvent::ShareChanged {
                                share_id: ev.share_id.clone(),
                                path: ev.path.clone(),
                                kind: ev.kind.clone(),
                            });
                        }
                    }
                }
            }
        }
    });
}

/// Stop watching a share (called when a share is removed).
pub fn unwatch_share(state: &Arc<AppState>, share_id: &str) {
    state.watchers.lock().remove(share_id);
}

/// Watch all currently active shares (called at startup).
pub fn watch_all_shares(state: &Arc<AppState>) {
    let shares: Vec<(String, std::path::PathBuf)> = {
        let config = state.config.lock();
        config.shares.iter().map(|s| (s.id.clone(), s.path.clone())).collect()
    };
    for (id, path) in shares {
        watch_share(state, &id, path);
    }
}