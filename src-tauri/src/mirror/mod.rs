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
//!   代价是"用户在镜像里删东西"我们不知道 —— 所以**创建侧一律幂等**：同名同身份直接复用、
//!   未变化就保留已水合字节；需要刷新时按**同名**重建，名字被占就等它释放（绝不换名）。
//!   只有同名落点确认是**另一项**（清洗后撞名）才改用不冲突的本地名，绝不删正在读的项。
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
/// macOS 没有 CFAPI 的公开等价物：对端项在**起拖之前**整份落到镜像目录，再按真实路径交给
/// `NSDraggingSession`（见 `docs/macos.md`）。文件/目录的回收与 Windows 共用下面的 `sweep`。
#[cfg(target_os = "macos")]
mod mac;

/// 镜像里一项在磁盘上留多久（兜底）。真正让回收及时的是"目标关句柄"那条路（见 [`note_closed`]）：
/// 进程重启过、回调没收到的情况才靠这条按龄扫。
///
/// 年龄必须按**本地放入镜像的时刻**算（[`Lifecycle::placed`]），不能用文件 mtime：占位符的
/// mtime 是对端的修改时间（可能是几天前），拿它当"放了多久"会在 sweep 里删掉刚放好、
/// 甚至正在被目标复制的那一项 —— 目标侧看到的就是"源文件已损坏或不可用"。
#[cfg(target_os = "macos")]
const MIRROR_TTL: Duration = Duration::from_secs(24 * 60 * 60);
/// Windows 的占位符水合有「目标关句柄」那条及时回收；macOS 的暂存是普通文件，没有句柄回调，
/// 只能靠 TTL，所以给足一天（用户把一份大目录拖到 Finder、过一夜再回来读也不该空）。
#[cfg(not(target_os = "macos"))]
const MIRROR_TTL: Duration = Duration::from_secs(6 * 60 * 60);

/// 目标关句柄之后多久可以回收。留一段余地：复制完再校验一遍、杀软顺手扫一眼都会重新打开，
/// 关掉就删会把紧接着的那次读打成失败。
const CLOSE_GRACE: Duration = Duration::from_secs(60);

/// 后台回收的节拍。回收**不在拖拽路径上**：拖拽路径里做扫描既慢，又会和"刚放好的项"抢同一
/// 路径（并发拖两个项时，后一个的扫描可能把前一个正在复制的占位符删掉）。
#[cfg(any(windows, target_os = "macos"))]
const SWEEP_INTERVAL: Duration = Duration::from_secs(120);

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
    #[cfg(target_os = "macos")]
    {
        mac::place_item(state, item).await
    }
    #[cfg(not(any(windows, target_os = "macos")))]
    {
        let _ = (state, item);
        Err("当前平台没有对端项的拖出实现".into())
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

/// 一项在进程内的生命周期记账。四张表由同一把锁保护，判据才不会读到互相矛盾的状态。
#[derive(Default)]
struct Lifecycle {
    /// 此刻仍开着的句柄数（按路径）。平台的 OPEN / CLOSE_COMPLETION 成对回调维护它。
    open: HashMap<String, usize>,
    /// 最后一个句柄关掉的时刻：回收的 grace 从这一刻起算。
    idle: HashMap<String, Instant>,
    /// 本地占位符放入/复用的时刻：TTL 兜底从这里起算（**不是**文件 mtime）。
    placed: HashMap<String, Instant>,
    /// 本地路径 → 我们写进去的身份 blob。同名复用/撞名判定用它，不用再去平台问
    /// （`CfGetPlaceholderInfo` 的返回布局/打开行为都多一层不确定性）。
    identities: HashMap<String, Vec<u8>>,
}

static LIFE: LazyLock<Mutex<Lifecycle>> = LazyLock::new(|| Mutex::new(Lifecycle::default()));

/// 目标又打开了这个文件：句柄计数 +1，撤掉"可回收"标记。
pub fn note_opened(path: &str) {
    let key = normalize(path);
    let mut life = LIFE.lock().unwrap();
    *life.open.entry(key.clone()).or_insert(0) += 1;
    life.idle.remove(&key);
}

/// 目标关掉了一个句柄：只有**最后一个**关掉时才记"空闲"。
///
/// 旧实现每关一个句柄就记一次空闲 —— 多个读者时，先关的那个会让整项在 [`CLOSE_GRACE`]
/// 之后被回收，剩下的读者当场失败。
pub fn note_closed(path: &str) {
    let key = normalize(path);
    let mut life = LIFE.lock().unwrap();
    match life.open.get_mut(&key) {
        Some(count) if *count > 1 => *count -= 1,
        Some(_) => {
            life.open.remove(&key);
            life.idle.insert(key, Instant::now());
        }
        // 没见到对应的 OPEN（回调丢失/进程外删除）：仍然按"刚刚关掉"处理，宁可不删。
        None => {
            life.idle.insert(key, Instant::now());
        }
    }
}

/// 项被删掉了（目标删除型关闭 / 我们自己回收）：状态表里别再留着它。
///
/// **身份 blob 故意保留**：同一项下一次拖出（尤其是上一次复制出错后重试）要靠它证明
/// "同名落点就是这一项"，从而坚持用**同一个本地名**继续，而不是换名产生 `(2)` 副本。
/// 新的 `place` 会覆盖这条记录；内存开销只有每条路径一个 blob。
pub fn note_deleted(path: &str) {
    let key = normalize(path);
    let mut life = LIFE.lock().unwrap();
    match life.open.get_mut(&key) {
        Some(count) if *count > 1 => *count -= 1,
        _ => {
            life.open.remove(&key);
        }
    }
    life.idle.remove(&key);
    life.placed.remove(&key);
}

/// 某项刚被放入镜像（新建或复用）：TTL 从这一刻起算，并记下它的身份 blob。
pub fn note_placed(path: &Path, blob: &[u8]) {
    let key = normalize(&path.to_string_lossy());
    let mut life = LIFE.lock().unwrap();
    life.placed.insert(key.clone(), Instant::now());
    life.idle.remove(&key);
    life.identities.insert(key, blob.to_vec());
}

/// 这个本地路径最近一次放的是哪一项（创建/复用/删除后都保留）。
///
/// 用途是区分「同一项重试」与「另一项清洗后撞名」：同一项必须沿用原名继续，
/// 只有确认是另一项时才允许换名。
pub fn known_identity(path: &Path) -> Option<Vec<u8>> {
    let key = normalize(&path.to_string_lossy());
    LIFE.lock().unwrap().identities.get(&key).cloned()
}

/// 这一项（或它目录下任意子项）此刻还有目标在读写，或刚刚才关掉句柄。
///
/// `create_placeholder` 用它决定"能不能删掉重建"：正在读的项一律复用/换名，
/// 删掉会让目标正在进行的复制失败，还可能留下 delete-pending 让后续同名创建一直失败。
pub fn is_busy(path: &Path) -> bool {
    let key = normalize(&path.to_string_lossy());
    let prefix = format!("{key}\\");
    let life = LIFE.lock().unwrap();
    let now = Instant::now();
    let recently_closed = |closed: &Instant| now.saturating_duration_since(*closed) < CLOSE_GRACE;
    life.open.contains_key(&key)
        || life.open.keys().any(|open| open.starts_with(&prefix))
        || life.idle.get(&key).is_some_and(recently_closed)
        // 目录：任意子项刚被读过，整棵树也不能删（资源管理器复制完一个文件还会接着读下一个）。
        || life
            .idle
            .iter()
            .any(|(closed_path, closed)| closed_path.starts_with(&prefix) && recently_closed(closed))
}

fn normalize(path: &str) -> String {
    path.to_ascii_lowercase()
}

/// 清掉可以回收的镜像项：关句柄满 [`CLOSE_GRACE`] 的，或按龄超过 [`MIRROR_TTL`] 的。
///
/// 只在启动时和后台周期里跑，**不在拖拽路径上**：拖拽路径里跑扫描会和"刚放好的项"抢路径
/// （并发拖两项时，后一项的扫描可能把前一项正在复制的占位符删掉，目标报"源文件不可用"）。
pub fn sweep() {
    let root = root();
    let Ok(peers) = std::fs::read_dir(&root) else {
        return; // 还没放过东西
    };
    for peer in peers.flatten() {
        let Ok(shares) = std::fs::read_dir(peer.path()) else {
            continue;
        };
        for share in shares.flatten() {
            let Ok(items) = std::fs::read_dir(share.path()) else {
                continue;
            };
            for item in items.flatten() {
                let path = item.path();
                if !reclaimable(&path) {
                    continue;
                }
                // 删之前再确认一次：放置/打开可能刚好挤进来。
                if !reclaimable(&path) {
                    continue;
                }
                let removed = if path.is_dir() {
                    std::fs::remove_dir_all(&path)
                } else {
                    std::fs::remove_file(&path)
                };
                match removed {
                    Ok(()) => {
                        note_deleted(&path.to_string_lossy());
                        info!(path = %path.display(), "镜像项已回收");
                    }
                    Err(e) => warn!(path = %path.display(), error = %e, "镜像项回收失败"),
                }
            }
            // 空的共享项/对端目录**不删**：那会和并发 `place` 的 `create_dir_all` 抢同一路径
            // （删掉刚建好的父目录 → 建占位符时报"路径不存在"）。空目录留着无妨，退出时
            // 整棵镜像树一起清掉。
        }
    }
}

fn reclaimable(path: &Path) -> bool {
    let life = LIFE.lock().unwrap();
    should_reclaim(path, &life, Instant::now())
}

/// 判据：目标已经不再读它满 grace，或本地放入超过 TTL；**任何开着的句柄都一票否决**。
fn should_reclaim(path: &Path, life: &Lifecycle, now: Instant) -> bool {
    let key = normalize(&path.to_string_lossy());
    // 刚放入/复用的项先放行：这是常态，也避免为每一项去扫 open 表。
    if let Some(placed) = life.placed.get(&key) {
        if now.saturating_duration_since(*placed) < MIRROR_TTL {
            return false;
        }
    }
    if life.open.contains_key(&key) {
        return false;
    }
    // 目录：只要有子项还开着，整棵树都不能删（sweep 扫的是顶层项）。
    let prefix = format!("{key}\\");
    if life.open.keys().any(|open| open.starts_with(&prefix)) {
        return false;
    }
    if let Some(last_close) = life.idle.get(&key) {
        return now.saturating_duration_since(*last_close) >= CLOSE_GRACE;
    }
    if let Some(placed) = life.placed.get(&key) {
        return now.saturating_duration_since(*placed) >= MIRROR_TTL;
    }
    // 上一个进程留下的项（本进程没记过账）：只能看本地创建时间。启动时的 sweep 在任何目标
    // 打开它们之前跑，不存在"正在读"的窗口。
    std::fs::metadata(path)
        .and_then(|m| m.created())
        .ok()
        .and_then(|created| SystemTime::now().duration_since(created).ok())
        .is_some_and(|age| age >= MIRROR_TTL)
}

/// 启动后台周期回收。启动时由调用方先同步 [`sweep`] 一次，之后按 [`SWEEP_INTERVAL`] 收。
pub fn start_sweeper() {
    #[cfg(any(windows, target_os = "macos"))]
    crate::task::spawn("mirror sweep", async {
        loop {
            tokio::time::sleep(SWEEP_INTERVAL).await;
            if tokio::task::spawn_blocking(sweep).await.is_err() {
                warn!("镜像回收任务异常退出");
                return;
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reclaim_needs_either_idle_timeout_or_age() {
        let life = Lifecycle::default();
        // 不存在的路径 + 没有记账 → 既不超龄也不空闲，不能回收（宁可不删）
        assert!(!should_reclaim(
            Path::new(r"C:\definitely\missing"),
            &life,
            Instant::now()
        ));
    }

    #[test]
    fn close_grace_holds_items_that_are_still_hot() {
        let path = Path::new(r"C:\mirror\p\s\a.bin");
        let now = Instant::now();
        let mut life = Lifecycle::default();
        life.idle.insert(normalize(&path.to_string_lossy()), now);
        assert!(!should_reclaim(path, &life, now), "刚关掉句柄要等 grace");
        assert!(
            should_reclaim(path, &life, now + CLOSE_GRACE),
            "过了 grace 就可回收"
        );
    }

    /// 占位符的 mtime 是**对端的修改时间**（可能是几天前）。TTL 必须看本地放入时刻，
    /// 否则下一次 sweep 会把刚放好的项当成"放了六小时"删掉 —— 实机上的
    /// "源文件已损坏或不可用"就是它。
    #[test]
    fn fresh_placement_survives_an_old_remote_mtime() {
        let path = Path::new(r"C:\mirror\p\s\a.bin");
        let now = Instant::now();
        let mut life = Lifecycle::default();
        life.placed.insert(normalize(&path.to_string_lossy()), now);
        assert!(!should_reclaim(path, &life, now + Duration::from_secs(60)));
        assert!(
            !should_reclaim(path, &life, now + MIRROR_TTL - Duration::from_secs(1)),
            "TTL 之前不能回收"
        );
        assert!(
            should_reclaim(path, &life, now + MIRROR_TTL),
            "TTL 之后才可回收"
        );
    }

    /// 还开着的项（包括目录下某个子项还开着）一票否决，年龄再大也不许删。
    #[test]
    fn open_handles_veto_reclaim() {
        let dir = Path::new(r"C:\mirror\p\s\dir");
        let now = Instant::now();
        let mut life = Lifecycle::default();
        life.placed.insert(
            normalize(&dir.to_string_lossy()),
            now - MIRROR_TTL - Duration::from_secs(1),
        );
        life.open.insert(normalize(&dir.to_string_lossy()), 1);
        assert!(!should_reclaim(dir, &life, now));

        let mut life = Lifecycle::default();
        life.placed.insert(
            normalize(&dir.to_string_lossy()),
            now - MIRROR_TTL - Duration::from_secs(1),
        );
        life.open
            .insert(normalize(r"C:\mirror\p\s\dir\child.bin"), 1);
        assert!(
            !should_reclaim(dir, &life, now),
            "子项还开着，整棵树都不能删"
        );
    }

    #[test]
    fn paths_are_matched_case_insensitively() {
        assert_eq!(
            normalize(r"C:\Mirror\P\A.BIN"),
            normalize(r"c:\mirror\p\a.bin")
        );
    }
}
