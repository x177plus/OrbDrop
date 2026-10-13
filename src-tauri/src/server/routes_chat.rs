//! 会话（聊天）的 HTTP 那一跳。
//!
//! 消息**不以"先配对 + 点连接"为前提**：长连接在就走 WS（快、即时），不在就 POST 到这里 ——
//! 与推送邀请同一条路数（`/push/invite`）。
//!
//! **不要求预先配对**：未绑定设备也能发消息（消息本身不落盘、不占带宽，风险只有"谁都能往
//! 你会话里塞字"，由用户自己决定留不留这台设备）。安全只靠一条与推送同一个判断：
//! **自称是本机已配对设备的，必须出示那枚 token** —— 否则等于谁都能冒充你的老搭档说话。
//!
//! 认完人之后**直接交给同一个 dispatcher**（`ws::session::handle_inbound`）：WS 那条路
//! 用的就是它，落账、推 `chat://changed`、按窗口标已读都只有一份实现。

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{ConnectInfo, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use serde::Deserialize;
use tracing::info;

use crate::config::PeerDevice;
use crate::net;
use crate::state::AppState;
use crate::ws::events::{PeerInfo, WsEvent};
use crate::ws::session::peer_id_from_token;

/// POST /chat/message —— 收到对端一条会话消息（没有长连接时走这条路）。
#[derive(Deserialize)]
pub struct ChatMessageBody {
    id: String,
    text: String,
    at: u64,
    /// 发送方的**服务端口**：请求的源端口是一次性的，回执要发回它真正的服务端口。
    port: u16,
    /// 发送方自报的身份（未配对时本机只能采信它 —— 与推送邀请同一档信任）。
    sender: PeerInfo,
}

/// POST /chat/read —— 收到对端一枚已读回执（没有长连接时走这条路）。
///
/// 字段名跟着**线上那份 `WsEvent` 的 serde 形状**写（`chat_read` 的载荷就叫 `up_to`）：
/// 请求体就是那条 WS 事件 + 两个额外字段，名字一旦对不上，axum 的 Json 提取会直接 422
/// —— 而发送方只看得到"没打通"。`body_shapes_match_the_wire_event` 那条单测钉着这件事。
#[derive(Deserialize)]
pub struct ChatReadBody {
    up_to: u64,
    port: u16,
    sender: PeerInfo,
}

pub async fn chat_message_handler(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<ChatMessageBody>,
) -> Result<StatusCode, (StatusCode, String)> {
    let event = WsEvent::ChatText { id: body.id, text: body.text, at: body.at };
    accept(&state, addr, &headers, &body.sender, body.port, event).await
}

/// POST /chat/image —— 收到对端一条**图片消息**（没有长连接时走这条路；带长连接那条路走
/// `WsEvent::ChatImage`，见 `ws::session`）。
///
/// 体就是那条事件 + `port` / `sender`（`deliver_http` 就是这么拼的，见 `body_shapes`……那条
/// 单测钉着这件事）。体里可能装着几 MB 的 base64 —— 这条路由的**体积上限**比别的会话路由高
/// 一档（`server::IMAGE_BODY_LIMIT`），不然默认那 2MB 会把一张两兆的截图直接 413 掉。
#[derive(Deserialize)]
pub struct ChatImageBody {
    id: String,
    at: u64,
    name: String,
    #[serde(default)]
    size: u64,
    data: String,
    port: u16,
    sender: PeerInfo,
}

pub async fn chat_image_handler(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<ChatImageBody>,
) -> Result<StatusCode, (StatusCode, String)> {
    let event = WsEvent::ChatImage {
        id: body.id,
        at: body.at,
        name: body.name,
        size: body.size,
        data: body.data,
    };
    accept(&state, addr, &headers, &body.sender, body.port, event).await
}

pub async fn chat_read_handler(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<ChatReadBody>,
) -> Result<StatusCode, (StatusCode, String)> {
    accept(&state, addr, &headers, &body.sender, body.port, WsEvent::ChatRead { up_to: body.up_to }).await
}

/// POST /chat/group —— 收到对端一条群消息（没有长连接时走这条路）。
///
/// 字段名与线上那份 `WsEvent::GroupText` 逐字对应（`deliver_http` 就是把事件序列化后加
/// `port`/`sender` 发过来的）；`body_shapes_match_the_wire_event` 那条单测钉着这件事。
#[derive(Deserialize)]
pub struct GroupTextBody {
    group_id: String,
    id: String,
    text: String,
    at: u64,
    seq: u64,
    rev: u64,
    /// **谁说的**（中转过来的消息里它不是信封上那一台）。收方只在它确实是名册成员时才采信。
    author: PeerInfo,
    #[serde(default)]
    hops: u8,
    #[serde(default)]
    reached: Vec<String>,
    /// 图片消息那几格（与线上那条 `WsEvent::GroupText` 逐字对应，见 `events.rs`）：
    /// `data` 是内容本身（base64）—— 所以 `/chat/group` 这条路由的体积上限也抬了一档。
    #[serde(default)]
    image: bool,
    #[serde(default)]
    name: String,
    #[serde(default)]
    size: u64,
    #[serde(default)]
    data: String,
    port: u16,
    /// 信封上的身份（本帧是谁发来的）：认人、`upsert_peer` 都用它，与 `author` 是两件事。
    sender: PeerInfo,
}

/// POST /chat/group-read —— 收到对端一枚群已读回执。
#[derive(Deserialize)]
pub struct GroupReadBody {
    group_id: String,
    up_to: u64,
    port: u16,
    sender: PeerInfo,
}

/// POST /chat/group-roster —— 收到对端一份群名册（建群邀请 / 成员变更 / 补发时补的那份）。
///
/// 名册是**嵌套**的（事件里就是 `roster: {...}`），体形状照搬，不摊平。
#[derive(Deserialize)]
pub struct GroupRosterBody {
    roster: crate::group::RosterPayload,
    port: u16,
    sender: PeerInfo,
}

pub async fn group_text_handler(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<GroupTextBody>,
) -> Result<StatusCode, (StatusCode, String)> {
    let event = WsEvent::GroupText {
        group_id: body.group_id,
        id: body.id,
        text: body.text,
        at: body.at,
        seq: body.seq,
        rev: body.rev,
        author: body.author,
        hops: body.hops,
        reached: body.reached,
        image: body.image,
        name: body.name,
        size: body.size,
        data: body.data,
    };
    accept(&state, addr, &headers, &body.sender, body.port, event).await
}

pub async fn group_read_handler(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<GroupReadBody>,
) -> Result<StatusCode, (StatusCode, String)> {
    let event = WsEvent::GroupRead { group_id: body.group_id, up_to: body.up_to };
    accept(&state, addr, &headers, &body.sender, body.port, event).await
}

pub async fn group_roster_handler(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<GroupRosterBody>,
) -> Result<StatusCode, (StatusCode, String)> {
    let event = WsEvent::GroupRoster { roster: body.roster, sender: body.sender.clone() };
    accept(&state, addr, &headers, &body.sender, body.port, event).await
}

/// POST /chat/group-sync —— 收到对端一个补拉请求（没有长连接时走这条路）。
#[derive(Deserialize)]
pub struct GroupSyncAskBody {
    group_id: String,
    ask_id: String,
    since_at: u64,
    #[serde(default)]
    holes: Vec<crate::group::SyncHole>,
    port: u16,
    sender: PeerInfo,
}

/// POST /chat/group-sync-post —— 收到补拉的回答。
#[derive(Deserialize)]
pub struct GroupSyncPostBody {
    group_id: String,
    ask_id: String,
    #[serde(default)]
    roster: Option<crate::group::RosterPayload>,
    #[serde(default)]
    has_more: bool,
    #[serde(default)]
    messages: Vec<crate::group::GroupMsgWire>,
    port: u16,
    sender: PeerInfo,
}

/// POST /chat/group-ack —— 收到"我手上有你发的这些"（补回来的消息的回执）。
#[derive(Deserialize)]
pub struct GroupAckBody {
    group_id: String,
    up_to: u64,
    port: u16,
    sender: PeerInfo,
}

pub async fn group_sync_ask_handler(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<GroupSyncAskBody>,
) -> Result<StatusCode, (StatusCode, String)> {
    let event = WsEvent::GroupSyncAsk {
        group_id: body.group_id,
        ask_id: body.ask_id,
        since_at: body.since_at,
        holes: body.holes,
    };
    accept(&state, addr, &headers, &body.sender, body.port, event).await
}

pub async fn group_sync_post_handler(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<GroupSyncPostBody>,
) -> Result<StatusCode, (StatusCode, String)> {
    let event = WsEvent::GroupSyncPost {
        group_id: body.group_id,
        ask_id: body.ask_id,
        roster: body.roster,
        has_more: body.has_more,
        messages: body.messages,
    };
    accept(&state, addr, &headers, &body.sender, body.port, event).await
}

pub async fn group_ack_handler(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<GroupAckBody>,
) -> Result<StatusCode, (StatusCode, String)> {
    let event = WsEvent::GroupAck { group_id: body.group_id, up_to: body.up_to };
    accept(&state, addr, &headers, &body.sender, body.port, event).await
}

/// POST /chat/group-reject —— 收到"这一条我收不下"（拒收回执）。
///
/// 没有长连接时，收方的这条回话只能反拨发送方；`reason` 是这个形状里**唯一有信息量**的字段
/// （`declined` 让发送方把那位成员标成「已拒绝加入」）。
#[derive(Deserialize)]
pub struct GroupTextRejectBody {
    group_id: String,
    id: String,
    #[serde(default)]
    reason: String,
    port: u16,
    sender: PeerInfo,
}

pub async fn group_text_reject_handler(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<GroupTextRejectBody>,
) -> Result<StatusCode, (StatusCode, String)> {
    let event = WsEvent::GroupTextRejected {
        group_id: body.group_id,
        id: body.id,
        reason: body.reason,
    };
    accept(&state, addr, &headers, &body.sender, body.port, event).await
}

/// 认人 + 落账。两条路（消息 / 回执）共用。
async fn accept(
    state: &Arc<AppState>,
    addr: SocketAddr,
    headers: &HeaderMap,
    sender: &PeerInfo,
    port: u16,
    event: WsEvent,
) -> Result<StatusCode, (StatusCode, String)> {
    // 地址簿里这台是不是"已绑定"（已配对）。
    let paired = state.get_peer(&sender.id).is_some_and(|p| p.paired);
    // 冒充已配对设备：拿不出 token 就拒 —— 走得过这一关的才是身份确认过的老搭档。
    if paired {
        let claimed = bearer_token(headers).and_then(|t| peer_id_from_token(state, &t));
        if claimed.as_deref() != Some(sender.id.as_str()) {
            info!(sender = %sender.id, "会话消息被拒：自称已配对设备但 token 不认");
            return Err((StatusCode::UNAUTHORIZED, "身份校验失败".into()));
        }
    }

    // 未绑定/没见过的发送方也要能在会话列表里露出来：登记进设备表（ip 取自 TCP 来源，
    // 不可伪造；端口用它自报的服务端口）。已经配对的那些原样保留（`upsert_peer` 只增不减）。
    //
    // 来源地址是本机自己的监听端点时（模拟器/NAT 后面的对端回连进来，见
    // `net::observed_dial_target`）、或者来源是**本机认识的另一台设备**时（这条连接是
    // 经它中转来的，F-106），**一处都不记**：前者没有可拨的地址；后者记下来就把中转方
    // 的地址当成了发送方的地址（本机以后会拿它去"拨发送方"，实际拨到中转方身上）。
    // 两种情形的可达性都只能由长连接证明（`ws::session` 在链路断开时把这种行标回离线），
    // 这里也不替它声明"在线"，否则设备行会在"离线"与"已配对"之间来回跳。
    let (ip, port, online) = match net::observed_dial_target(addr.ip(), port, state.listening_port()) {
        Some((ip, port)) if !state.is_relayed_source(addr.ip(), &sender.id) => (ip, port, true),
        _ => (String::new(), 0, false),
    };
    state.upsert_peer(&PeerDevice {
        id: sender.id.clone(),
        name: sender.name.clone(),
        ip,
        port,
        online,
        paired,
        auto_connect: false,
        paused: false,
        relay: None,
    });

    info!(sender = %sender.id, name = %sender.name, paired, "inbound chat over http");
    // **群消息这一条要看落账结果**：收不下（本机拒绝过这个群 / 名册没到）就回 409 ——
    // 恒回 204 会让发送方把那一条记成「已送达」，而它实际被丢掉了（长连接那条路靠
    // `group_text_rejected` 反向纠正，HTTP 这一跳就靠这个状态码）。理由代码同时带上：
    // 日志里要能看出"是哪种收不下"。
    match crate::ws::session::handle_inbound(state, &sender.id, event).await {
        crate::ws::session::InboundResult::Accepted => Ok(StatusCode::NO_CONTENT),
        crate::ws::session::InboundResult::GroupRefused(reason) => {
            Err((StatusCode::CONFLICT, format!("group text refused: {reason}")))
        }
        // 图片内容收不下（坏了 / 太大 / 写不进去）：回 400 —— 发送方落「未送达」、不重试，
        // 本机也**没有**记下这条消息（见 `chat::accept_inbound_image`）。回 204 会让发送方
        // 以为这条到了，而它在本机根本不存在。
        crate::ws::session::InboundResult::ImageRefused => {
            Err((StatusCode::BAD_REQUEST, "image content refused".to_string()))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 这条路收到的体就是"那条 WS 事件 + `port` / `sender`"（见 `chat::deliver_http`）。
    /// 字段名必须与线上形状逐字对上 —— 对不上是 422，发送方那边只看到"没打通"。
    #[test]
    fn body_shapes_match_the_wire_event() {
        let mut text = serde_json::to_value(WsEvent::ChatText {
            id: "m1".into(),
            text: "在吗".into(),
            at: 42,
        })
        .unwrap();
        text["port"] = serde_json::json!(3890);
        text["sender"] = serde_json::json!({ "id": "p1", "name": "PC-B" });
        let parsed: ChatMessageBody = serde_json::from_value(text).expect("消息体的字段名必须对得上");
        assert_eq!(parsed.id, "m1");
        assert_eq!(parsed.text, "在吗");
        assert_eq!(parsed.at, 42);
        assert_eq!(parsed.port, 3890);
        assert_eq!(parsed.sender.id, "p1");

        let mut read = serde_json::to_value(WsEvent::ChatRead { up_to: 42 }).unwrap();
        read["port"] = serde_json::json!(3890);
        read["sender"] = serde_json::json!({ "id": "p1", "name": "PC-B" });
        let parsed: ChatReadBody = serde_json::from_value(read).expect("回执体的字段名必须对得上");
        assert_eq!(parsed.up_to, 42);
        assert_eq!(parsed.sender.name, "PC-B");
    }

    /// 群聊三条同理：体就是"那条 WS 事件 + `port` / `sender`"。
    ///
    /// `group_text` 的 `sender` 有两重身份（事件里自报的名字 + 信封上的身份），是同一个值，
    /// 所以体里只有一个 `sender` 字段。
    #[test]
    fn group_body_shapes_match_the_wire_event() {
        let sender = PeerInfo { id: "p1".into(), name: "PC-B".into() };
        // 中转过来的那一条最容易写错：**作者**是 A，**信封上的发送方**是搬运工 B ——
        // HTTP 体上这两个字段必须各是各的（同名会被信封覆盖掉，作者就变成搬运工）。
        let mut text = serde_json::to_value(WsEvent::GroupText {
            group_id: "g1".into(),
            id: "m1".into(),
            text: "群里说一句".into(),
            at: 42,
            seq: 3,
            rev: 7,
            author: PeerInfo { id: "p0".into(), name: "PC-A".into() },
            hops: 1,
            reached: vec!["p0".into()],
            image: true,
            name: "图片-1.png".into(),
            size: 7,
            data: "REFUQQ==".into(),
        })
        .unwrap();
        text["port"] = serde_json::json!(3890);
        text["sender"] = serde_json::json!({ "id": "p1", "name": "PC-B" });
        let parsed: GroupTextBody = serde_json::from_value(text).expect("群消息体的字段名必须对得上");
        assert_eq!(parsed.group_id, "g1");
        assert_eq!(parsed.seq, 3);
        assert_eq!(parsed.rev, 7);
        assert_eq!(parsed.sender.id, "p1", "信封上是搬运工");
        assert_eq!(parsed.author.id, "p0", "作者另有其人，没被信封覆盖");
        assert_eq!(parsed.hops, 1, "中转的跳数要在体里带回来");
        // 图片消息那几格也要逐字对上（体里装着那张图的字节）。
        assert!(parsed.image);
        assert_eq!(parsed.name, "图片-1.png");
        assert_eq!(parsed.size, 7);
        assert_eq!(parsed.data, "REFUQQ==");
        assert_eq!(parsed.reached, vec!["p0".to_string()]);
        assert_eq!(sender.name, "PC-B");

        let mut read = serde_json::to_value(WsEvent::GroupRead { group_id: "g1".into(), up_to: 42 }).unwrap();
        read["port"] = serde_json::json!(3890);
        read["sender"] = serde_json::json!({ "id": "p1", "name": "PC-B" });
        let parsed: GroupReadBody = serde_json::from_value(read).expect("群回执体的字段名必须对得上");
        assert_eq!((parsed.group_id.as_str(), parsed.up_to), ("g1", 42));

        let mut roster = serde_json::to_value(WsEvent::GroupRoster {
            roster: crate::group::RosterPayload {
                id: "g1".into(),
                name: "群里".into(),
                rev: 2,
                updated_by: "p1".into(),
                updated_at: 9,
                members: vec![sender.clone()],
            },
            sender: sender.clone(),
        })
        .unwrap();
        roster["port"] = serde_json::json!(3890);
        roster["sender"] = serde_json::json!({ "id": "p1", "name": "PC-B" });
        let parsed: GroupRosterBody = serde_json::from_value(roster).expect("名册体的字段名必须对得上");
        assert_eq!(parsed.roster.rev, 2);
        assert_eq!(parsed.roster.members.len(), 1);

        // 拒收回执（`/chat/group-reject`）：与线上事件逐字对应 —— `reason` 是**唯一有信息量**
        // 的字段（`declined` 让发送方把那台设备标成「已拒绝加入」），键名错了就等于没带回来。
        let mut reject = serde_json::to_value(WsEvent::GroupTextRejected {
            group_id: "g1".into(),
            id: "m1".into(),
            reason: crate::group::REASON_DECLINED.into(),
        })
        .unwrap();
        reject["port"] = serde_json::json!(3890);
        reject["sender"] = serde_json::json!({ "id": "p1", "name": "PC-B" });
        let parsed: GroupTextRejectBody = serde_json::from_value(reject).expect("拒收体的字段名必须对得上");
        assert_eq!((parsed.group_id.as_str(), parsed.id.as_str()), ("g1", "m1"));
        assert_eq!(parsed.reason, crate::group::REASON_DECLINED);
    }
}

fn bearer_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get("Authorization")?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(|t| t.to_string())
}