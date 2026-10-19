//! `POST /relay/open`（F-106 / S-09）与 `POST /relay/route`（F-107）。
//!
//! **谁可以借用中转**（S-09 的放宽，F-107 §12.1）：中转的机制全在 `crate::relay`；这里只回答
//! "谁在请求"。两档：
//!
//! - **已配对对端**：带 `Bearer <token>`，token 反解出的 id 就是它的身份（最稳，也是 F-106
//!   原来的唯一一档）。
//! - **未配对的局域网设备**：不带凭据，但必须在请求体里**自报** `requester.id` + 自己的监听
//!   端口；本机用「TCP 来源地址 + 那个端口」去问一次 `/pair/info`，自报的 id 对得上才算数。
//!   少了这一关，"开着开关"就等于"给整个局域网开隧道 + 谁都能冒名"。
//!
//! 因此这条路由**不在鉴权中间件后面**（未配对设备本来就没有 token 可带），
//! 认人规则写在 handler 里 —— 与 `/push/invite`、`/chat/message` 同一条路数。
//!
//! 拒绝的情形与状态码：开关关着 403、目标不合格 404、身份核实不过 403、
//! 没有身份可认 401、限流/上限 429、本机开不出来 503。
//!
//! **`POST /relay/route`（F-107 §10）另挂鉴权路由**：反向路由记录只接受**已配对**对端写的
//! （记录会影响本机的选路，必须有一个可问责的来源）。

use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{ConnectInfo, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use tracing::{debug, info};

use crate::state::AppState;

/// 请求方自报的身份（F-107）：未配对设备借中转时的唯一凭据 —— 本机会去核实。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RelayRequester {
    /// 请求方的 device id。
    pub id: String,
    /// 请求方**实际监听**的端口（本机去 `来源地址:这个端口` 问 `/pair/info` 核实）。
    #[serde(default)]
    pub port: u16,
    /// 请求方自报的**设备名**。
    ///
    /// 只用来**补名字**：核实成功时以**问出来的那个名字**为准（那才是核实过的，直接更新
    /// 设备行）；这一份只在"核实不过、却确实是经它自己点名的中转方送达"那一档用得上
    /// （`carried_by_named_relay`），那一档没核实过，所以只填占位行（`fill_peer_name`）。
    /// 以前这条报文只带 id + 端口，本机建出来的行名字就是那串 device id —— 表现就是
    /// "设备列表/会话里先显示一串 deviceid"。
    #[serde(default)]
    pub name: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RelayOpenRequest {
    /// 要连的**目标**设备 id（不是请求方自己）。
    pub target: String,
    /// 请求方给出的目标地址（`host:port`）。只在中转方**不认识**目标、或目标在它视野里
    /// 不在线时用；用之前中转方会到那个地址问一次身份，确认自报的 id 就是 `target`。
    /// 跨网段的中转方常常"够得着目标却从没认识过它"（mDNS 跨不过路由），留这一手。
    #[serde(default)]
    pub addr: Option<String>,
    /// 请求方自报的身份（F-107）。带凭据时以凭据为准，这一份只在没有凭据时用。
    #[serde(default)]
    pub requester: Option<RelayRequester>,
}

/// POST /relay/open — 请本机开一条「请求方 → 本机 → 目标」的中转端点。
///
/// 回话里的 `host`/`port` 就是请求方以后拨目标地址时该用的地址（它拿这台设备当"够得着
/// 目标的那条路"）。`host` 是 IPv4 映射的 IPv6 字面量，理由见 `net::relay_host`。
pub async fn relay_open(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: axum::http::HeaderMap,
    Json(req): Json<RelayOpenRequest>,
) -> Response {
    // ── 谁在请求：凭据优先，其次"自报 + 核实"（F-107 §12.1）───────────────────
    let bearer = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .map(str::to_string);
    let (requester, trusted_caller) = match bearer
        .as_deref()
        .and_then(|t| crate::ws::session::peer_id_from_token(&state, t))
    {
        // 凭据反解出的身份：可信（端点的来源白名单也认它在地址簿里的地址）。
        Some(id) => (id, true),
        None => {
            let Some(claim) = req.requester.as_ref() else {
                return (
                    StatusCode::UNAUTHORIZED,
                    "中转只对已配对对端开放；未配对设备请在请求里自报身份（id + 监听端口）",
                )
                    .into_response();
            };
            if claim.id.trim().is_empty() || claim.port == 0 {
                return (
                    StatusCode::UNAUTHORIZED,
                    "自报身份不完整（要 device id + 监听端口）",
                )
                    .into_response();
            }
            // 「来源地址 + 自报端口」上问一次"你是谁"：自报的 id 必须对得上。
            // 这是放宽之后唯一挡住"冒名"的一关 —— 局域网里任何设备都能声称自己是别人，
            // 但只有**真的在那个地址上开着那个端口**的那一台能答出那个 id。
            //
            // ⚠️ 它能证明的只有"这个地址上有个会回话、且会回这个 id 的东西"：`/pair/info`
            // 本身是公开的自报端点，所以这个 id 拿来做限流键或界面标签都不够 —— 限流按
            // **来源地址**记（见 `relay::open`），端点来源白名单也只认来源地址（`trusted_caller`
            // = false）。失败细节只进日志，不回给调用方。
            let at = SocketAddr::new(addr.ip(), claim.port);
            let observed = match crate::relay::verify_identity_at(at, claim.id.trim()).await {
                Ok(observed) => observed,
                Err(why) => {
                    debug!(%at, claimed = %claim.id, "中转请求方身份核实不过: {why}");
                    return (
                        StatusCode::FORBIDDEN,
                        "请求方身份核实不过（自报的 id 与那个地址上问出来的不一致）",
                    )
                        .into_response();
                }
            };
            // 刚才这一问**证明了这个地址上答得出这个 id**：记下来（只补空行）。
            // 为什么重要：请求方往往正是"本机从别处只知其名、没有地址"的那一台 ——
            // 记下来之后，它下一次来请中转时就不必再靠 hint（真机上"本机不认识设备 X"
            // 就是这条缺失造成的）。
            state.observe_verified_addr(claim.id.trim(), &addr.ip().to_string(), claim.port);
            // 同一问还证明了两件事，一并收下：
            // ① **它此刻活着**（与 5s 探活同一级的证据，见 `AppState::heard`）；
            // ② 它叫什么 —— **问出来的那份是核实过的**（这个地址上答得出这个 id）→ 直接更新
            //    （`note_peer_name`：对端改名跟着走）；它没报名字时才退回自报的那份，那一份
            //    没核实过，只填占位行（`fill_peer_name`）。
            state.note_heard_online(claim.id.trim());
            match observed {
                Some(name) => state.note_peer_name(claim.id.trim(), &name),
                None => state.fill_peer_name(claim.id.trim(), claim.name.trim()),
            }
            (claim.id.trim().to_string(), false)
        }
    };

    let target = req.target.trim().to_string();
    // `host:port` → (host, port)。不合法就当没给（后面那条"不认识目标"的错误更直白）。
    let hint = req.addr.as_deref().and_then(|s| {
        let (host, port) = s.rsplit_once(':')?;
        Some((host.trim().to_string(), port.trim().parse::<u16>().ok()?))
    });
    match crate::relay::open(&state, &requester, addr.ip(), &target, hint, trusted_caller).await {
        Ok(opened) => {
            debug!(
                caller = %requester,
                from = %addr.ip(),
                trusted = trusted_caller,
                target = %target,
                port = opened.port,
                "中转端点已开"
            );
            Json(opened).into_response()
        }
        Err(e) => (e.status(), e.message().to_string()).into_response(),
    }
}

/// `POST /relay/route` 的请求体（F-107 §10.2）。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RouteRequest {
    pub routes: Vec<crate::relay::RouteRecord>,
    /// 未配对调用方的自报身份（与 `/relay/open` 同一套：本机会到「来源地址 + 这个端口」核实）。
    #[serde(default)]
    pub requester: Option<RelayRequester>,
}

/// POST /relay/route — 对端把「这一段设备怎么够得着」告诉本机（F-107 §10）。
///
/// 挂在**公开路由**上，认人规则写在 handler 里（与 `/relay/open` 逐字同一套）：
/// 凭据优先；没有凭据就必须自报 `id` + 监听端口，并由本机到「TCP 来源地址 + 那个端口」
/// 问一次 `/pair/info` 核实。**为什么不再要求配对**：记录要送达的那一侧（被中转的 B）
/// 通常从来没跟锚点（C）配过 —— 而"B 也能知道 A 是怎么来的"正是这条功能的用途。
/// 代价与收口：记录只是**提示**（`apply_relay_routes` 里限了条数、只并新 id、有 TTL，
/// 用的时候目标身份仍由中转方核实），说谎的一方最多让本机白试一次。
/// 经中转送来的一档另有一条判据（见 [`carried_by_named_relay`]）。
pub async fn relay_route(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: axum::http::HeaderMap,
    Json(req): Json<RouteRequest>,
) -> Response {
    // 谁在写：凭据优先（这条路由在公开面上，认人规则自己来），没有凭据就按自报核实。
    let bearer = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .map(str::to_string);
    let caller_id = match bearer
        .as_deref()
        .and_then(|t| crate::ws::session::peer_id_from_token(&state, t))
    {
        Some(id) => id,
        None => {
            let Some(claim) = req.requester.as_ref() else {
                return (
                    StatusCode::UNAUTHORIZED,
                    "要么带凭据，要么在请求里自报身份（id + 监听端口）",
                )
                    .into_response();
            };
            if claim.id.trim().is_empty() || claim.port == 0 {
                return (
                    StatusCode::UNAUTHORIZED,
                    "自报身份不完整（要 device id + 监听端口）",
                )
                    .into_response();
            }
            let at = SocketAddr::new(addr.ip(), claim.port);
            let verified = match crate::relay::verify_identity_at(at, claim.id.trim()).await {
                Ok(observed) => Some(observed),
                Err(why) => {
                    // **经中转过来的那一侧**：它自己够不着本机（AP 隔离那类），上面这一问必然
                    // 问不出它的身份 —— 来源地址是**中转方**，不是它。判据换成"这条连接是从
                    // **记录自己点名的中转方**那里转发来的"：TCP 来源地址伪造不了，而记录只是
                    // 提示（目标身份在借中转那一跳由中转方到地址上核实，见 `relay::open`）。
                    //
                    // 少了这一条，F-107 §10 的反向记录**恰恰在它唯一有用的场合**送不到：A 经
                    // C 到 B，A 想把自己那一段的路由告诉 B，而这一推只能经 C（A 够不着 B）——
                    // 被这一关 403 掉，B 于是永远不知道该经谁回去。"单向通、反向不通"就是这么来的。
                    if !carried_by_named_relay(addr.ip(), &req.routes) {
                        debug!(%at, claimed = %claim.id, "路由记录：调用方身份核实不过: {why}");
                        return (
                            StatusCode::FORBIDDEN,
                            "调用方身份核实不过（自报的 id 与那个地址上问出来的不一致）",
                        )
                            .into_response();
                    }
                    info!(
                        claimed = %claim.id,
                        from = %addr.ip(),
                        "路由记录经它自己点名的中转方送达（自报那一关问不到中转方背后的那一台）"
                    );
                    None
                }
            };
            // 名字这一格与"它在不在网上"分开看：
            // - **核实成功**：问出来的那份名字是核实过的 → 直接更新（`note_peer_name`）；
            //   它没报名字才退回自报那份（那一份没核实，只填占位行）；并且这一问本身
            //   就证明它活着 → 记进"刚听到过"（见 `AppState::heard`）。
            // - **核实不过**（经中转方送达那一档）：只能采信自报的名字，与未配对会话信封上的
            //   名字同一档信任（**只填占位行**）；**不声明在线** —— 这条 TCP 来源是**中转方**，
            //   证明不了它活着。
            if let Some(observed) = verified {
                state.observe_verified_addr(claim.id.trim(), &addr.ip().to_string(), claim.port);
                state.note_heard_online(claim.id.trim());
                match observed {
                    Some(name) => state.note_peer_name(claim.id.trim(), &name),
                    None => state.fill_peer_name(claim.id.trim(), claim.name.trim()),
                }
            } else {
                state.fill_peer_name(claim.id.trim(), claim.name.trim());
            }
            claim.id.trim().to_string()
        }
    };
    let (accepted, ignored) = state.apply_relay_routes(&caller_id, &req.routes);
    // `debug` 而不是 `info`：这条路由是**事件驱动**的（每次路由记忆有变化就推一拍，最短 30s
    // 一次），而绝大多数请求什么都没被采纳（`accepted: 0`，本机不是那个目标段）。按 `info`
    // 记就是每半分钟一条"什么都没发生"，实测一天 1.4 兆。**真正被采纳的那一刻**由
    // `AppState::apply_relay_routes` 记一条 INFO（那条只在 `accepted` 非空时才走），
    // 事件本身一个字都没漏，只是不再复述。
    debug!(
        caller = %caller_id,
        from = %addr.ip(),
        accepted = accepted.len(),
        ignored = ignored.len(),
        "收到反向中转路由记录"
    );
    Json(serde_json::json!({ "accepted": accepted, "ignored": ignored })).into_response()
}

/// 这条连接是不是从**记录自己点名的中转方**那里转发来的？
///
/// 判据只有一条：TCP 来源地址等于记录里某一台中转设备自报的地址（`via_devices[*].ip`）。
/// 为什么只认这一条：被中转的那一侧（F-107 §10.3 的 B）自己够不着本机 —— 它要是够得着，
/// 就直接自己把记录送过来了，用不着借中转。来源地址是这一路上唯一伪造不了的东西。
///
/// **它证明不了"发送方就是自报的那个 id"**：经中转的连接里本机看不到发送方的地址，而中转方
/// 与请求方之间的关系本机无从核实。代价与收口和这条路由一直以来的口径一致 —— 记录只是
/// **提示**：采纳前 `apply_relay_routes` 仍要求目标段含本机地址、条数与地址合法；用的时候
/// 目标身份由中转方到地址上核实，与"谁写的这条记录"无关。少了它，反向记录恰恰在它唯一
/// 有用的场合送不到（A 经 C 到 B，A 想把自己那一段的路由告诉 B，只能经 C）。
fn carried_by_named_relay(source: std::net::IpAddr, routes: &[crate::relay::RouteRecord]) -> bool {
    routes.iter().any(|r| {
        r.via_devices
            .iter()
            .any(|d| crate::net::parse_host(&d.ip).is_some_and(|ip| ip == source))
    })
}