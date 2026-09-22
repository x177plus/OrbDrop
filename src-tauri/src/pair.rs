use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use rand::Rng;
use tokio::sync::oneshot;
use tracing::{info, warn};

use crate::config::PeerDevice;
use crate::credential;
use crate::state::AppState;
use crate::ws;
use crate::ws::events::ui;

/// Pending pairing codes (F-12): the 6-digit codes this machine has issued and
/// is currently displaying, each valid for 2 minutes and consumable once.
///
/// Codes are *not* bound to a peer id: possession of the code shown on our
/// screen is the proof, and a peer whose advertised id changed under us (app
/// restarted with a fresh identity, stale discovery entry, address book ghost)
/// would otherwise be locked out with "配对码错误或已过期".
///
/// 同一个 store 还管**配对邀请**（免配对码那条路，F-12）：进来的邀请等本机用户在
/// 通知里做决定，出去的邀请等对端做决定（用户点「取消」时要能把它撤回来）。
pub struct PairStore {
    pending: Mutex<Vec<PendingPair>>,
    /// 等本机用户处理的邀请，键 = invite id。
    invites: Mutex<Vec<PendingInvite>>,
    /// 本机发出、还没结束的邀请：invite id → 对端 id（撤回时要凭它找到对端）。
    outbound: Mutex<HashMap<String, String>>,
}

struct PendingPair {
    code: String,
    created_at: Instant,
}

pub const CODE_TTL: Duration = Duration::from_secs(120);

/// 一条配对邀请在接收方屏幕上停留多久：到点自动作废。
///
/// 发起方在等（邀请是同步 HTTP，等回执的），接收方这边没人管的话，那条通知会一直挂在
/// 面板上，而发起方早就超时走人了 —— 两边状态必须同时作废。与推送邀请同一个数。
pub const INVITE_TTL: Duration = Duration::from_secs(60);

/// 发起方等邀请回执的时限：比 `INVITE_TTL` 多一点余量，让"对方没点、自动作废"这条
/// 正常路径能走到回执，而不是本机先断在网上。
const HTTP_INVITE_TIMEOUT: Duration = Duration::from_secs(75);

/// 撤回邀请的请求不必久等：对面多半已经处理完了，没撤成就让它的 TTL 兜底。
const WITHDRAW_TIMEOUT: Duration = Duration::from_secs(3);

impl PairStore {
    pub fn new() -> Self {
        Self {
            pending: Mutex::new(Vec::new()),
            invites: Mutex::new(Vec::new()),
            outbound: Mutex::new(HashMap::new()),
        }
    }

    /// Issue a fresh 6-digit code (dropping everything already expired).
    pub fn create_code(&self) -> String {
        let code: u32 = rand::thread_rng().gen_range(100_000..1_000_000);
        let code = code.to_string();
        let mut pending = self.pending.lock();
        pending.retain(|p| !p.is_expired());
        pending.push(PendingPair { code: code.clone(), created_at: Instant::now() });
        info!(code, "pairing code issued");
        code
    }

    /// True if `code` is one of our live codes; consumes it (single use).
    pub fn verify_code(&self, code: &str) -> bool {
        let mut pending = self.pending.lock();
        pending.retain(|p| !p.is_expired());
        match pending.iter().position(|p| p.code == code) {
            Some(pos) => {
                pending.remove(pos);
                true
            }
            None => false,
        }
    }

    pub fn insert_invite(&self, invite: PendingInvite) {
        self.invites.lock().push(invite);
    }

    /// 摘走一条进来的邀请（用户决定 / 超时 / 对方撤回都从这里出去）。
    /// 摘走之后谁也不再碰它 —— 一条邀请只可能被处理一次。
    pub fn take_invite(&self, invite_id: &str) -> Option<PendingInvite> {
        let mut invites = self.invites.lock();
        let pos = invites.iter().position(|i| i.invite_id == invite_id)?;
        Some(invites.remove(pos))
    }

    /// 记下一条本机发出的邀请（用户点「取消」时要凭它对上号）。
    pub fn insert_outbound(&self, invite_id: String, peer_id: String) {
        self.outbound.lock().insert(invite_id, peer_id);
    }

    /// 摘掉一条本机发出的邀请，返回对端 id（撤回用；任务收尾只是清理）。
    pub fn take_outbound(&self, invite_id: &str) -> Option<String> {
        self.outbound.lock().remove(invite_id)
    }

    /// 这条进来的邀请此刻是不是还挂在本机界面上（等用户决定）。
    pub fn has_invite(&self, invite_id: &str) -> bool {
        self.invites.lock().iter().any(|i| i.invite_id == invite_id)
    }

    /// 本机发出的这条邀请是不是还在等结果（结束了 = 对端已处理/超时/已撤回）。
    pub fn has_outbound(&self, invite_id: &str) -> bool {
        self.outbound.lock().contains_key(invite_id)
    }
}

impl PendingPair {
    fn is_expired(&self) -> bool {
        Instant::now().duration_since(self.created_at) >= CODE_TTL
    }
}

impl Default for PairStore {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// 配对邀请（F-12 免配对码那条路）
// ---------------------------------------------------------------------------

/// 配对要落库的那一端：id / 显示名 / 地址。
///
/// 两条路各有各的来源 —— 配对码那条路把对端自报的端口与"已知地址"凑起来；邀请那条路
/// 用 TCP 来源地址（不可伪造）+ 对端自报的服务端口。
#[derive(Debug, Clone)]
pub struct PairedPeer {
    pub id: String,
    pub name: String,
    pub ip: String,
    pub port: u16,
}

/// 配对成功时给对方的那份回应：本机 id/名字 + **本机为他铸的 token**
/// （他拿它来认证他自己对本机的调用）。
#[derive(Debug, Clone)]
pub struct PairGrant {
    pub token: String,
    pub peer_id: String,
    pub peer_name: String,
}

/// 一条挂在接收方界面上的配对邀请：对端点「发起邀请」，等本机用户在通知里接受/拒绝。
pub struct PendingInvite {
    pub invite_id: String,
    pub peer: PairedPeer,
    /// 用户的决定送回那个正挂着的 HTTP 请求（发起方在同步等）。
    responder: Option<oneshot::Sender<Result<PairGrant, String>>>,
}

impl PendingInvite {
    pub fn new(invite_id: String, peer: PairedPeer, responder: oneshot::Sender<Result<PairGrant, String>>) -> Self {
        Self { invite_id, peer, responder: Some(responder) }
    }

    /// 把决定送回发起方。返回 false = 发起方已经不在了（取消 / 断网 / 进程没了）。
    fn decide(&mut self, outcome: Result<PairGrant, String>) -> bool {
        match self.responder.take() {
            Some(tx) => tx.send(outcome).is_ok(),
            None => false,
        }
    }
}

/// 本机这一半的回执：自己的 id/名字 + 新铸的 token。
pub fn mint_grant(state: &AppState) -> PairGrant {
    let config = state.config.lock();
    PairGrant {
        token: generate_token(),
        peer_id: config.device_id.clone(),
        peer_name: config.device_name.clone(),
    }
}

/// 落定一条配对关系（配对码与邀请两条路共用）：写凭据库、写地址簿、预写 BC profile、
/// 广播事件。**token 先由 `mint_grant` 铸好** —— 邀请那条路要先把回执送出去，
/// 送不到就什么都别写（见 `accept_invite`）。
pub fn commit_pairing(
    state: &Arc<AppState>,
    peer: PairedPeer,
    token: &str,
    invite_id: Option<&str>,
) -> Result<(), String> {
    credential::set_peer(&peer.id, token).map_err(|e| format!("凭据存储失败: {e}"))?;
    state.mark_paired(&PeerDevice {
        id: peer.id.clone(),
        name: peer.name.clone(),
        ip: peer.ip.clone(),
        port: peer.port,
        online: true,
        paired: true,
        // 刚配对：还没长连接，没有要恢复的东西。
        auto_connect: false, paused: false,
    });
    // 刚拿到 token：把 BC 的 WebDAV profile 预写好（BC 只在启动时登记 profile 文件，
    // 所以要抢在用户下次启动 BC 之前落盘）。
    let written = crate::bc::provision_profiles(state);
    info!(peer_id = %peer.id, written, "配对完成，已预写 BC profile");
    // `inviteId` 只有邀请那条路带：界面在等这条邀请的回执，凭它把弹窗收掉
    // （对端 id 与本机记的那台不一致时，按 id 对不上号）。
    ws::emit_ui(state, ui::PAIR_COMPLETED, &serde_json::json!({
        "peerId": peer.id,
        "peerName": peer.name,
        "inviteId": invite_id,
    }));
    ws::emit_ui(state, ui::DEVICES_CHANGED, &serde_json::json!({}));
    Ok(())
}

/// 接收方：用户在通知里点「接收」→ 落定这条关系，并放行那个挂着的 HTTP 请求。
///
/// 顺序是刻意的：**先把手伸过去**（oneshot 送得到 = 发起方还在等），再写凭据与地址簿。
/// 反过来的话，发起方中途取消/断网就会在本机留下一条单边的"已配对"。
pub fn accept_invite(state: &Arc<AppState>, invite_id: &str) -> Result<String, String> {
    let mut invite = state
        .pair
        .take_invite(invite_id)
        .ok_or_else(|| "配对邀请不存在或已作废".to_string())?;
    let grant = mint_grant(state);
    let peer = invite.peer.clone();
    if !invite.decide(Ok(grant.clone())) {
        return Err(format!("「{}」已离开，配对未完成", peer.name));
    }
    commit_pairing(state, peer.clone(), &grant.token, None)?;
    Ok(peer.name)
}

/// 接收方：点「拒绝」——给发起方一个明确的"不"，本机什么都不写。
pub fn reject_invite(state: &AppState, invite_id: &str) {
    if let Some(mut invite) = state.pair.take_invite(invite_id) {
        invite.decide(Err("对方拒绝".to_string()));
    }
}

/// 一条进来的邀请作废：回执一个原因给发起方，并把本机界面上那条通知撤掉。
/// 返回是否真有一条挂着（已经处理过的邀请不会再动它一次）。
fn close_invite(state: &AppState, invite_id: &str, reason: &str) -> bool {
    let Some(mut invite) = state.pair.take_invite(invite_id) else {
        return false;
    };
    invite.decide(Err(reason.to_string()));
    ws::emit_ui(state, ui::PAIR_INVITE_CLOSED, &serde_json::json!({
        "inviteId": invite_id,
        "reason": reason,
    }));
    true
}

/// 接收方：邀请到点没人处理就自动作废（发起方在等回执，两边必须同时收手）。
pub fn schedule_invite_timeout(state: Arc<AppState>, invite_id: String) {
    crate::task::spawn("pair invite timeout", async move {
        tokio::time::sleep(INVITE_TTL).await;
        if close_invite(&state, &invite_id, "超时未处理") {
            info!(%invite_id, "pair invite expired");
        }
    });
}

/// 接收方：发起方把邀请撤回了（`POST /pair/invite/withdraw`）。
pub fn withdraw_incoming_invite(state: &AppState, invite_id: &str) {
    close_invite(state, invite_id, "对方已取消邀请");
}

// ---------------------------------------------------------------------------
// 发起方
// ---------------------------------------------------------------------------

#[derive(serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct PairResponseWire {
    token: String,
    peer_id: String,
    peer_name: String,
}

/// 发起方：给一台在线设备发配对邀请（F-12 免配对码）。
///
/// 从点「发起邀请」到对端在它自己的通知里点「接收」，全程不需要任何输入：邀请是一次
/// HTTP POST（`/pair/invite`），对端点「接收」时铸一枚 token 作为这次请求的响应回来；
/// 对端拒绝 / 60 秒没人处理 / 用户在本机点「取消」（撤回）→ 403。
///
/// 返回邀请 id；成败走 `pair://completed` / `pair://invite_failed` 事件回来。
pub async fn invite_peer(state: Arc<AppState>, peer_id: &str) -> Result<String, String> {
    let peer = state
        .get_peer(peer_id)
        .ok_or_else(|| format!("设备不存在: {peer_id}"))?;
    if !peer.online || peer.ip.is_empty() || peer.port == 0 {
        return Err(format!("「{}」不在网上，邀请未发出", peer.name));
    }
    let invite_id = uuid::Uuid::new_v4().to_string();
    // 服务端口：对端要凭它回连/调用本机（这个 HTTP 请求的源端口是一次性的）。
    let own_port = match state.server_port.load(Ordering::SeqCst) {
        0 => state.config.lock().port,
        p => p,
    };
    let (own_id, own_name) = {
        let config = state.config.lock();
        (config.device_id.clone(), config.device_name.clone())
    };
    state.pair.insert_outbound(invite_id.clone(), peer_id.to_string());

    let state2 = state.clone();
    let id2 = invite_id.clone();
    crate::task::spawn("pair invite", async move {
        let outcome = deliver_invite_http(&peer, own_port, &id2, &own_id, &own_name).await;
        // 这条邀请已经结束（用户取消时也会摘走它）：清理外发表。
        state2.pair.take_outbound(&id2);
        match outcome {
            Ok(grant) => {
                // 对端已经铸好并保存了本机这枚 token（它靠"回执送得到"确认本机还在等），
                // 所以回执一到就必须落库 —— 不因为"用户中途点过取消"而丢弃，否则两边
                // 状态会不一致（对端以为配成了，本机什么都没记）。
                let paired = PairedPeer {
                    id: grant.peer_id.clone(),
                    name: grant.peer_name.clone(),
                    ip: peer.ip.clone(),
                    port: peer.port,
                };
                if let Err(e) = commit_pairing(&state2, paired, &grant.token, Some(&id2)) {
                    warn!(invite = %id2, "pairing commit failed: {e}");
                    ws::emit_ui(&state2, ui::PAIR_INVITE_FAILED, &serde_json::json!({
                        "inviteId": id2,
                        "peerId": grant.peer_id,
                        "reason": e,
                    }));
                }
            }
            Err(reason) => {
                info!(invite = %id2, peer = %peer.id, "pair invite failed: {reason}");
                ws::emit_ui(&state2, ui::PAIR_INVITE_FAILED, &serde_json::json!({
                    "inviteId": id2,
                    "peerId": peer.id,
                    "reason": reason,
                }));
            }
        }
    });

    Ok(invite_id)
}

/// 发起方：撤回一条还没被处理的邀请（用户点了「取消」）。
///
/// 不撤回的话，本机早就走人了，对面那条通知还挂在面板上 —— 用户点「接收」才发现
/// "配对邀请不存在或已作废"。撤不回来（对端已经处理/不在网上）不是错误：它的 TTL 兜底。
pub async fn withdraw_invite_remote(state: Arc<AppState>, invite_id: &str) -> Result<(), String> {
    let Some(peer_id) = state.pair.take_outbound(invite_id) else {
        return Ok(()); // 这条邀请已经结束了（对端已处理 / 已超时）
    };
    let Some(peer) = state.get_peer(&peer_id) else {
        return Ok(());
    };
    let url = format!("http://{}:{}/pair/invite/withdraw", peer.ip, peer.port);
    let resp = reqwest::Client::new()
        .post(&url)
        .timeout(WITHDRAW_TIMEOUT)
        .json(&serde_json::json!({ "inviteId": invite_id }))
        .send()
        .await
        .map_err(|e| format!("联系不上「{}」：{e}", peer.name))?;
    if !resp.status().is_success() {
        return Err(format!("撤回邀请失败（HTTP {}）", resp.status().as_u16()));
    }
    info!(%invite_id, peer = %peer.id, "pair invite withdrawn");
    Ok(())
}

/// 把邀请 POST 到对端的 `/pair/invite`，返回对端铸好的回执。
///
/// 对端**已配对**时必须带上本机手里那枚 token（对端就是靠它挡住"自称你的老搭档"的
/// 冒充者，与 `/push/invite` 同一道关）；重新配对走的就是这条路。
async fn deliver_invite_http(
    peer: &PeerDevice,
    own_port: u16,
    invite_id: &str,
    own_id: &str,
    own_name: &str,
) -> Result<PairGrant, String> {
    let url = format!("http://{}:{}/pair/invite", peer.ip, peer.port);
    let mut req = reqwest::Client::new()
        .post(&url)
        .timeout(HTTP_INVITE_TIMEOUT)
        .json(&serde_json::json!({
            "inviteId": invite_id,
            "peerId": own_id,
            "peerName": own_name,
            "port": own_port,
        }));
    if let Ok(Some(token)) = credential::get_peer_token(&peer.id) {
        req = req.bearer_auth(token);
    }

    let resp = req
        .send()
        .await
        .map_err(|e| format!("联系不上「{}」：{e}", peer.name))?;
    if !resp.status().is_success() {
        // 对端拒绝 / 超时 / 撤回：正文就是原因（axum 文本响应），它比我们编的准。
        let reason = resp.text().await.unwrap_or_default();
        return Err(if reason.trim().is_empty() {
            format!("「{}」没有接受邀请", peer.name)
        } else {
            reason
        });
    }
    let grant: PairResponseWire = resp
        .json()
        .await
        .map_err(|e| format!("「{}」的回应无法解析：{e}", peer.name))?;
    Ok(PairGrant { token: grant.token, peer_id: grant.peer_id, peer_name: grant.peer_name })
}

/// Generate a random 32-byte hex token for peer authentication.
pub fn generate_token() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}