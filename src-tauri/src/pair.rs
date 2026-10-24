use std::collections::HashMap;
use std::net::IpAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use rand::Rng;
use tokio::sync::oneshot;
use tracing::{info, warn};

use crate::config::PeerDevice;
use crate::credential;
use crate::pake::{
    PendingPake, MAX_PENDING_PAKE, MAX_PENDING_PAKE_PER_SOURCE, PAKE_START_LIMIT,
    PAKE_START_WINDOW, PAKE_TTL,
};
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
    /// 配对码**猜错**的记账：来源地址 → 这个窗口里已经错了几次（P1-5）。
    code_failures: Mutex<HashMap<IpAddr, Attempts>>,
    /// 当前这个码一共错了几次（不分来源；`create_code` 清零）。
    code_failures_total: AtomicU32,
    /// "自称已配对却拿不出凭据"的邀请记账：对端 id → 上次放行到通知的时刻（P1-6）。
    unproven_invites: Mutex<HashMap<String, Instant>>,
    /// 待完成的 PAKE 配对会话，键 = 发起方生成的 session id。
    pake: Mutex<HashMap<String, PendingPake>>,
    /// PAKE start 的发起频率（P-256 运算不便宜，未认证来源不能无限打）。
    pake_starts: Mutex<HashMap<IpAddr, Attempts>>,
}

/// 一个来源地址在一个窗口里猜错了几次（窗口过期即清零）。
struct Attempts {
    count: u32,
    window: Instant,
}

/// 配对码每分钟允许猜错几次。
///
/// 6 位码 = 9e5 种、`CODE_TTL` 120s：不限次数时，局域网里每秒几千发就能在有效期内扫完
/// —— 用户正把码显示在屏幕上，陌生设备把他自己配上。5 次/分钟把爆破从分钟级拉到按天算，
/// 而正常人输错一两次完全不受影响（同一个地址也可能是家里几台设备在试）。
pub const CODE_FAILURE_LIMIT: u32 = 5;
/// **每个码**允许的失败总数（不分来源 IP）。
///
/// PAKE 把离线枚举堵死之后，在线猜测是唯一入口；只按来源限流时，换 IP 的僵尸网络
/// 仍能在 120s 内撞很多次。20 次 = 1e6 里猜 20 次（2e-5），到顶就让这个码作废。
pub const CODE_FAILURE_TOTAL_LIMIT: u32 = 20;
/// 失败计数的时间窗。
pub const CODE_FAILURE_WINDOW: Duration = Duration::from_secs(60);
/// 同一个设备 id 的"无凭据邀请"最短间隔（P1-6）：同网段的人只要能制造一次 401（拨号被拒）
/// 就能让本机放行邀请，那就别让他连点 —— 用户最多每两分钟看到一条，而且卡片上写明
/// 身份未经校验、仍要他自己点「接收」。
pub const UNPROVEN_INVITE_COOLDOWN: Duration = Duration::from_secs(120);

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

/// 同时在界面上挂着的入站配对邀请上限。未认证来源可以无限 POST `/pair/invite`，
/// 每条挂 60 秒并 spawn 一个超时任务；没有上限时内存/通知/任务数都能被拉到任意大。
pub const MAX_PENDING_INVITES: usize = 32;

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
            code_failures: Mutex::new(HashMap::new()),
            code_failures_total: AtomicU32::new(0),
            unproven_invites: Mutex::new(HashMap::new()),
            pake: Mutex::new(HashMap::new()),
            pake_starts: Mutex::new(HashMap::new()),
        }
    }

    /// 这个来源地址现在还能不能试配对码（`false` = 已经错太多，先歇一会儿）。
    pub fn code_attempt_allowed(&self, from: IpAddr) -> bool {
        if self.code_failures_total.load(Ordering::SeqCst) >= CODE_FAILURE_TOTAL_LIMIT {
            return false;
        }
        let mut map = self.code_failures.lock();
        map.retain(|_, a| a.window.elapsed() < CODE_FAILURE_WINDOW);
        map.get(&from)
            .map(|a| a.count < CODE_FAILURE_LIMIT)
            .unwrap_or(true)
    }

    /// 记一次配对码猜错。
    pub fn note_code_failure(&self, from: IpAddr) {
        let mut map = self.code_failures.lock();
        map.retain(|_, a| a.window.elapsed() < CODE_FAILURE_WINDOW);
        let entry = map.entry(from).or_insert(Attempts {
            count: 0,
            window: Instant::now(),
        });
        entry.count += 1;
        drop(map);

        let total = self.code_failures_total.fetch_add(1, Ordering::SeqCst) + 1;
        if total >= CODE_FAILURE_TOTAL_LIMIT {
            // 这个码已经被撞够次数了：整条作废，用户必须生成新的（界面上的旧码到点
            // 也会自己过期）。
            self.pending.lock().clear();
            warn!(total, "配对码失败总数超限，已作废");
        }
    }

    /// 猜对一次就把这个地址的账清掉（用户自己输错的那些不该一直攒着）。
    pub fn clear_code_failures(&self, from: IpAddr) {
        self.code_failures.lock().remove(&from);
        self.code_failures_total.store(0, Ordering::SeqCst);
    }

    /// 一条"自称已配对却没带凭据"的邀请现在能不能放行到通知里（P1-6 的频率约束）。
    pub fn unproven_invite_allowed(&self, peer_id: &str) -> bool {
        let mut map = self.unproven_invites.lock();
        map.retain(|_, at| at.elapsed() < UNPROVEN_INVITE_COOLDOWN);
        !map.contains_key(peer_id)
    }

    /// 记下一条已经放行到通知的无凭据邀请。
    pub fn note_unproven_invite(&self, peer_id: &str) {
        self.unproven_invites
            .lock()
            .insert(peer_id.to_string(), Instant::now());
    }

    /// Issue a fresh 6-digit code（**新码让旧码立刻作废**）。
    ///
    /// PAKE 的一轮握手要绑定"这一个码"：同一台设备同时挂两个有效码时，服务端无法知道
    /// 对面输的是哪一个，只能逐个试；而用户点「生成配对码」的本意本来就是"换一个"。
    pub fn create_code(&self) -> String {
        let code: u32 = rand::thread_rng().gen_range(100_000..1_000_000);
        let code = code.to_string();
        let mut pending = self.pending.lock();
        pending.clear();
        self.code_failures_total.store(0, Ordering::SeqCst);
        pending.push(PendingPair {
            code: code.clone(),
            created_at: Instant::now(),
        });
        // 码是本次配对的唯一凭据：只留在内存与屏幕上，别按 info 落进日志文件。
        tracing::debug!(code, "pairing code issued");
        code
    }

    /// 当前有效的配对码（没有/过期 = `None`）。
    ///
    /// PAKE 的应答方要用它派生口令标量；单码语义见 [`Self::create_code`]。
    pub fn current_code(&self) -> Option<String> {
        let mut pending = self.pending.lock();
        pending.retain(|p| !p.is_expired());
        pending.last().map(|p| p.code.clone())
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

    /// 这个来源地址现在还能不能再发起一次 PAKE start（独立于"猜错码"的闸门）。
    pub fn pake_start_allowed(&self, from: IpAddr) -> bool {
        let mut map = self.pake_starts.lock();
        map.retain(|_, a| a.window.elapsed() < PAKE_START_WINDOW);
        map.get(&from)
            .map(|a| a.count < PAKE_START_LIMIT)
            .unwrap_or(true)
    }

    /// 记一次 PAKE start。
    pub fn note_pake_start(&self, from: IpAddr) {
        let mut map = self.pake_starts.lock();
        map.retain(|_, a| a.window.elapsed() < PAKE_START_WINDOW);
        let entry = map.entry(from).or_insert(Attempts {
            count: 0,
            window: Instant::now(),
        });
        entry.count += 1;
    }

    /// 挂一条待完成的 PAKE 会话。`false` = 全局表满 / session 重复（调用方回 429）。
    ///
    /// 单来源满额时**换掉它自己最旧的一条**：码输错时发起方不会走到 `finish`，那条
    /// 会话没人消费；若直接回 429，用户连错几次就会被自己前几次的残影挡住。全局上限
    /// 仍然管住内存，每条也都有 TTL。
    pub fn insert_pake(&self, pending: PendingPake) -> bool {
        let mut map = self.pake.lock();
        map.retain(|_, p| p.created_at.elapsed() < PAKE_TTL);
        if map.len() >= MAX_PENDING_PAKE || map.contains_key(&pending.session) {
            return false;
        }
        let same_source: Vec<(String, Instant)> = map
            .iter()
            .filter(|(_, p)| p.from == pending.from)
            .map(|(session, p)| (session.clone(), p.created_at))
            .collect();
        if same_source.len() >= MAX_PENDING_PAKE_PER_SOURCE {
            if let Some((oldest, _)) = same_source.iter().min_by_key(|(_, at)| *at) {
                map.remove(oldest);
            }
        }
        map.insert(pending.session.clone(), pending);
        true
    }

    /// 取走一条 PAKE 会话（一次性；过期的顺手清掉）。
    pub fn take_pake(&self, session: &str) -> Option<PendingPake> {
        let mut map = self.pake.lock();
        map.retain(|_, p| p.created_at.elapsed() < PAKE_TTL);
        map.remove(session)
    }

    /// 记下一条入站邀请。`false` = 已满或同一个 `invite_id` 已经在表里（调用方回 429）。
    pub fn insert_invite(&self, invite: PendingInvite) -> bool {
        let mut invites = self.invites.lock();
        if invites.iter().any(|i| i.invite_id == invite.invite_id) {
            return false;
        }
        if invites.len() >= MAX_PENDING_INVITES {
            return false;
        }
        invites.push(invite);
        true
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
    /// 对端在配对请求里自报的证书指纹（可选；老对端没有）。本机这一跳如果没见过
    /// 对端证书（收方），就用它落库，避免留下"配对成功但指纹为空"的窗口。
    pub fingerprint: String,
}

/// 配对成功时给对方的那份回应：本机 id/名字 + **本机为他铸的 token**
/// （他拿它来认证他自己对本机的调用）。
#[derive(Debug, Clone)]
pub struct PairGrant {
    pub token: String,
    pub peer_id: String,
    pub peer_name: String,
    /// 本机证书指纹：对方拿到回执后可以直接钉住本机，不必等下一次 TOFU。
    pub fingerprint: String,
}

/// 一条挂在接收方界面上的配对邀请：对端点「发起邀请」，等本机用户在通知里接受/拒绝。
pub struct PendingInvite {
    pub invite_id: String,
    pub peer: PairedPeer,
    /// 用户的决定送回那个正挂着的 HTTP 请求（发起方在同步等）。
    responder: Option<oneshot::Sender<Result<PairGrant, String>>>,
}

impl PendingInvite {
    pub fn new(
        invite_id: String,
        peer: PairedPeer,
        responder: oneshot::Sender<Result<PairGrant, String>>,
    ) -> Self {
        Self {
            invite_id,
            peer,
            responder: Some(responder),
        }
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
    let (peer_id, peer_name) = {
        let config = state.config.lock();
        (config.device_id.clone(), config.device_name.clone())
    };
    PairGrant {
        token: generate_token(),
        peer_id,
        peer_name,
        fingerprint: crate::net::local_fingerprint().unwrap_or_default(),
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
    // 凭据一次写全：**token 与对端指纹是同一份关系**（`set_peer_credential` 存在的理由）。
    //
    // 这里也是**唯一会覆盖旧指纹**的地方：用户刚刚（重新）建立了这段关系，那个地址上钉着的
    // 旧指纹（对端重装过、地址换了主人、上次配对留下的）必须让位 —— 不让位就会变成
    // "刚重新配对好，却依旧连不上（指纹与钉住的不一致）"，而且这一跳如果是**收方**
    // （邀请那条路），本机根本见不到对端证书，没法当场把新指纹写下来。
    //
    // - 本机这一跳真见过它的证书（配对码那条路是本机拨出去的）→ 直接钉住这枚；
    // - 见不到（邀请那条路）→ 把指纹写**空**（= 这个地址暂时没有钉住的指纹），
    //   交给下一次成功连线按 TOFU 重新认。写空而不是留着旧的，是本条注释前半段那件事。
    let seen = crate::net::trust()
        .observed_at(&peer.ip)
        .filter(|fp| !fp.is_empty())
        // 收方在配对握手时看不到对端证书，只能采信请求里自报的指纹（新版本会带；
        // 老对端为空 → 仍按原来的 TOFU 迁移路径走）。
        .or_else(|| (!peer.fingerprint.is_empty()).then(|| peer.fingerprint.clone()));
    commit_pairing_with_fingerprint(state, peer, token, invite_id, seen.as_deref().unwrap_or(""))
}

/// 用调用方**已经认证过**的那一枚指纹落库（PAKE、以及带了客户端证书的新配对）。
///
/// 与 [`commit_pairing`] 的区别只有一个：不再从 `trust().observed_at(peer.ip)` 猜。
/// 收方那一跳的 `observed_at` 是"本机之前拨它时看到的服务器证书"，不是这条入站连接
/// 出示的客户端证书；全局表还可能被并发的探活/别的配对覆盖。调用方手里已经有权威
/// 指纹（TLS 客户端证书 / PAKE 的 AAD）时，必须用它。
pub fn commit_pairing_with_fingerprint(
    state: &Arc<AppState>,
    peer: PairedPeer,
    token: &str,
    invite_id: Option<&str>,
    fingerprint: &str,
) -> Result<(), String> {
    // 所有配对路径（配对码 / 邀请 / PAKE）都汇到这里：对端自报的 id 在写凭据和地址簿**之前**
    // 就得收口。合法值只有本机生成的 UUID 一种形状（见 `crate::ids`），坏 id 会被当成镜像
    // 目录名与 BC 的 URL 路径段用 —— 那是本机文件系统，不是可以"先收下再说"的东西。
    if !crate::ids::is_segment(&peer.id) {
        warn!(peer_id = %peer.id, "配对被拒：对方设备标识不合法");
        return Err("对方设备标识不合法（只允许字母、数字与 `-_.`），已拒绝配对".into());
    }
    credential::set_peer_credential(&peer.id, token, fingerprint)
        .map_err(|e| format!("凭据存储失败: {e}"))?;
    // 刚配对：还没长连接，没有要恢复的东西。`online` 要看**有没有可拨的地址**：有地址
    // （mDNS 看到的、手动填的、配对请求里学到的）就是"在线、送得到"；没有地址的是本机
    // 拨不到的那种对端（模拟器/NAT 后面 —— 它把自己看到的本机地址回连进来，那一对地址
    // 记不得，见 `net::observed_dial_target`），它的可达性只能等长连接来证明，先如实按
    // "离线"记，别在设备行上先挂一颗绿点。
    let dialable = !peer.ip.is_empty() && peer.port != 0;
    state.mark_paired(&PeerDevice {
        id: peer.id.clone(),
        name: peer.name.clone(),
        ip: peer.ip.clone(),
        port: peer.port,
        online: dialable,
        paired: true,
        auto_connect: false,
        paused: false,
        relay: None,
    });
    // 刚拿到 token：把 BC 的 WebDAV profile 预写好（BC 只在启动时登记 profile 文件，
    // 所以要抢在用户下次启动 BC 之前落盘）。
    let written = crate::bc::provision_profiles(state);
    info!(peer_id = %peer.id, written, "配对完成，已预写 BC profile");
    // `inviteId` 只有邀请那条路带：界面在等这条邀请的回执，凭它把弹窗收掉
    // （对端 id 与本机记的那台不一致时，按 id 对不上号）。
    ws::emit_ui(
        state,
        ui::PAIR_COMPLETED,
        &serde_json::json!({
            "peerId": peer.id,
            "peerName": peer.name,
            "inviteId": invite_id,
        }),
    );
    ws::emit_ui(state, ui::DEVICES_CHANGED, &serde_json::json!({}));
    // 设备刚进地址簿：Trust 那张"地址 → 指纹"的表要跟着重灌一次（上面刚写过凭据）。
    crate::state::refresh_trust(state);
    // 刚配对：如果两边有共同群，立刻补一次历史。以前只有"对端判在线"才会触发，
    // 而配对本身不保证 `set_peer_online` 被调用 —— 用户看到的是"配对成功但历史不补"。
    // 这条 ask 会带上本机签名，对端也能顺手把本机的群公钥认证掉。
    crate::group::sync_with_peer(state, &peer.id);
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
    // 新客户端在邀请请求里带了客户端证书：这个指纹才是这条连接的权威身份；
    // 老客户端（空）仍走 `commit_pairing` 的兼容路径。
    if peer.fingerprint.is_empty() {
        commit_pairing(state, peer.clone(), &grant.token, None)?;
    } else {
        commit_pairing_with_fingerprint(
            state,
            peer.clone(),
            &grant.token,
            None,
            &peer.fingerprint,
        )?;
    }
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
    ws::emit_ui(
        state,
        ui::PAIR_INVITE_CLOSED,
        &serde_json::json!({
            "inviteId": invite_id,
            "reason": reason,
        }),
    );
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
    #[serde(default)]
    cert_fingerprint: String,
}

/// 发起方拿到的邀请信息：邀请 id + 这次邀请的 8 位 SAS。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PairInviteStart {
    pub invite_id: String,
    /// 8 位 Base32（40 bit），发起方与接收方屏幕上应显示同一串。
    pub sas: String,
}

/// 发起方：给一台在线设备发配对邀请（F-12 免配对码）。
///
/// 从点「发起邀请」到对端在它自己的通知里点「接收」，全程不需要任何输入：邀请是一次
/// HTTP POST（`/pair/invite`），对端点「接收」时铸一枚 token 作为这次请求的响应回来；
/// 对端拒绝 / 60 秒没人处理 / 用户在本机点「取消」（撤回）→ 403。
///
/// 返回邀请 id + SAS；成败走 `pair://completed` / `pair://invite_failed` 事件回来。
pub async fn invite_peer(state: Arc<AppState>, peer_id: &str) -> Result<PairInviteStart, String> {
    let peer = state
        .get_peer(peer_id)
        .ok_or_else(|| format!("设备不存在: {peer_id}"))?;
    if !peer.online || !peer.dialable() {
        return Err(format!("「{}」不在网上，邀请未发出", peer.name));
    }
    // 旧指纹/旧凭据在配对成功之前**一律不动**：输错码、对方拒绝、60 秒超时、网络断，
    // 都不能把一段好好的关系弄成"已配对但没有指纹"的 Deny 状态。
    // 邀请这一跳仍只拿凭据库里那枚指纹钉 TLS；对端重装、证书换了就让它失败，用户改走配对码。
    let pin = credential::get_peer_fingerprint(peer_id)
        .ok()
        .flatten()
        .filter(|fp| !fp.is_empty());
    let token = credential::get_peer_token(peer_id)
        .map_err(|e| format!("凭据读取失败，邀请未发出: {e}"))?
        .filter(|t| !t.is_empty());
    // 有 token 就必须有可核对的指纹：不能把一枚旧 token 发到 TOFU 通道上。
    if token.is_some() && pin.is_none() {
        return Err(format!(
            "「{}」的配对凭据缺少证书指纹，请改用配对码重新配对",
            peer.name
        ));
    }
    let invite_id = uuid::Uuid::new_v4().to_string();
    let own_fingerprint = crate::net::local_fingerprint().unwrap_or_default();

    // 先握手一次（独立信任表，不受全局 pin/observed 干扰）：确认对端支持 SAS，
    // 并拿到**这条连接实际看到**的对端证书指纹。SAS 必须能在邀请挂起之前算出来，
    // 否则发起方在等待界面没有码可显示。
    let (host, port) = peer.dial();
    let base = crate::net::peer_base(host, port);
    let probe_trust = Arc::new(crate::net::Trust::new());
    let client = crate::net::client_builder_with_trust(probe_trust.clone())
        .build()
        .map_err(|e| format!("创建连接失败: {e}"))?;
    let probe = client
        .get(format!("{base}/pair/info"))
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .map_err(|e| format!("联系不上「{}」：{e}", peer.name))?;
    if !probe.status().is_success() {
        return Err(format!("「{}」此刻不在线", peer.name));
    }
    let info: serde_json::Value =
        crate::net::read_json_capped(probe, crate::net::PAIR_INFO_BODY_MAX)
            .await
            .map_err(|e| format!("对端回应无法解析：{e}"))?;
    if info.get("sas").and_then(|v| v.as_bool()) != Some(true) {
        // 不降级：对端不会显示核对码，用户没法比对。
        return Err("对端版本过旧，无法核对邀请身份；请改用配对码".into());
    }
    let peer_fingerprint = probe_trust
        .observed_at(host)
        .filter(|fp| !fp.is_empty())
        .ok_or_else(|| "没有取得对端证书指纹（这条连接不是 TLS？）".to_string())?;
    let sas = crate::sas::invite_sas(&invite_id, &own_fingerprint, &peer_fingerprint)
        .ok_or_else(|| "本机证书指纹为空，无法生成核对码".to_string())?;
    // 邀请这一跳只钉凭据库里那枚（未配对时为 None，走 TOFU + SAS）。
    // 不再回落到 probe 观察值：有 token 时那条回落等于把旧 token 发到 TOFU 通道上。
    let invite_pin = pin;
    // 服务端口：对端要凭它回连/调用本机（这个 HTTP 请求的源端口是一次性的）。
    let own_port = match state.server_port.load(Ordering::SeqCst) {
        0 => state.config.lock().port,
        p => p,
    };
    let (own_id, own_name) = {
        let config = state.config.lock();
        (config.device_id.clone(), config.device_name.clone())
    };
    state
        .pair
        .insert_outbound(invite_id.clone(), peer_id.to_string());

    let state2 = state.clone();
    let id2 = invite_id.clone();
    crate::task::spawn("pair invite", async move {
        let outcome = deliver_invite_http(
            &peer,
            own_port,
            &id2,
            &own_id,
            &own_name,
            &own_fingerprint,
            invite_pin,
            token,
        )
        .await;
        // 这条邀请已经结束（用户取消时也会摘走它）：清理外发表。
        state2.pair.take_outbound(&id2);
        match outcome {
            Ok(grant) => {
                // 对端已经铸好并保存了本机这枚 token（它靠"回执送得到"确认本机还在等），
                // 所以回执一到就必须落库 —— 不因为"用户中途点过取消"而丢弃，否则两边
                // 状态会不一致（对端以为配成了，本机什么都没记）。
                //
                // 回执在 `deliver_invite_http` 里已经逐项校验过：id 就是这次拨的那台，
                // fingerprint 是这条 TLS 连接实际出示的证书。这里用权威指纹落库，
                // 不再走 `commit_pairing` 的"全局 observed/自报"猜测。
                let PairGrant {
                    token,
                    peer_id,
                    peer_name,
                    fingerprint,
                } = grant;
                let paired = PairedPeer {
                    id: peer_id,
                    name: peer_name,
                    ip: peer.ip.clone(),
                    port: peer.port,
                    fingerprint: fingerprint.clone(),
                };
                if let Err(e) = commit_pairing_with_fingerprint(
                    &state2,
                    paired,
                    &token,
                    Some(&id2),
                    &fingerprint,
                ) {
                    warn!(invite = %id2, "pairing commit failed: {e}");
                    ws::emit_ui(
                        &state2,
                        ui::PAIR_INVITE_FAILED,
                        &serde_json::json!({
                            "inviteId": id2,
                            "peerId": peer.id,
                            "reason": e,
                        }),
                    );
                }
            }
            Err(reason) => {
                info!(invite = %id2, peer = %peer.id, "pair invite failed: {reason}");
                ws::emit_ui(
                    &state2,
                    ui::PAIR_INVITE_FAILED,
                    &serde_json::json!({
                        "inviteId": id2,
                        "peerId": peer.id,
                        "reason": reason,
                    }),
                );
            }
        }
    });

    Ok(PairInviteStart { invite_id, sas })
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
    let url = format!(
        "{}/pair/invite/withdraw",
        crate::net::peer_base(peer.dial().0, peer.dial().1)
    );
    let resp = crate::net::client()
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

/// 校验邀请回执并返回**可信的落库值**。
///
/// 回执是对端自报的：id 必须就是这次拨的那台，指纹必须就是这条 TLS 连接实际出示的
/// 证书（老版本不带时以观察值为准），token 必须是本协议的形状。少一样都不落库 ——
/// `commit_pairing` 会按 id 整条覆盖凭据，这里放过去等于让对端决定本机地址簿里
/// 另一台设备是谁。
fn accept_grant(
    peer: &PeerDevice,
    observed_fingerprint: Option<&str>,
    grant: PairResponseWire,
) -> Result<PairGrant, String> {
    if grant.peer_id != peer.id {
        warn!(
            claimed = %grant.peer_id,
            dialed = %peer.id,
            "配对邀请回执的身份与本次拨号的目标不一致，已拒绝"
        );
        return Err("对端身份与预期不一致".into());
    }
    let fingerprint = observed_fingerprint
        .filter(|fp| !fp.is_empty())
        .ok_or_else(|| "没有取得对端证书指纹（这条连接不是 TLS？）".to_string())?;
    if !grant.cert_fingerprint.is_empty()
        && !grant.cert_fingerprint.eq_ignore_ascii_case(fingerprint)
    {
        warn!(
            claimed = %crate::net::short_fingerprint(&grant.cert_fingerprint),
            seen = %crate::net::short_fingerprint(fingerprint),
            "配对邀请回执的证书指纹与本次连接不一致，已拒绝"
        );
        return Err("对端证书指纹与本次连接不一致".into());
    }
    if !is_token_shape(&grant.token) {
        warn!(peer = %peer.id, "配对邀请回执的 token 形状不合法，已拒绝");
        return Err("对端回执的 token 不合法".into());
    }
    Ok(PairGrant {
        token: grant.token,
        peer_id: peer.id.clone(),
        peer_name: grant.peer_name,
        fingerprint: fingerprint.to_string(),
    })
}

/// 本机铸的 token 是 32 字节 hex（64 个字符）；协议上不再接受别的形状。
fn is_token_shape(token: &str) -> bool {
    token.len() == 64 && token.bytes().all(|b| b.is_ascii_hexdigit())
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
    own_fingerprint: &str,
    pin: Option<String>,
    token: Option<String>,
) -> Result<PairGrant, String> {
    let (host, port) = peer.dial();
    let url = format!("{}/pair/invite", crate::net::peer_base(host, port));
    // 邀请这一跳用**按这台设备重建的 trust**，不复用全局表：
    // - 已配对的老搭档：把凭据库里的指纹钉进来（有指纹就必须匹配）；
    // - 未配对：这张表是空的，走 TOFU —— 身份保证是用户比对 SAS，不是地址钉扎。
    // 这样同一个地址（回环测试、同机多实例、NAT）上别的设备留下的 pin 不会把这条邀请
    // 钉死，也不会把 token 发给一个与凭据库指纹不符的证书。
    //
    // 有 token 却没有可核对的指纹：不能把 token 发到一条 TOFU 通道上。让用户走配对码
    // （PAKE）重新建立关系，比"先发 token 再指望对面拒绝"安全。
    if token.is_some() && pin.is_none() {
        return Err(format!(
            "「{}」的配对凭据缺少证书指纹，请改用配对码重新配对",
            peer.name
        ));
    }
    let trust = Arc::new(crate::net::Trust::new());
    if let Some(fp) = pin {
        trust.seed(std::iter::once((host.to_string(), fp)));
    }
    let client = crate::net::client_builder_with_trust(trust.clone())
        .build()
        .map_err(|e| format!("创建邀请连接失败：{e}"))?;
    let mut req = client
        .post(&url)
        .timeout(HTTP_INVITE_TIMEOUT)
        .json(&serde_json::json!({
            "inviteId": invite_id,
            "peerId": own_id,
            "peerName": own_name,
            "port": own_port,
            "certFingerprint": own_fingerprint,
        }));
    if let Some(token) = token {
        req = req.bearer_auth(token);
    }

    let resp = req
        .send()
        .await
        .map_err(|e| format!("联系不上「{}」：{e}", peer.name))?;
    if !resp.status().is_success() {
        // 对端拒绝 / 超时 / 撤回：正文就是原因（axum 文本响应），它比我们编的准。
        let reason = crate::net::read_text_capped(resp, crate::net::CONTROL_BODY_MAX)
            .await
            .unwrap_or_default();
        return Err(if reason.trim().is_empty() {
            format!("「{}」没有接受邀请", peer.name)
        } else {
            reason
        });
    }
    let grant: PairResponseWire =
        crate::net::read_json_capped(resp, crate::net::CONTROL_BODY_MAX)
            .await
            .map_err(|e| format!("「{}」的回应无法解析：{e}", peer.name))?;
    // 这条连接实际出示的证书：TOFU 那一档也只能从这里拿，不能采信回执里的自报值。
    accept_grant(peer, trust.observed_at(host).as_deref(), grant)
}

/// Generate a random 32-byte hex token for peer authentication.
pub fn generate_token() -> String {
    use rand::RngCore;
    let mut bytes = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut bytes);
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer(id: &str) -> PairedPeer {
        PairedPeer {
            id: id.into(),
            name: id.into(),
            ip: "10.0.0.1".into(),
            port: 3890,
            fingerprint: String::new(),
        }
    }

    fn device(id: &str, port: u16) -> PeerDevice {
        PeerDevice {
            id: id.into(),
            name: id.into(),
            ip: "127.0.0.1".into(),
            port,
            online: true,
            paired: true,
            auto_connect: false,
            paused: false,
            relay: None,
        }
    }

    fn wire(peer_id: &str, token: &str, fingerprint: &str) -> PairResponseWire {
        PairResponseWire {
            token: token.into(),
            peer_id: peer_id.into(),
            peer_name: "PC-B".into(),
            cert_fingerprint: fingerprint.into(),
        }
    }

    fn state_for(name: &str) -> (Arc<AppState>, std::path::PathBuf) {
        let dir =
            std::env::temp_dir().join(format!("orbdrop_pair_{name}_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let state = Arc::new(AppState::new(
            crate::config::test_config(),
            dir.join("config.toml"),
        ));
        (state, dir)
    }

    fn unused_port() -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.local_addr().unwrap().port()
    }

    /// 回执里的 id 必须就是这次拨的那台；否则 A 会把另一台设备的凭据整条覆盖。
    #[test]
    fn invite_grant_must_name_the_dialed_peer() {
        let err = accept_grant(
            &device("peer-b", 3890),
            Some(&"ab".repeat(32)),
            wire("peer-c", &"a".repeat(64), &"ab".repeat(32)),
        )
        .expect_err("id 不一致必须拒绝");
        assert!(err.contains("身份"), "{err}");
    }

    /// 回执自报的证书指纹必须与这条 TLS 连接实际出示的一致。
    #[test]
    fn invite_grant_must_match_the_certificate_seen_on_this_connection() {
        let err = accept_grant(
            &device("peer-b", 3890),
            Some(&"ab".repeat(32)),
            wire("peer-b", &"a".repeat(64), &"cd".repeat(32)),
        )
        .expect_err("指纹不一致必须拒绝");
        assert!(err.contains("证书指纹"), "{err}");
    }

    /// 老对端不带指纹时，以这条连接实际看到的证书为准（不能采信空值）。
    #[test]
    fn invite_grant_uses_the_observed_certificate_when_the_peer_omits_it() {
        let seen = "ab".repeat(32);
        let grant = accept_grant(
            &device("peer-b", 3890),
            Some(&seen),
            wire("peer-b", &"a".repeat(64), ""),
        )
        .expect("老对端空指纹按观察值落库");
        assert_eq!(grant.fingerprint, seen);
        assert_eq!(grant.peer_id, "peer-b");
    }

    #[test]
    fn invite_grant_rejects_a_malformed_token() {
        let seen = "ab".repeat(32);
        let err = accept_grant(
            &device("peer-b", 3890),
            Some(&seen),
            wire("peer-b", "not-a-token", &seen),
        )
        .expect_err("token 形状不合法必须拒绝");
        assert!(err.contains("token"), "{err}");
    }

    #[test]
    fn invite_grant_requires_a_certificate_from_this_connection() {
        let err = accept_grant(
            &device("peer-b", 3890),
            None,
            wire("peer-b", &"a".repeat(64), &"ab".repeat(32)),
        )
        .expect_err("没有观察值不能落库");
        assert!(err.contains("证书指纹"), "{err}");
    }

    /// 配对失败（探测不通）不能把旧指纹清掉：用户只是重配了一次没成功，
    /// 原来那段关系必须原样保留。
    #[tokio::test]
    async fn a_failed_invite_keeps_the_stored_fingerprint() {
        let (state, dir) = state_for("keep_fp");
        let peer_id = "device-invite-keep";
        credential::remove_peer(peer_id).ok();
        let fingerprint = "ab".repeat(32);
        credential::set_peer_credential(peer_id, &"cd".repeat(32), &fingerprint).unwrap();
        state.upsert_peer(&device(peer_id, unused_port()));

        invite_peer(state.clone(), peer_id)
            .await
            .expect_err("端口上没人，邀请必须失败");

        assert_eq!(
            credential::get_peer_fingerprint(peer_id).unwrap(),
            Some(fingerprint),
            "失败路径不许清掉旧指纹"
        );
        credential::remove_peer(peer_id).ok();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 有 token 却没有可核对的指纹：邀请在发出前就拒绝，不能把 token 发上 TOFU 通道。
    #[tokio::test]
    async fn invite_with_a_token_requires_a_stored_fingerprint() {
        let (state, dir) = state_for("no_pin");
        let peer_id = "device-invite-nopin";
        credential::remove_peer(peer_id).ok();
        credential::set_peer_credential(peer_id, &"cd".repeat(32), "").unwrap();
        state.upsert_peer(&device(peer_id, 1));

        let err = invite_peer(state.clone(), peer_id)
            .await
            .expect_err("没有指纹不能带着 token 发邀请");
        assert!(err.contains("配对码"), "{err}");
        credential::remove_peer(peer_id).ok();
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 未认证来源可以无限 POST `/pair/invite`：表必须有上限，且同一个 invite id
    /// 不能重复插入（否则 withdraw 只摘走第一条，剩下的永远挂着）。
    #[test]
    fn invite_queue_is_capped_and_deduped() {
        let store = PairStore::new();
        for i in 0..MAX_PENDING_INVITES {
            let (tx, _rx) = tokio::sync::oneshot::channel();
            assert!(store.insert_invite(PendingInvite::new(format!("inv-{i}"), peer("peer"), tx)));
        }
        let (tx, _rx) = tokio::sync::oneshot::channel();
        assert!(!store.insert_invite(PendingInvite::new("overflow".to_string(), peer("peer"), tx)));

        let (tx, _rx) = tokio::sync::oneshot::channel();
        assert!(!store.insert_invite(PendingInvite::new("inv-0".to_string(), peer("peer"), tx)));
        assert_eq!(store.invites.lock().len(), MAX_PENDING_INVITES);
    }

    /// PAKE 的一轮握手只绑定"当前这一个码"：生成新码必须让旧码立刻作废。
    #[test]
    fn a_new_code_replaces_the_old_one() {
        let store = PairStore::new();
        let first = store.create_code();
        let second = store.create_code();
        assert_ne!(first, second);
        assert_eq!(store.current_code().as_deref(), Some(second.as_str()));
        assert!(!store.verify_code(&first), "旧码必须立刻作废");
        assert!(store.verify_code(&second));
        assert!(store.current_code().is_none(), "验证即消费");
    }

    /// 发起邀请前要先探测对端：能拿到证书指纹、且对端声明支持 SAS，才返回邀请 id + 码。
    #[tokio::test]
    async fn invite_peer_probes_and_returns_a_sas() {
        let b_dir = std::env::temp_dir().join(format!("orbdrop_invite_b_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&b_dir).unwrap();
        let b_state = Arc::new(AppState::new(
            crate::config::test_config(),
            b_dir.join("config.toml"),
        ));
        let b_port = crate::server::start_server_loopback(b_state.clone(), 4324)
            .await
            .expect("server start");
        let b_id = b_state.config.lock().device_id.clone();

        let a_dir = std::env::temp_dir().join(format!("orbdrop_invite_a_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&a_dir).unwrap();
        let a_state = Arc::new(AppState::new(
            crate::config::test_config(),
            a_dir.join("config.toml"),
        ));
        a_state.upsert_peer(&crate::config::PeerDevice {
            id: b_id.clone(),
            name: "PC-B".into(),
            ip: "127.0.0.1".into(),
            port: b_port,
            online: true,
            paired: false,
            auto_connect: false,
            paused: false,
            relay: None,
        });

        let start = invite_peer(a_state.clone(), &b_id)
            .await
            .expect("探测通过后应返回邀请 id 与 SAS");
        assert!(!start.invite_id.is_empty());
        assert_eq!(start.sas.len(), 9, "8 位 Base32 + 连字符");
        assert_eq!(&start.sas[4..5], "-");

        crate::server::stop_server(&b_state).await;
        let _ = std::fs::remove_dir_all(&b_dir);
        let _ = std::fs::remove_dir_all(&a_dir);
    }
}
