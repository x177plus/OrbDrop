use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU16, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::http::StatusCode;
use mdns_sd::ServiceDaemon;
use parking_lot::Mutex;
use serde::Serialize;
use tauri::AppHandle;
use tauri::Emitter;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use crate::config::{Config, PeerDevice};
use crate::pair::PairStore;
use crate::transfer::push_job::PushStore;
use crate::ws::WsManager;

/// 长连接意图（F-16）：`PeerDevice::auto_connect` / `PeerDevice::paused` 这对字段的语义，
/// 也是它们的唯一写点 [`AppState::set_link_intent`] 的入参。
///
/// 两个字段**互斥**（同一个意图的两面：本机要不要拨 / 接不接对端的自动重拨），所以
/// 它们不该被分头写 —— 一边写"不拨"、另一边忘了写"不接"，就得到那条自相矛盾的状态：
/// 本机安静，却被对端的记忆把链路接回来。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LinkIntent {
    /// 要这条链：点过「连接」，或链路已经建起来（被拨号的一端同样算）。
    Wanted,
    /// 点过「断开」：本机不主动拨，也拒绝对端的自动重拨（403）。
    Paused,
}

impl LinkIntent {
    /// 落到地址簿的那对字段：(本机主动拨, 拒绝对端的自动重拨)。
    fn fields(self) -> (bool, bool) {
        match self {
            LinkIntent::Wanted => (true, false),
            LinkIntent::Paused => (false, true),
        }
    }
}

/// Application-wide shared state, accessible from Tauri commands and the HTTP server.
pub struct AppState {
    pub config_path: PathBuf,
    pub config: Mutex<Config>,
    /// Known devices: paired + discovered + manual (merged view).
    pub peers: Mutex<Vec<PeerDevice>>,
    pub server_port: AtomicU16,
    pub server_shutdown: Mutex<Option<oneshot::Sender<()>>>,
    pub server_task: Mutex<Option<JoinHandle<()>>>,
    /// Kept alive for the app's lifetime to keep the mDNS registration live.
    pub mdns_daemon: Mutex<Option<ServiceDaemon>>,
    /// WebSocket manager (peer links).
    pub ws: WsManager,
    /// Push jobs (sender + receiver) and resume store.
    pub push: PushStore,
    /// 拖拽下载的票据表（暂存副本 / 已备好的本机路径）。
    pub drag: crate::drag::DragStore,
    /// Pairing code store.
    pub pair: PairStore,
    /// 会话（聊天）记录：文本消息的落盘 + 界面快照（`chat.json`）。
    pub chat: crate::chat::ChatStore,
    /// 群聊（共享群）：名册 + 群消息（`groups.json`）。
    ///
    /// 与 `chat.json` **分开一个文件**：老版本进程读 `chat.json`（serde 静默忽略不认识的
    /// 顶层键）再原样写回，会把新增的顶层键抹掉 —— 名册与群消息丢了不可恢复。
    pub groups: crate::group::GroupStore,
    /// Tauri handle for emitting UI events.
    pub app_handle: Mutex<Option<AppHandle>>,
    /// File watchers keyed by share id.
    pub watchers: Mutex<std::collections::HashMap<String, notify::RecommendedWatcher>>,
    /// 「学来的」设备（F-10 跨网段）：对端 `/discover` 表里带过来、而本机自己从没发现过的
    /// 设备 id → 最近一次被提到的时刻。
    ///
    /// 为什么单开一张表而不是塞进 `PeerDevice`：这个状态**只属于本进程**（重启即忘），
    /// 而 `PeerDevice` 是要落盘进地址簿的形状。存在这里的 id，`peers` 里对应那行就是
    /// "临时条目"—— 不写地址簿、不被探活接管、超时由 `sweep_learned` 收走。
    pub learned: Mutex<HashMap<String, Instant>>,
    /// 本机替别人开出去的中转端点（F-106，运行时；见 `relay`）。开关一关就全收。
    pub relays: crate::relay::RelayRegistry,
    /// 本机替别人中转**累计搬过的字节**（收发两方向之和，F-106 / S-09 的用量记录）。
    ///
    /// 两个来源合流到这一个数：① 盲转的连接字节（每条连接的 `pump` 边搬边加，见
    /// `relay::CountingStream` —— 长连接也能看到数在涨，而不是等到断开才跳一下）；
    /// ② 替群聊转发出去的消息字节（[`AppState::note_relay_message`]）。
    /// 启动时从 [`Config::relay_bytes_total`] 接着算，落盘见 [`AppState::note_relay_traffic`]。
    pub relay_bytes: AtomicU64,
    /// 本机替别人中转过的**连接数**（累计，与 [`AppState::relay_bytes`] 同一份记录）。
    pub relay_conns: AtomicU64,
    /// 对端自报的"本机愿意替别人中转"能力（`/pair/info`、`/discover` 里的 `relay` 位）：
    /// 挑中转方的依据之一。**运行时缓存**，随 5s 探活刷新（对端关掉开关后，最多一拍就
    /// 不再被选中）。
    relay_capable: Mutex<HashSet<String>>,
    /// 请求方侧的限流：某台"够不着的"设备最近一次尝试开中转的时刻（`relay::ensure_for`）。
    pub relay_attempts: Mutex<HashMap<String, Instant>>,
    /// 「看得见但连不上」（F-106）：某台设备最近一次**被 mDNS 记录证实存在**的时刻。
    ///
    /// 与 `online`（探活成功 = 真的连得上）**刻意分开**。这一位给界面的用处只有两处：
    /// ① 离线且未绑定的那一行照旧列出来（否则"闪一下就没"）；② 用户**发起连接**（配对 /
    /// 连接 / 立即连接）时，界面据此补一句"这张网可能把单播隔开了"的 tip。它**不是**一个
    /// 状态档 —— 那一行照常画「未绑定」/「离线」，理由只在拨号那一刻说。
    /// 记录的存续本身就是信号：mDNS 缓存里的记录被撤（对端发 TTL=0 告别、或 TTL 过期）
    /// 时这里跟着清掉。只属于本进程（重启即忘），与 `learned` 同一档。
    pub seen: Mutex<HashMap<String, Instant>>,
    /// 对端**自报的网段**（F-107）：`设备 id → 它自己报的网段列表`（`192.168.1.0/24` 这种）。
    ///
    /// 来源三处，全都是对端自己说的：mDNS TXT 的 `nets`（同段零请求）、`/pair/info` 与
    /// `/discover` 的活回话（跨段唯一来源）。**本机的掩码对远端网段不适用**，所以"这台设备
    /// 属于哪一段"只能靠它自报；算不出来时就退回"它落在本机哪块网卡的段里"（同段场景）。
    peer_nets: Mutex<HashMap<String, Vec<String>>>,
    /// 中转池（F-107）：`中转网段 → 该段里「自报允许中转 + 在线」的设备`。
    ///
    /// 活视野：每次锚点的 `/discover` 回来就整份重建（见 [`AppState::absorb_view`]）。
    /// 落在这一层的**只有**"够得着谁替本机中转"，不是"本机跟它的关系" —— 因此地址取自
    /// 那张表（对端的观察值），配对状态取自本机自己的记录。
    relay_pool: Mutex<HashMap<String, Vec<PoolEntry>>>,
    /// 直连成功连拍（F-107 §9.2）：`设备 id → 连续几拍直连可达`。
    /// 直连连续 3 拍（15s）才清设备级记忆 —— 一次抖动不该把记忆抹掉。
    direct_streak: Mutex<HashMap<String, u32>>,
    /// 反向路由记录的发送状态（F-107 §10.3）：`对端 id → (内容哈希, 上次发送, 已被确认)`。
    /// 内容没变就不重发（否则每台已配对设备每 5s 来一发），最短 30s 一拍。
    route_push: Mutex<HashMap<String, RoutePushState>>,
    /// 刚失败过的中转方（F-107 §8.1）：`目标 id → (中转设备 id, 失败时刻)`。
    ///
    /// 记在 `clear_relay` 那一刻：中转那一跳也探不通时，端点是"刚证明自己不通"的那一台，
    /// 下一轮选路要把它排到最后（而不是又拿它当第一候选、白等一次控制面往返）。
    relay_failed: Mutex<HashMap<String, (String, Instant)>>,
    /// **反向记录带进来的**设备行（F-107 §10.3）：`设备 id → 采纳时刻`。
    ///
    /// 与 `learned`（担保人的视野，20s）分开：这一档的担保是"对端推来的路由记录"，而记录
    /// 只在内容变化或每 5 分钟刷新一次，所以它的 TTL 要长于那个间隔（[`ADOPTED_TTL`]）。
    /// 两个用途：① 有 TTL，不会永久留在设备表里；② 探活时**跳过**它们 —— 本机跟这些设备
    /// 之间没有任何关系，没必要每 5s 敲一遍（要借中转时按需拨，那一次就验证了可达性）。
    adopted: Mutex<HashMap<String, Instant>>,
}

/// 反向记录带进来的设备行的寿命（长于记录 5 分钟一次的刷新间隔）。
///
/// 太短：记录还没重推，池里的地址就没了（借不到中转）；太长：对端换了一批设备之后旧行赖着
/// 不走。15 分钟 = 三次刷新，够稳。
const ADOPTED_TTL: Duration = Duration::from_secs(900);

/// 反向记录一次最多并进来多少台设备（总量上限；单条记录另有 [`crate::relay::MAX_ROUTE_DEVICES`]）。
///
/// 没有上限时，一次 64MB 的请求体就能往设备表里塞上万行，还得每 5s 逐个探活。
const MAX_ADOPTED_DEVICES: usize = 64;

/// 刚失败过的中转方在这么久内排到最后（之后它可能有别的目标还能用，不永久拉黑）。
pub const FAILED_RELAY_COOLDOWN: Duration = Duration::from_secs(300);

/// 中转池里的一台设备（F-107）。地址来自**锚点的** `/discover` 表（它对那一段的观察值）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PoolEntry {
    pub id: String,
    pub name: String,
    pub ip: String,
    pub port: u16,
    /// 本机与它**已配对**（控制面更稳：有 token 可用；没有也能借，见 S-09）。
    pub paired: bool,
    /// 它**最后一次出现在一份快照里**的时刻（本机自己的观察时间，**不序列化**）。
    ///
    /// 只用于**条数封顶**时挑"最久没露面"的那几台淘汰（[`merge_pool`]）—— 池**不按缺席
    /// 淘汰**：一段快照里没有它，不代表它没了。
    #[serde(skip)]
    pub seen_at: Option<Instant>,
}

/// 一个目标段的池最多记多少台。
///
/// **只数"这一段里允许为其它设备中转、而且真的被报给我们过的"那几台** —— 段里设备再多，
/// 没开 S-09 的一台都不进池（`/discover` 的 `relay` 位为假、中转方自报里也不含它们）。
/// 所以家用/办公的段（允许中转的通常一两台）离这个上限远得很；它是**防炸与防积累**的兜底：
/// 池要落盘进 `config.toml`（每 5 秒都可能重写），而"谁都在线"这种正常刷新不该让文件无限长大。
///
/// **不是新鲜度规则**：池不按缺席淘汰；超了才按"最久没露面"淘汰（留下的就是最近见过的那些）。
const MAX_POOL_ENTRIES: usize = 64;

/// 把一份**新快照**（锚点的活视野 / 中转方的自报 / 反向记录）并进某个**目标段**的池。
///
/// 规则（记忆是"**最后已知事实**"，不是"此刻谁在线"）：
/// - 这一份里有的：以它为准（地址、名字、配对），并记下"刚刚见过"（`seen_at`）；
/// - 这一份里**没有的**：留着 —— 一次快照里没露面不等于它没了。**不按缺席淘汰**：这台
///   设备可能只是离线一会儿，而它恰恰是"锚点离线之后要接手的那一台"（真机反馈第七轮）；
/// - `gone`：**第一手**证据的"它不干这活了"（锚点自己在回话头里说它关着中转开关 —— 那是它
///   亲口说的、活的）。表里那台的 `relay:false` 是**二手**知识（锚点对它的能力位可能还没刷新
///   到，见 §19 根因 2），不当删除证据：它只是这一份里没被列进候选；
/// - 只有超出一段的条数上限（[`MAX_POOL_ENTRIES`]）时才淘汰，且按"最久没露面"淘汰 ——
///   这一份里的刚被报过、时间戳最新，不会先被淘汰。整条策略的退休（该段直连稳定恢复、
///   或长期没有一次成功）另见 [`AppState::sweep_relay_memory`] —— 池跟着策略走。
///
/// 返回并好的那一份（调用方据此刷新 `RelayRoute::via_addrs` 落盘）。
fn merge_pool(
    pool: &mut HashMap<String, Vec<PoolEntry>>,
    target: &str,
    fresh: Vec<PoolEntry>,
    gone: &HashSet<String>,
) -> Vec<PoolEntry> {
    let now = Instant::now();
    let old = pool.remove(target).unwrap_or_default();
    // `fresh_ids` 用拥有型 String（下面的循环会把 `fresh` 移走）。
    let fresh_ids: HashSet<String> = fresh.iter().map(|e| e.id.clone()).collect();
    let mut merged: Vec<PoolEntry> = Vec::with_capacity(fresh.len() + old.len());
    for mut e in fresh {
        e.seen_at = Some(now);
        merged.push(e);
    }
    for e in old {
        if fresh_ids.contains(&e.id) || gone.contains(&e.id) {
            continue;
        }
        merged.push(e);
    }
    if merged.len() > MAX_POOL_ENTRIES {
        // 留**最近见过**的那些：按"露面时间"新→旧排，砍掉尾巴（没有时间戳的排最后 = 先淘汰）。
        // 这一份里的刚记过时间戳，排在最前 —— 超上限时不会先淘汰刚刚还被报过的那几台。
        merged.sort_by_key(|e| {
            std::cmp::Reverse((e.seen_at.is_some(), e.seen_at.unwrap_or(now)))
        });
        merged.truncate(MAX_POOL_ENTRIES);
    }
    // 已配对的排前面（控制面更稳），其余按 id 字典序 —— 顺序固定，行为可复现。
    merged.sort_by(|a, b| b.paired.cmp(&a.paired).then_with(|| a.id.cmp(&b.id)));
    pool.insert(target.to_string(), merged.clone());
    merged
}

impl PoolEntry {
    /// 池里这台设备的**可拨地址**（落盘进 `RelayRoute::via_addrs`）。
    ///
    /// 池的 id 拨不动任何东西 —— 地址必须在池自己身上，否则锚点一离线（设备表里那些由它
    /// 带来的行随之被收走）这一池就没有一台还能拨（见 `RelayRoute::via_addrs` 的说明）。
    pub fn to_route_device(&self) -> crate::relay::RouteDevice {
        crate::relay::RouteDevice {
            id: self.id.clone(),
            name: self.name.clone(),
            ip: self.ip.clone(),
            port: self.port,
        }
    }
}

/// 反向路由记录的发送状态（F-107 §10.3）。
#[derive(Debug, Clone)]
struct RoutePushState {
    /// 最近一次发出去的内容哈希（内容没变就不重发）。
    hash: String,
    sent_at: Instant,
    /// 对端回 200 且采纳了。已确认后再变才重发。
    acked: bool,
}

/// 锚点 `/discover` 表里的一台设备（F-107 §5）：身份 + 地址 + **归属段** + 中转能力。
///
/// 与 `PeerDevice` 分开：这是**对端的观察值**（"它那一段有哪些设备"），不是"本机跟它的
/// 关系" —— 所以没有配对、广播、长连接意图，地址也照抄对端报的（用于试着借用中转）。
#[derive(Debug, Clone, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoveredPeer {
    pub id: String,
    #[serde(default)]
    pub name: String,
    pub ip: String,
    pub port: u16,
    /// **报出它的那一方**认为它属于哪一段。老版本不报（`None`）：本机退回按自己的掩码算。
    #[serde(default)]
    pub cidr: Option<String>,
    /// 它是否允许替别人中转（报出方所知）。老版本不报 = 不允许（与 S-09 默认关一致）。
    #[serde(default)]
    pub relay: bool,
}

/// 直连连续几拍可达才清设备级记忆（F-107 §9.2）：一次抖动不该把记忆抹掉。
pub const DIRECT_STABLE_ROUNDS: u32 = 3;

/// 反向路由记录的重发节拍（F-107 §10.3）：未确认或内容变了才发，且最短这么久一拍。
const ROUTE_RESEND_MIN: Duration = Duration::from_secs(30);

/// 已确认且内容没变时的刷新间隔：对端重启后手里没有这条记忆，靠这一拍补回去。
const ROUTE_REFRESH_MIN: Duration = Duration::from_secs(300);

/// 锚点长期不再担保（`seen_at` 超过这么久）→ 删掉那条网段级策略（F-107 §11）。
const ROUTE_TTL_DAYS: i64 = 7;

/// 设备级链路记忆的兜底寿命：这么久没再成功过就忘掉它。
const LINK_TTL_DAYS: i64 = 30;

/// 这个名字有没有信息量：空串、或**就是设备 id 本身**，都算"还不知道它叫什么"。
///
/// `AppState::new`（凭据库里有、地址簿里没有的已配对行）与 `observe_verified_addr`
/// （中转控制面核实过的地址）建行时都拿设备 id 顶着名字当占位。这两处之后各有别的路能
/// 补上真名（mDNS / 会话信封 / [`AppState::fill_peer_name`] / [`AppState::learn_peers`]），
/// 判据统一在这里：**占位名可以被任何一条更可信的路改写，真名永不**（真名是"本机自己
/// 知道的"，不该被对端的转述改写）。
pub(crate) fn name_is_placeholder(name: &str, id: &str) -> bool {
    let n = name.trim();
    n.is_empty() || n == id
}

impl AppState {
    pub fn new(config: Config, config_path: PathBuf) -> Self {
        // Peers: config address book + paired entries from the credential store.
        // Our own device id is never a peer: two instances sharing one credential
        // store (same machine, one OS user) would otherwise both key the same
        // token under each other's id and list a bogus self row.
        let own_id = config.device_id.clone();
        let mut peers: Vec<PeerDevice> = config
            .peers
            .clone()
            .into_iter()
            .filter(|p| p.id != own_id)
            .collect();
        let tokens = crate::credential::load_peers().unwrap_or_default();
        for (id, _) in tokens.iter() {
            if id == &own_id {
                continue;
            }
            if !peers.iter().any(|p| &p.id == id) {
                peers.push(PeerDevice {
                    id: id.clone(),
                    name: id.clone(),
                    ip: String::new(),
                    port: 0,
                    online: false,
                    paired: true,
                    auto_connect: false, paused: false, relay: None,
                });
            } else if let Some(p) = peers.iter_mut().find(|p| &p.id == id) {
                p.paired = true;
            }
        }

        // 「断开」的暂停标记从地址簿读回来（F-16）：它和 `auto_connect` 是同一个意图的
        // 两面，重启后必须一起生效。少了这一半，本机是不主动拨了，却会照收对端记忆里
        // 的那次拨号 —— 用户看到的就是"点了断开、退出重开又自己连上了"。
        // 顺带把理由摆到设备行上（`linkState`），界面才说得出"为什么它不自动连"。
        let ws = WsManager::new();
        for p in peers.iter().filter(|p| p.paused) {
            ws.set_paused(&p.id, true);
            ws.set_link_state(
                &p.id,
                crate::ws::LinkState::Stopped { reason: crate::ws::StopReason::LocalDisconnected },
            );
        }

        // 会话记录（`chat.json`）放在 config.toml **旁边**，不写死全局数据目录：
        // 它和配置一样是"这个实例的用户数据"，实例换了个家（便携部署 / 测试指向临时目录），
        // 会话记录就该跟着走。
        let chat_path = match config_path.parent() {
            Some(dir) => dir.join("chat.json"),
            None => crate::config::data_dir().join("chat.json"),
        };
        // 群聊（`groups.json`）与它同一个家、同一个理由：都是一个实例自己的用户数据。
        let group_path = match config_path.parent() {
            Some(dir) => dir.join("groups.json"),
            None => crate::config::data_dir().join("groups.json"),
        };

        // 中转流量是"累计记录"：从落盘的总量接着算，而不是每次启动归零。
        let (relay_bytes0, relay_conns0) = (config.relay_bytes_total, config.relay_conns_total);

        Self {
            config_path,
            config: Mutex::new(config),
            peers: Mutex::new(peers),
            server_port: AtomicU16::new(0),
            server_shutdown: Mutex::new(None),
            server_task: Mutex::new(None),
            mdns_daemon: Mutex::new(None),
            ws,
            push: PushStore::new(),
            drag: crate::drag::DragStore::new(),
            pair: PairStore::new(),
            chat: crate::chat::ChatStore::with_path(chat_path),
            groups: crate::group::GroupStore::with_path(group_path),
            app_handle: Mutex::new(None),
            watchers: Mutex::new(std::collections::HashMap::new()),
            learned: Mutex::new(HashMap::new()),
            relays: crate::relay::RelayRegistry::default(),
            relay_bytes: AtomicU64::new(relay_bytes0),
            relay_conns: AtomicU64::new(relay_conns0),
            relay_capable: Mutex::new(HashSet::new()),
            relay_attempts: Mutex::new(HashMap::new()),
            seen: Mutex::new(HashMap::new()),
            peer_nets: Mutex::new(HashMap::new()),
            relay_pool: Mutex::new(HashMap::new()),
            direct_streak: Mutex::new(HashMap::new()),
            route_push: Mutex::new(HashMap::new()),
            relay_failed: Mutex::new(HashMap::new()),
            adopted: Mutex::new(HashMap::new()),
        }
    }

    // ------------------------------------------------------------------
    // Peer management helpers
    // ------------------------------------------------------------------

    /// Upsert a peer into the in-memory list. Persists the address book
    /// entry (config.peers) if the peer is paired or was manually added.
    pub fn upsert_peer(&self, peer: &PeerDevice) {
        let mut peers = self.peers.lock();
        if let Some(existing) = peers.iter_mut().find(|p| p.id == peer.id) {
            existing.name = peer.name.clone();
            if !peer.ip.is_empty() {
                existing.ip = peer.ip.clone();
            }
            if peer.port != 0 {
                existing.port = peer.port;
            }
            existing.online = existing.online || peer.online;
            existing.paired = existing.paired || peer.paired;
        } else {
            peers.push(peer.clone());
        }
        drop(peers);

        // Persist if in the address book.
        let mut config = self.config.lock();
        if config.peers.iter().any(|p| p.id == peer.id) {
            if let Some(p) = config.peers.iter_mut().find(|p| p.id == peer.id) {
                p.name = peer.name.clone();
                if !peer.ip.is_empty() {
                    p.ip = peer.ip.clone();
                }
                if peer.port != 0 {
                    p.port = peer.port;
                }
                p.paired = peer.paired;
            }
            let _ = crate::config::save_config_at(&self.config_path, &config);
        }
        self.emit_devices_changed();
    }

    /// Add a peer to the address book (manual add) and persist.
    ///
    /// 已经在列表里的设备**也要把地址补上**，不能因为"这一行已经在了"就整条丢掉：
    /// 凭据库里有 token、却没地址的设备会先被 `AppState::new` 合成一行（ip 空、端口 0），
    /// 于是"添加设备"填的地址会被静默忽略 —— 之后配对学到的地址也是空的，长连接
    /// 永远拨不出去（表现为设备行显示 `:0`）。
    pub fn add_peer_book(&self, peer: PeerDevice) {
        {
            let mut peers = self.peers.lock();
            match peers.iter_mut().find(|p| p.id == peer.id) {
                Some(p) => {
                    if !peer.name.is_empty() {
                        p.name = peer.name.clone();
                    }
                    if !peer.ip.is_empty() {
                        p.ip = peer.ip.clone();
                    }
                    if peer.port != 0 {
                        p.port = peer.port;
                    }
                    p.paired = p.paired || peer.paired;
                }
                None => peers.push(peer.clone()),
            }
        }
        {
            let mut config = self.config.lock();
            match config.peers.iter_mut().find(|p| p.id == peer.id) {
                Some(p) => {
                    if !peer.name.is_empty() {
                        p.name = peer.name.clone();
                    }
                    if !peer.ip.is_empty() {
                        p.ip = peer.ip.clone();
                    }
                    if peer.port != 0 {
                        p.port = peer.port;
                    }
                    p.paired = p.paired || peer.paired;
                }
                None => config.peers.push(peer),
            }
            let _ = crate::config::save_config_at(&self.config_path, &config);
        }
        self.emit_devices_changed();
    }

    /// 记下一台设备的地址 —— **只在它还没有地址时**（`upsert_peer` 的"只补空、绝不改写"
    /// 那一半，单独成一个入口）。
    ///
    /// 两个来路，都是**刚刚核实过**的：中转方拿请求方给的 hint 到那个地址上问出了身份、
    /// 中转控制面拿「TCP 来源地址 + 自报端口」问出了身份、或中转方在回话里报回"目标此刻在哪"。
    /// 但"核实"到这一步为止只是 `/pair/info` 上答出了那个 id —— 与 mDNS 记录是同一级的知识，
    /// 所以规矩也一样：**只补空行、不覆盖**（覆盖等于让一次中转请求把地址簿里某台设备的地址
    /// 搬到别处去），也**不声明在线**（可达性由 5s 探活自己判）。
    pub fn observe_verified_addr(&self, peer_id: &str, ip: &str, port: u16) {
        if peer_id.is_empty() || ip.is_empty() || port == 0 {
            return;
        }
        let changed = {
            let mut peers = self.peers.lock();
            match peers.iter_mut().find(|p| p.id == peer_id) {
                Some(p) => {
                    if p.ip.is_empty() || p.port == 0 {
                        p.ip = ip.to_string();
                        p.port = port;
                        true
                    } else {
                        false
                    }
                }
                None => {
                    // 本机还不认识它（记录/请求里第一次见到 id）：建一行，名字先拿 id 顶着 ——
                    // 之后 mDNS / 会话信封的 upsert 会把真名字写进来（`upsert_peer` 更新名字）；跨段
                    // 看不见它的那类（本机 mDNS 到不了）靠探活回话与担保人的转述填
                    // （`fill_peer_name` / `learn_peers`，判据是同一处 `name_is_placeholder`）。
                    peers.push(PeerDevice {
                        id: peer_id.to_string(),
                        name: peer_id.to_string(),
                        ip: ip.to_string(),
                        port,
                        online: false,
                        paired: false,
                        auto_connect: false,
                        paused: false,
                        relay: None,
                    });
                    true
                }
            }
        };
        if !changed {
            return;
        }
        // 地址簿里本来就有这一行（配对过 / 手动加的）、只是地址空着：补上就顺手落盘。
        {
            let mut config = self.config.lock();
            if let Some(p) = config.peers.iter_mut().find(|p| p.id == peer_id) {
                if p.ip.is_empty() || p.port == 0 {
                    p.ip = ip.to_string();
                    p.port = port;
                    let _ = crate::config::save_config_at(&self.config_path, &config);
                }
            }
        }
        self.emit_devices_changed();
    }

    /// 这一行还顶着占位名（见 [`name_is_placeholder`]）的时候，把刚核实到的真名填进去。
    ///
    /// 唯一写点为什么需要它：`observe_verified_addr` 建行时拿设备 id 顶着名字，而这类行
    /// **本机 mDNS 看不见**（跨段，正是它走中转的原因），会话信封与 mDNS 也就都救不了它
    /// —— 表现就是"设备列表里那台设备显示的是 deviceid"。机会其实一直在手里：这条行有
    /// 地址、每 5s 被探活敲一次，`/pair/info` 的回话里就带着它的真名（见
    /// [`probe_identity`]），只是此前没人把它写回设备行。
    ///
    /// **只填占位行**：真名一旦有了就一个字都不动 —— 那可能是用户手改过的名字，也可能是
    /// 对端换过名之前本机存下的，都不该被这里的观察值抹掉。
    pub fn fill_peer_name(&self, peer_id: &str, name: &str) {
        let name = name.trim();
        if name.is_empty() || name == peer_id {
            return;
        }
        let mut changed = false;
        {
            let mut peers = self.peers.lock();
            if let Some(p) = peers.iter_mut().find(|p| p.id == peer_id) {
                if name_is_placeholder(&p.name, &p.id) {
                    p.name = name.to_string();
                    changed = true;
                }
            }
        }
        if !changed {
            return;
        }
        // 地址簿里也有这一行（配对过 / 手动加的）就顺手更正它 —— 只占位名那一档，
        // 免得把用户改过的名字落盘冲成对端自报的。
        {
            let mut config = self.config.lock();
            if let Some(p) = config.peers.iter_mut().find(|p| p.id == peer_id) {
                if name_is_placeholder(&p.name, &p.id) {
                    p.name = name.to_string();
                    let _ = crate::config::save_config_at(&self.config_path, &config);
                }
            }
        }
        self.emit_devices_changed();
    }

    /// Mark a peer paired in both lists and persist the address book.
    pub fn mark_paired(&self, peer: &PeerDevice) {
        {
            let mut peers = self.peers.lock();
            if let Some(p) = peers.iter_mut().find(|p| p.id == peer.id) {
                p.paired = true;
                p.name = peer.name.clone();
                if !peer.ip.is_empty() {
                    p.ip = peer.ip.clone();
                }
                if peer.port != 0 {
                    p.port = peer.port;
                }
            } else {
                peers.push(peer.clone());
            }
        }
        {
            let mut config = self.config.lock();
            if let Some(p) = config.peers.iter_mut().find(|p| p.id == peer.id) {
                p.paired = true;
                p.name = peer.name.clone();
                if !peer.ip.is_empty() {
                    p.ip = peer.ip.clone();
                }
                if peer.port != 0 {
                    p.port = peer.port;
                }
            } else {
                config.peers.push(peer.clone());
            }
            let _ = crate::config::save_config_at(&self.config_path, &config);
        }
        // 配对只确立关系、不建链（F-16）：把上一次留下的拨号状态清掉。最要紧的是
        // **重新配对**：token 失效时这台设备停在 `Stopped{TokenRejected}`，不清就会一直
        // 显示「需配对」—— 用户刚换到新 token，界面却还在叫他重新配对。
        self.ws.clear_link_state(&peer.id);
        self.emit_devices_changed();
    }

    /// 记下这台设备的长连接意图（F-16）。
    ///
    /// 这是 `auto_connect` / `paused` 这一对字段的**唯一写点**（落盘 + 同步运行时的
    /// 暂停标记）。分开写两个字段迟早会写出"本机不拨、却照收对端的自动重拨"—— 那正是
    /// 「断开」被对端记忆悄悄撤销的样子。
    ///
    /// - `Wanted`：点过「连接」，或链路已经建起来（两侧都算，见 `ws::session`）——
    ///   写点在**链路建立那一刻**而不是退出时，所以进程被强杀、断电也不会漏掉这次长连接。
    /// - `Paused`：用户点过「断开」——本机不拨，也不接对端的自动重拨；重启照样算数
    ///   （`AppState::new` 把它读回内存，`server::events_ws_handler` 照旧回 403）。
    ///
    /// 只对地址簿（`config.peers`）里的设备落盘：纯发现条目没有可恢复的身份，重启后
    /// 本来也不会出现在列表里（也不会有链接可拒 —— WS 升级本来就要 token）。
    pub fn set_link_intent(&self, peer_id: &str, intent: LinkIntent) {
        let (auto_connect, paused) = intent.fields();
        self.ws.set_paused(peer_id, paused);
        {
            let mut peers = self.peers.lock();
            if let Some(p) = peers.iter_mut().find(|p| p.id == peer_id) {
                p.auto_connect = auto_connect;
                p.paused = paused;
            }
        }
        let mut config = self.config.lock();
        let Some(p) = config.peers.iter_mut().find(|p| p.id == peer_id) else {
            return;
        };
        if p.auto_connect == auto_connect && p.paused == paused {
            return;
        }
        p.auto_connect = auto_connect;
        p.paused = paused;
        let _ = crate::config::save_config_at(&self.config_path, &config);
    }

    /// 本机是否「在线」（面板右上角状态框）。离线 = 隐身，见 `Config::online`。
    ///
    /// 别和 `set_peer_online` 搞混：那个是**对端**是否在网上，这个是**本机自己**是否
    /// 对局域网可见。
    pub fn is_online(&self) -> bool {
        self.config.lock().online
    }

    /// 本机实际在听的 HTTP 端口：被占用时服务器从配置端口往上试到 +10，所以两者可能不同
    /// （mDNS 记录里播的、`/pair/info` 报的、判"来源地址是不是我自己"用的都是这一个）。
    pub fn listening_port(&self) -> u16 {
        match self.server_port.load(std::sync::atomic::Ordering::SeqCst) {
            0 => self.config.lock().port,
            p => p,
        }
    }

    /// 记住本机「在线 / 离线」并落盘（用户意图，跨重启有效）。
    ///
    /// 只写配置：运行时的连带动作（注销 mDNS、断链、拒收入站请求）在 `lib.rs::set_online`，
    /// 那里才有 daemon 与运行时。
    pub fn set_local_online(&self, on: bool) {
        let mut config = self.config.lock();
        if config.online == on {
            return;
        }
        config.online = on;
        let _ = crate::config::save_config_at(&self.config_path, &config);
    }

    /// Set online state. Emits only on a real change.
    ///
    /// 对端**刚上线**这件事还带两层副作用：①它离线期间排队等它的会话消息（`Delivery::Queued`）
    /// 该发出去了；②本机在共同群里缺的那些（跨网段漏掉的、离线期间别人发的）该去补拉了。
    /// 探活 / mDNS / 链路三条路都汇到这里，所以这两个触发点都放在这一个地方
    /// （`chat::flush_queued` 与 `group::sync_with_peer` 都自带闸门，重复叫是安全的）。
    pub fn set_peer_online(self: &Arc<Self>, peer_id: &str, online: bool) {
        let changed;
        {
            let mut peers = self.peers.lock();
            let existing = peers.iter_mut().find(|p| p.id == peer_id);
            changed = match existing {
                Some(p) if p.online != online => {
                    p.online = online;
                    true
                }
                Some(_) => false,
                None => false,
            };
        }
        if changed {
            let event = if online { "peer://online" } else { "peer://offline" };
            if let Some(app) = self.app_handle.lock().as_ref() {
                let _ = app.emit(event, serde_json::json!({ "peerId": peer_id }));
            }
            self.emit_devices_changed();
            if online {
                crate::chat::flush_queued(self, peer_id);
                // 共同群里缺的那些：这台设备是"本机连得上的人"，问它一遍。
                crate::group::sync_with_peer(self, peer_id);
            }
        }
    }

    // ------------------------------------------------------------------
    // 中转（F-106）
    // ------------------------------------------------------------------

    /// 记下"这台设备此刻经谁中转"（F-106）。
    ///
    /// 只动**内存行上的运行时字段**（`PeerDevice::relay`，`#[serde(skip)]`）：地址簿里的
    /// `ip`/`port` 仍是它自己的地址 —— 那是"它长什么样"，而中转端点是"本机怎么走到它"。
    /// 混在一起写的话，mDNS 下一次 `upsert` 就会把中转地址当成它的地址又播出去。
    pub fn set_relay(self: &Arc<Self>, peer_id: &str, relay: crate::config::RelayAddr) {
        {
            let mut peers = self.peers.lock();
            let Some(peer) = peers.iter_mut().find(|p| p.id == peer_id) else {
                return;
            };
            peer.relay = Some(relay);
        }
        // 指纹表要跟着重建：中转端点是**新地址键位**，不钉它就只剩 TOFU（见 `refresh_trust`）。
        refresh_trust(self);
        self.emit_devices_changed();
    }

    /// 摘掉这台设备上的中转、回到直连。返回"之前确实挂着一跳"。
    ///
    /// F-107 §8.1：顺手把**刚失败的那台中转设备**记下来（[`AppState::failed_relay`]）——
    /// 走到这里通常就是"中转这一跳也不通了"，下一轮选路要把它排到最后，别又白等一次。
    pub fn clear_relay(self: &Arc<Self>, peer_id: &str) -> bool {
        let failed = {
            let mut peers = self.peers.lock();
            match peers.iter_mut().find(|p| p.id == peer_id) {
                Some(peer) if peer.relay.is_some() => {
                    let via = peer.relay.as_ref().map(|r| r.via.clone()).unwrap_or_default();
                    peer.relay = None;
                    Some(via)
                }
                _ => None,
            }
        };
        let Some(via) = failed else {
            return false;
        };
        if !via.is_empty() {
            self.relay_failed
                .lock()
                .insert(peer_id.to_string(), (via, Instant::now()));
        }
        refresh_trust(self);
        self.emit_devices_changed();
        true
    }

    /// 某一台目标刚刚失败过的中转方（[`FAILED_RELAY_COOLDOWN`] 之内才算）——选路时排到最后。
    pub fn failed_relay(&self, target: &str) -> Option<String> {
        self.relay_failed.lock().get(target).and_then(|(via, at)| {
            (at.elapsed() < FAILED_RELAY_COOLDOWN).then(|| via.clone())
        })
    }

    /// 对端自报的中转能力（`/pair/info` / `/discover` 里的 `relay` 位）。
    pub fn set_relay_capable(&self, peer_id: &str, capable: bool) {
        let mut set = self.relay_capable.lock();
        if capable {
            set.insert(peer_id.to_string());
        } else {
            set.remove(peer_id);
        }
    }

    /// 这台设备此刻愿不愿意替别人中转（挑中转方的依据之一）。
    pub fn is_relay_capable(&self, peer_id: &str) -> bool {
        self.relay_capable.lock().contains(peer_id)
    }

    // ------------------------------------------------------------------
    // 中转记忆（F-107）：网段自报、中转池、链路记忆、退休
    // ------------------------------------------------------------------

    /// 记下对端**自报**的网段（F-107 §5）。空列表不动已有值 —— `nets` 是"活回话优先"的
    /// 一份知识，别让一次缺字段的 TXT 把它抹成空。
    pub fn note_peer_nets(&self, peer_id: &str, nets: &[String]) {
        if peer_id.is_empty() || nets.is_empty() {
            return;
        }
        let mut table = self.peer_nets.lock();
        let slot = table.entry(peer_id.to_string()).or_default();
        if slot.as_slice() != nets {
            *slot = nets.to_vec();
        }
    }

    /// 对端自报的网段（`nets` 的字符串原样）。
    pub fn peer_nets_of(&self, peer_id: &str) -> Vec<String> {
        self.peer_nets.lock().get(peer_id).cloned().unwrap_or_default()
    }

    /// 这台设备属于哪一段（F-107 §5）：优先**它对端自报**的、能覆盖它地址的那一段；
    /// 没有自报（老版本、或还没换到视野）时退回"它落在本机哪块网卡的段里"（同段场景，
    /// 那种情形两台设备本来就同 L2，本机掩码是对的）。都不知道 → `None`（只记设备级）。
    pub fn cidr_of(&self, peer_id: &str, ip: &str) -> Option<String> {
        let parsed: std::net::Ipv4Addr = ip.trim().parse().ok()?;
        let reported = self.peer_nets_of(peer_id);
        if let Some(c) = reported
            .iter()
            .filter_map(|s| crate::subnet::parse(s))
            .find(|c| c.contains(parsed))
        {
            return Some(c.to_cidr_string());
        }
        crate::subnet::local_net_of(parsed).map(|c| c.to_cidr_string())
    }

    /// 吸收一次 `/discover` 活视野（F-107 §7 / §8.1）：按**应答方自己的网段**重建中转池。
    ///
    /// 池的构造（文档 §7）：`{ 锚点自己（若它允许中转） } ∪ { 表里 cidr == viaCidr 且 relay 且
    /// 在线者 }`。只信活视野 —— 每次回话整份替换，所以 D 上线/关掉中转/换地址都会跟着变。
    pub fn absorb_view(
        &self,
        _voucher_id: &str,
        voucher_nets: &[String],
        voucher_relay: bool,
        voucher: &DiscoveredPeer,
        entries: &[DiscoveredPeer],
    ) {
        let voucher_cidrs: Vec<crate::subnet::Cidr> =
            voucher_nets.iter().filter_map(|s| crate::subnet::parse(s)).collect();
        if voucher_cidrs.is_empty() {
            return; // 应答方没自报网段：这一段分不出来，别拿本机掩码猜远端
        }
        let paired_ids: std::collections::HashSet<String> = self
            .peers
            .lock()
            .iter()
            .filter(|p| p.paired)
            .map(|p| p.id.clone())
            .collect();
        // ⚠️ 池的键是**目标段**，不是应答方所在的段（这一行踩过坑）：选路要回答的是
        // "去这一段该借谁"，而"这一段的中转方在哪一段"是**策略**（`relayRoutes`）说了算。
        // 所以先把"应答方网段 → 它的目标段们"这张表算出来（**要在取 pool 之前读 config**，
        // 加锁顺序统一为 config → peers → pool），再进池循环。
        let targets_for: HashMap<String, Vec<String>> = {
            let config = self.config.lock();
            voucher_cidrs
                .iter()
                .map(|cidr| {
                    let targets: Vec<String> = config
                        .relay_routes
                        .iter()
                        .filter(|r| {
                            crate::subnet::parse(&r.via_cidr)
                                .map(|vc| vc.same_as(cidr))
                                .unwrap_or(false)
                        })
                        .map(|r| r.target_cidr.clone())
                        .collect();
                    (cidr.to_cidr_string(), targets)
                })
                .collect()
        };
        let mut pool = self.relay_pool.lock();
        // 池里那些设备的地址要跟着这份活视野走（见 `RelayRoute::via_addrs`）：设备表里的行
        // 会随锚点离散被收走，而"拨得动池里那台"这件事必须活到策略退休为止。
        let mut addr_updates: Vec<(String, Vec<crate::relay::RouteDevice>)> = Vec::new();
        for cidr in voucher_cidrs {
            let mut devices: Vec<PoolEntry> = Vec::new();
            // 这一份里**在场、而且是它亲口说不干这活**的（回话头里锚点自己关着中转开关）：
            // 第一手证据，池里那一台直接删掉。缺席（下面 `merge_pool` 里留着的那种）不算证据
            // —— 它可能只是离线一会儿；表里那台的 `relay:false` 也是二手知识，同样不算。
            let mut gone: HashSet<String> = HashSet::new();
            // 锚点自己进池：`/discover` 的表会过滤掉应答方自己（`p.id != own_id`），
            // 所以它那一条只能从回话头里拿。只进**它自己地址所在的那一段** ——
            // 一台多网卡的锚点不该出现在它并不在的那些段里。
            let voucher_here = voucher
                .ip
                .parse::<std::net::Ipv4Addr>()
                .map(|vip| cidr.contains(vip))
                .unwrap_or(false);
            if voucher_here {
                if voucher_relay {
                    devices.push(PoolEntry {
                        id: voucher.id.clone(),
                        name: voucher.name.clone(),
                        ip: voucher.ip.clone(),
                        port: voucher.port,
                        paired: paired_ids.contains(&voucher.id),
                        seen_at: None,
                    });
                } else {
                    gone.insert(voucher.id.clone());
                }
            }
            for e in entries {
                let Some(ec) = e.cidr.as_deref().and_then(crate::subnet::parse) else {
                    continue; // 报出方没说它属于哪一段：宁可不用，也别猜
                };
                if !ec.same_as(&cidr) {
                    continue;
                }
                if !e.relay {
                    // 表里那台"关着开关"是**二手**知识（锚点自己对它的能力位可能还没刷新到，
                    // 见 §19 根因 2）：不算删除证据，只是这一份里没把它列进候选。
                    continue;
                }
                if devices.iter().any(|d| d.id == e.id) {
                    continue;
                }
                devices.push(PoolEntry {
                    id: e.id.clone(),
                    name: e.name.clone(),
                    ip: e.ip.clone(),
                    port: e.port,
                    paired: paired_ids.contains(&e.id),
                    seen_at: None,
                });
            }
            // 应答方这一段里"允许中转"的设备，记到**所有"经它中转"的目标段**上。
            // 没有对应策略时（还没成功过一次）不建池 —— 那时由"广搜"负责第一跳。
            //
            // **按"并"更新**（`merge_pool`）：这一份里没有的**不删**（它可能只是离线一会儿，
            // 而它恰好是锚点离线之后要接手的那一台），只有"它亲口说不干"（`gone`）或超出一段
            // 的条数上限时才走。
            for target in targets_for.get(&cidr.to_cidr_string()).cloned().unwrap_or_default() {
                let merged = merge_pool(&mut pool, &target, devices.clone(), &gone);
                addr_updates.push((
                    target,
                    merged.iter().map(PoolEntry::to_route_device).collect(),
                ));
            }
        }
        drop(pool);
        if !addr_updates.is_empty() {
            // 只改内存：这一拍每 5s 就来一次，写盘会把配置文件刷爆。落盘发生在两处本来就
            // 会写盘的时刻 —— 经中转成功那一刻（`note_relay_success`）与收到反向记录那一刻
            // （`apply_relay_routes`）；那两份快照就是"冷启动的最后已知池"。
            let mut config = self.config.lock();
            for (target, addrs) in addr_updates {
                config.relay_route_mut(&target).via_addrs = addrs;
            }
        }
    }

    /// 记下"这台设备在这一段里"——**只在还没有它的自报时**用（转述来的段不该盖掉它自己说的）。
    ///
    /// 为什么要它：反向记录/中转方报池带进来的是**别人的地址**，而"这台设备属于哪一段"原先只能
    /// 靠它自己自报（`/pair/info` 的 `nets`）—— 而 `adopted` 那一档**不进探活队列**，永远不会
    /// 自报。于是 `cidr_of()` 返回 `None`，`note_relay_success` 就只写设备级、写不出网段级策略
    /// （真机上就是这个表现：日志里 `via_cidr=` 是空的），"同段其他设备复用"与"把这条路由
    /// 推给对端"两件事都跟着落空。
    pub fn note_relay_net(&self, peer_id: &str, cidr: &str) {
        if peer_id.is_empty() || crate::subnet::parse(cidr).is_none() {
            return;
        }
        let mut table = self.peer_nets.lock();
        table
            .entry(peer_id.to_string())
            .or_insert_with(|| vec![cidr.to_string()]);
    }

    /// 某个**目标段**此刻可用的中转池（活视野）。
    ///
    /// 键是**目标段**（"去这一段该借谁"），不是中转方所在的段 —— 后者是策略
    /// （`relayRoutes[目标段].via_cidr`）里的信息。
    pub fn pool_for(&self, target_cidr: &str) -> Vec<PoolEntry> {
        self.relay_pool
            .lock()
            .get(target_cidr)
            .cloned()
            .unwrap_or_default()
    }

    /// **本机这一段**的那些策略段（`target_cidr` 满足 [`subnet::own_ip_in`]，与
    /// `apply_relay_routes` 收记录那一关同一套判据）。
    ///
    /// 给"目标地址未知"的选路用（只经中转见过一面的那类对端）：池的键是**目标段**，而本机
    /// 手里"含本机地址"的那几条策略说的正是"要够着一台与本机同段的设备该借谁"。
    pub fn own_segment_route_cidrs(&self) -> Vec<String> {
        let mut out: Vec<String> = self
            .config
            .lock()
            .relay_routes
            .iter()
            .filter(|r| {
                crate::subnet::parse(&r.target_cidr).is_some_and(|c| crate::subnet::own_ip_in(&c))
            })
            .map(|r| r.target_cidr.clone())
            .collect();
        out.sort();
        out.dedup();
        out
    }

    /// 给界面看的池快照：`[{cidr, devices:[…]}]`。
    pub fn pool_json(&self) -> serde_json::Value {
        let pool = self.relay_pool.lock();
        let mut keys: Vec<&String> = pool.keys().collect();
        keys.sort();
        serde_json::Value::Array(
            keys.into_iter()
                .map(|k| {
                    serde_json::json!({
                        "cidr": k,
                        "devices": pool.get(k).cloned().unwrap_or_default(),
                    })
                })
                .collect(),
        )
    }

    /// **经中转真的够着了一台设备**（F-107 §7 的唯一写入点）：写设备级链路，并把它提升成
    /// 网段级策略（一次成功即写 —— 策略只是"直连不通时的候选顺序"，不是绕行的许可）。
    ///
    /// 必须在"对中转端点那一跳探到目标"之后调用，不能只看控制面 200：那只能证明"中转方
    /// 愿意开"，不证明"目标真的在"。
    pub fn note_relay_success(self: &Arc<Self>, target_id: &str, via_id: &str) {
        if !self.config.lock().relay_memory {
            return; // S-10 关着：完全退回 F-106，什么都不记
        }
        // 先取快照（避免"config → peers/pool"与别处的加锁顺序打架）。
        let (via_ip, target_ip) = {
            let peers = self.peers.lock();
            let get = |id: &str| peers.iter().find(|p| p.id == id).map(|p| p.ip.clone()).unwrap_or_default();
            (get(via_id), get(target_id))
        };
        let via_cidr = self.cidr_of(via_id, &via_ip).unwrap_or_default();
        let target_cidr = self.cidr_of(target_id, &target_ip);
        // 池按**目标段**取（键就是目标段）；再把这次真正成功的那一台并进来 —— 池的刷新
        // （`/discover` 或中转方的自报）可能还没轮到这条策略，少了它这一条就只剩空池。
        let mut pool = match &target_cidr {
            Some(t) => self.pool_for(t),
            None => Vec::new(),
        };
        if let Some(entry) = pool.iter_mut().find(|d| d.id == via_id) {
            // 这一台刚刚真的把我们送到了目标：刷新它的"露面时间" —— 它是在的，
            // 不该因为最近几份快照里没露面就被慢慢淘汰掉。
            entry.seen_at = Some(Instant::now());
        } else {
            let (via_name, via_port) = {
                let peers = self.peers.lock();
                peers
                    .iter()
                    .find(|p| p.id == via_id)
                    .map(|p| (p.name.clone(), p.port))
                    .unwrap_or_default()
            };
            pool.insert(
                0,
                PoolEntry {
                    id: via_id.to_string(),
                    name: via_name,
                    ip: via_ip.clone(),
                    port: via_port,
                    paired: false,
                    seen_at: Some(Instant::now()),
                },
            );
        }
        let at = chrono::Utc::now().to_rfc3339();

        let mut config = self.config.lock();
        config.set_relay_link(target_id, via_id, &via_cidr, &at);
        if let (Some(target_cidr), false) = (target_cidr.clone(), via_cidr.is_empty()) {
            let route = config.relay_route_mut(&target_cidr);
            route.via_cidr = via_cidr.clone();
            route.via_devices = pool.iter().map(|d| d.id.clone()).collect();
            route.via_addrs = pool.iter().map(PoolEntry::to_route_device).collect();
            route.via_paired = pool.iter().filter(|d| d.paired).map(|d| d.id.clone()).collect();
            route.last_ok_at = Some(at.clone());
            route.ok_count = route.ok_count.saturating_add(1);
            route.seen_at = Some(at.clone());
        }
        let saved = crate::config::save_config_at(&self.config_path, &config);
        drop(config);
        if let Err(e) = saved {
            warn!("中转记忆落盘失败: {e}");
        }
        info!(
            target = %target_id,
            via = %via_id,
            via_cidr = %via_cidr,
            target_cidr = %target_cidr.unwrap_or_default(),
            "中转记忆：经中转够着了目标"
        );
        self.emit_devices_changed();
    }

    /// 直连这一拍的结果（F-107 §9.2）。连续 [`DIRECT_STABLE_ROUNDS`] 拍可达 → 清设备级记忆。
    pub fn note_direct_probe(self: &Arc<Self>, peer_id: &str, ok: bool) {
        let streak = {
            let mut table = self.direct_streak.lock();
            if ok {
                let n = table.entry(peer_id.to_string()).or_insert(0);
                *n = n.saturating_add(1);
                *n
            } else {
                table.insert(peer_id.to_string(), 0);
                0
            }
        };
        if streak != DIRECT_STABLE_ROUNDS {
            return;
        }
        // 直连恢复且稳了：设备级那份"上次经谁"的提示退休。
        let mut config = self.config.lock();
        if config.clear_relay_link(peer_id) {
            let peer_name = self
                .peers
                .lock()
                .iter()
                .find(|p| p.id == peer_id)
                .map(|p| p.name.clone())
                .unwrap_or_else(|| peer_id.to_string());
            info!(peer_id = %peer_id, name = %peer_name, "直连恢复且稳定，清掉设备级中转记忆");
            if let Err(e) = crate::config::save_config_at(&self.config_path, &config) {
                warn!("清中转记忆落盘失败: {e}");
            }
            drop(config);
            self.emit_devices_changed();
        }
    }

    /// 反向路由记录的发送闸门（F-107 §10.3）：内容变了、或上次没被确认且已过重发间隔 → 该发。
    ///
    /// 已确认且内容没变的，按 [`ROUTE_REFRESH_MIN`] 再刷一次：对端**重启**之后手里就没有
    /// 这条记忆了，而本机这边的哈希没变（不重发它就再也拿不到）。
    pub fn route_push_gate(&self, peer_id: &str, hash: &str) -> bool {
        let now = Instant::now();
        let mut table = self.route_push.lock();
        match table.get(peer_id) {
            Some(s) if s.hash == hash && s.acked => {
                if now.duration_since(s.sent_at) < ROUTE_REFRESH_MIN {
                    return false;
                }
                table.insert(
                    peer_id.to_string(),
                    RoutePushState { hash: hash.to_string(), sent_at: now, acked: true },
                );
                true
            }
            Some(s) if s.hash == hash && now.duration_since(s.sent_at) < ROUTE_RESEND_MIN => false,
            _ => {
                table.insert(
                    peer_id.to_string(),
                    RoutePushState { hash: hash.to_string(), sent_at: now, acked: false },
                );
                true
            }
        }
    }

    /// 反向路由记录的发送结果（对端采纳了才算 `acked`）。
    pub fn route_push_result(&self, peer_id: &str, acked: bool) {
        if let Some(s) = self.route_push.lock().get_mut(peer_id) {
            s.acked = acked;
        }
    }

    /// 收下对端推来的反向路由记录（F-107 §10.3）。返回 `(采纳的目标段, 丢弃的目标段)`。
    ///
    /// 三道校验，缺一不可：
    /// ① **目标段必须含本机地址** —— 一条与本机无关的路由没有意义（也是防投毒的第一关：
    ///    乱报的段先进不来）；
    /// ② 网段字符串与中转设备地址必须合法，且**条数有上限**（[`MAX_ADOPTED_DEVICES`]）——
    ///    一次 64MB 的请求体就能塞进上万行，不设限就是让人随手灌爆设备表与配置；
    /// ③ 池里至少有一台可拨的设备。
    ///
    /// 通过就落库，并把池里**本机还不认识**的设备并进设备列表（内存行）：接收方对那一段没有
    /// mDNS 视野，不先认识这些地址就拨不出去。
    ///
    /// ⚠️ **地址簿里已有的行一个字都不动**：`upsert_peer` 对已存在的行是覆盖语义（名字会被
    /// 这次传进去的空串抹掉、地址会被对端的观察值改写、`paired` 会被写成 `false` 并落盘）。
    /// 正常双向流程就会踩到 —— A 把「AB 段经 C 中转」推给 B，B 再反向推回时，那条记录里的
    /// 中转设备名单里就含着 C。所以这里只并**新 id**（见 `is_known_peer`）。
    ///
    /// 并进来的行归 [`AppState::adopted`] 那一档：有 TTL、**不进探活队列**（本机跟它们之间
    /// 没有关系，没必要每 5s 敲一遍；要借中转时按需拨，那一次就验证了可达性）。
    ///
    /// **记录只是提示**：用它的时候仍然"先探直连 → 再按池顺序试着借 → 目标身份由中转方核实"。
    /// 记录错了最多白试一次。
    pub fn apply_relay_routes(
        self: &Arc<Self>,
        from: &str,
        records: &[crate::relay::RouteRecord],
    ) -> (Vec<String>, Vec<String>) {
        if !self.config.lock().relay_memory {
            return (
                Vec::new(),
                records.iter().map(|r| r.target_cidr.clone()).collect(),
            );
        }
        let paired: HashSet<String> = self
            .peers
            .lock()
            .iter()
            .filter(|p| p.paired)
            .map(|p| p.id.clone())
            .collect();
        // 「本机自己的行」由 `adopt_devices` 自己滤（它要在取 config 之前读地址簿）——
        // 这里不再维护第二份。
        let at = chrono::Utc::now().to_rfc3339();
        let mut accepted = Vec::new();
        let mut ignored = Vec::new();
        // 采纳的那些设备（出了 config 作用域再并进设备表 + 记它们的段）。
        // 二元组是 (记录说的中转段, 设备)：第一个用于 `note_relay_net`。
        let mut seed: Vec<(String, Vec<crate::relay::RouteDevice>)> = Vec::new();
        {
            let mut config = self.config.lock();
            for r in records {
                let Some(target) = crate::subnet::parse(&r.target_cidr) else {
                    ignored.push(r.target_cidr.clone());
                    continue;
                };
                if !crate::subnet::own_ip_in(&target) || crate::subnet::parse(&r.via_cidr).is_none() {
                    ignored.push(r.target_cidr.clone());
                    continue;
                }
                let devices: Vec<crate::relay::RouteDevice> = r
                    .via_devices
                    .iter()
                    .filter(|d| !d.id.is_empty() && !d.ip.is_empty() && d.port != 0)
                    .take(crate::relay::MAX_ROUTE_DEVICES)
                    .cloned()
                    .collect();
                if devices.is_empty() {
                    ignored.push(r.target_cidr.clone());
                    continue;
                }
                // 池**按"并"更新**（`merge_pool`）：记录只是提示，不该把本机已知的那几台
                // 挤出去（对端那份记录里没有它，可能只是对端那边它刚好离线）。
                let fresh: Vec<PoolEntry> = devices
                    .iter()
                    .map(|d| PoolEntry {
                        id: d.id.clone(),
                        name: d.name.clone(),
                        ip: d.ip.clone(),
                        port: d.port,
                        paired: paired.contains(&d.id),
                        seen_at: None,
                    })
                    .collect();
                let merged = {
                    // 加锁顺序统一为 config → pool。
                    let mut pool = self.relay_pool.lock();
                    merge_pool(&mut pool, &r.target_cidr, fresh, &HashSet::new())
                };
                let route = config.relay_route_mut(&r.target_cidr);
                route.via_cidr = r.via_cidr.clone();
                route.via_devices = merged.iter().map(|d| d.id.clone()).collect();
                route.via_addrs = merged.iter().map(PoolEntry::to_route_device).collect();
                route.via_paired = merged
                    .iter()
                    .filter(|d| d.paired)
                    .map(|d| d.id.clone())
                    .collect();
                route.seen_at = Some(at.clone());
                accepted.push(r.target_cidr.clone());
                seed.push((r.via_cidr.clone(), devices));
            }
            if !accepted.is_empty() {
                if let Err(e) = crate::config::save_config_at(&self.config_path, &config) {
                    warn!("反向中转路由落盘失败: {e}");
                }
            }
        }
        if !accepted.is_empty() {
            for (via_cidr, devices) in &seed {
                // 池在采纳那一趟就按"并"更新过了（`merge_pool` 也把活池写好了）—— 这里只做
                // "并进设备表 + 记它们的段"这两件事。
                //
                // 记录里那些设备是**别人报来的地址**：它们不会自报网段（adopted 不进探活队列），
                // 记录说的这一段就是它们唯一的归属信息（见 `note_relay_net`）。
                for d in devices {
                    self.note_relay_net(&d.id, via_cidr);
                }
                self.adopt_devices(devices, &[]);
            }
            info!(
                from = %from,
                accepted = accepted.len(),
                ignored = ignored.len(),
                "收到反向中转路由记录"
            );
            // 记录一到，"出路"就刚刚出现了：**当场**给还欠着消息的那几台叫一次补发。
            // 只有一条借来的路、直连地址从没学到过的那类对端没有别的触发器能告诉本机
            // "它上线了"；少了这一句，排队的消息要等用户再发一条才会动（`flush_queued`
            // 里会先试着借一条中转 —— 闸门与限流都在，重复叫是安全的）。
            crate::chat::flush_all_queued(self);
            self.emit_devices_changed();
        }
        (accepted, ignored)
    }

    /// 这一行是不是"反向记录带进来的"（有 TTL、不参与探活）。
    pub fn is_adopted(&self, peer_id: &str) -> bool {
        self.adopted.lock().contains_key(peer_id)
    }

    /// 把"本机还不认识"的设备并进设备列表（反向记录 / 中转方报来的池都走这一条路）。
    ///
    /// 归 [`AppState::adopted`] 那一档：有 TTL、不进探活队列。**已有的行一个字都不动** ——
    /// `upsert_peer` 是覆盖语义（空名字会抹掉对方的名字、地址会被改写、`paired` 会被写成
    /// `false` 并落盘），而这正是"反向推回来的记录里含着锚点自己"那条路会踩到的坑。
    ///
    /// 返回真正并进来的台数。
    pub fn adopt_devices(&self, devices: &[crate::relay::RouteDevice], exclude: &[&str]) -> usize {
        let known: HashSet<String> = {
            let peers: HashSet<String> = self.peers.lock().iter().map(|p| p.id.clone()).collect();
            let booked: HashSet<String> =
                self.config.lock().peers.iter().map(|p| p.id.clone()).collect();
            peers.union(&booked).cloned().collect()
        };
        let fresh: Vec<&crate::relay::RouteDevice> = devices
            .iter()
            .filter(|d| {
                !d.id.is_empty()
                    && !d.ip.is_empty()
                    && d.port != 0
                    && !known.contains(&d.id)
                    && !exclude.contains(&d.id.as_str())
            })
            .take(MAX_ADOPTED_DEVICES)
            .collect();
        if fresh.is_empty() {
            return 0;
        }
        let now = Instant::now();
        // 先记时间戳、**放开这把守卫**再去 upsert（加锁顺序统一成 `adopted → peers`）。
        {
            let mut adopted = self.adopted.lock();
            for d in &fresh {
                adopted.insert(d.id.clone(), now);
            }
        }
        for d in &fresh {
            self.upsert_peer(&PeerDevice {
                id: d.id.clone(),
                name: d.name.clone(),
                ip: d.ip.clone(),
                port: d.port,
                online: false,
                paired: false,
                ..Default::default()
            });
        }
        fresh.len()
    }

    /// 吸收**中转方在 `/relay/open` 回话里自报的那一段池**（F-107 §8.1 的第二个来源）。
    ///
    /// 为什么要它：池原先只从**已配对**对端的 `/discover` 视野来，于是"没配对就借中转"那条路
    /// （S-09 放宽之后的正常用法）虽然能借到**当前这一台**，却永远不知道那一段里**还有谁** ——
    /// C 掉线就再也借不到 D。中转方是我**主动挑的**、而且它刚刚真的把目标连上了，让它报一句
    /// "我这一段里愿意中转的还有谁"，就把那一格补上了。
    ///
    /// 信任等级与反向记录相同：对方说谎只会让我白试一次（借的时候目标身份仍由中转方核实），
    /// 收口也相同 —— 不认识的 id 归 `adopted`（TTL + 不探活）、池按**目标段**记。
    ///
    /// 返回并进来的设备台数。
    pub fn absorb_relay_report(
        self: &Arc<Self>,
        target_id: &str,
        via_cidr: &str,
        devices: &[crate::relay::RouteDevice],
    ) -> usize {
        if !self.config.lock().relay_memory {
            return 0; // S-10 关着：不记、也不借池
        }
        let Some(via) = crate::subnet::parse(via_cidr) else {
            return 0;
        };
        let target_ip = self
            .peers
            .lock()
            .iter()
            .find(|p| p.id == target_id)
            .map(|p| p.ip.clone())
            .unwrap_or_default();
        let Some(target_cidr) = self.cidr_of(target_id, &target_ip) else {
            return 0; // 目标的段都算不出来：这条池没有落点
        };
        let adopted = self.adopt_devices(devices, &[target_id]);
        // 这些设备是**别人报来的地址**，它们不会自报网段（adopted 不进探活队列）——
        // 由报出方说的这一段就是它们唯一的归属信息（见 `note_relay_net`）。
        for d in devices {
            self.note_relay_net(&d.id, via_cidr);
        }
        let paired: HashSet<String> = self
            .peers
            .lock()
            .iter()
            .filter(|p| p.paired)
            .map(|p| p.id.clone())
            .collect();
        let fresh: Vec<PoolEntry> = devices
            .iter()
            .filter(|d| {
                d.ip.parse::<std::net::Ipv4Addr>()
                    .map(|ip| via.contains(ip))
                    .unwrap_or(false)
            })
            .map(|d| PoolEntry {
                id: d.id.clone(),
                name: d.name.clone(),
                ip: d.ip.clone(),
                port: d.port,
                paired: paired.contains(&d.id),
                seen_at: None,
            })
            .collect();
        if fresh.is_empty() {
            return adopted;
        }
        // **按"并"更新**：中转方这一份自报里没有的，不代表那台没了（它可能只是离线一会儿
        // —— 而 C 离线、D 在线正是这条路要服务的场合，见 `merge_pool`）。
        let merged = merge_pool(
            &mut self.relay_pool.lock(),
            &target_cidr,
            fresh,
            &HashSet::new(),
        );
        {
            // 中转方报来的这一池，地址也要跟着记（锚点离散之后仍要拨得动它 —— 这正是这份
            // 自报的用途，见 `RelayRoute::via_addrs`）。先取 config 再取池（加锁顺序）。
            let mut config = self.config.lock();
            config.relay_route_mut(&target_cidr).via_addrs =
                merged.iter().map(PoolEntry::to_route_device).collect();
        }
        info!(
            target = %target_id,
            target_cidr = %target_cidr,
            via_cidr = %via_cidr,
            devices = devices.len(),
            adopted,
            "中转方报来它那一段的中转池"
        );
        adopted
    }

    /// 本机在 `cidr` 这一段里**此刻能替别人中转**的设备（含本机自己，若本机也在这一段且开着
    /// S-09）。给中转方在 `/relay/open` 的回话里自报用（见 [`AppState::absorb_relay_report`]）。
    ///
    /// 只算**本机真正认识**的那些（mDNS / 手动添加 / 探活过）：`learned`（担保人转述的）与
    /// `adopted`（反向记录带进来的）不算 —— 本机没跟它们建立过关系，不该把它们报成"这一段的
    /// 中转方"给请求方去拨。
    pub fn relay_devices_in(
        &self,
        cidr: &crate::subnet::Cidr,
        exclude: &[&str],
    ) -> Vec<crate::relay::RouteDevice> {
        let (own_id, own_name, allow) = {
            let config = self.config.lock();
            (
                config.device_id.clone(),
                config.device_name.clone(),
                config.allow_relay,
            )
        };
        let learned: HashSet<String> = self.learned.lock().keys().cloned().collect();
        let adopted: HashSet<String> = self.adopted.lock().keys().cloned().collect();
        let mut out: Vec<crate::relay::RouteDevice> = Vec::new();
        if allow {
            if let Some(ip) = crate::server::local_ipv4_addresses()
                .into_iter()
                .find(|ip| ip.parse::<std::net::Ipv4Addr>().map(|a| cidr.contains(a)).unwrap_or(false))
            {
                out.push(crate::relay::RouteDevice {
                    id: own_id.clone(),
                    name: own_name.clone(),
                    ip,
                    port: self.listening_port(),
                });
            }
        }
        for p in self.peers.lock().iter() {
            if p.id == own_id
                || exclude.contains(&p.id.as_str())
                || learned.contains(&p.id)
                || adopted.contains(&p.id)
                || !p.online
                || p.ip.is_empty()
                || p.port == 0
                || !self.is_relay_capable(&p.id)
            {
                continue;
            }
            let Ok(ip) = p.ip.parse::<std::net::Ipv4Addr>() else {
                continue;
            };
            if !cidr.contains(ip) {
                continue;
            }
            out.push(crate::relay::RouteDevice {
                id: p.id.clone(),
                name: p.name.clone(),
                ip: p.ip.clone(),
                port: p.port,
            });
        }
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }

    /// 每拍扫一遍（F-107 §11）：网段级策略的退休与 TTL、设备级链接的兜底寿命。
    pub fn sweep_relay_memory(self: &Arc<Self>) {
        // ① 反向记录带进来的行：超过 TTL 就连设备表里那一行一起收走。
        //    （加锁顺序：adopted → peers，与 `apply_relay_routes`、探活候选快照一致。）
        let expired: Vec<String> = {
            let mut adopted = self.adopted.lock();
            let now = Instant::now();
            let expired: Vec<String> = adopted
                .iter()
                .filter(|(_, at)| now.duration_since(**at) > ADOPTED_TTL)
                .map(|(id, _)| id.clone())
                .collect();
            for id in &expired {
                adopted.remove(id);
            }
            expired
        };
        if !expired.is_empty() {
            {
                let mut peers = self.peers.lock();
                peers.retain(|p| !expired.contains(&p.id));
            }
            debug!(n = expired.len(), "反向记录带进来的设备行到期，收走");
            self.emit_devices_changed();
        }

        // ② 策略与链路：先取快照（加锁顺序统一为 peers → direct_streak → config → pool）。
        let peers: Vec<PeerDevice> = self.peers.lock().clone();
        let streaks: HashMap<String, u32> = self.direct_streak.lock().clone();
        let now = chrono::Utc::now();
        let parsed_at = |s: &str| chrono::DateTime::parse_from_rfc3339(s).ok().map(|t| t.with_timezone(&chrono::Utc));

        let mut config = self.config.lock();
        let mut changed = false;

        for route in config.relay_routes.clone() {
            let Some(cidr) = crate::subnet::parse(&route.target_cidr) else {
                // 网段字符串坏掉（手改过）：这条策略没法判归属，直接退休。
                changed |= config.drop_relay_route(&route.target_cidr);
                continue;
            };
            // 该段已知设备（本机见过的那些）：全部直连稳定可达 → 这一段恢复了，策略退休。
            let in_segment: Vec<&PeerDevice> = peers
                .iter()
                .filter(|p| p.ip.parse::<std::net::Ipv4Addr>().map(|ip| cidr.contains(ip)).unwrap_or(false))
                .collect();
            if !in_segment.is_empty()
                && in_segment.iter().all(|p| {
                    p.online && streaks.get(&p.id).copied().unwrap_or(0) >= DIRECT_STABLE_ROUNDS
                })
            {
                if config.drop_relay_route(&route.target_cidr) {
                    info!(cidr = %route.target_cidr, "该段直连恢复且稳定，网段级中转策略退休");
                    changed = true;
                }
                continue;
            }
            // 锚点长期不再担保（池的 `seen_at` 太旧）→ 删掉。
            let stale = route
                .seen_at
                .as_deref()
                .and_then(parsed_at)
                .map(|t| now.signed_duration_since(t) > chrono::Duration::days(ROUTE_TTL_DAYS))
                .unwrap_or(false);
            if stale && config.drop_relay_route(&route.target_cidr) {
                info!(cidr = %route.target_cidr, "中转段长期不再担保，网段级策略退休");
                changed = true;
            }
        }

        for (id, link) in config.relay_links.clone() {
            let too_old = link
                .ok_at
                .as_deref()
                .and_then(parsed_at)
                .map(|t| now.signed_duration_since(t) > chrono::Duration::days(LINK_TTL_DAYS))
                .unwrap_or(false);
            // 对端已经不在设备列表里（解除配对后又没再被发现）：这条提示没有落点了。
            let gone = !peers.iter().any(|p| p.id == id);
            if (too_old || gone) && config.clear_relay_link(&id) {
                changed = true;
            }
        }

        // 池：只收掉**策略已经没了**的那些键与空池。
        //
        // 这里以前按"设备表里还认不认识它"清 —— 那是错的：池里的设备本就是**别段报来的**
        // （设备表那一行会随锚点离散被收走），按设备表清就等于"锚点一离线，池里那台谁也拨
        // 不动"（真机反馈第五轮踩过一次）。**成员本身不在这里淘汰**：池是"最后已知事实"，
        // 只按条数上限（[`merge_pool`]）与**策略本身的退休**走 —— 策略退休（该段直连稳定
        // 恢复、或长期没有一次成功）时，池跟着这条键一起收（上面那两档）。
        {
            let mut pool = self.relay_pool.lock();
            pool.retain(|target, devices| {
                !devices.is_empty() && config.relay_route(target).is_some()
            });
        }

        if changed {
            if let Err(e) = crate::config::save_config_at(&self.config_path, &config) {
                warn!("中转记忆回收落盘失败: {e}");
            }
            drop(config);
            self.emit_devices_changed();
        }
    }

    /// 给界面：本机的中转记忆（网段级策略 + 设备级链路），池解析成设备行。
    ///
    /// 池里那些设备**地址优先取设备表**（它反映"本机此刻跟它的关系"），表里没有那一行时
    /// 退回策略自己记下的**最后已知地址**（`via_addrs`）—— 锚点离散、那些行被收走之后，
    /// 界面照样得把"这一段经谁中转、各自在哪"列出来（设计 §12.3：记忆落盘了就该看得见）。
    pub fn relay_routes_json(&self) -> Vec<serde_json::Value> {
        let config = self.config.lock();
        let peers = self.peers.lock().clone();
        let view = |id: &str| {
            peers
                .iter()
                .find(|p| p.id == id)
                .map(|p| (p.name.clone(), p.ip.clone(), p.port, p.online, p.paired))
        };
        config
            .relay_routes
            .iter()
            .map(|r| {
                let devices: Vec<serde_json::Value> = r
                    .pool_ids()
                    .iter()
                    .map(|id| {
                        let (mut name, mut ip, mut port, online, paired) =
                            view(id).unwrap_or((id.clone(), String::new(), 0, false, false));
                        if ip.is_empty() || port == 0 {
                            if let Some(d) = r.pool_addr(id) {
                                ip = d.ip.clone();
                                port = d.port;
                                if !d.name.is_empty() {
                                    name = d.name.clone();
                                }
                            }
                        }
                        serde_json::json!({
                            "id": id,
                            "name": name,
                            "ip": ip,
                            "port": port,
                            "online": online,
                            "paired": paired || r.via_paired.contains(id),
                            "live": self
                                .relay_pool
                                .lock()
                                .get(&r.target_cidr)
                                .map(|d| d.iter().any(|e| &e.id == id))
                                .unwrap_or(false),
                        })
                    })
                    .collect();
                let links: Vec<serde_json::Value> = config
                    .relay_links
                    .iter()
                    .filter(|(_, l)| {
                        crate::subnet::parse(&l.via_cidr)
                            .zip(crate::subnet::parse(&r.via_cidr))
                            .map(|(a, b)| a.same_as(&b))
                            .unwrap_or(false)
                    })
                    .map(|(id, l)| {
                        serde_json::json!({ "id": id, "viaId": l.via_id, "okAt": l.ok_at })
                    })
                    .collect();
                serde_json::json!({
                    "targetCidr": r.target_cidr,
                    "viaCidr": r.via_cidr,
                    "devices": devices,
                    "links": links,
                    "lastOkAt": r.last_ok_at,
                    "okCount": r.ok_count,
                    "seenAt": r.seen_at,
                })
            })
            .collect()
    }

    /// 清掉中转记忆：`Some(cidr)` 只清那一条网段级策略，`None` 全清（含设备级链路）。
    /// 返回清掉的条数（网段级 + 设备级）。
    pub fn clear_relay_memory(self: &Arc<Self>, target_cidr: Option<&str>) -> usize {
        let mut removed = 0;
        {
            let mut config = self.config.lock();
            match target_cidr {
                Some(cidr) => {
                    removed += usize::from(config.drop_relay_route(cidr));
                    let keys: Vec<String> = config
                        .relay_links
                        .iter()
                        .filter(|(_, l)| l.via_cidr == cidr)
                        .map(|(id, _)| id.clone())
                        .collect();
                    for id in keys {
                        removed += usize::from(config.clear_relay_link(&id));
                    }
                }
                None => {
                    removed += config.relay_routes.len() + config.relay_links.len();
                    config.relay_routes.clear();
                    config.relay_links.clear();
                }
            }
            if let Err(e) = crate::config::save_config_at(&self.config_path, &config) {
                warn!("清中转记忆落盘失败: {e}");
            }
        }
        if removed > 0 {
            self.emit_devices_changed();
            info!(removed, "中转记忆已清除");
        }
        removed
    }

    /// 记一条刚结束的中转连接搬过的字节（`relay::pump` 结束时调用）：累加 live 计数并落盘
    /// （[`Config::relay_bytes_total`] / [`Config::relay_conns_total`]），重启后接着算。
    ///
    /// 落盘**只在连接结束时发生**（不是每条字节、也不是每 5s 一拍）：中转连接由事件驱动、
    /// 数量有限，写盘形不成风暴；而字节数在内存里是 live 的（[`AppState::relay_bytes`]），
    /// 一条长连接（`/events` 的 WS）中途也看得到它在涨。
    pub fn note_relay_traffic(&self, bytes: u64) {
        self.relay_bytes.fetch_add(bytes, Ordering::Relaxed);
        self.relay_conns.fetch_add(1, Ordering::Relaxed);
        self.persist_relay_traffic();
    }

    /// 本机替别人**转发出去的群消息**字节（F-100 的中转那一跳，`group::maybe_relay` 调用）：
    /// 累加 live 计数并落盘。
    ///
    /// 与 [`AppState::note_relay_traffic`] 分开的理由只有一条：**这不是一条连接**，不该动连接数
    /// —— 那一格记的是"本机替别人开出去的中转端点"（一条端点可能搬很久）。字节数与它合流
    /// （界面上"替别人搬了多少"是一个数）。
    pub fn note_relay_message(&self, bytes: u64) {
        if bytes == 0 {
            return;
        }
        self.relay_bytes.fetch_add(bytes, Ordering::Relaxed);
        self.persist_relay_traffic();
    }

    /// 把 live 的两个计数写回 `config.toml`（`relayBytesTotal` / `relayConnsTotal`）。
    /// 值没变就不写盘。
    fn persist_relay_traffic(&self) {
        let (b, c) = (
            self.relay_bytes.load(Ordering::Relaxed),
            self.relay_conns.load(Ordering::Relaxed),
        );
        let mut config = self.config.lock();
        if config.relay_bytes_total == b && config.relay_conns_total == c {
            return;
        }
        config.relay_bytes_total = b;
        config.relay_conns_total = c;
        if let Err(e) = crate::config::save_config_at(&self.config_path, &config) {
            warn!("中转流量落盘失败: {e}");
        }
    }

    /// 给界面：本机替别人中转的累计流量（字节，收发两方向之和）与连接数。
    pub fn relay_traffic_json(&self) -> serde_json::Value {
        serde_json::json!({
            "bytes": self.relay_bytes.load(Ordering::Relaxed),
            "connections": self.relay_conns.load(Ordering::Relaxed),
        })
    }

    /// 收到一条 mDNS 记录：**这台设备在喊**（不管此刻连不连得上）。
    ///
    /// 与 `set_peer_online` 是两回事：那个是"探活成功 = 真的连得上"，这个是"它的记录还在"。
    /// 界面把两者分开用：记录还在的离线行照旧列出来，用户**发起连接**时才据此补一句
    /// "这张网可能把单播隔开了"（它本身不是一个状态档）。
    pub fn note_seen(&self, peer_id: &str) {
        if peer_id.is_empty() {
            return; // 记录里没带 id 的（不是本项目的、或旧版本）：不当"看得见"
        }
        self.seen.lock().insert(peer_id.to_string(), Instant::now());
    }

    /// 记录被撤（对端发 TTL=0 告别，或 TTL 过期）：不再"看得见"。
    pub fn clear_seen(&self, peer_id: &str) {
        self.seen.lock().remove(peer_id);
    }

    /// 这台设备此刻"看得见"吗（最近收到过它的 mDNS 记录，且记录还没被撤）。
    pub fn is_seen(&self, peer_id: &str) -> bool {
        self.seen.lock().contains_key(peer_id)
    }

    /// 数据面该拨的地址（F-106）：优先地址簿里**此刻**的拨号地址（可能就是中转端点），
    /// 地址簿里没有这一台、或它没有可拨地址时，才退回调用方手里那个"对端自己声明过的"
    /// 地址（推送邀请里带来的 `callback` 就是这种）。
    ///
    /// 为什么优先地址簿：`callback` 是**某一刻**的观察值（对端在邀请里自报的地址，或本机
    /// 那一晚看到的来源地址），对端换了地址、或本机刚开始借用中转端点之后它就旧了 ——
    /// 拿它当唯一来源，表现是"直连恢复/改走中转之后老的那几笔推送还往死路上走"。
    pub fn dial_endpoint(
        &self,
        peer_id: &str,
        fallback: Option<(String, u16)>,
    ) -> Option<(String, u16)> {
        if let Some(peer) = self.peers.lock().iter().find(|p| p.id == peer_id) {
            if peer.dialable() {
                let (host, port) = peer.dial();
                return Some((host.to_string(), port));
            }
        }
        fallback
    }

    /// 这个 TCP 来源地址是不是"本机认识的**另一台**设备"（F-106）。
    ///
    /// 是的话，这条入站连接是**经它中转**过来的：来源是**中转方**，而信封里的身份是
    /// **请求方**。中转方的地址绝不能当成请求方"可拨过来的地址"记下 —— 记了以后本机就会
    /// 拿它去拨请求方（结果是拨到中转方自己的服务或某条中转端点上），表现成"消息/请求
    /// 发给了一台不相干的设备"。请求方自己的地址由它那条路（mDNS / 手动填 / 它自己请的
    /// 中转）确立，这里不猜。
    pub fn is_relayed_source(&self, source: std::net::IpAddr, caller_id: &str) -> bool {
        self.peers.lock().iter().any(|p| {
            p.id != caller_id && crate::net::parse_host(&p.ip).is_some_and(|ip| ip == source)
        })
    }

    /// 解除配对（F-14）：清 token、断链，但**保留设备行**并把它标回"未绑定"。
    ///
    /// 设备还在 mDNS 视野里就该继续出现在列表里（可以再点「配对」把关系建回来）；对方
    /// **离线**时这一行也还在 —— 界面把它排在在线设备下方（F-13），那正是唯一能"解除
    /// 配对"的机会：解除本身是本机动作，不需要对方在线（见 `lib.rs::unpair_peer`）。
    /// 地址簿（config.peers）里删掉，重启后不再记着它 —— 除非它再次被发现（或再次配对）。
    pub fn unpair(&self, peer_id: &str) {
        {
            let mut peers = self.peers.lock();
            if let Some(p) = peers.iter_mut().find(|p| p.id == peer_id) {
                p.paired = false;
            }
        }
        {
            let mut config = self.config.lock();
            config.peers.retain(|p| p.id != peer_id);
            let _ = crate::config::save_config_at(&self.config_path, &config);
        }
        self.emit_devices_changed();
    }

    /// 这个地址上除了 `keep_id` 之外还记着哪些 id。
    ///
    /// 一台设备在一个地址上只能有一个身份：对端被重置过（数据目录清了 → device id
    /// 换了）之后，本机为旧 id 存的那一行还指着同一个地址 —— 返回值就是这些旧行。
    pub fn stale_ids_at(&self, ip: &str, port: u16, keep_id: &str) -> Vec<String> {
        self.peers
            .lock()
            .iter()
            .filter(|p| p.id != keep_id && p.port == port && p.ip == ip)
            .map(|p| p.id.clone())
            .collect()
    }

    /// 把一台设备的行换个 id，其余照旧（配对状态、地址、长连接意图都保留）。
    ///
    /// 用在对端被重置过、但配对凭据还在（本机的凭据文件里那一条没跟着清）的场合：关系
    /// 还活着，只是对端的 id 变了。列表里**只留一行** —— 新 id 那行（mDNS 刚发现的）
    /// 并进这一行，不是并排出现两行。
    ///
    /// `to` 只提供**新 id 那一行的观察值**（名字/地址/在线）。`auto_connect` /
    /// `paused` 是用户意图、不属于新发现的这一行：照搬 `to` 的会把刚点过「断开」的
    /// 设备在新 id 上重新变成"要这条链"（`to` 是发现形状的行，两个字段恒为 false）。
    /// 意图跟着这一行走，不跟着 `to` 走。
    pub fn rekey_peer(&self, from: &str, to: &PeerDevice) {
        {
            let mut peers = self.peers.lock();
            let Some(pos) = peers.iter().position(|p| p.id == from) else {
                return; // 没有这一行，就没有可迁的身份
            };
            let mut row = peers.remove(pos);
            peers.retain(|p| p.id != to.id);
            row.id = to.id.clone();
            row.name = to.name.clone();
            row.ip = to.ip.clone();
            row.port = to.port;
            row.online = to.online;
            row.paired = to.paired;
            peers.push(row);
        }
        {
            let mut config = self.config.lock();
            if let Some(pos) = config.peers.iter().position(|p| p.id == from) {
                let mut row = config.peers.remove(pos);
                config.peers.retain(|p| p.id != to.id);
                row.id = to.id.clone();
                row.name = to.name.clone();
                row.ip = to.ip.clone();
                row.port = to.port;
                row.online = to.online;
                row.paired = to.paired;
                config.peers.push(row);
                let _ = crate::config::save_config_at(&self.config_path, &config);
            }
        }
        self.emit_devices_changed();
    }

    /// 这一行存的地址上已经是**别的设备**了（对端换过 device id，且它不认本机的
    /// 凭据）：作废地址并标记离线。
    ///
    /// 配对记录与 token 都**不动**：对端要是拿原 id 回来（配置恢复、备份还原），
    /// 这一行还能接着用。界面只显示在线设备，作废的行自然从列表里消失 —— 留下来
    /// 的只有那台设备真正的新身份，一行。
    pub fn forget_peer_address(&self, peer_id: &str) {
        let went_offline = {
            let mut peers = self.peers.lock();
            match peers.iter_mut().find(|p| p.id == peer_id) {
                Some(p) => {
                    let was = p.online;
                    p.online = false;
                    p.ip.clear();
                    p.port = 0;
                    was
                }
                None => false,
            }
        };
        {
            let mut config = self.config.lock();
            if let Some(p) = config.peers.iter_mut().find(|p| p.id == peer_id) {
                p.ip.clear();
                p.port = 0;
                // `online` 也落盘：不以 false 写回的话，重启后这一行会被当成"在线"
                // 重新出现在列表里 —— 它已经没有地址、不会被探活纠正，就永远赖着不走。
                p.online = false;
                let _ = crate::config::save_config_at(&self.config_path, &config);
            }
        }
        if went_offline {
            if let Some(app) = self.app_handle.lock().as_ref() {
                let _ = app.emit("peer://offline", serde_json::json!({ "peerId": peer_id }));
            }
        }
        self.emit_devices_changed();
    }

    pub fn get_peer(&self, peer_id: &str) -> Option<PeerDevice> {
        self.peers.lock().iter().find(|p| p.id == peer_id).cloned()
    }

    // ------------------------------------------------------------------
    // 跨网段发现：对端报来的设备（F-10 延伸）
    // ------------------------------------------------------------------

    /// 这台设备是不是「学来的」（本机自己从没发现过、由某台对端转述）。学来的行不参与
    /// 探活：本机跟它们之间没有任何关系，存活由担保人的 `/discover` 表刷新。
    pub fn is_learned(&self, peer_id: &str) -> bool {
        self.learned.lock().contains_key(peer_id)
    }

    /// 把对端（担保人）报来的「它那一段在线的设备」并进列表。
    ///
    /// 三条规则，缺一不可：
    ///
    /// - **只接手没见过的 id**：已经在本机列表里的行（mDNS 发现的、手动添加的、已配对的）
    ///   一个字都不动。那是"本机自己知道的"，不该被对端的转述改写，更不能让它被这一层
    ///   的超时清掉 —— 否则 B 一离线，A 连自己 mDNS 看得见的设备都会掉。**唯一例外**是
    ///   名字还顶着占位符的行（空串 / 就是设备 id，见 [`name_is_placeholder`]）：跨段设备
    ///   本机 mDNS 看不见，占位名只有这条转述能改掉。
    /// - **临时**：不写地址簿、不进凭据库，重启即忘。本机没跟它们建立过任何关系。
    /// - **存活跟着担保人**：刷新时间戳。担保人不再提到它（或担保人自己掉了、问不到了），
    ///   超过 [`LEARNED_TTL`] 就由 [`Self::sweep_learned`] 收走。
    pub fn learn_peers(&self, voucher_id: &str, learned: Vec<PeerDevice>) {
        let now = Instant::now();
        let own_id = self.config.lock().device_id.clone();
        let mut changed = false;
        {
            let mut table = self.learned.lock();
            let mut peers = self.peers.lock();
            for mut l in learned {
                // 自己、报它的那一方、以及没有地址的条目：进不了这张表。
                if l.id.is_empty() || l.id == own_id || l.id == voucher_id {
                    continue;
                }
                if l.ip.is_empty() || l.port == 0 {
                    continue;
                }
                // 担保人没说出名字（老版本、或它自己也只有占位名）：行里得有东西可显示 ——
                // 拿 id 顶着，与 `observe_verified_addr` 的占位一致（见 `name_is_placeholder`）。
                if l.name.trim().is_empty() {
                    l.name = l.id.clone();
                }
                match peers.iter_mut().find(|p| p.id == l.id) {
                    Some(existing) => {
                        // 已经是本机自己的行（mDNS/手动/配对/中转核实过地址的）：不接管它的
                        // 存活、地址与真名，一个字都不动。
                        if !table.contains_key(&l.id) {
                            // **唯一的例外是占位名**：`observe_verified_addr` 与
                            // `AppState::new` 建行时拿设备 id 顶着名字，此后本机 mDNS 看不见
                            // 它（跨段）、探活也填不回（`fill_peer_name` 走的是它，但被学来的
                            // 行恰好在探活队列之外）。担保人此刻报来了真名，就该收下 ——
                            // 不然那台设备在列表里永远显示 deviceid。
                            if name_is_placeholder(&existing.name, &existing.id)
                                && !name_is_placeholder(&l.name, &l.id)
                            {
                                existing.name = l.name.clone();
                                changed = true;
                            }
                            continue;
                        }
                        if existing.ip != l.ip || existing.port != l.port {
                            info!(
                                peer_id = %l.id,
                                was = %format!("{}:{}", existing.ip, existing.port),
                                now = %format!("{}:{}", l.ip, l.port),
                                "学来的对端换了地址"
                            );
                            existing.ip = l.ip;
                            existing.port = l.port;
                        }
                        existing.name = l.name;
                        existing.online = true;
                        table.insert(l.id.clone(), now);
                        changed = true;
                    }
                    None => {
                        peers.push(PeerDevice {
                            id: l.id.clone(),
                            name: l.name,
                            ip: l.ip,
                            port: l.port,
                            online: true,
                            paired: false,
                            auto_connect: false, paused: false, relay: None,
                        });
                        table.insert(l.id, now);
                        changed = true;
                    }
                }
            }
        }
        if changed {
            self.emit_devices_changed();
        }
    }

    /// 收走超时的「学来的」条目：担保人最近 [`LEARNED_TTL`] 内没再提到它们。
    ///
    /// 已经变成"本机自己的关系"的行不归这一层管 —— 用户手动添加过（地址簿里有它），
    /// 或者已经配对（凭据库里有 token）。那些行该由配对与探活维护，这里只把记录
    /// 从表里摘掉，行留着。
    pub fn sweep_learned(&self) {
        let now = Instant::now();
        let expired: Vec<String> = {
            let table = self.learned.lock();
            table
                .iter()
                .filter(|(_, seen)| now.duration_since(**seen) > LEARNED_TTL)
                .map(|(id, _)| id.clone())
                .collect()
        };
        if expired.is_empty() {
            return;
        }

        let adopted: HashSet<String> = {
            let config = self.config.lock();
            let paired = crate::credential::load_peers().unwrap_or_default();
            expired
                .iter()
                .filter(|id| {
                    config.peers.iter().any(|p| &p.id == *id) || paired.contains_key(*id)
                })
                .cloned()
                .collect()
        };

        let mut removed = false;
        {
            let mut table = self.learned.lock();
            let mut peers = self.peers.lock();
            for id in &expired {
                table.remove(id);
                if adopted.contains(id) {
                    continue;
                }
                let before = peers.len();
                peers.retain(|p| &p.id != id);
                removed |= peers.len() != before;
            }
        }
        if removed {
            self.emit_devices_changed();
        }
    }

    pub fn emit_devices_changed(&self) {
        if let Some(app) = self.app_handle.lock().as_ref() {
            let _ = app.emit("devices://changed", serde_json::json!({}));
        }
    }
}

/// Convenience: rebuild the list of visible peers for a broadcast share
/// (used by the share watcher to decide who to notify).
pub fn visible_peers(state: &Arc<AppState>, targets: &[String]) -> Vec<PeerDevice> {
    let peers = state.peers.lock();
    peers
        .iter()
        .filter(|p| targets.contains(&p.id))
        .cloned()
        .collect()
}

// ---------------------------------------------------------------------------
// 证书指纹（TLS 信任）
// ---------------------------------------------------------------------------

/// 把凭据库里钉住的指纹灌进 `net::Trust`：TLS 校验器**每一次握手**读的就是那份快照。
///
/// 整份重建而不是增量改：地址簿与凭据库才是真相。凡是这两处变了（配对、解除配对、地址
/// 漂移、指纹落库、对端换 id），都要跟着调一次 —— 少调一次，表现就是"刚配对好却连不上"。
pub fn refresh_trust(state: &Arc<AppState>) {
    let credentials = crate::credential::load_credentials().unwrap_or_default();
    let peers = state.peers.lock();
    let entries = peers.iter().flat_map(|peer| {
        let Some(pinned) = credentials
            .get(&peer.id)
            .map(|c| c.fingerprint.clone())
            .filter(|fp| !fp.is_empty())
        else {
            return Vec::new();
        };
        let mut out = Vec::new();
        if !peer.ip.is_empty() {
            out.push((peer.ip.clone(), pinned.clone()));
        }
        // 中转端点（F-106）：端点上呈现的是**同一台设备**的证书，但它在指纹表里占的是
        // **另一个地址键位**（IPv4 映射的 IPv6，见 `net::relay_host`）。必须一起钉 ——
        // 不钉的话那条路只剩 TOFU，而"中转这一跳有没有人在中间"正是要防的东西。
        if let Some(relay) = &peer.relay {
            out.push((relay.host.clone(), pinned));
        }
        out
    });
    crate::net::trust().seed(entries);
}

/// 记下这次连上对端时见到的证书指纹（TOFU 的落库那一半）。
///
/// 只在**本机还没有它的指纹**时写：已经有了，就说明这次握手正是拿那枚指纹校验过的，
/// 没有别的可记。调用点都在"刚跟对端成功建立了一条 TLS 连接"的地方。
pub fn remember_peer_fingerprint(state: &Arc<AppState>, peer_id: &str, ip: &str) {
    if matches!(
        crate::credential::get_peer_fingerprint(peer_id),
        Ok(Some(_))
    ) {
        return;
    }
    let Some(observed) = crate::net::trust().observed_at(ip) else {
        return; // 这一跳没走 TLS（回环 / 测试），没什么可记
    };
    match crate::credential::set_peer_fingerprint(peer_id, &observed) {
        Ok(()) => {
            info!(
                %peer_id,
                fingerprint = %crate::net::short_fingerprint(&observed),
                "已钉住对端证书指纹"
            );
            refresh_trust(state);
        }
        Err(e) => warn!(%peer_id, "指纹落库失败: {e}"),
    }
}

// ---------------------------------------------------------------------------
// 设备身份核对（同一台设备不能有两行）
// ---------------------------------------------------------------------------

/// 「学来的」条目能活多久：担保人超过这段时间没再提到它，就当作它已经不在那一段了。
///
/// 取探活周期的整数倍（5s → 4 拍）：对端报来一次就够它活着，中间丢一拍不会误伤；
/// 真的下线了，最迟 20 秒从列表里消失。
const LEARNED_TTL: Duration = Duration::from_secs(20);

/// 一个地址上现在是谁，以及它把带 token 的调用方认作谁。
#[derive(Debug, Clone)]
pub struct PeerIdentity {
    /// 地址上这台设备**此刻**的 device id。
    pub id: String,
    pub name: String,
    /// 对面的凭据库里，本机带过去的 token 反解出的 id。等于本机 id = 配对关系还活着
    /// （对端换过 device id 也照样活着）。没带 token、或 token 它不认 = `None`。
    pub paired_as: Option<String>,
    /// 对方此刻是否愿意替别人中转（`/pair/info` 的 `relay` 位，F-106）。缺字段（老版本）
    /// 按"不愿意"读 —— 默认关的那一侧语义就是"不愿意"。
    pub relay: bool,
    /// 对端**自报**的网段（F-107 的 `nets` 位）。缺字段（老版本）按空读 —— 那时退回
    /// "它落在本机哪块网卡的段里"。
    pub nets: Vec<String>,
    /// 对端**实际监听**的端口（`/pair/info` 的 `port` 位）。借中转时当请求方自报端口用。
    pub port: Option<u16>,
}

/// 问一个地址上的设备"你是谁、你把带这个 token 的调用方认作谁"（`/pair/info`）。
///
/// 这是**同一台设备两行**这个问题的关键一问：光"这个地址上有人答话"不足以证明
/// 旧行存的那台设备还在，必须问出它现在的 id。
///
/// **`Rejected` 必须与 `Unreachable` 分开**：前者是"这个地址上的身份不是本机认的那一个"
/// （对端换了机器 / 有人在中间），它既不是"离线"、也不是"换了 id 的另一台设备"，而是一条
/// 明确的关系问题（界面显示「需配对」）；后者只是"这会儿问不到"，判离线、等下一轮。
#[derive(Debug)]
pub enum IdentityProbe {
    Seen(PeerIdentity),
    Rejected,
    Unreachable,
}

pub async fn probe_identity(
    client: &reqwest::Client,
    ip: &str,
    port: u16,
    token: Option<&str>,
) -> IdentityProbe {
    let url = format!("{}/pair/info", crate::net::peer_base(ip, port));
    let mut request = client.get(&url);
    if let Some(t) = token {
        request = request.bearer_auth(t);
    }
    let response = match request.send().await {
        Ok(response) => response,
        Err(e) => {
            return if crate::net::is_identity_rejected(&e) {
                IdentityProbe::Rejected
            } else {
                IdentityProbe::Unreachable
            };
        }
    };
    let response = match response.error_for_status() {
        Ok(response) => response,
        Err(_) => return IdentityProbe::Unreachable,
    };
    match response.json::<serde_json::Value>().await {
        Ok(body) => IdentityProbe::Seen(PeerIdentity {
            id: body["id"].as_str().unwrap_or_default().to_string(),
            name: body["name"].as_str().unwrap_or_default().to_string(),
            paired_as: body["pairedAs"].as_str().map(str::to_string),
            relay: body["relay"].as_bool().unwrap_or(false),
            nets: body["nets"]
                .as_str()
                .map(crate::subnet::parse_list)
                .unwrap_or_default()
                .iter()
                .map(|c| c.to_cidr_string())
                .collect(),
            port: body["port"].as_u64().and_then(|p| u16::try_from(p).ok()),
        }),
        Err(_) => IdentityProbe::Unreachable,
    }
}

/// 只要"问到了"的那一份（指纹被拒 / 问不到都折叠成 `None`）。给"问不到就不妄动"的
/// 调用方用；需要区分原因的地方用 `probe_identity`。
pub async fn fetch_identity(
    client: &reqwest::Client,
    ip: &str,
    port: u16,
    token: Option<&str>,
) -> Option<PeerIdentity> {
    match probe_identity(client, ip, port, token).await {
        IdentityProbe::Seen(identity) => Some(identity),
        _ => None,
    }
}

/// [`fetch_discover`] 的三种结果。
#[derive(Debug)]
pub enum DiscoverOutcome {
    /// 对端答了：它是谁 + 它那一段里在线的设备（带**归属段**与中转能力，F-107 的池靠它）。
    Ok {
        identity: PeerIdentity,
        entries: Vec<DiscoveredPeer>,
        /// 应答方自报的网段（F-107）：`identity.nets` 的同一份，单独给池用。
        nets: Vec<String>,
        /// 应答方**实际监听**的端口（池里的锚点那一条要拿它当拨号目标）。
        listening_port: u16,
    },
    /// 这一问在这台对端上得不到答案（老版本没有这个端点；或者它不认我们手里的 token）。
    /// 退回 `/pair/info` —— 那一问不带 token 也答，能问出"它还认不认我们"。
    Fallback,
    /// 问不到（连不上、或对端此刻隐身/离线）。**不要再打第二遍**：对端此刻不算在线。
    Unreachable,
}

/// 向一个地址要「你是谁 + 你此刻看得见的设备」（`/discover`，跨网段发现）。
///
/// `own_id` 只用来填 `paired_as`：这一问带的是本机的 token，对端**认了**（否则 401，
/// 走 `Fallback`），所以"它把带这个 token 的调用方认作谁"的答案恒等于本机 id。
/// 对端换过 device id 的情形不在这里 —— 那时它不认旧 token，401 退回 `/pair/info`，
/// 由那条路去问出新的 id。
pub async fn fetch_discover(
    client: &reqwest::Client,
    ip: &str,
    port: u16,
    token: &str,
    own_id: &str,
) -> DiscoverOutcome {
    let url = format!("{}/discover", crate::net::peer_base(ip, port));
    let response = match client.get(&url).bearer_auth(token).send().await {
        Ok(r) => r,
        Err(_) => return DiscoverOutcome::Unreachable,
    };

    match response.status() {
        s if s.is_success() => {}
        // 401/403：这个 token 它不认（被解除配对、或对端换过 id）；404/405：老版本没有
        // 这个端点。两种都得回到 `/pair/info` 才有答案。
        StatusCode::UNAUTHORIZED
        | StatusCode::FORBIDDEN
        | StatusCode::NOT_FOUND
        | StatusCode::METHOD_NOT_ALLOWED => return DiscoverOutcome::Fallback,
        // 503 隐身、5xx：当作问不到，语义与探活一致（非 200 即不在网上）。
        _ => return DiscoverOutcome::Unreachable,
    }

    let body: serde_json::Value = match response.json().await {
        Ok(v) => v,
        Err(_) => return DiscoverOutcome::Fallback,
    };
    let Some(peer_id) = body["id"].as_str().filter(|s| !s.is_empty()) else {
        // 答得不是这个形状（中间有代理、或对端是自己拼的响应）→ 走老路，别瞎认。
        return DiscoverOutcome::Fallback;
    };

    let entries: Vec<DiscoveredPeer> = body["peers"]
        .as_array()
        .map(|arr| {
            arr.iter()
                .filter_map(|v| serde_json::from_value::<DiscoveredPeer>(v.clone()).ok())
                .filter(|e| !e.id.is_empty() && !e.ip.is_empty() && e.port != 0)
                .collect()
        })
        .unwrap_or_default();

    let nets: Vec<String> = body["nets"]
        .as_str()
        .map(crate::subnet::parse_list)
        .unwrap_or_default()
        .iter()
        .map(|c| c.to_cidr_string())
        .collect();
    let listening_port = body["port"].as_u64().and_then(|p| u16::try_from(p).ok()).unwrap_or(port);
    let relay = body["relay"].as_bool().unwrap_or(false);

    DiscoverOutcome::Ok {
        identity: PeerIdentity {
            id: peer_id.to_string(),
            name: body["name"].as_str().unwrap_or_default().to_string(),
            paired_as: Some(own_id.to_string()),
            relay,
            nets: nets.clone(),
            port: Some(listening_port),
        },
        entries,
        nets,
        listening_port,
    }
}

/// 「顺带交换视野」（F-11a）：把对端此刻看得见的设备要一份并进列表。
///
/// **链路活着也要做**：链路证明的是"本机跟它这一台的关系"，不是"它那一段有哪些设备已经
/// 同步过"。学来的条目 20s（[`LEARNED_TTL`]）没人再担保就会被 `sweep_learned` 收走，
/// 所以这件事得跟着 5s 这一拍持续做（而不是"配对那一次做一次"）。
///
/// 只有 `Ok` 才动手：401/404（对端不认这枚 token、或老版本没有这个端点）与连不上，
/// 都只是"这一拍没换到"—— 链路那一侧自己会处置（拨号被拒 → 「需配对」），不另报警。
async fn learn_from_peer(
    state: &Arc<AppState>,
    peer_id: &str,
    ip: &str,
    port: u16,
    token: &str,
    own_id: &str,
) {
    if let DiscoverOutcome::Ok { identity, entries, nets, listening_port } =
        fetch_discover(&PROBE_CLIENT, ip, port, token, own_id).await
    {
        absorb_discover_view(state, peer_id, ip, port, &identity, &nets, listening_port, &entries);
        state.learn_peers(peer_id, discovered_to_peers(&entries));
    }
}

/// `/discover` 的条目 → 设备行的观察值（身份 + 地址 + 在线；别的字段不是对端能说的）。
fn discovered_to_peers(entries: &[DiscoveredPeer]) -> Vec<PeerDevice> {
    entries
        .iter()
        .map(|e| PeerDevice {
            id: e.id.clone(),
            name: e.name.clone(),
            ip: e.ip.clone(),
            port: e.port,
            online: true,
            paired: false,
            auto_connect: false,
            paused: false,
            relay: None,
        })
        .collect()
}

/// 吸收一次 `/discover` 活视野（F-11a 的设备表 + F-107 的网段与中转池）。
///
/// 顺序要紧：先记网段（`note_peer_nets`），再建池（`absorb_view` 靠刚记下的 `nets` 分段）。
/// `host` 是**这一问打过去的地址** —— 直连时是对端的地址，经中转时是端点字面量（那时
/// 应答方自己进不了任何池：端点字面量不是它的网段地址，`absorb_view` 会因此跳过它，
/// 这是对的，池里的地址必须是能被直连拨通的地址）。
#[allow(clippy::too_many_arguments)]
fn absorb_discover_view(
    state: &Arc<AppState>,
    voucher_id: &str,
    host: &str,
    port: u16,
    identity: &PeerIdentity,
    nets: &[String],
    listening_port: u16,
    entries: &[DiscoveredPeer],
) {
    let _ = port;
    state.note_peer_nets(voucher_id, nets);
    // 顺带刷新"它愿不愿意替别人中转"。**这一句不能省**：已配对 + 长连接活着的对端走的是
    // `learn_from_peer` 这条路（不经过 `probe_addr`），少了它这一位的唯一来源就只剩 mDNS
    // TXT —— 而 TXT 只在它注册/重播时到过，开关后来打开的那台在锚点的 `/discover` 表里
    // 永远是 `relay:false`，于是"同段里还有谁允许中转"这一格在请求方那边永远是空的，
    // C 掉线也就没有 D 可换（真机就是这样）。活回话是权威：`/discover` 的 `relay` 就是
    // 它自己的 `allow_relay`。
    state.set_relay_capable(voucher_id, identity.relay);
    let voucher = DiscoveredPeer {
        id: identity.id.clone(),
        name: identity.name.clone(),
        ip: host.to_string(),
        port: listening_port,
        cidr: None,
        relay: identity.relay,
    };
    state.absorb_view(voucher_id, nets, identity.relay, &voucher, entries);
}

/// 一次探活得出的结论。
#[derive(Debug)]
pub enum ProbeStep {
    Online,
    Offline,
    /// 这个地址上现在回答的是**另一台设备**（对端换过 device id）。
    OtherIdentity(PeerIdentity),
}

/// 探活顺带问出的"配对关系还活着没有"：它亲口给的答案。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenLiveness {
    /// 对端认本机带过去的这枚 token（`pairedAs` 有值）。
    Alive,
    /// 对端答话了、id 也对得上，但它**不认**这枚 token —— 它把本机解除配对 / 清过数据。
    Dead,
    /// 问不出结论：本机没 token、地址上换了设备（那是另一条路）、或压根没问到。
    Unknown,
}

/// 从 `/pair/info`（或 `/discover`）的回话里读"这枚 token 它还认不认"。
///
/// 只有 `id` 对得上才算数：id 变了是"地址上换了设备"，由
/// [`apply_identity_reconcile`] 处理，不是这里的结论。`paired_as` 的语义见
/// [`PeerIdentity`]（对端凭据库里反解出的是谁）—— 有值 = 它认，没值 = 它不认。
pub fn token_liveness(seen: Option<&PeerIdentity>, expected_id: &str) -> TokenLiveness {
    match seen {
        Some(identity) if identity.id == expected_id => {
            if identity.paired_as.is_some() {
                TokenLiveness::Alive
            } else {
                TokenLiveness::Dead
            }
        }
        _ => TokenLiveness::Unknown,
    }
}

/// 由"地址上的回话"判定这台设备的死活。
///
/// 中间那条是全部要害：**"地址上有人答话"不等于"这台设备在"**。探活只按 ip:port
/// 打 `/pair/info`、拿到 200 就当在线的话，对端被重置过（id 变了）之后，旧行指向的
/// 还是同一个地址（同一台机器、同一个端口），它会 200 到永远 —— 界面上就出现
/// "两个同名设备，一个已绑定、一个未绑定"，而且旧行永远不消失。
pub fn probe_step(expected_id: &str, seen: Option<PeerIdentity>) -> ProbeStep {
    match seen {
        None => ProbeStep::Offline,
        // id 为空 = 老版本/中间有代理，问不出身份就当"是它"（别拿不确定的事杀行）
        Some(identity) if identity.id.is_empty() || identity.id == expected_id => {
            ProbeStep::Online
        }
        Some(identity) => ProbeStep::OtherIdentity(identity),
    }
}

/// 探活的客户端。**必须是带 pinning 的那一个**（`net::client_builder`）：到对端那一跳
/// 是 TLS，随手 `Client::new()` 只会在自签证书上失败 —— 表现是"所有设备都离线"，
/// 而且看不出原因。所以这个客户端不由调用方传进来，免得哪一处传错了。
static PROBE_CLIENT: std::sync::LazyLock<reqwest::Client> = std::sync::LazyLock::new(|| {
    crate::net::client_builder()
        .timeout(Duration::from_millis(1500))
        .build()
        .unwrap_or_default()
});

/// 身份核对用的客户端（超时比探活宽一点：这一步的回话更长，一问一答里带整段设备表）。
static RECONCILE_CLIENT: std::sync::LazyLock<reqwest::Client> = std::sync::LazyLock::new(|| {
    crate::net::client_builder()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap_or_default()
});

/// 按**一个具体地址**问对端"你是谁"（带 token 时顺便换视野、认配对关系）。
///
/// 单独成函数是因为一台设备可能有两个地址：它自己的直连地址，以及"直连不通时借的那条
/// 中转端点"（F-106）。两条路上站着的是同一台设备，判定逻辑一模一样。
async fn probe_addr(
    state: &Arc<AppState>,
    client: &reqwest::Client,
    peer_id: &str,
    host: &str,
    port: u16,
    token: Option<&str>,
    own_id: &str,
) -> IdentityProbe {
    let probe = match token {
        Some(t) => match fetch_discover(client, host, port, t, own_id).await {
            DiscoverOutcome::Ok { identity, entries, nets, listening_port } => {
                absorb_discover_view(state, peer_id, host, port, &identity, &nets, listening_port, &entries);
                state.learn_peers(peer_id, discovered_to_peers(&entries));
                IdentityProbe::Seen(identity)
            }
            DiscoverOutcome::Fallback => probe_identity(client, host, port, token).await,
            DiscoverOutcome::Unreachable => IdentityProbe::Unreachable,
        },
        None => probe_identity(client, host, port, token).await,
    };
    // 顺带刷新"它愿不愿意替别人中转"（/pair/info 与 /discover 都带这一位）。
    // 两条路问到的都是同一台设备，所以哪一种答案都能用。
    if let IdentityProbe::Seen(identity) = &probe {
        state.set_relay_capable(peer_id, identity.relay);
        // F-107：网段自报在两处都要记 —— `/discover` 那条路已经顺手吸了视野，但退回
        // `/pair/info` 的那条（没 token / 老版本没有 /discover）也带着 `nets`，
        // 少了这一句就会"有的设备有段、有的没有"。
        state.note_peer_nets(peer_id, &identity.nets);
    }
    probe
}

/// 一轮探活：给每台"有地址"的设备问一次身份，判死活或收拾换了 id 的行。
///
/// 由 lib.rs 的 5s 循环调用；单独成函数是为了让"地址上现在是另一台设备"这条路径
/// 能在测试里对着真服务器跑（见 tests/p1_integration.rs）。
pub async fn probe_peers_once(state: &Arc<AppState>) {
    let client = &PROBE_CLIENT;
    // 「学来的」行不在这里探：它们不是本机发现的，本机跟它们之间没有关系，拨过去既没有
    // 意义（本机到不了它时那一跳纯属空转），也正是这条路要避免的多余一跳。它们的存活由
    // 担保人的 `/discover` 表刷新 —— 见 `learn_peers` / `sweep_learned`。
    let learned: HashSet<String> = state.learned.lock().keys().cloned().collect();
    // 反向记录带进来的行也快照成一份：不能在持 `peers` 守卫时去锁 `adopted`（`sweep` 是反过来
    // 先 `adopted` 再 `peers` 的，两处抢锁顺序必须一致）。
    let adopted: HashSet<String> = state.adopted.lock().keys().cloned().collect();
    let own_id = state.config.lock().device_id.clone();

    let candidates: Vec<PeerDevice> = state
        .peers
        .lock()
        .iter()
        .filter(|p| {
            // 只排除"本机跟它之间没有关系"的那两类（学来的 / 反向记录带进来的：要借中转时
            // 按需拨）。**有地址的、只有一条借来端点的、以及什么都还没有的**都要进来 ——
            // 最后那一类（只经中转见过一面、重启后又什么都不剩）在下面只能"借一次"，
            // 而这正是它的出路所在。
            !learned.contains(&p.id) && !adopted.contains(&p.id)
        })
        .cloned()
        .collect();
    for p in candidates {
        // 链路活着就不用再探身份 —— **但正走中转的那几台要除外**：链路证明的是"本机跟它
        // 之间的关系"，而中转那条链是**借来的**，直连什么时候回来只能靠直连那一跳自己说
        // （见下，直连一通就收起中转）。少了这一条，只要中转链不断，本机就永远不再试直连。
        if state.ws.has_connection(&p.id) && p.relay.is_none() {
            state.set_peer_online(&p.id, true);
            // 有一台链路活着但还欠着消息（那一跳当时没写进去）：每一拍都值得再试一次。
            // 有闸门，重复叫不会叠出一条并发的补发。
            crate::chat::flush_queued(state, &p.id);
            // 推送同理：链路活着的时候把"还没落终态的邀请"问一遍，回执丢了就靠这一拍补回来
            // （同样有闸门 —— `PushStore::ask_due` 把同一台设备的节奏按在 20 秒）。
            crate::transfer::reconcile(state, &p.id);
            // 链路活着就不用再探身份 —— **但视野交换与链路无关**：链路证明的是"本机跟它
            // 这一台的关系"，不是"它那一段有哪些设备已经同步过"。原先这里直接 `continue`，
            // 于是"配对之后点「连接」"这台锚点就再也不报表了：学来的条目 20s
            // （`LEARNED_TTL`）内被 `sweep_learned` 收走，而且再也不会回来。
            // 凭据库读不到只是这一拍换不成，不动上面那三件事。
            if let Ok(Some(token)) = crate::credential::get_peer_token(&p.id) {
                learn_from_peer(state, &p.id, &p.ip, p.port, &token, &own_id).await;
            }
            continue;
        }
        // 一条路都没有（没地址、没借来的端点、也没有长连接 —— 上面那一支已经滤掉有链路的）：
        // 直连与中转都无处可探，但**出路可能就在手里**（记录里的池）。试着借一次：
        // 借到了，下一拍就有探活、设备行也就说得清"经谁中转"（不再等用户先发一条消息）；
        // 借不到也不亏 —— `ensure_for` 没候选时一次请求都不发，有候选时 30s 才一次。
        if !p.dialable() {
            let st = Arc::clone(state);
            let id = p.id.clone();
            crate::task::spawn("relay warm-up", async move {
                crate::relay::ensure_for(&st, &id).await;
            });
            continue;
        }
        // 手里那条 token 一起捎上：对端顺带回答"它把本机认作谁"，那是配对关系
        // 是否还活着的证据。**读不到 ≠ 没有**：凭据库的瞬时故障（凭据管理器忙、同机多
        // 实例并发）不能当成"我们没配对"—— 那会问出一个"它不认我们"的假答案，
        // 进而把一条好好的地址当垃圾扔掉。这一轮跳过它，下一轮（5s 后）再说。
        let token = match crate::credential::get_peer_token(&p.id) {
            Ok(t) => t,
            Err(e) => {
                warn!(peer_id = %p.id, "凭据库读失败，这一轮跳过探活: {e}");
                continue;
            }
        };
        // 直连那一跳**恒用地址簿里的地址**：它同时是"直连通不通"的信号 —— 决定要不要走
        // 中转，也决定直连什么时候恢复（恢复的那一刻就收起中转）。
        //
        // 没有直连地址的那一类（只有一条借来的中转链）：直连这一档恒"够不着"，
        // 可达性由下面那一跳（中转端点）证明，别拿空地址去拨。
        let direct = if p.ip.is_empty() || p.port == 0 {
            IdentityProbe::Unreachable
        } else {
            probe_addr(state, client, &p.id, &p.ip, p.port, token.as_deref(), &own_id).await
        };
        // F-107 §9.2：直连这一拍的结果（连续 3 拍可达 → 清设备级记忆）。在 `direct` 被
        // move 走之前先记下来。
        let direct_ok = matches!(direct, IdentityProbe::Seen(_));
        state.note_direct_probe(&p.id, direct_ok);
        // 直连不通、而此刻正为它挂着一跳中转：再问一次**中转端点**（端点上站着的是同一台
        // 设备，只是路是借来的）。它答话就说明"够得着"仍然成立，设备行按在线算；连它也
        // 不通就把这条借来的路收掉，下面按"离线"处理（会限流地重新找一条）。
        let (probe, host, via_relay) = match (direct, p.relay.clone()) {
            (IdentityProbe::Unreachable, Some(relay)) => {
                let retry = probe_addr(
                    state,
                    client,
                    &p.id,
                    &relay.host,
                    relay.port,
                    token.as_deref(),
                    &own_id,
                )
                .await;
                match retry {
                    IdentityProbe::Seen(seen) => {
                        // **F-107 的唯一写入点**：经中转端点真的探到了目标（不是"控制面
                        // 200"——那只能证明中转方愿意开）。写设备级链路 + 提升成网段级策略。
                        state.note_relay_success(&p.id, &relay.via);
                        (IdentityProbe::Seen(seen), relay.host, true)
                    }
                    other => {
                        state.clear_relay(&p.id);
                        (other, relay.host, true)
                    }
                }
            }
            (other, _) => (other, p.ip.clone(), false),
        };
        // 指纹对不上：这个地址上的身份不是本机认的那一个。既不是"离线"（它答话了），
        // 也不是"换了 id 的另一台设备"（那是同一台机器上的新身份，有 token 可以迁）——
        // 这是一条明确的关系问题，界面上就该显示「需配对」。
        if matches!(probe, IdentityProbe::Rejected) {
            warn!(peer_id = %p.id, "对端证书指纹与钉住的不一致：需重新配对");
            state.set_peer_online(&p.id, false);
            state.ws.set_link_state(
                &p.id,
                crate::ws::LinkState::Stopped {
                    reason: crate::ws::StopReason::IdentityChanged,
                },
            );
            continue;
        }
        let seen = match probe {
            IdentityProbe::Seen(identity) => Some(identity),
            _ => None,
        };
        match probe_step(&p.id, seen.clone()) {
            ProbeStep::Online => {
                // 直连这一跳通了：借来的那条收掉（"能直连就直连"）。走中转时
                // （`via_relay`）当然不动它 —— 这才是这条路本身。
                if !via_relay {
                    crate::relay::clear_for(state, &p.id);
                }
                // 顺带把"它叫什么"填回设备行：这一问的回话里就带着名字，而**占位名**
                // （`observe_verified_addr` 拿设备 id 顶着的那种）此前没有哪条路能填它
                // —— 这类设备本机 mDNS 看不见（跨段）、学来的行又在探活队列之外。
                // 只填占位行，真名一个字都不动（见 `fill_peer_name`）。
                if let Some(identity) = &seen {
                    state.fill_peer_name(&p.id, &identity.name);
                }
                // 指纹只在这一档才认：这个地址上答话的确实是**这台设备**，见到的证书
                // 才是它的。在 `Seen` 里就钉（不管 id 对不对）会把别台设备的证书记到
                // 这一行名下 —— 安卓模拟器那条路上，被钉住的正是**本机自己的**证书
                // （对端自报的地址恰好是本机的监听端点），下一次真去连它就成了
                // "指纹与钉住的不一致"。
                //
                // 钉的是**刚拨的那个地址**（`host`）：走中转时那是中转端点的字面量，
                // 与它自己的地址各占一个键位（见 `net::relay_host`）。
                remember_peer_fingerprint(state, &p.id, &host);
                state.set_peer_online(&p.id, true);
                // F-107 §10：本机的中转记忆变了就把路由记录推给这台对端（内容没变不重发，
                // 最短 30s 一拍）。**直连与中转两条路都推** —— 反向那一侧要的是"这段设备
                // 怎么够得着"，跟本机这一跳怎么走无关。
                crate::relay::maybe_send_route(state, &p.id);
                // 排队等它的消息：够不着常常只是暂时的（它刚起来、或上一次那一跳正好撞上它的
                // 重启），所以**每一拍**都再叫一次 —— 不只是"离线→在线"那一次翻转。闸门在，
                // 已经在跑的那条不会被叠第二条。
                crate::chat::flush_queued(state, &p.id);
                // 推送那笔账也一样补一次（同一批调用点、同一套理由）。
                crate::transfer::reconcile(state, &p.id);
                // 顺带确认"配对还活不活着"：本机带着 token 问过去、它答话却说"不认这枚"
                // （`pairedAs` 为空），而本机这一行还写着已配对 —— 那就是对端把本机解除配对
                // / 清过数据的**它亲口的证据**。标成「需配对」（与拨号 401 落到同一个状态）：
                // 界面才会提示该重新配对，对端发来的配对邀请也才会放行到通知里（见
                // `server::routes_pair`）。token 与配对标记**都不动** —— 万一它只是瞬时读不到，
                // 下一轮探活就会把这一档撤掉。
                if p.paired && token.as_deref().is_some_and(|t| !t.is_empty()) {
                    match token_liveness(seen.as_ref(), &p.id) {
                        TokenLiveness::Dead => {
                            // 已经是「需配对」就别每 5s 重写一遍（状态没变，只是徒增广播）。
                            if !state.ws.needs_repair(&p.id) {
                                info!(
                                    peer_id = %p.id,
                                    "对端不认本机的凭据（它解除过配对 / 清过数据）：需重新配对"
                                );
                                state.ws.set_link_state(
                                    &p.id,
                                    crate::ws::LinkState::Stopped {
                                        reason: crate::ws::StopReason::TokenRejected,
                                    },
                                );
                                state.emit_devices_changed();
                            }
                        }
                        // 它又认了（上一次是瞬时读不到，或对端恢复过备份）：把这一档撤掉，
                        // 否则「需配对」会一直赖在界面上。
                        TokenLiveness::Alive => {
                            if state.ws.needs_repair(&p.id) {
                                state.ws.clear_link_state(&p.id);
                                state.emit_devices_changed();
                            }
                        }
                        TokenLiveness::Unknown => {}
                    }
                }
                // 离线→在线：这就是"对端回到网上了"这个证据。正在待命的拨号循环光靠
                // 自己的 15s 轮询要慢一拍，这里直接把它叫起来（已经在跑的循环不会被动）。
                if !p.online {
                    crate::ws::redial_if_remembered(state, &p.id);
                }
            }
            ProbeStep::Offline => {
                if p.online {
                    info!(peer_id = %p.id, "peer unreachable, marking offline");
                }
                state.set_peer_online(&p.id, false);
                // 直连不通：如果本机认识"愿意替别人中转、又够得着它"的设备，请一台替它开
                // 条路（F-106）。只在**这一档**发起，而且 `ensure_for` 自带限流（同一台
                // 设备 30s 一次）—— 直连恢复时上面那一档已经把中转收掉了。
                let relay_state = Arc::clone(state);
                let target_id = p.id.clone();
                tokio::spawn(async move {
                    crate::relay::ensure_for(&relay_state, &target_id).await;
                });
            }
            // 地址上回答的是另一台设备：对端换过 device id。别把这一行当成"它在线"
            // ——那正是列表里同一台设备两行（旧行已绑定、新行未绑定）的来源。
            ProbeStep::OtherIdentity(identity) => {
                apply_identity_reconcile(state, &p.id, &identity);
                // 换 id 时凭据整条（token + 指纹）跟着迁，地址簿也变了 —— Trust 那张
                // "地址 → 指纹"的表得重灌一次，否则新 id 那条记录不会生效。
                refresh_trust(state);
            }
        }
    }

    // 担保人这一轮没再提到、或担保人自己掉了的学习条目，从这里收走。放在一轮之末：
    // 这一轮里刚被提到的都已经刷新过时间戳，不会误伤。
    state.sweep_learned();
    // F-107 §11：网段级策略的退休与 TTL、设备级链接的兜底寿命、没有策略的池。
    state.sweep_relay_memory();
}

/// 向一个地址问清身份并按结果收拾旧行（探活与 mDNS 发现共用的入口）。
pub async fn reconcile_stale_identity(state: &Arc<AppState>, stale_id: &str) {
    let client = &RECONCILE_CLIENT;
    let Some(stale) = state.get_peer(stale_id) else {
        return;
    };
    if stale.ip.is_empty() || stale.port == 0 {
        return;
    }
    let token = match crate::credential::get_peer_token(stale_id) {
        Ok(t) => t,
        // 读不到不是"没配对"：带着错的前提去问，会得到一个错的身份结论（进而
        // 把一条好好的地址当垃圾扔掉）。交给下一轮。
        Err(e) => {
            warn!(%stale_id, "凭据库读失败，暂不处理这一行: {e}");
            return;
        }
    };
    let Some(seen) = fetch_identity(client, &stale.ip, stale.port, token.as_deref()).await else {
        return; // 问不到就不妄动：这一行交给探活去判死活
    };
    apply_identity_reconcile(state, stale_id, &seen);
}

/// 已经拿到"这个地址上现在是谁"之后怎么处置那一行（网络部分见 `fetch_identity`）。
///
/// 两条路，都不会留下"同一台设备两行"：
///
/// - **它还认我们**（`paired_as` = 本机 id）→ 对端被清的只是它那边的一部分状态，本机手里
///   那条凭据还有效。这一行**迁到新 id**：配对状态、地址、"长连接"
///   记忆都跟着过去，凭据的键也换掉（token 与指纹不变，两边照旧互相认）。
/// - **它不认我们** → 关系确实断了（对端的凭据库也清了）。只作废这一行的地址、标记
///   离线，配对记录与 token 都留着 —— 对端拿原 id 回来时这一行还能用。列表里剩下的
///   是它真正的新身份（mDNS 发现的那一行）。
///
/// 返回 `true` = 这一行被迁到了新 id。
pub fn apply_identity_reconcile(
    state: &Arc<AppState>,
    stale_id: &str,
    seen: &PeerIdentity,
) -> bool {
    let Some(stale) = state.get_peer(stale_id) else {
        return false;
    };
    if seen.id.is_empty() || seen.id == stale_id {
        return false; // 还是它自己：没有什么可收拾的
    }
    let own_id = state.config.lock().device_id.clone();
    // 这一步直接决定要不要动这一行（迁 id / 作废地址），所以读失败必须**什么都不做**：
    // 把"读不到"当成"它不认我们"，会拿瞬时故障去删一条有效的地址。
    let token = match crate::credential::get_peer_token(stale_id) {
        Ok(t) => t,
        Err(e) => {
            warn!(%stale_id, "凭据库读失败，暂不处置这一行: {e}");
            return false;
        }
    };

    if token.is_some() && seen.paired_as.as_deref() == Some(own_id.as_str()) {
        info!(
            old = %stale_id,
            new = %seen.id,
            addr = %format!("{}:{}", stale.ip, stale.port),
            "对端换过 device id（配置被清、凭据库还在）：配对关系迁到新 id"
        );
        if let Err(e) = crate::credential::rekey_peer(stale_id, &seen.id) {
            warn!(%e, "凭据换键失败，新 id 会以未绑定出现");
        }
        // 旧 id 上的链路与「断开」暂停标记一起作废、转到新 id：链路按 token 建，
        // 换 key 后新 id 继续用；留着旧 key 会让界面同时显示"连着长连接的旧行"。
        let was_paused = state.ws.is_paused(stale_id);
        state.ws.drop_peer(stale_id);
        if was_paused {
            state.ws.pause(&seen.id);
        }
        state.rekey_peer(
            stale_id,
            &PeerDevice {
                id: seen.id.clone(),
                name: if seen.name.is_empty() { stale.name.clone() } else { seen.name.clone() },
                ip: stale.ip.clone(),
                port: stale.port,
                online: true,
                paired: true,
                // 这一行只是观察值：配对状态与地址从旧行搬（见 `rekey_peer`），长连接
                // 意图（`auto_connect` / `paused`）同样跟着旧行走，不在这里填。
                ..Default::default()
            },
        );
        if stale.auto_connect && !was_paused {
            crate::ws::restore_link(state, &seen.id);
        }
        true
    } else {
        warn!(
            old = %stale_id,
            new = %seen.id,
            addr = %format!("{}:{}", stale.ip, stale.port),
            "地址上已是另一台设备（id 变了），且它不认本机的凭据：作废这一行存的地址"
        );
        state.forget_peer_address(stale_id);
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一个不落盘、不碰真实数据目录的 state（F-107 的记忆逻辑都作用在它上面）。
    fn memory_state() -> Arc<AppState> {
        let dir = std::env::temp_dir().join(format!("orbdrop_mem_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        Arc::new(AppState::new(
            crate::config::Config {
                device_id: "own-mem".into(),
                device_name: "OWN".into(),
                relay_memory: true,
                ..Default::default()
            },
            dir.join("config.toml"),
        ))
    }

    fn discovered(id: &str, ip: &str, cidr: Option<&str>, relay: bool) -> DiscoveredPeer {
        DiscoveredPeer {
            id: id.to_string(),
            name: id.to_uppercase(),
            ip: ip.to_string(),
            port: 3890,
            cidr: cidr.map(str::to_string),
            relay,
        }
    }

    /// 池只收「报出方说自己在这一段 + 允许中转」的设备，外加应答方自己（它不在自己的表里）。
    ///
    /// **键是目标段**（"去这一段该借谁"），由策略（`relayRoutes[目标].via_cidr`）把"应答方
    /// 网段"映射过来 —— 这条断言就是那个曾经写错的地方（按应答方网段查池，跨网段永远查不到）。
    #[test]
    fn pool_from_discover_keeps_only_same_cidr_relay_capable() {
        let state = memory_state();
        // 策略：去 192.168.1.0/24 这一段，经 192.168.50.0/24 中转。
        state.config.lock().relay_route_mut("192.168.1.0/24").via_cidr = "192.168.50.0/24".into();

        let voucher = discovered("c", "192.168.50.7", None, true);
        let entries = vec![
            discovered("d", "192.168.50.8", Some("192.168.50.0/24"), true), // 同段 + 允许 → 进池
            discovered("e", "192.168.50.9", Some("192.168.50.0/24"), false), // 同段但不允许
            discovered("f", "10.9.9.9", Some("10.9.0.0/16"), true),          // 别的段
            discovered("g", "192.168.50.10", None, true),                    // 没说自己在哪一段
        ];
        state.absorb_view("c", &["192.168.50.0/24".into()], true, &voucher, &entries);

        let ids: Vec<String> = state
            .pool_for("192.168.1.0/24")
            .into_iter()
            .map(|p| p.id)
            .collect();
        assert_eq!(ids, vec!["c".to_string(), "d".to_string()], "锚点自己 + 同段且允许中转的");
        assert!(
            state.pool_for("192.168.50.0/24").is_empty(),
            "中转方所在的那一段不是目标段：池不该挂在这个键上"
        );
        assert!(state.pool_for("10.9.0.0/16").is_empty(), "应答方不在那一段：不建池");
        assert!(
            state.pool_for("192.168.1.0/24").iter().all(|p| p.ip.starts_with("192.168.50.")),
            "池里的地址必须是应答方报的那一份"
        );

        // 应答方关掉中转 → 它自己出池（表里的别人不受影响）。
        state.absorb_view(
            "c",
            &["192.168.50.0/24".into()],
            false,
            &discovered("c", "192.168.50.7", None, false),
            &entries[..1],
        );
        let ids: Vec<String> = state
            .pool_for("192.168.1.0/24")
            .into_iter()
            .map(|p| p.id)
            .collect();
        assert_eq!(ids, vec!["d".to_string()], "锚点关掉开关就不该再被当候选");
    }

    /// 池的地址要跟着**活视野**走进策略里（`RelayRoute::via_addrs`）—— 锚点不再供表之后，
    /// 拨得动池里那台就全靠这一份（真机反馈第五轮：池只存 id，锚点一离线就谁也拨不动，
    /// §8.2 的失效转移因此从不发生）。
    #[test]
    fn pool_refresh_keeps_the_addresses_on_the_route() {
        let state = memory_state();
        state
            .config
            .lock()
            .relay_route_mut("192.168.1.0/24")
            .via_cidr = "192.168.50.0/24".into();
        state.absorb_view(
            "c",
            &["192.168.50.0/24".into()],
            true,
            &discovered("c", "192.168.50.7", None, true),
            &[discovered("d", "192.168.50.8", Some("192.168.50.0/24"), true)],
        );
        let route = state
            .config
            .lock()
            .relay_route("192.168.1.0/24")
            .cloned()
            .expect("策略还在");
        assert_eq!(
            route
                .via_addrs
                .iter()
                .map(|d| (d.id.as_str(), d.ip.as_str(), d.port))
                .collect::<Vec<_>>(),
            vec![("c", "192.168.50.7", 3890), ("d", "192.168.50.8", 3890)],
            "活池的地址要写进策略：锚点离散后池里那台就靠它拨得动"
        );
        assert!(
            route.via_devices.is_empty(),
            "这一档只更新地址；id 列表由成功那一刻（`note_relay_success`）写"
        );
    }

    /// 已配对 + 长连接活着的对端**不走 `probe_addr`**（那条路才刷能力位），它的 `/discover`
    /// 活回话必须自己把 `relay` 位刷掉 —— 否则锚点表里那台允许中转的设备永远是 `relay:false`，
    /// 请求方那边的池里就永远没有"同段还有谁"，锚点掉线也就没有第二台可换（真机就是这样）。
    #[test]
    fn relay_capability_refreshes_from_the_live_discover_reply() {
        let state = memory_state();
        state.set_relay_capable("c", false);
        let identity = PeerIdentity {
            id: "c".into(),
            name: "PC-C".into(),
            paired_as: None,
            relay: true,
            nets: vec!["192.168.50.0/24".into()],
            port: Some(3890),
        };
        absorb_discover_view(&state, "c", "192.168.50.7", 3890, &identity, &identity.nets, 3890, &[]);
        assert!(
            state.is_relay_capable("c"),
            "活回话说它开着 S-09，能力位就得跟着变（mDNS 的 TXT 只在注册时到过一次）"
        );
        assert_eq!(state.peer_nets_of("c"), vec!["192.168.50.0/24".to_string()]);
    }

    /// 中转方在 `/relay/open` 回话里自报的那一段池：不配对也能拿到 → C 掉线才有 D 可换。
    #[test]
    fn relay_report_fills_the_pool_without_pairing() {
        let state = memory_state();
        state.note_peer_nets("b", &["192.168.1.0/24".to_string()]); // 目标自报的段
        state.add_peer_book(PeerDevice {
            id: "b".into(),
            name: "B".into(),
            ip: "192.168.1.6".into(),
            port: 3890,
            online: true,
            ..Default::default()
        });
        let devices = vec![
            crate::relay::RouteDevice {
                id: "c".into(),
                name: "C".into(),
                ip: "192.168.50.7".into(),
                port: 3890,
            },
            crate::relay::RouteDevice {
                id: "d".into(),
                name: "D".into(),
                ip: "192.168.50.8".into(),
                port: 3890,
            },
        ];
        let adopted = state.absorb_relay_report("b", "192.168.50.0/24", &devices);
        assert_eq!(adopted, 2, "不认识的设备并进列表（有 TTL、不探活）");
        let ids: Vec<String> = state.pool_for("192.168.1.0/24").into_iter().map(|p| p.id).collect();
        assert_eq!(ids, vec!["c".to_string(), "d".to_string()], "池按**目标段**记");
        // 报来的设备不会自报网段（adopted 不进探活队列）→ 由"报出方说的那一段"补上，
        // 否则 `cidr_of` 是空、网段级策略写不出来（真机上 `via_cidr=` 空就是这个原因）。
        assert_eq!(
            state.cidr_of("d", "192.168.50.8").as_deref(),
            Some("192.168.50.0/24")
        );
        assert!(
            state
                .relay_devices_in(&crate::subnet::parse("192.168.50.0/24").unwrap(), &[])
                .is_empty(),
            "adopted 那些行不算本机认识的关系：不该被报成这一段的候选"
        );

        // 目标段算不出来（既没自报、也不在本机任何网段）→ 这条池没有落点，不记。
        assert_eq!(state.absorb_relay_report("zz", "192.168.50.0/24", &devices), 0);
    }

    /// 复现（真机反馈第七轮）：一台中转方**离线过一会儿**就会被快照从记忆里抹掉。
    ///
    /// 活视野（锚点的 `/discover`）与中转方的自报都只含**此刻在线**的设备，而它们原先
    /// 是**整份替换**记忆的 —— 于是"D 离线一会儿（C 还在报）"这一拍就把 D 的最后已知地址
    /// 从 `viaAddrs` / 池里删掉；等 C 离线、D 又上线时，谁也想不起 D 在哪 → 消息发不出去。
    #[test]
    fn a_relay_that_blinked_stays_remembered() {
        let state = memory_state();
        state.note_peer_nets("b", &["192.168.1.0/24".to_string()]);
        state.add_peer_book(PeerDevice {
            id: "b".into(),
            name: "B".into(),
            ip: "192.168.1.6".into(),
            port: 3890,
            online: true,
            ..Default::default()
        });
        let devices = vec![
            crate::relay::RouteDevice {
                id: "c".into(),
                name: "C".into(),
                ip: "192.168.50.7".into(),
                port: 3890,
            },
            crate::relay::RouteDevice {
                id: "d".into(),
                name: "D".into(),
                ip: "192.168.50.8".into(),
                port: 3890,
            },
        ];
        state.absorb_relay_report("b", "192.168.50.0/24", &devices);
        state.note_relay_success("b", "c");
        assert!(
            state
                .config
                .lock()
                .relay_route("192.168.1.0/24")
                .expect("策略已建")
                .via_addrs
                .iter()
                .any(|x| x.id == "d"),
            "前提：D 在记忆里"
        );

        // D 离线一会儿：锚点 C 这一拍的活视野里没有 D（`/discover` 只报在线的）。
        state.absorb_view(
            "c",
            &["192.168.50.0/24".into()],
            true,
            &discovered("c", "192.168.50.7", None, true),
            &[],
        );

        let route = state
            .config
            .lock()
            .relay_route("192.168.1.0/24")
            .cloned()
            .expect("策略还在");
        assert!(
            route
                .via_addrs
                .iter()
                .any(|x| x.id == "d" && x.ip == "192.168.50.8"),
            "一次快照里没有它 ≠ 它没了：D 的最后已知地址要留着，实际 {:?}",
            route.via_addrs
        );
        assert!(
            state.pool_for("192.168.1.0/24").iter().any(|p| p.id == "d"),
            "活池也要留着它"
        );
    }

    /// 解除配对**不动中转记忆**（真机反馈第七轮追问：解绑之后也要能直接用 D 发消息）。
    ///
    /// 池与 `viaAddrs` 不是"关系"，是"这一段怎么够得着"的**拓扑事实**：解绑只清 token 与配对
    /// 标记（`AppState::unpair`），记忆里的那台照样拨得动 —— 不必等锚点再在线互换一次视图。
    #[test]
    fn unpairing_keeps_the_relay_memory() {
        let state = memory_state();
        state.note_peer_nets("b", &["192.168.1.0/24".to_string()]);
        state.add_peer_book(PeerDevice {
            id: "b".into(),
            name: "B".into(),
            ip: "192.168.1.6".into(),
            port: 3890,
            online: true,
            ..Default::default()
        });
        state.note_peer_nets("c", &["192.168.50.0/24".to_string()]);
        state.add_peer_book(PeerDevice {
            id: "c".into(),
            name: "C".into(),
            ip: "192.168.50.7".into(),
            port: 3890,
            online: true,
            paired: true,
            ..Default::default()
        });
        let devices = vec![
            crate::relay::RouteDevice {
                id: "c".into(),
                name: "C".into(),
                ip: "192.168.50.7".into(),
                port: 3890,
            },
            crate::relay::RouteDevice {
                id: "d".into(),
                name: "D".into(),
                ip: "192.168.50.8".into(),
                port: 3890,
            },
        ];
        state.absorb_relay_report("b", "192.168.50.0/24", &devices);
        state.note_relay_success("b", "c");

        // 解除与锚点（C）与目标（B）的配对。
        state.unpair("c");
        state.unpair("b");

        let route = state
            .config
            .lock()
            .relay_route("192.168.1.0/24")
            .cloned()
            .expect("解绑不该动策略");
        assert_eq!(
            route.pool_ids(),
            vec!["c".to_string(), "d".to_string()],
            "池的成员原样保留"
        );
        assert_eq!(
            route.pool_addr("d").map(|d| d.ip.as_str()),
            Some("192.168.50.8"),
            "D 的最后已知地址要留着：解绑之后直接拨它"
        );
        assert!(
            state
                .pool_for("192.168.1.0/24")
                .iter()
                .any(|p| p.id == "d" && !p.ip.is_empty()),
            "活池同样保留（candidate 的地址来源之一）"
        );
    }

    /// 池的成员上限（[`MAX_POOL_ENTRIES`]）：超了先淘汰"不在这份新快照里、且最久没露面"的，
    /// 新鲜的那几台一台都不能丢。
    #[test]
    fn pool_is_capped_and_keeps_the_fresh_ones() {
        let state = memory_state();
        state.note_peer_nets("b", &["192.168.1.0/24".to_string()]);
        state.add_peer_book(PeerDevice {
            id: "b".into(),
            name: "B".into(),
            ip: "192.168.1.6".into(),
            port: 3890,
            online: true,
            ..Default::default()
        });
        let device = |id: &str, last: u8| crate::relay::RouteDevice {
            id: id.to_string(),
            name: id.to_uppercase(),
            ip: format!("192.168.50.{last}"),
            port: 3890,
        };
        // 先塞满上限（64 台）→ 再来 8 台**全新**的：总 72，超了 8 台，淘汰的只能是"最久没露面"的
        // 老的那一批里最老的 8 台。
        let old: Vec<_> = (1..=64)
            .map(|i| device(&format!("old-{i:02}"), i))
            .collect();
        state.absorb_relay_report("b", "192.168.50.0/24", &old);
        let fresh: Vec<_> = (1..=8).map(|i| device(&format!("new-{i:02}"), 100 + i)).collect();
        state.absorb_relay_report("b", "192.168.50.0/24", &fresh);

        let ids: Vec<String> = state
            .pool_for("192.168.1.0/24")
            .into_iter()
            .map(|p| p.id)
            .collect();
        assert_eq!(ids.len(), MAX_POOL_ENTRIES, "封顶就是封顶：{ids:?}");
        assert!(
            fresh.iter().all(|d| ids.contains(&d.id)),
            "这一份里的那几台一台都不能丢（它们是最新被报过的）：{ids:?}"
        );
        assert_eq!(
            ids.iter().filter(|id| id.starts_with("old-")).count(),
            MAX_POOL_ENTRIES - fresh.len(),
            "剩下的位置留给老的那一批（它们只是这一份里没露面）"
        );
    }

    /// 上限只数"允许中转"的设备：段里设备再多，没开 S-09 的一台都不进池。
    #[test]
    fn pool_ignores_devices_that_do_not_relay() {
        let state = memory_state();
        state
            .config
            .lock()
            .relay_route_mut("192.168.1.0/24")
            .via_cidr = "192.168.50.0/24".into();
        let entry = |id: &str, last: u8, relay: bool| DiscoveredPeer {
            id: id.to_string(),
            name: id.to_uppercase(),
            ip: format!("192.168.50.{last}"),
            port: 3890,
            cidr: Some("192.168.50.0/24".into()),
            relay,
        };
        let mut entries: Vec<DiscoveredPeer> = (1..=200)
            .map(|i| entry(&format!("no-relay-{i:03}"), (i % 250) as u8 + 1, false))
            .collect();
        entries.push(entry("the-only-one", 9, true));
        state.absorb_view(
            "c",
            &["192.168.50.0/24".into()],
            false, // 锚点自己没开：它自己也不进池
            &entry("c", 7, false),
            &entries,
        );
        let ids: Vec<String> = state
            .pool_for("192.168.1.0/24")
            .into_iter()
            .map(|p| p.id)
            .collect();
        assert_eq!(
            ids,
            vec!["the-only-one".to_string()],
            "段里 200 台设备只有一台允许中转 → 池里就一台（上限 64 根本碰不到）"
        );
    }

    /// 设备属于哪一段：对端**自报**优先（跨网段只有它能说），没有自报才退回本机掩码。
    #[test]
    fn cidr_of_prefers_the_peers_own_report() {
        let state = memory_state();
        let foreign = "192.168.33.7"; // 本机网卡上不会有这一段
        assert!(
            state.cidr_of("x", foreign).is_none(),
            "既没自报、也不在本机任何网段 → 只能记设备级"
        );
        state.note_peer_nets("x", &["192.168.33.0/24".to_string()]);
        assert_eq!(state.cidr_of("x", foreign).as_deref(), Some("192.168.33.0/24"));
        // 自报的段必须**覆盖它自己的地址**才算数（乱报的段不该被采纳）。
        state.note_peer_nets("y", &["10.0.0.0/8".to_string()]);
        assert!(state.cidr_of("y", foreign).is_none(), "自报的段盖不住这个地址 → 不算");

        // 同段（本机网卡）那一档：不需要自报，按本机掩码算。
        if let Some(ip) = crate::server::local_ipv4_addresses().first() {
            let expected = crate::subnet::local_net_of(ip.parse().unwrap()).map(|c| c.to_cidr_string());
            assert_eq!(state.cidr_of("z", ip), expected);
        }
    }

    /// F-107（回归）：反向记录**不许改写地址簿里已有的行**。
    ///
    /// 正常双向流程就会踩到：A 把「AB 段经 C 中转」推给 B，B 再按 §10 反向推回同一份记录，
    /// 而那条记录的中转设备名单里就含着 C。要是照单全收地 `upsert_peer`（它是覆盖语义），
    /// A 地址簿里 C 那一行会被抹成"空名字 + 对端报的地址 + 未配对"，还会落盘。
    #[test]
    fn route_record_never_rewrites_known_rows() {
        let state = memory_state();
        let Some(net) = crate::subnet::host_nets().first().map(|c| c.to_cidr_string()) else {
            return; // 没有非回环网卡：这条判断无从谈起
        };
        state.add_peer_book(PeerDevice {
            id: "peer-c".into(),
            name: "书房的台式机".into(),
            ip: "10.9.9.9".into(),
            port: 3890,
            online: true,
            paired: true,
            ..Default::default()
        });
        let records = vec![crate::relay::RouteRecord {
            target_cidr: net.clone(),
            via_cidr: "10.44.0.0/16".into(),
            via_devices: vec![
                // 谎报：名字空、地址与端口都换掉、还指望本机当成"未配对"
                crate::relay::RouteDevice {
                    id: "peer-c".into(),
                    name: String::new(),
                    ip: "1.2.3.4".into(),
                    port: 9,
                },
                crate::relay::RouteDevice {
                    id: "peer-new".into(),
                    name: "NEW".into(),
                    ip: "10.44.0.8".into(),
                    port: 3890,
                },
            ],
            via_paired: vec!["peer-c".into()],
            confirmed_at: None,
        }];
        assert_eq!(state.apply_relay_routes("peer-a", &records).0, vec![net.clone()]);

        let row = state.get_peer("peer-c").expect("已有的行还在");
        assert_eq!(row.name, "书房的台式机", "名字不能被对方的空名字抹掉");
        assert_eq!(row.ip, "10.9.9.9", "地址不能按对方报的改写");
        assert!(row.paired, "配对状态不能被写成未配对");
        let booked = state
            .config
            .lock()
            .peers
            .iter()
            .find(|p| p.id == "peer-c")
            .cloned()
            .expect("地址簿里那一行还在");
        assert_eq!(booked.name, "书房的台式机", "落盘的名字也不能被改写");
        assert_eq!(booked.ip, "10.9.9.9");
        assert!(booked.paired, "落盘的配对状态也不能被改写");

        // 本机还不认识的 id 照旧并进来，并归 `adopted` 那一档（有 TTL、不进探活队列）。
        assert!(state.get_peer("peer-new").is_some());
        assert!(state.is_adopted("peer-new"));
        assert!(!state.is_adopted("peer-c"));
        // 记录说的那一段要补到新 id 上（它不会自报）；已有行不被这一层改。
        assert_eq!(
            state.cidr_of("peer-new", "10.44.0.8").as_deref(),
            Some("10.44.0.0/16")
        );
    }

    /// 回归：**占位名能被担保人的转述换成真名**。
    ///
    /// `observe_verified_addr`（中转控制面核实过地址）建行时拿设备 id 顶着名字，而这类
    /// 设备本机 mDNS 看不见（跨段，正是它走中转的原因）—— 表现就是"设备列表里那台设备
    /// 显示的是 deviceid"。此前 `learn_peers` 对"本机自己的行"一律 `continue`，真名来了
    /// 也换不掉。
    #[test]
    fn learned_view_fills_a_placeholder_name() {
        let state = memory_state();
        state.observe_verified_addr("dev-hw", "10.20.30.40", 3890);
        assert_eq!(
            state.get_peer("dev-hw").unwrap().name,
            "dev-hw",
            "前提：核实地址建出的行拿 id 顶着名字"
        );

        state.learn_peers(
            "voucher",
            vec![PeerDevice {
                id: "dev-hw".into(),
                name: "华为 Mate60".into(),
                ip: "10.20.30.40".into(),
                port: 3890,
                online: true,
                ..Default::default()
            }],
        );

        let row = state.get_peer("dev-hw").expect("本机自己的行还在");
        assert_eq!(row.name, "华为 Mate60", "占位名要换成担保人报来的真名");
        assert!(!state.is_learned("dev-hw"), "它仍是本机自己的行，不归学来那一档");
        assert_eq!(row.ip, "10.20.30.40", "地址不因这次转述改写");
    }

    /// 上一条的反面：**真名一个字都不动**。转述改不了本机已经知道的名字（可能是用户
    /// 手改的，也可能是对端换名之前本机存下的）。
    #[test]
    fn learned_view_never_rewrites_a_real_name() {
        let state = memory_state();
        state.add_peer_book(PeerDevice {
            id: "dev-c".into(),
            name: "书房的台式机".into(),
            ip: "10.9.9.9".into(),
            port: 3890,
            online: true,
            ..Default::default()
        });
        state.learn_peers(
            "voucher",
            vec![PeerDevice {
                id: "dev-c".into(),
                name: "别人叫它 C".into(),
                ip: "10.9.9.9".into(),
                port: 3890,
                online: true,
                ..Default::default()
            }],
        );
        assert_eq!(
            state.get_peer("dev-c").unwrap().name,
            "书房的台式机",
            "真名不能被对端的转述改写"
        );
    }

    /// `fill_peer_name` 的边界：只填占位名（空串 / 等于 id），真名原样不动；报来的名字
    /// 若本身就是占位（空串 / 等于 id）也不算真名。
    #[test]
    fn fill_peer_name_only_fills_placeholders() {
        let state = memory_state();
        state.observe_verified_addr("dev-p", "10.0.0.7", 3890);
        state.fill_peer_name("dev-p", "PC-P");
        assert_eq!(state.get_peer("dev-p").unwrap().name, "PC-P");
        state.fill_peer_name("dev-p", "另一个名字");
        assert_eq!(
            state.get_peer("dev-p").unwrap().name,
            "PC-P",
            "已经有真名：观察值不覆盖"
        );
        state.observe_verified_addr("dev-q", "10.0.0.8", 3890);
        state.fill_peer_name("dev-q", "dev-q");
        assert_eq!(state.get_peer("dev-q").unwrap().name, "dev-q", "等于 id = 占位，不写");
        state.fill_peer_name("dev-q", "   ");
        assert_eq!(state.get_peer("dev-q").unwrap().name, "dev-q", "空串 = 占位，不写");
    }

    /// 记忆的退休：直连**连续** 3 拍可达才清（中间抖一拍就重新数）。
    #[test]
    fn memory_retires_when_direct_is_stable() {
        let state = memory_state();
        state
            .config
            .lock()
            .set_relay_link("b", "c", "192.168.50.0/24", "2026-09-30T12:00:00Z");

        state.note_direct_probe("b", true);
        state.note_direct_probe("b", false); // 抖动
        state.note_direct_probe("b", true);
        state.note_direct_probe("b", true);
        assert!(
            state.config.lock().relay_link("b").is_some(),
            "两拍还不够：一次抖动之后的计数要重新数，不该把记忆抹掉"
        );
        state.note_direct_probe("b", true);
        assert!(
            state.config.lock().relay_link("b").is_none(),
            "连续 3 拍直连可达 → 设备级记忆退休"
        );
    }

    /// 记忆的唯一写入点：**经中转真的够着了目标** → 设备级链路 + 提升成网段级策略（带池）。
    #[test]
    fn relay_success_promotes_the_device_link_into_a_subnet_route() {
        let state = memory_state();
        // 目标段由**它自报**（跨网段时这是唯一来源：本机的掩码对远端网段不适用）。
        state.note_peer_nets("b", &["192.168.1.0/24".to_string()]);
        state.add_peer_book(PeerDevice {
            id: "b".into(),
            name: "B".into(),
            ip: "192.168.1.6".into(),
            port: 3890,
            online: true,
            ..Default::default()
        });
        // 中转段里两台允许中转的设备（锚点 C 与同段的 D）。
        state.note_peer_nets("c", &["192.168.50.0/24".to_string()]);
        state.add_peer_book(PeerDevice {
            id: "c".into(),
            name: "C".into(),
            ip: "192.168.50.7".into(),
            port: 3890,
            online: true,
            paired: true,
            ..Default::default()
        });
        state.add_peer_book(PeerDevice {
            id: "d".into(),
            name: "D".into(),
            ip: "192.168.50.8".into(),
            port: 3890,
            online: true,
            ..Default::default()
        });
        // 先说清期望的那一档：这条策略**已经存在**（上一次成功写下的），所以这一次刷新
        // 会把"中转方那一段里允许中转的设备"记到这条策略的目标段上 —— 池不再是空的。
        state
            .config
            .lock()
            .relay_route_mut("192.168.1.0/24")
            .via_cidr = "192.168.50.0/24".into();
        let voucher = discovered("c", "192.168.50.7", None, true);
        state.absorb_view(
            "c",
            &["192.168.50.0/24".into()],
            true,
            &voucher,
            &[discovered("d", "192.168.50.8", Some("192.168.50.0/24"), true)],
        );

        state.note_relay_success("b", "c");

        let route = state
            .config
            .lock()
            .relay_route("192.168.1.0/24")
            .cloned()
            .expect("网段级策略已落盘");
        assert_eq!(route.via_cidr, "192.168.50.0/24");
        assert_eq!(
            route.via_devices,
            vec!["c".to_string(), "d".to_string()],
            "池 = 那一段里允许中转的设备（含锚点自己）"
        );
        assert_eq!(route.via_paired, vec!["c".to_string()]);
        assert_eq!(
            route
                .via_addrs
                .iter()
                .map(|d| (d.id.as_str(), d.ip.as_str(), d.port))
                .collect::<Vec<_>>(),
            vec![("c", "192.168.50.7", 3890), ("d", "192.168.50.8", 3890)],
            "池里那些设备的地址要一起记 —— 锚点离散后 id 是拨不动的（真机就卡在这里）"
        );
        assert_eq!(route.ok_count, 1, "一次成功即写（不做 N 次才信）");
        let link = state.config.lock().relay_link("b").cloned().expect("设备级链路已落盘");
        assert_eq!(link.via_id, "c");
        assert_eq!(link.via_cidr, "192.168.50.0/24");

        // 「记忆」的意义就是跨重启：从配置文件重新读一遍。
        let back = crate::config::load_config_at(&state.config_path);
        assert_eq!(
            back.relay_route("192.168.1.0/24").map(|r| r.via_cidr.clone()),
            Some("192.168.50.0/24".into())
        );
        assert_eq!(back.relay_links.get("b").map(|l| l.via_id.clone()), Some("c".into()));

        // 关掉 S-10 就什么都不记（完全退回 F-106）。
        state.clear_relay_memory(None);
        state.config.lock().relay_memory = false;
        state.note_relay_success("b", "c");
        assert!(
            state.config.lock().relay_route("192.168.1.0/24").is_none(),
            "S-10 关着时不该写记忆"
        );
    }

    /// 反向记录：**目标段必须含本机地址**，否则丢弃（一条与本机无关的路由没有意义）。
    #[test]
    fn route_record_rejected_when_target_cidr_excludes_self() {
        let state = memory_state();
        // 本机与 peer-c 是配对的（池里的 `via_paired` 记的是**本机自己的**配对状态，
        // 不是发送方说的那一份）。
        state.mark_paired(&PeerDevice {
            id: "peer-c".into(),
            name: "C".into(),
            ip: "10.0.0.8".into(),
            port: 3890,
            online: true,
            paired: true,
            ..Default::default()
        });
        let Some(net) = crate::subnet::host_nets().first().map(|c| c.to_cidr_string()) else {
            return; // 没有任何非回环网卡的机器：这条判断无从谈起
        };
        let device = |id: &str| crate::relay::RouteDevice {
            id: id.into(),
            name: id.to_uppercase(),
            ip: "10.0.0.9".into(),
            port: 3890,
        };
        let records = vec![
            crate::relay::RouteRecord {
                target_cidr: net.clone(),
                via_cidr: "10.0.0.0/8".into(),
                via_devices: vec![device("peer-c"), device("peer-d")],
                via_paired: vec!["peer-c".into()],
                confirmed_at: None,
            },
            crate::relay::RouteRecord {
                target_cidr: "203.0.113.0/24".into(), // TEST-NET：与本机无关
                via_cidr: "10.0.0.0/8".into(),
                via_devices: vec![device("peer-e")],
                via_paired: vec![],
                confirmed_at: None,
            },
            crate::relay::RouteRecord {
                target_cidr: net.clone(),
                via_cidr: "10.0.0.0/8".into(),
                via_devices: vec![], // 没有可拨的设备：没用
                via_paired: vec![],
                confirmed_at: None,
            },
        ];
        let (accepted, ignored) = state.apply_relay_routes("peer-a", &records);
        assert_eq!(accepted, vec![net.clone()], "只有含本机地址、且有设备的那一条被采纳");
        assert_eq!(ignored.len(), 2);
        let route = state.config.lock().relay_route(&net).cloned().expect("策略已落库");
        assert_eq!(route.via_devices, vec!["peer-c".to_string(), "peer-d".to_string()]);
        assert_eq!(route.via_paired, vec!["peer-c".to_string()], "只记本机**确实**配对过的那台");
        // 池里的设备并进了设备列表（内存行），但**不写地址簿** —— 那是别人的段，不是本机的关系。
        assert!(state.get_peer("peer-d").is_some(), "不先认识地址就拨不出去");
        assert!(
            !state.config.lock().peers.iter().any(|p| p.id == "peer-d"),
            "反向记录不该往地址簿里塞设备（peer-c 在地址簿里是本用例自己配的对）"
        );
    }

    fn identity(id: &str, paired_as: Option<&str>) -> PeerIdentity {
        PeerIdentity {
            id: id.to_string(),
            name: "PC-B".to_string(),
            paired_as: paired_as.map(str::to_string),
            relay: false,
            nets: Vec::new(),
            port: None,
        }
    }

    /// 探活的三条路：答话的是它 / 答话的是别人 / 没人答话。
    #[test]
    fn probe_step_only_calls_it_online_when_the_id_matches() {
        assert!(matches!(probe_step("me", None), ProbeStep::Offline));
        assert!(matches!(
            probe_step("me", Some(identity("me", None))),
            ProbeStep::Online
        ));
        // 问不出 id（老版本/代理）时不把行判死
        assert!(matches!(
            probe_step("me", Some(identity("", None))),
            ProbeStep::Online
        ));
        // 同一个地址、另一个 id：这一行不是它了
        match probe_step("old", Some(identity("new", None))) {
            ProbeStep::OtherIdentity(seen) => assert_eq!(seen.id, "new"),
            other => panic!("应判为「地址上是另一台设备」，得到 {other:?}"),
        }
    }

    /// "这枚 token 它还认不认"只读一条：id 对得上 + `pairedAs` 有没有值。
    ///
    /// 这条判定的后果很实：`Dead` 会把设备行标成「需配对」，并让对端发来的配对邀请
    /// 放行到通知里。所以"没问到""地址上是别人"都必须是 `Unknown`，不能误判成 `Dead`
    /// —— 那会让一台好设备凭空变成"关系已失效"。
    #[test]
    fn token_liveness_needs_a_matching_id_and_a_verdict() {
        // 它认这枚 token：配对还活着
        assert_eq!(token_liveness(Some(&identity("me", Some("me"))), "me"), TokenLiveness::Alive);
        // 它答话了、id 也对，但不认这枚 token
        assert_eq!(token_liveness(Some(&identity("me", None)), "me"), TokenLiveness::Dead);
        // 没问到（超时 / 没答话）
        assert_eq!(token_liveness(None, "me"), TokenLiveness::Unknown);
        // 地址上换了设备：那条路由 `apply_identity_reconcile` 走，不是"关系失效"
        assert_eq!(
            token_liveness(Some(&identity("new", None)), "me"),
            TokenLiveness::Unknown
        );
        // id 问不出来（老版本 / 中间有代理）同样不下结论
        assert_eq!(token_liveness(Some(&identity("", None)), "me"), TokenLiveness::Unknown);
    }

    /// 替别人转发的消息字节进「中转流量」，但**不算一条连接** —— 那一格数的是盲转端点。
    #[test]
    fn relay_message_bytes_accumulate_without_counting_a_connection() {
        let state = memory_state();
        state.note_relay_message(120);
        state.note_relay_message(80);
        let json = state.relay_traffic_json();
        assert_eq!(json["bytes"], 200);
        assert_eq!(json["connections"], 0, "消息中转不是一条连接");
        // 落了盘：`config.toml` 里就是重启之后接着算的那两个数。
        {
            let cfg = state.config.lock();
            assert_eq!(cfg.relay_bytes_total, 200);
            assert_eq!(cfg.relay_conns_total, 0);
        }
        // 空转（0 字节）不该动计数。
        state.note_relay_message(0);
        assert_eq!(state.relay_traffic_json()["bytes"], 200);
    }

    /// 盲转与消息两条来路合流到同一个字节数上：一边一条连接、一边两条消息。
    #[test]
    fn relay_traffic_merges_connection_and_message_bytes() {
        let state = memory_state();
        state.note_relay_traffic(1000);
        state.note_relay_message(300);
        let json = state.relay_traffic_json();
        assert_eq!(json["bytes"], 1300);
        assert_eq!(json["connections"], 1, "只有盲转那条算连接");
    }
}