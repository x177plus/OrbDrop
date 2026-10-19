#[cfg(desktop)]
pub mod autostart;
// bc 不能门控：pair.rs / server::proxy / routes_pair.rs 这些共享协议路径也用它的
// DAV_USER 常量与 profile 预写（后者在非 Win/mac 上因 locate_exe 返回 None 而自然空转）。
pub mod bc;
/// 会话里图片消息的**内容**（`<数据目录>/chat-blobs/`）：消息本体只留元数据，字节单独一份文件。
pub mod blobs;
pub mod chat;
pub mod config;
pub mod credential;
pub mod discover;
pub mod drag;
#[cfg(desktop)]
pub mod fs_browse;
pub mod group;
#[cfg(desktop)]
pub mod hit_region;
pub mod link;
pub mod log;
pub mod mirror;
// 移动端专属命令（收件目录列举/删除）：只在 Android 构建里登记。
#[cfg(mobile)]
pub mod mobile;
pub mod net;
// 收到消息时手机通知栏里那一条（F-108）：桌面端是空操作，模块本身两平台都有
// （收消息那条路是共用的，调用点不该被 `cfg` 切成两半）。
pub mod notify;
#[cfg(desktop)]
pub mod orb_window;
pub mod pair;
/// 粘贴进来的图片：字节 → 暂存文件（会话里"发图片"走推送那条路，只认本机真实路径）。
pub mod paste;
pub mod peer_fs;
pub mod relay;
pub mod server;
pub mod share;
pub mod state;
/// IPv4 网段（CIDR）判定与自报（F-107 §5）。
pub mod subnet;
pub mod system_settings;
pub mod task;
#[cfg(desktop)]
pub mod tray;
pub mod transfer;
pub mod unsendable;
pub mod update;
pub mod ws;

#[cfg(test)]
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State};
#[cfg(desktop)]
use tauri::WebviewWindow;
use tauri_plugin_opener::OpenerExt;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use config::{AppSettings, DownloadMode, OrbShape, PeerDevice, ShareItem};
#[cfg(desktop)]
use config::{BcTarget, OrbPos, PickerFavorite};
use state::{AppState, LinkIntent};

// ---------------------------------------------------------------------------
// Tauri command payloads
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ShareItemDto {
    pub id: String,
    pub name: String,
    pub path: String,
    pub created_at: String,
    pub broadcast_enabled: bool,
    /// 真去问文件系统，不按路径猜 —— 老前端那条 `shareIsDir`（"没有扩展名就算目录"）
    /// 会把 `D:\MyFolder.v2` 当文件，图标与展开箭头跟着错。
    pub is_dir: bool,
    /// 文件大小；目录永远是 `None`（那是递归遍历，不在列清单这条路上做）。
    pub size: Option<u64>,
    /// 目录的**直接**子项数；文件永远是 `None`。卡片副标题就显示它。
    pub item_count: Option<u64>,
}

/// 共享项在文件系统上的样子。卡片副标题那两个数（「N 项」/ 大小）只能问文件系统，
/// 所以一次问清、一起带出去。
#[derive(Debug, Clone, Copy)]
struct ShareProbe {
    is_dir: bool,
    size: Option<u64>,
    item_count: Option<u64>,
}

/// 问一次文件系统（同步；调用方负责放到阻塞线程上）。
fn probe_share(path: &Path) -> ShareProbe {
    let Ok(meta) = std::fs::metadata(path) else {
        // 被删 / 无权限：连类型都不猜，两个数都给 `None`，界面退回「文件」这种中性说法。
        return ShareProbe { is_dir: false, size: None, item_count: None };
    };
    if meta.is_dir() {
        ShareProbe { is_dir: true, size: None, item_count: count_children(path) }
    } else {
        ShareProbe { is_dir: false, size: Some(meta.len()), item_count: None }
    }
}

/// 目录的直接子项数。**跳过 `.` 开头的项**：展开共享项走的是 `/tree`，那里也只有这一条过滤
/// —— 两个数对不上，用户会以为列表漏了东西。
///
/// 中途读错（权限、掉盘）返回 `None`：少一个数比报一个错的数好（同 `fs_browse::list_dir`
/// "静默少几项比报错更坏" 的另一面 —— 这里能安全地说"不知道"）。
fn count_children(path: &Path) -> Option<u64> {
    let dir = std::fs::read_dir(path).ok()?;
    let mut n = 0u64;
    for entry in dir {
        let Ok(entry) = entry else { return None };
        if !entry.file_name().to_string_lossy().starts_with('.') {
            n += 1;
        }
    }
    Some(n)
}

impl ShareItemDto {
    fn new(s: &ShareItem, probe: ShareProbe) -> Self {
        Self {
            id: s.id.clone(),
            name: s.name.clone(),
            path: s.path.to_string_lossy().to_string(),
            created_at: s.created_at.to_rfc3339(),
            broadcast_enabled: s.broadcast_enabled,
            is_dir: probe.is_dir,
            size: probe.size,
            item_count: probe.item_count,
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToggleBroadcastRequest {
    pub share_id: String,
    pub enabled: bool,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateSettingsRequest {
    pub port: u16,
    /// 唯一的落盘目录：手动下载与推送接收都写这里（见 `config::Config::download_dir`）。
    pub download_dir: String,
    /// 与前端 `AppSettings.downloadMode` 同一套字面量（`Direct` / `AskEachTime`）。
    pub download_mode: DownloadMode,
    pub device_name: String,
    pub auto_start: bool,
    /// 推送接收是否再放一层 `<发送方名>/`（设置里的「推送按发送方分目录」，默认开，S-12）。
    /// 缺省用配置的缺省（**开**）而不是 `bool::default()`（关）：老界面不带这个字段时，
    /// 不该把用户的落盘形状悄悄改掉。
    #[serde(default = "config::default_device_subdir")]
    pub device_subdir: bool,
    /// BeyondCompare 可执行文件路径；空串 = 自动探测。
    #[serde(default)]
    pub bc_exe_path: String,
    /// 折叠态悬浮形态（设置里的「悬浮形态」）。见 `config::OrbShape`。
    #[serde(default)]
    pub orb_shape: OrbShape,
    /// 新添加的目录/文件默认开播（设置里的「添加后自动共享」，默认关）。缺省 `false`：
    /// 字段缺失 = 沿用 `Config` 的缺省（关），不是替用户打开。
    #[serde(default)]
    pub auto_broadcast_new: bool,
    /// 是否允许本机为群聊中转消息（设置里的「允许中转」那颗开关管的第二条路，默认开）。
    /// 缺省用配置的缺省（**开**）而不是 `bool::default()`（关）：老界面不带这个字段时，
    /// 不该把用户已经开着的开关悄悄关掉。
    #[serde(default = "config::default_relay_enabled")]
    pub group_relay: bool,
    /// 是否允许本机替其它设备中转字节（设置里的「允许中转」，默认开，F-106）。缺省同
    /// `group_relay`：**开**。
    #[serde(default = "config::default_relay_enabled")]
    pub allow_relay: bool,
    /// 记住并自动复用跨网段中转（设置里的「记住并自动复用跨网段中转」，默认开，F-107/S-10）。
    /// 缺省用配置的缺省（**开**）而不是 `bool::default()`（关）：字段缺失不该替用户把它关掉。
    #[serde(default = "config::default_relay_memory")]
    pub relay_memory: bool,
    /// 接收推送的并发数（设置里的「传输并发数」）。缺省 4，取值域由
    /// `config::clamp_transfer_workers` 夹住（手改配置文件也不会越界）。
    #[serde(default = "config::default_transfer_workers")]
    pub transfer_workers: u32,
    /// 大文件分块大小（MB，设置里的「分块大小」）。缺省 4。
    #[serde(default = "config::default_transfer_block_mb")]
    pub transfer_block_mb: u32,
}

// ---------------------------------------------------------------------------
// Share commands (unchanged from P0)
// ---------------------------------------------------------------------------

#[tauri::command]
async fn add_share(path: String, state: State<'_, Arc<AppState>>) -> Result<ShareItemDto, String> {
    add_share_impl(state.inner(), path).await
}

/// `add_share` 的全部动作，命令层只做参数解包。抽出来是因为 tauri 命令要 `State`，集成测试
/// 里造不出来 —— 而"新加的共享项开不开播"这条规则值得直接驱动一遍（见 `tests/share_visibility.rs`）。
pub async fn add_share_impl(state: &Arc<AppState>, path: String) -> Result<ShareItemDto, String> {
    let p = PathBuf::from(&path);
    if !p.exists() {
        return Err(format!("路径不存在: {path}"));
    }
    // 镜像里的东西不能当共享项（见 `unsendable::is_mirror_path`）：那里放的是**占位符**，
    // 对端拿到的是一个读不出内容的空壳；而且它自己都没法被 watcher 挂上。
    if crate::unsendable::is_mirror_path(&p, &crate::mirror::root()) {
        return Err("镜像目录（拖放用的本地占位符）不能添加为共享项".into());
    }
    // 数据目录里的东西一律不行：`credentials.dat`（全部设备 token）、`identity.pem`、`config.toml`
    // 都在里面。它们不只是"不该被分享" —— 一条共享项加上一次推送就等于把本机凭据交给对端。
    if crate::unsendable::is_own_data_path(&p, &crate::config::data_dir()) {
        return Err("OrbDrop 自己的数据目录不能共享（里面是本机的设置与凭据）".into());
    }
    // **同一条路径只登记一次**（F-21）：拖回卡片、选择框里又挑到同一项、同一个目录换个写法
    // （结尾多个 `\`、大小写不同）都落到这里。已经登记过就**原样返回那一项** —— id 不变、
    // 名字（可能被改过）不动、广播开关不重置、不重新挂 watcher（重复挂会多出一条 watcher）。
    // 界面靠"回来的 id 是不是本来就有的"区分「已添加」与「已在共享项里」（见 `App.addShares`）。
    let existing = config::find_share_by_path(&state.config.lock().shares, &p).cloned();
    if let Some(existing) = existing {
        info!(path, "share already registered");
        return share_dto(&existing).await;
    }
    let mut item = ShareItem::new(p);
    // 新登记的共享项按设置里的「添加后自动共享」定初值（`Config::auto_broadcast_new`，默认关）。
    // **只作用于新条目**：已经在列表里的那条在上面就早返回了，开关不会被这里翻动 —— 用户自己
    // 设过的开关不会因为设置改动而被翻回来。
    item.broadcast_enabled = state.config.lock().auto_broadcast_new;
    let dto = share_dto(&item).await?;
    // 查重与插入在**同一次持锁**里完成（`config::register_share`）：一批登记是并发来的
    // （`Promise.all`），分成"查一次、再插一次"两步的话，两条一样的路径会在两次持锁之间漏过去。
    let (created, share) = {
        let mut config = state.config.lock();
        let (created, share) = config::register_share(&mut config.shares, item);
        if created {
            config::save_config_at(&state.config_path, &config).map_err(|e| e.to_string())?;
        }
        (created, share)
    };
    if !created {
        info!(path, "share already registered");
        return share_dto(&share).await;
    }
    // Watch the new share
    share::watcher::watch_share(state, &share.id, share.path);
    notify_shares_changed(state);
    info!(path, "share added");
    Ok(dto)
}

/// 共享项 → 给界面的那一份：与 `list_shares` 是同一份（`isDir` / 大小 / 子项数都由后端
/// 问一次文件系统得来），所以返回值就是「本机」标签页与卡片马上要显示的那一项。
async fn share_dto(item: &ShareItem) -> Result<ShareItemDto, String> {
    let probe_path = item.path.clone();
    let probe = tauri::async_runtime::spawn_blocking(move || probe_share(&probe_path))
        .await
        .map_err(|e| format!("读取共享项失败：{e}"))?;
    Ok(ShareItemDto::new(item, probe))
}

/// 选择框「常用」列表（F-50）：用户右键目录攒出来的快捷入口，随配置持久化。
#[cfg(desktop)]
#[tauri::command]
fn list_picker_favorites(state: State<'_, Arc<AppState>>) -> Vec<PickerFavorite> {
    state.config.lock().picker_favorites.clone()
}

/// 整份写回「常用」（界面给的就是它此刻显示的那份）；返回后端规整过的那份（去重后），
/// 界面以后端为准。**写完立刻落盘**：这几条是用户手动的选择，不能在退出时才写。
#[cfg(desktop)]
#[tauri::command]
fn set_picker_favorites(
    items: Vec<PickerFavorite>,
    state: State<'_, Arc<AppState>>,
) -> Result<Vec<PickerFavorite>, String> {
    let mut config = state.config.lock();
    let stored = config.set_picker_favorites(items);
    config::save_config_at(&state.config_path, &config).map_err(|e| e.to_string())?;
    Ok(stored)
}

/// 共享项清单。**必须 async**：每一项都要问一次文件系统（目录要数一层），而同步命令跑在
/// 主线程上 —— 一个挂在掉线映射盘上的共享项能让整个界面僵住（同 `fs_browse::fs_drives`）。
#[tauri::command]
async fn list_shares(state: State<'_, Arc<AppState>>) -> Result<Vec<ShareItemDto>, String> {
    // 先锁一下配置把清单抄出来就放锁：`MutexGuard` 不是 `Send`，抱着它进不了阻塞线程。
    let shares = state.config.lock().shares.clone();
    tauri::async_runtime::spawn_blocking(move || {
        shares.iter().map(|s| ShareItemDto::new(s, probe_share(&s.path))).collect()
    })
    .await
    .map_err(|e| format!("读取共享项失败：{e}"))
}

#[tauri::command]
fn remove_share(share_id: String, state: State<'_, Arc<AppState>>) -> Result<(), String> {
    let mut config = state.config.lock();
    let before = config.shares.len();
    config.shares.retain(|s| s.id != share_id);
    if config.shares.len() == before {
        return Err(format!("共享项不存在: {share_id}"));
    }
    config::save_config_at(&state.config_path, &config).map_err(|e| e.to_string())?;
    share::watcher::unwatch_share(state.inner(), &share_id);
    notify_shares_changed(state.inner());
    info!(share_id, "share removed");
    Ok(())
}

#[tauri::command]
fn rename_share(share_id: String, name: String, state: State<'_, Arc<AppState>>) -> Result<(), String> {
    let mut config = state.config.lock();
    let share = config.shares.iter_mut().find(|s| s.id == share_id)
        .ok_or_else(|| format!("共享项不存在: {share_id}"))?;
    share.name = name;
    config::save_config_at(&state.config_path, &config).map_err(|e| e.to_string())?;
    drop(config);
    notify_shares_changed(state.inner());
    Ok(())
}

/// 共享项清单变化（新增/移除/改名/开播）后，通知当前处于长连接的对端重新拉 /shares，
/// 否则对端设备标签里的列表要等下一次手动刷新才会更新（F-22「实时出现」）。
fn notify_shares_changed(state: &AppState) {
    for peer_id in state.ws.connected_peers() {
        ws::send_to_peer(state, &peer_id, ws::events::WsEvent::SharesChanged);
    }
}

#[tauri::command]
fn toggle_broadcast(
    req: ToggleBroadcastRequest,
    state: State<'_, Arc<AppState>>,
) -> Result<(), String> {
    let mut config = state.config.lock();
    let share = config.shares.iter_mut().find(|s| s.id == req.share_id)
        .ok_or_else(|| format!("共享项不存在: {}", req.share_id))?;
    share.broadcast_enabled = req.enabled;
    config::save_config_at(&state.config_path, &config).map_err(|e| e.to_string())?;
    drop(config);
    notify_shares_changed(state.inner());
    info!(share_id = req.share_id, enabled = req.enabled, "broadcast toggled");
    Ok(())
}

// ---------------------------------------------------------------------------
// Settings commands
// ---------------------------------------------------------------------------

#[tauri::command]
fn get_settings(state: State<'_, Arc<AppState>>) -> Result<AppSettings, String> {
    Ok(state.config.lock().settings())
}

#[tauri::command]
async fn update_settings(
    req: UpdateSettingsRequest,
    state: State<'_, Arc<AppState>>,
    app: AppHandle,
) -> Result<AppSettings, String> {
    let (old_port, old_name);
    let old_allow_relay;
    #[cfg(desktop)]
    let old_autostart;
    {
        let mut config = state.config.lock();
        old_port = config.port;
        old_name = config.device_name.clone();
        old_allow_relay = config.allow_relay;
        #[cfg(desktop)]
        {
            old_autostart = config.auto_start;
        }
        config.port = req.port;
        config.download_dir = PathBuf::from(&req.download_dir);
        config.download_mode = req.download_mode;
        config.device_subdir = req.device_subdir;
        config.device_name = req.device_name.clone();
        config.auto_start = req.auto_start;
        config.bc_exe_path = Some(PathBuf::from(req.bc_exe_path.trim()))
            .filter(|p| !p.as_os_str().is_empty());
        config.orb_shape = req.orb_shape;
        config.auto_broadcast_new = req.auto_broadcast_new;
        config.group_relay = req.group_relay;
        config.allow_relay = req.allow_relay;
        config.relay_memory = req.relay_memory;
        config.transfer_workers = config::clamp_transfer_workers(req.transfer_workers);
        config.transfer_block_mb = config::clamp_transfer_block_mb(req.transfer_block_mb);
        config::save_config_at(&state.config_path, &config).map_err(|e| e.to_string())?;
    }

    // Autostart change
    #[cfg(desktop)]
    if req.auto_start != old_autostart {
        if req.auto_start {
            let _ = autostart::enable_autostart();
        } else {
            let _ = autostart::disable_autostart();
        }
    }

    let name_changed = req.device_name != old_name;
    let port_changed = req.port != old_port;

    // 关掉「允许为其它设备中转」→ **立刻收掉**本机正替别人转的每一条。
    // 开关是用户意图：已经开出去的端点不能继续占本机上行带宽（开着的那几条 TCP
    // 也要断，否则"关了开关还在替人搬字节"）。
    if old_allow_relay && !req.allow_relay {
        crate::relay::close_all(state.inner());
    }
    // 开关本身也在 mDNS TXT 里自报（F-107：同段的观察者零请求就知道本机愿不愿意中转），
    // 所以它一变就重播一次广播 —— 否则对端要等自己的 5s 探活才知道，而没配对的同段设备
    // 根本不会探本机（它们只读 TXT）。
    if old_allow_relay != req.allow_relay && state.is_online() {
        register_mdns(state.inner(), bound_port(state.inner()));
    }

    // Device name changed（端口没动）→ 重注册 mDNS（丢掉旧 daemon、按新参数再注册一个）。
    // 播的是**实际**绑定端口，不是配置端口：配置端口被占时服务器会顺延到 +1..+10，
    // 播配置值等于给对端一个没人听的地址。
    // 离线时**不**重新注册：现在的状态就是"没有广播"（`set_online` 已经把 daemon 丢掉了），
    // 重注册等于替用户改回在线。
    if name_changed && !port_changed && state.is_online() {
        register_mdns(state.inner(), bound_port(state.inner()));
    }

    // Port changed → 重启 HTTP 服务器，**再按重启后实际绑定的端口重播 mDNS**。
    // 两件事必须成对：只重启不重播的话，广播里留着旧端口，而对端是拿广播里的端口当
    // 拨号目标的（5s 探活不刷新端口），它会一直拨一个没人听的地址 —— 直到本机重启，
    // 或用户切一次「离线→在线」（那条路会用 `bound_port` 重注册）。
    if port_changed {
        let arc_state = state.inner().clone();
        let _ = tokio::spawn(async move {
            server::stop_server(&arc_state).await;
            match server::ensure_server(arc_state.clone()).await {
                Ok(port) => {
                    info!(port, "HTTP server restarted");
                    // 广播跟着重启后的端口走（配置端口还被占的话会再顺延一次）；
                    // 离线时照旧不播。
                    if arc_state.is_online() {
                        register_mdns(&arc_state, port);
                    }
                    let _ = app.emit(ws::events::ui::SERVER_STARTED, port);
                }
                Err(e) => {
                    error!("HTTP server restart failed: {e}");
                    let _ = app.emit(ws::events::ui::SERVER_ERROR, e);
                }
            }
        });
    }

    Ok(state.config.lock().settings())
}

// ---------------------------------------------------------------------------
// BeyondCompare 集成（F-41 ~ F-43）
// ---------------------------------------------------------------------------

/// `slot_for` = 右键菜单里那一项是文件还是文件夹（前端给 `"file"` / `"folder"`）：
/// BC 按类型取槽，我们也按类型取，好让菜单里那行与真正要比的那一档**一致**。
/// 没有上下文（启动、设置面板）时传 `null`。
#[cfg(desktop)]
#[tauri::command]
fn bc_status(
    slot_for: Option<String>,
    state: State<'_, Arc<AppState>>,
) -> bc::BcStatusDto {
    let prefer = match slot_for.as_deref() {
        Some("file") => Some(bc::ShellSlot::File),
        Some("folder") => Some(bc::ShellSlot::Folder),
        _ => None,
    };
    bc::status(state.inner(), prefer)
}

#[cfg(desktop)]
#[tauri::command]
fn bc_set_left(
    target: BcTarget,
    state: State<'_, Arc<AppState>>,
) -> Result<bc::BcStatusDto, String> {
    bc::set_left(state.inner(), &target)?;
    // 刚写完两个槽都是这一项：读哪一档都一样，给 `None`（文件夹槽优先）
    Ok(bc::status(state.inner(), None))
}

/// 与左侧比对：左边读槽里的**原文**，右边是右键这一项 —— 本机共享项给本地路径，
/// 对端共享项给 `profile:<ip:port>?<shareId>/<路径>`（顺手写好 WebDAV profile）。
#[cfg(desktop)]
#[tauri::command]
fn bc_compare_with_left(
    target: BcTarget,
    state: State<'_, Arc<AppState>>,
) -> Result<(), String> {
    bc::compare_with_left(state.inner(), &target)
}

// ---------------------------------------------------------------------------
// Server / Device commands
// ---------------------------------------------------------------------------

#[tauri::command]
fn get_actual_port(state: State<'_, Arc<AppState>>) -> u16 {
    state.server_port.load(std::sync::atomic::Ordering::SeqCst)
}

/// 本机内网地址（左下角「端口 · IP」用）。
///
/// 多网卡（有线 + 无线 + VPN）时会返回好几个：`primary` 是界面显示的那一个，
/// `all` 供悬停提示把其余几个列全 —— 只显示第一个是必须的（footer 装不下），
/// 但"我还有别的地址"这件事得让人看得见，否则跨网段手动添加时会对不上号。
#[derive(Debug, Serialize)]
struct LocalAddresses {
    primary: Option<String>,
    all: Vec<String>,
}

#[tauri::command]
fn local_addresses() -> LocalAddresses {
    let all = server::local_ipv4_addresses();
    LocalAddresses {
        primary: all.first().cloned(),
        all,
    }
}

#[tauri::command]
fn list_devices(state: State<'_, Arc<AppState>>) -> Vec<serde_json::Value> {
    // 两份快照各自锁一次、不嵌套：设备表与配置表是两把独立的锁，嵌套着取是白送一个
    // 死锁面（两处顺序不同就够）。设备表本身很小（一屏设备数），拷一份不值一提。
    let peers = state.peers.lock().clone();
    let remarks = state.config.lock().device_remarks.clone();
    peers.iter().map(|p| {
    serde_json::json!({
        "id": p.id,
        "name": p.name,
        "ip": p.ip,
        "port": p.port,
        "online": p.online,
        "paired": p.paired,
        // 「看得见但连不上」（F-106）：记录还在（对端在喊），但探活没成 —— 界面据此把这一行
        // 留在列表里，并在用户**发起连接**时补一句"这张网可能把单播隔开了"；**不是**一个状态档
        // （那一行照常画「未绑定」/「离线」）。`online` 的定义不动（探活成功 = 真连得上）。
        "seen": state.is_seen(&p.id),
        // 此刻是不是经别人中转（F-106）：**不上界面**（用户要求，两端都不写「经 X 中转」），
        // 只留给前端 `unicastBlocked` 那条判据 —— 它区分"拨的是借来的中转端点"与"直连不通"。
        "relayVia": p.relay_via(),
        // Real WS link state — "长连接" in the UI follows this, not `online`
        // (a peer can answer HTTP push invites without any WS session).
        "connected": state.ws.has_connection(&p.id),
        // 没连着的时候"为什么"：重连中 / 等对端上线 / 已停止（对端断开过、token 失效…）。
        // 界面把它显示成人话，用户才知道该等、该点「连接」、还是该重新配对。
        "linkState": state.ws.link_state(&p.id),
        // 本机给这台设备起的备注（F-101）：只存在本机，**不参与任何对外报文** ——
        // 界面把它拼成显示名「备注(设备名)」（前端 `deviceDisplayName`）。
        "remark": remarks.get(&p.id),
    })
    }).collect()
}

/// F-107：本机的中转记忆（网段级策略 + 各自的池 + 设备级链路）。
///
/// 界面「设置 → 高级」拿它列出「目标网段 → 中转网段 → 池 + 最后成功时间」，并支持逐条清除
/// —— 记忆落了盘，就该让用户看得见、删得掉。
#[tauri::command]
fn relay_memory(state: State<'_, Arc<AppState>>) -> Vec<serde_json::Value> {
    state.relay_routes_json()
}

/// F-107：清掉中转记忆。`target_cidr` 给了就只清那一条网段级策略（连同挂在它上面的设备级
/// 链路），不给就全清。返回清掉的条数。
#[tauri::command]
fn clear_relay_memory(
    target_cidr: Option<String>,
    state: State<'_, Arc<AppState>>,
) -> usize {
    state.clear_relay_memory(target_cidr.as_deref())
}

/// F-106/F-107：本机此刻替别人开着的中转端点（设置里显示「正在替 N 台设备中转」并一键收掉）。
#[tauri::command]
fn relay_status(state: State<'_, Arc<AppState>>) -> serde_json::Value {
    let active: Vec<serde_json::Value> = state
        .relays
        .snapshot()
        .into_iter()
        .map(|(requester, target, port)| {
            serde_json::json!({ "requester": requester, "target": target, "port": port })
        })
        .collect();
    serde_json::json!({
        "active": active.len(),
        "entries": active,
        // 本机此刻看到的中转池（哪一段里有谁愿意中转）——排障用。
        "pools": state.pool_json(),
    })
}

/// F-106：立刻收掉本机替别人开着的全部中转（"一键收掉"，不改开关）。
#[tauri::command]
fn close_relays(state: State<'_, Arc<AppState>>) -> usize {
    let n = state.relays.active();
    crate::relay::close_all(state.inner());
    n
}

/// F-106 / S-09：本机替别人中转的**累计流量**（字节，收发两方向之和）与连接数。
///
/// 设置里只显示这一个数（"替别人搬了多少"）—— 中转方的代价就是上行带宽。它是落了盘的记录
/// （`Config::relay_bytes_total` / `relay_conns_total`），重启接着算。
#[tauri::command]
fn relay_traffic(state: State<'_, Arc<AppState>>) -> serde_json::Value {
    state.relay_traffic_json()
}

/// 写一台设备的备注（F-101）。空串 / 纯空白 = 清除；返回真正存下去的值（`null` = 已清除），
/// 界面以后端为准（超长会被截断，见 `config::REMARK_MAX_CHARS`）。
///
/// **纯本机动作**：不广播、不发 HTTP、不看在线状态 —— 未绑定与离线的设备照样能改备注
/// （设备右键菜单里这一项因此不受任何状态分档影响）。写完立刻落盘并广播 `devices://changed`，
/// 让设备行、标签栏、会话标题一起换成新显示名。
#[tauri::command]
fn set_device_remark(
    peer_id: String,
    remark: String,
    state: State<'_, Arc<AppState>>,
) -> Result<Option<String>, String> {
    let stored = {
        let mut config = state.config.lock();
        let stored = config.set_device_remark(&peer_id, &remark);
        config::save_config_at(&state.config_path, &config).map_err(|e| e.to_string())?;
        stored
    };
    state.emit_devices_changed();
    info!(peer_id, set = stored.is_some(), "device remark updated");
    Ok(stored)
}

/// 本机给设备起的**备注**（F-101）：`设备 id → 备注`，整份读一次。
///
/// 为什么要单独开一条命令（`list_devices` 的每一行里也带着 `remark`）：**备注不跟着设备行走**。
/// 没有那一行的设备（未配对、也不在地址簿：那一行只活在内存里，重启即忘 / 还没被观察到）
/// 在会话列表里照样要显示成用户起的那个备注，而那时前端手上只有"记录里对端自报的名字"
/// —— 于是"备注优先"这条设计在**离线且没有行**的时候整条失效（真机反馈：会话里显示成了
/// 设备名而不是备注）。备注是**本机的用户数据**（`config.toml` 的 `deviceRemarks`，键是设备 id），
/// 与"此刻有没有那一行"无关，所以按 id 单独给一份。
#[tauri::command]
fn list_device_remarks(state: State<'_, Arc<AppState>>) -> std::collections::HashMap<String, String> {
    state.config.lock().device_remarks.clone()
}

#[tauri::command]
async fn start_http_server(state: State<'_, Arc<AppState>>) -> Result<u16, String> {
    let arc_state = state.inner().clone();
    server::ensure_server(arc_state).await
}

// ---------------------------------------------------------------------------
// 本机「在线 / 离线」（面板右上角状态框）
// ---------------------------------------------------------------------------

/// 本机实际在听的 HTTP 端口：被占用时服务器从配置端口往上试到 +10，所以两者可能不同
/// （mDNS 记录里播的必须是**实际**端口，否则对端拿到一个没人听的地址）。
///
/// 规则只有一份（`AppState::listening_port`）—— 判"一个来源地址是不是我自己"用的
/// 也是它，两处各算各的迟早会分叉。
fn bound_port(state: &AppState) -> u16 {
    state.listening_port()
}

/// 本机切到「离线」后，所有**对外**动作一律挡在门口。
///
/// 不是安全边界，是**诚实**：这些动作都要对端能回连本机（WS 升级、HTTP 回执、探活），
/// 而离线时本机对一切入站请求回 503 —— 放出去只会卡到超时、或者更糟，在对端留下一个
/// "本机在线"的假象。错误文案直接告诉用户切回「在线」。
fn ensure_online(state: &AppState) -> Result<(), String> {
    if state.is_online() {
        Ok(())
    } else {
        Err("本机处于「离线」，先在上方状态框切回「在线」".to_string())
    }
}

/// 切到「离线」时要做的本机副作用（**唯一入口**：`apply_online(false)`；集成测试也用它）。
///
/// 四件事：① 注销 mDNS 广播（对端发现不到本机）；② 掐掉全部长连接与拨号循环；③ **收掉本机
/// 替别人开着的中转端点** —— 离线 = 隐身，对所有设备都不可见，中转不能例外（端点在 `relay`
/// 里是个裸 TCP 监听，不归 HTTP 那层管，只有这里收得掉；`relay::serve` 另有一道 `is_online`
/// 兜底）；④ 把现有设备行标成离线（显示层，切回在线后 5s 探活会纠正回来）。
pub fn apply_offline(arc: &Arc<AppState>) {
    unregister_mdns(arc);
    arc.ws.drop_all();
    crate::relay::close_all(arc);
    // "刚听到过"那一档也一起清掉：离线 = 隐身，这一刻设备列表就该是"谁都不在线"，
    // 而这一档是**本机在线上**才成立的知识 —— 留着它，切回在线之后会有一批行靠 60s 内
    // 的旧证据"凭空"在线（探活还没敲过它们）。切回在线由探活重新建立。
    arc.clear_all_heard();
    let ids: Vec<String> = arc.peers.lock().iter().map(|p| p.id.clone()).collect();
    for id in ids {
        arc.set_peer_online(&id, false);
    }
}

/// 重新挂上 mDNS 广播：设备名/端口改了之后要重注册，切回「在线」时也要。
///
/// 注册的记录里带着端口与设备名，所以每次都是"丢掉旧 daemon、按新参数再注册一个"。
fn register_mdns(state: &Arc<AppState>, port: u16) {
    // 旧的那一份**先撤下来**：改名/换端口都是"替换"，不是"再加一条"。而且必须
    // 注销 + 关掉 daemon（`discover::release`），只丢句柄的话旧记录会一直挂在网上 ——
    // 对端每次解析到它，就把旧名字写回设备行一次。
    release_mdns(state);
    let (device_name, device_id, allow_relay) = {
        let cfg = state.config.lock();
        (cfg.device_name.clone(), cfg.device_id.clone(), cfg.allow_relay)
    };
    // 注册失败时保持"没有注册"（`None`）：留着一个空的注册等于以为自己在网上。
    *state.mdns_reg.lock() = discover::register(port, &device_name, &device_id, allow_relay);
}

/// 撤下当前 mDNS 注册（换注册、切「离线」共用）。
///
/// 注销必须按**注册那一刻的全名**点名（存在 `mdns_reg` 里）：按当前配置拼出来的全名在
/// 改过名之后已经不是线上那一条了。注销会立刻发一条 TTL=0 的告别包，而不是等对端缓存
/// 自然过期（TXT 的 TTL 有 75 分钟）。
fn release_mdns(state: &Arc<AppState>) {
    let Some(reg) = state.mdns_reg.lock().take() else {
        return;
    };
    info!(fullname = %reg.fullname, "mDNS service released");
    discover::release(reg);
}

/// 注销 mDNS 广播（离线 = 隐身）。
///
/// 与"换注册"共用 `release_mdns`：注销要点名到注册那一刻的全名，并且**关掉** daemon
/// （`ServiceDaemon` 没有 `Drop`，只丢句柄等于没注销 —— 对端缓存里那条记录要等到 TTL
/// 过期才消失，而且旧记录会一直答 query）。
fn unregister_mdns(state: &Arc<AppState>) {
    release_mdns(state);
}

/// 切本机「在线 / 离线」（面板右上角状态框、托盘右键菜单两个入口共用这一份实现）。
///
/// **离线 = 隐身**，三件事一起做：注销 mDNS 广播（对端发现不到本机）、掐掉全部长连接与
/// 拨号循环、由 `server::presence_middleware` 拒收一切入站请求（对端的 5s 探活因此把它
/// 判成离线）。**其余一律不动**：配对关系、token、共享项、地址簿里的长连接记忆全保留，
/// 所以切回「在线」就是"重新广播 + 按记忆把链路接回来"（F-16 那条路）。
///
/// 状态落盘（`Config::online`）：用户选了离线就一直离线，重启也是。
///
/// **必须是唯一入口**：副作用有 mDNS 注销、断链、落盘三样，多一份实现就必然漏一样。
/// 命令（前端）与托盘菜单都走这里；状态没变时早退，所以重复调用是安全的。
pub(crate) fn apply_online(
    arc: &Arc<AppState>,
    app: &AppHandle,
    online: bool,
) -> Result<bool, String> {
    if arc.is_online() == online {
        return Ok(online);
    }
    arc.set_local_online(online);

    if online {
        register_mdns(arc, bound_port(arc));
        ws::restore_links_on_startup(arc);
        // 离线期间对端收不到任何东西（本机整个路由回 503），够不着的那些消息在排队：
        // 切回在线就把它们发出去（"在途"的那些不退 —— 它们是真的在飞）。
        chat::flush_all_queued(arc);
    } else {
        apply_offline(arc);
    }

    arc.emit_devices_changed();
    let _ = app.emit(
        ws::events::ui::LOCAL_ONLINE,
        serde_json::json!({ "online": online }),
    );
    info!(online, "local presence changed");
    Ok(online)
}

#[tauri::command]
fn set_online(online: bool, state: State<'_, Arc<AppState>>, app: AppHandle) -> Result<bool, String> {
    apply_online(state.inner(), &app, online)
}

#[tauri::command]
fn get_peer_token(peer_id: String) -> Result<Option<String>, String> {
    credential::get_peer_token(&peer_id).map_err(|e| e.to_string())
}

/// 用户点「连接」（F-16）：**先问对端一句**（同意制，见 `crate::link`），得到同意才拨号。
///
/// 返回 `ConnectOutcome`（前端按它决定要不要挂"等待对方同意"的卡片）：
/// - `asked`：请求已挂在对方通知里等它点头，本机不等在这儿 —— 结果走
///   `link://request_result` 事件回来（那边同意后本机自己拨号）。
/// - `dialing`：**已经没有可问的人**（对端不在网上 / 是老版本）或链路本来就在：直接拨号，
///   与旧行为一致。
///
/// 无论哪一条，本机「断开」的暂停都当场结束：按钮已经按了，这一台不该再拿"本机暂停"
/// 挡着自己（`link::request` 在同意后会写 `Wanted`；`dialing` 这一支在这里写）。
///
/// **保持 async**：请求与拨号任务都要在运行时上跑。
#[tauri::command]
async fn connect_peer(
    peer_id: String,
    state: State<'_, Arc<AppState>>,
) -> Result<ConnectOutcome, String> {
    ensure_online(&state)?;
    let Some(peer) = state.get_peer(&peer_id) else {
        return Err("设备不存在".to_string());
    };
    // 长连接只与**已配对**的设备建立（`dial_now` 里同一道关）：未绑定的设备得先配对。
    if !peer.paired {
        return Err(format!("还没有与「{}」配对", peer.name));
    }
    if state.ws.has_connection(&peer_id) {
        return Ok(ConnectOutcome::Dialing);
    }
    // 本机的暂停先结束（内存 + 地址簿），再问对端。
    state.ws.resume(&peer_id);

    // 对端不在网上（探活就判"离线"）、或**手上没有任何能拨的地址**（直连地址与借来的中转
    // 端点都没有）→ 没人能点头：退回直接拨号，让拨号循环去等它上线。
    if !peer.online || !peer.dialable() {
        state.set_link_intent(&peer_id, LinkIntent::Wanted);
        ws::ensure_link_now(state.inner(), &peer_id);
        return Ok(ConnectOutcome::Dialing);
    }

    link::request(state.inner(), &peer_id)?;
    Ok(ConnectOutcome::Asked)
}

/// 「连接」这件动作的两种开局（前端只认这两种：挂卡片 / 直接等链路）。
///
/// 卡片由 `link://request_sent` / `link://request_result` 两条事件驱动（与配对邀请同一套），
/// 所以这里不带 requestId —— 界面不该同时从两个地方拼同一件事。
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
enum ConnectOutcome {
    /// 请求已经发出去、挂在对方通知里等它点头（结果走 `link://request_result`）。
    Asked,
    /// 本机正在拨号（对端不在网上 / 老版本 / 链路本来就在）：与旧行为一致。
    Dialing,
}

/// 接收方：用户点了「接受」——放行对方的这一次拨号（并结束本机对它的「断开」暂停）。
#[tauri::command]
fn accept_link_request(
    request_id: String,
    state: State<'_, Arc<AppState>>,
) -> Result<String, String> {
    let peer = link::accept(state.inner(), &request_id)?;
    info!(peer = %peer, "link request accepted, waiting for the peer to dial");
    Ok(peer)
}

/// 接收方：用户点了「拒绝」——给发起方一个明确的"不"。
#[tauri::command]
fn reject_link_request(request_id: String, state: State<'_, Arc<AppState>>) {
    link::reject(state.inner(), &request_id);
}

/// 发起方：用户点了「取消」——不再等这一次结果，并（尽力）把对方那条通知撤掉。
#[tauri::command]
fn cancel_link_request(request_id: String, state: State<'_, Arc<AppState>>) -> Result<(), String> {
    link::cancel(state.inner(), &request_id)
}

/// Drop the long connection and keep it down (auto-connect stays off) until the
/// user here reconnects, or the peer asks to connect with its own 「连接」.
///
/// 「断开」的意图也跨重启有效，而且**两个方向都要**：本机不主动拨（`auto_connect`
/// 清零），也拒绝对端的自动重拨（`paused` 落盘 —— 这一半要是只留在内存里，重启后
/// 对端照旧记着这次长连接，它一看见本机上线就拨过来、本机一收，双方又回到「长连接」，
/// 用户按的这一下等于没按）。
#[tauri::command]
fn disconnect_peer(peer_id: String, state: State<'_, Arc<AppState>>) -> Result<(), String> {
    if state.get_peer(&peer_id).is_none() {
        return Err("设备不存在".to_string());
    }
    state.ws.pause(&peer_id);
    state.set_link_intent(&peer_id, LinkIntent::Paused);
    state.emit_devices_changed();
    Ok(())
}

// ---------------------------------------------------------------------------
// Device management commands (F-11, F-14, F-71)
// ---------------------------------------------------------------------------

/// 手动添加设备（F-11）时，留空端口要探的端口个数：与 S-06 的端口段一致
/// （`3890` 起 10 个 —— 对端启动时默认端口被占会顺延到 `+1..+9`，只探 3890 会在那种
/// 机器上"添加不上"）。
const MANUAL_PORT_SCAN: u16 = 10;

/// Add a device manually by IP:port. Probes /pair/info to get identity.
///
/// 本机这一半是"存一行"；**对方那一半**由 [`notify_manual_add`] 补上（同一件事的两侧：
/// 同网段时 mDNS 会让两边互相看见，跨网段只剩这一条单播）。
///
/// `port == 0` = 用户没填端口：从默认端口起探 `MANUAL_PORT_SCAN` 个（见 `manual_peer`）。
#[tauri::command]
async fn add_peer(ip: String, port: u16, state: State<'_, Arc<AppState>>) -> Result<PeerDevice, String> {
    let own_id = state.config.lock().device_id.clone();
    let (port, info) = manual_peer(&ip, port, &own_id).await?;
    let id = info["id"].as_str().unwrap_or("").to_string();
    let name = info["name"].as_str().unwrap_or(&ip).to_string();
    let peer = PeerDevice { id, name, ip, port, online: true, paired: false, ..Default::default() };
    state.add_peer_book(peer.clone());
    // 另一半（F-11）：到对方那个地址上自报一次，让它也建一行（见 `notify_manual_add`）。
    // 甩到后台：最坏要等满 1.5s 超时，而"添加"在本机已经成立，不该被它拖住。
    crate::task::spawn("manual add hello", notify_manual_add(state.inner().clone(), peer.clone()));
    Ok(peer)
}

/// 手动添加成功之后的另一半（F-11）：到对方那个地址上自报一次身份，让对方也有一行。
///
/// 手动添加是**手动版的 mDNS** —— 同网段时两边本来就互相看得见（收到一条公告各建一行），
/// 而 mDNS 跨不过路由：跨网段那台"添加得上，却只有本机看得见对方"。这一跳把另一半补上：
/// 对方的设备列表里出现本机（未绑定、在线），它才有地方输本机屏幕上那 6 位配对码
/// （没有这一行时，跨网段那台连配对都发起不了 —— `pair_confirm` 要求设备行已存在）。
/// **只建行、不建关系**：配对 / 长连接 / 共享 / 推送同意制一条都不动，关系照旧要双方主动。
///
/// 已有凭据就带上：`/pair/hello` 对"自称已配对设备"要求出示 token（与推送邀请同一道关，
/// 重新添加一台已配对设备的地址时走的就是这一档）。**best-effort**：对端是老版本（没有这条
/// 路由 → 404）、此刻隐身（503）、离线或被防火墙挡住，都只是"对方那边没有这一行"。
async fn notify_manual_add(state: Arc<AppState>, peer: PeerDevice) {
    // 取锁顺序：`listening_port` 自己会锁 config（`server_port == 0` 那一支），不能套在外面。
    let own_port = bound_port(&state);
    let (own_id, own_name) = {
        let config = state.config.lock();
        (config.device_id.clone(), config.device_name.clone())
    };
    let url = format!("{}/pair/hello", net::peer_base(&peer.ip, peer.port));
    let client = match net::client_builder()
        .timeout(std::time::Duration::from_millis(1500))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            warn!(peer = %peer.id, "failed to build a client for the manual-add hello: {e}");
            return;
        }
    };
    let mut req = client.post(&url).json(&serde_json::json!({
        "peerId": own_id,
        "peerName": own_name,
        "port": own_port,
    }));
    if let Ok(Some(token)) = credential::get_peer_token(&peer.id) {
        req = req.bearer_auth(token);
    }
    match req.send().await {
        Ok(resp) if resp.status().is_success() => {
            info!(peer = %peer.id, "peer told about the manual add")
        }
        // 404 = 对端还是老版本（没有这条路由）；503 = 它此刻隐身。都不是错误。
        Ok(resp) => info!(peer = %peer.id, status = %resp.status(), "peer did not register this device"),
        Err(e) => info!(peer = %peer.id, "peer unreachable for the manual-add hello: {e}"),
    }
}

/// 在 `ip` 上找到那台设备：返回**它答话的端口**与 `/pair/info` 的回话。
///
/// - 给了端口（≠ 0）：只问那一个，问不到就报错（错在端口上，说清楚是哪一个）。
/// - 留空（0）：从 `config::default_port()` 起探 `MANUAL_PORT_SCAN` 个端口段。
///
/// 十个端口**并发**探：串行最坏情况是十次连接超时（用户只能看到"点了没反应"），并发后
/// 一轮就是一次超时的时间。答话的端口里取**最小的那个** —— S-06 是"从默认端口往上找第一个
/// 空闲的"，所以对端真实监听的端口就是这一段里最小的那个活着的（上面的顺延规则）。
///
/// **本机自己那一台要跳过**（`own_id`）：跨网段手动添加时用户完全可能把自己那台的 IP 粘进来
/// （从 footer 复制 `IP:端口` 那条路就是这么用的），而扫描时本机在默认端口上一定答话 ——
/// 不跳过就会"添加了自己"，设备列表里多一行自己，后面所有关系判定都跟着乱。
async fn manual_peer(ip: &str, port: u16, own_id: &str) -> Result<(u16, serde_json::Value), String> {
    manual_peer_from(ip, port, crate::config::default_port(), own_id).await
}

/// [`manual_peer`] 的实体：扫描起点单独给（测试要在一个自己的端口段上跑，不然会撞上开发者
/// 正在跑的那份实例 —— 那台设备也答 `/pair/info`）。
async fn manual_peer_from(
    ip: &str,
    port: u16,
    base: u16,
    own_id: &str,
) -> Result<(u16, serde_json::Value), String> {
    let ports = manual_scan_ports(port, base);
    let client = net::client_builder()
        .timeout(std::time::Duration::from_millis(1500))
        .build()
        .map_err(|e| format!("HTTP 客户端建不起来: {e}"))?;
    let answers = futures_util::future::join_all(
        ports.iter().map(|p| probe_pair_info(&client, ip, *p)),
    )
    .await;
    let mut self_only = false;
    for (p, answer) in ports.iter().zip(answers) {
        let Some(info) = answer else { continue };
        if info["id"].as_str() == Some(own_id) {
            self_only = true;
            continue;
        }
        return Ok((*p, info));
    }
    if self_only {
        return Err(format!("{ip} 上是本机自己：手动添加填的是本机自己的地址"));
    }
    Err(match ports.as_slice() {
        [only] => format!("无法连接 {ip}:{only}（对方没启动 / 端口不对 / 被防火墙挡住）"),
        _ => format!(
            "无法连接 {ip}（已试 {}~{} 这 {} 个端口：对方没启动 / 端口不对 / 被防火墙挡住）",
            ports[0],
            ports[ports.len() - 1],
            ports.len()
        ),
    })
}

/// 手动添加要探的端口表：`port != 0` 就是那一个，`0` 就是 `base` 起 `MANUAL_PORT_SCAN` 个
/// （升序，答话时取最小的那个）。
fn manual_scan_ports(port: u16, base: u16) -> Vec<u16> {
    if port != 0 {
        return vec![port];
    }
    (0..MANUAL_PORT_SCAN).map(|i| base.saturating_add(i)).collect()
}

/// 问一次 `ip:port` 的 `/pair/info`：**HTTP 200 + 合法 JSON** 才算"有人答话"。
///
/// 中间那道 `error_for_status` 不能省：一段里开着别的 HTTP 服务时，它可能回 404/500/一坨
/// 别的东西 —— 那都不算"这里有一台 OrbDrop"。
async fn probe_pair_info(
    client: &reqwest::Client,
    ip: &str,
    port: u16,
) -> Option<serde_json::Value> {
    let url = format!("{}/pair/info", net::peer_base(ip, port));
    let resp = client.get(&url).send().await.ok()?.error_for_status().ok()?;
    resp.json::<serde_json::Value>().await.ok()
}

/// 解除配对（F-14）：**先清本机**（这是本机动作，对端在不在网上都不该挡在这里），
/// 再尽力通知对端 —— 那条通知放到后台跑，调用方不等它。
///
/// 走 HTTP 而不是 WS 事件：解除的那一刻链路可能已经断了，而且 WS 事件是**排队**
/// 发送的，紧接着的 `drop_peer` 会把还没写出去的它一起丢掉 —— 对端就一直停在
/// "已配对"（用户实际遇到的）。HTTP 不依赖长连接，谁拨的号也不影响。
///
/// **通知不挡在本机动作前面**：对端离线时那次 POST 要等到超时（3s）才回来，挡在前面
/// 就是"点了解除配对，界面／凭据要愣好几秒才认"（离线设备必现）。token 在删除前读出来
/// 就够了 —— 值就是两端共有的那一份，本机删掉自己那份不影响这次发送。
///
/// 本机只清 token 与配对标记、**保留设备行**：对方还在网络上看得见，就该继续
/// 以"未绑定"出现在列表里（离线时前端自然不显示）。
pub async fn unpair_peer(state: &Arc<AppState>, peer_id: &str) -> Result<(), String> {
    let peer = state.get_peer(peer_id).ok_or_else(|| "设备不存在".to_string())?;

    // 通知对端要用的三样东西（token 必须在本机删除之前读出来）。
    // body 里的 peerId 是**本机自己的 id**：对端要解除的是"调用方这台设备"
    // （与 WS 事件 `Unpair { peer_id: 本机 id }` 的语义一致）。远端调用时身份
    // 由 token 反解、这个字段只是冗余；同机回环调用没有 token 可解，就靠它。
    let notice = if peer.ip.is_empty() || peer.port == 0 {
        None
    } else {
        credential::get_peer_token(peer_id)
            .ok()
            .flatten()
            .map(|token| {
                let url = format!("{}/unpair", net::peer_base(peer.dial().0, peer.dial().1));
                let own_id = state.config.lock().device_id.clone();
                (url, token, own_id)
            })
    };

    credential::remove_peer(peer_id).map_err(|e| e.to_string())?;
    state.ws.drop_peer(peer_id);
    state.unpair(peer_id);

    // 关系没了，BC 里那条指向它的 WebDAV profile 也就没用了（token 已作废）：按当前
    // 在册设备刷新一遍 —— prune 只删我们自己写下且已失效的条目（见 `bc::prune_webdav_profiles`）。
    #[cfg(desktop)]
    bc::provision_profiles(state);
    // 指纹也一起忘掉：这一段关系结束了，地址再被别人拿到时不该还拿着旧指纹去比对
    // （那条地址现在是"没见过的对端"，走 TOFU 重新认）。
    if let Ok(ip) = peer.ip.parse() {
        net::trust().unpin(ip);
    }
    state::refresh_trust(state);

    // 本机这一侧已经解除完毕（上面全是本地动作），通知对端是**尽力而为**的后话：
    // 谁也不能保证它此刻在网上，最坏要等满 3s 超时 —— 所以在这里甩出去，不让它
    // 拖住用户看到的"已解除"。
    if let Some((url, token, own_id)) = notice {
        crate::task::spawn("unpair notice", notify_unpair(peer_id.to_string(), url, token, own_id));
    }
    Ok(())
}

/// 告诉对端"这段关系到此为止"（`POST /unpair`）：成功与否只落一条日志。
///
/// 与 `unpair_peer` 分开放，是因为调用方**不**等它（见那里的注释）。发不出去不等于
/// 白做：对端下次拿旧 token 来拨号/请求时会吃 401，界面上就是「需重新配对」。
async fn notify_unpair(peer_id: String, url: String, token: String, own_id: String) {
    let client = match net::client_builder()
        .timeout(std::time::Duration::from_secs(3))
        .build()
    {
        Ok(c) => c,
        Err(e) => {
            warn!(%peer_id, "failed to build a client for the unpair notice: {e}");
            return;
        }
    };
    match client
        .post(&url)
        .bearer_auth(&token)
        .json(&serde_json::json!({ "peerId": own_id }))
        .send()
        .await
    {
        Ok(resp) if resp.status().is_success() => info!(%peer_id, "peer notified of unpair"),
        Ok(resp) => warn!(%peer_id, status = %resp.status(), "peer did not accept unpair"),
        Err(e) => warn!(%peer_id, "peer unreachable for unpair: {e}"),
    }
}

#[tauri::command]
async fn remove_peer(peer_id: String, state: State<'_, Arc<AppState>>) -> Result<(), String> {
    unpair_peer(state.inner(), &peer_id).await
}

/// 一键清空本机**全部**配对凭据（F-15）：用户不必（也不该）去翻文件或系统凭据管理器。
///
/// 三处一起清，少一处就等于没清干净：
/// 1. 凭据文件（`%LOCALAPPDATA%\OrbDrop\credentials.dat`）—— 全部设备的 token 与对端
///    证书指纹都在里面，删它就全清（`credential::wipe`）；
/// 2. 地址簿里的配对标记与地址行 —— 设备行留着（还在网上看得见就继续以"未绑定"列出，
///    见 F-14），只是关系回到未配对；
/// 3. 本机写进 Beyond Compare 的 WebDAV profile —— 那是 token 的**第二份拷贝**，
///    清凭据不清它，BC 里就还躺着一份能连上对端的密码。token 没了，`provision_profiles`
///    会把我们自己写下的条目全部剪掉（只删用户名是 `OrbDrop` 的那些）。
///
/// 只清本机：不逐个通知对端（那要 N 次 HTTP，对端离线时"清空"会被网络卡住）。
/// 对端的反应与"本机凭据被外部清掉"完全一样：它下次请求 401 → 「需重新配对」。
pub fn wipe_all_credentials(state: &Arc<AppState>) -> Result<usize, String> {
    let ids: Vec<String> = state.peers.lock().iter().map(|p| p.id.clone()).collect();

    // 先清凭据：失败就原样退出，别把地址簿清了却留下还能用的 token
    // （用户点了"清空"却发现对端还认我们 —— 那是最糟的半清状态）。
    credential::wipe().map_err(|e| e.to_string())?;
    for id in &ids {
        state.ws.drop_peer(id);
        state.unpair(id);
    }
    #[cfg(desktop)]
    bc::provision_profiles(state);
    // 凭据全清了，钉住的指纹也就没有归属了：Trust 表跟着清空，地址上的对端从此按
    // "没见过的身份"重新认（那正是"清空后重新配对"该有的样子）。
    state::refresh_trust(state);
    info!(peers = ids.len(), "wiped all pairing credentials");
    Ok(ids.len())
}

#[tauri::command]
async fn wipe_credentials(state: State<'_, Arc<AppState>>) -> Result<usize, String> {
    wipe_all_credentials(state.inner())
}

// ---------------------------------------------------------------------------
// Pairing commands (F-12)
// ---------------------------------------------------------------------------

/// Initiate pairing with a peer (F-12): issue a 6-digit code *on this machine*
/// and return it for display. The other side reads it off our screen and enters
/// it there; its `POST /pair` lands here, where the code is verified and the
/// token exchanged. Codes live 2 minutes and are single-use.
#[tauri::command]
fn pair_start(peer_id: String, state: State<'_, Arc<AppState>>) -> Result<String, String> {
    ensure_online(&state)?;
    if state.get_peer(&peer_id).is_none() {
        return Err("设备不存在".to_string());
    }
    Ok(state.pair.create_code())
}

/// Confirm pairing with a code shown on the peer's screen. The peer verifies it
/// and hands back the token we store for it.
#[tauri::command]
async fn pair_confirm(peer_id: String, code: String, state: State<'_, Arc<AppState>>) -> Result<String, String> {
    ensure_online(&state)?;
    let peer = state.get_peer(&peer_id).ok_or_else(|| "设备不存在".to_string())?;
    let own_id = state.config.lock().device_id.clone();
    let own_name = state.config.lock().device_name.clone();

    // 用户点了「配对」= 明确要（重新）建立关系：这个地址上钉着的旧指纹先让位，
    // 否则这次拨号会在 TLS 那一跳被"指纹不一致"直接拒掉（见 `pair::forget_stale_pin`）。
    pair::forget_stale_pin(&state, &peer_id);
    let url = format!("{}/pair", net::peer_base(peer.dial().0, peer.dial().1));
    // Our own HTTP port: the peer we post to may be the side that dials the WS
    // link (smaller device id) and it has no other way to learn our address.
    let own_port = bound_port(&state);
    let client = net::client();
    let resp = client
        .post(&url)
        .json(&serde_json::json!({
            "peerId": own_id,
            "peerName": own_name,
            "peerPort": own_port,
            "code": code,
        }))
        .send()
        .await
        .map_err(|e| format!("配对确认失败: {e}"))?;

    if !resp.status().is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(format!("配对失败: {text}"));
    }

    let result: serde_json::Value = resp.json().await.map_err(|e| format!("响应解析失败: {e}"))?;
    let token = result["token"].as_str().ok_or_else(|| "未收到 token".to_string())?.to_string();
    let remote_id = result["peerId"].as_str().unwrap_or(&peer_id).to_string();
    let remote_name = result["peerName"].as_str().unwrap_or(&peer.name).to_string();

    // Store token under the peer's actual id + mark paired （与邀请那条路共用同一段落库：
    // 凭据、地址簿、BC profile 预写、界面事件）。
    let peer_name = remote_name.clone();
    pair::commit_pairing(
        state.inner(),
        pair::PairedPeer {
            id: remote_id,
            name: remote_name,
            ip: peer.ip.clone(),
            port: peer.port,
        },
        &token,
        None,
    )?;

    // 配对只确立"已配对"关系：长连接等用户自己点「连接」再建（不再自动拨号）。
    Ok(peer_name)
}

// ---------------------------------------------------------------------------
// Pairing invite commands (F-12 免配对码那条路)
// ---------------------------------------------------------------------------

/// 发起配对邀请：对端在它自己的通知里点「接收」即可完成配对，不需要任何人输入配对码。
/// 返回邀请 id；对方接受与否走 `pair://completed` / `pair://invite_failed` 事件回来。
#[tauri::command]
async fn pair_invite(peer_id: String, state: State<'_, Arc<AppState>>) -> Result<String, String> {
    ensure_online(&state)?;
    pair::invite_peer(state.inner().clone(), &peer_id).await
}

/// 撤回一条还没被处理的配对邀请（用户点了「取消」）：对端界面上那条通知跟着撤掉。
#[tauri::command]
async fn cancel_pair_invite(invite_id: String, state: State<'_, Arc<AppState>>) -> Result<(), String> {
    pair::withdraw_invite_remote(state.inner().clone(), &invite_id).await
}

/// 接收方：在通知里点「接收」——交换 token、两边都变「已配对」。返回对方设备名。
///
/// **同步命令是安全的**：它只做内存/文件写（凭据、地址簿、BC profile），不起后台任务。
#[tauri::command]
fn accept_pair_invite(invite_id: String, state: State<'_, Arc<AppState>>) -> Result<String, String> {
    pair::accept_invite(state.inner(), &invite_id)
}

/// 接收方：在通知里点「拒绝」。
#[tauri::command]
fn reject_pair_invite(invite_id: String, state: State<'_, Arc<AppState>>) {
    pair::reject_invite(state.inner(), &invite_id);
}

// ---------------------------------------------------------------------------
// Push commands (F-50 ~ F-54)
// ---------------------------------------------------------------------------

/// 推送本机文件/目录（用户在文件选择框里挑的，可多选）给**一台或多台**设备（F-50）。
///
/// 目标可以是任何在线设备：长连接、已配对未连、未绑定都能推（未绑定走 HTTP 邀请）。
/// 一次选择只开一次文件选择框；多选的文件合并成**每台一条**推送：对端只弹一张卡、
/// 只同意一次、一条进度。
///
/// 一次操作 = 一批：清单只建一次，每台各有一条独立的邀请（接收侧与单台推送完全一致）；
/// 不在网上的那几台**就地失败、不排队**（F-53），但一台都派不出去时整体报错
/// （界面弹一条提示，不建卡片 —— 与原来单台那条路一模一样）。
///
/// 返回整批快照；界面的卡片由 `push://batch` 事件推进（同一个 `batchId`，按它合并）。
///
/// `at` = 这批文件在**会话时间线**上的位置（发送方的墙上时间，epoch 毫秒）。只有"输入框里
/// 文字与图片交错着一次发出去"那条路会点名给它：一条消息里图片夹在前后两段文字中间，两端
/// 都要按这个顺序排（见 `stores/chat.svelte.ts` 的 `sendComposer`）。**没人点名时（普通推送）
/// 由后端当场取现在** —— 与文本消息同一个口径。
///
/// `names` = 与 `paths` **逐位对齐**的覆盖名（F-119，空串 = 这一项不改名），可以整个省掉。
/// 它只换"对端落盘用的那个名字"，本机读字节仍走 `paths` 那一条 —— 安卓「＋」里的「应用」
/// 就是靠它把 `/data/app/…/base.apk` 发成 `应用名_versionName(versionCode)_包名.apk`
/// （见 `transfer::manifest_for_named_paths`）。
#[tauri::command]
async fn push_paths(
    paths: Vec<String>,
    peer_ids: Vec<String>,
    names: Option<Vec<String>>,
    at: Option<u64>,
    state: State<'_, Arc<AppState>>,
) -> Result<transfer::push_job::BatchView, String> {
    ensure_online(&state)?;
    transfer::push_paths(state.inner().clone(), paths, peer_ids, names, at).await
}

/// 把剪贴板里的图片字节落成一个**暂存文件**，返回它的绝对路径（见 `paste`）。
///
/// 字节走 **base64 字符串**而不是 `Vec<u8>`：`invoke` 的载荷是 JSON，`Vec<u8>` 得摊成一串
/// 数字（一张 5 MB 的截图就是 20 MB 的 JSON 文本），base64 只多三分之一且是一个字符串。
///
/// 会话里发图片走的是推送，而推送只认本机真实路径 —— 这一步就是那中间的一跳。暂存落在系统
/// 临时目录（不是自家数据目录：那棵子树会被 `push_paths` 当成本机凭据拒掉）。
#[tauri::command]
fn save_pasted_image(data: String, ext: String) -> Result<String, String> {
    paste::save_base64(&data, &ext)
}

/// 一张落盘的图片 → `data:` URL：会话里画缩略图、点开看大图都用它。
#[tauri::command]
fn read_image_data_url(path: String) -> Result<String, String> {
    paste::preview_data_url(&path)
}

/// 发送方取消一整批推送：逐台走 `cancel_push`（两个阶段为什么都要通知对端，见那里）。
#[tauri::command]
async fn cancel_push_batch(batch_id: String, state: State<'_, Arc<AppState>>) -> Result<(), String> {
    transfer::cancel_push_batch(&state, &batch_id)
}

/// 接收方点「同意接收」：起下载任务。
///
/// **必须是 async 命令**：下载任务要在运行时上跑。写成同步命令的话它跑在主线程上 —— 那里
/// 没有 reactor，任务会被丢掉（用户侧："点了同意，两边都停在 0%"）。`task::spawn` 现在有
/// 兜底（退到 Tauri 的全局运行时），但异步命令才是这里最自然的上下文。
///
/// `root` = 这次接收的落盘根目录：界面在「每次询问」（`DownloadMode::AskEachTime`）下问过
/// 用户就带上，否则 `None`（用设置里的下载目录）。已配对设备那条路不问、也就恒为 `None`。
#[tauri::command]
async fn accept_push(
    invite_id: String,
    root: Option<String>,
    state: State<'_, Arc<AppState>>,
) -> Result<(), String> {
    let root = root.map(PathBuf::from);
    transfer::accept_invite(state.inner().clone(), &invite_id, root)
}

#[tauri::command]
fn reject_push(invite_id: String, state: State<'_, Arc<AppState>>) -> Result<(), String> {
    transfer::reject_invite(&state, &invite_id);
    Ok(())
}

/// 发送方取消：要顺带把撤销告诉对端（没有长连接时是 `POST /push/report`）—— 同样需要运行时。
#[tauri::command]
async fn cancel_push(invite_id: String, state: State<'_, Arc<AppState>>) -> Result<(), String> {
    transfer::cancel_push(&state, &invite_id)
}

/// 接收方取消：要回执发送方（同样可能走 `POST /push/report`）—— 同样需要运行时。
#[tauri::command]
async fn cancel_receive(invite_id: String, state: State<'_, Arc<AppState>>) -> Result<(), String> {
    transfer::cancel_receive(&state, &invite_id);
    Ok(())
}

// ---------------------------------------------------------------------------
// 会话（聊天）
// ---------------------------------------------------------------------------

/// 会话的整份状态（**一对一 + 群**）。面板打开时拉一次，之后由 `chat://changed` 推进
/// （载荷是同一个形状）。
#[tauri::command]
fn chat_snapshot(state: State<'_, Arc<AppState>>) -> group::Snapshot {
    group::snapshot(&state)
}

/// `chat_send` 的返回：这条消息的 id + 它挂在哪份群发名单上 + 整份状态。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatSendView {
    pub id: String,
    /// `None` = 一对一发的；否则是那份群发名单的 id。
    pub batch_id: Option<String>,
    pub snapshot: group::Snapshot,
}

/// 发一条文本消息给一台或多台设备；返回整份会话快照 + 这条消息的 id。
///
/// 一台设备存一份副本（**同一个消息 id**，各带各的送达状态）：群发给 B、C 之后，「与 B」
/// 「与 C」两条会话各自带着这条消息（1:1 是"与这台设备的完整往来"），"群发 · 名字"那条则是
/// "我发给这几台的记录"（按副本上的 `batchId` 筛）。
/// **对端永远只收到一条普通消息**（它不知道也不需要在某个"群"里），回复各自回到与发送方的 1:1。
///
/// `batch_id` = 从哪个群发里发的（空 = 一次新的群发：按成员集合复用已有名单，没有就新建）。
/// 落账与逐台投递都在 `chat::send_text`（集成测试与这里共用同一条路）。
///
/// `at` = 这条消息在会话时间线上的位置（发送方的墙上时间，epoch 毫秒）。只有"输入框里文字与
/// 图片交错着一次发出去"那条路会点名给它（见 `stores/chat.svelte.ts` 的 `sendComposer`）：
/// 那时前后两段文字的先后是被图片隔开的，得由界面统一编号；不点名就当场取现在。
#[tauri::command]
fn chat_send(
    peer_ids: Vec<String>,
    text: String,
    batch_id: Option<String>,
    at: Option<u64>,
    state: State<'_, Arc<AppState>>,
) -> Result<ChatSendView, String> {
    ensure_online(&state)?;
    let text = text.trim().to_string();
    if text.is_empty() {
        return Err("消息为空".to_string());
    }
    // 去重 + 只认本机设备表里有的 id（设备刚被移除时界面可能还捏着它）。
    let mut targets: Vec<String> = Vec::new();
    for id in peer_ids {
        if state.get_peer(&id).is_none() || targets.contains(&id) {
            continue;
        }
        targets.push(id);
    }
    if targets.is_empty() {
        return Err("收件人不存在".to_string());
    }
    let result = chat::send_text(&state, &targets, &text, batch_id.as_deref(), at);
    group::emit(&state);
    Ok(ChatSendView {
        id: result.id,
        batch_id: result.batch_id,
        snapshot: group::snapshot(&state),
    })
}

/// 发一张**图片消息**给一台或多台设备（输入框里粘一张图就是走这一条）。
///
/// 与 `chat_send`（文字）同一个口径：每台各存一份副本、逐台记送达状态、够不着的排队等它上线，
/// 区别只在内容换成那张图 —— **图片是消息内容，不是推送文件**：对端直接就能看，没有
/// "接收 / 同意"那一步，也不进"传输中"那本账。
///
/// `path` 是本机那份**暂存**图片（粘贴那一刻就落下的，见 `paste`）；这一条命令把字节读进来，
/// 落成会话内容（`blobs`）再发出去。`name` 是显示名（只用来取扩展名），`size` 由后端按读到的
/// 字节算。超过上限（`blobs::MAX_IMAGE_BYTES`）当场拒绝，**不改走推送** —— 那会弹出
/// "对方要不要接收"，正是这个需求要去掉的东西。
///
/// `at` 同 `chat_send`：输入区"图文混排"一次发三条时由界面统一编号。
#[tauri::command]
fn chat_send_image(
    peer_ids: Vec<String>,
    path: String,
    name: String,
    at: Option<u64>,
    batch_id: Option<String>,
    state: State<'_, Arc<AppState>>,
) -> Result<ChatSendView, String> {
    ensure_online(&state)?;
    let bytes = std::fs::read(&path).map_err(|e| format!("读不到那张图（{path}）：{e}"))?;
    // 去重 + 只认本机设备表里有的 id（与 `chat_send` 同一套）。
    let mut targets: Vec<String> = Vec::new();
    for id in peer_ids {
        if state.get_peer(&id).is_none() || targets.contains(&id) {
            continue;
        }
        targets.push(id);
    }
    if targets.is_empty() {
        return Err("收件人不存在".to_string());
    }
    let result = chat::send_image(&state, &targets, &name, &bytes, at, batch_id.as_deref())?;
    group::emit(&state);
    Ok(ChatSendView {
        id: result.id,
        batch_id: result.batch_id,
        snapshot: group::snapshot(&state),
    })
}

/// 界面读过这些设备的会话：推进本地已读水位，并给对端回一条已读回执。
#[tauri::command]
fn chat_mark_read(peer_ids: Vec<String>, state: State<'_, Arc<AppState>>) -> group::Snapshot {
    chat::mark_read(&state, &peer_ids);
    group::emit(&state);
    group::snapshot(&state)
}

/// 发送侧那张卡的**初值**：界面挂载时拉一次（`push://batch` 只管"变化"，重启后那些恢复出来的
/// 卡只能靠它拿到 —— 与 `chat_snapshot` 同一个路数：初值靠拉、变化靠推）。
#[tauri::command]
fn push_batches(state: State<'_, Arc<AppState>>) -> Vec<transfer::push_job::BatchView> {
    state.push.all_batch_views()
}

/// 会话（聊天）命令的返回里那一小截：这份群发名单的 id + 整份快照。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchRef {
    pub id: String,
    pub snapshot: group::Snapshot,
}

/// 「这批设备」用哪份群发名单：点名了 `id` 就用它（并把成员更新成这次选的这几台），
/// 没点名就按成员集合复用一份、都没有再新建（名字留空，随后由 `chat_batch_rename` 命名）。
///
/// 两条路都走它：**新建群发**（不点名 → 复用/新建）与会话里的**「编辑收件人」**（点名 →
/// 成员当场落定，不必等到下次发送）。`id` 已经不在名单里（另一边删掉了）时按"没点名"走。
#[tauri::command]
fn chat_batch_ensure(members: Vec<String>, id: Option<String>, state: State<'_, Arc<AppState>>) -> BatchRef {
    let id = state.chat.resolve_batch(id.as_deref(), &members);
    group::emit(&state);
    BatchRef { id, snapshot: group::snapshot(&state) }
}

/// 改群发的名字（新建时命名 / 会话里的「编辑收件人与群发名称」）。名字留空 = 「群发 · 未命名群发」。
#[tauri::command]
fn chat_batch_rename(id: String, name: String, state: State<'_, Arc<AppState>>) -> group::Snapshot {
    state.chat.rename_batch(&id, &name);
    group::emit(&state);
    group::snapshot(&state)
}

/// 删掉一份群发名单（会话菜单里的「删除群发」）。**消息一条不动**：它们本来就在各台会话里。
#[tauri::command]
fn chat_batch_delete(id: String, state: State<'_, Arc<AppState>>) -> group::Snapshot {
    state.chat.delete_batch(&id);
    group::emit(&state);
    group::snapshot(&state)
}

/// 清空与一台设备的聊天记录（会话菜单里的「清空聊天记录」）。
#[tauri::command]
fn chat_clear(peer_id: String, state: State<'_, Arc<AppState>>) -> group::Snapshot {
    state.chat.clear_peer(&peer_id);
    group::emit(&state);
    group::snapshot(&state)
}

/// 把一台设备的**文件消息记录**整份落盘（`chat::ChatFileMsg`）。
///
/// 界面是这些记录的实时作者（进度、邀请状态都在它手上），只在"能定下来的时刻"（刚发出 /
/// 刚收到 / 接受或拒绝 / 收完或失败 / 整批走完）把该设备那份整列表交回来 —— 所以这条命令
/// **不推 `chat://changed`**：界面手上已经有这份数据，推一份回去没有任何人要读。
#[tauri::command]
fn chat_files_set(
    peer_id: String,
    files: Vec<chat::ChatFileMsg>,
    state: State<'_, Arc<AppState>>,
) -> group::Snapshot {
    state.chat.set_files(&peer_id, files);
    group::snapshot(&state)
}

// ---------------------------------------------------------------------------
// 群聊（共享群）
// ---------------------------------------------------------------------------
//
// 与「群发」的区别：群发是**一批收件人**（对端只看到一条普通私聊，成员之间互不可见）；
// 群聊是**一个共享的群**（名册 + 群消息，群里任何一台发的消息所有成员都能看到）。
// 群成员**不需要配对**（D6=b）：名册里只有 id 与名字，投递走长连接优先、HTTP 兜底。

/// 群聊命令的返回里那一小截：这个群（或这条消息）的 id + 整份状态。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupRef {
    pub id: String,
    pub snapshot: group::Snapshot,
}

/// 新建一个共享群聊：名册当场定下来并广播给每个成员。
///
/// 对方收到名册 = 界面上一张**邀请卡**，点了「加入」才算进群（未知群的第一份名册要人确认）。
#[tauri::command]
fn group_create(members: Vec<String>, name: String, state: State<'_, Arc<AppState>>) -> Result<GroupRef, String> {
    let id = group::create(&state, &members, &name).ok_or_else(|| "至少要选一台设备".to_string())?;
    group::emit(&state);
    Ok(GroupRef { id, snapshot: group::snapshot(&state) })
}

/// 改群名 / 加减成员：名册版本 +1 后广播（对等的 LWW：任何成员都能改，冲突留一个胜者）。
#[tauri::command]
fn group_edit(id: String, members: Vec<String>, name: String, state: State<'_, Arc<AppState>>) -> Result<group::Snapshot, String> {
    if !group::edit(&state, &id, &members, &name) {
        return Err("群聊不存在".to_string());
    }
    group::emit(&state);
    Ok(group::snapshot(&state))
}

/// 在群里发一条文本（每台各一份；够不着的排队等它上线）。
///
/// `at` = 点名这条消息在时间线上的位置（输入区"图文混排"的每一段会点名，见 `chat_send`）。
#[tauri::command]
fn group_send(id: String, text: String, at: Option<u64>, state: State<'_, Arc<AppState>>) -> Result<GroupRef, String> {
    ensure_online(&state)?;
    let text = text.trim().to_string();
    if text.is_empty() {
        return Err("消息为空".to_string());
    }
    let msg_id = group::send_text(&state, &id, &text, at).ok_or_else(|| "本机不在这个群里".to_string())?;
    Ok(GroupRef { id: msg_id, snapshot: group::snapshot(&state) })
}

/// 在群里发一张**图片消息**（输入框里粘一张图就是走这一条）。
///
/// 与 `group_send` 同一个口径（每台各投一份、够不着的排队等它上线），区别只在内容换成那张图：
/// **图片是消息内容，不是文件** —— 群里的每个成员直接就能看，没有"接收 / 同意"那一步。
///
/// `path` 是本机那份**暂存**图片；后端把字节读进来、落成会话内容（`blobs`）再发出去。
/// 超过上限（`blobs::MAX_IMAGE_BYTES`）当场拒绝，不改走推送。
#[tauri::command]
fn group_send_image(
    id: String,
    path: String,
    name: String,
    at: Option<u64>,
    state: State<'_, Arc<AppState>>,
) -> Result<GroupRef, String> {
    ensure_online(&state)?;
    let bytes = std::fs::read(&path).map_err(|e| format!("读不到那张图（{path}）：{e}"))?;
    let msg_id = group::send_image(&state, &id, &name, &bytes, at)?
        .ok_or_else(|| "本机不在这个群里".to_string())?;
    Ok(GroupRef { id: msg_id, snapshot: group::snapshot(&state) })
}

/// 界面读过这个群：推进本地读位，并给每个成员回一条**逐发送者水位**的已读回执。
#[tauri::command]
fn group_mark_read(id: String, state: State<'_, Arc<AppState>>) -> group::Snapshot {
    group::mark_read(&state, &id);
    group::snapshot(&state)
}

/// 打开群会话时拉一次："我缺的那些，向能连上的成员要一遍"（补拉）。
///
/// 拿回来的东西**不是这一条命令的返回**：它由那一跳异步落账，落完照常推 `chat://changed`。
/// 所以这里只负责"叫一次"（自带闸门与冷却，重复调是安全的），返回当前这份状态。
#[tauri::command]
fn group_sync(id: String, state: State<'_, Arc<AppState>>) -> group::Snapshot {
    group::sync_group(&state, &id);
    group::snapshot(&state)
}

/// 用户点了邀请卡上的「加入」。
#[tauri::command]
fn group_join(id: String, state: State<'_, Arc<AppState>>) -> Result<group::Snapshot, String> {
    if !group::accept(&state, &id) {
        return Err("邀请已经不在列表里了".to_string());
    }
    group::emit(&state);
    Ok(group::snapshot(&state))
}

/// 用户点了「不加入」：记住这个决定，之后这个群的事件直接忽略。
#[tauri::command]
fn group_dismiss(id: String, state: State<'_, Arc<AppState>>) -> group::Snapshot {
    group::dismiss(&state, &id);
    group::emit(&state);
    group::snapshot(&state)
}

/// 退出群聊：本地删掉，并把"没有本机了"的名册广播给其他成员。
#[tauri::command]
fn group_leave(id: String, state: State<'_, Arc<AppState>>) -> Result<group::Snapshot, String> {
    if !group::leave(&state, &id) {
        return Err("群聊不存在".to_string());
    }
    group::emit(&state);
    Ok(group::snapshot(&state))
}

// ---------------------------------------------------------------------------
// 对端浏览（前端不再直连对端）
// ---------------------------------------------------------------------------

/// 调对端某个端点要用的地址、token、以及**刚拨的那个地址**（第三项是给"把这次见到的
/// 指纹钉住"用的 —— 钉的必须是**实际拨到的主机**：走中转时那是中转端点的字面量，与
/// 它自己的地址各占一个键位，见 `net::relay_host`）。
///
/// 一处拼地址 = 一处改协议：到对端的那一跳是 TLS + 自签证书，而 WebView 既不会认我们的
/// 证书、也不该让它认。所以对端浏览整体挪进后端（认证、指纹校验、token 只有一份实现）。
fn peer_endpoint(state: &AppState, peer_id: &str, path: &str) -> Result<(String, String, String), String> {
    let peer = state.get_peer(peer_id).ok_or_else(|| "设备不在列表中".to_string())?;
    if peer.ip.is_empty() || peer.port == 0 {
        return Err(format!("「{}」的地址未知（还没被发现或还没上线）", peer.name));
    }
    let token = credential::get_peer_token(peer_id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "未配对，无法读取对方的共享项".to_string())?;
    let (host, port) = peer.dial();
    Ok((
        format!("{}{path}", net::peer_base(host, port)),
        token,
        host.to_string(),
    ))
}

/// 调对端失败时给界面看的话。**指纹不一致要单独说**：它不是"它没开机"，重试也没用
/// （用户得去重新配对），跟「离线」混在一起会让人一直等一台永远连不上的设备。
fn peer_call_error(err: &reqwest::Error) -> String {
    if net::is_identity_rejected(err) {
        "对端证书与本机记住的不一致（它换过机器？）：需要重新配对".to_string()
    } else {
        format!("联系不上对端：{err}")
    }
}

/// 读对端"广播给本机"的共享项清单（对端标签页的数据源）。
#[tauri::command]
async fn peer_shares(
    peer_id: String,
    state: State<'_, Arc<AppState>>,
) -> Result<serde_json::Value, String> {
    let (url, token, ip) = peer_endpoint(state.inner(), &peer_id, "/shares")?;
    let response = net::client()
        .get(&url)
        .bearer_auth(&token)
        .send()
        .await
        .map_err(|e| peer_call_error(&e))?;
    if !response.status().is_success() {
        return Err(format!("读取失败: HTTP {}", response.status().as_u16()));
    }
    let body = response
        .json()
        .await
        .map_err(|e| format!("响应异常: {e}"))?;
    state::remember_peer_fingerprint(state.inner(), &peer_id, &ip);
    Ok(body)
}

/// 读对端某个共享项里的一层目录（对端标签页展开树用）。
#[tauri::command]
async fn peer_tree(
    peer_id: String,
    share_id: String,
    path: String,
    state: State<'_, Arc<AppState>>,
) -> Result<serde_json::Value, String> {
    let query = format!(
        "/tree?share={}&path={}",
        urlencode(&share_id),
        urlencode(&path)
    );
    let (url, token, ip) = peer_endpoint(state.inner(), &peer_id, &query)?;
    let response = net::client()
        .get(&url)
        .bearer_auth(&token)
        .send()
        .await
        .map_err(|e| peer_call_error(&e))?;
    if !response.status().is_success() {
        return Err(format!("读取失败: HTTP {}", response.status().as_u16()));
    }
    let body = response
        .json()
        .await
        .map_err(|e| format!("响应异常: {e}"))?;
    state::remember_peer_fingerprint(state.inner(), &peer_id, &ip);
    Ok(body)
}

// ---------------------------------------------------------------------------
// Download commands (F-40 / F-44)
// ---------------------------------------------------------------------------

/// 对端文件坐标（`peer_fs` 只认这个结构）：远端下载按它一段段要字节。
fn peer_file_of(
    peer: &crate::config::PeerDevice,
    share_id: &str,
    path_in_share: &str,
    token: &str,
) -> peer_fs::PeerFile {
    peer_fs::PeerFile {
        base: net::peer_base(peer.dial().0, peer.dial().1),
        token: token.to_string(),
        share_id: share_id.to_string(),
        path: path_in_share.to_string(),
    }
}

/// Download a file from a remote peer (P1 remote browsing download).
///
/// 返回落盘路径；**用户在保存框里取消时返回 `None`**（「每次询问」那一档）——
/// 界面据此当"什么都没发生"，不弹提示、不当失败。
#[tauri::command]
async fn download_from_peer(
    peer_id: String,
    share_id: String,
    path_in_share: String,
    // 共享项直接指向一个文件时 path_in_share 为空，文件名只能由调用方（对端界面）给出。
    file_name: Option<String>,
    state: State<'_, Arc<AppState>>,
    app: AppHandle,
) -> Result<Option<String>, String> {
    let (download_dir, mode) = {
        let cfg = state.config.lock();
        (cfg.download_dir.clone(), cfg.download_mode)
    };
    let peer = state.get_peer(&peer_id).ok_or_else(|| "设备不在列表中".to_string())?;
    let token = credential::get_peer_token(&peer_id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "未配对".to_string())?;
    let file_name = Path::new(&path_in_share)
        .file_name().map(|s| s.to_string_lossy().to_string())
        .or(file_name)
        .unwrap_or_else(|| "download".into());
    // 落盘位置遵循 S-02 下载模式，与「本机共享项下载」同一套规则。
    let target_dir = match mode {
        DownloadMode::AskEachTime => {
            use tauri_plugin_dialog::DialogExt;
            match app.dialog().file().set_file_name(&file_name)
                .blocking_save_file().map(|p| PathBuf::from(p.to_string()))
            {
                Some(p) => p,
                // 取消 = 不落盘（从前是"退回下载目录照下"：用户点了取消却还是拿到一份副本）。
                None => return Ok(None),
            }
        }
        DownloadMode::Direct => config::unique_path(&download_dir, &file_name),
    };
    let file_ref = peer_file_of(&peer, &share_id, &path_in_share, &token);
    let bytes = stream_peer_file(&peer_fs::client(), &file_ref, &target_dir).await?;
    info!(target = %target_dir.display(), bytes, "file downloaded");
    Ok(Some(target_dir.to_string_lossy().to_string()))
}

/// 把对端共享项里的一个文件**流式**写到 `target`：先写 `<name>.orbdrop-part`、写完再原子改名。
///
/// 为什么不是"`resp.bytes()` 读到内存再写盘"（曾经就是这么写的）：一个 4GB 的蓝光原盘就是
/// 4GB 常驻内存，内存不够时表现为"下载失败"甚至进程被杀。现在按 `peer_fs::READ_CHUNK`
/// （4MB，与镜像水合同一条粒度）一段段要、顺着写，内存只与一块挂钩。
///
/// 返回落盘的字节数。失败时**不留残骸**：`.part` 会被删掉（这条路上没有续传记录，
/// 留着一个"半成品"只会在下载目录里堆着）。
///
/// `pub`：它被集成测试直接调（这条路上过"整份进内存"的坑，得有东西钉着）。
pub async fn stream_peer_file(
    client: &reqwest::Client,
    file: &peer_fs::PeerFile,
    target: &std::path::Path,
) -> Result<u64, String> {
    if let Some(parent) = target.parent() {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("建目录失败: {e}"))?;
    }
    let part = target.with_file_name(format!(
        "{}.orbdrop-part",
        target
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "download".into())
    ));
    let mut out = match tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&part)
        .await
    {
        Ok(f) => f,
        Err(e) => return Err(format!("建临时文件失败: {e}")),
    };

    let mut offset: u64 = 0;
    let mut result: Result<(), String> = Ok(());
    {
        use tokio::io::AsyncWriteExt as _;
        loop {
            let want = peer_fs::READ_CHUNK;
            match peer_fs::fetch_file_range(client, file, offset, want).await {
                Ok(bytes) => {
                    if bytes.is_empty() {
                        break;
                    }
                    if let Err(e) = out.write_all(&bytes).await {
                        result = Err(format!("写入失败: {e}"));
                        break;
                    }
                    offset += bytes.len() as u64;
                    // 短读 = 到文件末尾（服务端按 `len` 截断，见 `peer_fs::fetch_file_range`）。
                    if (bytes.len() as u64) < want {
                        break;
                    }
                }
                Err(e) => {
                    result = Err(e);
                    break;
                }
            }
        }
        if result.is_ok() {
            if let Err(e) = out.flush().await {
                result = Err(format!("落盘失败: {e}"));
            } else if let Err(e) = out.sync_all().await {
                result = Err(format!("落盘失败: {e}"));
            }
        }
    }
    drop(out);

    if let Err(e) = result {
        let _ = tokio::fs::remove_file(&part).await;
        return Err(e);
    }
    if let Err(e) = tokio::fs::rename(&part, target).await {
        // 改名失败同样要清残骸：目标被别的进程占着（Windows 上很常见）或目标是个目录时
        // 都会走到这里，而这之后没人再管这个 `.orbdrop-part` 了 —— 留着它只会在下载目录里
        // 越积越多（文件已经收全但顶着临时名，用户看不见、也不会去删）。
        let _ = tokio::fs::remove_file(&part).await;
        return Err(format!("改名失败: {e}"));
    }
    Ok(offset)
}

/// 返回落盘路径；**用户在保存框里取消时返回 `None`**（「每次询问」那一档，见
/// `download_from_peer`）。与那条命令是同一套口径 —— 同一个设置、同一个行为。
#[tauri::command]
async fn download_file(
    share_id: String,
    path_in_share: String,
    state: State<'_, Arc<AppState>>,
    app: AppHandle,
) -> Result<Option<String>, String> {
    let (share, download_dir, mode) = {
        let config = state.config.lock();
        let share = config.shares.iter().find(|s| s.id == share_id)
            .ok_or_else(|| format!("共享项不存在: {share_id}"))?.clone();
        (share, config.download_dir.clone(), config.download_mode)
    };

    // 空路径（或只有目录）时退成**共享项自己的名字**：那正是"共享项根就是一个文件"那一档，
    // 旧写法退成 "download" —— 扩展名都没了，用户拿到一个打不开的文件。
    let file_name = Path::new(&path_in_share)
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| share.name.clone());

    let target_dir = match mode {
        DownloadMode::AskEachTime => {
            use tauri_plugin_dialog::DialogExt;
            match app.dialog().file().set_file_name(&file_name)
                .blocking_save_file().map(|p| PathBuf::from(p.to_string()))
            {
                Some(p) => p,
                // 取消 = 不落盘（见 `download_from_peer`：同一个口径）。
                None => return Ok(None),
            }
        }
        DownloadMode::Direct => config::unique_path(&download_dir, &file_name),
    };

    let port = state.server_port.load(std::sync::atomic::Ordering::SeqCst);
    if port == 0 {
        return Err("HTTP 服务未运行".into());
    }

    let url = format!("http://127.0.0.1:{port}/file?share={share_id}&path={}",
        urlencode(&path_in_share));

    let resp = reqwest::get(&url).await.map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("下载失败: HTTP {}", resp.status()));
    }
    let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
    if let Some(parent) = target_dir.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(|e| e.to_string())?;
    }
    tokio::fs::write(&target_dir, &bytes).await.map_err(|e| e.to_string())?;
    info!(target = %target_dir.display(), "file downloaded");
    Ok(Some(target_dir.to_string_lossy().to_string()))
}

/// 下载一份副本并在文件管理器里高亮它；用户在保存框里取消 → `None`（什么都不做）。
#[tauri::command]
async fn download_and_open(
    share_id: String,
    path_in_share: String,
    state: State<'_, Arc<AppState>>,
    app: AppHandle,
) -> Result<Option<String>, String> {
    let Some(path) = download_file(share_id, path_in_share, state, app.clone()).await? else {
        return Ok(None);
    };
    if let Err(e) = app.opener().reveal_item_in_dir(&path) {
        error!("Failed to reveal item: {e}");
    }
    Ok(Some(path))
}

/// 用系统默认方式打开一个路径：目录就落在文件管理器里。收完推送那条通知上的「打开目录」
/// 走这里（`dest` 是落盘那层 `<落盘目录>/<发送方名>`，是个目录）。
///
/// 路径已被移走/删掉时 `open_path` 会真的报错（先做一次 `metadata()` 探活）—— 不当成
/// 静默成功：界面要据此把那条通知**留住**，否则"东西落在哪"就随着气泡一起没了。
///
/// 不用 `reveal_item_in_dir`：那是"在父目录里高亮这一项"（还要再点一下才进去），
/// 而这里按的就是"打开目录"。
///
/// 移动端是**另一份定义**（下面那一条）：收件落在应用私有目录里，`file://` 递不出去
/// （安卓 7 起直接抛 `FileUriExposedException`），那边要的是 FileProvider 的 `content://`，
/// 而且 `.apk` 还得落到系统安装器上（F-109）。
#[cfg(not(mobile))]
#[tauri::command]
async fn open_path(path: String, app: AppHandle) -> Result<(), String> {
    app.opener()
        .open_path(path, None::<&str>)
        .map_err(|e| e.to_string())
}

/// 移动端的 `open_path`（同一条命令；上面那份说明里除了"打开方式"都还适用）。
///
/// `content://`（FileProvider，根声明在 `res/xml/file_paths.xml`）+ 按扩展名给的 MIME：
/// **`.apk` 那一档因此落到系统安装器上 —— 收到 APK 点一下就直接装**（F-109）。
#[cfg(mobile)]
#[tauri::command]
async fn open_path(path: String) -> Result<(), String> {
    mobile::open_file(path).await
}

/// 安卓：把输入法剪贴板塞进输入框的那个图片 URI（`content://`）拷成一份暂存文件，
/// 返回它的路径与字节数。
///
/// `content://` 在 WebView 的 JS 里读不出来（它属于 ContentResolver），而会话里发图片走的是
/// 推送 —— 推送只认本机真实路径。所以这一跳必须回原生，见 `mobile::resolve_image_uri`。
#[cfg(mobile)]
#[tauri::command]
async fn resolve_image_uri(uri: String) -> Result<mobile::PickedImage, String> {
    mobile::resolve_image_uri(uri).await
}

/// 安卓：开一次系统选择器（图片 / 文件，可多选），把选中的几项拷成应用缓存里的暂存文件，
/// 返回它们的路径 / 真实名字 / 大小。
///
/// 桌面端那颗「＋」开的是自绘的本机文件浏览器（`fs_browse` 那几条命令只在桌面端登记），安卓上
/// 没有对等物：分区存储下非媒体文件拿不到可读路径，只能走 SAF 的 URI 读授权 —— 而推送与
/// 会话图片都只认本机真实路径，所以 URI 必须在原生那一侧换成文件。取消回空表（**不是**错误）。
/// 机制与取舍见 `mobile::pick_files`。
#[cfg(mobile)]
#[tauri::command]
async fn pick_files(kind: String) -> Result<Vec<mobile::PickedFile>, String> {
    mobile::pick_files(kind).await
}

/// 安卓：列出设备上已安装的应用（F-119）—— 会话里那颗「＋」的「应用」那一项就是它。
///
/// 每一项带着自己的 `base.apk` 路径与**拼好并净化过的落盘名**；发送时把两者分别交给
/// `push_paths` 的 `paths` 与 `names`（源文件一个字节都不动，见
/// `transfer::manifest_for_named_paths`）。机制与取舍见 `mobile::list_installed_apps`。
#[cfg(mobile)]
#[tauri::command]
async fn list_installed_apps() -> Result<Vec<mobile::InstalledApp>, String> {
    mobile::list_installed_apps().await
}

/// 安卓：取一个应用的图标（`data:image/png;base64,…`，F-119）—— 表里**每一行**按需取一张。
///
/// 与 `list_installed_apps` 分开是为了"表先出来、图标随后补"：见 `mobile::app_icon`。
#[cfg(mobile)]
#[tauri::command]
async fn app_icon(package_name: String) -> Result<String, String> {
    mobile::app_icon(package_name).await
}

// ---------------------------------------------------------------------------
// 拖拽下载（把树里的任意文件/目录拖到资源管理器里落盘）
// ---------------------------------------------------------------------------

/// 登记一次拖拽意图（界面按下鼠标动起来就调它）。本机共享项当场返回可拖的票据，
/// 对端共享项返回"还在准备"的票据并开始在后台下到暂存目录 ——
/// 拖放循环要跟着按住的主键走，所以下载只能赶在起拖之前先跑起来。
///
/// **保持 async**：对端项要在后台拉元数据/内容，这活儿得在运行时上跑。
#[cfg(desktop)]
#[tauri::command]
async fn prepare_drag_out(
    req: drag::DragOutRequest,
    state: State<'_, Arc<AppState>>,
) -> Result<drag::DragTicket, String> {
    state.drag.prepare(state.inner(), &req)
}

/// 起拖：等暂存备好后在主线程跑 OLE 拖放（阻塞到用户松手）。返回时拖放已经结束。
#[cfg(desktop)]
#[tauri::command]
async fn start_drag_out(
    app: AppHandle,
    ticket: String,
    state: State<'_, Arc<AppState>>,
) -> Result<(), String> {
    drag::start(app, state.inner().clone(), ticket).await
}

fn urlencode(s: &str) -> String {
    let mut out = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char);
            }
            _ => out.push_str(&format!("%{:02X}", b)),
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Server lifecycle
// ---------------------------------------------------------------------------

#[tauri::command]
fn get_app_state(state: State<'_, Arc<AppState>>) -> Result<String, String> {
    let s = state.config.lock();
    Ok(format!("device_id={}, peers={}, port={}",
        s.device_id, s.peers.len(), s.port))
}

// ---------------------------------------------------------------------------
// 悬浮球窗口几何（展开方向 / 夹取）
// ---------------------------------------------------------------------------

/// 悬浮球所在显示器的工作区（物理像素），用于决定面板往哪边展开。
#[cfg(desktop)]
#[tauri::command]
fn orb_screen_metrics(window: WebviewWindow) -> Result<orb_window::ScreenMetrics, String> {
    orb_window::metrics(&window)
}

/// 一次性移动 + 缩放悬浮球窗口（物理像素）。
#[cfg(desktop)]
#[tauri::command]
fn orb_set_rect(window: WebviewWindow, x: i32, y: i32, w: i32, h: i32) -> Result<(), String> {
    orb_window::set_rect(&window, orb_window::Rect { x, y, w, h })
}

/// 开始拖动悬浮球窗口：先按按下点校正抓取锚点，再当场把拖动交给 OS。
///
/// 校正与起拖必须在同一次调用里完成（OS 是在起拖那一刻把窗口钉在指针上的，中间多一趟往返，
/// 指针就又走了一段）—— 见 `orb_window::drag_start`。
#[cfg(desktop)]
#[tauri::command]
fn orb_drag_start(window: WebviewWindow, press_x: f64, press_y: f64) -> Result<(), String> {
    orb_window::drag_start(&window, press_x, press_y)
}

/// 鼠标主键是否仍按着：拖动悬浮球时页面收不到 mouseup，贴边落位得靠它等松手。
#[cfg(desktop)]
#[tauri::command]
fn orb_mouse_down() -> bool {
    orb_window::mouse_down()
}

/// 窗口里「吃点击」的那几块（CSS px，窗口左上角为原点）。
///
/// 窗口比看得见的那件东西大一圈（四周是 `--shadow` 的余量），而透明像素在 Windows 上照样算
/// 命中 —— 余量里那一下点击会被我们的窗口吃掉，压在下面的东西收不到。所以由界面报「当前该吃
/// 指针的是哪几个盒子」，后端按指针位置开关整窗的 `WS_EX_TRANSPARENT`（见 `hit_region`）。
#[cfg(desktop)]
#[tauri::command]
fn orb_set_hit_region(window: WebviewWindow, rects: Vec<hit_region::RegionRect>) -> Result<(), String> {
    hit_region::set_region(&window, rects)
}

/// 记录悬浮体**本体中心**（球心 / 卡片白底中心；物理像素）：下次启动复原到同一位置（F-74）。
///
/// 由界面在**几何落定之后**调用（拖动松手 → 吸附判定跑完 → 窗口不再动），见 App.svelte
/// `rememberOrbCenter`；松手那一刻记下的是自由落点，贴边还会再挪一次窗口。位置没变就不落盘。
#[cfg(desktop)]
#[tauri::command]
fn orb_save_center(x: i32, y: i32, state: State<'_, Arc<AppState>>) -> Result<(), String> {
    let mut config = state.config.lock();
    if config.orb_center == Some(OrbPos { x, y }) {
        return Ok(());
    }
    config.orb_center = Some(OrbPos { x, y });
    config::save_config_at(&state.config_path, &config).map_err(|e| e.to_string())
}

/// 启动落位：有记录就回到记录的位置，没记录（首启 / 配置丢了）就放到默认落位。
///
/// 两种都在 `setup` 里做完、赶在首帧之前：窗口是按 tauri.conf 的 `center: true` 建的，
/// 等界面起来再挪会看到球从屏幕中央跳过去。落位之后界面还会对同一个矩形重跑一次吸附
/// 判定，复原贴边形态（吸附幂等，默认落位则因为余量大于阈值而不吸附）。
///
/// 记的是**本体中心**，窗口左上角 = 中心 − `collapsed_center_inset`（跟随形态与 DPI）。
/// 老配置存的是窗口左上角（`orbPos`）：这里顺手换算成本体中心并落盘，迁移只做一次。
#[cfg(desktop)]
fn place_orb_on_startup(app: &AppHandle) {
    let Some(win) = app.get_webview_window("main") else {
        warn!("未找到 main 窗口，跳过悬浮球落位");
        return;
    };
    let (shape, saved) = {
        let state = app.state::<Arc<AppState>>();
        let mut config = state.config.lock();
        let center = match config.orb_center {
            Some(c) => Some(c),
            // 老配置（`orbPos` = 窗口左上角）：换算成本体中心，并当场落盘完成迁移
            None => match config.orb_pos.take() {
                Some(legacy) => {
                    let sf = win.scale_factor().unwrap_or(1.0);
                    let c = config::orb_center_from_legacy(legacy, config.orb_shape, sf);
                    config.orb_center = Some(c);
                    if let Err(e) = config::save_config_at(&state.config_path, &config) {
                        warn!("迁移悬浮球落位（orbPos → orbCenter）落盘失败: {e}");
                    } else {
                        info!(x = c.x, y = c.y, "已把悬浮球落位从窗口左上角迁到本体中心");
                    }
                    Some(c)
                }
                None => None,
            },
        };
        (config.orb_shape, center)
    };
    // 折叠态尺寸先按形态定下来：tauri.conf 里只写了圆球那一档（90×90），圆角矩形悬浮框是
    // 236×162。等界面起来再改尺寸，悬浮框会先以 90×90 露出半张卡再跳大。
    let (width, height) = collapsed_logical(&app.config(), shape);
    if let Err(e) = win.set_size(tauri::LogicalSize::new(width, height)) {
        warn!("按形态设置折叠态窗口尺寸失败: {e}");
    }
    let (x, y) = match saved {
        // 上次那块显示器没了 / 分辨率变小：留在居中，否则球落到屏幕外、用户拖不回来
        // （透明无边框窗口没有标题栏）
        Some(pos) if !orb_window::visible_on_some_monitor(&win, pos.x, pos.y) => {
            warn!(x = pos.x, y = pos.y, "上次的悬浮球位置已不在任何显示器上，保持居中");
            return;
        }
        Some(center) => {
            let sf = win.scale_factor().unwrap_or(1.0);
            let (ix, iy) = config::collapsed_center_inset(shape);
            (center.x - (ix * sf).round() as i32, center.y - (iy * sf).round() as i32)
        }
        None => match orb_window::default_pos(&win, width) {
            Ok(p) => p,
            Err(e) => {
                warn!("算默认悬浮球落位失败，保持居中: {e}");
                return;
            }
        },
    };
    match win.set_position(tauri::PhysicalPosition::new(x, y)) {
        Ok(()) if saved.is_some() => info!(x, y, "已复原悬浮球位置"),
        Ok(()) => info!(x, y, "首启：悬浮球放在默认落位（右上 1/3，靠边不吸附）"),
        Err(e) => warn!("悬浮球落位失败: {e}"),
    }
}

/// 看门狗线程的代号：`Exit` 时 +1，跑着的那一份看到代号变了就自己退场
/// （与 `tray.rs` 那个闪动线程同一个套路：不 join、不阻塞退出）。
#[cfg(desktop)]
static ORB_WATCHDOG_GEN: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// 悬浮球窗口看门狗（桌面端）：把"窗口没了但进程还在"这一类事故从**不可恢复**变成**自己回来**。
///
/// **为什么非要有它**：全仓没有第二处会在运行时把窗口叫回来（`show`/`unminimize`/`set_focus`
/// 只在单实例那条分支里出现过一次），而这几件事都发生在运行中：
///
/// - 窗口被系统藏起来或被最小化（"显示桌面"、别的程序的系统动作、会话切换）——
///   tao 自己那份"可见"缓存察觉不到，前端的 `holdOrb` 之类状态更是无从知道；
/// - 窗口落在**没有任何显示器覆盖**的坐标上：拔掉显示器、分辨率变小、远控工具（UU 远程等在
///   被控端没有物理屏时会自动建虚拟屏）临时插进来的虚拟屏随后又撤掉 —— 启动那一次兜底
///   （`place_orb_on_startup`）只管启动那一刻，而这里的日志里"上次的悬浮球位置已不在任何
///   显示器上"实测出现过几十次，说明这种拓扑变化在这台机器上很常见；
/// - 显示器拓扑/缩放变了：tao 会按系统建议矩形**自己**搬窗口（`WM_DPICHANGED`），而前端
///   只在自己动手那一刻现读 `scaleFactor`，手上那份几何（吸附偏移、展开方向、命中区域）就过时了。
///
/// 用户看到的现象是同一件事："置顶窗口和主界面都不见了，托盘还在"（托盘图标由资源管理器画，
/// 跟本进程状态无关），而透明无边框窗口没有标题栏可以拖回来 —— 只能靠这里。
///
/// 空闲时（状态正常、拓扑没变）这个线程只做几次 `IsWindowVisible`/`GetMonitorInfo` 级别的查询，
/// 不碰窗口 —— 不影响 N-04（空闲近 0 CPU）。
#[cfg(desktop)]
fn start_orb_watchdog(app: tauri::AppHandle) {
    use std::sync::atomic::Ordering;

    let gen = ORB_WATCHDOG_GEN.fetch_add(1, Ordering::SeqCst) + 1;
    let spawned = std::thread::Builder::new().name("orb-watchdog".into()).spawn(move || {
        // 拓扑指纹的初值在第一次拍里取（那一刻窗口一定已经在，`setup` 里排在落位之后）
        let mut last: Option<u64> = None;
        loop {
            std::thread::sleep(orb_window::WATCHDOG_TICK);
            if ORB_WATCHDOG_GEN.load(Ordering::SeqCst) != gen {
                return; // 退出（或又起了一份）：自己退场
            }
            let Some(win) = app.get_webview_window("main") else {
                continue; // 窗口还没建好 / 正在退出：下一拍再看
            };

            let mut rescue = false;

            // 1) OS 眼里的真实状态：不可见或被最小化 → 叫回台面
            let (visible, minimized) = orb_window::os_state(&win);
            if !visible || minimized {
                warn!(visible, minimized, "看门狗：悬浮球窗口被藏起或最小化，叫回台面");
                if let Err(e) = orb_window::raise(&win) {
                    warn!("看门狗叫回窗口失败: {e}");
                }
                rescue = true;
            }

            // 2) 落在所有显示器之外 → 搬回最近的工作区
            match orb_window::clamp_into_work_area(&win) {
                Ok(true) => {
                    warn!("看门狗：悬浮球窗口已不在任何显示器上，搬回最近的工作区");
                    rescue = true;
                }
                Ok(false) => {}
                Err(e) => warn!("看门狗查询窗口位置失败: {e}"),
            }

            // 3) 拓扑/缩放变了 → 界面那份几何过时了，要它重排一次
            let fp = orb_window::topology_fingerprint(&win);
            match last {
                Some(prev) if prev == fp => {}
                Some(_) => {
                    info!("看门狗：显示器拓扑或缩放变了，要求界面重排几何");
                    rescue = true;
                }
                None => {}
            }
            last = Some(fp);

            // 救过就顺手逼一次重画：透明窗口"什么都没画"时 OS 侧一切正常，
            // 没有任何状态可以判据 —— 这一下是唯一不看运气就能把它逼回来的手段。
            if rescue {
                orb_window::force_repaint(&win);
                if let Err(e) = app.emit(ws::events::ui::ORB_RECOVER, ()) {
                    warn!("看门狗：通知界面重排几何失败: {e}");
                }
            }
        }
    });
    match spawned {
        Ok(_) => info!("悬浮球看门狗已启动（每 {:.0}s 一拍）", orb_window::WATCHDOG_TICK.as_secs_f64()),
        Err(e) => warn!("悬浮球看门狗线程起不来: {e}"),
    }
}

/// 折叠态窗口的标称尺寸（逻辑像素，宽 × 高）。
///
/// 圆球取 tauri.conf 里 main 窗口的 `width`/`height`（90×90）；圆角矩形悬浮框是**定尺**
/// 236×162 —— 它由卡片 176 + 右侧分页点那 30（间距 10 + 列宽 20）+ 四周阴影/角标余量拼出来，
/// 前端在 `src/lib/orbShape.ts` 的 `rectLayout` 里算同一个尺寸。**两处必须逐值一致**：
/// 这里只用来定首启的默认落位与首帧尺寸，界面起来后会按自己那份重设一次。
#[cfg(desktop)]
fn collapsed_logical(config: &tauri::Config, shape: OrbShape) -> (f64, f64) {
    match shape {
        OrbShape::Rect => (236.0, 162.0),
        OrbShape::Circle => {
            let win = config.app.windows.iter().find(|w| w.label == "main");
            (win.map(|w| w.width).unwrap_or(90.0), win.map(|w| w.height).unwrap_or(90.0))
        }
    }
}

// ---------------------------------------------------------------------------
// App setup
// ---------------------------------------------------------------------------

fn setup_server_task(app: &AppHandle) {
    let handle = app.clone();
    let state_arc = handle.state::<Arc<AppState>>().inner().clone();

    std::thread::spawn(move || {
        let rt = tokio::runtime::Runtime::new()
            .expect("Failed to create server tokio runtime");
        rt.block_on(async move {
            let arc_state = state_arc.clone();
            match server::ensure_server(arc_state.clone()).await {
                Ok(port) => {
                    info!(port, "HTTP server started");
                    // 上次退出时是「离线」就照旧不广播（`Config::online` 跨重启有效）：
                    // 服务器照常起（切回在线要立刻可用），只是不进 mDNS。
                    if arc_state.is_online() {
                        register_mdns(&arc_state, port);
                    }
                    // 服务已经起来了：把凭据库里钉住的指纹灌进 TLS 校验器读的那份表。
                    // 必须赶在 mDNS 发现/探活/长连接恢复之前 —— 那几条路一拨号就要校验。
                    state::refresh_trust(&arc_state);
                    let _ = handle.emit(ws::events::ui::SERVER_STARTED, port);

                    // Spawn mDNS scanner.
                    let (mdns_tx, mut mdns_rx) = mpsc::channel::<discover::PeerEvent>(64);
                    let own_id = arc_state.config.lock().device_id.clone();
                    tokio::spawn(discover::scan(mdns_tx, own_id));

                    // 推送批次走完之后要在宽限期到了才真丢（留给"回执还在路上 / 链路回来才对账"
                    // 的纠正，见 `push_job::SETTLED_GRACE`）—— 这里负责到点收掉。
                    {
                        let sweeper = arc_state.clone();
                        tokio::spawn(async move {
                            loop {
                                tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                                sweeper.push.sweep_settled_batches();
                            }
                        });
                    }

                    // Forward mDNS events to the state + UI.
                    let s2 = arc_state.clone();
                    tokio::spawn(async move {
                        use discover::PeerEvent;
                        while let Some(event) = mdns_rx.recv().await {
                            // 离线 = 隐身：本机不参与局域网。发现结果一概不登记（也不按
                            // "对端回来了"接回长连接）—— 离线期间设备列表就该是"谁都不在线"
                            // （`set_online` 已经把现有行标记成离线），切回在线后由 5s 探活
                            // 与 mDNS 重新解析纠正。
                            if !s2.is_online() {
                                continue;
                            }
                            match event {
                                PeerEvent::Online { id, name, ip, port, nets, relay } => {
                                    // 「它在喊」这件事先记下（F-106）：探活成不成是另一回事 ——
                                    // 列表要靠这一档把"在同一网段、但被 AP 隔开"的设备显示出来。
                                    // 「第一次看见它」（下面那个接回/flush 的触发判据）。
                                    let was_seen = s2.is_seen(&id);
                                    s2.note_seen(&id);
                                    // F-107：TXT 里自报的网段与中转能力。零请求就拿到了 ——
                                    // 之后 5s 探活那一拍会用活回话把这两项刷新（网卡换了 TXT
                                    // 不会重播，活回话才是权威）。
                                    s2.note_peer_nets(&id, &nets);
                                    if relay {
                                        s2.set_relay_capable(&id, true);
                                    }
                                    let is_paired = credential::has_peer(&id);
                                    // Already paired in our records → keep the flag.
                                    let paired = is_paired || {
                                        let peers = s2.peers.lock();
                                        peers.iter().any(|p| p.id == id && p.paired)
                                    };
                                    // ⚠️ **mDNS 记录只说"看得见"，不说"连得上"**：`online` 是
                                    // "5s 探活成功 = 真够得着"（DTO 与界面那颗点都这么读它）。
                                    // 这里以前写 `online: true`，于是"同段但被 AP 隔离"的设备
                                    // ——mDNS 看得见、单播全灭——会一直顶着"在线"的绿灯：而它
                                    // 其实一条消息都收不到（`seen` 那一档才是这时的真相）。
                                    // `upsert_peer` 对已有的行是 `||`（只增不减），所以这里给
                                    // `false` 不会把链路/中转刚证实的在线状态打回去。
                                    s2.upsert_peer(&PeerDevice {
                                        id: id.clone(),
                                        name,
                                        ip: ip.to_string(),
                                        port,
                                        online: false,
                                        paired,
                                        // 发现条目不带长连接意图：这一行只是观察值，意图由
                                        // `AppState::set_link_intent` 一处写（`upsert_peer`
                                        // 也不合并这两个字段，别在这里覆盖）。
                                        ..Default::default()
                                    });
                                    // mDNS is often the only source of a paired
                                    // peer's address (the credential store keeps
                                    // no address) — but it only fills the address
                                    // in: the link comes from a 「连接」 click or
                                    // from `restore_links_on_startup` on boot（或
                                    // 由下面这条"重新出现"的证据触发）。
                                    if !was_seen {
                                        ws::redial_if_remembered(&s2, &id);
                                    }
                                    // 刚才这一行还可能只是"把地址补上了"（之前地址簿里没有它，
                                    // 所以够不着只能排队）—— 现在有地址了，把排队的消息发出去。
                                    crate::chat::flush_queued(&s2, &id);

                                    // 一个地址上只能有一个身份：对端被重置过（数据目录
                                    // 清了 → 换了 device id）时，本机为旧 id 存的那一行
                                    // 还指着这个地址 —— 收拾掉它，别让同一台设备在列表里
                                    // 留两行（旧行"已绑定"、新行"未绑定"）。
                                    for stale_id in s2.stale_ids_at(&ip.to_string(), port, &id) {
                                        state::reconcile_stale_identity(&s2, &stale_id).await;
                                    }
                                }
                                PeerEvent::Offline(id) => {
                                    // 记录被撤（告别包 / TTL 过期）：不再"看得见"。
                                    s2.clear_seen(&id);
                                    // **记录没了不等于它不可达**：告别包/TTL 过期只说明"这条
                                    // mDNS 记录不在了"，而对端可能正在跟本机说话（组播被 Wi-Fi
                                    // 过滤、跨段、它刚换过网卡）。可达性交给 5s 探活去判，这里
                                    // 至多按"刚听到过"那一档收口 —— 少了这一条，一台正在收发的
                                    // 设备会被一次记录过期画成离线（见 `AppState::heard`）。
                                    s2.set_peer_offline_unless_heard(&id);
                                }
                            }
                        }
                    });

                    // Watch existing shares.
                    share::watcher::watch_all_shares(&arc_state);

                    // 恢复上次关机时的长连接（F-16）：地址簿里记着"还连着"的设备自动
                    // 拨回来——用户不用重新点一遍「连接」。
                    ws::restore_links_on_startup(&arc_state);

                    // 上次关机/强杀时还走在 HTTP 那一跳上的消息：那次尝试已经随进程没了 —— 退回落定的档，
                    // 再给每一台欠着的设备叫一次补发（对端一回应就会收到）。
                    chat::requeue_inflight(&arc_state);
                    chat::flush_all_queued(&arc_state);

                    // 上次关机/强杀时**没收完的那几条推送**：对端还持有那条推送的话，
                    // 自己接着收（块位图与 `.part` 都还在盘上，见 `transfer::resume_pending`）。
                    transfer::resume_pending(&arc_state);
                    // 发送端那一半：上次没走完的**发出去**的推送捡回来（job + 卡片）——
                    // 这样"两端都重启过"时，对面来要块照样要得到（见 `push_job::restore_outbound`）。
                    transfer::push_job::restore_outbound(&arc_state);

                    // Liveness backstop: mDNS only reports a departure when the peer
                    // sends goodbye or its records expire (up to ~2 min), and a
                    // re-registration under the same instance name may not be
                    // re-resolved at all. Probing the peer's public identity
                    // endpoint both demotes dead peers and promotes returned ones,
                    // so the device list reflects reachability within seconds.
                    // Peers with a live WS link need no probe.
                    //
                    // 探活认的是**身份**，不是"这个地址上有人答话"：同一个地址换过
                    // device id（对端清过数据）时必须认出来，否则旧行会永远"在线"，
                    // 与它真正的新身份并排显示成两行（见 `state::probe_step`）。
                    let probe_state = arc_state.clone();
                    tokio::spawn(async move {
                        loop {
                            // **启动即跑第一拍**（之后每 5s 一拍）。放在 `sleep` 之后的话，
                            // 地址簿里那份落盘的 `online` 就会当满 5 秒的"真相"：真在网的设备
                            // 先画成离线，要到第一拍探活才纠正 —— 用户看到的就是"启动后某台
                            // 离线、过几秒又在线"。真相只该由这一拍给（`AppState::new` 已经把
                            // 落盘值一律降级成离线，两者是同一件事的两半）。
                            //
                            // 离线（隐身）期间不发探活：本机对入站一律 503，探活只会把
                            // 一个"能回话的对端"反复标成在线，与状态框自相矛盾。
                            if probe_state.is_online() {
                                state::probe_peers_once(&probe_state).await;
                                // 没传完的那些接收任务：这一拍顺手看一眼（退避台阶在
                                // `resume::attempt_due` 里把关，"够不着"就只是跳过）。
                                transfer::resume_pending(&probe_state);
                                // 发出去、可对面一直没来续传的那些：过了窗口就按"放弃"收尾
                                // （发送端这一半的 TTL，见 `push_out::OUTBOUND_TTL_SECS`）。
                                transfer::push_job::sweep_outbound(&probe_state);
                            }
                            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                        }
                    });

                    // 本机网卡地址变了（插网线、开/关热点、切 Wi-Fi、VPN 起来）→ 通知界面
                    // 重拉左下角「端口 · IP」。**后端只负责"发现变化"**：值仍由界面自己拉
                    // （一份真相，不来回传），所以载荷是空的。
                    //
                    // mDNS 那一半不用这里管：`enable_addr_auto` 的 daemon 自己每 5s 查一次
                    // 网卡列表（mdns-sd 的 `check_ip_changes`），新网卡当场把服务播上去、
                    // 消失的网卡把地址撤掉。
                    let addr_state = arc_state.clone();
                    tokio::spawn(async move {
                        let mut last = server::local_ipv4_addresses();
                        loop {
                            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                            let now = server::local_ipv4_addresses();
                            if now != last {
                                info!(was = ?last, now = ?now, "本机网卡地址变化");
                                last = now;
                                ws::emit_ui(
                                    &addr_state,
                                    ws::events::ui::LOCAL_ADDRESSES,
                                    &serde_json::json!({}),
                                );
                            }
                        }
                    });

                    // Apply autostart per saved config.
                    #[cfg(desktop)]
                    if arc_state.config.lock().auto_start {
                        let _ = autostart::enable_autostart();
                    }

                    // 群聊补拉的**兜底节拍**：链路上线、对端被判在线、消息跳号都会立刻触发
                    // 一次，但"跨网段时对手握消息的那个中间节点"未必会有这些事件（它可能
                    // 一直在网、只是本机与它之间没有新消息）。每 60s 扫一遍，让每个成员各向
                    // 自己的邻居要一次 —— 缺的那一段就是靠这一拍顺着链路传过来的。
                    //
                    // 每（群，设备）自带 3s 冷却与轮数上限（`group::sync_gate_open`），
                    // 所以重复触发不会变成刷接口。
                    let sync_state = arc_state.clone();
                    tokio::spawn(async move {
                        loop {
                            tokio::time::sleep(std::time::Duration::from_secs(60)).await;
                            if !sync_state.is_online() {
                                continue;
                            }
                            group::sync_all(&sync_state);
                        }
                    });
                }
                Err(e) => {
                    error!("Failed to start HTTP server: {e}");
                    let _ = handle.emit(ws::events::ui::SERVER_ERROR, e);
                }
            }
            loop {
                tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
            }
        });
    });
}

/// 按落盘配置把应用状态建起来：读配置 → 起本机身份 → 升级凭据格式 → 建 `AppState`。
///
/// **两端调用时机不同**：桌面端在 `Builder` 之前调用（数据目录是普通 known folder，随手
/// 可得）；移动端必须等 Tauri 起来之后 —— Android 上应用私有目录只有 Tauri 的平台插件
/// 知道（`dirs` 在 Android 上没有可用实现，会一路退到应用写不进去的 `/data/local/tmp`），
/// 见 `run()` 里 `ORBDROP_DATA_DIR` 那一段。
fn boot_state() -> Arc<AppState> {
    // 日志目录挂在数据目录下（`log::init_log` 内部取 `config::data_dir`），所以这一步必须
    // 排在数据目录确定之后 —— 移动端正是这个原因把 `boot_state` 推迟到 `setup()` 里。
    log::init_log();

    let config = config::load_config();
    let config_path = config::config_file_path();

    // 启动第一行就写清"用什么配置起的"：出问题时（尤其 release 没有控制台）
    // 这行决定了能不能判断到没到 Tauri/服务那一步。
    info!(
        version = env!("CARGO_PKG_VERSION"),
        config = %config_path.display(),
        device_id = %config.device_id,
        device_name = %config.device_name,
        port = config.port,
        peers = config.peers.len(),
        shares = config.shares.len(),
        "OrbDrop 启动"
    );

    // 本机身份（自签证书 + 私钥）要在服务起来之前就位：没有它就没有 TLS，而对端之间的
    // 一切流量都走 TLS。失败是致命的第一步 —— 宁可在这里报清楚，也不要等到某次拨号
    // 才冒出一句"连接失败"。
    if let Err(e) = net::init() {
        error!("本机身份不可用（TLS 无法启用）: {e}");
    }
    // 本机的证书指纹绝不属于任何一台对端（来源是那条把本机自己的监听端点当对端地址的路，
    // 见 `credential::drop_own_fingerprint`）：启动时清一次，坏记录不该留到某次连线才发作。
    match net::load_or_create_identity() {
        // 身份都读不出来 = 上面已经报过错了，这里没什么可比的。
        Err(_) => {}
        Ok(identity) => match credential::drop_own_fingerprint(&identity.fingerprint) {
            Ok(0) => {}
            Ok(n) => info!(peers = n, "已抹掉钉在本机自己证书上的对端指纹"),
            Err(e) => warn!("清理本机指纹的记录失败（继续按原样用）: {e}"),
        },
    }

    // P1 写出的凭据文件是"头部 + base64 正文 + 署名"的明文遮挡，P2 是裸的 id→token 映射；
    // 现在每条记录还要带对端证书指纹。老文件照样读得回来，但只在启动时改写一次
    // （读路径绝不改用户的东西）。改不动不是致命错误：老格式的读兼容一直在，下次启动重来。
    match credential::upgrade_vault_format() {
        Ok(Some(peers)) => info!(peers, "已把凭据文件改写成当前格式"),
        Ok(None) => {}
        Err(e) => warn!("凭据文件格式改写失败（继续按老格式读）: {e}"),
    }

    let state = Arc::new(AppState::new(config, config_path));
    // 移动端：修掉"平台兜底"留下的下载目录与设备名（见 `mobile` 里那两个函数的说明）。
    #[cfg(mobile)]
    {
        mobile::repair_download_dir(&state);
        mobile::repair_device_name(&state);
    }
    state
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    let builder = tauri::Builder::default();

    // 单实例：第二份进程由插件负责收敛，**不能**让它自己静默退出 ——
    // 那在用户侧就是"启动一下就闪退"，而且什么提示都没有。这里把它变成
    // "把已在运行的实例叫到台面上来"（悬浮球面板直接展开）。
    //
    // 例外：`ORBDROP_ALLOW_MULTI=1` 时不挂这个插件 —— 一台机器上多开几份当"几台设备"
    // 是联调最省事的路子（真机验证"群发"至少要三个实例：A 发给 B、C）。多开时**每份
    // 必须有自己的 `ORBDROP_DATA_DIR` 和端口**：单实例锁按应用标识符取（插件在 Windows
    // 上开的是 `{identifier}-sim` 命名互斥体），同一个 exe 天然共享它，所以只能靠这个开关。
    #[cfg(desktop)]
    let single_instance = std::env::var_os("ORBDROP_ALLOW_MULTI").is_none();
    #[cfg(desktop)]
    let builder = {
        if single_instance {
            builder.plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
                info!("另一实例被启动：把本实例调到前台并展开面板");
                if let Some(win) = app.get_webview_window("main") {
                    // 置顶 / 显示 / 不最小化 / 前台：与托盘那一档共用同一个 helper
                    // （这里的"显示"必须真的能显示 —— tao 缓存说可见时 `show()` 是空操作，
                    // 而窗口可能是被系统藏起来的，见 `orb_window::raise`）。
                    if let Err(e) = orb_window::raise(&win) {
                        warn!("单实例：叫回悬浮球窗口失败: {e}");
                    }
                    let _ = win.emit(ws::events::ui::ORB_SHOW, ());
                }
            }))
        } else {
            warn!("ORBDROP_ALLOW_MULTI 已设置：跳过单实例约束（本机多开，各自的数据目录/端口必须分开）");
            builder
        }
    };

    let builder = builder
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init());

    // 收到消息的通知（F-108）：只有手机有通知栏。注册得在 `setup` 之前 —— 句柄在那一刻
    // 才拿得到，而收消息可能在 `setup` 之后任何一刻发生。
    #[cfg(mobile)]
    let builder = builder.plugin(notify::plugin());

    // 「用系统应用打开一个文件」在安卓上得自己递（`content://` + MIME），TAURI 那个 opener 插件
    // 的移动端分支是坏的 —— 见 `mobile::open_file`。
    #[cfg(mobile)]
    let builder = builder.plugin(mobile::open_plugin());

    // 「粘贴进来的那张图」：输入法剪贴板往输入框里插的是一个 `content://`，JS 读不出来，
    // 得请原生那一跳拷成暂存文件（会话里发图片走推送，只认本机路径）—— 见 `mobile::resolve_image_uri`。
    #[cfg(mobile)]
    let builder = builder.plugin(mobile::paste_plugin());

    // 会话里那颗「＋」（F-118）：桌面端开的是自绘的本机文件浏览器，安卓上没有对等物
    // （分区存储不给"按路径列共享存储"）—— 换成系统选择器，选中的 URI 由原生拷成暂存文件，
    // 界面拿到的是一条能直接进推送 / 会话图片的路径。见 `mobile::pick_files`。
    #[cfg(mobile)]
    let builder = builder.plugin(mobile::pick_plugin());

    // 「发送应用」（F-119）：会话里那颗「＋」的第三项要列设备上已安装的应用（各自的 base.apk）。
    // 列表由原生查（包可见性靠清单里的 `<queries>`），名字（`应用名_版本号(版本名)_包名.apk`）
    // 与净化在 Rust 这一侧拼 —— 与接收侧落盘那道闸门同一份判据。见 `mobile::list_installed_apps`。
    #[cfg(mobile)]
    let builder = builder.plugin(mobile::apps_plugin());

    // 引擎太低（WebView 低于下限）时兜底页上那个「去应用商店更新 WebView」按钮：那种状态下前端是死的，
    // 浮层只能发一次 `market://` 导航，由这个插件的导航钩子接住换成系统 Intent —— 见 `mobile::store_plugin`。
    #[cfg(mobile)]
    let builder = builder.plugin(mobile::store_plugin());

    // 「让应用在后台活着」所需的系统那一层（F-87）：电池优化白名单、ROM 的自启动/后台活动页。
    // 那一层是 ROM 私有的，应用改不了只能引导 —— 见 `system_settings` 的模块注释。
    #[cfg(mobile)]
    let builder = builder.plugin(system_settings::plugin());

    // 界面（Activity / WebView）还在不在：安卓把 Activity 销毁之后，Rust 叫 Kotlin 那条路
    // （wry 的 `dispatch`）会 panic，而 release 档 `panic = "abort"` 是整个进程倒地 ——
    // 见 `notify::set_ui_alive`。这里只记账，判断在发通知那一步。
    #[cfg(mobile)]
    let builder = builder.on_window_event(|_window, event| match event {
        tauri::WindowEvent::Destroyed => notify::set_ui_alive(false),
        // 界面又回来了（换配置重建、从最近任务点回来）：通知这条路跟着恢复。
        tauri::WindowEvent::Resumed => notify::set_ui_alive(true),
        _ => {}
    });

    // 桌面端：数据目录随时可得，状态在这里就建好，命令与 setup 都能直接用。
    #[cfg(desktop)]
    let builder = builder.manage(boot_state());

    builder
        .invoke_handler(tauri::generate_handler![
            add_share,
            list_shares,
            remove_share,
            rename_share,
            toggle_broadcast,
            get_settings,
            update_settings,
            #[cfg(desktop)]
            bc_status,
            #[cfg(desktop)]
            bc_set_left,
            #[cfg(desktop)]
            bc_compare_with_left,
            get_actual_port,
            local_addresses,
            list_devices,
            list_device_remarks,
            set_device_remark,
            relay_memory,
            clear_relay_memory,
            relay_status,
            close_relays,
            relay_traffic,
            add_peer,
            remove_peer,
            pair_start,
            pair_confirm,
            pair_invite,
            cancel_pair_invite,
            accept_pair_invite,
            reject_pair_invite,
            get_peer_token,
            peer_shares,
            peer_tree,
            connect_peer,
            disconnect_peer,
            accept_link_request,
            reject_link_request,
            cancel_link_request,
            wipe_credentials,
            #[cfg(desktop)]
            fs_browse::fs_drives,
            #[cfg(desktop)]
            fs_browse::fs_list,
            #[cfg(desktop)]
            fs_browse::path_picks,
            #[cfg(desktop)]
            list_picker_favorites,
            #[cfg(desktop)]
            set_picker_favorites,
            push_paths,
            push_batches,
            save_pasted_image,
            read_image_data_url,
            cancel_push_batch,
            accept_push,
            reject_push,
            cancel_push,
            cancel_receive,
            chat_snapshot,
            chat_send,
            chat_send_image,
            chat_mark_read,
            chat_batch_ensure,
            chat_batch_rename,
            chat_batch_delete,
            chat_clear,
            chat_files_set,
            group_create,
            group_edit,
            group_send,
            group_send_image,
            group_mark_read,
            group_sync,
            group_join,
            group_dismiss,
            group_leave,
            download_from_peer,
            download_file,
            #[cfg(mobile)]
            mobile::list_inbox,
            #[cfg(mobile)]
            mobile::delete_inbox_file,
            #[cfg(mobile)]
            resolve_image_uri,
            #[cfg(mobile)]
            pick_files,
            #[cfg(mobile)]
            list_installed_apps,
            #[cfg(mobile)]
            app_icon,
            // 「让应用在后台活着」所需的系统那一层（F-87）：读回电池优化白名单在不在、把用户送到
            // ROM 的自启动页；外加通知这一层的自查与跳转（F-108 的横幅那一半）。
            // 全是"读状态 + 跳系统页"，没有业务逻辑 —— 只有安卓有实体，见 `system_settings`。
            #[cfg(mobile)]
            system_settings::keepalive_state,
            #[cfg(mobile)]
            system_settings::request_ignore_battery_optimizations,
            #[cfg(mobile)]
            system_settings::open_autostart_settings,
            #[cfg(mobile)]
            system_settings::keepalive_guided,
            #[cfg(mobile)]
            system_settings::mark_keepalive_guided,
            #[cfg(mobile)]
            notify::notification_state,
            #[cfg(mobile)]
            notify::open_notification_settings,
            #[cfg(mobile)]
            notify::open_message_channel_settings,
            download_and_open,
            open_path,
            #[cfg(desktop)]
            prepare_drag_out,
            #[cfg(desktop)]
            start_drag_out,
            start_http_server,
            set_online,
            get_app_state,
            #[cfg(desktop)]
            orb_screen_metrics,
            #[cfg(desktop)]
            orb_set_rect,
            #[cfg(desktop)]
            orb_drag_start,
            #[cfg(desktop)]
            orb_mouse_down,
            #[cfg(desktop)]
            orb_set_hit_region,
            #[cfg(desktop)]
            orb_save_center,
            // 托盘上有事等处理吗（未读消息 / 通知 / 群邀请）：界面说一声，托盘据此挂红点 + 闪动。
            #[cfg(desktop)]
            tray::tray_set_alert,
            // 自更新（桌面端 + 安卓端同一套，见 `src/update/`）。
            update::update_state,
            update::update_check,
            update::update_apply,
        ])
        .setup(|app| {
            // 移动端：应用私有目录只有 Tauri 的平台插件知道，所以状态推迟到这里建 —— 先把
            // 目录经 `config::data_dir` 的既有覆盖口固定下来，再走同一条 `boot_state`。
            #[cfg(mobile)]
            {
                let private_dir = app.path().app_data_dir();
                if let Ok(dir) = &private_dir {
                    // 再落一层同名叶子：`app_data_dir` 在 Android 上是应用数据根
                    // （`/data/user/0/<包名>`），Android 自己会在那里放 `files/`、`cache/`、
                    // `shared_prefs/` —— 我们的 `config.toml`/`identity.pem`/`logs/` 不该跟它们
                    // 混在一起。加这一层后与桌面 `%LOCALAPPDATA%\OrbDrop` 同构。
                    std::env::set_var("ORBDROP_DATA_DIR", dir.join("OrbDrop"));
                }
                // 顺序要紧：`boot_state` 第一件事就是 `log::init_log`，所以"数据目录落在哪"
                // 这行只能在它之后记 —— 之前记还没有 subscriber，等于白记。
                let state = boot_state();
                match &private_dir {
                    Ok(dir) => info!(dir = %dir.display(), "数据目录（应用私有）"),
                    // 拿不到不是致命错误：退到临时目录照样能跑，只是不持久。
                    Err(e) => error!("拿不到应用私有目录，数据将落在临时目录（不持久）: {e}"),
                }
                app.manage(state);
            }

            // Store the AppHandle so WS events can be emitted.
            let state_arc = app.state::<Arc<AppState>>();
            *state_arc.app_handle.lock() = Some(app.handle().clone());

            // 自更新：填本机版本、清上次更新留下的残渣、安排一次静默检查（延迟 8 秒）。
            // 放在这里而不是更早：两条平台路径的状态都已经建好（上面 desktop 的
            // `boot_state` / mobile 的私有目录那条），`update` 里要读配置也读得到。
            update::init(app.handle());

            // 透明无边框窗口在 Win11 上会带一条系统绘制的 1px 边框（沿 OS 圆角露在面板外，
            // 每次改窗口尺寸重画 → 表现为窗口最外缘闪烁的线），启动时抹掉。
            #[cfg(desktop)]
            {
                match app.get_webview_window("main") {
                    Some(win) => match orb_window::strip_os_frame(&win) {
                        Ok(()) => info!("已抹掉系统窗口边框与系统圆角"),
                        Err(e) => warn!("抹掉系统窗口边框失败: {e}"),
                    },
                    None => warn!("未找到 main 窗口，跳过系统边框处理"),
                }

                // 窗口不在任务栏露面了，托盘是唯一的窗口外入口 —— 菜单得在这里就位，
                // 晚一步（比如等前端起来再挂）用户手上就有个没有出口的进程。
                match tray::setup(app.handle()) {
                    Ok(()) => info!("托盘菜单已就绪（右键可退出）"),
                    Err(e) => warn!("托盘菜单挂载失败: {e}"),
                }
            }

            // 悬浮球回到上次的位置；没有记录（首启）就放到默认落位（右上 1/3）
            #[cfg(desktop)]
            place_orb_on_startup(app.handle());

            // 运行中的兜底：窗口被系统藏起来 / 被最小化 / 落在所有显示器之外 / 显示器拓扑变了，
            // 都得有人把它叫回来（上面那次落位只管启动那一刻，见 `start_orb_watchdog`）。
            #[cfg(desktop)]
            start_orb_watchdog(app.handle().clone());

            // 粘贴图片的暂存目录：清掉上次留下的过期文件（草稿只活在内存里，进程一退，
            // 那些还没发出去的图就只剩这一个回收时机了，见 `paste::MAX_AGE`）。
            paste::sweep();
            // 会话里图片消息的**内容**：清掉没人引用的（消息被裁剪、写了一半进程没了、
            // 旧版本留下的，见 `blobs`）。这一条要排在状态建好之后 —— 它要读两份账本。
            if let Some(state) = app.try_state::<Arc<AppState>>() {
                chat::sweep_blobs(&state);
            }

            // 镜像（拖对端项走这条路）：先按龄回收上次留下的项，再把 provider 接上
            // （注册 + 连接）。失败只记日志 —— 拖放那条路会当场把原因报给用户。
            #[cfg(desktop)]
            {
                mirror::sweep();
                mirror::ensure_ready(state_arc.inner().clone());
            }

            setup_server_task(app.handle());

            // 尽早把 BC 的 profile 文件立起来（含所有已配对设备）：
            // BC 只在启动时登记 profile 文件，晚了就得等它重启才认。
            #[cfg(desktop)]
            {
                let written = bc::provision_profiles(&state_arc);
                info!(written, "启动预写 BeyondCompare profile 完成");
            }
            Ok(())
        })
        .build(tauri::generate_context!())
        .expect("error while building tauri application")
        .run(|_app, event| {
            // 退出时松开镜像连接。**注册留着**：注册一次长期存在（注销是 O(N)，实测 10,100 个
            // 占位符 1.9 s），下次启动 `mirror::ensure_ready` 只做连接；注销属于卸载路径。
            if let tauri::RunEvent::Exit = event {
                // 看门狗跟着退场（它每一拍都会查窗口，退出后那些查询没有意义）
                #[cfg(desktop)]
                ORB_WATCHDOG_GEN.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                #[cfg(desktop)]
                mirror::shutdown();
            }
        });
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一个临时共享项根：`a.txt` + `sub/` + 一个 `.hidden`（后者 `/tree` 与计数都跳过）。
    ///
    /// **每个调用各拿一个目录**：只按 pid 取名字的话，同进程里两个用它的用例并行跑会互相
    /// `remove_dir_all`（实测：`probe_share` 那两个用例会偶发少看到文件）。
    fn fixture_dir() -> PathBuf {
        let dir = std::env::temp_dir()
            .join(format!("orbdrop_probe_{}_{}", std::process::id(), uuid::Uuid::new_v4()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("sub")).unwrap();
        std::fs::write(dir.join("a.txt"), b"hello world").unwrap(); // 11 字节
        std::fs::write(dir.join(".hidden"), b"x").unwrap();
        dir
    }

    /// 卡片副标题的那两个数：目录给**直接**子项数（跳过 `.` 开头的项，与 `/tree` 展开后
    /// 看到的行数一致），文件给大小；两者互斥，拿不到时是 `None`（界面退回「文件夹 / 文件」）。
    #[test]
    fn probe_share_counts_one_level_and_sizes_files() {
        let dir = fixture_dir();
        let share = ShareItem::new(dir.clone());

        let probe = probe_share(&share.path);
        assert!(probe.is_dir);
        assert_eq!(probe.item_count, Some(2), "a.txt + sub；.hidden 不计入");
        assert_eq!(probe.size, None, "目录不给大小：那是递归遍历");

        let file_probe = probe_share(&dir.join("a.txt"));
        assert!(!file_probe.is_dir);
        assert_eq!(file_probe.size, Some(11));
        assert_eq!(file_probe.item_count, None, "文件不给子项数");

        let gone = probe_share(&dir.join("no-such-entry"));
        assert_eq!(
            (gone.is_dir, gone.size, gone.item_count),
            (false, None, None),
            "路径没了：不猜类型，两个数都不给"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 字段名是前端能看到的一切（`ShareItem.isDir` / `itemCount` / `size`）：改错了整张卡
    /// 会退化成「文件」，而且不报错。
    #[test]
    fn share_item_dto_keeps_its_wire_field_names() {
        let dir = fixture_dir();
        let share = ShareItem::new(dir.clone());
        let json = serde_json::to_value(ShareItemDto::new(&share, probe_share(&share.path))).unwrap();
        assert_eq!(json["isDir"], true);
        assert_eq!(json["itemCount"], 2);
        assert!(json["size"].is_null(), "目录的大小是 null，不是 0");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 手动添加（F-11）：**留空端口**时要在端口段里找到真正答话的那一台，且**跳过本机自己**。
    ///
    /// 搭法照着真实场景：本机先占住起点（`BASE`），对端随后起来、按 S-06 顺延到 `BASE+1`。
    /// 于是"留空端口的添加"必须**跳过 BASE 上那个自己**、找到 `+1` 上的对端 —— 只探起点、
    /// 或者把本机自己也当成对端，都会在这里失败。
    ///
    /// ⚠️ 拨的地址用**本机的内网地址**，不用 `127.0.0.1`：指纹表的键是**地址**（`net::Trust`
    /// 的 `pinned: HashMap<IpAddr, _>`），而同一进程里别的用例早就在 `127.0.0.1` 上钉过别的
    /// 证书（它们用的身份文件与这里不是同一份）—— 用回环就会在 TLS 那一跳被判"指纹不一致"，
    /// 假红。内网地址没有这个污染面。拿不到内网地址（纯离线）就跳过：这条路需要真的握手。
    #[tokio::test]
    async fn manual_add_without_a_port_finds_the_peer_and_skips_itself() {
        const BASE: u16 = 4890;
        let Some(lan) = local_lan_ip() else {
            eprintln!("这台机器没有内网地址（纯离线）：跳过");
            return;
        };
        let lan = lan.to_string();
        let own_dir = std::env::temp_dir().join(format!("orbdrop_add_own_{}", uuid::Uuid::new_v4()));
        let peer_dir = std::env::temp_dir().join(format!("orbdrop_add_peer_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&own_dir).unwrap();
        std::fs::create_dir_all(&peer_dir).unwrap();
        // 身份用 `test_config`：这一条**故意绑 0.0.0.0**（它要拨本机的内网地址，见上面那段
        // 说明），所以它是测试里唯一会把监听放到局域网上的地方 —— 名字中立、id 各取各的，
        // 对端看到的只是一台 `orbdrop-test`，不会与真机撞名。
        let own = Arc::new(AppState::new(
            crate::config::test_config(),
            own_dir.join("config.toml"),
        ));
        let peer = Arc::new(AppState::new(
            crate::config::test_config(),
            peer_dir.join("config.toml"),
        ));
        let own_id = own.config.lock().device_id.clone();
        let peer_id = peer.config.lock().device_id.clone();

        let own_port = crate::server::start_server(own.clone(), BASE).await.unwrap();
        assert_eq!(own_port, BASE, "本机占住起点");
        let peer_port = crate::server::start_server(peer.clone(), BASE).await.unwrap();
        assert_eq!(peer_port, BASE + 1, "对端起在起点上：按 S-06 顺延到 +1");

        // 两个服务器都要**真的答话**了再扫：扫描每一步只等 1.5s（真机上够，满载的测试进程里
        // 不够），先等它起来，这条用例才不会在并行跑时假红。
        let client = crate::net::client();
        for port in [own_port, peer_port] {
            let url = format!("{}/pair/info", crate::net::peer_base(&lan, port));
            let ok = wait_until_async(|| {
                let (client, url) = (client.clone(), url.clone());
                async move {
                    client
                        .get(&url)
                        .send()
                        .await
                        .map(|r| r.status().is_success())
                        .unwrap_or(false)
                }
            })
            .await;
            assert!(ok, "服务器还没起在 {lan}:{port} 上");
        }

        let (found, info) = manual_peer_from(&lan, 0, BASE, &own_id).await.unwrap();
        assert_eq!(found, BASE + 1, "留空端口要扫到真正答话的那一台（自己的那一个跳过）");
        assert_eq!(info["id"].as_str(), Some(peer_id.as_str()), "找到的是对端，不是本机");
        assert_eq!(info["port"].as_u64(), Some(u64::from(BASE + 1)), "回话里是它实际的端口");

        // 明确指到自己头上：报错要说清"是本机自己"，而不是"连不上"（用户才知道错在哪）。
        let self_err = manual_peer_from(&lan, BASE, BASE, &own_id).await.unwrap_err();
        assert!(self_err.contains("本机自己"), "是把本机自己填进来的：{self_err}");

        // 明确给了对端端口就只问那一个。
        assert_eq!(manual_peer_from(&lan, BASE + 1, BASE, &own_id).await.unwrap().0, BASE + 1);

        crate::server::stop_server(&own).await;
        crate::server::stop_server(&peer).await;
        let _ = std::fs::remove_dir_all(&own_dir);
        let _ = std::fs::remove_dir_all(&peer_dir);
    }

    /// 手动添加的另一半（F-11）：本机添加对端之后，**对端的设备表里也要有本机这一行**。
    ///
    /// 跨网段那台"添加得上、却只有本机看得见对方"时，它连配对都发起不了（`pair_confirm`
    /// 要求设备行已存在）—— 这条自报补的就是那一半。这里走**真 TLS + 真 HTTP**：路由名、
    /// 字段名、来源地址的算法、以及"对端真的建了一行"只有打过去才知道。地址用**本机的内网
    /// 地址**（理由同 `manual_add_without_a_port_finds_the_peer_and_skips_itself`：回环上那枚
    /// 指纹会被别的用例污染）。
    #[tokio::test]
    async fn a_manual_add_shows_up_on_the_peer_as_an_unbound_row() {
        const BASE: u16 = 4900;
        let Some(lan) = local_lan_ip() else {
            eprintln!("这台机器没有内网地址（纯离线）：跳过");
            return;
        };
        let lan = lan.to_string();
        let own_dir = std::env::temp_dir().join(format!("orbdrop_hello_own_{}", uuid::Uuid::new_v4()));
        let peer_dir = std::env::temp_dir().join(format!("orbdrop_hello_peer_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&own_dir).unwrap();
        std::fs::create_dir_all(&peer_dir).unwrap();
        let own = Arc::new(AppState::new(
            crate::config::test_config(),
            own_dir.join("config.toml"),
        ));
        let peer = Arc::new(AppState::new(
            crate::config::test_config(),
            peer_dir.join("config.toml"),
        ));
        let own_id = own.config.lock().device_id.clone();
        let peer_id = peer.config.lock().device_id.clone();

        let own_port = crate::server::start_server(own.clone(), BASE).await.unwrap();
        let peer_port = crate::server::start_server(peer.clone(), BASE + 1).await.unwrap();
        let client = crate::net::client();
        for port in [own_port, peer_port] {
            let url = format!("{}/pair/info", crate::net::peer_base(&lan, port));
            let ok = wait_until_async(|| {
                let (client, url) = (client.clone(), url.clone());
                async move {
                    client
                        .get(&url)
                        .send()
                        .await
                        .map(|r| r.status().is_success())
                        .unwrap_or(false)
                }
            })
            .await;
            assert!(ok, "服务器还没起在 {lan}:{port} 上");
        }

        // 真机那一步是 `add_peer`：先把对端记进本机地址簿（这一步本机自己那一半就成立了）。
        let target = crate::config::PeerDevice {
            id: peer_id.clone(),
            name: "PC-B".into(),
            ip: lan.clone(),
            port: peer_port,
            online: true,
            paired: false,
            ..Default::default()
        };
        own.add_peer_book(target.clone());

        // 然后是另一半：直接 await（不经过 `crate::task::spawn`）—— 测试里不等后台任务。
        notify_manual_add(own.clone(), target).await;

        let row = peer.get_peer(&own_id).expect("对端设备表里要有本机这一行");
        assert!(!row.paired, "只建行、不建关系");
        assert!(row.online, "刚打过招呼：这一行是活的");
        assert_eq!(row.ip, lan, "地址取对端观察到的 TCP 来源");
        assert_eq!(row.port, own_port, "端口取本机自报的那个（实际在听的那个）");
        // 关系确实没被顺手建立：本机这一侧那一行也还是未绑定。
        assert!(!own.get_peer(&peer_id).unwrap().paired);

        crate::server::stop_server(&own).await;
        crate::server::stop_server(&peer).await;
        let _ = std::fs::remove_dir_all(&own_dir);
        let _ = std::fs::remove_dir_all(&peer_dir);
    }

    /// 本机在局域网上的出口地址（拿不到 = 这台机器没有可用网卡，用它拨号的那几条用例跳过）。
    fn local_lan_ip() -> Option<std::net::IpAddr> {
        let socket = std::net::UdpSocket::bind("0.0.0.0:0").ok()?;
        socket.connect("192.0.2.1:9").ok()?;
        let ip = socket.local_addr().ok()?.ip();
        (!ip.is_loopback()).then_some(ip)
    }

    /// 等一个**异步**条件成立（最多 20 秒）。返回是否等到了。
    async fn wait_until_async<F, Fut>(mut f: F) -> bool
    where
        F: FnMut() -> Fut,
        Fut: Future<Output = bool>,
    {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(20);
        while std::time::Instant::now() < deadline {
            if f().await {
                return true;
            }
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        }
        false
    }

    /// 浏览 `_orbdrop._tcp` 若干秒，返回解析到的**服务全名**。
    ///
    /// 每一轮都新建一个 daemon（空缓存）：看到的只能是**此刻真在应答**的那些记录，
    /// 不带任何历史缓存 —— 这是"旧的还在不在网上"唯一诚实的问法。
    async fn browse_fullnames(secs: u64) -> Vec<String> {
        let daemon = match mdns_sd::ServiceDaemon::new() {
            Ok(d) => d,
            Err(e) => {
                eprintln!("mDNS 用不了（跳过）: {e}");
                return Vec::new();
            }
        };
        let rx = match daemon.browse("_orbdrop._tcp.local.") {
            Ok(r) => r,
            Err(e) => {
                eprintln!("mDNS 浏览起不来（跳过）: {e}");
                return Vec::new();
            }
        };
        let mut seen = Vec::new();
        let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(secs);
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            if left.is_zero() {
                break;
            }
            match tokio::time::timeout(left, rx.recv_async()).await {
                Ok(Ok(mdns_sd::ServiceEvent::ServiceResolved(info))) => {
                    seen.push(info.get_fullname().to_string());
                }
                Ok(Ok(_)) => {}
                _ => break,
            }
        }
        let _ = daemon.shutdown();
        seen
    }

    /// **改设备名之后，旧的那条 mDNS 记录必须从网上消失**。
    ///
    /// 这条用例只能是真组播：`ServiceDaemon` **没有 `Drop`**，只把句柄丢掉的话线程还活着、
    /// 注册还在，对端每次解析到它就把旧名字写回设备行一次 —— 表现正是"我改了名字，别人看到
    /// 的还是旧名"。同理 `Registration` 也必须是"注册那一刻的全名"（改过名之后按配置拼出来
    /// 的串已经指不到线上那一条了）。
    #[tokio::test]
    async fn renaming_releases_the_previous_mdns_registration() {
        let dir = std::env::temp_dir()
            .join(format!("orbdrop_mdns_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let state = Arc::new(AppState::new(
            crate::config::Config::default(),
            dir.join("config.toml"),
        ));
        let tag = uuid::Uuid::new_v4().to_string()[..8].to_string();
        let old = format!("mdns-old-{tag}");
        let new = format!("mdns-new-{tag}");

        state.config.lock().device_name = old.clone();
        register_mdns(&state, 3611);
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
        let before = browse_fullnames(5).await;
        if !before.iter().any(|f| f.contains(&old)) {
            // 这台机器播不出去（没有可用的非回环网卡之类的）：组播这条路验不了，不硬断言。
            eprintln!("mDNS 广播在这台机器上没成（旧名没被解析到），跳过：{before:?}");
            release_mdns(&state);
            let _ = std::fs::remove_dir_all(&dir);
            return;
        }

        // 改名 → 重注册（`update_settings` 走的就是这两句）。
        state.config.lock().device_name = new.clone();
        register_mdns(&state, 3611);
        tokio::time::sleep(std::time::Duration::from_secs(2)).await;

        let after = browse_fullnames(5).await;
        assert!(
            after.iter().any(|f| f.contains(&new)),
            "新名字要在网上：{after:?}"
        );
        assert!(
            !after.iter().any(|f| f.contains(&old)),
            "旧名字必须已经被撤下（注销 + 关掉 daemon）：{after:?}"
        );

        release_mdns(&state);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 改名失败（目标被占着 / 目标是个目录）**也要把 `.orbdrop-part` 清掉**。
    ///
    /// 收全了才改名，所以走到这条分支时临时文件里已经是完整数据 —— 可它顶着临时名躺在用户的
    /// 下载目录里，没人会去认它、更不会去删：每失败一次堆一个。对端是个只说 5 字节的桩
    /// （小于一个 `READ_CHUNK`，一轮就够），目标给一个**已存在的目录**让 `rename` 必失败。
    #[tokio::test]
    async fn a_failed_rename_does_not_leave_the_part_file_behind() {
        use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let stub = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf).await; // 桩不看请求内容
            sock.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 5\r\n\r\nhello").await.unwrap();
        });

        let dir = std::env::temp_dir()
            .join(format!("orbdrop_stream_part_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("occupied")).unwrap();
        let file = crate::peer_fs::PeerFile {
            base: format!("http://127.0.0.1:{port}"),
            token: "t".into(),
            share_id: "s".into(),
            path: "a.bin".into(),
        };

        let err = stream_peer_file(&reqwest::Client::new(), &file, &dir.join("occupied"))
            .await
            .expect_err("目标是个目录时改名必须失败");
        assert!(err.contains("改名失败"), "错误信息要说清: {err}");
        assert!(
            !dir.join("occupied.orbdrop-part").exists(),
            "改名失败后不能把临时文件留在下载目录里"
        );
        assert!(dir.join("occupied").is_dir(), "目标本身不能被动过");

        // 只 abort 不 await：桩要是没被连上（比如这条路在建连接之前就失败了），`await` 会
        // 永远等在那个 `accept()` 上，把用例吊死。
        stub.abort();
        let _ = std::fs::remove_dir_all(&dir);
    }
}

