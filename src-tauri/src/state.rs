use std::path::PathBuf;
use std::sync::atomic::AtomicU16;
use std::sync::Arc;

use mdns_sd::ServiceDaemon;
use parking_lot::Mutex;
use tauri::AppHandle;
use tauri::Emitter;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tracing::{info, warn};

use crate::config::{Config, PeerDevice};
use crate::pair::PairStore;
use crate::transfer::push_job::PushStore;
use crate::ws::WsManager;

/// Application-wide shared state, accessible from Tauri commands and the HTTP server.
pub struct AppState {
    pub config_path: PathBuf,
    pub config: Mutex<Config>,
    /// Known devices: paired + discovered + manual (merged view).
    pub peers: Mutex<Vec<PeerDevice>>,
    pub server_port: AtomicU16,
    pub server_shutdown: Mutex<Option<oneshot::Sender<()>>>,
    pub server_task: Mutex<Option<JoinHandle<()>>>,
    /// Kept alive for the app's lifetime to keep the mDNS registration live.
    pub mdns_daemon: Mutex<Option<ServiceDaemon>>,
    /// WebSocket manager (peer links).
    pub ws: WsManager,
    /// Push jobs (sender + receiver) and resume store.
    pub push: PushStore,
    /// 拖拽下载的票据表（暂存副本 / 已备好的本机路径）。
    pub drag: crate::drag::DragStore,
    /// Pairing code store.
    pub pair: PairStore,
    /// Tauri handle for emitting UI events.
    pub app_handle: Mutex<Option<AppHandle>>,
    /// File watchers keyed by share id.
    pub watchers: Mutex<std::collections::HashMap<String, notify::RecommendedWatcher>>,
}

impl AppState {
    pub fn new(config: Config, config_path: PathBuf) -> Self {
        // Peers: config address book + paired entries from the credential store.
        // Our own device id is never a peer: two instances sharing one credential
        // store (same machine, one OS user) would otherwise both key the same
        // token under each other's id and list a bogus self row.
        let own_id = config.device_id.clone();
        let mut peers: Vec<PeerDevice> = config
            .peers
            .clone()
            .into_iter()
            .filter(|p| p.id != own_id)
            .collect();
        let tokens = crate::credential::load_peers().unwrap_or_default();
        for (id, _) in tokens.iter() {
            if id == &own_id {
                continue;
            }
            if !peers.iter().any(|p| &p.id == id) {
                peers.push(PeerDevice {
                    id: id.clone(),
                    name: id.clone(),
                    ip: String::new(),
                    port: 0,
                    online: false,
                    paired: true,
                    auto_connect: false,
                });
            } else if let Some(p) = peers.iter_mut().find(|p| &p.id == id) {
                p.paired = true;
            }
        }

        Self {
            config_path,
            config: Mutex::new(config),
            peers: Mutex::new(peers),
            server_port: AtomicU16::new(0),
            server_shutdown: Mutex::new(None),
            server_task: Mutex::new(None),
            mdns_daemon: Mutex::new(None),
            ws: WsManager::new(),
            push: PushStore::new(),
            drag: crate::drag::DragStore::new(),
            pair: PairStore::new(),
            app_handle: Mutex::new(None),
            watchers: Mutex::new(std::collections::HashMap::new()),
        }
    }

    // ------------------------------------------------------------------
    // Peer management helpers
    // ------------------------------------------------------------------

    /// Upsert a peer into the in-memory list. Persists the address book
    /// entry (config.peers) if the peer is paired or was manually added.
    pub fn upsert_peer(&self, peer: &PeerDevice) {
        let mut peers = self.peers.lock();
        if let Some(existing) = peers.iter_mut().find(|p| p.id == peer.id) {
            existing.name = peer.name.clone();
            if !peer.ip.is_empty() {
                existing.ip = peer.ip.clone();
            }
            if peer.port != 0 {
                existing.port = peer.port;
            }
            existing.online = existing.online || peer.online;
            existing.paired = existing.paired || peer.paired;
        } else {
            peers.push(peer.clone());
        }
        drop(peers);

        // Persist if in the address book.
        let mut config = self.config.lock();
        if config.peers.iter().any(|p| p.id == peer.id) {
            if let Some(p) = config.peers.iter_mut().find(|p| p.id == peer.id) {
                p.name = peer.name.clone();
                if !peer.ip.is_empty() {
                    p.ip = peer.ip.clone();
                }
                if peer.port != 0 {
                    p.port = peer.port;
                }
                p.paired = peer.paired;
            }
            let _ = crate::config::save_config_at(&self.config_path, &config);
        }
        self.emit_devices_changed();
    }

    /// Add a peer to the address book (manual add) and persist.
    ///
    /// 已经在列表里的设备**也要把地址补上**，不能因为"这一行已经在了"就整条丢掉：
    /// 凭据库里有 token、却没地址的设备会先被 `AppState::new` 合成一行（ip 空、端口 0），
    /// 于是"添加设备"填的地址会被静默忽略 —— 之后配对学到的地址也是空的，长连接
    /// 永远拨不出去（表现为设备行显示 `:0`）。
    pub fn add_peer_book(&self, peer: PeerDevice) {
        {
            let mut peers = self.peers.lock();
            match peers.iter_mut().find(|p| p.id == peer.id) {
                Some(p) => {
                    if !peer.name.is_empty() {
                        p.name = peer.name.clone();
                    }
                    if !peer.ip.is_empty() {
                        p.ip = peer.ip.clone();
                    }
                    if peer.port != 0 {
                        p.port = peer.port;
                    }
                    p.paired = p.paired || peer.paired;
                }
                None => peers.push(peer.clone()),
            }
        }
        {
            let mut config = self.config.lock();
            match config.peers.iter_mut().find(|p| p.id == peer.id) {
                Some(p) => {
                    if !peer.name.is_empty() {
                        p.name = peer.name.clone();
                    }
                    if !peer.ip.is_empty() {
                        p.ip = peer.ip.clone();
                    }
                    if peer.port != 0 {
                        p.port = peer.port;
                    }
                    p.paired = p.paired || peer.paired;
                }
                None => config.peers.push(peer),
            }
            let _ = crate::config::save_config_at(&self.config_path, &config);
        }
        self.emit_devices_changed();
    }

    /// Mark a peer paired in both lists and persist the address book.
    pub fn mark_paired(&self, peer: &PeerDevice) {
        {
            let mut peers = self.peers.lock();
            if let Some(p) = peers.iter_mut().find(|p| p.id == peer.id) {
                p.paired = true;
                p.name = peer.name.clone();
                if !peer.ip.is_empty() {
                    p.ip = peer.ip.clone();
                }
                if peer.port != 0 {
                    p.port = peer.port;
                }
            } else {
                peers.push(peer.clone());
            }
        }
        {
            let mut config = self.config.lock();
            if let Some(p) = config.peers.iter_mut().find(|p| p.id == peer.id) {
                p.paired = true;
                p.name = peer.name.clone();
                if !peer.ip.is_empty() {
                    p.ip = peer.ip.clone();
                }
                if peer.port != 0 {
                    p.port = peer.port;
                }
            } else {
                config.peers.push(peer.clone());
            }
            let _ = crate::config::save_config_at(&self.config_path, &config);
        }
        // 配对只确立关系、不建链（F-16）：把上一次留下的拨号状态清掉。最要紧的是
        // **重新配对**：token 失效时这台设备停在 `Stopped{TokenRejected}`，不清就会一直
        // 显示「需配对」—— 用户刚换到新 token，界面却还在叫他重新配对。
        self.ws.clear_link_state(&peer.id);
        self.emit_devices_changed();
    }

    /// 记住/忘掉"这台设备处于长连接"（F-16 跨重启恢复）。
    ///
    /// 写点在**链路建立的那一刻**、而不是退出时：进程被强杀、断电、任务管理器结束都
    /// 不会漏掉这次长连接。用户点「断开」时清零 —— 那个意图跨重启一样算数，重启后不会
    /// 自动连回来。只对地址簿（`config.peers`）里的设备落盘：纯发现条目没有可恢复的
    /// 身份，重启后本来也不会出现在列表里。
    pub fn set_auto_connect(&self, peer_id: &str, on: bool) {
        {
            let mut peers = self.peers.lock();
            if let Some(p) = peers.iter_mut().find(|p| p.id == peer_id) {
                p.auto_connect = on;
            }
        }
        let mut config = self.config.lock();
        let Some(p) = config.peers.iter_mut().find(|p| p.id == peer_id) else {
            return;
        };
        if p.auto_connect == on {
            return;
        }
        p.auto_connect = on;
        let _ = crate::config::save_config_at(&self.config_path, &config);
    }

    /// Set online state. Emits only on a real change.
    pub fn set_peer_online(&self, peer_id: &str, online: bool) {
        let changed;
        {
            let mut peers = self.peers.lock();
            let existing = peers.iter_mut().find(|p| p.id == peer_id);
            changed = match existing {
                Some(p) if p.online != online => {
                    p.online = online;
                    true
                }
                Some(_) => false,
                None => false,
            };
        }
        if changed {
            let event = if online { "peer://online" } else { "peer://offline" };
            if let Some(app) = self.app_handle.lock().as_ref() {
                let _ = app.emit(event, serde_json::json!({ "peerId": peer_id }));
            }
            self.emit_devices_changed();
        }
    }

    /// 解除配对（F-14）：清 token、断链，但**保留设备行**并把它标回"未绑定"。
    ///
    /// 设备还在 mDNS 视野里就该继续出现在列表里（可以再点「配对」把关系建回来）；对方
    /// **离线**时这一行也还在 —— 界面把它排在在线设备下方（F-13），那正是唯一能"解除
    /// 配对"的机会：解除本身是本机动作，不需要对方在线（见 `lib.rs::unpair_peer`）。
    /// 地址簿（config.peers）里删掉，重启后不再记着它 —— 除非它再次被发现（或再次配对）。
    pub fn unpair(&self, peer_id: &str) {
        {
            let mut peers = self.peers.lock();
            if let Some(p) = peers.iter_mut().find(|p| p.id == peer_id) {
                p.paired = false;
            }
        }
        {
            let mut config = self.config.lock();
            config.peers.retain(|p| p.id != peer_id);
            let _ = crate::config::save_config_at(&self.config_path, &config);
        }
        self.emit_devices_changed();
    }

    /// 这个地址上除了 `keep_id` 之外还记着哪些 id。
    ///
    /// 一台设备在一个地址上只能有一个身份：对端被重置过（数据目录清了 → device id
    /// 换了）之后，本机为旧 id 存的那一行还指着同一个地址 —— 返回值就是这些旧行。
    pub fn stale_ids_at(&self, ip: &str, port: u16, keep_id: &str) -> Vec<String> {
        self.peers
            .lock()
            .iter()
            .filter(|p| p.id != keep_id && p.port == port && p.ip == ip)
            .map(|p| p.id.clone())
            .collect()
    }

    /// 把一台设备的行换个 id，其余照旧（配对状态、地址、"长连接"记忆都保留）。
    ///
    /// 用在对端被重置过、但配对凭据还在（系统凭据库里那一条没跟着清）的场合：关系
    /// 还活着，只是对端的 id 变了。列表里**只留一行** —— 新 id 那行（mDNS 刚发现的）
    /// 并进这一行，不是并排出现两行。
    pub fn rekey_peer(&self, from: &str, to: &PeerDevice) {
        {
            let mut peers = self.peers.lock();
            let Some(pos) = peers.iter().position(|p| p.id == from) else {
                return; // 没有这一行，就没有可迁的身份
            };
            let mut row = peers.remove(pos);
            peers.retain(|p| p.id != to.id);
            row.id = to.id.clone();
            row.name = to.name.clone();
            row.ip = to.ip.clone();
            row.port = to.port;
            row.online = to.online;
            row.paired = to.paired;
            row.auto_connect = to.auto_connect;
            peers.push(row);
        }
        {
            let mut config = self.config.lock();
            if let Some(pos) = config.peers.iter().position(|p| p.id == from) {
                let mut row = config.peers.remove(pos);
                config.peers.retain(|p| p.id != to.id);
                row.id = to.id.clone();
                row.name = to.name.clone();
                row.ip = to.ip.clone();
                row.port = to.port;
                row.online = to.online;
                row.paired = to.paired;
                row.auto_connect = to.auto_connect;
                config.peers.push(row);
                let _ = crate::config::save_config_at(&self.config_path, &config);
            }
        }
        self.emit_devices_changed();
    }

    /// 这一行存的地址上已经是**别的设备**了（对端换过 device id，且它不认本机的
    /// 凭据）：作废地址并标记离线。
    ///
    /// 配对记录与 token 都**不动**：对端要是拿原 id 回来（配置恢复、备份还原），
    /// 这一行还能接着用。界面只显示在线设备，作废的行自然从列表里消失 —— 留下来
    /// 的只有那台设备真正的新身份，一行。
    pub fn forget_peer_address(&self, peer_id: &str) {
        let went_offline = {
            let mut peers = self.peers.lock();
            match peers.iter_mut().find(|p| p.id == peer_id) {
                Some(p) => {
                    let was = p.online;
                    p.online = false;
                    p.ip.clear();
                    p.port = 0;
                    was
                }
                None => false,
            }
        };
        {
            let mut config = self.config.lock();
            if let Some(p) = config.peers.iter_mut().find(|p| p.id == peer_id) {
                p.ip.clear();
                p.port = 0;
                // `online` 也落盘：不以 false 写回的话，重启后这一行会被当成"在线"
                // 重新出现在列表里 —— 它已经没有地址、不会被探活纠正，就永远赖着不走。
                p.online = false;
                let _ = crate::config::save_config_at(&self.config_path, &config);
            }
        }
        if went_offline {
            if let Some(app) = self.app_handle.lock().as_ref() {
                let _ = app.emit("peer://offline", serde_json::json!({ "peerId": peer_id }));
            }
        }
        self.emit_devices_changed();
    }

    pub fn get_peer(&self, peer_id: &str) -> Option<PeerDevice> {
        self.peers.lock().iter().find(|p| p.id == peer_id).cloned()
    }

    pub fn emit_devices_changed(&self) {
        if let Some(app) = self.app_handle.lock().as_ref() {
            let _ = app.emit("devices://changed", serde_json::json!({}));
        }
    }
}

/// Convenience: rebuild the list of visible peers for a broadcast share
/// (used by the share watcher to decide who to notify).
pub fn visible_peers(state: &Arc<AppState>, targets: &[String]) -> Vec<PeerDevice> {
    let peers = state.peers.lock();
    peers
        .iter()
        .filter(|p| targets.contains(&p.id))
        .cloned()
        .collect()
}

// ---------------------------------------------------------------------------
// 设备身份核对（同一台设备不能有两行）
// ---------------------------------------------------------------------------

/// 一个地址上现在是谁，以及它把带 token 的调用方认作谁。
#[derive(Debug, Clone)]
pub struct PeerIdentity {
    /// 地址上这台设备**此刻**的 device id。
    pub id: String,
    pub name: String,
    /// 对面的凭据库里，本机带过去的 token 反解出的 id。等于本机 id = 配对关系还活着
    /// （对端换过 device id 也照样活着）。没带 token、或 token 它不认 = `None`。
    pub paired_as: Option<String>,
}

/// 问一个地址上的设备"你是谁、你把带这个 token 的调用方认作谁"（`/pair/info`）。
///
/// 这是**同一台设备两行**这个问题的关键一问：光"这个地址上有人答话"不足以证明
/// 旧行存的那台设备还在，必须问出它现在的 id。
pub async fn fetch_identity(
    client: &reqwest::Client,
    ip: &str,
    port: u16,
    token: Option<&str>,
) -> Option<PeerIdentity> {
    let url = format!("http://{ip}:{port}/pair/info");
    let mut request = client.get(&url);
    if let Some(t) = token {
        request = request.bearer_auth(t);
    }
    let body: serde_json::Value = request
        .send()
        .await
        .ok()?
        .error_for_status()
        .ok()?
        .json()
        .await
        .ok()?;
    Some(PeerIdentity {
        id: body["id"].as_str().unwrap_or_default().to_string(),
        name: body["name"].as_str().unwrap_or_default().to_string(),
        paired_as: body["pairedAs"].as_str().map(str::to_string),
    })
}

/// 一次探活得出的结论。
#[derive(Debug)]
pub enum ProbeStep {
    Online,
    Offline,
    /// 这个地址上现在回答的是**另一台设备**（对端换过 device id）。
    OtherIdentity(PeerIdentity),
}

/// 由"地址上的回话"判定这台设备的死活。
///
/// 中间那条是全部要害：**"地址上有人答话"不等于"这台设备在"**。探活只按 ip:port
/// 打 `/pair/info`、拿到 200 就当在线的话，对端被重置过（id 变了）之后，旧行指向的
/// 还是同一个地址（同一台机器、同一个端口），它会 200 到永远 —— 界面上就出现
/// "两个同名设备，一个已绑定、一个未绑定"，而且旧行永远不消失。
pub fn probe_step(expected_id: &str, seen: Option<PeerIdentity>) -> ProbeStep {
    match seen {
        None => ProbeStep::Offline,
        // id 为空 = 老版本/中间有代理，问不出身份就当"是它"（别拿不确定的事杀行）
        Some(identity) if identity.id.is_empty() || identity.id == expected_id => {
            ProbeStep::Online
        }
        Some(identity) => ProbeStep::OtherIdentity(identity),
    }
}

/// 一轮探活：给每台"有地址"的设备问一次身份，判死活或收拾换了 id 的行。
///
/// 由 lib.rs 的 5s 循环调用；单独成函数是为了让"地址上现在是另一台设备"这条路径
/// 能在测试里对着真服务器跑（见 tests/p1_integration.rs）。
pub async fn probe_peers_once(state: &Arc<AppState>, client: &reqwest::Client) {
    let candidates: Vec<PeerDevice> = state
        .peers
        .lock()
        .iter()
        .filter(|p| !p.ip.is_empty() && p.port != 0)
        .cloned()
        .collect();
    for p in candidates {
        if state.ws.has_connection(&p.id) {
            state.set_peer_online(&p.id, true);
            continue;
        }
        // 手里那条 token 一起捎上：对端顺带回答"它把本机认作谁"，那是配对关系
        // 是否还活着的证据。**读不到 ≠ 没有**：凭据库的瞬时故障（凭据管理器忙、同机多
        // 实例并发）不能当成"我们没配对"—— 那会问出一个"它不认我们"的假答案，
        // 进而把一条好好的地址当垃圾扔掉。这一轮跳过它，下一轮（5s 后）再说。
        let token = match crate::credential::get_peer_token(&p.id) {
            Ok(t) => t,
            Err(e) => {
                warn!(peer_id = %p.id, "凭据库读失败，这一轮跳过探活: {e}");
                continue;
            }
        };
        let seen = fetch_identity(client, &p.ip, p.port, token.as_deref()).await;
        match probe_step(&p.id, seen) {
            ProbeStep::Online => {
                state.set_peer_online(&p.id, true);
                // 离线→在线：这就是"对端回到网上了"这个证据。正在待命的拨号循环光靠
                // 自己的 15s 轮询要慢一拍，这里直接把它叫起来（已经在跑的循环不会被动）。
                if !p.online {
                    crate::ws::redial_if_remembered(state, &p.id);
                }
            }
            ProbeStep::Offline => {
                if p.online {
                    info!(peer_id = %p.id, "peer unreachable, marking offline");
                }
                state.set_peer_online(&p.id, false);
            }
            // 地址上回答的是另一台设备：对端换过 device id。别把这一行当成"它在线"
            // ——那正是列表里同一台设备两行（旧行已绑定、新行未绑定）的来源。
            ProbeStep::OtherIdentity(identity) => {
                apply_identity_reconcile(state, &p.id, &identity);
            }
        }
    }
}

/// 向一个地址问清身份并按结果收拾旧行（探活与 mDNS 发现共用的入口）。
pub async fn reconcile_stale_identity(
    state: &Arc<AppState>,
    client: &reqwest::Client,
    stale_id: &str,
) {
    let Some(stale) = state.get_peer(stale_id) else {
        return;
    };
    if stale.ip.is_empty() || stale.port == 0 {
        return;
    }
    let token = match crate::credential::get_peer_token(stale_id) {
        Ok(t) => t,
        // 读不到不是"没配对"：带着错的前提去问，会得到一个错的身份结论（进而
        // 把一条好好的地址当垃圾扔掉）。交给下一轮。
        Err(e) => {
            warn!(%stale_id, "凭据库读失败，暂不处理这一行: {e}");
            return;
        }
    };
    let Some(seen) = fetch_identity(client, &stale.ip, stale.port, token.as_deref()).await else {
        return; // 问不到就不妄动：这一行交给探活去判死活
    };
    apply_identity_reconcile(state, stale_id, &seen);
}

/// 已经拿到"这个地址上现在是谁"之后怎么处置那一行（网络部分见 `fetch_identity`）。
///
/// 两条路，都不会留下"同一台设备两行"：
///
/// - **它还认我们**（`paired_as` = 本机 id）→ 对端被清的只是数据目录，配对凭据在
///   系统凭据库里（那是另一处存储）。这一行**迁到新 id**：配对状态、地址、"长连接"
///   记忆都跟着过去，凭据的键也换掉（token 值不变，两边照旧互相认）。
/// - **它不认我们** → 关系确实断了（对端的凭据库也清了）。只作废这一行的地址、标记
///   离线，配对记录与 token 都留着 —— 对端拿原 id 回来时这一行还能用。列表里剩下的
///   是它真正的新身份（mDNS 发现的那一行）。
///
/// 返回 `true` = 这一行被迁到了新 id。
pub fn apply_identity_reconcile(
    state: &Arc<AppState>,
    stale_id: &str,
    seen: &PeerIdentity,
) -> bool {
    let Some(stale) = state.get_peer(stale_id) else {
        return false;
    };
    if seen.id.is_empty() || seen.id == stale_id {
        return false; // 还是它自己：没有什么可收拾的
    }
    let own_id = state.config.lock().device_id.clone();
    // 这一步直接决定要不要动这一行（迁 id / 作废地址），所以读失败必须**什么都不做**：
    // 把"读不到"当成"它不认我们"，会拿瞬时故障去删一条有效的地址。
    let token = match crate::credential::get_peer_token(stale_id) {
        Ok(t) => t,
        Err(e) => {
            warn!(%stale_id, "凭据库读失败，暂不处置这一行: {e}");
            return false;
        }
    };

    if token.is_some() && seen.paired_as.as_deref() == Some(own_id.as_str()) {
        info!(
            old = %stale_id,
            new = %seen.id,
            addr = %format!("{}:{}", stale.ip, stale.port),
            "对端换过 device id（配置被清、凭据库还在）：配对关系迁到新 id"
        );
        if let Err(e) = crate::credential::rekey_peer(stale_id, &seen.id) {
            warn!(%e, "凭据换键失败，新 id 会以未绑定出现");
        }
        // 旧 id 上的链路与「断开」暂停标记一起作废、转到新 id：链路按 token 建，
        // 换 key 后新 id 继续用；留着旧 key 会让界面同时显示"连着长连接的旧行"。
        let was_paused = state.ws.is_paused(stale_id);
        state.ws.drop_peer(stale_id);
        if was_paused {
            state.ws.pause(&seen.id);
        }
        state.rekey_peer(
            stale_id,
            &PeerDevice {
                id: seen.id.clone(),
                name: if seen.name.is_empty() { stale.name.clone() } else { seen.name.clone() },
                ip: stale.ip.clone(),
                port: stale.port,
                online: true,
                paired: true,
                auto_connect: stale.auto_connect,
            },
        );
        if stale.auto_connect && !was_paused {
            crate::ws::restore_link(state, &seen.id);
        }
        true
    } else {
        warn!(
            old = %stale_id,
            new = %seen.id,
            addr = %format!("{}:{}", stale.ip, stale.port),
            "地址上已是另一台设备（id 变了），且它不认本机的凭据：作废这一行存的地址"
        );
        state.forget_peer_address(stale_id);
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(id: &str, paired_as: Option<&str>) -> PeerIdentity {
        PeerIdentity {
            id: id.to_string(),
            name: "PC-B".to_string(),
            paired_as: paired_as.map(str::to_string),
        }
    }

    /// 探活的三条路：答话的是它 / 答话的是别人 / 没人答话。
    #[test]
    fn probe_step_only_calls_it_online_when_the_id_matches() {
        assert!(matches!(probe_step("me", None), ProbeStep::Offline));
        assert!(matches!(
            probe_step("me", Some(identity("me", None))),
            ProbeStep::Online
        ));
        // 问不出 id（老版本/代理）时不把行判死
        assert!(matches!(
            probe_step("me", Some(identity("", None))),
            ProbeStep::Online
        ));
        // 同一个地址、另一个 id：这一行不是它了
        match probe_step("old", Some(identity("new", None))) {
            ProbeStep::OtherIdentity(seen) => assert_eq!(seen.id, "new"),
            other => panic!("应判为「地址上是另一台设备」，得到 {other:?}"),
        }
    }
}