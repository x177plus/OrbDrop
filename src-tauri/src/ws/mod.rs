pub mod events;
pub mod session;

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use parking_lot::Mutex;
use rand::Rng;
use serde::Serialize;
use tauri::Emitter;
use tokio::sync::mpsc;
use tokio::time::interval;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tracing::{debug, error, info, warn};

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

/// 拨号循环对外可见的状态。
///
/// `connected`（有没有会话）由 `has_connection` 说了算，这里补的是**"没连着的时候为什么"**：
/// 重拨策略里任何"停手"的决定都必须能自证，否则用户只看到"已配对"却不知道发生了什么，
/// 排障时也说不清（"重启后没接回来"就是因为这个说不清才查了半天）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum LinkState {
    /// 有实时 WS 会话。
    Connected,
    /// 正在拨号 / 退避重试中。
    Retrying,
    /// 对端此刻不在网上，等它出现（有证据再拨，不再空发 TCP）。
    Waiting,
    /// 停手了，`reason` 说明为什么（界面显示成人话）。
    Stopped { reason: StopReason },
}

/// 停手的原因。**只有明确拒绝才算停手**：网络抖动/对端没开机都是"等一等再试"。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum StopReason {
    /// 对端点过「断开」：它的 403 是明确拒绝，再拨也不会变 —— 等它点「连接」拨过来，
    /// 或我们这边点「连接」（`manual=1` 会替它结束暂停）。
    PeerDisconnected,
    /// 对端不认我们的 token（它清过数据 / 已经解除配对）：只能重新配对。
    TokenRejected,
    /// 本机点过「断开」。
    LocalDisconnected,
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
    /// Why a peer currently has no link（[`LinkState`]）. Absent = 没有拨号意图
    /// （比如只有长连接记忆但对端从没连过、或被解除配对清过）——界面什么都不用说。
    links: Mutex<HashMap<String, LinkState>>,
    next_gen: AtomicU64,
}

impl WsManager {
    pub fn new() -> Self {
        Self {
            senders: Mutex::new(HashMap::new()),
            tasks: Mutex::new(HashMap::new()),
            paused: Mutex::new(HashSet::new()),
            links: Mutex::new(HashMap::new()),
            next_gen: AtomicU64::new(1),
        }
    }

    /// 记下/覆盖一台设备的拨号状态（`list_devices` 会带给界面）。
    pub fn set_link_state(&self, peer_id: &str, state: LinkState) {
        self.links.lock().insert(peer_id.to_string(), state);
    }

    /// 清掉状态：没有拨号意图了（解除配对、被拨号的会话结束等）。
    pub fn clear_link_state(&self, peer_id: &str) {
        self.links.lock().remove(peer_id);
    }

    pub fn link_state(&self, peer_id: &str) -> Option<LinkState> {
        self.links.lock().get(peer_id).copied()
    }

    /// 这台设备是否已经有一个拨号循环在跑（含正在退避/待命的）。
    ///
    /// "对端上线"这类证据触发要用它去重：已经在跑的循环自己会动手，不该再叠一个
    /// （两个循环会互相 `abort_dial`、界面就是"连-断"狂闪）。
    pub fn has_dial_task(&self, peer_id: &str) -> bool {
        self.tasks.lock().get(peer_id).is_some_and(|h| !h.is_finished())
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
        self.clear_link_state(peer_id);
    }

    /// User hit "断开": drop the link and turn away this peer's retries.
    pub fn pause(&self, peer_id: &str) {
        self.paused.lock().insert(peer_id.to_string());
        self.teardown(peer_id);
        self.set_link_state(
            peer_id,
            LinkState::Stopped { reason: StopReason::LocalDisconnected },
        );
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
/// 长连接只由**用户点「连接」**或**启动时的恢复**发起：配对只确立"已配对"关系，不碰
/// 链路；mDNS 发现与 5s 探活都不自行拨号。两端都能点，对端点过「断开」、或本机是被
/// 拨号的那一侧时同样算数。拨号带 `manual=1`，对端据此放行它自己的「断开」暂停。
pub fn ensure_link_now(state: &Arc<AppState>, peer_id: &str) {
    dial_now(state, peer_id, true, false);
}

/// 重启后恢复上次的长连接（F-16）。
///
/// 地址簿里记着"关机时还连着"的已配对设备，这里逐个把链路拨回来，用户不用再点一次
/// 「连接」。**不带 `manual=1`**：这是本机自己的记忆，不是对端有人按了「连接」——
/// 对端点过「断开」时照旧被拒，不替它解除暂停。设备晚点上线也没关系：自动拨号先试
/// `AUTO_RETRY_ATTEMPTS` 次，之后转入待命，等对端被观测到在线（探活/mDNS）再动手。
pub fn restore_links_on_startup(state: &Arc<AppState>) {
    let targets: Vec<String> = state
        .config
        .lock()
        .peers
        .iter()
        .filter(|p| p.paired && p.auto_connect)
        .map(|p| p.id.clone())
        .collect();
    for peer_id in targets {
        info!(%peer_id, "restoring long connection from the last session");
        dial_now(state, &peer_id, false, true);
    }
}

/// 单台设备把长连接接回来，语义同启动恢复（**不带** `manual=1`）。
///
/// 对端被重置过、device id 换了之后，这一行会迁到新 id（见
/// `state::apply_identity_reconcile`）：链路得跟着新 id 重新接上。这不是有人按了
/// 「连接」，所以对端点过「断开」时照旧被拒。
pub fn restore_link(state: &Arc<AppState>, peer_id: &str) {
    dial_now(state, peer_id, false, true);
}

/// 「对端重新出现在网上」这个**证据**触发一次接回（探活离线→在线、mDNS 重新发现）。
///
/// 待命状态（`LinkState::Waiting`）最短也要等一次 `IDLE_POLL` 才醒；由这个事实直接
/// 触发，接回是秒级的。只对"记着这次长连接"的设备动手，也不会打扰已经在跑的循环。
pub fn redial_if_remembered(state: &Arc<AppState>, peer_id: &str) {
    if state.ws.is_paused(peer_id)
        || state.ws.has_connection(peer_id)
        || state.ws.has_dial_task(peer_id)
    {
        return;
    }
    let remembered = state
        .config
        .lock()
        .peers
        .iter()
        .any(|p| p.id == peer_id && p.paired && p.auto_connect);
    if !remembered {
        return;
    }
    info!(%peer_id, "peer is back online, re-arming the long connection");
    dial_now(state, peer_id, false, true);
}

/// 一次拨号。`manual` / `auto` 见上面几个公开入口。
fn dial_now(state: &Arc<AppState>, peer_id: &str, manual: bool, auto: bool) {
    match state.get_peer(peer_id) {
        Some(p) if p.paired => {}
        _ => return,
    }
    if state.ws.has_connection(peer_id) {
        return;
    }
    // 一个设备一条链路：先掐掉可能还在退避里打转的旧循环，免得两条循环互踩。
    state.ws.abort_dial(peer_id);
    // 上一次的"停手理由"到此失效：用户点「连接」、对端上线、重启恢复都算重新开始。
    state.ws.set_link_state(peer_id, LinkState::Retrying);
    spawn_outbound_link(Arc::clone(state), peer_id.to_string(), manual, auto);
}

/// 自动拨号最多连续失败几次，之后转入**待命**（`AUTO_RETRY_ATTEMPTS` + `IDLE_POLL`）。
///
/// 用户点「连接」不受这个限制：他就在屏幕前等着，也随时能点「断开」。
const AUTO_RETRY_ATTEMPTS: u32 = 5;
/// 待命状态的检查节奏：不发 TCP，只看"对端现在算不算在线"（5s 探活 / mDNS 的结果）。
const IDLE_POLL: Duration = Duration::from_secs(15);

/// 这一轮该不该发起拨号（纯策略，边界见单测）。
///
/// - `auto = false`（用户点「连接」）：照拨 —— 那是一次明确的指令。
/// - `auto = true`（重启恢复 / 身份迁移后接回）：先按指数退避试 `AUTO_RETRY_ATTEMPTS` 次
///   （对端刚开机、地址刚变，这几下就能成）；之后只在**对端被观测到在线**时才动手。
///   对端压根没开机的那些分钟，反复发 TCP 只会每次都等满超时（Windows 上就是 10060）
///   再报一行 warn —— 等它出现在网上再拨才是准的；而且对方只要也记着这次长连接，
///   它启动时自己就会拨过来（两侧都记得 = 谁后上线谁拨）。
fn should_dial(auto: bool, fails: u32, peer_online: bool) -> bool {
    !auto || fails < AUTO_RETRY_ATTEMPTS || peer_online
}

/// 退避加 ±20% 抖动：多台设备（或多份实例）同时开机时错开，别在同一秒一起砸过来。
fn jittered(secs: u64) -> Duration {
    let spread = (secs / 5).max(1);
    let delta = rand::thread_rng().gen_range(0..=spread * 2) as i64 - spread as i64;
    Duration::from_secs((secs as i64 + delta).max(1) as u64)
}

/// 握手阶段的**明确拒绝**：这不是网络抖动，再拨多少次结果都一样，该停手并说明原因。
///
/// - `403` = 对端点过「断开」（它的 `events_ws_handler` 在暂停期间拒收自动重拨）；
/// - `401` = 它不认我们这枚 token（对方清过数据 / 已经解除配对）→ 得重新配对。
///
/// 其余（连接被拒、超时、DNS、TCP reset）都是"现在不行"，按退避继续试。
fn stop_reason_for(err: &tokio_tungstenite::tungstenite::Error) -> Option<StopReason> {
    let tokio_tungstenite::tungstenite::Error::Http(resp) = err else {
        return None;
    };
    match resp.status().as_u16() {
        403 => Some(StopReason::PeerDisconnected),
        401 => Some(StopReason::TokenRejected),
        _ => None,
    }
}

/// 拨号循环：由用户点「连接」或启动恢复进入，直到会话建立并保住。
fn spawn_outbound_link(state: Arc<AppState>, peer_id: String, manual: bool, auto: bool) {
    // One link per peer: a live channel means we are already linked. A loop still
    // running is aborted by `dial_now` first — two concurrent loops would
    // unregister each other's channel and flap the peer's online state.
    if state.ws.has_connection(&peer_id) {
        return;
    }

    let state_reg = state.clone();
    let pid_for_register = peer_id.clone();
    let body = async move {
        let mut backoff = 5u64;
        // 连续失败次数（连上即清零）：自动拨号据此从"退避重试"转入"待命"。
        let mut fails = 0u32;
        let mut idle_logged = false;
        // manual=1 只搭第一次尝试：它是「用户此刻让我连」的一次性声明，不是这个
        // 循环的长期属性 —— 否则会话建起来又掉之后，本机会一直替对端解除它的「断开」。
        let mut manual_pending = manual;

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
                    state.ws.clear_link_state(&peer_id);
                    return;
                }
            };
            if peer.ip.is_empty() || peer.port == 0 {
                // Not discoverable yet — wait for the address without burning
                // the backoff (a doomed dial would just delay the real one).
                state.ws.set_link_state(&peer_id, LinkState::Waiting);
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
            // 自动拨号的待命闸门：够 AUTO_RETRY_ATTEMPTS 次失败、且对端此刻不算在线
            // （探活失败 / 没被 mDNS 发现），就别再发 TCP 了 —— 每次都要等满超时
            // （Windows 上是 10060）再报一行 warn，对端一开机它自己会拨过来，或者
            // 探活/mDNS 一看到它就放行下一轮。
            if !should_dial(auto, fails, peer.online) {
                if !idle_logged {
                    info!(
                        %peer_id,
                        attempts = fails,
                        "自动拨号转入待命：对端未上线，等它出现在网上（或用户点「连接」）"
                    );
                    idle_logged = true;
                }
                state.ws.set_link_state(&peer_id, LinkState::Waiting);
                tokio::time::sleep(IDLE_POLL).await;
                continue;
            }

            // 凭据库读失败 **不等于** "没配对"：那是机器级存储的瞬时故障（凭据管理器忙、
            // 同一台机器上多个实例并发读写）。把它当成"没有 token"会让这个循环**永久退出** ——
            // 用户看到的就是"重启后再也接不回来，也不报错"。所以按"这次尝试失败"处理：
            // 退避后重试、日志量有上限，且不清空这个循环已经建立的其它前提。
            let token = match credential::get_peer_token(&peer_id) {
                Ok(Some(t)) if !t.is_empty() => t,
                Ok(_) => {
                    // 本机真没有它的 token = 关系不在本机：只能重新配对，别再拨了。
                    warn!(%peer_id, "no token for this peer, outbound link stopped");
                    state.ws.set_link_state(
                        &peer_id,
                        LinkState::Stopped { reason: StopReason::TokenRejected },
                    );
                    return;
                }
                Err(e) => {
                    fails += 1;
                    if fails <= AUTO_RETRY_ATTEMPTS {
                        warn!(%peer_id, "credential store read failed: {e}, retry {backoff}s");
                    } else {
                        debug!(%peer_id, "credential store read failed: {e}");
                    }
                    state.ws.set_link_state(&peer_id, LinkState::Retrying);
                    tokio::time::sleep(jittered(backoff)).await;
                    backoff = (backoff * 2).min(60);
                    continue;
                }
            };

            // manual=1 只在第一次尝试时带上：这是一次"用户让连"的请求，不是持续声明。
            let url = format!(
                "ws://{}:{}/events?token={}{}",
                peer.ip,
                peer.port,
                token,
                if manual_pending { "&manual=1" } else { "" },
            );
            manual_pending = false;
            state.ws.set_link_state(&peer_id, LinkState::Retrying);
            match connect_async(&url).await {
                Ok((ws_stream, _)) => {
                    info!(%peer_id, "outbound WS linked");
                    state.set_peer_online(&peer_id, true);
                    // 长连接意图落盘（F-16）：链路一建立就记下，重启后自动接回来。
                    state.set_auto_connect(&peer_id, true);
                    state.ws.set_link_state(&peer_id, LinkState::Connected);
                    backoff = 5;
                    // 连上了就把失败计数清零：下次掉线重新享受完整的退避+重试预算。
                    fails = 0;
                    idle_logged = false;

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
                    // 明确拒绝（对端点过「断开」/ 不认我们的 token）：停手并留下理由，
                    // 继续拨只是每分钟往对端门口敲一次，永远不会变成连接。
                    if let Some(reason) = stop_reason_for(&e) {
                        info!(%peer_id, error = %e, "link refused, giving up until the situation changes");
                        state.ws.set_link_state(&peer_id, LinkState::Stopped { reason });
                        return;
                    }
                    fails += 1;
                    if auto && fails > AUTO_RETRY_ATTEMPTS {
                        // 待命期间的偶发尝试（对端被探活看到在线，但链路仍连不上）：
                        // 一分钟一次，别再往日志里灌 warn。
                        debug!(%peer_id, "connect failed: {e}");
                    } else {
                        warn!(%peer_id, "connect failed: {e}, retry {backoff}s");
                    }
                    state.ws.set_link_state(&peer_id, LinkState::Retrying);
                    tokio::time::sleep(jittered(backoff)).await;
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

#[cfg(test)]
mod tests {
    use super::*;

    /// 自动拨号的"待命"边界：够次数 + 对端不在线才停手；对端一上线立刻放行。
    /// 用户点「连接」永远放行（他就在屏幕前，随时能点「断开」）。
    #[test]
    fn auto_dial_backs_off_once_the_budget_is_spent_and_the_peer_is_absent() {
        // 用户点的：不受预算限制，也不看在线状态
        assert!(should_dial(false, 0, false));
        assert!(should_dial(false, AUTO_RETRY_ATTEMPTS, false));
        assert!(should_dial(false, 99, false));

        // 自动（重启恢复）：预算内照试（对端可能刚开机、地址可能刚变）
        assert!(should_dial(true, 0, false));
        assert!(should_dial(true, AUTO_RETRY_ATTEMPTS - 1, false));

        // 预算用尽且对端不在线 → 待命，不再发 TCP（每次都要等满超时才报 10060）
        assert!(!should_dial(true, AUTO_RETRY_ATTEMPTS, false));
        assert!(!should_dial(true, AUTO_RETRY_ATTEMPTS + 3, false));

        // 对端上线（探活成功 / mDNS 重新发现）→ 立刻放行下一轮
        assert!(should_dial(true, AUTO_RETRY_ATTEMPTS, true));
        assert!(should_dial(true, 99, true));
    }

    fn http_err(status: u16) -> tokio_tungstenite::tungstenite::Error {
        tokio_tungstenite::tungstenite::Error::Http(Box::new(
            tokio_tungstenite::tungstenite::http::Response::builder()
                .status(status)
                .body(None)
                .expect("build response"),
        ))
    }

    /// 只有**明确拒绝**才停手：403 = 对端点过「断开」，401 = 它不认我们的 token。
    /// 其余（服务端打嗝、连接被拒、超时、链路刚断）都是"现在不行"，必须继续退避重试 ——
    /// 把它们也当成停手，就等于把网络抖动升级成"再也连不回来"。
    #[test]
    fn only_explicit_refusals_stop_the_loop() {
        assert_eq!(stop_reason_for(&http_err(403)), Some(StopReason::PeerDisconnected));
        assert_eq!(stop_reason_for(&http_err(401)), Some(StopReason::TokenRejected));
        assert_eq!(stop_reason_for(&http_err(503)), None, "服务端打嗝不算停手");
        assert_eq!(
            stop_reason_for(&tokio_tungstenite::tungstenite::Error::ConnectionClosed),
            None,
            "链路断开要重连，不是停手"
        );
    }

    /// 抖动的意义：多台设备同时开机时别在同一秒一起砸过来；范围必须收在 ±20% 且 ≥1s。
    #[test]
    fn backoff_jitter_stays_within_twenty_percent() {
        for nominal in [5u64, 10, 20, 40, 60] {
            let spread = (nominal / 5).max(1);
            for _ in 0..200 {
                let got = jittered(nominal).as_secs();
                assert!(
                    (nominal - spread..=nominal + spread).contains(&got),
                    "{nominal}s 的抖动越界：{got}s"
                );
            }
        }
        let samples: std::collections::HashSet<u64> =
            (0..50).map(|_| jittered(60).as_secs()).collect();
        assert!(samples.len() > 1, "抖动得真的抖，不能退化成定值");
    }

    /// 界面按这个形状渲染（`src/lib/types.ts`）：改字段名 = 改前后端契约。
    #[test]
    fn link_state_serializes_for_the_ui() {
        let value = |s: LinkState| serde_json::to_value(s).unwrap();
        assert_eq!(value(LinkState::Connected), serde_json::json!({ "kind": "connected" }));
        assert_eq!(value(LinkState::Retrying), serde_json::json!({ "kind": "retrying" }));
        assert_eq!(value(LinkState::Waiting), serde_json::json!({ "kind": "waiting" }));
        assert_eq!(
            value(LinkState::Stopped { reason: StopReason::PeerDisconnected }),
            serde_json::json!({ "kind": "stopped", "reason": "peerDisconnected" })
        );
        assert_eq!(
            value(LinkState::Stopped { reason: StopReason::TokenRejected }),
            serde_json::json!({ "kind": "stopped", "reason": "tokenRejected" })
        );
        assert_eq!(
            value(LinkState::Stopped { reason: StopReason::LocalDisconnected }),
            serde_json::json!({ "kind": "stopped", "reason": "localDisconnected" })
        );
    }
}