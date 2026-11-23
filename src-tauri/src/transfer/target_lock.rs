//! 同一落地路径的**跨邀请写锁**。
//!
//! 接收推送有两条数据面（新协议 `.part` + 位图、老协议最终路径直写），WebDAV 的 PUT 也会
//! 写同一个共享项里的文件。两条邀请/请求同时落同一个目标时，各自写自己的临时文件/续传
//! 记录，最后互相 `rename` 覆盖，可能交出一份“长度对、内容坏”的文件。这里只做一件事：
//! **同一目标路径在同一时刻只允许一个写者**；拿不到就明确失败/稍后重试，绝不并发写。
//!
//! 锁是进程内的（接收/DAV 都在同一个进程里），不落盘；进程退出即释放。

use std::collections::HashSet;
use std::path::Path;
use std::sync::LazyLock;

use parking_lot::Mutex;

static TARGETS: LazyLock<Mutex<HashSet<String>>> = LazyLock::new(|| Mutex::new(HashSet::new()));

/// 一个目标路径的租约；`Drop` 时自动释放。
pub(crate) struct TargetLease {
    key: String,
}

impl TargetLease {
    /// 拿到路径的写租约；已经有人在写同一路径时返回 `None`。
    pub(crate) fn try_acquire(path: &Path) -> Option<Self> {
        let key = normalize(path);
        let mut set = TARGETS.lock();
        if !set.insert(key.clone()) {
            return None;
        }
        Some(Self { key })
    }
}

impl Drop for TargetLease {
    fn drop(&mut self) {
        TARGETS.lock().remove(&self.key);
    }
}

fn normalize(path: &Path) -> String {
    let text = path.to_string_lossy();
    // `\\?\C:\...` 与 `C:\...` 是同一个文件；WebDAV/推送两边可能一个带前缀一个不带。
    let text = text.strip_prefix(r"\\?\").unwrap_or(&text);
    if cfg!(windows) {
        text.to_lowercase()
    } else {
        text.to_string()
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn only_one_writer_per_target_at_a_time() {
        let path = Path::new(r"C:\tmp\orbdrop-target-lock-test.bin");
        let first = TargetLease::try_acquire(path).expect("第一个写者");
        assert!(
            TargetLease::try_acquire(path).is_none(),
            "同一路径的第二个写者必须被拒"
        );
        drop(first);
        assert!(
            TargetLease::try_acquire(path).is_some(),
            "第一个写者释放后，后来的可以拿"
        );
    }

    #[test]
    fn different_targets_do_not_block_each_other() {
        let a = TargetLease::try_acquire(Path::new("a.bin")).unwrap();
        let b = TargetLease::try_acquire(Path::new("b.bin")).expect("不同路径互不阻塞");
        drop((a, b));
    }
}
