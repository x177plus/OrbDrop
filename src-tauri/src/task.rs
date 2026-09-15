//! 后台任务启动的唯一收口。
//!
//! Tauri 的**同步**命令跑在主线程上，那里没有 Tokio reactor：直接 `tokio::spawn`
//! 会 panic 掉整个进程，用户侧看到的是"点一下 App 就闪退"。凡是可能由按钮点击
//! 触发的后台任务都从这里启动 —— 没有运行时就记一条错误、什么都不做（对应的自动
//! 重试会在有运行时的上下文里补上），绝不让一个点击把 App 打死。

use std::future::Future;

use tokio::task::JoinHandle;

/// 在当前的 Tokio 运行时上启动 `fut`；没有运行时则记录并返回 `None`。
pub fn spawn<F>(what: &str, fut: F) -> Option<JoinHandle<()>>
where
    F: Future<Output = ()> + Send + 'static,
{
    match tokio::runtime::Handle::try_current() {
        Ok(rt) => Some(rt.spawn(fut)),
        Err(_) => {
            tracing::error!(task = what, "no tokio runtime here; task skipped");
            None
        }
    }
}