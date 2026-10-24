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

    let (tx, rx) = mpsc::unbounded_channel::<notify::Event>();

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

    // 记事件 → 攒批 → 安静窗口到点统一发（见 `debounce_loop`）。
    let broadcaster = state_clone.clone();
    tokio::spawn(async move {
        let sid_for_log = sid.clone();
        let sid_for_broadcast = sid.clone();
        debounce_loop(rx, sid, base, QUIET_WINDOW, move |batch| {
            broadcast(&broadcaster, &sid_for_broadcast, batch)
        })
        .await;
        tracing::debug!(share = %sid_for_log, "变更监听循环退出");
    });
}

/// 一批变更攒多久：从**第一个**事件起算的固定窗口（收到更多事件不往后推）。
///
/// 300ms 是"远端刷新看着实时"与"一次复制不刷屏"的折中：大目录复制期间每 300ms 发一批，
/// 对端一直在跟着刷新，而不是等复制结束才跳一下。
const QUIET_WINDOW: std::time::Duration = std::time::Duration::from_millis(300);

/// 一批最多攒这么多条（超了丢**最老的**）：窗口内的去重/合并只需要留下"最近的样子"，
/// 没有上限的话，几万个文件的一次复制会在内存里堆几万条（旧实现正是无界的）。
const MAX_PENDING: usize = 4096;

/// 变更监听的防抖循环。
///
/// 抽成独立函数是为了能被测试直接驱动（`watch_share` 只负责建监视器、把它 spawn 出去）。
///
/// 两个**必须守住**的点，旧实现两个都错了：
/// ① 窗口**只在没有待发批次时**起算 —— 旧写法每轮 `select!` 都新建一个 300ms 的 `sleep`，
///    任何事件都会把它取消，等于"事件一来就把计时归零"：持续变更期间远端一直收不到刷新；
/// ② 事件通道关闭（`unwatch_share` 丢掉了监视器）时**必须退出** —— 旧写法继续转，
///    `Some(event)` 不再匹配、sleep 分支空转，每 add/remove 一次共享项就泄漏一个每秒醒 4 次
///    的任务（对 N-04「CPU 空闲近 0」）。
async fn debounce_loop(
    mut rx: mpsc::UnboundedReceiver<notify::Event>,
    sid: String,
    base: std::path::PathBuf,
    quiet: std::time::Duration,
    mut dispatch: impl FnMut(Vec<ChangeEvent>),
) {
    use tokio::time::{sleep_until, Instant};

    let mut pending: Vec<ChangeEvent> = Vec::new();
    let mut deadline: Option<Instant> = None;

    loop {
        let Some(at) = deadline else {
            // 没有待发批次：只等事件（等价于旧实现里那个"永远不完成的 sleep 分支"）。
            let Some(event) = rx.recv().await else { return };
            collect_events(&mut pending, &sid, &base, event);
            deadline = Some(Instant::now() + quiet);
            continue;
        };
        tokio::select! {
            maybe = rx.recv() => {
                let Some(event) = maybe else {
                    // 监视器没了（这个共享项被下架/进程收尾）：这一批不用再发，直接收工。
                    return;
                };
                collect_events(&mut pending, &sid, &base, event);
                // 窗口**不动**：它是"第一批什么时候发"的期限，不是"最后一个事件之后"。
            }
            _ = sleep_until(at) => {
                let batch = std::mem::take(&mut pending);
                deadline = None;
                if !batch.is_empty() {
                    dispatch(batch);
                }
            }
        }
    }
}

/// 把一条 `notify` 事件摊成 `(相对路径, kind)` 塞进待发批次（同一批内按 `(路径, kind)` 去重）。
fn collect_events(
    pending: &mut Vec<ChangeEvent>,
    sid: &str,
    base: &std::path::Path,
    event: notify::Event,
) {
    let Some(kind) = change_kind(&event.kind) else {
        return;
    };
    for path in event.paths {
        let Some(rel) = pathdiff::diff_paths(&path, base) else {
            continue;
        };
        let ev = ChangeEvent {
            share_id: sid.to_string(),
            path: rel.to_string_lossy().replace('\\', "/"),
            kind: kind.to_string(),
        };
        if pending
            .iter()
            .any(|p| p.path == ev.path && p.kind == ev.kind)
        {
            continue; // 同一批里重复的（一次复制会疯狂重复报同一个文件）
        }
        pending.push(ev);
    }
    if pending.len() > MAX_PENDING {
        let drop = pending.len() - MAX_PENDING;
        pending.drain(..drop); // 丢最老的：留下的是"最近的样子"
    }
}

/// `notify` 的事件类型 → 我们对外的三档（其余一律忽略）。
fn change_kind(kind: &EventKind) -> Option<&'static str> {
    match kind {
        EventKind::Create(_) => Some("created"),
        EventKind::Modify(_) => Some("modified"),
        EventKind::Remove(_) => Some("deleted"),
        _ => None,
    }
}

/// 把一批变更发给**此刻处于长连接**的对端。
///
/// 一批是"同一个共享项、同一个 300ms 窗口"，发出去的是**一条**（窗口内的"最近的样子"，
/// 理由见下面那段注 —— 逐条发会把对端与本机一起压住）。
///
/// 与 `/shares` 的可见性判定同一套规则（开播开关 + 当前连接），因此对端连接/断开时
/// 变更通知范围立刻跟着变。
fn broadcast(state: &Arc<AppState>, sid: &str, batch: Vec<ChangeEvent>) {
    let enabled = {
        let config = state.config.lock();
        config
            .shares
            .iter()
            .find(|s| s.id == sid)
            .map(|s| s.broadcast_enabled)
            .unwrap_or(false)
    };
    if !enabled {
        return;
    }
    for target in state.ws.connected_peers() {
        // **一批只发一条**：一批 = 同一个共享项、同一个 300ms 窗口（见 `QUIET_WINDOW`）内的变更，
        // 而窗口里可能有几百条 —— 实测一台机器在共享目录里跑多通道 Gradle 构建时，本机这边
        // 峰值收到 1300 条/秒（对端逐条发、本机逐条记 + 逐条刷界面）。逐条发等于把本机的文件
        // 变动量原样压到网络上，而接收端把它们当的是**同一件事**：它只认 `share_id`
        // （`PeerTab` 的 `share://changed` 处理只读它，前端收到就去重拉那一层）。
        //
        // 留**最后一条**：与 `MAX_PENDING` 超出时丢最老的那套口径一致 —— 报的仍是"最近的样子"。
        // 旧接收端收到的是更粗的通知，但"该刷新的那一下"一条都没少（它本来也是重拉整层）。
        let Some(ev) = batch.last() else { continue };
        ws::send_to_peer(
            state,
            &target,
            WsEvent::ShareChanged {
                share_id: ev.share_id.clone(),
                path: ev.path.clone(),
                kind: ev.kind.clone(),
            },
        );
    }
}

/// Stop watching a share (called when a share is removed).
pub fn unwatch_share(state: &Arc<AppState>, share_id: &str) {
    state.watchers.lock().remove(share_id);
}

/// Watch all currently active shares (called at startup).
pub fn watch_all_shares(state: &Arc<AppState>) {
    let shares: Vec<(String, std::path::PathBuf)> = {
        let config = state.config.lock();
        config
            .shares
            .iter()
            .map(|s| (s.id.clone(), s.path.clone()))
            .collect()
    };
    for (id, path) in shares {
        watch_share(state, &id, path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use notify::event::{CreateKind, ModifyKind};
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    fn ev(kind: EventKind, path: &str) -> notify::Event {
        notify::Event {
            kind,
            paths: vec![PathBuf::from(path)],
            attrs: Default::default(),
        }
    }

    fn seen(batches: &Arc<Mutex<Vec<Vec<ChangeEvent>>>>) -> Vec<(String, String)> {
        batches
            .lock()
            .unwrap()
            .iter()
            .flat_map(|b| {
                b.iter()
                    .map(|e| (e.path.clone(), e.kind.clone()))
                    .collect::<Vec<_>>()
            })
            .collect()
    }

    /// 一批变更只发**一条**帧（`broadcast`）：对端一次构建能在 300ms 窗口里攒几百条，
    /// 逐条发就是实测的 1300 帧/秒 —— 而接收端只认 `share_id`，那几百条对它是一件事。
    /// 报的是窗口内**最后一条**（"最近的样子"，与 `MAX_PENDING` 丢最老同一套口径）。
    #[tokio::test]
    async fn a_burst_goes_out_as_a_single_frame() {
        let dir = std::env::temp_dir().join(format!("orbdrop_burst_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut share = crate::config::ShareItem::new(dir.clone());
        share.broadcast_enabled = true;
        let sid = share.id.clone();
        let mut config = crate::config::test_config();
        config.shares.push(share);
        let state = Arc::new(AppState::new(config, dir.join("config.toml")));
        let (tx, mut rx) = mpsc::channel(64);
        state.ws.register("peer-a", tx);

        let batch: Vec<ChangeEvent> = (0..200)
            .map(|i| ChangeEvent {
                share_id: sid.clone(),
                path: format!("build/out-{i}.class"),
                kind: "modified".into(),
            })
            .collect();
        broadcast(&state, &sid, batch);

        match rx.try_recv().expect("这一批该发出去") {
            WsEvent::ShareChanged { share_id, path, .. } => {
                assert_eq!(share_id, sid);
                assert_eq!(path, "build/out-199.class", "报的该是窗口内最后一条");
            }
            other => panic!("期望 ShareChanged，得到 {}", other.type_name()),
        }
        assert!(
            rx.try_recv().is_err(),
            "一批只该发一条 —— 多出来的就是逐文件那条老路"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 一个安静窗口 = 一批；同一批里重复的（一次复制会疯狂重报同一个文件）合并。
    #[tokio::test]
    async fn a_window_batches_and_dedupes() {
        let (tx, rx) = mpsc::unbounded_channel();
        let batches: Arc<Mutex<Vec<Vec<ChangeEvent>>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = batches.clone();
        let handle = tokio::spawn(debounce_loop(
            rx,
            "share-1".into(),
            PathBuf::from("C:/root"),
            Duration::from_millis(30),
            move |batch| sink.lock().unwrap().push(batch),
        ));

        tx.send(ev(EventKind::Create(CreateKind::File), "C:/root/a.txt"))
            .unwrap();
        tx.send(ev(EventKind::Create(CreateKind::File), "C:/root/a.txt"))
            .unwrap();
        tx.send(ev(EventKind::Modify(ModifyKind::Any), "C:/root/a.txt"))
            .unwrap();
        tx.send(ev(EventKind::Create(CreateKind::File), "C:/root/b.txt"))
            .unwrap();
        // 顺带验证"不是文件系统变更"的那些被忽略。
        tx.send(ev(
            EventKind::Access(notify::event::AccessKind::Read),
            "C:/root/a.txt",
        ))
        .unwrap();

        tokio::time::sleep(Duration::from_millis(150)).await;
        assert_eq!(
            seen(&batches),
            vec![
                ("a.txt".to_string(), "created".to_string()),
                ("a.txt".to_string(), "modified".to_string()),
                ("b.txt".to_string(), "created".to_string()),
            ],
            "一批、去重、只要三档里的那几种"
        );

        // 通道关掉（= 共享项被下架）：循环**必须退出**。旧实现这里空转到天荒地老，
        // 每 add/remove 一次共享项就漏一个每秒醒 4 次的任务。
        drop(tx);
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("监视器被丢掉之后循环必须收工")
            .expect("循环不该 panic");
    }

    /// 持续变更（大目录复制）期间，批次要**按期发**，不能被后续事件一直往后推。
    #[tokio::test]
    async fn a_long_burst_still_emits_batches() {
        let (tx, rx) = mpsc::unbounded_channel();
        let batches: Arc<Mutex<Vec<Vec<ChangeEvent>>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = batches.clone();
        let handle = tokio::spawn(debounce_loop(
            rx,
            "share-2".into(),
            PathBuf::from("C:/root"),
            Duration::from_millis(30),
            move |batch| sink.lock().unwrap().push(batch),
        ));

        // 每 10ms 一条、持续 150ms（比窗口长得多）：旧实现每来一条就把计时归零，
        // 这 150ms 里一批都发不出来。
        for i in 0..15 {
            tx.send(ev(
                EventKind::Create(CreateKind::File),
                &format!("C:/root/f{i}.txt"),
            ))
            .unwrap();
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let count = batches.lock().unwrap().len();
        assert!(
            count >= 2,
            "持续变更期间应当按期发批（实际发了 {count} 批）"
        );
        drop(tx);
        tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .expect("循环要能收工")
            .unwrap();
    }

    /// 窗口内堆太多时丢最老的（留下"最近的样子"），不无界增长。
    #[test]
    fn pending_is_capped_from_the_old_end() {
        let mut pending: Vec<ChangeEvent> = Vec::new();
        for i in 0..(MAX_PENDING + 10) {
            collect_events(
                &mut pending,
                "share-3",
                std::path::Path::new("C:/root"),
                ev(
                    EventKind::Create(CreateKind::File),
                    &format!("C:/root/f{i}.txt"),
                ),
            );
        }
        assert_eq!(pending.len(), MAX_PENDING);
        assert_eq!(pending[0].path, format!("f{}.txt", 10), "丢的是最老的那批");
    }
}
