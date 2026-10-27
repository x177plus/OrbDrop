//! 中转（F-106 / S-09）：替"够不着彼此、但都够得着本机"的两台设备**盲转字节**。
//!
//! 用在一种网络里：AP 把同一 Wi‑Fi 上客户端之间的**单播**隔开了（组播还能过 —— 所以
//! 设备列表会"闪一下"；单播全灭 —— 所以 ARP 应答、TCP 握手、mDNS 应答后的直连全都失败）。
//! 于是 A 与 B 各自只够得着第三台设备 R。R 为这一对开一条监听：A 拨上来，R 连到 B，
//! 之后两个方向原样搬字节。**R 看不到内容**（两端 TLS 端到端不变），也看不懂协议。
//!
//! 四条边界：
//! - **只在直连不通时用**：请求方由 5s 探活判定（`state::probe_peers_once`），直连一恢复
//!   就回退（`clear_for`），中转端点只在"直连这一跳不通、且中转这一跳通"时留在行上。
//! - **要中转方点头**：中转方必须打开设置里的「允许为其它设备中转」（默认关）；关着时
//!   控制面一律拒绝，已经开出去的也会被收掉（`update_settings` → [`close_all`]）。
//! - **单跳**：R 连目标只用目标**自己的直连地址**（`ip`/`port`），不借别家的中转 ——
//!   链式中转会把"谁在替谁搬"这件事变得说不清，也让带宽代价叠着涨。
//! - **不认识的不转**：目标必须是 R 地址簿里**此刻在线**的设备（R 的 5s 探活刚刚验证过），
//!   且不是请求方自己、不是 R 自己。
//!
//! 中转端点的**地址字面量**是 IPv4 映射的 IPv6（`net::relay_host`）：端点上游呈现的是
//! 目标设备的证书，而指纹表按地址字面量索引 —— 直接用 R 的 IPv4 会与 R 自己的服务撞
//! 同一个键位（同地址两枚指纹 = 不钉，退 TOFU，见 `Trust::seed`）。换字面量就各占一档。
//!
//! ⚠️ **已知限制**：Windows 上把一个 IPv4 映射的 IPv6 地址 `connect()` 出去需要 socket 上
//! 的 `IPV6_V6ONLY=0`，而 reqwest/hyper 自己建的 socket 不带这个标志（Windows 默认 v6-only）
//! —— 于是**Windows 作为请求方**时连不上这条回退形式（Linux/macOS/Android 的默认是双栈，
//! 实测可连）。所以 [`pick_own_ipv4`] 优先挑"同网段里的另一块本机网卡地址"：那样端点是
//! 一个普通的、键位不同的 IPv4，任何平台都能连。只有本机在请求方网段上只有**一个**地址时
//! 才回落到映射形式（那种机器上 Windows 请求方这一跳暂时连不上，日志里会看到
//! `AddrNotAvailable`）。彻底解法是"指纹按设备钉而不是按地址钉"（要改信任模型），见文档。

use std::collections::HashMap;
use std::net::{IpAddr as StdIpAddr, SocketAddr};
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tauri::Emitter;
use tokio::io::{copy_bidirectional, AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{watch, Semaphore};
use tracing::{debug, info, warn};

use crate::config::RelayAddr;
use crate::state::AppState;

// ---------------------------------------------------------------------------
// 常量：都是"说不清时按更保守的那一边"定的
// ---------------------------------------------------------------------------

/// 本机最多同时替别人开几条中转。8 条覆盖"几台手机互传"，又不至于让一台机器被当跳板。
pub const MAX_RELAYS: usize = 8;

/// 同一位请求方两次开中转之间的最小间隔（控制面上的闸门，防"每 5s 探活各开一条"）。
const CALLER_COOLDOWN: Duration = Duration::from_secs(3);

/// 中转目标不能是本机的**回环/未指定/组播/链路本地**地址：中转是裸 TCP，连到回环之后
/// 目标监听会把这条连接当成"本机进程"（`net::dispatch_conn` 只对回环放行明文，
/// `auth_middleware` 又对回环给 `Caller::Local`）。一旦请求方能指定
/// `127.0.0.1:<本机端口>`，就能借中转拿到本机 UI 的免认证权限 —— 必须堵死。
///
/// 本机**普通 LAN 地址**不在这里拒：中转过去时目标看到的来源是 LAN 地址（不是回环），
/// 仍要 token 才能进；同机多实例/集成测试也靠它模拟"另一台设备"。
///
/// IPv4 映射的 IPv6 地址先折回 IPv4 再判：`[::ffff:127.0.0.1]` 的 `is_loopback()` 是
/// false，但实际连的就是回环。
fn canonical_relay_ip(ip: StdIpAddr) -> StdIpAddr {
    match ip {
        StdIpAddr::V6(v6) => v6
            .to_ipv4_mapped()
            .map(StdIpAddr::V4)
            .unwrap_or(StdIpAddr::V6(v6)),
        other => other,
    }
}

fn forbidden_relay_target(ip: StdIpAddr) -> bool {
    let ip = canonical_relay_ip(ip);
    if ip.is_loopback() && loopback_relay_targets_allowed_for_tests() {
        return false;
    }
    if ip.is_loopback() || ip.is_unspecified() || ip.is_multicast() {
        return true;
    }
    match ip {
        StdIpAddr::V4(v4) => v4.is_link_local() || v4.is_broadcast(),
        StdIpAddr::V6(v6) => v6.is_unicast_link_local(),
    }
}

/// 测试专用：多个 `AppState` 跑在同一个进程里时，目标只能写 `127.0.0.1`。
///
/// **只在 debug 构建里生效**，而且必须显式设置环境变量；release 构建永远返回 false。
/// 集成测试的夹具会设置它。正常运行时这个分支不可达，不会放宽回环目标的拒绝。
fn loopback_relay_targets_allowed_for_tests() -> bool {
    cfg!(debug_assertions)
        && std::env::var_os("ORBDROP_TEST_ALLOW_LOOPBACK_RELAY").is_some()
}

/// 中转端点的空闲回收：**没有活着的连接**且这么久没有新连接，就把它收掉。
///
/// 只在"一条连接都没有"时计时：中转的典型用途是一条长连接（`/events` 的 WS）+ 若干短
/// 请求，长时间没有新连接但有活连接是正常的（一次大文件传输就是这样），那不算空闲。
/// 60s（不是 120s）：请求方自己 30s 重试一次，**同一对已经有端点时走复用**（见 `open`）；
/// 请求方真的走了，端点最多一分钟就收，不再靠两条旧端点空转。
const IDLE_TIMEOUT: Duration = Duration::from_secs(60);

/// 中转方向连目标那一跳的超时。连不上就断，不让请求方挂在一个永远不动的 socket 上。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// 请求方侧的限流：同一台"够不着的"设备，最多这么久尝试开一次中转。
///
/// 比探活节拍（5s）长得多是刻意的：开中转要问一台设备、它还要连一次目标，失败一遍
/// 不便宜；而"够不着"通常不是几秒内会变的事。
const REQUEST_COOLDOWN: Duration = Duration::from_secs(30);

/// 控制面调用（`/relay/open`）的超时。
const OPEN_TIMEOUT: Duration = Duration::from_secs(6);

/// 目标地址刚连不上时的冷却：这期间不再为它**新开**端点。
///
/// 请求方（`ensure_for`）每 30s 重试一次；没有这一档时，一台目标的端口死了，Mac 就会每
/// 30s 新开一个监听器 + 一条注定超时的出站连接（日志里"开了中转端点"一串新端口就是这么来的）。
/// 60s = 隔一次重试才放行一次，足够让"目标恢复了"在一分钟内被发现。
const RELAY_FAIL_COOLDOWN: Duration = Duration::from_secs(60);

/// 全局同时搬运的中转连接上限（每条占**两个** socket：入站 + 出站）。
///
/// 256 fd 的机器上，基线（mDNS/长连接/探活/文件）已经 150 左右；24 条 = 48 个 socket，
/// 把突发压在 200 上下。超出时**丢弃新连接**（不排队）：请求方是短连接重试模型，丢一条
/// 比把本机 fd 打满、连 UI 的 HTTP 都 accept 不了要好。
const MAX_RELAY_STREAMS: usize = 24;

/// 单个端点同时搬运的连接上限（一次传输的并行度通常 ≤8）。
const MAX_RELAY_STREAMS_PER_ENDPOINT: usize = 8;

// ---------------------------------------------------------------------------
// 注册表：本机替别人开出去的中转端点
// ---------------------------------------------------------------------------

#[derive(Debug)]
struct ActiveRelay {
    requester: String,
    target: String,
    port: u16,
    /// 本机给请求方的端点地址（`pick_own_ipv4` → `net::relay_host` 的字面量）。
    /// 复用同一对 (请求方, 目标) 时原样回给请求方，不能再算一次（算出来可能换了网卡）。
    host: String,
    /// 这条端点要连的目标地址；复用与控制面回话都用它。
    target_addr: SocketAddr,
    /// 关停信号：`send(true)` 让监听任务与它底下所有在跑的连接一起收。
    stop: watch::Sender<bool>,
}

/// 本机开出去的中转端点（`AppState::relays`）。
#[derive(Debug)]
pub struct RelayRegistry {
    inner: Mutex<HashMap<u64, ActiveRelay>>,
    /// 控制面限流：`请求方 id → 上次开中转的时刻`。
    last_open: Mutex<HashMap<String, Instant>>,
    next_id: AtomicU64,
    /// 全局同时在跑的中转连接额度（见 [`MAX_RELAY_STREAMS`]）。
    streams: Arc<Semaphore>,
    /// 刚连不上的目标地址 → 失败时刻（见 [`RELAY_FAIL_COOLDOWN`]）。
    failures: Mutex<HashMap<SocketAddr, Instant>>,
}

impl Default for RelayRegistry {
    fn default() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
            last_open: Mutex::new(HashMap::new()),
            next_id: AtomicU64::new(0),
            streams: Arc::new(Semaphore::new(MAX_RELAY_STREAMS)),
            failures: Mutex::new(HashMap::new()),
        }
    }
}

impl RelayRegistry {
    pub fn active(&self) -> usize {
        self.inner.lock().len()
    }

    /// 同一对 (请求方, 目标) 已经开着的端点：复用它的端口与地址。
    fn find(&self, requester: &str, target: &str) -> Option<(u16, String, SocketAddr)> {
        self.inner
            .lock()
            .values()
            .find(|r| r.requester == requester && r.target == target)
            .map(|r| (r.port, r.host.clone(), r.target_addr))
    }

    /// 全局中转连接额度（`serve` 的 accept 循环用它兜住突发）。
    fn streams(&self) -> Arc<Semaphore> {
        Arc::clone(&self.streams)
    }

    /// 记一次"目标地址连不上"：冷却期内 `open` 不再为它新开端点。
    fn target_failed(&self, addr: SocketAddr) {
        let now = Instant::now();
        let mut f = self.failures.lock();
        f.retain(|_, at| now.saturating_duration_since(*at) < RELAY_FAIL_COOLDOWN);
        f.insert(addr, now);
    }

    fn target_cooling(&self, addr: SocketAddr) -> bool {
        let now = Instant::now();
        let mut f = self.failures.lock();
        f.retain(|_, at| now.saturating_duration_since(*at) < RELAY_FAIL_COOLDOWN);
        f.contains_key(&addr)
    }

    /// 目标连上了：清掉失败冷却（下一次换地址/恢复时立刻可用）。
    fn target_ok(&self, addr: SocketAddr) {
        self.failures.lock().remove(&addr);
    }

    /// 控制面限流：同一位请求方在 [`CALLER_COOLDOWN`] 内只放一次。
    fn caller_allowed(&self, requester: &str) -> bool {
        let now = Instant::now();
        let mut last = self.last_open.lock();
        match last.get(requester) {
            Some(prev) if now.duration_since(*prev) < CALLER_COOLDOWN => false,
            _ => {
                last.insert(requester.to_string(), now);
                true
            }
        }
    }

    fn insert(&self, relay: ActiveRelay) -> Result<u64, ()> {
        let mut inner = self.inner.lock();
        if inner.len() >= MAX_RELAYS {
            return Err(());
        }
        let id = self.next_id.fetch_add(1, Ordering::SeqCst) + 1;
        inner.insert(id, relay);
        Ok(id)
    }

    fn remove(&self, id: u64) {
        self.inner.lock().remove(&id);
    }

    /// 收掉本机开出去的全部中转（关闭开关、退出时用）。
    fn close_all(&self) {
        let mut inner = self.inner.lock();
        for relay in inner.values() {
            let _ = relay.stop.send(true);
        }
        inner.clear();
    }

    /// 诊断用：当前开出去的端点（请求方、目标、端口）。
    pub fn snapshot(&self) -> Vec<(String, String, u16)> {
        self.inner
            .lock()
            .values()
            .map(|r| (r.requester.clone(), r.target.clone(), r.port))
            .collect()
    }
}

/// 收掉本机开出去的全部中转（设置里把开关关掉时由 `lib.rs` 调用）。
pub fn close_all(state: &Arc<AppState>) {
    state.relays.close_all();
}

// ---------------------------------------------------------------------------
// 中转方：控制面
// ---------------------------------------------------------------------------

/// 开中转失败的原因。状态码在路由层映射（见 `server::routes_relay`）。
#[derive(Debug)]
pub enum OpenError {
    /// 中转方没打开「允许为其它设备中转」。
    Disabled,
    /// 请求方把自己当中转目标、或目标压根不在本机地址簿里、或目标此刻不在线。
    NotEligible(String),
    /// 控制面限流 / 并发上限。
    Busy(String),
    /// 绑定监听或挑本机地址失败（本机没有可用的局域网地址）。
    Unavailable(String),
}

impl OpenError {
    pub fn status(&self) -> axum::http::StatusCode {
        use axum::http::StatusCode as S;
        match self {
            OpenError::Disabled => S::FORBIDDEN,
            OpenError::NotEligible(_) => S::NOT_FOUND,
            OpenError::Busy(_) => S::TOO_MANY_REQUESTS,
            OpenError::Unavailable(_) => S::SERVICE_UNAVAILABLE,
        }
    }

    pub fn message(&self) -> &str {
        match self {
            OpenError::Disabled => "本机没有打开「允许为其它设备中转」",
            OpenError::NotEligible(m) | OpenError::Busy(m) | OpenError::Unavailable(m) => m,
        }
    }
}

/// 开出去的中转端点（请求方拿它当目标设备的地址用）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Opened {
    pub host: String,
    pub port: u16,
    pub target: String,
    pub expires_in_secs: u64,
    /// F-107：本机（中转方）**自己所在的那一段**（请求方据此知道"这一段在哪"）。
    pub via_cidr: String,
    /// F-107：本机那一段里**此刻愿意中转**的设备（含本机自己）。
    ///
    /// 请求方**不必与本机配对**就能拿到它 —— 否则"没配对就借中转"那条路虽然能借到当前这一台，
    /// 却永远不知道那一段里还有谁，中转方掉线之后没法自动换一台（F-107 §8.2）。
    /// 这是**提示**：请求方拿到之后仍然按"先直连、再按池顺序试、目标身份由中转方核实"用。
    #[serde(default)]
    pub via_devices: Vec<RouteDevice>,
    /// F-107：**目标此刻的地址**（本机刚才用它连上/核实过的那一个，`ip:port`，算不出来时为空）。
    ///
    /// 为什么给请求方：只经中转见过一面的对端（AP 隔离下被中转的那一侧）手里**没有**目标的
    /// 地址 —— 那一侧的设备行就只能显示 `:0`，也拿不出下一次请中转要用的 `addr` 提示。
    /// 这一格是"我们刚替你核实过它在哪"：请求方拿它补上自己那一行（只补空、不覆盖已有的），
    /// 于是设备行说得清地址与状态，下一次借中转也不必再让中转方去猜。
    #[serde(default)]
    pub target_addr: String,
}

/// 替 `requester` 到 `target` 开一条中转端点（`POST /relay/open`）。
///
/// `from` 是请求方在控制面上的来源地址：只有**它**（以及它此刻在地址簿里的地址）能被
/// 允许拨这个端点 —— 端点本身是匿名的 TCP 监听，来源白名单是它唯一的门。
///
/// `hint` = 请求方给出的目标地址（`host:port`），只在本机**不认识**目标、或目标在本机
/// 视野里不在线时用；用之前必须核实"那个地址上的设备自报的 id 就是 `target`"。
///
/// `trusted_caller` = 请求方的身份是**凭据（Bearer）核实过的已配对对端**（`false` = 它只是
/// 自报身份 + 被到地址上核实过 —— 见 `server::routes_relay`）。这一个位决定端点来源白名单的
/// 宽窄：可信调用方额外认"它在地址簿里的地址"（换过 DHCP 也能回来），自报的那一档**只认它
/// 控制面的来源地址** —— 否则冒名者可以凭空让**别人**（地址簿里那台设备）也能拨这条端点。
pub async fn open(
    state: &Arc<AppState>,
    requester: &str,
    from: StdIpAddr,
    target: &str,
    hint: Option<(String, u16)>,
    trusted_caller: bool,
) -> Result<Opened, OpenError> {
    if !state.config.lock().allow_relay {
        return Err(OpenError::Disabled);
    }
    // 本机自己既不能当目标、也不能当请求方：请求方那一半是"选路把自己当成了中转候选"
    // （池里留着一条指向本机的旧地址），会在核实阶段把自己登记成一台设备。
    let own_id = state.own_id().to_string();
    if requester == own_id.as_str() {
        return Err(OpenError::NotEligible(
            "不能把本机自己作为中转请求方".into(),
        ));
    }
    if requester == target {
        return Err(OpenError::NotEligible(
            "不能把请求方自己作为中转目标".into(),
        ));
    }
    // 本机自己不能当中转目标：本机不在地址簿里，请求方却可以拿 `/pair/info` 问出本机 id
    // 再把 `addr` 填成 `127.0.0.1:<本机端口>`。那会让裸 TCP 中转连回自己的回环监听，
    // 目标端于是把这条连接当成本机 UI（免认证）—— 见 `forbidden_relay_target` 的说明。
    if target == own_id.as_str() {
        debug!(%target, "中转请求被拒：不能把本机自己作为中转目标");
        return Err(OpenError::NotEligible(
            "不能把本机自己作为中转目标".into(),
        ));
    }

    // 目标优先取本机地址簿里**此刻在线**的那一台（本机的 5s 探活刚验证过它够得着）；
    // 「学来的」行不算（那是"担保人报的"，本机跟它之间没有过关系）。
    //
    // 本机不认识它、或它此刻不在线时，退回**请求方给出的地址**：跨网段的中转方常常
    // "够得着目标却从没认识过它"（mDNS 跨不过路由，手动加的还可能是旧 id）—— 这一条就是
    // 给那种情形留的。但那份地址是一份**声明**，必须核实成事实：地址上那台设备要自报
    // **同一个 device id**。否则中转方就成了"给任意局域网地址开隧道"的工具。
    let known = {
        let learned = state.learned.lock().contains_key(target);
        if learned {
            None
        } else {
            state.peers.lock().iter().find(|p| p.id == target).cloned()
        }
    };
    // 本机**记得**的那份地址（哪怕它此刻不算"在线" —— 例如只是刚才核实过一次留下的）
    // 也当 hint 用：与请求方捎来的那一个走**同一关**（到那个地址问一次 `/pair/info`）。
    // 少了这一档，"本机核实过一次、但 5s 探活还没轮到它"的目标还是会 404。
    let hint = hint.or_else(|| {
        known
            .as_ref()
            .filter(|p| !p.ip.is_empty() && p.port != 0)
            .map(|p| (p.ip.clone(), p.port))
    });
    let (target_name, target_addr, target_in_book, verified_name) = match known {
        Some(peer) if peer.online && !peer.ip.is_empty() && peer.port != 0 => {
            let Some(addr) = socket_addr(&peer.ip, peer.port) else {
                return Err(OpenError::NotEligible(format!(
                    "设备 {} 的地址不可用",
                    peer.name
                )));
            };
            // 地址簿被投毒、或配置里手写了回环/链路本地地址时，同样不能开这一跳。
            // 本机 LAN 地址不拒：中转过去时目标看到的来源是 LAN 地址（不是 127.0.0.1），
            // 仍要 token 才能进；拒掉它反而会误伤同机多实例/测试。
            if forbidden_relay_target(addr.ip()) {
                debug!(%target, addr = %addr, "中转请求被拒：目标地址是本机回环/特殊地址");
                return Err(OpenError::NotEligible(
                    "目标地址不可用：中转目标不能是本机回环".into(),
                ));
            }
            (peer.name.clone(), addr, true, None)
        }
        _ => {
            let Some((host, port)) = &hint else {
                return Err(OpenError::NotEligible(format!(
                    "本机不认识设备 {target}（请求方也没给出可核验的地址）"
                )));
            };
            let Some(addr) = socket_addr(host, *port) else {
                return Err(OpenError::NotEligible(format!("地址 {host}:{port} 不合法")));
            };
            // **先于 `verify_identity_at`**：回环地址本身就能答出本机 id，先问再判等于
            // 已经让本机朝自己的回环监听发了一次请求；这里直接拒，连问都不问。
            if forbidden_relay_target(addr.ip()) {
                debug!(%target, addr = %addr, "中转目标地址是本机回环/特殊地址，已拒绝");
                return Err(OpenError::NotEligible(
                    "目标地址不可用：中转目标不能是本机回环".into(),
                ));
            }
            let observed = match verify_identity_at(addr, target).await {
                Ok(observed) => observed,
                Err(why) => {
                    // **细节只进日志，不回给调用方**：回话里带着"那个地址上是谁 / 通不通"，
                    // 而调用方可能只是个自报身份、我们并不认识它。把 id 回给它，等于把"绕过
                    // 核实那一关所需的 id"亲手交出去 —— 它下一句就能点名那个 id，让本机去连
                    // 一个它自己够不着的地址（那正是核实这一关要防的"给任意地址开隧道"）。
                    debug!(addr = %addr, target = %target, "目标身份核实不过: {why}");
                    return Err(OpenError::NotEligible(
                        "核实不了目标身份（本机不认识这台设备，给的地址上也没问出它）".into(),
                    ));
                }
            };
            (target.to_string(), addr, false, Some(observed))
        }
    };

    // 刚才这一路把目标的地址**核实过了**（地址簿里那一条，或请求方给的那个 hint 上问出的
    // 身份）：记下来（只补空行）—— 中转方认识的设备越多，"请求方手里没地址"这件事就越少
    // 成为死路（真机上就是它让反方向的第一次借中转卡在"本机不认识设备 X"）。
    state.observe_verified_addr(target, &target_addr.ip().to_string(), target_addr.port());
    // 刚才那一问（只在第二档发生）**证明了两件事**：那个地址上答得出这个 id，而且它此刻
    // 活着 —— 与 5s 探活同一级的证据，记进"刚听到过"那一档（见 `AppState::heard`）；
    // 顺手把问到的名字填进占位行（本机记着的真名一个字都不动）。
    if let Some(observed) = verified_name {
        state.note_heard_online(target);
        if let Some(name) = observed {
            // 核实过的名字（在那个地址上问出来的）→ 直接更新（对端改名跟着走）。
            state.note_peer_name(target, &name);
        }
    }

    // 限流放在**所有资格判定之后**：一次说不清的请求（目标不认识、自己当目标）不该
    // 把这位请求方接下来几秒的额度吃掉 —— 那就成了"报错一次，三秒内连正经请求也开不了"。
    //
    // 键用**控制面的来源地址**、不是它自报的 id：自报的 id 只由公开的 `/pair/info` 背书，
    // 换一个名字就能重来一遍 —— 那样 8 条并发额度在几秒内就能被一台机器占满。
    if !state.relays.caller_allowed(&from.to_string()) {
        return Err(OpenError::Busy("开得太频繁，稍后再试".into()));
    }

    // 端点地址：本机在**请求方那个网段**上的地址（请求方能直连本机，靠的就是它）。
    let Some(own_ip) = pick_own_ipv4(from) else {
        return Err(OpenError::Unavailable(
            "本机没有可用的局域网 IPv4 地址".into(),
        ));
    };
    let Some(host) = crate::net::relay_host(&own_ip) else {
        return Err(OpenError::Unavailable(format!("地址 {own_ip} 不是 IPv4")));
    };
    // 本机在这个请求方眼里属于哪一段（端点上用的是这块网卡地址，所以就是它的网段）。
    let own_net = own_ip
        .parse::<std::net::Ipv4Addr>()
        .ok()
        .and_then(crate::subnet::local_net_of);

    // 同一对请求方→目标已经有端点：**复用**，不再新开监听。
    //
    // 请求方（`ensure_for`）在目标暂时不可达时每 30s 重问一次控制面；每次都新开监听器 +
    // 出站连接，旧端点还要空转 120s，日志里就是一串不同端口。复用只花一次查表。
    if let Some((port, host, target_addr)) = state.relays.find(requester, target) {
        info!(%requester, %target, port, "复用已开的中转端点");
        return Ok(Opened {
            host,
            port,
            target: target.to_string(),
            expires_in_secs: IDLE_TIMEOUT.as_secs(),
            via_cidr: own_net
                .as_ref()
                .map(|c| c.to_cidr_string())
                .unwrap_or_default(),
            via_devices: own_net
                .as_ref()
                .map(|c| state.relay_devices_in(c, &[requester, target]))
                .unwrap_or_default(),
            target_addr: target_addr.to_string(),
        });
    }
    // 刚连不上的目标地址：冷却期内不再新开端点。请求方 30s 后会再问一次，那一次才放行。
    if state.relays.target_cooling(target_addr) {
        debug!(%target, %target_addr, "目标刚连不上，冷却中，暂不新开端点");
        return Err(OpenError::Unavailable("目标刚连不上，稍后再试".into()));
    }

    let listener = TcpListener::bind(("0.0.0.0", 0))
        .await
        .map_err(|e| OpenError::Unavailable(format!("绑定中转监听失败: {e}")))?;
    let port = listener
        .local_addr()
        .map_err(|e| OpenError::Unavailable(e.to_string()))?
        .port();

    let (stop, stop_rx) = watch::channel(false);
    let id = state
        .relays
        .insert(ActiveRelay {
            requester: requester.to_string(),
            target: target.to_string(),
            port,
            host: host.clone(),
            target_addr,
            stop,
        })
        .map_err(|_| OpenError::Busy(format!("本机同时在转的中转已达上限（{MAX_RELAYS}）")))?;

    info!(
        requester = %requester,
        target = %target,
        target_name = %target_name,
        in_book = target_in_book,
        host = %host,
        port,
        "开了中转端点"
    );

    let task_state = Arc::clone(state);
    let requester_owned = requester.to_string();
    let target_id = target.to_string();
    tokio::spawn(async move {
        serve(
            task_state,
            id,
            listener,
            requester_owned,
            target_id,
            target_addr,
            target_in_book,
            trusted_caller,
            from,
            stop_rx,
        )
        .await;
    });

    Ok(Opened {
        host,
        port,
        target: target.to_string(),
        expires_in_secs: IDLE_TIMEOUT.as_secs(),
        // 本机（中转方）自己那一段 + 那一段里此刻愿意中转的设备（含本机自己）：
        // 请求方靠它知道"这一段里还有谁" —— 中转方掉线时才换得动（F-107 §8.2）。
        via_cidr: own_net
            .as_ref()
            .map(|c| c.to_cidr_string())
            .unwrap_or_default(),
        via_devices: own_net
            .as_ref()
            .map(|c| state.relay_devices_in(c, &[requester, target]))
            .unwrap_or_default(),
        target_addr: format!("{}:{}", target_addr.ip(), target_addr.port()),
    })
}

/// `host:port` → `SocketAddr`（主机字面量容忍 URL 里的方括号形式）。
fn socket_addr(host: &str, port: u16) -> Option<SocketAddr> {
    Some(SocketAddr::new(crate::net::parse_host(host)?, port))
}

/// 到 `addr` 问一次 `/pair/info`，确认它就是 `expected`。
///
/// 请求方给出的地址是一份**声明**，这里把它核实成事实：中转只连**真 OrbDrop 对端**，
/// 而且必须是它自称的那一台 —— 否则中转方就成了"给任意局域网地址开隧道"的工具。
///
/// 两处用它：① 核实**目标**（请求方捎来的 `addr`）；② 核实**请求方自己**（F-107 §12.1
/// 放宽之后，非配对设备也能借中转，那时它必须自报 `id`，这里到「它的 TCP 来源地址 + 自报
/// 端口」上问一次，自报 id 对不上就拒）。
///
/// 成功时把回话里的**名字**一起带回去（`None` = 它没报名字）：那是一份**核实过的**观察值，
/// 调用方拿它更新名字（`AppState::note_peer_name`：对端改名跟着走）。核实成功同时还意味着
/// "它此刻活着" —— 与 5s 探活同一级的证据（见 `AppState::heard`）。
pub(crate) async fn verify_identity_at(
    addr: SocketAddr,
    expected: &str,
) -> Result<Option<String>, String> {
    let url = format!(
        "{}/pair/info",
        crate::net::peer_base(&addr.ip().to_string(), addr.port())
    );
    let response = match OPEN_CLIENT.get(&url).send().await {
        Ok(r) => r,
        Err(e) => return Err(format!("地址 {addr} 连不上：{e}")),
    };
    if !response.status().is_success() {
        return Err(format!(
            "地址 {addr} 回的是 {}（不是 OrbDrop 对端？）",
            response.status()
        ));
    }
    // 这个地址是**调用方给的声明**，上面跑的不一定是 OrbDrop：TOFU 会接受它的自签证书，
    // 所以回话体积必须自己卡死（`.json()` 会先把整个身体读进内存）。
    match crate::net::read_json_capped::<serde_json::Value>(
        response,
        crate::net::PAIR_INFO_BODY_MAX,
    )
    .await
    {
        Ok(body) if body["id"].as_str() == Some(expected) => Ok(body["name"]
            .as_str()
            .map(str::to_string)
            .filter(|n| !n.trim().is_empty())),
        Ok(body) => Err(format!(
            "地址 {addr} 上是另一台设备（{}）",
            body["id"].as_str().unwrap_or("问不出身份")
        )),
        Err(e) => Err(format!("地址 {addr}：{e}")),
    }
}

/// 挑一个"请求方够得着"的本机 IPv4 当端点地址：优先**请求方所在网段里的另一块网卡地址**
/// （同一个网段里本机有两块网卡时，那一块请求方也够得着，而且**键位与主地址不同** ——
/// 于是不必动用下面那条 IPv4 映射的形式，Windows 请求方也能连），其次按界面那套偏好排序
/// 取第一个（地址与主地址相同 → 回落成 IPv4 映射的 IPv6 字面量，见 `net::relay_host`）。
fn pick_own_ipv4(from: StdIpAddr) -> Option<String> {
    let local = crate::server::local_ipv4_addresses();
    if let StdIpAddr::V4(from) = from {
        let want = from.octets();
        let same_subnet: Vec<&String> = local
            .iter()
            .filter(|ip| {
                let mut it = ip.split('.').filter_map(|s| s.parse::<u8>().ok());
                matches!((it.next(), it.next(), it.next()), (Some(a), Some(b), Some(c)) if [a, b, c] == want[..3])
            })
            .collect();
        // 同网段里有多块网卡：挑**最后**一个（`local_ipv4_addresses()` 按"该先报哪个"排过序，
        // 第一个是主地址 —— 请求方记的多半就是它，换一个才拿到不同的指纹键位）。
        if let Some(alt) = same_subnet.last() {
            return Some((*alt).clone());
        }
    }
    local.into_iter().next()
}

// ---------------------------------------------------------------------------
// 中转方：监听与盲转
// ---------------------------------------------------------------------------

/// 一条中转端点的监听任务：accept → 验来源 → 连目标 → 双向搬字节。
///
/// 生命周期：请求方关掉开关 / 目标离线 / 空闲超时 / 进程退出。任何一条路径退出都要
/// 把自己从注册表里摘掉（否则并发额度会被死端点占满）。
#[allow(clippy::too_many_arguments)]
async fn serve(
    state: Arc<AppState>,
    id: u64,
    listener: TcpListener,
    requester: String,
    target: String,
    target_addr: SocketAddr,
    // 目标是不是**本机地址簿里的**那一台。不是（请求方给了地址、本机核验过身份）时，
    // 下面那条"目标离线就别再开新连接"的检查没有依据（本机压根不认识它）—— 跳过。
    target_in_book: bool,
    // 请求方的身份是不是**凭据核实过的**（见 `open` 的说明）：决定来源白名单宽窄。
    trusted_caller: bool,
    opened_from: StdIpAddr,
    mut stop: watch::Receiver<bool>,
) {
    let live = Arc::new(AtomicU64::new(0));
    // 并发额度：全局（24 条）+ 本端点（8 条）。超了**丢连接**而不是排队 —— 请求方是
    // 短连接重试模型；把 fd 打满、连本机 UI 的 HTTP 都 accept 不了才是更坏的结局。
    let global_streams = state.relays.streams();
    let endpoint_streams = Arc::new(Semaphore::new(MAX_RELAY_STREAMS_PER_ENDPOINT));
    loop {
        // F-107：本机切到「离线」= 隐身 —— 对所有设备都不可见，中转也不该例外。
        // `apply_offline` 会直接把这些端点收掉（close_all → stop），这一条是兜底：万一有哪条
        // 路径漏了，端点也不会继续替别人搬字节。
        if !state.is_online() {
            debug!(%requester, %target, "本机已离线，中转端点关闭");
            break;
        }
        let accepted = tokio::select! {
            _ = stop.changed() => break,
            waited = tokio::time::timeout(IDLE_TIMEOUT, listener.accept()) => match waited {
                // 空闲回收：一条活连接都没有，而且这么久没有新连接。
                Err(_) if live.load(Ordering::SeqCst) == 0 => {
                    debug!(%requester, %target, "中转端点空闲回收");
                    break;
                }
                // 还有连接在跑：接着等，别把一次大传输掐了。
                Err(_) => continue,
                Ok(Err(e)) => {
                    warn!(%requester, %target, "中转 accept 失败: {e}");
                    continue;
                }
                Ok(Ok(pair)) => pair,
            },
        };
        let (inbound, peer) = accepted;

        // 门：只有请求方（以及它此刻在地址簿里的地址）能拨这个端点。
        // 这是端点唯一的认证 —— 端点上跑的 TLS 两端自己会验证书，这里挡的是"别人拿它
        // 当免费跳板"。
        if !source_allowed(&state, &requester, opened_from, peer.ip(), trusted_caller) {
            warn!(%requester, %target, from = %peer.ip(), "中转端点收到白名单外的来源，已丢弃");
            continue;
        }

        // 目标离线了就别再开新连接（老连接照跑，它们是既成事实的那条路）。只对本机**地址簿里**
        // 的那一台问这件事：请求方给地址、本机核验过身份的那种目标，本机压根不认识它，
        // 没有"它离线了"这回事（它的可达性由每条连接自己证明）。
        let target_online = !target_in_book
            || state
                .peers
                .lock()
                .iter()
                .any(|p| p.id == target && p.online);
        if !target_online {
            debug!(%requester, %target, "目标已离线，中转端点关闭");
            break;
        }

        let Ok(global_permit) = global_streams.clone().try_acquire_owned() else {
            warn!(%requester, %target, "中转并发已达全局上限，丢弃这条连接");
            drop(inbound);
            continue;
        };
        let Ok(endpoint_permit) = endpoint_streams.clone().try_acquire_owned() else {
            warn!(%requester, %target, "中转并发已达单端点上限，丢弃这条连接");
            drop(inbound);
            continue;
        };

        let _ = inbound.set_nodelay(true);
        let live_child = Arc::clone(&live);
        let stop_child = stop.clone();
        let conn_state = Arc::clone(&state);
        live.fetch_add(1, Ordering::SeqCst);
        let label = (requester.clone(), target.clone());
        tokio::spawn(async move {
            // 两个 permit 跟着这条连接活到结束（drop 即归还）。
            let _global_permit = global_permit;
            let _endpoint_permit = endpoint_permit;
            pump(inbound, target_addr, stop_child, conn_state).await;
            live_child.fetch_sub(1, Ordering::SeqCst);
            debug!(requester = %label.0, target = %label.1, "一条中转连接结束");
        });
    }

    state.relays.remove(id);
}

/// 来源白名单：开这一跳的请求方（地址可能换过 DHCP，所以同时认它此刻在地址簿里的地址）。
///
/// `book_address_ok` 只对**凭据核实过的**请求方为真：自报身份那一档只认它控制面的来源地址。
/// 否则冒名者报上"地址簿里某台设备的 id"，就等于顺手把那台设备也写进了白名单。
fn source_allowed(
    state: &AppState,
    requester: &str,
    opened_from: StdIpAddr,
    peer: StdIpAddr,
    book_address_ok: bool,
) -> bool {
    if peer == opened_from {
        return true;
    }
    if !book_address_ok {
        return false;
    }
    state
        .peers
        .lock()
        .iter()
        .find(|p| p.id == requester)
        .and_then(|p| crate::net::parse_host(&p.ip))
        .is_some_and(|ip| ip == peer)
}

/// 一条中转连接：连上目标后双向原样搬字节（不解析、不改写），并**边搬边记账**。
async fn pump(
    inbound: TcpStream,
    target_addr: SocketAddr,
    mut stop: watch::Receiver<bool>,
    state: Arc<AppState>,
) {
    let outbound =
        match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect(target_addr)).await {
            Ok(Ok(stream)) => {
                state.relays.target_ok(target_addr);
                stream
            }
            Ok(Err(e)) => {
                warn!(target = %target_addr, "中转连目标失败: {e}");
                state.relays.target_failed(target_addr);
                return;
            }
            Err(_) => {
                warn!(target = %target_addr, "中转连目标超时");
                state.relays.target_failed(target_addr);
                return;
            }
        };
    let _ = outbound.set_nodelay(true);
    // 两个方向共用一个计数器：`copy_bidirectional` 对每个方向都是"读这边、写那边"，
    // 读侧合计就是这条连接搬过的总字节（见 [`CountingStream`]）。
    let bytes = Arc::new(AtomicU64::new(0));
    let mut inbound = CountingStream {
        inner: inbound,
        bytes: Arc::clone(&bytes),
    };
    let mut outbound = CountingStream {
        inner: outbound,
        bytes: Arc::clone(&bytes),
    };
    tokio::select! {
        _ = stop.changed() => {}
        r = copy_bidirectional(&mut inbound, &mut outbound) => {
            if let Err(e) = r {
                debug!(target = %target_addr, "中转连接结束: {e}");
            }
        }
    }
    // 连接无论怎么结束（对方关、目标关、开关收掉、本机退出）都记账：已经搬过的字节不丢。
    state.note_relay_traffic(bytes.load(Ordering::Relaxed));
}

/// 计数流：把 `poll_read` 读到的字节累加到 `bytes`。
///
/// 只计**读**侧（两个方向各包一层）：上面那条"读侧合计 = 总量"，再计写侧就翻倍。
///
/// 为什么边搬边计、而不是等 `copy_bidirectional` 返回再拿它的返回值：中转的典型流量是一条
/// **长连接**（`/events` 的 WS），可能开几小时 —— 只在结束时记账，用户开着设置看到的永远
/// 是 0（与"替人中转却一直显示 0"同一个坑）。计在 `poll_read` 里，字节一流过就涨。
struct CountingStream<S> {
    inner: S,
    bytes: Arc<AtomicU64>,
}

impl<S: AsyncRead + Unpin> AsyncRead for CountingStream<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        let this = self.get_mut();
        let before = buf.filled().len();
        let r = Pin::new(&mut this.inner).poll_read(cx, buf);
        if matches!(r, std::task::Poll::Ready(Ok(()))) {
            let n = buf.filled().len().saturating_sub(before);
            this.bytes.fetch_add(n as u64, Ordering::Relaxed);
        }
        r
    }
}

impl<S: AsyncWrite + Unpin> AsyncWrite for CountingStream<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
        buf: &[u8],
    ) -> std::task::Poll<std::io::Result<usize>> {
        Pin::new(&mut self.get_mut().inner).poll_write(cx, buf)
    }

    fn poll_flush(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_flush(cx)
    }

    fn poll_shutdown(
        self: Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }
}

// ---------------------------------------------------------------------------
// 请求方：直连不通时开一条中转
// ---------------------------------------------------------------------------

/// 控制面调用用的客户端（超时短，失败就走下一台候选）。
static OPEN_CLIENT: std::sync::LazyLock<reqwest::Client> = std::sync::LazyLock::new(|| {
    crate::net::client_builder()
        .timeout(OPEN_TIMEOUT)
        .build()
        .unwrap_or_default()
});

/// 一个"可以请它中转"的候选（F-107 起：来源不止"已配对"一种）。
#[derive(Debug, Clone)]
struct Candidate {
    id: String,
    name: String,
    /// 拨号地址（它自己的直连地址 —— 请它开中转是**本机到它**这一跳）。
    host: String,
    port: u16,
    /// 本机与它已配对（有 token：控制面走 Bearer；没有也能借，见 S-09 的放宽）。
    paired: bool,
}

/// 挑"可以请它中转"的候选（F-107 §9.1 的候选顺序）。
///
/// 顺序（前面的先用，四档）：
/// 1. **设备级链路**：这台目标上次实际经手的那一台（最可能仍然通）；
/// 2. **中转池**（活视野）：目标所在段的策略所指的那一段里，此刻「允许中转 + 在线」的设备；
/// 3. **策略里记下的池**（冷启动：锚点还没被探到、活池还是空的）；
/// 4. **广扫**：本机认识、自报允许中转、此刻在线的设备（已配对的排前 —— 现状 F-106 那一档，
///    未配对的排后 —— S-09 放宽之后才可能被借用）。
///
/// `target_cidrs` 是**池的键**（目标段）候选：通常只有一段（由目标的地址算出来），目标地址
/// 未知（只经中转见过一面）时是"含本机地址的那几段"（见 `ensure_for` / `own_segment_route_cidrs`）。
///
/// 一律排除：目标自己、**正在替别人中转的设备**（不做链式）、没有可拨地址的。
/// 刚失败过的那一台（[`AppState::failed_relay`]）排到最后 —— 它刚证明自己不通。
///
/// **地址的来源不止设备表**（真机踩出来的）：池里那些设备原先只能按 id 去设备表里找地址，
/// 而那些行是**锚点带来的**（`learned` 20s / `adopted` 15min）—— 锚点一离线，行被收走，
/// 池里剩下的 id 就谁也拨不动了：`ensure_for` 找不到候选，`relayRoutes` 形同虚设
/// （设计 §8.2 的"C 离线自动换 D"在真机上从不发生）。所以池自己带的地址（活池
/// `PoolEntry.ip/port`、落盘的 `RelayRoute::via_addrs`）也算数 —— 它们与设备表无关，
/// 锚点离散之后照样能拨；陈旧的地址最多白试一次（目标身份仍由中转方到地址上核实）。
fn relay_candidates(state: &AppState, target: &str, target_cidrs: &[String]) -> Vec<Candidate> {
    let peers: Vec<crate::config::PeerDevice> = state.peers.lock().clone();
    let failed = state.failed_relay(target);

    // 池里的地址（活池优先：那是锚点此刻的观察值；其次策略里落盘的最后已知地址）。
    let pooled = |id: &str| -> Option<(String, String, u16, bool)> {
        for cidr in target_cidrs {
            let live = state.pool_for(cidr);
            if let Some(e) = live.into_iter().find(|e| e.id == id) {
                if !e.ip.is_empty() && e.port != 0 {
                    return Some((e.name, e.ip, e.port, e.paired));
                }
            }
        }
        let config = state.config.lock();
        for cidr in target_cidrs {
            let Some(route) = config.relay_route(cidr) else {
                continue;
            };
            if let Some(d) = route.pool_addr(id) {
                let paired = route.via_paired.contains(&id.to_string());
                return Some((d.name.clone(), d.ip.clone(), d.port, paired));
            }
        }
        None
    };

    let mut tiers: Vec<(u8, Candidate)> = Vec::new();
    {
        let add = |tier: u8, id: &str, tiers: &mut Vec<(u8, Candidate)>| {
            // 本机自己永远不是中转候选：反向记录 / 落盘的池里可能留着一条指向本机的
            // id 与地址（接收侧有 own-id 闸，这里是使用侧的最后一道）。少了这一挡，
            // "请中转"会 POST 到自己的监听端点 —— 服务端核实请求方（本机）之后就把
            // 自己登记成了一台在线设备（真机：更新重启后列表里出现本机）。
            if id == target || id.is_empty() || id == state.own_id() {
                return;
            }
            let row = peers.iter().find(|p| p.id == id);
            // 本机已经在经它中转（对某台目标）：不重复当候选（不做链式）。
            if row.map(|p| p.relay.is_some()).unwrap_or(false) {
                return;
            }
            let resolved = match row.filter(|p| !p.ip.is_empty() && p.port != 0) {
                Some(p) => (p.name.clone(), p.ip.clone(), p.port, p.paired),
                None => match pooled(id) {
                    Some(v) => v,
                    None => return,
                },
            };
            let (name, host, port, paired) = resolved;
            if host.is_empty() || port == 0 {
                return;
            }
            // 地址兜底：池里那条可能挂着**别的 id**、地址却是本机的监听端点（旧配置 /
            // DHCP 换号后的残留）。判据与入站会话那一侧同一套：地址在本机网卡上、端口
            // 正好是本机在听的那个 = 本机自己（同机多开的另一个端口仍然合法）。
            if is_own_endpoint(state, &host, port) {
                return;
            }
            tiers.push((
                tier,
                Candidate {
                    id: id.to_string(),
                    name,
                    host,
                    port,
                    paired,
                },
            ));
        };

        // 1) 设备级链路：上次实际经手的那一台（只在本机开了记忆时有意义）。
        let device_via = state
            .config
            .lock()
            .relay_link(target)
            .map(|l| l.via_id.clone());
        if let Some(via) = device_via {
            add(0, &via, &mut tiers);
        }
        // 2) 活池 + 3) 池里落盘的那一份：**逐段**来。调用方一般只给一段（目标的地址算出来的
        // 那一段）；目标地址未知时给的是"含本机地址"的那几段（见 `ensure_for`）。
        for cidr in target_cidrs {
            // 2) 活池（锚点此刻报的"这一段里谁允许中转"）。
            for e in state.pool_for(cidr) {
                add(1, &e.id, &mut tiers);
            }
            // 3) 策略里那份池（冷启动、以及锚点离散后不再刷新时）。读法是 id 与地址两半的
            //    并集（`RelayRoute::pool_ids`）：只落盘了地址、没进 id 列表的那一类也要能拨。
            let remembered = state
                .config
                .lock()
                .relay_route(cidr)
                .map(crate::config::RelayRoute::pool_ids)
                .unwrap_or_default();
            for id in remembered {
                add(2, &id, &mut tiers);
            }
        }
        // 4) 广扫：在线 + 自报允许中转；已配对的排前面。
        for p in peers.iter() {
            if p.online && p.id != target && p.relay.is_none() && state.is_relay_capable(&p.id) {
                add(if p.paired { 3 } else { 4 }, &p.id, &mut tiers);
            }
        }
    }

    tiers.sort_by(|a, b| {
        a.0.cmp(&b.0).then_with(|| {
            (&a.1.id, &a.1.name, &a.1.host, a.1.port)
                .cmp(&(&b.1.id, &b.1.name, &b.1.host, b.1.port))
        })
    });
    let mut out: Vec<Candidate> = Vec::new();
    for (_, c) in tiers {
        if !out.iter().any(|o| o.id == c.id) {
            out.push(c);
        }
    }
    // 刚失败的那一台排到最后（其余相对顺序不变）。
    if let Some(bad) = failed {
        out.sort_by_key(|c| u8::from(c.id == bad));
    }
    out
}

/// `host:port` 是不是**本机自己在听的那个端点**（判据与入站会话那一侧同一套，
/// 见 `net::observed_dial_target` 与 `net::is_own_endpoint` 的说明）。
///
/// 候选的地址来自对端报来的池 / 落盘的 `viaAddrs`：池里的 id 可能还是**别台设备**，
/// 地址却已经指向本机（旧配置、DHCP 换号后的残留、或对端把自己那条推回来的池）。
/// 拨它等于向自己请中转，服务的核实随后会把本机登记成一台设备。
fn is_own_endpoint(state: &AppState, host: &str, port: u16) -> bool {
    let Ok(ip) = host.parse::<std::net::IpAddr>() else {
        return false;
    };
    crate::net::observed_dial_target(ip, port, state.listening_port()).is_none()
}

/// 一条消息既直连不通、又没有可用的中转时，把**为什么**报到界面上（F-106）。
///
/// 排队本身是既有语义（F-95：等它上线再发，不丢），这里补的只是那句解释：用户看到的
/// 应该是一句"暂时送不出去（这张网可能隔离了客户端 / 没有可用的中转方）"，而不是"发了没反应"。
pub fn notify_undeliverable(state: &Arc<AppState>, peer_id: &str) {
    let (name, seen) = state
        .peers
        .lock()
        .iter()
        .find(|p| p.id == peer_id)
        .map(|p| (p.name.clone(), p.relay_via().is_none()))
        .unwrap_or_else(|| (peer_id.to_string(), true));
    let reason = if relay_candidates(state, peer_id, &[]).is_empty() {
        "no_relay"
    } else {
        "relay_failed"
    };
    info!(
        peer_id = %peer_id,
        reason,
        "消息暂时送不出去：直连不通，也没有可用的中转（已排队等它上线）"
    );
    if let Some(app) = state.app_handle.lock().as_ref() {
        let _ = app.emit(
            crate::ws::events::ui::CHAT_UNDELIVERABLE,
            serde_json::json!({
                "peerId": peer_id,
                "name": name,
                "reason": reason,
                // 目标此刻"看得见"吗 —— 界面据此补一句"它在同一网段，只是连不上"。
                "seen": seen,
            }),
        );
    }
}

/// 直连不通时，为 `target` 找一台"愿意中转、而且此刻够得着它"的设备开一条中转。
///
/// 由 `state::probe_peers_once` 在"直连这一跳不通"时调用（每台设备有
/// [`REQUEST_COOLDOWN`] 的限流，见 `AppState::relay_attempts`）。返回开好的端点。
///
/// 候选顺序见 [`relay_candidates`]（F-107 §9.1）：设备级链路 → 活池 → 记住的池 → 广扫。
/// 本机开着「记住并自动复用跨网段中转」（S-10）时才查记忆，关着就是现状 F-106 的广扫。
///
/// **不需要与中转方配对**（S-09 的放宽）：池里那几台只要开着「允许为其它设备中转」就能借
/// —— 请求方在这里自报自己的 `id` 与监听端口，中转方会用「TCP 来源地址 + 这个端口」去核对
/// 它到底是谁（见 `server::routes_relay`）。有 token 的（已配对）走 Bearer，更稳。
///
/// **目标没有直连地址也借得动**（F-107 的反向那一侧）：只经中转见过一面对端、或对端是我们
/// 从来没发现过的那台时，`addr` 这一格就空着 —— 中转方自己按 id 在它地址簿里找，找到才开、
/// 找不到就 404（我们换下一台候选）。少了这一条，"单向通"就永远单向。
pub async fn ensure_for(state: &Arc<AppState>, target: &str) -> Option<RelayAddr> {
    // 目标此刻的**直连地址**：一起捎给中转方。跨网段的中转方常常"够得着目标却从没认识
    // 过它"（mDNS 跨不过路由、手动加的还可能是旧 id），有这一份它才能核实着转。
    //
    // **地址未知也照样往下走**：只经中转见过一面的对端（AP 隔离下被中转的那一侧，F-107 §10
    // 反向记录要解决的就是它）从来没有过直连地址 —— 以前这里当场返回，于是"单向通"到这就
    // 断了：够不着、也没有可借的候选，永远停「排队」。目标身份由中转方在地址上核实，所以
    // "不知道它在哪"不构成不能借的理由；hint 缺了就让中转方去它自己的地址簿里找。
    let (target_addr_hint, target_ip) = {
        let peers = state.peers.lock();
        let peer = peers.iter().find(|p| p.id == target)?;
        // 已经有路了：不用再开（下一条消息直接走它）。
        if peer.relay.is_some() {
            return None;
        }
        if peer.ip.is_empty() || peer.port == 0 {
            (None, String::new())
        } else {
            (Some(format!("{}:{}", peer.ip, peer.port)), peer.ip.clone())
        }
    };
    let (memory_on, own_id, own_name) = {
        let config = state.config.lock();
        (
            config.relay_memory,
            config.device_id.clone(),
            config.device_name.clone(),
        )
    };
    // 池的键是**目标段**。地址未知时退回"含本机地址的那几条策略段"：`apply_relay_routes`
    // 只收这种段，而"要够着与本机同段的一台设备"正是那几条策略要说的事（同段的对端地址
    // 未知，只可能是"只经中转见过一面"的那类）。猜错最多白试一次 —— 目标身份仍由中转方核实。
    let target_cidrs: Vec<String> = if memory_on {
        match state.cidr_of(target, &target_ip) {
            Some(c) => vec![c],
            None => state.own_segment_route_cidrs(),
        }
    } else {
        Vec::new()
    };
    let candidates = relay_candidates(state, target, &target_cidrs);
    // **没有候选就不动额度**：这一档一次 HTTP 都不发（纯内存判断），而额度是给"请中转"
    // 那几发用的。花在这里的代价是：记录/池晚一秒到，本机却还要干等 30s 才能再试一次
    // （用户那侧就是"明明刚通了，这条消息还卡着"）。
    if candidates.is_empty() {
        return None;
    }
    // 限流：同一台设备最多 30s 试一次。
    {
        let now = Instant::now();
        let mut attempts = state.relay_attempts.lock();
        match attempts.get(target) {
            Some(prev) if now.duration_since(*prev) < REQUEST_COOLDOWN => return None,
            _ => {
                attempts.insert(target.to_string(), now);
            }
        }
    }
    let own_port = state.listening_port();
    for c in candidates {
        let token = crate::credential::get_peer_token(&c.id).ok().flatten();
        let url = format!("{}/relay/open", crate::net::peer_base(&c.host, c.port));
        let body = serde_json::json!({
            "target": target,
            "addr": target_addr_hint,
            // F-107：请求方自报身份。中转方拿它当"来源白名单的键"与"限流的键"，
            // 并且会用「TCP 来源地址 + 这个端口」去核实（有 Bearer 时以 token 为准）。
            // 名字是捎给对面填**占位行**用的（本机 mDNS 到不了它那边时，那是它唯一能
            // 拿到真名的地方 —— 以前这条只带 id，对面于是显示一串 deviceid）。
            "requester": { "id": own_id, "port": own_port, "name": own_name },
        });
        // 带 token 的控制面也按目标建表：请求方与中转方可能同地址，不能让另一台
        // 已配对设备的证书把 token 收走。
        let client = crate::net::dial_client_timeout(
            &c.host,
            &crate::credential::dial_pin(&c.id, c.paired),
            OPEN_TIMEOUT,
        );
        let mut request = client.post(&url).json(&body);
        if let Some(t) = &token {
            request = request.bearer_auth(t);
        }
        let response = match request.send().await {
            Ok(r) => r,
            Err(e) => {
                debug!(peer_id = %c.id, "请中转失败: {e}");
                continue;
            }
        };
        if !response.status().is_success() {
            let status = response.status();
            let reason = crate::net::read_text_capped(response, crate::net::CONTROL_BODY_MAX)
                .await
                .unwrap_or_default();
            debug!(peer_id = %c.id, status = %status, "请中转被拒：{reason}");
            continue;
        }
        let Ok(opened) =
            crate::net::read_json_capped::<Opened>(response, crate::net::CONTROL_BODY_MAX).await
        else {
            continue;
        };
        let relay = RelayAddr {
            host: opened.host,
            port: opened.port,
            via: c.id.clone(),
            via_name: c.name.clone(),
        };
        // 中转方在回话里自报了它那一段的中转池：记下来（**不配对也能拿到** —— 这正是
        // "C 掉线之后自动换 D"的依据，见 `AppState::absorb_relay_report`）。
        if !opened.via_cidr.is_empty() {
            state.absorb_relay_report(target, &opened.via_cidr, &opened.via_devices);
        }
        info!(
            target = %target,
            via = %c.name,
            paired = c.paired,
            host = %relay.host,
            port = relay.port,
            "直连不通，改走中转"
        );
        // 中转方在回话里带回了**目标此刻的地址**（它刚核实过）：只经由中转见过一面的那一侧
        // 手里从来没有这个地址 —— 补上它，设备行才说得清"它在哪"，下一次借中转也才有 `addr` 提示。
        if let Some((ip, port)) = opened
            .target_addr
            .rsplit_once(':')
            .and_then(|(ip, port)| Some((ip.to_string(), port.parse::<u16>().ok()?)))
        {
            state.observe_verified_addr(target, &ip, port);
        }
        state.set_relay(target, relay.clone());
        return Some(relay);
    }
    None
}

/// 一条反向路由记录里最多认多少台中转设备（接收方另有总量上限，见 `state::MAX_ADOPTED_DEVICES`）。
///
/// 一条策略的池本来就只该是"那一段里愿意中转的几台"；不设限时，一次大请求体就能把设备表灌满。
pub const MAX_ROUTE_DEVICES: usize = 16;

/// 一次 `/relay/route` 最多带多少条记录（按**目标段**计）。
///
/// 一台设备的本机网段是"几块网卡"量级；64 已经给了多网卡/虚拟网卡足够的余量。
/// 设上限是为了让公开面上的这条路由在"请求体很大、记录成千条"时不至于先解析、再逐条
/// 做归属判断（未认证来源的那一档尤其如此）。
pub const MAX_ROUTE_RECORDS: usize = 64;

/// 反向路由记录里的一台中转设备（F-107 §10.2）：接收方要拨它，所以要地址。
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RouteDevice {
    pub id: String,
    #[serde(default)]
    pub name: String,
    pub ip: String,
    pub port: u16,
}

/// 反向路由记录（F-107 §10.2）：**这一段设备怎么够得着**。
///
/// 由"知道这条路的那一侧"（经中转真的到过对方的那台，见 `state::note_relay_success`）经
/// **那条已建立的链路**推给对端；对端因此不必与中转方配对就知道那条路存在。
///
/// 记录里**不含目标自己的地址**：只有段与中转设备。这既够用，又绕开"经中转过来的 TCP 来源
/// 是中转方"这个坑（`state::is_relayed_source` 那条规则照顾的问题）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RouteRecord {
    /// 「这一段里的设备需要中转」（接收方只在它**含本机地址**时才采纳）。
    pub target_cidr: String,
    /// 中转网段。
    pub via_cidr: String,
    /// 池：该段里「允许中转」的设备（带地址，接收方要拨）。
    pub via_devices: Vec<RouteDevice>,
    /// 发送方与池里哪些**已配对**（接收方据此知道这枚记录的出处，不作为自己的配对依据）。
    #[serde(default)]
    pub via_paired: Vec<String>,
    #[serde(default)]
    pub confirmed_at: Option<String>,
}

/// 本机为 `peer_ip` 那一段攒的路由记录（F-107 §10）。
///
/// 只挑**目标段含对端地址**的那些策略 —— 对端要的是"我这一段怎么够得着"，把别的段塞过去
/// 只会让它记下一条与自己无关的路由（接收方也会丢弃，见 `state::apply_relay_routes`）。
/// 设备表按 id 排序后再序列化：内容哈希要稳定（决定"要不要重发"）。
fn routes_for(state: &AppState, peer_ip: &str) -> Vec<RouteRecord> {
    let Ok(ip) = peer_ip.parse::<std::net::Ipv4Addr>() else {
        return Vec::new();
    };
    routes_where(state, |c| c.contains(ip))
}

/// 同上，但挑的是**含本机地址**的那些段（"要够着本机这一段，该借谁"）。
///
/// 给"对端地址未知"的那一档用（只经中转见过一面、本机从来没发现过它的那类）：不知道对端
/// 在哪一段，就只能推本机这一段 —— 而它要知道的恰好也正是"怎么够着本机"。收不收由对端按
/// 自己那一条判据定（`apply_relay_routes` 只收含**它**地址的段），推错了它直接丢弃。
fn routes_for_own_segments(state: &AppState) -> Vec<RouteRecord> {
    routes_where(state, crate::subnet::own_ip_in)
}

/// 按"目标段是否满足 `keep`"挑记录（两处共用同一份拼装与排序）。
fn routes_where(state: &AppState, keep: impl Fn(&crate::subnet::Cidr) -> bool) -> Vec<RouteRecord> {
    let peers = state.peers.lock().clone();
    let config = state.config.lock();
    let mut out: Vec<RouteRecord> = config
        .relay_routes
        .iter()
        // **未认证来源的临时提示不往对端推**：本机都还没核实这条记录的出处，
        // 不能再把它转述给第三方（否则一条局域网报文会沿着拓扑自己传播）。
        .filter(|r| !r.ephemeral && crate::subnet::parse(&r.target_cidr).is_some_and(|c| keep(&c)))
        .map(|r| {
            // 池的成员 = `via_devices`（id 列表）与 `via_addrs`（最后已知地址）的**并集**，
            // 与选路（`relay_candidates`）同一套读法。地址优先取设备表里那一行（它反映
            // 本机此刻跟它的关系），表里没有就退回池自己记下的**最后已知地址** —— 那些行
            // 会随锚点离散被收走，而"这条记录要让对端拨得动"不能依赖本机的设备表
            // （真机反馈第七轮：对端那边"看不到 D"，就是因为这里只认设备表、池里那台被漏掉）。
            let mut via_devices: Vec<RouteDevice> = r
                .pool_ids()
                .iter()
                .filter_map(|id| {
                    if let Some(p) = peers
                        .iter()
                        .find(|p| &p.id == id && !p.ip.is_empty() && p.port != 0)
                    {
                        return Some(RouteDevice {
                            id: p.id.clone(),
                            name: p.name.clone(),
                            ip: p.ip.clone(),
                            port: p.port,
                        });
                    }
                    r.pool_addr(id).cloned()
                })
                .collect();
            via_devices.sort_by(|a, b| (&a.id, &a.ip, a.port).cmp(&(&b.id, &b.ip, b.port)));
            RouteRecord {
                target_cidr: r.target_cidr.clone(),
                via_cidr: r.via_cidr.clone(),
                via_devices,
                via_paired: r.via_paired.clone(),
                confirmed_at: r.last_ok_at.clone(),
            }
        })
        .filter(|r| !r.via_devices.is_empty())
        .collect();
    out.sort_by(|a, b| a.target_cidr.cmp(&b.target_cidr));
    out
}

/// 把记录里**收件人自己**那一项剔掉；池被剔空的记录整条丢弃。
///
/// 只剔 `via_devices`（地址半份跟着 id 走，`via_devices` 空 = 纪录没有候选中转方，
/// 接收方留着的 `viaAddrs` 也拨不动）；`via_paired` 是同一批 id 的标注，一起剔。
fn without_recipient(mut routes: Vec<RouteRecord>, peer_id: &str) -> Vec<RouteRecord> {
    routes.retain_mut(|r| {
        r.via_devices.retain(|d| d.id != peer_id);
        r.via_paired.retain(|id| id != peer_id);
        !r.via_devices.is_empty()
    });
    routes
}

/// 把本机的中转记忆推给这台对端（F-107 §10）。
///
/// 由 5s 探活的"它在线"那一档调用（直连与中转两条路都会走到）。三道闸：
/// ① S-10 关着就不记也不推；② 没有可推的路由（空）不发；③ 内容哈希没变、且还在重发间隔内
/// 就不发（见 [`AppState::route_push_gate`] —— 未确认 30s 重试、已确认 5 分钟刷新一次，
/// 后者是为了对端重启后也能重新拿到）。
///
/// **配对不是前提**：已配对的对端走 Bearer；没配对的对端走"自报身份 + 被核实"那一档
/// （与 `/relay/open` 同一套，见 `server::routes_relay`）。被中转的那一侧（B）往往从来没跟
/// 锚点（C）配过 —— 它要知道"A 是怎么过来的"，只能靠 A 把这条记录送过来；少了这一条，
/// 它就只能等用户手动把 C 也加到它旁边去。
pub fn maybe_send_route(state: &Arc<AppState>, peer_id: &str) {
    if !state.config.lock().relay_memory {
        return;
    }
    let Some(peer) = state.get_peer(peer_id) else {
        return;
    };
    // 对端地址未知（只经中转见过一面）时推"本机这一段怎么够得着"：它要知道的正是这个。
    // 以前这里直接返回 —— 于是被中转的那一侧连"该怎么回去"都拿不到。
    let routes = if peer.ip.is_empty() {
        routes_for_own_segments(state)
    } else {
        routes_for(state, &peer.ip)
    };
    // **一条路由不能点名收件人自己当中转方**。记录里的池是"这一段该经谁中转"，而
    // `routes_where` 会把池里那几台的 `viaAddrs` 一并带上 —— 对端的池里可能还留着本机
    // 的地址（`viaAddrs` 是"最后已知事实"，锚点离散后也不清）。推给 P 的记录里必须剔掉 P：
    // 否则 P 收下后池里就多了"自己"，重启后的第一拍会向自己请中转，把自己登记成一台
    // **在线设备**（真机：更新完成、应用重启后，设备列表里出现本机）。
    let routes = without_recipient(routes, peer_id);
    if routes.is_empty() {
        return;
    }
    let Ok(hash) = serde_json::to_string(&routes) else {
        return;
    };
    if !state.route_push_gate(peer_id, &hash) {
        return;
    }
    let token = crate::credential::get_peer_token(peer_id).ok().flatten();
    let (own_id, own_name, own_port) = {
        let config = state.config.lock();
        (
            config.device_id.clone(),
            config.device_name.clone(),
            state.listening_port(),
        )
    };
    let Some((host, port)) = state.dial_endpoint(peer_id, None) else {
        return;
    };
    let client = crate::net::dial_client_timeout(&host, &peer.dial_pin(), OPEN_TIMEOUT);
    let st = Arc::clone(state);
    let pid = peer_id.to_string();
    crate::task::spawn("relay route record", async move {
        let url = format!("{}/relay/route", crate::net::peer_base(&host, port));
        // 未配对的对端走"自报身份 + 被核实"那一档（与 `/relay/open` 同一套）：这是
        // "B 从来没跟锚点配过，也能知道 A 是怎么来的"的唯一通路 —— 少了它，被中转的那一侧
        // 什么都学不到，只能等用户手动把它也加到锚点旁边去。
        let body = serde_json::json!({
            "routes": routes,
            "requester": { "id": own_id, "port": own_port, "name": own_name },
        });
        let mut request = client.post(&url).json(&body);
        if let Some(t) = &token {
            request = request.bearer_auth(t);
        }
        match request.send().await {
            Ok(r) if r.status().is_success() => {
                let accepted = crate::net::read_json_capped::<serde_json::Value>(
                    r,
                    crate::net::CONTROL_BODY_MAX,
                )
                .await
                .ok()
                .and_then(|v| v["accepted"].as_array().map(|a| a.len()))
                .unwrap_or(0);
                st.route_push_result(&pid, accepted > 0);
                debug!(peer_id = %pid, accepted, "中转路由记录已送达");
            }
            Ok(r) => {
                debug!(peer_id = %pid, status = %r.status(), "中转路由记录被拒（下一拍再试）");
            }
            Err(e) => {
                debug!(peer_id = %pid, "中转路由记录发送失败: {e}");
            }
        }
    });
}

/// 直连恢复（或中转这一跳也不通了）：把这台设备上的中转端点摘掉，回到直连。
///
/// 端点在中转方那边靠**空闲回收**自己消失（见 [`IDLE_TIMEOUT`]）—— 直连恢复之后那条路
/// 不再有人走，两分钟内就空了。这里不做"通知中转方关掉"：多一条控制面就多一种失败形态，
/// 而回收本来就必须按空闲做。
pub fn clear_for(state: &Arc<AppState>, target: &str) {
    if state.clear_relay(target) {
        info!(target = %target, "直连恢复，收起中转");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn own_ipv4_prefers_the_requesters_subnet() {
        // 本机地址由网卡决定，测试只验证"挑不出来时不 panic、挑得出来时是同网段优先"。
        let picked = pick_own_ipv4(StdIpAddr::V4(std::net::Ipv4Addr::new(10, 198, 71, 9)));
        if let Some(host) = picked {
            assert!(
                crate::net::relay_host(&host).is_some(),
                "挑出来的必须是 IPv4"
            );
        }
    }

    #[test]
    fn registry_enforces_the_cap() {
        let registry = RelayRegistry::default();
        for i in 0..MAX_RELAYS {
            let (stop, _rx) = watch::channel(false);
            registry
                .insert(ActiveRelay {
                    requester: "requester".into(),
                    target: format!("target-{i}"),
                    port: 0,
                    host: "127.0.0.1".into(),
                    target_addr: "127.0.0.1:1".parse().unwrap(),
                    stop,
                })
                .expect("上限之内的插入必须成功");
        }
        let (stop, _rx) = watch::channel(false);
        assert!(
            registry
                .insert(ActiveRelay {
                    requester: "requester".into(),
                    target: "overflow".into(),
                    port: 0,
                    host: "127.0.0.1".into(),
                    target_addr: "127.0.0.1:1".parse().unwrap(),
                    stop,
                })
                .is_err(),
            "超过上限必须被拒"
        );
        registry.close_all();
        assert_eq!(registry.active(), 0);
    }

    #[test]
    fn registry_reuses_endpoint_and_cools_failed_target() {
        let registry = RelayRegistry::default();
        let addr: SocketAddr = "10.0.0.2:3890".parse().unwrap();
        let (stop, _rx) = watch::channel(false);
        registry
            .insert(ActiveRelay {
                requester: "requester".into(),
                target: "target".into(),
                port: 41000,
                host: "10.0.0.1".into(),
                target_addr: addr,
                stop,
            })
            .unwrap();
        let (port, host, target) = registry.find("requester", "target").expect("同一对应能复用");
        assert_eq!(port, 41000);
        assert_eq!(host, "10.0.0.1");
        assert_eq!(target, addr);
        assert!(registry.find("other", "target").is_none());
        registry.target_failed(addr);
        assert!(registry.target_cooling(addr), "刚失败的目标应处于冷却期");
        registry.target_ok(addr);
        assert!(!registry.target_cooling(addr), "连上后冷却必须清掉");
    }

    #[test]
    fn caller_cooldown_blocks_immediate_retry() {
        let registry = RelayRegistry::default();
        assert!(registry.caller_allowed("a"));
        assert!(!registry.caller_allowed("a"), "冷却期内同一位请求方要挡住");
        assert!(registry.caller_allowed("b"), "另一位请求方互不影响");
    }

    /// 中转是裸 TCP：目标地址一旦落在本机回环/本机网卡上，目标监听就会把连接当成本机
    /// 进程（`Caller::Local`）。这条测试钉住"本机地址永不作为中转目标"。
    #[test]
    fn relay_targets_never_include_local_or_link_local_addresses() {
        for bad in [
            "127.0.0.1",
            "127.1.2.3",
            "::1",
            "::ffff:127.0.0.1",
            "0.0.0.0",
            "::",
            "224.0.0.1",
            "ff02::1",
            "169.254.10.20",
            "fe80::1",
            "255.255.255.255",
        ] {
            let ip: StdIpAddr = bad.parse().expect(bad);
            assert!(forbidden_relay_target(ip), "必须拒绝: {bad}");
        }
        for ok in ["192.0.2.10", "10.0.0.5", "2001:db8::10"] {
            let ip: StdIpAddr = ok.parse().expect(ok);
            assert!(!forbidden_relay_target(ip), "不该误伤: {ok}");
        }
    }

    /// `open` 在**发任何网络请求之前**就拒绝"目标是本机自己"与"目标是回环地址"。
    /// 这条是 P0 回归：旧实现会先去回环问 `/pair/info`，问通了就开出中转端点。
    #[tokio::test]
    async fn open_refuses_to_target_ourselves_or_loopback() {
        let config = crate::config::test_config();
        let own_id = config.device_id.clone();
        let config_path =
            std::env::temp_dir().join(format!("orbdrop_relay_cfg_{}.toml", uuid::Uuid::new_v4()));
        let state = Arc::new(AppState::new(config, config_path));
        let from: StdIpAddr = "192.0.2.10".parse().unwrap();

        let err = match open(&state, &own_id, from, "some-other-device", None, false).await {
            Err(e) => e,
            Ok(_) => panic!("本机自己不能当中转请求方"),
        };
        assert!(matches!(err, OpenError::NotEligible(_)));

        let err = match open(&state, "attacker", from, &own_id, None, false).await {
            Err(e) => e,
            Ok(_) => panic!("本机自己不能当中转目标"),
        };
        assert!(matches!(err, OpenError::NotEligible(_)));

        let err = match open(
            &state,
            "attacker",
            from,
            "some-other-device",
            Some(("127.0.0.1".to_string(), 3890)),
            false,
        )
        .await
        {
            Err(e) => e,
            Ok(_) => panic!("回环地址不能当中转目标"),
        };
        assert!(matches!(err, OpenError::NotEligible(_)));

        let err = match open(
            &state,
            "attacker",
            from,
            "some-other-device",
            Some(("[::ffff:127.0.0.1]".to_string(), 3890)),
            false,
        )
        .await
        {
            Err(e) => e,
            Ok(_) => panic!("IPv4 映射的回环地址同样不能当中转目标"),
        };
        assert!(matches!(err, OpenError::NotEligible(_)));
    }

    /// 更新重启后的真机回归：对端推回来的反向记录里点名本机、而本机正是收件人时，
    /// 推给对端之前先剔掉收件人自己；池被剔空的记录整条丢弃。
    #[test]
    fn a_route_pushed_to_a_peer_never_names_that_peer() {
        let dev = |id: &str| RouteDevice {
            id: id.into(),
            name: id.to_uppercase(),
            ip: "10.20.30.40".into(),
            port: 3890,
        };
        let record = |devices: Vec<RouteDevice>, paired: Vec<String>| RouteRecord {
            target_cidr: "10.20.30.0/24".into(),
            via_cidr: "10.20.30.0/24".into(),
            via_devices: devices,
            via_paired: paired,
            confirmed_at: None,
        };
        let out = without_recipient(
            vec![record(
                vec![dev("recipient"), dev("peer-c")],
                vec!["recipient".into()],
            )],
            "recipient",
        );
        assert_eq!(out.len(), 1);
        assert_eq!(
            out[0]
                .via_devices
                .iter()
                .map(|d| d.id.as_str())
                .collect::<Vec<_>>(),
            vec!["peer-c"],
            "收件人自己不能出现在它收到的池里"
        );
        assert!(out[0].via_paired.is_empty());
        assert!(
            without_recipient(
                vec![record(vec![dev("recipient")], vec!["recipient".into()])],
                "recipient"
            )
            .is_empty(),
            "只剩收件人自己的记录整条丢弃"
        );
    }

    fn memory_state(own_id: &str) -> Arc<AppState> {
        let dir = std::env::temp_dir().join(format!(
            "orbdrop_relay_self_{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Arc::new(AppState::new(
            crate::config::Config {
                device_id: own_id.into(),
                device_name: "OWN".into(),
                relay_memory: true,
                ..Default::default()
            },
            dir.join("config.toml"),
        ))
    }

    /// 池里留着本机自己的 id/地址（旧配置 / 对端推回来的记录）时，选路永远不把自己当中转：
    /// 否则会向自己的监听端点 POST `/relay/open`，服务端核实"请求方 = 本机"后把自己登记成
    /// 一台在线设备（真机：更新重启后设备列表里出现自己）。
    #[test]
    fn relay_candidates_never_include_the_local_device() {
        let state = memory_state("own-relay");
        {
            let mut config = state.config.lock();
            let route = config.relay_route_mut("10.20.30.0/24");
            route.via_cidr = "10.20.30.0/24".into();
            route.via_devices = vec!["own-relay".into(), "peer-c".into()];
            route.via_addrs = vec![
                RouteDevice {
                    id: "own-relay".into(),
                    name: "OWN".into(),
                    ip: "10.20.30.40".into(),
                    port: 3890,
                },
                RouteDevice {
                    id: "peer-c".into(),
                    name: "C".into(),
                    ip: "10.20.30.41".into(),
                    port: 3890,
                },
            ];
        }
        state.upsert_peer(&crate::config::PeerDevice {
            id: "peer-c".into(),
            name: "C".into(),
            ip: "10.20.30.41".into(),
            port: 3890,
            online: true,
            ..Default::default()
        });
        let ids: Vec<String> = relay_candidates(&state, "target-x", &["10.20.30.0/24".into()])
            .into_iter()
            .map(|c| c.id)
            .collect();
        assert!(
            ids.contains(&"peer-c".to_string()),
            "正常的池成员仍要被选中：{ids:?}"
        );
        assert!(
            !ids.contains(&"own-relay".to_string()),
            "本机自己永远不是候选：{ids:?}"
        );
    }

    /// 池里那条挂着**别台 id**、地址却是本机监听端点的旧记录也不能拨（同机多开的另一个
    /// 端口是合法对端，不能误伤）。
    #[test]
    fn relay_candidates_skip_addresses_that_are_our_own_listener() {
        let state = memory_state("own-relay");
        for (id, port) in [("stale-own", 3890_u16), ("second-instance", 3891)] {
            state.upsert_peer(&crate::config::PeerDevice {
                id: id.into(),
                name: id.to_uppercase(),
                ip: "127.0.0.1".into(),
                port,
                online: true,
                ..Default::default()
            });
            state.set_relay_capable(id, true);
        }
        let ids: Vec<String> = relay_candidates(&state, "target-x", &[])
            .into_iter()
            .map(|c| c.id)
            .collect();
        assert!(
            !ids.contains(&"stale-own".to_string()),
            "指向本机监听端点的 id 不许当候选：{ids:?}"
        );
        assert!(
            ids.contains(&"second-instance".to_string()),
            "同机多开的另一个端口是合法对端：{ids:?}"
        );
    }
}
