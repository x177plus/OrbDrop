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
            WsEvent::ShareChanged { .. } => "share_changed",
            WsEvent::SharesChanged => "shares_changed",
            WsEvent::Unpair { .. } => "unpair",
            WsEvent::ChatText { .. } => "chat_text",
            WsEvent::ChatRead { .. } => "chat_read",
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
    pub const SHARE_CHANGED: &str = "share://changed";
    /// 对端共享项清单变化（新增/移除/改名/开播开关）——设备标签里的列表需要重新拉取。
    pub const SHARES_CHANGED: &str = "shares://changed";
    pub const SERVER_STARTED: &str = "server://started";
    pub const SERVER_ERROR: &str = "server://error";
    /// 第二份进程被启动 → 本实例被调到前台，悬浮球面板展开。
    pub const ORB_SHOW: &str = "orb://show";
    /// 本机「在线 / 离线」变了（面板右上角状态框）：界面更新那个框。载荷 `{ online: bool }`。
    pub const LOCAL_ONLINE: &str = "local://online";
    /// 会话状态变了（收到消息 / 自己发出 / 送达状态推进 / 已读 / 建群退群）。
    ///
    /// 载荷是**整份会话快照**（和 `chat_snapshot` 命令同一个形状）：会话是人的打字速度，
    /// 增量事件的收益抵不过"两份状态各自演算、迟早对不上"的风险 —— 与 `PUSH_BATCH` 同一个取舍。
    pub const CHAT_CHANGED: &str = "chat://changed";
}
