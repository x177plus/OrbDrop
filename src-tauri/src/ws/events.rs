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
        is_dir: bool,
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
        }
    }
}

/// Tauri event names emitted to the frontend.
pub mod ui {
    pub const PEER_ONLINE: &str = "peer://online";
    pub const PEER_OFFLINE: &str = "peer://offline";
    pub const DEVICES_CHANGED: &str = "devices://changed";
    pub const PAIR_COMPLETED: &str = "pair://completed";
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
}
