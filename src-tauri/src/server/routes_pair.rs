use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{ConnectInfo, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::credential;
use crate::net;
use crate::pair;
use crate::state::AppState;
use crate::ws;
use crate::ws::events::ui;

/// GET /pair/info — public endpoint for manual-add probe.
///
/// Reports the port we are **actually listening on**: the server falls forward to
/// port+1..port+10 when the configured port is taken, and a caller that stores the
/// configured value would dial the wrong process (or nothing) forever.
pub async fn pair_info(
    State(state): State<Arc<AppState>>,
    headers: axum::http::HeaderMap,
) -> Json<serde_json::Value> {
    // 「你把带这个 token 的调用方认作谁」：对端换过 device id（数据目录被清、凭据库
    // 还在）之后，这是对端唯一能确认"配对关系还活着"的地方 —— 凭据本身不回显，只回
    // 它反解出的 id。
    //
    // 必须在锁 config 之前算：`peer_id_from_token` 自己也要读 config，parking_lot 的
    // Mutex 不可重入。
    let paired_as = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .and_then(|h| h.strip_prefix("Bearer "))
        .and_then(|t| crate::ws::session::peer_id_from_token(&state, t));

    let port = state.listening_port();
    let config = state.config.lock();
    serde_json::json!({
        "id": config.device_id,
        "name": config.device_name,
        "port": port,
        "ver": 1,
        "pairedAs": paired_as,
        // 中转能力位（F-106）：本机此刻是否愿意替别人搬字节（设置里 S-09，默认关）。
        // 这是个**能力**不是**关系**：回话本来就不带鉴权，谁问都答；发出去的只有"愿不愿意"，
        // 不含任何一台设备的信息。请求方据此挑中转方（还要它自己在线、已配对）。
        "relay": config.allow_relay,
        // F-107：本机**自报**的网段（`192.168.1.0/24,10.0.0.0/8`）。跨网段时"这台设备属于
        // 哪一段"只能由它自己说 —— 本机的掩码对远端网段不适用。与 mDNS TXT 的 `nets` 同义，
        // 但这是**活回话**（网卡换了当场就变），TXT 只是注册那一刻的快照。
        "nets": crate::subnet::format_list(&crate::subnet::host_nets()),
    }).into()
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PairRequest {
    /// Confirming device's id.
    pub peer_id: Option<String>,
    /// Confirming device's display name.
    pub peer_name: Option<String>,
    /// Confirming device's own HTTP port — the dial target for the side that
    /// owns the long connection (smaller device id), which may never have seen
    /// this peer through mDNS.
    pub peer_port: Option<u16>,
    /// The 6-digit code this machine issued (F-12).
    pub code: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PairResponse {
    pub token: String,
    pub peer_id: String,
    pub peer_name: String,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UnpairRequest {
    /// 要解除配对的对象。远端对端由 token 反解身份、不读这个字段；
    /// 本机（回环）调用没有 token 可解，只能显式给出（与回环本来就免鉴权一致）。
    pub peer_id: Option<String>,
}

/// POST /unpair — 解除配对的另一半（F-14）。
///
/// 对端用「解除配对」时它手里还有**本机给它的** token（它随后才删），所以这里带的是
/// 那个 token（Bearer），身份由 auth 中间件解析成 `Caller::Peer(id)`；同机回环调用
/// 会走 `Caller::Local` 免鉴权分支，此时从 body 取 peerId。
///
/// 只清 token 与配对状态、**保留设备行**：本端仍在网络上看得见对方，就应该继续显示为
/// "未绑定"（离线时才从列表消失）。
pub async fn unpair(
    State(state): State<Arc<AppState>>,
    axum::Extension(caller): axum::Extension<crate::server::auth::Caller>,
    body: Option<Json<UnpairRequest>>,
) -> Result<StatusCode, (StatusCode, String)> {
    let peer_id = match caller {
        crate::server::auth::Caller::Peer(id) => id,
        crate::server::auth::Caller::Local => body
            .and_then(|Json(req)| req.peer_id)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| (StatusCode::BAD_REQUEST, "本机调用需要给出 peerId".to_string()))?,
    };

    if let Err(e) = credential::remove_peer(&peer_id) {
        return Err((StatusCode::INTERNAL_SERVER_ERROR, format!("凭据清理失败: {e}")));
    }
    state.ws.drop_peer(&peer_id);
    state.unpair(&peer_id);

    // 同一件收尾：BC 里那条指向对方的 profile 已随 token 作废（只删我们自己写下的条目）。
    crate::bc::provision_profiles(&state);

    ws::emit_ui(&state, ui::DEVICES_CHANGED, &serde_json::json!({}));
    info!(%peer_id, "peer unpaired us via HTTP");
    Ok(StatusCode::NO_CONTENT)
}

/// POST /pair — verifying half of the pairing handshake (F-12).
///
/// The peer that initiated pairing issued its 6-digit code locally (`pair_start`)
/// and displays it; whoever read that code off the peer's screen posts it here.
/// On a match: mint a 32-byte hex token, store it under the caller's device id,
/// mark the caller paired, and return the token plus our own identity. Codes are
/// single-use and expire 2 minutes after issue.
pub async fn pair_handshake(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    Json(req): Json<PairRequest>,
) -> Result<Json<PairResponse>, (StatusCode, String)> {
    let peer_id = req.peer_id.as_deref().unwrap_or_default().to_string();
    if peer_id.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "缺少 peerId".into()));
    }
    // 猜码限流（P1-5）：6 位码 + 2 分钟有效期，不限次数时局域网里每秒几千发就能扫完，
    // 用户正把码显示在屏幕上时被陌生设备配上。按**来源地址**记账（TCP 来源不可伪造），
    // 一分钟内错满 `CODE_FAILURE_LIMIT` 次就先拒一段时间；猜对则把账清掉。
    let from = addr.ip();
    if !state.pair.code_attempt_allowed(from) {
        warn!(%peer_id, %from, "配对码尝试次数超限，暂时拒绝");
        return Err((
            StatusCode::TOO_MANY_REQUESTS,
            "配对码尝试次数过多，请稍后再试".into(),
        ));
    }
    if !state.pair.verify_code(&req.code) {
        state.pair.note_code_failure(from);
        warn!(%peer_id, "pairing code rejected (unknown or expired)");
        return Err((StatusCode::UNAUTHORIZED, "配对码错误或已过期".into()));
    }
    state.pair.clear_code_failures(from);

    let token = pair::generate_token();

    // The caller's address: TCP source address plus the port it reports for its
    // own server. This side may be the one that owes the outbound link (smaller
    // device id) and it has usually never seen the caller over mDNS — without
    // the address the link could only ever be dialled from a later discovery.
    let known = state.get_peer(&peer_id);
    let source = addr.ip();
    let port = req
        .peer_port
        .filter(|p| *p != 0)
        .or_else(|| known.as_ref().map(|p| p.port).filter(|p| *p != 0))
        .unwrap_or(0);
    let name = req
        .peer_name
        .clone()
        .or_else(|| known.as_ref().map(|p| p.name.clone()))
        .unwrap_or_else(|| peer_id.clone());
    // 来源地址只有在"不是本机自己的监听端点"时才是一次可用的观察（见
    // `net::observed_dial_target`）：模拟器/NAT 后面的对端会把自己看到的本机地址回连
    // 进来，那一对记下来就是本机自己 —— 那种情况退回本机原先知道的地址（多半是空）。
    // 同理，**经中转过来的**握手（F-106：来源是那台中转设备，而不是对端自己）也不能
    // 当观察值用：记下来本机就会拿中转方的地址去拨对端。
    let ip = if source.is_ipv4() && !state.is_relayed_source(source, &peer_id) {
        match net::observed_dial_target(source, port, state.listening_port()) {
            Some((ip, _)) => ip,
            None => known.as_ref().map(|p| p.ip.clone()).unwrap_or_default(),
        }
    } else {
        known.as_ref().map(|p| p.ip.clone()).unwrap_or_default()
    };

    // 落库这一步与邀请那条路共用（写凭据 + 写地址簿 + 预写 BC profile + 广播事件）。
    let grant = pair::mint_grant(&state);
    pair::commit_pairing(
        &state,
        pair::PairedPeer { id: peer_id.clone(), name, ip, port },
        &token,
        None,
    )
    .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;

    // 配对只到"已配对"为止：不在这里建长连接，等本机用户点「连接」再拨号。
    info!(%peer_id, "pairing completed");
    Ok(Json(PairResponse { token, peer_id: grant.peer_id, peer_name: grant.peer_name }))
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PairInviteRequest {
    /// 邀请 id：发起方生成，只用来把两边的这一条邀请对上号。
    pub invite_id: String,
    /// 发起方的身份（自报）——配对成后本机按它登记凭据与设备行。
    pub peer_id: String,
    pub peer_name: String,
    /// 发起方的服务端口（自报）：地址簿要拿它回连/调用本机。
    pub port: u16,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PairInviteWithdraw {
    pub invite_id: String,
}

/// 一条进来的配对邀请要不要按"已配对的老搭档"来验那枚凭据。
///
/// - 本机没把它记成已配对：不验 —— 未绑定设备本来就能发起配对，安全兜底是用户的「接收」；
/// - 已配对、且出示的凭据能反解成它：不必拦（正常路径：它只是换地址/重装前先重新配对）；
/// - 已配对、但拿不出凭据：默认拒 —— 否则谁都能自称你的老搭档，用户点一次「接收」就把
///   那台设备的凭据换成对面手里那枚；
/// - **例外**：本机已经知道这段关系不成立（`known_broken`，界面正显示「需配对」）时放行。
///   这时那枚凭据早被对端删掉了，拦住的结果是"对端点「配对」、本机毫无提示"—— 一次正常
///   的重新配对被静默丢掉。放行 ≠ 同意：仍然要用户在通知里手动点「接收」。
fn must_prove_identity(paired: bool, proven: bool, known_broken: bool) -> bool {
    paired && !proven && !known_broken
}

/// POST /pair/invite —— 免输入配对码那条路（发起方点「发起邀请」，F-12）。
///
/// 与 `POST /push/invite` 同一个形状：这是个**同步**接口，请求一直挂着，直到本机用户在
/// 右下角通知里点「接收/拒绝」、或 60 秒没人处理自动作废 —— 决定一下来就作为响应回给
/// 发起方。响应体与 `POST /pair` 完全一样（`PairResponse`），两条路给发起方的是同一样东西。
///
/// 安全靠两点：① 同意制（没人点「接收」就不会交换 token）；② 若发起方自称的是本机
/// **已配对**的设备，就必须出示那枚 token —— 否则谁都能自称你的老搭档，用户点一次
/// 「接收」就把那台设备的 token 换成了对面手里的那枚（与 `push_invite_handler` 同一道关）。
/// ② 有一个例外：本机**已经知道**这段关系不成立（`LinkState` 停在「需配对」）时放行 ——
/// 那枚凭据早被对端删了，拦住只会让一次正常的重新配对变成"对端点配对、本机毫无提示"
/// （见 `must_prove_identity`）。
pub async fn pair_invite_handler(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: axum::http::HeaderMap,
    Json(req): Json<PairInviteRequest>,
) -> Result<Json<PairResponse>, (StatusCode, String)> {
    if req.peer_id.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "缺少 peerId".into()));
    }
    if req.port == 0 {
        return Err((StatusCode::BAD_REQUEST, "缺少对方服务端口".into()));
    }
    let paired = state.get_peer(&req.peer_id).is_some_and(|p| p.paired);
    let proven = !paired || {
        headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|h| h.strip_prefix("Bearer "))
            .and_then(|t| crate::ws::session::peer_id_from_token(&state, t))
            .as_deref()
            == Some(req.peer_id.as_str())
    };
    // 本机**已经知道**这段关系不成立（界面正显示「需配对」）时不能拦：那枚凭据早已被对端
    // 删掉（它解除过配对 / 清过数据），拿它当门槛只会把一次正常的重新配对挡成静默 401 ——
    // 对端点「配对」，本机连提示都没有。放行到通知里，由用户手动决定（同意制不变）。
    if must_prove_identity(paired, proven, state.ws.needs_repair(&req.peer_id)) {
        info!(peer = %req.peer_id, "配对邀请被拒：自称已配对设备但 token 不认");
        return Err((StatusCode::UNAUTHORIZED, "身份校验失败".into()));
    }
    if paired && !proven {
        // 这条是"关系已经断了"的例外，但仍然是一条**没有身份证明**的邀请：它可能是换了
        // 新地址的老搭档，也可能是同网段的人先制造一次 401 再借这个例外冒充。所以：
        // ① 通知里带上 `unproven`（界面要写明"身份未经校验"）；
        // ② 频率约束 —— 同一个 peer id 每 `UNPROVEN_INVITE_COOLDOWN` 只放行一条，
        //    页面级别地挡住"连点刷卡片"。修不了"它是谁"，但把窗口收到很小。
        if !state.pair.unproven_invite_allowed(&req.peer_id) {
            info!(peer = %req.peer_id, "配对邀请被拒：无凭据的重新配对请求太频繁");
            return Err((
                StatusCode::TOO_MANY_REQUESTS,
                "无凭据的配对邀请过于频繁，请稍后再试".into(),
            ));
        }
        state.pair.note_unproven_invite(&req.peer_id);
        info!(peer = %req.peer_id, "配对邀请放行：本机已知需重新配对（对端已不认这枚凭据）");
    }

    let invite_id = req.invite_id.clone();
    // 地址只信 TCP 来源：邀请体里的地址谁都能填。**但本机自己的监听端点不能当对端地址**
    // —— 安卓模拟器那类 NAT 后面的对端回连进来时，来源地址在**本机**这一侧就是本机自己
    // （见 `net::observed_dial_target`）：记下来会让探活问回自己、把本机的证书钉到对端
    // 名下，最后那一行被当成"这里换了设备"作废，设备行就永远停在「离线」。
    // 地址要么成对记、要么一对都不记（`("", 0)` 是本仓库里"没有地址"的唯一写法）。
    // **经中转过来的**邀请同理（F-106）：来源是那台中转设备，记下来就是把它的地址
    // 当成了邀请方的地址。
    let (ip, port) = if state.is_relayed_source(addr.ip(), &req.peer_id) {
        (String::new(), 0)
    } else {
        net::observed_dial_target(addr.ip(), req.port, state.listening_port()).unwrap_or_default()
    };
    let peer = pair::PairedPeer {
        id: req.peer_id.clone(),
        name: req.peer_name.clone(),
        ip,
        port,
    };
    info!(%invite_id, peer = %req.peer_name, paired, "inbound pair invite (http)");
    crate::ws::emit_ui(&state, ui::PAIR_INVITE, &serde_json::json!({
        "inviteId": invite_id,
        "peerId": req.peer_id,
        "peerName": req.peer_name,
        // 自称是"本机已配对的设备"却没带凭据：通知正文要说清这件事（用户是在这个前提下
        // 按「接收」的 —— 它可能就是换了新地址的老搭档，也可能不是）。
        "unproven": paired && !proven,
    }));

    let (tx, rx) = tokio::sync::oneshot::channel();
    state
        .pair
        .insert_invite(pair::PendingInvite::new(invite_id.clone(), peer, tx));
    pair::schedule_invite_timeout(state.clone(), invite_id.clone());

    // 等用户在通知里做决定（到点由超时任务先回一个原因）。
    match tokio::time::timeout(pair::INVITE_TTL + std::time::Duration::from_secs(15), rx).await {
        Ok(Ok(Ok(grant))) => Ok(Json(PairResponse {
            token: grant.token,
            peer_id: grant.peer_id,
            peer_name: grant.peer_name,
        })),
        Ok(Ok(Err(reason))) => Err((StatusCode::FORBIDDEN, reason)),
        // oneshot 断了（本机正在关停之类）：按"没接受"回。
        _ => Err((StatusCode::FORBIDDEN, "配对邀请已作废".into())),
    }
}

/// POST /pair/invite/withdraw —— 发起方点了「取消」，把它挂在本机界面上的邀请撤掉。
///
/// 不认人、也不回错误：邀请 id 本身就是一次性的能力（只在这条邀请里出现过），
/// 撤不成就让 TTL 兜底。
pub async fn pair_invite_withdraw_handler(
    State(state): State<Arc<AppState>>,
    Json(req): Json<PairInviteWithdraw>,
) -> StatusCode {
    pair::withdraw_incoming_invite(&state, &req.invite_id);
    StatusCode::NO_CONTENT
}

#[cfg(test)]
mod tests {
    use super::must_prove_identity;
    use crate::config::{Config, PeerDevice};
    use crate::state::AppState;
    use axum::http::StatusCode;
    use std::sync::Arc;

    /// 配对邀请那道门的四种组合（见 `must_prove_identity` 的说明）。
    ///
    /// 第三行是这道门存在的理由（挡住"自称你的老搭档"的冒充者），第四行是这次修的那件事
    /// （关系已经断了，就不能再拿那枚死凭据把重新配对挡成静默 401）。
    #[test]
    fn the_invite_gate_only_blocks_unproven_claims_on_live_relationships() {
        assert!(!must_prove_identity(false, false, false), "未绑定：不验凭据");
        assert!(!must_prove_identity(true, true, false), "已配对 + 凭据对得上：放行");
        assert!(must_prove_identity(true, false, false), "已配对 + 拿不出凭据：拒");
        assert!(
            !must_prove_identity(true, false, true),
            "已配对 + 拿不出凭据，但本机已知需重新配对：放行到通知，由用户决定"
        );
    }

    /// 上面那条判定在真接口上的样子：**本机已知需重新配对时，对端（没带凭据）发来的邀请
    /// 必须落到通知那条路上**，而不是当场 401 —— 401 会让用户连提示都看不见
    /// （`emit_ui(PAIR_INVITE)` 在那条 return 之后）。
    ///
    /// 「落到通知那条路」的可观察替身：接口请求**挂着**等用户决定（这里由撤回让它收场，
    /// 回 403），而不是像被挡掉那样立刻回 401。
    #[tokio::test]
    async fn a_repairing_peer_invite_reaches_the_user_instead_of_401() {
        let dir = std::env::temp_dir().join(format!("orbdrop_pair_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let state = Arc::new(AppState::new(Config::default(), dir.join("config.toml")));
        let port = crate::server::start_server(state.clone(), 4318)
            .await
            .expect("server start");

        state.mark_paired(&PeerDevice {
            id: "peer-a".into(),
            name: "PC-B".into(),
            ip: "127.0.0.1".into(),
            port: 1,
            online: true,
            paired: true,
            ..Default::default()
        });
        let url = format!("http://127.0.0.1:{port}/pair/invite");
        let body = |invite_id: &str| {
            serde_json::json!({
                "inviteId": invite_id, "peerId": "peer-a", "peerName": "PC-B", "port": 1,
            })
        };
        let post = |invite_id: &'static str| {
            reqwest::Client::new().post(&url).json(&body(invite_id)).send()
        };

        // ① 已配对、拿不出凭据、关系看起来还正常 → 当场 401（挡住"自称你的老搭档"的冒充者）
        let denied = post("inv-1").await.unwrap();
        assert_eq!(denied.status(), StatusCode::UNAUTHORIZED);

        // ② 本机已知需重新配对（拨号被对端 401）→ 不再挡：邀请挂进通知等用户决定。
        state.ws.set_link_state(
            "peer-a",
            crate::ws::LinkState::Stopped {
                reason: crate::ws::StopReason::TokenRejected,
            },
        );
        let closer = Arc::clone(&state);
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            crate::pair::withdraw_incoming_invite(&closer, "inv-2");
        });
        let asked = post("inv-2").await.unwrap();
        assert_eq!(
            asked.status(),
            StatusCode::FORBIDDEN,
            "邀请要挂到通知里等用户决定（由撤回/超时收场），不能是 401"
        );

        crate::server::stop_server(&state).await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 配对码**猜错要限流**（P1-5）：6 位码 + 120s 有效期内，不限次数时局域网里每秒几千发
    /// 就能把 9e5 的空间扫完 —— 用户正把码显示在屏幕上时被陌生设备配上。
    /// 同一个来源地址错满 `CODE_FAILURE_LIMIT` 次之后先拒一段时间（429），而**猜对**那条
    /// 正常路径不受影响。
    #[tokio::test]
    async fn pairing_code_attempts_are_throttled_per_source() {
        let dir = std::env::temp_dir().join(format!("orbdrop_pair_thr_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let state = Arc::new(AppState::new(Config::default(), dir.join("config.toml")));
        let port = crate::server::start_server(state.clone(), 4319)
            .await
            .expect("server start");
        let url = format!("http://127.0.0.1:{port}/pair");
        let post = |code: &str| {
            let url = url.clone();
            let code = code.to_string();
            async move {
                reqwest::Client::new()
                    .post(&url)
                    .json(&serde_json::json!({
                        "peerId": "peer-t", "peerName": "PC-T", "code": code,
                    }))
                    .send()
                    .await
                    .unwrap()
            }
        };

        for _ in 0..crate::pair::CODE_FAILURE_LIMIT {
            assert_eq!(
                post("000000").await.status(),
                StatusCode::UNAUTHORIZED,
                "限流阈值内的错码都是 401"
            );
        }
        assert_eq!(
            post("000000").await.status(),
            StatusCode::TOO_MANY_REQUESTS,
            "错满 {} 次之后这个地址要被挡住",
            crate::pair::CODE_FAILURE_LIMIT
        );
        // 挡的是**来源地址**，不是"配对"这件事本身：别的地址照旧能试。
        assert!(state.pair.code_attempt_allowed(std::net::IpAddr::from([10, 0, 0, 9])));

        // 猜对那条路不受影响（清掉这个地址的账之后，正常配对照旧成功）。
        let issued = state.pair.create_code();
        state.pair.clear_code_failures(std::net::IpAddr::from([127, 0, 0, 1]));
        assert_eq!(
            post(&issued).await.status(),
            StatusCode::OK,
            "正常配对不该被限流误伤"
        );

        crate::server::stop_server(&state).await;
        let _ = std::fs::remove_dir_all(&dir);
    }
}
