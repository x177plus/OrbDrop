//! CFAPI provider（Windows 实现）：注册/连接同步根，并把平台的回调落到对端上。
//!
//! 时序（一条完整的拖出去）：
//!
//! 1. **抓起**：`place_item` 建一个占位符（文件；或目录时建一个"未填充"的目录占位符），
//!    交给 `CF_HDROP` 的就是这个本地路径。零字节下载。
//! 2. **目标读它**：内核发现是占位符 → 把那个读挂起 → 回调 [`fetch_data`]（系统线程池线程）。
//!    这里按需从对端拉那一段，`CfExecute` 交回去；平台拿到"够用的数据"就放行用户的读
//!    （`PROGRESSIVE`），剩下的在句柄还开着时继续拉。
//! 3. **目标枚举目录**：回调 [`fetch_placeholders`] —— 我们问对端 `/tree`，把条目作为
//!    `TRANSFER_PLACEHOLDERS` 一次交上去，平台自己建子占位符。子目录同样保持"未填充"，
//!    所以整棵树是**边复制边长出来的**。
//! 4. **目标关掉最后一个句柄**：回调 [`close_completion`] 记账，`mirror::sweep` 随后回收。
//!
//! 几条踩过的硬约束（都有实测/文档依据）：
//!
//! - 目录占位符**也必须带身份 blob**：不带的话每条 entry 都报 `S_OK`，整体却返回
//!   `0x8007017C`，而且那个目录**访问不了** —— 一个会骗人的失败模式。
//! - `RelativeFileName` **只能是单段名字**（带路径分隔符会 `0x8007007B`），所以建树是
//!   "一个目录一次调用"，而且父目录必须先真实存在。
//! - 交付数据时 offset/length 必须 **4 KiB 对齐**，除非这一段结束在文件末尾。
//! - 失败必须用 `STATUS_CLOUD_FILE_*` 状态回报：否则平台一律折成 `UNSUCCESSFUL`，而**绝不能**
//!   报成功 —— 那会让目标拷出一份静默残缺的文件。
//! - **每条回调都必须有回音**：交付（`TRANSFER_DATA`）或判失败（`STATUS_CLOUD_FILE_*`）。静默
//!   返回 = 平台那边的读一直挂在这次请求上（文档：每个回调请求有 60 秒超时，而任何一次有效
//!   操作都会重置全部挂起请求的计时器）。实机踩死过一次：见 [`note_cancel`] —— 撤销不能记成
//!   "这个偏移以后都别拉"，否则平台重发的同一段会被吞掉，目标侧表现就是拖放卡住。

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, OnceLock};

use parking_lot::Mutex;

use tracing::{error, info, warn};
use windows::core::{GUID, PCWSTR};
use windows::Win32::Foundation::{
    ERROR_CLOUD_FILE_REQUEST_ABORTED, ERROR_CLOUD_FILE_REQUEST_CANCELED,
    ERROR_CLOUD_FILE_REQUEST_TIMEOUT, STATUS_CLOUD_FILE_UNSUCCESSFUL, STATUS_SUCCESS,
};
use windows::Win32::Storage::CloudFilters::*;
use windows::Win32::Storage::FileSystem::{
    FindClose, FindFirstFileW, FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL, FILE_BASIC_INFO,
    WIN32_FIND_DATAW,
};

use super::{identity, note_closed, note_deleted, note_opened, note_placed, MirroredItem};
use crate::peer_fs::{self, PeerFile, READ_CHUNK};
use crate::state::AppState;

/// 平台要求的对齐：交付数据的 offset/length 都是 4 KiB 的倍数（结束在文件末尾的那段除外）。
const ALIGN: u64 = 4096;

/// 同名落点被占用时，等它释放的节拍与上限（3 秒）。
///
/// 重试同一项时**绝不换名**：目标目录里的文件名由本地名决定，换名会多出一份 `(2)`。
const SAME_NAME_RETRIES: usize = 15;
const SAME_NAME_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(200);

/// OrbDrop 的 provider 身份。同一个 GUID 让平台把不同版本认成同一个提供程序（遥测用）。
const PROVIDER_ID: GUID = GUID::from_u128(0x7f3c1e9a_2d5b_4a86_9f14_6c8b0a5d2e37);

/// 回调线程要用到的东西：对端访问要 `AppState`，异步要**应用自己的**运行时。
///
/// HTTP 客户端不在这里存：它必须**按目标设备**钉住证书（见 `net::DialPin`），而一台
/// provider 会被同一台机器上的多台对端共用。`peer_fs::client_for` 会按
/// `(地址, 指纹)` 取缓存里的那一把 —— 每个区间回调都调它也不会重新握手
/// （真机实测：复用之前每段多几十毫秒，差值全在握手与新建客户端上）。
struct Provider {
    state: Arc<AppState>,
    /// 连接建立时抓下来的运行时：回调在系统线程池线程上跑，那里问不到"当前运行时"。
    rt: tokio::runtime::Handle,
}

/// 进程内唯一的 provider。回调只有函数指针可用，拿不到闭包，所以上下文走全局。
///
/// `Box::leak` 是有意的：连接活到进程结束，回收这点内存没有意义，而"连接期间指针必须有效"
/// 是平台的硬要求。
static PROVIDER: OnceLock<&'static Provider> = OnceLock::new();

/// 平台的连接句柄。`CF_CONNECTION_KEY` 是 `i64` 的透明包装，`Copy`、没有 Drop
/// （显式 `Free` 才会断开），所以在全局里存一份是安全的。
static CONNECTION: OnceLock<CF_CONNECTION_KEY> = OnceLock::new();

// ---------------------------------------------------------------------------
// 撤销：只作废**在飞的那一次请求**，绝不记成"这个偏移以后都别拉"
// ---------------------------------------------------------------------------

/// 此刻在飞的水合请求。平台的 `CANCEL_FETCH_DATA` 只能按 `(file_id, 区间)` 报，所以撤销的
/// 认领也只能靠这两样；认领的对象是**请求本身**，不是偏移（理由见 [`note_cancel`]）。
static IN_FLIGHT: LazyLock<Mutex<Vec<InFlight>>> = LazyLock::new(|| Mutex::new(Vec::new()));

/// 请求在表里的编号：同一个 file_id 上同区间的两条请求也得分得开（两处读会各发一条）。
static NEXT_FETCH_ID: AtomicU64 = AtomicU64::new(1);

struct InFlight {
    id: u64,
    file_id: i64,
    start: u64,
    end: u64,
    /// 平台把这次请求作废了（`CANCEL_FETCH_DATA` 认领到了它）。
    cancelled: bool,
}

/// 一次水合请求在表里的登记。`Drop` 时摘掉自己那条 —— 回调体里 panic 也不会留下垃圾。
struct FetchGuard {
    id: u64,
}

impl FetchGuard {
    /// 登记一次请求，`[start, end)` 是这次要交付的对齐区间。
    fn begin(file_id: i64, start: u64, end: u64) -> Self {
        let id = NEXT_FETCH_ID.fetch_add(1, Ordering::Relaxed);
        IN_FLIGHT.lock().push(InFlight {
            id,
            file_id,
            start,
            end,
            cancelled: false,
        });
        Self { id }
    }

    /// 平台撤销了**这一次**请求？拉取循环每次交付前看一眼，别白拉。
    fn cancelled(&self) -> bool {
        IN_FLIGHT
            .lock()
            .iter()
            .any(|f| f.id == self.id && f.cancelled)
    }
}

impl Drop for FetchGuard {
    fn drop(&mut self) {
        IN_FLIGHT.lock().retain(|f| f.id != self.id);
    }
}

/// 平台撤销了一次读（`CANCEL_FETCH_DATA`）：**只认领一条**在飞、区间重叠的请求。
///
/// 撤销是"这一次请求"的事，不是"这个偏移永远别拉"；而且平台只给 `(file_id, 区间)`，不给
/// 请求身份 —— 同一区间上**同时**可能有多条请求（资源管理器落下时先读一页解析路径，紧接着
/// 为真正的复制重发同一页）。老实现把**所有**重叠的请求都标成撤销，等于把那条刚发出的复制
/// 请求也一起吞掉：它既不交付也不判失败，平台那个读就一直挂着（官方文档：每个回调请求有
/// 60 秒超时，而**任何一次有效操作都会重置全部挂起请求的计时器**）—— 实机表现是资源管理器
/// 报"源文件已损坏或不可用"、随后整个拖放都不再响应。
///
/// 所以：**一次撤销只消费一条请求**，挑编号最小的那条 —— 撤销来自"句柄关闭"，对应的是先
/// 发起的那条读；后来的请求（真正的复制）一律照常交付。撤销回调比请求回调先到的竞态仍不
/// 在这里猜：那条请求照常交付，`CfExecute` 对已被平台作废的请求会返回错误（我们只当没发生），
/// 而猜错会误伤一条没人作废的读。
fn note_cancel(file_id: i64, start: u64, end: u64) {
    let mut table = IN_FLIGHT.lock();
    // 编号单调递增 = 发起顺序；只挑最早的那条，绝不连坐。
    let oldest = table
        .iter_mut()
        .filter(|f| f.file_id == file_id && !f.cancelled && overlaps(f.start, f.end, start, end))
        .min_by_key(|f| f.id);
    match oldest {
        Some(fetch) => {
            fetch.cancelled = true;
            let id = fetch.id;
            drop(table);
            info!(
                file_id,
                id, start, end, "平台撤销了一次读（只作废这一条在飞请求）"
            );
        }
        None => {
            drop(table);
            // 没有在飞的请求可作废：这次撤销对应的区间我们已经交付完了。记一行就够，不留在表里。
            info!(
                file_id,
                start, end, "撤销到的区间没有在飞的请求（多半已经交付完）"
            );
        }
    }
}

fn overlaps(a_start: u64, a_end: u64, b_start: u64, b_end: u64) -> bool {
    a_start < b_end && b_start < a_end
}

/// 回调表必须以 `CF_CALLBACK_REGISTRATION_END`（= `{ CF_CALLBACK_TYPE_NONE, NULL }`）收尾；
/// 这个哨兵在 `windows` crate 里没有常量，得自己写出来。
///
/// 只注册我们确实实现的：**不注册 `NOTIFY_DELETE` / `NOTIFY_RENAME`** —— 镜像只读单向，
/// 用户/别的程序在镜像里删改东西我们不需要回写对端（见模块头）。
static CALLBACKS: [CF_CALLBACK_REGISTRATION; 6] = [
    CF_CALLBACK_REGISTRATION {
        Type: CF_CALLBACK_TYPE_FETCH_DATA,
        Callback: Some(fetch_data_cb),
    },
    CF_CALLBACK_REGISTRATION {
        Type: CF_CALLBACK_TYPE_FETCH_PLACEHOLDERS,
        Callback: Some(fetch_placeholders_cb),
    },
    CF_CALLBACK_REGISTRATION {
        Type: CF_CALLBACK_TYPE_CANCEL_FETCH_DATA,
        Callback: Some(cancel_fetch_data_cb),
    },
    CF_CALLBACK_REGISTRATION {
        Type: CF_CALLBACK_TYPE_NOTIFY_FILE_OPEN_COMPLETION,
        Callback: Some(open_completion_cb),
    },
    CF_CALLBACK_REGISTRATION {
        Type: CF_CALLBACK_TYPE_NOTIFY_FILE_CLOSE_COMPLETION,
        Callback: Some(close_completion_cb),
    },
    CF_CALLBACK_REGISTRATION {
        Type: CF_CALLBACK_TYPE_NONE,
        Callback: None,
    },
];

// ---------------------------------------------------------------------------
// 生命周期：注册 → 连接 → 注销
// ---------------------------------------------------------------------------

/// 注册（需要时）并连接同步根。幂等：已经连上就直接返回。
///
/// 调用点有两处：启动时（`mirror::ensure_ready`）与每次放镜像项之前 —— 前者是为了"应用一起来
/// 镜像就能用"，后者是为了在注册失败后又自愈回来。
pub fn ensure_ready(state: Arc<AppState>) -> Result<(), String> {
    if CONNECTION.get().is_some() {
        return Ok(());
    }
    let root = super::root();
    std::fs::create_dir_all(&root)
        .map_err(|e| format!("建镜像目录失败（{}）: {e}", root.display()))?;
    let wide_root = to_wide(root.as_os_str());

    if !is_registered(&wide_root) {
        register(&wide_root, false)?;
        info!(root = %root.display(), "同步根已注册");
    } else {
        // 已经注册过（上次进程被杀、或上一版留下的注册）：**必须用 UPDATE 把策略与标志刷新
        // 一遍** —— 注册是持久的，不刷新就会一直沿用旧策略。实机踩过：改好了根标志，可旧的
        // 注册还在，症状一模一样（镜像根列不出来 → 拖了没反应）。
        register(&wide_root, true)?;
        info!(root = %root.display(), "同步根已就绪（UPDATE 刷新策略）");
    }

    let provider: &'static Provider = PROVIDER.get_or_init(|| {
        // 在 async 上下文里调用时就是当前运行时；启动时（Tauri setup，同步上下文）退回
        // Tauri 的全局运行时 —— 那本来就是应用自己的那一个。
        let rt = tokio::runtime::Handle::try_current().unwrap_or_else(|_| crate::task::handle());
        info!("水合用的 HTTP 客户端按对端延迟建立（同一目标复用同一条 TLS 连接）");
        Box::leak(Box::new(Provider { state, rt }))
    });
    let key = connect(provider, &wide_root)?;
    let _ = CONNECTION.set(key);
    info!(root = %root.display(), "同步根已连接（镜像可用）");
    Ok(())
}

/// 退出：断开 → 删镜像树 → 注销同步根。
///
/// 为什么不只是"断开连接、把注册留着"：留着注册 = 留下一堆"有注册、没 provider"的占位符
/// （我们不在时它们读不了，OS error 362），而且用户下次重装/卸载之后那个注册还在。**镜像里的
/// 东西全是能从对端重建的**，所以退出时清干净、下次启动从头建，是最省心的做法；代价只有
/// 注册那一下（实测 1.53 ms）。
///
/// 顺序要紧：先断开（不再有回调进来），再删（删占位符时不该还有目标在读），最后注销
/// （先删再注销：注销之后残留的项会变成谁也水合不了的重解析点）。
pub fn shutdown() {
    let key = CONNECTION.get().copied();
    if let Some(key) = key {
        if !key.is_invalid() {
            // `Free` 是这条 API 家族里"释放连接"的正规入口（它会调 `CfDisconnectSyncRoot`）。
            unsafe { windows_core::Free::free(&mut { key }) };
        }
    }
    let root = super::root();
    if root.exists() {
        match std::fs::remove_dir_all(&root) {
            Ok(()) => info!(root = %root.display(), "镜像树已清空"),
            Err(e) => warn!(root = %root.display(), error = %e, "清空镜像树失败"),
        }
    }
    let wide_root = to_wide(root.as_os_str());
    if is_registered(&wide_root) {
        match unsafe { CfUnregisterSyncRoot(PCWSTR(wide_root.as_ptr())) } {
            Ok(()) => info!("同步根已注销"),
            Err(e) => warn!(error = ?e, "注销同步根失败"),
        }
    }
}

/// 这个路径上有没有同步根注册。
///
/// **必须用 BASIC**（8 字节的 `SyncRootFileId`）：`STANDARD` 是 1 KB 级的（两个 256 字符的名字
/// 数组），缓冲区给小了会返回 `ERROR_MORE_DATA` —— 那会被"只看成败"的调用方读成"没注册"，
/// 于是启动时每次重复注册、退出时又跳过注销，留下"有注册、没 provider"的残骸。
/// （这就是实测踩到的：512 字节查 STANDARD 失败，BASIC 与 4096 字节的 STANDARD 都成功。）
fn is_registered(wide_root: &[u16]) -> bool {
    let mut buf = [0u8; 32];
    unsafe {
        CfGetSyncRootInfoByPath(
            PCWSTR(wide_root.as_ptr()),
            CF_SYNC_ROOT_INFO_BASIC,
            buf.as_mut_ptr() as *mut core::ffi::c_void,
            buf.len() as u32,
            None,
        )
    }
    .is_ok()
}

fn register(wide_root: &[u16], already_registered: bool) -> Result<(), String> {
    let name = to_wide(OsStr::new("OrbDrop"));
    let version = to_wide(OsStr::new(env!("CARGO_PKG_VERSION")));
    let registration = CF_SYNC_REGISTRATION {
        StructSize: std::mem::size_of::<CF_SYNC_REGISTRATION>() as u32,
        ProviderName: PCWSTR(name.as_ptr()),
        ProviderVersion: PCWSTR(version.as_ptr()),
        SyncRootIdentity: std::ptr::null(),
        SyncRootIdentityLength: 0,
        FileIdentity: std::ptr::null(),
        FileIdentityLength: 0,
        ProviderId: PROVIDER_ID,
    };
    let policies = CF_SYNC_POLICIES {
        StructSize: std::mem::size_of::<CF_SYNC_POLICIES>() as u32,
        // PROGRESSIVE：够用的数据一到就放行用户 IO，其余在句柄还开着时继续拉（大文件是流式）。
        // AUTO_DEHYDRATION_ALLOWED：允许平台自己脱水已同步的占位符（我们不做脱水时的兜底）。
        Hydration: CF_HYDRATION_POLICY {
            Primary: CF_HYDRATION_POLICY_PROGRESSIVE,
            Modifier: CF_HYDRATION_POLICY_MODIFIER_AUTO_DEHYDRATION_ALLOWED,
        },
        // FULL：目录按需填充 —— 这是"抓起只建顶层一个占位符"的开关。
        //
        // 为什么是 FULL 而不是 PARTIAL（实测过两种）：我们的处理器每次都把**整层**拉回来、
        // 并顺手用 `DISABLE_ON_DEMAND_POPULATION` 声明"这一层齐了"，所以每个目录只需要一次回调；
        // PARTIAL 是"平台按应用实际要的条目来问"，野生目录列表会让同一个目录被反复问
        // （官方文档原话："potentially 100+ times"），而每问一次我们就要重新 `/tree` 一次。
        Population: CF_POPULATION_POLICY {
            Primary: CF_POPULATION_POLICY_FULL,
            Modifier: CF_POPULATION_POLICY_MODIFIER_NONE,
        },
        InSync: CF_INSYNC_POLICY_NONE,
        HardLink: CF_HARDLINK_POLICY(0),
        PlaceholderManagement: CF_PLACEHOLDER_MANAGEMENT_POLICY_DEFAULT,
    };
    // `DISABLE_ON_DEMAND_POPULATION_ON_ROOT` 是**必需**的（实机踩过）：不给它，平台会把
    // **镜像根自己**当成"未填充目录"来问我们要内容（`FETCH_PLACEHOLDERS`），而根是我们
    // `create_dir_all` 出来的普通目录、没有身份 blob —— 我们只能判失败，于是**整个同步根
    // 在目标眼里变得列不出来**，落下后什么都没有（实测：资源管理器解析落点路径时撞上，
    // 目标侧表现就是"拖了没反应"）。这个标志的含义正是"根不用按需填充，根下面的目录照旧按需"。
    let flags = CF_REGISTER_FLAG_DISABLE_ON_DEMAND_POPULATION_ON_ROOT
        | CF_REGISTER_FLAG_MARK_IN_SYNC_ON_ROOT;
    if already_registered {
        // 刷新既有注册（策略与标志是持久化的，不刷新就沿用旧的）。
        unsafe {
            CfRegisterSyncRoot(
                PCWSTR(wide_root.as_ptr()),
                &registration,
                &policies,
                flags | CF_REGISTER_FLAG_UPDATE,
            )
        }
        .map_err(|e| format!("刷新同步根注册失败: {e:?}"))
    } else {
        unsafe { CfRegisterSyncRoot(PCWSTR(wide_root.as_ptr()), &registration, &policies, flags) }
            .or_else(|first| {
                warn!(error = ?first, "首次注册同步根失败，改用 UPDATE 重注册");
                unsafe {
                    CfRegisterSyncRoot(
                        PCWSTR(wide_root.as_ptr()),
                        &registration,
                        &policies,
                        flags | CF_REGISTER_FLAG_UPDATE,
                    )
                }
            })
            .map_err(|e| format!("注册同步根失败: {e:?}"))
    }
}

fn connect(provider: &'static Provider, wide_root: &[u16]) -> Result<CF_CONNECTION_KEY, String> {
    // REQUIRE_PROCESS_INFO：回调里能拿到"是谁在读"（验证期判定索引器/杀软读镜像要靠它）。
    // REQUIRE_FULL_FILE_PATH：路径信息更全，日志里能直接对上是哪一项。
    let flags = CF_CONNECT_FLAG_REQUIRE_PROCESS_INFO | CF_CONNECT_FLAG_REQUIRE_FULL_FILE_PATH;
    unsafe {
        CfConnectSyncRoot(
            PCWSTR(wide_root.as_ptr()),
            CALLBACKS.as_ptr(),
            Some(provider as *const Provider as *const core::ffi::c_void),
            flags,
        )
    }
    .map_err(|e| format!("连接同步根失败: {e:?}"))
}

// ---------------------------------------------------------------------------
// 发起一侧：建占位符
// ---------------------------------------------------------------------------

/// 把一项放进镜像并返回本地路径。
pub async fn place_item(state: &Arc<AppState>, item: &MirroredItem) -> Result<PathBuf, String> {
    // 落点先校验（两个 id 都是对端自报的，见 `identity::share_dir`），**再**问本机有没有这枚
    // token：标识不合法、或不是已配对设备的项，一个目录都不该建出来。
    let dir = identity::share_dir(&super::root(), &item.peer_id, &item.share_id)?;
    let (base, token) = peer_fs::coords(state, &item.peer_id)?;
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| format!("建镜像目录失败（{}）: {e}", dir.display()))?;
    let local = identity::child_path(&dir, &item.name)?;

    // 占位符必须报**真实大小**，否则目标会拷出 0 字节。顺带把 `is_dir` 校正过来：界面的判断
    // 来自对端 `/shares` 的旧值，可能过时 —— 与旧路径同一条判据（元数据说了算）。
    let client = peer_fs::client_for(state, &item.peer_id)?;
    let meta = peer_fs::probe_entry(&client, &base, &token, &item.share_id, &item.path).await?;
    let is_dir = meta.map(|m| m.is_dir).unwrap_or(item.is_dir);
    let (size, mtime) = if is_dir {
        // 目录也保留对端 mtime：资源管理器里显示的是真实修改时间，日志/一致性判断也能用。
        (0, meta.and_then(|m| m.mtime))
    } else {
        let meta = meta.ok_or_else(|| format!("问不到「{}」的大小，这一项不走镜像", item.name))?;
        (meta.size, meta.mtime)
    };

    let id = identity::Identity {
        peer_id: item.peer_id.clone(),
        share_id: item.share_id.clone(),
        path: item.path.clone(),
        // 用**校正过**的那个判断，不能用 `item.is_dir`：界面的旧值把文件说成目录时，
        // 身份里也会跟着错，水合时就按目录去伺候了。
        is_dir,
    };
    let blob = id.encode()?;
    let name = local
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .ok_or_else(|| "镜像落点没有名字".to_string())?;
    let parent = local
        .parent()
        .ok_or_else(|| "镜像落点没有父目录".to_string())?
        .to_path_buf();

    create_placeholder(&parent, &name, &blob, size, mtime, is_dir).await
}

/// 在 `dir` 下放一个占位符，返回实际落点。
///
/// **同一项的重试必须沿用同一个本地名**：拖出去的文件名决定目标目录里的文件名，换名会在
/// 目标目录里多出一份 `(2)`，与"继续/重试同一项"的语义不符。判定顺序：
///
/// 1. 同名落点的身份与本项相同（本进程记账）：
///    - 正被目标读取 → 复用（绝不能删，删了正在复制的目标会报"源文件不可用"）；
///    - 文件且对端大小/时间戳没变 → 复用（保留已水合字节，重试只补缺的区间）；
///    - 否则按**同名**重建（等旧句柄释放，绝不加后缀）。
/// 2. 同名落点属于**另一项**（清洗后撞名）→ 换名，绝不覆盖别人的项。
/// 3. 本进程没有记录（上次进程留下的）且当前没人读 → 当作过期缓存按同名重建。
/// 4. 名字暂时不可用（delete-pending / 临时占用）→ 等它释放后**同名**重试；
///    超出上限就明确报错，不偷偷换名。
///
/// 第 1、3 条的"重建"是**删掉再建**，所以删之前必须确认落点是**本机建的 CFAPI 占位符**：
/// 普通文件/目录一律不删（交给 [`create_with_suffix`] 换名）。镜像根之外的越界落点
/// （修复前对端自报 id 能拼出去，见 `identity::share_dir`）与用户手动放进镜像里的东西
/// 都不归我们处置 —— 而目录那条走的是 `remove_dir_all`，误删不可逆。
async fn create_placeholder(
    dir: &Path,
    name: &str,
    blob: &[u8],
    size: u64,
    mtime: Option<i64>,
    is_dir: bool,
) -> Result<PathBuf, String> {
    // 父目录是 `place_item` 建的，但后台 sweep 可能刚好把空目录收掉（并发拖拽时）。
    std::fs::create_dir_all(dir)
        .map_err(|e| format!("建镜像目录失败（{}）: {e}", dir.display()))?;

    let desired = dir.join(name);
    let known = super::known_identity(&desired);
    let same = known.as_deref() == Some(blob);

    if desired.exists() {
        let placeholder = is_placeholder(&desired);
        let busy = super::is_busy(&desired);

        if same {
            if placeholder && busy {
                // 目标还在读：**不能**为了"对齐最新内容"去删/重建，否则正在进行的复制
                // 当场失败（这正是当初"源文件损坏/不可用"的来源）。这里只做一次便宜的
                // 元数据比对并记录；占用中的项一律沿用旧占位符，等句柄释放后下次拖拽再刷新。
                if !is_dir && !metadata_matches(&desired, size, mtime) {
                    warn!(
                        path = %desired.display(),
                        "目标仍在读取，但占位符与对端的大小/时间戳不一致；本次沿用旧占位符以保护进行中的复制"
                    );
                }
                info!(path = %desired.display(), "镜像项正在被目标读取，复用而不重建（同名）");
                note_placed(&desired, blob);
                return Ok(desired);
            }
            // 文件没变就复用：保留已经水合好的字节，重试只补缺的区间。
            // 目录一律按同名重建（刷新按需填充出来的子树）。
            if placeholder && !is_dir && metadata_matches(&desired, size, mtime) {
                info!(path = %desired.display(), "镜像项与对端一致，复用（保留已水合字节）");
                note_placed(&desired, blob);
                return Ok(desired);
            }
            // 同一项但需要刷新：按同名重建（`place_same_name` 只在落点确实是本机占位符时才删，
            // 被外部换成普通文件的那一份会换名，不覆盖）。
            return place_same_name(dir, name, blob, size, mtime, is_dir).await;
        }

        // 不是同一项。
        if known.is_some() {
            // 确认是另一项（清洗后撞名）：换名，避免覆盖别人的项、也避免目标目录里同名混淆。
            info!(path = %desired.display(), "同名落点属于另一项（清洗后撞名），改用新名");
            return create_with_suffix(dir, name, blob, size, mtime, is_dir, 2);
        }
        if busy {
            // 本进程没有身份记录，且落点正被读/被别的进程按住：不猜它是谁，换名最安全。
            info!(path = %desired.display(), "同名落点正被占用且本进程没有身份记录，改用新名");
            return create_with_suffix(dir, name, blob, size, mtime, is_dir, 2);
        }
        // 过期缓存/普通文件且没人用：同名重建。
        return place_same_name(dir, name, blob, size, mtime, is_dir).await;
    }

    // 名字现在不存在：同名新建；碰到 delete-pending / 临时占用就等它释放，**绝不换名**。
    place_same_name(dir, name, blob, size, mtime, is_dir).await
}

/// 用**同一个本地名**放下这一项；同名暂时不可用就等它释放，绝不换名。
///
/// 上一个复制出错/还没撒手时，落点可能是 delete-pending 或被共享占用。这里每 200ms 重试
/// 一次、最多 3 秒；释放了就用原名继续。超时只报错，不偷偷生成 `(2)` —— 用户重试同一项时
/// 要的是目标目录里那份文件继续/覆盖，而不是多一份副本。
async fn place_same_name(
    dir: &Path,
    name: &str,
    blob: &[u8],
    size: u64,
    mtime: Option<i64>,
    is_dir: bool,
) -> Result<PathBuf, String> {
    let desired = dir.join(name);
    let mut last = String::new();
    for attempt in 0..SAME_NAME_RETRIES {
        if desired.exists() {
            // **只删本机建的 CFAPI 占位符**。同名落点可能是普通文件/目录：镜像根之外的
            // 历史越界落点、用户或别的程序顺手放进镜像里的东西、占位符被外部替换成的真文件
            // —— 都不归我们处置。目录那条走 `remove_dir_all`，误删不可逆，宁可换名。
            if !is_placeholder(&desired) {
                info!(
                    path = %desired.display(), attempt,
                    "同名落点不是本机占位符（普通文件/目录），不删，改用新名"
                );
                return create_with_suffix(dir, name, blob, size, mtime, is_dir, 2);
            }
            let removed = if desired.is_dir() {
                std::fs::remove_dir_all(&desired)
            } else {
                std::fs::remove_file(&desired)
            };
            if let Err(e) = removed {
                last = format!("清理同名镜像项失败（{}）: {e}", desired.display());
                warn!(
                    path = %desired.display(), attempt, error = %e,
                    "同名落点仍被占用，等待后用同一个名字重试"
                );
                tokio::time::sleep(SAME_NAME_RETRY_DELAY).await;
                continue;
            }
        }
        match create_one(dir, name, blob, size, mtime, is_dir) {
            Ok(()) => {
                note_placed(&desired, blob);
                return Ok(desired);
            }
            Err(e) if name_unavailable(&e) => {
                last = format!("建占位符失败（{}）: {e:?}", desired.display());
                warn!(
                    path = %desired.display(), attempt, error = ?e,
                    "同名落点暂不可用，等待后用同一个名字重试"
                );
                tokio::time::sleep(SAME_NAME_RETRY_DELAY).await;
                continue;
            }
            Err(e) => return Err(format!("建占位符失败（{}）: {e:?}", desired.display())),
        }
    }
    Err(format!(
        "{last}；同名落点仍被占用，请等上一项释放后再拖一次（不会改名，以免目标目录出现重复）"
    ))
}

/// 同名落点确实是**另一项**时的退路：`name (2)`、`name (3)`… 直到建成为止。
fn create_with_suffix(
    dir: &Path,
    name: &str,
    blob: &[u8],
    size: u64,
    mtime: Option<i64>,
    is_dir: bool,
    from: usize,
) -> Result<PathBuf, String> {
    for n in from..=100 {
        let candidate = suffixed_name(name, n);
        let path = dir.join(&candidate);
        if path.exists() {
            // 上一次拖拽留下的同名副本；别覆盖它（可能还有读者），换下一个。
            continue;
        }
        match create_one(dir, &candidate, blob, size, mtime, is_dir) {
            Ok(()) => {
                note_placed(&path, blob);
                info!(path = %path.display(), "镜像项改用不冲突的本地名");
                return Ok(path);
            }
            Err(e) if name_unavailable(&e) => continue,
            Err(e) => return Err(format!("建占位符失败（{}）: {e:?}", path.display())),
        }
    }
    Err(format!("建占位符失败：找不到可用的本地名（{name}）"))
}

/// 这个路径现在是不是 CFAPI 占位符（不是普通文件/目录）。
///
/// 用 `FindFirstFileW` 取属性与 reparse tag 再问平台，**不打开文件**（不会触发水合）。
fn is_placeholder(path: &Path) -> bool {
    let wide = to_wide(path.as_os_str());
    let mut data = WIN32_FIND_DATAW::default();
    let Ok(handle) = (unsafe { FindFirstFileW(PCWSTR(wide.as_ptr()), &mut data) }) else {
        return false;
    };
    let _ = unsafe { FindClose(handle) };
    let state =
        unsafe { CfGetPlaceholderStateFromAttributeTag(data.dwFileAttributes, data.dwReserved0) };
    state.contains(CF_PLACEHOLDER_STATE_PLACEHOLDER)
}

/// 磁盘上的占位符元数据是否与刚问到的对端一致（只用于文件）。
///
/// 一致 = 内容没变，可以复用占位符与已经水合好的本地字节；不一致才按同名重建。
fn metadata_matches(path: &Path, size: u64, mtime: Option<i64>) -> bool {
    let Ok(meta) = std::fs::metadata(path) else {
        return false;
    };
    if meta.is_dir() || meta.len() != size {
        return false;
    }
    if let Some(want) = mtime {
        let Ok(modified) = meta.modified() else {
            return false;
        };
        let Ok(age) = modified.duration_since(std::time::UNIX_EPOCH) else {
            return false;
        };
        // 对端时间戳是秒级；占位符也是按秒写入的，留 2 秒余量。
        if (age.as_secs() as i64 - want).abs() > 2 {
            return false;
        }
    }
    true
}

/// 调一次 `CfCreatePlaceholders`（单条）。`Result` 字段必须逐条看：整体 OK 而单条失败是
/// 踩过的坑（目录不带身份就是那样）。
fn create_one(
    dir: &Path,
    name: &str,
    blob: &[u8],
    size: u64,
    mtime: Option<i64>,
    is_dir: bool,
) -> windows::core::Result<()> {
    let dir_w = to_wide(dir.as_os_str());
    let name_w = to_wide(OsStr::new(name));
    let mut info = CF_PLACEHOLDER_CREATE_INFO::default();
    info.RelativeFileName = PCWSTR(name_w.as_ptr());
    info.FsMetadata = fs_metadata(size, mtime, is_dir);
    info.FileIdentity = blob.as_ptr() as *const core::ffi::c_void;
    info.FileIdentityLength = blob.len() as u32;
    // MARK_IN_SYNC：内容是远端已有的事实，不是待上传的本地改动。
    // 目录**不**加 DISABLE_ON_DEMAND_POPULATION —— 那才是"未填充目录"该有的样子。
    info.Flags = CF_PLACEHOLDER_CREATE_FLAG_MARK_IN_SYNC;

    let mut array = [info];
    unsafe {
        CfCreatePlaceholders(
            PCWSTR(dir_w.as_ptr()),
            &mut array,
            CF_CREATE_FLAG_NONE,
            None,
        )
    }?;
    array[0].Result.ok()
}

/// 这个名字现在是不是"被占住/不可写"，值得换一个名字重试。
///
/// 典型：delete-pending（旧占位符还有句柄，Windows 在最后一个句柄关掉前不允许同名新建）、
/// 两个并发拖拽同时建同名项、杀软临时打开。
fn name_unavailable(e: &windows::core::Error) -> bool {
    const RETRYABLE: [u32; 5] = [
        5,   // ERROR_ACCESS_DENIED
        32,  // ERROR_SHARING_VIOLATION
        80,  // ERROR_FILE_EXISTS
        183, // ERROR_ALREADY_EXISTS
        303, // ERROR_DELETE_PENDING
    ];
    RETRYABLE.contains(&(e.code().0 as u32 & 0xFFFF))
}

/// 单段文件名加序号：`报告.pdf` → `报告 (2).pdf`；无扩展名 → `报告 (2)`。
fn suffixed_name(name: &str, n: usize) -> String {
    let path = Path::new(name);
    match (path.file_stem(), path.extension()) {
        (Some(stem), Some(ext)) => {
            format!("{} ({n}).{}", stem.to_string_lossy(), ext.to_string_lossy())
        }
        _ => format!("{name} ({n})"),
    }
}

// ---------------------------------------------------------------------------
// 回调：水合、目录懒填充、取消、开关句柄
// ---------------------------------------------------------------------------

/// 回调是 FFI 边界：里面任何 panic 都会让**整个进程**直接退出（C 栈上没有 unwind 保护，
/// Windows 上表现为"拖到一半应用消失"）。这里是最后一道网：panic 只算这一次回调失败，
/// 留一行日志，进程继续活着。（`log::record_crash` 的 panic hook 仍会照常记一条 crash.log。）
fn guarded(what: &str, f: impl FnOnce()) -> bool {
    if let Err(payload) = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)) {
        let msg = payload
            .downcast_ref::<&str>()
            .map(|s| (*s).to_string())
            .or_else(|| payload.downcast_ref::<String>().cloned())
            .unwrap_or_else(|| "未知 panic".into());
        error!(callback = what, panic = %msg, "同步根回调 panic（已拦下，进程继续）");
        return false;
    }
    true
}

unsafe extern "system" fn fetch_data_cb(
    info: *const CF_CALLBACK_INFO,
    params: *const CF_CALLBACK_PARAMETERS,
) {
    let ok = guarded("fetch_data", || {
        if let Some((info, params)) = unsafe { info.as_ref() }.zip(unsafe { params.as_ref() }) {
            fetch_data(info, params);
        }
    });
    if !ok {
        // 回调 panic 也必须给平台一个回音：静默返回 = 那个读一直挂着（见模块头）。
        if let Some(info) = unsafe { info.as_ref() } {
            fail_range(info, 0, info.FileSize.max(0) as u64, "provider 回调 panic");
        }
    }
}

unsafe extern "system" fn fetch_placeholders_cb(
    info: *const CF_CALLBACK_INFO,
    params: *const CF_CALLBACK_PARAMETERS,
) {
    let ok = guarded("fetch_placeholders", || {
        if let Some((info, params)) = unsafe { info.as_ref() }.zip(unsafe { params.as_ref() }) {
            fetch_placeholders(info, params);
        }
    });
    if !ok {
        if let Some(info) = unsafe { info.as_ref() } {
            fail_placeholders(info, "provider 回调 panic");
        }
    }
}

unsafe extern "system" fn cancel_fetch_data_cb(
    info: *const CF_CALLBACK_INFO,
    params: *const CF_CALLBACK_PARAMETERS,
) {
    guarded("cancel_fetch_data", || {
        if let Some((info, params)) = unsafe { info.as_ref() }.zip(unsafe { params.as_ref() }) {
            let range = unsafe { params.Anonymous.Cancel.Anonymous.FetchData };
            let start = range.FileOffset.max(0) as u64;
            let end = start.saturating_add(range.Length.max(0) as u64);
            info!(
                file_id = info.FileId,
                start, end, "平台撤销了这段读（只作废在飞的这次请求）"
            );
            note_cancel(info.FileId, start, end);
        }
    });
}

unsafe extern "system" fn open_completion_cb(
    info: *const CF_CALLBACK_INFO,
    _params: *const CF_CALLBACK_PARAMETERS,
) {
    guarded("open_completion", || {
        if let Some(info) = unsafe { info.as_ref() } {
            let path = win_path(info);
            let process = process_name(info);
            // 我们自己的打开/关闭不算"目标在读"：把这些也记进账，刚被本进程读过的项会在
            // grace 内被误判为 busy（复用旧元数据），回收判据也会被自己拖着走。
            if is_self(info) {
                return;
            }
            info!(path = %path, process = %process, "镜像项被打开");
            note_opened(&path);
        }
    });
}

unsafe extern "system" fn close_completion_cb(
    info: *const CF_CALLBACK_INFO,
    params: *const CF_CALLBACK_PARAMETERS,
) {
    guarded("close_completion", || {
        if let Some((info, params)) = unsafe { info.as_ref() }.zip(unsafe { params.as_ref() }) {
            let flags = unsafe { params.Anonymous.CloseCompletion.Flags };
            let path = win_path(info);
            let process = process_name(info);
            info!(
                path = %path, deleted = flags.contains(CF_CALLBACK_CLOSE_COMPLETION_FLAG_DELETED),
                process = %process,
                "镜像项已关闭（此后可回收）"
            );
            // 自己的探测关闭不参与记账（见 `open_completion_cb`）。
            if is_self(info) {
                return;
            }
            // 记账：`mirror::sweep` 会在这之后按 grace 回收。删除型关闭不必记空闲 —— 文件已经没了，
            // 四张表里都要清掉，免得留下永远匹配不上的记录。
            if flags.contains(CF_CALLBACK_CLOSE_COMPLETION_FLAG_DELETED) {
                note_deleted(&path);
            } else {
                note_closed(&path);
            }
        }
    });
}

/// 目标要读一段字节：按需从对端拉，交给平台。
fn fetch_data(info: &CF_CALLBACK_INFO, params: &CF_CALLBACK_PARAMETERS) {
    let fetch = unsafe { params.Anonymous.FetchData };
    let size = info.FileSize.max(0) as u64;
    let required_start = fetch.RequiredFileOffset.max(0) as u64;
    let required_end = match fetch.RequiredLength {
        len if len < 0 => size, // 负值 = CF_EOF：一直到文件末尾
        len => required_start.saturating_add(len as u64).min(size),
    };

    // 对齐：起点向下、终点向上（可以多给，文档明确允许），但结束在文件末尾的那一段不要求对齐。
    let start = required_start & !(ALIGN - 1);
    let end = if required_end >= size {
        size
    } else {
        align_up(required_end)
    };

    let Some(provider) = PROVIDER.get() else {
        // 没连上（理论上不会走到这里；连上了才会收到回调）。**必须回一个失败**：什么也不回的话
        // 平台那边的读会一直挂在这次请求上（见 [`note_cancel`]：回调请求有 60 秒超时，而且任何
        // 一次有效操作都会重置全部挂起请求的计时器），目标进程就卡在那里；报成功则更糟 ——
        // 目标会拷出一份静默残缺的文件。
        warn!("收到取数据回调，但 provider 没有连接");
        fail_range(info, start, end.saturating_sub(start), "provider 没有连接");
        return;
    };

    let Some(id) = decode_identity(info) else {
        fail_range(info, start, end.saturating_sub(start), "身份解不出来");
        return;
    };
    if end <= start {
        // 空文件：平台可能发一条长度为 0 的取数据请求（0 字节没有区间可对齐）。
        // 回一条 0 长度的成功 —— 判失败会让目标把 0 字节文件报成"已损坏/不可用"。
        // 如果平台不接受 0 长度，再明确判失败（绝不能静默返回）。
        if size == 0 && matches!(execute_transfer(info, &[], start, 0), Ok(())) {
            return;
        }
        fail_range(info, start, 0, "请求区间为空");
        return;
    }

    // 登记这次请求：平台随后的撤销只认领**它**（见 [`note_cancel`]），落地时（含提前返回）摘掉。
    let fetch_guard = FetchGuard::begin(info.FileId, start, end);
    info!(path = %id.path, start, end, size, resume = fetch.Flags.contains(CF_CALLBACK_FETCH_DATA_FLAG_RECOVERY), "开始水合");
    match transfer_range(provider, info, &id, start, end, &fetch_guard) {
        Ok(()) => {}
        Err(Stop::Cancelled) => {
            // 我们看到了这次请求的撤销。**必须给平台一个回音**：静默返回会让这个读一直挂着
            // （其它有效操作还会不断重置它的超时计时器）。平台若早已把请求作废，这次
            // `CfExecute` 会返回错误，忽略即可。
            info!(path = %id.path, start, end, "这次读已被平台撤销，明确判失败收尾");
            fail_range(info, start, end - start, "这次读已被平台撤销");
        }
        Err(Stop::Dropped) => {
            // 平台自己把这次读收掉了（撤销/作废/超时）：平台那边已经收尾，我们不用再说什么。
            info!(path = %id.path, start, end, "这次读已被平台收掉（撤销/作废），不再交付");
        }
        Err(Stop::Failed(e)) => {
            warn!(path = %id.path, start, end, error = %e, "水合失败");
            fail_range(info, start, end - start, &e);
        }
    }
}

/// 把 `[start, end)` 从对端搬过来交给平台。分块交付：`PROGRESSIVE` 下平台一拿到"够用的数据"
/// 就放行用户 IO，所以**不要**攒一整块再交。
fn transfer_range(
    provider: &Provider,
    info: &CF_CALLBACK_INFO,
    id: &identity::Identity,
    start: u64,
    end: u64,
    fetch: &FetchGuard,
) -> Result<(), Stop> {
    let began = std::time::Instant::now();
    let client = peer_fs::client_for(&provider.state, &id.peer_id).map_err(Stop::Failed)?;
    let (base, token) = peer_fs::coords(&provider.state, &id.peer_id).map_err(Stop::Failed)?;
    let file = PeerFile {
        base,
        token,
        share_id: id.share_id.clone(),
        path: id.path.clone(),
    };

    let total = (end - start) as i64;
    let mut delivered = 0u64;
    while start + delivered < end {
        let offset = start + delivered;
        if fetch.cancelled() {
            // 平台把**这一次**请求收掉了：停手。调用方会显式判失败收尾（绝不静默返回），
            // 见 `note_cancel` —— 一次撤销只认领一条请求，绝不外溢到后来的请求。
            return Err(Stop::Cancelled);
        }
        let want = READ_CHUNK.min(end - offset);
        // `run_blocking` 要求 future 是 'static：把客户端与坐标移进去（都是廉价的克隆）。
        let (task_client, task_file) = (client.clone(), file.clone());
        let bytes = crate::task::run_blocking_on(&provider.rt, async move {
            peer_fs::fetch_file_range(&task_client, &task_file, offset, want).await
        })
        .ok_or_else(|| Stop::Failed("取数据任务没能回到运行时".to_string()))?
        .map_err(Stop::Failed)?;
        if bytes.is_empty() {
            return Err(Stop::Failed(format!(
                "对端「{}」在 {offset} 处读到 0 字节",
                id.path
            )));
        }
        // 平台要求 offset/length 4 KiB 对齐，**除非**这一段结束在文件末尾。对端那边文件变小了
        // （远端被改过）会让这条不成立 —— 那种情况必须报错，不能交一段错位的字节。
        let length = bytes.len() as u64;
        let ends_at_eof = offset + length >= (info.FileSize.max(0) as u64);
        if !ends_at_eof && (offset % ALIGN != 0 || length % ALIGN != 0) {
            return Err(Stop::Failed(format!(
                "对端返回的区间没有 4 KiB 对齐（offset={offset} len={length}），又不落在文件末尾"
            )));
        }
        execute_transfer(info, &bytes, offset, length)?;
        delivered += length;
        report_progress(info, total, delivered as i64);
        if length < want {
            // 服务端按 len 截断：拿回来的比要的少 = 已经到末尾。
            break;
        }
    }
    // 一行"这一段搬完了"：验证期要靠它把"目标侧看到的等待"和"我们实际拉了多少/多久"对上，
    // 也是唯一能直接看出水合吞吐的地方。
    info!(
        path = %id.path, bytes = delivered, range_start = start, range_end = end,
        ms = began.elapsed().as_millis() as u64,
        "水合完成"
    );
    Ok(())
}

/// 拉取收兵的原因。
enum Stop {
    /// 平台撤销了这次读（我们看到了 `CANCEL_FETCH_DATA` 并认领到它）：调用方要明确判失败，
    /// 不能静默返回（静默 = 平台的读没人交付也没人判失败，可能一直挂着）。
    Cancelled,
    /// 平台已经把这次读自己收掉了（`CfExecute` 报撤销/作废/超时）：不用再回音。
    Dropped,
    /// 真失败：必须 `fail_range`，绝不能让平台那个读挂着（见模块头"失败必须用 `STATUS_CLOUD_FILE_*`"）。
    Failed(String),
}

/// 一个子占位符的三件套：本地名（宽字符）、身份 blob、大小、修改时间、是否目录。
///
/// **三样必须同进同出**：调用方按下标一起用（名字配身份），任何一条备不齐就整条丢，
/// 由 `dropped` 决定"这一层要不要整体报错"。老实现把名字与身份分成两个数组各自
/// `filter_map` 再按下标取 —— 丢一个就整体错位，身份少一个还会越界 panic（在平台回调里
/// 等于进程退出）。这个函数把"对齐"这件事收在一处，也让它能被单测直接盯住。
type PreparedChild = (Vec<u16>, Vec<u8>, u64, Option<i64>, bool);

fn prepare_children(
    parent: &identity::Identity,
    entries: &[peer_fs::TreeEntry],
) -> (Vec<PreparedChild>, usize) {
    let mut prepared: Vec<PreparedChild> = Vec::with_capacity(entries.len());
    let mut dropped = 0usize;
    for entry in entries.iter() {
        // 本地文件名用清洗过的（对端给的原始名可能带路径分隔符之类的怪东西）。
        let Some(clean) = peer_fs::plain_component(&entry.name) else {
            dropped += 1;
            continue;
        };
        // 远端路径必须用**原始**名字（对端按它找文件）。
        let child = identity::Identity {
            peer_id: parent.peer_id.clone(),
            share_id: parent.share_id.clone(),
            path: peer_fs::join_slash(&parent.path, &entry.name),
            is_dir: entry.is_dir,
        };
        match child.encode() {
            Ok(blob) => prepared.push((
                to_wide(OsStr::new(&clean)),
                blob,
                entry.size,
                peer_fs::parse_rfc3339_secs(&entry.modified),
                entry.is_dir,
            )),
            // 身份过长（远端路径 > IDENTITY_MAX）这类条目本机表示不了。
            Err(_) => dropped += 1,
        }
    }
    (prepared, dropped)
}

/// 目标在枚举一个"未填充"的目录：把那一层的条目交给平台，由它建子占位符。
///
/// 这一步是整个方案的省时关键 —— 抓起只建顶层一个占位符，子树在这里按目录长出来。
///
/// **实测到的行为**：平台不会为 provider **自己进程**的枚举去请求按需填充 —— 我们从本进程
/// `read_dir` 一个未填充目录只会拿到空的本地视图、回调根本不进来。真实的枚举者是资源管理器
/// 或目标应用（另一个进程），那条路是通的（集成测试用 `cmd /c dir` 顶替它来验）。
fn fetch_placeholders(info: &CF_CALLBACK_INFO, _params: &CF_CALLBACK_PARAMETERS) {
    let Some(provider) = PROVIDER.get() else {
        // 和取数据那条一样的规矩：**必须有回音**。什么也不回会让目标那边的目录枚举一直挂着。
        warn!("收到填充目录回调，但 provider 没有连接");
        fail_placeholders(info, "provider 没有连接");
        return;
    };
    let Some(id) = decode_identity(info) else {
        fail_placeholders(info, "身份解不出来");
        return;
    };
    let path = id.path.clone();
    let (peer_id, share_id, remote_path) =
        (id.peer_id.clone(), id.share_id.clone(), id.path.clone());
    let entries = crate::task::run_blocking_on(&provider.rt, async move {
        let client = peer_fs::client_for(&provider.state, &peer_id)?;
        let Ok((base, token)) = peer_fs::coords(&provider.state, &peer_id) else {
            return Err("解析对端坐标失败".to_string());
        };
        peer_fs::fetch_tree(&client, &base, &token, &share_id, &remote_path).await
    });
    let entries = match entries {
        Some(Ok(entries)) => entries,
        Some(Err(e)) => {
            warn!(path = %path, error = %e, "填充目录失败");
            fail_placeholders(info, &e);
            return;
        }
        None => {
            fail_placeholders(info, "取目录任务没能回到运行时");
            return;
        }
    };

    // **一个条目三样一起备齐**：名字、身份 blob、元数据必须一一对应。分成三个数组各自
    // `filter_map` 再按下标取是错的 —— 名字被过滤掉一个，后面的名字与身份就整体错位
    // （A 的名字配 B 的身份 = 目标按错的身份去拉另一个文件的字节）；身份编码失败让数组变短，
    // 按下标取还会越界 panic，而这里跑在平台的回调里（C 栈上没有 unwind 保护）= **进程直接退出**。
    // 备不齐的条目一律**整条丢掉**（绝不留半条），由下面那道 `dropped > 0` 的闸决定整层成败。
    let (prepared, dropped) = prepare_children(&id, &entries);

    // 有一条备不齐就**整层报错**：下面那条 `DISABLE_ON_DEMAND_POPULATION` 是在告诉平台
    // "这个目录的内容就是这些、不用再要了"，少交一条等于目标那边**悄悄**少一个文件/目录
    // （复制出来的目录树缺东西，还没有任何提示）。宁可不给，也不给一份不全的。
    if dropped > 0 {
        warn!(
            path = %path, dropped, kept = prepared.len(),
            "远端目录里有本机表示不了的条目（名字非法或身份过长），这一层整层报错"
        );
        fail_placeholders(info, "远端目录里有本机无法表示的条目");
        return;
    }

    // 指针数组要保活到 `CfExecute` 返回（`prepared` 在本函数里活着）。
    let mut array: Vec<CF_PLACEHOLDER_CREATE_INFO> = prepared
        .iter()
        .map(|(name, blob, size, mtime, is_dir)| {
            let mut ci = CF_PLACEHOLDER_CREATE_INFO::default();
            ci.RelativeFileName = PCWSTR(name.as_ptr());
            ci.FsMetadata = fs_metadata(*size, *mtime, *is_dir);
            ci.FileIdentity = blob.as_ptr() as *const core::ffi::c_void;
            ci.FileIdentityLength = blob.len() as u32;
            // 子目录同样保持"未填充"（不加 DISABLE_ON_DEMAND_POPULATION），树才会边复制边长。
            ci.Flags = CF_PLACEHOLDER_CREATE_FLAG_MARK_IN_SYNC;
            ci
        })
        .collect();

    info!(path = %path, count = array.len(), "填充目录：交回这一层");
    let mut params = CF_OPERATION_PARAMETERS::default();
    params.ParamSize = param_size::<CF_OPERATION_PARAMETERS_0_4>();
    params.Anonymous.TransferPlaceholders = CF_OPERATION_PARAMETERS_0_4 {
        // 这一层的条目一次给全 → 告诉平台"这个目录不再需要按需填充"。
        Flags: CF_OPERATION_TRANSFER_PLACEHOLDERS_FLAG_DISABLE_ON_DEMAND_POPULATION,
        CompletionStatus: STATUS_SUCCESS,
        PlaceholderTotalCount: array.len() as i64,
        PlaceholderArray: array.as_mut_ptr(),
        PlaceholderCount: array.len() as u32,
        EntriesProcessed: 0,
    };
    let op_info = operation_info(info, CF_OPERATION_TYPE_TRANSFER_PLACEHOLDERS);
    if let Err(e) = unsafe { CfExecute(&op_info, &mut params) } {
        // 和取数据那条一样：**每条回调都必须有回音**。平台已经作废这次请求时不用再说什么；
        // 其它失败要明确判失败，否则目标那边的目录枚举会一直挂在这次请求上。
        if !platform_dropped(&e) {
            warn!(path = %path, error = ?e, "交回目录内容失败，把这次枚举判失败");
            fail_placeholders(info, "交回目录内容失败");
        } else {
            info!(path = %path, error = ?e, "这次目录枚举已被平台收掉（撤销/作废）");
        }
    }
}

// ---------------------------------------------------------------------------
// CfExecute 的两个操作
// ---------------------------------------------------------------------------

fn execute_transfer(
    info: &CF_CALLBACK_INFO,
    buffer: &[u8],
    offset: u64,
    length: u64,
) -> Result<(), Stop> {
    let op_info = operation_info(info, CF_OPERATION_TYPE_TRANSFER_DATA);
    let mut params = CF_OPERATION_PARAMETERS::default();
    params.ParamSize = param_size::<CF_OPERATION_PARAMETERS_0_0>();
    params.Anonymous.TransferData = CF_OPERATION_PARAMETERS_0_0 {
        Flags: CF_OPERATION_TRANSFER_DATA_FLAG_NONE,
        CompletionStatus: STATUS_SUCCESS,
        Buffer: buffer.as_ptr() as *const core::ffi::c_void,
        Offset: offset as i64,
        Length: length as i64,
    };
    match unsafe { CfExecute(&op_info, &mut params) } {
        Ok(()) => Ok(()),
        Err(e) if platform_dropped(&e) => Err(Stop::Dropped),
        Err(e) => Err(Stop::Failed(format!("交付数据失败: {e:?}"))),
    }
}

/// 这个 `CfExecute` 失败是"平台已经把这次读自己收掉了"吗（撤销/作废/超时）？
///
/// 实测：`CANCEL_FETCH_DATA` 与拉取同时发生（撤销到的那次请求正在路上）时，交付会拿到
/// `ERROR_CLOUD_FILE_REQUEST_CANCELED`（`0x8007018E`，"用户已取消云操作。"）。这不是我们的
/// 失败 —— 平台那边早就把读作废了，交付只是白跑一趟。把它当失败去 `fail_range` 会灌两条
/// 误导性的 WARN（"水合失败"、"把这一段读判失败"），下次查日志的人会以为真出了故障。
fn platform_dropped(e: &windows::core::Error) -> bool {
    [
        ERROR_CLOUD_FILE_REQUEST_ABORTED,
        ERROR_CLOUD_FILE_REQUEST_CANCELED,
        ERROR_CLOUD_FILE_REQUEST_TIMEOUT,
    ]
    .iter()
    .any(|code| windows::core::HRESULT::from_win32(code.0) == e.code())
}

/// 让挂在这个区间上的读**明确失败**。
///
/// 必须用 `STATUS_CLOUD_FILE_*` 里的状态：这个范围之外的状态会被平台一律折成
/// `STATUS_CLOUD_FILE_UNSUCCESSFUL`。绝不报成功 —— 那会让目标拷出一份静默残缺的文件。
fn fail_range(info: &CF_CALLBACK_INFO, offset: u64, length: u64, why: &str) {
    warn!(
        file_id = info.FileId,
        offset, length, why, "把这一段读判失败"
    );
    let op_info = operation_info(info, CF_OPERATION_TYPE_TRANSFER_DATA);
    let mut params = CF_OPERATION_PARAMETERS::default();
    params.ParamSize = param_size::<CF_OPERATION_PARAMETERS_0_0>();
    params.Anonymous.TransferData = CF_OPERATION_PARAMETERS_0_0 {
        Flags: CF_OPERATION_TRANSFER_DATA_FLAG_NONE,
        CompletionStatus: STATUS_CLOUD_FILE_UNSUCCESSFUL,
        Buffer: std::ptr::null(),
        Offset: offset as i64,
        Length: length as i64,
    };
    let _ = unsafe { CfExecute(&op_info, &mut params) };
}

fn fail_placeholders(info: &CF_CALLBACK_INFO, why: &str) {
    warn!(file_id = info.FileId, why, "把这次目录枚举判失败");
    let op_info = operation_info(info, CF_OPERATION_TYPE_TRANSFER_PLACEHOLDERS);
    let mut params = CF_OPERATION_PARAMETERS::default();
    params.ParamSize = param_size::<CF_OPERATION_PARAMETERS_0_4>();
    params.Anonymous.TransferPlaceholders = CF_OPERATION_PARAMETERS_0_4 {
        Flags: CF_OPERATION_TRANSFER_PLACEHOLDERS_FLAG_NONE,
        CompletionStatus: STATUS_CLOUD_FILE_UNSUCCESSFUL,
        PlaceholderTotalCount: 0,
        PlaceholderArray: std::ptr::null_mut(),
        PlaceholderCount: 0,
        EntriesProcessed: 0,
    };
    let _ = unsafe { CfExecute(&op_info, &mut params) };
}

fn report_progress(info: &CF_CALLBACK_INFO, total: i64, completed: i64) {
    // 失败不是错误：进度只是给资源管理器的进度条用的。
    let _ =
        unsafe { CfReportProviderProgress(info.ConnectionKey, info.TransferKey, total, completed) };
}

fn operation_info(info: &CF_CALLBACK_INFO, op_type: CF_OPERATION_TYPE) -> CF_OPERATION_INFO {
    CF_OPERATION_INFO {
        StructSize: std::mem::size_of::<CF_OPERATION_INFO>() as u32,
        Type: op_type,
        ConnectionKey: info.ConnectionKey,
        TransferKey: info.TransferKey,
        CorrelationVector: std::ptr::null(),
        SyncStatus: std::ptr::null(),
        // 平台要求原样回带，别丢。
        RequestKey: info.RequestKey,
    }
}

/// `ParamSize` 必须是"子结构相对 `CF_OPERATION_PARAMETERS` 的偏移 + 子结构自身大小"。
fn param_size<T>() -> u32 {
    std::mem::offset_of!(CF_OPERATION_PARAMETERS, Anonymous) as u32
        + std::mem::size_of::<T>() as u32
}

// ---------------------------------------------------------------------------
// 小工具
// ---------------------------------------------------------------------------

fn decode_identity(info: &CF_CALLBACK_INFO) -> Option<identity::Identity> {
    if info.FileIdentity.is_null() || info.FileIdentityLength == 0 {
        // 路径一定要打出来：这种失败几乎总是"平台在问一个我们自己建出来的普通目录"
        // （镜像的父目录链），光有 file_id 只能看到"同一个 id 反复失败"。
        warn!(
            file_id = info.FileId, path = %win_path(info), size = info.FileSize,
            "回调没带身份 blob"
        );
        return None;
    }
    let bytes = unsafe {
        std::slice::from_raw_parts(
            info.FileIdentity as *const u8,
            info.FileIdentityLength as usize,
        )
    };
    match identity::Identity::decode(bytes) {
        Ok(id) => Some(id),
        Err(e) => {
            warn!(file_id = info.FileId, error = %e, "身份解析失败");
            None
        }
    }
}

/// 回调给的绝对路径（`\Device\HarddiskVolumeN\...`）换成 Win32 形式（`C:\...`）。
///
/// 幂等记账（`mirror::note_closed`）与 `mirror::sweep` 都按 Win32 路径比对，所以这一步必须做对。
fn win_path(info: &CF_CALLBACK_INFO) -> String {
    dos_path(&pcwstr(info.VolumeDosName), &pcwstr(info.NormalizedPath))
}

/// 回调给的绝对路径换成 Win32 形式（`C:\...`）。
///
/// `NormalizedPath` 实测有**两种形态**，不能只按其中一种切：
///
/// 1. **卷相对**：`\Users\me\AppData\...`（本机 Win11 就是这种）—— 直接拼上 `VolumeDosName`
///    就是 `C:\Users\me\AppData\...`。
/// 2. `\Device\HarddiskVolumeN\Users\me\...` —— 去掉 `\Device\HarddiskVolumeN` 再拼。
///
/// 旧实现固定按第 2 种切 3 段。在形态 1 上它把 `Users\me` 当成了设备前缀一起切掉，
/// 得到 `C:\AppData\...` —— 记账键与 `sweep`/`is_busy` 的路径**永远对不上**：
/// 打开/关闭回调记了个不存在的路径，回收判定只能退回文件 mtime（对端的修改时间，可能几天前），
/// 于是正在被资源管理器复制的那一项也会被删掉。实机症状就是"源文件已损坏或不可用"、
/// 之后拖放不再响应。
///
/// 幂等记账（`note_closed`）与 `sweep` 都按 Win32 路径比对，所以这一步必须做对。
fn dos_path(dos: &str, normalized: &str) -> String {
    if let Some(rest) = strip_device_prefix(normalized) {
        return if rest.is_empty() {
            format!("{dos}\\")
        } else {
            format!("{dos}\\{rest}")
        };
    }
    // 卷相对路径（一个前导 `\`，但不是 UNC `\\server`、也不是 `\??\` 前缀）。
    if normalized.starts_with('\\')
        && !normalized.starts_with("\\\\")
        && !normalized.starts_with("\\??\\")
    {
        return format!("{dos}{normalized}");
    }
    // 已经是 DOS/UNC/verbatim 形式，原样交回。
    normalized.to_string()
}

/// `\Device\HarddiskVolumeN\...` → `...`；不是这个形态返回 `None`。
fn strip_device_prefix(path: &str) -> Option<&str> {
    const PREFIX: &str = "\\Device\\";
    let head = path.get(..PREFIX.len())?;
    if !head.eq_ignore_ascii_case(PREFIX) {
        return None;
    }
    let rest = &path[PREFIX.len()..];
    let (volume, rest) = rest.split_once('\\').unwrap_or((rest, ""));
    if !volume.to_ascii_lowercase().starts_with("harddiskvolume") {
        return None;
    }
    Some(rest)
}

/// 触发这次回调的进程（连接时开了 `REQUIRE_PROCESS_INFO`）—— 验证期靠它分"谁读了镜像"。
fn process_name(info: &CF_CALLBACK_INFO) -> String {
    if info.ProcessInfo.is_null() {
        return String::new();
    }
    let image = pcwstr(unsafe { (*info.ProcessInfo).ImagePath });
    image
        .rsplit(['\\', '/'])
        .next()
        .unwrap_or_default()
        .to_string()
}

/// 这次回调是不是本进程自己发的（本进程自己的读、测试里的读）。
///
/// 它们不算"目标在读"：记进账会让刚被本进程读过的项在 grace 内被误判为 busy，
/// 回收判据也会被自己拖着走。
fn is_self(info: &CF_CALLBACK_INFO) -> bool {
    let process = process_name(info);
    if process.is_empty() {
        return false; // 判断不了就按"目标"算（宁可少回收，不能删错）
    }
    std::env::current_exe()
        .ok()
        .and_then(|exe| {
            exe.file_name()
                .map(|name| name.to_string_lossy().to_string())
        })
        .is_some_and(|exe| exe.eq_ignore_ascii_case(&process))
}

fn pcwstr(text: PCWSTR) -> String {
    if text.is_null() {
        return String::new();
    }
    unsafe { text.to_string().unwrap_or_default() }
}

fn to_wide(s: &OsStr) -> Vec<u16> {
    s.encode_wide().chain(std::iter::once(0)).collect()
}

fn align_up(value: u64) -> u64 {
    value.div_ceil(ALIGN) * ALIGN
}

fn fs_metadata(size: u64, mtime: Option<i64>, is_dir: bool) -> CF_FS_METADATA {
    // 时间戳拿不到就用"现在"：占位符总要有个值，写 0 会显示成 1601 年。
    //
    // **CreationTime 必须是本地现在**：它是"这一项什么时候放进本地镜像"的唯一持久记录
    // （进程重启后 TTL 兜底就靠它）。对端的修改时间只能写进 LastWriteTime（资源管理器里
    // 显示为"修改日期"）。旧实现把 CreationTime 也写成远端时间，于是几十天前的远端文件
    // 一放进来就"超龄"，下一次 sweep 直接删掉 —— 正在复制的那一项也一样。
    let now = now_filetime();
    let modified = mtime.map(unix_to_filetime).unwrap_or(now);
    CF_FS_METADATA {
        BasicInfo: FILE_BASIC_INFO {
            CreationTime: now,
            LastAccessTime: now,
            LastWriteTime: modified,
            ChangeTime: now,
            FileAttributes: if is_dir {
                FILE_ATTRIBUTE_DIRECTORY.0
            } else {
                FILE_ATTRIBUTE_NORMAL.0
            },
        },
        // 目录的大小由平台忽略。
        FileSize: if is_dir { 0 } else { size as i64 },
    }
}

/// Unix 秒 → FILETIME 刻度（1601-01-01 起的 100ns）。
fn unix_to_filetime(secs: i64) -> i64 {
    const EPOCH_DIFF: i64 = 11_644_473_600;
    secs.saturating_add(EPOCH_DIFF).saturating_mul(10_000_000)
}

fn now_filetime() -> i64 {
    unix_to_filetime(chrono::Utc::now().timestamp())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 名字里的本地名与身份 blob 必须**一条对一条**：老实现把名字、身份分成两个数组各自
    /// 过滤再按下标取，丢一条就整体错位（A 的名字配 B 的身份 = 目标按错的身份去拉另一个文件
    /// 的字节）。这里直接盯住"第 i 条的 blob 解出来就是第 i 条那个远端路径"。
    #[test]
    fn prepared_children_stay_aligned_with_their_own_identity() {
        let parent = identity::Identity {
            peer_id: "device-a".into(),
            share_id: "share-1".into(),
            path: "sub".into(),
            is_dir: true,
        };
        let entry = |name: &str, is_dir: bool, size: u64| peer_fs::TreeEntry {
            name: name.into(),
            is_dir,
            size,
            modified: "2026-09-26T00:00:00Z".into(),
        };
        // 中间夹两条本机表示不了的（`.` 与空名），再加一条超长远端路径（身份编码会失败）。
        let long = "x".repeat(identity::IDENTITY_MAX);
        let entries = vec![
            entry("a.txt", false, 11),
            entry(".", false, 0),
            entry("目录", true, 0),
            entry("", false, 0),
            entry(&long, false, 5),
            entry("z.bin", false, 33),
        ];

        let (prepared, dropped) = prepare_children(&parent, &entries);
        assert_eq!(dropped, 3, "`.`、空名、超长路径各算一条备不齐");
        assert_eq!(prepared.len(), 3, "能备齐的三条要留下来");

        let pulled: Vec<(String, u64, bool)> = prepared
            .iter()
            .map(|(name, blob, size, _mtime, is_dir)| {
                let decoded = identity::Identity::decode(blob).expect("身份必须解得回来");
                let local = String::from_utf16_lossy(name)
                    .trim_end_matches('\0')
                    .to_string();
                // 本地名是清洗过的、远端路径是原始的 —— 两者都得是**这一条**自己的。
                let remote_leaf = decoded
                    .path
                    .rsplit('/')
                    .next()
                    .unwrap_or_default()
                    .to_string();
                (format!("{local}|{remote_leaf}"), *size, *is_dir)
            })
            .collect();
        assert_eq!(
            pulled,
            vec![
                ("a.txt|a.txt".to_string(), 11, false),
                ("目录|目录".to_string(), 0, true),
                ("z.bin|z.bin".to_string(), 33, false),
            ],
            "每一条的本地名与身份必须配得上"
        );
        assert!(
            prepared.iter().all(|(_, blob, ..)| !blob.is_empty()),
            "身份 blob 不能是空的（空 blob 会让平台按空身份去拉）"
        );
    }

    /// 撤销必须**只作废它撤销的那一次请求**，绝不外溢成"这个偏移以后都别拉"。
    ///
    /// 这就是实机那次卡住的根因（日志：`开始水合 start=4096` → `平台撤销了这段读` → 又一次
    /// `开始水合 start=4096` → 立刻"停止拉取"、一个字节都没交）。老实现把撤销记成永久的偏移
    /// 黑名单，平台**重发**的同一页被上一张旧单子静默吞掉：既不交数据也不报复，那个读就一直
    /// 挂在平台上 → 目标进程里复制进度不动、`DoDragDrop` 也不返回。
    ///
    /// （这条测试碰的是进程内的登记表；`mod tests` 里只有它用它。）
    #[test]
    fn a_cancel_only_kills_the_request_it_cancels() {
        const FILE: i64 = 77;

        // 在飞的请求被撤销 → 它自己停手（这就是撤销的本意）。
        let first = FetchGuard::begin(FILE, 4096, 8192);
        note_cancel(FILE, 4096, 8192);
        assert!(first.cancelled(), "被撤销的那次请求必须知道");
        drop(first);

        // 平台随后重发同一区间（另一处读要它）→ 必须照常拉：旧单子已经被消费掉了。
        let second = FetchGuard::begin(FILE, 4096, 8192);
        assert!(
            !second.cancelled(),
            "新请求不能被上一张已经消费掉的撤销单拦住"
        );

        // 撤销只认自己那个 placeholder：别的 file_id 的同区间请求不受影响。
        note_cancel(FILE, 0, 4096);
        assert!(!second.cancelled(), "别的文件的撤销不该撤到这一条");
        drop(second);

        // 同文件、区间有重叠的在飞请求：认领（撤销报的是区间，不是精确边界）。
        let overlapping = FetchGuard::begin(FILE, 0, 8192);
        note_cancel(FILE, 4096, 8192);
        assert!(overlapping.cancelled(), "区间重叠的在飞请求就该被撤到");
        drop(overlapping);

        // 没有在飞请求时的撤销（那一段早就交付完了）不该留下任何后患。
        note_cancel(FILE, 12288, 16384);
        let after = FetchGuard::begin(FILE, 12288, 16384);
        assert!(!after.cancelled(), "没人认领的撤销单不能留在表里");
    }

    /// 同一区间上同时有两条在飞请求（落下时解析路径的那一页 + 真正的复制）：撤销只属于
    /// **先发起**的那条，后来的复制请求绝不能被连坐。
    ///
    /// 老实现把区间重叠的**所有**请求都标成撤销：复制请求既不交付也不判失败，平台那个读
    /// 一直挂着（其它操作还会重置它的超时计时器）—— 实机表现就是"源文件已损坏或不可用"、
    /// 之后拖放整个不再响应。
    #[test]
    fn a_cancel_does_not_kill_an_overlapping_live_request() {
        const FILE: i64 = 78;
        let header = FetchGuard::begin(FILE, 0, 4096);
        let copy = FetchGuard::begin(FILE, 0, 4096);
        note_cancel(FILE, 0, 4096);
        assert!(header.cancelled(), "撤销应当认领先发起的那条");
        assert!(!copy.cancelled(), "后发起的复制请求不能被连坐");
    }

    /// 重名退路：扩展名要留住，用户一眼还能认出是哪个文件。
    #[test]
    fn suffixed_names_keep_the_extension() {
        assert_eq!(suffixed_name("报告.pdf", 2), "报告 (2).pdf");
        assert_eq!(suffixed_name(".gitignore", 2), ".gitignore (2)");
        assert_eq!(suffixed_name("noext", 3), "noext (3)");
    }

    /// 回调路径的两种形态都要落到同一个 Win32 路径。旧实现只认 `\Device\HarddiskVolumeN\...`，
    /// 在卷相对形态（实机 Win11 的那个）上把用户目录也切掉了 —— 记账键永远对不上，
    /// 回收就会删正在被读的项。
    #[test]
    fn callback_paths_match_the_sweep_path() {
        assert_eq!(
            dos_path("C:", r"\Users\me\AppData\Local\Temp\orbdrop\mirror\a.bin"),
            r"C:\Users\me\AppData\Local\Temp\orbdrop\mirror\a.bin"
        );
        assert_eq!(
            dos_path("C:", r"\Device\HarddiskVolume3\Users\me\mirror\a.bin"),
            r"C:\Users\me\mirror\a.bin"
        );
        assert_eq!(dos_path("C:", r"\Device\HarddiskVolume3"), r"C:\");
        assert_eq!(dos_path("D:", r"\\nas\share\a.bin"), r"\\nas\share\a.bin");
        assert_eq!(
            dos_path("C:", r"C:\already\dos\a.bin"),
            r"C:\already\dos\a.bin"
        );
    }

    /// 占位符的 LastWriteTime = 对端修改时间，但 CreationTime 必须是**本地现在** ——
    /// 回收的 TTL 兜底就看它，写远端时间会让刚放好的项当场"超龄"。
    #[test]
    fn placeholder_creation_time_is_local_while_mtime_is_remote() {
        let remote_secs = 1_000_000_000; // 2001 年，肯定比 TTL 老
        let meta = fs_metadata(10, Some(remote_secs), false);
        assert_eq!(meta.BasicInfo.LastWriteTime, unix_to_filetime(remote_secs));
        let now = now_filetime();
        assert!(
            (meta.BasicInfo.CreationTime - now).abs() < 5_000_000,
            "创建时间必须是本地现在（误差 0.5 秒内）"
        );
    }

    #[test]
    fn data_ranges_are_4k_aligned_unless_they_end_at_eof() {
        // 起点向上取整的语义：起点向下、终点向上（多给一点是允许的）
        assert_eq!(align_up(4096), 4096);
        assert_eq!(align_up(1), 4096);
        assert_eq!(align_up(4097), 8192);
        assert_eq!(5000u64 & !(ALIGN - 1), 4096);
    }

    #[test]
    fn param_size_matches_the_documented_rule() {
        // 文档：ParamSize = 子结构偏移 + 子结构大小。
        // 偏移是 8（`ParamSize` 4 字节 + 4 字节填充，联合体要 8 字节对齐）。
        let offset = std::mem::offset_of!(CF_OPERATION_PARAMETERS, Anonymous);
        assert_eq!(offset, 8);
        // 两个子结构都是 32 字节（Flags/CompletionStatus 8 + 载荷 24），所以 ParamSize 都是 40。
        assert_eq!(std::mem::size_of::<CF_OPERATION_PARAMETERS_0_0>(), 32);
        assert_eq!(std::mem::size_of::<CF_OPERATION_PARAMETERS_0_4>(), 32);
        assert_eq!(param_size::<CF_OPERATION_PARAMETERS_0_0>(), 40);
        assert_eq!(param_size::<CF_OPERATION_PARAMETERS_0_4>(), 40);
    }

    #[test]
    fn unix_time_converts_to_filetime_ticks() {
        // 1970-01-01 应当是正值（1601 起点），且 1 秒 = 1000 万个刻度
        assert_eq!(unix_to_filetime(0), 116_444_736_000_000_000);
        assert_eq!(unix_to_filetime(1) - unix_to_filetime(0), 10_000_000);
    }
}
