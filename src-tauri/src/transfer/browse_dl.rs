//! 传输暂停闸：**「浏览」里的手动下载**与会话里**正在接收的推送**共用这一套。
//!
//! 两条下载都是"一条活到落盘才收尾"的任务：暂停不能靠取消 —— 那会把已经写好的
//! `.orbdrop-part` 丢掉，继续时又从 0 开始。这里只放一个闸门：任务在每一块数据之前调
//! [`DownloadControl::wait_if_paused`]，暂停时停在原地等，界面点继续就把闸门放开。字节与目标
//! 文件都不动，恢复后从当前 offset 接着要。
//!
//! 两张表都挂在 `AppState` 上：
//! - `browse_downloads`（键 = `peerId\0shareId\0path`，与 `browse://download` 事件同一套坐标，
//!   见 [`key`]）；
//! - `receive_downloads`（键 = `inviteId`；接收任务退出时由 `push_job::ReceiveGate` 摘掉）。
//!
//! 暂停条目不落盘 —— 进程重启后那些任务本来就不存在了。
//!
//! 用 `watch` 而不是 `Notify` + 一个 `AtomicBool`：后者的经典写法在"先建等待、后复查、
//! 中途被唤醒"这三步之间有丢唤醒的窗口（`notify_waiters` 只唤醒当时已经登记的等待者），
//! 表现就是点了继续却永远停在那里。`watch::Receiver::wait_for` 天生带当前值检查，没这个洞。

use tokio::sync::watch;

/// 一次浏览下载的暂停闸门。
pub struct DownloadControl {
    paused: watch::Sender<bool>,
}

impl Default for DownloadControl {
    fn default() -> Self {
        // 初始必须是「未暂停」：命令一登记就开始下。
        let (paused, _) = watch::channel(false);
        Self { paused }
    }
}

impl DownloadControl {
    /// 界面点圆环：`true` = 暂停，`false` = 继续。幂等，重复点同一档没有副作用。
    pub fn set_paused(&self, paused: bool) {
        // 接收方只在 `wait_if_paused` 里临时订阅；两次检查之间**没有订阅者**是常态。
        // `send` 在没有接收者时会失败且不更新值（点暂停就丢），`send_replace` 总是写入。
        self.paused.send_replace(paused);
    }

    /// 此刻是不是暂停着。
    ///
    /// 给**已经在流里**的循环用（老协议的逐块读取）：`wait_if_paused` 每块订阅一次太浪费，
    /// 先读一眼当前值，只有在暂停时才走那条等待路。
    pub fn is_paused(&self) -> bool {
        *self.paused.borrow()
    }

    /// 暂停时一直等，直到 `set_paused(false)`。
    pub async fn wait_if_paused(&self) {
        let mut rx = self.paused.subscribe();
        // 当前值是 false 就当场返回；是 true 才等下一拍（发送端被丢弃也不会把命令
        // 永久卡在这里：`Err` 只表示表被回收，按"不再暂停"处理）。
        let _ = rx.wait_for(|paused| !*paused).await;
    }
}

/// 下载坐标 → 控制表键。`path` 是共享项内相对路径（共享项根是文件时为空串）。
pub fn key(peer_id: &str, share_id: &str, path: &str) -> String {
    format!("{peer_id}\u{0}{share_id}\u{0}{path}")
}
