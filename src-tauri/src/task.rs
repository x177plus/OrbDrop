//! 后台任务启动的唯一收口。
//!
//! 两种上下文都必须起得来：
//!
//! 1. **已经在 reactor 里的**（Tauri 的 `async fn` 命令、HTTP/WS 请求处理、后台任务自己）
//!    → 用当前运行时；
//! 2. **没有 reactor 的**（Tauri 的同步命令跑在主线程上）→ `tokio::spawn` 会 panic 掉整个
//!    进程（用户侧："点一下就闪退"），而"没有运行时就跳过"会让这次点击**静默失效**
//!    （用户侧："点了没反应 / 进度一直是 0%"，日志里只留一行
//!    `no tokio runtime here; task skipped` —— 推送接收就踩过这个坑：点「同意」后下载任务
//!    被丢掉，两边都停在 0%）。所以退回 Tauri 的全局运行时：它随用随建，在 App 里和在纯库
//!    测试里都成立。
//!
//! 结论：命令尽量写成 `async fn`（那是最自然的上下文），而这个收口保证"就算写成了同步命令，
//! 后台任务照样起得来" —— **不允许一次点击被无声吞掉**。

use std::future::Future;

use tauri::async_runtime::RuntimeHandle;
use tokio::task::JoinHandle;

/// 当前运行时的句柄；没有就退回 Tauri 的全局运行时。
///
/// 两条路都必须成立：已经在 reactor 里的直接用当前运行时；Tauri 的同步命令跑在主线程上，
/// 那里没有 reactor（`tokio::spawn` 会 panic 掉整个进程，用户侧"点一下就闪退"）。
pub fn handle() -> tokio::runtime::Handle {
    if let Ok(rt) = tokio::runtime::Handle::try_current() {
        return rt;
    }
    let RuntimeHandle::Tokio(handle) = tauri::async_runtime::handle();
    tracing::debug!("no current reactor; using tauri's global runtime");
    handle
}

/// 在**任意线程**上同步等一个异步结果：把活派回运行时，用 channel 等回来。
///
/// 给那些没有运行时的线程用（平台的回调线程、COM 的 RPC 线程）。不能用 `Handle::block_on`：
/// 那些线程有可能正是运行时自己的 worker，`block_on` 会直接卡死。
pub fn run_blocking<T: Send + 'static>(fut: impl Future<Output = T> + Send + 'static) -> Option<T> {
    run_blocking_on(&handle(), fut)
}

/// 同上，但把运行时显式传进来。
///
/// 回调线程要用的必须是**应用自己的那个运行时**（连接建立时抓下来的），而不是"当时恰好
/// 能问到的" —— 测试进程里两者不是同一个，混用会让本该跑在应用运行时上的活跑到别处去。
pub fn run_blocking_on<T: Send + 'static>(
    runtime: &tokio::runtime::Handle,
    fut: impl Future<Output = T> + Send + 'static,
) -> Option<T> {
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    runtime.spawn(async move {
        let _ = tx.send(fut.await);
    });
    rx.recv().ok()
}

/// 启动 `fut` 并返回句柄（调用方可以拿它 abort）。
///
/// 没有"起不来"这个结果：要么在当前运行时上，要么在 Tauri 的全局运行时上。
pub fn spawn<F>(what: &str, fut: F) -> JoinHandle<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    if tokio::runtime::Handle::try_current().is_err() {
        tracing::debug!(
            task = what,
            "no current reactor; spawning on tauri's global runtime"
        );
    }
    handle().spawn(fut)
}
