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

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};
use std::sync::{Arc, LazyLock, Mutex, OnceLock};

use tracing::{info, warn};
use windows::core::{GUID, PCWSTR};
use windows::Win32::Foundation::{STATUS_CLOUD_FILE_UNSUCCESSFUL, STATUS_SUCCESS};
use windows::Win32::Storage::CloudFilters::*;
use windows::Win32::Storage::FileSystem::{
    FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL, FILE_BASIC_INFO,
};

use super::{identity, note_closed, note_opened, MirroredItem};
use crate::peer_fs::{self, PeerFile, READ_CHUNK};
use crate::state::AppState;

/// 平台要求的对齐：交付数据的 offset/length 都是 4 KiB 的倍数（结束在文件末尾的那段除外）。
const ALIGN: u64 = 4096;

/// OrbDrop 的 provider 身份。同一个 GUID 让平台把不同版本认成同一个提供程序（遥测用）。
const PROVIDER_ID: GUID = GUID::from_u128(0x7f3c1e9a_2d5b_4a86_9f14_6c8b0a5d2e37);

/// 回调线程要用到的东西：对端访问要 `AppState`，异步要**应用自己的**运行时。
struct Provider {
    state: Arc<AppState>,
    /// 连接建立时抓下来的运行时：回调在系统线程池线程上跑，那里问不到"当前运行时"。
    rt: tokio::runtime::Handle,
    /// 水合用的 HTTP 客户端：**建一次、全程复用**。
    ///
    /// 绝不能每个区间回调都 `peer_fs::client()` —— 那等于新建一个 reqwest 客户端，每次都要
    /// 重新 TLS 握手。真机实测（千兆内网、资源管理器按 1 MiB 一段读）：725 个区间，每段中位
    /// **41 ms**（≈24 MiB/s），而带宽本身够跑 100 MiB/s —— 差额全在握手与新建客户端上。
    /// 复用之后每段只剩一个往返。
    client: reqwest::Client,
}

/// 进程内唯一的 provider。回调只有函数指针可用，拿不到闭包，所以上下文走全局。
///
/// `Box::leak` 是有意的：连接活到进程结束，回收这点内存没有意义，而"连接期间指针必须有效"
/// 是平台的硬要求。
static PROVIDER: OnceLock<&'static Provider> = OnceLock::new();

/// 平台的连接句柄。`CF_CONNECTION_KEY` 是 `i64` 的透明包装，`Copy`、没有 Drop
/// （显式 `Free` 才会断开），所以在全局里存一份是安全的。
static CONNECTION: OnceLock<CF_CONNECTION_KEY> = OnceLock::new();

/// 已经不再需要的区间（目标撤了这次读）。拉取循环每次交付前看一眼，别白拉。
static CANCELLED: LazyLock<Mutex<Vec<Cancelled>>> = LazyLock::new(|| Mutex::new(Vec::new()));

struct Cancelled {
    file_id: i64,
    start: u64,
    end: u64,
}

/// 回调表必须以 `CF_CALLBACK_REGISTRATION_END`（= `{ CF_CALLBACK_TYPE_NONE, NULL }`）收尾；
/// 这个哨兵在 `windows` crate 里没有常量，得自己写出来。
///
/// 只注册我们确实实现的：**不注册 `NOTIFY_DELETE` / `NOTIFY_RENAME`** —— 镜像只读单向，
/// 用户/别的程序在镜像里删改东西我们不需要回写对端（见模块头）。
static CALLBACKS: [CF_CALLBACK_REGISTRATION; 6] = [
    CF_CALLBACK_REGISTRATION { Type: CF_CALLBACK_TYPE_FETCH_DATA, Callback: Some(fetch_data_cb) },
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
    CF_CALLBACK_REGISTRATION { Type: CF_CALLBACK_TYPE_NONE, Callback: None },
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
    std::fs::create_dir_all(&root).map_err(|e| format!("建镜像目录失败（{}）: {e}", root.display()))?;
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
        let client = peer_fs::client();
        info!("水合用的 HTTP 客户端已建（后续所有区间复用同一条 TLS 连接）");
        Box::leak(Box::new(Provider { state, rt, client }))
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
        unsafe { CfRegisterSyncRoot(PCWSTR(wide_root.as_ptr()), &registration, &policies, flags | CF_REGISTER_FLAG_UPDATE) }
            .map_err(|e| format!("刷新同步根注册失败: {e:?}"))
    } else {
        unsafe { CfRegisterSyncRoot(PCWSTR(wide_root.as_ptr()), &registration, &policies, flags) }
            .or_else(|first| {
                warn!(error = ?first, "首次注册同步根失败，改用 UPDATE 重注册");
                unsafe {
                    CfRegisterSyncRoot(PCWSTR(wide_root.as_ptr()), &registration, &policies, flags | CF_REGISTER_FLAG_UPDATE)
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
    let dir = identity::share_dir(&super::root(), &item.peer_id, &item.share_id);
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| format!("建镜像目录失败（{}）: {e}", dir.display()))?;
    let local = identity::child_path(&dir, &item.name)?;

    // 占位符必须报**真实大小**，否则目标会拷出 0 字节。顺带把 `is_dir` 校正过来：界面的判断
    // 来自对端 `/shares` 的旧值，可能过时 —— 与旧路径同一条判据（元数据说了算）。
    let client = peer_fs::client();
    let (base, token) = peer_fs::coords(state, &item.peer_id)?;
    let meta = peer_fs::probe_entry(&client, &base, &token, &item.share_id, &item.path).await?;
    let is_dir = meta.map(|m| m.is_dir).unwrap_or(item.is_dir);
    let (size, mtime) = if is_dir {
        (0, None)
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
    let parent = local.parent().ok_or_else(|| "镜像落点没有父目录".to_string())?.to_path_buf();

    create_placeholder(&parent, &name, &blob, size, mtime, is_dir)?;
    Ok(local)
}

/// 在 `dir` 下建一个占位符。**幂等**：同名先清掉。
///
/// 为什么必须幂等：我们不注册删除通知，所以"用户在镜像里删过东西""上次拖同一项留下的过期占位符"
/// 我们都不知道 —— 不先清同名，`CfCreatePlaceholders` 会直接失败。
fn create_placeholder(
    dir: &Path,
    name: &str,
    blob: &[u8],
    size: u64,
    mtime: Option<i64>,
    is_dir: bool,
) -> Result<(), String> {
    let target = dir.join(name);
    if target.exists() {
        let removed = if target.is_dir() {
            std::fs::remove_dir_all(&target)
        } else {
            std::fs::remove_file(&target)
        };
        removed.map_err(|e| format!("清理旧的镜像项失败（{}）: {e}", target.display()))?;
    }

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
    unsafe { CfCreatePlaceholders(PCWSTR(dir_w.as_ptr()), &mut array, CF_CREATE_FLAG_NONE, None) }
        .map_err(|e| format!("建占位符失败（{}）: {e:?}", target.display()))?;
    // 逐条结果也要看：整体 OK 而单条失败是踩过的坑（目录不带身份就是那样）。
    array[0]
        .Result
        .ok()
        .map_err(|e| format!("建占位符失败（{}）: {e:?}", target.display()))
}

// ---------------------------------------------------------------------------
// 回调：水合、目录懒填充、取消、开关句柄
// ---------------------------------------------------------------------------

unsafe extern "system" fn fetch_data_cb(
    info: *const CF_CALLBACK_INFO,
    params: *const CF_CALLBACK_PARAMETERS,
) {
    if let (Some(info), Some(params)) = (unsafe { info.as_ref() }, unsafe { params.as_ref() }) {
        fetch_data(info, params);
    }
}

unsafe extern "system" fn fetch_placeholders_cb(
    info: *const CF_CALLBACK_INFO,
    params: *const CF_CALLBACK_PARAMETERS,
) {
    if let (Some(info), Some(params)) = (unsafe { info.as_ref() }, unsafe { params.as_ref() }) {
        fetch_placeholders(info, params);
    }
}

unsafe extern "system" fn cancel_fetch_data_cb(
    info: *const CF_CALLBACK_INFO,
    params: *const CF_CALLBACK_PARAMETERS,
) {
    if let (Some(info), Some(params)) = (unsafe { info.as_ref() }, unsafe { params.as_ref() }) {
        let range = unsafe { params.Anonymous.Cancel.Anonymous.FetchData };
        let start = range.FileOffset.max(0) as u64;
        let end = start.saturating_add(range.Length.max(0) as u64);
        info!(file_id = info.FileId, start, end, "目标撤销了这段读，后续不再拉它");
        let mut cancelled = CANCELLED.lock().unwrap();
        // 只留最近的：撤销是"当前这次读"的事，堆着没有意义。
        if cancelled.len() > 256 {
            cancelled.drain(..128);
        }
        cancelled.push(Cancelled { file_id: info.FileId, start, end });
    }
}

unsafe extern "system" fn open_completion_cb(
    info: *const CF_CALLBACK_INFO,
    _params: *const CF_CALLBACK_PARAMETERS,
) {
    if let Some(info) = unsafe { info.as_ref() } {
        let path = win_path(info);
        info!(path = %path, process = %process_name(info), "镜像项被打开");
        note_opened(&path);
    }
}

unsafe extern "system" fn close_completion_cb(
    info: *const CF_CALLBACK_INFO,
    params: *const CF_CALLBACK_PARAMETERS,
) {
    if let (Some(info), Some(params)) = (unsafe { info.as_ref() }, unsafe { params.as_ref() }) {
        let flags = unsafe { params.Anonymous.CloseCompletion.Flags };
        let path = win_path(info);
        info!(
            path = %path, deleted = flags.contains(CF_CALLBACK_CLOSE_COMPLETION_FLAG_DELETED),
            process = %process_name(info),
            "镜像项已关闭（此后可回收）"
        );
        // 记账：`mirror::sweep` 会在这之后按 grace 回收。删除型关闭不必记账 —— 文件已经没了。
        if !flags.contains(CF_CALLBACK_CLOSE_COMPLETION_FLAG_DELETED) {
            note_closed(&path);
        }
    }
}

/// 目标要读一段字节：按需从对端拉，交给平台。
fn fetch_data(info: &CF_CALLBACK_INFO, params: &CF_CALLBACK_PARAMETERS) {
    let Some(provider) = PROVIDER.get() else {
        // 没连上（理论上不会走到这里；连上了才会收到回调）。不回报 = 平台的读会超时失败，
        // 这比"回报成功但没数据"安全。
        warn!("收到取数据回调，但 provider 没有连接");
        return;
    };

    let fetch = unsafe { params.Anonymous.FetchData };
    let size = info.FileSize.max(0) as u64;
    let required_start = fetch.RequiredFileOffset.max(0) as u64;
    let required_end = match fetch.RequiredLength {
        len if len < 0 => size, // 负值 = CF_EOF：一直到文件末尾
        len => required_start.saturating_add(len as u64).min(size),
    };

    // 对齐：起点向下、终点向上（可以多给，文档明确允许），但结束在文件末尾的那一段不要求对齐。
    let start = required_start & !(ALIGN - 1);
    let end = if required_end >= size { size } else { align_up(required_end) };

    let Some(id) = decode_identity(info) else {
        fail_range(info, start, end.saturating_sub(start), "身份解不出来");
        return;
    };
    if end <= start {
        // 空区间没有可交付的字节；报失败让平台重来，比静默成功安全。
        fail_range(info, start, 0, "请求区间为空");
        return;
    }

    info!(path = %id.path, start, end, size, resume = fetch.Flags.contains(CF_CALLBACK_FETCH_DATA_FLAG_RECOVERY), "开始水合");
    match transfer_range(provider, info, &id, start, end) {
        Ok(()) => {}
        Err(e) => {
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
) -> Result<(), String> {
    let began = std::time::Instant::now();
    let client = provider.client.clone();
    let (base, token) = peer_fs::coords(&provider.state, &id.peer_id)?;
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
        if is_cancelled(info.FileId, offset) {
            // 目标已经不要这一段了：停手但不算失败。
            info!(file_id = info.FileId, offset, "这段读已被撤销，停止拉取");
            return Ok(());
        }
        let want = READ_CHUNK.min(end - offset);
        // `run_blocking` 要求 future 是 'static：把客户端与坐标移进去（都是廉价的克隆）。
        let (task_client, task_file) = (client.clone(), file.clone());
        let bytes = crate::task::run_blocking_on(&provider.rt, async move {
            peer_fs::fetch_file_range(&task_client, &task_file, offset, want).await
        })
        .ok_or_else(|| "取数据任务没能回到运行时".to_string())??;
        if bytes.is_empty() {
            return Err(format!("对端「{}」在 {offset} 处读到 0 字节", id.path));
        }
        // 平台要求 offset/length 4 KiB 对齐，**除非**这一段结束在文件末尾。对端那边文件变小了
        // （远端被改过）会让这条不成立 —— 那种情况必须报错，不能交一段错位的字节。
        let length = bytes.len() as u64;
        let ends_at_eof = offset + length >= (info.FileSize.max(0) as u64);
        if !ends_at_eof && (offset % ALIGN != 0 || length % ALIGN != 0) {
            return Err(format!(
                "对端返回的区间没有 4 KiB 对齐（offset={offset} len={length}），又不落在文件末尾"
            ));
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

fn is_cancelled(file_id: i64, offset: u64) -> bool {
    CANCELLED
        .lock()
        .unwrap()
        .iter()
        .any(|c| c.file_id == file_id && offset >= c.start && offset < c.end)
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
        warn!("收到填充目录回调，但 provider 没有连接");
        return;
    };
    let Some(id) = decode_identity(info) else {
        fail_placeholders(info, "身份解不出来");
        return;
    };
    let path = id.path.clone();
    let (peer_id, share_id, remote_path) = (id.peer_id.clone(), id.share_id.clone(), id.path.clone());
    let entries = crate::task::run_blocking_on(&provider.rt, async move {
        let client = provider.client.clone();
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

    // 两趟：先把名字与身份 blob 建好，再取指针 —— 边 push 边取指针会在扩容时悬空。
    let names: Vec<String> = entries
        .iter()
        .filter_map(|e| peer_fs::plain_component(&e.name).map(|clean| clean))
        .collect();
    let blobs: Vec<Vec<u8>> = entries
        .iter()
        .filter_map(|e| {
            identity::Identity {
                peer_id: id.peer_id.clone(),
                share_id: id.share_id.clone(),
                // 远端路径必须用**原始**名字（对端按它找文件），本地名字才用清洗过的。
                path: peer_fs::join_slash(&id.path, &e.name),
                is_dir: e.is_dir,
            }
            .encode()
            .ok()
        })
        .collect();
    if names.len() != entries.len() || blobs.len() != entries.len() {
        warn!(path = %path, "有条目的名字本机用不了或身份过长，这一层只填充能填的");
    }

    // 指针数组要保活到 `CfExecute` 返回。
    let wide_names: Vec<Vec<u16>> = names.iter().map(|n| to_wide(OsStr::new(n))).collect();
    let entries_used = names.len();
    let (names, blobs, entries) = (&wide_names, &blobs, &entries);
    let mut array: Vec<CF_PLACEHOLDER_CREATE_INFO> = (0..entries_used)
        .map(|i| {
            let entry = &entries[i];
            let mut ci = CF_PLACEHOLDER_CREATE_INFO::default();
            ci.RelativeFileName = PCWSTR(names[i].as_ptr());
            ci.FsMetadata = fs_metadata(
                entry.size,
                peer_fs::parse_rfc3339_secs(&entry.modified),
                entry.is_dir,
            );
            ci.FileIdentity = blobs[i].as_ptr() as *const core::ffi::c_void;
            ci.FileIdentityLength = blobs[i].len() as u32;
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
        warn!(path = %path, error = ?e, "交回目录内容失败");
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
) -> Result<(), String> {
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
    unsafe { CfExecute(&op_info, &mut params) }.map_err(|e| format!("交付数据失败: {e:?}"))
}

/// 让挂在这个区间上的读**明确失败**。
///
/// 必须用 `STATUS_CLOUD_FILE_*` 里的状态：这个范围之外的状态会被平台一律折成
/// `STATUS_CLOUD_FILE_UNSUCCESSFUL`。绝不报成功 —— 那会让目标拷出一份静默残缺的文件。
fn fail_range(info: &CF_CALLBACK_INFO, offset: u64, length: u64, why: &str) {
    warn!(file_id = info.FileId, offset, length, why, "把这一段读判失败");
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
    let _ = unsafe { CfReportProviderProgress(info.ConnectionKey, info.TransferKey, total, completed) };
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
        std::slice::from_raw_parts(info.FileIdentity as *const u8, info.FileIdentityLength as usize)
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
    let dos = pcwstr(info.VolumeDosName);
    let normalized = pcwstr(info.NormalizedPath);
    let rest: Vec<&str> = normalized.split('\\').skip(3).collect(); // 丢掉 `\Device\HarddiskVolumeN`
    if rest.is_empty() {
        return normalized;
    }
    format!("{dos}\\{}", rest.join("\\"))
}

/// 触发这次回调的进程（连接时开了 `REQUIRE_PROCESS_INFO`）—— 验证期靠它分"谁读了镜像"。
fn process_name(info: &CF_CALLBACK_INFO) -> String {
    if info.ProcessInfo.is_null() {
        return String::new();
    }
    let image = pcwstr(unsafe { (*info.ProcessInfo).ImagePath });
    image.rsplit(['\\', '/']).next().unwrap_or_default().to_string()
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
    let ticks = mtime.map(unix_to_filetime).unwrap_or_else(now_filetime);
    CF_FS_METADATA {
        BasicInfo: FILE_BASIC_INFO {
            CreationTime: ticks,
            LastAccessTime: ticks,
            LastWriteTime: ticks,
            ChangeTime: ticks,
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