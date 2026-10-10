use serde::{Deserialize, Serialize};

/// Identity of a peer as carried in events (F-12 / push protocol).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerInfo {
    pub id: String,
    pub name: String,
}

/// One file inside a pushed directory manifest.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FileEntry {
    /// Relative path from the pushed root, using '/' separators.
    pub path: String,
    pub size: u64,
    /// 源文件的修改时刻（unix 秒）。
    ///
    /// 用来做**身份**：接收端拿它算续传键（`transfer::stream::resume_key`），并在传输前
    /// 拿对端报的 `size`/`mtime` 核对"这份是不是清单里那一版" —— 源文件在邀请之后被改过就
    /// 当场判这条失败，而不是拼出一份半新半旧的文件。
    ///
    /// `default`：老对端不带这个字段，读进来是 0 = "时间戳未知"（接收端据此跳过时间戳对账，
    /// 行为退化成"只对大小"）。
    #[serde(default)]
    pub mtime: i64,
}

/// 「这次推送了几项」的缺省值：老对端不带这个字段时按"一项"算。
fn one_item() -> usize {
    1
}

/// Wire protocol for the peer WebSocket channel (server-to-server, WS /events).
///
/// Every variant is valid in both directions; the receiving side interprets
/// it according to its role (sender/receiver/viewer).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum WsEvent {
    /// A device came online (mDNS discovery) — relayed to local UI.
    PeerOnline {
        peer: PeerInfo,
    },
    /// A device went offline — relayed to local UI.
    PeerOffline {
        peer_id: String,
    },
    /// Sender → Receiver: a push invitation (F-50).
    PushInvite {
        invite_id: String,
        sender: PeerInfo,
        /// File or directory display name.
        name: String,
        /// Total size in bytes; 0 for directories (sum of files instead).
        size: u64,
        /// 清单是否权威（按 `files` 逐条取；单文件推送为 false）。
        is_dir: bool,
        /// 发送方这次选了几项（>1 = 多选合并成的一条推送）。
        /// `default` 是为了容忍不认识这个字段的旧对端。
        #[serde(default = "one_item")]
        count: usize,
        /// Directory manifest; empty for single-file pushes.
        files: Vec<FileEntry>,
        /// 发送方支持的能力位（`"stream1"` = 新的分帧并发协议 `POST /push/stream`）。
        ///
        /// 接收方据此选走哪条数据面：**不带**这个字段的对端（老版本）照旧走
        /// `GET /push/download` 的逐文件串行循环，两边都能跑，滚动升级不需要同步。
        #[serde(default)]
        caps: Vec<String>,
    },
    /// Receiver → Sender: accept / reject an invitation.
    PushResponse {
        invite_id: String,
        accepted: bool,
    },
    /// Receiver → Sender: download progress.
    PushProgress {
        invite_id: String,
        received: u64,
        total: u64,
    },
    /// Receiver → Sender: transfer finished.
    PushDone {
        invite_id: String,
        status: String, // "completed" | "failed"
        reason: Option<String>,
    },
    /// Sender → Receiver: **对账**（推送回执的补漏）。
    ///
    /// 回执（进度/完成/拒绝）是"一次、尽力而为"的：长连接在"检查有没有会话"和"真的发出去"
    /// 之间断掉、或那一跳 HTTP 补发失败，回执就没了 —— 发送方那一台会一直停在「等待」，
    /// 5 分钟后被判成「对方未响应」，而对面其实早就收完了。链路回来（或探活发现链路在）时，
    /// 发送方把自己**还没落终态**的邀请号问一遍，由接收方按它的记录回答（`PushState`）。
    /// 一次一批，只走长连接（对账的前提就是"链路在"）。
    PushAsk {
        /// 这一次问的凭据（回执原样带回来，日志里对得上）。
        ask_id: String,
        /// 发送方还没落终态的邀请号。
        invite_ids: Vec<String>,
    },
    /// Receiver → Sender: 对账的答案（`PushAsk` 的回执）。
    ///
    /// **只答"本机确实收到过、或此刻还拿着"的邀请**：没有记录的答 `unknown` ——
    /// 那说明这条邀请根本没送到，发送方据此当场收手，不必再等满 5 分钟。
    PushState {
        ask_id: String,
        states: Vec<crate::transfer::push_job::PushInviteStateWire>,
    },
    /// Sharer → Viewer: a broadcast share changed (F-33).
    ShareChanged {
        share_id: String,
        /// Path relative to the share root.
        path: String,
        /// created | modified | deleted
        kind: String,
    },
    /// Sharer → Viewer: 本机共享项清单本身变了（新增/移除/改名/开播开关）。
    /// 载荷为空——对端收到后重新拉一次 `/shares` 即可。
    SharesChanged,
    /// One side unpairs; the other clears the token too (F-14).
    Unpair {
        peer_id: String,
    },
    /// 会话：Sender → Receiver 的一条文本消息。
    ///
    /// `id` 由发送方生成，接收方原样存下来 —— 接收方的回执（`ChatRead`）按 `at` 比水位，
    /// 而 `id` 是"这条到底是哪一条"的凭据（重复投递、重连补发都靠它认）。
    ChatText {
        id: String,
        text: String,
        /// 发送方的墙上时间（epoch 毫秒）：接收方原样存，回执按它比水位。
        at: u64,
    },
    /// 会话：Receiver → Sender 的已读回执。`up_to` 是接收方**见到过的、你发来的最大 `at`**。
    ///
    /// 一条回执覆盖一批消息（不是逐条 ack）：人是一屏一屏读的，回执也该一屏一次。
    ChatRead {
        up_to: u64,
    },
    /// 群聊：Sender → Receiver 的一条**群消息**。
    ///
    /// 与 `ChatText` 分开而不是给它加可选字段：群消息属于"这个群"，一对一属于"与这台设备的
    /// 往来"，两者落盘、取未读、算回执的方式都不一样（见 `crate::group`）。
    ///
    /// - `id` 由发送方生成：去重键是 `(作者, id)`（补发/重连重投都是同一个 id）。
    /// - `seq` = 这个作者在该群里的第几条（1 起）：同一个人的先后不看墙钟。
    /// - `rev` = 作者手上的名册版本：收方据此知道自己落后（补拉会给回来）。
    /// - **`author` 是"谁说的"，不是本帧的对端**：中转过来的消息里，本帧的对端是搬运工
    ///   （`POST /chat/group` 体上的 `sender` / WS 会话的持有者），作者另有其人。
    ///   所以这个字段**不能**叫 `sender` —— HTTP 兜底那一跳的信封上已经有一个 `sender`
    ///   （发送方自报的身份，用来认人），同名会被信封覆盖掉，作者就变成了搬运工。
    /// - 收方对这个自称只在**它确实是名册成员**时才采信（否则谁都能借一台成员设备冒名）。
    GroupText {
        group_id: String,
        id: String,
        text: String,
        at: u64,
        seq: u64,
        rev: u64,
        author: PeerInfo,
        /// 这条被**中转**了几跳（发送方自己发 = 0）。跨网段时两端之间没有路径，缺的那一段
        /// 只能由链路上也在群里的成员帮着搬（本机开关 `groupRelay`，默认关）；跳数封顶
        /// （`group::MAX_HOPS`）保证转发一定停下来。
        #[serde(default)]
        hops: u8,
        /// 这一路上**已经经手过**的成员 id。收到的人据此跳过"已经有人送过"的成员，
        /// 不必再把同一条投给全群（去重另有 `(from, id)` 兜底）。
        ///
        /// 注意它是**声明**、没有签名：谎报只会让别人少转发，不会伪造出消息本身。
        #[serde(default)]
        reached: Vec<String>,
    },
    /// 群聊：Receiver → Sender 的已读回执。`up_to` = 读到的、**你在这个群里发的**最大 `at`。
    ///
    /// 与 `ChatRead` 同一个语义，只是多一个群维度：群里有多个人，水位必须逐发送者算
    /// （对方的时钟只有它自己那一条可比）。
    GroupRead {
        group_id: String,
        up_to: u64,
    },
    /// 群聊：名册（建群邀请 / 成员变更 / 补发时补的那一份）。
    ///
    /// 收到不认识的名册**不等于**加入：它先落成一张邀请卡，用户点了「加入」才算群
    /// （见 `crate::group::PendingInvite`）。
    GroupRoster {
        roster: crate::group::RosterPayload,
        sender: PeerInfo,
    },
    /// 群聊：**补拉** —— 向一个成员要"我手上没有的那些"。
    ///
    /// 为什么要它：群消息是逐成员直投的（无组播），跨网段/离线期间漏掉的那些，发送方投不
    /// 进来也没法重投；由**接收方**向"它连得上的成员"要，缺的那一段才补得回来
    /// （每个成员各向自己的邻居要，信息像涟漪一样沿链路传下去）。
    ///
    /// - `since_at` = 我手上最新一条的 `at`（**0 = 我一条都没有**：这时要的是"从头发一份给
    ///   我"，新加入的成员就是这样追上群里的；这种情况下对方只回一批、不再追后续）。
    /// - `holes` = 我**明确知道**缺的那几条（按发送方 + `seq`）：`since_at` 只能表达"之后的
    ///   全要"，中间那几个洞只能一条条点名。
    GroupSyncAsk {
        group_id: String,
        /// 这一问的凭据（回执原样带回来，日志里对得上）。
        ask_id: String,
        since_at: u64,
        holes: Vec<crate::group::SyncHole>,
    },
    /// 群聊：补拉的回答（有货就回，没货回空）。
    GroupSyncPost {
        group_id: String,
        ask_id: String,
        /// 顺带把名册给它：补拉常常正是"对方的群名册已经落后"的场景，一次往返解决。
        roster: Option<crate::group::RosterPayload>,
        /// 我手上还有更多（对方按这个继续要下一批）。
        has_more: bool,
        messages: Vec<crate::group::GroupMsgWire>,
    },
    /// 群聊：**"我手上有你发的这些"** —— 补拉拿到的消息回执给原发送方。
    ///
    /// 与 `GroupRead` 分开：那是"读过"，这是"**拿到了**"（补拉回来的消息往往还没被人看过）。
    /// 原发送方据此把那条副本从「排队/未送达」改成「已同步」。
    GroupAck {
        group_id: String,
        /// 我手上你发的消息里最大的一条 `at`。
        up_to: u64,
    },
}

impl WsEvent {
    pub fn type_name(&self) -> &'static str {
        match self {
            WsEvent::PeerOnline { .. } => "peer_online",
            WsEvent::PeerOffline { .. } => "peer_offline",
            WsEvent::PushInvite { .. } => "push_invite",
            WsEvent::PushResponse { .. } => "push_response",
            WsEvent::PushProgress { .. } => "push_progress",
            WsEvent::PushDone { .. } => "push_done",
            WsEvent::PushAsk { .. } => "push_ask",
            WsEvent::PushState { .. } => "push_state",
            WsEvent::ShareChanged { .. } => "share_changed",
            WsEvent::SharesChanged => "shares_changed",
            WsEvent::Unpair { .. } => "unpair",
            WsEvent::ChatText { .. } => "chat_text",
            WsEvent::ChatRead { .. } => "chat_read",
            WsEvent::GroupText { .. } => "group_text",
            WsEvent::GroupRead { .. } => "group_read",
            WsEvent::GroupRoster { .. } => "group_roster",
            WsEvent::GroupSyncAsk { .. } => "group_sync_ask",
            WsEvent::GroupSyncPost { .. } => "group_sync_post",
            WsEvent::GroupAck { .. } => "group_ack",
        }
    }
}

/// Tauri event names emitted to the frontend.
pub mod ui {
    pub const PEER_ONLINE: &str = "peer://online";
    pub const PEER_OFFLINE: &str = "peer://offline";
    pub const DEVICES_CHANGED: &str = "devices://changed";
    pub const PAIR_COMPLETED: &str = "pair://completed";
    /// 收到一条配对邀请（F-12 免配对码）：界面挂一条带「接收/拒绝」的通知。
    pub const PAIR_INVITE: &str = "pair://invite";
    /// 一条挂着/等着的配对邀请作废了（超时 / 对端撤回）：两端的界面都凭它收手。
    pub const PAIR_INVITE_CLOSED: &str = "pair://invite_closed";
    /// 本机发起的配对邀请没成（对端拒绝 / 不在网上 / 撤回）：只有发起方收这条。
    pub const PAIR_INVITE_FAILED: &str = "pair://invite_failed";
    /// 一批推送（= 界面上一张卡片）的快照：创建、进度、成功/失败/取消都走它。
    /// 它是**发送侧卡片唯一的数据来源** —— 单台推送也是一批（只有一条目标），
    /// 所以"等待对方同意 → 传输中 → 已发送"和"3 台里成了几台"共用同一条事件。
    pub const PUSH_BATCH: &str = "push://batch";
    pub const PUSH_INVITE: &str = "push://invite";
    pub const PUSH_RESPONSE: &str = "push://response";
    pub const PUSH_PROGRESS: &str = "push://progress";
    pub const PUSH_DONE: &str = "push://done";
    /// 一条消息**暂时送不出去**（直连不通、也没有可用的中转方，F-106）：界面挂一句提示。
    ///
    /// 排队本身是既有语义（F-95：等它上线再发），这条事件补的是**为什么现在送不出去** ——
    /// 少了它，用户在"被隔离的 Wi‑Fi 上发给一台没配上的设备"时看到的只是"发了没反应"。
    pub const CHAT_UNDELIVERABLE: &str = "chat://undeliverable";
    pub const SHARE_CHANGED: &str = "share://changed";
    /// 对端共享项清单变化（新增/移除/改名/开播开关）——设备标签里的列表需要重新拉取。
    pub const SHARES_CHANGED: &str = "shares://changed";
    pub const SERVER_STARTED: &str = "server://started";
    pub const SERVER_ERROR: &str = "server://error";
    /// 第二份进程被启动 → 本实例被调到前台，悬浮球面板展开。
    pub const ORB_SHOW: &str = "orb://show";
    /// 本机「在线 / 离线」变了（面板右上角状态框）：界面更新那个框。载荷 `{ online: bool }`。
    pub const LOCAL_ONLINE: &str = "local://online";
    /// 本机网卡地址变了（插网线、开/关热点、切 Wi-Fi、VPN 起来）：界面重新拉一次
    /// `local_addresses`（左下角「端口 · IP」那一条）。载荷为空 —— 值由界面自己拉
    /// （一份真相，不来回传）。
    pub const LOCAL_ADDRESSES: &str = "local://addresses";
    /// 自更新的状态变了（检查中 / 已是最新 / 有新版 / 下载进度 / 校验 / 落地 / 出错）。
    ///
    /// 载荷是**整份 `update::UpdateState` 快照**（含已下载字节数），与 `PUSH_PROGRESS`
    /// 同一个取舍：进度是高频事件，增量事件会让前端自己攒状态，迟早对不上；快照每拍都全，
    /// 界面只负责画。
    pub const UPDATE_STATE: &str = "update://state";
    /// 会话状态变了（收到消息 / 自己发出 / 送达状态推进 / 已读 / 建群退群）。
    ///
    /// 载荷是**整份会话快照**（和 `chat_snapshot` 命令同一个形状）：会话是人的打字速度，
    /// 增量事件的收益抵不过"两份状态各自演算、迟早对不上"的风险 —— 与 `PUSH_BATCH` 同一个取舍。
    pub const CHAT_CHANGED: &str = "chat://changed";
}
