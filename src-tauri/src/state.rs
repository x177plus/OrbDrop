use std::path::PathBuf;
use std::sync::atomic::AtomicU16;
use std::sync::Arc;

use mdns_sd::ServiceDaemon;
use parking_lot::Mutex;
use tauri::AppHandle;
use tauri::Emitter;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

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
    pub fn add_peer_book(&self, peer: PeerDevice) {
        {
            let mut peers = self.peers.lock();
            if !peers.iter().any(|p| p.id == peer.id) {
                peers.push(peer.clone());
            }
        }
        {
            let mut config = self.config.lock();
            if !config.peers.iter().any(|p| p.id == peer.id) {
                config.peers.push(peer);
                let _ = crate::config::save_config_at(&self.config_path, &config);
            }
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
        self.emit_devices_changed();
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
    /// 设备还在 mDNS 视野里就该继续出现在列表里（可以再点「配对」把关系建回来），
    /// 离线时才自然消失（前端只显示 online 的设备）。地址簿（config.peers）里删掉，
    /// 重启后不再记着它 —— 除非它再次被发现。
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