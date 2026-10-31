//! LocalSend v2.2 的 UDP 组播发现。
//!
//! 只做三件事：按网卡加入 `224.0.0.167:53317` 收发公告、把公告里的对端登记到
//! `LocalSendState`、周期探活/清理由此得到的设备行。文件面（`/register`、上传）
//! 在 `server.rs`；这里不碰 HTTP 监听，也不发任何 OrbDrop 自己的广播。
//!
//! socket 的生命周期属于 supervisor task：`ServiceHandle.cancel` 一取消，supervisor
//! 先停掉子任务，再 drop 全部 socket —— `mod.rs::stop` 的 abort 也会把它们一起带走。

use std::collections::HashMap;
use std::future::Future;
use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket as StdUdpSocket};
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use socket2::{Domain, Protocol, Socket, Type};
use tokio::net::UdpSocket;
use tokio::task::JoinHandle;
use tokio::time::MissedTickBehavior;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use super::dto;
use super::ServiceHandle;
use crate::discover::Scope;
use crate::net;
use crate::state::AppState;

/// LocalSend 的固定组播地址与端口（不与 OrbDrop 的 mDNS 共用）。
const MULTICAST_ADDR: Ipv4Addr = Ipv4Addr::new(224, 0, 0, 167);
const DISCOVERY_PORT: u16 = 53317;

/// 启动公告的重复节奏（累计 `100ms`、`+500ms`、`+2s` 各发一次），随后每 5 分钟一轮。
const ANNOUNCE_BURST: [Duration; 3] = [
    Duration::from_millis(100),
    Duration::from_millis(500),
    Duration::from_secs(2),
];
const ANNOUNCE_EVERY: Duration = Duration::from_secs(5 * 60);

/// 已知对端每 30s 探活；连续失败 3 次标离线。
const PROBE_EVERY: Duration = Duration::from_secs(30);
const PROBE_FAIL_LIMIT: u32 = 3;
/// 多久没见就丢掉这一行。
const PEER_STALE: Duration = Duration::from_secs(10 * 60);
const PRUNE_EVERY: Duration = Duration::from_secs(60);

/// 同一条公告可能从每张网卡各来一次；这层去重只挡“同一来源的重复投递”。
const DEDUPE_WINDOW: Duration = Duration::from_secs(2);

/// 启动发现服务。失败只返回字符串，由 `mod.rs` 打 warn（不 panic）。
pub fn start(state: Arc<AppState>) -> Result<ServiceHandle, String> {
    // 同机多开/显式 local|off：组播会被局域网上的其他用户看到，宁可不起。
    if !scope_allows_discovery() {
        return Err("当前为联调/本地发现作用域，不发送 LocalSend 局域网公告".to_string());
    }
    if !super::enabled(&state) {
        return Err("LocalSend 兼容未启用".to_string());
    }
    if !state.is_online() {
        return Err("本机离线，LocalSend 发现不启动".to_string());
    }

    let sockets = bind_sockets()?;
    let cancel = CancellationToken::new();
    let task = spawn_any(run(state, sockets, cancel.clone()));
    Ok(ServiceHandle { cancel, task })
}

/// 联调 / 显式 local|off 作用域下不发局域网组播（`announce_ephemeral` 也走这一条）。
fn scope_allows_discovery() -> bool {
    std::env::var_os("ORBDROP_ALLOW_MULTI").is_none()
        && !matches!(Scope::current(), Scope::Local | Scope::Off)
}

/// 枚举非回环 IPv4 网卡地址：每张网卡一份 socket / 一次公告。
fn candidate_interfaces() -> Result<Vec<Ipv4Addr>, String> {
    let interfaces = if_addrs::get_if_addrs().map_err(|e| format!("枚举网卡失败：{e}"))?;
    let mut candidates = Vec::new();
    for iface in interfaces {
        if let if_addrs::IfAddr::V4(v4) = iface.addr {
            if !v4.ip.is_loopback() && !v4.ip.is_unspecified() {
                candidates.push(v4.ip);
            }
        }
    }
    if candidates.is_empty() {
        return Err("没有可用的 IPv4 网卡，LocalSend 发现无法启动".to_string());
    }
    Ok(candidates)
}

/// 在运行时内用当前 runtime，否则交给 Tauri 的全局运行时（`start` 是同步入口）。
fn spawn_any<F>(fut: F) -> JoinHandle<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    match tokio::runtime::Handle::try_current() {
        Ok(handle) => handle.spawn(fut),
        Err(_) => crate::task::spawn("localsend-discovery", fut),
    }
}

/// 每张非回环 IPv4 网卡一个 socket：`0.0.0.0:53317` + `SO_REUSEADDR(/SO_REUSEPORT)`，
/// 各自 join 组播并设定出口网卡。
///
/// 返回的是 **`std` socket**：这一层刻意不碰 `tokio::net::UdpSocket`。tokio 的
/// `from_std` 要问当前 reactor（没有就 `scheduler::Handle::current()` panic），而
/// [`start`] 的调用方有一半是**同步**入口 —— 设置页开关 `set_localsend_enabled`、
/// 托盘里的「在线 / 离线」都在主线程上、没有 reactor。以前在这里转 tokio 的行为是
/// 用户侧"点一下开关，App 当场闪退"（release 档 `panic = "abort"`）。转换只放在
/// [`run`] / [`announce_ephemeral`] 里做 —— 它们一定是被运行时 poll 的异步函数。
fn bind_sockets() -> Result<Vec<StdUdpSocket>, String> {
    let candidates = candidate_interfaces()?;
    let mut sockets = Vec::with_capacity(candidates.len());
    for ip in candidates {
        match bind_one(ip) {
            Ok(sock) => sockets.push(sock),
            // 单张网卡（VPN/虚拟网卡）不支持组播不应该拖垮整个发现。
            Err(e) => warn!(interface = %ip, "LocalSend 组播绑定失败：{e}"),
        }
    }
    if sockets.is_empty() {
        Err("所有 IPv4 网卡都无法绑定 UDP 53317".to_string())
    } else {
        Ok(sockets)
    }
}

fn bind_one(interface: Ipv4Addr) -> Result<StdUdpSocket, String> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))
        .map_err(|e| format!("创建 UDP socket 失败：{e}"))?;
    // Windows 上必须在 bind 之前设置，否则第二个 socket 会绑不上。
    socket
        .set_reuse_address(true)
        .map_err(|e| format!("SO_REUSEADDR 失败：{e}"))?;
    #[cfg(unix)]
    {
        socket
            .set_reuse_port(true)
            .map_err(|e| format!("SO_REUSEPORT 失败：{e}"))?;
    }

    let bind_addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, DISCOVERY_PORT);
    socket
        .bind(&bind_addr.into())
        .map_err(|e| format!("绑定 {bind_addr} 失败：{e}"))?;
    socket
        .join_multicast_v4(&MULTICAST_ADDR, &interface)
        .map_err(|e| format!("加入 {MULTICAST_ADDR}（{interface}）失败：{e}"))?;
    // 出口网卡：每张网卡的 socket 只从自己那条链路发公告。
    socket
        .set_multicast_if_v4(&interface)
        .map_err(|e| format!("设置组播出口 {interface} 失败：{e}"))?;
    socket
        .set_multicast_ttl_v4(1)
        .map_err(|e| format!("设置组播 TTL 失败：{e}"))?;
    // 本机其他实例/回环也要能看到（LocalSend 的默认行为）。
    socket
        .set_multicast_loop_v4(true)
        .map_err(|e| format!("设置组播 loopback 失败：{e}"))?;
    socket
        .set_nonblocking(true)
        .map_err(|e| format!("设置非阻塞失败：{e}"))?;

    Ok(socket.into())
}

/// 不占 53317 的发送 socket：绑定临时端口，只设出口网卡/TTL/loopback，不 join 组播。
///
/// 官方 LocalSend 在 `sendAnnouncement` 里也是 `bind(anyIPv4, 0)`：公告的源端口
/// 不重要，对端看的是 `port` 字段；UDP 53317 被独占时，我们至少还能把公告发出去。
///
/// 与 [`bind_one`] 同一个口径：只建 `std` socket，tokio 的转换由异步调用方
/// [`announce_ephemeral`] 做（它一定在 reactor 里）。
fn ephemeral_socket(interface: Ipv4Addr) -> Result<StdUdpSocket, String> {
    let socket = Socket::new(Domain::IPV4, Type::DGRAM, Some(Protocol::UDP))
        .map_err(|e| format!("创建 UDP socket 失败：{e}"))?;
    let bind_addr = SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0);
    socket
        .bind(&bind_addr.into())
        .map_err(|e| format!("绑定临时 UDP 端口失败：{e}"))?;
    socket
        .set_multicast_if_v4(&interface)
        .map_err(|e| format!("设置组播出口 {interface} 失败：{e}"))?;
    socket
        .set_multicast_ttl_v4(1)
        .map_err(|e| format!("设置组播 TTL 失败：{e}"))?;
    socket
        .set_multicast_loop_v4(true)
        .map_err(|e| format!("设置组播 loopback 失败：{e}"))?;
    socket
        .set_nonblocking(true)
        .map_err(|e| format!("设置非阻塞失败：{e}"))?;
    Ok(socket.into())
}

/// supervisor：持有 socket，起收包/公告/探活三组任务；cancel 时先停子任务再 drop socket。
async fn run(state: Arc<AppState>, sockets: Vec<StdUdpSocket>, cancel: CancellationToken) {
    // **转换必须在这里做**：`run` 由 [`spawn_any`] 起，一定被运行时 poll；而同步入口
    // （设置页开关 / 托盘切在线）没有 reactor，在那里 `from_std` 会 panic —— release
    // 档 `panic = "abort"`，用户侧就是闪退。见 [`bind_sockets`]。
    let sockets: Vec<UdpSocket> = sockets
        .into_iter()
        .filter_map(|sock| match UdpSocket::from_std(sock) {
            Ok(sock) => Some(sock),
            Err(e) => {
                warn!(error = %e, "LocalSend 组播 socket 转 tokio 失败，少一张网卡");
                None
            }
        })
        .collect();
    if sockets.is_empty() {
        warn!("LocalSend 发现没有可用的组播 socket，退出");
        return;
    }
    // 子任务和 supervisor 共享同一批 socket；取消时先 abort 子任务，引用归零后端口释放。
    let sockets: Arc<Vec<Arc<UdpSocket>>> = Arc::new(sockets.into_iter().map(Arc::new).collect());
    let own_fingerprint = Arc::<str>::from(
        net::local_fingerprint()
            .unwrap_or_default()
            .to_ascii_lowercase(),
    );
    let seen = Arc::new(Mutex::new(HashMap::<String, Instant>::new()));
    let mut children = Vec::new();

    for sock in sockets.iter() {
        children.push(tokio::spawn(recv_loop(
            state.clone(),
            sock.clone(),
            cancel.clone(),
            seen.clone(),
            own_fingerprint.clone(),
        )));
    }
    children.push(tokio::spawn(announce_loop(
        state.clone(),
        sockets.clone(),
        cancel.clone(),
    )));
    children.push(tokio::spawn(probe_loop(state.clone(), cancel.clone())));

    cancel.cancelled().await;
    for child in children {
        child.abort();
    }
    // 子任务都已 abort，socket 的最后一个引用随 `sockets` 一起释放（端口立即归还）。
    drop(sockets);
    debug!("LocalSend 发现服务已退出");
}

// ---------------------------------------------------------------------------
// 公告
// ---------------------------------------------------------------------------

/// 公告的线上形状：与 `/register` 的 `DeviceInfo` 相同，多一个 `announce` 标记。
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct Announcement<'a> {
    alias: &'a str,
    version: &'a str,
    device_model: &'a str,
    device_type: &'a str,
    fingerprint: &'a str,
    port: u16,
    protocol: &'a str,
    download: bool,
    announce: bool,
}

fn should_announce(state: &AppState) -> bool {
    super::enabled(state) && state.is_online()
}

async fn announce_loop(
    state: Arc<AppState>,
    sockets: Arc<Vec<Arc<UdpSocket>>>,
    cancel: CancellationToken,
) {
    for delay in ANNOUNCE_BURST {
        if !sleep_or_cancelled(delay, &cancel).await {
            return;
        }
        if !should_announce(&state) {
            info!("LocalSend 已关闭或本机离线，停止公告");
            return;
        }
        announce_once(&state, sockets.as_slice()).await;
    }

    let mut ticker = tokio::time::interval(ANNOUNCE_EVERY);
    ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
    // interval 的第一次 tick 是立即的，公告刚刚发过，先吃掉。
    ticker.tick().await;
    let notify = &state.localsend.announce_notify;
    loop {
        tokio::select! {
            _ = cancel.cancelled() => return,
            _ = notify.notified() => {
                // 53317 刚被接管（或调用方明确要求重播）：立刻把新端口播出去。
                if !should_announce(&state) {
                    info!("LocalSend 已关闭或本机离线，停止公告");
                    return;
                }
                announce_once(&state, sockets.as_slice()).await;
            }
            _ = ticker.tick() => {
                if !should_announce(&state) {
                    info!("LocalSend 已关闭或本机离线，停止公告");
                    return;
                }
                announce_once(&state, sockets.as_slice()).await;
            }
        }
    }
}

/// 组装一条公告。
///
/// `port` 用 [`super::local_info`] 的“对端该拨的端口”：53317 独立监听在跑就是它，
/// 否则是主端口。**不要**直接写 `server_port` —— 那会把 `start_http` 的回落结果
/// 和公告各说各话（`advertised_port` 就白算了）。
fn announcement_payload(state: &AppState) -> Option<Vec<u8>> {
    // 端口 0 表示 HTTP 服务器还没绑上，这时播出去的公告对端连不回来。
    if state.server_port.load(Ordering::SeqCst) == 0 {
        debug!("LocalSend 公告跳过：HTTP 端口尚未绑定");
        return None;
    }
    let info = super::local_info(state);
    if info.port == 0 {
        return None;
    }
    let payload = Announcement {
        alias: &info.alias,
        version: &info.version,
        device_model: info.device_model.as_deref().unwrap_or(""),
        device_type: info.device_type.as_deref().unwrap_or("desktop"),
        fingerprint: &info.fingerprint,
        port: info.port,
        protocol: "https",
        download: false,
        announce: true,
    };
    match serde_json::to_vec(&payload) {
        Ok(bytes) => Some(bytes),
        Err(e) => {
            warn!(error = %e, "序列化 LocalSend 公告失败");
            None
        }
    }
}

async fn announce_once(state: &AppState, sockets: &[Arc<UdpSocket>]) {
    let Some(bytes) = announcement_payload(state) else {
        return;
    };
    let target = SocketAddr::V4(SocketAddrV4::new(MULTICAST_ADDR, DISCOVERY_PORT));
    for sock in sockets {
        match sock.send_to(&bytes, target).await {
            Ok(sent) => debug!(bytes = sent, "发送 LocalSend 公告"),
            Err(e) => debug!(error = %e, "发送 LocalSend 公告失败"),
        }
    }
}

/// 不占 53317 的发送路径：UDP 53317 被别人独占时收不到组播，但公告仍能从临时端口
/// 发出去（官方 LocalSend 的 `sendAnnouncement` 就是这么发的）。对端收到后会走 TCP
/// `register` 回来，所以我们至少不会“两边互相看不见”。
pub async fn announce_ephemeral(state: &AppState) {
    if !scope_allows_discovery() || !should_announce(state) {
        return;
    }
    let Some(bytes) = announcement_payload(state) else {
        return;
    };
    let interfaces = match candidate_interfaces() {
        Ok(interfaces) => interfaces,
        Err(e) => {
            debug!(error = %e, "LocalSend 临时端口公告跳过：没有可用网卡");
            return;
        }
    };
    let target = SocketAddr::V4(SocketAddrV4::new(MULTICAST_ADDR, DISCOVERY_PORT));
    for interface in interfaces {
        // 转换放在这里：这是个 `async fn`，被 poll 时一定在 reactor 里（见 [`bind_sockets`]）。
        let sock = match ephemeral_socket(interface).and_then(|sock| {
            UdpSocket::from_std(sock).map_err(|e| format!("转换 tokio socket 失败：{e}"))
        }) {
            Ok(sock) => sock,
            Err(e) => {
                debug!(%interface, error = %e, "临时端口建 LocalSend 公告 socket 失败");
                continue;
            }
        };
        if let Err(e) = sock.send_to(&bytes, target).await {
            debug!(%interface, error = %e, "临时端口发送 LocalSend 公告失败");
        }
    }
}

/// 睡 `delay`，期间被 cancel 就返回 false。
async fn sleep_or_cancelled(delay: Duration, cancel: &CancellationToken) -> bool {
    tokio::select! {
        _ = cancel.cancelled() => false,
        _ = tokio::time::sleep(delay) => true,
    }
}

// ---------------------------------------------------------------------------
// 收公告
// ---------------------------------------------------------------------------

/// 收到的组播报文：`DeviceInfo` + 可选的 `announce`（未知字段由 serde 忽略）。
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct Incoming {
    #[serde(flatten)]
    info: dto::DeviceInfo,
    #[serde(default)]
    announce: Option<bool>,
}

async fn recv_loop(
    state: Arc<AppState>,
    sock: Arc<UdpSocket>,
    cancel: CancellationToken,
    seen: Arc<Mutex<HashMap<String, Instant>>>,
    own_fingerprint: Arc<str>,
) {
    let mut buf = vec![0u8; 16 * 1024];
    loop {
        let received = tokio::select! {
            _ = cancel.cancelled() => return,
            result = sock.recv_from(&mut buf) => result,
        };
        let (len, from) = match received {
            Ok(value) => value,
            Err(e) => {
                debug!(error = %e, "LocalSend UDP 接收失败");
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let source = match from {
            SocketAddr::V4(addr) => *addr.ip(),
            SocketAddr::V6(_) => continue,
        };
        dispatch_datagram(
            &state,
            &buf[..len],
            source,
            &cancel,
            &seen,
            &own_fingerprint,
        );
    }
}

/// 解析一条公告；合法且不是自己的，就起一个 register 任务（带 2s 超时）。
fn dispatch_datagram(
    state: &Arc<AppState>,
    data: &[u8],
    source: Ipv4Addr,
    cancel: &CancellationToken,
    seen: &Mutex<HashMap<String, Instant>>,
    own_fingerprint: &str,
) {
    let incoming: Incoming = match serde_json::from_slice(data) {
        Ok(value) => value,
        Err(e) => {
            debug!(error = %e, "忽略无法解析的 LocalSend 报文");
            return;
        }
    };
    // `announce: false` 不是公告（部分实现会发其他类型的组播）。
    if incoming.announce == Some(false) {
        return;
    }
    let info = incoming.info;
    let fingerprint = info.fingerprint.trim().to_ascii_lowercase();
    if fingerprint.is_empty() || fingerprint == own_fingerprint {
        return;
    }
    if info.port == 0 {
        debug!(peer = %source, "忽略端口为 0 的 LocalSend 公告");
        return;
    }
    if !info.protocol.eq_ignore_ascii_case("https") {
        debug!(peer = %source, protocol = %info.protocol, "忽略非 HTTPS 的 LocalSend 公告");
        return;
    }

    // 同一报文会从每张网卡各来一次；短窗口去重，避免对同一台设备连发 register。
    let key = format!("{fingerprint}|{source}");
    {
        let now = Instant::now();
        let mut seen = seen.lock();
        if seen
            .get(&key)
            .is_some_and(|at| now.duration_since(*at) < DEDUPE_WINDOW)
        {
            return;
        }
        if seen.len() > 512 {
            seen.retain(|_, at| now.duration_since(*at) < DEDUPE_WINDOW * 5);
        }
        seen.insert(key, now);
    }

    let state = state.clone();
    let cancel = cancel.clone();
    let port = info.port;
    let own_fingerprint = own_fingerprint.to_string();
    // 报文不可认证、来源 IP 可伪造：先按来源限速，再按并发名额封顶。没有这两道闸，
    // 改一改指纹/端口就能让本机对任意地址发起成千上万个 TLS 握手。
    if !state.localsend.allow_datagram(source.into()) {
        debug!(peer = %source, "LocalSend 公告过于频繁，丢弃");
        return;
    }
    let Some(permit) = state.localsend.probe_permit() else {
        debug!(peer = %source, "LocalSend 公告拨号并发已满，丢弃");
        return;
    };
    tokio::spawn(async move {
        let _permit = permit;
        tokio::select! {
            _ = cancel.cancelled() => {}
            result = register_peer(&state, source, port, &fingerprint) => {
                match result {
                    Ok(reg) => {
                        // 本机自己的独立监听也可能在这个端口上：拨过去握到的就是本机证书。
                        // 不拦的话，同机第三方 LocalSend 的公告会把自己登记成一台设备。
                        if reg.fingerprint == own_fingerprint {
                            debug!(peer = %source, "LocalSend register 拨回本机，忽略");
                            return;
                        }
                        let now = Instant::now();
                        // 身份以**握手证书**为准：第三方实现可能把设备 UUID 之类的值填在
                        // `fingerprint` 里（见 `super::announced_pin`），那个值不是证书指纹。
                        state.localsend.upsert(super::LocalSendPeer {
                            id: super::id_for_fingerprint(&reg.fingerprint),
                            fingerprint: reg.fingerprint,
                            alias: reg.info.alias,
                            device_model: reg.info.device_model,
                            device_type: reg.info.device_type,
                            ip: source.to_string(),
                            port,
                            protocol: "https".to_string(),
                            download: reg.info.download,
                            online: true,
                            seen_at: now,
                            last_seen: now,
                            certified: true,
                        });
                        state.emit_devices_changed();
                    }
                    Err(e) => debug!(peer = %source, error = %e, "LocalSend register 失败"),
                }
            }
        }
    });
}

/// 一次成功的 `register`：对端回包 + **握手证书**的指纹（小写）。
struct Registered {
    info: dto::InfoResponse,
    /// 对端真实证书的 SHA-256。公告里的自报值不是 64 位 hex 时，这是唯一能钉的身份
    /// （见 [`super::announced_pin`]）；两者一致时它就是公告值。
    fingerprint: String,
}

/// 向 `https://<ip>:<port>/api/localsend/v2/register` 报一次本机信息。
///
/// 信任表按公告里的指纹钉死（[`net::DialPin::Fingerprint`]），但第三方实现的自报值
/// 不一定是证书哈希（真有填设备 UUID 的）：那一档退到 TOFU —— 拒绝对端没有意义，
/// 它是协议不规范的合法 LocalSend；握手后把真实证书指纹交给调用方钉后续的拨号。
///
/// 这条拨号用 [`net::dial_client_isolated`]：LocalSend 见到的证书**不写** OrbDrop 的
/// 进程级观察值 —— 同一地址上常常同时住着一台 OrbDrop 设备（真机场景：对端既跑
/// OrbDrop 又开着第三方 LocalSend，见 `net::Trust::isolated`）。
async fn register_peer(
    state: &AppState,
    ip: Ipv4Addr,
    port: u16,
    announced: &str,
) -> Result<Registered, String> {
    if port == 0 {
        return Err("对端端口为 0".to_string());
    }
    let host = ip.to_string();
    let pin = super::announced_pin(announced);
    if matches!(pin, net::DialPin::Tofu) {
        debug!(
            %host,
            %port,
            announced,
            "LocalSend 对端自报的指纹不是证书 SHA-256，按 TOFU 认证书"
        );
    }
    let (client, trust) = net::dial_client_isolated(&host, port, &pin)?;
    let url = format!("https://{host}:{port}/api/localsend/v2/register");
    let resp = client
        .post(&url)
        .timeout(Duration::from_secs(2))
        .json(&super::local_info(state))
        .send()
        .await
        .map_err(|e| format!("POST {url} 失败：{e}"))?;
    if !resp.status().is_success() {
        return Err(format!("POST {url} 返回 {}", resp.status()));
    }
    let info = resp
        .json::<dto::InfoResponse>()
        .await
        .map_err(|e| format!("解析 {url} 回包失败：{e}"))?;
    // 握手成功时 `PinnedVerifier` 一定记过这枚证书；拿不到只可能是这条请求没走 TLS。
    let fingerprint = trust
        .observed_at(&host)
        .ok_or_else(|| format!("{url} 的握手没有记录到对端证书指纹"))?;
    Ok(Registered { info, fingerprint })
}

// ---------------------------------------------------------------------------
// 探活 + 清理
// ---------------------------------------------------------------------------

async fn probe_loop(state: Arc<AppState>, cancel: CancellationToken) {
    let mut failures: HashMap<String, u32> = HashMap::new();
    let mut probe = tokio::time::interval(PROBE_EVERY);
    let mut prune = tokio::time::interval(PRUNE_EVERY);
    probe.set_missed_tick_behavior(MissedTickBehavior::Delay);
    prune.set_missed_tick_behavior(MissedTickBehavior::Delay);
    // 两个 interval 的第一次 tick 都是立即的；启动时还没有 peer，直接吃掉。
    probe.tick().await;
    prune.tick().await;

    loop {
        let probe_due = tokio::select! {
            _ = cancel.cancelled() => return,
            _ = probe.tick() => true,
            _ = prune.tick() => false,
        };
        // 两个分支共用 `failures`，所以在 select 之外再执行（select 同时求值各分支）。
        if probe_due {
            probe_all(&state, &mut failures).await;
        } else {
            prune_peers(&state, &mut failures);
        }
    }
}

async fn probe_all(state: &Arc<AppState>, failures: &mut HashMap<String, u32>) {
    if !super::enabled(state) || !state.is_online() {
        return;
    }
    let peers = state.localsend.snapshot();
    if peers.is_empty() {
        return;
    }

    let results = futures_util::future::join_all(peers.into_iter().map(|peer| {
        let state = state.clone();
        async move {
            let ip = peer.ip.parse::<Ipv4Addr>().ok();
            let result = match ip {
                Some(ip) => register_peer(&state, ip, peer.port, &peer.fingerprint).await,
                None => Err(format!("无效的 peer IP：{}", peer.ip)),
            };
            (peer, result)
        }
    }))
    .await;

    let mut changed = false;
    for (peer, result) in results {
        match result {
            Ok(reg) => {
                failures.remove(&peer.id);
                let was_online = peer.online;
                let id = peer.id.clone();
                // 只换元数据；在线位与时间戳交给 touch_alive（成功后一定在线）。
                // 指纹用这次握手看到的（钉死的那条路上与 peer.fingerprint 相同）。
                state.localsend.upsert(super::LocalSendPeer {
                    id: id.clone(),
                    fingerprint: reg.fingerprint,
                    alias: reg.info.alias,
                    device_model: reg.info.device_model,
                    device_type: reg.info.device_type,
                    ip: peer.ip,
                    port: peer.port,
                    protocol: "https".to_string(),
                    download: reg.info.download,
                    online: was_online,
                    seen_at: peer.seen_at,
                    last_seen: peer.last_seen,
                    certified: peer.certified,
                });
                state.localsend.touch_alive(&id);
                // 从离线恢复需要让界面重绘；单纯刷新 last_seen 不必每次 emit。
                if !was_online {
                    changed = true;
                }
            }
            Err(e) => {
                let strikes = failures.entry(peer.id.clone()).or_insert(0);
                *strikes += 1;
                debug!(peer = %peer.id, strikes = *strikes, error = %e, "LocalSend 探活失败");
                if *strikes >= PROBE_FAIL_LIMIT && mark_offline(state, &peer.id) {
                    changed = true;
                }
            }
        }
    }
    if changed {
        state.emit_devices_changed();
    }
}

/// 把 peer 标成离线。`LocalSendState::set_online` 只改在线位，不动 `last_seen`
/// （10 分钟清理会按最后一次成功接触的时间自然收走这一行）。
fn mark_offline(state: &AppState, id: &str) -> bool {
    let was_online = state.localsend.get(id).is_some_and(|peer| peer.online);
    if !was_online {
        return false;
    }
    state.localsend.set_online(id, false);
    true
}

fn prune_peers(state: &AppState, failures: &mut HashMap<String, u32>) {
    let mut removed = false;
    for peer in state.localsend.snapshot() {
        if peer.last_seen.elapsed() > PEER_STALE {
            state.localsend.remove(&peer.id);
            failures.remove(&peer.id);
            removed = true;
            debug!(peer = %peer.id, "LocalSend 设备超过 10 分钟未出现，已移除");
        }
    }
    if removed {
        state.emit_devices_changed();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::AppState;

    /// 第三方 LocalSend 把设备 UUID 填在公告的 `fingerprint` 里（真机日志：
    /// `E44E30D3-4553-4D…`）。那不是证书哈希，拿去钉 TLS 会让每次 register 都报
    /// "指纹不一致"，一台正常的第三方设备永远发现不了。正确行为：这一档按 TOFU
    /// 认出证书，并用**握手证书**的指纹作为后续拨号的身份。
    #[tokio::test]
    async fn uuid_announcement_falls_back_to_the_certificate() {
        let mut config = crate::config::test_config();
        config.localsend_enabled = true;
        let config_path =
            std::env::temp_dir().join(format!("orbdrop_ls_disc_{}.toml", uuid::Uuid::new_v4()));
        let state = Arc::new(AppState::new(config, config_path));
        let port = crate::server::start_server_loopback(state.clone(), 4400)
            .await
            .expect("server start");
        let own = crate::net::local_fingerprint().unwrap().to_ascii_lowercase();

        let reg = register_peer(
            &state,
            Ipv4Addr::LOCALHOST,
            port,
            "E44E30D3-4553-4D02-9A1B-7C6F5E4D3C2B",
        )
        .await
        .expect("UUID 公告应按 TOFU 认出证书");
        assert_eq!(reg.fingerprint, own, "身份以握手证书为准");
        assert_eq!(reg.info.alias, state.config.lock().device_name);

        // 自报值确实是证书哈希时照旧钉死：命中放行。
        let reg = register_peer(&state, Ipv4Addr::LOCALHOST, port, &own.to_ascii_uppercase())
            .await
            .expect("正确的证书指纹应通过（大小写不敏感）");
        assert_eq!(reg.fingerprint, own);

        // 自报值形状正确但证书对不上：拒绝（这一档不能退 TOFU，否则钉住就失去意义）。
        assert!(
            register_peer(&state, Ipv4Addr::LOCALHOST, port, &"ab".repeat(32))
                .await
                .is_err(),
            "指纹与证书不符必须拒绝"
        );

        // 整条发现链路：公告里是 UUID，登记出来的行必须按握手证书的指纹（而不是 UUID）。
        let datagram = serde_json::to_vec(&serde_json::json!({
            "alias": "Third-party LocalSend",
            "version": "2.2",
            "deviceModel": "Test",
            "deviceType": "desktop",
            "fingerprint": "E44E30D3-4553-4D02-9A1B-7C6F5E4D3C2B",
            "port": port,
            "protocol": "https",
            "download": false,
            "announce": true,
        }))
        .unwrap();
        let cancel = CancellationToken::new();
        let seen: Mutex<HashMap<String, Instant>> = Mutex::new(HashMap::new());
        dispatch_datagram(
            &state,
            &datagram,
            Ipv4Addr::LOCALHOST,
            &cancel,
            &seen,
            &"00".repeat(32),
        );
        let mut row = None;
        for _ in 0..100 {
            if let Some(peer) = state.localsend.snapshot().into_iter().next() {
                row = Some(peer);
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        let row = row.expect("UUID 公告也应登记出一行");
        assert_eq!(row.fingerprint, own, "行身份取握手证书");
        assert_eq!(row.id, super::super::id_for_fingerprint(&own));
        assert_eq!(row.port, port);
        assert!(row.online);

        crate::server::stop_server(&state).await;
    }

    /// 公告里的 `port` 必须是对端该拨的那一个：53317 独立监听在跑时是它，
    /// 不是主端口（`advertised_port` 与 `server_port` 各自的值）。
    #[test]
    fn announcement_uses_the_advertised_port() {
        let mut config = crate::config::test_config();
        config.localsend_enabled = true;
        let state = Arc::new(AppState::new(
            config,
            std::env::temp_dir().join(format!(
                "orbdrop_ls_announce_{}.toml",
                uuid::Uuid::new_v4()
            )),
        ));
        state.server_port.store(3890, Ordering::SeqCst);
        state.localsend.port.store(53317, Ordering::SeqCst);

        let bytes = announcement_payload(&state).expect("公告应能序列化");
        let value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(value["port"], 53317);
        assert_eq!(value["version"], dto::PROTOCOL_VERSION);

        state.localsend.port.store(0, Ordering::SeqCst);
    }
}
