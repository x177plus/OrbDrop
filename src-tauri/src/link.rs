//! 长连接的**同意制**（F-16 的补充）：本机点「连接」不是当场拨号，而是先把请求送到对端 ——
//! 对端在它自己的通知里点「接受」，链路才建起来。
//!
//! 为什么要有这一条：长连接不是一条"本机自己能建"的东西，它是**两端**各留一条实时通道
//! （对端要一直挂着会话、要接广播、要在它自己的界面里多出一台设备）。配对只确立"可以互相
//! 调用"的关系，而"现在把链路挂上"是当下这一刻的事 —— 该由对面的人点头。
//!
//! 一句话协议（一进一出两条 HTTP，**不要求先有链路** —— 链路正是这次在申请的）：
//! - 发起方 `POST /link/request`（带 token）→ 请求挂在对方通知里等它决定；
//! - 对方点「接受」→ 这条 HTTP 回 200，发起方随即拨号（`manual=1`，对端刚点过同意）；
//!   点「拒绝」/ 60 秒没人处理 → 回 403 + 原因，本机不拨号，界面写明是谁的决定。
//! - 发起方点「取消」→ `POST /link/request/withdraw`，把对面那条通知撤掉。
//!
//! **发不出去时退回旧行为**（对端不在网上、或对端是老版本没有这个端点）：直接拨号。对面
//! 压根没人能点头，把"连不上"变成"发不出请求"只是换了个说法；拨号循环本来就会重试与等待。
//!
//! **同意是这一次的**：接受只放行这一次拨号（并结束对端可能有的「断开」暂停）。链路建起来
//! 之后的两侧记忆（F-16 的 `autoConnect`）照旧 —— 掉线重连、重启恢复都不再打扰用户。

use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::sync::oneshot;
use tracing::{info, warn};

use crate::credential;
use crate::state::{AppState, LinkIntent};
use crate::ws;
use crate::ws::events::ui;

/// 一条请求在**对端屏幕上**停多久：到点自动作废（发起方那边同时收手）。
pub const REQUEST_TTL: Duration = Duration::from_secs(60);

/// 发起方等回执的时限：比 `REQUEST_TTL` 多留一点余量，让"对方没点、自动作废"这条正常
/// 路径能走到回执，而不是本机先断在网上。
const HTTP_TIMEOUT: Duration = Duration::from_secs(75);

/// 撤回不必久等：对面多半已经处理完了，撤不成就让它自己的 TTL 兜底。
const WITHDRAW_TIMEOUT: Duration = Duration::from_secs(3);

/// 一条挂在本机界面上、等用户点头的请求。
pub struct PendingLinkRequest {
    pub request_id: String,
    pub peer_id: String,
    pub peer_name: String,
    /// 用户的决定送回那条正挂着的 HTTP 请求（发起方在同步等）。
    responder: Option<oneshot::Sender<Result<(), String>>>,
}

impl PendingLinkRequest {
    pub fn new(
        request_id: String,
        peer_id: String,
        peer_name: String,
        responder: oneshot::Sender<Result<(), String>>,
    ) -> Self {
        Self { request_id, peer_id, peer_name, responder: Some(responder) }
    }

    /// 把决定送回发起方。返回 false = 发起方已经不在了（取消 / 断网 / 进程没了）。
    fn decide(&mut self, outcome: Result<(), String>) -> bool {
        match self.responder.take() {
            Some(tx) => tx.send(outcome).is_ok(),
            None => false,
        }
    }
}

/// 请求表：进来的（等本机用户决定）与出去的（用户点「取消」时凭它对上号）。
#[derive(Default)]
pub struct LinkStore {
    incoming: Mutex<Vec<PendingLinkRequest>>,
    /// `request_id → peer_id`
    outgoing: Mutex<std::collections::HashMap<String, String>>,
}

impl LinkStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert_incoming(&self, req: PendingLinkRequest) {
        self.incoming.lock().push(req);
    }

    /// 摘走一条进来的请求（用户决定 / 超时 / 对方撤回都从这里出去）：谁先摘到谁说了算。
    pub fn take_incoming(&self, request_id: &str) -> Option<PendingLinkRequest> {
        let mut list = self.incoming.lock();
        let pos = list.iter().position(|r| r.request_id == request_id)?;
        Some(list.remove(pos))
    }

    pub fn insert_outgoing(&self, request_id: String, peer_id: String) {
        self.outgoing.lock().insert(request_id, peer_id);
    }

    /// 摘掉一条本机发出的请求，返回对端 id（收尾清理 / 用户取消）。
    pub fn take_outgoing(&self, request_id: &str) -> Option<String> {
        self.outgoing.lock().remove(request_id)
    }

    /// 这条进来的请求此刻是不是还挂在本机界面上（等用户决定）。
    pub fn has_incoming(&self, request_id: &str) -> bool {
        self.incoming.lock().iter().any(|r| r.request_id == request_id)
    }

    /// 本机发出去的请求表里有没有这一台（撤回时凭它对上号）。
    pub fn has_outgoing_for(&self, peer_id: &str) -> bool {
        self.outgoing.lock().values().any(|p| p == peer_id)
    }
}

// ---------------------------------------------------------------------------
// 接收方（有人来请求与本机建立长连接）
// ---------------------------------------------------------------------------

/// 接收方：挂上一条请求（`POST /link/request` 里调），等用户决定。
pub fn note_incoming(state: &Arc<AppState>, req: PendingLinkRequest) {
    let request_id = req.request_id.clone();
    state.link.insert_incoming(req);
    schedule_timeout(state, request_id);
}

/// 接收方：用户点了「接受」→ 结束本机可能有的「断开」暂停，放行这一次拨号。
///
/// 顺序是刻意的：**先把手伸过去**（oneshot 送得到 = 发起方还在等），再动本机状态 ——
/// 反过来的话，发起方中途取消/断网就会在本机留下一条"我同意过"的记忆。
pub fn accept(state: &Arc<AppState>, request_id: &str) -> Result<String, String> {
    let mut req = state
        .link
        .take_incoming(request_id)
        .ok_or_else(|| "连接请求不存在或已作废".to_string())?;
    let peer = req.peer_name.clone();
    if !req.decide(Ok(())) {
        return Err(format!("「{peer}」已离开，连接请求未生效"));
    }
    // **对端点过「断开」时，接受就是它的撤销**：于是它这一次拨号不该再被 403 挡住。
    // 落盘的意图也一起写上（只清内存的话，本机下次启动又会拒绝它 —— 与
    // `server::events_ws_handler` 里 `manual=1` 那条路同一个理由）。
    state.ws.resume(&req.peer_id);
    state.set_link_intent(&req.peer_id, LinkIntent::Wanted);
    state.emit_devices_changed();
    info!(peer_id = %req.peer_id, request_id, "link request accepted");
    Ok(peer)
}

/// 接收方：用户点了「拒绝」—— 给发起方一个明确的"不"，本机什么都不写。
pub fn reject(state: &AppState, request_id: &str) {
    close_incoming(state, request_id, "对方拒绝了连接请求");
}

/// 接收方：一条进来的请求作废（超时 / 发起方撤回）：回执一个原因，并把本机界面上那条
/// 通知撤掉。返回是否真有一条挂着（处理过的不会再来一次）。
fn close_incoming(state: &AppState, request_id: &str, reason: &str) -> bool {
    let Some(mut req) = state.link.take_incoming(request_id) else {
        return false;
    };
    req.decide(Err(reason.to_string()));
    ws::emit_ui(state, ui::LINK_REQUEST_CLOSED, &serde_json::json!({
        "requestId": request_id,
        "reason": reason,
    }));
    true
}

/// 接收方：请求到点没人处理就自动作废（发起方在等回执，两边必须同时收手）。
pub fn schedule_timeout(state: &Arc<AppState>, request_id: String) {
    let st = state.clone();
    crate::task::spawn("link request timeout", async move {
        tokio::time::sleep(REQUEST_TTL).await;
        if close_incoming(&st, &request_id, "超时未处理") {
            info!(request_id, "link request expired");
        }
    });
}

/// 接收方：发起方把请求撤回了（`POST /link/request/withdraw`）。
pub fn withdraw_incoming(state: &AppState, request_id: &str) {
    close_incoming(state, request_id, "对方已取消");
}

// ---------------------------------------------------------------------------
// 发起方
// ---------------------------------------------------------------------------

/// 本机点「连接」：把请求发给对端，返回请求 id。立刻返回、不等对端决定 —— 等待最长一分钟，
/// 界面不能卡在那儿；结果走 `LINK_REQUEST_RESULT` 事件回来（本机这边挂一张"等待对方同意"
/// 的卡片）。
pub fn request(state: &Arc<AppState>, peer_id: &str) -> Result<String, String> {
    let peer = state
        .get_peer(peer_id)
        .ok_or_else(|| format!("设备不存在: {peer_id}"))?;
    if !peer.paired {
        return Err(format!("还没有与「{}」配对", peer.name));
    }
    let request_id = uuid::Uuid::new_v4().to_string();
    state.link.insert_outgoing(request_id.clone(), peer_id.to_string());
    // 界面凭这条挂"等待对方同意"的卡片（与配对邀请同一套：发起方也看得到自己那一步）。
    ws::emit_ui(state, ui::LINK_REQUEST_SENT, &serde_json::json!({
        "requestId": request_id,
        "peerId": peer.id,
        "peerName": peer.name,
    }));

    let own_name = state.config.lock().device_name.clone();
    let state2 = state.clone();
    let id2 = request_id.clone();
    crate::task::spawn("link request", async move {
        let outcome = deliver_http(&peer, &id2, &own_name).await;
        // 用户点过「取消」时这条请求已经从这里摘走了：那次点击的意思是"别连了" ——
        // 悄悄收场，既不拨号也不去改链路状态。
        if state2.link.take_outgoing(&id2).is_none() {
            info!(request_id = %id2, "link request cancelled locally, dropping the outcome");
            return;
        }
        let (outcome, reason) = match outcome {
            Ok(()) => {
                info!(peer_id = %peer.id, request_id = %id2, "link request accepted by peer");
                // 对端刚点过「接受」：这一次拨号带 `manual=1`（它的暂停已经被它自己撤了，
                // 这里也照旧只按"用户此刻让我连"的一次性声明拨第一下）。
                state2.ws.resume(&peer.id);
                state2.set_link_intent(&peer.id, LinkIntent::Wanted);
                ws::ensure_link_now(&state2, &peer.id);
                ("granted", String::new())
            }
            Err(Failure::Unreachable(why)) => {
                // 请求送不到（对端不在网上 / 老版本没有这个端点）：退回旧行为 —— 直接拨号。
                // 对面压根没人能点头，而拨号循环自己会重试、会等它上线。
                info!(peer_id = %peer.id, "link request undeliverable ({why}), dialing directly");
                state2.ws.resume(&peer.id);
                ws::ensure_link_now(&state2, &peer.id);
                ("fallback", why)
            }
            Err(Failure::Refused(why)) => {
                info!(peer_id = %peer.id, request_id = %id2, "link request refused: {why}");
                state2.ws.set_link_state(
                    &peer.id,
                    ws::LinkState::Stopped { reason: ws::StopReason::PeerRejected },
                );
                state2.emit_devices_changed();
                ("denied", why)
            }
        };
        ws::emit_ui(&state2, ui::LINK_REQUEST_RESULT, &serde_json::json!({
            "requestId": id2,
            "peerId": peer.id,
            "outcome": outcome,
            "reason": reason,
        }));
    });

    Ok(request_id)
}

/// 发起方：用户点了「取消」——本机不再等这一次结果，并（尽力）把对端那条通知撤掉。
pub fn cancel(state: &Arc<AppState>, request_id: &str) -> Result<(), String> {
    let Some(peer_id) = state.link.take_outgoing(request_id) else {
        return Ok(()); // 已经收场了（对方先答了 / 早撤过）
    };
    let Some(peer) = state.get_peer(&peer_id) else {
        return Ok(());
    };
    let id = request_id.to_string();
    crate::task::spawn("link request withdraw", async move {
        let url = format!("{}/link/request/withdraw", crate::net::peer_base(peer.dial().0, peer.dial().1));
        let mut req = crate::net::client()
            .post(&url)
            .timeout(WITHDRAW_TIMEOUT)
            .json(&serde_json::json!({ "requestId": id }));
        if let Ok(Some(token)) = credential::get_peer_token(&peer.id) {
            req = req.bearer_auth(token);
        }
        if let Err(e) = req.send().await {
            // 撤不回来不是错误：对面那条通知有自己的 TTL（到点自己消失）。
            warn!(request_id = %id, "link request withdraw failed: {e}");
        }
    });
    Ok(())
}

/// 请求失败的两种性质，处置完全不同。
enum Failure {
    /// 请求**没有送到**（连不上 / 对端老版本没有这个端点）：退回旧行为，直接拨号。
    Unreachable(String),
    /// 请求送到了，对端说了"不"（拒绝 / 超时未处理 / 撤回）。
    Refused(String),
}

/// 把请求 POST 到对端的 `/link/request`，等它对用户做决定。
async fn deliver_http(
    peer: &crate::config::PeerDevice,
    request_id: &str,
    own_name: &str,
) -> Result<(), Failure> {
    let url = format!(
        "{}/link/request",
        crate::net::peer_base(peer.dial().0, peer.dial().1)
    );
    let mut req = crate::net::client()
        .post(&url)
        .timeout(HTTP_TIMEOUT)
        .json(&serde_json::json!({
            "requestId": request_id,
            // 名字只是给对端**显示**用（它自己设备表里那一份优先，见 routes_link）。
            "peerName": own_name,
        }));
    let token = match credential::get_peer_token(&peer.id) {
        Ok(Some(t)) => Some(t),
        // 没有 token（理论上不该：上一步已经确认配对了）→ 请求送不出去，退回旧行为。
        _ => None,
    };
    let Some(token) = token else {
        return Err(Failure::Unreachable("本机没有它的凭据".to_string()));
    };
    req = req.bearer_auth(token);

    let resp = match req.send().await {
        Ok(resp) => resp,
        Err(_) => {
            // 连不上：不是"对面说了不"，是这条 HTTP 本身没走到（对端不在网上 / TLS 不通）。
            return Err(Failure::Unreachable(format!("联系不上「{}」", peer.name)));
        }
    };
    refusal_for_status(resp.status(), resp).await
}

/// 回执状态码 → 三种去向（纯函数：判错一个，老版本对端就会显示成"对方拒绝"）。
#[derive(Debug, PartialEq, Eq)]
enum StatusOutcome {
    /// 对端点了「接受」。
    Accepted,
    /// **这个端点不存在** = 老版本对端：它没法点头同意，该退回旧行为直接拨号。
    NoSuchEndpoint,
    /// 送到了，对端说了"不"（拒绝 / 超时未处理 / 已撤回）。
    Refused,
}

fn outcome_of_status(status: reqwest::StatusCode) -> StatusOutcome {
    if status.is_success() {
        return StatusOutcome::Accepted;
    }
    if status == reqwest::StatusCode::NOT_FOUND || status == reqwest::StatusCode::METHOD_NOT_ALLOWED {
        return StatusOutcome::NoSuchEndpoint;
    }
    StatusOutcome::Refused
}

/// 这条回执怎么算，见 [`StatusOutcome`]。
///
/// `reqwest` 不会因为非 2xx 就在 `send()` 上报错，所以只能按状态码自己判 —— 这也是
/// 这里单列一个函数的原因（"404 当成拒绝"那种错在真机上要两台版本不同的设备才碰得到）。
async fn refusal_for_status(
    status: reqwest::StatusCode,
    resp: reqwest::Response,
) -> Result<(), Failure> {
    match outcome_of_status(status) {
        StatusOutcome::Accepted => Ok(()),
        StatusOutcome::NoSuchEndpoint => Err(Failure::Unreachable("对端版本不支持连接请求".to_string())),
        StatusOutcome::Refused => {
            // 403（拒绝 / 超时未处理 / 已撤回）等：正文就是对端给的原因（axum 的文本响应）。
            let reason = resp.text().await.unwrap_or_default();
            Err(Failure::Refused(if reason.trim().is_empty() {
                "对方没有接受连接请求".to_string()
            } else {
                reason
            }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 状态码的三档：**404/405 必须是"没有这个端点"**（老版本对端 → 退回直接拨号），
    /// 不能混进"对方拒绝" —— 那会让升级不同步的两台机器之间"点一次「连接」就被拒一次"。
    #[test]
    fn status_outcomes_separate_an_old_peer_from_a_refusal() {
        assert_eq!(outcome_of_status(reqwest::StatusCode::OK), StatusOutcome::Accepted);
        assert_eq!(
            outcome_of_status(reqwest::StatusCode::NO_CONTENT),
            StatusOutcome::Accepted
        );
        assert_eq!(
            outcome_of_status(reqwest::StatusCode::NOT_FOUND),
            StatusOutcome::NoSuchEndpoint
        );
        assert_eq!(
            outcome_of_status(reqwest::StatusCode::METHOD_NOT_ALLOWED),
            StatusOutcome::NoSuchEndpoint
        );
        assert_eq!(
            outcome_of_status(reqwest::StatusCode::FORBIDDEN),
            StatusOutcome::Refused
        );
        assert_eq!(
            outcome_of_status(reqwest::StatusCode::INTERNAL_SERVER_ERROR),
            StatusOutcome::Refused
        );
    }
}