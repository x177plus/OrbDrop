//! 进程级 fd 上限与占用指标（桌面端）。
//!
//! macOS 的 `RLIMIT_NOFILE` **软上限默认只有 256**（硬上限通常是 unlimited），而 OrbDrop
//! 同时持有：mDNS 的多网卡 UDP、主 HTTP 监听、长连接、5s 探活连接、以及最多 8 个中转端点
//! （每个端点一条监听 + 若干入站/出站连接）。在设备多、又替别人中转的那台机器上，实测
//! 进程已经用到 200+ 个 fd；一次中转/传输突发就会撞到 256，`accept()` 随即报
//! `Too many open files`（见 `docs/macos.md` §1.3）。
//!
//! 两件事：
//! 1. **启动时把软上限抬到硬上限**（封顶 [`FD_SOFT_CAP`]）。这是平台补丁，不改业务逻辑；
//! 2. 每 30s 采一次占用，到 80% 打一条 warning（5 分钟内不重复）—— 下一次不用再靠 `lsof` 猜。
//!
//! Windows 没有这种低句柄上限；本模块在非 Unix 上是空操作。

#[cfg(unix)]
use std::time::{Duration, Instant};

#[cfg(unix)]
use std::sync::atomic::{AtomicU64, Ordering};

#[cfg(unix)]
use tracing::{info, warn};

/// 软上限最多抬到这里。65535 对局域网应用足够，也避免把 `RLIM_INFINITY` 直接塞给
/// 某些会拒绝无限值的系统。硬上限比这更低时用硬上限。
#[cfg(unix)]
const FD_SOFT_CAP: u64 = 65535;

/// 占用告警阈值（百分比）与告警最短间隔。
#[cfg(unix)]
const FD_WARN_PERCENT: u64 = 80;
#[cfg(unix)]
const FD_WARN_EVERY: Duration = Duration::from_secs(300);
#[cfg(unix)]
const FD_SAMPLE_EVERY: Duration = Duration::from_secs(30);

/// 启动时调用一次：抬上限 + 起采样线程。
pub fn prepare_process() {
    raise_fd_limit();
    start_fd_watchdog();
}

#[cfg(unix)]
pub fn raise_fd_limit() {
    // SAFETY: `getrlimit`/`setrlimit` 只写调用方给的 `rlimit`，没有别名问题。
    unsafe {
        let mut lim = libc::rlimit {
            rlim_cur: 0,
            rlim_max: 0,
        };
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) != 0 {
            warn!("读取 RLIMIT_NOFILE 失败，跳过 fd 上限调整");
            return;
        }
        let hard = lim.rlim_max as u64;
        let target = hard.min(FD_SOFT_CAP);
        if target > lim.rlim_cur as u64 {
            let next = libc::rlimit {
                rlim_cur: target as libc::rlim_t,
                rlim_max: lim.rlim_max,
            };
            if libc::setrlimit(libc::RLIMIT_NOFILE, &next) != 0 {
                warn!(
                    old = lim.rlim_cur as u64,
                    wanted = target,
                    hard,
                    "抬高 RLIMIT_NOFILE 失败（保持原值）"
                );
                return;
            }
            lim = next;
        }
        info!(
            soft = lim.rlim_cur as u64,
            hard,
            "fd 上限已就绪（macOS 默认软上限只有 256，见 docs/macos.md §1.3）"
        );
    }
}

#[cfg(not(unix))]
pub fn raise_fd_limit() {}

/// 当前进程打开的 fd 数 / 软上限。Windows 返回 `None`（没有等价的低上限问题）。
#[cfg(unix)]
pub fn fd_usage() -> Option<(u64, u64)> {
    let dir = if cfg!(target_os = "macos") {
        "/dev/fd"
    } else {
        "/proc/self/fd"
    };
    let mut used = std::fs::read_dir(dir).ok()?.count() as u64;
    // 读目录本身也占一个 fd（有些系统把它列进目录项，有些不列）：扣掉它只影响 1 的精度。
    used = used.saturating_sub(1);
    // SAFETY: 同 `raise_fd_limit`。
    let mut lim = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    let soft = unsafe {
        if libc::getrlimit(libc::RLIMIT_NOFILE, &mut lim) != 0 {
            return None;
        }
        lim.rlim_cur as u64
    };
    Some((used, soft))
}

#[cfg(not(unix))]
pub fn fd_usage() -> Option<(u64, u64)> {
    None
}

#[cfg(unix)]
fn start_fd_watchdog() {
    // 用独立线程而不是 tokio 任务：这个采样在 tokio runtime 起来之前就该能跑，且不占运行时的
    // 调度配额；进程退出时线程随进程一起结束。
    static LAST_WARN_AT: AtomicU64 = AtomicU64::new(0);
    std::thread::Builder::new()
        .name("fd-watchdog".into())
        .spawn(move || {
            let started = Instant::now();
            loop {
                std::thread::sleep(FD_SAMPLE_EVERY);
                let Some((used, soft)) = fd_usage() else { continue };
                if soft == 0 {
                    continue;
                }
                let percent = used.saturating_mul(100) / soft;
                if percent < FD_WARN_PERCENT {
                    continue;
                }
                let now = started.elapsed().as_secs();
                let last = LAST_WARN_AT.load(Ordering::Relaxed);
                if last != 0 && now.saturating_sub(last) < FD_WARN_EVERY.as_secs() {
                    continue;
                }
                LAST_WARN_AT.store(now.max(1), Ordering::Relaxed);
                warn!(
                    used,
                    soft,
                    percent,
                    "fd 占用已达 {percent}%：检查探活死地址/中转连接是否堆积（docs/macos.md §1.3）"
                );
            }
        })
        .ok();
}

#[cfg(not(unix))]
fn start_fd_watchdog() {}

#[cfg(test)]
mod tests {
    #[test]
    fn fd_usage_is_sane_on_unix() {
        #[cfg(unix)]
        {
            let (used, soft) = super::fd_usage().expect("unix 上应能读到 fd 用量");
            assert!(used > 0, "进程至少占着 stdout/stderr");
            assert!(soft > 0);
        }
        #[cfg(not(unix))]
        assert!(super::fd_usage().is_none());
    }
}