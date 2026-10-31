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

use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use axum::extract::{ConnectInfo, Extension, State};
use axum::http::{HeaderMap, StatusCode};
use axum::Json;
use parking_lot::Mutex;
use serde::Deserialize;
use tracing::{debug, info, warn};

use crate::config::PeerDevice;
use crate::net;
use crate::server::auth::bearer_token;
use crate::state::AppState;
use crate::ws::events::{PeerInfo, WsEvent};
use crate::ws::session::peer_id_from_token;

/// 未配对来源每 IP 每分钟最多多少条会话请求。已配对设备不经过这道闸（它们有 token，
/// 而且正常用户不会一分钟发几百条）。120 远高于人的打字速度，只挡脚本刷盘。
const UNPAIRED_CHAT_PER_MINUTE: u32 = 120;
const UNPAIRED_WINDOW: Duration = Duration::from_secs(60);

static UNPAIRED_RATE: LazyLock<Mutex<HashMap<IpAddr, (Instant, u32)>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

fn allow_unpaired(ip: IpAddr) -> bool {
    // 回环是本机 UI/测试，不走这道闸；限流只针对局域网来源（也避免同机多实例互相限）。
    if ip.is_loopback() {
        return true;
    }
    let mut map = UNPAIRED_RATE.lock();
    let now = Instant::now();
    map.retain(|_, (at, _)| now.duration_since(*at) < UNPAIRED_WINDOW);
    let entry = map.entry(ip).or_insert((now, 0));
    if now.duration_since(entry.0) >= UNPAIRED_WINDOW {
        *entry = (now, 0);
    }
    if entry.1 >= UNPAIRED_CHAT_PER_MINUTE {
        return false;
    }
    entry.1 += 1;
    true
}

/// 公开面上的发送方身份是**自称**的：先校验形状/长度（`ids::is_segment`：`[A-Za-z0-9._-]`、
/// ≤128），再进日志/验签/落账。少了这一道，一条 2MB 的 `sender.id` 会被当成会话键或
/// `read_by` 的键写进 `chat.dat` / `groups.dat`（`upsert_peer` 只是跳过建设备行，不挡后面）。
fn check_sender(sender: &PeerInfo) -> Result<(), (StatusCode, String)> {
    if crate::ids::is_segment(&sender.id) {
        return Ok(());
    }
    // 只记长度：非法 id 本身可能就是几 MB，不能原样写进日志。
    info!(
        id_len = sender.id.len(),
        "会话请求被拒：发送方设备 id 不合法"
    );
    Err((StatusCode::BAD_REQUEST, "身份标识不合法".into()))
}

/// 这条 TLS 连接出示的客户端证书指纹（明文回环 / 老客户端 = `None`）。
///
/// 未配对设备的地址变更拿它做连续性校验：证书相同 = 对面还持有同一把私钥。
fn client_fingerprint(cert: &Option<Extension<net::ClientCertFingerprint>>) -> Option<&str> {
    cert.as_ref().and_then(|Extension(fp)| fp.0.as_deref())
}

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
    client_cert: Option<Extension<net::ClientCertFingerprint>>,
    Json(body): Json<ChatMessageBody>,
) -> Result<StatusCode, (StatusCode, String)> {
    check_sender(&body.sender)?;
    // 信封上的名字一并装进事件（长连接那条路只能从载荷里带名字，见 `ChatText::from_name`）：
    // 于是"填占位名"只有一处实现（`handle_inbound` 的 `ChatText` 臂），HTTP 与长连接两条
    // 路的收尾完全一致。
    let event = WsEvent::ChatText {
        id: body.id,
        text: body.text,
        at: body.at,
        from_name: body.sender.name.clone(),
    };
    accept(
        &state,
        addr,
        &headers,
        client_fingerprint(&client_cert),
        &body.sender,
        body.port,
        event,
    )
    .await
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
    client_cert: Option<Extension<net::ClientCertFingerprint>>,
    Json(body): Json<ChatImageBody>,
) -> Result<StatusCode, (StatusCode, String)> {
    check_sender(&body.sender)?;
    let event = WsEvent::ChatImage {
        id: body.id,
        at: body.at,
        name: body.name,
        size: body.size,
        data: body.data,
        from_name: body.sender.name.clone(),
    };
    accept(
        &state,
        addr,
        &headers,
        client_fingerprint(&client_cert),
        &body.sender,
        body.port,
        event,
    )
    .await
}

pub async fn chat_read_handler(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    client_cert: Option<Extension<net::ClientCertFingerprint>>,
    Json(body): Json<ChatReadBody>,
) -> Result<StatusCode, (StatusCode, String)> {
    check_sender(&body.sender)?;
    accept(
        &state,
        addr,
        &headers,
        client_fingerprint(&client_cert),
        &body.sender,
        body.port,
        WsEvent::ChatRead { up_to: body.up_to },
    )
    .await
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
    #[serde(default)]
    epoch: u64,
    #[serde(default)]
    pubkey: String,
    #[serde(default)]
    sig: String,
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
    client_cert: Option<Extension<net::ClientCertFingerprint>>,
    Json(body): Json<GroupTextBody>,
) -> Result<StatusCode, (StatusCode, String)> {
    check_sender(&body.sender)?;
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
        epoch: body.epoch,
        pubkey: body.pubkey,
        sig: body.sig,
    };
    accept(
        &state,
        addr,
        &headers,
        client_fingerprint(&client_cert),
        &body.sender,
        body.port,
        event,
    )
    .await
}

pub async fn group_read_handler(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    client_cert: Option<Extension<net::ClientCertFingerprint>>,
    Json(body): Json<GroupReadBody>,
) -> Result<StatusCode, (StatusCode, String)> {
    check_sender(&body.sender)?;
    let event = WsEvent::GroupRead {
        group_id: body.group_id,
        up_to: body.up_to,
    };
    accept(
        &state,
        addr,
        &headers,
        client_fingerprint(&client_cert),
        &body.sender,
        body.port,
        event,
    )
    .await
}

pub async fn group_roster_handler(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    client_cert: Option<Extension<net::ClientCertFingerprint>>,
    Json(body): Json<GroupRosterBody>,
) -> Result<StatusCode, (StatusCode, String)> {
    check_sender(&body.sender)?;
    let event = WsEvent::GroupRoster {
        roster: body.roster,
        sender: body.sender.clone(),
    };
    accept(
        &state,
        addr,
        &headers,
        client_fingerprint(&client_cert),
        &body.sender,
        body.port,
        event,
    )
    .await
}

/// POST /chat/group-sync —— 收到对端一个补拉请求（没有长连接时走这条路）。
#[derive(Deserialize)]
pub struct GroupSyncAskBody {
    group_id: String,
    ask_id: String,
    since_at: u64,
    #[serde(default)]
    holes: Vec<crate::group::SyncHole>,
    #[serde(default)]
    ts: u64,
    #[serde(default)]
    pubkey: String,
    #[serde(default)]
    sig: String,
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
    #[serde(default)]
    ts: u64,
    #[serde(default)]
    sig: String,
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
    // 这一条要在验签/写日志之前挡：非法 id 本身可能是几 MB，不能进签名计算和日志。
    check_sender(&body.sender)?;
    let paired = state.get_peer(&body.sender.id).is_some_and(|p| p.paired);
    if paired {
        // 已配对：token 就是身份证明（老客户端没有签名也照常放行）。
        let claimed = bearer_token(&headers).and_then(|t| peer_id_from_token(&state, &t));
        if claimed.as_deref() != Some(body.sender.id.as_str()) {
            info!(sender = %body.sender.id, "补拉请求被拒：自称已配对设备但 token 不认");
            return Err((StatusCode::UNAUTHORIZED, "身份校验失败".into()));
        }
        // 新客户端顺带带签名：把它的群公钥认证进名册，之后就算解除配对也还能用签名补拉。
        crate::group::attest_sync_ask(
            &state,
            &body.sender.id,
            &body.group_id,
            &body.ask_id,
            body.since_at,
            body.ts,
            &body.holes,
            &body.pubkey,
            &body.sig,
        );
    } else {
        // 未配对：必须由已认证的成员公钥签名。验不过就**在 upsert 之前**返回 ——
        // 否则攻击者能先把该成员的地址改到自己身上，再把历史引走。
        crate::group::verify_sync_ask(
            &state,
            &body.sender.id,
            &body.group_id,
            &body.ask_id,
            body.since_at,
            body.ts,
            &body.holes,
            &body.pubkey,
            &body.sig,
        )
        .map_err(|e| {
            // 对端会按补拉节奏反复重试：拒绝本身不是状态变化，重复刷 INFO 只会淹没真正的故障。
            debug!(sender = %body.sender.id, group = %body.group_id, reason = e.reason(), "补拉请求被拒：身份证明失败");
            (StatusCode::FORBIDDEN, format!("group sync refused: {}", e.reason()))
        })?;
        if !state
            .groups
            .note_incoming_ask(&body.sender.id, &body.ask_id)
        {
            return Err((StatusCode::FORBIDDEN, "group sync refused: replay".into()));
        }
    }
    let event = WsEvent::GroupSyncAsk {
        group_id: body.group_id,
        ask_id: body.ask_id,
        since_at: body.since_at,
        holes: body.holes,
        ts: body.ts,
        pubkey: body.pubkey,
        sig: body.sig,
    };
    // 未配对的签名请求不改写设备地址：签名只证明"它是这个成员"，不证明"这条 TCP 来源
    // 就是它此刻的可拨地址"（重放者/中转方也能替它发）。地址由发现/配对那一档维护。
    accept_with(
        &state,
        addr,
        &headers,
        None,
        &body.sender,
        body.port,
        event,
        paired,
    )
    .await
}

pub async fn group_sync_post_handler(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<GroupSyncPostBody>,
) -> Result<StatusCode, (StatusCode, String)> {
    check_sender(&body.sender)?;
    let paired = state.get_peer(&body.sender.id).is_some_and(|p| p.paired);
    if !paired {
        crate::group::verify_sync_post(
            &state,
            &body.sender.id,
            &body.group_id,
            &body.ask_id,
            body.ts,
            body.has_more,
            &body.roster,
            &body.messages,
            &body.sig,
        )
        .map_err(|e| {
            debug!(sender = %body.sender.id, group = %body.group_id, reason = e.reason(), "补拉回答被拒：身份证明失败");
            (StatusCode::FORBIDDEN, format!("group sync post refused: {}", e.reason()))
        })?;
    }
    let event = WsEvent::GroupSyncPost {
        group_id: body.group_id,
        ask_id: body.ask_id,
        roster: body.roster,
        has_more: body.has_more,
        messages: body.messages,
        ts: body.ts,
        sig: body.sig,
    };
    accept_with(
        &state,
        addr,
        &headers,
        None,
        &body.sender,
        body.port,
        event,
        paired,
    )
    .await
}

pub async fn group_ack_handler(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    client_cert: Option<Extension<net::ClientCertFingerprint>>,
    Json(body): Json<GroupAckBody>,
) -> Result<StatusCode, (StatusCode, String)> {
    check_sender(&body.sender)?;
    let event = WsEvent::GroupAck {
        group_id: body.group_id,
        up_to: body.up_to,
    };
    accept(
        &state,
        addr,
        &headers,
        client_fingerprint(&client_cert),
        &body.sender,
        body.port,
        event,
    )
    .await
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
    client_cert: Option<Extension<net::ClientCertFingerprint>>,
    Json(body): Json<GroupTextRejectBody>,
) -> Result<StatusCode, (StatusCode, String)> {
    check_sender(&body.sender)?;
    let event = WsEvent::GroupTextRejected {
        group_id: body.group_id,
        id: body.id,
        reason: body.reason,
    };
    accept(
        &state,
        addr,
        &headers,
        client_fingerprint(&client_cert),
        &body.sender,
        body.port,
        event,
    )
    .await
}

/// 认人 + 落账。两条路（消息 / 回执）共用。
async fn accept(
    state: &Arc<AppState>,
    addr: SocketAddr,
    headers: &HeaderMap,
    client_fp: Option<&str>,
    sender: &PeerInfo,
    port: u16,
    event: WsEvent,
) -> Result<StatusCode, (StatusCode, String)> {
    accept_with(state, addr, headers, client_fp, sender, port, event, true).await
}

/// `update_address` = 这条**路由**允许学习地址（补拉那两条故意关掉）。
///
/// 真正能不能写还看身份：
/// - 已配对 → token 反解出的 id 必须就是发送方（走得过这一关的才是老搭档）；
/// - 未配对、设备表里还没这一行 → 接受这次观察（首次接触只能 TOFU，否则未配对消息
///   连回执都发不回去）；
/// - 未配对、已有这一行 → 只接受**同一枚 TLS 客户端证书**带来的地址变更。证书证明
///   对面还持有同一把私钥（握手会验签）；对端换 IP 时证书不变，冒充者拿不出来。
///   旧版本客户端不带证书 / 首次见到还没学到指纹时，这条观察不写地址，同行里的旧地址
///   保持不动（消息照收）。
async fn accept_with(
    state: &Arc<AppState>,
    addr: SocketAddr,
    headers: &HeaderMap,
    client_fp: Option<&str>,
    sender: &PeerInfo,
    port: u16,
    event: WsEvent,
    update_address: bool,
) -> Result<StatusCode, (StatusCode, String)> {
    // 所有公开会话路由的最后一道同门：handler 已各自校验（尤其是验签之前那两条），
    // 这里再挡一次，保证未来新增调用点也不会把超长/非法 id 带进落账。
    check_sender(sender)?;
    // 地址簿里这台是不是"已绑定"（已配对）。
    let existing = state.get_peer(&sender.id);
    let paired = existing.as_ref().is_some_and(|p| p.paired);
    // 冒充已配对设备：拿不出 token 就拒 —— 走得过这一关的才是身份确认过的老搭档。
    if paired {
        let claimed = bearer_token(headers).and_then(|t| peer_id_from_token(state, &t));
        if claimed.as_deref() != Some(sender.id.as_str()) {
            info!(sender = %sender.id, "会话消息被拒：自称已配对设备但 token 不认");
            return Err((StatusCode::UNAUTHORIZED, "身份校验失败".into()));
        }
    } else if !allow_unpaired(addr.ip()) {
        // 未配对来源可以随便换 `sender.id` 新建会话；限速只按 TCP 来源地址，不按自报 id。
        warn!(from = %addr.ip(), "未配对会话请求过于频繁，已限流");
        return Err((StatusCode::TOO_MANY_REQUESTS, "请求过于频繁".into()));
    }

    // 未配对来源的身份连续性：这条 TLS 连接出示的客户端证书，必须是本机以前给它
    // 记下的那一枚（`observedFingerprints`）。新行没有旧地址可保护，走 TOFU。
    let identity_matches =
        !paired && existing.is_some() && state.observed_fingerprint_matches(&sender.id, client_fp);
    let addr_trusted = paired || existing.is_none() || identity_matches;
    let may_publish = update_address && addr_trusted;

    // 未绑定/没见过的发送方也要能在会话列表里露出来：登记进设备表（ip 取自 TCP 来源，
    // 不可伪造；端口用它自报的服务端口）。已经配对的那些原样保留（`upsert_peer` 只增不减）。
    //
    // 来源地址是本机自己的监听端点时（模拟器/NAT 后面的对端回连进来，见
    // `net::observed_dial_target`）、或者来源是**本机认识的另一台设备**时（这条连接是
    // 经它中转来的，F-106），**一处都不记**：前者没有可拨的地址；后者记下来就把中转方
    // 的地址当成了发送方的地址（本机以后会拿它去"拨发送方"，实际拨到中转方身上）。
    // 两种情形的可达性都只能由长连接证明（`ws::session` 在链路断开时把这种行标回离线），
    // 这里也不替它声明"在线"，否则设备行会在"离线"与"已配对"之间来回跳。
    let (ip, port, online) = if !may_publish {
        (String::new(), 0, false)
    } else {
        match net::observed_dial_target(addr.ip(), port, state.listening_port()) {
            Some((ip, port)) if !state.is_relayed_source(addr.ip(), &sender.id) => (ip, port, true),
            _ => (String::new(), 0, false),
        }
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
    // 首次接触：把这条连接出示的证书记成这台设备的身份锚。之后它换 IP 时靠这枚
    // 证书判断"回来的还是不是它"。
    if !paired && existing.is_none() {
        if let Some(fp) = client_fp {
            state.note_observed_fingerprint(&sender.id, fp);
        }
    }
    if existing.is_some() && !addr_trusted {
        debug!(
            sender = %sender.id,
            from = %addr.ip(),
            "未配对来源的地址变更未被采信：身份连续性不匹配（旧地址保持不动）"
        );
    }

    // 每一条走 HTTP 的会话消息都会到这里；消息级细节在默认日志里没有留存价值
    // （长连接那条路同样只记 `debug`）。
    debug!(sender = %sender.id, name = %sender.name, paired, addr_trusted, "inbound chat over http");
    // **群消息这一条要看落账结果**：收不下（本机拒绝过这个群 / 名册没到）就回 409 ——
    // 恒回 204 会让发送方把那一条记成「已送达」，而它实际被丢掉了（长连接那条路靠
    // `group_text_rejected` 反向纠正，HTTP 这一跳就靠这个状态码）。理由代码同时带上：
    // 日志里要能看出"是哪种收不下"。
    match crate::ws::session::handle_inbound(state, &sender.id, event).await {
        crate::ws::session::InboundResult::Accepted => Ok(StatusCode::NO_CONTENT),
        crate::ws::session::InboundResult::GroupRefused(reason) => Err((
            StatusCode::CONFLICT,
            format!("group text refused: {reason}"),
        )),
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
    use crate::chat::Delivery;

    /// 这条路收到的体就是"那条 WS 事件 + `port` / `sender`"（见 `chat::deliver_http`）。
    /// 字段名必须与线上形状逐字对上 —— 对不上是 422，发送方那边只看到"没打通"。
    #[test]
    fn body_shapes_match_the_wire_event() {
        let mut text = serde_json::to_value(WsEvent::ChatText {
            id: "m1".into(),
            text: "在吗".into(),
            at: 42,
            from_name: "PC-B".into(),
        })
        .unwrap();
        text["port"] = serde_json::json!(3890);
        text["sender"] = serde_json::json!({ "id": "p1", "name": "PC-B" });
        let parsed: ChatMessageBody =
            serde_json::from_value(text).expect("消息体的字段名必须对得上");
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
        let sender = PeerInfo {
            id: "p1".into(),
            name: "PC-B".into(),
        };
        // 中转过来的那一条最容易写错：**作者**是 A，**信封上的发送方**是搬运工 B ——
        // HTTP 体上这两个字段必须各是各的（同名会被信封覆盖掉，作者就变成搬运工）。
        let mut text = serde_json::to_value(WsEvent::GroupText {
            group_id: "g1".into(),
            id: "m1".into(),
            text: "群里说一句".into(),
            at: 42,
            seq: 3,
            rev: 7,
            author: PeerInfo {
                id: "p0".into(),
                name: "PC-A".into(),
            },
            hops: 1,
            reached: vec!["p0".into()],
            image: true,
            name: "图片-1.png".into(),
            size: 7,
            data: "REFUQQ==".into(),
            epoch: 1,
            pubkey: "pk".into(),
            sig: "sig".into(),
        })
        .unwrap();
        text["port"] = serde_json::json!(3890);
        text["sender"] = serde_json::json!({ "id": "p1", "name": "PC-B" });
        let parsed: GroupTextBody =
            serde_json::from_value(text).expect("群消息体的字段名必须对得上");
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

        let mut read = serde_json::to_value(WsEvent::GroupRead {
            group_id: "g1".into(),
            up_to: 42,
        })
        .unwrap();
        read["port"] = serde_json::json!(3890);
        read["sender"] = serde_json::json!({ "id": "p1", "name": "PC-B" });
        let parsed: GroupReadBody =
            serde_json::from_value(read).expect("群回执体的字段名必须对得上");
        assert_eq!((parsed.group_id.as_str(), parsed.up_to), ("g1", 42));

        let mut roster = serde_json::to_value(WsEvent::GroupRoster {
            roster: crate::group::RosterPayload {
                id: "g1".into(),
                name: "群里".into(),
                rev: 2,
                updated_by: "p1".into(),
                updated_at: 9,
                members: vec![crate::group::Member {
                    id: sender.id.clone(),
                    name: sender.name.clone(),
                    pubkey: String::new(),
                }],
                epoch: 1,
                sig: String::new(),
            },
            sender: sender.clone(),
        })
        .unwrap();
        roster["port"] = serde_json::json!(3890);
        roster["sender"] = serde_json::json!({ "id": "p1", "name": "PC-B" });
        let parsed: GroupRosterBody =
            serde_json::from_value(roster).expect("名册体的字段名必须对得上");
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
        let parsed: GroupTextRejectBody =
            serde_json::from_value(reject).expect("拒收体的字段名必须对得上");
        assert_eq!((parsed.group_id.as_str(), parsed.id.as_str()), ("g1", "m1"));
        assert_eq!(parsed.reason, crate::group::REASON_DECLINED);
    }

    /// 未配对、无签名的补拉请求：**在 upsert 之前**就拒绝，设备的已知地址不许被这条
    /// TCP 来源改写。这就是"冒充成员 id 把历史引到自己身上"那条攻击链的第一环。
    #[tokio::test]
    async fn an_unproven_sync_ask_is_rejected_without_moving_the_address() {
        let dir = std::env::temp_dir().join(format!("orbdrop_sync_gate_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let state = Arc::new(AppState::new(
            crate::config::test_config(),
            dir.join("config.toml"),
        ));
        let own = state.config.lock().device_id.clone();
        let roster = crate::group::RosterPayload {
            id: "g1".into(),
            name: "群".into(),
            rev: 1,
            updated_by: "a".into(),
            updated_at: 1,
            members: vec![
                crate::group::Member {
                    id: "a".into(),
                    name: "PC-A".into(),
                    pubkey: String::new(),
                },
                crate::group::Member {
                    id: own.clone(),
                    name: "本机".into(),
                    pubkey: String::new(),
                },
            ],
            epoch: 1,
            sig: String::new(),
        };
        assert_eq!(
            state.groups.apply_roster(
                &own,
                &roster,
                &PeerInfo {
                    id: "a".into(),
                    name: "PC-A".into()
                },
            ),
            crate::group::RosterOutcome::Pending
        );
        assert!(state.groups.accept_pending("g1", &own));
        state.upsert_peer(&PeerDevice {
            id: "a".into(),
            name: "PC-A".into(),
            ip: "10.0.0.5".into(),
            port: 4000,
            paired: false,
            ..Default::default()
        });

        let body = GroupSyncAskBody {
            group_id: "g1".into(),
            ask_id: "ask-1".into(),
            since_at: 0,
            holes: Vec::new(),
            ts: 0,
            pubkey: String::new(),
            sig: String::new(),
            port: 5000,
            sender: PeerInfo {
                id: "a".into(),
                name: "PC-A".into(),
            },
        };
        let addr: SocketAddr = "127.0.0.1:5555".parse().unwrap();
        let refused = group_sync_ask_handler(
            State(state.clone()),
            ConnectInfo(addr),
            HeaderMap::new(),
            Json(body),
        )
        .await
        .expect_err("未证明的补拉请求必须拒绝");
        assert_eq!(refused.0, StatusCode::FORBIDDEN);
        let row = state.get_peer("a").expect("设备行还在");
        assert_eq!(
            (row.ip.as_str(), row.port),
            ("10.0.0.5", 4000),
            "被拒的请求不许把成员地址改到 TCP 来源上"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 非法/超长的自报 id 在进入验签与落账之前就被拒：它不能被当成会话键或 `read_by` 的键落盘。
    #[tokio::test]
    async fn an_invalid_sender_id_is_rejected_before_any_state_change() {
        let dir = std::env::temp_dir().join(format!("orbdrop_bad_sender_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let state = Arc::new(AppState::new(
            crate::config::test_config(),
            dir.join("config.toml"),
        ));
        let own = state.config.lock().device_id.clone();
        let roster = crate::group::RosterPayload {
            id: "g1".into(),
            name: "群".into(),
            rev: 1,
            updated_by: "a".into(),
            updated_at: 1,
            members: vec![
                crate::group::Member {
                    id: "a".into(),
                    name: "PC-A".into(),
                    pubkey: String::new(),
                },
                crate::group::Member {
                    id: own.clone(),
                    name: "本机".into(),
                    pubkey: String::new(),
                },
            ],
            epoch: 1,
            sig: String::new(),
        };
        assert_eq!(
            state.groups.apply_roster(
                &own,
                &roster,
                &PeerInfo {
                    id: "a".into(),
                    name: "PC-A".into(),
                },
            ),
            crate::group::RosterOutcome::Pending
        );
        assert!(state.groups.accept_pending("g1", &own));
        state.groups.record_outbound(
            "g1",
            "m1",
            &own,
            "你好",
            100,
            &[(String::from("a"), Delivery::Queued)],
        );

        let body = GroupReadBody {
            group_id: "g1".into(),
            up_to: u64::MAX,
            port: 5000,
            sender: PeerInfo {
                id: format!("bad id {}", "x".repeat(1024)),
                name: "bad".into(),
            },
        };
        let addr: SocketAddr = "127.0.0.1:5555".parse().unwrap();
        let refused = group_read_handler(
            State(state.clone()),
            ConnectInfo(addr),
            HeaderMap::new(),
            None,
            Json(body),
        )
        .await
        .expect_err("非法 sender.id 必须拒绝");
        assert_eq!(refused.0, StatusCode::BAD_REQUEST);
        let view = state.groups.view("g1", &own).unwrap();
        assert_eq!(view.messages[0].read_count, 0, "非法 id 不许改任何状态");
        assert_eq!(view.messages[0].delivery.get("a"), Some(&Delivery::Queued));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 真 HTTP 路由上的同一条：未证明的 `/chat/group-sync` 必须在落地址之前被 403 掉。
    #[tokio::test]
    async fn the_http_route_refuses_an_unproven_sync_ask() {
        let dir = std::env::temp_dir().join(format!("orbdrop_sync_http_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let state = Arc::new(AppState::new(
            crate::config::test_config(),
            dir.join("config.toml"),
        ));
        let own = state.config.lock().device_id.clone();
        let roster = crate::group::RosterPayload {
            id: "g1".into(),
            name: "群".into(),
            rev: 1,
            updated_by: "a".into(),
            updated_at: 1,
            members: vec![
                crate::group::Member {
                    id: "a".into(),
                    name: "PC-A".into(),
                    pubkey: String::new(),
                },
                crate::group::Member {
                    id: own.clone(),
                    name: "本机".into(),
                    pubkey: String::new(),
                },
            ],
            epoch: 1,
            sig: String::new(),
        };
        assert_eq!(
            state.groups.apply_roster(
                &own,
                &roster,
                &PeerInfo {
                    id: "a".into(),
                    name: "PC-A".into()
                },
            ),
            crate::group::RosterOutcome::Pending
        );
        assert!(state.groups.accept_pending("g1", &own));
        state.upsert_peer(&PeerDevice {
            id: "a".into(),
            name: "PC-A".into(),
            ip: "10.0.0.5".into(),
            port: 4000,
            paired: false,
            ..Default::default()
        });

        let port = {
            let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("bind probe");
            probe.local_addr().expect("probe addr").port()
        };
        let port = crate::server::start_server_loopback(state.clone(), port)
            .await
            .expect("server start");
        let body = serde_json::json!({
            "group_id": "g1",
            "ask_id": "ask-http-1",
            "since_at": 0,
            "holes": [],
            "ts": 0,
            "pubkey": "",
            "sig": "",
            "port": 5000,
            "sender": { "id": "a", "name": "PC-A" },
        });
        let resp = reqwest::Client::new()
            .post(format!("http://127.0.0.1:{port}/chat/group-sync"))
            .json(&body)
            .send()
            .await
            .expect("request");
        assert_eq!(resp.status(), reqwest::StatusCode::FORBIDDEN);
        let row = state.get_peer("a").expect("设备行还在");
        assert_eq!((row.ip.as_str(), row.port), ("10.0.0.5", 4000));
        crate::server::stop_server(&state).await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 真 HTTP 路由上的正例：名册里已绑定成员公钥的签名 ask 放行，且未配对来源
    /// 仍然不改写设备地址。
    #[tokio::test]
    async fn the_http_route_accepts_a_signed_sync_ask_without_moving_the_address() {
        use base64::Engine as _;
        use ring::rand::SystemRandom;
        use ring::signature::{Ed25519KeyPair, KeyPair};

        let dir = std::env::temp_dir().join(format!("orbdrop_sync_ok_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let state = Arc::new(AppState::new(
            crate::config::test_config(),
            dir.join("config.toml"),
        ));
        let own = state.config.lock().device_id.clone();
        let roster = crate::group::RosterPayload {
            id: "g1".into(),
            name: "群".into(),
            rev: 1,
            updated_by: "a".into(),
            updated_at: 1,
            members: vec![
                crate::group::Member {
                    id: "a".into(),
                    name: "PC-A".into(),
                    pubkey: String::new(),
                },
                crate::group::Member {
                    id: own.clone(),
                    name: "本机".into(),
                    pubkey: String::new(),
                },
            ],
            epoch: 1,
            sig: String::new(),
        };
        assert_eq!(
            state.groups.apply_roster(
                &own,
                &roster,
                &PeerInfo {
                    id: "a".into(),
                    name: "PC-A".into()
                },
            ),
            crate::group::RosterOutcome::Pending
        );
        assert!(state.groups.accept_pending("g1", &own));
        state.upsert_peer(&PeerDevice {
            id: "a".into(),
            name: "PC-A".into(),
            ip: "10.0.0.5".into(),
            port: 4000,
            paired: false,
            ..Default::default()
        });

        let rng = SystemRandom::new();
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        let pair = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        let pubkey = base64::engine::general_purpose::STANDARD.encode(pair.public_key().as_ref());
        assert!(state
            .groups
            .bind_member_key("g1", &own, "a", &pubkey, false)
            .is_some());
        let ts = crate::chat::now_ms();
        let holes: Vec<crate::group::SyncHole> = Vec::new();
        let sig = base64::engine::general_purpose::STANDARD.encode(
            pair.sign(&crate::group::sync_ask_signing_bytes(
                "g1", "ask-ok-1", 0, ts, "a", &pubkey, &holes,
            ))
            .as_ref(),
        );

        let port = {
            let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("bind probe");
            probe.local_addr().expect("probe addr").port()
        };
        let port = crate::server::start_server_loopback(state.clone(), port)
            .await
            .expect("server start");
        let body = serde_json::json!({
            "group_id": "g1",
            "ask_id": "ask-ok-1",
            "since_at": 0,
            "holes": [],
            "ts": ts,
            "pubkey": pubkey,
            "sig": sig,
            "port": 5000,
            "sender": { "id": "a", "name": "PC-A" },
        });
        let resp = reqwest::Client::new()
            .post(format!("http://127.0.0.1:{port}/chat/group-sync"))
            .json(&body)
            .send()
            .await
            .expect("request");
        assert_eq!(resp.status(), reqwest::StatusCode::NO_CONTENT);
        let row = state.get_peer("a").expect("设备行还在");
        assert_eq!(
            (row.ip.as_str(), row.port),
            ("10.0.0.5", 4000),
            "未配对签名请求也不改写地址"
        );
        crate::server::stop_server(&state).await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 一条未配对的入站消息：证书指纹由调用方指定，来源地址由调用方指定。
    async fn inbound_chat(
        state: &Arc<AppState>,
        from: &str,
        cert_fp: &str,
        sender_id: &str,
        msg_id: &str,
    ) -> StatusCode {
        let cert = Some(Extension(net::ClientCertFingerprint(Some(
            cert_fp.to_string(),
        ))));
        let body = ChatMessageBody {
            id: msg_id.into(),
            text: "你好".into(),
            at: 1,
            port: 5000,
            sender: PeerInfo {
                id: sender_id.into(),
                name: "PC-A".into(),
            },
        };
        chat_message_handler(
            State(state.clone()),
            ConnectInfo(format!("{from}:5555").parse().unwrap()),
            HeaderMap::new(),
            cert,
            Json(body),
        )
        .await
        .expect("未配对会话消息应当收下")
    }

    /// 未配对的地址变更必须是**同一枚客户端证书**带来的：换一张证书就不改地址
    /// （消息照收），换回原证书才跟着走。这就是"手动添加的换 IP 自动纠正"的安全版本。
    #[tokio::test]
    async fn unpaired_address_change_requires_the_same_client_certificate() {
        let dir = std::env::temp_dir().join(format!("orbdrop_addr_gate_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let state = Arc::new(AppState::new(
            crate::config::test_config(),
            dir.join("config.toml"),
        ));
        let known = "aa".repeat(32);
        let other = "bb".repeat(32);
        state.upsert_peer(&PeerDevice {
            id: "a".into(),
            name: "PC-A".into(),
            ip: "10.0.0.5".into(),
            port: 4000,
            paired: false,
            ..Default::default()
        });
        assert!(state.note_observed_fingerprint("a", &known));

        inbound_chat(&state, "10.0.0.9", &other, "a", "m1").await;
        let row = state.get_peer("a").expect("设备行还在");
        assert_eq!(
            (row.ip.as_str(), row.port),
            ("10.0.0.5", 4000),
            "不同证书不许改已有行的地址"
        );

        inbound_chat(&state, "10.0.0.10", &known, "a", "m2").await;
        let row = state.get_peer("a").expect("设备行还在");
        assert_eq!(
            (row.ip.as_str(), row.port),
            ("10.0.0.10", 5000),
            "同一枚证书换 IP 要自动跟着走"
        );
        assert!(row.online);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 首次接触没有旧地址可保护：接受这次观察，并把客户端证书钉成以后换 IP 的锚。
    #[tokio::test]
    async fn first_contact_pins_the_client_certificate_for_later_moves() {
        let dir = std::env::temp_dir().join(format!("orbdrop_addr_pin_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let state = Arc::new(AppState::new(
            crate::config::test_config(),
            dir.join("config.toml"),
        ));
        let fp = "cc".repeat(32);
        inbound_chat(&state, "10.0.0.9", &fp, "new-peer", "m1").await;

        assert_eq!(
            state.observed_fingerprint("new-peer").as_deref(),
            Some(fp.as_str()),
            "第一次来的证书要记下来（以后换 IP 靠它认人）"
        );
        let row = state.get_peer("new-peer").expect("设备行已建");
        assert_eq!((row.ip.as_str(), row.port), ("10.0.0.9", 5000));

        let _ = std::fs::remove_dir_all(&dir);
    }
}
