//! 会话（聊天）：文本消息的本地真相 + 给界面的快照。
//!
//! 线上协议只有两条（`WsEvent::ChatText` / `WsEvent::ChatRead`，见 `ws::events`）——
//! 这一层不碰网络，只管"谁跟谁说过什么、哪条到没到、哪条已被对方读过"。
//!
//! **为什么落盘**：会话记录是用户数据（跟共享项、地址簿同一档），放在 `config.toml`
//! 旁边的 `chat-vault/chat.dat` 里，重启后还在。不塞进 `config.toml`：那是"配置"，不该被
//! 聊天记录撑大，也不该因为手改一次配置被连带丢掉。文件是封装格式（`envelope`，
//! **不是加密**）：双击看不到明文。
//!
//! **落盘的两种消息**：文本（`ChatMessage`，这一层的线上协议产生）与**文件消息**
//! （`ChatFileMsg`，会话里那条"发了/收到了一个文件"的气泡）。文件消息的实时状态由前端
//! 掌握（进度每 200ms 一跳），前端只在能定下来的时刻把整份列表写回来（`chat_files_set`）——
//! 它以前只活在内存里，重启就丢，害得"只发过文件"的会话整条从列表里消失。
//!
//! **模型**：消息按**对端设备**存（`peer_id → 消息`）——这是唯一的消息存储。
//!
//! 「群发」是**一批收件人**（`Batch`：id + 名字 + 成员），它**不拥有消息**：
//! 群发就是把这条消息给**每台各存一份**（同一个消息 id，各带各的送达状态），并在副本上
//! 打一个 `batch_id` 标明"这次是走哪个群发发的"。于是：
//! - A 的会话列表里，「与 B」「与 C」两条各自带着这条消息（**两边都在**：1:1 是"与这台设备
//!   的完整往来"，不能让对方的回复悬空），群发那条则是"我发给这几台的记录"（筛 batch_id）。
//! - 逐台状态由同一个 id 的几份副本拼出来（谁已读、谁已送达、谁没送到）。
//! - **对端永远只看到一条普通消息**（它不知道也不需要在某个"群"里），回复各自回到与发送方的
//!   1:1 —— 这是本项目网络拓扑（两两配对、无组播）下唯一诚实的语义。
//! - 删掉一个群发只删"这批收件人的名字/名单"，消息一条不动（它们本来就在各台会话里）。
//!
//! **已读水位**：每台设备一条 `read_at`，`at <= read_at` 的**对端消息**算读过，未读数由
//! 它算出来（不逐条记"已读"）。水位用的 `at` 是**发送方**的时钟，回执原样带回去，
//! 所以只在自己的时钟域里比较，两端不需要对表。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::blobs;
use crate::config::{data_dir, write_atomic};
use crate::envelope;
use crate::state::AppState;

/// 每台设备最多留多少条：文本消息很小，但"会话"不该无限长下去。
/// 超了从头砍 —— 会话列表与详情都是看最近的，砍掉的是最够不着的那一头。
const MAX_PER_PEER: usize = 500;

/// 最多保留多少个一对一会话。未配对来源可以凭自报 id 无限新建会话，而每条消息都会
/// 整份重写 `chat.dat` —— 没有总数上限时，一个随机 id 循环就能把磁盘/CPU 打满。
/// 4096 远高于正常设备数；超出后新来源的消息被忽略（已有会话不受影响）。
const MAX_CONVOS: usize = 4096;

/// 一条消息在一台设备上的状态（本机视角）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum Delivery {
    /// 在途：没有长连接，正走 HTTP 兜底那一跳（`deliver_http`）还没回来。
    Sending,
    /// 本机发出，已交给对端：写进长连接，或 HTTP `POST /chat/*` 拿到了 200。
    Sent,
    /// 对端读过（收到它的 `ChatRead` 回执）。
    Read,
    /// **排队等对端上线**：发的时候它不在网上（连不上、或它自己 5xx —— 隐身时它整个
    /// 路由回 503）。它一上线（探活/mDNS 判它在网、链路接上、下一次探活那一拍）就按
    /// 原顺序自动发出去，见 `flush_queued`。这一档的消息**没丢**，只是还在排队。
    Queued,
    /// 收到的消息：本机视角没有"送达"可言。
    Received,
    /// **经别人补到的**：本机（补拉/别人中转）拿到的消息让对方回了"我手上有你发的这些"。
    ///
    /// 与「已送达」分开：它不是"本机投出去的那一跳成功了"，而是"**另一方自己从别人那儿
    /// 补到了**"。跨网段时本机根本没有到对方的路径，这一档是唯一说得出口的实话。
    Synced,
    /// 送不出去：对端**明确拒绝**（4xx：不认本机凭据、认不出这个请求体 —— 只能重新配对）。
    /// 重试不会变好，所以只试这一次，不等它上线。
    ///
    /// 顺带兼作**未知状态的兜底**（serde 要求它挂在最后一个变体上）：以后再加状态时，
    /// 老版本读到不认识的词就当「未送达」，而不是把整份 `chat.dat` 解析废掉
    /// （一个词不认识 → 解析失败 → 整份历史回落成空）。
    #[serde(other)]
    Failed,
}

/// 会话里的一条文本消息。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatMessage {
    /// 消息 id 由发送方生成；群发的每台副本共用同一个 id（前端据此合并成一条）。
    pub id: String,
    pub from_me: bool,
    /// 对端消息的发送方名字（群视图要写"谁说的"）；本机发出的为空。
    pub from_name: String,
    /// 文字消息的内容。**图片消息这里是空串**（内容在 `blobs` 那份文件里）。
    pub text: String,
    /// 发送方的墙上时间（epoch 毫秒）：排序与已读水位都按它。
    pub at: u64,
    pub delivery: Delivery,
    /// 本机发出时这条消息的目标设备清单（群发时每份副本都带着它，界面上才能逐台写状态）。
    pub targets: Vec<String>,
    /// 这条是走哪个群发发出去的（`None` = 一对一发的）。老文件里没有这个键。
    #[serde(default)]
    pub batch_id: Option<String>,
    /// 这条是不是**图片消息**（图片是消息内容，不是文件 —— 对端直接就能看，见 `blobs`）。
    /// 老文件里没有这个键 = 文字消息。
    #[serde(default)]
    pub image: bool,
    /// 图片消息的内容文件名（"图片-20261013-153012.png"）：扩展名是它决定的，界面退回
    /// "文件行"样式时也写它。文字消息为空。
    #[serde(default)]
    pub name: String,
    /// 图片消息的字节数（界面上写"多大"；文字消息为 0）。
    #[serde(default)]
    pub size: u64,
}

/// 一批收件人（界面上的「群发 · 名字」）：名字可以留空，成员可以加减。
///
/// 它只是一份**名单**：消息不在它这儿，成员变了也不动历史消息（老消息的 `targets` 就是它
/// 当时的那份名单，逐台状态按各自的副本显示）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatBatch {
    pub id: String,
    /// 显示名（`群发 · <名字>`）。空 = 界面显示「群发 · 未命名群发」。
    #[serde(default)]
    pub name: String,
    /// 成员设备 id（保持用户选择的顺序，已去重）。
    #[serde(default)]
    pub members: Vec<String>,
}

/// 会话里那条**文件（推送）消息**的落盘形状 —— 与前端 `ChatFileMsg` 逐字段对应。
///
/// **为什么它也在 `chat.dat` 里**：以前这条气泡只活在前端内存里，重启即丢 —— 而"会话列表
/// 只列有内容的会话"，于是只发过文件、没说过话的那台设备整条从列表里消失；重发一条文本把它
/// 拉回来时，文件那几条也永远回不来了。文件消息与文本一样是用户数据，所以与文本同处落盘。
///
/// **前端仍是它的实时作者**：这条气泡的进度是传输中的高频状态（`push://progress` 每 200ms
/// 一拍），不值得每一拍都落盘。前端只在**能定下来的时刻**（刚发出 / 刚收到 / 接受或拒绝 /
/// 收完或失败 / 整批走完）把该设备那份**整列表**写回这里（`chat_files_set`）——所以这份落盘
/// 是"记录"，不是"实时视图"；断在这个中间态的进程重启时由 `requeue_inflight` 落成终态。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatFileMsg {
    #[serde(default)]
    pub id: String,
    #[serde(default)]
    pub at: u64,
    #[serde(default)]
    pub from_me: bool,
    #[serde(default)]
    pub name: String,
    /// 文件夹还是文件（气泡/卡片上那颗图标）。
    #[serde(default)]
    pub is_dir: bool,
    /// 收到的那条是谁推过来的（`from_me` 时为空）。
    #[serde(default)]
    pub peer_id: String,
    /// 显示用的大小串（`文件夹` / `8.2 MB`）：前端算什么就记什么，后端不重算。
    #[serde(default)]
    pub size: String,
    /// 发送侧：这批推到几台、几台没成。
    #[serde(default)]
    pub targets: u32,
    #[serde(default)]
    pub failed: u32,
    #[serde(default)]
    pub pct: u32,
    #[serde(default)]
    pub sending: bool,
    /// 发送侧：`push://batch` 那张卡的 id（结算后前端会清掉）。
    #[serde(default)]
    pub batch_id: String,
    /// 发送侧：这条属于哪份**群发名单**（空 = 不是群发那条发的）。
    #[serde(default)]
    pub chat_batch_id: String,
    /// 发送侧：这条属于哪个**群**（空 = 不是发到群里的）。
    #[serde(default)]
    pub group_id: String,
    /// 发送侧：逐台的推送状态（前端透传的字符串，后端不解读）。
    #[serde(default)]
    pub per_targets: Vec<ChatFileTarget>,
    /// 接收侧：那条推送邀请。
    #[serde(default)]
    pub invite_id: String,
    /// 接收侧：`pending` / `accepted` / `received` / `rejected` / `failed` / 空。
    #[serde(default)]
    pub invite_state: String,
    #[serde(default)]
    pub received: u64,
    #[serde(default)]
    pub bytes: u64,
    /// 本机上这一项的绝对路径（空 = 还没有 / 不知道，界面别开它）。
    #[serde(default)]
    pub path: String,
    /// 接收侧：发送方这次选了几项（>1 = 多选合并成的一条推送）。
    #[serde(default)]
    pub count: u32,
}

/// 文件消息里的一台目标设备（`ChatFileMsg.per_targets` 的一行）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatFileTarget {
    #[serde(default)]
    pub peer_id: String,
    /// 这一台那条推送邀请的 id（前端在气泡上按停止时用它逐台取消；空 = 老记录 / 已不可取消）。
    #[serde(default)]
    pub invite_id: String,
    /// `PushTargetState` 的取值（`invited` / `sending` / `done` / `cancelled` / `rejected` / `failed`），
    /// 前端透传，后端只在进程重启时把它落成 `failed`。
    #[serde(default)]
    pub state: String,
}

#[derive(Debug, Default, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PeerConvo {
    #[serde(default)]
    messages: Vec<ChatMessage>,
    #[serde(default)]
    read_at: u64,
    /// 这台设备会话里的文件消息（老文件里没有这个键 = 空）。
    #[serde(default)]
    files: Vec<ChatFileMsg>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct ChatFile {
    #[serde(default)]
    convos: HashMap<String, PeerConvo>,
    /// 群发名单（"这批人"）。往老文件里读时没有这个键 = 空。
    #[serde(default)]
    batches: Vec<ChatBatch>,
}

/// 一台设备的会话，给界面看的那份。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerConvoView {
    pub peer_id: String,
    pub messages: Vec<ChatMessageView>,
    /// 这台设备会话里的文件消息（记录，不是实时视图 —— 见 `ChatFileMsg`）。
    pub files: Vec<ChatFileMsg>,
    pub read_at: u64,
    pub unread: u32,
}

/// 一条消息给界面看的样子：落盘那份（[`ChatMessage`]）加一个**本机此刻**的内容路径。
///
/// `path` 不进 `chat.dat`：那串是"这台机器上这个文件在哪"，换台机器、换个数据目录
/// （便携部署、`ORBDROP_DATA_DIR`）就全错了。所以它只在快照这一层现算（`flatten` 之后
/// 界面上读到的还是那一堆平字段）。
///
/// `Deref` 转发到里面那条消息：Rust 这边读 `view.delivery` / `view.id` 这样的字段自动落到
/// `msg` 上（`path` 是这一层自己的字段，直接读）—— 两者**没有同名字段**，不会打架。
#[derive(Debug, Clone, Serialize)]
pub struct ChatMessageView {
    #[serde(flatten)]
    pub msg: ChatMessage,
    /// 图片消息的内容文件（绝对路径）；文字消息、或内容文件已经不在盘上的一律空串。
    pub path: String,
}

impl std::ops::Deref for ChatMessageView {
    type Target = ChatMessage;
    fn deref(&self) -> &ChatMessage {
        &self.msg
    }
}

impl ChatMessageView {
    /// 一条落盘的消息 → 界面那一份（图片消息顺手把内容路径算出来）。
    fn of(msg: ChatMessage, root: &std::path::Path) -> Self {
        let path = if msg.image {
            crate::blobs::existing_path(root, &msg.id, &msg.name)
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default()
        } else {
            String::new()
        };
        Self { msg, path }
    }
}

/// 整份会话状态（命令返回 + `chat://changed` 事件的载荷）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub convos: Vec<PeerConvoView>,
    /// 群发名单（界面上的「群发 · 名字」一行行就是它）。
    pub batches: Vec<ChatBatch>,
    /// 全部未读（底栏那颗角标）。
    pub unread: u32,
}

pub struct ChatStore {
    path: PathBuf,
    /// 图片消息的**内容**目录（见 `blobs`）：生产上是 `chat-vault/blobs/`；测试由
    /// `with_path` 推到临时目录旁边（与账本同一个家）。
    blobs_root: PathBuf,
    inner: Mutex<ChatFile>,
    /// 正在补发的设备（`flush_queued` 的闸门）：一台设备同一时刻只跑一条补发。
    /// 值 = "这一轮在飞的时候又有人叫过一次" —— 那一轮跑完会再走一轮（见 `begin_flush`）。
    flushing: Mutex<HashMap<String, bool>>,
}

impl ChatStore {
    pub fn new() -> Self {
        let root = data_dir();
        Self::with_blobs(envelope::chat_path(&root), envelope::blobs_dir(&root))
    }

    /// 指定文件的构造（单测用：不碰真实数据目录）。
    pub fn with_path(path: PathBuf) -> Self {
        let blobs_root = path
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join("blobs");
        Self::with_blobs(path, blobs_root)
    }

    /// 指定账本文件与图片目录的构造（迁移回退时两者可能不在同一个目录）。
    pub fn with_blobs(path: PathBuf, blobs_root: PathBuf) -> Self {
        let inner = match std::fs::read(&path) {
            Ok(raw) => {
                let decoded = match envelope::decode_if(&raw) {
                    Ok(Some((_, bytes))) => Some(bytes),
                    // 老版本的明文：迁移还没跑到/回退时仍要能读。
                    Ok(None) => Some(raw),
                    Err(e) => {
                        // 损坏的记录不是"没有记录"：先留一份 `.bad`，再从空库启动。
                        // 备份也写成封装格式，避免老明文借 `.bad` 漏出去。
                        match envelope::backup_bad(&path, &raw) {
                            Some(backup) => warn!("会话记录损坏（{e}）：已备份到 {backup:?}"),
                            None => warn!(
                                "会话记录损坏（{e}），且无法备份 {:?}：本次从空库启动，但不会主动覆盖原文件",
                                path
                            ),
                        }
                        None
                    }
                };
                match decoded {
                    Some(bytes) => match serde_json::from_slice::<ChatFile>(&bytes) {
                        Ok(file) => file,
                        Err(e) => {
                            match envelope::backup_bad(&path, &bytes) {
                                Some(backup) => warn!("会话记录损坏（{e}）：已备份到 {backup:?}"),
                                None => warn!(
                                    "会话记录损坏（{e}），且无法备份 {:?}：本次从空库启动，但不会主动覆盖原文件",
                                    path
                                ),
                            }
                            ChatFile::default()
                        }
                    },
                    None => ChatFile::default(),
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => ChatFile::default(),
            Err(e) => {
                warn!("会话记录读取失败（{e}）：本次从空库启动");
                ChatFile::default()
            }
        };
        Self {
            path,
            blobs_root,
            inner: Mutex::new(inner),
            flushing: Mutex::new(HashMap::new()),
        }
    }

    /// 图片消息的内容目录（生产上就是 `blobs::dir()`，见那里的说明）。
    pub fn blobs_root(&self) -> &std::path::Path {
        &self.blobs_root
    }

    /// 现在这一刻的快照。
    pub fn snapshot(&self) -> Snapshot {
        let guard = self.inner.lock();
        let root = self.blobs_root.clone();
        let mut views: Vec<PeerConvoView> = guard
            .convos
            .iter()
            .map(|(peer_id, c)| PeerConvoView {
                peer_id: peer_id.clone(),
                messages: c
                    .messages
                    .iter()
                    .cloned()
                    .map(|m| ChatMessageView::of(m, &root))
                    .collect(),
                files: c.files.clone(),
                read_at: c.read_at,
                unread: unread_of(c),
            })
            .collect();
        // 稳定顺序：界面上"哪台有未读"不该随机跳动（会话列表自己按最近一条排）。
        views.sort_by(|a, b| a.peer_id.cmp(&b.peer_id));
        let unread = views.iter().map(|v| v.unread).sum();
        Snapshot {
            convos: views,
            batches: guard.batches.clone(),
            unread,
        }
    }

    /// 每台设备**最后一个非空的、对端自己报过的名字**（`设备 id → 名字`）。
    ///
    /// 名字的第二个来源（第一个是设备行那份观察值）：对端自己发来的消息带着它当时的设备名，
    /// 落在这里、跨重启还在。用途见 `AppState::new` —— 启动时用它给**占位名**的设备行播种，
    /// 否则"设备列表/会话列表先显示一串 deviceid"要等到第二次观察（探活回话 / 它再开口）。
    ///
    /// 只认对端发来的那几条（`from_me` 的 `from_name` 按约定是空串）、空串一律跳过；
    /// 从最后一条往前找，找到就用（"它现在叫什么"比"它当年叫什么"更接近事实）。
    pub fn last_inbound_names(&self) -> HashMap<String, String> {
        let guard = self.inner.lock();
        let mut out = HashMap::new();
        for (peer_id, convo) in guard.convos.iter() {
            for m in convo.messages.iter().rev() {
                let name = m.from_name.trim();
                if !m.from_me && !name.is_empty() {
                    out.insert(peer_id.clone(), name.to_string());
                    break;
                }
            }
        }
        out
    }

    /// 本机发出一条消息：每台目标各存一份（同一个 id），送达状态逐台记。
    ///
    /// `targets` 已经由调用方定过这一份的初态（`send_text` / `send_image`）：`Sent` = 写进了
    /// 那条长连接，`Sending` = 正走 HTTP 兜底那一跳，`Queued` = 这一台还欠着更早的消息（跟着排队）。
    /// `batch_id` = 这次是走哪个群发发的（副本上打标，群发那条视图据此筛出来）。
    pub fn record_outbound(
        &self,
        id: &str,
        content: impl Into<blobs::Content>,
        at: u64,
        targets: &[(String, Delivery)],
        batch_id: Option<&str>,
    ) -> Snapshot {
        let content = content.into();
        let ids: Vec<String> = targets.iter().map(|(p, _)| p.clone()).collect();
        let image = content.image.clone();
        {
            let mut guard = self.inner.lock();
            for (peer_id, delivery) in targets {
                let convo = guard.convos.entry(peer_id.clone()).or_default();
                convo.messages.push(ChatMessage {
                    id: id.to_string(),
                    from_me: true,
                    from_name: String::new(),
                    text: content.text.clone(),
                    at,
                    delivery: *delivery,
                    targets: ids.clone(),
                    batch_id: batch_id.map(str::to_string),
                    image: image.is_some(),
                    name: image.as_ref().map(|(n, _)| n.clone()).unwrap_or_default(),
                    size: image.as_ref().map(|(_, s)| *s).unwrap_or(0),
                });
                prune(convo, &self.blobs_root);
            }
            persist(&self.path, &guard);
        }
        debug!(id, at, targets = targets.len(), "chat outbound recorded");
        self.snapshot()
    }

    /// 收到对端一条消息（文字）。
    ///
    /// **按 id 幂等**：这条路的投递是"至少一次"（补发、重连后重投、HTTP 到了但对端没来得及
    /// 回话、进程中途被杀又重投），同一个 id 进来第二次不能再写一条 —— 否则对方一句"在吗"
    /// 会变成两句。已存在就整条不动（内容一样，`at` 是对方给的发送时刻，本来就该原样）。
    ///
    /// 返回值第一格是"这一条**确实新记下**"（重复投递 = `false`）：手机通知栏响不响看它
    /// （见 `notify`）—— 同一条消息因为重投响第二遍，比不响更烦人。
    pub fn record_inbound(
        &self,
        peer_id: &str,
        from_name: &str,
        id: &str,
        text: &str,
        at: u64,
    ) -> (bool, Snapshot) {
        self.record_inbound_msg(peer_id, from_name, id, blobs::Content::text(text), at)
    }

    /// 收到对端一条**图片消息**（内容已经由调用方写进 `blobs` 了）。
    ///
    /// 与文字那条同一个幂等口径：重投进来的同一条**不重写**内容（对方给的字节与本地那份一样，
    /// 而重写会在渲染着这一条的时候把文件从底下换掉）。
    pub fn record_inbound_image(
        &self,
        peer_id: &str,
        from_name: &str,
        id: &str,
        name: &str,
        size: u64,
        at: u64,
    ) -> (bool, Snapshot) {
        self.record_inbound_msg(
            peer_id,
            from_name,
            id,
            blobs::Content::image(name, size),
            at,
        )
    }

    /// 收到一条消息的公共那一半（文字与图片只差内容那几格）。
    fn record_inbound_msg(
        &self,
        peer_id: &str,
        from_name: &str,
        id: &str,
        content: blobs::Content,
        at: u64,
    ) -> (bool, Snapshot) {
        let duplicate = {
            let guard = self.inner.lock();
            guard
                .convos
                .get(peer_id)
                .is_some_and(|c| c.messages.iter().any(|m| !m.from_me && m.id == id))
        };
        if duplicate {
            debug!(id, %peer_id, "inbound chat already recorded (redelivery)");
            return (false, self.snapshot());
        }
        let image = content.image.clone();
        {
            let mut guard = self.inner.lock();
            if !guard.convos.contains_key(peer_id) && guard.convos.len() >= MAX_CONVOS {
                drop(guard);
                warn!(%peer_id, limit = MAX_CONVOS, "收到新会话但会话数已达上限，丢弃这条消息");
                return (false, self.snapshot());
            }
            let convo = guard.convos.entry(peer_id.to_string()).or_default();
            convo.messages.push(ChatMessage {
                id: id.to_string(),
                from_me: false,
                from_name: from_name.to_string(),
                text: content.text,
                at,
                delivery: Delivery::Received,
                targets: vec![peer_id.to_string()],
                batch_id: None,
                image: image.is_some(),
                name: image.as_ref().map(|(n, _)| n.clone()).unwrap_or_default(),
                size: image.as_ref().map(|(_, s)| *s).unwrap_or(0),
            });
            prune(convo, &self.blobs_root);
            persist(&self.path, &guard);
        }
        (true, self.snapshot())
    }

    /// 把一条副本的送达状态落定（HTTP 兜底那一跳、补发那一跳回来时用）。
    ///
    /// **只从"还没定"的档改到终态**：`Sending`（那一跳在途）与 `Queued`（排着队，补发正试）
    /// 可以变成终态；已经「已送达/已读」的不回退（HTTP 回来晚于对端的已读回执时，
    /// 不能把「已读」改写回「已送达」）；`Failed` 也不会被这一下洗白（那是另一条路写下的）。
    pub fn set_delivery(&self, peer_id: &str, msg_id: &str, delivery: Delivery) -> Snapshot {
        {
            let mut guard = self.inner.lock();
            if let Some(convo) = guard.convos.get_mut(peer_id) {
                if let Some(m) = convo
                    .messages
                    .iter_mut()
                    .find(|m| m.from_me && m.id == msg_id)
                {
                    if matches!(m.delivery, Delivery::Sending | Delivery::Queued) {
                        m.delivery = delivery;
                        persist(&self.path, &guard);
                    }
                }
            }
        }
        self.snapshot()
    }

    /// 对端回执：它读到了 `at <= up_to` 的那些消息 → 我的那几份副本从"已送达"变"已读"。
    ///
    /// 只动 `Sent` / `Sending`：回执本身就是"对方收到了"的证据（HTTP 那条路还在飞的时候
    /// 回执可能先到 —— 那种情况也要落成「已读」，不然它会永远停在「在途」）；`Read` 不重复标，
    /// `Failed` 更不能被这一下洗白 —— 没送出去的东西对端不可能读过。
    pub fn mark_read_by_peer(&self, peer_id: &str, up_to: u64) -> Snapshot {
        {
            let mut guard = self.inner.lock();
            if let Some(convo) = guard.convos.get_mut(peer_id) {
                for m in convo.messages.iter_mut() {
                    if m.from_me
                        && m.at <= up_to
                        && matches!(m.delivery, Delivery::Sent | Delivery::Sending)
                    {
                        m.delivery = Delivery::Read;
                    }
                }
            }
            persist(&self.path, &guard);
        }
        self.snapshot()
    }

    /// 本机读过这台设备的消息：水位推进（只增不减）。
    pub fn mark_read_local(&self, peer_id: &str, at: u64) -> Snapshot {
        {
            let mut guard = self.inner.lock();
            let convo = guard.convos.entry(peer_id.to_string()).or_default();
            if at > convo.read_at {
                convo.read_at = at;
                persist(&self.path, &guard);
            }
        }
        self.snapshot()
    }

    /// 这台设备发来的消息里最大的一条 `at` —— 回执要带回去的那个水位（没消息 = 0）。
    pub fn inbound_watermark(&self, peer_id: &str) -> u64 {
        self.inner
            .lock()
            .convos
            .get(peer_id)
            .map(|c| {
                c.messages
                    .iter()
                    .filter(|m| !m.from_me)
                    .map(|m| m.at)
                    .max()
                    .unwrap_or(0)
            })
            .unwrap_or(0)
    }

    /// 本机在这台设备的会话上读到哪了（已读水位）。
    pub fn read_watermark(&self, peer_id: &str) -> u64 {
        self.inner
            .lock()
            .convos
            .get(peer_id)
            .map(|c| c.read_at)
            .unwrap_or(0)
    }

    /// 这台设备是不是还欠着消息：有排队等它上线的副本（`Queued`）。
    ///
    /// 「欠着」不只是"这一条还没发出去"，它还是**顺序**的判据：补发期间副本**留在
    /// 「排队」档**（不挪成「在途」，见 `next_queued`），所以这几秒里新消息也会跟着排队。
    /// 否则后发的那条从长连接抢到前面去，对端看到的顺序就乱了 —— 而已读水位是"按发送方的
    /// 时钟比大小"（`at <= up_to`），乱序之后老的那条会被新回执一并算成"对方读过"。
    pub fn has_backlog(&self, peer_id: &str) -> bool {
        self.inner.lock().convos.get(peer_id).is_some_and(|c| {
            c.messages
                .iter()
                .any(|m| m.from_me && m.delivery == Delivery::Queued)
        })
    }

    /// 还欠着消息的设备（启动时一次扫完，见 `flush_all_queued`）。
    pub fn peers_with_backlog(&self) -> Vec<String> {
        let guard = self.inner.lock();
        let mut ids: Vec<String> = guard
            .convos
            .iter()
            .filter(|(_, c)| {
                c.messages
                    .iter()
                    .any(|m| m.from_me && m.delivery == Delivery::Queued)
            })
            .map(|(id, _)| id.clone())
            .collect();
        ids.sort();
        ids
    }

    /// 最早那条还排着队的消息（`None` = 不欠它了）。**不看别的档**：`Failed` 是"不发第二次"，
    /// 已经送达/已读的更不用管。**也不改状态** —— 补发中它仍是「排队」，见 `has_backlog`。
    pub fn next_queued(&self, peer_id: &str) -> Option<ChatMessage> {
        self.inner
            .lock()
            .convos
            .get(peer_id)?
            .messages
            .iter()
            .find(|m| m.from_me && m.delivery == Delivery::Queued)
            .cloned()
    }

    /// 补发的闸门：拿到 `true` 的调用方负责跑这一轮（跑完必须 `end_flush`）。
    ///
    /// 已经被占着时**不丢这次调用**，只记下"有人又叫过一次"：在飞的那一轮跑完会再走一轮。
    /// 丢掉它就是丢掉最新的证据 —— 那一轮手里那个判定（"够不着"）是几秒前的，而这一次调用
    /// 多半正是"对端刚上线"（探活/链路/地址刚补上），白白等到下一拍探活才发。
    pub fn begin_flush(&self, peer_id: &str) -> bool {
        let mut flushing = self.flushing.lock();
        match flushing.get_mut(peer_id) {
            Some(rerun) => {
                *rerun = true;
                false
            }
            None => {
                flushing.insert(peer_id.to_string(), false);
                true
            }
        }
    }

    /// 这一轮跑完了。返回 `true` = 期间有人又叫过 → 调用方**再跑一轮**（闸门还在自己手里）。
    pub fn end_flush(&self, peer_id: &str) -> bool {
        let mut flushing = self.flushing.lock();
        if flushing.get(peer_id).copied().unwrap_or(false) {
            flushing.insert(peer_id.to_string(), false);
            return true;
        }
        flushing.remove(peer_id);
        false
    }

    /// 把「在途」退回「排队」（**只在启动时**）：那次 HTTP 尝试已经随进程没了，谁也不该再等它。
    ///
    /// 退回的代价是可能重投一条其实已经送达的消息 —— 接收方按 id 幂等（`record_inbound`），
    /// 多送一次不会写成两条；不退回的代价是"这条永远停在发送中"，那才是真的丢了。
    ///
    /// 同一次启动扫尾也把**传输中的文件消息**落成终态（文本能补发，文件没有离线队列）：
    /// 见下面那段。
    pub fn requeue_inflight(&self, resumable: &std::collections::HashSet<String>) {
        let mut guard = self.inner.lock();
        let mut changed = false;
        for convo in guard.convos.values_mut() {
            for m in convo.messages.iter_mut() {
                if m.from_me && m.delivery == Delivery::Sending {
                    m.delivery = Delivery::Queued;
                    changed = true;
                }
            }
            // 文件消息同理：上次进程里还在传的那些随进程没了（推送不做离线排队，F-53），
            // 留成"还在传"就是让界面永远挂着一个不会动的进度 —— 落成终态（发送侧「未送达」、
            // 接收侧「接收失败」）。已经定下来的那些（已送达 / 已收完 / 已拒绝 / 未送达）不动。
            //
            // **例外：马上就要接着收的那几条**（`resumable` = 还在接手的那批邀请 id）：
            // 它们保住「接收中」，否则进度条会先闪一下"接收失败"再被重试接上 —— 那条路
            // 走的是后台重试，过程中不发任何界面事件（见 `transfer::resume_pending`）。
            for f in convo.files.iter_mut() {
                if f.sending {
                    f.sending = false;
                    f.pct = 0;
                    f.failed = f.targets;
                    for t in f.per_targets.iter_mut() {
                        t.state = "failed".to_string();
                    }
                    changed = true;
                }
                if f.invite_state == "pending"
                    || (f.invite_state == "accepted" && !resumable.contains(&f.invite_id))
                {
                    f.invite_state = "failed".to_string();
                    changed = true;
                }
            }
        }
        if changed {
            persist(&self.path, &guard);
        }
    }

    /// 这次群发用哪份名单：给了 id 就用它（顺便把成员更新成这次选的这几台），
    /// 没给就**按成员集合**找一份已有的（同一批人重复群发不该越攒越多），都没有就新建一份
    /// （名字留空 —— 用户在会话列表里右键改名，见 `rename_batch`）。返回名单 id。
    pub fn resolve_batch(&self, id: Option<&str>, members: &[String]) -> String {
        let mut ids: Vec<String> = members.iter().filter(|m| !m.is_empty()).cloned().collect();
        ids.dedup();
        let sig = batch_sig(&ids);
        let mut guard = self.inner.lock();
        if let Some(want) = id {
            if let Some(found) = guard.batches.iter_mut().find(|b| b.id == want) {
                if found.members != ids {
                    found.members = ids;
                    persist(&self.path, &guard);
                }
                return want.to_string();
            }
        }
        if let Some(found) = guard.batches.iter().find(|b| batch_sig(&b.members) == sig) {
            return found.id.clone();
        }
        let fresh = ChatBatch {
            id: uuid::Uuid::new_v4().to_string(),
            name: String::new(),
            members: ids,
        };
        let id = fresh.id.clone();
        guard.batches.push(fresh);
        persist(&self.path, &guard);
        id
    }

    /// 改群发的名字（`群发 · <名字>` 里 `·` 后面那截）。空串 = 界面显示「群发 · 未命名群发」。
    pub fn rename_batch(&self, id: &str, name: &str) -> Snapshot {
        {
            let mut guard = self.inner.lock();
            if let Some(found) = guard.batches.iter_mut().find(|b| b.id == id) {
                found.name = name.trim().to_string();
                persist(&self.path, &guard);
            }
        }
        self.snapshot()
    }

    /// 删掉一份群发名单。**消息一条不动** —— 它们本来就在各台设备的会话里（两边都在），
    /// 这里删的只是"这批人"这个视图。
    pub fn delete_batch(&self, id: &str) -> Snapshot {
        {
            let mut guard = self.inner.lock();
            let before = guard.batches.len();
            guard.batches.retain(|b| b.id != id);
            if guard.batches.len() != before {
                persist(&self.path, &guard);
            }
        }
        self.snapshot()
    }

    /// 清空与一台设备的聊天记录（一对一会话的「清空聊天记录」）。
    pub fn clear_peer(&self, peer_id: &str) -> Snapshot {
        let mut dropped: Vec<ChatMessage> = Vec::new();
        {
            let mut guard = self.inner.lock();
            if let Some(convo) = guard.convos.get_mut(peer_id) {
                dropped.extend(convo.messages.drain(..));
                // 文件消息也是这台设备的会话记录：清一半的话，列表里"最近一条"还会挂着
                // 一条已经不该在的记录（前端 `dropLocalFiles` 清的是同一件事的另一半）。
                convo.files.clear();
                convo.read_at = 0;
                persist(&self.path, &guard);
            }
        }
        // 记录没了，那几张图也没有留着的道理（不然它们会一直占着盘、也没人再引用）。
        drop_blobs(&dropped, &self.blobs_root);
        self.snapshot()
    }

    /// 把一台设备的**文件消息**整份换掉（前端那份列表的落盘，见 `ChatFileMsg`）。
    ///
    /// **整列表换**而不是逐条 upsert：前端本来就是"按设备存一份列表"（`patchFiles`），
    /// 它手上那份就是该设备此刻的全部记录 —— 逐条合并只会多出一套"谁该在列表里"的判定，
    /// 而清空、删某条这类"少了什么"的变化，upsert 根本说不出来。
    pub fn set_files(&self, peer_id: &str, files: Vec<ChatFileMsg>) -> Snapshot {
        {
            let mut guard = self.inner.lock();
            let convo = guard.convos.entry(peer_id.to_string()).or_default();
            convo.files = files;
            prune(convo, &self.blobs_root);
            persist(&self.path, &guard);
        }
        self.snapshot()
    }

    /// 一次性清掉一台设备的会话（解除配对等：关系没了，对话也没有存着的道理）。
    pub fn drop_peer(&self, peer_id: &str) -> Snapshot {
        let mut dropped: Vec<ChatMessage> = Vec::new();
        {
            let mut guard = self.inner.lock();
            if let Some(convo) = guard.convos.remove(peer_id) {
                dropped = convo.messages;
                persist(&self.path, &guard);
            }
        }
        drop_blobs(&dropped, &self.blobs_root);
        self.snapshot()
    }

    /// 手上还引用着的**图片消息 id**（启动时扫孤儿用，见 `blobs::sweep_in`）。
    pub fn image_ids(&self) -> std::collections::HashSet<String> {
        self.inner
            .lock()
            .convos
            .values()
            .flat_map(|c| c.messages.iter())
            .filter(|m| m.image)
            .map(|m| m.id.clone())
            .collect()
    }
}

impl Default for ChatStore {
    fn default() -> Self {
        Self::new()
    }
}

/// 成员集合的签名（排序去重后的 id 列表）：判断"是不是同一批人"只看它，不看顺序。
fn batch_sig(members: &[String]) -> Vec<String> {
    let mut ids: Vec<String> = members.iter().filter(|m| !m.is_empty()).cloned().collect();
    ids.sort();
    ids.dedup();
    ids
}

fn unread_of(convo: &PeerConvo) -> u32 {
    convo
        .messages
        .iter()
        .filter(|m| !m.from_me && m.at > convo.read_at)
        .count() as u32
}

fn prune(convo: &mut PeerConvo, root: &std::path::Path) {
    // 消息与文件消息各留 `MAX_PER_PEER` 条：砍的都是最够不着的那一头（列表/详情看的是最近的）。
    let excess = convo.messages.len().saturating_sub(MAX_PER_PEER);
    if excess > 0 {
        let dropped: Vec<ChatMessage> = convo.messages.drain(..excess).collect();
        // 砍掉的那几条如果是图片消息，内容文件也一起走（见 `blobs`）—— 不然它们会永远
        // 占着盘、还谁都引用不到。落盘那一下（`persist` 整份重写）本来就在同一把锁里，
        // 这里多删一个文件不算多。
        drop_blobs(&dropped, root);
    }
    let excess = convo.files.len().saturating_sub(MAX_PER_PEER);
    if excess > 0 {
        convo.files.drain(..excess);
    }
}

/// 这几条消息的**内容文件**一起删掉（裁剪 / 清空记录时用）。
///
/// 只在锁外面叫（文件 IO 不该占着账本）。文字消息本来就没有文件，`remove_in` 也认不出图片
/// 扩展名，所以这里简单地按"图片消息"过一遍就够。
fn drop_blobs(messages: &[ChatMessage], root: &std::path::Path) {
    for m in messages.iter().filter(|m| m.image) {
        crate::blobs::remove_in(root, &m.id, &m.name);
    }
}

/// 每次改动都落盘：消息很小、频率是人打字的速度，攒批只会让"刚刚说的话"多一层丢的风险。
fn persist(path: &PathBuf, file: &ChatFile) {
    match serde_json::to_vec(file) {
        Ok(bytes) => {
            if let Some(parent) = path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            // 落盘的是封装格式（`envelope`，**不是加密**）：双击打开看不到聊天明文。
            let wrapped = envelope::encode("", &bytes);
            if let Err(e) = write_atomic(path, &wrapped) {
                warn!(path = %path.display(), %e, "chat store persist failed");
            }
        }
        Err(e) => warn!(%e, "chat store serialize failed"),
    }
}

/// 现在（epoch 毫秒）。
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// 投放：长连接优先，HTTP 兜底
// ---------------------------------------------------------------------------

/// HTTP 那一跳的超时：一条消息不值得等太久（等不到就落「排队」，等对端上线再发）。
const HTTP_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(6);

/// 一条会话事件投出去的结果。**"够不着"与"被拒绝"是两回事**：够不着就排队等对端上线，
/// 被拒绝（4xx）再试一百次也一样 —— 只能重新配对。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Outcome {
    /// 已交给对端：写进长连接，或 HTTP 拿到 2xx。
    Delivered,
    /// 够不着：连不上 / 超时，或对端自己 5xx（隐身时它整个路由回 503）。→ 排队等它上线。
    Unreachable,
    /// 对端**明确拒绝**（4xx：不认凭据、认不出这个请求体）。→ 落「未送达」，不发第二次。
    Rejected,
}

impl Outcome {
    /// 这个结果落成一份副本的状态（群聊那侧也用它落定每一份）。
    pub(crate) fn delivery(self) -> Delivery {
        match self {
            Outcome::Delivered => Delivery::Sent,
            Outcome::Unreachable => Delivery::Queued,
            Outcome::Rejected => Delivery::Failed,
        }
    }
}

/// 一条会话事件的投放：**长连接优先，HTTP 兜底**（与推送邀请同一条路数）。
///
/// 为什么要有兜底：会话消息不该以"先配对 + 点连接"为前提 —— 推送早就是"未绑定也能推"，
/// 消息同理。长连接在就写它（快、即时），不在就 POST 到对端的 `/chat/message`（200 = 已送达）。
/// 两条都走不通也不是终点：够不着的排队等对端上线（`flush_queued`），被明确拒绝的才落「未送达」。
pub async fn deliver(
    state: &Arc<AppState>,
    peer_id: &str,
    event: &crate::ws::events::WsEvent,
) -> Outcome {
    if state.ws.has_connection(peer_id) && state.ws.send(peer_id, event.clone()) {
        return Outcome::Delivered;
    }
    deliver_http(state, peer_id, event).await
}

/// HTTP 那一跳：地址、token、钉过指纹的 TLS 客户端都与推送共用同一套。
async fn deliver_http(
    state: &Arc<AppState>,
    peer_id: &str,
    event: &crate::ws::events::WsEvent,
) -> Outcome {
    let Some(peer) = state.get_peer(peer_id) else {
        return Outcome::Unreachable;
    };
    // 判据是"此刻**拨得动**它吗"，不是"有没有它自己的地址"：只有一条借来的中转链、
    // 而直连地址从没学到过的那类对端（AP 隔离下被中转的那一侧）照样得送得到 ——
    // `dial()` 已经把手里那条路（直连地址，或中转端点）选好了。
    if !peer.dialable() {
        // 两条路都没有（没被发现过 / 手动加的还没填 / 中转也还没开出来）：够不着，等它出现。
        return Outcome::Unreachable;
    }
    let (host, port) = peer.dial();
    let path = match event {
        crate::ws::events::WsEvent::ChatText { .. } => "/chat/message",
        crate::ws::events::WsEvent::ChatImage { .. } => "/chat/image",
        crate::ws::events::WsEvent::ChatRead { .. } => "/chat/read",
        // 群聊三条走自己的路由（体形状与各自的线上事件逐字对应，见 `routes_chat` 的单测）。
        crate::ws::events::WsEvent::GroupText { .. } => "/chat/group",
        crate::ws::events::WsEvent::GroupRead { .. } => "/chat/group-read",
        crate::ws::events::WsEvent::GroupRoster { .. } => "/chat/group-roster",
        crate::ws::events::WsEvent::GroupSyncAsk { .. } => "/chat/group-sync",
        crate::ws::events::WsEvent::GroupSyncPost { .. } => "/chat/group-sync-post",
        crate::ws::events::WsEvent::GroupAck { .. } => "/chat/group-ack",
        crate::ws::events::WsEvent::GroupTextRejected { .. } => "/chat/group-reject",
        _ => return Outcome::Unreachable,
    };
    let (own_id, own_name) = {
        let c = state.config.lock();
        (c.device_id.clone(), c.device_name.clone())
    };
    let mut body = match serde_json::to_value(event) {
        Ok(v) => v,
        Err(_) => return Outcome::Rejected,
    };
    // 端口报**本机实际在听的那个**：`server_port` 是"服务器绑好了没有"的原子，服务器还没
    // 起来（或被重启的窗口里）它是 0 —— 对端照 0 记下来的地址永远拨不动（那一行于是永久
    // 停在离线）。`listening_port()` 就是这一处的收口（服务器没起时退回配置端口，与
    // `/pair/info`、中转控制面报的是同一个数）。
    body["port"] = serde_json::json!(state.listening_port());
    body["sender"] = serde_json::json!({ "id": own_id, "name": own_name });

    let url = format!("{}{}", crate::net::peer_base(host, port), path);
    // 带 token 的拨号必须按**这台目标**建信任表：全局表按 IP 聚合指纹，同一地址上
    // 另一台已配对设备的证书也能通过校验，随后 token 就发给了它（见 `net::DialPin`）。
    let mut req = crate::net::dial_client(host, &peer.dial_pin())
        .post(&url)
        .timeout(HTTP_TIMEOUT)
        .json(&body);
    // 已配对的对端要出示那枚 token（对端据此确认"是本机那个老搭档"）；未配对的没有 token。
    if peer.paired {
        if let Ok(Some(token)) = crate::credential::get_peer_token(peer_id) {
            req = req.bearer_auth(token);
        }
    }
    match req.send().await {
        Ok(r) if r.status().is_success() => Outcome::Delivered,
        Ok(r) if r.status().is_server_error() => {
            // 对端在网线上、但它这会儿收不下：隐身（`presence_middleware` 回 503）、或它自己
            // 出了毛病。这不是"关系不成立"，等它回来就该补上 —— 所以是「够不着」，不是「被拒」。
            debug!(%peer_id, status = r.status().as_u16(), path, "会话 HTTP 那一跳被对端推掉，排队等它");
            Outcome::Unreachable
        }
        Ok(r) => {
            // 4xx：不认这枚凭据 / 认不出这个请求体。重试不会变好，值得留一行 ——
            // 不然"消息没到"在日志里一点痕迹都没有。
            warn!(%peer_id, status = r.status().as_u16(), path, "会话 HTTP 那一跳被拒");
            Outcome::Rejected
        }
        Err(e) => {
            // 联系不上（对端没开机/不在网上）是常态，不该刷 warn。
            debug!(%peer_id, %e, path, "会话 HTTP 那一跳没打通");
            Outcome::Unreachable
        }
    }
}

/// 把一条副本从"还没定"落定成终态，并把整份状态推给界面。
fn settle(state: &Arc<AppState>, peer_id: &str, msg_id: &str, delivery: Delivery) {
    state.chat.set_delivery(peer_id, msg_id, delivery);
    crate::group::emit(state);
}

// ---------------------------------------------------------------------------
// 编排：命令与集成测试共用这两条路
// ---------------------------------------------------------------------------

/// 发一条文本消息给这些设备（`chat_send` 命令与集成测试共用这一条路）。
///
/// 一台设备存一份副本（**同一个消息 id**）。这一份的初态在发出这一刻就定：写进长连接的
/// = `Sent`；没有长连接的去走 HTTP 兜底那一跳 —— 到了 = `Sent`，**够不着 = `Queued`（排队
/// 等它上线，不丢）**，对端明确拒绝 = `Failed`（不发第二次）。
///
/// **欠着消息的设备一律跟着排队**（`has_backlog`）：顺序比"这条能不能马上走长连接"重要。
///
/// 发一条消息：每台各存一份（同一个 id），**多台时挂上一份群发名单**（`ChatBatch`）。
///
/// - `batch_id` = 界面从某个「群发 · 名字」里发的（成员可能刚被加减过 → 一并更新）；
/// - `batch_id` 为空但目标 ≥2 台 = 一次新的群发 → 按成员集合复用已有名单，没有就新建一份
///   （名字留空，等用户在列表里改名）；
/// - 目标只有一台 = 一对一，不打标（不产生"只有一台的群发"）。
///
/// 返回 `SendResult`：这条消息的 id 给界面**逐台读状态**用（每台的副本共用它），
/// 界面就不用去猜"哪条是刚发出去的"；`batch_id` 告诉它这次挂在哪份群发名单上。
///
/// `at` = 点名这条消息在时间线上的位置（发送方的墙上时间）。只有"输入框里文字与图片交错着
/// 一次发出去"那条路会点名给它 —— 那时是**界面**在给整条消息的每一段编号，后端各取各的
/// "现在"会让图片两边的两段文字挤到同一毫秒上去（顺序就不再是用户看到的那个顺序）。
/// 不点名（绝大多数调用）= 当场取现在，与老行为一字不差。
pub fn send_text(
    state: &Arc<AppState>,
    targets: &[String],
    text: &str,
    batch_id: Option<&str>,
    at: Option<u64>,
) -> SendResult {
    let id = uuid::Uuid::new_v4().to_string();
    let at = at.unwrap_or_else(now_ms);
    let event = crate::ws::events::WsEvent::ChatText {
        id: id.clone(),
        text: text.to_string(),
        at,
        // 本机此刻的名字：长连接那条路上没有信封，接收方只能从这里知道"这台叫什么"
        // （否则它只能拿设备表里那份占位名 —— 表现就是一串 deviceid）。
        from_name: state.config.lock().device_name.clone(),
    };
    let (deliveries, inflight) = dispatch(state, targets, &event);
    // 多台 = 群发（复用/新建一份名单）；单台且没点名某个群发 = 一对一发。
    let batch = match (batch_id, targets.len() > 1) {
        (Some(want), _) => Some(state.chat.resolve_batch(Some(want), targets)),
        (None, true) => Some(state.chat.resolve_batch(None, targets)),
        (None, false) => None,
    };
    let snapshot = state
        .chat
        .record_outbound(&id, text, at, &deliveries, batch.as_deref());
    follow_up_http(state, &id, event, inflight);
    SendResult {
        id,
        batch_id: batch,
        snapshot,
    }
}

/// 发一张**图片消息**：与文字同一个口径（同一台设备的会话、同一套排队补发、同一条 HTTP 兜底），
/// 只是内容换成那张图。
///
/// 两处与文字不一样：
/// - **发送方自己那份内容先落下来**（`blobs`）：补发、重启、群里给别人补拉都要用它。落不下
///   就别发 —— 发出去的会是一条对端看得到、本机却再也拿不出来的消息。
/// - 字节随事件**内联**（base64）走。上限见 `blobs::MAX_IMAGE_BYTES`：超了如实拒绝，
///   不改走推送（那会弹出"对方要不要接收"，正是这个需求要去掉的东西）。
pub fn send_image(
    state: &Arc<AppState>,
    targets: &[String],
    name: &str,
    bytes: &[u8],
    at: Option<u64>,
    batch_id: Option<&str>,
) -> Result<SendResult, String> {
    use base64::Engine;
    if bytes.len() as u64 > crate::blobs::MAX_IMAGE_BYTES {
        return Err(format!(
            "这张图 {}，超过了会话里能发的上限（{} MB）—— 用「＋」当文件发过去",
            human_size(bytes.len() as u64),
            crate::blobs::MAX_IMAGE_BYTES / (1024 * 1024)
        ));
    }
    let id = uuid::Uuid::new_v4().to_string();
    crate::blobs::save_in(state.chat.blobs_root(), &id, name, bytes)?;
    let at = at.unwrap_or_else(now_ms);
    let event = crate::ws::events::WsEvent::ChatImage {
        id: id.clone(),
        at,
        name: name.to_string(),
        size: bytes.len() as u64,
        data: base64::engine::general_purpose::STANDARD.encode(bytes),
        // 与 `ChatText` 同一条理由：名字只用来填对端的占位行。
        from_name: state.config.lock().device_name.clone(),
    };
    let (deliveries, inflight) = dispatch(state, targets, &event);
    let batch = match (batch_id, targets.len() > 1) {
        (Some(want), _) => Some(state.chat.resolve_batch(Some(want), targets)),
        (None, true) => Some(state.chat.resolve_batch(None, targets)),
        (None, false) => None,
    };
    let snapshot = state.chat.record_outbound(
        &id,
        blobs::Content::image(name, bytes.len() as u64),
        at,
        &deliveries,
        batch.as_deref(),
    );
    follow_up_http(state, &id, event, inflight);
    Ok(SendResult {
        id,
        batch_id: batch,
        snapshot,
    })
}

/// 一条消息往各台设备的**初态**：长连接在就当场写进去（即时、状态就是「已送达」）；不在的
/// 落「在途」，由后台那一跳走 HTTP 兜底（见 [`follow_up_http`]）；还欠着更早消息的那几台
/// 跟着排队（顺序见 `ChatStore::has_backlog`）。文字与图片共用这一份。
fn dispatch(
    state: &Arc<AppState>,
    targets: &[String],
    event: &crate::ws::events::WsEvent,
) -> (Vec<(String, Delivery)>, Vec<String>) {
    let mut deliveries: Vec<(String, Delivery)> = Vec::new();
    let mut inflight: Vec<String> = Vec::new();
    for peer_id in targets {
        if state.chat.has_backlog(peer_id) {
            deliveries.push((peer_id.clone(), Delivery::Queued));
            continue;
        }
        // 只有真有长连接的那几台才需要拷一份载荷 —— 图片消息的载荷是几 MB 的 base64，
        // 给一台没有会话的设备白拷一份不值当（`has_connection` 与 `send` 之间的竞争没关系：
        // 那一下 `send` 返回 false，这一台照样落「在途」走 HTTP 兜底）。
        if state.ws.has_connection(peer_id) && state.ws.send(peer_id, event.clone()) {
            deliveries.push((peer_id.clone(), Delivery::Sent));
        } else {
            deliveries.push((peer_id.clone(), Delivery::Sending));
            inflight.push(peer_id.clone());
        }
    }
    (deliveries, inflight)
}

/// 「在途」的那几台：后台走 HTTP 兜底那一跳，回来把状态落定（文字与图片共用）。
fn follow_up_http(
    state: &Arc<AppState>,
    id: &str,
    event: crate::ws::events::WsEvent,
    inflight: Vec<String>,
) {
    for peer_id in inflight {
        let state = state.clone();
        let msg_id = id.to_string();
        let event = event.clone();
        crate::task::spawn("chat http", async move {
            let mut outcome = deliver(&state, &peer_id, &event).await;
            // 够不着：**先当场试一次中转**（F-106）。探活每 5s 才会去开中转，等它来得及的话，
            // 用户眼前就是"消息卡在那儿"；这里当场试一次，成了就把这一条立刻重发过去。
            // `ensure_for` 自带限流、且"已经有路了就直接返回"，重复叫是安全的。
            if outcome == Outcome::Unreachable {
                if let Some(relay) = crate::relay::ensure_for(&state, &peer_id).await {
                    debug!(peer_id = %peer_id, via = %relay.via_name, "这条够不着，已改走中转重发");
                    outcome = deliver(&state, &peer_id, &event).await;
                }
            }
            settle(&state, &peer_id, &msg_id, outcome.delivery());
            if outcome == Outcome::Unreachable {
                // 这一条排上队了（直连不通、也没有可用的中转）。**告诉界面为什么** ——
                // 排队本身是既有语义（F-95），缺的只是那句解释。
                crate::relay::notify_undeliverable(&state, &peer_id);
                // 顺手叫一次补发：它有可能**就在这一跳的时间里**上线了（探活/链路比这个
                // 请求慢半拍），闸门在挡，叫不动就是它真的还没到。
                flush_queued(&state, &peer_id);
            }
        });
    }
}

/// 给错误文案用的大小（与 `paste.rs` 同一种写法，只是这里在 Rust 侧说这句话）。
pub(crate) fn human_size(bytes: u64) -> String {
    const MB: f64 = 1024.0 * 1024.0;
    const KB: f64 = 1024.0;
    let b = bytes as f64;
    if b >= MB {
        format!("{:.1} MB", b / MB)
    } else if b >= KB {
        format!("{:.0} KB", b / KB)
    } else {
        format!("{bytes} B")
    }
}

/// 收到一条**图片消息**：先把内容落下来，再记账。
///
/// 落不下来（base64 坏了 / 超过上限 / 写盘失败）就**不记账**并把原因交回去：HTTP 那一跳据此
/// 回非 2xx（发送方落「未送达」，还能重试），长连接那一跳只能记一行日志 —— 对端那条消息于是
/// 不会出现在本机会话里。这是"至少一次"投递的固有缺口：宁可漏一条，也不在本机留一条打不开
/// 的空图（那条气泡会一直显示"图读不出来"，比没有更让人困惑）。
///
/// 返回 `true` = 这一条确实是新的（重复投递不再响一遍通知）。
pub fn accept_inbound_image(
    state: &Arc<AppState>,
    peer_id: &str,
    from_name: &str,
    id: &str,
    name: &str,
    data: &str,
    at: u64,
) -> Result<bool, String> {
    // 解码 → 过上限 → 落盘只有一份实现（见 `blobs::save_base64`）；名字只用来取扩展名
    // （`blobs::path_in` 自己过名单），字节数按**解出来的**算（收方以自己那份为准）。
    let path = crate::blobs::save_base64(state.chat.blobs_root(), id, name, data)?;
    let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
    let (fresh, _) = state
        .chat
        .record_inbound_image(peer_id, from_name, id, name, size, at);
    Ok(fresh)
}

/// 一条**排队等补发**的消息 → 线上事件。
///
/// 文字那一档直接从消息本体拼；图片那一档得回 `blobs` 把内容读回来（消息里只存了名字与字节数）
/// —— 读不到就是"这条送不出去了"（`Err`，调用方落「未送达」，见 `flush_queued`）。
fn queued_event(
    msg: &ChatMessage,
    root: &std::path::Path,
    from_name: &str,
) -> Result<crate::ws::events::WsEvent, String> {
    use base64::Engine;
    if !msg.image {
        return Ok(crate::ws::events::WsEvent::ChatText {
            id: msg.id.clone(),
            text: msg.text.clone(),
            at: msg.at,
            from_name: from_name.to_string(),
        });
    }
    let bytes = crate::blobs::read_in(root, &msg.id, &msg.name)
        .ok_or_else(|| format!("图片内容不在了：{}", msg.name))?;
    Ok(crate::ws::events::WsEvent::ChatImage {
        id: msg.id.clone(),
        at: msg.at,
        name: msg.name.clone(),
        size: bytes.len() as u64,
        data: base64::engine::general_purpose::STANDARD.encode(bytes),
        from_name: from_name.to_string(),
    })
}

/// 对端上线了 / 链路接上了：把排队等它的消息**按原顺序**发出去。
///
/// 这是"离线时发的消息，对端上线就收到"这条承诺的落地点 —— `send_text` 里够不着的那几条
/// 就躺在这儿等它。触发点有好几处（对端被判在线、链路接上、探活的每一拍、启动时），
/// 所以它必须**幂等、随时可叫**：闸门（`begin_flush`）保证同一台设备同时只有一条补发在跑，
/// 而被闸门挡住的那次调用不丢 —— 在飞的那一轮跑完会再走一轮（手里那个判定可能已经过时）。
///
/// - **串行**一条一条发：同一台设备上后一条抢到前一条前面去，对端的会话就乱了。
/// - 够不着就停手：这一条留在「排队」里（后面的几条没必要再试，同一个对端同一个结果），
///   等下一次触发再来 —— 探活每 5s 会叫一次，够不着通常只是暂时的。
/// - 对端明确拒绝的那条落「未送达」，**继续往下走**：一条坏消息不该挡住后面几条。
///
/// 群消息排在**同一个循环**里（同一台设备只跑一条补发）：顺序只在各自会话内部有意义，
/// 而闸门只需要一个 —— 群与一对一不该同时往同一台设备上灌。
pub fn flush_queued(state: &Arc<AppState>, peer_id: &str) {
    // 不欠它东西就到此为止：探活每 5s 叫一次，没欠的时候连任务都不必起。
    if !state.chat.has_backlog(peer_id) && !state.groups.has_backlog_for(peer_id) {
        return;
    }
    if !state.chat.begin_flush(peer_id) {
        return;
    }
    let state = state.clone();
    let peer = peer_id.to_string();
    crate::task::spawn("chat flush", async move {
        loop {
            while let Some(msg) = state.chat.next_queued(&peer) {
                // 补发也要带上本机此刻的名字（与第一次发出去时同一条理由：接收方只能从这里
                // 知道"这台叫什么"，否则它只能拿设备表里那份占位名）。
                let own_name = state.config.lock().device_name.clone();
                let event = match queued_event(&msg, state.chat.blobs_root(), &own_name) {
                    Ok(event) => event,
                    Err(reason) => {
                        // 内容不在了（图片消息那几 MB 的字节没落下来 / 被人删了）：这一条
                        // 送不出去了，如实落「未送达」并继续往下走 —— 别把它卡在「排队」里
                        // 让后面的消息陪着一起等。
                        warn!(id = %msg.id, %reason, "排队的消息没法重投");
                        settle(&state, &peer, &msg.id, Delivery::Failed);
                        continue;
                    }
                };
                let mut outcome = deliver(&state, &peer, &event).await;
                if outcome == Outcome::Unreachable {
                    // 够不着：**先当场试一次借中转**（与 `send_text` 里那一跳同一套）。排队等它
                    // 上线的承诺要靠这一下 —— 只有一条借来的路、直连地址从没学到过的那类对端，
                    // "它上线"这件事本来就只能由那条路成立；少了这一下，记录推到了、
                    // 路也认得出来，可排队的消息要等用户再发一条才会动。
                    if crate::relay::ensure_for(&state, &peer).await.is_some() {
                        outcome = deliver(&state, &peer, &event).await;
                    }
                }
                if outcome == Outcome::Unreachable {
                    break; // 还留在「排队」：等下一次触发
                }
                settle(&state, &peer, &msg.id, outcome.delivery());
            }
            // 群：同一条闸门、同一个循环。够不着就停手（剩下的等下一次触发）。
            while let Some((group_id, msg)) = state.groups.next_queued_for(&peer) {
                let Some(group) = state.groups.group(&group_id) else {
                    break;
                };
                if let crate::group::FlushStep::Blocked =
                    crate::group::flush_one(&state, &peer, &group_id, &msg, &group).await
                {
                    break;
                }
            }
            // 这一轮在飞的时候又有人叫过（多半正是"对端刚上线"）：再走一轮 —— 手里那个
            // "够不着"的判定是几秒前的，现在该拿着新证据重试。
            if !state.chat.end_flush(&peer) {
                break;
            }
        }
    });
}

/// **启动时**：把上次进程里「在途」的副本退回「排队」（那次 HTTP 尝试已经随进程没了，
/// 谁也不该再等它）。退回的代价是可能重投一条其实已经送达的消息 —— 接收方按 id 幂等
/// （`record_inbound` / `group::record_inbound`），多送一次不会写成两条；不退回的代价是
/// "这条永远停在发送中"。
///
/// 只在启动时做：运行中那些「在途」是真的在飞，退回来就是无谓的重投。
pub fn requeue_inflight(state: &Arc<AppState>) {
    // 还在接手的接收任务（`transfer::resume_pending` 马上就会接着收）：那几条文件消息
    // 得留住「接收中」，别被扫成「接收失败」。
    let resumable = state.push.resume.pending_ids();
    state.chat.requeue_inflight(&resumable);
    state.groups.requeue_inflight();
}

/// 启动时清掉**没人引用**的图片内容（见 `blobs`）：账本里已经不在的消息、写了一半进程就
/// 没了的、以及旧版本留下的。
///
/// 引用表来自**所有**账本：1:1 与群发在 `chat.dat`（`ChatStore::image_ids`）、群聊在
/// `groups.dat`（`GroupStore::image_ids`，含还攒在邀请卡后面的那几条）—— 漏一份就会把
/// 另一份还在引用的图删掉。
///
/// 两个 store 正常共用 `chat-vault/blobs`；迁移失败回退时可能一个在 vault、一个在旧
/// `chat-blobs`，所以两边都扫一遍（同一个目录扫两次也无害）。
pub fn sweep_blobs(state: &AppState) {
    let mut keep = state.chat.image_ids();
    keep.extend(state.groups.image_ids());
    let mut roots = vec![state.chat.blobs_root()];
    if state.groups.blobs_root() != state.chat.blobs_root() {
        roots.push(state.groups.blobs_root());
    }
    let mut removed = 0;
    for root in roots {
        removed += crate::blobs::sweep_in(root, &keep);
    }
    if removed > 0 {
        tracing::info!(removed, "清掉没人引用的图片内容");
    }
}

/// 给每一台**还欠着消息**的设备叫一次补发（启动时、以及本机从「离线」切回「在线」时）。
pub fn flush_all_queued(state: &Arc<AppState>) {
    let mut peers = state.chat.peers_with_backlog();
    for peer in state.groups.peers_with_backlog() {
        if !peers.contains(&peer) {
            peers.push(peer);
        }
    }
    for peer_id in peers {
        flush_queued(state, &peer_id);
    }
}

/// 会话命令里"发一条消息"的返回：整份快照 + 这条消息的 id + 它挂在哪份群发名单上。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SendResult {
    pub id: String,
    /// `None` = 一对一发的（或只剩一台的群发框）；否则是那份群发名单的 id。
    pub batch_id: Option<String>,
    pub snapshot: Snapshot,
}

/// 界面读过这些设备的会话（`chat_mark_read` 命令与集成测试共用这一条路）。
///
/// 水位取的是**对端发来的最大 `at`**（不是"现在几点"）：回执带的是发送方的时钟，对端拿它
/// 跟自己的消息比即可。没读到新东西就不发（别每次聚焦都往对端丢一条空回执）。
///
/// 回执本身也走"长连接优先 + HTTP 兜底"（对端没长连接时，"已读"照样得能传回去）；
/// 丢了不重试：链路上来时会按本地水位补一次（`flush_read_receipt`）。
pub fn mark_read(state: &Arc<AppState>, peer_ids: &[String]) {
    for peer_id in peer_ids {
        let watermark = state.chat.inbound_watermark(peer_id);
        if watermark == 0 || watermark <= state.chat.read_watermark(peer_id) {
            continue;
        }
        state.chat.mark_read_local(peer_id, watermark);
        send_receipt(state, peer_id, watermark);
    }
}

/// 把一枚已读回执送出去（后台那一跳：长连接优先、HTTP 兜底）。
pub fn send_receipt(state: &Arc<AppState>, peer_id: &str, up_to: u64) {
    let state = state.clone();
    let peer_id = peer_id.to_string();
    crate::task::spawn("chat read receipt", async move {
        deliver(
            &state,
            &peer_id,
            &crate::ws::events::WsEvent::ChatRead { up_to },
        )
        .await;
    });
}

/// 链路刚接上：把「我读到哪了」补给对端。
///
/// 已读回执只在界面读到的那一刻发一次 —— 那时链路断了/对端刚重启，这一下就丢了，而回执
/// 丢了就是把"已读"**永久**丢掉（对端那条消息会一直挂着「已送达」，谁也没机会再纠正）。
/// 链路一接上补一次是幂等的：`up_to` 就是本地已读水位，对端拿它跟自己的消息比即可。
///
/// 没有已读水位（`0`）就什么都不发：**收到 ≠ 读过**，别顺手替用户把没看过的消息标成已读。
pub fn flush_read_receipt(state: &Arc<AppState>, peer_id: &str) {
    let up_to = state.chat.read_watermark(peer_id);
    if up_to == 0 {
        return;
    }
    send_receipt(state, peer_id, up_to);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store(name: &str) -> ChatStore {
        // 每个用例自己**一个目录**（不只是自己一份文件）：图片消息的内容目录是从账本
        // 的位置推出来的（见 `with_path`），共用一个临时目录的话，并行跑的几条会互相删对方的图。
        let dir =
            std::env::temp_dir().join(format!("orbdrop_chat_{name}_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        ChatStore::with_path(dir.join("chat.dat"))
    }

    fn peers(ids: &[&str]) -> Vec<(String, Delivery)> {
        ids.iter()
            .map(|id| (id.to_string(), Delivery::Sent))
            .collect()
    }

    /// 图片消息那一套：内容落在**自己的**目录里（`store()` 给的临时目录旁边），
    /// 快照里带的是**绝对路径**，重启（同一份文件重新打开）之后还在。
    #[test]
    fn an_image_message_keeps_its_content_and_its_path() {
        let s = store("image_round_trip");
        let root = s.blobs_root().to_path_buf();
        let written = crate::blobs::save_in(&root, "img-1", "图片-1.png", b"PNGDATA").unwrap();
        let snap = s.record_outbound(
            "img-1",
            blobs::Content::image("图片-1.png", 7),
            100,
            &peers(&["a"]),
            None,
        );
        let a = snap.convos.iter().find(|c| c.peer_id == "a").unwrap();
        assert!(a.messages[0].image, "这条是图片消息");
        assert_eq!(a.messages[0].name, "图片-1.png");
        assert_eq!(a.messages[0].size, 7);
        assert_eq!(
            a.messages[0].path,
            written.to_string_lossy(),
            "快照里给的是本机那份内容文件"
        );
        assert!(a.messages[0].text.is_empty(), "图片消息的 text 是空串");

        // 重启：同一份 chat.dat 重新打开 → 路径照旧算得出来。
        let reopened = ChatStore::with_path(s.path.clone());
        let a = reopened
            .snapshot()
            .convos
            .into_iter()
            .find(|c| c.peer_id == "a")
            .unwrap();
        assert_eq!(a.messages[0].path, written.to_string_lossy());
    }

    /// 收到的图片消息**按 id 幂等**：重投不写第二条，也不重写内容。
    #[test]
    fn an_inbound_image_is_recorded_once() {
        let s = store("image_inbound");
        let (fresh, snap) = s.record_inbound_image("a", "PC-A", "img-1", "图片-1.png", 3, 100);
        assert!(fresh);
        assert!(snap.convos[0].messages[0].image);
        let (fresh_again, snap) =
            s.record_inbound_image("a", "PC-A", "img-1", "图片-1.png", 3, 100);
        assert!(!fresh_again, "重投不算新的");
        assert_eq!(snap.convos[0].messages.len(), 1, "不能变成两条");
    }

    /// 裁剪：砍掉的老消息如果是图片，它的**内容文件**也跟着走（不然会永远占着盘）。
    #[test]
    fn pruning_drops_the_content_of_the_oldest_images() {
        let s = store("image_prune");
        let root = s.blobs_root().to_path_buf();
        // 第 1 条会被裁掉（上限 500），最后一条留着。
        let first = crate::blobs::save_in(&root, "img-0", "图片.png", b"OLD").unwrap();
        s.record_outbound(
            "img-0",
            blobs::Content::image("图片.png", 3),
            1,
            &peers(&["a"]),
            None,
        );
        for i in 1..MAX_PER_PEER {
            let id = format!("img-{i}");
            crate::blobs::save_in(&root, &id, "图片.png", b"NEW").unwrap();
            s.record_inbound_image("a", "PC-A", &id, "图片.png", 3, 1 + i as u64);
        }
        let last = crate::blobs::save_in(&root, "img-last", "图片.png", b"NEW").unwrap();
        s.record_inbound_image("a", "PC-A", "img-last", "图片.png", 3, 9999);

        let snap = s.snapshot();
        let a = snap.convos.iter().find(|c| c.peer_id == "a").unwrap();
        assert_eq!(a.messages.len(), MAX_PER_PEER, "上限就是上限");
        assert!(
            !a.messages.iter().any(|m| m.id == "img-0"),
            "最老的那条被砍了"
        );
        assert!(!first.exists(), "被砍那条的内容也要删掉");
        assert!(last.exists(), "还引用着的内容一个不动");
        assert!(a.messages.iter().any(|m| m.path == last.to_string_lossy()));
    }

    /// 清空一台设备的聊天记录 = 连它的图片内容一起清（记录没了，那些图也没有留着的道理）。
    #[test]
    fn clearing_a_peer_drops_its_image_content() {
        let s = store("image_clear");
        let root = s.blobs_root().to_path_buf();
        let written = crate::blobs::save_in(&root, "img-1", "图片.png", b"DATA").unwrap();
        s.record_outbound(
            "img-1",
            blobs::Content::image("图片.png", 4),
            100,
            &peers(&["a"]),
            None,
        );
        assert!(written.exists());
        s.clear_peer("a");
        assert!(!written.exists(), "清空记录要连内容一起清");
        assert!(s.image_ids().is_empty(), "引用表也空了（启动扫孤儿据此判）");
    }

    /// 补发那一跳：图片消息的事件是**从内容文件重建**的；内容不在了就如实报错
    /// （调用方落「未送达」，见 `flush_queued`），不让它卡在「排队」里。
    #[test]
    fn a_queued_image_is_rebuilt_from_disk_or_fails_loudly() {
        let s = store("image_queued");
        let root = s.blobs_root().to_path_buf();
        crate::blobs::save_in(&root, "img-1", "图片.png", b"DATA").unwrap();
        s.record_outbound(
            "img-1",
            blobs::Content::image("图片.png", 4),
            100,
            &[(String::from("a"), Delivery::Queued)],
            None,
        );
        let msg = s.next_queued("a").expect("排队了就该取得到");
        match queued_event(&msg, &root, "PC-ME") {
            Ok(crate::ws::events::WsEvent::ChatImage {
                name,
                data,
                size,
                from_name,
                ..
            }) => {
                assert_eq!(name, "图片.png");
                assert_eq!(size, 4);
                assert_eq!(data, "REFUQQ==", "base64(DATA)");
                // 补发这一跳也要带着本机名字（接收方靠它填占位行）。
                assert_eq!(from_name, "PC-ME");
            }
            other => panic!("应当是图片事件：{other:?}"),
        }
        crate::blobs::remove_in(&root, "img-1", "图片.png");
        assert!(
            queued_event(&msg, &root, "PC-ME").is_err(),
            "内容不在了要报错，不能装作发得出去"
        );
    }

    /// 落定的规矩：**只从"还没定"的那两档（在途 / 排队）改到终态** —— 已读不回退成
    /// 已送达，"未送达"也不被洗白。
    #[test]
    fn settle_only_ever_moves_an_unsettled_copy() {
        let s = store("settle");
        // m1 比回执水位晚（这条回执覆盖不到它），专门用来看"在途 → 已送达"那一跳。
        s.record_outbound(
            "m1",
            "没有长连接也要到",
            300,
            &[(String::from("a"), Delivery::Sending)],
            None,
        );
        s.record_outbound(
            "m2",
            "已经读过的",
            100,
            &[(String::from("a"), Delivery::Sent)],
            None,
        );
        s.record_outbound(
            "m3",
            "没送出去的",
            200,
            &[(String::from("a"), Delivery::Failed)],
            None,
        );
        s.record_outbound(
            "m4",
            "排队等它上线的",
            400,
            &[(String::from("a"), Delivery::Queued)],
            None,
        );
        s.mark_read_by_peer("a", 100); // 只覆盖 m2 → 已读

        let snap = s.set_delivery("a", "m1", Delivery::Sent);
        let a = snap.convos.iter().find(|c| c.peer_id == "a").unwrap();
        assert_eq!(a.messages[0].delivery, Delivery::Sent, "在途 → 已送达");
        // 补发那一跳回来：排队的副本也落定（它同样"还没定"）。
        let snap = s.set_delivery("a", "m4", Delivery::Sent);
        let a = snap.convos.iter().find(|c| c.peer_id == "a").unwrap();
        assert_eq!(a.messages[3].delivery, Delivery::Sent, "排队 → 已送达");
        // 迟到的 HTTP 成功不准把「已读」改回「已送达」，也不准给「未送达」洗白。
        let _ = s.set_delivery("a", "m2", Delivery::Sent);
        let _ = s.set_delivery("a", "m3", Delivery::Sent);
        let a = snap.convos.iter().find(|c| c.peer_id == "a").unwrap();
        assert_eq!(a.messages[1].delivery, Delivery::Read, "已读不回退");
        assert_eq!(a.messages[2].delivery, Delivery::Failed, "未送达不被洗白");
    }

    /// 排队等对端上线的那些：认得出来（`has_backlog`）、补发从**最早那条**开始、
    /// 落定之后就不再是积压。
    ///
    /// 「在途」那条**不算积压**：它有自己的那一跳在跑，够不着时会被那一跳退回落「排队」。
    #[test]
    fn queued_copies_are_retried_in_send_order() {
        let s = store("queue");
        s.record_outbound(
            "m1",
            "第一句",
            100,
            &[(String::from("a"), Delivery::Queued)],
            None,
        );
        s.record_outbound(
            "m2",
            "第二句",
            200,
            &[(String::from("a"), Delivery::Sending)],
            None,
        );
        assert!(s.has_backlog("a"), "欠着 a 一条");
        assert!(!s.has_backlog("b"), "没跟 b 说过话 = 不欠它");
        assert_eq!(s.peers_with_backlog(), vec!["a".to_string()]);
        assert_eq!(
            s.next_queued("a").expect("有排队的").id,
            "m1",
            "补发从最早那条开始"
        );
        // 补发**不**把它挪出「排队」：这几秒里新消息才会跟着排（顺序不许乱）。
        assert!(s.has_backlog("a"));
        s.set_delivery("a", "m1", Delivery::Sent);
        assert!(
            !s.has_backlog("a"),
            "发出去就不欠了（m2 是另一条路在途的，不算积压）"
        );
        assert!(s.next_queued("a").is_none());
    }

    /// 启动时把「在途」退回「排队」（那次 HTTP 尝试随进程没了）；`Failed` 不动（不发第二次）。
    #[test]
    fn a_restart_requeues_inflight_copies_only() {
        let s = store("requeue");
        s.record_outbound(
            "m1",
            "在途",
            100,
            &[(String::from("a"), Delivery::Sending)],
            None,
        );
        s.record_outbound(
            "m2",
            "被拒的",
            200,
            &[(String::from("a"), Delivery::Failed)],
            None,
        );
        s.record_outbound(
            "m3",
            "已送达",
            300,
            &[(String::from("a"), Delivery::Sent)],
            None,
        );
        s.requeue_inflight(&Default::default());

        let convo = s
            .snapshot()
            .convos
            .into_iter()
            .find(|c| c.peer_id == "a")
            .unwrap();
        assert_eq!(
            convo.messages[0].delivery,
            Delivery::Queued,
            "在途 → 排队（重投一次）"
        );
        assert_eq!(
            convo.messages[1].delivery,
            Delivery::Failed,
            "明确拒绝的不重来"
        );
        assert_eq!(convo.messages[2].delivery, Delivery::Sent, "到了的不动");
        assert_eq!(s.peers_with_backlog(), vec!["a".to_string()]);
    }

    /// 一条**还在传**的发送侧文件消息。
    fn sending_file(id: &str, at: u64) -> ChatFileMsg {
        ChatFileMsg {
            id: id.to_string(),
            at,
            from_me: true,
            name: "报告.pdf".to_string(),
            is_dir: false,
            peer_id: String::new(),
            size: "8.2 MB".to_string(),
            targets: 2,
            failed: 0,
            pct: 62,
            sending: true,
            batch_id: "pb1".to_string(),
            chat_batch_id: String::new(),
            group_id: String::new(),
            per_targets: vec![
                ChatFileTarget {
                    peer_id: "a".to_string(),
                    invite_id: "i-a".to_string(),
                    state: "done".to_string(),
                },
                ChatFileTarget {
                    peer_id: "b".to_string(),
                    invite_id: "i-b".to_string(),
                    state: "sending".to_string(),
                },
            ],
            invite_id: String::new(),
            invite_state: String::new(),
            received: 0,
            bytes: 0,
            path: "C:/tmp/报告.pdf".to_string(),
            count: 0,
        }
    }

    /// 文件消息是**用户数据**：落盘之后重开进程还在 —— 只发过文件、没说过话的那条会话
    /// 才不会整条从会话列表里消失（列表按"有没有内容"画）。
    #[test]
    fn file_messages_survive_a_restart() {
        let path =
            std::env::temp_dir().join(format!("orbdrop_chat_files_{}", uuid::Uuid::new_v4()));
        let s = ChatStore::with_path(path.clone());
        s.set_files("a", vec![sending_file("f1", 100)]);
        assert_eq!(s.snapshot().convos[0].files.len(), 1);

        // 重开一份（同一个文件）：内容逐字段读得回来。
        let reopened = ChatStore::with_path(path.clone());
        let convo = reopened
            .snapshot()
            .convos
            .into_iter()
            .find(|c| c.peer_id == "a")
            .unwrap();
        assert_eq!(convo.files.len(), 1);
        assert_eq!(convo.files[0].name, "报告.pdf");
        assert_eq!(convo.files[0].path, "C:/tmp/报告.pdf");
        assert_eq!(convo.files[0].per_targets[1].state, "sending");
        // 界面收到的那份快照里带着它（会话列表"有没有内容"、详情里那条气泡都读这里）。
        let json = serde_json::to_value(reopened.snapshot()).expect("快照要能序列化");
        assert_eq!(json["convos"][0]["files"][0]["name"], "报告.pdf");
        assert_eq!(
            json["convos"][0]["files"][0]["fromMe"], true,
            "键名是 camelCase"
        );
        let _ = std::fs::remove_file(path);
    }

    /// 断在传输中间的文件消息：重启扫尾落成终态（推送不做离线排队）—— 不落的话界面会永远
    /// 挂着一个不会动的进度。**已经定下来的那些一个不动**。
    #[test]
    fn a_restart_settles_interrupted_file_messages_only() {
        let s = store("file_requeue");
        let mut done = sending_file("f0", 50);
        done.sending = false;
        done.pct = 100;
        done.failed = 0;
        done.batch_id = String::new();
        let mut inbound = sending_file("f2", 200);
        inbound.from_me = false;
        inbound.sending = false;
        inbound.invite_state = "accepted".to_string();

        s.set_files("a", vec![done, sending_file("f1", 100), inbound]);
        s.requeue_inflight(&Default::default());

        let convo = s
            .snapshot()
            .convos
            .into_iter()
            .find(|c| c.peer_id == "a")
            .unwrap();
        assert!(
            !convo.files[0].sending && convo.files[0].pct == 100,
            "已送达的不动"
        );
        let stopped = &convo.files[1];
        assert!(!stopped.sending, "还在传的那条落成终态");
        assert_eq!(stopped.failed, stopped.targets, "发送侧落成「未送达」");
        assert_eq!(stopped.pct, 0);
        assert!(stopped.per_targets.iter().all(|t| t.state == "failed"));
        assert_eq!(
            convo.files[2].invite_state, "failed",
            "接收侧落成「接收失败」"
        );
    }

    /// 「清空聊天记录」清的是这台设备的**全部**记录：文件消息跟着走（只清一半的话，
    /// 列表里"最近一条"还会挂着一条已经不该在的文件）。
    #[test]
    fn clearing_a_peer_drops_its_file_messages_too() {
        let s = store("file_clear");
        s.set_files("a", vec![sending_file("f1", 100)]);
        s.record_inbound("a", "PC-A", "m1", "在吗", 100);

        let snap = s.clear_peer("a");
        let convo = snap.convos.into_iter().find(|c| c.peer_id == "a").unwrap();
        assert!(convo.messages.is_empty() && convo.files.is_empty());
    }

    /// 启动扫尾与**自动续传**的分工：还在接手的那些邀请（`resumable`）保住「接收中」，
    /// 不然进度条会先闪一下"接收失败"、再被后台重试悄悄接上（那条路不发界面事件）。
    #[test]
    fn a_resumable_receive_keeps_its_receiving_state() {
        let s = store("file_resumable");
        let mut keep = sending_file("f-keep", 100);
        keep.from_me = false;
        keep.sending = false;
        keep.invite_id = "inv-keep".into();
        keep.invite_state = "accepted".into();
        let mut gone = sending_file("f-gone", 200);
        gone.from_me = false;
        gone.sending = false;
        gone.invite_id = "inv-gone".into();
        gone.invite_state = "accepted".into();
        s.set_files("a", vec![keep, gone]);

        let resumable: std::collections::HashSet<String> =
            std::iter::once("inv-keep".to_string()).collect();
        s.requeue_inflight(&resumable);

        let convo = s
            .snapshot()
            .convos
            .into_iter()
            .find(|c| c.peer_id == "a")
            .unwrap();
        assert_eq!(
            convo.files[0].invite_state, "accepted",
            "马上要接着收的保住「接收中」"
        );
        assert_eq!(convo.files[1].invite_state, "failed", "没人接手的落终态");
    }

    /// `chat_files_set` 的入参就是前端那份 `ChatFileMsg`：键名对不上时 serde 会把字段落成
    /// 默认值 —— 记录会静默变空（"重启后文件记录没了"的另一种形态），所以这里按前端的字面
    /// 形状逐个解析一遍。
    #[test]
    fn the_frontend_file_payload_parses_field_for_field() {
        let payload = serde_json::json!([{
            "id": "file:pb1:0",
            "at": 1_791_864_416_780u64,
            "fromMe": true,
            "name": "报告.pdf",
            "isDir": false,
            "peerId": "",
            "size": "8.2 MB",
            "targets": 2,
            "failed": 1,
            "pct": 100,
            "sending": false,
            "batchId": "",
            "chatBatchId": "bc1",
            "groupId": "",
            "perTargets": [
                { "peerId": "a", "inviteId": "i-a", "state": "done" },
                { "peerId": "b", "inviteId": "i-b", "state": "failed" }
            ],
            "inviteId": "",
            "inviteState": "",
            "received": 0,
            "bytes": 0,
            "path": "C:/tmp/报告.pdf",
            "count": 3
        }]);
        let files: Vec<ChatFileMsg> =
            serde_json::from_value(payload).expect("前端那份形状要能解析");
        let f = &files[0];
        assert_eq!(f.id, "file:pb1:0");
        assert_eq!(f.at, 1_791_864_416_780);
        assert!(f.from_me && !f.is_dir);
        assert_eq!(f.name, "报告.pdf");
        assert_eq!(f.size, "8.2 MB");
        assert_eq!(f.targets, 2);
        assert_eq!(f.failed, 1);
        assert_eq!(f.pct, 100);
        assert!(!f.sending);
        assert_eq!(f.chat_batch_id, "bc1");
        assert_eq!(f.path, "C:/tmp/报告.pdf");
        assert_eq!(f.count, 3);
        assert_eq!(f.per_targets.len(), 2);
        assert_eq!(f.per_targets[0].peer_id, "a");
        assert_eq!(f.per_targets[0].invite_id, "i-a");
        assert_eq!(f.per_targets[1].state, "failed");
    }

    /// 补发的闸门：同一台设备同时只跑一条（探活每 5s 会再叫一次），被挡下的那次**不丢** ——
    /// 记成"再跑一轮"，在飞的那轮跑完接着走。
    #[test]
    fn only_one_flush_runs_per_peer_at_a_time() {
        let s = store("gate");
        assert!(s.begin_flush("a"));
        assert!(
            !s.begin_flush("a"),
            "已经在跑的那条不会被叠第二条（记下再跑一轮）"
        );
        assert!(s.begin_flush("b"), "别的设备各有一条");
        assert!(s.end_flush("a"), "期间有人又叫过 → 再跑一轮");
        assert!(!s.end_flush("a"), "这一轮没人叫 → 收工");
        assert!(s.begin_flush("a"), "收工后闸门放开");
    }

    /// 同一个 id 进来第二次不再写一条（补发这条路是"至少一次"）：对方那句"在吗"不会变两句。
    #[test]
    fn a_redelivered_inbound_message_is_recorded_once() {
        let s = store("dedupe");
        let (first, _) = s.record_inbound("a", "PC-A", "m1", "在吗", 100);
        s.record_inbound("a", "PC-A", "m2", "在不", 200);
        let (again, snap) = s.record_inbound("a", "PC-A", "m1", "在吗", 100);

        assert!(first, "头一次投递是新的一条");
        assert!(!again, "重复投递不是新的（手机那边靠它决定通不通知）");

        let convo = snap.convos.iter().find(|c| c.peer_id == "a").unwrap();
        assert_eq!(convo.messages.len(), 2, "重复投递不再写一条");
        assert_eq!(convo.unread, 2, "未读也不会算重");
        assert_eq!(s.inbound_watermark("a"), 200, "水位照旧是对方那条最大的 at");
    }

    /// 群发就是"每台各一条"：A 发给 B、C 之后，世界里只多出"与 B""与 C"两条会话，
    /// 各自挂着同一条消息（同一个 id）——**没有第三个"群"那样的东西**。
    #[test]
    fn outbound_fans_out_one_copy_per_target() {
        let s = store("fanout");
        let snap = s.record_outbound("m1", "在吗", 100, &peers(&["a", "b"]), None);

        assert_eq!(snap.convos.len(), 2);
        for view in &snap.convos {
            assert_eq!(view.messages.len(), 1);
            let m = &view.messages[0];
            // 同一个 id：逐台状态（"送到了几台"）靠它把几份副本认成同一条。
            assert_eq!(m.id, "m1");
            assert!(m.from_me);
            assert_eq!(m.targets, vec!["a".to_string(), "b".to_string()]);
        }
        assert_eq!(snap.unread, 0);
    }

    /// 群发名单：同一批人重复群发**复用同一份**（不越攒越多），成员变了才另起一份；
    /// 副本上打标（群发那条视图据此筛），一对一发的则不打标。
    #[test]
    fn a_batch_is_reused_for_the_same_members_and_tags_its_messages() {
        let s = store("batch_reuse");
        let b = s.resolve_batch(None, &["a".to_string(), "b".to_string()]);
        // 同一批人换个顺序 → 还是那一份。
        assert_eq!(
            s.resolve_batch(None, &["b".to_string(), "a".to_string()]),
            b
        );
        // 多一台 → 另起一份（老的还在）。
        let c = s.resolve_batch(None, &["a".to_string(), "b".to_string(), "c".to_string()]);
        assert_ne!(c, b);
        assert_eq!(s.snapshot().batches.len(), 2);

        // 点名某个群发发的：副本带上它的 id，成员按这次选的更新。
        s.record_outbound("m1", "两位", 100, &peers(&["a", "b"]), Some(&b));
        let view = s.snapshot();
        for c2 in &view.convos {
            assert_eq!(c2.messages[0].batch_id.as_deref(), Some(b.as_str()));
        }
        // 一对一发的：没有标记。
        s.record_outbound("m2", "只给 a 的", 200, &peers(&["a"]), None);
        let a = view.convos.iter().find(|v| v.peer_id == "a").unwrap();
        let _ = a;
        let view = s.snapshot();
        let a = view.convos.iter().find(|v| v.peer_id == "a").unwrap();
        assert_eq!(a.messages.last().unwrap().batch_id, None);
    }

    /// 改名改的是名字；**删掉一份群发，消息一条都不少**（它们本来就在各台会话里）。
    #[test]
    fn renaming_and_deleting_a_batch_never_touches_messages() {
        let s = store("batch_rename");
        let b = s.resolve_batch(None, &["a".to_string(), "b".to_string()]);
        s.record_outbound("m1", "两位", 100, &peers(&["a", "b"]), Some(&b));

        let renamed = s.rename_batch(&b, "  客户组  ");
        assert_eq!(renamed.batches[0].name, "客户组", "改名顺手去掉两端空白");

        let deleted = s.delete_batch(&b);
        assert!(deleted.batches.is_empty(), "名单没了");
        for peer in ["a", "b"] {
            let convo = deleted.convos.iter().find(|c| c.peer_id == peer).unwrap();
            assert_eq!(convo.messages.len(), 1, "消息还在（{peer}）");
            // 标记也还在：将来重新建一份同成员的群发时，历史仍然认得出来。
            assert_eq!(convo.messages[0].batch_id.as_deref(), Some(b.as_str()));
        }
    }

    #[test]
    fn inbound_counts_unread_until_read() {
        let s = store("unread");
        s.record_inbound("a", "PC-A", "m1", "你好", 100);
        s.record_inbound("a", "PC-A", "m2", "在吗", 200);
        let snap = s.snapshot();
        assert_eq!(snap.unread, 2);
        assert_eq!(snap.convos[0].unread, 2);
        assert_eq!(s.inbound_watermark("a"), 200);

        s.mark_read_local("a", 100);
        assert_eq!(s.snapshot().unread, 1);
        s.mark_read_local("a", 200);
        assert_eq!(s.snapshot().unread, 0);
        // 水位只增不减：旧回执（或乱序到达的）不能把已读又变回未读。
        s.mark_read_local("a", 50);
        assert_eq!(s.read_watermark("a"), 200);
    }

    #[test]
    fn peer_read_receipt_marks_only_sent_copies() {
        let s = store("receipt");
        s.record_outbound(
            "m1",
            "早",
            100,
            &[(String::from("a"), Delivery::Sent)],
            None,
        );
        s.record_outbound(
            "m2",
            "没送出去的",
            200,
            &[(String::from("a"), Delivery::Failed)],
            None,
        );
        // 排队等对端上线的那条：**对端根本没收到过**，回执更不可能覆盖它。
        s.record_outbound(
            "m3",
            "还排着队的",
            300,
            &[(String::from("a"), Delivery::Queued)],
            None,
        );
        s.mark_read_by_peer("a", 300);

        let convo = s
            .snapshot()
            .convos
            .into_iter()
            .find(|c| c.peer_id == "a")
            .unwrap();
        assert_eq!(convo.messages[0].delivery, Delivery::Read);
        // Failed 不会被回执洗白。
        assert_eq!(convo.messages[1].delivery, Delivery::Failed);
        assert_eq!(
            convo.messages[2].delivery,
            Delivery::Queued,
            "排队的也不算读过"
        );
    }

    /// 老版本（群里那套）写下的 `chat.json` 里多出来一个 `groups` 字段：读进来看不懂就忽略，
    /// 别因为一个多余的键让整份历史读不出来。
    #[test]
    fn a_legacy_file_with_a_groups_key_still_loads() {
        let path =
            std::env::temp_dir().join(format!("orbdrop_chat_legacy_{}", uuid::Uuid::new_v4()));
        std::fs::write(
            &path,
            r#"{"convos":{"a":{"messages":[{"id":"m1","fromMe":false,"fromName":"PC-A","text":"你好","at":100,"delivery":"received","targets":["a"]}],"readAt":0}},"groups":[["a","b"]]}"#,
        )
        .unwrap();
        let s = ChatStore::with_path(path.clone());
        let snap = s.snapshot();
        assert_eq!(snap.convos.len(), 1);
        assert_eq!(snap.convos[0].messages[0].text, "你好");
        // 老文件里没有 `batches` 与消息上的 `batchId`：按"没有群发、这条不是群发发的"读。
        assert!(snap.batches.is_empty());
        assert_eq!(snap.convos[0].messages[0].batch_id, None);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn clear_peer_drops_that_history_only() {
        let s = store("clear");
        s.record_inbound("a", "PC-A", "m1", "你好", 100);
        s.record_inbound("b", "PC-B", "m2", "在吗", 200);
        s.mark_read_local("a", 100);
        let snap = s.clear_peer("a");
        let a = snap.convos.iter().find(|c| c.peer_id == "a").unwrap();
        assert!(a.messages.is_empty());
        assert_eq!(a.unread, 0);
        // 别的设备那份原样留着（群发过去的那条在别人会话里也是独立的一份）。
        let b = snap.convos.iter().find(|c| c.peer_id == "b").unwrap();
        assert_eq!(b.messages.len(), 1);
    }

    /// 落盘不是明文：双击 `chat.dat` 看不到聊天内容；解封装后仍是完整 JSON。
    #[test]
    fn persisted_chat_is_enveloped_and_still_reloads() {
        let path = std::env::temp_dir().join(format!("orbdrop_chat_env_{}", uuid::Uuid::new_v4()));
        {
            let s = ChatStore::with_path(path.clone());
            s.record_inbound("a", "PC-A", "m1", "你好，这是明文不该出现", 100);
        }
        let raw = std::fs::read(&path).unwrap();
        assert!(
            !raw.windows("你好，这是明文不该出现".len())
                .any(|w| w == "你好，这是明文不该出现".as_bytes()),
            "账本不该以明文落盘"
        );
        let (ext, json) = envelope::decode_if(&raw).unwrap().unwrap();
        assert!(ext.is_empty());
        assert!(std::str::from_utf8(&json).unwrap().contains("\"convos\""));
        let reopened = ChatStore::with_path(path.clone());
        assert_eq!(
            reopened.snapshot().convos[0].messages[0].text,
            "你好，这是明文不该出现"
        );
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn survives_a_restart() {
        let path =
            std::env::temp_dir().join(format!("orbdrop_chat_restart_{}", uuid::Uuid::new_v4()));
        {
            let s = ChatStore::with_path(path.clone());
            s.record_inbound("a", "PC-A", "m1", "你好", 100);
            s.record_outbound("m2", "收到", 200, &peers(&["a"]), None);
            s.mark_read_local("a", 100);
        }
        let reopened = ChatStore::with_path(path.clone());
        let snap = reopened.snapshot();
        let convo = snap.convos.into_iter().find(|c| c.peer_id == "a").unwrap();
        assert_eq!(convo.messages.len(), 2);
        assert_eq!(convo.unread, 0);
        assert_eq!(reopened.read_watermark("a"), 100);
        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn history_is_capped_from_the_old_end() {
        let s = store("cap");
        for i in 0..(MAX_PER_PEER as u64 + 10) {
            s.record_inbound("a", "PC-A", &format!("m{i}"), "x", i + 1);
        }
        let convo = s
            .snapshot()
            .convos
            .into_iter()
            .find(|c| c.peer_id == "a")
            .unwrap();
        assert_eq!(convo.messages.len(), MAX_PER_PEER);
        // 砍掉的是最早的，留下的是最新的。
        assert_eq!(convo.messages.last().unwrap().at, MAX_PER_PEER as u64 + 10);
        assert_eq!(convo.messages[0].at, 11);
    }
}
