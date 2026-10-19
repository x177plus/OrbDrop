//! 群聊（共享群）：名册 + 群消息的本地真相 + 给界面的快照。
//!
//! **为什么和 `chat.rs`（一对一）分开**：一对一的消息属于"与这台设备的往来"，群消息属于
//! "这个群"。硬塞进同一张表，就得在每条消息上挂一串可空字段（`group_id`/`seq`/转发标记…）
//! 再逐个判断"这条到底算哪种"；分开之后两条路各自是直的，共享的只有很短的一段
//! （投放与状态机：`chat::deliver` 与 `chat::Delivery`）。
//!
//! **为什么另立一个文件**：`chat.json` 没有版本位，老版本进程读进来（serde 静默忽略不认识的
//! 顶层键）再原样写回，就会把新增的顶层键**抹掉** —— 这件事在本仓发生过（老文件里的 `groups`
//! 字段今天被显式忽略，见 `chat.rs` 的 `a_legacy_file_with_a_groups_key_still_loads`）。
//! 名册与群消息丢了不可恢复，所以放 `groups.json`，让老版本碰不到。
//!
//! **模型**（拓扑约束见 `requirements.md:25,413-415`：仅局域网、无中继服务器、两两配对无组播）：
//! - **名册**（`Group`）是一个 LWW 寄存器：`(rev, updated_by)` 大者胜。任何成员都能改名/加减人
//!   （D5 对等），并发变更**只留一个胜者**、不做合并 —— 结果是确定的、各端收敛。
//! - **成员不需要配对**（D6=b）：名册里只有 `{id, name}`，地址不进名册（地址是"本机跟它之间
//!   的关系"，见 `routes_discover.rs` 的同一原则）。未配对成员没有长连接（拨号要求 `p.paired`），
//!   群消息只能走 HTTP 兜底那一跳。
//! - **一条群消息就是一条**：`delivery` 是"本机发给每个成员的状态"，而不是像「群发」那样
//!   每台存一份副本（`chat.rs::record_outbound`）。消息属于群，不属于某个成员关系。
//! - **幂等**：去重键 `(from, id)`。补发、重连重投、HTTP 到了但对端没回话都会重投。
//! - **顺序**：展示顺序 `(at, from, seq)` —— `at` 是发送方墙钟（跨设备不可比但 LAN 够用），
//!   `seq` 是同一发送方在这个群里的第几条（1 起），把同一个人的话钉死先后。
//! - **未读**：本地读位 `read_pos`（最后读过的那条消息 id）。**不用墙钟水位** —— 群里有多个
//!   发送方，`at` 之间不可比（一对一能用 `at <= read_at` 是因为只有两个人、方向单一）。
//! - **已读回执**：成员 M 打开群 → 对每个成员 X 发 `GroupRead { group_id, up_to }`，`up_to` =
//!   M 读到的、**X 发的**最大 `at`（与一对一的回执逐字同一个语义，只是多一个群维度）。
//! - **未知群的第一份名册要用户确认**：不认识的名册 → 落成邀请卡（`pending`），点了「加入」才
//!   成为正式的群；拒绝过的（`dismissed`）之后一律忽略。
//! - **被移出**：名册里没有本机 → `left = true`（停发、不再列出来），历史留在盘上。
//!
//! **离线补发**：与一对一同一套 —— 够不着的成员那一份落 `Queued`，等它上线由
//! `chat::flush_queued` 按原顺序发出去（群里排队的那几条也排在同一个补发循环里）。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::chat::{now_ms, Delivery, Outcome};
use crate::config::{data_dir, write_atomic};
use crate::state::AppState;
use crate::ws::events::{ui, PeerInfo, WsEvent};

/// 每个群最多留多少条（与一对一的 `chat::MAX_PER_PEER` 同一个取舍：砍最够不着的那一头）。
const MAX_PER_GROUP: usize = 500;

/// 一个群最多几个人。**不是随手定的**：发送是"每台各一份"（无组播），名册变更同理 ——
/// 每加一个人就多一份投递、多一份失败面。
const MAX_MEMBERS: usize = 32;

/// 最多同时挂几张邀请卡（别人狂建群不该把界面撑爆）。
const MAX_PENDING: usize = 32;

/// 邀请还挂在卡上、群里就已经有消息进来了：**先攒着**（用户点「加入」那一刻并进会话，
/// 点「不加入」就跟着丢掉）。
///
/// 为什么要攒：名册与消息是两条独立投递，邀请卡可能正躺在界面上等人点，而这条消息已经到
/// 门口了 —— 直接丢掉就是"我还没点加入，别人说的话就永久消失了"（不诚实，也没法补救）。
/// 上限很小：这是一张卡片的等待窗口，不是离线队列。
const MAX_ORPHANS: usize = 50;
const MAX_ORPHAN_GROUPS: usize = 8;

/// 一次补拉最多回多少条（回到的那些也照样受 `MAX_PER_GROUP` 截断）。
const MAX_SYNC_MESSAGES: usize = 200;

/// 一次补拉最多点名几个发送方的"洞"，每个发送方最多点几条。洞是补的边角料（正常路径下
/// 一条都不该有），所以上限小：它只是"我明确知道缺这几条"，不是"把整段都搬过来"。
const MAX_HOLE_SENDERS: usize = 8;
const MAX_HOLES_PER_SENDER: usize = 64;

/// 连续追批的上限：锚点 > 0 时（我手上已经有东西，只是落后）最多来回这么几轮；
/// **冷启动（一条都没有）只回一批**——新入群的人看到最近一批就够，别把他灌满 500 条。
const MAX_SYNC_ROUNDS: u32 = 6;

/// 同一（群，设备）两次发问的最小间隔：补拉是幂等的（重复投递由 `(from, id)` 吸收），
/// 但探活每 5s 一拍、链路一上线也叫一次，没有这个闸门就成了持续刷接口。
const SYNC_COOLDOWN: std::time::Duration = std::time::Duration::from_secs(3);

/// 群消息最多被中转几跳（发送方自己发 = 0 跳）。跨网段时缺的那一段由链路上的成员帮着搬，
/// 但**不能无限传下去**：环路由 `(from, id)` 与"只在第一次见到这条时转发"一起挡住，
/// 跳数则是最后那道闸。
pub const MAX_HOPS: u8 = 3;

/// `groups.json` 的形状版本：以后加字段/换语义时用它区分（`chat.json` 没这一栏，才吃着
/// 老版本抹字段的亏）。
const FORMAT_VERSION: u32 = 1;

/// 名册里的一行。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Member {
    pub id: String,
    #[serde(default)]
    pub name: String,
}

/// 一个群的名册（本机视角的唯一真相）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Group {
    pub id: String,
    /// 群名；空 = 界面标题回落到成员名。
    #[serde(default)]
    pub name: String,
    /// 名册版本（每次变更 +1）。
    #[serde(default)]
    pub rev: u64,
    #[serde(default)]
    pub created_by: String,
    #[serde(default)]
    pub created_at: u64,
    /// 最后一次改动是谁做的（冲突裁决的第二关键字）。
    #[serde(default)]
    pub updated_by: String,
    #[serde(default)]
    pub updated_at: u64,
    /// **含本机**（"这个群有谁"是自洽的；视图层再滤掉本机）。
    #[serde(default)]
    pub members: Vec<Member>,
    /// 本机**被移出**这个群（名册里没有本机了）：停发、不再列出来，历史留在盘上。
    /// 本机视角的标记，**不跟着名册走**。
    #[serde(default)]
    pub left: bool,
    /// 名册里那些**明确拒绝收这个群的消息**的成员 id（本机看到的：`group_text_rejected`
    /// 那一条回来时记下，见 `on_group_text_rejected`）。
    ///
    /// **为什么不改名册**：名册是 LWW 共享物，"那台设备拒绝了"是**本机的观察** —— 写进名册
    /// 就等于替所有成员改了名单，还会跟着 LWW 跟别人的版本打架（而且拒绝它的那台设备自己
    /// 未必知道别人怎么看）。所以它只活在本机这一份里：界面上把那枚胶囊标出来，要不要把它
    /// 移出名单由用户按那颗 ✕ 决定。
    ///
    /// 老文件没有这一栏 → 空表（那时也没人记过这件事）。
    #[serde(default)]
    pub declined: Vec<String>,
}

impl Group {
    fn is_member(&self, peer_id: &str) -> bool {
        self.members.iter().any(|m| m.id == peer_id)
    }

    /// 这个成员是不是**拒绝收本群消息**的（本机看到的）。
    fn has_declined(&self, peer_id: &str) -> bool {
        self.declined.iter().any(|d| d == peer_id)
    }

    /// 除了 `except` 之外的成员 id（投递目标）。
    fn others(&self, except: &str) -> Vec<String> {
        self.members.iter().filter(|m| m.id != except).map(|m| m.id.clone()).collect()
    }

    /// 名册的线上/落盘形状。
    fn payload(&self) -> RosterPayload {
        RosterPayload {
            id: self.id.clone(),
            name: self.name.clone(),
            rev: self.rev,
            updated_by: self.updated_by.clone(),
            updated_at: self.updated_at,
            members: self
                .members
                .iter()
                .map(|m| PeerInfo { id: m.id.clone(), name: m.name.clone() })
                .collect(),
        }
    }
}

/// 群里的一条消息。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupMsg {
    pub id: String,
    /// 发送方设备 id（本机发的 = 本机 id）。
    pub from: String,
    /// 发送方显示名（**收方填**：优先本机设备表，回落载荷里自称的名字）。
    #[serde(default)]
    pub from_name: String,
    pub text: String,
    /// 发送方的墙钟（epoch 毫秒）：跨发送方排序用它。
    pub at: u64,
    /// 同一个发送方在这个群里的第几条（1 起）。
    #[serde(default)]
    pub seq: u64,
    /// **仅本机发出的消息有意义**：成员 id → 这一份的投递状态。收来的消息是空的。
    #[serde(default)]
    pub delivery: HashMap<String, Delivery>,
    /// 这条是不是**图片消息**（图片是消息内容，不是文件 —— 与一对一那条路同一套语义，
    /// 见 `crate::blobs`）。老文件里没有这个键 = 文字消息。
    #[serde(default)]
    pub image: bool,
    /// 图片消息的内容文件名（"图片-20261013-153012.png"）：扩展名由它决定。
    #[serde(default)]
    pub name: String,
    /// 图片消息的字节数（界面上写"多大"）。
    #[serde(default)]
    pub size: u64,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GroupConvo {
    #[serde(default)]
    messages: Vec<GroupMsg>,
    /// 本机读到哪条了（那条消息 id；`None` = 一条都没读过）。
    #[serde(default)]
    read_pos: Option<String>,
    /// 读过本机消息的人：reader 设备 id → 它读到的"本机发的"最大 `at`。
    #[serde(default)]
    read_by: HashMap<String, u64>,
    /// 这个群本机**已经追过一次**（冷启动那一批补拉走完了）。
    ///
    /// 冷启动（本机在这个群里一条消息都没有）要问一次"从头发一批给我"才追得上；但这件事
    /// 只能做一次 —— 否则每一轮定时补拉都会再要一批回来（内容一样、被去重，纯浪费带宽）。
    #[serde(default)]
    bootstrapped: bool,
    /// 每个发送方**被本地裁剪掉的下界**：`seq <= 这个号`的消息是本机为了控制体积主动丢掉的
    /// （见 `prune`），**不是"没收到"**。
    ///
    /// 补拉找洞要从这个下界之上开始（`holes_in`）：否则超过每群上限之后，被裁掉的那些号会被
    /// 永远当成缺口 —— 顶部常驻一条"有 N 条消息未能同步（来源设备不在线）"的假告警，而且每收到
    /// 一条新消息都会再点名去要那几个永远不会回来的号（F-99 的 `holes` 是逐条点名，点的是
    /// 要不到的名字），变成一条持续的后台流量。
    #[serde(default)]
    pruned_below: HashMap<String, u64>,
}

/// 补拉闸门（内存，重启即忘）：同一（群，设备）的节奏与轮数。
#[derive(Debug, Default, Clone, Copy)]
struct SyncGate {
    last: Option<std::time::Instant>,
    /// **连续追批**已经追了几轮（"对方说还有更多"那一串）；普通一问会把它清零。
    ///
    /// 旧字段叫 `rounds`，普通一问也累加、唯一的清零点在"对方回话"（`sync_gate_note`）——
    /// 于是一台**有地址但离线**的成员，被 60s 兜底扫每轮 +1，约 6 分钟后就永远不再被问
    /// （`rounds >= MAX_SYNC_ROUNDS` 之后 `sync_gate_note` 再也不会被调用，到进程重启为止），
    /// F-99 承诺的"必然收敛"静默失效。现在只有追批占额度，普通一问把它重置。
    chase_rounds: u32,
}

/// 收到但**还没被用户确认**的群邀请。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingInvite {
    /// 线上与落盘的键是 **`id`**（就是名册里那个群 id）：界面读的是 `PendingGroup.id`，
    /// 并原样把它交给 `group_join` / `group_dismiss`。
    ///
    /// 这里曾经直接让字段名 camelCase 出去 → 键叫 `groupId`，而前端按 `id` 读 → 读到
    /// `undefined` → `invoke('group_join', { id: undefined })` 序列化时那个键**消失**，
    /// 后端报 `missing required key id`：「加入」和「拒绝」两颗按钮双双点不动。
    /// `alias` 兜住已经落盘的老文件（里面写的是 `groupId`）。
    #[serde(rename = "id", alias = "groupId")]
    pub group_id: String,
    #[serde(default)]
    pub name: String,
    /// 邀请人（名册是从它那儿来的）。
    pub from: String,
    #[serde(default)]
    pub from_name: String,
    /// 名册里的成员（不含本机）。
    #[serde(default)]
    pub members: Vec<Member>,
    pub at: u64,
    /// 收到的名册本身：用户点「加入」时要**原样**成为本地名册（版本、谁改的都不能丢，
    /// 否则之后的名册会按一个假的本地版本比大小）。
    #[serde(default)]
    rev: u64,
    #[serde(default)]
    updated_by: String,
    #[serde(default)]
    updated_at: u64,
}

/// 邀请卡**给界面**的那份形状。
///
/// 与落盘那份（`PendingInvite`）唯一的不同是多一个 `buffered`，而它只能活在这里：攒着几条
/// 是"此刻"从 `orphans` 里数出来的事实，存进 `groups.json` 只会是一个过期的数（新消息进来
/// 时没人去改它）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PendingInviteView {
    /// 线上的键是 **`id`**（就是名册里那个群 id）：界面读的是 `PendingGroup.id`，并原样把它
    /// 交给 `group_join` / `group_dismiss`。这里曾经 camelCase 出去叫 `groupId`，而前端按
    /// `id` 读 → 读到 `undefined` → `invoke` 里那个键**直接消失** → 两颗按钮双双点不动。
    #[serde(rename = "id")]
    pub group_id: String,
    pub name: String,
    /// 邀请人（名册是从它那儿来的）。
    pub from: String,
    pub from_name: String,
    /// 名册里的成员（不含本机）。
    pub members: Vec<Member>,
    pub at: u64,
    /// 邀请挂着期间已经到门口的消息条数（`MAX_ORPHANS`）：点「加入」那一刻它们并进会话。
    pub buffered: usize,
}

/// 名册（线上事件 `group_roster` 的载荷，也是落盘那份邀请的原始形状）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RosterPayload {
    pub id: String,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub rev: u64,
    #[serde(default)]
    pub updated_by: String,
    #[serde(default)]
    pub updated_at: u64,
    #[serde(default)]
    pub members: Vec<PeerInfo>,
}

/// 补拉：某个发送方身上"我**明确知道**缺了哪几条"（`seq` 号点名）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncHole {
    pub from: String,
    #[serde(default)]
    pub seqs: Vec<u64>,
}

/// 补拉搬一条消息时在线上传的那几个字段（与 `GroupText` 同一套语义，少一个 `rev`：
/// 名册跟着 `GroupSyncPost` 单独走）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupMsgWire {
    pub id: String,
    pub from: String,
    #[serde(default)]
    pub from_name: String,
    pub text: String,
    pub at: u64,
    #[serde(default)]
    pub seq: u64,
    /// 图片消息那几格（与 `GroupMsg` 同一套语义）：`image` / `name` / `size` 是元数据，
    /// `data` 是**内容本身**（base64）。
    ///
    /// 补拉是"把手上有的发回去"，所以内容得跟着走（不然补回来的那条只有一个标题）；
    /// 服务方手上那份内容要是不在盘上了（被裁剪过），`data` 就是空串 —— 收方拿到一条
    /// **没有内容的图片消息**（气泡上如实写"读不出来"，见 `group::on_sync_post`）。
    #[serde(default)]
    pub image: bool,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub size: u64,
    #[serde(default)]
    pub data: String,
}

/// 名册吃进来之后发生了什么（命令与测试都看它）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RosterOutcome {
    /// 本机已知的群：名册更新了。
    Updated,
    /// 没见过的群 → 挂一张邀请卡，还没成为正式的群。
    Pending,
    /// 拒绝过的、或版本不比本地新的：什么都没做。
    Ignored,
}

#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct GroupFile {
    #[serde(default)]
    version: u32,
    #[serde(default)]
    groups: Vec<Group>,
    #[serde(default)]
    convos: HashMap<String, GroupConvo>,
    #[serde(default)]
    pending: Vec<PendingInvite>,
    /// 明确拒绝过的群 id：之后收到它的事件直接忽略（否则那张卡会一直弹回来）。
    #[serde(default)]
    dismissed: Vec<String>,
    /// 邀请卡等着人点的那段时间里到达的群消息（键 = 群 id）。见 `MAX_ORPHANS`。
    #[serde(default)]
    orphans: HashMap<String, Vec<GroupMsg>>,
    /// 本机在每个群里的**下一个** `seq`。不从消息里推导：历史会被截断（`MAX_PER_GROUP`），
    /// 截断之后 `max(seq)` 就比真实的小了，接着用会发出重复的号。
    #[serde(default)]
    my_seq: HashMap<String, u64>,
}

pub struct GroupStore {
    path: PathBuf,
    /// 图片消息的**内容**目录（见 `crate::blobs`）：与一对一那份**同一个目录**
    /// （`<数据目录>/chat-blobs`）—— 两边的消息 id 都是 uuid，内容文件按 id 认人，
    /// 分开放只会让"谁引用着它"多一份账（启动扫孤儿要把两边合起来看）。
    blobs_root: PathBuf,
    inner: Mutex<GroupFile>,
    /// `(群, 设备)` → 本机**已经告诉过它**的名册版本（内存即可：重启后多发一次名册无害）。
    ///
    /// 为什么要记：名册是"建群/改名单"那一刻广播出去的，对端当时离线就永远收不到了。
    /// 补发/补拉那一刻对一遍，把落后的补上（见 `needs_roster`）。
    told: Mutex<HashMap<(String, String), u64>>,
    /// `(群, 设备)` → 补拉的节奏（内存）。
    sync: Mutex<HashMap<(String, String), SyncGate>>,
}

impl GroupStore {
    pub fn new() -> Self {
        Self::with_path(data_dir().join("groups.json"))
    }

    /// 指定文件的构造（单测用：不碰真实数据目录）。
    pub fn with_path(path: PathBuf) -> Self {
        let inner = std::fs::read_to_string(&path)
            .ok()
            .and_then(|s| serde_json::from_str::<GroupFile>(&s).ok())
            .unwrap_or_default();
        let blobs_root = crate::blobs::root_of(&path);
        Self {
            path,
            blobs_root,
            inner: Mutex::new(inner),
            told: Mutex::new(HashMap::new()),
            sync: Mutex::new(HashMap::new()),
        }
    }

    /// 图片消息的内容目录（与 `ChatStore` 那个是同一个：见上面那段说明）。
    pub fn blobs_root(&self) -> &std::path::Path {
        &self.blobs_root
    }

    /// 手上还引用着的**图片消息 id**（启动时扫孤儿用，见 `blobs::sweep_in`）。
    pub fn image_ids(&self) -> std::collections::HashSet<String> {
        let guard = self.inner.lock();
        let convo = guard.convos.values().flat_map(|c| c.messages.iter());
        let orphan = guard.orphans.values().flatten();
        convo
            .chain(orphan)
            .filter(|m| m.image)
            .map(|m| m.id.clone())
            .collect()
    }

    // ── 读 ────────────────────────────────────────────────────────────────

    /// 界面上那份群列表（不含被移出的、也不含还没确认的邀请）。
    pub fn views(&self, own_id: &str) -> Vec<GroupView> {
        let guard = self.inner.lock();
        guard
            .groups
            .iter()
            .filter(|g| !g.left)
            .map(|g| view_of(g, guard.convos.get(&g.id), own_id, &self.blobs_root))
            .collect()
    }

    /// 邀请卡（成员里不含本机）；`buffered` 是此刻还攒着的条数。
    pub fn pending_views(&self, own_id: &str) -> Vec<PendingInviteView> {
        let guard = self.inner.lock();
        guard
            .pending
            .iter()
            .map(|p| {
                let mut members = p.members.clone();
                members.retain(|m| m.id != own_id);
                PendingInviteView {
                    group_id: p.group_id.clone(),
                    name: p.name.clone(),
                    from: p.from.clone(),
                    from_name: p.from_name.clone(),
                    members,
                    at: p.at,
                    buffered: guard.orphans.get(&p.group_id).map_or(0, |v| v.len()),
                }
            })
            .collect()
    }

    /// 群未读合计（进底栏那颗角标）。
    pub fn unread_total(&self, own_id: &str) -> u32 {
        let guard = self.inner.lock();
        guard
            .groups
            .iter()
            .filter(|g| !g.left)
            .map(|g| unread_of(g, guard.convos.get(&g.id), own_id))
            .sum()
    }

    /// 名册（`None` = 没有这个群）。
    pub fn group(&self, group_id: &str) -> Option<Group> {
        self.inner.lock().groups.iter().find(|g| g.id == group_id).cloned()
    }

    pub fn is_member(&self, group_id: &str, peer_id: &str) -> bool {
        self.group(group_id).is_some_and(|g| !g.left && g.is_member(peer_id))
    }

    /// 这个群的成员设备 id（含本机）。
    pub fn members(&self, group_id: &str) -> Vec<String> {
        self.group(group_id).map(|g| g.members.iter().map(|m| m.id.clone()).collect()).unwrap_or_default()
    }

    /// 界面上那份群视图（`None` = 不在列表里）。
    pub fn view(&self, group_id: &str, own_id: &str) -> Option<GroupView> {
        let guard = self.inner.lock();
        let g = guard.groups.iter().find(|g| g.id == group_id)?;
        if g.left {
            return None;
        }
        Some(view_of(g, guard.convos.get(group_id), own_id, &self.blobs_root))
    }

    /// 本机在这个群里下一条要用的 `seq`（1 起，只增）。
    pub fn peek_seq(&self, group_id: &str) -> u64 {
        self.inner.lock().my_seq.get(group_id).copied().unwrap_or(1)
    }

    /// 这个群是不是还欠着消息（有排队等对端的）。
    ///
    /// 与新消息的**顺序**有关：欠着的时候新消息也跟着排队（与一对一 `chat::has_backlog`
    /// 同一个理由 —— 后发的那条抢到前面去，对端看到的顺序就乱了）。
    pub fn has_queued(&self, group_id: &str) -> bool {
        let guard = self.inner.lock();
        guard.convos.get(group_id).is_some_and(|c| {
            c.messages.iter().any(|m| m.delivery.values().any(|d| *d == Delivery::Queued))
        })
    }

    /// 这台设备在这个群里还有没有排队等它的消息（补发循环的判据）。
    pub fn has_backlog_for(&self, peer_id: &str) -> bool {
        let guard = self.inner.lock();
        guard
            .convos
            .values()
            .any(|c| c.messages.iter().any(|m| m.delivery.get(peer_id) == Some(&Delivery::Queued)))
    }

    /// 某一条的某一份此刻是不是还排着队（补发那一步用它确认"账真的落了"）。
    pub fn has_queued_fragment(&self, group_id: &str, msg_id: &str, peer_id: &str) -> bool {
        self.inner
            .lock()
            .convos
            .get(group_id)
            .and_then(|c| c.messages.iter().find(|m| m.id == msg_id))
            .is_some_and(|m| m.delivery.get(peer_id) == Some(&Delivery::Queued))
    }

    /// 还欠着群消息的设备（启动时扫一遍，与一对一并起来补发）。
    pub fn peers_with_backlog(&self) -> Vec<String> {
        let guard = self.inner.lock();
        let mut ids: Vec<String> = Vec::new();
        for convo in guard.convos.values() {
            for m in &convo.messages {
                for (peer, d) in &m.delivery {
                    if *d == Delivery::Queued && !ids.contains(peer) {
                        ids.push(peer.clone());
                    }
                }
            }
        }
        ids.sort();
        ids
    }

    /// 最早那条还排着队的群消息（`None` = 不欠了）。
    ///
    /// 只保证**群内的先后**（跨群之间没有顺序可言）：群之间的次序按 `groups` 的插入顺序，
    /// 稳定即可。
    pub fn next_queued_for(&self, peer_id: &str) -> Option<(String, GroupMsg)> {
        let guard = self.inner.lock();
        for g in &guard.groups {
            let Some(convo) = guard.convos.get(&g.id) else { continue };
            if let Some(m) = convo.messages.iter().find(|m| m.delivery.get(peer_id) == Some(&Delivery::Queued)) {
                return Some((g.id.clone(), m.clone()));
            }
        }
        None
    }

    /// 名册还没告诉过这台设备（版本对不上、或从没说过）→ 发消息前先补一条名册。
    pub fn needs_roster(&self, group_id: &str, peer_id: &str) -> bool {
        let Some(rev) = self.group(group_id).map(|g| g.rev) else { return false };
        self.told
            .lock()
            .get(&(group_id.to_string(), peer_id.to_string()))
            .copied()
            != Some(rev)
    }

    /// 名册已经告诉过它了（**投递成功之后**才记）。
    pub fn mark_told(&self, group_id: &str, peer_id: &str) {
        if let Some(rev) = self.group(group_id).map(|g| g.rev) {
            self.told
                .lock()
                .insert((group_id.to_string(), peer_id.to_string()), rev);
        }
    }

    /// 「在途」退回「排队」（**只在启动时**）：那次 HTTP 尝试已经随进程没了。
    pub fn requeue_inflight(&self) {
        let mut guard = self.inner.lock();
        let mut changed = false;
        for convo in guard.convos.values_mut() {
            for m in convo.messages.iter_mut() {
                for d in m.delivery.values_mut() {
                    if *d == Delivery::Sending {
                        *d = Delivery::Queued;
                        changed = true;
                    }
                }
            }
        }
        if changed {
            persist(&self.path, &mut guard);
        }
    }

    // ── 写：名册 ──────────────────────────────────────────────────────────

    /// 找一份**成员完全一样**的名册（比 id 集合，顺序无关）。
    ///
    /// **不含已经退出的群**：用户退过之后再"建"同样的名单，那该是个新群 —— 他不在了，
    /// 旧群后续的名册/消息事件也不该再回到他这里（`left` 那一档的处理见 `leave`）。
    fn find_by_members(&self, members: &[Member]) -> Option<String> {
        let mut want: Vec<&str> = members.iter().map(|m| m.id.as_str()).collect();
        want.sort_unstable();
        self.inner
            .lock()
            .groups
            .iter()
            .find(|g| {
                if g.left {
                    return false;
                }
                let mut have: Vec<&str> = g.members.iter().map(|m| m.id.as_str()).collect();
                have.sort_unstable();
                have == want
            })
            .map(|g| g.id.clone())
    }

    /// 本机建群：名册含本机，`rev = 1`。返回群 id；**名单一样的群已经存在就复用那个 id**
    ///（见 `find_by_members` 的注释）。
    pub fn create(&self, own: Member, others: Vec<Member>, name: &str) -> String {
        let mut members = vec![own.clone()];
        normalize_members(&mut members, others);
        // **同一批人只该有一个群**：名单一样就复用现有的那个（不新建、也不改名 —— 改名是
        // 名册编辑那件事）。不然"再建一次同样的三个人"会生出一个成员完全相同的第二个群，
        // 消息从此分叉在两条会话里，谁也看不全。
        if let Some(existing) = self.find_by_members(&members) {
            return existing;
        }
        let id = uuid::Uuid::new_v4().to_string();
        let now = now_ms();
        let mut guard = self.inner.lock();
        guard.groups.push(Group {
            id: id.clone(),
            name: name.trim().to_string(),
            rev: 1,
            created_by: own.id.clone(),
            created_at: now,
            updated_by: own.id,
            updated_at: now,
            members,
            left: false,
            declined: Vec::new(),
        });
        guard.convos.entry(id.clone()).or_default();
        guard.my_seq.entry(id.clone()).or_insert(1);
        persist(&self.path, &mut guard);
        id
    }

    /// 本机改名/加减人：`rev + 1`（D5：任何成员都能改，冲突 LWW）。返回改后的名册。
    pub fn edit_local(&self, group_id: &str, own_id: &str, name: &str, others: Vec<Member>) -> Option<Group> {
        let mut guard = self.inner.lock();
        let now = now_ms();
        let own_name = guard
            .groups
            .iter()
            .find(|g| g.id == group_id)
            .and_then(|g| g.members.iter().find(|m| m.id == own_id).map(|m| m.name.clone()))
            .unwrap_or_default();
        let mut members = vec![Member { id: own_id.to_string(), name: own_name }];
        normalize_members(&mut members, others);
        let out = {
            let group = guard.groups.iter_mut().find(|g| g.id == group_id)?;
            group.name = name.trim().to_string();
            group.rev += 1;
            group.updated_by = own_id.to_string();
            group.updated_at = now;
            group.members = members;
            // "拒绝过"只对**还在名册里**的人有意义：名单里没有它了，那一格就跟着作废
            //（重新加回来 = 一次新的邀请，得重新看它答不答）。
            group.declined.retain(|id| group.members.iter().any(|m| &m.id == id));
            // 名册里又有本机了 → 之前"被移出"的标记作废（被重新拉回来了）。
            group.left = false;
            group.clone()
        };
        guard.convos.entry(group_id.to_string()).or_default();
        persist(&self.path, &mut guard);
        Some(out)
    }

    /// 本机退群：名册里去掉本机、`rev + 1`（这份名册要广播出去），本地把这个群删干净。
    pub fn leave(&self, group_id: &str, own_id: &str) -> Option<Group> {
        let mut guard = self.inner.lock();
        let now = now_ms();
        let out = {
            let group = guard.groups.iter_mut().find(|g| g.id == group_id)?;
            if !group.is_member(own_id) {
                return None;
            }
            group.members.retain(|m| m.id != own_id);
            group.rev += 1;
            group.updated_by = own_id.to_string();
            group.updated_at = now;
            group.clone()
        };
        // 本地删干净：用户按下的是「退出会话（本地删除并广播离开）」—— 会话该没了。
        guard.groups.retain(|g| g.id != group_id);
        let gone = guard.convos.remove(group_id).unwrap_or_default();
        guard.my_seq.remove(group_id);
        guard.pending.retain(|p| p.group_id != group_id);
        let discarded = guard.orphans.remove(group_id).unwrap_or_default();
        // 会话记录（含攒着的那几条）连同里面的图片内容一起走。
        drop_content(&gone.messages, &self.blobs_root);
        drop_content(&discarded, &self.blobs_root);
        persist(&self.path, &mut guard);
        Some(out)
    }

    /// 名册进来了（建群邀请 / 成员变更）。**日志与界面都看它返回什么。**
    pub fn apply_roster(&self, own_id: &str, roster: &RosterPayload, from: &PeerInfo) -> RosterOutcome {
        let mut members: Vec<Member> = Vec::new();
        members.extend(
            roster
                .members
                .iter()
                .filter(|m| !m.id.is_empty())
                .map(|m| Member { id: m.id.clone(), name: m.name.clone() }),
        );
        let mut unique: Vec<Member> = Vec::new();
        for m in members {
            if !unique.iter().any(|u| u.id == m.id) {
                unique.push(m);
            }
        }
        unique.truncate(MAX_MEMBERS);

        let mut guard = self.inner.lock();
        if guard.dismissed.iter().any(|d| d == &roster.id) {
            debug!(group = %roster.id, "群名册被忽略：本机明确拒绝过这个群");
            return RosterOutcome::Ignored;
        }
        // 发名册的这台设备必须**本来就是这个群的人**（P1-7）：名册是 LWW（rev 大者胜），谁都能写一个
        // 更大的 rev —— 不校验发送者身份，任何知道群 id 的局域网主机都能把别人的群改写掉
        // （改群名、踢人、把自己加进去）。
        //
        // 判据用**本机手上那份名册**，不是它自己送来的这份（攻击者当然会把自己写进去）。
        // 自己退群（广播一份"没有我"的名册）那一档也照样成立：发送者在**本机手上的旧名册**里。
        // 中转（F-100）那一跳的发送者同样是成员，所以正常路径不受影响。
        if let Some(idx) = guard.groups.iter().position(|g| g.id == roster.id) {
            if !guard.groups[idx].is_member(&from.id) {
                warn!(
                    group = %roster.id, sender = %from.id,
                    "名册被忽略：发送者不是本机手上这份名册的成员"
                );
                return RosterOutcome::Ignored;
            }
            let stale = {
                let g = &guard.groups[idx];
                // LWW：`(rev, updated_by)` 大者胜。并发变更只留一个胜者（结果是确定的、各端收敛）。
                (roster.rev, roster.updated_by.as_str()) <= (g.rev, g.updated_by.as_str())
            };
            if stale {
                return RosterOutcome::Ignored;
            }
            let left;
            {
                let g = &mut guard.groups[idx];
                g.name = roster.name.clone();
                g.rev = roster.rev;
                g.updated_by = roster.updated_by.clone();
                g.updated_at = roster.updated_at;
                g.members = unique;
                // 名册里还有本机 → 之前的"被移出"作废；没有了 → 停发（历史留着）。
                g.left = !g.is_member(own_id);
                // "拒绝过"的人被移出名单了 → 那一格作废（与 `edit_local` 同一条规矩）。
                g.declined.retain(|id| g.members.iter().any(|m| &m.id == id));
                left = g.left;
            }
            guard.convos.entry(roster.id.clone()).or_default();
            persist(&self.path, &mut guard);
            if left {
                warn!(group = %roster.id, "本机已被移出这个群");
            }
            return RosterOutcome::Updated;
        }
        // 没见过的群：**要用户点一下才算加入**（未知来源也能邀请，但得有人确认）。
        //
        // 这里手上没有"旧名册"可查，只能要求发送者出现在**它自己送来的**成员表里 ——
        // 一份把自己排除在外的名册没有任何正常含义（自己退群那条路只对已知的群有意义）。
        // 这不构成信任，只是把"随便编一份名册来弹卡片"这件事收窄到"至少自称是成员"；
        // 真正的门槛仍然是用户点「加入」。
        if !unique.iter().any(|m| m.id == from.id) {
            warn!(
                group = %roster.id, sender = %from.id,
                "陌生群的名册被忽略：发送者不在它自己给的成员表里"
            );
            return RosterOutcome::Ignored;
        }
        let invite = {
            let mut others = unique;
            others.retain(|m| m.id != own_id);
            PendingInvite {
                group_id: roster.id.clone(),
                name: roster.name.clone(),
                from: from.id.clone(),
                from_name: from.name.clone(),
                members: others,
                at: now_ms(),
                rev: roster.rev,
                updated_by: roster.updated_by.clone(),
                updated_at: roster.updated_at,
            }
        };
        match guard.pending.iter_mut().find(|p| p.group_id == roster.id) {
            Some(existing) => *existing = invite,
            None => {
                guard.pending.push(invite);
                if guard.pending.len() > MAX_PENDING {
                    guard.pending.remove(0);
                }
            }
        }
        persist(&self.path, &mut guard);
        RosterOutcome::Pending
    }

    /// 用户点了「加入」：邀请卡变成正式的群（从这一刻起名册变更静默采用）。
    pub fn accept_pending(&self, group_id: &str, own_id: &str) -> bool {
        let mut guard = self.inner.lock();
        let Some(idx) = guard.pending.iter().position(|p| p.group_id == group_id) else {
            return false;
        };
        let invite = guard.pending.remove(idx);
        let mut members = vec![Member { id: own_id.to_string(), name: String::new() }];
        normalize_members(&mut members, invite.members.clone());
        guard.groups.push(Group {
            id: group_id.to_string(),
            name: invite.name.clone(),
            rev: invite.rev,
            created_by: invite.from.clone(),
            created_at: invite.at,
            updated_by: invite.updated_by.clone(),
            updated_at: invite.updated_at,
            members,
            left: false,
            // 本机这一刻才成为成员：在此之前没人对本机说过"拒绝"，这一栏从空起步。
            declined: Vec::new(),
        });
        guard.my_seq.entry(group_id.to_string()).or_insert(1);
        // 邀请挂着的那段时间里到的消息：这一刻并进会话（按 `(from, id)` 去重、按展示顺序排）。
        let buffered = guard.orphans.remove(group_id).unwrap_or_default();
        let convo = guard.convos.entry(group_id.to_string()).or_default();
        for m in buffered {
            if convo.messages.iter().any(|x| x.from == m.from && x.id == m.id) {
                continue;
            }
            convo.messages.push(m);
        }
        convo.messages.sort_by(|a, b| sort_key(a).cmp(&sort_key(b)));
        let dropped = prune(convo);
        drop_content(&dropped, &self.blobs_root);
        persist(&self.path, &mut guard);
        true
    }

    /// 用户点了「不加入」：记住这个决定，之后这个群的事件直接忽略。
    pub fn dismiss_pending(&self, group_id: &str) -> bool {
        let mut guard = self.inner.lock();
        let had = guard.pending.iter().any(|p| p.group_id == group_id);
        guard.pending.retain(|p| p.group_id != group_id);
        // 攒着的那几条跟着这个决定一起丢掉（用户说了不进这个群）—— 内容一起走。
        let discarded = guard.orphans.remove(group_id).unwrap_or_default();
        drop_content(&discarded, &self.blobs_root);
        if !guard.dismissed.iter().any(|d| d == group_id) {
            guard.dismissed.push(group_id.to_string());
        }
        persist(&self.path, &mut guard);
        had
    }

    // ── 写：消息 ──────────────────────────────────────────────────────────

    /// 本机在群里发一条：**先落账**（`delivery` 由调用方按每台的结果填好；还没定下来的那一份
    /// 就是 `Sending`）。返回落账后的那条（`id` 由调用方给、`seq` 在这里分配）。
    pub fn record_outbound(
        &self,
        group_id: &str,
        msg_id: &str,
        from: &str,
        content: impl Into<crate::blobs::Content>,
        at: u64,
        deliveries: &[(String, Delivery)],
    ) -> GroupMsg {
        let content = content.into();
        let mut guard = self.inner.lock();
        let seq = {
            let next = guard.my_seq.entry(group_id.to_string()).or_insert(1);
            let used = *next;
            *next += 1;
            used
        };
        let msg = msg_with(content, msg_id, from, "", at, seq, deliveries.iter().cloned().collect());
        let convo = guard.convos.entry(group_id.to_string()).or_default();
        convo.messages.push(msg.clone());
        let dropped = prune(convo);
        drop_content(&dropped, &self.blobs_root);
        persist(&self.path, &mut guard);
        msg
    }

    /// 收到对端一条群消息。`true` = 是新的一条（`false` = 重复投递）。
    pub fn record_inbound(
        &self,
        group_id: &str,
        from: &str,
        from_name: &str,
        id: &str,
        content: impl Into<crate::blobs::Content>,
        at: u64,
        seq: u64,
    ) -> bool {
        let content = content.into();
        let mut guard = self.inner.lock();
        let dropped;
        {
            let convo = guard.convos.entry(group_id.to_string()).or_default();
            // 幂等键 `(from, id)`：补发/重连重投是常态，同一句不能变成两句。
            if convo.messages.iter().any(|m| m.from == from && m.id == id) {
                debug!(%id, %from, group_id, "群消息重复投递，丢弃");
                return false;
            }
            convo.messages.push(msg_with(content, id, from, from_name, at, seq, HashMap::new()));
            dropped = prune(convo);
        }
        drop_content(&dropped, &self.blobs_root);
        persist(&self.path, &mut guard);
        true
    }

    /// 这个群此刻正挂着邀请卡（等用户点「加入」）。
    pub fn has_pending(&self, group_id: &str) -> bool {
        self.inner.lock().pending.iter().any(|p| p.group_id == group_id)
    }

    /// 本机**明确拒绝过**这个群（用户点过那张卡上的「不加入」）—— 之后它的事件一律忽略。
    ///
    /// 与 `has_pending` 互斥：拒绝的那一刻邀请卡就被撤了。收不下去的群消息按这两种分开回话
    /// （理由不同，发送方那边「已拒绝加入」只在第一种亮）。
    pub fn is_dismissed(&self, group_id: &str) -> bool {
        self.inner.lock().dismissed.iter().any(|d| d == group_id)
    }

    /// 这台设备是不是"挂着的那张群邀请卡"里的成员之一（含发起人）。
    ///
    /// 用在校验入站帧的发送者时（P1-7）：群还没加入、名册还没落库，手边只有邀请卡上的成员表；
    /// 把它也算进"成员"，挂着期间的群消息才能照旧攒进 `orphans`（而不是被当成陌生人丢掉）。
    pub fn pending_has_member(&self, group_id: &str, peer_id: &str) -> bool {
        let guard = self.inner.lock();
        guard
            .pending
            .iter()
            .find(|p| p.group_id == group_id)
            .is_some_and(|p| p.from == peer_id || p.members.iter().any(|m| m.id == peer_id))
    }

    /// 邀请挂着期间攒下的消息条数（界面与测试都能看）。
    pub fn buffered_count(&self, group_id: &str) -> usize {
        self.inner.lock().orphans.get(group_id).map(|v| v.len()).unwrap_or(0)
    }

    /// 邀请还没确认、但群里已经有消息进来了：先攒着（见 `MAX_ORPHANS`）。`true` = 攒下了。
    ///
    /// 名册都还没到（连邀请卡都没有）时返回 `false` —— 那种消息无处可放，调用方记一行日志。
    pub fn buffer_pending_message(
        &self,
        group_id: &str,
        from: &str,
        from_name: &str,
        id: &str,
        content: impl Into<crate::blobs::Content>,
        at: u64,
        seq: u64,
    ) -> bool {
        let content = content.into();
        let mut guard = self.inner.lock();
        if !guard.pending.iter().any(|p| p.group_id == group_id) {
            return false;
        }
        if guard.orphans.len() >= MAX_ORPHAN_GROUPS && !guard.orphans.contains_key(group_id) {
            return false;
        }
        let list = guard.orphans.entry(group_id.to_string()).or_default();
        if list.iter().any(|m| m.from == from && m.id == id) {
            return false;
        }
        list.push(msg_with(content, id, from, from_name, at, seq, HashMap::new()));
        if list.len() > MAX_ORPHANS {
            let drop = list.len() - MAX_ORPHANS;
            // 挤出去的那几条如果是图片，内容也跟着走（不然它会一直躺在那儿没人引用）。
            let evicted: Vec<GroupMsg> = list.drain(..drop).collect();
            drop_content(&evicted, &self.blobs_root);
        }
        persist(&self.path, &mut guard);
        true
    }

    /// 把几条（本机发的）副本的投递状态一次落定（一次落盘）。**只动"还没定"的档**。
    pub fn set_deliveries(&self, group_id: &str, msg_id: &str, updates: &[(String, Delivery)]) -> bool {
        let mut guard = self.inner.lock();
        let Some(convo) = guard.convos.get_mut(group_id) else { return false };
        let Some(m) = convo.messages.iter_mut().find(|m| m.id == msg_id) else { return false };
        let mut changed = false;
        for (peer, next) in updates {
            match m.delivery.get(peer).copied() {
                Some(Delivery::Sending) | Some(Delivery::Queued) => {
                    m.delivery.insert(peer.clone(), *next);
                    changed = true;
                }
                _ => {}
            }
        }
        if changed {
            persist(&self.path, &mut guard);
        }
        changed
    }

    /// 那台设备**明确拒绝了这一条**（`group_text_rejected`）：这一份从"已送达/在途/排队"改判成
    /// 「未送达」，并且把 `declined` 记进名册视图（理由 = 本机拒绝过这个群时）。
    ///
    /// 为什么不走 `set_deliveries`：那个闸门只许 `Sending/Queued → 终态`（防一条迟到的失败
    /// 把已经写进长连接的那份降级）。而这条拒收是**对端自己说的**，它比任何推测都硬 ——
    /// 长连接那条路"写成功 = 已送达"正好是它要纠正的那种情形。
    pub fn mark_rejected(&self, group_id: &str, msg_id: &str, peer_id: &str, declined: bool) -> bool {
        let mut guard = self.inner.lock();
        let mut changed = false;
        if let Some(convo) = guard.convos.get_mut(group_id) {
            if let Some(m) = convo.messages.iter_mut().find(|m| m.id == msg_id) {
                match m.delivery.get(peer_id).copied() {
                    // 已经读过 = 那一条确实到过它手上（拒收的是**这条**，不改历史）。
                    Some(Delivery::Read) | Some(Delivery::Failed) => {}
                    Some(_) => {
                        m.delivery.insert(peer_id.to_string(), Delivery::Failed);
                        changed = true;
                    }
                    None => {}
                }
            }
        }
        if declined {
            if let Some(g) = guard.groups.iter_mut().find(|g| g.id == group_id) {
                // 只记名册里的人：不在名单里的（早被移出）没有可标的那一格。
                if g.is_member(peer_id) && !g.has_declined(peer_id) {
                    g.declined.push(peer_id.to_string());
                    changed = true;
                }
            }
        }
        if changed {
            persist(&self.path, &mut guard);
        }
        changed
    }

    /// 对端回执：成员 `reader` 读到了本机 `at <= up_to` 的那些群消息 → 水位推进（只增不减）。
    ///
    /// 顺带把**那一份副本**升成「已读」：读回执本身就是"它收到了"的**证据** —— 跨网段时
    /// 本机可能一直没有到它的路径（那一格停在「排队」），但它从别人那儿补到、又读过了，
    /// 这一格就该跟着变过来（否则界面上永远写着"没送到"，而对方明明已经读过）。
    pub fn mark_read_by_peer(&self, group_id: &str, reader: &str, up_to: u64) -> bool {
        let mut guard = self.inner.lock();
        let Some(convo) = guard.convos.get_mut(group_id) else { return false };
        let mut changed = false;
        let cur = convo.read_by.get(reader).copied().unwrap_or(0);
        if up_to > cur {
            convo.read_by.insert(reader.to_string(), up_to);
            changed = true;
        }
        for m in convo.messages.iter_mut() {
            if m.at > up_to {
                continue;
            }
            match m.delivery.get(reader).copied() {
                Some(Delivery::Queued) | Some(Delivery::Sending) | Some(Delivery::Failed) | Some(Delivery::Synced) => {
                    m.delivery.insert(reader.to_string(), Delivery::Read);
                    changed = true;
                }
                _ => {}
            }
        }
        if changed {
            persist(&self.path, &mut guard);
        }
        changed
    }

    /// 本机读过这个群了：读位推进到 `msg_id`（**只向前**，按群里的先后而不是墙钟大小）。
    pub fn mark_read_local(&self, group_id: &str, msg_id: &str) -> bool {
        let mut guard = self.inner.lock();
        let Some(convo) = guard.convos.get_mut(group_id) else { return false };
        let Some(target) = convo.messages.iter().find(|m| m.id == msg_id) else { return false };
        let target_key = sort_key(target);
        let cur_key = convo
            .read_pos
            .as_ref()
            .and_then(|id| convo.messages.iter().find(|m| &m.id == id))
            .map(sort_key);
        if cur_key.is_some_and(|k| k >= target_key) {
            return false;
        }
        convo.read_pos = Some(msg_id.to_string());
        persist(&self.path, &mut guard);
        true
    }

    /// 本机读到哪条了（界面算未读、回执都按它）。
    pub fn read_pos(&self, group_id: &str) -> Option<String> {
        self.inner.lock().convos.get(group_id).and_then(|c| c.read_pos.clone())
    }

    /// 本机读到的、**`sender` 发的**最大 `at` —— 回执带回去的那个水位（没读到 = 0）。
    ///
    /// 与一对一逐字同一个语义（`chat::inbound_watermark` 也是"对端发的最大 at"）：群里多一层
    /// `sender` 维度，因为回执是发给**某一个人**的，而对方的时钟只有它自己那一条可比。
    pub fn read_up_to_for(&self, group_id: &str, sender: &str) -> u64 {
        let guard = self.inner.lock();
        let Some(convo) = guard.convos.get(group_id) else { return 0 };
        let Some(pos) = convo
            .read_pos
            .as_ref()
            .and_then(|id| convo.messages.iter().find(|m| &m.id == id))
            .map(sort_key)
        else {
            return 0;
        };
        convo
            .messages
            .iter()
            .filter(|m| m.from == sender && sort_key(m) <= pos)
            .map(|m| m.at)
            .max()
            .unwrap_or(0)
    }

    // ── 补拉 ──────────────────────────────────────────────────────────────

    /// 这个群**明确知道**缺了哪几条（按发送方分组的洞）。
    pub fn holes(&self, group_id: &str) -> Vec<SyncHole> {
        let guard = self.inner.lock();
        let Some(convo) = guard.convos.get(group_id) else { return Vec::new() };
        holes_in(convo)
    }

    /// 已知缺口的条数（界面那行提示就写它）。
    pub fn missing_count(&self, group_id: &str) -> u32 {
        self.holes(group_id).iter().map(|h| h.seqs.len() as u32).sum()
    }

    /// 这位发送方身上**还缺着号**吗（`on_group_text` 记完一条之后用它决定要不要当场补拉）。
    ///
    /// 只看"有没有洞"，不管刚记下的那条的号有多大：洞补上之前，这个人每说一句都会再问一次
    /// （频率由补拉闸门收着），这才不会因为中间丢了一条就永远缺着。
    pub fn has_holes(&self, group_id: &str, from: &str) -> bool {
        self.holes(group_id).iter().any(|h| h.from == from && !h.seqs.is_empty())
    }

    /// 这次补拉要问什么：`None` = 没什么可问（不在群里、或冷启动那批已经拿过了）。
    ///
    /// `since_at = 0` = 本机在这个群里一条消息都没有 → 要的是"从头发一批给我"（新入群的人
    /// 这样追上群里的），**只做一次**（`bootstrapped`）。
    pub fn ask_payload(&self, group_id: &str, own_id: &str) -> Option<(u64, Vec<SyncHole>)> {
        let guard = self.inner.lock();
        let group = guard.groups.iter().find(|g| g.id == group_id)?;
        if group.left || !group.is_member(own_id) {
            return None;
        }
        let convo = guard.convos.get(group_id);
        let empty = GroupConvo::default();
        let convo = convo.unwrap_or(&empty);
        let holes = holes_in(convo);
        let anchor = convo.messages.iter().map(|m| m.at).max().unwrap_or(0);
        if anchor == 0 {
            // 冷启动：只有还没追过的那一次才问。
            if convo.bootstrapped {
                return None;
            }
            return Some((0, holes));
        }
        Some((anchor, holes))
    }

    /// 补拉的闸门：现在能不能问这台设备，能就问。
    ///
    /// `chase` = 这是"对方说还有更多"的追批 —— 不受冷却限制（一来一回可能只有几毫秒，
    /// 冷却会把追批挡在门外），但**连续追批的轮数**封顶（`MAX_SYNC_ROUNDS`）：对方手里
    /// 总有更多时不能无限追下去。上限只作用在"这一串追批"上，而**普通一问把这一串清零**
    /// （用户点、链路恢复、60s 兜底扫都走这条）—— 否则对一台离线但有地址的成员，
    /// 兜底扫每轮 +1，几分钟后这个 (群, 设备) 就再也不问了（P1-10）。
    pub fn sync_gate_open(&self, group_id: &str, peer_id: &str, chase: bool) -> bool {
        let mut sync = self.sync.lock();
        let gate = sync
            .entry((group_id.to_string(), peer_id.to_string()))
            .or_default();
        if chase {
            if gate.chase_rounds >= MAX_SYNC_ROUNDS {
                return false;
            }
            gate.chase_rounds += 1;
        } else {
            if let Some(last) = gate.last {
                if last.elapsed() < SYNC_COOLDOWN {
                    return false;
                }
            }
            // 新的一串从这里开始。
            gate.chase_rounds = 0;
        }
        gate.last = Some(std::time::Instant::now());
        true
    }

    /// 这一轮问完了：对方还有更多（`has_more`）就**继续追**，追完把轮数清零（下一次从头算）。
    pub fn sync_gate_note(&self, group_id: &str, peer_id: &str, has_more: bool) {
        let mut sync = self.sync.lock();
        let gate = sync
            .entry((group_id.to_string(), peer_id.to_string()))
            .or_default();
        if !has_more {
            gate.chase_rounds = 0;
        }
    }

    /// 对方要的那些：`at > since_at` 的全给（按展示顺序），上限 `cap` 条；返回 `(消息, 还有更多)`。
    pub fn messages_after(&self, group_id: &str, since_at: u64, cap: usize) -> (Vec<GroupMsgWire>, bool) {
        let guard = self.inner.lock();
        let Some(convo) = guard.convos.get(group_id) else { return (Vec::new(), false) };
        let mut list: Vec<&GroupMsg> = convo.messages.iter().filter(|m| m.at > since_at).collect();
        list.sort_by(|a, b| sort_key(a).cmp(&sort_key(b)));
        let more = list.len() > cap;
        list.truncate(cap);
        // **带上内容**：图片消息那几条要把盘上那份读出来（`wire_of`）。读不到就只带元数据
        // 过去（收方那台如实显示"读不出来"）—— 补拉是尽力而为。
        (list.into_iter().map(|m| wire_of(&self.blobs_root, m)).collect(), more)
    }

    /// 对方点名要的那几条（按 `(from, seq)` 精确找）。
    pub fn messages_by_holes(&self, group_id: &str, holes: &[SyncHole], cap: usize) -> Vec<GroupMsgWire> {
        let guard = self.inner.lock();
        let Some(convo) = guard.convos.get(group_id) else { return Vec::new() };
        let mut out: Vec<GroupMsgWire> = Vec::new();
        for hole in holes.iter().take(MAX_HOLE_SENDERS) {
            for seq in hole.seqs.iter().take(MAX_HOLES_PER_SENDER) {
                if out.len() >= cap {
                    return out;
                }
                if let Some(m) = convo.messages.iter().find(|m| m.from == hole.from && m.seq == *seq) {
                    out.push(wire_of(&self.blobs_root, m));
                }
            }
        }
        out
    }

    /// 把补拉回来的消息并进会话。
    ///
    /// 返回值 = "这些消息里、**确实新增的**那些的发送方 → 它发的最大 `at`"：调用方据此给
    /// 原发送方回一条 `GroupAck`（"我拿到了"），别人才知道那一份其实到了。
    pub fn apply_sync(&self, group_id: &str, own_id: &str, messages: &[GroupMsgWire]) -> Vec<(String, u64)> {
        let mut guard = self.inner.lock();
        let mut acked: HashMap<String, u64> = HashMap::new();
        let dropped;
        {
            let convo = guard.convos.entry(group_id.to_string()).or_default();
            for m in messages {
                if m.from == own_id || m.from.is_empty() {
                    continue;
                }
                // 图片消息的内容先落下来（补拉那一路内容跟着走）。落不下（id/名字不可用、
                // 字节坏了）就把这一条当**没有内容的图片消息**收下 —— 界面那条气泡会写
                // "读不出来"，比整条丢掉诚实。
                if m.image && !m.data.is_empty() {
                    if let Err(reason) =
                        crate::blobs::save_base64(&self.blobs_root, &m.id, &m.name, &m.data)
                    {
                        warn!(id = %m.id, %reason, "补拉回来的图片内容没能落下来");
                    }
                }
                if convo.messages.iter().any(|x| x.from == m.from && x.id == m.id) {
                    continue;
                }
                convo.messages.push(GroupMsg {
                    id: m.id.clone(),
                    from: m.from.clone(),
                    from_name: m.from_name.clone(),
                    text: m.text.clone(),
                    at: m.at,
                    seq: m.seq,
                    delivery: HashMap::new(),
                    image: m.image,
                    name: m.name.clone(),
                    size: m.size,
                });
                let entry = acked.entry(m.from.clone()).or_insert(0);
                *entry = (*entry).max(m.at);
            }
            convo.bootstrapped = true;
            convo.messages.sort_by(|a, b| sort_key(a).cmp(&sort_key(b)));
            dropped = prune(convo);
        }
        drop_content(&dropped, &self.blobs_root);
        persist(&self.path, &mut guard);
        acked.into_iter().collect()
    }

    /// 对端说"我手上有你发的这些"：那些副本从**还没定**的档升成「已同步」。
    ///
    /// 只动「排队 / 在途 / 未送达」：已经「已送达 / 已读」的不动（那是更强的证据）；
    /// 「未送达」能被这一下改过来，是因为它说的是"**另一方已经拿到了**" —— 与"对方明确
    /// 拒绝了本机的投递"不矛盾（他后来从别人那儿补到了）。
    pub fn mark_synced_by_peer(&self, group_id: &str, peer_id: &str, up_to: u64) -> bool {
        let mut guard = self.inner.lock();
        let Some(convo) = guard.convos.get_mut(group_id) else { return false };
        let mut changed = false;
        for m in convo.messages.iter_mut() {
            if !m.delivery.contains_key(peer_id) {
                continue;
            }
            if m.at > up_to {
                continue;
            }
            match m.delivery.get(peer_id).copied() {
                Some(Delivery::Queued) | Some(Delivery::Sending) | Some(Delivery::Failed) => {
                    m.delivery.insert(peer_id.to_string(), Delivery::Synced);
                    changed = true;
                }
                _ => {}
            }
        }
        if changed {
            persist(&self.path, &mut guard);
        }
        changed
    }
}

impl Default for GroupStore {
    fn default() -> Self {
        Self::new()
    }
}

/// 名册去重 + 上限（内存里的名册永远是"没有重复、不超过上限"的一份列表）。
fn normalize_members(members: &mut Vec<Member>, add: Vec<Member>) {
    for m in add {
        if m.id.is_empty() || members.iter().any(|x| x.id == m.id) {
            continue;
        }
        members.push(m);
        if members.len() >= MAX_MEMBERS {
            break;
        }
    }
}

fn prune(convo: &mut GroupConvo) -> Vec<GroupMsg> {
    let excess = convo.messages.len().saturating_sub(MAX_PER_GROUP);
    if excess == 0 {
        return Vec::new();
    }
    // 砍之前先记下"这些号是本机主动丢的"（每个发送方各记一个下界）：补拉找洞要从
    // 下界之上开始，否则被裁掉的老号会被永远当成缺口（见 `pruned_below`）。
    for m in convo.messages.iter().take(excess) {
        if m.seq == 0 {
            continue;
        }
        let slot = convo.pruned_below.entry(m.from.clone()).or_insert(0);
        *slot = (*slot).max(m.seq);
    }
    let dropped: Vec<GroupMsg> = convo.messages.drain(..excess).collect();
    // 读位指的那条可能正好被砍掉了：清掉，别让未读算出一个不存在的位置。
    if let Some(pos) = convo.read_pos.clone() {
        if !convo.messages.iter().any(|m| m.id == pos) {
            convo.read_pos = None;
        }
    }
    dropped
}

/// 这几条群消息的**内容文件**一起删掉（裁剪 / 丢攒着的那几条 / 退群时）。
///
/// 与 `chat::drop_blobs` 是同一件事（都走 `blobs::remove_many`）：内容目录是**同一个**，
/// 两边各按自己的消息类型挑出"谁是图片消息"。
fn drop_content(messages: &[GroupMsg], root: &std::path::Path) {
    crate::blobs::remove_many(root, messages.iter().filter(|m| m.image).map(|m| (m.id.as_str(), m.name.as_str())));
}

/// 一条消息的**内容文件**此刻在哪（文字消息、或那份文件不在盘上 = 空串）。
///
/// 界面上那条气泡按它读图；空串就是"这一条没有内容可看"（老版本留下的记录、被裁剪过、
/// 或补拉回来的那条没有内容）—— 界面如实写"读不出来"。
fn content_path(root: &std::path::Path, m: &GroupMsg) -> String {
    if !m.image {
        return String::new();
    }
    crate::blobs::path_in(root, &m.id, &m.name)
        .filter(|p| p.is_file())
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_default()
}

/// 一条图片消息的内容（base64）；文字消息、或内容不在盘上 = `None`。
///
/// 两条路都要它：补发那一刻重建事件（`flush_one`）、补拉把手上那几条发回去（`on_sync_ask`）。
fn image_data(root: &std::path::Path, m: &GroupMsg) -> Option<String> {
    use base64::Engine;
    let bytes = crate::blobs::read_in(root, &m.id, &m.name)?;
    Some(base64::engine::general_purpose::STANDARD.encode(bytes))
}

/// 展示顺序的键：`(at, from, seq)`（跨发送方按墙钟，同一个人按自己的序号）。
fn sort_key(m: &GroupMsg) -> (u64, &str, u64) {
    (m.at, m.from.as_str(), m.seq)
}

/// 会话里"明确知道的缺口"：每个发送方的号从 1 数到它自己的最大号，中间没到手的那些。
///
/// **从本地裁剪的下界之上开始数**（`pruned_below`）：被 `prune` 主动丢掉的老号不是"没收到"，
/// 点名去要也要不到（对方手里同样只留最近一批）。不扣掉这个下界的话，超过每群上限之后
/// 顶部会常驻一条"有 N 条消息未能同步"的假告警，每条新消息还都会再触发一次补拉（P1-11）。
///
/// 只看**已经在手上的人**：一条都没有的发送方不算"缺"（那可能是本机还没追过的历史，
/// 由冷启动那一批补拉解决，不是这里的洞）。
fn holes_in(convo: &GroupConvo) -> Vec<SyncHole> {
    let mut by_sender: HashMap<&str, Vec<u64>> = HashMap::new();
    for m in &convo.messages {
        if m.seq == 0 {
            continue;
        }
        by_sender.entry(m.from.as_str()).or_default().push(m.seq);
    }
    let mut out: Vec<SyncHole> = Vec::new();
    let mut senders: Vec<&&str> = by_sender.keys().collect();
    senders.sort();
    for from in senders.into_iter().take(MAX_HOLE_SENDERS) {
        let mut have = by_sender.get(*from).cloned().unwrap_or_default();
        have.sort_unstable();
        have.dedup();
        let Some(max) = have.last().copied() else { continue };
        let floor = convo.pruned_below.get(*from).copied().unwrap_or(0);
        let missing: Vec<u64> = (floor + 1..=max)
            .filter(|seq| !have.contains(seq))
            .take(MAX_HOLES_PER_SENDER)
            .collect();
        if !missing.is_empty() {
            out.push(SyncHole { from: (*from).to_string(), seqs: missing });
        }
    }
    out
}

/// 一条消息的**补拉/报名册**形状（`messages_after` / `on_sync_ask` 用）：内容跟着走。
fn wire_of(root: &std::path::Path, m: &GroupMsg) -> GroupMsgWire {
    GroupMsgWire {
        id: m.id.clone(),
        from: m.from.clone(),
        from_name: m.from_name.clone(),
        text: m.text.clone(),
        at: m.at,
        seq: m.seq,
        image: m.image,
        name: m.name.clone(),
        size: m.size,
        // 内容不在盘上（被裁剪过、或写了一半）：这一条就**没有内容**地过去 —— 收方那台会
        // 如实显示"读不出来"。补拉本来就是尽力而为，扣下整条不发反而让人以为丢了。
        data: if m.image { image_data(root, m).unwrap_or_default() } else { String::new() },
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupMsgView {
    pub id: String,
    pub from: String,
    pub from_name: String,
    pub text: String,
    pub at: u64,
    pub seq: u64,
    pub from_me: bool,
    /// 仅 `from_me` 有意义（收来的消息是空对象）。
    pub delivery: HashMap<String, Delivery>,
    /// 仅 `from_me` 有意义：读过这条的成员数（`read_by` 的水位 ≥ 这条的 `at`）。
    pub read_count: u32,
    /// 图片消息那几格（与一对一那条路同一套语义，见 `crate::blobs`）：`path` 是**本机此刻**
    /// 那份内容文件（后端现算，不进 `groups.json`），内容不在盘上时是空串。
    pub image: bool,
    pub name: String,
    pub size: u64,
    pub path: String,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct GroupView {
    pub id: String,
    pub name: String,
    pub rev: u64,
    pub created_by: String,
    /// **不含本机**（名册的存储里含本机）。
    pub members: Vec<Member>,
    /// 名册里**明确拒绝收本群消息**的那些成员 id（本机的观察，见 `Group::declined`）：
    /// 界面据此把那枚收件人胶囊标成「已拒绝加入」。
    pub declined: Vec<String>,
    pub messages: Vec<GroupMsgView>,
    pub read_pos: Option<String>,
    pub unread: u32,
    /// 已知缺了多少条（补拉还没补回来的洞）：界面据此说「有 N 条消息未能同步」。
    pub missing: u32,
}

fn view_of(group: &Group, convo: Option<&GroupConvo>, own_id: &str, blobs_root: &std::path::Path) -> GroupView {
    let empty = GroupConvo::default();
    let convo = convo.unwrap_or(&empty);
    let others: Vec<Member> = group.members.iter().filter(|m| m.id != own_id).cloned().collect();
    let mut ordered: Vec<&GroupMsg> = convo.messages.iter().collect();
    ordered.sort_by(|a, b| sort_key(a).cmp(&sort_key(b)));
    let messages = ordered
        .into_iter()
        .map(|m| {
            let from_me = m.from == own_id;
            let read_count = if from_me {
                others
                    .iter()
                    .filter(|o| convo.read_by.get(&o.id).copied().unwrap_or(0) >= m.at)
                    .count() as u32
            } else {
                0
            };
            GroupMsgView {
                id: m.id.clone(),
                from: m.from.clone(),
                from_name: m.from_name.clone(),
                text: m.text.clone(),
                at: m.at,
                seq: m.seq,
                from_me,
                delivery: if from_me { m.delivery.clone() } else { HashMap::new() },
                read_count,
                image: m.image,
                name: m.name.clone(),
                size: m.size,
                // 图片内容文件**本机此刻**在哪（不进 `groups.json`：换台机器就错了）。
                path: content_path(blobs_root, m),
            }
        })
        .collect();
    GroupView {
        id: group.id.clone(),
        name: group.name.clone(),
        rev: group.rev,
        created_by: group.created_by.clone(),
        members: others,
        declined: group.declined.clone(),
        messages,
        read_pos: convo.read_pos.clone(),
        unread: unread_in(convo, own_id),
        missing: holes_in(convo).iter().map(|h| h.seqs.len() as u32).sum(),
    }
}

/// 未读：排在读位之后、且不是本机发的条数。
fn unread_in(convo: &GroupConvo, own_id: &str) -> u32 {
    let pos = convo
        .read_pos
        .as_ref()
        .and_then(|id| convo.messages.iter().find(|m| &m.id == id))
        .map(sort_key);
    convo
        .messages
        .iter()
        .filter(|m| m.from != own_id)
        .filter(|m| match pos {
            Some(p) => sort_key(m) > p,
            None => true,
        })
        .count() as u32
}

fn unread_of(group: &Group, convo: Option<&GroupConvo>, own_id: &str) -> u32 {
    if group.left {
        return 0;
    }
    match convo {
        Some(c) => unread_in(c, own_id),
        None => 0,
    }
}

fn persist(path: &PathBuf, file: &mut GroupFile) {
    // 版本位在**写盘这一刻**补上：读到老文件（没有这一栏）也能立刻升级成当前形状。
    file.version = FORMAT_VERSION;
    match serde_json::to_vec(file) {
        Ok(bytes) => {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Err(e) = write_atomic(path, &bytes) {
                warn!(path = %path.display(), %e, "group store persist failed");
            }
        }
        Err(e) => warn!(%e, "group store serialize failed"),
    }
}

// ---------------------------------------------------------------------------
// 给界面的那份快照（与一对一那份合成同一个载荷）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub convos: Vec<crate::chat::PeerConvoView>,
    pub batches: Vec<crate::chat::ChatBatch>,
    pub groups: Vec<GroupView>,
    /// 还没被确认的群邀请（来的那份是**视图**，比落盘那份多一个 `buffered`）。
    pub pending_groups: Vec<PendingInviteView>,
    /// 全部未读（一对一 + 群）—— **不含**邀请卡：它不是消息，界面上也不进"未读"那个数，
    /// 而是与未读一起构成"会话侧待处理"（底栏「会话」与悬浮球左上角那颗，见前端 `convoBadgeTotal`）。
    pub unread: u32,
}

/// 整份会话状态：**一对一那份 + 群那份**（命令返回与 `chat://changed` 载荷共用这一个形状）。
pub fn snapshot(state: &AppState) -> Snapshot {
    let own_id = state.config.lock().device_id.clone();
    let chat = state.chat.snapshot();
    let groups = state.groups.views(&own_id);
    let pending_groups = state.groups.pending_views(&own_id);
    let unread = chat.unread + state.groups.unread_total(&own_id);
    Snapshot {
        convos: chat.convos,
        batches: chat.batches,
        groups,
        pending_groups,
        unread,
    }
}

/// 把整份状态推给界面。
///
/// **只有这一处**发 `chat://changed`：一对一那边改完（`chat.rs` 各处）、群这边改完（入站、
/// 发送、标已读、名册）都调它 —— 界面上拿到的永远是"这一刻的全量"。
pub fn emit(state: &AppState) {
    let value = serde_json::to_value(snapshot(state)).unwrap_or(serde_json::Value::Null);
    crate::ws::emit_ui(state, ui::CHAT_CHANGED, &value);
}

// ---------------------------------------------------------------------------
// 编排：命令与集成测试共用这几条路（与 `chat::send_text` 同档）
// ---------------------------------------------------------------------------

/// 本机建群并把名册广播出去。返回群 id（成员都为空 = `None`）。
pub fn create(state: &Arc<AppState>, members: &[String], name: &str) -> Option<String> {
    let own_id = state.config.lock().device_id.clone();
    let own_name = state.config.lock().device_name.clone();
    let others = resolve_members(state, members, &own_id);
    if others.is_empty() {
        return None;
    }
    let id = state.groups.create(Member { id: own_id, name: own_name }, others, name);
    broadcast_roster(state, &id);
    Some(id)
}

/// 本机改群名/成员：`rev + 1` 后广播。
pub fn edit(state: &Arc<AppState>, group_id: &str, members: &[String], name: &str) -> bool {
    let own_id = state.config.lock().device_id.clone();
    let others = resolve_members(state, members, &own_id);
    if state.groups.edit_local(group_id, &own_id, name, others).is_none() {
        return false;
    }
    broadcast_roster(state, group_id);
    true
}

/// 本机退群：本地删掉，并把"没有本机了"的名册广播给其他成员。
pub fn leave(state: &Arc<AppState>, group_id: &str) -> bool {
    let own_id = state.config.lock().device_id.clone();
    let Some(group) = state.groups.leave(group_id, &own_id) else {
        return false;
    };
    for peer in group.others(&own_id) {
        spawn_roster_to(state, &peer, &group);
    }
    true
}

/// 用户点了「加入」。名册是共享的、本机那一行本来就在里面，所以这里只需要本地落定。
pub fn accept(state: &Arc<AppState>, group_id: &str) -> bool {
    let own_id = state.config.lock().device_id.clone();
    state.groups.accept_pending(group_id, &own_id)
}

/// 用户点了「不加入」。
pub fn dismiss(state: &Arc<AppState>, group_id: &str) -> bool {
    state.groups.dismiss_pending(group_id)
}

/// 在群里发一条文本。返回这条消息的 id（本机不在这个群里 = `None`）。
///
/// 每台各投一份（无组播）：有长连接就写它；够不着的**排队等它上线**（与一对一同一套，由
/// `chat::flush_queued` 按原顺序发出去）；对端明确拒绝的落「未送达」，不发第二次。
///
/// `at` = 点名这条消息在时间线上的位置（只有输入区"图文混排"的每一段会点名，见
/// `chat::send_text` 的同名参数）。**`seq` 仍然由这里按序发**（同一个发送方说话的先后），
/// 所以点名 `at` 之后排序键（`at`，`seq`）照样是稳的。
pub fn send_text(state: &Arc<AppState>, group_id: &str, text: &str, at: Option<u64>) -> Option<String> {
    let (own_id, own_name) = own_ids(state);
    let group = sendable_group(state, group_id, &own_id)?;
    let at = at.unwrap_or_else(now_ms);
    let msg = send_group_msg(
        state,
        &group,
        &own_id,
        text,
        at,
        |msg| group_event_with(&group, &own_id, &own_name, msg, vec![own_id.clone()], String::new()),
        None,
    );
    Some(msg.id)
}

/// 在群里发一张**图片消息**（输入框里粘一张图就是走这一条）。
///
/// 与 `send_text` 同一个口径（每台各投一份、够不着的排队、对端明确拒绝的落「未送达」），
/// 区别只在内容换成那张图：**图片是消息内容，不是文件** —— 对端直接就能看，没有"接收 /
/// 同意"那一步（与一对一那条路同一套语义，见 `crate::blobs`）。
///
/// 发送方自己那份内容先落下来（补发、重启、给别人补拉都要用它）。`Err` 只有两种：
/// 内容超过上限、或落盘失败 —— `Ok(None)` = 本机不在这个群里。
pub fn send_image(
    state: &Arc<AppState>,
    group_id: &str,
    name: &str,
    bytes: &[u8],
    at: Option<u64>,
) -> Result<Option<String>, String> {
    if bytes.len() as u64 > crate::blobs::MAX_IMAGE_BYTES {
        return Err(format!(
            "这张图 {}，超过了会话里能发的上限（{} MB）—— 用「＋」当文件发过去",
            crate::chat::human_size(bytes.len() as u64),
            crate::blobs::MAX_IMAGE_BYTES / (1024 * 1024)
        ));
    }
    let (own_id, own_name) = own_ids(state);
    let Some(group) = sendable_group(state, group_id, &own_id) else {
        return Ok(None);
    };
    let at = at.unwrap_or_else(now_ms);
    let id = uuid::Uuid::new_v4().to_string();
    crate::blobs::save_in(state.groups.blobs_root(), &id, name, bytes)?;
    // 内容编一次，各台共用（`send_group_msg` 里那个闭包只叫一次）。
    use base64::Engine;
    let data = base64::engine::general_purpose::STANDARD.encode(bytes);
    let msg = send_group_msg(
        state,
        &group,
        &own_id,
        crate::blobs::Content::image(name, bytes.len() as u64),
        at,
        |msg| group_event_with(&group, &own_id, &own_name, msg, vec![own_id.clone()], data.clone()),
        Some(&id),
    );
    Ok(Some(msg.id))
}

/// 本机的身份（id 与设备名）。
fn own_ids(state: &AppState) -> (String, String) {
    let c = state.config.lock();
    (c.device_id.clone(), c.device_name.clone())
}

/// 这个群此刻能不能发消息（本机在名册里、且没退出）。
fn sendable_group(state: &AppState, group_id: &str, own_id: &str) -> Option<Group> {
    let group = state.groups.group(group_id)?;
    if group.left || !group.is_member(own_id) {
        return None;
    }
    Some(group)
}

/// 群消息的公共那一半：规划每台的初态 → 落账（拿 `seq`）→ 长连接当场发、写不进去的交给
/// 后台那一跳、要排队的那几台叫一次补发。
///
/// `event_of` 按**落账后**的那条消息造事件（图片那一档要用到内容字节，所以由调用方给）；
/// `pre_id` 非空时用它当消息 id（图片那一档要提前把内容落到那个 id 上）。
fn send_group_msg(
    state: &Arc<AppState>,
    group: &Group,
    own_id: &str,
    content: impl Into<crate::blobs::Content>,
    at: u64,
    event_of: impl FnOnce(&GroupMsg) -> WsEvent,
    pre_id: Option<&str>,
) -> GroupMsg {
    let group_id = group.id.as_str();
    let queued_group = state.groups.has_queued(group_id);
    let targets = group.others(own_id);
    let mut deliveries: Vec<(String, Delivery)> = Vec::new();
    let mut inflight: Vec<String> = Vec::new();
    let mut needs_flush: Vec<String> = Vec::new();
    for peer in &targets {
        // 名册落后、或这个群还欠着更早的消息 → 先排队：报名册与顺序都由补发那一跳保证。
        if queued_group || state.groups.needs_roster(group_id, peer) {
            deliveries.push((peer.clone(), Delivery::Queued));
            needs_flush.push(peer.clone());
            continue;
        }
        // 这一份的去向还没定（`Sending`）——投出去之后按结果落定，落账时先这么记。
        deliveries.push((peer.clone(), Delivery::Sending));
        inflight.push(peer.clone());
    }
    let id = pre_id.map(str::to_string).unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let msg = state.groups.record_outbound(group_id, &id, own_id, content, at, &deliveries);
    // 落账之后才有 seq：事件在这之后造（内容也只编一次）。
    let event = event_of(&msg);
    // 长连接那条路当场发（写成功 = 已送达），写不进去的交给后台那一跳。
    let mut settled: Vec<(String, Delivery)> = Vec::new();
    for peer in &targets {
        if !inflight.contains(peer) {
            continue;
        }
        if state.ws.has_connection(peer) && state.ws.send(peer, event.clone()) {
            settled.push((peer.clone(), Delivery::Sent));
            state.groups.mark_told(group_id, peer);
        }
    }
    if !settled.is_empty() {
        state.groups.set_deliveries(group_id, &msg.id, &settled);
    }
    for peer in inflight {
        if settled.iter().any(|(p, _)| p == &peer) {
            continue;
        }
        spawn_group_event(state, &peer, event.clone(), Some((group_id.to_string(), msg.id.clone())));
    }
    for peer in needs_flush {
        crate::chat::flush_queued(state, &peer);
    }
    emit(state);
    debug!(group = %group_id, msg = %msg.id, seq = msg.seq, targets = targets.len(), "group outbound recorded");
    msg
}

/// 本机读过这个群：读位推进到最新那条，并给每个成员回一条**逐发送者水位**的已读回执。
pub fn mark_read(state: &Arc<AppState>, group_id: &str) {
    let own_id = state.config.lock().device_id.clone();
    let Some(view) = state.groups.view(group_id, &own_id) else { return };
    let Some(last) = view.messages.last() else { return };
    let moved = state.groups.mark_read_local(group_id, &last.id);
    if !moved && view.unread == 0 {
        // 读位没动、也没未读：不必再发一遍回执（每次聚焦都往对端丢一条空回执没有意义）。
        return;
    }
    for member in &view.members {
        let up_to = state.groups.read_up_to_for(group_id, &member.id);
        if up_to == 0 {
            // 没读到过它发的东西：**收到 ≠ 读过**，别替用户把没看过的消息标成已读。
            continue;
        }
        spawn_group_event(
            state,
            &member.id,
            WsEvent::GroupRead { group_id: group_id.to_string(), up_to },
            None,
        );
    }
    emit(state);
}

/// 把本机的名册广播给全部其他成员（建群/改名/加减人/退群都用它）。
pub fn broadcast_roster(state: &Arc<AppState>, group_id: &str) {
    let own_id = state.config.lock().device_id.clone();
    let Some(group) = state.groups.group(group_id) else { return };
    for peer in group.others(&own_id) {
        spawn_roster_to(state, &peer, &group);
    }
}

/// 把一份名册投给一台设备（后台那一跳：长连接优先、HTTP 兜底；**投成功才记"告诉过了"**）。
fn spawn_roster_to(state: &Arc<AppState>, peer_id: &str, group: &Group) {
    let event = WsEvent::GroupRoster { roster: group.payload(), sender: own_info(state) };
    let (state, peer, gid) = (state.clone(), peer_id.to_string(), group.id.clone());
    crate::task::spawn("group roster", async move {
        if crate::chat::deliver(&state, &peer, &event).await == Outcome::Delivered {
            state.groups.mark_told(&gid, &peer);
        }
    });
}

/// 名册这一跳的结果怎么用。
///
/// **为什么要把「够不着」和「被拒」分开**：够不着（连不上/5xx）会一直排队等对端上线，
/// 重试是对的；而"认不出这个请求体"（4xx）重试一万次也还是认不出 —— 比如对端是**老版本**、
/// 根本没有群聊那几条路由（`/chat/group-roster` 回 404）。以前两者都落成 `Queued`，于是同一个
/// 404 每 5 秒重放一次（`flush_queued` 每个探活拍都会再试）、群里那条消息永远停在「等待发送」，
/// 而它其实永远也送不出去。按 `group.rs` 开头那条纪律，这种要落「未送达」、不发第二次。
#[derive(PartialEq, Eq, Debug)]
enum RosterStep {
    /// 名册已经送到了：接着投本体。
    Proceed,
    /// 够不着：整条留在「排队」里，等下一次触发（对端上线就补）。
    Queue,
    /// 对端明确拒绝：落「未送达」，别再重放。
    Reject,
}

fn roster_step(outcome: Outcome) -> RosterStep {
    match outcome {
        Outcome::Delivered => RosterStep::Proceed,
        Outcome::Unreachable => RosterStep::Queue,
        Outcome::Rejected => RosterStep::Reject,
    }
}

/// 投一条群事件（消息 / 回执）：**名册落后就先补名册**，再投本体。
///
/// 为什么必须先补：对方不认识的群，群里那条消息无处可放（会被丢掉）—— 名册是它的入场券。
/// `settle` 非空 = 这是一条本机发的消息，投递结果要落定到那一份副本上。
fn spawn_group_event(state: &Arc<AppState>, peer_id: &str, event: WsEvent, settle: Option<(String, String)>) {
    let (state, peer) = (state.clone(), peer_id.to_string());
    crate::task::spawn("group send", async move {
        let group_id = match &event {
            WsEvent::GroupText { group_id, .. } | WsEvent::GroupRead { group_id, .. } => Some(group_id.clone()),
            _ => None,
        };
        if let Some(gid) = group_id {
            if state.groups.needs_roster(&gid, &peer) {
                if let Some(g) = state.groups.group(&gid) {
                    let roster = WsEvent::GroupRoster { roster: g.payload(), sender: own_info(&state) };
                    match roster_step(crate::chat::deliver(&state, &peer, &roster).await) {
                        RosterStep::Proceed => state.groups.mark_told(&gid, &peer),
                        // 名册没送到就别发本体了：对方只会把它丢掉。
                        // 够不着 = 留在「排队」里等它上线；被拒 = 落「未送达」（别再重放）。
                        step => {
                            if let Some((gid, msg_id)) = settle.as_ref() {
                                let outcome = match step {
                                    RosterStep::Queue => Outcome::Unreachable,
                                    _ => Outcome::Rejected,
                                };
                                settle_group(&state, gid, msg_id, &peer, outcome);
                            }
                            return;
                        }
                    }
                }
            }
        }
        let outcome = crate::chat::deliver(&state, &peer, &event).await;
        if let Some((gid, msg_id)) = settle.as_ref() {
            settle_group(&state, gid, msg_id, &peer, outcome);
        }
    });
}

/// 一条群消息的某一份落定（并推整份状态给界面）。
pub fn settle_group(state: &Arc<AppState>, group_id: &str, msg_id: &str, peer_id: &str, outcome: Outcome) {
    if state.groups.set_deliveries(group_id, msg_id, &[(peer_id.to_string(), outcome.delivery())]) {
        emit(state);
    }
}

/// 补发一步的结果：`Advanced` = 这一份的账已经落定（接着看下一条），`Blocked` = 还够不着
/// （留在「排队」里等下一次触发）。
///
/// 与一对一的补发循环共用同一个闸门（`chat::begin_flush`），所以群与一对一在**同一台设备**
/// 上是串行的 —— 但两者的顺序互不影响（不同会话），只有各自会话内部才要求顺序。
pub enum FlushStep {
    Advanced,
    Blocked,
}

/// 补发一条排队的群消息：**先补名册**（对方不认识的群，消息无处可放），再发本体，再落定。
pub async fn flush_one(state: &Arc<AppState>, peer_id: &str, group_id: &str, msg: &GroupMsg, group: &Group) -> FlushStep {
    if state.groups.needs_roster(group_id, peer_id) {
        let roster = WsEvent::GroupRoster { roster: group.payload(), sender: own_info(state) };
        match roster_step(crate::chat::deliver(state, peer_id, &roster).await) {
            RosterStep::Proceed => state.groups.mark_told(group_id, peer_id),
            RosterStep::Queue => return FlushStep::Blocked,
            RosterStep::Reject => {
                // 对端认不出这条名册（老版本没有群聊路由之类）：这一份落「未送达」，
                // 别再排队重放 —— 否则同一个 404 会每 5 秒来一次，消息永远"等待"。
                settle_group(state, group_id, &msg.id, peer_id, Outcome::Rejected);
                return FlushStep::Advanced;
            }
        }
    }
    let own = own_info(state);
    // 图片那一档要回盘上取内容：取不到（被裁剪过 / 写了一半）就别发 —— 这一份落「未送达」
    // 并往下走（与一对一 `flush_queued` 同一档处理），别把一条只有标题的图片塞给对端，
    // 也别让它卡在「排队」里带着后面的消息一起等。
    let data = if msg.image {
        match image_data(state.groups.blobs_root(), msg) {
            Some(data) => data,
            None => {
                warn!(group = %group_id, msg = %msg.id, "这一份的内容不在了，落未送达");
                settle_group(state, group_id, &msg.id, peer_id, Outcome::Rejected);
                return FlushStep::Advanced;
            }
        }
    } else {
        String::new()
    };
    let event = group_event_with(group, &own.id, &own.name, msg, vec![msg.from.clone()], data);
    let outcome = crate::chat::deliver(state, peer_id, &event).await;
    if outcome == Outcome::Unreachable {
        return FlushStep::Blocked;
    }
    let before = state.groups.has_queued_fragment(group_id, &msg.id, peer_id);
    settle_group(state, group_id, &msg.id, peer_id, outcome);
    if before && state.groups.has_queued_fragment(group_id, &msg.id, peer_id) {
        // 落账没生效（消息被截断砍掉了之类）：不装作成功，交回调用方结束这一轮。
        FlushStep::Blocked
    } else {
        FlushStep::Advanced
    }
}

/// 造一条群消息的线上事件（文字与图片同一份：图片多带内容那几个字段）。
///
/// **每次都从这里造**（不是把消息原样发出去）：`reached` 从**作者自己**起算（收到的人据此
/// 跳过"已经有人送过"的成员）、`hops` 是这一跳的。`data` 是图片的内容（base64；文字消息给
/// 空串）—— 由调用方给：发送那一刻内容在手里，补发那一刻得回 `blobs` 取（取不到就别发，
/// 见 `flush_one`）。
fn group_event_with(
    group: &Group,
    own_id: &str,
    own_name: &str,
    msg: &GroupMsg,
    reached: Vec<String>,
    data: String,
) -> WsEvent {
    WsEvent::GroupText {
        group_id: group.id.clone(),
        id: msg.id.clone(),
        text: msg.text.clone(),
        at: msg.at,
        seq: msg.seq,
        rev: group.rev,
        author: PeerInfo { id: own_id.to_string(), name: own_name.to_string() },
        hops: 0,
        reached,
        image: msg.image,
        name: msg.name.clone(),
        size: msg.size,
        data,
    }
}

fn own_info(state: &AppState) -> PeerInfo {
    let c = state.config.lock();
    PeerInfo { id: c.device_id.clone(), name: c.device_name.clone() }
}

/// 界面给的成员 id → 名册行（名字以**本机设备表**为准；地址簿里没有的用 id 顶着）。
fn resolve_members(state: &AppState, ids: &[String], own_id: &str) -> Vec<Member> {
    let mut out: Vec<Member> = Vec::new();
    for id in ids {
        if id.is_empty() || id == own_id || out.iter().any(|m| &m.id == id) {
            continue;
        }
        let name = state
            .get_peer(id)
            .map(|p| p.name)
            .filter(|n| !n.is_empty())
            .unwrap_or_else(|| id.clone());
        out.push(Member { id: id.clone(), name });
        if out.len() >= MAX_MEMBERS - 1 {
            break;
        }
    }
    out
}

// ---------------------------------------------------------------------------
// 入站（`ws::session::handle_inbound` 与 HTTP 兜底两条路共用）
// ---------------------------------------------------------------------------

/// 收方**明确拒收**这条群消息的理由（线上代码，界面自己翻成人话）。
///
/// 为什么要有理由而不只是一个"没收到"：长连接那条路是"写成功 = 已送达"，收方丢弃时若只
/// 闷着，发送方会一直显示**已送达**（比"没提示"更糟）。反过来还得让发送方分得清"它明确
/// 拒绝了加入"与"它压根不知道这个群" —— 前一种要在那枚收件人胶囊上标出来（用户要的就是
/// 这个），后一种只是这一次没送成。
pub const REASON_DECLINED: &str = "declined";
/// 本机从没见过这个群的名册（对方以为告诉过本机，本机这边没有）。
pub const REASON_UNKNOWN: &str = "unknown";
/// 这一帧的发送方不在名册里（P1-7 防冒名）。
pub const REASON_NOT_MEMBER: &str = "not_member";

/// 收到一条群消息之后，本机到底把它放哪儿了。
///
/// **只有这一档需要回话**（其余入站事件没有回执）：`accepted()` 为假时，调用方要把理由
/// 反向告诉发送方（长连接那侧由 `reject_text` 回一条 `group_text_rejected`，HTTP 那侧另有
/// 409 兜底）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GroupTextOutcome {
    /// 落进群里了（`false` 那一支见 `record_inbound` 的去重）。
    Recorded,
    /// 名册挂着（邀请卡还没点）：**先攒着**，消息没丢 —— 对发送方算送达。
    Buffered,
    /// 本机明确拒绝过这个群（用户点过「不加入」）。
    Dismissed,
    /// 本机压根不认识这个群（名册没到 / 早删了）。
    Unknown,
    /// 这一帧的发送方不在名册里：丢掉（P1-7）。
    NotMember,
}

impl GroupTextOutcome {
    /// 这条收下了吗（收下了就别回头去reject）。
    pub fn accepted(self) -> bool {
        matches!(self, Self::Recorded | Self::Buffered)
    }

    /// 拒收的理由代码（`accepted()` 为真时没有意义）。
    pub fn reason(self) -> &'static str {
        match self {
            Self::Dismissed => REASON_DECLINED,
            Self::Unknown => REASON_UNKNOWN,
            _ => REASON_NOT_MEMBER,
        }
    }
}

/// 收到一条群消息。
pub fn on_group_text(state: &Arc<AppState>, peer_id: &str, event: GroupTextIn) -> GroupTextOutcome {
    let own_id = state.config.lock().device_id.clone();
    // 本机**拒绝过**这个群：这一条不管谁发来的都收不下 —— 理由如实说（`declined`），发送方
    // 才知道该把那台设备标出来（"一直显示已送达"就是这么来的）。
    //
    // **要放在下面那道成员闸门之前**：点「不加入」的那一刻邀请卡就撤了，本机手上没有这个群
    // 的任何名册可查 —— 落进成员闸门只会回一句没有信息量的"你不是成员"，发送方那边既不知道
    // 是拒绝、也不知道该标谁。代价是：知道群 id 的局域网主机能问出"这台拒绝过这个群"这一个
    // 布尔值（群 id 是 UUID，且这条路一个字都不落库）—— 权衡下来"发送方看得见是哪台拒绝"更重要。
    if state.groups.is_dismissed(&event.group_id) {
        debug!(group = %event.group_id, %peer_id, "群消息被拒收：本机拒绝过这个群");
        return GroupTextOutcome::Dismissed;
    }
    // 本机不在这个群里：**先分档**，再看这一帧值不值得收 ——
    // 拒绝过 / 名册没到 / 已被移出，对发送方是三种不同的理由（只有第一种要在界面上标红）。
    if !state.groups.is_member(&event.group_id, &own_id) {
        // 攒之前照样要认人：只有**名册里**（或挂着那张邀请卡上）的人**直发**的才攒 ——
        // 否则谁都能往别人的邀请卡里塞满 50 条垃圾，把真消息挤出去。
        //（中转（F-100）来的那一条这里收不了：作者是不是成员要拿手上的名册验，而本机手上
        //  连这个群的名册都没有 —— 与改动前的行为一致。）
        let sender_ok = (event.sender_id.is_empty() || event.sender_id == peer_id)
            && (state.groups.pending_has_member(&event.group_id, peer_id)
                || state.groups.members(&event.group_id).iter().any(|m| m == peer_id));
        if !sender_ok {
            warn!(
                group = %event.group_id, %peer_id,
                "群消息被丢弃：这帧的发送者不在名册（也不在挂着的邀请卡）里"
            );
            return GroupTextOutcome::NotMember;
        }
        // 名册已经到、只是**用户还没点「加入」**：先攒着（点了加入并进会话，点了不加入跟着丢）。
        // 攒下 = 对发送方算送达（它会在本机点「加入」那一刻出现），所以这一档不回拒收。
        let name = display_name(state, peer_id, &event.sender_name);
        stage_group_content(state, &event);
        if state.groups.buffer_pending_message(
            &event.group_id,
            peer_id,
            &name,
            &event.id,
            event_content(&event),
            event.at,
            event.seq,
        ) {
            debug!(group = %event.group_id, %peer_id, "邀请还没确认：先把这条群消息攒着");
            return GroupTextOutcome::Buffered;
        }
        // 连邀请卡都没有（名册没到、或本机早被移出）：这条无处可放 —— **要明说**，别让发送方
        // 那边显示「已送达」（长连接那条路"写成功"就是这个假象的来源）。
        debug!(group = %event.group_id, %peer_id, "群消息被拒收：本机不在这个群里（名册没到或已被移出）");
        return GroupTextOutcome::Unknown;
    }
    // 本机是成员：这一帧的**发送方必须是成员**（P1-7）—— 群消息允许未配对设备发
    // （成员不必配对），于是"谁都能往群里塞字"这件事只能靠这一条挡。
    // 中转（F-100）那一跳的发送者也是成员，所以正常路径不受影响；被移出的人同样进不来。
    if !state.groups.is_member(&event.group_id, peer_id) {
        warn!(
            group = %event.group_id, %peer_id,
            "群消息被丢弃：这帧的发送者不在名册里"
        );
        return GroupTextOutcome::NotMember;
    }
    // 作者是谁：直发时就是本帧的对端；**中转来的**那一条作者是别人（搬运工才是本帧对端）——
    // 载荷里自称的那个 id 只在它确实是名册成员时才采信，否则谁都能借一台成员设备往群里冒名塞字。
    let author = if event.sender_id.is_empty() || event.sender_id == peer_id {
        peer_id.to_string()
    } else if state.groups.is_member(&event.group_id, &event.sender_id) {
        event.sender_id.clone()
    } else {
        debug!(
            group = %event.group_id,
            claimed = %event.sender_id,
            %peer_id,
            "群消息被丢弃：自称的作者既不是本帧对端、也不在名册里"
        );
        return GroupTextOutcome::NotMember;
    };
    let name = display_name(state, &author, &event.sender_name);
    // 图片消息：**先把内容落下来**（攒着的那一档上面那条路也落过一次；重投是幂等的）。
    // 落不下（解码失败 / id 不可用 / 超上限）就把这一条当**没有内容的图片消息**收下：
    // 界面那条气泡会写"读不出来"，比整条丢掉诚实，也比闷着不说强。
    stage_group_content(state, &event);
    let content = event_content(&event);
    let inserted = state.groups.record_inbound(
        &event.group_id,
        &author,
        &name,
        &event.id,
        content.clone(),
        event.at,
        event.seq,
    );
    if !inserted {
        // 重复投递（补发/重连重投）：这条早就在群里了，对发送方**算收下** —— 别拿它回一条拒收。
        return GroupTextOutcome::Recorded;
    }
    emit(state);
    // 这位发送方身上**还缺着号**（中间那条没到手）：唯一能当场看见的缺口信号 —— 直接问作者。
    if state.groups.has_holes(&event.group_id, &author) {
        debug!(group = %event.group_id, %author, "群消息缺号：向发送方补拉");
        sync_with(state, &event.group_id, &author);
    }
    maybe_relay(state, &own_id, &author, &name, &event);
    GroupTextOutcome::Recorded
}

/// 把"这一条我收不下"反向告诉发送方（长连接优先、HTTP 兜底）。
///
/// 只由拒收那一档调用（`GroupTextOutcome::accepted()` 为假）。**必须真的发出去**：
/// 发送方那边那条消息的状态就靠它从「已送达」改判成「未送达」，`declined` 那一种还要在
/// 收件人胶囊上标红 —— 收方闷着不说的后果是"看起来送到了，其实一个字没进去"。
///
/// 长连接那条路上，这条回执走的就是**同一条链路**（对端刚用它把消息送过来），所以
/// "写成功"在这里是可信的；HTTP 兜底那一跳要反拨发送方（地址是它这一次连接时留下的，
/// 经中转来的那些对端不会留下地址 —— 那种情形这条回执会发不出去，此时发送方至少还有
/// 409 那个状态码兜底，只是拿不到理由）。
pub fn reject_text(state: &Arc<AppState>, peer_id: &str, group_id: &str, msg_id: &str, reason: &str) {
    let event = WsEvent::GroupTextRejected {
        group_id: group_id.to_string(),
        id: msg_id.to_string(),
        reason: reason.to_string(),
    };
    let (state, peer) = (state.clone(), peer_id.to_string());
    crate::task::spawn("group reject", async move {
        let outcome = crate::chat::deliver(&state, &peer, &event).await;
        if outcome != Outcome::Delivered {
            debug!(%peer, ?outcome, "群消息的拒收回执没送出去");
        }
    });
}

/// 收到"这一条我收不下"（`GroupTextRejected`）：那一份落「未送达」；理由是"本机拒绝过"时，
/// 还要把这位成员记进 `declined`（界面上那枚胶囊就是靠它变红）。
pub fn on_group_text_rejected(state: &Arc<AppState>, peer_id: &str, group_id: &str, msg_id: &str, reason: &str) {
    let declined = reason == REASON_DECLINED;
    if state.groups.mark_rejected(group_id, msg_id, peer_id, declined) {
        debug!(group = %group_id, msg = %msg_id, %peer_id, %reason, "对端拒收了这条群消息");
        emit(state);
    }
}

/// 跨网段那一档：把这条消息转给"本机够得着、而这一路上还没经手过"的成员。
///
/// 只在**第一次见到这条**时转（调用点只在 `record_inbound` 成功之后）：同一条消息每个成员
/// 最多转一次，环路由 `(from, id)` 与跳数（`MAX_HOPS`）一起收住。够不着的成员这里不管 ——
/// 补拉那一档会兜住（对方自己来要）。
///
/// **替别人搬消息要用户点头**（`Config::group_relay`，缺省**开**，用户可在设置里关掉）：
/// 它占用本机上行带宽 ——
/// 搬出去的字节按序列化后的消息体大小计入「中转流量」（S-11，`AppState::note_relay_message`）。
fn maybe_relay(state: &Arc<AppState>, own_id: &str, author: &str, author_name: &str, event: &GroupTextIn) {
    if !state.config.lock().group_relay || event.hops >= MAX_HOPS {
        return;
    }
    let Some(group) = state.groups.group(&event.group_id) else { return };
    if group.left || !group.is_member(own_id) {
        return;
    }
    let mut reached = event.reached.clone();
    if !reached.iter().any(|r| r == own_id) {
        reached.push(own_id.to_string());
    }
    let mut forwarded = 0usize;
    // 这一跳替别人搬出去的字节（序列化后的消息体大小）：与盲转的字节合流进「中转流量」
    // （S-11）—— 消息中转占的也是本机上行带宽，不该在这一格上不记账。
    let mut relayed_bytes: u64 = 0;
    for peer in group.others(own_id) {
        // 作者自己、以及这一路上已经经手过的成员都不再打扰。
        if peer == author || reached.iter().any(|r| r == &peer) || !reachable(state, &peer) {
            continue;
        }
        let ev = WsEvent::GroupText {
            group_id: event.group_id.clone(),
            id: event.id.clone(),
            text: event.text.clone(),
            at: event.at,
            seq: event.seq,
            rev: group.rev,
            author: PeerInfo { id: author.to_string(), name: author_name.to_string() },
            hops: event.hops + 1,
            reached: reached.clone(),
            // 图片消息那几格**原样搬**（内容也在 `event.data` 里）—— 中转就是"替别人再送一遍"，
            // 少带一格就等于替别人丢了一部分消息。
            image: event.image,
            name: event.name.clone(),
            size: event.size,
            data: event.data.clone(),
        };
        if let Ok(body) = serde_json::to_vec(&ev) {
            relayed_bytes += body.len() as u64;
        }
        spawn_group_event(state, &peer, ev, None);
        forwarded += 1;
    }
    if forwarded > 0 {
        state.note_relay_message(relayed_bytes);
        debug!(group = %event.group_id, msg = %event.id, hops = event.hops + 1, forwarded, bytes = relayed_bytes, "群消息中转");
    }
}

/// 收到一枚群已读回执。
pub fn on_group_read(state: &Arc<AppState>, peer_id: &str, group_id: &str, up_to: u64) -> bool {
    let changed = state.groups.mark_read_by_peer(group_id, peer_id, up_to);
    if changed {
        emit(state);
    }
    changed
}

/// 收到一份名册（建群邀请 / 成员变更）。
pub fn on_roster(state: &Arc<AppState>, peer_id: &str, roster: RosterPayload, sender_name: &str) -> RosterOutcome {
    let own_id = state.config.lock().device_id.clone();
    let from = PeerInfo {
        id: peer_id.to_string(),
        name: display_name(state, peer_id, sender_name),
    };
    let outcome = state.groups.apply_roster(&own_id, &roster, &from);
    match outcome {
        RosterOutcome::Ignored => {}
        RosterOutcome::Pending => {
            debug!(group = %roster.id, from = %peer_id, "收到未知群的名册：挂一张邀请卡等用户确认");
            emit(state);
        }
        RosterOutcome::Updated => emit(state),
    }
    outcome
}

// ── 补拉：编排 ─────────────────────────────────────────────────────────────
//
// 为什么是"接收方来要"而不是"发送方补投"：跨网段/分区时发送方**根本没有到对方的路径**
// （没有地址、也没有路由），它再重投一万次也到不了；而接收方看得见自己缺了东西，且它知道
// 自己能连上谁 —— 每个成员各向**自己的邻居**要一遍，缺的那一段就顺着链路传过来了。

/// 这个群里、这台设备上，本机**缺的那些**要不要现在去要。（`None` = 不在群里或没什么可要。）
pub fn sync_with(state: &Arc<AppState>, group_id: &str, peer_id: &str) {
    sync_with_inner(state, group_id, peer_id, false);
}

/// `chase` = 这是"对方说还有更多"的追批：不受冷却限制（否则 3s 冷却会把追批挡在门外），
/// 但照样记轮数（`MAX_SYNC_ROUNDS`）。
fn sync_with_inner(state: &Arc<AppState>, group_id: &str, peer_id: &str, chase: bool) {
    let own_id = state.config.lock().device_id.clone();
    let Some((since_at, holes)) = state.groups.ask_payload(group_id, &own_id) else { return };
    if !reachable(state, peer_id) {
        return;
    }
    if !state.groups.sync_gate_open(group_id, peer_id, chase) {
        return;
    }
    debug!(group = %group_id, %peer_id, since_at, holes = holes.len(), "群补拉：问一台设备");
    let ask_id = uuid::Uuid::new_v4().to_string();
    debug!(%ask_id, "群补拉：这一问的凭据（对方的回答会原样带回来）");
    let event = WsEvent::GroupSyncAsk {
        group_id: group_id.to_string(),
        ask_id,
        since_at,
        holes,
    };
    spawn_group_event(state, peer_id, event, None);
}

/// 这个群里**能连上**的成员各问一遍。
///
/// 冷启动（本机在这个群里一条消息都没有）**只问一台**：追一批最近的回来就够，别让新入群
/// 的人同时收到好几个成员的同一批历史。
pub fn sync_group(state: &Arc<AppState>, group_id: &str) {
    let own_id = state.config.lock().device_id.clone();
    let Some(group) = state.groups.group(group_id) else { return };
    if group.left || !group.is_member(&own_id) {
        return;
    }
    let cold = state
        .groups
        .ask_payload(group_id, &own_id)
        .is_some_and(|(since_at, _)| since_at == 0);
    let mut asked = false;
    for peer in group.others(&own_id) {
        if cold && asked {
            break;
        }
        if !reachable(state, &peer) {
            continue;
        }
        asked = true;
        sync_with(state, group_id, &peer);
    }
}

/// 这台设备在本机参与的**每个**群里各问一遍（链路上线 / 判在线 / 定时扫用它）。
pub fn sync_with_peer(state: &Arc<AppState>, peer_id: &str) {
    let own_id = state.config.lock().device_id.clone();
    for g in state.groups.views(&own_id) {
        if g.members.iter().any(|m| m.id == peer_id) {
            sync_with(state, &g.id, peer_id);
        }
    }
}

/// 全部群各问一遍（启动后、以及定时扫）。
pub fn sync_all(state: &Arc<AppState>) {
    let own_id = state.config.lock().device_id.clone();
    for g in state.groups.views(&own_id) {
        sync_group(state, &g.id);
    }
}

/// 收到一个补拉请求：把自己有的发回去。
///
/// **不是成员就什么都不答**（对方拿着别的群 id 乱问，本机没有可以给它的东西）。
pub fn on_sync_ask(
    state: &Arc<AppState>,
    peer_id: &str,
    group_id: &str,
    ask_id: &str,
    since_at: u64,
    holes: Vec<SyncHole>,
) -> bool {
    let own_id = state.config.lock().device_id.clone();
    if !state.groups.is_member(group_id, &own_id) {
        debug!(group = %group_id, %peer_id, "补拉请求被忽略：本机不在这个群里");
        return false;
    }
    // **问的人也得是这个群的成员**（P1-7）：原来只校验"本机自己是不是成员"，于是任何知道群 id
    // 的局域网主机（包括被移出、但还记得 id 的那台）都能要走最多 200 条群消息 + 完整名册。
    if !state.groups.is_member(group_id, peer_id) {
        warn!(group = %group_id, %peer_id, "补拉请求被忽略：问的人不在名册里");
        return false;
    }
    let (mut messages, has_more) = state.groups.messages_after(group_id, since_at, MAX_SYNC_MESSAGES);
    let extra = state.groups.messages_by_holes(group_id, &holes, MAX_SYNC_MESSAGES);
    for m in extra {
        if messages.len() >= MAX_SYNC_MESSAGES {
            break;
        }
        if !messages.iter().any(|x| x.from == m.from && x.id == m.id) {
            messages.push(m);
        }
    }
    messages.sort_by(|a, b| (a.at, a.from.as_str(), a.seq).cmp(&(b.at, b.from.as_str(), b.seq)));
    // 这两条来路（`messages_after` / `messages_by_holes`）给的**已经是线上形状**，
    // 图片消息的内容也在里面了（见 `wire_of`）。
    let roster = state.groups.group(group_id).map(|g| g.payload());
    debug!(group = %group_id, %peer_id, count = messages.len(), has_more, "补拉：把手上有的发回去");
    let event = WsEvent::GroupSyncPost {
        group_id: group_id.to_string(),
        ask_id: ask_id.to_string(),
        roster,
        has_more,
        messages,
    };
    spawn_group_event(state, peer_id, event, None);
    true
}

/// 收到补拉的回答：并进会话、给原发送方回"我拿到了"、对方还有更多就继续追。
pub fn on_sync_post(
    state: &Arc<AppState>,
    peer_id: &str,
    group_id: &str,
    roster: Option<RosterPayload>,
    has_more: bool,
    messages: Vec<GroupMsgWire>,
) -> bool {
    let own_id = state.config.lock().device_id.clone();
    // 名册顺带回：补拉常常正是"本机的名册落后"那一档（LWW 自己会判断要不要采用）。
    if let Some(roster) = roster {
        on_roster(state, peer_id, roster, "");
    }
    state.groups.sync_gate_note(group_id, peer_id, has_more);
    if !state.groups.is_member(group_id, &own_id) {
        // 还没点「加入」：这些消息无处可放（与 `on_group_text` 同一档）。
        debug!(group = %group_id, %peer_id, "补拉回来的消息被丢弃：本机不在这个群里");
        return false;
    }
    let acked = state.groups.apply_sync(group_id, &own_id, &messages);
    for (sender, up_to) in &acked {
        // "我拿到了"回给**原发送方**：他那边那条副本会从「排队/未送达」变成「已同步」。
        // 他到不了本机时这条回执自己也排队，等链路通了再送（回执丢了不影响消息本身）。
        let event = WsEvent::GroupAck { group_id: group_id.to_string(), up_to: *up_to };
        spawn_group_event(state, sender, event, None);
    }
    let changed = !acked.is_empty();
    if changed {
        emit(state);
    }
    if has_more {
        // 追下一批：不占用冷却（`chase`），但轮数封顶。
        sync_with_inner(state, group_id, peer_id, true);
    }
    changed
}

/// 收到"我手上有你发的这些"：那些副本升成「已同步」。
pub fn on_ack(state: &Arc<AppState>, peer_id: &str, group_id: &str, up_to: u64) -> bool {
    let changed = state.groups.mark_synced_by_peer(group_id, peer_id, up_to);
    if changed {
        emit(state);
    }
    changed
}

/// 这台设备此刻"能试着投递"吗（有长连接，或地址簿里有地址）。补拉是**尽力而为**：
/// 够不着就不问（下一次触发再来），不像消息那样排队等它上线 —— 要的是"我现在缺什么"，
/// 过期再问一次就是了。
fn reachable(state: &AppState, peer_id: &str) -> bool {
    if state.ws.has_connection(peer_id) {
        return true;
    }
    // 「有地址」= 「此刻拨得动」：只有一条借来的中转链、直连地址从没学到过的那类对端
    // （AP 隔离下被中转的那一侧）照样够得着，别把它当成"不在网上"。
    state.get_peer(peer_id).is_some_and(|p| p.dialable())
}

/// 入站群消息的载荷（`handle_inbound` 与 HTTP 兜底把各自的形状收敛到这里）。
#[derive(Debug, Clone)]
pub struct GroupTextIn {
    pub group_id: String,
    pub id: String,
    pub text: String,
    pub at: u64,
    pub seq: u64,
    /// 发送方手上的名册版本（名册比本机新 = 本机落后，补拉会给回来）。
    pub rev: u64,
    /// 载荷里自称的作者 id（**中转过来的消息里它不是本帧的对端**：搬运工才是）。
    pub sender_id: String,
    pub sender_name: String,
    /// 已被中转了几跳、以及这一路上经手过的成员（见 `MAX_HOPS`）。
    pub hops: u8,
    pub reached: Vec<String>,
    /// 图片消息那几格（与 `GroupMsg` / `GroupMsgWire` 同一套语义）：内容在 `data` 里。
    /// 文字消息这一档全是缺省值。
    pub image: bool,
    pub name: String,
    pub size: u64,
    pub data: String,
}

/// 造一条落盘的群消息（三处都走它：本机发出的、收到的、以及邀请卡挂着时攒下的）。
///
/// 一份 `Content` 挪进消息里之后就读不出它那几格了，所以"拆"这件事只有这一处做。
fn msg_with(
    content: crate::blobs::Content,
    id: &str,
    from: &str,
    from_name: &str,
    at: u64,
    seq: u64,
    delivery: HashMap<String, Delivery>,
) -> GroupMsg {
    let name = content.image.as_ref().map(|(n, _)| n.clone()).unwrap_or_default();
    let size = content.image.as_ref().map(|(_, s)| *s).unwrap_or(0);
    GroupMsg {
        id: id.to_string(),
        from: from.to_string(),
        from_name: from_name.to_string(),
        text: content.text,
        at,
        seq,
        delivery,
        image: content.image.is_some(),
        name,
        size,
    }
}

/// 一条入站群消息的**内容**（图片那一档带名字与字节数，文字的就是那段字）。
fn event_content(event: &GroupTextIn) -> crate::blobs::Content {
    if event.image {
        crate::blobs::Content::image(event.name.clone(), event.size)
    } else {
        crate::blobs::Content::text(event.text.clone())
    }
}

/// 图片消息：把内容落下来（两条存储路都先叫它：攒着的那一档与直接记账那一档）。
///
/// 落不下就**只记一行日志**、照样把这条消息收下（当"没有内容的图片消息"）：收方这边没有
/// 回话通道（群消息的拒收回执说的是"这个群我收不下"，不是"这张图解不开"），而丢整条会让
/// 用户以为消息没了 —— 一条写"读不出来"的气泡更诚实。
fn stage_group_content(state: &AppState, event: &GroupTextIn) {
    if !event.image || event.data.is_empty() {
        return;
    }
    if let Err(reason) = crate::blobs::save_base64(state.groups.blobs_root(), &event.id, &event.name, &event.data) {
        warn!(group = %event.group_id, id = %event.id, %reason, "群里的图片内容没能落下来");
    }
}

/// 显示名：**本机设备表优先**（长连接是 token 换来的身份；载荷里自称的名字只在没见过时用）——
/// 没学到或已被移除的那台才退回名册里带来 / 载荷里自称的那个。
///
/// `pub(crate)`：入站那条通知（`notify`）要跟落账时用的是**同一个名字**，否则通知栏与群里
/// 那条消息会显示成两个人。
pub(crate) fn display_name(state: &AppState, peer_id: &str, claimed: &str) -> String {
    state
        .get_peer(peer_id)
        .map(|p| p.name)
        .filter(|n| !n.is_empty())
        .unwrap_or_else(|| if claimed.is_empty() { peer_id.to_string() } else { claimed.to_string() })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(name: &str) -> GroupStore {
        // 每个用例自己**一个目录**：图片消息的内容目录是从 `groups.json` 的位置推出来的
        // （见 `with_path`），共用一个临时目录的话，并行跑的几条会互相删对方的内容。
        let dir = std::env::temp_dir().join(format!("orbdrop_group_{name}_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        GroupStore::with_path(dir.join("groups.json"))
    }

    fn roster(id: &str, rev: u64, by: &str, members: &[&str]) -> RosterPayload {
        RosterPayload {
            id: id.to_string(),
            name: format!("群{rev}"),
            rev,
            updated_by: by.to_string(),
            updated_at: rev * 10,
            members: members
                .iter()
                .map(|m| PeerInfo { id: m.to_string(), name: m.to_uppercase() })
                .collect(),
        }
    }

    fn inviter() -> PeerInfo {
        PeerInfo { id: "a".into(), name: "PC-A".into() }
    }

    fn member(id: &str) -> Member {
        Member { id: id.to_string(), name: id.to_uppercase() }
    }

    /// 建一个"本机已加入"的群（本机 + a + b）。
    fn joined(s: &GroupStore, group_id: &str) -> Group {
        s.apply_roster("me", &roster(group_id, 1, "a", &["a", "me", "b"]), &inviter());
        s.accept_pending(group_id, "me");
        s.group(group_id).unwrap()
    }

    /// 群里的图片消息：内容落在**自己的**目录里、视图里带本机那份路径、收到的按 `(from,id)` 幂等。
    #[test]
    fn a_group_image_keeps_its_content_and_its_path() {
        let s = store("image_round_trip");
        joined(&s, "g1");
        let written = crate::blobs::save_in(s.blobs_root(), "img-1", "图片-1.png", b"PNGDATA").unwrap();
        let msg = s.record_outbound(
            "g1",
            "img-1",
            "me",
            crate::blobs::Content::image("图片-1.png", 7),
            100,
            &[(String::from("a"), Delivery::Sent)],
        );
        assert_eq!(msg.seq, 1);
        assert!(msg.image);
        let view = s.view("g1", "me").unwrap();
        let m = &view.messages[0];
        assert!(m.image && m.name == "图片-1.png" && m.size == 7);
        assert_eq!(m.path, written.to_string_lossy(), "视图里给的是本机那份内容文件");

        // 收到的同一条（重投第二次不写第二条）。
        assert!(s.record_inbound("g1", "a", "PC-A", "img-2", crate::blobs::Content::image("图片-2.png", 3), 200, 1));
        assert!(!s.record_inbound("g1", "a", "PC-A", "img-2", crate::blobs::Content::image("图片-2.png", 3), 200, 1));
        assert_eq!(s.view("g1", "me").unwrap().messages.len(), 2);
    }

    /// 补拉：发回去的那几条**带上内容**；收方按 `(from,id)` 并进来，内容也落下来。
    #[test]
    fn a_sync_answer_carries_the_content_both_ways() {
        let mine = store("sync_serve");
        joined(&mine, "g1");
        crate::blobs::save_in(mine.blobs_root(), "img-1", "图片.png", b"DATA").unwrap();
        mine.record_inbound("g1", "a", "PC-A", "img-1", crate::blobs::Content::image("图片.png", 4), 100, 1);

        let (wire, _) = mine.messages_after("g1", 0, 10);
        assert_eq!(wire.len(), 1);
        assert!(wire[0].image);
        assert_eq!(wire[0].name, "图片.png");
        assert_eq!(wire[0].data, "REFUQQ==", "base64(DATA)");

        // 收方：内容落下来、那条消息也进来了（`path` 指得到那份文件）。
        let theirs = store("sync_take");
        joined(&theirs, "g1");
        let acked = theirs.apply_sync("g1", "me", &wire);
        assert_eq!(acked.len(), 1, "确实新增了一条");
        let view = theirs.view("g1", "me").unwrap();
        let m = &view.messages[0];
        assert!(m.image, "补回来的这条也是图片消息");
        assert!(m.path.ends_with("img-1.png"), "内容落到了本机：{}", m.path);
        assert!(std::path::Path::new(&m.path).is_file());
    }

    /// 退群 / 不加入：那条会话（或攒着的那几条）连同里面的图片内容一起走 —— 别把盘留脏。
    #[test]
    fn leaving_or_dismissing_drops_the_content() {
        let s = store("image_gc");
        joined(&s, "g1");
        let kept = crate::blobs::save_in(s.blobs_root(), "img-1", "图片.png", b"DATA").unwrap();
        s.record_outbound("g1", "img-1", "me", crate::blobs::Content::image("图片.png", 4), 100, &[]);
        assert!(kept.exists());

        // 攒着的那几条（邀请卡还挂着）也要能被清掉。
        s.apply_roster("me", &roster("g2", 1, "a", &["a", "me"]), &inviter());
        let buffered = crate::blobs::save_in(s.blobs_root(), "img-2", "图片.png", b"DATA").unwrap();
        assert!(s.buffer_pending_message("g2", "a", "PC-A", "img-2", crate::blobs::Content::image("图片.png", 4), 100, 1));
        assert!(buffered.exists());

        // 「不加入」→ 攒着的那条跟着丢。
        s.dismiss_pending("g2");
        assert!(!buffered.exists(), "不加入：攒着的那条内容也要删");
        // 退群 → 那条会话的内容跟着丢。
        s.leave("g1", "me");
        assert!(!kept.exists(), "退群：会话里的内容也要删");
        assert!(s.image_ids().is_empty(), "引用表空了");
    }

    /// 启动扫孤儿看的引用表要**两份账本合起来**（这里只看群这一半：会话里 + 还攒着的）。
    #[test]
    fn image_ids_cover_the_convo_and_the_buffered_ones() {
        let s = store("image_ids");
        joined(&s, "g1");
        s.record_outbound("g1", "img-1", "me", crate::blobs::Content::image("图片.png", 4), 100, &[]);
        s.apply_roster("me", &roster("g2", 1, "a", &["a", "me"]), &inviter());
        s.buffer_pending_message("g2", "a", "PC-A", "img-2", crate::blobs::Content::image("图片.png", 4), 100, 1);
        let ids = s.image_ids();
        assert!(ids.contains("img-1") && ids.contains("img-2"), "{ids:?}");
    }

    /// 没见过的群 → 一张邀请卡（不是正式的群）；用户点了「加入」才成为群。
    #[test]
    fn an_unknown_roster_becomes_a_pending_invite_until_accepted() {
        let s = store("pending");
        assert_eq!(
            s.apply_roster("me", &roster("g1", 1, "a", &["a", "me", "b"]), &inviter()),
            RosterOutcome::Pending
        );
        assert!(s.group("g1").is_none(), "还没加入：不建群");
        let pending = s.pending_views("me");
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0].from, "a");
        assert_eq!(
            pending[0].members.iter().map(|m| m.id.clone()).collect::<Vec<_>>(),
            vec!["a", "b"],
            "邀请卡里不含本机"
        );

        assert!(s.accept_pending("g1", "me"));
        let g = s.group("g1").expect("加入后有群了");
        assert!(g.is_member("me"), "名册的存储含本机");
        assert_eq!(g.rev, 1, "加入时把名册的版本原样带过来");
        assert!(s.pending_views("me").is_empty());
        assert_eq!(s.view("g1", "me").expect("视图里有").members.len(), 2, "视图不含本机");
    }

    /// 拒绝过的群之后一律忽略（卡片不会一直弹回来）。
    #[test]
    fn a_dismissed_group_is_ignored_forever() {
        let s = store("dismiss");
        s.apply_roster("me", &roster("g1", 1, "a", &["a", "me"]), &inviter());
        assert!(s.dismiss_pending("g1"));
        assert!(s.pending_views("me").is_empty());
        assert_eq!(
            s.apply_roster("me", &roster("g1", 2, "a", &["a", "me"]), &inviter()),
            RosterOutcome::Ignored
        );
        assert!(s.group("g1").is_none());
    }

    /// 名册是 LWW：版本大的胜；版本相同的按 `updated_by` 字典序裁决（两端收敛到同一个）。
    #[test]
    fn roster_is_last_writer_wins_and_converges() {
        let s = store("lww");
        s.apply_roster("me", &roster("g1", 1, "a", &["a", "me"]), &inviter());
        s.accept_pending("g1", "me");

        assert_eq!(
            s.apply_roster("me", &roster("g1", 3, "a", &["a", "me", "b"]), &inviter()),
            RosterOutcome::Updated
        );
        assert_eq!(s.group("g1").unwrap().members.len(), 3);
        // 旧版本回不来。
        assert_eq!(
            s.apply_roster("me", &roster("g1", 2, "b", &["a", "me"]), &inviter()),
            RosterOutcome::Ignored
        );
        assert_eq!(s.group("g1").unwrap().members.len(), 3);
        // 同版本不同人：`updated_by` 大的胜（另一端算出来是同一个）。
        assert_eq!(
            s.apply_roster("me", &roster("g1", 3, "z", &["me", "z"]), &inviter()),
            RosterOutcome::Updated
        );
        assert_eq!(s.group("g1").unwrap().updated_by, "z");
    }

    /// 名册里没有本机了 = 被移出：停发、不再列出来，历史留着。
    #[test]
    fn a_roster_without_me_means_removed_but_history_stays() {
        let s = store("removed");
        s.apply_roster("me", &roster("g1", 1, "a", &["a", "me"]), &inviter());
        s.accept_pending("g1", "me");
        s.record_inbound("g1", "a", "PC-A", "m1", "你好", 100, 1);
        assert_eq!(s.unread_total("me"), 1);

        assert_eq!(
            s.apply_roster("me", &roster("g1", 2, "a", &["a", "b"]), &inviter()),
            RosterOutcome::Updated
        );
        assert!(s.view("g1", "me").is_none(), "被移出之后不再列出来");
        assert_eq!(s.unread_total("me"), 0);
        assert!(!s.is_member("g1", "me"));
        // 消息还在盘上。
        let reopened = GroupStore::with_path(s.path.clone());
        assert!(reopened.group("g1").unwrap().left);
    }

    /// 同一个 `(from, id)` 进来两次只写一条（补发/重投是常态）。
    #[test]
    fn a_redelivered_group_message_is_recorded_once() {
        let s = store("dedupe");
        s.apply_roster("me", &roster("g1", 1, "a", &["a", "me"]), &inviter());
        s.accept_pending("g1", "me");
        assert!(s.record_inbound("g1", "a", "PC-A", "m1", "在吗", 100, 1));
        assert!(!s.record_inbound("g1", "a", "PC-A", "m1", "在吗", 100, 1));
        assert_eq!(s.view("g1", "me").unwrap().messages.len(), 1);
        // 不同的人用同一个 id：算两条（去重键是 `(from, id)`）。
        assert!(s.record_inbound("g1", "b", "PC-B", "m1", "我也说一句", 101, 1));
        assert_eq!(s.view("g1", "me").unwrap().messages.len(), 2);
    }

    /// 未读跟着读位走；读位只向前，而且是**按群里的先后**（不是墙钟水位）。
    #[test]
    fn unread_follows_the_read_position_only_forward() {
        let s = store("unread");
        s.apply_roster("me", &roster("g1", 1, "a", &["a", "b", "me"]), &inviter());
        s.accept_pending("g1", "me");
        s.record_inbound("g1", "a", "PC-A", "m1", "甲", 100, 1);
        s.record_inbound("g1", "b", "PC-B", "m2", "乙", 200, 1);
        s.record_inbound("g1", "a", "PC-A", "m3", "丙", 300, 2);
        assert_eq!(s.unread_total("me"), 3);

        assert!(s.mark_read_local("g1", "m2"));
        assert_eq!(s.unread_total("me"), 1, "读位之后只剩 m3");
        assert!(!s.mark_read_local("g1", "m1"), "回头读更早的那条：读位不动");
        assert_eq!(s.unread_total("me"), 1);
        assert!(s.mark_read_local("g1", "m3"));
        assert_eq!(s.unread_total("me"), 0);
    }

    /// 本机发的消息：逐台状态各自独立；回执按**发送者维度**的水位算「已读 n/m」，
/// 而且回执本身就是"它收到了"的证据（那一份顺带升成「已读」）。
    #[test]
    fn per_member_delivery_and_read_counts() {
        let s = store("delivery");
        let gid = s.create(member("me"), vec![member("a"), member("b"), member("c")], "开会");
        let msg = s.record_outbound(
            &gid,
            "m1",
            "me",
            "开会了",
            100,
            &[
                (String::from("a"), Delivery::Sent),
                (String::from("b"), Delivery::Queued),
                (String::from("c"), Delivery::Queued),
            ],
        );
        let view = s.view(&gid, "me").unwrap();
        assert_eq!(view.messages[0].delivery.get("a"), Some(&Delivery::Sent));
        assert_eq!(view.messages[0].delivery.get("b"), Some(&Delivery::Queued));
        assert_eq!(view.messages[0].read_count, 0);

        assert!(s.mark_read_by_peer(&gid, "a", 100));
        assert_eq!(s.view(&gid, "me").unwrap().messages[0].read_count, 1, "a 读了");
        assert!(!s.mark_read_by_peer(&gid, "a", 50), "水位只增不减");
        assert!(s.mark_read_by_peer(&gid, "b", 100));
        let view = s.view(&gid, "me").unwrap();
        assert_eq!(view.messages[0].read_count, 2);
        assert_eq!(view.messages[0].delivery.get("b"), Some(&Delivery::Read), "它读过 = 它拿到了");

        // 「排队」那一份能落定；已经定下的（已送达/已读）不再被改写。
        assert!(s.set_deliveries(&gid, &msg.id, &[(String::from("c"), Delivery::Sent)]));
        assert!(
            !s.set_deliveries(&gid, &msg.id, &[(String::from("a"), Delivery::Sent)]),
            "已送达不再回退"
        );
        assert!(
            !s.set_deliveries(&gid, &msg.id, &[(String::from("b"), Delivery::Sent)]),
            "已读不会被落成已送达"
        );
    }

    /// 回执水位是**逐发送者**的：读位之前、某个人发的最大 at。
    #[test]
    fn read_up_to_is_per_sender() {
        let s = store("upto");
        s.apply_roster("me", &roster("g1", 1, "a", &["a", "b", "me"]), &inviter());
        s.accept_pending("g1", "me");
        s.record_inbound("g1", "a", "PC-A", "m1", "甲", 100, 1);
        s.record_inbound("g1", "b", "PC-B", "m2", "乙", 200, 1);
        s.record_inbound("g1", "a", "PC-A", "m3", "丙", 300, 2);
        s.mark_read_local("g1", "m3");
        assert_eq!(s.read_up_to_for("g1", "a"), 300, "读到 a 的最后一条");
        assert_eq!(s.read_up_to_for("g1", "b"), 200);
        assert_eq!(s.read_up_to_for("g1", "c"), 0, "没读过它发的东西 = 0（不替用户标已读）");
        assert_eq!(s.read_pos("g1").as_deref(), Some("m3"));
    }

    /// 展示顺序：跨发送方按墙钟，同一个人按自己的 `seq`（墙钟打平也不乱）。
    #[test]
    fn display_order_uses_at_then_sender_seq() {
        let s = store("order");
        s.apply_roster("me", &roster("g1", 1, "a", &["a", "b", "me"]), &inviter());
        s.accept_pending("g1", "me");
        s.record_inbound("g1", "a", "PC-A", "m2", "a 的第二句", 100, 2);
        s.record_inbound("g1", "a", "PC-A", "m1", "a 的第一句", 100, 1);
        s.record_inbound("g1", "b", "PC-B", "m3", "b 说的", 50, 1);
        let view = s.view("g1", "me").unwrap();
        let order: Vec<&str> = view.messages.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(order, vec!["b 说的", "a 的第一句", "a 的第二句"]);
    }

    /// 截断从最老那头砍，并且**不会把读位指到一条已经不存在的消息上**。
    #[test]
    fn history_is_capped_and_read_pos_is_cleared_when_pruned_away() {
        let s = store("cap");
        s.apply_roster("me", &roster("g1", 1, "a", &["a", "me"]), &inviter());
        s.accept_pending("g1", "me");
        for i in 0..(MAX_PER_GROUP as u64 + 10) {
            s.record_inbound("g1", "a", "PC-A", &format!("m{i}"), "x", i + 1, i + 1);
        }
        // 510 条进来 → 砍掉最早 10 条，留下 m10..m509：读位就指在现在最老的那条上。
        assert!(s.mark_read_local("g1", "m10"), "先读一条最老的");
        s.record_inbound("g1", "a", "PC-A", "m-last", "x", 99999, 99999);
        let view = s.view("g1", "me").unwrap();
        assert_eq!(view.messages.len(), MAX_PER_GROUP);
        assert_eq!(view.read_pos, None, "读位那条被砍掉了就该清掉");
        assert_eq!(s.peek_seq("g1"), 1, "本机还没发过");
    }

    /// 退群：本地这个群没了，但广播出去的名册里没有本机、版本 +1。
    #[test]
    fn leaving_removes_it_locally_and_drops_me_from_the_roster() {
        let s = store("leave");
        s.apply_roster("me", &roster("g1", 1, "a", &["a", "b", "me"]), &inviter());
        s.accept_pending("g1", "me");
        let broadcast = s.leave("g1", "me").expect("退群给出要广播的名册");
        assert_eq!(broadcast.rev, 2, "名册版本 +1");
        assert!(!broadcast.is_member("me"), "广播出去的名册里没有本机");
        assert!(s.group("g1").is_none(), "本地删掉了");
        assert!(s.views("me").is_empty());
    }

    /// 补发：够不着的落「排队」，认得出来、按群内顺序取、落定之后不再欠。
    #[test]
    fn queued_group_messages_are_drained_in_group_order() {
        let s = store("queue");
        let gid = s.create(member("me"), vec![member("a")], "");
        let m1 = s.record_outbound(&gid, "m1", "me", "第一句", 100, &[(String::from("a"), Delivery::Queued)]);
        s.record_outbound(&gid, "m2", "me", "第二句", 200, &[(String::from("a"), Delivery::Sent)]);
        assert!(s.has_backlog_for("a"));
        assert!(s.has_queued(&gid), "这个群还欠着 → 新消息也要跟着排队");
        assert_eq!(s.peers_with_backlog(), vec!["a".to_string()]);
        let (next_gid, next) = s.next_queued_for("a").expect("有排队的");
        assert_eq!(next_gid, gid);
        assert_eq!(next.id, m1.id, "从最早那条开始");
        assert!(s.has_queued(&gid), "补发中它仍是「排队」（顺序判据还成立）");

        assert!(s.set_deliveries(&gid, &m1.id, &[(String::from("a"), Delivery::Sent)]));
        assert!(!s.has_backlog_for("a"), "发出去就不欠了");
        assert!(!s.has_queued(&gid));
        assert!(s.peers_with_backlog().is_empty());
    }

    /// 一台**不在名册里**的设备不能改写别人的群（P1-7）。
    ///
    /// 名册是 LWW（rev 大者胜）：不校验发送者身份时，任何知道群 id 的局域网主机发一份
    /// 更大 rev 的名册就能改群名、踢人、把自己加进去 —— 他连配对都不需要。
    #[test]
    fn a_foreign_peer_cannot_rewrite_a_known_roster() {
        let s = store("roster_gate");
        assert_eq!(
            s.apply_roster("me", &roster("g1", 1, "a", &["a", "me", "b"]), &inviter()),
            RosterOutcome::Pending
        );
        assert!(s.accept_pending("g1", "me"));
        let before = s.group("g1").expect("加入后有群");

        // 陌生人 c：自称成员、rev 拉高 → 丢掉，名册一个字不动。
        let attacked = roster("g1", 99, "c", &["c", "a"]);
        assert_eq!(
            s.apply_roster("me", &attacked, &PeerInfo { id: "c".into(), name: "PC-C".into() }),
            RosterOutcome::Ignored,
            "不在名册里的发送者改不动这个群"
        );
        let after = s.group("g1").expect("群还在");
        assert_eq!(after.rev, before.rev, "rev 不动");
        assert_eq!(after.name, before.name, "群名不动");
        assert_eq!(
            after.members.iter().map(|m| m.id.clone()).collect::<Vec<_>>(),
            before.members.iter().map(|m| m.id.clone()).collect::<Vec<_>>(),
            "成员表不动（既没被踢人，也没被塞进陌生人）"
        );

        // 成员照旧能改（对等 LWW 没有被这条挡住）。
        assert_eq!(
            s.apply_roster("me", &roster("g1", 2, "a", &["a", "me"]), &inviter()),
            RosterOutcome::Updated
        );
        assert_eq!(s.group("g1").expect("群还在").rev, 2);

        // 自己退群（广播一份"没有我"的名册）也照旧成立：发送者在本机手上的旧名册里。
        assert_eq!(
            s.apply_roster("me", &roster("g1", 3, "a", &["me"]), &inviter()),
            RosterOutcome::Updated,
            "成员发来的名册照旧生效（哪怕是把自己移出去那种）"
        );
    }

    /// 陌生群的邀请卡也要"发送者自称是成员"，但真正的门槛仍是用户点「加入」。
    #[test]
    fn an_unknown_group_roster_from_a_non_member_never_shows_a_card() {
        let s = store("pending_gate");
        let hostile = roster("g9", 1, "c", &["a", "b"]);
        assert_eq!(
            s.apply_roster("me", &hostile, &PeerInfo { id: "c".into(), name: "PC-C".into() }),
            RosterOutcome::Ignored,
            "名册里没有发送者自己 → 连卡片都不该弹出"
        );
        assert!(!s.has_pending("g9"), "没有挂着的邀请");
    }

    /// 不在名册里的人：拉不走历史（`on_sync_ask`）、也塞不进消息（`on_group_text`）（P1-7）。
    #[tokio::test]
    async fn a_foreign_peer_gets_no_history_and_cannot_post() {
        // 自己的目录：群/会话那些状态文件都落在**配置所在目录**，共用 temp 目录会被套件里
        // 别的用例写脏（本轮就撞上过：另一个用例把 g1 记进了"拒绝过的群"）。
        let dir = std::env::temp_dir().join(format!("orbdrop_group_gate_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let state = Arc::new(crate::state::AppState::new(
            crate::config::Config::default(),
            dir.join("config.toml"),
        ));
        let own = state.config.lock().device_id.clone();
        assert_eq!(
            state.groups.apply_roster(&own, &roster("g1", 1, "a", &["a", &own, "b"]), &inviter()),
            RosterOutcome::Pending
        );
        assert!(state.groups.accept_pending("g1", &own));

        // ① 陌生人问补拉：不给（原来只校验"本机自己是不是成员"，谁问都给 200 条 + 名册）。
        assert!(
            !on_sync_ask(&state, "c", "g1", "ask-1", 0, Vec::new()),
            "不在名册里的人问不到历史"
        );
        // 成员问：照旧答。
        assert!(on_sync_ask(&state, "a", "g1", "ask-2", 0, Vec::new()));

        // ② 陌生人借一个真实成员的 id 冒名发言：这一帧的**发送者**不是成员 → 丢。
        let forged = GroupTextIn {
            group_id: "g1".into(),
            id: "forged-1".into(),
            text: "假装是 a 说的".into(),
            at: 100,
            seq: 1,
            rev: 1,
            sender_id: "a".into(),
            sender_name: "PC-A".into(),
            hops: 0,
            reached: Vec::new(),
            image: false,
            name: String::new(),
            size: 0,
            data: String::new(),
        };
        assert_eq!(
            on_group_text(&state, "c", forged),
            GroupTextOutcome::NotMember,
            "冒充成员发言要丢掉"
        );
        assert!(
            state.groups.messages_after("g1", 0, 50).0.is_empty(),
            "伪造的那条不该落进会话"
        );

        // ③ 成员自己发：照旧收下。
        let real = GroupTextIn {
            group_id: "g1".into(),
            id: "real-1".into(),
            text: "真的".into(),
            at: 200,
            seq: 1,
            rev: 1,
            sender_id: String::new(),
            sender_name: "PC-A".into(),
            hops: 0,
            reached: Vec::new(),
            image: false,
            name: String::new(),
            size: 0,
            data: String::new(),
        };
        assert_eq!(on_group_text(&state, "a", real), GroupTextOutcome::Recorded, "成员发言照旧收下");

    }

    /// 替别人转发的群消息要**记账**（S-11）：字节进「中转流量」，但不算一条连接。
    ///
    /// 判据是"占了本机上行带宽就该在那个数里看得见" —— 原先群消息中转完全不计账：开着
    /// 「允许中转」替别的成员搬了半天消息，设置里那个数一直是 0。
    #[tokio::test]
    async fn relaying_a_group_message_counts_into_relay_traffic() {
        let dir = std::env::temp_dir().join(format!("orbdrop_group_acct_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut config = crate::config::Config::default();
        config.group_relay = true;
        let state = Arc::new(crate::state::AppState::new(config, dir.join("config.toml")));
        let own = state.config.lock().device_id.clone();
        // 群：作者 a、本机、成员 b。`others(own)` 里 a 是作者（不回头转）、b 够得着 → 转一份。
        assert_eq!(
            state.groups.apply_roster(&own, &roster("g1", 1, "a", &["a", &own, "b"]), &inviter()),
            RosterOutcome::Pending
        );
        assert!(state.groups.accept_pending("g1", &own));
        // b 够得着（有直连地址就够）：投递本身会失败（127.0.0.1:1 没人听），记账在投递之前。
        state.upsert_peer(&crate::config::PeerDevice {
            id: "b".into(),
            name: "PC-B".into(),
            ip: "127.0.0.1".into(),
            port: 1,
            online: true,
            ..Default::default()
        });

        let inbound = GroupTextIn {
            group_id: "g1".into(),
            id: "relay-1".into(),
            text: "替别人搬的这条".into(),
            at: 300,
            seq: 1,
            rev: 1,
            sender_id: "a".into(),
            sender_name: "PC-A".into(),
            hops: 0,
            reached: Vec::new(),
            image: false,
            name: String::new(),
            size: 0,
            data: String::new(),
        };
        assert_eq!(on_group_text(&state, "a", inbound), GroupTextOutcome::Recorded, "成员发的这条要收下");

        let traffic = state.relay_traffic_json();
        let bytes = traffic["bytes"].as_u64().expect("字节数是数");
        assert!(bytes > 0, "转发出去的消息字节要进「中转流量」");
        assert_eq!(traffic["connections"], 0, "消息中转不是一条连接");
        // 转过去的正是那条线上事件（含正文），所以怎么算都不该小于正文本身。
        assert!(bytes >= "替别人搬的这条".len() as u64, "至少把正文那几字节算进去");
    }

    /// **普通一问永远不会被"6 轮"锁死**（P1-10）。
    ///
    /// 兜底扫是 60s 一轮、目标可能长期离线（有地址但连不上）：旧实现让普通一问也累加轮数，
    /// 而唯一的清零点在"对方回话"上 —— 6 分钟后就再也不问了，到进程重启为止（F-99 的
    /// "必然收敛"静默失效）。这里把时钟拨快 20 轮，每一轮都必须放行。
    #[test]
    fn repeated_sweeps_never_lock_the_sync_gate_shut() {
        let s = store("gate_sweep");
        let gate_key = ("g1".to_string(), "peer-offline".to_string());
        let backdate = || {
            let mut sync = s.sync.lock();
            if let Some(gate) = sync.get_mut(&gate_key) {
                gate.last = Some(
                    std::time::Instant::now() - SYNC_COOLDOWN - std::time::Duration::from_secs(1),
                );
            }
        };
        for round in 0..20 {
            backdate(); // 等价于"又过了 60 秒，兜底扫再来一次"
            assert!(
                s.sync_gate_open("g1", "peer-offline", false),
                "第 {round} 轮兜底扫就该照常问"
            );
        }

        // 追批的额度仍然封顶（不能因为对方总说"还有更多"就无限追下去）。
        let mut chased = 0;
        for _ in 0..(MAX_SYNC_ROUNDS + 5) {
            if s.sync_gate_open("g1", "peer-live", true) {
                chased += 1;
            }
        }
        assert_eq!(chased, MAX_SYNC_ROUNDS, "连续追批封顶 {} 轮", MAX_SYNC_ROUNDS);
        // 对方回一句"没有更多" → 预算复位，能接着追。
        s.sync_gate_note("g1", "peer-live", false);
        assert!(s.sync_gate_open("g1", "peer-live", true), "回话之后重新开始算");
        // 普通一问也会把追批串清掉（下一串从头算）。
        s.sync_gate_note("g1", "peer-live", true);
        backdate_for(&s, "g1", "peer-live");
        assert!(s.sync_gate_open("g1", "peer-live", false), "普通一问照常放行");
        assert!(s.sync_gate_open("g1", "peer-live", true), "新的一串可以从头追");
    }

    /// 把某个闸门的冷却时间戳往回拨（测试里模拟"又过了一轮"）。
    fn backdate_for(s: &GroupStore, group_id: &str, peer_id: &str) {
        let mut sync = s.sync.lock();
        if let Some(gate) = sync.get_mut(&(group_id.to_string(), peer_id.to_string())) {
            gate.last = Some(
                std::time::Instant::now() - SYNC_COOLDOWN - std::time::Duration::from_secs(1),
            );
        }
    }

    /// **被本地裁剪掉的老消息不算缺口**（P1-11）。
    ///
    /// 每个群只留最近 `MAX_PER_GROUP` 条：超过之后最老的被 `prune` 丢掉，那不是"没收到"。
    /// 旧实现把它们当成永远的洞 —— 顶部常驻"有 N 条消息未能同步"的假告警，而且每来一条
    /// 新消息都会再点名去要那几个号（持续的后台流量）。
    #[test]
    fn pruned_history_is_not_reported_as_a_hole() {
        let s = store("pruned_holes");
        let gid = s.create(member("me"), vec![member("a")], "");
        let total = MAX_PER_GROUP + 120;
        for seq in 1..=total as u64 {
            s.record_inbound(&gid, "a", "PC-A", &format!("m{seq}"), "字", seq * 10, seq);
        }
        assert_eq!(
            s.view(&gid, "me").unwrap().messages.len(),
            MAX_PER_GROUP,
            "本地只留最近 {MAX_PER_GROUP} 条"
        );
        assert!(
            s.holes(&gid).is_empty(),
            "连号收进来的历史被裁掉之后不该冒出缺口：{:?}",
            s.holes(&gid)
        );
        assert_eq!(s.missing_count(&gid), 0, "界面顶部那条提示靠它");
        assert!(!s.has_holes(&gid, "a"));
        // 真的漏了一条（号在裁剪下界之上）照旧认得出来。
        let last_seq = total as u64 + 2;
        s.record_inbound(&gid, "a", "PC-A", &format!("m{last_seq}"), "字", 0, last_seq);
        assert_eq!(
            s.holes(&gid)[0].seqs,
            vec![total as u64 + 1],
            "裁剪下界之上的洞照旧要认出来"
        );
    }

    /// 「在途」在启动时退回「排队」；重启之后同一份文件还能读回来。
    #[test]
    fn inflight_is_requeued_on_startup_and_state_survives_a_restart() {
        let path = std::env::temp_dir().join(format!("orbdrop_group_restart_{}", uuid::Uuid::new_v4()));
        let _ = std::fs::remove_file(&path);
        {
            let s = GroupStore::with_path(path.clone());
            let gid = s.create(member("me"), vec![member("a")], "");
            s.record_outbound(&gid, "m1", "me", "在途的那条", 100, &[(String::from("a"), Delivery::Sending)]);
            s.record_inbound(&gid, "a", "PC-A", "m-in", "收到的", 90, 1);
            s.mark_read_local(&gid, "m-in");
        }
        let reopened = GroupStore::with_path(path.clone());
        let gid = reopened.views("me")[0].id.clone();
        let view = reopened.view(&gid, "me").unwrap();
        assert_eq!(view.messages.len(), 2);
        assert_eq!(view.unread, 0, "读位落盘了");
        reopened.requeue_inflight();
        let (_, msg) = reopened.next_queued_for("a").expect("退回排队了");
        assert_eq!(msg.text, "在途的那条");
        let _ = std::fs::remove_file(path);
    }

    /// 名册还没告诉过这台设备 → 要再发一次（投递成功才记「说过了」）。
    #[test]
    fn a_stale_roster_is_resent_before_the_next_message() {
        let s = store("told");
        let gid = s.create(member("me"), vec![member("a")], "");
        assert!(s.needs_roster(&gid, "a"), "还没告诉过它");
        s.mark_told(&gid, "a");
        assert!(!s.needs_roster(&gid, "a"));
        s.edit_local(&gid, "me", "改个名", vec![member("a")]);
        assert!(s.needs_roster(&gid, "a"), "名册版本变了 → 要再告诉它一次");
    }

    /// 本机的 `seq` 只增；名册视图不含本机；本机发的那条 `from_me`。
    #[test]
    fn my_seq_only_grows_and_views_exclude_self() {
        let s = store("seq");
        let gid = s.create(member("me"), vec![member("a")], "");
        let m1 = s.record_outbound(&gid, "m1", "me", "一", 10, &[(String::from("a"), Delivery::Sent)]);
        let m2 = s.record_outbound(&gid, "m2", "me", "二", 20, &[(String::from("a"), Delivery::Sent)]);
        assert_eq!((m1.seq, m2.seq), (1, 2));
        let view = s.view(&gid, "me").unwrap();
        assert_eq!(view.members.iter().map(|m| m.id.clone()).collect::<Vec<_>>(), vec!["a"]);
        assert!(view.messages.iter().all(|m| m.from_me));
    }

    /// 补拉：洞认得出来、问得出来、补得回来（补进来之后按展示顺序插回原位）。
    #[test]
    fn holes_are_detected_and_refilled_by_sync() {
        let s = store("holes");
        let gid = s.create(member("me"), vec![member("a")], "");
        s.record_inbound(&gid, "a", "PC-A", "m1", "一", 100, 1);
        s.record_inbound(&gid, "a", "PC-A", "m3", "三", 300, 3);
        let holes = s.holes(&gid);
        assert_eq!(holes.len(), 1, "只有 a 身上有洞");
        assert_eq!(holes[0].from, "a");
        assert_eq!(holes[0].seqs, vec![2]);
        assert_eq!(s.missing_count(&gid), 1);
        assert_eq!(s.view(&gid, "me").unwrap().missing, 1, "视图里那行提示靠它");
        assert!(s.has_holes(&gid, "a"), "缺着号当场就能看见");

        let (since, holes) = s.ask_payload(&gid, "me").expect("有东西可问");
        assert_eq!(since, 300, "锚点 = 手上最新那条的 at");
        assert_eq!(holes[0].seqs, vec![2], "洞要一条条点名");

        let acked = s.apply_sync(
            &gid,
            "me",
            &[GroupMsgWire {
                id: "m2".into(),
                from: "a".into(),
                from_name: "PC-A".into(),
                text: "二".into(),
                at: 200,
                seq: 2,
                image: false,
                name: String::new(),
                size: 0,
                data: String::new(),
            }],
        );
        assert_eq!(acked, vec![("a".to_string(), 200)], "新增的那条要回执给作者");
        assert_eq!(s.missing_count(&gid), 0, "洞补上了");
        let view = s.view(&gid, "me").unwrap();
        let order: Vec<&str> = view.messages.iter().map(|m| m.text.as_str()).collect();
        assert_eq!(order, vec!["一", "二", "三"], "补进来的插回原位");
        assert!(!s.has_holes(&gid, "a"), "补完就不缺了");
        assert!(s.apply_sync(&gid, "me", &[]).is_empty(), "空回：没有可回执的");
    }

    /// 冷启动（本机在这个群里一条都没有）只问一次"从头发一批"；追过之后就不再问。
    #[test]
    fn a_cold_group_is_bootstrapped_exactly_once() {
        let s = store("cold");
        let gid = s.create(member("me"), vec![member("a")], "");
        assert_eq!(s.ask_payload(&gid, "me"), Some((0, Vec::new())), "冷启动：从头发一批");
        s.apply_sync(
            &gid,
            "me",
            &[GroupMsgWire {
                id: "m1".into(),
                from: "a".into(),
                from_name: "PC-A".into(),
                text: "早先说的".into(),
                at: 100,
                seq: 1,
                image: false,
                name: String::new(),
                size: 0,
                data: String::new(),
            }],
        );
        assert_eq!(s.ask_payload(&gid, "me"), Some((100, Vec::new())), "之后按锚点问");
        let other = store("cold2");
        let gid2 = other.create(member("me"), vec![member("a")], "");
        other.apply_sync(&gid2, "me", &[]);
        assert_eq!(other.ask_payload(&gid2, "me"), None, "对方没货也是追过了一次");
    }

    /// 「我拿到了」把本机发出去的那几份从「排队/未送达」升成「已同步」；更强的档不动。
    #[test]
    fn an_ack_promotes_unsettled_copies_to_synced() {
        let s = store("ack");
        let gid = s.create(member("me"), vec![member("a"), member("b"), member("c")], "");
        s.record_outbound(
            &gid,
            "m1",
            "me",
            "跨网段那句",
            100,
            &[
                (String::from("a"), Delivery::Queued),
                (String::from("b"), Delivery::Failed),
                (String::from("c"), Delivery::Sent),
            ],
        );
        s.mark_synced_by_peer(&gid, "a", 100);
        s.mark_synced_by_peer(&gid, "b", 100);
        let view = s.view(&gid, "me").unwrap();
        let delivery = &view.messages[0].delivery;
        assert_eq!(delivery.get("a"), Some(&Delivery::Synced));
        assert_eq!(delivery.get("b"), Some(&Delivery::Synced), "未送达也能被「别人补到了」纠正");
        assert_eq!(delivery.get("c"), Some(&Delivery::Sent), "已送达不受影响");
        assert_eq!(view.messages[0].read_count, 0, "拿到了 ≠ 读过");
    }

    /// 读回执也是"它收到了"的证据：那一份顺带升成「已读」（跨网段时自我纠正的那条路）。
    #[test]
    fn a_read_receipt_also_proves_the_copy_arrived() {
        let s = store("readproof");
        let gid = s.create(member("me"), vec![member("a")], "");
        s.record_outbound(&gid, "m1", "me", "你那边收不到？", 100, &[(String::from("a"), Delivery::Queued)]);
        assert!(s.mark_read_by_peer(&gid, "a", 100));
        let view = s.view(&gid, "me").unwrap();
        assert_eq!(view.messages[0].delivery.get("a"), Some(&Delivery::Read));
        assert_eq!(view.messages[0].read_count, 1);
    }

    /// 补拉的回答受上限约束，并且如实说"我这儿还有更多"。
    #[test]
    fn sync_responses_are_capped_and_say_so() {
        let s = store("cap2");
        let gid = s.create(member("me"), vec![member("a")], "");
        for i in 0..(MAX_SYNC_MESSAGES as u64 + 5) {
            s.record_inbound(&gid, "a", "PC-A", &format!("m{i}"), "x", i + 1, i + 1);
        }
        let (msgs, more) = s.messages_after(&gid, 0, MAX_SYNC_MESSAGES);
        assert_eq!(msgs.len(), MAX_SYNC_MESSAGES);
        assert!(more, "还有更多要有交代，对方才会追下一批");
        let (rest, more) = s.messages_after(&gid, msgs.last().unwrap().at, MAX_SYNC_MESSAGES);
        assert_eq!(rest.len(), 5);
        assert!(!more);
        assert_eq!(rest[0].seq, MAX_SYNC_MESSAGES as u64 + 1, "接着上一批往下给");
    }

    /// 邀请还挂在卡上时到达的群消息：先攒着，点「加入」那一刻并进会话；点「不加入」跟着丢。
    #[test]
    fn messages_arriving_before_the_invite_is_confirmed_are_buffered() {
        let s = store("orphan");
        s.apply_roster("me", &roster("g1", 1, "a", &["a", "me"]), &inviter());
        assert!(s.buffer_pending_message("g1", "a", "PC-A", "m1", "趁你还没点", 100, 1));
        assert!(
            !s.buffer_pending_message("g1", "a", "PC-A", "m1", "趁你还没点", 100, 1),
            "同一条只攒一次"
        );
        assert!(s.has_pending("g1"));
        assert!(s.view("g1", "me").is_none(), "还没加入：没有会话");
        assert_eq!(s.buffered_count("g1"), 1);
        // 卡上那句「已有 N 条消息在等你」的数就来自这里：攒着几条要**看得见**，否则用户
        // 不点「加入」就不知道里面有东西。
        assert_eq!(s.pending_views("me")[0].buffered, 1);

        assert!(s.accept_pending("g1", "me"));
        let view = s.view("g1", "me").unwrap();
        assert_eq!(view.messages.len(), 1, "加入那一刻并进来");
        assert_eq!(view.messages[0].text, "趁你还没点");
        assert_eq!(view.unread, 1, "攒进来的照样是未读");
        assert_eq!(s.buffered_count("g1"), 0);
        assert!(
            !s.buffer_pending_message("g1", "a", "PC-A", "m2", "x", 200, 2),
            "已经是成员了：不再走攒这条路"
        );

        // 拒绝那条路：攒的跟着一起丢。
        s.apply_roster("me", &roster("g2", 1, "a", &["a", "me"]), &inviter());
        assert!(s.buffer_pending_message("g2", "a", "PC-A", "m1", "会被丢掉", 100, 1));
        assert!(s.dismiss_pending("g2"));
        assert_eq!(s.buffered_count("g2"), 0);
        assert!(s.view("g2", "me").is_none());
    }

    /// 老文件里多出别的键（别的版本写的）：读进来看不懂就忽略，别把整份群数据读废。
    #[test]
    fn a_file_with_extra_keys_still_loads() {
        let path = std::env::temp_dir().join(format!("orbdrop_group_legacy_{}", uuid::Uuid::new_v4()));
        std::fs::write(
            &path,
            r#"{"version":1,"groups":[{"id":"g1","name":"老群","rev":2,"createdBy":"a","createdAt":1,"updatedBy":"a","updatedAt":2,"members":[{"id":"a","name":"PC-A"},{"id":"me","name":"PC-ME"}]}],"convos":{"g1":{"messages":[{"id":"m1","from":"a","fromName":"PC-A","text":"你好","at":100,"seq":1}],"readPos":null,"readBy":{}}},"somethingNew":[1,2]}"#,
        )
        .unwrap();
        let s = GroupStore::with_path(path.clone());
        let view = s.view("g1", "me").expect("老文件里的群读得出来");
        assert_eq!(view.name, "老群");
        assert_eq!(view.messages[0].text, "你好");
        assert_eq!(view.unread, 1);
        let _ = std::fs::remove_file(path);
    }

    /// 常量与形状版本：写盘时带上版本，未来才分得清"这是哪一版的数据"。
    #[test]
    fn persisted_file_carries_a_version() {
        let path = std::env::temp_dir().join(format!("orbdrop_group_ver_{}", uuid::Uuid::new_v4()));
        let s = GroupStore::with_path(path.clone());
        let gid = s.create(member("me"), vec![member("a")], "名字");
        assert_eq!(s.group(&gid).unwrap().name, "名字");
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("\"version\""), "落盘要带版本位：{text}");
        assert_eq!(FORMAT_VERSION, 1);
        let _ = std::fs::remove_file(path);
    }

    /// 名册那一跳的三档：送到 → 继续；够不着 → 排队等下次；**被拒（4xx，例如老版本没有
    /// 群聊路由）→ 落「未送达」，不许再排队重放**。最后一档分错的话，同一个 404 会每 5 秒
    /// 重放一次、群里那条消息永远停在「等待」（实测过）。
    #[test]
    fn roster_step_separates_rejection_from_unreachable() {
        assert_eq!(roster_step(Outcome::Delivered), RosterStep::Proceed);
        assert_eq!(roster_step(Outcome::Unreachable), RosterStep::Queue);
        assert_eq!(roster_step(Outcome::Rejected), RosterStep::Reject);
        // 被拒落的是「未送达」（`Delivery::Failed`），不是「排队」—— 与 `Outcome::delivery` 一致。
        assert_eq!(Outcome::Rejected.delivery(), Delivery::Failed);
        assert_eq!(Outcome::Unreachable.delivery(), Delivery::Queued);
    }

    /// 邀请卡的**线上键是 `id`**：界面按 `PendingGroup.id` 读，并原样交给
    /// `group_join` / `group_dismiss`。曾经是 `groupId` → 前端读到 `undefined` →
    /// `invoke('group_join', { id: undefined })` 里那个键直接消失，后端报
    /// `missing required key id`：「加入」与「拒绝」双双点不动。
    #[test]
    fn pending_invite_exposes_id_not_group_id() {
        // 给界面的那份是**视图**（`pending_views` 造的那个），线上形状拿它来验。
        let view = PendingInviteView {
            group_id: "g-1".into(),
            name: "群".into(),
            from: "p1".into(),
            from_name: "P1".into(),
            members: vec![member("p1")],
            at: 1,
            buffered: 2,
        };
        let json = serde_json::to_value(&view).unwrap();
        assert_eq!(json["id"], "g-1", "界面读的键就是它");
        assert!(json.get("groupId").is_none(), "别再吐 groupId");
        assert_eq!(json["buffered"], 2, "攒着几条也要给界面（卡上那句「已有 N 条消息在等你」）");

        // 老文件（那个键叫 `groupId`）要照样读得进来，不能因为改键丢掉别人挂着的邀请。
        let old = serde_json::json!({
            "groupId": "g-2", "name": "旧", "from": "p1", "fromName": "P1",
            "members": [{ "id": "p1", "name": "P1" }], "at": 1,
            "rev": 1, "updatedBy": "p1", "updatedAt": 1
        });
        let back: PendingInvite = serde_json::from_value(old).expect("老键要读得进");
        assert_eq!(back.group_id, "g-2");
    }

    /// **同一批人只该有一个群**：再"建"一次同样的名单 → 复用同一个 id（不新建）。
    /// 顺序无关；名单不同才是新群；退过的群不复用。
    #[test]
    fn creating_the_same_member_set_reuses_the_group() {
        let s = store("dedupe");
        let me = member("me");

        let first = s.create(me.clone(), vec![member("b"), member("c")], "项目组");
        let again = s.create(me.clone(), vec![member("c"), member("b")], "另一个名字");
        assert_eq!(first, again, "名单一样（顺序不同）就是同一个群");
        assert_eq!(s.group(&first).unwrap().name, "项目组", "复用不改名：改名走名册编辑那条路");
        assert_eq!(s.views(&me.id).len(), 1, "只该有一个群");

        let other = s.create(me.clone(), vec![member("b"), member("d")], "换个人");
        assert_ne!(other, first, "名单不同 = 新群");

        // 退过之后同样名单再建 → 新的（旧群他不在了，不该把旧群的事件接回来）
        assert!(s.leave(&first, &me.id).is_some());
        let rebuilt = s.create(me.clone(), vec![member("b"), member("c")], "重建");
        assert_ne!(rebuilt, first, "退过的群不复用");
    }

    // ── 拒收（F-98 那一档）────────────────────────────────────────────────

    /// 收方说"这一条我收不下"（`declined`）：那一份从「已送达」改判成「未送达」，并记进
    /// `declined` —— 收件人那一排就是靠它变红的。**名册一个字都不动**。
    #[test]
    fn a_declined_member_is_marked_but_stays_on_the_roster() {
        let s = store("declined");
        let gid = s.create(member("me"), vec![member("a"), member("b")], "");
        // 长连接那条路：写成功就落「已送达」（这正是要被拒收纠正的那种状态）。
        s.record_outbound(&gid, "m1", "me", "在吗", 100, &[
            (String::from("a"), Delivery::Sent),
            (String::from("b"), Delivery::Sent),
        ]);
        assert!(s.mark_rejected(&gid, "m1", "a", true));
        let view = s.view(&gid, "me").unwrap();
        assert_eq!(view.messages[0].delivery.get("a"), Some(&Delivery::Failed), "被拒的那份改判未送达");
        assert_eq!(view.messages[0].delivery.get("b"), Some(&Delivery::Sent), "别人不受影响");
        assert_eq!(view.declined, vec!["a".to_string()], "界面要能看见是哪一台");
        assert!(
            view.members.iter().any(|m| m.id == "a"),
            "名册不动：拒绝是本机的观察，替所有人改名单要跟 LWW 打架"
        );
        // 同一条又来一次（重复投递）：不重复记、也不重复报改动。
        assert!(!s.mark_rejected(&gid, "m1", "a", true), "第二次没有新东西可改");

        // 落盘要带着它（重启后那枚胶囊还是红的）。
        let reloaded = GroupStore::with_path(s.path.clone());
        assert_eq!(reloaded.view(&gid, "me").unwrap().declined, vec!["a".to_string()]);

        // 把这台移出名单 → 那一格作废（重新加回来 = 一次新的邀请，得重新看它答不答）。
        assert!(s.edit_local(&gid, "me", "", vec![member("b")]).is_some());
        assert!(s.view(&gid, "me").unwrap().declined.is_empty(), "名单里没有它了就不该再标红");
    }

    /// 老文件（没有 `declined` 那一栏）要照样读得进来：那一栏为空，界面不画红。
    #[test]
    fn a_legacy_file_without_the_declined_key_still_loads() {
        let path = std::env::temp_dir().join(format!("orbdrop_group_legacy_{}", uuid::Uuid::new_v4()));
        std::fs::write(
            &path,
            r#"{"version":1,"groups":[{"id":"g1","name":"老群","rev":1,"createdBy":"a","updatedBy":"a",
                "members":[{"id":"a","name":"A"},{"id":"me","name":"ME"}]}],
                "convos":{"g1":{"messages":[]}}}"#,
        )
        .unwrap();
        let s = GroupStore::with_path(path.clone());
        let view = s.view("g1", "me").expect("老文件里的群读得出来");
        assert!(view.declined.is_empty(), "老文件里没人记过拒绝这件事");
        let _ = std::fs::remove_file(path);
    }

    /// 收方点了「不加入」的那个群：下一条群消息**明确拒收**（理由 = `declined`）。
    ///
    /// 这条正是"发送方一直显示已送达"的解药 —— 收方闷着不说，长连接那条路写成功就等于
    /// 送达，界面上看它一切正常。
    #[tokio::test]
    async fn a_dismissed_invitee_refuses_the_next_group_message() {
        let dir = std::env::temp_dir().join(format!("orbdrop_group_refuse_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let state = Arc::new(crate::state::AppState::new(crate::config::Config::default(), dir.join("config.toml")));
        let own = state.config.lock().device_id.clone();

        // 收到一份不认识的名册（邀请卡）→ 用户点「不加入」。
        assert_eq!(
            state.groups.apply_roster(&own, &roster("g1", 1, "a", &["a", &own]), &inviter()),
            RosterOutcome::Pending
        );
        assert!(state.groups.dismiss_pending("g1"));
        assert!(state.groups.is_dismissed("g1"), "拒绝过就记着（同一份邀请不会再弹）");

        let inbound = |gid: &str, id: &str, text: &str| GroupTextIn {
            group_id: gid.into(),
            id: id.into(),
            text: text.into(),
            at: 100,
            seq: 1,
            rev: 1,
            sender_id: String::new(),
            sender_name: "PC-A".into(),
            hops: 0,
            reached: Vec::new(),
            image: false,
            name: String::new(),
            size: 0,
            data: String::new(),
        };
        assert_eq!(
            on_group_text(&state, "a", inbound("g1", "m1", "进来聊聊")),
            GroupTextOutcome::Dismissed,
            "拒绝过的群：要明确回话（理由 = 已拒绝加入），不能闷着"
        );
        assert_eq!(GroupTextOutcome::Dismissed.reason(), REASON_DECLINED);
        assert!(!GroupTextOutcome::Dismissed.accepted());
        assert!(
            state.groups.messages_after("g1", 0, 50).0.is_empty(),
            "拒绝过的群不落任何消息"
        );

        // 邀请还挂着（没点加入）时是另一档：**先攒着**，对发送方算送达（它会在点「加入」那一刻出现）。
        assert_eq!(
            state.groups.apply_roster(&own, &roster("g2", 1, "a", &["a", &own]), &inviter()),
            RosterOutcome::Pending
        );
        assert_eq!(on_group_text(&state, "a", inbound("g2", "m2", "先攒着")), GroupTextOutcome::Buffered);
        assert!(GroupTextOutcome::Buffered.accepted());
        assert_eq!(state.groups.buffered_count("g2"), 1);
    }

    /// 被移出名单之后：本机不认识这个群里的消息（理由 = `unknown`，**不是**"我拒绝过"）。
    /// 两档分开才不会把那台设备误标成"已拒绝加入"。
    #[tokio::test]
    async fn a_removed_member_refuses_as_unknown_not_as_declined() {
        let dir = std::env::temp_dir().join(format!("orbdrop_group_removed_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let state = Arc::new(crate::state::AppState::new(crate::config::Config::default(), dir.join("config.toml")));
        let own = state.config.lock().device_id.clone();
        assert_eq!(
            state.groups.apply_roster(&own, &roster("g1", 1, "a", &["a", &own, "b"]), &inviter()),
            RosterOutcome::Pending
        );
        assert!(state.groups.accept_pending("g1", &own));
        // 名册换了一份**没有本机**的（rev 更大）→ 本机被移出。
        assert_eq!(
            state.groups.apply_roster(&own, &roster("g1", 2, "a", &["a", "b"]), &inviter()),
            RosterOutcome::Updated
        );
        let inbound = GroupTextIn {
            group_id: "g1".into(),
            id: "m1".into(),
            text: "还在吗".into(),
            at: 100,
            seq: 1,
            rev: 2,
            sender_id: String::new(),
            sender_name: "PC-A".into(),
            hops: 0,
            reached: Vec::new(),
            image: false,
            name: String::new(),
            size: 0,
            data: String::new(),
        };
        assert_eq!(on_group_text(&state, "a", inbound), GroupTextOutcome::Unknown);
        assert_eq!(GroupTextOutcome::Unknown.reason(), REASON_UNKNOWN);
        assert_ne!(
            GroupTextOutcome::Unknown.reason(),
            GroupTextOutcome::Dismissed.reason(),
            "两档的理由不能混：`declined` 才会在发送方那边把那枚胶囊标红"
        );
    }

    /// 收到拒收回执：那一份落「未送达」，`declined` 记进视图（界面那枚胶囊变红的那条路）。
    #[tokio::test]
    async fn a_rejection_receipt_settles_the_copy_and_marks_the_member() {
        let dir = std::env::temp_dir().join(format!("orbdrop_group_reject_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let state = Arc::new(crate::state::AppState::new(crate::config::Config::default(), dir.join("config.toml")));
        let own = state.config.lock().device_id.clone();
        assert_eq!(
            state.groups.apply_roster(&own, &roster("g1", 1, "a", &["a", &own]), &inviter()),
            RosterOutcome::Pending
        );
        assert!(state.groups.accept_pending("g1", &own));
        state.groups.record_outbound("g1", "m1", &own, "在吗", 100, &[(String::from("a"), Delivery::Sent)]);

        on_group_text_rejected(&state, "a", "g1", "m1", REASON_DECLINED);
        let view = state.groups.view("g1", &own).unwrap();
        assert_eq!(view.messages[0].delivery.get("a"), Some(&Delivery::Failed));
        assert_eq!(view.declined, vec!["a".to_string()]);

        // 别的理由（名册没到那种）只落「未送达」，**不许**把对方标成"已拒绝加入"。
        state.groups.record_outbound("g1", "m2", &own, "再问一句", 200, &[(String::from("a"), Delivery::Sent)]);
        on_group_text_rejected(&state, "a", "g1", "m2", REASON_UNKNOWN);
        let view = state.groups.view("g1", &own).unwrap();
        let m2 = view.messages.iter().find(|m| m.id == "m2").unwrap();
        assert_eq!(m2.delivery.get("a"), Some(&Delivery::Failed));
        assert_eq!(view.declined, vec!["a".to_string()], "理由不是 declined 就不该多记一个");
    }
}
