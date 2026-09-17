use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use tracing_appender::non_blocking::WorkerGuard;
use tracing_subscriber::prelude::*;
use tracing_subscriber::{fmt, EnvFilter};
use tracing_appender::rolling;

use crate::config::data_dir;

/// 非阻塞文件写入器的 guard 必须活到进程结束（tracing-appender 的要求）：它负责把
/// 队列刷盘，绑成 `_guard` 这种立即丢弃的绑定等于提前结束这个职责。日志文件建了却
/// 长期没有内容时，这里是第一个要看的地方。
static LOG_GUARD: OnceLock<WorkerGuard> = OnceLock::new();

/// 启动链路上一旦 panic，必须留下痕迹：**同步**追加一行到 `logs/crash.log`。
///
/// 刻意不走 tracing：它的文件写入是异步刷盘的（队列 + 后台线程），abort 或异常退出
/// 时队列里的内容会直接丢掉 —— release 构建又没有控制台可看，用户侧就只剩
/// "启动一闪就没了、什么提示都没有"。
pub fn record_crash(path: &Path, message: &str) {
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{} {message}", chrono::Local::now().to_rfc3339());
    }
}

/// 崩溃记录文件的位置：数据目录不可写时退到系统临时目录，总之要留下东西。
fn crash_log_path() -> PathBuf {
    let dir = data_dir().join("logs");
    if std::fs::create_dir_all(&dir).is_ok() {
        dir.join("crash.log")
    } else {
        std::env::temp_dir().join("orbdrop-crash.log")
    }
}

fn install_panic_recorder() {
    let path = crash_log_path();
    std::panic::set_hook(Box::new(move |info| {
        let message = format!("panic: {info}");
        record_crash(&path, &message);
        eprintln!("OrbDrop {message}");
    }));
}

/// Initialize tracing with daily rolling file appender in the data dir /logs.
pub fn init_log() {
    install_panic_recorder();

    let log_dir = data_dir().join("logs");
    if let Err(e) = std::fs::create_dir_all(&log_dir) {
        eprintln!("Failed to create log directory {log_dir:?}: {e}");
        return;
    }

    let file_appender = rolling::daily(log_dir, "orbdrop.log");
    let (file_writer, guard) = tracing_appender::non_blocking(file_appender);
    let _ = LOG_GUARD.set(guard);

    tracing_subscriber::registry()
        .with(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with(fmt::Layer::new().with_writer(file_writer).json())
        .with(fmt::Layer::new().with_writer(std::io::stdout))
        .init();
}