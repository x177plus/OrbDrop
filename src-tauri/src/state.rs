use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::AtomicU16;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::http::StatusCode;
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

/// 长连接意图（F-16）：`PeerDevice::auto_connect` / `PeerDevice::paused` 这对字段的语义，
/// 也是它们的唯一写点 [`AppState::set_link_intent`] 的入参。
///
/// 两个字段**互斥**（同一个意图的两面：本机要不要拨 / 接不接对端的自动重拨），所以
/// 它们不该被分头写 —— 一边写"不拨"、另一边忘了写"不接"，就得到那条自相矛盾的状态：
/// 本机安静，却被对端的记忆把链路接回来。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkIntent {
    /// 要这条链：点过「连接」，或链路已经建起来（被拨号的一端同样算）。
    Wanted,
    /// 点过「断开」：本机不主动拨，也拒绝对端的自动重拨（403）。
    Paused,
}

impl LinkIntent {
    /// 落到地址簿的那对字段：(本机主动拨, 拒绝对端的自动重拨)。
    fn fields(self) -> (bool, bool) {
        match self {
            LinkIntent::Wanted => (true, false),
            LinkIntent::Paused => (false, true),
        }
    }
}

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
    /// 会话（聊天）记录：文本消息的落盘 + 界面快照（`chat.json`）。
    pub chat: crate::chat::ChatStore,
    /// Tauri handle for emitting UI events.
    pub app_handle: Mutex<Option<AppHandle>>,
    /// File watchers keyed by share id.
    pub watchers: Mutex<std::collections::HashMap<String, notify::RecommendedWatcher>>,
    /// 「学来的」设备（F-10 跨网段）：对端 `/discover` 表里带过来、而本机自己从没发现过的
    /// 设备 id → 最近一次被提到的时刻。
    ///
    /// 为什么单开一张表而不是塞进 `PeerDevice`：这个状态**只属于本进程**（重启即忘），
    /// 而 `PeerDevice` 是要落盘进地址簿的形状。存在这里的 id，`peers` 里对应那行就是
    /// "临时条目"—— 不写地址簿、不被探活接管、超时由 `sweep_learned` 收走。
    pub learned: Mutex<HashMap<String, Instant>>,
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
                    auto_connect: false, paused: false,
                });
            } else if let Some(p) = peers.iter_mut().find(|p| &p.id == id) {
                p.paired = true;
            }
        }

        // 「断开」的暂停标记从地址簿读回来（F-16）：它和 `auto_connect` 是同一个意图的
        // 两面，重启后必须一起生效。少了这一半，本机是不主动拨了，却会照收对端记忆里
        // 的那次拨号 —— 用户看到的就是"点了断开、退出重开又自己连上了"。
        // 顺带把理由摆到设备行上（`linkState`），界面才说得出"为什么它不自动连"。
        let ws = WsManager::new();
        for p in peers.iter().filter(|p| p.paused) {
            ws.set_paused(&p.id, true);
            ws.set_link_state(
                &p.id,
                crate::ws::LinkState::Stopped { reason: crate::ws::StopReason::LocalDisconnected },
            );
        }

        // 会话记录（`chat.json`）放在 config.toml **旁边**，不写死全局数据目录：
        // 它和配置一样是"这个实例的用户数据"，实例换了个家（便携部署 / 测试指向临时目录），
        // 会话记录就该跟着走。
        let chat_path = match config_path.parent() {
            Some(dir) => dir.join("chat.json"),
            None => crate::config::data_dir().join("chat.json"),
        };

        Self {
            config_path,
            config: Mutex::new(config),
            peers: Mutex::new(peers),
            server_port: AtomicU16::new(0),
            server_shutdown: Mutex::new(None),
            server_task: Mutex::new(None),
            mdns_daemon: Mutex::new(None),
            ws,
            push: PushStore::new(),
            drag: crate::drag::DragStore::new(),
            pair: PairStore::new(),
            chat: crate::chat::ChatStore::with_path(chat_path),
            app_handle: Mutex::new(None),
            watchers: Mutex::new(std::collections::HashMap::new()),
            learned: Mutex::new(HashMap::new()),
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

    /// 记下这台设备的长连接意图（F-16）。
    ///
    /// 这是 `auto_connect` / `paused` 这一对字段的**唯一写点**（落盘 + 同步运行时的
    /// 暂停标记）。分开写两个字段迟早会写出"本机不拨、却照收对端的自动重拨"—— 那正是
    /// 「断开」被对端记忆悄悄撤销的样子。
    ///
    /// - `Wanted`：点过「连接」，或链路已经建起来（两侧都算，见 `ws::session`）——
    ///   写点在**链路建立那一刻**而不是退出时，所以进程被强杀、断电也不会漏掉这次长连接。
    /// - `Paused`：用户点过「断开」——本机不拨，也不接对端的自动重拨；重启照样算数
    ///   （`AppState::new` 把它读回内存，`server::events_ws_handler` 照旧回 403）。
    ///
    /// 只对地址簿（`config.peers`）里的设备落盘：纯发现条目没有可恢复的身份，重启后
    /// 本来也不会出现在列表里（也不会有链接可拒 —— WS 升级本来就要 token）。
    pub fn set_link_intent(&self, peer_id: &str, intent: LinkIntent) {
        let (auto_connect, paused) = intent.fields();
        self.ws.set_paused(peer_id, paused);
        {
            let mut peers = self.peers.lock();
            if let Some(p) = peers.iter_mut().find(|p| p.id == peer_id) {
                p.auto_connect = auto_connect;
                p.paused = paused;
            }
        }
        let mut config = self.config.lock();
        let Some(p) = config.peers.iter_mut().find(|p| p.id == peer_id) else {
            return;
        };
        if p.auto_connect == auto_connect && p.paused == paused {
            return;
        }
        p.auto_connect = auto_connect;
        p.paused = paused;
        let _ = crate::config::save_config_at(&self.config_path, &config);
    }

    /// 本机是否「在线」（面板右上角状态框）。离线 = 隐身，见 `Config::online`。
    ///
    /// 别和 `set_peer_online` 搞混：那个是**对端**是否在网上，这个是**本机自己**是否
    /// 对局域网可见。
    pub fn is_online(&self) -> bool {
        self.config.lock().online
    }

    /// 记住本机「在线 / 离线」并落盘（用户意图，跨重启有效）。
    ///
    /// 只写配置：运行时的连带动作（注销 mDNS、断链、拒收入站请求）在 `lib.rs::set_online`，
    /// 那里才有 daemon 与运行时。
    pub fn set_local_online(&self, on: bool) {
        let mut config = self.config.lock();
        if config.online == on {
            return;
        }
        config.online = on;
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

    /// 把一台设备的行换个 id，其余照旧（配对状态、地址、长连接意图都保留）。
    ///
    /// 用在对端被重置过、但配对凭据还在（本机的凭据文件里那一条没跟着清）的场合：关系
    /// 还活着，只是对端的 id 变了。列表里**只留一行** —— 新 id 那行（mDNS 刚发现的）
    /// 并进这一行，不是并排出现两行。
    ///
    /// `to` 只提供**新 id 那一行的观察值**（名字/地址/在线）。`auto_connect` /
    /// `paused` 是用户意图、不属于新发现的这一行：照搬 `to` 的会把刚点过「断开」的
    /// 设备在新 id 上重新变成"要这条链"（`to` 是发现形状的行，两个字段恒为 false）。
    /// 意图跟着这一行走，不跟着 `to` 走。
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

    // ------------------------------------------------------------------
    // 跨网段发现：对端报来的设备（F-10 延伸）
    // ------------------------------------------------------------------

    /// 这台设备是不是「学来的」（本机自己从没发现过、由某台对端转述）。学来的行不参与
    /// 探活：本机跟它们之间没有任何关系，存活由担保人的 `/discover` 表刷新。
    pub fn is_learned(&self, peer_id: &str) -> bool {
        self.learned.lock().contains_key(peer_id)
    }

    /// 把对端（担保人）报来的「它那一段在线的设备」并进列表。
    ///
    /// 三条规则，缺一不可：
    ///
    /// - **只接手没见过的 id**：已经在本机列表里的行（mDNS 发现的、手动添加的、已配对的）
    ///   一个字都不动。那是"本机自己知道的"，不该被对端的转述改写，更不能让它被这一层
    ///   的超时清掉 —— 否则 B 一离线，A 连自己 mDNS 看得见的设备都会掉。
    /// - **临时**：不写地址簿、不进凭据库，重启即忘。本机没跟它们建立过任何关系。
    /// - **存活跟着担保人**：刷新时间戳。担保人不再提到它（或担保人自己掉了、问不到了），
    ///   超过 [`LEARNED_TTL`] 就由 [`Self::sweep_learned`] 收走。
    pub fn learn_peers(&self, voucher_id: &str, learned: Vec<PeerDevice>) {
        let now = Instant::now();
        let own_id = self.config.lock().device_id.clone();
        let mut changed = false;
        {
            let mut table = self.learned.lock();
            let mut peers = self.peers.lock();
            for l in learned {
                // 自己、报它的那一方、以及没有地址的条目：进不了这张表。
                if l.id.is_empty() || l.id == own_id || l.id == voucher_id {
                    continue;
                }
                if l.ip.is_empty() || l.port == 0 {
                    continue;
                }
                match peers.iter_mut().find(|p| p.id == l.id) {
                    Some(existing) => {
                        // 已经是本机自己的行（mDNS/手动/配对）：不接管它的存活，也不改它。
                        if !table.contains_key(&l.id) {
                            continue;
                        }
                        if existing.ip != l.ip || existing.port != l.port {
                            info!(
                                peer_id = %l.id,
                                was = %format!("{}:{}", existing.ip, existing.port),
                                now = %format!("{}:{}", l.ip, l.port),
                                "学来的对端换了地址"
                            );
                            existing.ip = l.ip;
                            existing.port = l.port;
                        }
                        existing.name = l.name;
                        existing.online = true;
                        table.insert(l.id.clone(), now);
                        changed = true;
                    }
                    None => {
                        peers.push(PeerDevice {
                            id: l.id.clone(),
                            name: l.name,
                            ip: l.ip,
                            port: l.port,
                            online: true,
                            paired: false,
                            auto_connect: false, paused: false,
                        });
                        table.insert(l.id, now);
                        changed = true;
                    }
                }
            }
        }
        if changed {
            self.emit_devices_changed();
        }
    }

    /// 收走超时的「学来的」条目：担保人最近 [`LEARNED_TTL`] 内没再提到它们。
    ///
    /// 已经变成"本机自己的关系"的行不归这一层管 —— 用户手动添加过（地址簿里有它），
    /// 或者已经配对（凭据库里有 token）。那些行该由配对与探活维护，这里只把记录
    /// 从表里摘掉，行留着。
    pub fn sweep_learned(&self) {
        let now = Instant::now();
        let expired: Vec<String> = {
            let table = self.learned.lock();
            table
                .iter()
                .filter(|(_, seen)| now.duration_since(**seen) > LEARNED_TTL)
                .map(|(id, _)| id.clone())
                .collect()
        };
        if expired.is_empty() {
            return;
        }

        let adopted: HashSet<String> = {
            let config = self.config.lock();
            let paired = crate::credential::load_peers().unwrap_or_default();
            expired
                .iter()
                .filter(|id| {
                    config.peers.iter().any(|p| &p.id == *id) || paired.contains_key(*id)
                })
                .cloned()
                .collect()
        };

        let mut removed = false;
        {
            let mut table = self.learned.lock();
            let mut peers = self.peers.lock();
            for id in &expired {
                table.remove(id);
                if adopted.contains(id) {
                    continue;
                }
                let before = peers.len();
                peers.retain(|p| &p.id != id);
                removed |= peers.len() != before;
            }
        }
        if removed {
            self.emit_devices_changed();
        }
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
// 证书指纹（TLS 信任）
// ---------------------------------------------------------------------------

/// 把凭据库里钉住的指纹灌进 `net::Trust`：TLS 校验器**每一次握手**读的就是那份快照。
///
/// 整份重建而不是增量改：地址簿与凭据库才是真相。凡是这两处变了（配对、解除配对、地址
/// 漂移、指纹落库、对端换 id），都要跟着调一次 —— 少调一次，表现就是"刚配对好却连不上"。
pub fn refresh_trust(state: &Arc<AppState>) {
    let credentials = crate::credential::load_credentials().unwrap_or_default();
    let peers = state.peers.lock();
    let entries = peers.iter().filter_map(|peer| {
        let pinned = credentials.get(&peer.id)?.fingerprint.clone();
        if pinned.is_empty() || peer.ip.is_empty() {
            return None;
        }
        Some((peer.ip.clone(), pinned))
    });
    crate::net::trust().seed(entries);
}

/// 记下这次连上对端时见到的证书指纹（TOFU 的落库那一半）。
///
/// 只在**本机还没有它的指纹**时写：已经有了，就说明这次握手正是拿那枚指纹校验过的，
/// 没有别的可记。调用点都在"刚跟对端成功建立了一条 TLS 连接"的地方。
pub fn remember_peer_fingerprint(state: &Arc<AppState>, peer_id: &str, ip: &str) {
    if matches!(
        crate::credential::get_peer_fingerprint(peer_id),
        Ok(Some(_))
    ) {
        return;
    }
    let Some(observed) = crate::net::trust().observed_at(ip) else {
        return; // 这一跳没走 TLS（回环 / 测试），没什么可记
    };
    match crate::credential::set_peer_fingerprint(peer_id, &observed) {
        Ok(()) => {
            info!(
                %peer_id,
                fingerprint = %crate::net::short_fingerprint(&observed),
                "已钉住对端证书指纹"
            );
            refresh_trust(state);
        }
        Err(e) => warn!(%peer_id, "指纹落库失败: {e}"),
    }
}

// ---------------------------------------------------------------------------
// 设备身份核对（同一台设备不能有两行）
// ---------------------------------------------------------------------------

/// 「学来的」条目能活多久：担保人超过这段时间没再提到它，就当作它已经不在那一段了。
///
/// 取探活周期的整数倍（5s → 4 拍）：对端报来一次就够它活着，中间丢一拍不会误伤；
/// 真的下线了，最迟 20 秒从列表里消失。
const LEARNED_TTL: Duration = Duration::from_secs(20);

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
///
/// **`Rejected` 必须与 `Unreachable` 分开**：前者是"这个地址上的身份不是本机认的那一个"
/// （对端换了机器 / 有人在中间），它既不是"离线"、也不是"换了 id 的另一台设备"，而是一条
/// 明确的关系问题（界面显示「需配对」）；后者只是"这会儿问不到"，判离线、等下一轮。
#[derive(Debug)]
pub enum IdentityProbe {
    Seen(PeerIdentity),
    Rejected,
    Unreachable,
}

pub async fn probe_identity(
    client: &reqwest::Client,
    ip: &str,
    port: u16,
    token: Option<&str>,
) -> IdentityProbe {
    let url = format!("{}/pair/info", crate::net::peer_base(ip, port));
    let mut request = client.get(&url);
    if let Some(t) = token {
        request = request.bearer_auth(t);
    }
    let response = match request.send().await {
        Ok(response) => response,
        Err(e) => {
            return if crate::net::is_identity_rejected(&e) {
                IdentityProbe::Rejected
            } else {
                IdentityProbe::Unreachable
            };
        }
    };
    let response = match response.error_for_status() {
        Ok(response) => response,
        Err(_) => return IdentityProbe::Unreachable,
    };
    match response.json::<serde_json::Value>().await {
        Ok(body) => IdentityProbe::Seen(PeerIdentity {
            id: body["id"].as_str().unwrap_or_default().to_string(),
            name: body["name"].as_str().unwrap_or_default().to_string(),
            paired_as: body["pairedAs"].as_str().map(str::to_string),
        }),
        Err(_) => IdentityProbe::Unreachable,
    }
}

/// 只要"问到了"的那一份（指纹被拒 / 问不到都折叠成 `None`）。给"问不到就不妄动"的
/// 调用方用；需要区分原因的地方用 `probe_identity`。
pub async fn fetch_identity(
    client: &reqwest::Client,
    ip: &str,
    port: u16,
    token: Option<&str>,
) -> Option<PeerIdentity> {
    match probe_identity(client, ip, port, token).await {
        IdentityProbe::Seen(identity) => Some(identity),
        _ => None,
    }
}

/// [`fetch_discover`] 的三种结果。
#[derive(Debug)]
pub enum DiscoverOutcome {
    /// 对端答了：它是谁 + 它那一段里在线的设备。
    Ok {
        identity: PeerIdentity,
        peers: Vec<PeerDevice>,
    },
    /// 这一问在这台对端上得不到答案（老版本没有这个端点；或者它不认我们手里的 token）。
    /// 退回 `/pair/info` —— 那一问不带 token 也答，能问出"它还认不认我们"。
    Fallback,
    /// 问不到（连不上、或对端此刻隐身/离线）。**不要再打第二遍**：对端此刻不算在线。
    Unreachable,
}

/// 向一个地址要「你是谁 + 你这一段里在线的设备」（`/discover`，跨网段发现）。
///
/// `own_id` 只用来填 `paired_as`：这一问带的是本机的 token，对端**认了**（否则 401，
/// 走 `Fallback`），所以"它把带这个 token 的调用方认作谁"的答案恒等于本机 id。
/// 对端换过 device id 的情形不在这里 —— 那时它不认旧 token，401 退回 `/pair/info`，
/// 由那条路去问出新的 id。
pub async fn fetch_discover(
    client: &reqwest::Client,
    ip: &str,
    port: u16,
    token: &str,
    own_id: &str,
) -> DiscoverOutcome {
    let url = format!("{}/discover", crate::net::peer_base(ip, port));
    let response = match client.get(&url).bearer_auth(token).send().await {
        Ok(r) => r,
        Err(_) => return DiscoverOutcome::Unreachable,
    };

    match response.status() {
        s if s.is_success() => {}
        // 401/403：这个 token 它不认（被解除配对、或对端换过 id）；404/405：老版本没有
        // 这个端点。两种都得回到 `/pair/info` 才有答案。
        StatusCode::UNAUTHORIZED
        | StatusCode::FORBIDDEN
        | StatusCode::NOT_FOUND
        | StatusCode::METHOD_NOT_ALLOWED => return DiscoverOutcome::Fallback,
        // 503 隐身、5xx：当作问不到，语义与探活一致（非 200 即不在网上）。
        _ => return DiscoverOutcome::Unreachable,
    }

    let body: serde_json::Value = match response.json().await {
        Ok(v) => v,
        Err(_) => return DiscoverOutcome::Fallback,
    };
    let Some(peer_id) = body["id"].as_str().filter(|s| !s.is_empty()) else {
        // 答得不是这个形状（中间有代理、或对端是自己拼的响应）→ 走老路，别瞎认。
        return DiscoverOutcome::Fallback;
    };

    let peers = body["peers"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| {
                    Some(PeerDevice {
                        id: v["id"].as_str()?.to_string(),
                        name: v["name"].as_str().unwrap_or_default().to_string(),
                        ip: v["ip"].as_str()?.to_string(),
                        port: u16::try_from(v["port"].as_u64()?).ok()?,
                        online: true,
                        paired: false,
                        auto_connect: false, paused: false,
                    })
                })
                .collect()
        })
        .unwrap_or_default();

    DiscoverOutcome::Ok {
        identity: PeerIdentity {
            id: peer_id.to_string(),
            name: body["name"].as_str().unwrap_or_default().to_string(),
            paired_as: Some(own_id.to_string()),
        },
        peers,
    }
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

/// 探活的客户端。**必须是带 pinning 的那一个**（`net::client_builder`）：到对端那一跳
/// 是 TLS，随手 `Client::new()` 只会在自签证书上失败 —— 表现是"所有设备都离线"，
/// 而且看不出原因。所以这个客户端不由调用方传进来，免得哪一处传错了。
static PROBE_CLIENT: std::sync::LazyLock<reqwest::Client> = std::sync::LazyLock::new(|| {
    crate::net::client_builder()
        .timeout(Duration::from_millis(1500))
        .build()
        .unwrap_or_default()
});

/// 身份核对用的客户端（超时比探活宽一点：这一步的回话更长，一问一答里带整段设备表）。
static RECONCILE_CLIENT: std::sync::LazyLock<reqwest::Client> = std::sync::LazyLock::new(|| {
    crate::net::client_builder()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap_or_default()
});

/// 一轮探活：给每台"有地址"的设备问一次身份，判死活或收拾换了 id 的行。
///
/// 由 lib.rs 的 5s 循环调用；单独成函数是为了让"地址上现在是另一台设备"这条路径
/// 能在测试里对着真服务器跑（见 tests/p1_integration.rs）。
pub async fn probe_peers_once(state: &Arc<AppState>) {
    let client = &PROBE_CLIENT;
    // 「学来的」行不在这里探：它们不是本机发现的，本机跟它们之间没有关系，拨过去既没有
    // 意义（可达性由"和担保人同子网"保证），也正是这条路要避免的多余一跳。它们的存活由
    // 担保人的 `/discover` 表刷新 —— 见 `learn_peers` / `sweep_learned`。
    let learned: HashSet<String> = state.learned.lock().keys().cloned().collect();
    let own_id = state.config.lock().device_id.clone();

    let candidates: Vec<PeerDevice> = state
        .peers
        .lock()
        .iter()
        .filter(|p| !p.ip.is_empty() && p.port != 0 && !learned.contains(&p.id))
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
        // 有 token 就走 `/discover`：它一举两得 —— 既认出对端是谁，又把「对端那一段里
        // 在线的设备」带回来（跨网段发现）。没有 token 的（未绑定/只手动加过）仍旧只问
        // 身份：它们不该看到本机认识了谁。
        let probe = match token.as_deref() {
            Some(t) => match fetch_discover(client, &p.ip, p.port, t, &own_id).await {
                DiscoverOutcome::Ok { identity, peers } => {
                    state.learn_peers(&p.id, peers);
                    IdentityProbe::Seen(identity)
                }
                DiscoverOutcome::Fallback => {
                    probe_identity(client, &p.ip, p.port, token.as_deref()).await
                }
                DiscoverOutcome::Unreachable => IdentityProbe::Unreachable,
            },
            None => probe_identity(client, &p.ip, p.port, token.as_deref()).await,
        };
        // 指纹对不上：这个地址上的身份不是本机认的那一个。既不是"离线"（它答话了），
        // 也不是"换了 id 的另一台设备"（那是同一台机器上的新身份，有 token 可以迁）——
        // 这是一条明确的关系问题，界面上就该显示「需配对」。
        if matches!(probe, IdentityProbe::Rejected) {
            warn!(peer_id = %p.id, "对端证书指纹与钉住的不一致：需重新配对");
            state.set_peer_online(&p.id, false);
            state.ws.set_link_state(
                &p.id,
                crate::ws::LinkState::Stopped {
                    reason: crate::ws::StopReason::IdentityChanged,
                },
            );
            continue;
        }
        let seen = match probe {
            IdentityProbe::Seen(identity) => {
                // 这一问走的是 TLS：连上了就把这次见到的指纹补上（老记录、或首次连上）。
                remember_peer_fingerprint(state, &p.id, &p.ip);
                Some(identity)
            }
            _ => None,
        };
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
                // 换 id 时凭据整条（token + 指纹）跟着迁，地址簿也变了 —— Trust 那张
                // "地址 → 指纹"的表得重灌一次，否则新 id 那条记录不会生效。
                refresh_trust(state);
            }
        }
    }

    // 担保人这一轮没再提到、或担保人自己掉了的学习条目，从这里收走。放在一轮之末：
    // 这一轮里刚被提到的都已经刷新过时间戳，不会误伤。
    state.sweep_learned();
}

/// 向一个地址问清身份并按结果收拾旧行（探活与 mDNS 发现共用的入口）。
pub async fn reconcile_stale_identity(state: &Arc<AppState>, stale_id: &str) {
    let client = &RECONCILE_CLIENT;
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
/// - **它还认我们**（`paired_as` = 本机 id）→ 对端被清的只是它那边的一部分状态，本机手里
///   那条凭据还有效。这一行**迁到新 id**：配对状态、地址、"长连接"
///   记忆都跟着过去，凭据的键也换掉（token 与指纹不变，两边照旧互相认）。
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
                // 这一行只是观察值：配对状态与地址从旧行搬（见 `rekey_peer`），长连接
                // 意图（`auto_connect` / `paused`）同样跟着旧行走，不在这里填。
                ..Default::default()
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