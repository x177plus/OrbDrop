pub mod events;
pub mod session;

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use parking_lot::Mutex;
use tauri::Emitter;
use tokio::sync::mpsc;
use tokio::time::interval;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tracing::{error, info, warn};

use crate::credential;
use crate::state::AppState;
use crate::ws::events::WsEvent;

// ---------------------------------------------------------------------------
// WS Manager
// ---------------------------------------------------------------------------

/// One live WS session with a peer.
///
/// `gen` identifies the session for [`WsManager::unregister`]: only the session
/// that registered a slot may clear it. Without that, a dying session removes the
/// entry a newer session just installed — and since dropping the last sender is
/// what ends an inbound session, the two links then kill each other.
struct Session {
    gen: u64,
    tx: mpsc::UnboundedSender<WsEvent>,
}

pub struct WsManager {
    /// Peer connections: peer_id → live sessions (usually one; two when both
    /// sides dial at the same instant — they must coexist, not fight).
    senders: Mutex<HashMap<String, Vec<Session>>>,
    /// Task handles (for abort on unpair).
    tasks: Mutex<HashMap<String, tokio::task::JoinHandle<()>>>,
    /// Peers the user disconnected by hand（「断开」）: their dial-loop reconnects
    /// are turned away (403) until the user hits 「连接」 here or the peer asks to
    /// connect with its own 「连接」. Without this the peer's retry a second later
    /// would silently undo the click and the "断开" button would be a lie.
    paused: Mutex<HashSet<String>>,
    next_gen: AtomicU64,
}

impl WsManager {
    pub fn new() -> Self {
        Self {
            senders: Mutex::new(HashMap::new()),
            tasks: Mutex::new(HashMap::new()),
            paused: Mutex::new(HashSet::new()),
            next_gen: AtomicU64::new(1),
        }
    }

    /// Send an event to a connected peer; returns false if not connected.
    pub fn send(&self, peer_id: &str, event: WsEvent) -> bool {
        let mut senders = self.senders.lock();
        let Some(sessions) = senders.get_mut(peer_id) else { return false };
        // 新会话优先；它刚死掉时（还没注销）退回旧会话。
        while let Some(session) = sessions.last() {
            if session.tx.send(event.clone()).is_ok() {
                return true;
            }
            sessions.pop();
        }
        senders.remove(peer_id);
        false
    }

    /// True if a channel is registered for the peer (connection exists).
    pub fn has_connection(&self, peer_id: &str) -> bool {
        self.senders.lock().get(peer_id).is_some_and(|s| !s.is_empty())
    }

    /// 此刻处于长连接的对端 id 集合。共享项可见性与变更广播都按这个集合算，
    /// 不再读「开播那一刻的快照」。
    pub fn connected_peers(&self) -> Vec<String> {
        self.senders
            .lock()
            .iter()
            .filter(|(_, s)| !s.is_empty())
            .map(|(id, _)| id.clone())
            .collect()
    }

    /// Register a session's channel; returns the generation to hand back to
    /// [`WsManager::unregister`].
    pub fn register(&self, peer_id: &str, tx: mpsc::UnboundedSender<WsEvent>) -> u64 {
        let gen = self.next_gen.fetch_add(1, Ordering::Relaxed);
        let mut senders = self.senders.lock();
        let slot = senders.entry(peer_id.to_string()).or_default();
        let was_empty = slot.is_empty();
        slot.push(Session { gen, tx });
        drop(senders);
        if was_empty {
            info!(%peer_id, gen, "WS session registered (link up)");
        }
        gen
    }

    /// Unregister one session. Only the owner can clear its slot, and the peer
    /// stays connected while any other session is still alive.
    pub fn unregister(&self, peer_id: &str, gen: u64) {
        let mut senders = self.senders.lock();
        let Some(slot) = senders.get_mut(peer_id) else { return };
        let before = slot.len();
        slot.retain(|s| s.gen != gen);
        let emptied = slot.is_empty();
        if emptied {
            senders.remove(peer_id);
        }
        drop(senders);
        if emptied && before > 0 {
            info!(%peer_id, gen, "WS session unregistered (link down)");
        }
    }

    pub fn register_task(&self, peer_id: &str, handle: tokio::task::JoinHandle<()>) {
        self.tasks.lock().insert(peer_id.to_string(), handle);
    }

    /// Kill the live session and the dial loop for a peer, keeping the pause flag.
    fn teardown(&self, peer_id: &str) {
        self.senders.lock().remove(peer_id);
        if let Some(h) = self.tasks.lock().remove(peer_id) {
            h.abort();
        }
    }

    /// 掐掉正在跑的拨号循环（保留暂停标记）。手动「连接」要立刻发起一次新拨号，
    /// 不能干等一个还在指数退避里打转（或正被对端拒绝）的旧循环。
    pub fn abort_dial(&self, peer_id: &str) {
        if let Some(h) = self.tasks.lock().remove(peer_id) {
            h.abort();
        }
    }

    /// Unpair: tear everything down and forget the manual-disconnect intent.
    pub fn drop_peer(&self, peer_id: &str) {
        self.paused.lock().remove(peer_id);
        self.teardown(peer_id);
    }

    /// User hit "断开": drop the link and turn away this peer's retries.
    pub fn pause(&self, peer_id: &str) {
        self.paused.lock().insert(peer_id.to_string());
        self.teardown(peer_id);
    }

    /// User hit "连接" — or the peer asked to connect with its own: let it dial.
    pub fn resume(&self, peer_id: &str) {
        self.paused.lock().remove(peer_id);
    }

    pub fn is_paused(&self, peer_id: &str) -> bool {
        self.paused.lock().contains(peer_id)
    }
}

impl Default for WsManager {
    fn default() -> Self { Self::new() }
}

// ---------------------------------------------------------------------------
// Tauri event helper
// ---------------------------------------------------------------------------

pub fn emit_ui(state: &AppState, event: &str, payload: &serde_json::Value) {
    if let Some(app) = state.app_handle.lock().as_ref() {
        let _ = app.emit(event, payload);
    }
}

/// Convenience: send to peer or warn.
pub fn send_to_peer(state: &AppState, peer_id: &str, event: WsEvent) {
    if !state.ws.send(peer_id, event) {
        warn!(%peer_id, "no WS session to peer");
    }
}

// ---------------------------------------------------------------------------
// Outbound link (we are the initiator — our device_id < peer's)
// ---------------------------------------------------------------------------

/// 用户点「连接」：发一次拨号，把到对端的长连接建起来（F-16）。
///
/// 长连接**只由用户的显式点击发起**：配对只确立"已配对"关系，不碰链路；mDNS 发现、
/// 5s 探活、启动都不再自行拨号。两端都能点，对端点过「断开」、或本机是被拨号的那一侧
/// 时同样算数。拨号带 `manual=1`，对端据此放行它自己的「断开」暂停。
pub fn ensure_link_now(state: &Arc<AppState>, peer_id: &str) {
    match state.get_peer(peer_id) {
        Some(p) if p.paired => {}
        _ => return,
    }
    if state.ws.has_connection(peer_id) {
        return;
    }
    state.ws.abort_dial(peer_id);
    spawn_outbound_link(Arc::clone(state), peer_id.to_string());
}

/// 拨号循环：每一次连接都由用户点击进入这里，直到会话建立并保住。
pub fn spawn_outbound_link(state: Arc<AppState>, peer_id: String) {
    // One link per peer: a live channel means we are already linked. A loop still
    // running is aborted by `ensure_link_now` first — two concurrent loops would
    // unregister each other's channel and flap the peer's online state.
    if state.ws.has_connection(&peer_id) {
        return;
    }

    let state_reg = state.clone();
    let pid_for_register = peer_id.clone();
    let body = async move {
        let mut backoff = 5u64;
        // manual=1 只搭第一次尝试：它是「用户此刻让我连」的一次性声明，不是这个
        // 循环的长期属性 —— 否则会话建起来又掉之后，本机会一直替对端解除它的「断开」。
        let mut manual_pending = true;

        loop {
            // 已经有一条链接就不要重复拨号（对端点「连接」拨上来、或双方同时拨）。
            // 两条链接各自持有 socket，重复拨号会让它们互相踩，界面就会"连-断"狂闪。
            if state.ws.has_connection(&peer_id) {
                tokio::time::sleep(Duration::from_secs(1)).await;
                continue;
            }
            // Address and pairing are re-read every attempt: the address arrives
            // later (mDNS discovery, the pairing handshake) and can change under
            // DHCP, so a snapshot taken at spawn time would either dial nowhere
            // or dial a stale address forever.
            let peer = match state.get_peer(&peer_id) {
                Some(p) if p.paired => p,
                _ => {
                    info!(%peer_id, "peer unpaired or gone, outbound link stopped");
                    return;
                }
            };
            if peer.ip.is_empty() || peer.port == 0 {
                // Not discoverable yet — wait for the address without burning
                // the backoff (a doomed dial would just delay the real one).
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }

            let token = credential::get_peer_token(&peer_id)
                .ok().flatten().unwrap_or_default();
            if token.is_empty() {
                warn!(%peer_id, "no token, outbound link stopped");
                return;
            }

            // manual=1 只在第一次尝试时带上：这是一次"用户让连"的请求，不是持续声明。
            let url = format!(
                "ws://{}:{}/events?token={}{}",
                peer.ip,
                peer.port,
                token,
                if manual_pending { "&manual=1" } else { "" },
            );
            manual_pending = false;
            match connect_async(&url).await {
                Ok((ws_stream, _)) => {
                    info!(%peer_id, "outbound WS linked");
                    state.set_peer_online(&peer_id, true);
                    backoff = 5;

                    let (mut write, mut read) = ws_stream.split();
                    let (tx, mut rx) = mpsc::unbounded_channel::<WsEvent>();
                    let gen = state.ws.register(&peer_id, tx);
                    state.emit_devices_changed();

                    let mut ping_interval = interval(Duration::from_secs(30));
                    let mut missed = 0u32;

                    'session: loop {
                        tokio::select! {
                            _ = ping_interval.tick() => {
                                if let Err(e) = write.send(Message::Ping(vec![].into())).await {
                                    warn!(%peer_id, "ping send: {e}");
                                    break 'session;
                                }
                                missed += 1;
                                if missed >= 3 {
                                    warn!(%peer_id, "3 missed pongs");
                                    break 'session;
                                }
                            }
                            event = rx.recv() => {
                                let event = match event {
                                    Some(e) => e,
                                    None => break 'session,
                                };
                                let json = match serde_json::to_string(&event) {
                                    Ok(s) => s,
                                    Err(e) => { error!(%e, "WS serialize"); continue; }
                                };
                                if let Err(e) = write.send(Message::Text(json.into())).await {
                                    warn!(%peer_id, "write: {e}");
                                    break 'session;
                                }
                            }
                            msg = read.next() => {
                                match msg {
                                    Some(Ok(Message::Text(text))) => {
                                        if let Ok(event) = serde_json::from_str::<WsEvent>(&text) {
                                            session::handle_inbound(&state, &peer_id, event).await;
                                        }
                                    }
                                    Some(Ok(Message::Pong(_))) => { missed = 0; }
                                    Some(Ok(Message::Ping(data))) => {
                                        let _ = write.send(Message::Pong(data)).await;
                                    }
                                    Some(Ok(Message::Close(_))) | None => {
                                        info!(%peer_id, "outbound WS closed");
                                        break 'session;
                                    }
                                    Some(Err(e)) => {
                                        warn!(%peer_id, "read err: {e}");
                                        break 'session;
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }

                    state.ws.unregister(&peer_id, gen);
                    state.emit_devices_changed();
                    // Reachability is not ours to judge: a dropped link says
                    // nothing about whether the peer still answers HTTP (push
                    // works without WS). mDNS and the probe demote it if it is
                    // really gone.
                }
                Err(e) => {
                    warn!(%peer_id, "connect failed: {e}, retry {backoff}s");
                    tokio::time::sleep(Duration::from_secs(backoff)).await;
                    backoff = (backoff * 2).min(60);
                }
            }
        }
    };

    // 拨号任务必须有 Tokio 运行时。Tauri 的**同步**命令跑在主线程（没有 reactor），
    // 直接 tokio::spawn 会 panic 掉整个进程（用户侧表现为点「连接」闪退），
    // 所以走统一收口：没有运行时就不拨号（用户再点一次「连接」即可），绝不 panic。
    let handle = match crate::task::spawn("outbound ws link", body) {
        Some(h) => h,
        None => return,
    };
    // peer_id was moved into the async block; register via the pre-cloned id.
    state_reg.ws.register_task(&pid_for_register, handle);
}