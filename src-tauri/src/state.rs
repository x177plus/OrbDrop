use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU16, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::http::StatusCode;
use futures_util::StreamExt;
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
    /// 当前挂着的 mDNS 注册（daemon 句柄 + 它报的服务全名）。换名字/端口/在线状态时
    /// **必须整体换掉并释放旧的那一份**（见 `discover::release`）：只丢句柄不注销，对端会
    /// 一直解析到旧记录 —— 用户改了设备名，别人的设备行还是旧名。
    pub mdns_reg: Mutex<Option<crate::discover::Registration>>,
    /// WebSocket manager (peer links).
    pub ws: WsManager,
    /// Push jobs (sender + receiver) and resume store.
    pub push: PushStore,
    /// 拖拽下载的票据表（暂存副本 / 已备好的本机路径）。
    pub drag: crate::drag::DragStore,
    /// Pairing code store.
    pub pair: PairStore,
    /// 长连接请求（F-16 的同意制）：进来的等本机用户点头，出去的等对端。
    pub link: crate::link::LinkStore,
    /// 会话（聊天）记录：文本消息的落盘 + 界面快照（`chat-vault/chat.dat`）。
    pub chat: crate::chat::ChatStore,
    /// 群聊（共享群）：名册 + 群消息（`chat-vault/groups.dat`）。
    ///
    /// 与一对一账本**分开一个文件**：老版本进程读 `chat.json`（serde 静默忽略不认识的
    /// 顶层键）再原样写回，会把新增的顶层键抹掉 —— 名册与群消息丢了不可恢复。
    pub groups: crate::group::GroupStore,
    /// 会话快照的版本号（[`crate::group::snapshot`] 每造一份 +1）。
    ///
    /// 用途只有一个：**让界面能认出"这是旧的那一份"**。命令的返回（`chat_send` 等）与
    /// `chat://changed` 事件是两条独立通道（Windows 上一个是 IPC 的 HTTP 响应、一个是
    /// `eval` 进来的脚本，谁先到没有保证），所以"发出去那一刻还是 `Sending` 的那份返回"
    /// 完全可能晚于"后台 HTTP 那一跳落定后推的 `Sent`"到达。界面按这个号丢掉旧的那份，
    /// 就不会把已经送达的消息又显示回「发送中」。
    pub chat_rev: AtomicU64,
    /// Tauri handle for emitting UI events.
    pub app_handle: Mutex<Option<AppHandle>>,
    /// File watchers keyed by share id（推送批次的源变化监视用 `push:<batch_id>`，见
    /// `transfer::watch` —— 两类键不会撞：共享项那个是 UUID）。
    pub watchers: Mutex<std::collections::HashMap<String, notify::RecommendedWatcher>>,
    /// 「浏览」里正在跑的手动下载的暂停闸（键见 `transfer::browse_dl::key`）。
    ///
    /// 只有**还在跑**的命令在表里：命令一开始登记、结束（成功/失败）自己摘掉。
    /// 不落盘 —— 进程重启后那些命令本来就不存在了。
    pub browse_downloads:
        Mutex<HashMap<String, Arc<crate::transfer::browse_dl::DownloadControl>>>,
    /// 会话里**正在接收**的推送的暂停闸（键 = `inviteId`）。
    ///
    /// 与 `browse_downloads` 同一套 [`crate::transfer::browse_dl::DownloadControl`]：界面点那条
    /// 文件气泡右侧的圆环 = 暂停/继续这条接收，已收字节与 `.orbdrop-part` 都留着。
    /// 任务一开始登记、退出（完成 / 失败 / 取消）时由 `push_job::ReceiveGate` 摘掉；
    /// 同样不落盘（重启后接收任务按续传记录自己接着跑，不该还停在"已暂停"）。
    pub receive_downloads:
        Mutex<HashMap<String, Arc<crate::transfer::browse_dl::DownloadControl>>>,
    /// 「学来的」设备（F-10 跨网段）：对端 `/discover` 表里带过来、而本机自己从没发现过的
    /// 设备 id → 最近一次被提到的时刻。
    ///
    /// 为什么单开一张表而不是塞进 `PeerDevice`：这个状态**只属于本进程**（重启即忘），
    /// 而 `PeerDevice` 是要落盘进地址簿的形状。存在这里的 id，`peers` 里对应那行就是
    /// "临时条目"—— 不写地址簿、不被探活接管、超时由 `sweep_learned` 收走。
    pub learned: Mutex<HashMap<String, Instant>>,
    /// 5s 探活**连续失败**计数（只活在本进程）：未配对的行到
    /// [`PROBE_FAIL_LIMIT_UNPAIRED`] 就整行忘掉，配对的行保留关系、只记一条 debug。
    ///
    /// 为什么要这一档：`knownPeers` 按 device id 存，对端重装/清数据后 id 变了，
    /// 旧 id 若没被身份核对接手，就会一直指着同一个死地址；5s 一拍各拨一次，macOS 的
    /// 256 fd 上限很快被这些无用的 `SYN_SENT` 吃掉（见 docs/macos.md §1.3）。
    probe_failures: Mutex<HashMap<String, u32>>,
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
    /// 「刚听到过它」：某台设备最近一次**从它那边收到东西**的时刻（会话报文、控制面核实）。
    ///
    /// 为什么需要这一档：`online` 原来的唯一判据是"本机拨得动它"（5s 探活成功，或长连接
    /// 活着）—— 而对端**能拨进来**与**本机拨得回去**是两件事。AP 把同段客户端的单播隔开、
    /// 两端跨段、只有一条借来的中转链、或者地址簿里那条地址早就过期时，对端明明在会话里
    /// 说着话（本机一直在收它的消息），却永远画成「离线」：会话列表那一行灰点、设备列表
    /// 还因为"离线 + 未绑定 + 没被 mDNS 看见"把它整行挡在外面。
    ///
    /// 语义边界（[`HEARD_TTL`]）：**"它在网上"**这一条成立，**不**代表"本机够得着它" ——
    /// 地址/端口仍然只由"观察到的来源地址 + 自报端口"与 mDNS 记录决定，推送与配对的门槛
    /// 照旧要求地址（`transfer`/`pair` 都自己再判一次），所以这一档不会让本机往死路上发东西。
    /// 只活在内存里（重启即忘），与 `seen`/`learned` 同一档。
    pub heard: Mutex<HashMap<String, Instant>>,
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
    /// `/relay/route` **未认证来源**的限流表：`来源地址 → (窗口起点, 本窗口已发数)`。
    ///
    /// 只挡没有 Bearer 的调用方：它们可能是合法的"被中转的一侧"（每 30s~5min 才推一次），
    /// 也可能是局域网里随手写一条报文的人。按来源地址而不是自报 id 记账 —— 后者是免费的。
    pub route_push_sources: Mutex<HashMap<String, (Instant, u32)>>,
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
    /// 对端报来的「共享项变更」的**节流**表：`共享项 id → 上次真的向上报的时刻`。
    ///
    /// 为什么需要：对端那条路上一次窗口（300ms）里可能装着**几百条**逐文件事件 —— 实测一台
    /// 机器在共享目录里跑多通道 Gradle 构建，本机一天收到 42 万条、峰值 1300 条/秒。而每一条
    /// 在这里都会变成一次 Tauri 事件，前端每收到一条就把 `refreshSignal` +1 并重拉根层，
    /// 于是展开中的每一层树各发一次 HTTP `/tree` 给对端：1300 条/秒 = 一场请求风暴，
    /// 而对端和本机界面一起被压住。
    ///
    /// 口径是**前缘节流**（不是等安静下来再报）：窗口内第一条立刻上报 —— 正常场景（有人往共享
    /// 目录里丢一个文件）零延迟，这正是要保住的行为；窗口内的重复一律吞掉。**尾沿**那一半
    /// （一轮爆发结束之后还要再刷一次、别停在中间态）由前端自己的防抖负责（`PeerTab.svelte`）。
    ///
    /// 键是**对端给的共享项 id**（未鉴权的载荷）：满了就按时间戳清一遍陈旧的（见
    /// [`AppState::share_changed_due`]），不给它无界增长的机会。
    share_changed_at: Mutex<HashMap<String, Instant>>,
}

/// 同一条共享项的两次上报之间至少隔这么久（见 [`AppState::share_changed_due`]）。
///
/// 取 300ms 与发送端那个攒批窗口（`share::watcher` 的 `QUIET_WINDOW`）同一个量级：
/// 它决定的是"一批能不能合并成一次上报"，比它小就等于不合并，比它大则开始感觉得到延迟。
const SHARE_CHANGED_THROTTLE: Duration = Duration::from_millis(300);

/// 共享项变更节流表最多记多少个 id（键是**对端给的字串**，见 [`AppState::share_changed_due`]）。
///
/// 一台对端真有的共享项是"几十个"量级；256 是给"对端刷 id"那种情况的上界：真刷满了，
/// 新 id 的变化**一律按窗口内的重复处理**（不上报、也不建表项）—— 宁可少刷一次界面，
/// 也不让这张表无界长。
const MAX_SHARE_CHANGED_KEYS: usize = 256;

/// 「刚听到过它」的保留时长（见 [`AppState::heard`]）。
///
/// 取 60s = 12 拍 5s 探活：够长，能盖住"消息之间的停顿"（说完一句、隔半分钟再说一句，
/// 中间那几拍不该把它翻成离线）；够短，早就不在的那一台最迟一分钟就从"在线"落回来
/// （探活那一刻仍会如实把它标离线 —— 这一档只是"别把会说话的设备画成不在网上"）。
const HEARD_TTL: Duration = Duration::from_secs(60);

/// 反向记录带进来的设备行的寿命（长于记录 5 分钟一次的刷新间隔）。
///
/// 太短：记录还没重推，池里的地址就没了（借不到中转）；太长：对端换了一批设备之后旧行赖着
/// 不走。15 分钟 = 三次刷新，够稳。
const ADOPTED_TTL: Duration = Duration::from_secs(900);

/// 反向记录一次最多并进来多少台设备（总量上限；单条记录另有 [`crate::relay::MAX_ROUTE_DEVICES`]）。
///
/// 没有上限时，一次 64MB 的请求体就能往设备表里塞上万行，还得每 5s 逐个探活。
const MAX_ADOPTED_DEVICES: usize = 64;

/// 设备表里**没有归宿**的行最多留多少。"没有归宿" = 既不在 `config.toml` 的地址簿里、
/// 也没配对、也没被 `learned`/`adopted` 那两档 TTL 管着，而且此刻既不可达也没被 mDNS 看见。
///
/// **这个数不管"互换设备列表"**：对端 `/discover` 报来的表进的是 `learned`（20 秒 TTL，跟着
/// 担保人刷新），反向路由记录带进来的进 `adopted`（15 分钟 TTL），两者都在判据里被显式排除，
/// 不由这份预算承担。它要挡的只有一类：**别人能凭一条报文凭空造出来的 id** —— mDNS 公告里一个
/// 没见过的 id（`upsert_peer`），或中转控制面一次自报核实（`observe_verified_addr`，那条路
/// 未鉴权）。不加界时，局域网上一台机器换着 id 喊、或换着 id 请求中转，本机就一行行攒：每行
/// 还要每 5s 探活一次（`PROBE_CONCURRENCY` 是并发上限，不是行的上限），内存与空转一起涨。
///
/// 为什么是 256 而不是更大：**真正的成本不是内存**（一行 `PeerDevice` 约 250 字节，256 行 ≈
/// 64 KB），而是 5s 那一拍探活是 **O(行数)** 的 —— 每台不通的设备最多吃掉 1.5s 的探活超时、
/// 并发只有 8；而同一拍还负责给**所有**对端补发排队消息（`chat::flush_queued`）与推送对账
/// （`transfer::reconcile`）。行数上了几百，"一拍"就会被拖到几十秒，那是拿"给所有设备补发消息
/// 的及时性"换来的。256 已经是一个公司在同一网段的设备量级；真要有几百台跨段、且此刻全都不通
/// 的设备要同时留着，该做的是**按行退避探活**（长不通的那几行拉长间隔），而不是把这里的数
/// 继续调大 —— 那件事的代价是"对端回来之后多久被发现"（跨段时 mDNS 看不见它，只能靠探活）。
///
/// 淘汰的后果只是把它从**列表**里去掉：它下次公告/请求时重新建行，而这一档本来就只活在内存里
/// （不落地址簿，重启一样没有它）—— 收掉它只是把重启会做的事提前了，不是丢用户数据。
/// **不动有归宿的行**：地址簿里的、已配对的、`learned`/`adopted` 管着的、以及此刻在线或
/// 被 mDNS 看见的，一个都不淘汰 —— 淘汰一个正在网上的设备行就是用户眼里"它没了"。
const MAX_UNBOOKED_ROWS: usize = 256;

/// 一台设备最多记几条**自报**网段（`/discover`、`/pair/info` 的 `nets`）。
///
/// `nets` 是对端说的话，而这段列表会**原样存下来**（`note_peer_nets`）跟着进程活很久。用它的
/// 地方只有 `cidr_of`（找覆盖它地址的那一条）—— 一台设备有几块网卡就有几条，8 条已经宽到
/// 离谱。没有上限时，一个回话里塞几十万条就能让这张表涨几十兆。
const MAX_PEER_NETS: usize = 8;

/// 网段归属这张表最多记几台设备的（键的条数，不是每条的长度）。
///
/// 与 [`MAX_UNBOOKED_ROWS`] 同一类问题：键来自"反向路由记录里的设备"（未鉴权的那条路）与
/// 对端自报，而这张表**没有删除路径**。比行预算高出一档（512 > 256）：行都可能留着的那几台，
/// 它们的段更该留着 —— 少了段的后果是"这台设备的段算不出来"（`cidr_of` 本来就允许"都不知道
/// → 退回本机掩码/None"），路由退成设备级，不会把谁的行判错。
///
/// 满了就**不再收养新 id**（已有的一个不动），不是淘汰旧的：这里换掉谁都会让一台**还在用的**
/// 设备丢掉它的段归属，而不收养只是让最新的那台退回"不知道"。
const MAX_PEER_NET_IDS: usize = 512;

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
        merged.sort_by_key(|e| std::cmp::Reverse((e.seen_at.is_some(), e.seen_at.unwrap_or(now))));
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

/// **未认证来源**的路由提示在本进程内的寿命（F-107 §10.3）。
///
/// 这类记录只可能经中转方送来（发送方与接收方没有配对、接收方也认证不了它），因此只能
/// 当提示：不落盘（见 `config::save_config_at`）、不能覆盖已落盘的记忆，并且到期即收。
/// 取 15 分钟 = 三次 5 分钟刷新：发送方在线时一直续得上，掉线/换段之后不会赖着不走。
pub const EPHEMERAL_ROUTE_TTL: Duration = Duration::from_secs(900);

/// `/relay/route` **未认证来源**的限流窗口与额度。
///
/// 合法的发送方最短 30s 一发（`ROUTE_RESEND_MIN`，且只在未确认时），装上 5 分钟才刷新一次；
/// 30 发/分钟足够覆盖"同一 IP 上跑着多个实例"以及锚点池频繁变化时每 5s 一发的极端情况，
/// 也把"一条报文就触发选路/补发"的放大按在可见的量级上（写盘在 `apply_relay_route_hints`
/// 那一档已经收掉了，这里挡的是无谓的选路与事件）。
const ROUTE_PUSH_WINDOW: Duration = Duration::from_secs(60);
const ROUTE_PUSH_MAX_PER_WINDOW: u32 = 30;

/// 锚点长期不再担保（`seen_at` 超过这么久）→ 删掉那条网段级策略（F-107 §11）。
const ROUTE_TTL_DAYS: i64 = 7;

/// 设备级链路记忆的兜底寿命：这么久没再成功过就忘掉它。
const LINK_TTL_DAYS: i64 = 30;

/// 这个名字有没有信息量：空串、或**就是设备 id 本身**，都算"还不知道它叫什么"。
///
/// `AppState::new`（凭据库里有、地址簿里没有的已配对行）与 `observe_verified_addr`
/// （中转控制面核实过的地址）建行时都拿设备 id 顶着名字当占位。这两处之后各有别的路能
/// 补上真名（mDNS / 会话信封 / [`AppState::note_peer_name`] / [`AppState::learn_peers`]），
/// 判据统一在这里。
///
/// 名字这一格的规矩（2026-10-16 起，见 `docs/p1-implementation.md` §42.六）：
/// **占位名（空串 / 等于设备 id）任何一条路都能改写**；**真名只有"核实过的自报"能改**
/// —— [`AppState::note_peer_name`]（探活回话 / 有 token 的会话 / 在它自己的地址上问出来的）
/// 与 `upsert_peer` 收的 mDNS TXT / 会话信封是同一档，所以"对端改名"跟着走；
/// **没核实的主张**（经中转送来的路由记录、邀请里的自称）只填占位行
/// （[`AppState::fill_peer_name`]）。
///
/// 用户自己的命名不在这里：那是**备注**（F-101，另一个字段、另一张表），从不被这些路径碰。
pub(crate) fn name_is_placeholder(name: &str, id: &str) -> bool {
    let n = name.trim();
    n.is_empty() || n == id
}

/// 刚收到的名字该不该写进设备行（`current` = 现有那份，`incoming` = 这次收到的）。
///
/// **只有一条禁令：空名 / 占位名（== id）不许冲掉已有的真名。** 对端某一条报文没带名字
/// （老版本、或那条报文本来就不带 —— 比如一条只报 id + 端口的控制面请求）时，一次无条件
/// 覆盖就把用户看得见的名字换成一串设备 id，地址簿落盘的那一份也跟着一起坏。
/// 真名之间照旧互相覆盖：对端改名走的正是这条路（mDNS TXT、会话信封、配对握手报的都是
/// 它自己），所以不能反过来把名字冻住 —— 那是另一个 bug。
pub(crate) fn should_take_name(current: &str, incoming: &str, id: &str) -> bool {
    !name_is_placeholder(incoming, id) || name_is_placeholder(current, id)
}

/// 设备 id 是对端自报的（mDNS 公告、配对握手、转述、凭据库合成）：形态不合法的行一律不收。
///
/// 入口（`server::routes_pair` / `pair::commit_pairing_with_fingerprint`）已经拦过并给了
/// 明确的错误；这里兜的是**旁路** —— mDNS 的一条公告、经别人转述的一行、凭据库里手改过的
/// 键。坏 id 会被当成镜像目录名与 BC 的 URL 路径段用（见 [`crate::ids`]），不值得赌调用方
/// 都没漏。
pub(crate) fn accepts_peer_id(id: &str) -> bool {
    if crate::ids::is_segment(id) {
        return true;
    }
    warn!(peer_id = %id, "忽略设备标识不合法的设备行");
    false
}

impl AppState {
    pub fn new(mut config: Config, config_path: PathBuf) -> Self {
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
        // 落盘的 `online` **不作为首帧真相**：本机这一趟还一台都没探过，那个值只是"那一行
        // 被创建时的那一刻"的残留 —— `set_peer_online` 从不回写它（见 `PeerDevice::online`）。
        // 拿它当真会同时得到两种假象：真不在网的设备顶着"在线"、真在网的设备先画成离线，
        // 而后者要等第一拍探活才纠正（用户看到的就是"启动后 A 离线、过几秒又在线"）。
        // 一律从「离线」起步，真相只由**启动即跑的第一拍探活**（`lib.rs` 的探活循环）与
        // 链路建立（`ws::restore_links_on_startup` / 入站会话）给出 —— 都在毫秒到几百毫秒这一档。
        for p in peers.iter_mut() {
            p.online = false;
        }
        let tokens = crate::credential::load_peers().unwrap_or_default();
        for (id, _) in tokens.iter() {
            // 凭据库的键也是 id 的来源之一（手改过 / 修复前对端写进来的）：坏 id 不进设备表。
            if id == &own_id || !crate::ids::is_segment(id) {
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
                    auto_connect: false,
                    paused: false,
                    relay: None,
                });
            } else if let Some(p) = peers.iter_mut().find(|p| &p.id == id) {
                p.paired = true;
            }
        }

        // 「以前见过」那一档（`config.known_peers`）：**没有关系**的那些设备也补一行。
        //
        // 为什么必须有这一步：未配对、也不在地址簿的设备行只活在内存里（`upsert_peer` 只在
        // 地址簿里才落盘），重启即忘 —— 那台明明还在会话列表里，却"设备名要等一段时间、
        // 在线也要等一段时间（两件一起出现）"。补上这一行之后：名字**当场就有**（那一格是
        // 上次知道的名字），而可达性由**启动即跑的第一拍探活**给（拨得动就直接拨；一条路都
        // 没有的走"借一次中转"，路线记忆本来也是落盘的）。
        //
        // **名字可以旧、状态必须新**：这里一律 `online: false`（与上面那份落盘值同一条规矩），
        // 真相只由这一趟的证据给。
        //
        // 只补"地址簿与凭据库里都没有的 id"：地址簿里已经有那一行就一个字都不动（内存行 >
        // 地址簿 > 这张表，谁也不能越级覆盖）。顺带清一遍过期的条目（TTL 只在内存里，不值得
        // 为它单独写一次盘 —— 下次真有变化时落盘的就是清过的那一份）。
        config.prune_known_peers();
        config.prune_observed_fingerprints();
        let known_rows: Vec<(String, crate::config::KnownPeer)> = config
            .known_peers
            .iter()
            .map(|(id, k)| (id.clone(), k.clone()))
            .collect();
        for (id, k) in known_rows {
            if id == own_id || !crate::ids::is_segment(&id) || peers.iter().any(|p| p.id == id) {
                continue;
            }
            peers.push(PeerDevice {
                id,
                name: k.name,
                ip: k.ip,
                port: k.port,
                online: false,
                paired: false,
                auto_connect: false,
                paused: false,
                relay: None,
            });
        }

        // 「本机记得上次是怎么够着它的」那一档（设备级链路记忆，F-107 §4.1）也补一行，
        // **哪怕不知道它叫什么、也不知道它在哪**：名字先拿 id 顶着，靠启动第一拍去借
        // 那条记得的路（`probe_peer` → `!dialable()` → `ensure_for` → 端点探到 → 名字与
        // 在线一起回来）。少了这一行，"需要中转的设备"就得**等它自己先开口**才有状态
        // （它的名字/地址本机都没有，除了这条记忆之外没有别的线索）。
        //
        // 只在开着记忆（S-10，默认开）时才会有这些条目 —— 关着就是"每次现开"，那也
        // 意味着没有可恢复的线索，与设计一致。
        for (id, link) in config.relay_links.iter() {
            if id == &own_id || link.via_id.is_empty() || peers.iter().any(|p| &p.id == id) {
                continue;
            }
            peers.push(PeerDevice {
                id: id.clone(),
                name: id.clone(),
                ip: String::new(),
                port: 0,
                online: false,
                paired: false,
                auto_connect: false,
                paused: false,
                relay: None,
            });
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
                crate::ws::LinkState::Stopped {
                    reason: crate::ws::StopReason::LocalDisconnected,
                },
            );
        }

        // 会话记录放在 config.toml **旁边**，不写死全局数据目录：
        // 它和配置一样是"这个实例的用户数据"，实例换了个家（便携部署 / 测试指向临时目录），
        // 会话记录就该跟着走。落盘在 `chat-vault/`（封装格式，见 `envelope`）。
        let data_home = match config_path.parent() {
            Some(dir) => dir.to_path_buf(),
            None => crate::config::data_dir(),
        };
        // 首启/升级：把老版本的 `chat.json` / `groups.json` / `chat-blobs/` 搬进 vault。
        // 幂等；失败时保留旧文件并回退到旧路径，至少不让用户的历史突然变空。
        let migration = crate::envelope::migrate(&data_home);
        let (chat_path, chat_blobs) = if migration.chat_ready {
            (
                crate::envelope::chat_path(&data_home),
                crate::envelope::blobs_dir(&data_home),
            )
        } else {
            (data_home.join("chat.json"), data_home.join("chat-blobs"))
        };

        // 占位名（空串 / 等于设备 id）的行：先用**会话记录里那个真名**补上。
        //
        // 为什么必须在这一刻做：占位名是"还没有名字"的起步值（`AppState::new` 给"只有凭据、
        // 地址簿里没有"的行起名就是 id，中转核实建的行也一样），而填它的路此前全是**观察值**
        // （探活回话 / 它自己发来的报文 / mDNS / 担保人转述）—— 启动那一刻一条都还没发生，
        // 用户看到的就是"设备列表与会话列表先显示一串 deviceid，过一会儿才变成名字"。
        // 而这个名字本机**早就知道过**：对端自己发来的消息带着它当时的设备名，落盘在
        // 会话记录里、跨重启还在。没有理由等第二次观察。
        //
        // 只补占位名：地址簿里那份真名一个字都不动（那可能是**配对握手**报的、也可能是用户手动
        // 添加时的观察值）。也**不落盘**：每次启动都从记录里重新取，配置文件里不因此多一份
        // 会漂移的副本。方法论上它只是"本机已经有过的观察"，所以之后任何一条**核实过的**
        // 自报名字（探活回话 / 有 token 的会话）都能改写它（`note_peer_name`）—— 记录里的
        // 名字不会因此变成"改不掉的旧名字"。
        let chat = crate::chat::ChatStore::with_blobs(chat_path, chat_blobs);
        {
            let known = chat.last_inbound_names();
            for p in peers.iter_mut() {
                if !name_is_placeholder(&p.name, &p.id) {
                    continue;
                }
                if let Some(name) = known.get(&p.id).filter(|n| !name_is_placeholder(n, &p.id)) {
                    p.name = name.clone();
                }
            }
        }
        // 群聊与它同一个家、同一个理由：都是一个实例自己的用户数据。
        let (group_path, group_blobs) = if migration.groups_ready {
            (
                crate::envelope::groups_path(&data_home),
                crate::envelope::blobs_dir(&data_home),
            )
        } else {
            (data_home.join("groups.json"), data_home.join("chat-blobs"))
        };

        // 中转流量是"累计记录"：从落盘的总量接着算，而不是每次启动归零。
        let (relay_bytes0, relay_conns0) = (config.relay_bytes_total, config.relay_conns_total);

        // 旧版本可能已经把伪造回执留下的幽灵 id / `u64::MAX` 写进了 `groups.dat`：
        // 加载后先洗一遍，否则那些水位会继续把新消息算成「已读」。
        let groups = crate::group::GroupStore::with_blobs(group_path, group_blobs);
        groups.sanitize_read_by(&own_id);

        let state = Self {
            config_path,
            config: Mutex::new(config),
            peers: Mutex::new(peers),
            server_port: AtomicU16::new(0),
            server_shutdown: Mutex::new(None),
            server_task: Mutex::new(None),
            mdns_reg: Mutex::new(None),
            ws,
            push: PushStore::new(),
            drag: crate::drag::DragStore::new(),
            pair: PairStore::new(),
            link: crate::link::LinkStore::new(),
            chat,
            groups,
            chat_rev: AtomicU64::new(0),
            app_handle: Mutex::new(None),
            watchers: Mutex::new(std::collections::HashMap::new()),
            browse_downloads: Mutex::new(HashMap::new()),
            receive_downloads: Mutex::new(HashMap::new()),
            learned: Mutex::new(HashMap::new()),
            relays: crate::relay::RelayRegistry::default(),
            relay_bytes: AtomicU64::new(relay_bytes0),
            relay_conns: AtomicU64::new(relay_conns0),
            relay_capable: Mutex::new(HashSet::new()),
            relay_attempts: Mutex::new(HashMap::new()),
            seen: Mutex::new(HashMap::new()),
            heard: Mutex::new(HashMap::new()),
            peer_nets: Mutex::new(HashMap::new()),
            relay_pool: Mutex::new(HashMap::new()),
            direct_streak: Mutex::new(HashMap::new()),
            route_push: Mutex::new(HashMap::new()),
            route_push_sources: Mutex::new(HashMap::new()),
            relay_failed: Mutex::new(HashMap::new()),
            adopted: Mutex::new(HashMap::new()),
            share_changed_at: Mutex::new(HashMap::new()),
            probe_failures: Mutex::new(HashMap::new()),
        };
        // 启动时先合并同一地址上的重复未配对行（knownPeers 按 device id 存，对端换过 id
        // 就会留下多条指向同一格的死行 —— 每 5s 各拨一次）。
        state.dedupe_known_addresses();
        state
    }

    // ------------------------------------------------------------------
    // Peer management helpers
    // ------------------------------------------------------------------

    /// Upsert a peer into the in-memory list. Persists the address book
    /// entry (config.peers) if the peer is paired or was manually added.
    pub fn upsert_peer(&self, peer: &PeerDevice) {
        if !accepts_peer_id(&peer.id) {
            return;
        }
        let mut created = false;
        let mut address_changed = false;
        {
            let mut peers = self.peers.lock();
            if let Some(existing) = peers.iter_mut().find(|p| p.id == peer.id) {
                // 名字走 `should_take_name`：空名/占位名不许冲掉真名（见那一条的说明）。
                if should_take_name(&existing.name, &peer.name, &peer.id) {
                    existing.name = peer.name.clone();
                }
                if !peer.ip.is_empty() && existing.ip != peer.ip {
                    existing.ip = peer.ip.clone();
                    address_changed = true;
                }
                if peer.port != 0 && existing.port != peer.port {
                    existing.port = peer.port;
                    address_changed = true;
                }
                existing.online = existing.online || peer.online;
                existing.paired = existing.paired || peer.paired;
            } else {
                peers.push(peer.clone());
                created = true;
            }
        }
        // 只有**新行**才过一遍预算：改一行早就存在的不会让表变长，而这一步要读一次凭据库，
        // mDNS 的重复公告是常事，别让它每拍都花（见 [`MAX_UNBOOKED_ROWS`]）。
        if created {
            self.enforce_row_budget();
        }

        // Persist if in the address book.
        let mut config = self.config.lock();
        if config.peers.iter().any(|p| p.id == peer.id) {
            if let Some(p) = config.peers.iter_mut().find(|p| p.id == peer.id) {
                if should_take_name(&p.name, &peer.name, &peer.id) {
                    p.name = peer.name.clone();
                }
                if !peer.ip.is_empty() && p.ip != peer.ip {
                    p.ip = peer.ip.clone();
                    address_changed = true;
                }
                if peer.port != 0 && p.port != peer.port {
                    p.port = peer.port;
                    address_changed = true;
                }
                p.paired = peer.paired;
            }
            let _ = crate::config::save_config_at(&self.config_path, &config);
        }
        drop(config);
        // 地址变了：`Trust` 是按 IP 存的快照，不重灌的话新地址就没有 pin，TLS 会按
        // TOFU 接受任意证书 —— 下一步探活/会话就会把 token 发出去。这里重建一次，
        // 让新地址拿到的仍是**凭据库里这台设备的指纹**（mDNS 投毒因此会在 TLS 握手失败）。
        if address_changed {
            self.refresh_trust_from_credentials();
        }
        // 没有关系的那一档（未配对、也不在地址簿）单独记一份：它只活在内存里，
        // 不记下来重启之后那一行连名字都没有（见 [`KnownPeer`]）。
        self.note_known_peer(&peer.id);
        self.emit_devices_changed();
    }

    /// Add a peer to the address book (manual add) and persist.
    ///
    /// 已经在列表里的设备**也要把地址补上**，不能因为"这一行已经在了"就整条丢掉：
    /// 凭据库里有 token、却没地址的设备会先被 `AppState::new` 合成一行（ip 空、端口 0），
    /// 于是"添加设备"填的地址会被静默忽略 —— 之后配对学到的地址也是空的，长连接
    /// 永远拨不出去（表现为设备行显示 `:0`）。
    pub fn add_peer_book(&self, peer: PeerDevice) {
        if !accepts_peer_id(&peer.id) {
            return;
        }
        let peer_id = peer.id.clone();
        {
            let mut peers = self.peers.lock();
            match peers.iter_mut().find(|p| p.id == peer.id) {
                Some(p) => {
                    if should_take_name(&p.name, &peer.name, &peer.id) {
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
                    if should_take_name(&p.name, &peer.name, &peer.id) {
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
            // **互斥**：关系接管了这一格，观察记忆那份删掉（两张表永不同时持有一个 id）。
            config.forget_known_peer(&peer_id);
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
        let mut created = false;
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
                    created = true;
                    true
                }
            }
        };
        if !changed {
            return;
        }
        // 这条路是**未鉴权**的（`/relay/open` 的自报核实、`/relay/route` 的自报），设备 id
        // 由请求方给 —— 建出来的行一定要过预算，否则换着 id 请求就能把设备表撑爆
        // （见 [`MAX_UNBOOKED_ROWS`]）。
        if created {
            self.enforce_row_budget();
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
        // 没有关系的那一档也记一份（地址+名字）：它重启即忘，记下来启动才有东西可拨。
        self.note_known_peer(peer_id);
        self.emit_devices_changed();
    }

    /// 这台未配对设备**已观察**的证书指纹（没有 = `None`）。
    pub fn observed_fingerprint(&self, peer_id: &str) -> Option<String> {
        self.config
            .lock()
            .observed_fingerprints
            .get(peer_id)
            .cloned()
    }

    /// 外来请求出示的客户端证书，是否就是本机此前观察到的那一枚。
    ///
    /// 这是**未配对**设备地址变更的连续性判据：证书相同 = 对面还持有同一把私钥
    /// （TLS 握手会验签，不是只看自报值）；证书不同/没有 = 不认这次地址变更。
    /// 已配对的那一档走 token + 凭据指纹，不读这里。
    pub fn observed_fingerprint_matches(&self, peer_id: &str, fingerprint: Option<&str>) -> bool {
        let Some(fingerprint) = fingerprint.filter(|fp| !fp.is_empty()) else {
            return false;
        };
        self.config
            .lock()
            .observed_fingerprints
            .get(peer_id)
            .is_some_and(|stored| stored.eq_ignore_ascii_case(fingerprint))
    }

    /// 记下/刷新一台**未配对**设备的观察指纹。值没变不落盘。
    pub fn note_observed_fingerprint(&self, peer_id: &str, fingerprint: &str) -> bool {
        if peer_id.is_empty() || fingerprint.is_empty() {
            return false;
        }
        let mut config = self.config.lock();
        if !config.set_observed_fingerprint(peer_id, fingerprint) {
            return false;
        }
        let _ = crate::config::save_config_at(&self.config_path, &config);
        true
    }

    /// 全清观察指纹（一键清空凭据时用）：它们也是"认识过谁"的一部分。
    pub fn clear_observed_fingerprints(&self) {
        let mut config = self.config.lock();
        if config.observed_fingerprints.is_empty() {
            return;
        }
        config.observed_fingerprints.clear();
        let _ = crate::config::save_config_at(&self.config_path, &config);
    }

    /// 对端每 5s 的 `/pair/info` 探活：请求本身不带 id/端口，但**带客户端证书**。
    ///
    /// 如果这枚证书正好是本机观察过的某台未配对设备，而来源 IP 与设备行里的地址不同，
    /// 就在后台到「新 IP + 旧端口」上核实一次身份；核实通过才更新地址。这是"对端沉默
    /// 时也能自动纠正 IP"的那一半：真设备换网后，它自己的探活就是最早到达的信号。
    /// 探活不带端口，所以只能沿用旧端口；端口也变了的那一档仍由它主动发言的
    /// `/chat/*` 或 `/pair/hello` 补上。
    pub fn note_probe_source(self: &Arc<Self>, source: std::net::IpAddr, fingerprint: &str) {
        let Some((peer_id, port)) = self.probe_source_candidate(source, fingerprint) else {
            return;
        };
        {
            let mut gate = ADDR_PROBE_GATE.lock();
            gate.retain(|_, at| at.elapsed() < ADDR_PROBE_COOLDOWN);
            if gate.contains_key(&peer_id) {
                return;
            }
            gate.insert(peer_id.clone(), Instant::now());
        }
        let state = self.clone();
        let fingerprint = fingerprint.to_string();
        crate::task::spawn("learn peer address from probe", async move {
            let host = source.to_string();
            if probe_peer_id_at(&host, port, &peer_id, &fingerprint).await {
                state.apply_verified_address(&peer_id, &host, port);
                ADDR_PROBE_GATE.lock().remove(&peer_id);
            }
        });
    }

    /// `note_probe_source` 的判据那一半：这条探活有没有资格触发一次地址核实？
    /// 返回 `(设备 id, 要沿用的旧端口)`。
    ///
    /// 抽出来是为了能单测：真正的核实要发一个 TLS 请求（证书必须与观察值一致），
    /// 而"该不该发"不该跟着网络一起测。
    fn probe_source_candidate(
        &self,
        source: std::net::IpAddr,
        fingerprint: &str,
    ) -> Option<(String, u16)> {
        let ids: Vec<String> = {
            let config = self.config.lock();
            config
                .observed_fingerprints
                .iter()
                .filter(|(_, fp)| fp.eq_ignore_ascii_case(fingerprint))
                .map(|(id, _)| id.clone())
                .collect()
        };
        if ids.len() != 1 {
            return None; // 没观察过 / 多个 id 共用一枚证书（抄配置）：不猜
        }
        let peer_id = ids.into_iter().next().expect("len == 1");
        let peer = self.get_peer(&peer_id)?;
        if peer.paired || peer.ip.is_empty() || peer.port == 0 {
            return None;
        }
        if crate::net::parse_host(&peer.ip).is_some_and(|ip| ip == source) {
            return None; // 来源就是已存的地址，没什么可学
        }
        if crate::net::observed_dial_target(source, peer.port, self.listening_port()).is_none() {
            return None; // 本机自己的监听端点
        }
        if self.is_relayed_source(source, &peer_id) {
            return None; // 来源是另一台已知设备：这条连接可能是经它中转来的
        }
        Some((peer_id, peer.port))
    }

    /// 一条**已验证过身份**的地址变更（目前只有探活来源那条路）：更新内存行、
    /// 地址簿/观察记忆，并把排队的东西往新地址放。
    ///
    /// 调用方必须先做过身份核实（`note_probe_source` 用证书指纹钉住新地址）；这里不再
    /// 做信任判断，只防并发：行在核实窗口里被配对/删掉了就不动。
    pub fn apply_verified_address(self: &Arc<Self>, peer_id: &str, ip: &str, port: u16) {
        if ip.is_empty() || port == 0 {
            return;
        }
        let mut changed = false;
        {
            let mut peers = self.peers.lock();
            let Some(p) = peers.iter_mut().find(|p| p.id == peer_id) else {
                return;
            };
            if p.paired {
                return;
            }
            if p.ip != ip || p.port != port || !p.online {
                p.ip = ip.to_string();
                p.port = port;
                p.online = true;
                changed = true;
            }
        }
        if !changed {
            return;
        }
        {
            let mut config = self.config.lock();
            if let Some(p) = config.peers.iter_mut().find(|p| p.id == peer_id) {
                if !p.paired {
                    p.ip = ip.to_string();
                    p.port = port;
                    p.online = true;
                    let _ = crate::config::save_config_at(&self.config_path, &config);
                }
            }
        }
        // 没有关系的那一档也记一份；地址簿里那一行上面已经自己落盘。
        self.note_known_peer(peer_id);
        self.emit_devices_changed();
        // 之前排队等它上线的消息：新地址已经核实，往那儿补发。
        crate::chat::flush_queued(self, peer_id);
        info!(%peer_id, %ip, port, "探活来源核实通过，已更新未配对设备地址");
    }

    /// 一台设备**经核实**的自报名字：直接更新（对端改名就跟着走）。
    ///
    /// "核实过"指这几条路：5s 探活的 `/pair/info` 回话、**有 token 的长连接会话**里的
    /// `from_name`（会话消息 / 推送邀请）、在它自己的地址上问出来的名字（中转那一跳）。
    /// 它们与 `upsert_peer` 收的 mDNS TXT / 会话信封是**同一档证据** —— 于是口径统一了：
    /// **对端最新的自报名字总是生效**。
    ///
    /// 为什么可以放开"真名永不覆盖"（那条老规矩守的东西不存在）：设备行的 `name`
    /// **从来不是用户填的** —— 手动添加（F-11）也只让用户填 IP/端口，名字是问对端
    /// `/pair/info` 要的；用户自己的命名只走**备注**（F-101，另一个字段、另一张表）。
    /// 所以这一格没有"用户的东西"可保护，只有"对端以前叫什么" —— 而旧名字赖着不走
    /// 是真会发生的：手机改名之后，只有 mDNS / HTTP 信封 / 担保人转述能纠正它，而
    /// "配过对、只走长连接、mDNS 又看不见"那一档（真机上就是跨段的手机）一条都轮不到。
    pub fn note_peer_name(&self, peer_id: &str, name: &str) {
        self.merge_peer_name(peer_id, name, true);
    }

    /// 对端**只是自报**的名字（未核实的那几条：经中转送来的路由记录、配对邀请里的自称）
    /// 只用来填**占位名**（本机已有的名字一个字都不动）。
    ///
    /// 与 [`Self::note_peer_name`] 的分界就是"这份名字我们核过没有"：核实过的当事实更新，
    /// 声称的只用来补"本机还不知道它叫什么"那一档。
    pub fn fill_peer_name(&self, peer_id: &str, name: &str) {
        self.merge_peer_name(peer_id, name, false);
    }

    /// 两条入口共用的那一半：能不能写、写哪儿（内存行 + 地址簿那份）。
    ///
    /// 占位名（空串 / 等于设备 id）任何一条路都能改写；**真名只有核实过的那条路能改**
    /// （`trusted`）—— 这就是上面两条入口唯一的区别。
    fn merge_peer_name(&self, peer_id: &str, name: &str, trusted: bool) {
        let name = name.trim();
        if name.is_empty() || name == peer_id {
            return;
        }
        let mut changed = false;
        {
            let mut peers = self.peers.lock();
            if let Some(p) = peers.iter_mut().find(|p| p.id == peer_id) {
                let take = trusted || name_is_placeholder(&p.name, &p.id);
                if take && p.name != name {
                    p.name = name.to_string();
                    changed = true;
                }
            }
        }
        if !changed {
            return;
        }
        // 地址簿里也有这一行（配对过 / 手动加的）就跟着更新：不然重启之后又回到旧名字
        // （那份是落盘的，`AppState::new` 直接读它）。空名/占位名不许冲掉真名那条闸
        // 仍然在 `should_take_name` 里，对两条入口一视同仁。
        {
            let mut config = self.config.lock();
            if let Some(p) = config.peers.iter_mut().find(|p| p.id == peer_id) {
                if should_take_name(&p.name, name, &p.id) {
                    p.name = name.to_string();
                    let _ = crate::config::save_config_at(&self.config_path, &config);
                }
            }
        }
        // 没有关系的那一档也记一份（`config.known_peers`）：它只活在内存里，不记下来
        // 重启之后那一行就没有名字可显示了（见 `KnownPeer`）。
        self.note_known_peer(peer_id);
        self.emit_devices_changed();
    }

    /// 把这一台记进「以前见过」那一档（`config.known_peers`）—— **值没变就一个字都不写**。
    ///
    /// 只在名字或地址真的变了时落盘一次（不是每条消息都写 config）；**与地址簿互斥**：
    /// 已配对的、或在地址簿里的 id 一律跳过（关系表说了算，那边自己会落盘）；名字还是占位名
    /// （空串 / 等于设备 id）时也不写（那不是名字）。
    ///
    /// 调用点：学到名字那条路（`merge_peer_name`）与学到地址那条路（`upsert_peer` /
    /// `observe_verified_addr`）的尾巴上各一次 —— 于是"见过它"这件事三处都收得住。
    pub fn note_known_peer(&self, peer_id: &str) {
        let Some(p) = self.get_peer(peer_id) else {
            return;
        };
        if p.paired || name_is_placeholder(&p.name, &p.id) {
            return;
        }
        let (name, ip, port) = (p.name.clone(), p.ip.clone(), p.port);
        let mut config = self.config.lock();
        if config.peers.iter().any(|b| b.id == peer_id) {
            return; // 地址簿里那一行由它自己维护（`merge_peer_name` / `upsert_peer`）
        }
        if config.note_known_peer(peer_id, &name, &ip, port) {
            let _ = crate::config::save_config_at(&self.config_path, &config);
        }
    }

    /// Mark a peer paired in both lists and persist the address book.
    pub fn mark_paired(&self, peer: &PeerDevice) {
        if !accepts_peer_id(&peer.id) {
            return;
        }
        {
            let mut peers = self.peers.lock();
            if let Some(p) = peers.iter_mut().find(|p| p.id == peer.id) {
                p.paired = true;
                if should_take_name(&p.name, &peer.name, &peer.id) {
                    p.name = peer.name.clone();
                }
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
                if should_take_name(&p.name, &peer.name, &peer.id) {
                    p.name = peer.name.clone();
                }
                if !peer.ip.is_empty() {
                    p.ip = peer.ip.clone();
                }
                if peer.port != 0 {
                    p.port = peer.port;
                }
            } else {
                config.peers.push(peer.clone());
            }
            // **互斥**：关系接管了这一格，观察记忆那份删掉（两张表永不同时持有一个 id）。
            config.forget_known_peer(&peer.id);
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
            let event = if online {
                "peer://online"
            } else {
                "peer://offline"
            };
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
                    let via = peer
                        .relay
                        .as_ref()
                        .map(|r| r.via.clone())
                        .unwrap_or_default();
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
        self.relay_failed
            .lock()
            .get(target)
            .and_then(|(via, at)| (at.elapsed() < FAILED_RELAY_COOLDOWN).then(|| via.clone()))
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
        // 这张表没有删除路径，键的条数因此要在这里把住（满了就不再收养新 id，已有的不动）。
        if !table.contains_key(peer_id) && table.len() >= MAX_PEER_NET_IDS {
            return;
        }
        // 值也封顶：`nets` 是对端说的话，条数由它定（见 [`MAX_PEER_NETS`]）。
        let capped: Vec<String> = nets.iter().take(MAX_PEER_NETS).cloned().collect();
        let slot = table.entry(peer_id.to_string()).or_default();
        if slot.as_slice() != capped.as_slice() {
            *slot = capped;
        }
    }

    /// 对端自报的网段（`nets` 的字符串原样）。
    pub fn peer_nets_of(&self, peer_id: &str) -> Vec<String> {
        self.peer_nets
            .lock()
            .get(peer_id)
            .cloned()
            .unwrap_or_default()
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
        let voucher_cidrs: Vec<crate::subnet::Cidr> = voucher_nets
            .iter()
            .filter_map(|s| crate::subnet::parse(s))
            .collect();
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
            for target in targets_for
                .get(&cidr.to_cidr_string())
                .cloned()
                .unwrap_or_default()
            {
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
        // 与 `note_peer_nets` 同一道闸：键来自**未鉴权的反向路由记录**里那些设备 id，
        // 而这张表没有删除路径。满了不再收养新 id —— 后果只是它那台设备的段算不出来
        // （`cidr_of` 退回本机掩码/None），路由退成设备级，不会误判谁的行。
        if !table.contains_key(peer_id) && table.len() >= MAX_PEER_NET_IDS {
            return;
        }
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
            let get = |id: &str| {
                peers
                    .iter()
                    .find(|p| p.id == id)
                    .map(|p| p.ip.clone())
                    .unwrap_or_default()
            };
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
            route.via_paired = pool
                .iter()
                .filter(|d| d.paired)
                .map(|d| d.id.clone())
                .collect();
            route.last_ok_at = Some(at.clone());
            route.ok_count = route.ok_count.saturating_add(1);
            route.seen_at = Some(at.clone());
            // 这条策略刚刚**真的把本机送到了目标**：即使它原先只是未认证来源的临时提示，
            // 现在也有事实背书了 —— 升格为可落盘的记忆（否则一次成功会被 15 分钟 TTL 收走）。
            route.ephemeral = false;
            route.ephemeral_at = None;
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
                    RoutePushState {
                        hash: hash.to_string(),
                        sent_at: now,
                        acked: true,
                    },
                );
                true
            }
            Some(s) if s.hash == hash && now.duration_since(s.sent_at) < ROUTE_RESEND_MIN => false,
            _ => {
                table.insert(
                    peer_id.to_string(),
                    RoutePushState {
                        hash: hash.to_string(),
                        sent_at: now,
                        acked: false,
                    },
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

    /// `/relay/route` 的**来源地址**限流（只对没有 Bearer 的调用方；见 `server::routes_relay`）。
    ///
    /// 合法的发送方最短 30s 一发、已确认后 5 分钟一发；30 发/分钟按来源地址记账，既不会
    /// 误伤同一 IP 上的多个实例，也把"一条报文就触发一次选路/补发"的放大压住。
    pub fn route_push_source_allowed(&self, source: &str) -> bool {
        let now = Instant::now();
        let mut table = self.route_push_sources.lock();
        // 键是**来源地址**（TCP 给的，伪造不了），但局域网里出现大量源地址时也不能无界长。
        if table.len() > 1024 {
            table.retain(|_, (start, _)| now.duration_since(*start) < ROUTE_PUSH_WINDOW);
        }
        let entry = table.entry(source.to_string()).or_insert((now, 0));
        if now.duration_since(entry.0) >= ROUTE_PUSH_WINDOW {
            *entry = (now, 0);
        }
        if entry.1 >= ROUTE_PUSH_MAX_PER_WINDOW {
            return false;
        }
        entry.1 += 1;
        true
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
    /// 通过就落库（**只对可信来源**；未认证来源走 [`Self::apply_relay_route_hints`]，只做
    /// 本进程内的临时提示），并把池里**本机还不认识**的设备并进设备列表（内存行）：接收方对那一段没有
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
        self.apply_relay_routes_inner(from, records, true)
    }

    /// 收下**未认证来源**推来的记录（只有经中转方送达的那一档会走到这里）。
    ///
    /// 与 [`Self::apply_relay_routes`] 的唯一区别是"能留下什么"：
    /// - 已经落盘（可信来源写过）的段**一个字都不动** —— 未认证的一方不许改写本机的记忆；
    /// - 新建/刷新出来的那条策略标成 `ephemeral`：只活在本进程内（`save_config_at` 会把它
    ///   滤掉），[`EPHEMERAL_ROUTE_TTL`] 到期由 [`Self::sweep_relay_memory`] 收走；
    /// - 刷新照旧会更新 `seen_at`、池与设备行，所以"B 立刻知道该经谁回去"这条功能不变。
    ///
    /// 为什么不能直接落盘：发送方（被中转的那一侧）与中转方之间的关系本机无从核实，一条
    /// 报文就能把它写进 `config.toml` 的权限不该发给局域网里的任意一台机器。记录本来就只是
    /// 提示 —— 发送方每 5 分钟（未确认时 30 秒）会重推一次，本机重启后最多等一拍就有。
    pub fn apply_relay_route_hints(
        self: &Arc<Self>,
        from: &str,
        records: &[crate::relay::RouteRecord],
    ) -> (Vec<String>, Vec<String>) {
        self.apply_relay_routes_inner(from, records, false)
    }

    fn apply_relay_routes_inner(
        self: &Arc<Self>,
        from: &str,
        records: &[crate::relay::RouteRecord],
        persist: bool,
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
        // 采纳的这几条里**内容真的变了**的条数：决定日志、是否落盘、是否通知界面。
        // 对端每 [`crate::state::ROUTE_REFRESH_MIN`] 会把没变过的记录重推一次，那是防"对端
        // 重启后手里没有这条记忆"的刷新，不是事件 —— 不能让它按 INFO 复述一遍，也不该让它
        // 再写一次盘（`seen_at` 只留在内存里；下一次内容变化会一起落盘）。
        let mut changed = 0usize;
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
                if !crate::subnet::own_ip_in(&target) || crate::subnet::parse(&r.via_cidr).is_none()
                {
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
                // **未认证来源不许改写已经落盘的记忆**：那条策略是本机从可信来源（Bearer
                // 对端 / 自己经中转成功）得到的，不能被一条"局域网里谁都能发"的报文覆盖。
                // 新建的、或上一次也是未认证来源建立的那一条，可以继续刷新。
                if !persist {
                    if let Some(existing) = config.relay_route(&r.target_cidr) {
                        if !existing.ephemeral {
                            ignored.push(r.target_cidr.clone());
                            continue;
                        }
                    }
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
                // 采纳之前那一份（`None` = 这条策略还不存在）。下面写完拿它比对：
                // 只有段、池地址或配对成员变了，才算"本机的路由记忆真的变了"。
                let before = config.relay_route(&r.target_cidr).cloned();
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
                // 未认证来源建立/刷新的那一条只活在本进程里；可信来源写它时顺手升格。
                let upgrading = persist && before.as_ref().is_some_and(|b| b.ephemeral);
                route.ephemeral = !persist;
                route.ephemeral_at = if persist { None } else { Some(Instant::now()) };
                let differs = match &before {
                    None => true,
                    Some(b) => {
                        b.via_cidr != route.via_cidr
                            || b.via_addrs != route.via_addrs
                            || b.via_paired != route.via_paired
                    }
                };
                // 升格（临时提示 → 可落盘）本身就要写一次盘，哪怕内容一个字没变。
                if differs || upgrading {
                    changed += 1;
                }
                accepted.push(r.target_cidr.clone());
                seed.push((r.via_cidr.clone(), devices));
            }
            // **只在内容真的变了时落盘**（且只对可信来源）：5 分钟一次的刷新重推不该
            // 反复重写整份 config；未认证来源的记录则根本不许进盘。`seen_at` 的刷新只留
            // 在内存里 —— 下一次内容变化会把它一起写下去，重启后的恢复靠发送方那一拍。
            if persist && changed > 0 {
                if let Err(e) = crate::config::save_config_at(&self.config_path, &config) {
                    warn!("反向中转路由落盘失败: {e}");
                }
            }
        }
        if !accepted.is_empty() {
            let mut adopted = 0usize;
            for (via_cidr, devices) in &seed {
                // 池在采纳那一趟就按"并"更新过了（`merge_pool` 也把活池写好了）—— 这里只做
                // "并进设备表 + 记它们的段"这两件事。
                //
                // 记录里那些设备是**别人报来的地址**：它们不会自报网段（adopted 不进探活队列），
                // 记录说的这一段就是它们唯一的归属信息（见 `note_relay_net`）。
                for d in devices {
                    self.note_relay_net(&d.id, via_cidr);
                }
                adopted += self.adopt_devices(devices, &[]);
            }
            // **这条事件唯一的 INFO 在这里**（HTTP 那一层只记 `debug`）：而且只在
            // `changed > 0`（内容真的变了）时才走。对端每 5 分钟还会把**没变过**的记录
            // 重推一次（`ROUTE_REFRESH_MIN`，防对端重启丢记忆）—— 那一趟照样采纳、照样
            // 刷新 `seen_at`，但日志里不该多一条"什么都没发生"的 INFO（实测一天几百条）。
            // 未认证来源的提示只记 `debug`：它的内容变化不值得按 INFO 复述（可能是别人在刷）。
            if changed > 0 {
                if persist {
                    info!(
                        from = %from,
                        accepted = accepted.len(),
                        changed,
                        ignored = ignored.len(),
                        "收到反向中转路由记录"
                    );
                } else {
                    debug!(
                        from = %from,
                        accepted = accepted.len(),
                        changed,
                        ignored = ignored.len(),
                        "收到未认证来源的反向中转提示（只在本进程内有效）"
                    );
                }
            }
            // 记录一到，"出路"就刚刚出现了：**当场**给还欠着消息的那几台叫一次补发。
            // 只有一条借来的路、直连地址从没学到过的那类对端没有别的触发器能告诉本机
            // "它上线了"；少了这一句，排队的消息要等用户再发一条才会动（`flush_queued`
            // 里会先试着借一条中转 —— 闸门与限流都在，重复叫是安全的）。
            crate::chat::flush_all_queued(self);
            // 未认证来源的提示不进持久记忆、也不会让设备列表多出可见的行（adopted 不满足
            // `online || paired || seen`），所以不必为它刷新界面 —— 少了它，一条局域网报文
            // 也不能拿 `devices://changed` 当放大器。
            if persist && (changed > 0 || adopted > 0) {
                self.emit_devices_changed();
            }
        }
        (accepted, ignored)
    }

    /// 这一行是不是"反向记录带进来的"（有 TTL、不参与探活）。
    pub fn is_adopted(&self, peer_id: &str) -> bool {
        self.adopted.lock().contains_key(peer_id)
    }

    /// 这一条中转策略是不是"未认证来源的临时提示"（只在本进程内有效、不落盘）。
    pub fn is_ephemeral_route(&self, target_cidr: &str) -> bool {
        self.config
            .lock()
            .relay_route(target_cidr)
            .is_some_and(|r| r.ephemeral)
    }

    /// 测试用：把临时提示的刷新时刻往前拨，验证 TTL 退休。生产代码不调用。
    #[cfg(test)]
    pub(crate) fn age_ephemeral_routes(&self, by: Duration) {
        let mut config = self.config.lock();
        for r in config.relay_routes.iter_mut().filter(|r| r.ephemeral) {
            r.ephemeral_at = r.ephemeral_at.and_then(|at| at.checked_sub(by));
        }
    }

    /// 把"本机还不认识"的设备并进设备列表（反向记录 / 中转方报来的池都走这一条路）。
    ///
    /// 归 [`AppState::adopted`] 那一档：有 TTL、不进探活队列。**已有的行一个字都不动** ——
    /// `upsert_peer` 是覆盖语义（空名字会抹掉对方的名字、地址会被改写、`paired` 会被写成
    /// `false` 并落盘），而这正是"反向推回来的记录里含着锚点自己"那条路会踩到的坑。
    ///
    /// 这一档**唯一的出路**是已配对的对端在 `/discover` 里报来同一台（[`Self::learn_peers`]
    /// 会把它升格成"学来的"行：那一档不探活也不打 `seen`，界面的准入一条都不满足）。
    ///
    /// 返回真正并进来的台数。
    pub fn adopt_devices(&self, devices: &[crate::relay::RouteDevice], exclude: &[&str]) -> usize {
        let known: HashSet<String> = {
            let peers: HashSet<String> = self.peers.lock().iter().map(|p| p.id.clone()).collect();
            let booked: HashSet<String> = self
                .config
                .lock()
                .peers
                .iter()
                .map(|p| p.id.clone())
                .collect();
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
                .find(|ip| {
                    ip.parse::<std::net::Ipv4Addr>()
                        .map(|a| cidr.contains(a))
                        .unwrap_or(false)
                })
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
        let parsed_at = |s: &str| {
            chrono::DateTime::parse_from_rfc3339(s)
                .ok()
                .map(|t| t.with_timezone(&chrono::Utc))
        };

        let mut config = self.config.lock();
        let mut changed = false;

        for route in config.relay_routes.clone() {
            let Some(cidr) = crate::subnet::parse(&route.target_cidr) else {
                // 网段字符串坏掉（手改过）：这条策略没法判归属，直接退休。
                changed |= config.drop_relay_route(&route.target_cidr);
                continue;
            };
            // **未认证来源的提示**（`ephemeral`）：只在本进程内活 [`EPHEMERAL_ROUTE_TTL`]。
            // 发送方在线时会按 5 分钟重推（刷新 `ephemeral_at`）；掉线/换段之后不能赖着不走。
            if route.ephemeral {
                let expired = route
                    .ephemeral_at
                    .map_or(true, |at| at.elapsed() > EPHEMERAL_ROUTE_TTL);
                if expired {
                    if config.drop_relay_route(&route.target_cidr) {
                        debug!(cidr = %route.target_cidr, "未认证来源的中转提示到期，收走");
                        changed = true;
                    }
                    continue;
                }
            }
            // 该段已知设备（本机见过的那些）：全部直连稳定可达 → 这一段恢复了，策略退休。
            let in_segment: Vec<&PeerDevice> = peers
                .iter()
                .filter(|p| {
                    p.ip.parse::<std::net::Ipv4Addr>()
                        .map(|ip| cidr.contains(ip))
                        .unwrap_or(false)
                })
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

    /// **听到**它了：从它那边收到了一条东西（会话信封、控制面核实过的回话）。
    ///
    /// 这是"它在网上"的第二条证据（第一条是探活/长连接）。写点多在**入站**那条路上：
    /// 对端能拨进来、本机拨不回去时，这是唯一能说明"它还在"的东西（见 [`AppState::heard`]）。
    /// 与 `note_seen` 同一档：只记时刻，不碰 `online`（那一位的写点仍然只有
    /// [`AppState::set_peer_online`] 一处）。
    pub fn note_heard(&self, peer_id: &str) {
        if peer_id.is_empty() {
            return; // 载荷里没带 id 的（老版本/中间有代理）：不当"听到"
        }
        self.heard
            .lock()
            .insert(peer_id.to_string(), Instant::now());
    }

    /// [`HEARD_TTL`] 之内听到过它吗。
    pub fn heard_recently(&self, peer_id: &str) -> bool {
        match self.heard.lock().get(peer_id) {
            Some(at) => at.elapsed() < HEARD_TTL,
            None => false,
        }
    }

    /// 忘掉"听到过"（解绑、清空凭据这类关系终结的场合：留着它会让一个已经不存在的关系
    /// 在 60s 里继续算"在网上"）。
    pub fn clear_heard(&self, peer_id: &str) {
        self.heard.lock().remove(peer_id);
    }

    /// 入站证据落地：**听到**它，并按这一档把在线状态当场收口（`online = true`）。
    ///
    /// 为什么不只记时刻、等 5s 那一拍：会话那一行正是用户看着的地方 —— 对端刚发来一条消息，
    /// 而那一行还挂着"离线"（设备页还会因为"离线 + 未绑定 + 没被 mDNS 看见"把它整行挡在
    /// 外面），是最刺眼的一处。收到就先记先收口；探活那一拍只负责在 [`HEARD_TTL`] 之后
    /// 如实落回来（[`AppState::set_peer_offline_unless_heard`]）。
    ///
    /// 只对**已经在设备表里的行**生效（`set_peer_online` 就是这样：表里没有那一行时它什么
    /// 都不做）—— 入站载荷里的 id 不配凭空造出一行"在线设备"。
    pub fn note_heard_online(self: &Arc<Self>, peer_id: &str) {
        self.note_heard(peer_id);
        self.set_peer_online(peer_id, true);
    }

    /// 全部忘掉（切「离线」= 隐身：那一刻设备列表就该是"谁都不在线"，回来的路要靠探活重走）。
    pub fn clear_all_heard(&self) {
        self.heard.lock().clear();
    }

    /// 收掉过期的"听到过"（5s 那一拍顺手跑，别让它跟着设备表无界长）。
    ///
    /// 判据两条：时间过了 [`HEARD_TTL`]、或设备表里已经没有这一行（行被预算/解绑收走之后，
    /// 这条记录再留着就只是内存垃圾）。
    pub fn prune_heard(self: &Arc<Self>) {
        let alive: HashSet<String> = self.peers.lock().iter().map(|p| p.id.clone()).collect();
        let mut dropped: Vec<String> = Vec::new();
        self.heard.lock().retain(|id, at| {
            let keep = at.elapsed() < HEARD_TTL && alive.contains(id);
            if !keep {
                dropped.push(id.clone());
            }
            keep
        });
        // 探活**跳过**的那两类里，`adopted`（只由反向记录带进来的行）在别处没有降级路径：
        // 候选过滤把它挡在探活之外、mDNS 从没提过它、`ws::session` 那条也轮不到它 ——
        // 它的"在线"只由这一档撑着（`learn_peers` 也一样，但它由担保人每 5s 的转述自己
        // 刷新，在这里翻会闪）。所以证据一过期就在这儿如实落回来：不然一台只在 60s 前
        // 说过一次话的设备会一直挂着"在线"，直到 900s 的 `ADOPTED_TTL` 把整行收走。
        for id in dropped {
            if self.is_adopted(&id) {
                self.set_peer_online(&id, false);
            }
        }
    }

    /// 把一台设备标回**离线** —— 除非[`HEARD_TTL`]之内刚听到过它（那时它只是"本机拨不动"，
    /// 不是"不在网上"）。
    ///
    /// 返回 `true` = 真的翻了（调用方据此决定要不要记日志/发事件）。三处"本机这一侧证据
    /// 用完了"的地方共用它：5s 探活的 `Offline` 那一档、mDNS 记录被撤、长连接断开且没有
    /// 可拨地址（`ws::session`）。
    ///
    /// 为什么不能各写一遍：这三处都在**本机视角**下结论（探活失败 / 记录没了 / 链路断了），
    /// 而对端可能正在往本机发消息 —— 少一处判据，"会说话的设备画成离线"就从那个口子漏出来。
    pub fn set_peer_offline_unless_heard(self: &Arc<Self>, peer_id: &str) -> bool {
        if self.heard_recently(peer_id) {
            return false;
        }
        let was = self.get_peer(peer_id).is_some_and(|p| p.online);
        self.set_peer_online(peer_id, false);
        was
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
    ///
    /// **判据里那一条"还活着"不能省**：只按"地址簿里有一行的 ip 等于来源地址"判，会把
    /// **直连**进来的发送方误判成"经它中转" —— 残留行的 ip 撞上（对端重装/清数据后换了
    /// device id、DHCP 把同一个地址分给了另一台、或某次观察值留下的旧行）时，本机就不记地址、
    /// 也不认那条连接的可达性，那一行于是永久停在「离线」。所以要求对上的那一行**此刻确实
    /// 还在本机视野里**（在线 / 已配对 / 刚听到过 / 被 mDNS 看见）—— 这是"本机真的认识它、
    /// 它也确实在网"，而一条残留行四条都不占。
    pub fn is_relayed_source(&self, source: std::net::IpAddr, caller_id: &str) -> bool {
        // 锁顺序：`peers` → (`seen`/`heard`) 单向，且**不嵌套**（先把需要的 id 拷出来），
        // 与 `probe_peers_once`/`sweep_*` 那两处的取锁顺序一致。
        let (candidates, alive_by_row): (Vec<String>, HashSet<String>) = {
            let peers = self.peers.lock();
            let mut candidates = Vec::new();
            let mut alive = HashSet::new();
            for p in peers.iter() {
                if p.id == caller_id
                    || !crate::net::parse_host(&p.ip).is_some_and(|ip| ip == source)
                {
                    continue;
                }
                if p.online || p.paired {
                    alive.insert(p.id.clone());
                }
                candidates.push(p.id.clone());
            }
            (candidates, alive)
        };
        if candidates.is_empty() {
            return false;
        }
        let (seen, heard) = {
            let seen = self.seen.lock();
            let heard = self.heard.lock();
            (
                seen.keys().cloned().collect::<HashSet<String>>(),
                heard
                    .iter()
                    .filter(|(_, at)| at.elapsed() < HEARD_TTL)
                    .map(|(id, _)| id.clone())
                    .collect::<HashSet<String>>(),
            )
        };
        candidates
            .iter()
            .any(|id| alive_by_row.contains(id) || seen.contains(id) || heard.contains(id))
    }

    /// 解除配对（F-14）：清 token、断链，但**保留设备行**并把它标回"未绑定"。
    ///
    /// 设备还在 mDNS 视野里就该继续出现在列表里（可以再点「配对」把关系建回来）；对方
    /// **离线**时这一行也还在 —— 界面把它排在在线设备下方（F-13），那正是唯一能"解除
    /// 配对"的机会：解除本身是本机动作，不需要对方在线（见 `lib.rs::unpair_peer`）。
    /// 地址簿（config.peers）里删掉，重启后不再记着它 —— 除非它再次被发现（或再次配对）。
    pub fn unpair(&self, peer_id: &str) {
        // "刚听到过"是**关系**层面的一条证据：关系解除了就不该再拿它撑着 60s 的"在线"
        // （那一行照旧在列表里，但状态由探活决定）。
        self.clear_heard(peer_id);
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
        // 关系没了，但"见过它"这件事还在：记进观察记忆那一档（互斥规则的**反方向** ——
        // 出了地址簿就该由它接手），于是重启后那一行照样有名字、第一拍照样有东西可探。
        self.note_known_peer(peer_id);
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
            // 观察指纹跟着设备的身份走：旧 id 的那一枚迁到新 id 名下，否则对端换了
            // device id 之后，地址连续性校验会永远找不到它的锚。
            let fp_changed = if let Some(fp) = config.observed_fingerprints.remove(from) {
                config.observed_fingerprints.insert(to.id.clone(), fp);
                true
            } else {
                false
            };
            let mut row_changed = false;
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
                row_changed = true;
            }
            if row_changed || fp_changed {
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

    /// 忘掉一台**未配对**设备：设备行、地址簿里的观察记忆、备注、观察指纹、临时表一起清。
    ///
    /// 已配对的行返回 `false`（调用方应先「解除配对」；否则清掉 token 之外的记录会让
    /// 对端还认着本机，而本机这里已经没有它 —— 那是最糟的半清状态）。凭据读不到时也不动，
    /// 宁可让用户重试，不拿一次瞬时 IO 故障去删用户数据。
    pub fn forget_peer(&self, peer_id: &str) -> bool {
        match crate::credential::get_peer_token(peer_id) {
            Ok(None) => {}
            Ok(Some(_)) => return false,
            Err(e) => {
                debug!(%peer_id, %e, "读取凭据失败，暂不忘记这台设备");
                return false;
            }
        }
        self.clear_heard(peer_id);
        self.ws.drop_peer(peer_id);
        self.learned.lock().remove(peer_id);
        self.seen.lock().remove(peer_id);
        self.adopted.lock().remove(peer_id);
        self.probe_failures.lock().remove(peer_id);
        self.peers.lock().retain(|p| p.id != peer_id);
        {
            let mut config = self.config.lock();
            config.known_peers.remove(peer_id);
            config.device_remarks.remove(peer_id);
            config.observed_fingerprints.remove(peer_id);
            config.peers.retain(|p| p.id != peer_id);
            let _ = crate::config::save_config_at(&self.config_path, &config);
        }
        self.emit_devices_changed();
        true
    }

    /// 同一个 `ip:port` 只留一行：未配对的重复行忘掉，已配对的行一律不动。
    ///
    /// `knownPeers` 按 **device id** 存，对端重装/清数据后 id 变了，旧 id 若没被身份核对接手
    /// （旧端口又不通）就会和新行一起指着同一格地址 —— 5s 探活各拨一次，等于把同一台死设备
    /// 拨 N 遍。启动时与每轮探活后各调一次。
    ///
    /// 保留顺序：已配对 > 在线 > id 字典序（确定性）。已配对的行**永远不自动删**：
    /// 关系是用户数据，宁可多留一行，也不替用户解除配对。
    pub fn dedupe_known_addresses(&self) -> usize {
        let mut groups: HashMap<(String, u16), Vec<(String, bool, bool)>> = HashMap::new();
        for p in self.peers.lock().iter() {
            if p.ip.is_empty() {
                continue;
            }
            groups
                .entry((p.ip.clone(), p.port))
                .or_default()
                .push((p.id.clone(), p.paired, p.online));
        }
        let mut doomed = Vec::new();
        for (_, mut rows) in groups {
            if rows.len() < 2 {
                continue;
            }
            rows.sort_by(|a, b| {
                b.1.cmp(&a.1)
                    .then(b.2.cmp(&a.2))
                    .then(a.0.cmp(&b.0))
            });
            for (id, paired, _) in rows.into_iter().skip(1) {
                if !paired {
                    doomed.push(id);
                }
            }
        }
        let mut removed = 0;
        for id in doomed {
            if self.forget_peer(&id) {
                removed += 1;
            }
        }
        if removed > 0 {
            info!(removed, "同一地址上的重复未配对设备行已合并");
        }
        removed
    }

    /// 一轮探活结束后记账：在线即清零；连续失败到阈值时，未配对的行整行忘掉。
    ///
    /// 配对的行走 [`PROBE_FAIL_LIMIT_PAIRED`] 那档，只留一条 debug —— 关系与地址都不动。
    fn note_probe_outcome(&self, peer_id: &str) {
        let (online, paired) = match self.peers.lock().iter().find(|p| p.id == peer_id) {
            Some(p) => (p.online, p.paired),
            None => return,
        };
        if online {
            self.probe_failures.lock().remove(peer_id);
            return;
        }
        let limit = if paired {
            PROBE_FAIL_LIMIT_PAIRED
        } else {
            PROBE_FAIL_LIMIT_UNPAIRED
        };
        let count = {
            let mut failures = self.probe_failures.lock();
            let n = failures.entry(peer_id.to_string()).or_insert(0);
            *n = n.saturating_add(1);
            *n
        };
        if count < limit {
            return;
        }
        self.probe_failures.lock().remove(peer_id);
        if paired {
            debug!(%peer_id, count, "连续探活失败已达上限：已配对行保留地址");
        } else {
            info!(%peer_id, count, "连续探活失败：忘掉未配对设备行");
            self.forget_peer(peer_id);
        }
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
    ///   的超时清掉 —— 否则 B 一离线，A 连自己 mDNS 看得见的设备都会掉。**两个例外**：
    ///   ① 名字还顶着占位符的行（空串 / 就是设备 id，见 [`name_is_placeholder`]）：跨段设备
    ///   本机 mDNS 看不见，占位名只有这条转述能改掉；② 只是**反向记录带进来的**那一档
    ///   （`adopted`，见 [`Self::adopt_devices`]）—— 本机跟它之间没有任何关系，它自己也没有
    ///   存活来源（不探活、不打 `seen`），所以担保人报来的就是它唯一的活路（升格成正常行，
    ///   并从 `adopted` 摘掉）。
    /// - **临时**：不写地址簿、不进凭据库，重启即忘。本机没跟它们建立过任何关系。
    /// - **存活跟着担保人**：刷新时间戳。担保人不再提到它（或担保人自己掉了、问不到了），
    ///   超过 [`LEARNED_TTL`] 就由 [`Self::sweep_learned`] 收走。
    pub fn learn_peers(&self, voucher_id: &str, learned: Vec<PeerDevice>) {
        let now = Instant::now();
        let (own_id, booked) = {
            let config = self.config.lock();
            (
                config.device_id.clone(),
                config
                    .peers
                    .iter()
                    .map(|p| p.id.clone())
                    .collect::<HashSet<String>>(),
            )
        };
        // 「反向记录带进来的」那一档的快照（升格判据用）。**先取快照**：下面持
        // `learned`/`peers` 守卫时不再去锁 `adopted` —— 那是 `adopt_devices` 与
        // `sweep_relay_memory` 的另一套顺序（`adopted → peers`）。
        let adopted: HashSet<String> = self.adopted.lock().keys().cloned().collect();
        let mut changed = false;
        // 这次真的被升格的（出了守卫再摘 `adopted`，顺序同上）。
        let mut promoted: Vec<String> = Vec::new();
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
                        // 这次转述能不能**接管**这一行（写存活、地址、名字）？默认不能：那是
                        // 本机自己的行，不该被对端的转述改写。能接管的有两类 ——
                        // ① 它已经是「学来的」那一档（下面 `table.contains_key`，刷新它的时间戳）；
                        // ② 它只是**反向记录带进来的**（`adopted`）：本机跟它没有关系、它也没有
                        // 别的存活来源，不接管它就永远进不了列表（本机借它中转、列表里却没有它，
                        // 而那一行只在 900s 的 `ADOPTED_TTL` 到期时才被收走）。地址簿里有它的
                        // （用户手动加过）或已配对的，仍按本机自己的行办 —— 存活只由探活说。
                        let promotable =
                            adopted.contains(&l.id) && !booked.contains(&l.id) && !existing.paired;
                        if !table.contains_key(&l.id) && !promotable {
                            // **例外只有占位名**：`observe_verified_addr` 与
                            // `AppState::new` 建行时拿设备 id 顶着名字，此后本机 mDNS 看不见
                            // 它（跨段）、探活也填不回（`note_peer_name` 走的是它，但被学来的
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
                        // 名字也走 `should_take_name`：担保人没说出名字时上面刚拿 id 顶过，
                        // 那条占位名不许把本机记着的真名冲成 `deviceid`（升格进来的那一档
                        // —— `promotable` —— 走的正是这条路，而它此前只在"填占位名"那一档
                        // 收名字）。真名之间照旧覆盖：担保人报的就是对端自己说的名字。
                        if should_take_name(&existing.name, &l.name, &l.id) {
                            existing.name = l.name.clone();
                        }
                        existing.online = true;
                        table.insert(l.id.clone(), now);
                        if promotable {
                            promoted.push(l.id.clone());
                        }
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
                            auto_connect: false,
                            paused: false,
                            relay: None,
                        });
                        table.insert(l.id, now);
                        changed = true;
                    }
                }
            }
        }
        if !promoted.is_empty() {
            // 升格之后它不再是"反向记录带进来的"：存活归这一层管（20s、跟着担保人刷新），
            // 也该跟着 `learned` 一起被收走。留在 `adopted` 里只会让它继续被探活跳过。
            let mut ad = self.adopted.lock();
            for id in &promoted {
                ad.remove(id);
            }
            debug!(n = promoted.len(), "反向记录带进来的行被担保人的转述升格");
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
                .filter(|id| config.peers.iter().any(|p| &p.id == *id) || paired.contains_key(*id))
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

    /// 把设备表里**没有归宿**的行收到 [`MAX_UNBOOKED_ROWS`] 以内，返回收掉几行。
    ///
    /// 判据要**同时**成立才收：不在地址簿里、没配对、不在 `learned`/`adopted` 那两档 TTL 里、
    /// 此刻不在线、也没被 mDNS 看见。前两条 =「重启之后它也不会在表里」（这一档的行只活在
    /// 内存里，`config.toml` 里没它、凭据库里也没它），后三条 =「它此刻不是活的」——
    /// 收掉一个正在网上的行，用户看到的就是"设备没了"，所以宁可让那几行暂时超预算，
    /// 也一个都不碰。
    ///
    /// 淘汰顺序按表里的先后：`peers` 是追加的 `Vec`，越靠前越老，而刷 id 的那一方攒出来的
    /// 正是靠前的那些。
    ///
    /// **调用点只有两处**（`upsert_peer` / `observe_verified_addr` 建成新行之后）：没建新行
    /// 就没什么可收的，不进 5s 拍 —— 那一拍已经有 `sweep_learned`/`sweep_relay_memory` 在
    /// 管另外两档。
    fn enforce_row_budget(&self) -> usize {
        // 快路：表本身就不到预算（`peers.len()` 恒 ≥ 可收的行数），连一次凭据读取都不花。
        if self.peers.lock().len() <= MAX_UNBOOKED_ROWS {
            return 0;
        }
        // 谁"有归宿"：地址簿里的、已配对的、两档 TTL 管着的、此刻看得见的。
        let booked: HashSet<String> = self
            .config
            .lock()
            .peers
            .iter()
            .map(|p| p.id.clone())
            .collect();
        let paired: HashSet<String> = crate::credential::load_peers()
            .unwrap_or_default()
            .keys()
            .cloned()
            .collect();
        let learned: HashSet<String> = self.learned.lock().keys().cloned().collect();
        let adopted: HashSet<String> = self.adopted.lock().keys().cloned().collect();
        let seen: HashSet<String> = self.seen.lock().keys().cloned().collect();
        // 谁"有归宿"：地址簿里的、已配对的、两档 TTL 管着的、此刻看得见的，以及**本机
        // 确实拨得动**的在线行。
        //
        // 最后那一格为什么是"在线**且**拨得动"而不是光"在线"：`online` 现在还能由
        // "刚听到过它"（`heard`）撑起来，而那一条**别人一条报文就能给**（未配对的会话信封
        // 没有 token，见 `routes_chat::accept`）—— 光看 `online`，一台换着 id 发报文的机器
        // 就能一行行攒出**收不走**的行（每一行还每 5s 被探活敲一次）。加上"拨得动"之后，
        // 能留住的是"有直连地址或手里有一条端点"的那些 —— 那正是这一档要保护的
        // "看得见、能用"的设备；只有听到过、本机一条路都没有的行照旧按可收处理
        // （与 `heard` 之前的行为一致）。
        let homeless = |p: &PeerDevice| {
            !booked.contains(&p.id)
                && !paired.contains(&p.id)
                && !learned.contains(&p.id)
                && !adopted.contains(&p.id)
                && !seen.contains(&p.id)
                && !(p.online && p.dialable())
        };

        let mut peers = self.peers.lock();
        let over = peers.iter().filter(|p| homeless(p)).count();
        if over <= MAX_UNBOOKED_ROWS {
            return 0;
        }
        let mut budget = over - MAX_UNBOOKED_ROWS;
        let before = peers.len();
        peers.retain(|p| {
            if budget > 0 && homeless(p) {
                budget -= 1;
                false
            } else {
                true
            }
        });
        let dropped = before - peers.len();
        if dropped > 0 {
            warn!(
                dropped,
                kept = MAX_UNBOOKED_ROWS,
                "设备表里没有归宿的行超了预算，收掉最老的那几行（地址簿/已配对/在线/看得见的都没动）"
            );
        }
        dropped
    }

    /// 对端报来一条「共享项变更」：这一次该不该向上报（口径见 [`SHARE_CHANGED_THROTTLE`]）。
    ///
    /// `true` = 这条是窗口内的第一条，调用方照常发 Tauri 事件；`false` = 同一个窗口里的重复，
    /// 吞掉（**不是错误**，调用方什么都不用做）。
    pub fn share_changed_due(&self, share_id: &str) -> bool {
        let now = Instant::now();
        let mut table = self.share_changed_at.lock();
        if table.len() >= MAX_SHARE_CHANGED_KEYS {
            let cutoff = now - SHARE_CHANGED_THROTTLE * 8;
            table.retain(|_, at| *at > cutoff);
            if table.len() >= MAX_SHARE_CHANGED_KEYS {
                return false; // 清不动 = 这一瞬真的见过这么多不同 id：不建表项、也不上报
            }
        }
        match table.get(share_id) {
            Some(at) if now.duration_since(*at) < SHARE_CHANGED_THROTTLE => false,
            _ => {
                table.insert(share_id.to_string(), now);
                true
            }
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
    state.refresh_trust_from_credentials();
}

impl AppState {
    /// 把凭据库里的指纹重灌进 `Trust`。读凭据失败时**保持旧表**，不要用空表覆盖：
    /// 空表 = 所有地址退回 TOFU，等于把已经钉住的身份全部作废。
    ///
    /// **带 token 的拨号不要读这张表**：它按 IP 聚合多枚指纹（共享中转端点 / 同机多实例），
    /// 校验器命中任意一枚就放行，于是同一地址上另一台已配对设备可以顶替目标收走 token。
    /// 那条路必须用 `net::dial_client` / `Trust::scoped` 按目标建表；这里只服务不带 token
    /// 的发现、学习与地址核实。
    pub fn refresh_trust_from_credentials(&self) {
        let credentials = match crate::credential::load_credentials() {
            Ok(c) => c,
            Err(e) => {
                warn!("凭据库读失败，保留现有指纹表: {e}");
                return;
            }
        };
        let peers = self.peers.lock();
        let entries = peers.iter().flat_map(|peer| {
            let Some(cred) = credentials.get(&peer.id) else {
                return Vec::new();
            };
            // 空指纹 = 老记录/旧版本配对：给它一枚永远匹配不上的哨兵，阻止出站 TLS
            // 在迁移完成前把 token 按 TOFU 发给任何证书。回环（本机测试/同机多实例）
            // 不设哨兵：那条路没有"局域网地址被投毒"这一层。
            // 未配对（只有 token、没有关系标记）不加哨兵：那条路不会带 token 出去，
            // 且测试/手动添加都靠它先连一次。
            let pinned = if cred.fingerprint.is_empty() {
                if peer.paired {
                    crate::net::DENY_FINGERPRINT.to_string()
                } else {
                    return Vec::new();
                }
            } else {
                cred.fingerprint.clone()
            };
            let loopback =
                |host: &str| crate::net::parse_host(host).is_some_and(|ip| ip.is_loopback());
            let mut out = Vec::new();
            if !peer.ip.is_empty() && !loopback(&peer.ip) {
                out.push((peer.ip.clone(), pinned.clone()));
            }
            // 中转端点（F-106）：端点上呈现的是**同一台设备**的证书，但它在指纹表里占的是
            // **另一个地址键位**（IPv4 映射的 IPv6，见 `net::relay_host`）。必须一起钉 ——
            // 不钉的话那条路只剩 TOFU，而"中转这一跳有没有人在中间"正是要防的东西。
            if let Some(relay) = &peer.relay {
                if !loopback(&relay.host) {
                    out.push((relay.host.clone(), pinned));
                }
            }
            out
        });
        crate::net::trust().seed(entries);
    }
}

/// 记下这次连上对端时见到的证书指纹（TOFU 的落库那一半）。
///
/// 只在**本机还没有它的指纹**时写：已经有了，就说明这次握手正是拿那枚指纹校验过的，
/// 没有别的可记。调用点都在"刚跟对端成功建立了一条 TLS 连接"的地方。
///
/// **调用频率与日志级别是一件事**：这条函数在 5s 探活里被每台有地址的设备各调一次，
/// 其中绝大多数是未配对的内网设备（凭据库里根本没有它们）—— 那一档现在回 `Ok(false)`，
/// 只记 `debug`。别把那里改回 `info`：一句话不是新闻却按新闻记，就是每天十几兆的刷屏。
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
    let unpaired = state.get_peer(peer_id).is_some_and(|p| !p.paired);
    match crate::credential::set_peer_fingerprint(peer_id, &observed) {
        Ok(true) => {
            info!(
                %peer_id,
                fingerprint = %crate::net::short_fingerprint(&observed),
                "已钉住对端证书指纹"
            );
            refresh_trust(state);
        }
        // 凭据库里没有这台设备（未配对）：观察值落到 `observedFingerprints` 那一档。
        // 它是**未配对地址变更**的连续性锚（见 `observed_fingerprint_matches` /
        // `note_probe_source`），与凭据里的关系指纹分开存。
        //
        // 这一档必须**不是** INFO：调用点在 5s 探活里，探活队列里的内网设备绝大多数是
        // 未配对的（mDNS 发现的行）。每台每 5 秒一句 INFO 就是每天十几兆的刷屏。
        Ok(false) if unpaired => {
            if state.note_observed_fingerprint(peer_id, &observed) {
                debug!(
                    %peer_id,
                    %ip,
                    fingerprint = %crate::net::short_fingerprint(&observed),
                    "已记下未配对设备的证书指纹"
                );
            }
        }
        Ok(false) => {
            debug!(
                %peer_id,
                %ip,
                fingerprint = %crate::net::short_fingerprint(&observed),
                "这台设备不在凭据库里（未配对），没有可钉的指纹"
            );
        }
        Err(e) => warn!(%peer_id, "指纹落库失败: {e}"),
    }
}

/// 「探活来源学新地址」的闸：同一台设备 30 秒内只核实一次，避免对端每 5s 的探活
/// 把本机变成一台不停向新地址发请求的机器。成功即清，失败留到冷却结束再试。
static ADDR_PROBE_GATE: std::sync::LazyLock<Mutex<HashMap<String, Instant>>> =
    std::sync::LazyLock::new(|| Mutex::new(HashMap::new()));
const ADDR_PROBE_COOLDOWN: Duration = Duration::from_secs(30);

/// 到 `host:port` 问一次 `/pair/info`，要求 **TLS 证书就是 `fingerprint` 这一枚**、
/// 回话的 id 就是 `expected_id`。两个条件都满足才算"这台设备真的在这里"。
///
/// 证书钉死是这条路的要害：只看 id 的话，任何能访问那个地址的人都能答一个 json。
async fn probe_peer_id_at(host: &str, port: u16, expected_id: &str, fingerprint: &str) -> bool {
    let client = crate::net::dial_client_timeout(
        host,
        &crate::net::DialPin::Fingerprint(fingerprint.to_string()),
        PROBE_TIMEOUT,
    );
    let url = format!("{}/pair/info", crate::net::peer_base(host, port));
    let response = match client.get(&url).send().await {
        Ok(response) => response,
        Err(_) => return false,
    };
    if !response.status().is_success() {
        return false;
    }
    match crate::net::read_json_capped::<serde_json::Value>(
        response,
        crate::net::PAIR_INFO_BODY_MAX,
    )
    .await
    {
        Ok(body) => body.get("id").and_then(|id| id.as_str()) == Some(expected_id),
        Err(_) => false,
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
    match crate::net::read_json_capped::<serde_json::Value>(
        response,
        crate::net::PAIR_INFO_BODY_MAX,
    )
    .await
    {
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
    /// 这条 TLS 连接上的证书不是目标设备：地址被改、有人在中间，或对端换了身份。
    /// 与"问不到"分开 —— 这是明确的关系问题（界面显示「需配对」）。
    Rejected,
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
        // 指纹不符是"这个地址上不是它"，不是"它没开机" —— 必须单独报出去，
        // 否则界面会把一次明确的身份变化画成离线。
        Err(e) if crate::net::is_identity_rejected(&e) => return DiscoverOutcome::Rejected,
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

    let body: serde_json::Value = match crate::net::read_json_capped(
        response,
        crate::net::CONTROL_BODY_MAX,
    )
    .await
    {
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
    let listening_port = body["port"]
        .as_u64()
        .and_then(|p| u16::try_from(p).ok())
        .unwrap_or(port);
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
    // 这一问带 token：按目标建表，别让同一地址上另一台已配对设备把 token 收走。
    let paired = state.get_peer(peer_id).is_some_and(|p| p.paired);
    let client = crate::net::dial_client_timeout(
        ip,
        &crate::credential::dial_pin(peer_id, paired),
        PROBE_TIMEOUT,
    );
    if let DiscoverOutcome::Ok {
        identity,
        entries,
        nets,
        listening_port,
    } = fetch_discover(&client, ip, port, token, own_id).await
    {
        absorb_discover_view(
            state,
            peer_id,
            ip,
            port,
            &identity,
            &nets,
            listening_port,
            &entries,
        );
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
        Some(identity) if identity.id.is_empty() || identity.id == expected_id => ProbeStep::Online,
        Some(identity) => ProbeStep::OtherIdentity(identity),
    }
}

const PROBE_TIMEOUT: Duration = Duration::from_millis(1500);
const RECONCILE_TIMEOUT: Duration = Duration::from_secs(2);

/// 探活的客户端（**不带 token** 的那一档）。带 token 的探活必须走
/// `net::dial_client_timeout` 按目标建表：全局表的键只有 IP，同一地址上的另一台设备
/// 出示自己的证书也会被放行，随后 token 就发给了它（见 `net::DialPin`）。
static PROBE_CLIENT: std::sync::LazyLock<reqwest::Client> = std::sync::LazyLock::new(|| {
    crate::net::client_builder()
        .timeout(PROBE_TIMEOUT)
        .build()
        .unwrap_or_default()
});

/// 同时探几台设备。探活是**本地**的一跳（同段/近段，1.5s 超时），一屏设备数的并发不值一提；
/// 定这个上限只为一件事：一台够不着的设备（1.5s 超时）别把排在它后面的每一台都拖住 ——
/// 串行时一屏死行能把最后那一台拖到十几秒，而它其实第一拍就该被判在线。
const PROBE_CONCURRENCY: usize = 8;

/// 未配对的行连续失败多少拍就整行忘掉（12 × 5s = 60s）。
///
/// 只对**未配对**的行用：它没有关系要保，行里的地址只是"上次见过"，对端一上线 mDNS 会
/// 重新把它带回来。60s 足够盖住一次短暂掉线，又不至于让死地址在 256 fd 的机器上无限拨。
const PROBE_FAIL_LIMIT_UNPAIRED: u32 = 12;

/// 已配对的行连续失败多少拍只记一条 debug（关系要保，不清地址）。
///
/// 配对设备可能在另一个网段、或只是暂时离线；清掉地址等于让用户重新配对。这里只用于
/// "别让失败计数无界长 + 每 5 分钟留一条可查的痕迹"。
const PROBE_FAIL_LIMIT_PAIRED: u32 = 60;

/// 身份核对用的客户端（超时比探活宽一点：这一步的回话更长，一问一答里带整段设备表）。
static RECONCILE_CLIENT: std::sync::LazyLock<reqwest::Client> = std::sync::LazyLock::new(|| {
    crate::net::client_builder()
        .timeout(RECONCILE_TIMEOUT)
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
    // 带 token 的拨号必须按**这台目标**建信任表：全局表按 IP 聚合指纹，同一地址上
    // 另一台已配对设备的证书也能通过校验，随后这次请求的 token 就发给了它。临时表里
    // 只有本机手里的这枚指纹（或"已配对但没有指纹"的拒绝哨兵），校验器的"任意一枚
    // 命中"于是退化成"必须是它"。
    //
    // 不带 token 的那一档没有可泄漏的东西，继续走全局表（TOFU / 既有 pin 语义不变）。
    let scoped_client;
    let client = match token {
        Some(_) => {
            let paired = state.get_peer(peer_id).is_some_and(|p| p.paired);
            scoped_client = crate::net::dial_client_timeout(
                host,
                &crate::credential::dial_pin(peer_id, paired),
                PROBE_TIMEOUT,
            );
            &scoped_client
        }
        None => client,
    };
    let probe = match token {
        Some(t) => match fetch_discover(client, host, port, t, own_id).await {
            DiscoverOutcome::Ok {
                identity,
                entries,
                nets,
                listening_port,
            } => {
                absorb_discover_view(
                    state,
                    peer_id,
                    host,
                    port,
                    &identity,
                    &nets,
                    listening_port,
                    &entries,
                );
                state.learn_peers(peer_id, discovered_to_peers(&entries));
                IdentityProbe::Seen(identity)
            }
            DiscoverOutcome::Fallback => probe_identity(client, host, port, token).await,
            DiscoverOutcome::Rejected => IdentityProbe::Rejected,
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
///
/// **各台之间并发、互不排队**（上限 [`PROBE_CONCURRENCY`]）：一台够不着的设备要占满一次
/// 1.5s 超时，串行时它后面每一台都得跟着等 —— 而这正是"启动后某台明明在线却画成离线"
/// 的放大器（列表里几台死行在前，真在网的那台排在最后）。
pub async fn probe_peers_once(state: &Arc<AppState>) {
    // 「学来的」行不在这里探：它们不是本机发现的，本机跟它们之间没有关系，拨过去既没有
    // 意义（本机到不了它时那一跳纯属空转），也正是这条路要避免的多余一跳。它们的存活由
    // 担保人的 `/discover` 表刷新 —— 见 `learn_peers` / `sweep_learned`。
    let learned: HashSet<String> = state.learned.lock().keys().cloned().collect();
    // 反向记录带进来的行也快照成一份：不能在持 `peers` 守卫时去锁 `adopted`（`sweep` 是反过来
    // 先 `adopted` 再 `peers` 的，两处抢锁顺序必须一致）。
    let adopted: HashSet<String> = state.adopted.lock().keys().cloned().collect();
    let own_id = state.config.lock().device_id.clone();

    let candidates: Vec<PeerDevice> = {
        // 同一个 `ip:port` 只探一次：`knownPeers` 按 device id 存，对端换 id 后可能有多条
        // 行指着同一格地址（见 `dedupe_known_addresses`）；这里再兜一层，避免一轮里把
        // 同一台死设备拨 N 遍。空地址的行不参与这个去重（它们只能走"借中转"那一条）。
        let mut seen_addr: HashSet<(String, u16)> = HashSet::new();
        state
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
            .filter(|p| p.ip.is_empty() || seen_addr.insert((p.ip.clone(), p.port)))
            .cloned()
            .collect()
    };

    /// 探一台设备：判死活、收拾换了 id 的行、顺带交换视野与"配对还活不活着"的活口供。
    ///
    /// 从原来的循环体里提出来只为一件事：好让下面用 `buffer_unordered` 并发跑 ——
    /// 慢的那一台不再挡着后面每一台。
    async fn probe_peer(state: &Arc<AppState>, p: PeerDevice, own_id: &str) {
        let client = &PROBE_CLIENT;
        // 入站证据先落地：这一拍不管拨不拨得动它，只要 [`HEARD_TTL`] 之内听到过它就算在线
        // （拨不动 ≠ 不在网上 —— 见 `AppState::heard`）。放在这一支之前，是因为下面三条
        // 出口（链路活着 / 一条路都没有 / 真的去拨）都会提前 `return`，各写一遍迟早漏一处。
        if state.heard_recently(&p.id) {
            state.set_peer_online(&p.id, true);
        }
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
                learn_from_peer(state, &p.id, &p.ip, p.port, &token, own_id).await;
            }
            // 这一台到此为止（原来是 `continue`：现在是这一台的独立任务）
            return;
        }
        // 一条路都没有（没地址、没借来的端点、也没有长连接 —— 上面那一支已经滤掉有链路的）：
        // 直连与中转都无处可探，但**出路可能就在手里**（记录里的池）。试着借一次：
        // 借到了，下一拍就有探活、设备行也就说得清"经谁中转"（不再等用户先发一条消息）；
        // 借不到也不亏 —— `ensure_for` 没候选时一次请求都不发，有候选时 30s 才一次。
        if !p.dialable() {
            // 一条路都没有时，可达性只剩"刚听到过它"这一条证据（`HEARD_TTL`）—— 而这一支
            // 会直接返回，下面那个 `Offline` 档永远轮不到它：不在一个 TTL 之后自己收回来，
            // 一台"只闻其声、本机压根拨不动"的设备就永久停在在线了。
            state.set_peer_offline_unless_heard(&p.id);
            let st = Arc::clone(state);
            let id = p.id.clone();
            crate::task::spawn("relay warm-up", async move {
                crate::relay::ensure_for(&st, &id).await;
            });
            return;
        }
        // 手里那条 token 一起捎上：对端顺带回答"它把本机认作谁"，那是配对关系
        // 是否还活着的证据。**读不到 ≠ 没有**：凭据库的瞬时故障（凭据管理器忙、同机多
        // 实例并发）不能当成"我们没配对"—— 那会问出一个"它不认我们"的假答案，
        // 进而把一条好好的地址当垃圾扔掉。这一轮跳过它，下一轮（5s 后）再说。
        let token = match crate::credential::get_peer_token(&p.id) {
            Ok(t) => t,
            Err(e) => {
                warn!(peer_id = %p.id, "凭据库读失败，这一轮跳过探活: {e}");
                return;
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
            probe_addr(
                state,
                client,
                &p.id,
                &p.ip,
                p.port,
                token.as_deref(),
                own_id,
            )
            .await
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
                    own_id,
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
            return;
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
                // 顺带把"它叫什么"更新到设备行：这一问的回话里就带着名字，而且**是核实过的**
                // （这个地址上答话的确实是这台设备），所以对端改名跟着走（`note_peer_name`）；
                // 占位名（`observe_verified_addr` 拿设备 id 顶着的那种）也正是靠这一条修好的
                // —— 这类设备本机 mDNS 看不见（跨段）、学来的行又在探活队列之外。
                if let Some(identity) = &seen {
                    state.note_peer_name(&p.id, &identity.name);
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
                // **拨不动 ≠ 不在网上**：这一拍问不到它（地址过期、AP 把单播隔开、跨段、
                // 或者它根本没有可拨的地址），但本机可能一直在收它的消息。那时"在线"
                // 由入站证据撑着（`HEARD_TTL`），别把一台正在说话的设备画成离线
                // （见 `AppState::set_peer_offline_unless_heard`）。
                if state.heard_recently(&p.id) {
                    debug!(peer_id = %p.id, "probe missed but recently heard from it, keeping online");
                } else if p.online {
                    info!(peer_id = %p.id, "peer unreachable, marking offline");
                }
                state.set_peer_offline_unless_heard(&p.id);
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

    futures_util::stream::iter(candidates)
        .map(|p| {
            let id = p.id.clone();
            let own = own_id.clone();
            async move {
                probe_peer(state, p, own.as_str()).await;
                state.note_probe_outcome(&id);
            }
        })
        .buffer_unordered(PROBE_CONCURRENCY)
        .for_each(|()| async {})
        .await;

    // 一轮结束后再合并一次：这一轮里 mDNS/中转可能又带进来同一地址的新 id。
    state.dedupe_known_addresses();
    // 担保人这一轮没再提到、或担保人自己掉了的学习条目，从这里收走。放在一轮之末：
    // 这一轮里刚被提到的都已经刷新过时间戳，不会误伤。
    state.sweep_learned();
    // "刚听到过"这一档也一样：过期的、或设备表里已经没有的行，从这里收掉（别让它无界长）。
    state.prune_heard();
    // F-107 §11：网段级策略的退休与 TTL、设备级链接的兜底寿命、没有策略的池。
    state.sweep_relay_memory();
}

/// 向一个地址问清身份并按结果收拾旧行（探活与 mDNS 发现共用的入口）。
pub async fn reconcile_stale_identity(state: &Arc<AppState>, stale_id: &str) {
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
    // 带 token 的一问按这台设备建表；不带 token 的沿用全局（没有可泄漏的东西）。
    let scoped_client;
    let client: &reqwest::Client = match token.as_deref() {
        Some(_) => {
            scoped_client =
                crate::net::dial_client_timeout(&stale.ip, &stale.dial_pin(), RECONCILE_TIMEOUT);
            &scoped_client
        }
        None => &RECONCILE_CLIENT,
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
                name: if seen.name.is_empty() {
                    stale.name.clone()
                } else {
                    seen.name.clone()
                },
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
        state
            .config
            .lock()
            .relay_route_mut("192.168.1.0/24")
            .via_cidr = "192.168.50.0/24".into();

        let voucher = discovered("c", "192.168.50.7", None, true);
        let entries = vec![
            discovered("d", "192.168.50.8", Some("192.168.50.0/24"), true), // 同段 + 允许 → 进池
            discovered("e", "192.168.50.9", Some("192.168.50.0/24"), false), // 同段但不允许
            discovered("f", "10.9.9.9", Some("10.9.0.0/16"), true),         // 别的段
            discovered("g", "192.168.50.10", None, true),                   // 没说自己在哪一段
        ];
        state.absorb_view("c", &["192.168.50.0/24".into()], true, &voucher, &entries);

        let ids: Vec<String> = state
            .pool_for("192.168.1.0/24")
            .into_iter()
            .map(|p| p.id)
            .collect();
        assert_eq!(
            ids,
            vec!["c".to_string(), "d".to_string()],
            "锚点自己 + 同段且允许中转的"
        );
        assert!(
            state.pool_for("192.168.50.0/24").is_empty(),
            "中转方所在的那一段不是目标段：池不该挂在这个键上"
        );
        assert!(
            state.pool_for("10.9.0.0/16").is_empty(),
            "应答方不在那一段：不建池"
        );
        assert!(
            state
                .pool_for("192.168.1.0/24")
                .iter()
                .all(|p| p.ip.starts_with("192.168.50.")),
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
            &[discovered(
                "d",
                "192.168.50.8",
                Some("192.168.50.0/24"),
                true,
            )],
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
        absorb_discover_view(
            &state,
            "c",
            "192.168.50.7",
            3890,
            &identity,
            &identity.nets,
            3890,
            &[],
        );
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
        let ids: Vec<String> = state
            .pool_for("192.168.1.0/24")
            .into_iter()
            .map(|p| p.id)
            .collect();
        assert_eq!(
            ids,
            vec!["c".to_string(), "d".to_string()],
            "池按**目标段**记"
        );
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
        assert_eq!(
            state.absorb_relay_report("zz", "192.168.50.0/24", &devices),
            0
        );
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
        let fresh: Vec<_> = (1..=8)
            .map(|i| device(&format!("new-{i:02}"), 100 + i))
            .collect();
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
        assert_eq!(
            state.cidr_of("x", foreign).as_deref(),
            Some("192.168.33.0/24")
        );
        // 自报的段必须**覆盖它自己的地址**才算数（乱报的段不该被采纳）。
        state.note_peer_nets("y", &["10.0.0.0/8".to_string()]);
        assert!(
            state.cidr_of("y", foreign).is_none(),
            "自报的段盖不住这个地址 → 不算"
        );

        // 同段（本机网卡）那一档：不需要自报，按本机掩码算。
        if let Some(ip) = crate::server::local_ipv4_addresses().first() {
            let expected =
                crate::subnet::local_net_of(ip.parse().unwrap()).map(|c| c.to_cidr_string());
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
        let Some(net) = crate::subnet::host_nets()
            .first()
            .map(|c| c.to_cidr_string())
        else {
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
        assert_eq!(
            state.apply_relay_routes("peer-a", &records).0,
            vec![net.clone()]
        );

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
                name: "示例对端".into(),
                ip: "10.20.30.40".into(),
                port: 3890,
                online: true,
                ..Default::default()
            }],
        );

        let row = state.get_peer("dev-hw").expect("本机自己的行还在");
        assert_eq!(row.name, "示例对端", "占位名要换成担保人报来的真名");
        assert!(
            !state.is_learned("dev-hw"),
            "它仍是本机自己的行，不归学来那一档"
        );
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

    /// 回归（真机反馈）：**反向记录带进来的行，不能被担保人后来的转述挡住**。
    ///
    /// `adopt_devices` 建出来的行是 `online: false`、不打 `seen`、也不进探活队列 —— 界面准入
    /// （`online || paired || seen`）三条一条都不满足，所以担保人（F-10 那条 `/discover` 表）的
    /// 转述是它唯一的活路。此前 `learn_peers` 对"已存在的行"只在它已经是 `learned` 成员时才
    /// 接管，于是这台设备**永远**不进列表：本机在借它中转（池里有地址）、设备行却看不见它，
    /// 而那一行只在 900s 的 `ADOPTED_TTL` 到期时才被收走（`sweep_relay_memory`）—— 真机上就是
    /// "经它中转，列表里却没有它"，偶尔还闪一下就没了。
    #[test]
    fn a_vouched_device_promotes_an_adopted_row() {
        let state = memory_state();
        let devices = vec![crate::relay::RouteDevice {
            id: "dev-c".into(),
            name: "C".into(),
            ip: "192.168.50.7".into(),
            port: 3890,
        }];
        assert_eq!(
            state.adopt_devices(&devices, &[]),
            1,
            "前提：这一台先经「反向记录」那一档进来"
        );
        assert!(state.is_adopted("dev-c"));
        assert!(
            !state.get_peer("dev-c").unwrap().online,
            "前提：adopted 行不声明在线（也不打 seen）"
        );

        // 一台**已配对**的对端每 5s 把它的 mDNS 视野报来。
        state.learn_peers(
            "voucher-b",
            vec![PeerDevice {
                id: "dev-c".into(),
                name: "书房的台式机".into(),
                ip: "192.168.50.7".into(),
                port: 3890,
                online: true,
                ..Default::default()
            }],
        );

        let row = state.get_peer("dev-c").expect("行还在");
        assert!(row.online, "担保人报来了就按在线算（与其它学来的行同一档）");
        assert_eq!(row.name, "书房的台式机", "名字跟着担保人走");
        assert!(
            state.is_learned("dev-c"),
            "并进「学来的」那一档：20s、跟着担保人刷新"
        );
        assert!(
            !state.is_adopted("dev-c"),
            "升格之后不再是 adopted —— 不该继续被探活跳过"
        );
    }

    /// 上一条的边界：**本机自己的行**（地址簿里的、已配对的）不会被转述声明在线 ——
    /// 存活只由探活说。这一档即便碰巧也在 `adopted` 里（先被采纳、之后用户手动添加），
    /// 也不归转述接管。
    #[test]
    fn a_vouched_device_never_takes_over_our_own_row() {
        let state = memory_state();
        state.add_peer_book(PeerDevice {
            id: "dev-c".into(),
            name: "书房的台式机".into(),
            ip: "10.9.9.9".into(),
            port: 3890,
            ..Default::default()
        });
        // 直接摆出"它同时也在 adopted 里"这个状态（正常流程里 `adopt_devices` 会跳过地址簿里的 id）。
        state.adopted.lock().insert("dev-c".into(), Instant::now());

        state.learn_peers(
            "voucher-b",
            vec![PeerDevice {
                id: "dev-c".into(),
                name: "别人叫它 C".into(),
                ip: "10.44.0.8".into(),
                port: 3890,
                online: true,
                ..Default::default()
            }],
        );

        let row = state.get_peer("dev-c").expect("行还在");
        assert!(!row.online, "地址簿里的行：存活只由探活说");
        assert_eq!(row.ip, "10.9.9.9", "地址不被转述改写");
        assert_eq!(row.name, "书房的台式机", "真名不被转述改写");
        assert!(!state.is_learned("dev-c"), "它不归「学来的」那一档");
        assert!(state.is_adopted("dev-c"), "adopted 那一档也不被这次转述动");
    }

    /// `fill_peer_name` 的边界 —— 它是**没核实过的主张**那条路：只填占位名（空串 / 等于 id），
    /// 本机已有的名字一个字都不动；报来的名字本身就是占位（空串 / 等于 id）也不算名字。
    ///
    /// 对照：核实过的自报名字走 `note_peer_name`，那条路**会**更新真名（改名跟着对端走）
    /// —— 见 `a_verified_self_report_updates_the_name_and_the_book`。
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
        assert_eq!(
            state.get_peer("dev-q").unwrap().name,
            "dev-q",
            "等于 id = 占位，不写"
        );
        state.fill_peer_name("dev-q", "   ");
        assert_eq!(
            state.get_peer("dev-q").unwrap().name,
            "dev-q",
            "空串 = 占位，不写"
        );
    }

    /// 名字这一格：**空名 / 占位名不许冲掉真名，真名之间照旧互相覆盖**（对端改名走的就是后者）。
    ///
    /// 以前 `upsert_peer` / `mark_paired` 无条件覆盖：一条不带名字的报文（老版本对端、
    /// 或控制面那条只报 id + 端口的请求）就能把用户看得见的名字换成一串 deviceid，
    /// 地址簿落盘的那一份也跟着一起坏。
    #[test]
    fn a_real_name_survives_reports_that_carry_none() {
        let state = memory_state();
        state.observe_verified_addr("dev-r", "10.0.0.9", 3890);
        state.fill_peer_name("dev-r", "PC-R");
        let report = |name: &str| PeerDevice {
            id: "dev-r".into(),
            name: name.into(),
            ip: "10.0.0.9".into(),
            port: 3890,
            ..Default::default()
        };
        for name in ["", "   ", "dev-r"] {
            state.upsert_peer(&report(name));
            assert_eq!(
                state.get_peer("dev-r").unwrap().name,
                "PC-R",
                "占位名/空名不该冲掉真名：{name:?}"
            );
        }
        // 真名之间照旧覆盖：对端改名（mDNS TXT / 会话信封 / 配对握手报的都是它自己）。
        state.upsert_peer(&report("PC-R2"));
        assert_eq!(state.get_peer("dev-r").unwrap().name, "PC-R2");
        // 反向：占位行要被真名填上（不然"设备列表显示的是 deviceid"永远修不回来）。
        state.observe_verified_addr("dev-s", "10.0.0.10", 3890);
        state.upsert_peer(&PeerDevice {
            id: "dev-s".into(),
            name: "PC-S".into(),
            ip: "10.0.0.10".into(),
            port: 3890,
            ..Default::default()
        });
        assert_eq!(state.get_peer("dev-s").unwrap().name, "PC-S");
    }

    /// 入站证据那一档（`heard`）：**拨不动 ≠ 不在网上**。
    ///
    /// 这是"明明在会话里说着话、设备行却一直画离线"的正面判据 —— 本机拨不回去的那些
    /// （AP 隔开单播、跨段、只有一条借来的中转链）靠它才算在线；过了 TTL 要如实落回来，
    /// 免得一台早就走了的设备永久停在线。
    #[test]
    fn being_heard_keeps_a_peer_online_until_the_evidence_expires() {
        let state = memory_state();
        state.observe_verified_addr("dev-h", "10.0.0.11", 3890);
        assert!(
            !state.get_peer("dev-h").unwrap().online,
            "前提：新行是离线的"
        );

        state.note_heard_online("dev-h");
        assert!(
            state.get_peer("dev-h").unwrap().online,
            "刚收到它的东西 = 它在网上（哪怕本机拨不动它）"
        );
        assert!(
            !state.set_peer_offline_unless_heard("dev-h"),
            "听到过就不翻"
        );
        assert!(state.get_peer("dev-h").unwrap().online);

        // 设备表里没有的行：载荷里的 id 不配凭空造出一台"在线设备"。
        state.note_heard_online("ghost");
        assert!(state.get_peer("ghost").is_none());
        state.prune_heard();

        // 过了 TTL：如实翻回离线，记录也被收掉（`prune_heard` 那一档）。
        state.heard.lock().insert(
            "dev-h".into(),
            Instant::now() - HEARD_TTL - Duration::from_secs(1),
        );
        assert!(!state.heard_recently("dev-h"));
        assert!(
            state.set_peer_offline_unless_heard("dev-h"),
            "TTL 之外要真的翻"
        );
        assert!(!state.get_peer("dev-h").unwrap().online);
        state.prune_heard();
        assert!(state.heard.lock().is_empty(), "过期记录不该留着");
    }

    /// 解绑要把"刚听到过"一起忘掉：关系都解除了，那一档不该再撑着 60s 的"在线"。
    #[test]
    fn unpairing_forgets_being_heard() {
        let state = memory_state();
        state.observe_verified_addr("dev-u", "10.0.0.12", 3890);
        state.note_heard_online("dev-u");
        state.unpair("dev-u");
        assert!(!state.heard_recently("dev-u"));
    }

    /// `is_relayed_source`：**残留行不算**。
    ///
    /// 只按"地址簿里有一行的 ip 等于来源地址"判，会把**直连**的发送方误判成"经它中转"
    /// （对端重装/清数据后换了 device id、DHCP 把同一个地址分给了另一台……那次留下的行
    /// 都在地址簿里）—— 误判的代价是地址不记、可达性不认，那一行于是永久停在「离线」。
    #[test]
    fn a_dead_row_does_not_make_a_direct_sender_look_relayed() {
        let state = memory_state();
        let source: std::net::IpAddr = "10.30.0.7".parse().unwrap();
        let stale = PeerDevice {
            id: "dev-old".into(),
            name: "旧行".into(),
            ip: "10.30.0.7".into(),
            port: 3890,
            ..Default::default()
        };
        // 残留行：同地址、离线、未配对、没被 mDNS 看见、也没听到过 —— 四条一条都不占。
        state.add_peer_book(stale.clone());
        assert!(
            !state.is_relayed_source(source, "dev-new"),
            "一条死行不该把直连的发送方说成中转"
        );
        // 它活着（已配对 / 在线 / 看得见 / 刚听到过）才配当"这条连接是它转发来的"。
        state.mark_paired(&PeerDevice {
            paired: true,
            ..stale.clone()
        });
        assert!(
            state.is_relayed_source(source, "dev-new"),
            "关系活着的那一台才当转发方"
        );
        // 自己永远不算"另一台"。
        assert!(!state.is_relayed_source(source, "dev-old"));
    }

    /// 启动那一刻：占位名的行**先拿会话记录里那个真名补上**（地址簿里的真名一个字不动）。
    ///
    /// 这是"启动时先显示一串 deviceid、过一会儿才变名字"的直接判据 —— 名字本机早就有了
    /// （对端自己发来的消息带着它当时的设备名、落盘在会话记录里），不该等第二次观察。
    #[test]
    fn a_placeholder_name_is_seeded_from_the_chat_record_on_boot() {
        let dir = std::env::temp_dir().join(format!("orbdrop_boot_name_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        // 记录里：dev-x 有一条**对端发来的**消息（带真名），dev-y 只有本机发出去的（没名字）。
        std::fs::write(
            dir.join("chat.json"),
            r#"{"convos":{
                "dev-x":{"messages":[{"id":"m1","fromMe":false,"fromName":"示例设备","text":"你好","at":1,"delivery":"received","targets":["dev-x"]}]},
                "dev-y":{"messages":[{"id":"m2","fromMe":true,"fromName":"","text":"喂","at":2,"delivery":"sent","targets":["dev-y"]}]}
            }}"#,
        )
        .unwrap();
        let mut config = crate::config::test_config();
        config.device_id = "me".into();
        config.peers = vec![
            // 占位名（= id）：该被记录里那个真名补上。
            PeerDevice {
                id: "dev-x".into(),
                name: "dev-x".into(),
                ..Default::default()
            },
            // 真名：一个字都不动（哪怕记录里也有名字）。
            PeerDevice {
                id: "dev-y".into(),
                name: "我的笔记本".into(),
                ..Default::default()
            },
        ];
        let state = AppState::new(config, dir.join("config.toml"));
        // 启动时已经搬到 vault：旧明文删除，新文件不是明文。
        let vault_chat = dir.join("chat-vault").join("chat.dat");
        assert!(vault_chat.is_file(), "首启应迁到 chat-vault/chat.dat");
        assert!(!dir.join("chat.json").exists(), "旧账本应清掉");
        let raw = std::fs::read(&vault_chat).unwrap();
        assert!(
            !raw.windows("你好".len()).any(|w| w == "你好".as_bytes()),
            "落盘不该有聊天明文"
        );
        assert_eq!(
            state.get_peer("dev-x").unwrap().name,
            "示例设备",
            "占位名要用记录里那个真名补上（不然启动那一刻显示的是 deviceid）"
        );
        assert_eq!(
            state.get_peer("dev-y").unwrap().name,
            "我的笔记本",
            "已经有真名：记录不动它"
        );
        // 播种只在内存里：配置那份不该因此多出一份会漂移的副本。
        crate::config::save_config_at(&dir.join("config.toml"), &state.config.lock()).unwrap();
        let on_disk = crate::config::load_config_at(&dir.join("config.toml"));
        assert_eq!(
            on_disk
                .peers
                .iter()
                .find(|p| p.id == "dev-x")
                .map(|p| p.name.clone()),
            Some("dev-x".into()),
            "落盘那份不动（每次启动都从记录里重新取）"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// **核实过的**自报名字会更新真名（对端改名跟着走），并且落进地址簿那份 —— 不然重启之后
    /// 又回到旧名字。占位名/空名的自报一律不算名字（两条入口共用的那一关）。
    ///
    /// 这是"手机改名之后本机停在旧名字"那个残留的正面判据：老规矩（`fill_peer_name` 只填
    /// 占位行）下，配过对、只走长连接、mDNS 又看不见的那一档没有任何一条路能纠正它。
    #[test]
    fn a_verified_self_report_updates_the_name_and_the_book() {
        let state = memory_state();
        state.add_peer_book(PeerDevice {
            id: "dev-r".into(),
            name: "PC-R".into(),
            ip: "10.0.0.9".into(),
            port: 3890,
            ..Default::default()
        });
        assert_eq!(state.get_peer("dev-r").unwrap().name, "PC-R");

        state.note_peer_name("dev-r", "PC-R2");
        assert_eq!(
            state.get_peer("dev-r").unwrap().name,
            "PC-R2",
            "核实过的名字要更新真名（改名跟着对端走）"
        );
        let on_disk = {
            crate::config::save_config_at(&state.config_path, &state.config.lock()).unwrap();
            crate::config::load_config_at(&state.config_path)
        };
        assert_eq!(
            on_disk
                .peers
                .iter()
                .find(|p| p.id == "dev-r")
                .map(|p| p.name.clone()),
            Some("PC-R2".into()),
            "地址簿那份也要跟着更新（否则重启又回到旧名字）"
        );

        // 占位名/空名不算名字：两条入口共用这一关。
        for bad in ["", "   ", "dev-r"] {
            state.note_peer_name("dev-r", bad);
            state.fill_peer_name("dev-r", bad);
            assert_eq!(
                state.get_peer("dev-r").unwrap().name,
                "PC-R2",
                "占位名不写：{bad:?}"
            );
        }
    }

    /// 「以前见过」那一档（`config.known_peers`）：**启动时给没有关系的设备补一行** ——
    /// 名字当场就有（不必等第二次观察），可达性交给启动即跑的第一拍探活。
    ///
    /// 这是"重新安装/重启之后那一台要等一段时间才有名字、而且不在设备列表里"的正面判据：
    /// 未配对 + 不在地址簿的行只活在内存里，不记这一份就重启即忘。
    #[test]
    fn a_known_peer_gets_a_row_at_boot() {
        let dir = std::env::temp_dir().join(format!("orbdrop_known_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut config = crate::config::test_config();
        config.device_id = "me".into();
        assert!(config.note_known_peer("dev-k", "示例设备", "10.0.0.7", 3890));
        // 地址簿里也有一台（它有联系方式、名字以它为准）。
        config.peers = vec![PeerDevice {
            id: "dev-booked".into(),
            name: "我的笔记本".into(),
            ip: "10.0.0.8".into(),
            port: 3890,
            ..Default::default()
        }];
        assert!(config.note_known_peer("dev-booked", "旧的观察名", "10.0.0.99", 999));
        crate::config::save_config_at(&dir.join("config.toml"), &config).unwrap();

        let loaded = crate::config::load_config_at(&dir.join("config.toml"));
        let state = AppState::new(loaded, dir.join("config.toml"));

        let k = state
            .get_peer("dev-k")
            .expect("没有关系的设备也要有一行（名字才不用等）");
        assert_eq!(k.name, "示例设备", "名字当场就有（上次知道的名字）");
        assert_eq!(
            (k.ip.as_str(), k.port),
            ("10.0.0.7", 3890),
            "地址也补上了：第一拍才有东西可拨"
        );
        assert!(
            !k.online,
            "状态必须新：一律从离线起步，真相只由这一趟的证据给"
        );
        assert!(!k.paired, "没有关系就是没有关系");
        assert!(
            !state
                .get_peer("dev-booked")
                .unwrap()
                .name
                .contains("旧的观察名"),
            "地址簿里那一行优先：观察记忆不能越级覆盖它"
        );
        assert_eq!(
            state.get_peer("dev-booked").unwrap().ip,
            "10.0.0.8",
            "地址同理"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 设备 id 是对端自报的：形态不合法的行不许进内存表，也不许进地址簿。
    ///
    /// 回归 P0：`mirror::identity::share_dir` 会把这枚 id 拼成镜像目录名，`..\..` 能逃出
    /// 镜像根（那一半由 `share_dir_refuses_traversal_and_absolute_ids` 盯着）。
    #[test]
    fn peer_rows_with_an_invalid_id_are_ignored() {
        let state = memory_state();
        let bad = PeerDevice {
            id: r"..\..\Users\victim".into(),
            name: "evil".into(),
            ip: "10.0.0.9".into(),
            port: 3890,
            paired: true,
            ..Default::default()
        };
        state.upsert_peer(&bad);
        state.add_peer_book(bad.clone());
        state.mark_paired(&bad);

        assert!(state.get_peer(&bad.id).is_none(), "内存表不收坏 id");
        assert!(
            !state.config.lock().peers.iter().any(|p| p.id == bad.id),
            "地址簿也不收"
        );
    }

    /// 两张表**互斥**：学到名字/地址时记进观察记忆那一档（未配对、且不在地址簿）；
    /// 进了地址簿（配对 / 手动添加）就把那一格删掉，出了地址簿（解除配对）再记回来。
    #[test]
    fn the_known_table_and_the_address_book_never_hold_the_same_id() {
        let state = memory_state();
        // 未配对、也不在地址簿：学到名字与地址 → 记进观察记忆。
        state.upsert_peer(&PeerDevice {
            id: "dev-n".into(),
            name: "示例设备".into(),
            ip: "10.0.0.7".into(),
            port: 3890,
            ..Default::default()
        });
        let k = state
            .config
            .lock()
            .known_peer("dev-n")
            .cloned()
            .expect("该记下来");
        assert_eq!(
            (k.name.as_str(), k.ip.as_str(), k.port),
            ("示例设备", "10.0.0.7", 3890)
        );

        // 值没变就不再算"写"（调用方据此不落盘）。
        let before = state.config.lock().known_peers.len();
        state.note_known_peer("dev-n");
        assert_eq!(state.config.lock().known_peers.len(), before);

        // 配对（关系接管）→ 那一格删掉。
        state.mark_paired(&PeerDevice {
            id: "dev-n".into(),
            name: "示例设备".into(),
            ip: "10.0.0.7".into(),
            port: 3890,
            paired: true,
            ..Default::default()
        });
        assert!(
            state.config.lock().known_peer("dev-n").is_none(),
            "进了地址簿就从观察记忆里删掉"
        );
        assert!(
            state.config.lock().peers.iter().any(|p| p.id == "dev-n"),
            "关系那一份在"
        );

        // 解除配对（关系没了）→ 观察记忆接手。
        state.unpair("dev-n");
        assert!(
            state.config.lock().known_peer("dev-n").is_some(),
            "出了地址簿就记回观察记忆"
        );
        assert!(!state.config.lock().peers.iter().any(|p| p.id == "dev-n"));

        // 已配对的 id 不许被写进观察记忆（那条路由关系表自己维护）。
        state.mark_paired(&PeerDevice {
            id: "dev-p2".into(),
            name: "PC-P2".into(),
            ip: "10.0.0.12".into(),
            port: 3890,
            paired: true,
            ..Default::default()
        });
        state.note_known_peer("dev-p2");
        assert!(state.config.lock().known_peer("dev-p2").is_none());

        // 占位名（= 设备 id）不写：那不是名字。
        state.upsert_peer(&PeerDevice {
            id: "dev-ph".into(),
            name: "dev-ph".into(),
            ..Default::default()
        });
        state.note_known_peer("dev-ph");
        assert!(state.config.lock().known_peer("dev-ph").is_none());
    }

    /// 设备级链路记忆（"上次是怎么够着它的"）也补一行：**哪怕不知道它叫什么、在哪** ——
    /// 启动第一拍就会去借那条记得的路，于是"需要中转的设备"不必先自己开口。
    #[test]
    fn a_remembered_relay_link_gets_a_row_at_boot() {
        let dir = std::env::temp_dir().join(format!("orbdrop_relayrow_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut config = crate::config::test_config();
        config.device_id = "me".into();
        config.set_relay_link(
            "dev-via",
            "relay-c",
            "192.168.50.0/24",
            "2026-09-30T12:00:00Z",
        );
        // 只有一条链路记忆、名字与地址都没有的那种：也要补行（不然没有任何线索）。
        crate::config::save_config_at(&dir.join("config.toml"), &config).unwrap();

        let loaded = crate::config::load_config_at(&dir.join("config.toml"));
        let state = AppState::new(loaded, dir.join("config.toml"));
        let row = state
            .get_peer("dev-via")
            .expect("记得怎么够着它的设备也要有一行");
        assert_eq!(row.name, "dev-via", "名字先拿 id 顶着（探到之后再填）");
        assert!(row.ip.is_empty() && row.port == 0, "地址不知道就是不知道");
        assert!(!row.online, "状态仍然从离线起步");
        // 关掉记忆就没有条目，也就不会有这一行（与"每次现开"的设计一致）。
        let _ = std::fs::remove_dir_all(&dir);
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
            &[discovered(
                "d",
                "192.168.50.8",
                Some("192.168.50.0/24"),
                true,
            )],
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
        let link = state
            .config
            .lock()
            .relay_link("b")
            .cloned()
            .expect("设备级链路已落盘");
        assert_eq!(link.via_id, "c");
        assert_eq!(link.via_cidr, "192.168.50.0/24");

        // 「记忆」的意义就是跨重启：从配置文件重新读一遍。
        let back = crate::config::load_config_at(&state.config_path);
        assert_eq!(
            back.relay_route("192.168.1.0/24")
                .map(|r| r.via_cidr.clone()),
            Some("192.168.50.0/24".into())
        );
        assert_eq!(
            back.relay_links.get("b").map(|l| l.via_id.clone()),
            Some("c".into())
        );

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
        let Some(net) = crate::subnet::host_nets()
            .first()
            .map(|c| c.to_cidr_string())
        else {
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
        assert_eq!(
            accepted,
            vec![net.clone()],
            "只有含本机地址、且有设备的那一条被采纳"
        );
        assert_eq!(ignored.len(), 2);
        let route = state
            .config
            .lock()
            .relay_route(&net)
            .cloned()
            .expect("策略已落库");
        assert_eq!(
            route.via_devices,
            vec!["peer-c".to_string(), "peer-d".to_string()]
        );
        assert_eq!(
            route.via_paired,
            vec!["peer-c".to_string()],
            "只记本机**确实**配对过的那台"
        );
        // 池里的设备并进了设备列表（内存行），但**不写地址簿** —— 那是别人的段，不是本机的关系。
        assert!(state.get_peer("peer-d").is_some(), "不先认识地址就拨不出去");
        assert!(
            !state.config.lock().peers.iter().any(|p| p.id == "peer-d"),
            "反向记录不该往地址簿里塞设备（peer-c 在地址簿里是本用例自己配的对）"
        );
    }

    fn route_record(net: &str, via_cidr: &str, id: &str, ip: &str) -> crate::relay::RouteRecord {
        crate::relay::RouteRecord {
            target_cidr: net.to_string(),
            via_cidr: via_cidr.to_string(),
            via_devices: vec![crate::relay::RouteDevice {
                id: id.to_string(),
                name: id.to_uppercase(),
                ip: ip.to_string(),
                port: 3890,
            }],
            via_paired: Vec::new(),
            confirmed_at: None,
        }
    }

    fn first_own_net() -> Option<String> {
        crate::subnet::host_nets()
            .first()
            .map(|c| c.to_cidr_string())
    }

    /// 回归（写放大）：内容没变的刷新重推**不该重写 config.toml**。
    ///
    /// 对端每 5 分钟会把没变过的记录重推一次（`ROUTE_REFRESH_MIN`）。那一拍只该刷新内存里的
    /// `seen_at`（供 TTL 判断），不该把整份配置再写一遍盘 —— 局域网里谁都能发这条报文时，
    /// 那是一个可放大的写盘面。
    #[test]
    fn route_record_refresh_does_not_rewrite_the_config_file() {
        let state = memory_state();
        let Some(net) = first_own_net() else {
            return;
        };
        let record = route_record(&net, "10.44.0.0/16", "relay-c", "10.44.0.7");
        state.apply_relay_routes("peer-a", std::slice::from_ref(&record));
        let first = std::fs::read_to_string(&state.config_path).expect("第一次采纳会写盘");
        assert!(first.contains(&net), "可信来源的记录照旧落盘");

        state.apply_relay_routes("peer-a", std::slice::from_ref(&record));
        let second = std::fs::read_to_string(&state.config_path).expect("文件还在");
        assert_eq!(
            first, second,
            "内容没变的刷新只刷新内存里的 seen_at，不该重写 config.toml"
        );

        // 池地址真的变了 → 这一拍必须落盘（也只在这一拍记 INFO，见 `changed > 0`）。
        let changed = route_record(&net, "10.44.0.0/16", "relay-c", "10.44.0.9");
        state.apply_relay_routes("peer-a", std::slice::from_ref(&changed));
        let third = std::fs::read_to_string(&state.config_path).expect("内容变化会写盘");
        assert_ne!(second, third, "路由内容真的变了就该重写 config.toml");
        assert!(third.contains("10.44.0.9"));
    }

    /// 未认证来源的记录只做**本进程内**的临时提示：不落盘；Bearer 来源写同一段时升格。
    #[test]
    fn unverified_route_record_is_an_ephemeral_hint() {
        let state = memory_state();
        let Some(net) = first_own_net() else {
            return;
        };
        let record = route_record(&net, "10.44.0.0/16", "relay-c", "10.44.0.7");

        let (accepted, _) = state.apply_relay_route_hints("phone-a", std::slice::from_ref(&record));
        assert_eq!(accepted, vec![net.clone()]);
        assert!(state.is_ephemeral_route(&net));
        let raw = std::fs::read_to_string(&state.config_path).unwrap_or_default();
        assert!(
            !raw.contains(&net),
            "未认证来源的记录不该写进 config.toml: {raw}"
        );

        // 可信来源（Bearer 反解出的身份）写同一段：升格为可落盘的记忆。
        let (accepted, _) = state.apply_relay_routes("peer-a", std::slice::from_ref(&record));
        assert_eq!(accepted, vec![net.clone()]);
        assert!(!state.is_ephemeral_route(&net));
        let raw = std::fs::read_to_string(&state.config_path).expect("升格后会写盘");
        assert!(raw.contains(&net), "可信来源的记录照旧落盘");
    }

    /// 未认证来源**不许改写已经落盘的记忆**：策略与池都保持可信来源写下的那一份。
    #[test]
    fn unverified_route_record_never_overwrites_remembered_route() {
        let state = memory_state();
        let Some(net) = first_own_net() else {
            return;
        };
        let trusted = route_record(&net, "10.44.0.0/16", "relay-c", "10.44.0.7");
        state.apply_relay_routes("peer-a", std::slice::from_ref(&trusted));

        let hint = route_record(&net, "10.99.0.0/16", "evil-relay", "10.99.0.9");
        let (accepted, ignored) =
            state.apply_relay_route_hints("phone-a", std::slice::from_ref(&hint));
        assert!(accepted.is_empty(), "可信策略存在时未认证记录一律丢弃");
        assert_eq!(ignored, vec![net.clone()]);

        let route = state
            .config
            .lock()
            .relay_route(&net)
            .cloned()
            .expect("可信策略还在");
        assert_eq!(route.via_cidr, "10.44.0.0/16");
        assert_eq!(route.via_devices, vec!["relay-c".to_string()]);
        assert!(!route.ephemeral);
        assert!(
            state.get_peer("evil-relay").is_none(),
            "被丢弃的记录也不该把设备并进设备表"
        );
    }

    /// 未认证来源的提示到期即收（路由与池一起走）。
    #[test]
    fn ephemeral_route_retires_after_ttl() {
        let state = memory_state();
        let Some(net) = first_own_net() else {
            return;
        };
        let hint = route_record(&net, "10.44.0.0/16", "relay-c", "10.44.0.7");
        let (accepted, _) = state.apply_relay_route_hints("phone-a", std::slice::from_ref(&hint));
        assert_eq!(accepted, vec![net.clone()]);
        assert!(!state.pool_for(&net).is_empty(), "提示先能用于选路");

        state.age_ephemeral_routes(EPHEMERAL_ROUTE_TTL + Duration::from_secs(1));
        state.sweep_relay_memory();
        assert!(
            state.config.lock().relay_route(&net).is_none(),
            "到期后策略收走"
        );
        assert!(state.pool_for(&net).is_empty(), "池跟着策略一起收走");
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

    /// 探活来源学地址的判据：必须是**唯一一台**未配对、已观察过同一枚证书、地址确实
    /// 变了、且来源不是本机/另一台已知设备的行。
    #[test]
    fn probe_source_candidate_only_considers_unpaired_known_identities() {
        let state = memory_state();
        let fp = "aa".repeat(32);
        state.upsert_peer(&PeerDevice {
            id: "a".into(),
            name: "PC-A".into(),
            ip: "10.0.0.5".into(),
            port: 4000,
            paired: false,
            ..Default::default()
        });
        assert!(state.note_observed_fingerprint("a", &fp));

        let source: std::net::IpAddr = "10.0.0.9".parse().unwrap();
        assert_eq!(
            state.probe_source_candidate(source, &fp),
            Some(("a".to_string(), 4000))
        );
        assert_eq!(
            state.probe_source_candidate("10.0.0.5".parse().unwrap(), &fp),
            None,
            "来源就是已存地址：没有可学的"
        );
        assert_eq!(
            state.probe_source_candidate(source, &"bb".repeat(32)),
            None,
            "没观察过这枚证书"
        );

        // 已配对的行走 token + 凭据指纹，不走这条观察值通道。
        state.upsert_peer(&PeerDevice {
            id: "b".into(),
            name: "PC-B".into(),
            ip: "10.0.0.6".into(),
            port: 4000,
            paired: true,
            ..Default::default()
        });
        assert!(state.note_observed_fingerprint("b", &"cc".repeat(32)));
        assert_eq!(
            state.probe_source_candidate(source, &"cc".repeat(32)),
            None,
            "已配对行不归这条路管"
        );
    }

    /// 核实通过的地址更新只动未配对行；已配对行仍然只由配对/凭据那一档决定地址。
    #[test]
    fn verified_address_updates_unpaired_rows_only() {
        let state = memory_state();
        state.upsert_peer(&PeerDevice {
            id: "a".into(),
            name: "PC-A".into(),
            ip: "10.0.0.5".into(),
            port: 4000,
            paired: false,
            ..Default::default()
        });
        state.apply_verified_address("a", "10.0.0.9", 5000);
        let row = state.get_peer("a").expect("设备行还在");
        assert_eq!(
            (row.ip.as_str(), row.port, row.online),
            ("10.0.0.9", 5000, true)
        );

        state.upsert_peer(&PeerDevice {
            id: "b".into(),
            name: "PC-B".into(),
            ip: "10.0.0.6".into(),
            port: 4000,
            paired: true,
            ..Default::default()
        });
        state.apply_verified_address("b", "10.0.0.10", 5000);
        let row = state.get_peer("b").expect("设备行还在");
        assert_eq!(
            (row.ip.as_str(), row.port),
            ("10.0.0.6", 4000),
            "已配对行不许被探活来源改地址"
        );
    }

    /// "这枚 token 它还认不认"只读一条：id 对得上 + `pairedAs` 有没有值。
    ///
    /// 这条判定的后果很实：`Dead` 会把设备行标成「需配对」，并让对端发来的配对邀请
    /// 放行到通知里。所以"没问到""地址上是别人"都必须是 `Unknown`，不能误判成 `Dead`
    /// —— 那会让一台好设备凭空变成"关系已失效"。
    #[test]
    fn token_liveness_needs_a_matching_id_and_a_verdict() {
        // 它认这枚 token：配对还活着
        assert_eq!(
            token_liveness(Some(&identity("me", Some("me"))), "me"),
            TokenLiveness::Alive
        );
        // 它答话了、id 也对，但不认这枚 token
        assert_eq!(
            token_liveness(Some(&identity("me", None)), "me"),
            TokenLiveness::Dead
        );
        // 没问到（超时 / 没答话）
        assert_eq!(token_liveness(None, "me"), TokenLiveness::Unknown);
        // 地址上换了设备：那条路由 `apply_identity_reconcile` 走，不是"关系失效"
        assert_eq!(
            token_liveness(Some(&identity("new", None)), "me"),
            TokenLiveness::Unknown
        );
        // id 问不出来（老版本 / 中间有代理）同样不下结论
        assert_eq!(
            token_liveness(Some(&identity("", None)), "me"),
            TokenLiveness::Unknown
        );
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

    /// **地址簿里落盘的 `online` 不当首帧真相**：重启后本机一台都还没探过，那个值只是
    /// 上一次运行留下来的残留（`set_peer_online` 从不回写它）。全信它会同时得到两种假象
    /// —— 真不在网的设备顶着「在线」、真在网的设备先画成离线，而后者要等第一拍探活才
    /// 纠正（真机报的"启动后 A 与 B 明明同网段，A 却先写离线、过几秒才转在线"）。
    /// 真相只由启动即跑的第一拍探活与链路建立给出，见 `probe_peers_once` 的调用点（`lib.rs`）。
    #[test]
    fn persisted_online_is_not_the_first_frame() {
        let dir = std::env::temp_dir().join(format!("orbdrop_boot_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let state = Arc::new(AppState::new(
            crate::config::Config {
                device_id: "own-boot".into(),
                device_name: "OWN".into(),
                peers: vec![
                    // 落盘写着"在线"的那一行（上次运行留下的，本机这一趟还没探过）
                    PeerDevice {
                        id: "boot-online".into(),
                        name: "P1".into(),
                        ip: "192.168.1.9".into(),
                        port: 3890,
                        online: true,
                        paired: true,
                        ..Default::default()
                    },
                    // 没有地址的那一行（配对时就没学到可拨地址）
                    PeerDevice {
                        id: "boot-no-addr".into(),
                        name: "P2".into(),
                        ip: String::new(),
                        port: 0,
                        online: false,
                        paired: true,
                        ..Default::default()
                    },
                ],
                ..Default::default()
            },
            dir.join("config.toml"),
        ));
        for p in state.peers.lock().iter() {
            assert!(!p.online, "首帧不许拿落盘的 online 当真相：{}", p.id);
        }
        // 地址簿里那份**一个字都不动**：它是用户的记录，不是"此刻在不在"——下一趟启动照样
        // 从离线起步，再被那一趟的探活纠正。
        assert!(
            state
                .config
                .lock()
                .peers
                .iter()
                .any(|p| p.id == "boot-online" && p.online),
            "落盘的 online 只是不作数，不该被重写"
        );
    }

    /// 对端「共享项变更」的节流（`share_changed_due`）：窗口内第一条**立刻**过（正常场景零延迟），
    /// 之后的重复一律吞掉；窗口过了又放行；节流按共享项各算各的。
    #[test]
    fn share_changed_is_throttled_per_share() {
        let state = memory_state();
        assert!(
            state.share_changed_due("share-a"),
            "第一条必须立刻上报 —— 正常场景（丢一个文件进共享目录）不能有延迟"
        );
        for _ in 0..100 {
            assert!(
                !state.share_changed_due("share-a"),
                "窗口里的重复一律吞掉（实测一轮构建 1300 条/秒）"
            );
        }
        assert!(state.share_changed_due("share-b"), "节流按共享项各算各的");
        assert!(!state.share_changed_due("share-b"));

        // 时间往前走一个窗口（直接改时间戳，不 sleep）：又该放行。
        *state
            .share_changed_at
            .lock()
            .get_mut("share-a")
            .expect("刚写过") -= SHARE_CHANGED_THROTTLE * 2;
        assert!(state.share_changed_due("share-a"), "过了窗口就该再放行一次");
    }

    /// 设备行的预算（[`MAX_UNBOOKED_ROWS`]）：**没有归宿**的行（不在地址簿、没配对、不在
    /// 两档 TTL 里、不在线、没被 mDNS 看见）才是别人能无限往里塞的那一类 —— 只收它们，
    /// 收的是最老的那几行；有归宿的一个都不许动（那是用户眼里的"设备没了"）。
    #[test]
    fn unbooked_rows_are_kept_within_budget() {
        let state = memory_state();

        // 一行"有归宿"的：在地址簿里，而且此刻在线 —— 预算再紧也不许碰它。
        state.add_peer_book(PeerDevice {
            id: "booked".into(),
            name: "BOOKED".into(),
            ip: "10.198.1.5".into(),
            port: 3890,
            online: true,
            paired: false,
            ..Default::default()
        });

        // 一批"没有归宿"的（未配对设备每 5s 探活走到 `observe_verified_addr` 时的样子），
        // 靠前的先建 → 也就是最老的。
        let extra = MAX_UNBOOKED_ROWS + 5;
        for i in 0..extra {
            state.observe_verified_addr(&format!("ghost-{i:03}"), "10.198.9.9", 3890);
        }
        // 每一行都过了预算（`observe_verified_addr` 建新行之后就会跑一次），所以收到上限为止。
        let ids: Vec<String> = state.peers.lock().iter().map(|p| p.id.clone()).collect();
        let ghosts = ids.iter().filter(|id| id.starts_with("ghost-")).count();
        assert_eq!(
            ghosts, MAX_UNBOOKED_ROWS,
            "没有归宿的行收到预算为止：{ids:?}"
        );
        assert!(
            ids.iter().any(|id| id == "booked"),
            "地址簿里那行不许被收掉"
        );
        assert!(
            !ids.iter().any(|id| id == "ghost-000"),
            "先建的那几行（最老的）先走：{ids:?}"
        );
        assert!(
            ids.iter()
                .any(|id| id == &format!("ghost-{:03}", extra - 1)),
            "刚建的那一行必须留着"
        );

        // 反例：其它条件全中，但只要它"看得见"（mDNS 有记录）或"在线"，就不收。
        state.observe_verified_addr("seen-row", "10.198.9.10", 3890);
        state.seen.lock().insert("seen-row".into(), Instant::now());
        state.observe_verified_addr("online-row", "10.198.9.11", 3890);
        if let Some(p) = state.peers.lock().iter_mut().find(|p| p.id == "online-row") {
            p.online = true;
        }
        for i in 0..extra {
            state.observe_verified_addr(&format!("flood-{i:03}"), "10.198.9.12", 3890);
        }
        let ids: Vec<String> = state.peers.lock().iter().map(|p| p.id.clone()).collect();
        assert!(ids.iter().any(|id| id == "seen-row"), "看得见的行不许被收");
        assert!(ids.iter().any(|id| id == "online-row"), "在线的行不许被收");

        // 反例之二（`heard` 之后新增的那一格）：**只被"刚听到过"撑着、本机一条路都没有**的行
        // 照旧算"没有归宿"。`online` 现在也能由"刚听到过它"撑起来，而那一条**别人一条报文
        // 就能给**（未配对的会话信封不需要 token）—— 只认 `online`，一台换着 id 发报文的
        // 机器就能攒出一堆收不走、还要每 5s 被探活敲一次的行。
        state.note_heard_online("heard-row"); // 表里没有这一行：它连行都不该建出来
        assert!(
            state.get_peer("heard-row").is_none(),
            "载荷里的 id 不配凭空建行"
        );
        state.upsert_peer(&PeerDevice {
            id: "heard-row".into(),
            name: "heard-row".into(),
            ..Default::default()
        });
        state.note_heard_online("heard-row");
        assert!(
            state.get_peer("heard-row").unwrap().online,
            "前提：它此刻算在线"
        );
        assert!(
            !state.get_peer("heard-row").unwrap().dialable(),
            "前提：本机没有它的路"
        );
        for i in 0..extra {
            state.observe_verified_addr(&format!("heardflood-{i:03}"), "10.198.9.13", 3890);
        }
        let ids: Vec<String> = state.peers.lock().iter().map(|p| p.id.clone()).collect();
        assert!(
            !ids.iter().any(|id| id == "heard-row"),
            "只被听到过、本机一条路都没有的行照旧可收：{ids:?}"
        );
    }

    /// 只有"刚听到过"撑着的那一类行（`adopted`：探活跳过它、mDNS 从没提过它）在证据过期时
    /// 要**自己落回离线** —— 不然它会一直挂着"在线"，直到 900s 的 `ADOPTED_TTL` 把整行收走
    /// （那是"设备行永远停在在线"的新来源，本轮 `heard` 引入的）。
    #[test]
    fn an_adopted_row_falls_back_offline_when_being_heard_expires() {
        let state = memory_state();
        state.adopt_devices(
            &[crate::relay::RouteDevice {
                id: "dev-ad".into(),
                name: "AD".into(),
                ip: "10.55.0.9".into(),
                port: 3890,
            }],
            &[],
        );
        assert!(state.is_adopted("dev-ad"));
        state.note_heard_online("dev-ad");
        assert!(state.get_peer("dev-ad").unwrap().online, "刚听到过 = 在线");

        // 探活那一拍收走过期的记录 —— 同一个时刻它就该被标回离线。
        state.heard.lock().insert(
            "dev-ad".into(),
            Instant::now() - HEARD_TTL - Duration::from_secs(1),
        );
        state.prune_heard();
        assert!(
            !state.get_peer("dev-ad").unwrap().online,
            "证据过期要如实落回离线"
        );
        assert!(!state.heard_recently("dev-ad"));
    }

    fn row(id: &str, ip: &str, port: u16, paired: bool) -> PeerDevice {
        PeerDevice {
            id: id.into(),
            name: id.into(),
            ip: ip.into(),
            port,
            online: false,
            paired,
            ..Default::default()
        }
    }

    #[test]
    fn dedupe_keeps_paired_row_and_one_unpaired_at_the_same_address() {
        let state = memory_state();
        state.peers.lock().extend([
            row("paired", "10.0.0.9", 3890, true),
            row("stale-a", "10.0.0.9", 3890, false),
            row("stale-b", "10.0.0.9", 3890, false),
        ]);
        assert_eq!(state.dedupe_known_addresses(), 2);
        let ids: Vec<String> = state.peers.lock().iter().map(|p| p.id.clone()).collect();
        assert_eq!(ids, vec!["paired".to_string()]);
    }

    #[test]
    fn unpaired_row_is_forgotten_after_the_failure_limit() {
        let state = memory_state();
        state.peers.lock().push(row("dead", "10.0.0.8", 3891, false));
        for _ in 0..PROBE_FAIL_LIMIT_UNPAIRED - 1 {
            state.note_probe_outcome("dead");
        }
        assert!(state.get_peer("dead").is_some(), "未到阈值前不能忘");
        state.note_probe_outcome("dead");
        assert!(state.get_peer("dead").is_none(), "到阈值后整行忘掉");
    }
}
