//! LocalSend v2.2 兼容层（可选）。
//!
//! 与 OrbDrop 自己的 mDNS/推送/配对协议**完全隔离**：
//! - 设备行单独存在 `LocalSendState`，不写 `Config::peers`，也不进 `AppState::peers`；
//! - HTTP 面只在 `/api/localsend/*`，默认关闭；关闭时 404，离线时 503；
//! - 传输是“发送方推、接收方逐次确认”，不与 OrbDrop 的拉取式推送共用会话/续传。
//!
//! 这里做文件，以及 LocalSend 的“文本消息”约定（单文件 + `text/plain` + `preview`）；
//! 整批图片在自动接收时也会落成会话里的图片消息（底层仍是 LocalSend 文件上传）。
//! WebRTC / `web.localsend.org` 是另一套协议，不在本模块内。

pub mod client;
pub mod discovery;
pub mod dto;
pub mod paths;
pub mod server;
pub mod session;

use std::collections::HashMap;
use std::future::Future;
use std::net::{IpAddr, Ipv4Addr, TcpListener as StdTcpListener};
use std::pin::Pin;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use serde_json::json;
use tokio::sync::{mpsc, Notify};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use crate::state::AppState;

/// LocalSend 协议的默认 HTTP 端口（与发现用的 UDP 端口同号）。
///
/// 本机主监听通常不是它，所以 [`start_http`] 会在这里再挂一份只含 `/api/localsend/*`
/// 的监听。**不能省**：第三方实现（真机：localsend-go 系）发送时把目标端口写死成
/// 53317，不读公告里的 `port` —— 少了这一份，它们能发现我们、我们也能发给它们，
/// 但它们永远发不过来。
const HTTP_PORT: u16 = 53317;

/// 一台 LocalSend 对端。`id` 用 `ls-<大写指纹的小写形式>`，符合 `ids::is_segment`
/// （只允许 `[A-Za-z0-9._-]`；不能用冒号）。
#[derive(Debug, Clone)]
pub struct LocalSendPeer {
    pub id: String,
    /// 证书 DER 的 SHA-256，**小写**（与 `net::fingerprint_of` / `Trust` 同一套）。
    pub fingerprint: String,
    pub alias: String,
    pub device_model: Option<String>,
    pub device_type: Option<String>,
    pub ip: String,
    pub port: u16,
    pub protocol: String,
    pub download: bool,
    pub online: bool,
    pub seen_at: Instant,
    pub last_seen: Instant,
    pub certified: bool,
}

impl LocalSendPeer {
    pub fn fingerprint_upper(&self) -> String {
        self.fingerprint.to_ascii_uppercase()
    }

    pub fn seen(&self) -> bool {
        self.seen_at.elapsed() < SEEN_TTL
    }

    pub fn alive(&self) -> bool {
        self.online && self.last_seen.elapsed() < ONLINE_TTL
    }
}

/// 发现记录多久算“看得见”。只用于设备列表的 `seen`。
const SEEN_TTL: Duration = Duration::from_secs(90);
/// 探活成功多久算在线。
const ONLINE_TTL: Duration = Duration::from_secs(120);
/// LocalSend 设备表上限（对方可以伪造身份刷行；正常一屏远小于这个数）。
const MAX_PEERS: usize = 128;

/// 自动接收图片的滚动预算窗口（**跨发送方**）。
///
/// 单批有上限（16 个 / 48 MiB），但批数不限：开到 LocalSend 之后，同一段里任何人
/// 都能一直往里塞图（自动收，没有确认卡）。这个预算是"静默落盘"的总闸，超了的那一批
/// 不再自动收，退回普通文件那条同意路（用户点「接收」才落）。
const AUTO_IMAGE_WINDOW: Duration = Duration::from_secs(3600);
const AUTO_IMAGE_WINDOW_BYTES: u64 = 2 * 1024 * 1024 * 1024;

/// 同时进行的「公告 → register 拨号」上限。报文不可认证、来源 IP 可伪造，
/// 不封顶的话一秒钟就能让本机对任意地址发起几千个 TLS 握手。
const PROBE_SLOTS: usize = 8;

#[derive(Debug, Clone, Copy)]
struct AutoBudget {
    window: Instant,
    bytes: u64,
}

impl Default for AutoBudget {
    fn default() -> Self {
        Self {
            window: Instant::now(),
            bytes: 0,
        }
    }
}

/// 一次 LocalSend 服务任务（UDP 发现 supervisor，或 TCP 53317 上的 HTTP 监听）。
/// `stop` 时 cancel + abort，socket 随任务一起 drop。
pub struct ServiceHandle {
    pub cancel: CancellationToken,
    pub task: JoinHandle<()>,
}

/// TCP 53317 独立监听的运行状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, Default)]
#[serde(rename_all = "camelCase")]
pub enum StandardPortState {
    /// 53317 已监听（协议默认端口）。
    Bound,
    /// 53317 被别的程序占用，已回落到主监听端口。
    Busy,
    /// 主监听端口本身就是 53317，不需要第二份监听。
    MainIsStandard,
    /// 绑定失败（不是端口占用，例如身份读不出来）。
    Failed,
    /// 服务没起（关闭 / 离线 / 主端口还没绑上）。
    #[default]
    Stopped,
}

/// LocalSend 兼容面的运行时状态（**不落盘**）。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct LocalSendStatus {
    pub enabled: bool,
    pub online: bool,
    /// UDP 发现 supervisor 是否在跑。
    pub discovery: bool,
    /// 最近一次发现启动失败的原因。
    pub discovery_error: Option<String>,
    pub standard_port: StandardPortState,
    /// 53317 独立监听失败时的原始错误。
    pub standard_error: Option<String>,
    /// 对端该拨的端口（0 = 服务没起）。
    pub advertised_port: u16,
}

#[derive(Debug, Clone, Default)]
struct Runtime {
    discovery: bool,
    discovery_error: Option<String>,
    standard: StandardPortState,
    standard_error: Option<String>,
}

/// 一条排队中的 LocalSend 出站任务（文本/图片）。
///
/// 官方接收端只有一个上传会话槽：`prepare-upload` 在用户点「接受」之前一直占着它，
/// 同一时间再来一条直接回 `409 Blocked by another session`。所以同一台设备的出站
/// 请求必须**按用户发送顺序逐条**走，不能并发。
pub type OutboundJob = Pin<Box<dyn Future<Output = ()> + Send + 'static>>;

/// 进程内 LocalSend 状态。**不落盘**：设备是临时观察值，重启重新发现。
pub struct LocalSendState {
    peers: Mutex<HashMap<String, LocalSendPeer>>,
    service: Mutex<Option<ServiceHandle>>,
    /// TCP 53317 上的独立 HTTP 监听（见 [`start_http`]）。
    http: Mutex<Option<ServiceHandle>>,
    /// 对端公告里该写的 HTTP 端口（0 = 服务没起）。
    ///
    /// 正常情况下是 [`HTTP_PORT`]；53317 被别的程序占用时回落到主监听端口
    /// （官方客户端会读公告里的 `port`，写死端口的那一档此时本来就够不着）。
    port: AtomicU16,
    pub sessions: session::SessionStore,
    /// 出站传输的取消令牌：`invite_id → token`（`cancel_push_batch` 用）。
    outbound: Mutex<HashMap<String, CancellationToken>>,
    /// 自动接收图片的滚动预算（见 [`AUTO_IMAGE_WINDOW_BYTES`]）。
    auto_budget: Mutex<AutoBudget>,
    /// 公告触发的 register 拨号并发上限（见 [`PROBE_SLOTS`]）。
    probe_slots: Arc<tokio::sync::Semaphore>,
    /// 发现报文按来源 IP 限速（报文不可认证，频次必须自己收）。
    datagram_rate: Mutex<RateLimiter>,
    /// 每台设备一条出站 lane（`peer id → 队列`）：同一时刻只跑一条 `prepare-upload`，
    /// 顺序 = 用户点发送的顺序。见 [`LocalSendState::enqueue_outbound`]。
    lanes: Mutex<HashMap<String, mpsc::UnboundedSender<OutboundJob>>>,
    pub rate: Mutex<RateLimiter>,
    /// 运行时状态（发现是否在跑、53317 监听是哪种结果）。
    runtime: Mutex<Runtime>,
    /// 53317 被占用 / 发现失败时的重试监督（启用且在线期间常驻）。
    guard: Mutex<Option<ServiceHandle>>,
    /// 让发现 supervisor 立刻补发一轮公告（53317 刚接管时）。
    pub announce_notify: Notify,
    /// 让重试 guard 立刻跑一拍（设置页「重试」；只补缺的监听，不 restart）。
    pub guard_wake: Notify,
}

impl Default for LocalSendState {
    fn default() -> Self {
        Self::new()
    }
}

impl LocalSendState {
    pub fn new() -> Self {
        Self {
            peers: Mutex::new(HashMap::new()),
            service: Mutex::new(None),
            http: Mutex::new(None),
            port: AtomicU16::new(0),
            sessions: session::SessionStore::new(),
            outbound: Mutex::new(HashMap::new()),
            auto_budget: Mutex::new(AutoBudget::default()),
            probe_slots: Arc::new(tokio::sync::Semaphore::new(PROBE_SLOTS)),
            datagram_rate: Mutex::new(RateLimiter::new()),
            lanes: Mutex::new(HashMap::new()),
            rate: Mutex::new(RateLimiter::new()),
            runtime: Mutex::new(Runtime::default()),
            guard: Mutex::new(None),
            announce_notify: Notify::new(),
            guard_wake: Notify::new(),
        }
    }

    pub fn get(&self, id: &str) -> Option<LocalSendPeer> {
        self.peers.lock().get(id).cloned()
    }

    /// 按 `ip:port` 找一行（一个端点只该有一行）。
    ///
    /// 给**没有客户端证书、自报值也不是证书 SHA-256** 的入站请求用（第三方实现的
    /// 常见形态）：如果这台设备已经通过公告被我方登记过，沿用那一行的身份，别在通知/
    /// 会话里再造一个以来源 IP 为 id 的"第二台设备"。
    pub fn find_by_endpoint(&self, ip: &str, port: u16) -> Option<LocalSendPeer> {
        self.peers
            .lock()
            .values()
            .find(|p| p.ip == ip && p.port == port)
            .cloned()
    }

    /// 对端**自报**的指纹能不能用来认这条请求的身份。
    ///
    /// 身份的本体是 TLS 握手证书；`fingerprint` 只是对端自己填的字符串，而且每台设备的
    /// 公告都明文广播它 —— 同一段里谁都能抄。若这个指纹已经对应一台**还在线**、地址却
    /// 不是这次来源的设备，就不认这个自报值：否则任何一台机器都能顶着别人的身份往它的
    /// 会话里塞消息，或者把它那一行的地址挪到自己身上（后续发给它的东西全打错人）。
    ///
    /// 地址真变了（DHCP）时旧行已被探活判离线（`alive()` 为假），自报值照常被接受。
    pub fn claim_is_plausible(&self, fingerprint: &str, ip: &str) -> bool {
        let id = id_for_fingerprint(fingerprint);
        match self.peers.lock().get(&id) {
            Some(p) => p.ip == ip || !p.alive(),
            None => true,
        }
    }

    /// 自动接收图片的预算：`cost` 字节还能不能**静默**收。
    ///
    /// 窗口 1 小时、跨发送方。超预算的那一批不是被拒收，而是退回普通文件那条同意路
    /// （`server::prepare_upload` 里把 `auto_images` 降级）—— 要不要落盘仍然由用户点。
    pub fn take_auto_image_budget(&self, cost: u64) -> bool {
        let mut budget = self.auto_budget.lock();
        if budget.window.elapsed() >= AUTO_IMAGE_WINDOW {
            *budget = AutoBudget {
                window: Instant::now(),
                bytes: 0,
            };
        }
        if budget.bytes.saturating_add(cost) > AUTO_IMAGE_WINDOW_BYTES {
            return false;
        }
        budget.bytes += cost;
        true
    }

    /// 一条发现报文能不能处理（按来源 IP 限速；报文不可认证）。
    pub fn allow_datagram(&self, ip: IpAddr) -> bool {
        self.datagram_rate
            .lock()
            .allow(ip, 120, Duration::from_secs(10))
    }

    /// 拿一个「公告 → register 拨号」名额；满了就直接丢弃这条公告。
    pub fn probe_permit(&self) -> Option<tokio::sync::OwnedSemaphorePermit> {
        self.probe_slots.clone().try_acquire_owned().ok()
    }

    pub fn snapshot(&self) -> Vec<LocalSendPeer> {
        self.peers.lock().values().cloned().collect()
    }

    pub fn upsert(&self, mut peer: LocalSendPeer) -> LocalSendPeer {
        // 对端自报的指纹会拼成设备 id；它必须是一个安全路径段（`ls-<64hex>` 正好合法）。
        // 不合法就原样返回、不入表 —— 表里绝不留一个 id 不能当 URL/路径段用的行。
        if !crate::ids::is_segment(&peer.id) {
            return peer;
        }
        let mut peers = self.peers.lock();
        if let Some(existing) = peers.get_mut(&peer.id) {
            // 没有出示证书的自报身份，不许把一条**还在线**的行搬到别的地址：那等于让
            // 同一段里任何一台机器把「你正在发文件的那台设备」顶掉（后续发送全打到
            // 它身上，而钉住的证书又让握手失败）。证书核过的、或旧行已经离线（DHCP
            // 换地址）时照常更新。
            let same_endpoint = existing.ip == peer.ip && existing.port == peer.port;
            if same_endpoint || peer.certified || !existing.alive() {
                existing.ip = peer.ip.clone();
                existing.port = peer.port;
                existing.protocol = peer.protocol.clone();
                existing.download = peer.download;
            }
            existing.alias = peer.alias.clone();
            existing.device_model = peer.device_model.clone();
            existing.device_type = peer.device_type.clone();
            existing.last_seen = peer.last_seen;
            existing.seen_at = peer.seen_at;
            existing.online = peer.online || existing.alive();
            existing.certified = existing.certified || peer.certified;
            return existing.clone();
        }
        peer.online = true;
        peers.insert(peer.id.clone(), peer.clone());
        if peers.len() > MAX_PEERS {
            // 先淘汰最老的离线行；没有离线行就淘汰最老的一行。只影响发现结果，不丢用户数据。
            let victim = peers
                .iter()
                .min_by_key(|(_, p)| {
                    (if p.online { 1 } else { 0 }, p.last_seen)
                })
                .map(|(id, _)| id.clone());
            if let Some(id) = victim {
                peers.remove(&id);
            }
        }
        peer
    }

    pub fn remove(&self, id: &str) -> bool {
        self.peers.lock().remove(id).is_some()
    }

    /// 收到公告/注册时把“看得见”推进；在线由成功 register 决定。
    pub fn mark_seen(&self, id: &str) {
        if let Some(p) = self.peers.lock().get_mut(id) {
            p.seen_at = Instant::now();
        }
    }

    pub fn set_online(&self, id: &str, online: bool) {
        if let Some(p) = self.peers.lock().get_mut(id) {
            p.online = online;
            if online {
                p.last_seen = Instant::now();
            }
        }
    }

    pub fn touch_alive(&self, id: &str) {
        if let Some(p) = self.peers.lock().get_mut(id) {
            p.online = true;
            p.last_seen = Instant::now();
            p.seen_at = Instant::now();
        }
    }

    pub fn clear(&self) {
        self.peers.lock().clear();
    }

    /// 丢掉每设备的出站 lane（sender 全放掉，worker 把队列里剩下的跑完就退出）。
    ///
    /// 关开关 / 切离线时用：还在排队的文本/图片会在入口看到"已关闭"直接落失败，不会
    /// 带着整张图片的字节在用户以为已经关掉之后继续外发。
    pub fn clear_lanes(&self) {
        self.lanes.lock().clear();
    }

    /// 设备列表的合并行：与 OrbDrop 的 `PeerDevice` DTO 同形，多一个 `transport`。
    pub fn rows(&self, remarks: &HashMap<String, String>) -> Vec<serde_json::Value> {
        let mut out = Vec::new();
        for p in self.peers.lock().values() {
            out.push(json!({
                "id": p.id,
                "name": p.alias,
                "ip": p.ip,
                "port": p.port,
                "online": p.alive(),
                "paired": false,
                "seen": p.seen(),
                "relayVia": null,
                "connected": false,
                "verified": false,
                "linkState": null,
                "remark": remarks.get(&p.id),
                "transport": "localsend",
            }));
        }
        out
    }

    pub fn outbound_cancel(&self, invite_id: &str) -> Option<CancellationToken> {
        self.outbound.lock().get(invite_id).cloned()
    }

    pub fn register_outbound(&self, invite_id: String, cancel: CancellationToken) {
        self.outbound.lock().insert(invite_id, cancel);
    }

    pub fn unregister_outbound(&self, invite_id: &str) {
        self.outbound.lock().remove(invite_id);
    }

    pub fn cancel_all_outbound(&self) {
        for (_, c) in self.outbound.lock().drain() {
            c.cancel();
        }
    }

    /// 把一条出站任务排进 `peer_id` 的 lane（FIFO）。同一条 lane 上的任务逐个 `await`：
    /// 前一条 `prepare-upload` 收尾（文本 = 用户点了接受；图片 = 上传完成）之后才发下一条。
    ///
    /// 为什么必须串行：LocalSend 接收端（官方 App 与我们的 `server::prepare_upload`）
    /// 只有一个会话槽，并发请求除了第一条都会当场 409；文本消息没有"重试/补发"语义，
    /// 409 就是永久丢消息（界面落「未送达」）。lane 由命令线程同步入队，因此顺序就是
    /// 用户点发送的顺序。
    pub fn enqueue_outbound(&self, peer_id: &str, mut job: OutboundJob) {
        let mut lanes = self.lanes.lock();
        if let Some(tx) = lanes.get(peer_id) {
            match tx.send(job) {
                Ok(()) => return,
                // 通道已关（worker 退出/状态被替换）：换一条新 lane，别丢这条任务。
                Err(err) => {
                    lanes.remove(peer_id);
                    job = err.0;
                }
            }
        }
        let (tx, mut rx) = mpsc::unbounded_channel::<OutboundJob>();
        let _ = tx.send(job);
        lanes.insert(peer_id.to_string(), tx);
        drop(lanes);
        crate::task::spawn("localsend outbound lane", async move {
            while let Some(job) = rx.recv().await {
                job.await;
            }
        });
    }

    /// 当前运行时状态（供 `localsend_status` 命令与事件载荷用）。
    pub fn status(&self, enabled: bool, online: bool) -> LocalSendStatus {
        let runtime = self.runtime.lock().clone();
        LocalSendStatus {
            enabled,
            online,
            discovery: runtime.discovery,
            discovery_error: runtime.discovery_error,
            standard_port: runtime.standard,
            standard_error: runtime.standard_error,
            advertised_port: self.advertised_port(),
        }
    }

    pub fn set_standard(&self, standard: StandardPortState, error: Option<String>) {
        let mut runtime = self.runtime.lock();
        runtime.standard = standard;
        runtime.standard_error = error;
    }

    pub fn standard(&self) -> StandardPortState {
        self.runtime.lock().standard
    }

    pub fn set_discovery(&self, ok: bool, error: Option<String>) {
        let mut runtime = self.runtime.lock();
        runtime.discovery = ok;
        runtime.discovery_error = error;
    }

    pub fn discovery_ok(&self) -> bool {
        self.runtime.lock().discovery
    }

    pub fn discovery_error(&self) -> Option<String> {
        self.runtime.lock().discovery_error.clone()
    }

    fn reset_runtime(&self) {
        *self.runtime.lock() = Runtime::default();
    }

    /// 对端公告里该写的 HTTP 端口（0 = 服务没起）。
    pub fn advertised_port(&self) -> u16 {
        self.port.load(Ordering::SeqCst)
    }
}

/// 本机对外的 LocalSend 信息。
///
/// `port` 是**对端该拨的端口**：独立监听（TCP 53317）在跑就是它，否则是主监听端口
/// （配置端口被占时会顺延）。见 [`start_http`]。
pub fn local_info(state: &AppState) -> dto::DeviceInfo {
    let (name, configured_port) = {
        let cfg = state.config.lock();
        (cfg.device_name.clone(), cfg.port)
    };
    // 公告端口必须是**对端真的能连上的那个**。LocalSend 的独立监听跑着就写 53317：
    // 那是协议默认端口，写死端口的第三方实现只认它；否则写主监听端口（官方实现会读
    // 公告里的 `port`）。见 `start_http`。
    let advertised = state.localsend.advertised_port();
    let serving = state
        .server_port
        .load(std::sync::atomic::Ordering::SeqCst);
    let port = if advertised != 0 {
        advertised
    } else if serving != 0 {
        serving
    } else {
        configured_port
    };
    let fingerprint = crate::net::local_fingerprint()
        .unwrap_or_default()
        .to_ascii_uppercase();
    dto::DeviceInfo {
        alias: name,
        version: dto::PROTOCOL_VERSION.to_string(),
        device_model: Some(device_model()),
        device_type: Some(if cfg!(mobile) { "mobile" } else { "desktop" }.to_string()),
        fingerprint,
        port,
        protocol: "https".to_string(),
        download: false,
    }
}

pub fn info_response(state: &AppState) -> dto::InfoResponse {
    let info = local_info(state);
    dto::InfoResponse {
        alias: info.alias,
        version: info.version,
        device_model: info.device_model,
        device_type: info.device_type,
        fingerprint: info.fingerprint,
        download: info.download,
    }
}

fn device_model() -> String {
    if cfg!(target_os = "android") {
        "Android".to_string()
    } else if cfg!(target_os = "macos") {
        "macOS".to_string()
    } else if cfg!(target_os = "windows") {
        "Windows".to_string()
    } else {
        "Linux".to_string()
    }
}

pub fn id_for_fingerprint(fingerprint: &str) -> String {
    format!("ls-{}", fingerprint.to_ascii_lowercase())
}

/// 把对端自报的指纹收敛成小写 SHA-256 hex（64 位）。
///
/// 形状不对返回 `None`。LocalSend 规范要求这里是证书 DER 的 SHA-256（通常大写），但
/// **第三方实现不保证**：真机上见过把设备 UUID（`E44E30D3-4553-4D…`）填进这个字段的
/// 实现。UUID 拿去钉 TLS 必然每次都被拒（对端地址上一开发现就是每 5 秒一条"指纹不一致"，
/// 而且对端明明是一台正常的第三方 LocalSend），所以这一档要当"没有可钉的身份"处理，
/// 由 [`announced_pin`] 退回 TOFU，再以握手证书为准。
///
/// 分隔符（`:`, `-`, 空白）先去掉：有的实现按 4 位一组写指纹；去完仍是 64 位 hex 才算数
/// （32 位的 UUID 去掉分隔符后只有 32 位，自然出局）。
fn clean_fingerprint(raw: &str) -> Option<String> {
    let raw = raw.trim();
    if raw.is_empty() || raw.len() > 128 {
        return None;
    }
    let hex: String = raw
        .chars()
        .filter(|c| !matches!(c, ':' | '-' | ' '))
        .map(|c| c.to_ascii_lowercase())
        .collect();
    if hex.len() != 64 || !hex.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    Some(hex)
}

/// 公告里的自报指纹该按哪种方式钉。
///
/// 只有"确实是 64 位 hex"才按它钉死；其余自查值（UUID、未知格式）不构成可验证的身份，
/// 按 TOFU 接受证书，握手后由 [`crate::net::Trust::observed_at`] 记下真实指纹。
fn announced_pin(announced: &str) -> crate::net::DialPin {
    match clean_fingerprint(announced) {
        Some(fingerprint) => crate::net::DialPin::Fingerprint(fingerprint),
        None => crate::net::DialPin::Tofu,
    }
}

/// 本机证书指纹（小写 SHA-256）。缓存一份：`register` 可能每 5 秒来一次，
/// 每次都读/解析 PEM 不值当。
pub fn own_fingerprint() -> &'static str {
    static OWN: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
        crate::net::local_fingerprint()
            .unwrap_or_default()
            .to_ascii_lowercase()
    });
    OWN.as_str()
}

pub fn enabled(state: &AppState) -> bool {
    state.config.lock().localsend_enabled
}

/// 启动 LocalSend 服务（幂等：先停旧的，再按当前配置起）。
pub fn start(state: &Arc<AppState>) {
    if !enabled(state) || !state.is_online() {
        stop(state);
        return;
    }
    teardown(state);
    let port = state.server_port.load(Ordering::SeqCst);
    if port == 0 {
        warn!("LocalSend 未启动：HTTP 服务器还没有绑定端口");
        state.localsend.set_standard(StandardPortState::Stopped, None);
        state.localsend.set_discovery(
            false,
            Some("HTTP 服务器还没有绑定端口".to_string()),
        );
        emit_status(state);
        state.emit_devices_changed();
        return;
    }
    // 先挂 HTTP（公告里要写它的端口），再起发现。
    let StartedHttp {
        state: standard,
        error,
        handle,
    } = start_http(state, port, HTTP_PORT);
    if matches!(
        standard,
        StandardPortState::Busy | StandardPortState::Failed
    ) {
        warn!(
            port = HTTP_PORT,
            error = ?error,
            "LocalSend 独立监听未启动，公告回落到主端口"
        );
    }
    state.localsend.set_standard(standard, error);
    *state.localsend.http.lock() = handle;
    match discovery::start(state.clone()) {
        Ok(handle) => {
            *state.localsend.service.lock() = Some(handle);
            state.localsend.set_discovery(true, None);
            info!(
                port = state.localsend.advertised_port(),
                "LocalSend 兼容已启动"
            );
        }
        Err(e) => {
            state.localsend.set_discovery(false, Some(e.clone()));
            warn!("LocalSend 发现启动失败（HTTP 面仍可手动直连）：{e}");
        }
    }
    *state.localsend.guard.lock() = Some(spawn_guard(state.clone()));
    emit_status(state);
    state.emit_devices_changed();
}

/// `start_http` 的结果：除句柄外还要把“为什么没有监听 53317”带回调用方。
struct StartedHttp {
    state: StandardPortState,
    error: Option<String>,
    handle: Option<ServiceHandle>,
}

/// 在 `listen_port` 上挂一份只含 LocalSend 路由的 HTTPS 监听。
///
/// **为什么需要第二个监听**：LocalSend 协议的默认 HTTP 端口是 53317，而第三方实现
/// （真机：localsend-go 系）发送时把目标端口**写死**成 53317，不读公告里的 `port`。
/// 本机主监听通常是 OrbDrop 的端口（默认 3890），只挂主端口时会出现"我们能发给它、
/// 它发不过来"的单向故障：它的 `POST /prepare-upload` 连 53317 直接被拒。
///
/// 行为：
/// - 主监听恰好就是 `listen_port`：不需要第二份，公告写它；
/// - 绑定成功：公告写 `listen_port`，官方/第三方都够得着；
/// - 绑定失败（本机同时跑着官方 LocalSend 等）：**不是错误**，公告回落到主端口
///   （官方实现读公告，仍能互通；写死端口的实现此时本来就只能连到那另一个程序）。
///
/// `listen_port` 做成参数只为测试能换一个空闲端口；生产固定 [`HTTP_PORT`]。
///
/// 这里只 `debug!`：启动时由 [`start`] 打一条 warn，重试由 guard 在状态变化时打日志，
/// 否则 53317 被长期占用会每轮重试刷一条。
fn start_http(state: &Arc<AppState>, main_port: u16, listen_port: u16) -> StartedHttp {
    if main_port == listen_port {
        state.localsend.port.store(listen_port, Ordering::SeqCst);
        return StartedHttp {
            state: StandardPortState::MainIsStandard,
            error: None,
            handle: None,
        };
    }
    let listener = match StdTcpListener::bind((Ipv4Addr::UNSPECIFIED, listen_port)) {
        Ok(listener) => listener,
        Err(e) => {
            state.localsend.port.store(main_port, Ordering::SeqCst);
            debug!(port = listen_port, error = %e, "LocalSend 默认端口绑定失败");
            let state = if e.kind() == std::io::ErrorKind::AddrInUse {
                StandardPortState::Busy
            } else {
                StandardPortState::Failed
            };
            return StartedHttp {
                state,
                error: Some(e.to_string()),
                handle: None,
            };
        }
    };
    if let Err(e) = listener.set_nonblocking(true) {
        debug!(port = listen_port, error = %e, "LocalSend 独立监听无法设为非阻塞");
        state.localsend.port.store(main_port, Ordering::SeqCst);
        return StartedHttp {
            state: StandardPortState::Failed,
            error: Some(e.to_string()),
            handle: None,
        };
    }
    let identity = match crate::net::load_or_create_identity() {
        Ok(identity) => identity,
        Err(e) => {
            debug!(error = %e, "LocalSend 独立监听读不到本机身份");
            state.localsend.port.store(main_port, Ordering::SeqCst);
            return StartedHttp {
                state: StandardPortState::Failed,
                error: Some(e),
                handle: None,
            };
        }
    };
    let tls = crate::net::server_config(&identity);
    let router = crate::server::localsend_standalone_router(state.clone());
    let cancel = CancellationToken::new();
    let shutdown = cancel.clone();
    let task = crate::task::spawn("localsend-http", async move {
        let listener = match tokio::net::TcpListener::from_std(listener) {
            Ok(listener) => listener,
            Err(e) => {
                warn!(port = listen_port, error = %e, "LocalSend 独立监听启动失败");
                return;
            }
        };
        crate::net::serve(listener, router, tls, async move {
            shutdown.cancelled().await;
        })
        .await;
    });
    // 先落端口再返回：公告的第一拍在 100ms 后，读到的必须是这个值。
    state.localsend.port.store(listen_port, Ordering::SeqCst);
    info!(
        port = listen_port,
        "LocalSend 兼容额外监听（协议默认 HTTP 端口）"
    );
    StartedHttp {
        state: StandardPortState::Bound,
        error: None,
        handle: Some(ServiceHandle { cancel, task }),
    }
}

/// 53317 被占用 / UDP 被独占时的自愈任务。
///
/// 只补缺的那一份监听，**不调用 `restart()`**：那会把正在进行的会话与出站队列一起清掉。
/// 53317 空出来后自动接管；UDP 空出来后自动恢复发现。绑定成功时立刻补发一轮公告。
fn spawn_guard(state: Arc<AppState>) -> ServiceHandle {
    let cancel = CancellationToken::new();
    let shutdown = cancel.clone();
    let task = crate::task::spawn("localsend-port-guard", async move {
        // 首轮 5s 后试：用户可能在开关打开后马上关掉本机 LocalSend。
        let mut delay = Duration::from_secs(5);
        let mut last_ephemeral: Option<Instant> = None;
        loop {
            tokio::select! {
                _ = shutdown.cancelled() => return,
                // 设置页「重试」：立刻试一拍，不等退避。
                _ = state.localsend.guard_wake.notified() => {}
                _ = tokio::time::sleep(delay) => {}
            }
            if !enabled(&state) || !state.is_online() {
                return;
            }
            // 空转的会话（对端 `prepare-upload` 之后一声不吭，或重试用完就消失）会一直
            // 占着单会话槽：此后所有对端的普通文件接收都回 409，界面上也没有入口能清。
            // `spawn_guard` 在启用且在线期间常驻，就借它这一拍顺手回收。
            let reaped = state
                .localsend
                .sessions
                .reap_idle(Duration::from_secs(120));
            if reaped > 0 {
                info!(count = reaped, "LocalSend 回收空转的上传会话");
            }
            let mut changed = false;
            let main_port = state.server_port.load(Ordering::SeqCst);
            let standard = state.localsend.standard();

            // 1) 53317 的独立监听：只在缺失时补。
            if main_port != 0
                && matches!(
                    standard,
                    StandardPortState::Busy
                        | StandardPortState::Failed
                        | StandardPortState::Stopped
                )
            {
                let StartedHttp {
                    state: next,
                    error,
                    handle,
                } = start_http(&state, main_port, HTTP_PORT);
                if shutdown.is_cancelled() {
                    // `teardown` 可能在同步的 bind 之后、存句柄之前跑完：这份新监听
                    // 没人收，必须在这里自己收掉，否则 stop 之后端口还开着。
                    if let Some(handle) = handle {
                        handle.cancel.cancel();
                        handle.task.abort();
                    }
                    return;
                }
                *state.localsend.http.lock() = handle;
                state.localsend.set_standard(next, error);
                if next != standard {
                    changed = true;
                }
                if next == StandardPortState::Bound {
                    info!(port = HTTP_PORT, "LocalSend 已重新接管 53317");
                    changed = true;
                    state.localsend.announce_notify.notify_one();
                    // 发现 supervisor 没跑时，notify 没人接；用临时端口把新端口立刻播出去。
                    if !state.localsend.discovery_ok() {
                        discovery::announce_ephemeral(&state).await;
                        last_ephemeral = Some(Instant::now());
                    }
                }
            }

            // 2) UDP 发现：绑定失败时继续重试；恢复前用临时端口公告，别让对端完全看不到本机。
            if main_port != 0 && !state.localsend.discovery_ok() {
                let started = discovery::start(state.clone());
                if shutdown.is_cancelled() {
                    if let Ok(handle) = started {
                        handle.cancel.cancel();
                        handle.task.abort();
                    }
                    return;
                }
                match started {
                    Ok(handle) => {
                        *state.localsend.service.lock() = Some(handle);
                        state.localsend.set_discovery(true, None);
                        info!("LocalSend 发现服务已恢复");
                        changed = true;
                    }
                    Err(e) => {
                        if state.localsend.discovery_error().as_deref() != Some(e.as_str()) {
                            state.localsend.set_discovery(false, Some(e));
                            changed = true;
                        }
                        let due = last_ephemeral
                            .map_or(true, |at| at.elapsed() >= Duration::from_secs(60));
                        if due {
                            discovery::announce_ephemeral(&state).await;
                            last_ephemeral = Some(Instant::now());
                        }
                    }
                }
            }

            if changed {
                emit_status(&state);
                state.emit_devices_changed();
            }

            let ready = matches!(
                state.localsend.standard(),
                StandardPortState::Bound | StandardPortState::MainIsStandard
            ) && state.localsend.discovery_ok();
            delay = if ready {
                Duration::from_secs(60)
            } else {
                (delay * 2).min(Duration::from_secs(60))
            };
        }
    });
    ServiceHandle { cancel, task }
}

/// 拆掉全部 LocalSend 后台任务与临时状态。**不**发事件 —— 由 `start`/`stop` 统一发，
/// 避免“重启”时界面先看到一次 stopped 再看到 running。
fn teardown(state: &Arc<AppState>) -> bool {
    let service = state.localsend.service.lock().take();
    let http = state.localsend.http.lock().take();
    let guard = state.localsend.guard.lock().take();
    let had_service = service.is_some() || http.is_some();
    if let Some(handle) = service {
        handle.cancel.cancel();
        handle.task.abort();
    }
    if let Some(handle) = http {
        handle.cancel.cancel();
        handle.task.abort();
    }
    if let Some(handle) = guard {
        handle.cancel.cancel();
        handle.task.abort();
    }
    state.localsend.port.store(0, Ordering::SeqCst);
    let cancelled = state.localsend.sessions.cancel_all();
    state.localsend.cancel_all_outbound();
    // 文本/图片排队走的是每设备 lane（不在这张表里）：把 sender 丢掉让 worker 自己收尾，
    // 排队中的任务在 `send_text`/`send_image` 入口会看到"已关闭"直接落失败。
    state.localsend.clear_lanes();
    state.localsend.clear();
    state.localsend.reset_runtime();
    // 接收侧不会再有别的收尾路径（上传 handler 那边 `mark_failed` 找不到会话，发不出事件）：
    // 在这里把每条有卡的会话如实落成「已取消」，否则界面会一直停在"接收中"。
    for invite_id in cancelled {
        crate::ws::emit_ui(
            state,
            crate::ws::events::ui::PUSH_DONE,
            &json!({
                "inviteId": invite_id,
                "status": "cancelled",
                "reason": "已取消",
                "transport": "localsend",
            }),
        );
    }
    had_service
}

pub fn start_if_enabled(state: &Arc<AppState>) {
    if enabled(state) && state.is_online() {
        start(state);
    }
}

pub fn stop(state: &Arc<AppState>) {
    if teardown(state) {
        info!("LocalSend 兼容已停止");
    }
    emit_status(state);
    state.emit_devices_changed();
}

pub fn restart(state: &Arc<AppState>) {
    teardown(state);
    if enabled(state) && state.is_online() {
        start(state);
    } else {
        emit_status(state);
        state.emit_devices_changed();
    }
}

pub fn set_online(state: &Arc<AppState>, online: bool) {
    if online {
        start_if_enabled(state);
    } else {
        stop(state);
    }
}

/// 设置页「重试」：叫醒 guard 立刻补一拍。**不**重启兼容层，不会掐断在途会话。
pub fn retry_now(state: &Arc<AppState>) {
    state.localsend.guard_wake.notify_one();
}

/// 把当前运行时状态推给界面（设置页的状态行）。
pub fn emit_status(state: &AppState) {
    let status = state.localsend.status(enabled(state), state.is_online());
    if let Ok(value) = serde_json::to_value(status) {
        crate::ws::emit_ui(state, crate::ws::events::ui::LOCALSEND_STATUS, &value);
    }
}

/// 按 IP 的简单窗口限流。只给公开的 LocalSend 路由用，挡住“换 id 连发”的弹窗轰炸。
pub struct RateLimiter {
    hits: HashMap<IpAddr, (Instant, u32)>,
}

impl RateLimiter {
    pub fn new() -> Self {
        Self {
            hits: HashMap::new(),
        }
    }

    pub fn allow(&mut self, ip: IpAddr, max: u32, window: Duration) -> bool {
        let now = Instant::now();
        // 顺手清掉过期的键，表不随历史 IP 增长。
        if self.hits.len() > 512 {
            self.hits.retain(|_, (at, _)| now.duration_since(*at) < window);
        }
        let entry = self.hits.entry(ip).or_insert((now, 0));
        if now.duration_since(entry.0) >= window {
            *entry = (now, 0);
        }
        if entry.1 >= max {
            return false;
        }
        entry.1 += 1;
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprints_are_normalized_or_rejected() {
        assert_eq!(
            clean_fingerprint("AB:CD:EF:01:23:45:67:89:AB:CD:EF:01:23:45:67:89:AB:CD:EF:01:23:45:67:89:AB:CD:EF:01:23:45:67:89"),
            Some("abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".into())
        );
        assert_eq!(
            clean_fingerprint("ABCD-EF01-2345-6789-ABCD-EF01-2345-6789-ABCD-EF01-2345-6789-ABCD-EF01-2345-6789"),
            Some("abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789".into())
        );
        assert_eq!(
            clean_fingerprint("AB:CD:EF:01:23:45:67:89:AB:CD:EF:01:23:45:67:89"),
            None,
            "半截指纹不是身份"
        );
        assert_eq!(clean_fingerprint(""), None);
        assert_eq!(clean_fingerprint("zz"), None);
        assert_eq!(clean_fingerprint("not hex!!"), None);
    }

    /// 第三方实现的真实形状：设备 UUID 填在 `fingerprint` 里。它不是证书哈希，钉上去
    /// 每次握手都会拒绝；只能退回 TOFU，由握手证书接替身份。
    #[test]
    fn a_uuid_announcement_is_not_a_pinnable_fingerprint() {
        let uuid = "E44E30D3-4553-4D02-9A1B-7C6F5E4D3C2B";
        assert_eq!(clean_fingerprint(uuid), None);
        assert_eq!(announced_pin(uuid), crate::net::DialPin::Tofu);
        assert_eq!(
            announced_pin("E44E30D345534D029A1B7C6F5E4D3C2B"),
            crate::net::DialPin::Tofu,
            "UUID 去掉分隔符是 32 位，仍然不是 SHA-256"
        );
    }

    #[test]
    fn a_real_fingerprint_is_pinned_lowercase() {
        let fp = "E44E30D345534D029A1B7C6F5E4D3C2B0A1B2C3D4E5F60718293A4B5C6D7E8F0";
        assert_eq!(
            announced_pin(fp),
            crate::net::DialPin::Fingerprint(fp.to_ascii_lowercase())
        );
    }

    fn peer_row(fingerprint: &str, ip: &str, last_seen: Instant) -> LocalSendPeer {
        LocalSendPeer {
            id: id_for_fingerprint(fingerprint),
            fingerprint: fingerprint.to_string(),
            alias: "Peer".to_string(),
            device_model: None,
            device_type: None,
            ip: ip.to_string(),
            port: 53317,
            protocol: "https".to_string(),
            download: false,
            online: true,
            seen_at: last_seen,
            last_seen,
            certified: false,
        }
    }

    /// 自报指纹（公告里明文广播）不能把一台**还在线**的设备从别的地址上顶掉；
    /// 地址真变了（旧行已经离线）才认。
    #[test]
    fn a_claimed_fingerprint_cannot_move_a_live_row_to_another_address() {
        let state = LocalSendState::new();
        let fp = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
        assert!(state.claim_is_plausible(fp, "10.0.0.5"), "没见过 = 可以登记");
        let row = peer_row(fp, "10.0.0.5", Instant::now());
        state.peers.lock().insert(row.id.clone(), row);
        assert!(state.claim_is_plausible(fp, "10.0.0.5"), "同一地址照常");
        assert!(
            !state.claim_is_plausible(fp, "10.0.0.66"),
            "在线设备的身份不能被别的地址冒用"
        );
        {
            let mut peers = state.peers.lock();
            let row = peers.values_mut().next().unwrap();
            row.last_seen = Instant::now() - Duration::from_secs(600);
        }
        assert!(
            state.claim_is_plausible(fp, "10.0.0.66"),
            "旧行已离线（DHCP 换了地址）时要能接回来"
        );
    }

    /// 静默接收图片的总闸：单批有上限、批数没有，这里按小时跨发送方封顶。
    #[test]
    fn auto_image_bytes_are_capped_per_hour() {
        let state = LocalSendState::new();
        assert!(state.take_auto_image_budget(1500 * 1024 * 1024));
        assert!(
            !state.take_auto_image_budget(1024 * 1024 * 1024),
            "一小时 2GiB 之后必须停下来"
        );
        {
            let mut budget = state.auto_budget.lock();
            budget.window = Instant::now() - Duration::from_secs(3601);
        }
        assert!(
            state.take_auto_image_budget(1024 * 1024 * 1024),
            "窗口翻篇后重新有预算"
        );
    }

    /// 发现报文按来源限速：一条伪造报文不该变成一次无上限的拨号洪泛。
    #[test]
    fn datagrams_from_one_source_are_rate_limited() {
        let state = LocalSendState::new();
        let ip: IpAddr = "10.0.0.9".parse().unwrap();
        let allowed = (0..200).filter(|_| state.allow_datagram(ip)).count();
        assert_eq!(allowed, 120);
        assert!(state.allow_datagram("10.0.0.10".parse().unwrap()), "别的来源不受影响");
    }

    /// 协议的默认 HTTP 端口（53317）上必须能直接收发：第三方实现（真机：localsend-go
    /// 系）发送时把端口写死成 53317，不读公告里的 `port`。这条监听只挂 LocalSend 路由，
    /// OrbDrop 自己的路由一条都不该从这里露出去。
    #[tokio::test]
    async fn the_default_localsend_port_serves_only_localsend_routes() {
        let mut config = crate::config::test_config();
        config.localsend_enabled = true;
        let name = config.device_name.clone();
        let state = Arc::new(AppState::new(
            config,
            std::env::temp_dir().join(format!("orbdrop_ls_http_{}.toml", uuid::Uuid::new_v4())),
        ));
        let port = {
            let probe = StdTcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            probe.local_addr().unwrap().port()
        };
        let StartedHttp {
            state: standard,
            handle,
            ..
        } = start_http(&state, 3890, port);
        assert_eq!(standard, StandardPortState::Bound);
        let handle = handle.expect("第二个监听应能起来");
        assert_eq!(state.localsend.advertised_port(), port);
        assert_eq!(
            local_info(&state).port,
            port,
            "公告端口必须是对端真的连得上的那个"
        );

        let client = crate::net::dial_client("127.0.0.1", &crate::net::DialPin::Tofu);
        let info: serde_json::Value = client
            .get(format!("https://127.0.0.1:{port}/api/localsend/v2/info"))
            .send()
            .await
            .expect("独立监听应接受 HTTPS 请求")
            .json()
            .await
            .unwrap();
        assert_eq!(info["alias"], name);
        // 独立监听只挂 LocalSend 面：OrbDrop 的受保护路由不能从这里露出去。
        let other = client
            .get(format!("https://127.0.0.1:{port}/shares"))
            .send()
            .await
            .unwrap();
        assert_eq!(other.status(), 404);

        handle.cancel.cancel();
        handle.task.abort();
        state.localsend.port.store(0, Ordering::SeqCst);
    }

    /// 53317 被占用不是“打开失败”：独立监听放弃，公告回落到主端口，状态是 Busy。
    #[test]
    fn busy_standard_port_falls_back_to_the_main_port() {
        let mut config = crate::config::test_config();
        config.localsend_enabled = true;
        let state = Arc::new(AppState::new(
            config,
            std::env::temp_dir().join(format!("orbdrop_ls_busy_{}.toml", uuid::Uuid::new_v4())),
        ));
        // 占住一个随机端口，充当“本机 LocalSend”。
        let busy = StdTcpListener::bind((Ipv4Addr::UNSPECIFIED, 0)).unwrap();
        let busy_port = busy.local_addr().unwrap().port();

        let StartedHttp {
            state: standard,
            error,
            handle,
        } = start_http(&state, 3890, busy_port);
        assert_eq!(standard, StandardPortState::Busy);
        assert!(error.is_some(), "原始错误要留给状态行/排障");
        assert!(handle.is_none(), "绑定失败不应产生监听任务");
        assert_eq!(
            state.localsend.advertised_port(),
            3890,
            "公告端口必须回落到主端口"
        );
        assert_eq!(local_info(&state).port, 3890);

        drop(busy);
        state.localsend.port.store(0, Ordering::SeqCst);
    }

    /// 设置页按 camelCase 读这几个字段；这里把线上形状钉住，避免改名后前端静默画空。
    #[test]
    fn status_wire_shape_matches_the_settings_panel() {
        assert_eq!(
            serde_json::to_value(StandardPortState::MainIsStandard).unwrap(),
            "mainIsStandard"
        );
        assert_eq!(
            serde_json::to_value(StandardPortState::Busy).unwrap(),
            "busy"
        );
        assert_eq!(
            serde_json::to_value(StandardPortState::Failed).unwrap(),
            "failed"
        );
        let status = LocalSendStatus {
            enabled: true,
            online: false,
            discovery: true,
            discovery_error: None,
            standard_port: StandardPortState::Busy,
            standard_error: Some("in use".into()),
            advertised_port: 3890,
        };
        let value = serde_json::to_value(status).unwrap();
        assert_eq!(value["standardPort"], "busy");
        assert_eq!(value["advertisedPort"], 3890);
        assert_eq!(value["discoveryError"], serde_json::Value::Null);
        assert_eq!(value["standardError"], "in use");
    }
}