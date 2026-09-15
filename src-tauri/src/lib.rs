pub mod autostart;
pub mod config;
pub mod credential;
pub mod discover;
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

use config::{AppSettings, DownloadMode, PeerDevice, ShareItem};
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
    pub download_dir: String,
    pub download_mode: String,
    pub inbox_dir: String,
    pub device_name: String,
    pub auto_start: bool,
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
    let mode = match req.download_mode.as_str() {
        "Direct" => DownloadMode::Direct,
        "AskEachTime" => DownloadMode::AskEachTime,
        other => return Err(format!("未知下载模式: {other}")),
    };

    let (old_port, old_name, old_autostart);
    {
        let mut config = state.config.lock();
        old_port = config.port;
        old_name = config.device_name.clone();
        old_autostart = config.auto_start;
        config.port = req.port;
        config.download_dir = PathBuf::from(&req.download_dir);
        config.download_mode = mode;
        config.inbox_dir = PathBuf::from(&req.inbox_dir);
        config.device_name = req.device_name.clone();
        config.auto_start = req.auto_start;
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
// Server / Device commands
// ---------------------------------------------------------------------------

#[tauri::command]
fn get_actual_port(state: State<'_, Arc<AppState>>) -> u16 {
    state.server_port.load(std::sync::atomic::Ordering::SeqCst)
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

/// Arm the long connection by hand（F-16）: 拨号只由用户点「连接」发起，
/// 点它就无视"必须已配对"以外的任何规则直接拨号，并结束本机之前的「断开」暂停。
///
/// **必须是 async 命令**：Tauri 的同步命令跑在主线程（没有 Tokio reactor），
/// 里面 `tokio::spawn` 会 panic 掉整个进程（表现为点「连接」闪退）。
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
#[tauri::command]
fn disconnect_peer(peer_id: String, state: State<'_, Arc<AppState>>) -> Result<(), String> {
    if state.get_peer(&peer_id).is_none() {
        return Err("设备不存在".to_string());
    }
    state.ws.pause(&peer_id);
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
    let peer = PeerDevice { id, name, ip, port, online: true, paired: false };
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
    Ok(())
}

#[tauri::command]
async fn remove_peer(peer_id: String, state: State<'_, Arc<AppState>>) -> Result<(), String> {
    unpair_peer(state.inner(), &peer_id).await
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

    // Store token under the peer's actual id.
    credential::set_peer(&remote_id, &token).map_err(|e| e.to_string())?;

    // Mark paired.
    let peer_name = remote_name.clone();
    state.mark_paired(&PeerDevice {
        id: remote_id.clone(),
        name: remote_name,
        ip: peer.ip.clone(),
        port: peer.port,
        online: true,
        paired: true,
    });

    // 配对只确立"已配对"关系：长连接等用户自己点「连接」再建（不再自动拨号）。
    ws::emit_ui(&state, ws::events::ui::PAIR_COMPLETED, &serde_json::json!({
        "peerId": remote_id,
    }));
    ws::emit_ui(&state, ws::events::ui::DEVICES_CHANGED, &serde_json::json!({}));

    Ok(peer_name)
}

// ---------------------------------------------------------------------------
// Push commands (F-50 ~ F-54)
// ---------------------------------------------------------------------------

#[tauri::command]
async fn push_invite(
    share_id: String,
    path: String,
    peer_id: String,
    state: State<'_, Arc<AppState>>,
) -> Result<String, String> {
    transfer::invite_push(state.inner().clone(), &share_id, &path, &peer_id).await
}

#[tauri::command]
fn accept_push(invite_id: String, state: State<'_, Arc<AppState>>) -> Result<(), String> {
    transfer::accept_invite(state.inner().clone(), &invite_id)
}

#[tauri::command]
fn reject_push(invite_id: String, state: State<'_, Arc<AppState>>) -> Result<(), String> {
    transfer::reject_invite(&state, &invite_id);
    Ok(())
}

#[tauri::command]
fn cancel_push(invite_id: String, state: State<'_, Arc<AppState>>) -> Result<(), String> {
    transfer::cancel_push(&state, &invite_id)
}

#[tauri::command]
fn cancel_receive(invite_id: String, state: State<'_, Arc<AppState>>) -> Result<(), String> {
    transfer::cancel_receive(&state, &invite_id);
    Ok(())
}

#[tauri::command]
fn push_job_status(invite_id: String, state: State<'_, Arc<AppState>>) -> Result<Option<String>, String> {
    Ok(state.push.job_status(&invite_id))
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

/// 鼠标主键是否仍按着：拖动悬浮球时页面收不到 mouseup，贴边落位得靠它等松手。
#[tauri::command]
fn orb_mouse_down() -> bool {
    orb_window::mouse_down()
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
                        while let Some(event) = mdns_rx.recv().await {
                            match event {
                                PeerEvent::Online { id, name, ip, port } => {
                                    let is_paired = credential::has_peer(&id);
                                    // Already paired in our records → keep the flag.
                                    let paired = is_paired || {
                                        let peers = s2.peers.lock();
                                        peers.iter().any(|p| p.id == id && p.paired)
                                    };
                                    s2.upsert_peer(&PeerDevice {
                                        id: id.clone(),
                                        name,
                                        ip: ip.to_string(),
                                        port,
                                        online: true,
                                        paired,
                                    });
                                    // mDNS is often the only source of a paired
                                    // peer's address (the credential store keeps
                                    // no address) — but it only fills the address
                                    // in: the long connection waits for a click.
                                }
                                PeerEvent::Offline(id) => {
                                    s2.set_peer_online(&id, false);
                                }
                            }
                        }
                    });

                    // Watch existing shares.
                    share::watcher::watch_all_shares(&arc_state);

                    // Liveness backstop: mDNS only reports a departure when the peer
                    // sends goodbye or its records expire (up to ~2 min), and a
                    // re-registration under the same instance name may not be
                    // re-resolved at all. Probing the peer's public identity
                    // endpoint both demotes dead peers and promotes returned ones,
                    // so the device list reflects reachability within seconds.
                    // Peers with a live WS link need no probe.
                    let probe_state = arc_state.clone();
                    tokio::spawn(async move {
                        let client = reqwest::Client::builder()
                            .timeout(std::time::Duration::from_millis(1500))
                            .build()
                            .unwrap_or_default();
                        loop {
                            tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                            let candidates: Vec<PeerDevice> = probe_state
                                .peers
                                .lock()
                                .iter()
                                .filter(|p| !p.ip.is_empty() && p.port != 0)
                                .cloned()
                                .collect();
                            for p in candidates {
                                if probe_state.ws.has_connection(&p.id) {
                                    probe_state.set_peer_online(&p.id, true);
                                    continue;
                                }
                                let url = format!("http://{}:{}/pair/info", p.ip, p.port);
                                let alive = matches!(client.get(&url).send().await, Ok(r) if r.status().is_success());
                                if !alive && p.online {
                                    info!(peer_id = %p.id, "peer unreachable, marking offline");
                                }
                                probe_state.set_peer_online(&p.id, alive);
                            }
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
            get_actual_port,
            list_devices,
            add_peer,
            remove_peer,
            pair_start,
            pair_confirm,
            get_peer_token,
            connect_peer,
            disconnect_peer,
            push_invite,
            accept_push,
            reject_push,
            cancel_push,
            cancel_receive,
            push_job_status,
            download_from_peer,
            download_file,
            download_and_open,
            start_http_server,
            get_app_state,
            orb_screen_metrics,
            orb_set_rect,
            orb_mouse_down,
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

            setup_server_task(app.handle());
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

