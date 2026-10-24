//! 等对方点头期间盯着"这次推的源"：源一变（增 / 删 / 改 / 改名），这批推送当场作废。
//!
//! 为什么要有它：邀请改成"一直等"之后（见 `push_job::CAP_AWAIT`），悬挂的邀请不再自己过期，
//! **得有人替发送方把话收回**。用户点了「取消」是一种；另一种是"这次要推的东西已经不是
//! 原来那份了" —— 清单里的 `size` / `mtime` 当场作废，对面就算点头，接下来也只会收到
//! 一份跟邀请对不上的东西（旧实现里这要等接收端逐文件对账才炸出来）。
//!
//! 只盯**等对方点头**那个窗口（`TargetState::Invited`）：一旦对方点头（开始传）就卸掉监视，
//! 那时候改源是另一回事（接收端按清单 `size`/`mtime` 对账，见 `stream::resume_key`）。

use std::path::PathBuf;
use std::sync::Arc;

use notify::{EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::state::AppState;

/// 共享监视注册表（`state.watchers`）里的键：共享项用裸 `share_id`，推送批次用 `push:<id>`。
fn key(batch_id: &str) -> String {
    format!("push:{batch_id}")
}

/// 开始盯这批推送的源。`roots` = 用户这次选的那几项，**原样**（清单里那份绝对路径是
/// 解析过的样子，而"用户点的是哪一项"才是他心里的那个东西）。
///
/// 一个源都挂不上（拔掉的盘、权限不对）就不注册：宁可不做这件事，也不要留一个空监视器
/// 让后面的人以为有人在盯着。
pub fn watch_sources(state: &Arc<AppState>, batch_id: &str, roots: &[PathBuf]) {
    let (tx, mut rx) = mpsc::unbounded_channel::<notify::Event>();
    let callback = move |res: notify::Result<notify::Event>| {
        if let Ok(event) = res {
            let _ = tx.send(event); // 无界通道，绝不阻塞平台那边的回调
        }
    };
    let mut watcher = match RecommendedWatcher::new(callback, notify::Config::default()) {
        Ok(w) => w,
        Err(e) => {
            warn!(batch = %batch_id, "源变化监视建不起来：{e}");
            return;
        }
    };
    let mut watched = 0usize;
    for root in roots {
        // 目录递归盯；文件只盯它自己（`notify` 对文件路径就是"只报这个文件的事件"，
        // 见 `Watcher::watch` 的文档）—— 推单个文件时不会因为同目录别的文件动就误判。
        let mode = if root.is_dir() {
            RecursiveMode::Recursive
        } else {
            RecursiveMode::NonRecursive
        };
        match watcher.watch(root, mode) {
            Ok(()) => watched += 1,
            Err(e) => warn!(batch = %batch_id, path = %root.display(), "源变化监视挂不上：{e}"),
        }
    }
    if watched == 0 {
        warn!(batch = %batch_id, "一个源都盯不上，这批不设源变化取消");
        return;
    }
    state.watchers.lock().insert(key(batch_id), watcher);

    let state = state.clone();
    let owned = batch_id.to_string();
    crate::task::spawn("push source watch", async move {
        // 第一件"真变化"就是终局：一次推送的源变了，没有"改回来"这一说（清单已经对不上）。
        while let Some(event) = rx.recv().await {
            if !is_change(&event.kind) {
                continue;
            }
            debug!(batch = %owned, kind = ?event.kind, paths = ?event.paths, "推送的源变了");
            // 先卸监视（丢监视器 → 这个通道关闭 → 本循环下一拍自然收工）。
            unwatch(&state, &owned);
            if !state.push.batch_has_invited(&owned) {
                // 已经没人等着点头了（都点了 / 都落了终态）：这场变化跟本批无关。
                return;
            }
            if let Err(e) = crate::transfer::cancel_push_batch_with(
                &state,
                &owned,
                "源文件已变化，本机已取消",
                "源文件已变化，对方已取消",
            ) {
                debug!(batch = %owned, %e, "源变化时这批已经收尾了");
            }
            return;
        }
    });
}

/// 卸掉这批的监视（结算、取消、或者源变化本身）。监视器一丢，上面那个循环的通道就关了。
pub fn unwatch(state: &AppState, batch_id: &str) {
    state.watchers.lock().remove(&key(batch_id));
}

/// 哪些 `notify` 事件算"源变了"。
///
/// **读不算**（`Access`）：别的后端会给每次读报一条事件，而"读一下"不是变化 ——
/// 接收端正在拉数据、杀软顺手扫一眼都会读它。创建 / 写入 / 删除 / 改名一律算。
fn is_change(kind: &EventKind) -> bool {
    !matches!(kind, EventKind::Access(_))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transfer::push_job::{BatchTarget, PushBatch, PushJob, TargetState};
    use std::time::{Duration, Instant};

    fn temp_dir(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("orbdrop_watch_{tag}_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn state() -> Arc<AppState> {
        let dir = temp_dir("state");
        Arc::new(AppState::new(
            crate::config::Config::default(),
            dir.join("config.toml"),
        ))
    }

    /// 等点头期间**源真被改了一下** → 这批当场作废（卡片落「已取消」，原因写清是源变了）。
    ///
    /// 这是"我这边取消"的自动那一半：真的挂上 `notify`、真的写一次文件，走完整条链。
    #[tokio::test]
    async fn a_changed_source_cancels_the_waiting_push() {
        let state = state();
        let src = temp_dir("src");
        std::fs::write(src.join("a.txt"), b"one").unwrap();

        state.push.insert_batch(PushBatch {
            id: "b1".into(),
            name: "src".into(),
            count: 1,
            skipped: 0,
            per_target_total: 3,
            targets: vec![BatchTarget {
                peer_id: "p1".into(),
                peer_name: "p1".into(),
                invite_id: "i1".into(),
                paired: false,
                state: TargetState::Invited,
                received: 0,
                error: None,
            }],
            last_emit: Instant::now(),
        });
        state.push.insert_job(PushJob::new(
            "i1".into(),
            "b1".into(),
            "p1".into(),
            "p1".into(),
            "src".into(),
            true,
            std::collections::HashMap::new(),
            false,
        ));

        watch_sources(&state, "b1", &[src.clone()]);
        std::fs::write(src.join("a.txt"), b"two").unwrap();

        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let target = state.push.target_of_invite("i1").unwrap();
            if target.state.is_terminal() {
                assert_eq!(target.state, TargetState::Cancelled, "源变了 → 整批作废");
                assert_eq!(target.error.as_deref(), Some("源文件已变化，本机已取消"));
                break;
            }
            assert!(
                Instant::now() < deadline,
                "源变了之后这批该当场作废，实际还停在等待"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }

        assert!(
            !state.watchers.lock().contains_key(&key("b1")),
            "作废之后监视要卸掉，别留一个没人要的监视器"
        );
        std::fs::remove_dir_all(&src).unwrap();
    }

    /// 已经没人等点头（对方早就同意了）时，源变化**不**该把正在传的那批撤掉 ——
    /// 那时候"传的东西变了"由接收端按清单 `size`/`mtime` 对账，不是这里的事。
    #[tokio::test]
    async fn a_change_after_consent_leaves_the_batch_alone() {
        let state = state();
        let src = temp_dir("post");
        std::fs::write(src.join("a.txt"), b"one").unwrap();
        state.push.insert_batch(PushBatch {
            id: "b2".into(),
            name: "src".into(),
            count: 1,
            skipped: 0,
            per_target_total: 3,
            targets: vec![BatchTarget {
                peer_id: "p1".into(),
                peer_name: "p1".into(),
                invite_id: "i2".into(),
                paired: false,
                state: TargetState::Sending,
                received: 0,
                error: None,
            }],
            last_emit: Instant::now(),
        });
        assert!(!state.push.batch_has_invited("b2"), "没人等点头");
        watch_sources(&state, "b2", &[src.clone()]);
        std::fs::write(src.join("a.txt"), b"two").unwrap();
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(
            state.push.target_of_invite("i2").unwrap().state,
            TargetState::Sending,
            "同意之后改源不该撤销这批"
        );
        std::fs::remove_dir_all(&src).unwrap();
    }
}
