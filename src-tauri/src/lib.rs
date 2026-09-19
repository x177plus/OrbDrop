pub mod autostart;
pub mod bc;
pub mod config;
pub mod credential;
pub mod discover;
pub mod drag;
pub mod fs_browse;
pub mod log;
pub mod orb_window;
pub mod pair;
pub mod server;
pub mod share;
pub mod state;
pub mod task;
pub mod transfer;
pub mod ws;

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, Manager, State, WebviewWindow};
use tauri_plugin_opener::OpenerExt;
use tokio::sync::mpsc;
use tracing::{error, info, warn};

use config::{AppSettings, BcTarget, DownloadMode, OrbPos, PeerDevice, PickerFavorite, ShareItem};
use state::AppState;

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
}

impl From<&ShareItem> for ShareItemDto {
    fn from(s: &ShareItem) -> Self {
        Self {
            id: s.id.clone(),
            name: s.name.clone(),
            path: s.path.to_string_lossy().to_string(),
            created_at: s.created_at.to_rfc3339(),
            broadcast_enabled: s.broadcast_enabled,
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
    /// BeyondCompare 可执行文件路径；空串 = 自动探测。
    #[serde(default)]
    pub bc_exe_path: String,
}

// ---------------------------------------------------------------------------
// Share commands (unchanged from P0)
// ---------------------------------------------------------------------------

#[tauri::command]
async fn add_share(path: String, state: State<'_, Arc<AppState>>) -> Result<ShareItemDto, String> {
    let p = PathBuf::from(&path);
    if !p.exists() {
        return Err(format!("路径不存在: {path}"));
    }
    let item = ShareItem::new(p);
    let dto = ShareItemDto::from(&item);
    let item_clone = item.clone();
    {
        let mut config = state.config.lock();
        config.shares.push(item);
        config::save_config_at(&state.config_path, &config).map_err(|e| e.to_string())?;
    }
    // Watch the new share
    share::watcher::watch_share(state.inner(), &item_clone.id, item_clone.path);
    notify_shares_changed(state.inner());
    info!(path, "share added");
    Ok(dto)
}

/// 选择框「常用」列表（F-50）：用户右键目录攒出来的快捷入口，随配置持久化。
#[tauri::command]
fn list_picker_favorites(state: State<'_, Arc<AppState>>) -> Vec<PickerFavorite> {
    state.config.lock().picker_favorites.clone()
}

/// 整份写回「常用」（界面给的就是它此刻显示的那份）；返回后端规整过的那份（去重后），
/// 界面以后端为准。**写完立刻落盘**：这几条是用户手动的选择，不能在退出时才写。
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

#[tauri::command]
fn list_shares(state: State<'_, Arc<AppState>>) -> Result<Vec<ShareItemDto>, String> {
    let config = state.config.lock();
    Ok(config.shares.iter().map(ShareItemDto::from).collect())
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
    let (old_port, old_name, old_autostart);
    {
        let mut config = state.config.lock();
        old_port = config.port;
        old_name = config.device_name.clone();
        old_autostart = config.auto_start;
        config.port = req.port;
        config.download_dir = PathBuf::from(&req.download_dir);
        config.download_mode = req.download_mode;
        config.device_name = req.device_name.clone();
        config.auto_start = req.auto_start;
        config.bc_exe_path = Some(PathBuf::from(req.bc_exe_path.trim()))
            .filter(|p| !p.as_os_str().is_empty());
        config::save_config_at(&state.config_path, &config).map_err(|e| e.to_string())?;
    }

    // Autostart change
    if req.auto_start != old_autostart {
        if req.auto_start {
            let _ = autostart::enable_autostart();
        } else {
            let _ = autostart::disable_autostart();
        }
    }

    // Device name changed → re-register mDNS (drop existing daemon, re-register)
    if req.device_name != old_name {
        *state.mdns_daemon.lock() = None; // drop old
        let device_id = state.config.lock().device_id.clone();
        if let Some(daemon) = discover::register(req.port, &req.device_name, &device_id) {
            *state.mdns_daemon.lock() = Some(daemon);
        }
    }

    // Port changed → restart HTTP server
    if req.port != old_port {
        let arc_state = state.inner().clone();
        let _ = tokio::spawn(async move {
            server::stop_server(&arc_state).await;
            match server::ensure_server(arc_state).await {
                Ok(port) => {
                    info!(port, "HTTP server restarted");
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

#[tauri::command]
fn bc_status(state: State<'_, Arc<AppState>>) -> bc::BcStatusDto {
    bc::status(state.inner())
}

#[tauri::command]
fn bc_set_side(
    side: String,
    target: BcTarget,
    state: State<'_, Arc<AppState>>,
) -> Result<bc::BcStatusDto, String> {
    {
        let mut config = state.config.lock();
        match side.as_str() {
            "left" => config.bc_left = Some(target),
            "right" => config.bc_right = Some(target),
            other => return Err(format!("未知比较位: {other}")),
        }
        config::save_config_at(&state.config_path, &config).map_err(|e| e.to_string())?;
    }
    Ok(bc::status(state.inner()))
}

#[tauri::command]
fn bc_clear_side(
    side: String,
    state: State<'_, Arc<AppState>>,
) -> Result<bc::BcStatusDto, String> {
    {
        let mut config = state.config.lock();
        match side.as_str() {
            "left" => config.bc_left = None,
            "right" => config.bc_right = None,
            other => return Err(format!("未知比较位: {other}")),
        }
        config::save_config_at(&state.config_path, &config).map_err(|e| e.to_string())?;
    }
    Ok(bc::status(state.inner()))
}

/// 用 BeyondCompare 打开两侧比较：目标是本机共享项就直接给本地路径，
/// 是对端共享项就给 `profile:<ip:port>?<shareId>/<路径>`（顺手写好 WebDAV profile）。
#[tauri::command]
fn bc_compare(
    a: BcTarget,
    b: BcTarget,
    state: State<'_, Arc<AppState>>,
) -> Result<(), String> {
    let exe = bc::locate_exe(state.config.lock().bc_exe_path.as_deref())
        .ok_or_else(|| "未找到 BeyondCompare 可执行文件，请在设置里指定路径".to_string())?;
    let left = bc::resolve_side(state.inner(), &exe, &a)?;
    let right = bc::resolve_side(state.inner(), &exe, &b)?;
    bc::launch_compare(&exe, &left.arg, &right.arg)?;
    info!(left = %left.arg, right = %right.arg, "已在 BeyondCompare 中打开比较");
    Ok(())
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
    let peers = state.peers.lock();
    peers.iter().map(|p| serde_json::json!({
        "id": p.id,
        "name": p.name,
        "ip": p.ip,
        "port": p.port,
        "online": p.online,
        "paired": p.paired,
        // Real WS link state — "长连接" in the UI follows this, not `online`
        // (a peer can answer HTTP push invites without any WS session).
        "connected": state.ws.has_connection(&p.id),
        // 没连着的时候"为什么"：重连中 / 等对端上线 / 已停止（对端断开过、token 失效…）。
        // 界面把它显示成人话，用户才知道该等、该点「连接」、还是该重新配对。
        "linkState": state.ws.link_state(&p.id),
    })).collect()
}

#[tauri::command]
async fn start_http_server(state: State<'_, Arc<AppState>>) -> Result<u16, String> {
    let arc_state = state.inner().clone();
    server::ensure_server(arc_state).await
}

#[tauri::command]
fn get_peer_token(peer_id: String) -> Result<Option<String>, String> {
    credential::get_peer_token(&peer_id).map_err(|e| e.to_string())
}

/// Arm the long connection by hand（F-16）: 拨号只由用户点「连接」（或启动时的恢复）
/// 发起，点它就无视"必须已配对"以外的任何规则直接拨号，并结束本机之前的「断开」暂停。
///
/// 与启动恢复的区别在 `manual=1`：这里是**有人按了按钮**，对端收到后连它自己的暂停
/// 一起结束；启动恢复（`ws::restore_links_on_startup`）不带这个标记，对端点过「断开」
/// 时照旧被拒。
///
/// **保持 async**：拨号任务要在运行时上跑。异步命令天然满足；写成同步的话就得靠
/// `task::spawn` 退到 Tauri 的全局运行时（能跑，但多绕一层）。
#[tauri::command]
async fn connect_peer(peer_id: String, state: State<'_, Arc<AppState>>) -> Result<(), String> {
    if state.get_peer(&peer_id).is_none() {
        return Err("设备不存在".to_string());
    }
    state.ws.resume(&peer_id);
    ws::ensure_link_now(state.inner(), &peer_id);
    Ok(())
}

/// Drop the long connection and keep it down (auto-connect stays off) until the
/// user here reconnects, or the peer asks to connect with its own 「连接」.
///
/// 「断开」的意图也跨重启有效：把地址簿里的长连接标记一起清掉，否则下次启动会照着
/// 上次的记忆（F-16 重启恢复）把它自动连回来。
#[tauri::command]
fn disconnect_peer(peer_id: String, state: State<'_, Arc<AppState>>) -> Result<(), String> {
    if state.get_peer(&peer_id).is_none() {
        return Err("设备不存在".to_string());
    }
    state.ws.pause(&peer_id);
    state.set_auto_connect(&peer_id, false);
    state.emit_devices_changed();
    Ok(())
}

// ---------------------------------------------------------------------------
// Device management commands (F-11, F-14, F-71)
// ---------------------------------------------------------------------------

/// Add a device manually by IP:port. Probes /pair/info to get identity.
#[tauri::command]
async fn add_peer(ip: String, port: u16, state: State<'_, Arc<AppState>>) -> Result<PeerDevice, String> {
    let url = format!("http://{ip}:{port}/pair/info");
    let resp = reqwest::get(&url).await.map_err(|e| format!("无法连接 {ip}:{port}: {e}"))?;
    let info: serde_json::Value = resp.json().await.map_err(|e| format!("响应异常: {e}"))?;
    let id = info["id"].as_str().unwrap_or("").to_string();
    let name = info["name"].as_str().unwrap_or(&ip).to_string();
    let peer = PeerDevice { id, name, ip, port, online: true, paired: false, auto_connect: false };
    state.add_peer_book(peer.clone());
    Ok(peer)
}

/// 解除配对（F-14）：先让对方也清掉（HTTP，尽力而为），再清本机。
///
/// 走 HTTP 而不是 WS 事件：解除的那一刻链路可能已经断了，而且 WS 事件是**排队**
/// 发送的，紧接着的 `drop_peer` 会把还没写出去的它一起丢掉 —— 对端就一直停在
/// "已配对"（用户实际遇到的）。HTTP 不依赖长连接，谁拨的号也不影响。
///
/// 本机只清 token 与配对标记、**保留设备行**：对方还在网络上看得见，就该继续
/// 以"未绑定"出现在列表里（离线时前端自然不显示）。
pub async fn unpair_peer(state: &Arc<AppState>, peer_id: &str) -> Result<(), String> {
    let peer = state.get_peer(peer_id).ok_or_else(|| "设备不存在".to_string())?;

    // 通知对端用的是本机手里"对它"的 token —— 必须在本机删除之前用掉。
    if !peer.ip.is_empty() && peer.port != 0 {
        if let Ok(Some(token)) = credential::get_peer_token(peer_id) {
            let url = format!("http://{}:{}/unpair", peer.ip, peer.port);
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(3))
                .build()
                .map_err(|e| e.to_string())?;
            // body 里的 peerId 是**本机自己的 id**：对端要解除的是"调用方这台设备"
            // （与 WS 事件 `Unpair { peer_id: 本机 id }` 的语义一致）。远端调用时身份
            // 由 token 反解、这个字段只是冗余；同机回环调用没有 token 可解，就靠它。
            let own_id = state.config.lock().device_id.clone();
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
    }

    credential::remove_peer(peer_id).map_err(|e| e.to_string())?;
    state.ws.drop_peer(peer_id);
    state.unpair(peer_id);

    // 关系没了，BC 里那条指向它的 WebDAV profile 也就没用了（token 已作废）：按当前
    // 在册设备刷新一遍 —— prune 只删我们自己写下且已失效的条目（见 `bc::prune_webdav_profiles`）。
    bc::provision_profiles(state);
    Ok(())
}

#[tauri::command]
async fn remove_peer(peer_id: String, state: State<'_, Arc<AppState>>) -> Result<(), String> {
    unpair_peer(state.inner(), &peer_id).await
}

/// 一键清空本机**全部**配对凭据（F-15）：用户不必（也不该）去翻文件或系统凭据管理器。
///
/// 三处一起清，少一处就等于没清干净：
/// 1. 凭据文件（`%LOCALAPPDATA%\OrbDrop\credentials.dat`）—— 全部设备的 token 都在里面，
///    删它就全清（`credential::wipe`，顺带清掉老版本留在系统凭据库里的那一条）；
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
    bc::provision_profiles(state);
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
    if state.get_peer(&peer_id).is_none() {
        return Err("设备不存在".to_string());
    }
    Ok(state.pair.create_code())
}

/// Confirm pairing with a code shown on the peer's screen. The peer verifies it
/// and hands back the token we store for it.
#[tauri::command]
async fn pair_confirm(peer_id: String, code: String, state: State<'_, Arc<AppState>>) -> Result<String, String> {
    let peer = state.get_peer(&peer_id).ok_or_else(|| "设备不存在".to_string())?;
    let own_id = state.config.lock().device_id.clone();
    let own_name = state.config.lock().device_name.clone();

    let url = format!("http://{}:{}/pair", peer.ip, peer.port);
    // Our own HTTP port: the peer we post to may be the side that dials the WS
    // link (smaller device id) and it has no other way to learn our address.
    let own_port = match state.server_port.load(std::sync::atomic::Ordering::SeqCst) {
        0 => state.config.lock().port,
        p => p,
    };
    let client = reqwest::Client::new();
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

/// 推送本机文件/目录（用户在文件选择框里挑的，可多选）给某台设备（F-50）。
///
/// 目标可以是任何在线设备：长连接、已配对未连、未绑定都能推（未绑定走 HTTP 邀请）。
/// 多选合并成**一条**推送：对端只弹一张卡、只同意一次、一条进度。
/// 返回邀请 id；对方同意与否由 `push://response` / `push://done` 事件回来。
#[tauri::command]
async fn push_paths(
    paths: Vec<String>,
    peer_id: String,
    state: State<'_, Arc<AppState>>,
) -> Result<String, String> {
    transfer::push_paths(state.inner().clone(), paths, &peer_id).await
}

/// 接收方点「同意接收」：起下载任务。
///
/// **必须是 async 命令**：下载任务要在运行时上跑。写成同步命令的话它跑在主线程上 —— 那里
/// 没有 reactor，任务会被丢掉（用户侧："点了同意，两边都停在 0%"）。`task::spawn` 现在有
/// 兜底（退到 Tauri 的全局运行时），但异步命令才是这里最自然的上下文。
#[tauri::command]
async fn accept_push(invite_id: String, state: State<'_, Arc<AppState>>) -> Result<(), String> {
    transfer::accept_invite(state.inner().clone(), &invite_id)
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
// Download commands (F-40 / F-44)
// ---------------------------------------------------------------------------

/// Download a file from a remote peer (P1 remote browsing download).
#[tauri::command]
async fn download_from_peer(
    peer_id: String,
    share_id: String,
    path_in_share: String,
    // 共享项直接指向一个文件时 path_in_share 为空，文件名只能由调用方（对端界面）给出。
    file_name: Option<String>,
    state: State<'_, Arc<AppState>>,
    app: AppHandle,
) -> Result<String, String> {
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
            let picked = app.dialog().file().set_file_name(&file_name)
                .blocking_save_file().map(|p| PathBuf::from(p.to_string()));
            picked.unwrap_or_else(|| config::unique_path(&download_dir, &file_name))
        }
        DownloadMode::Direct => config::unique_path(&download_dir, &file_name),
    };
    let url = format!("http://{}:{}/file?share={}&path={}", peer.ip, peer.port, share_id, urlencode(&path_in_share));

    let resp = reqwest::Client::new()
        .get(&url)
        .bearer_auth(&token)
        .send()
        .await
        .map_err(|e| e.to_string())?;
    if !resp.status().is_success() {
        return Err(format!("下载失败: HTTP {}", resp.status()));
    }
    let bytes = resp.bytes().await.map_err(|e| e.to_string())?;
    if let Some(parent) = target_dir.parent() {
        tokio::fs::create_dir_all(parent).await.map_err(|e| e.to_string())?;
    }
    tokio::fs::write(&target_dir, &bytes).await.map_err(|e| e.to_string())?;
    Ok(target_dir.to_string_lossy().to_string())
}

#[tauri::command]
async fn download_file(
    share_id: String,
    path_in_share: String,
    state: State<'_, Arc<AppState>>,
    app: AppHandle,
) -> Result<String, String> {
    let (_share, download_dir, mode) = {
        let config = state.config.lock();
        let share = config.shares.iter().find(|s| s.id == share_id)
            .ok_or_else(|| format!("共享项不存在: {share_id}"))?.clone();
        (share, config.download_dir.clone(), config.download_mode)
    };

    let file_name = Path::new(&path_in_share)
        .file_name().map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "download".into());

    let target_dir = match mode {
        DownloadMode::AskEachTime => {
            use tauri_plugin_dialog::DialogExt;
            let picked = app.dialog().file().set_file_name(&file_name)
                .blocking_save_file().map(|p| PathBuf::from(p.to_string()));
            picked.unwrap_or_else(|| config::unique_path(&download_dir, &file_name))
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
    Ok(target_dir.to_string_lossy().to_string())
}

#[tauri::command]
async fn download_and_open(
    share_id: String,
    path_in_share: String,
    state: State<'_, Arc<AppState>>,
    app: AppHandle,
) -> Result<String, String> {
    let path = download_file(share_id, path_in_share, state, app.clone()).await?;
    if let Err(e) = app.opener().reveal_item_in_dir(&path) {
        error!("Failed to reveal item: {e}");
    }
    Ok(path)
}

// ---------------------------------------------------------------------------
// 拖拽下载（把树里的任意文件/目录拖到资源管理器里落盘）
// ---------------------------------------------------------------------------

/// 登记一次拖拽意图（界面按下鼠标动起来就调它）。本机共享项当场返回可拖的票据，
/// 对端共享项返回"还在准备"的票据并开始在后台下到暂存目录 ——
/// 拖放循环要跟着按住的主键走，所以下载只能赶在起拖之前先跑起来。
///
/// **保持 async**：对端项要在后台拉元数据/内容，这活儿得在运行时上跑。
#[tauri::command]
async fn prepare_drag_out(
    req: drag::DragOutRequest,
    state: State<'_, Arc<AppState>>,
) -> Result<drag::DragTicket, String> {
    state.drag.prepare(state.inner(), &req)
}

/// 起拖：等暂存备好后在主线程跑 OLE 拖放（阻塞到用户松手）。返回时拖放已经结束。
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
#[tauri::command]
fn orb_screen_metrics(window: WebviewWindow) -> Result<orb_window::ScreenMetrics, String> {
    orb_window::metrics(&window)
}

/// 一次性移动 + 缩放悬浮球窗口（物理像素）。
#[tauri::command]
fn orb_set_rect(window: WebviewWindow, x: i32, y: i32, w: i32, h: i32) -> Result<(), String> {
    orb_window::set_rect(&window, orb_window::Rect { x, y, w, h })
}

/// 开始拖动悬浮球窗口：先按按下点校正抓取锚点，再当场把拖动交给 OS。
///
/// 校正与起拖必须在同一次调用里完成（OS 是在起拖那一刻把窗口钉在指针上的，中间多一趟往返，
/// 指针就又走了一段）—— 见 `orb_window::drag_start`。
#[tauri::command]
fn orb_drag_start(window: WebviewWindow, press_x: f64, press_y: f64) -> Result<(), String> {
    orb_window::drag_start(&window, press_x, press_y)
}

/// 鼠标主键是否仍按着：拖动悬浮球时页面收不到 mouseup，贴边落位得靠它等松手。
#[tauri::command]
fn orb_mouse_down() -> bool {
    orb_window::mouse_down()
}

/// 记录悬浮球落位（折叠态窗口左上角，物理像素）：下次启动复原到同一位置（F-74）。
///
/// 由界面在**几何落定之后**调用（拖动松手 → 吸附判定跑完 → 窗口不再动），见 App.svelte
/// `rememberOrbPos`；松手那一刻记下的是自由落点，贴边还会再挪一次窗口。位置没变就不落盘。
#[tauri::command]
fn orb_save_pos(x: i32, y: i32, state: State<'_, Arc<AppState>>) -> Result<(), String> {
    let mut config = state.config.lock();
    if config.orb_pos == Some(OrbPos { x, y }) {
        return Ok(());
    }
    config.orb_pos = Some(OrbPos { x, y });
    config::save_config_at(&state.config_path, &config).map_err(|e| e.to_string())
}

/// 启动落位：有记录就回到记录的位置，没记录（首启 / 配置丢了）就放到默认落位。
///
/// 两种都在 `setup` 里做完、赶在首帧之前：窗口是按 tauri.conf 的 `center: true` 建的，
/// 等界面起来再挪会看到球从屏幕中央跳过去。落位之后界面还会对同一个矩形重跑一次吸附
/// 判定，复原贴边形态（吸附幂等，默认落位则因为余量大于阈值而不吸附）。
fn place_orb_on_startup(app: &AppHandle) {
    let Some(win) = app.get_webview_window("main") else {
        warn!("未找到 main 窗口，跳过悬浮球落位");
        return;
    };
    let saved = app.state::<Arc<AppState>>().config.lock().orb_pos;
    let (x, y) = match saved {
        // 上次那块显示器没了 / 分辨率变小：留在居中，否则球落到屏幕外、用户拖不回来
        // （透明无边框窗口没有标题栏）
        Some(pos) if !orb_window::visible_on_some_monitor(&win, pos.x, pos.y) => {
            warn!(x = pos.x, y = pos.y, "上次的悬浮球位置已不在任何显示器上，保持居中");
            return;
        }
        Some(pos) => (pos.x, pos.y),
        None => match orb_window::default_pos(&win, collapsed_logical(&app.config())) {
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

/// 折叠态窗口的标称边长（逻辑像素，即 tauri.conf 里 main 窗口的 `width`）。
///
/// 默认落位要用它算窗口边长：启动那一刻窗口的 `outer_size()` 还带着 DWM 阴影余量，
/// 不是真实边长（见 `orb_window::default_pos`）。
fn collapsed_logical(config: &tauri::Config) -> f64 {
    config
        .app
        .windows
        .iter()
        .find(|w| w.label == "main")
        .map(|w| w.width)
        .unwrap_or(90.0)
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
                    let device_name = arc_state.config.lock().device_name.clone();
                    let device_id = arc_state.config.lock().device_id.clone();
                    if let Some(daemon) = discover::register(port, &device_name, &device_id) {
                        *arc_state.mdns_daemon.lock() = Some(daemon);
                    }
                    let _ = handle.emit(ws::events::ui::SERVER_STARTED, port);

                    // Spawn mDNS scanner.
                    let (mdns_tx, mut mdns_rx) = mpsc::channel::<discover::PeerEvent>(64);
                    let own_id = device_id.clone();
                    tokio::spawn(discover::scan(mdns_tx, own_id));

                    // Forward mDNS events to the state + UI.
                    let s2 = arc_state.clone();
                    tokio::spawn(async move {
                        use discover::PeerEvent;
                        // 身份核对要按对端自己的 token 问一次 `/pair/info`（见
                        // `state::reconcile_stale_identity`）：超时给短一点，别让
                        // 一次慢回话卡住后面的事件。
                        let client = reqwest::Client::builder()
                            .timeout(std::time::Duration::from_secs(2))
                            .build()
                            .unwrap_or_default();
                        while let Some(event) = mdns_rx.recv().await {
                            match event {
                                PeerEvent::Online { id, name, ip, port } => {
                                    let is_paired = credential::has_peer(&id);
                                    // Already paired in our records → keep the flag.
                                    let paired = is_paired || {
                                        let peers = s2.peers.lock();
                                        peers.iter().any(|p| p.id == id && p.paired)
                                    };
                                    // 之前是不是已经算在线：只有"重新出现"[离线→在线]才值得
                                    // 触发一次接回（mDNS 反复重解析同一条记录很常见）。
                                    let was_online = s2
                                        .get_peer(&id)
                                        .map(|p| p.online)
                                        .unwrap_or(false);
                                    s2.upsert_peer(&PeerDevice {
                                        id: id.clone(),
                                        name,
                                        ip: ip.to_string(),
                                        port,
                                        online: true,
                                        paired,
                                        // 发现条目不带长连接意图：这由链路本身
                                        // （`set_auto_connect`）记，别在这里覆盖。
                                        auto_connect: false,
                                    });
                                    // mDNS is often the only source of a paired
                                    // peer's address (the credential store keeps
                                    // no address) — but it only fills the address
                                    // in: the link comes from a 「连接」 click or
                                    // from `restore_links_on_startup` on boot（或
                                    // 由下面这条"重新出现"的证据触发）。
                                    if !was_online {
                                        ws::redial_if_remembered(&s2, &id);
                                    }

                                    // 一个地址上只能有一个身份：对端被重置过（数据目录
                                    // 清了 → 换了 device id）时，本机为旧 id 存的那一行
                                    // 还指着这个地址 —— 收拾掉它，别让同一台设备在列表里
                                    // 留两行（旧行"已绑定"、新行"未绑定"）。
                                    for stale_id in s2.stale_ids_at(&ip.to_string(), port, &id) {
                                        state::reconcile_stale_identity(
                                            &s2,
                                            &client,
                                            &stale_id,
                                        )
                                        .await;
                                    }
                                }
                                PeerEvent::Offline(id) => {
                                    s2.set_peer_online(&id, false);
                                }
                            }
                        }
                    });

                    // Watch existing shares.
                    share::watcher::watch_all_shares(&arc_state);

                    // 恢复上次关机时的长连接（F-16）：地址簿里记着"还连着"的设备自动
                    // 拨回来——用户不用重新点一遍「连接」。
                    ws::restore_links_on_startup(&arc_state);

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
                        let client = reqwest::Client::builder()
                            .timeout(std::time::Duration::from_millis(1500))
                            .build()
                            .unwrap_or_default();
                        loop {
                            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                            state::probe_peers_once(&probe_state, &client).await;
                        }
                    });

                    // Apply autostart per saved config.
                    if arc_state.config.lock().auto_start {
                        let _ = autostart::enable_autostart();
                    }
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

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
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

    // 老版本把整份配对凭据放在系统凭据库里；现在它住在数据目录的凭据文件里。
    // 迁移**只在这里**发生（启动时显式一次）：读路径绝不改用户的东西。
    // 先写文件、后删老条目，中途失败只是两处都在，下次启动重来。
    match credential::migrate_legacy_entry() {
        Ok(Some(peers)) => info!(peers = peers.len(), "已把系统凭据库里的配对迁移到凭据文件"),
        Ok(None) => {}
        // 迁不动不是致命错误：读路径会只读地回落到老条目，功能照旧。
        Err(e) => warn!("配对凭据迁移失败（继续用系统凭据库里的老条目）: {e}"),
    }

    // P1 写出的凭据文件是"头部 + base64 正文 + 署名"的明文遮挡；现在是整块混淆。
    // 老文件照样读得回来，但只在启动时改写一次（读路径绝不改用户的东西）。
    // 改不动不是致命错误：老格式的读兼容一直在，下次启动重来。
    match credential::upgrade_vault_format() {
        Ok(Some(peers)) => info!(peers, "已把凭据文件改写成整块混淆格式"),
        Ok(None) => {}
        Err(e) => warn!("凭据文件格式改写失败（继续按老格式读）: {e}"),
    }

    let state = Arc::new(AppState::new(config, config_path));

    tauri::Builder::default()
        // 单实例：第二份进程由插件负责收敛，**不能**让它自己静默退出 ——
        // 那在用户侧就是"启动一下就闪退"，而且什么提示都没有。这里把它变成
        // "把已在运行的实例叫到台面上来"（悬浮球面板直接展开）。
        .plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            info!("另一实例被启动：把本实例调到前台并展开面板");
            if let Some(win) = app.get_webview_window("main") {
                let _ = win.show();
                let _ = win.unminimize();
                let _ = win.set_focus();
                let _ = win.emit(ws::events::ui::ORB_SHOW, ());
            }
        }))
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_dialog::init())
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            add_share,
            list_shares,
            remove_share,
            rename_share,
            toggle_broadcast,
            get_settings,
            update_settings,
            bc_status,
            bc_set_side,
            bc_clear_side,
            bc_compare,
            get_actual_port,
            local_addresses,
            list_devices,
            add_peer,
            remove_peer,
            pair_start,
            pair_confirm,
            pair_invite,
            cancel_pair_invite,
            accept_pair_invite,
            reject_pair_invite,
            get_peer_token,
            connect_peer,
            disconnect_peer,
            wipe_credentials,
            fs_browse::fs_drives,
            fs_browse::fs_list,
            list_picker_favorites,
            set_picker_favorites,
            push_paths,
            accept_push,
            reject_push,
            cancel_push,
            cancel_receive,
            download_from_peer,
            download_file,
            download_and_open,
            prepare_drag_out,
            start_drag_out,
            start_http_server,
            get_app_state,
            orb_screen_metrics,
            orb_set_rect,
            orb_drag_start,
            orb_mouse_down,
            orb_save_pos,
        ])
        .setup(|app| {
            // Store the AppHandle so WS events can be emitted.
            let state_arc = app.state::<Arc<AppState>>();
            *state_arc.app_handle.lock() = Some(app.handle().clone());

            // 透明无边框窗口在 Win11 上会带一条系统绘制的 1px 边框（沿 OS 圆角露在面板外，
            // 每次改窗口尺寸重画 → 表现为窗口最外缘闪烁的线），启动时抹掉。
            match app.get_webview_window("main") {
                Some(win) => match orb_window::strip_os_frame(&win) {
                    Ok(()) => info!("已抹掉系统窗口边框与系统圆角"),
                    Err(e) => warn!("抹掉系统窗口边框失败: {e}"),
                },
                None => warn!("未找到 main 窗口，跳过系统边框处理"),
            }

            // 悬浮球回到上次的位置；没有记录（首启）就放到默认落位（右上 1/3）
            place_orb_on_startup(app.handle());

            setup_server_task(app.handle());

            // 尽早把 BC 的 profile 文件立起来（含所有已配对设备）：
            // BC 只在启动时登记 profile 文件，晚了就得等它重启才认。
            let written = bc::provision_profiles(&state_arc);
            info!(written, "启动预写 BeyondCompare profile 完成");
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

