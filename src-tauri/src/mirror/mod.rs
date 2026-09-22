//! CFAPI 镜像：拖出去的是**本地真实路径**，字节等到目标真的读它时才从对端拉。
//!
//! 为什么是这条路（与旧的"提前转存 / 虚拟文件"两条路的区别）：
//!
//! - **起拖零等待、零下载**：抓起时只在本地建一个**占位符**（实测 0.22 ms、0 字节磁盘），
//!   而不是把整份内容先下到暂存目录。1 GiB 的占位符在资源管理器里逻辑大小是 1 GiB、
//!   "磁盘上的大小"是 0 —— 占位符是元数据，不是文件。
//! - **等待发生在目标进程里**：目标第一次 `ReadFile` 时内核把那个读挂起，回调我们拉对端，
//!   我们 `CfExecute` 把字节交回去。我们自己的窗口与拖放循环早就结束了。
//! - **只对真正被消费的部分付钱**：用户抓起后取消、拖到空白处、目标只瞄一眼 —— 都不会白下载
//!   （旧的提前转存是无条件先下、而且留 6 小时）。
//! - **一条路通吃**：`CF_HDROP` 给的是真实路径，只认路径的应用（微信这类）不再需要单独照顾。
//!
//! 几个必须照做的取舍（都有实测或官方文档支撑，别随手改）：
//!
//! - **水合策略 `PROGRESSIVE`**：目标读到哪一段就交付哪一段，剩余内容在句柄还开着时后台继续拉。
//!   `FULL` 是"整份到齐才放行"（大文件就是长时间卡住），`ALWAYS_FULL` 更直接 —— 平台会拒绝
//!   `CfCreatePlaceholders`。代价：平台**不保证**回调从 offset 0 顺序到达，要能处理任意区间。
//! - **填充策略 `FULL` + 目录占位符不设 `DISABLE_ON_DEMAND_POPULATION`**：抓起只建**一个**
//!   顶层占位符，子树等目标枚举那个目录时再按目录拉（`FETCH_PLACEHOLDERS`，一次回调给整层并
//!   声明"这一层齐了"）。拖一个十万文件的目录也是秒起拖，且只对真被复制的部分付钱。
//!   实测：**平台不会为 provider 自己进程**的枚举请求按需填充（本进程 `read_dir` 只看到空的
//!   本地视图）；枚举者是资源管理器/目标应用时那条路才通 —— 也就是真实场景里的那一个。
//! - **退出清干净、启动从头建**：注册只在进程活着的时候有意义（我们不在时占位符读不了，
//!   实测 os error 362），所以退出是"断开连接 → 删镜像树 → 注销同步根"，启动是"注册 + 连接"。
//!   镜像里的一切都能从对端重建，留着只会留下"有注册、没 provider"的残骸。
//!   （注册本身很便宜：实测 1.53 ms；注销是 O(N)，靠"先删树"把它压回常量级。）
//! - **不注册 `NOTIFY_DELETE` / `NOTIFY_RENAME`**：镜像只读单向，没有要回写对端的状态。
//!   代价是"用户在镜像里删东西"我们不知道 —— 所以**创建侧一律幂等**（先清同名再建）。
//! - **provider 必须常驻**：我们不在时镜像里的占位符读不了（实测：os error 362 "云文件提供程序
//!   未运行"）。所以拖放只声明复制（落点是目标自己写的真实文件，不依赖我们），启动自愈、退出注销。
//!
//! 对端项**只有这一条路**了：`drag::prepare_peer_payload` 直接调 [`place`]。曾经的两条旧路
//! （小项提前转存到 `staging`、大项走 OLE 虚拟文件 + 自定义 `IStream`）与那个验证期开关
//! （`ORBDROP_DRAG_MIRROR`）在实机验证通过后已整体删除。
//!
//! 实测数值、踩过的坑、以及**还没验证的部分**都记在 `docs/cfapi-mirror.md`，改这里之前先读它。
//!
//! 注意：镜像与已经被删掉的 `staging` 只是"都在数据目录里"这一点相同，语义完全不同 ——
//! `staging` 放的是**真实字节**，镜像放的是**占位符**。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant, SystemTime};

use tracing::{info, warn};

use crate::state::AppState;

pub mod identity;

#[cfg(windows)]
mod win;

/// 镜像里一项在磁盘上留多久（兜底）。真正让回收及时的是"目标关句柄"那条路（见 [`note_closed`]）：
/// 进程重启过、回调没收到的情况才靠这条按龄扫。
const MIRROR_TTL: Duration = Duration::from_secs(6 * 60 * 60);

/// 目标关句柄之后多久可以回收。留一段余地：复制完再校验一遍、杀软顺手扫一眼都会重新打开，
/// 关掉就删会把紧接着的那次读打成失败。
const CLOSE_GRACE: Duration = Duration::from_secs(60);

/// 镜像根：`<数据目录>/mirror`。
///
/// 与 `config.toml` / `logs` 并列，同样放在数据目录里 —— `fs_browse` 已经约定不把数据目录
/// 列给用户看，这里沿用。
pub fn root() -> PathBuf {
    crate::config::data_dir().join("mirror")
}

/// 要镜像到本地的一项。
#[derive(Debug, Clone)]
pub struct MirroredItem {
    pub peer_id: String,
    pub share_id: String,
    /// 共享项内相对路径（`/` 分隔；`""` = 共享项根）
    pub path: String,
    /// 用户看到的名字（平台只接受单段名字，清洗由 [`identity::child_path`] 负责）
    pub name: String,
    pub is_dir: bool,
}

/// 把一项镜像到本地：建占位符，返回**交给 `CF_HDROP` 的真实路径**。
///
/// 不下载任何字节；文件的大小与时间戳要问一次对端（占位符必须报真实大小，否则目标会拷出
/// 0 字节）。大小问不到就报错 —— 由调用方退回旧路径，而不是拿一个 0 字节的镜像糊弄过去。
pub async fn place(state: &Arc<AppState>, item: &MirroredItem) -> Result<PathBuf, String> {
    #[cfg(windows)]
    {
        ensure_ready(state.clone());
        win::place_item(state, item).await
    }
    #[cfg(not(windows))]
    {
        let _ = (state, item);
        Err("镜像只在 Windows 上有实现".into())
    }
}

/// 启动时自愈：注册（需要时）+ 连接。失败只记日志 —— 开关打开才有人走这条路，
/// 起不来就退回旧路径，不该拦住应用启动。
pub fn ensure_ready(state: Arc<AppState>) {
    #[cfg(windows)]
    {
        if let Err(e) = win::ensure_ready(state) {
            warn!(error = %e, "镜像 provider 没起来（拖放会退回旧路径）");
        }
    }
    #[cfg(not(windows))]
    {
        let _ = state;
    }
}

/// 退出时注销同步根。
///
/// 不做这一步会留下"有同步根、没有 provider"的目录：里面的占位符在别的程序看来是坏文件。
pub fn shutdown() {
    #[cfg(windows)]
    win::shutdown();
}

/// 目标关掉了镜像里某个文件的最后一个句柄 —— 之后它随时可以被回收。
///
/// 记的是路径（Win32 形式），与 [`sweep`] 里的路径对得上。
pub fn note_closed(path: &str) {
    IDLE.lock().unwrap().insert(normalize(path), Instant::now());
}

/// 目标又打开了这个文件：把"可回收"标记撤掉。
pub fn note_opened(path: &str) {
    IDLE.lock().unwrap().remove(&normalize(path));
}

/// 已回收/已失效的路径要从记账里去掉，免得它永远留在表里。
fn forget_idle(path: &str) {
    IDLE.lock().unwrap().remove(&normalize(path));
}

fn normalize(path: &str) -> String {
    path.to_ascii_lowercase()
}

/// 路径 → 目标最后一次关句柄的时刻。
static IDLE: LazyLock<Mutex<HashMap<String, Instant>>> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// 清掉可以回收的镜像项：关句柄满 [`CLOSE_GRACE`] 的，或按龄超过 [`MIRROR_TTL`] 的。
///
/// 与"启动 + 每次拖拽时按龄扫"同一套做法，但判据多一条"目标已经不再读它" ——
/// 这条让回收从"六小时后"提前到"复制完一分钟"。
pub fn sweep() {
    let root = root();
    let Ok(peers) = std::fs::read_dir(&root) else {
        return; // 还没放过东西
    };
    let idle = IDLE.lock().unwrap().clone();
    let now = Instant::now();
    for peer in peers.flatten() {
        let Ok(shares) = std::fs::read_dir(peer.path()) else {
            continue;
        };
        for share in shares.flatten() {
            let Ok(items) = std::fs::read_dir(share.path()) else {
                continue;
            };
            for item in items.flatten() {
                if !should_reclaim(&item.path(), &idle, now) {
                    continue;
                }
                let path = item.path();
                let removed = if path.is_dir() {
                    std::fs::remove_dir_all(&path)
                } else {
                    std::fs::remove_file(&path)
                };
                match removed {
                    Ok(()) => {
                        forget_idle(&path.to_string_lossy());
                        info!(path = %path.display(), "镜像项已回收");
                    }
                    Err(e) => warn!(path = %path.display(), error = %e, "镜像项回收失败"),
                }
            }
            // 空的共享项/对端目录一并收掉（它们是普通目录，会一直留着）
            let _ = std::fs::remove_dir(share.path());
        }
        let _ = std::fs::remove_dir(peer.path());
    }
}

/// 判据：要么"目标已经不再读它了"，要么"放太久了"。
fn should_reclaim(path: &Path, idle: &HashMap<String, Instant>, now: Instant) -> bool {
    let key = normalize(&path.to_string_lossy());
    if let Some(last_close) = idle.get(&key) {
        return now.duration_since(*last_close) >= CLOSE_GRACE;
    }
    let age = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| SystemTime::now().duration_since(t).ok());
    age.is_some_and(|age| age >= MIRROR_TTL)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reclaim_needs_either_idle_timeout_or_age() {
        let mut idle = HashMap::new();
        // 不存在的路径 + 没有记账 → 既不超龄也不空闲，不能回收（宁可不删）
        idle.clear();
        assert!(!should_reclaim(Path::new(r"C:\definitely\missing"), &idle, Instant::now()));
    }

    #[test]
    fn close_grace_holds_items_that_are_still_hot() {
        let path = Path::new(r"C:\mirror\p\s\a.bin");
        let now = Instant::now();
        let mut idle = HashMap::new();
        idle.insert(normalize(&path.to_string_lossy()), now);
        assert!(!should_reclaim(path, &idle, now), "刚关掉句柄要等 grace");
        assert!(
            should_reclaim(path, &idle, now + CLOSE_GRACE),
            "过了 grace 就可回收"
        );
    }

    #[test]
    fn paths_are_matched_case_insensitively() {
        assert_eq!(normalize(r"C:\Mirror\P\A.BIN"), normalize(r"c:\mirror\p\a.bin"));
    }
}