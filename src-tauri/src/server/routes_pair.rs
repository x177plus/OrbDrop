use std::net::SocketAddr;
use std::sync::Arc;

use axum::extract::{ConnectInfo, Extension, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

use crate::config::PeerDevice;
use crate::credential;
use crate::net;
use crate::pair;
use crate::server::auth::bearer_token;
use crate::state::AppState;
use crate::ws;
use crate::ws::events::ui;

/// 对端自报的设备 id 必须是**单一安全路径段**（它会当镜像目录名与 BC 的 URL 路径段用，
/// 见 [`crate::ids`]）。合法端只会发 UUID，这条对真实流量从不拦人；不合法的一律 400，
/// 别让它先写进凭据库/地址簿再在下游某处炸成一句看不懂的错误。
fn ensure_peer_id(peer_id: &str) -> Result<(), (StatusCode, String)> {
    if crate::ids::is_segment(peer_id) {
        return Ok(());
    }
    warn!(peer_id = %peer_id, "拒绝：对端自报的设备标识不合法");
    Err((StatusCode::BAD_REQUEST, "设备标识不合法".to_string()))
}

/// GET /pair/info — public endpoint for manual-add probe.
///
/// Reports the port we are **actually listening on**: the server falls forward to
/// port+1..port+10 when the configured port is taken, and a caller that stores the
/// configured value would dial the wrong process (or nothing) forever.
pub async fn pair_info(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    client_cert: Option<Extension<net::ClientCertFingerprint>>,
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
    // 先把回话组出来、把 config 锁放掉：`note_probe_source` 会走 `get_peer`/`listening_port`，
    // parking_lot 的 Mutex 不可重入，不能套在这把锁里。
    let body = {
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
            // 能力位：本机支持邀请的 SAS 核对（8 位 Base32）。新发起方据此拒绝
            // "对端不会显示核对码"的旧版本，避免用户对着一串对方看不见的码。
            "sas": true,
        })
    };
    // 对端的 5s 探活也带客户端证书：如果这正是本机观察过的某台未配对设备，而来源 IP
    // 与设备行里的地址不同，就在后台到「新 IP + 旧端口」核实一次，核实通过才改地址。
    // 这是"对端沉默、只探活"时也能自动纠正 IP 的那一半；不阻塞这次回话。
    if let Some(Extension(cert)) = client_cert {
        if let Some(fp) = cert.0 {
            state.note_probe_source(addr.ip(), &fp);
        }
    }
    Json(body)
}

/// POST /pair/hello 的请求体：**添加方**自报身份（地址不在这里 —— 取 TCP 来源）。
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PairHello {
    /// 添加方的设备 id。
    pub peer_id: String,
    /// 添加方的显示名。
    pub peer_name: String,
    /// 添加方**实际监听**的端口（服务器顺延过的话，配置值是不对的，见 `/pair/info`）。
    pub port: u16,
}

/// POST /pair/hello —— 「手动添加」的另一半（F-11）：被添加的那一方据此把添加者记成一行。
///
/// 手动添加是**手动版的 mDNS**：同网段时两边本来就互相看得见（`discover::scan` 收到一条
/// 公告就各建一行），而 mDNS 跨不过路由 —— 跨网段只剩这一条单播。所以添加成功后由添加方到
/// 它刚探测过的那个地址上自报一次，让对方也有一行：对方的设备列表里出现本机、它的配对弹窗
/// 里能选中本机，用户才有地方输本机屏幕上那 6 位码（没有这一行时，跨网段那台**根本没法
/// 发起配对** —— `pair_confirm` 要求设备行已存在）。
///
/// **只建行，不建关系**：配对、长连接、共享、推送同意制一条都不动。这一行就是"未绑定"。
///
/// 信任分档与入站会话（`routes_chat::accept`）逐条同规：
/// - 自称的是本机**已配对**的设备 → 必须出示本机给它铸的那枚 token。否则谁都能自称你的
///   老搭档，把那一行的地址改写到他自己那个地址上（`upsert_peer` 对已有行会改写 `ip`）；
/// - 其余（没见过的 id / 未绑定）→ 不验：这一行的地址取 **TCP 来源**（不可伪造）、端口取
///   它自报的服务端口 —— 与一条 mDNS 公告是同一档证据，兜底的是 `upsert_peer` 的行预算
///   （`MAX_UNBOOKED_ROWS`）与"未绑定的离线行不列"（`visibleDevices`）。
///
/// 本机隐身/离线时入站整个被 `presence_middleware` 拦成 503（与 mDNS 注销广播同一件事）；
/// 添加方那边是 best-effort，收不到就算了，添加在本机已经成立。
pub async fn pair_hello(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: axum::http::HeaderMap,
    client_cert: Option<Extension<net::ClientCertFingerprint>>,
    Json(req): Json<PairHello>,
) -> Result<StatusCode, (StatusCode, String)> {
    let peer_id = req.peer_id.trim().to_string();
    if peer_id.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "缺少 peerId".into()));
    }
    ensure_peer_id(&peer_id)?;
    // 自称是本机自己：id 是每份数据目录一份的随机值，"另一台设备拿着本机的 id"只可能是
    // 抄了配置/伪造。收下它就等于给本机列表里插一行"自己"（地址还是别人的），后面所有关系
    // 判定都跟着乱 —— 与手动添加那一侧的"填的是本机自己的地址"同一个口径，直接拒。
    if peer_id == state.config.lock().device_id {
        info!(%peer_id, "手动添加的自报被拒：自称是本机自己");
        return Err((StatusCode::BAD_REQUEST, "不能把本机自己加进来".into()));
    }
    let existing = state.get_peer(&peer_id);
    let paired = existing.as_ref().is_some_and(|p| p.paired);
    if paired {
        let claimed =
            bearer_token(&headers).and_then(|t| ws::session::peer_id_from_token(&state, &t));
        if claimed.as_deref() != Some(peer_id.as_str()) {
            info!(%peer_id, "手动添加的自报被拒：自称已配对设备但 token 不认");
            return Err((StatusCode::UNAUTHORIZED, "身份校验失败".into()));
        }
    }
    // 已有行 + 未配对：只接受**同一枚 TLS 客户端证书**带来的地址变更（与 `/chat/*`
    // 同一条判据）。真设备换 IP 时证书不变；冒充者拿不出那把私钥，旧地址保持不动。
    let cert_fp = client_cert
        .as_ref()
        .and_then(|Extension(fp)| fp.0.as_deref());
    let identity_matches =
        !paired && existing.is_some() && state.observed_fingerprint_matches(&peer_id, cert_fp);
    let addr_trusted = paired || existing.is_none() || identity_matches;

    // 来源地址要在**不是本机自己的监听端点**时才是一次可用的观察，理由与入站会话那一跳
    // 完全相同（模拟器/NAT 后面的对端会把自己看到的本机地址回连进来；经中转来的那一跳的
    // 来源是中转方）。那种情形一行都不记：没有可拨的地址，可达性只能由长连接证明。
    let (ip, port, online) = if !addr_trusted {
        (String::new(), 0, false)
    } else {
        match net::observed_dial_target(addr.ip(), req.port, state.listening_port()) {
            Some((ip, port)) if !state.is_relayed_source(addr.ip(), &peer_id) => (ip, port, true),
            _ => (String::new(), 0, false),
        }
    };
    let row = PeerDevice {
        id: peer_id.clone(),
        name: req.peer_name.clone(),
        ip,
        port,
        online,
        paired,
        auto_connect: false,
        paused: false,
        relay: None,
    };
    state.upsert_peer(&row);
    if !paired && existing.is_none() {
        if let Some(fp) = cert_fp {
            state.note_observed_fingerprint(&peer_id, fp);
        }
    }
    if existing.is_some() && !addr_trusted {
        info!(
            %peer_id,
            from = %addr.ip(),
            "手动添加的自报未采信地址变更：身份连续性不匹配（旧地址保持不动）"
        );
    }
    // 记成什么要跟着日志走：`online=false`（来源地址是本机自己 / 像"经中转来的"）时那一行是
    // **未绑定 + 离线**，而界面按 `visibleDevices` 不列未绑定的离线行 —— 用户那边就是"对方
    // 添加了我，我却没看见"。这一行日志是唯一能分清"报文根本没到"与"到了但这一格没认下"的地方。
    info!(
        %peer_id,
        name = %row.name,
        paired,
        addr_trusted,
        ip = %row.ip,
        port = row.port,
        online = row.online,
        "对方手动添加了本机，已登记设备行"
    );
    Ok(StatusCode::NO_CONTENT)
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
    /// 发起方自报的证书指纹（可选；老对端没有）。本机这一跳看不到对端证书，
    /// 用它把关系在配对时就钉住，避免留下空指纹窗口。
    #[serde(default)]
    pub cert_fingerprint: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PairResponse {
    pub token: String,
    pub peer_id: String,
    pub peer_name: String,
    /// 本机证书指纹：发起方拿到回执后直接钉住本机。
    #[serde(default)]
    pub cert_fingerprint: String,
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
            .ok_or_else(|| {
                (
                    StatusCode::BAD_REQUEST,
                    "本机调用需要给出 peerId".to_string(),
                )
            })?,
    };

    if let Err(e) = credential::remove_peer(&peer_id) {
        return Err((
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("凭据清理失败: {e}"),
        ));
    }
    state.ws.drop_peer(&peer_id);
    state.unpair(&peer_id);

    // 同一件收尾：BC 里那条指向对方的 profile 已随 token 作废（只删我们自己写下的条目）。
    crate::bc::provision_profiles(&state);

    ws::emit_ui(&state, ui::DEVICES_CHANGED, &serde_json::json!({}));
    info!(%peer_id, "peer unpaired us via HTTP");
    Ok(StatusCode::NO_CONTENT)
}

/// 请求体里自报的证书指纹必须就是这条 TLS 连接出示的客户端证书。
///
/// 返回**最终要钉住的那一枚**：
/// - 连接出示了客户端证书 → 以它为准（老对端不带 `certFingerprint` 也能钉住真身份）；
/// - 没出示 → 只能退回请求体（回环明文 / 老版本客户端）；
/// - 自报了指纹却没出示证书 → 拒。否则"自报"仍只是一句谁都能说的话。
///
/// 这**不是**配对中间人的解药：中间人可以用自己的证书终止两端 TLS，两边照样各自钉住它。
/// 它解决的是"自报的身份与连接上出示的身份不一致"，以及让下一步的带外核对（SAS）
/// 有真实的双方证书可用（此前服务端根本看不到客户端证书）。
fn bound_peer_fingerprint(
    claimed: &str,
    observed: Option<&str>,
) -> Result<String, (StatusCode, String)> {
    match observed {
        Some(seen) => {
            if !claimed.is_empty() && !seen.eq_ignore_ascii_case(claimed) {
                warn!(
                    claimed = %net::short_fingerprint(claimed),
                    seen = %net::short_fingerprint(seen),
                    "配对请求自报的证书指纹与这条连接出示的不一致"
                );
                return Err((StatusCode::UNAUTHORIZED, "证书指纹与本次连接不一致".into()));
            }
            Ok(seen.to_string())
        }
        None if !claimed.is_empty() => {
            warn!("配对请求自报了指纹，但这条连接没有出示客户端证书");
            Err((StatusCode::UNAUTHORIZED, "请求没有出示客户端证书".into()))
        }
        None => Ok(String::new()),
    }
}

/// 配对请求里"对端此刻怎么拨"的那一半：TCP 来源地址 + 对方自报的服务端口。
///
/// 来源地址只有在"不是本机自己的监听端点"时才是一次可用的观察（见
/// `net::observed_dial_target`）：模拟器/NAT 后面的对端会把自己看到的本机地址回连
/// 进来，那一对记下来就是本机自己 —— 那种情况退回本机原先知道的地址（多半是空）。
/// 同理，**经中转过来的**握手（F-106：来源是那台中转设备，而不是对端自己）也不能
/// 当观察值用：记下来本机就会拿中转方的地址去拨对端。
///
/// 配对码、PAKE 两条路共用同一份判据，别各写一遍。
pub(crate) fn peer_endpoint(
    state: &Arc<AppState>,
    addr: SocketAddr,
    peer_id: &str,
    reported_port: u16,
) -> (String, u16) {
    let known = state.get_peer(peer_id);
    let port = if reported_port != 0 {
        reported_port
    } else {
        known
            .as_ref()
            .map(|p| p.port)
            .filter(|p| *p != 0)
            .unwrap_or(0)
    };
    let source = addr.ip();
    let ip = if source.is_ipv4() && !state.is_relayed_source(source, peer_id) {
        match net::observed_dial_target(source, port, state.listening_port()) {
            Some((ip, _)) => ip,
            None => known.as_ref().map(|p| p.ip.clone()).unwrap_or_default(),
        }
    } else {
        known.as_ref().map(|p| p.ip.clone()).unwrap_or_default()
    };
    (ip, port)
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
    client_cert: Option<Extension<net::ClientCertFingerprint>>,
    Json(req): Json<PairRequest>,
) -> Result<Json<PairResponse>, (StatusCode, String)> {
    let peer_id = req.peer_id.as_deref().unwrap_or_default().to_string();
    if peer_id.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "缺少 peerId".into()));
    }
    ensure_peer_id(&peer_id)?;
    // 先核对"自报的指纹"与这条连接出示的证书（先于消耗配对码：对不上不该烧掉一枚好码）。
    let observed = client_cert
        .as_ref()
        .and_then(|Extension(fp)| fp.0.as_deref());
    let fingerprint = bound_peer_fingerprint(&req.cert_fingerprint, observed)?;
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
    let name = req
        .peer_name
        .clone()
        .or_else(|| state.get_peer(&peer_id).map(|p| p.name.clone()))
        .unwrap_or_else(|| peer_id.clone());
    let (ip, port) = peer_endpoint(&state, addr, &peer_id, req.peer_port.unwrap_or(0));

    // 落库这一步与邀请那条路共用（写凭据 + 写地址簿 + 预写 BC profile + 广播事件）。
    let grant = pair::mint_grant(&state);
    let pinned = fingerprint.clone();
    let peer = pair::PairedPeer {
        id: peer_id.clone(),
        name,
        ip,
        port,
        fingerprint,
    };
    // 新客户端带客户端证书：`fingerprint` 是这条连接实际出示的那一枚，直接钉它；
    // 老客户端（空）仍走 `commit_pairing` 的"看到过/自报"兼容逻辑。
    let committed = if pinned.is_empty() {
        pair::commit_pairing(&state, peer, &token, None)
    } else {
        pair::commit_pairing_with_fingerprint(&state, peer, &token, None, &pinned)
    };
    committed.map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e))?;

    // 配对只到"已配对"为止：不在这里建长连接，等本机用户点「连接」再拨号。
    info!(%peer_id, "pairing completed");
    Ok(Json(PairResponse {
        token,
        peer_id: grant.peer_id,
        peer_name: grant.peer_name,
        cert_fingerprint: grant.fingerprint,
    }))
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
    /// 发起方自报的证书指纹（可选；老对端没有）。
    #[serde(default)]
    pub cert_fingerprint: String,
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
    client_cert: Option<Extension<net::ClientCertFingerprint>>,
    headers: axum::http::HeaderMap,
    Json(req): Json<PairInviteRequest>,
) -> Result<Json<PairResponse>, (StatusCode, String)> {
    if req.peer_id.trim().is_empty() {
        return Err((StatusCode::BAD_REQUEST, "缺少 peerId".into()));
    }
    ensure_peer_id(req.peer_id.trim())?;
    if req.port == 0 {
        return Err((StatusCode::BAD_REQUEST, "缺少对方服务端口".into()));
    }
    // 邀请这条路口径与配对码那条一致：自报指纹必须就是这条连接出示的证书。
    let observed = client_cert
        .as_ref()
        .and_then(|Extension(fp)| fp.0.as_deref());
    let peer_fingerprint = bound_peer_fingerprint(&req.cert_fingerprint, observed)?;
    // 旧版本没有客户端证书：本机既没法把邀请绑到某个证书上，也没有可核对的 SAS。
    // 回环留给测试/本机自验；**局域网来源直接拒**，否则"不发证书"就成了降级开关。
    if peer_fingerprint.is_empty() && !addr.ip().is_loopback() {
        return Err((
            StatusCode::BAD_REQUEST,
            "对方版本过旧，无法核对邀请身份；请改用配对码".into(),
        ));
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
    // 老记录的空指纹 = 关系还在、但本机没有可校验的身份锚。和「需配对」同档：
    // 放行到通知里让用户手动决定，否则这类设备永远没法通过重新配对补上指纹。
    let legacy_unpinned = paired
        && crate::credential::has_peer(&req.peer_id)
        && crate::credential::get_peer_fingerprint(&req.peer_id)
            .ok()
            .flatten()
            .is_none();
    if must_prove_identity(
        paired,
        proven,
        state.ws.needs_repair(&req.peer_id) || legacy_unpinned,
    ) {
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
    // 8 位 SAS：发起方（A）与应答方（B）各算一次，用户比对一致再接收。
    // 没有客户端证书（回环测试 / 旧版本）就没有可核对的第二枚指纹 → 不给码。
    let own_fp = net::local_fingerprint().unwrap_or_default();
    let sas = crate::sas::invite_sas(&invite_id, &peer_fingerprint, &own_fp);
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
        fingerprint: peer_fingerprint,
    };
    info!(%invite_id, peer = %req.peer_name, paired, "inbound pair invite (http)");
    crate::ws::emit_ui(
        &state,
        ui::PAIR_INVITE,
        &serde_json::json!({
            "inviteId": invite_id,
            "peerId": req.peer_id,
            "peerName": req.peer_name,
            // 自称是"本机已配对的设备"却没带凭据：通知正文要说清这件事（用户是在这个前提下
            // 按「接收」的 —— 它可能就是换了新地址的老搭档，也可能不是）。
            "unproven": paired && !proven,
            // 8 位 Base32；没有可核对的证书时是 null（界面要写明"无法核对"）。
            "sas": sas,
        }),
    );

    let (tx, rx) = tokio::sync::oneshot::channel();
    if !state
        .pair
        .insert_invite(pair::PendingInvite::new(invite_id.clone(), peer, tx))
    {
        warn!(%invite_id, "配对邀请被拒：待处理邀请已达上限或重复");
        return Err((
            StatusCode::TOO_MANY_REQUESTS,
            "待处理的配对邀请过多，请稍后重试".to_string(),
        ));
    }
    pair::schedule_invite_timeout(state.clone(), invite_id.clone());

    // 等用户在通知里做决定（到点由超时任务先回一个原因）。
    match tokio::time::timeout(pair::INVITE_TTL + std::time::Duration::from_secs(15), rx).await {
        Ok(Ok(Ok(grant))) => Ok(Json(PairResponse {
            token: grant.token,
            peer_id: grant.peer_id,
            peer_name: grant.peer_name,
            cert_fingerprint: grant.fingerprint,
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
    use super::{bound_peer_fingerprint, must_prove_identity, pair_hello, PairHello};
    use crate::config::PeerDevice;
    use crate::net;
    use crate::state::AppState;
    use axum::extract::{ConnectInfo, Extension, State};
    use axum::http::HeaderMap;
    use axum::http::StatusCode;
    use axum::Json;
    use std::sync::Arc;

    /// 自报指纹与连接出示的证书：一致才认；自报了却拿不出证书要拒；老客户端（空自报）
    /// 保持原样。
    #[test]
    fn a_claimed_fingerprint_must_be_the_one_on_the_connection() {
        let a = "aa".repeat(32);
        let b = "bb".repeat(32);
        assert_eq!(
            bound_peer_fingerprint(&a, Some(&a)).unwrap(),
            a,
            "一致：以连接出示的那一枚为准"
        );
        assert_eq!(
            bound_peer_fingerprint("", Some(&a)).unwrap(),
            a,
            "老客户端不带自报：仍然按连接出示的证书钉住"
        );
        assert_eq!(
            bound_peer_fingerprint("", None).unwrap(),
            "",
            "回环明文/老客户端：维持旧行为"
        );
        assert_eq!(
            bound_peer_fingerprint(&b, Some(&a)).unwrap_err().0,
            StatusCode::UNAUTHORIZED,
            "自报的不是这条连接的证书：拒"
        );
        assert_eq!(
            bound_peer_fingerprint(&a, None).unwrap_err().0,
            StatusCode::UNAUTHORIZED,
            "自报了却没出示证书：拒"
        );
    }

    /// 端到端：真 TLS 上，服务端能拿到客户端证书，配对请求里自报的指纹必须与它一致。
    ///
    /// 这是"把指纹绑到通道上"的可观察结果 —— 之前服务端完全看不到客户端证书，只能
    /// 相信请求体。断言三档：错的自报 → 401；对的自报 → 200 且凭据库里钉的就是它；
    /// 不自报（老客户端）→ 200 且仍然按连接出示的证书钉住。
    #[tokio::test]
    async fn pairing_pins_the_certificate_seen_on_the_tls_connection() {
        let dir = std::env::temp_dir().join(format!("orbdrop_pair_bind_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let state = Arc::new(AppState::new(
            crate::config::test_config(),
            dir.join("config.toml"),
        ));
        let port = crate::server::start_server_loopback(state.clone(), 4322)
            .await
            .expect("server start");
        let url = format!("https://127.0.0.1:{port}/pair");
        // 必须在本线程建 client：`cfg(test)` 下身份文件按线程分，客户端证书要与
        // `local_fingerprint()` 读到的是同一份。
        let client = crate::net::client_builder().build().expect("client");
        let own = crate::net::local_fingerprint().expect("identity");

        let wrong = "00".repeat(32);
        let code = state.pair.create_code();
        let resp = client
            .post(&url)
            .json(&serde_json::json!({
                "peerId": "peer-bind", "peerName": "PC-B", "code": code,
                "certFingerprint": wrong,
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(
            resp.status(),
            StatusCode::UNAUTHORIZED,
            "自报的指纹不是这条连接出示的那一枚：拒"
        );

        let code = state.pair.create_code();
        let resp = client
            .post(&url)
            .json(&serde_json::json!({
                "peerId": "peer-bind", "peerName": "PC-B", "code": code,
                "certFingerprint": own,
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            crate::credential::get_peer_fingerprint("peer-bind")
                .unwrap()
                .as_deref(),
            Some(own.as_str()),
            "钉住的必须是连接上出示的那一枚"
        );

        // 老客户端：不带 `certFingerprint`，但仍出示了证书 —— 也要钉住真身份。
        let code = state.pair.create_code();
        let resp = client
            .post(&url)
            .json(&serde_json::json!({
                "peerId": "peer-bind-legacy", "peerName": "PC-L", "code": code,
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        assert_eq!(
            crate::credential::get_peer_fingerprint("peer-bind-legacy")
                .unwrap()
                .as_deref(),
            Some(own.as_str()),
            "老客户端不带自报：按连接出示的证书补上，不再留空指纹窗口"
        );

        crate::server::stop_server(&state).await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 配对邀请那道门的四种组合（见 `must_prove_identity` 的说明）。
    ///
    /// 第三行是这道门存在的理由（挡住"自称你的老搭档"的冒充者），第四行是这次修的那件事
    /// （关系已经断了，就不能再拿那枚死凭据把重新配对挡成静默 401）。
    #[test]
    fn the_invite_gate_only_blocks_unproven_claims_on_live_relationships() {
        assert!(
            !must_prove_identity(false, false, false),
            "未绑定：不验凭据"
        );
        assert!(
            !must_prove_identity(true, true, false),
            "已配对 + 凭据对得上：放行"
        );
        assert!(
            must_prove_identity(true, false, false),
            "已配对 + 拿不出凭据：拒"
        );
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
        let state = Arc::new(AppState::new(
            crate::config::test_config(),
            dir.join("config.toml"),
        ));
        let port = crate::server::start_server_loopback(state.clone(), 4318)
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
            reqwest::Client::new()
                .post(&url)
                .json(&body(invite_id))
                .send()
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

    /// 手动添加的另一半（F-11）：被添加的一方据此也建一行，而这一行是**低信任**的。
    ///
    /// 三档一次测完：
    /// - 没见过的 id：不验凭据，落一行"未绑定"（地址取 TCP 来源、端口取它自报的那个）；
    /// - 自称本机**已配对**的设备却拿不出凭据：401，且那一行一个字都不动 —— 地址不会被改写到
    ///   来源地址上。这一档挡的就是"自称你的老搭档，把那一行指到他自己身上"的冒充者；
    /// - 同一台带上本机铸给它的那枚 token：放行（"重新添加一台已配对设备的地址"走这一档）。
    #[tokio::test]
    async fn the_hello_route_registers_the_adder_without_trusting_a_paired_claim() {
        let dir = std::env::temp_dir().join(format!("orbdrop_hello_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let state = Arc::new(AppState::new(
            crate::config::test_config(),
            dir.join("config.toml"),
        ));
        let port = crate::server::start_server_loopback(state.clone(), 4321)
            .await
            .expect("server start");
        let url = format!("http://127.0.0.1:{port}/pair/hello");
        let post = |body: serde_json::Value, token: Option<&'static str>| {
            let url = url.clone();
            async move {
                let mut req = reqwest::Client::new().post(&url).json(&body);
                if let Some(t) = token {
                    req = req.bearer_auth(t);
                }
                req.send().await.unwrap()
            }
        };
        let body = |id: &str, name: &str| serde_json::json!({ "peerId": id, "peerName": name, "port": 3890 });

        // ① 没见过的 id：进设备表，未绑定 + 在线，地址是这条 TCP 连接的来源。
        assert_eq!(
            post(body("peer-new", "PC-N"), None).await.status(),
            StatusCode::NO_CONTENT
        );
        let row = state.get_peer("peer-new").expect("被添加的一方要建这一行");
        assert!(!row.paired, "只建行、不建关系");
        assert!(row.online);
        assert_eq!((row.ip.as_str(), row.port), ("127.0.0.1", 3890));

        // ①之二 自称是本机自己：不是本机自己加自己（`manual_peer` 那一侧也拦着这个），直接拒。
        let own_id = state.config.lock().device_id.clone();
        assert_eq!(
            post(body(&own_id, "本机"), None).await.status(),
            StatusCode::BAD_REQUEST
        );
        assert!(
            state.get_peer(&own_id).is_none(),
            "设备表里不许冒出本机自己那一行"
        );

        // ② 自称已配对、拿不出凭据 → 401，那一行原样不动。
        state.mark_paired(&PeerDevice {
            id: "peer-p".into(),
            name: "PC-P".into(),
            ip: "10.0.0.5".into(),
            port: 4000,
            online: true,
            paired: true,
            ..Default::default()
        });
        assert_eq!(
            post(body("peer-p", "冒充者"), None).await.status(),
            StatusCode::UNAUTHORIZED
        );
        let row = state.get_peer("peer-p").unwrap();
        assert_eq!(
            (row.ip.as_str(), row.port, row.name.as_str()),
            ("10.0.0.5", 4000, "PC-P"),
            "冒充者不许把那一行改到他自己身上"
        );

        // ③ 带上本机铸给它的那枚 token：放行（这一档是"重新添加一台已配对设备的地址"）。
        //
        // 地址那一格在这里**不能断言**：回环上所有连接都来自 `127.0.0.1`，而 ① 刚建的那一行
        // 正占着这个来源地址 → `is_relayed_source` 会判定"这条连接可能是经它中转来的"，
        // 按规矩不记地址（真机上"两台设备共用一个来源地址"就是中转那一跳）。地址取自
        // TCP 来源这件事由 `a_manual_add_shows_up_on_the_peer_as_an_unbound_row` 在内网
        // 地址上走真 TLS 验。
        crate::credential::set_peer_credential("peer-p", "tok-hello", "").unwrap();
        assert_eq!(
            post(body("peer-p", "PC-P"), Some("tok-hello"))
                .await
                .status(),
            StatusCode::NO_CONTENT
        );
        let row = state.get_peer("peer-p").unwrap();
        assert!(row.paired && row.name == "PC-P", "凭据对得上：这一行照旧");

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
        let state = Arc::new(AppState::new(
            crate::config::test_config(),
            dir.join("config.toml"),
        ));
        let port = crate::server::start_server_loopback(state.clone(), 4319)
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
        assert!(state
            .pair
            .code_attempt_allowed(std::net::IpAddr::from([10, 0, 0, 9])));

        // 猜对那条路不受影响（清掉这个地址的账之后，正常配对照旧成功）。
        let issued = state.pair.create_code();
        state
            .pair
            .clear_code_failures(std::net::IpAddr::from([127, 0, 0, 1]));
        assert_eq!(
            post(&issued).await.status(),
            StatusCode::OK,
            "正常配对不该被限流误伤"
        );

        crate::server::stop_server(&state).await;
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `/pair/hello` 与 `/chat/*` 同一条闸：已有未配对行只接受同一枚客户端证书带来的
    /// 地址变更。手动添加的对方换 IP 时证书不变 → 自动纠正；冒充者换证书 → 地址不动。
    #[tokio::test]
    async fn pair_hello_moves_a_known_address_only_with_the_same_certificate() {
        let dir = std::env::temp_dir().join(format!("orbdrop_hello_gate_{}", uuid::Uuid::new_v4()));
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

        async fn hello(state: &Arc<AppState>, from: &str, fp: &str) -> StatusCode {
            pair_hello(
                State(state.clone()),
                ConnectInfo(format!("{from}:5555").parse().unwrap()),
                HeaderMap::new(),
                Some(Extension(net::ClientCertFingerprint(Some(fp.to_string())))),
                Json(PairHello {
                    peer_id: "a".into(),
                    peer_name: "PC-A".into(),
                    port: 5000,
                }),
            )
            .await
            .expect("hello 应当被收下")
        }

        assert_eq!(
            hello(&state, "10.0.0.9", &other).await,
            StatusCode::NO_CONTENT
        );
        let row = state.get_peer("a").expect("设备行还在");
        assert_eq!(
            (row.ip.as_str(), row.port),
            ("10.0.0.5", 4000),
            "不同证书不许改已有行的地址"
        );

        assert_eq!(
            hello(&state, "10.0.0.10", &known).await,
            StatusCode::NO_CONTENT
        );
        let row = state.get_peer("a").expect("设备行还在");
        assert_eq!(
            (row.ip.as_str(), row.port),
            ("10.0.0.10", 5000),
            "同一枚证书换 IP 要跟着走"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 端到端：对端**不说话、只探活**，本机也能把它的新 IP 学回来。
    ///
    /// A 是那台换了地址的设备（真实服务器，监听回环），B 是持有 A 旧地址与观察指纹的
    /// 本机。B 的 `/pair/info` 收到 A 的探活（带客户端证书）后，应到「新 IP + 旧端口」
    /// 核实 A 的 id 与证书，并把行更新到新地址。
    #[tokio::test]
    async fn a_silent_peer_is_learned_from_its_probe_source() {
        let dir = std::env::temp_dir().join(format!("orbdrop_probe_src_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let peer_dir = dir.join("peer");
        let own_dir = dir.join("own");
        std::fs::create_dir_all(&peer_dir).unwrap();
        std::fs::create_dir_all(&own_dir).unwrap();

        // A：设备 id 固定，B 记的就是它。
        let peer = Arc::new(AppState::new(
            crate::config::Config {
                device_id: "peer-a".into(),
                device_name: "PC-A".into(),
                ..crate::config::test_config()
            },
            peer_dir.join("config.toml"),
        ));
        let peer_port = {
            let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("bind probe");
            probe.local_addr().expect("probe addr").port()
        };
        let peer_port = crate::server::start_server_loopback(peer.clone(), peer_port)
            .await
            .expect("peer server start");

        // B：行里是 A 的旧地址，观察指纹是本进程的身份（A 的服务器证书也是它）。
        let own = Arc::new(AppState::new(
            crate::config::Config {
                device_id: "own-b".into(),
                device_name: "PC-B".into(),
                ..crate::config::test_config()
            },
            own_dir.join("config.toml"),
        ));
        own.upsert_peer(&PeerDevice {
            id: "peer-a".into(),
            name: "PC-A".into(),
            ip: "10.0.0.5".into(),
            port: peer_port,
            paired: false,
            ..Default::default()
        });
        let fp = crate::net::local_fingerprint().expect("identity");
        assert!(own.note_observed_fingerprint("peer-a", &fp));

        let own_port = {
            let probe = std::net::TcpListener::bind("127.0.0.1:0").expect("bind probe");
            probe.local_addr().expect("probe addr").port()
        };
        let own_port = crate::server::start_server_loopback(own.clone(), own_port)
            .await
            .expect("own server start");

        // A 的定时探活就是这个形状：GET /pair/info，带自己的客户端证书。
        //
        // **不要用进程级 `crate::net::client()`**：它的客户端证书在第一次调用时就被固定
        // 下来，而测试身份文件是**按线程**分的 —— 另一个测试先初始化了那个全局客户端时，
        // 这里出示的会是另一把证书，`note_probe_source` 自然认不出本测试观察到的指纹。
        // 本线程现建一个，证书才与 `local_fingerprint()` 读到的是同一份。
        let client = crate::net::client_builder().build().expect("probe client");
        let resp = client
            .get(format!("https://127.0.0.1:{own_port}/pair/info"))
            .send()
            .await
            .expect("probe request");
        assert!(resp.status().is_success());

        // 后台核实是一个任务：等它把地址写回来（最多 2 秒）。
        let mut row = own.get_peer("peer-a").expect("设备行还在");
        for _ in 0..40 {
            if row.ip == "127.0.0.1" {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            row = own.get_peer("peer-a").expect("设备行还在");
        }
        assert_eq!(
            (row.ip.as_str(), row.port),
            ("127.0.0.1", peer_port),
            "探活来源核实通过后要自动纠正到新 IP"
        );

        crate::server::stop_server(&own).await;
        crate::server::stop_server(&peer).await;
        let _ = std::fs::remove_dir_all(&dir);
    }
}
