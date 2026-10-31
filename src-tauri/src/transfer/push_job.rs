use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::sync::oneshot;
use tokio::time::sleep;
use tracing::{error, info, warn};

use crate::credential;
use crate::state::AppState;
use crate::ws;
use crate::ws::events::{FileEntry, PeerInfo, WsEvent};

/// 回报发送方的间隔（进度）。
///
/// 下载循环按数据块推进（每块 50ms），逐块回报等于 20 次/秒 —— 走 HTTP 那条路时是
/// 20 个请求/秒，而界面看 5 次/秒已经足够平滑。
const REPORT_INTERVAL: Duration = Duration::from_millis(200);

/// 接收方给发送方的一条对账答案（`WsEvent::PushState` 里的一项）。
///
/// `state` 的取值：`done` / `failed` / `cancelled` / `rejected` / `waiting`（本机还拿着这条
/// 邀请：等用户处理或正在拉）/ `unknown`（本机没见过它 —— 那条邀请根本没送到）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PushInviteStateWire {
    pub invite_id: String,
    pub state: String,
    /// 已收字节。只有 `waiting` 有参考价值（进度回执本身也可能丢）。
    pub received: u64,
    pub reason: Option<String>,
}

// ---------------------------------------------------------------------------
// Data models
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub enum PushStatus {
    Invited,
    Accepted,
    Downloading { received: u64, total: u64 },
    Completed,
    Failed(String),
}

/// Sender-side state of a push job (F-50 ~ F-52)：**一台设备一条**。
///
/// 清单里的文件表不在这里 —— 取文件只按 `sources` 查表（见 `download_target`），
/// 而一份清单会被同批的每台设备各建一条 job：那张表多存一份就是白克隆一份
/// （推一个几万文件的目录给 8 台设备，就是 8 份几万条的表）。
#[derive(Clone)]
pub struct PushJob {
    pub id: String,
    /// 这台属于哪一批（F-50 多目标）：界面那张聚合卡片按它归拢。
    pub batch_id: String,
    pub peer_id: String,
    pub peer_name: String,
    pub name: String,
    pub is_dir: bool,
    /// 清单相对路径 → 本机绝对路径（服务端按它取文件，见 `download_target`）。
    ///
    /// **表**而不是列表：下载端点每请求查一次（推一个 2 万文件的目录就是 2 万次查询）。
    pub sources: HashMap<String, PathBuf>,
    pub status: PushStatus,
    pub created_at: Instant,
    /// 对端是否已配对：它决定了下载端点怎么认人（见 `routes_push`）。
    pub peer_paired: bool,
    /// 这条 job 是不是已经作废（取消 / 已完成 / 失败被清掉）。
    ///
    /// 下载端点**每读一帧**都要问一次"还能不能继续发"，那是整份流的每一帧；拿
    /// `PushStore::has_job` 问就是每 256KB 抢一次全局互斥锁。换成一个原子量：`remove_job`
    /// 在这里置位，正在发的流下一帧就收手 —— 同样的即时性，零锁。
    pub cancel: Arc<AtomicBool>,
}

impl PushJob {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: String,
        batch_id: String,
        peer_id: String,
        peer_name: String,
        name: String,
        is_dir: bool,
        sources: HashMap<String, PathBuf>,
        peer_paired: bool,
    ) -> Self {
        Self {
            id,
            batch_id,
            peer_id,
            peer_name,
            name,
            is_dir,
            sources,
            status: PushStatus::Invited,
            created_at: Instant::now(),
            peer_paired,
            cancel: Arc::new(AtomicBool::new(false)),
        }
    }
}

/// 接收方的一条待处理邀请。
pub struct InboundInvite {
    pub invite_id: String,
    pub sender: PeerInfo,
    pub name: String,
    pub size: u64,
    pub is_dir: bool,
    /// 发送方这次选了几项（>1 = 多选合并成的一条推送；通知文案用它）。
    pub count: usize,
    pub files: Vec<FileEntry>,
    /// 发送方声明支持的能力位（见 `WsEvent::PushInvite::caps`）。
    pub caps: Vec<String>,
    /// 这次接收的落盘**根目录**：`None` = 用设置里的下载目录（S-01）。
    ///
    /// 只有"需要用户同意"那条路（未绑定设备，或已配对但关了「自动接收」）才可能带它 ——
    /// 开着自动接收时"到了就收"，没有让人先挑目录的时机（见 `transfer::accept_invite`
    /// 的 `root` 参数）。
    ///
    /// 它只是**根**：`<根>/<发送方名>/…` 这一层由 `resume::landing_dir` 按设置（S-12）拼、
    /// 名字照样净化；关掉那层时根就是落点（`safe_join` 逐段校验仍是围栏）。
    pub root: Option<std::path::PathBuf>,
    /// 发送方的地址（ip, 服务端口）：没有长连接时，进度/完成/取消这些回执要 POST 回去。
    ///
    /// ip 取自邀请请求的 TCP 来源（不可伪造），端口取自邀请体里发送方自报的服务端口 ——
    /// 请求的源端口是一次性的，发回那儿没人听。
    ///
    /// 下载真正开始时（`accept_invite`）这个地址**必须**解析得出来，否则拉不到数据；
    /// 那一步是唯一的入口，也就在那儿把"地址未知"变成一条明确错误。
    pub callback: Option<(String, u16)>,
    /// HTTP 邀请的同步回执口：用户在界面上点同意/拒绝时把它填上。
    /// 长连接那条路没有它（回执走 WS `push_response`）。
    pub responder: Option<oneshot::Sender<bool>>,
}

/// 新数据面（分帧并发）的能力位。
pub const CAP_STREAM: &str = "stream1";

/// 「决定走报告」的能力位：邀请**投递即算送达**，用户点完「接收 / 拒绝」之后，那道决定
/// 才通过回执通道（WS `push_response` 或 `POST /push/report`）走回来。
///
/// 不带这个位的对端（老版本）走的是另一套：那条 HTTP 邀请**挂着等同步响应**，本机 60 秒
/// 没人处理就自动拒绝（见 `transfer::schedule_invite_timeout`）。带了这个位，本机与对端
/// 两边都**没有时钟** —— 邀请一直有效，等用户点、或发送方撤销（见 `transfer::cancel_push`；
/// 发送方还会在源文件/目录变化时自己取消）。
pub const CAP_AWAIT: &str = "await1";

/// 一条入站邀请的硬上限。**正常使用远低于这些值**（README 的基准是 3000 个文件），
/// 它们只用来挡住"远端报一个 9PB 的 size 让接收端自己分配位图/任务队列"这类攻击。
pub const MAX_INVITE_FILES: usize = 100_000;
pub const MAX_INVITE_TOTAL_BYTES: u64 = 10 * 1024 * 1024 * 1024 * 1024; // 10 TiB
pub const MAX_INVITE_FILE_BYTES: u64 = 1 * 1024 * 1024 * 1024 * 1024; // 1 TiB
pub const MAX_INVITE_PATH_BYTES: usize = 4096;

/// 校验远端清单。**在 insert/accept 之前调用**：`download::run` 会按清单里的 size
/// 分配块位图与任务队列，一个伪造的巨大 size 就能在真正传输前把内存打满。
pub fn validate_invite(inv: &InboundInvite) -> Result<(), String> {
    if inv.count > MAX_INVITE_FILES || inv.files.len() > MAX_INVITE_FILES {
        return Err(format!(
            "文件数超过上限（{} > {MAX_INVITE_FILES}）",
            inv.files.len().max(inv.count)
        ));
    }
    if !inv.is_dir && inv.size > MAX_INVITE_FILE_BYTES {
        return Err(format!(
            "单文件大小超过上限（{} > {MAX_INVITE_FILE_BYTES}）",
            inv.size
        ));
    }
    let mut total = 0u64;
    for f in &inv.files {
        if f.path.len() > MAX_INVITE_PATH_BYTES {
            return Err(format!("路径过长（{} 字节）", f.path.len()));
        }
        if f.size > MAX_INVITE_FILE_BYTES {
            return Err(format!(
                "单文件大小超过上限（{} > {MAX_INVITE_FILE_BYTES}）",
                f.size
            ));
        }
        total = total.saturating_add(f.size);
    }
    if total > MAX_INVITE_TOTAL_BYTES {
        return Err(format!(
            "单批总大小超过上限（{total} > {MAX_INVITE_TOTAL_BYTES}）"
        ));
    }
    Ok(())
}

impl InboundInvite {
    /// 走新协议还是老循环。判据只有一个：**对端说自己支持**（老对端不带这个位）。
    pub fn supports_stream(&self) -> bool {
        self.caps.iter().any(|c| c == CAP_STREAM)
    }

    /// 对端是不是"等报告"那一版（见 [`CAP_AWAIT`]）：是就不要排那个 60 秒作废。
    pub fn supports_await(&self) -> bool {
        self.caps.iter().any(|c| c == CAP_AWAIT)
    }

    pub fn total_bytes(&self) -> u64 {
        if self.is_dir {
            self.files
                .iter()
                .fold(0u64, |acc, f| acc.saturating_add(f.size))
        } else {
            self.size
        }
    }

    /// 这条邀请要收的**文件清单**（老循环 `spawn_download_legacy` 与终结清理 `discard_legacy`
    /// 必须按同一份算）。
    ///
    /// 目录推送：清单本身（`files` 是权威的，见 `manifest_for_paths` 里 `is_dir` 的含义）。
    /// 单文件推送：对端那条路上本机只认名字与大小（原来这段合成写在下载循环里），现场合成一条。
    /// 清理要拿每项的 `size` 判"哪一个是我们写了一半的"，所以合成规则只能有一处 —— 两处不一致
    /// 就等于删错文件。
    pub fn manifest(&self) -> Vec<FileEntry> {
        if self.is_dir {
            self.files.clone()
        } else {
            vec![FileEntry {
                path: self.name.clone(),
                size: self.size,
                // 单文件推送这条路上对端只有名字与大小（没有清单），时间戳无从谈起。
                mtime: 0,
            }]
        }
    }

    /// 落成一条**待续传任务**（`transfer::accept_invite` 放行那一刻）。
    ///
    /// 邀请本体（`InboundInvite`）带 `oneshot` 与响应口，落不了盘也不该落盘；能落的就是
    /// "重新发一次这条接收请求"要的那几样（见 `PendingInbound` 那段说明）。
    pub fn to_pending(&self) -> super::resume::PendingInbound {
        super::resume::PendingInbound {
            invite_id: self.invite_id.clone(),
            sender_id: self.sender.id.clone(),
            sender_name: self.sender.name.clone(),
            name: self.name.clone(),
            size: self.size,
            is_dir: self.is_dir,
            count: self.count as u32,
            files: self
                .files
                .iter()
                .map(|f| super::resume::PendingFile {
                    path: f.path.clone(),
                    size: f.size,
                    mtime: f.mtime,
                })
                .collect(),
            caps: self.caps.clone(),
            root: self.root.as_ref().map(|p| p.to_string_lossy().into_owned()),
            callback: self.callback.clone(),
            updated_at: super::resume::now_secs(),
        }
    }
}

impl super::resume::PendingInbound {
    /// 从落盘那条任务重建一份邀请（发起续传那一跳用它）。
    ///
    /// `responder: None`：那条同步回执口只属于"邀请刚到、等用户点头"那一刻 —— 续传时用户
    /// 早就点过头了（我们只记已经放行的邀请）。
    pub fn to_invite(&self) -> InboundInvite {
        InboundInvite {
            invite_id: self.invite_id.clone(),
            sender: PeerInfo {
                id: self.sender_id.clone(),
                name: self.sender_name.clone(),
            },
            name: self.name.clone(),
            size: self.size,
            is_dir: self.is_dir,
            count: self.count as usize,
            files: self
                .files
                .iter()
                .map(|f| FileEntry {
                    path: f.path.clone(),
                    size: f.size,
                    mtime: f.mtime,
                })
                .collect(),
            caps: self.caps.clone(),
            root: self.root.as_ref().map(PathBuf::from),
            callback: self.callback.clone(),
            responder: None,
        }
    }
}

// ---------------------------------------------------------------------------
// PushStore
// ---------------------------------------------------------------------------

/// 下载端点为一次请求要的东西：**只有它**，不克隆整份 job。
pub enum DownloadTarget {
    /// 这条推送不在了（已取消 / 传完了）。
    Missing,
    /// 这条推送里没有这个相对路径（对端给错了，或想越过清单读别的文件）。
    NotInManifest,
    Found {
        path: PathBuf,
        peer_paired: bool,
        peer_id: String,
        /// 这条 job 的作废标志（`remove_job` 置位）：流式读取按帧看它，见 `CancelAwareReader`。
        cancel: Arc<AtomicBool>,
    },
}

// ---------------------------------------------------------------------------
// 推送批次（F-50 多目标）：一次操作 = 一批 = 界面上一张卡片
// ---------------------------------------------------------------------------

/// 一台目标设备在一批推送里的状态。
///
/// `Cancelled` 不并进 `Failed`：取消是**用户自己做的事**，行文案必须是「已取消」而不是
/// 「失败」；两者只在"这一批算不算全成"这件事上同类（卡片的收起规则见前端）。
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum TargetState {
    /// 邀请已发出、还没有任何回执（已配对且对面开着自动接收的会很快转 `Sending`）。
    Invited,
    /// 对端正在拉数据。
    Sending,
    Done,
    Cancelled,
    Rejected,
    Failed,
}

impl TargetState {
    pub fn key(self) -> &'static str {
        match self {
            TargetState::Invited => "invited",
            TargetState::Sending => "sending",
            TargetState::Done => "done",
            TargetState::Cancelled => "cancelled",
            TargetState::Rejected => "rejected",
            TargetState::Failed => "failed",
        }
    }

    /// 终态只落一次：取消之后对端补报的失败不再把「已取消」改写成「失败」。
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            TargetState::Done
                | TargetState::Cancelled
                | TargetState::Rejected
                | TargetState::Failed
        )
    }

    /// `key()` 的反向：从盘上那份恢复时用。认不出来的（以后加了档、又被降级读）当**终态**处理
    /// —— 宁可让那一行显示成"失败"，也不要凭空造一个"还在等"的假象。
    pub fn from_key(key: &str) -> Option<Self> {
        Some(match key {
            "invited" => TargetState::Invited,
            "sending" => TargetState::Sending,
            "done" => TargetState::Done,
            "cancelled" => TargetState::Cancelled,
            "rejected" => TargetState::Rejected,
            "failed" => TargetState::Failed,
            _ => return None,
        })
    }

    /// 还在路上 —— "整批走完没有"和"还能不能取消"都看它。
    pub fn is_waiting(self) -> bool {
        matches!(self, TargetState::Invited | TargetState::Sending)
    }
}

/// 一批推送里的一台目标设备（界面卡片上的一行）。
#[derive(Clone)]
pub struct BatchTarget {
    pub peer_id: String,
    pub peer_name: String,
    /// 这一台对应的邀请 id（= 它那条 `PushJob.id`；LocalSend 是本地任务 id）。
    pub invite_id: String,
    pub transport: crate::config::PeerTransport,
    /// 已配对 = 身份可信；是否免同意还取决于**对方**的「已配对设备自动接收」设置，
    /// 行文案不能只凭它断定"对方没点过任何东西"。
    pub paired: bool,
    pub state: TargetState,
    pub received: u64,
    pub error: Option<String>,
}

/// 一次推送操作：**一份清单、N 台设备、界面上一张卡片**。
///
/// 单台推送也是一批（`targets` 只有一条）—— 后端与界面都只有这一条路，不为"多设备"
/// 分叉出第二套事件与收尾逻辑；接收侧则完全不知道批次存在（每台收到的邀请和以前一模一样，
/// 同意规则、落盘规则、取消规则、断点续传全都不变）。
pub struct PushBatch {
    pub id: String,
    /// 清单的显示名（"推的是什么"），同 `PushSource.name`，不参与落盘路径。
    pub name: String,
    /// 用户这次选了几项（>1 = 多选合并成的一条推送）。
    pub count: usize,
    /// 清单里被跳过的项数（链接 / 读不到的项）：>0 时卡片上必须说出来（见 `PushSource`）。
    pub skipped: usize,
    /// 每台的字节总量（各台都是同一份清单）。
    pub per_target_total: u64,
    pub targets: Vec<BatchTarget>,
    /// 上次发快照的时刻：纯进度更新按 `REPORT_INTERVAL` 合并，
    /// 状态跃迁（成功/失败/取消）不受它约束（见 `batch_set`）。
    pub last_emit: Instant,
}

/// 一批推送的快照（`push://batch` 事件体 / `push_paths` 的返回值）。字段走 camelCase。
#[derive(serde::Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct BatchView {
    pub batch_id: String,
    pub name: String,
    pub count: usize,
    /// 清单里被跳过的项数（链接 / 读不到的项）：卡片上那句"已跳过 N 项"就是它。
    pub skipped: usize,
    /// 成功 / 等待（含传输中）/ 失败（含被拒、被取消）—— 界面那行「成功 x · 等待 y」就是它。
    pub done: usize,
    pub waiting: usize,
    pub failed: usize,
    /// 聚合进度：分子是各台已收字节之和，分母**只算还活着的台**（失败/取消的那台整条不计），
    /// 所以有台失败时进度条照样能走到 100%（每行自己还有百分比与状态，信息不会丢）。
    pub received: u64,
    pub total: u64,
    pub targets: Vec<BatchTargetView>,
}

#[derive(serde::Serialize, Debug)]
#[serde(rename_all = "camelCase")]
pub struct BatchTargetView {
    pub peer_id: String,
    /// 设备名（界面那一行行首的字）。
    pub target: String,
    pub invite_id: String,
    pub transport: &'static str,
    pub paired: bool,
    pub state: &'static str,
    pub received: u64,
    pub error: Option<String>,
}

fn snapshot(batch: &PushBatch) -> BatchView {
    let (mut done, mut waiting, mut failed) = (0, 0, 0);
    let (mut received, mut total) = (0u64, 0u64);
    for t in &batch.targets {
        match t.state {
            TargetState::Done => {
                done += 1;
                received += batch.per_target_total;
                total += batch.per_target_total;
            }
            TargetState::Invited | TargetState::Sending => {
                waiting += 1;
                received += t.received.min(batch.per_target_total);
                total += batch.per_target_total;
            }
            // 失败 / 被拒 / 被取消：整条不计入进度分母（上面那条注释）。
            _ => failed += 1,
        }
    }
    BatchView {
        batch_id: batch.id.clone(),
        name: batch.name.clone(),
        count: batch.count,
        skipped: batch.skipped,
        done,
        waiting,
        failed,
        received,
        total,
        targets: batch
            .targets
            .iter()
            .map(|t| BatchTargetView {
                peer_id: t.peer_id.clone(),
                target: t.peer_name.clone(),
                invite_id: t.invite_id.clone(),
                transport: t.transport.key(),
                paired: t.paired,
                state: t.state.key(),
                received: t.received,
                error: t.error.clone(),
            })
            .collect(),
    }
}

pub struct PushStore {
    pub jobs: Mutex<HashMap<String, PushJob>>,
    pub inbound: Mutex<HashMap<String, InboundInvite>>,
    /// 发送端那份"还没走完的推送"的落盘（跨重启那一半，见 `push_out`）。
    ///
    /// 与 `resume`（接收端那份）分两个文件：老版本会重写 `resume.json`，而 `push_out.json`
    /// 它压根不认识 —— 降级也就抹不掉（见 `push_out` 头注释）。
    pub out: super::push_out::PushOutStore,
    pub resume: super::resume::ResumeStore,
    /// 推送批次（= 界面上的卡片）。与 `jobs` 分开存：job 是"能不能取文件"的凭据，
    /// 传完就清；批次要活到整批走完那一刻，卡片才一直有数可显示。
    pub batches: Mutex<HashMap<String, PushBatch>>,
    /// invite_id → batch_id：回执（WS / `POST /push/report`）只知道邀请 id，
    /// 得 O(1) 找到它属于哪一批。
    batch_of_invite: Mutex<HashMap<String, String>>,
    /// 最近处理过的**别人推给本机**的邀请结局（有界）。
    ///
    /// 为什么要留一份：传完/失败之后 `inbound` 与 `jobs` 里那条都已经清了，而"回执丢了"
    /// 的对账（`WsEvent::PushAsk`）得答得出"这条我当时收没收到、结果如何" —— 没有这份记录，
    /// 发送方就只能一直停在「等待」，然后把一条已经成功的传输记成失败。
    recent: Mutex<HashMap<String, RecentOutcome>>,
    /// 对账的节流：peer_id → 上次问的时刻（一台设备别每 5 秒问一遍）。
    asked_at: Mutex<HashMap<String, Instant>>,
    /// 每条入站邀请**来到本机**的时刻：invite_id → Instant。
    ///
    /// 只给 `MAX_INBOUND_INVITES` 那条淘汰路用（"最老优先"要有个先后）。与 `inbound` 同生同死：
    /// 两个入口（`insert_invite` / `remove_invite`+`take_invite`）都同时维护两边，锁序固定
    /// `inbound` → `invite_seen`。
    invite_seen: Mutex<HashMap<String, Instant>>,
    /// 已经走完、但还在宽限期的批次：batch_id → 走完的时刻（见 `SETTLED_GRACE`）。
    settled: Mutex<HashMap<String, Instant>>,
}

/// 本机同时留着的**入站邀请**上限（未处理的卡片 + 正在跑的那几条，整张表一起算）。
///
/// 为什么要有：`POST /push/invite` 是**公开路由**（未鉴权也能投递 —— 这是"不配对也能推"的代价），
/// 每条邀请都揣着对端给的那份清单（单条上限 64MB，见 `server` 的 `MANIFEST_BODY_LIMIT`），
/// 而带 `await1` 的邀请**没有时钟**（F-51：用户不点就一直留着，见 `schedule_invite_timeout`）——
/// 于是"换一个 invite id 再投一条"就能让本机的内存无上界地涨。
///
/// 这个数是给那条滥用路兜底的，**不是给正常用法设的**：单用户同一时刻手上顶多两三张卡片
/// （多选/多设备推送也还是"一次操作一批"，加上正在跑的那几条），128 是十几倍的余量，
/// 正常用永远碰不到。挑这个量级的理由：最坏情况 = 128 × 单条 64MB ≈ 8GB，那得攻击者真的传上来
/// 8GB（网络先受不了），而这里要的是**让它有界**；再往下压（32 之类）省的内存有限，先被挤掉的
/// 却可能是正常用户手上的卡片。
///
/// 淘汰是"最老优先"：被挤掉的那条对端会停在「等待」—— 与 F-51 的可见语义一致（那条路上本来
/// 就没有时钟），代价是极端滥用下丢几张卡，而**不是**把本机 OOM。
const MAX_INBOUND_INVITES: usize = 128;

/// 所有入站邀请**合计**允许保留的清单字节数。
///
/// 只限条数不够：每条 body 上限 64 MiB，128 条就是约 8 GiB 常驻内存。正常用户同一时刻
/// 手上只有几张卡，512 MiB 对正常使用仍有数量级余量；到顶时与条数上限一样挤掉最老的，
/// 只影响滥用流量，不改变正常推送的可见行为。
const MAX_INBOUND_BYTES: usize = 512 * 1024 * 1024;

/// 一条入站邀请在内存里大概占多少：结构体本身 + 对端给的字符串/清单。估算只用于触发
/// 淘汰，不需要精确；宁可略高估。
fn invite_footprint(inv: &InboundInvite) -> usize {
    let mut bytes = 512usize
        .saturating_add(inv.invite_id.len())
        .saturating_add(inv.sender.id.len())
        .saturating_add(inv.sender.name.len())
        .saturating_add(inv.name.len())
        .saturating_add(inv.root.as_ref().map(|p| p.to_string_lossy().len()).unwrap_or(0))
        .saturating_add(inv.callback.as_ref().map(|(ip, _)| ip.len()).unwrap_or(0))
        .saturating_add(inv.caps.iter().map(String::len).sum::<usize>());
    for f in &inv.files {
        bytes = bytes
            .saturating_add(64)
            .saturating_add(f.path.len());
    }
    bytes
}

/// 一条**入站**邀请的处理结局（对账用；`state` 是 `PushInviteStateWire::state` 的取值）。
struct RecentOutcome {
    /// 发送方 id：只有它自己能问出这一条（别人的会话问同一个 id 一律 `unknown`）。
    sender_id: String,
    state: &'static str,
    received: u64,
    reason: Option<String>,
    at: Instant,
}

/// 记多少条结局：一笔推送一条，64 条足以覆盖"还没落终态"的那几分钟。
const RECENT_OUTCOMES: usize = 64;

/// 对账的间隔：链路在的时候每 5 秒探活一拍，但同一台设备最快 20 秒问一次
/// （回执丢了就靠下一拍补上，没必要 5 秒一问）。
const ASK_INTERVAL: Duration = Duration::from_secs(20);

/// 一批走完之后还留多久。
///
/// 走完就丢的代价是："本机推测的失败"再也没有机会被纠正 —— 对端的回执可能还在路上，
/// 而链路回来之后的对账（`PushAsk`）也得先找得到这批。留一段宽限期，到点由
/// `sweep_settled_batches` 收掉（跑得比这快的应用里也就几 KB）。
const SETTLED_GRACE: Duration = Duration::from_secs(600);

impl PushStore {
    pub fn new() -> Self {
        Self {
            jobs: Mutex::new(HashMap::new()),
            inbound: Mutex::new(HashMap::new()),
            out: super::push_out::PushOutStore::new(),
            resume: super::resume::ResumeStore::new(),
            batches: Mutex::new(HashMap::new()),
            batch_of_invite: Mutex::new(HashMap::new()),
            recent: Mutex::new(HashMap::new()),
            asked_at: Mutex::new(HashMap::new()),
            invite_seen: Mutex::new(HashMap::new()),
            settled: Mutex::new(HashMap::new()),
        }
    }

    // batch
    /// 登记一批推送，返回初始快照（每台都是"等待"）。
    pub fn insert_batch(&self, batch: PushBatch) -> BatchView {
        let view = snapshot(&batch);
        let index: Vec<(String, String)> = batch
            .targets
            .iter()
            // 派不出去的那几台（离线/设备不存在）没有邀请 id —— 别把空串塞进反查表。
            .filter(|t| !t.invite_id.is_empty())
            .map(|t| (t.invite_id.clone(), batch.id.clone()))
            .collect();
        // 两把锁的顺序固定为 batches → batch_of_invite（`drop_batch` 同序），别反过来。
        self.batches.lock().insert(batch.id.clone(), batch);
        let mut lookup = self.batch_of_invite.lock();
        for (invite_id, batch_id) in index {
            lookup.insert(invite_id, batch_id);
        }
        view
    }

    pub fn batch_id_of(&self, invite_id: &str) -> Option<String> {
        self.batch_of_invite.lock().get(invite_id).cloned()
    }

    /// 一批里各台的状态（取消整批用）。
    pub fn batch_targets(&self, batch_id: &str) -> Option<Vec<BatchTarget>> {
        self.batches.lock().get(batch_id).map(|b| b.targets.clone())
    }

    /// 现在这一刻的快照（与推给界面的那份同一个算法）。
    pub fn batch_snapshot(&self, batch_id: &str) -> Option<BatchView> {
        self.batches.lock().get(batch_id).map(snapshot)
    }

    /// 一批里**每台**的字节总量（各台共用同一份清单）。过期收尾时拿它判"最后是不是报满了"
    /// —— 不能用 `BatchView.total`：那是"还活着的台之和"，多台时会偏大。
    pub fn batch_per_target_total(&self, batch_id: &str) -> u64 {
        self.batches
            .lock()
            .get(batch_id)
            .map(|b| b.per_target_total)
            .unwrap_or(0)
    }

    /// 全部批次的快照（`push_batches` 命令用：界面挂载时拉一次初值 —— 重启后那些恢复出来的
    /// 卡只能靠它拿到，事件是"变化"，初值必须能拉）。
    ///
    /// 顺序按批次 id 排一下：界面按 `batchId` 归并，但稳定的顺序让"先来哪张卡"不随机。
    pub fn all_batch_views(&self) -> Vec<BatchView> {
        let batches = self.batches.lock();
        let mut ids: Vec<&String> = batches.keys().collect();
        ids.sort();
        ids.into_iter()
            .filter_map(|id| batches.get(id).map(snapshot))
            .collect()
    }

    /// 整批走完就丢掉：界面手里那份最后的快照已经是全量。
    pub fn drop_batch(&self, batch_id: &str) {
        // 盘上那份一并删（重启后不该再把这批恢复出来）。
        self.out.drop_batch(batch_id);
        let Some(batch) = self.batches.lock().remove(batch_id) else {
            return;
        };
        let mut lookup = self.batch_of_invite.lock();
        for t in &batch.targets {
            lookup.remove(&t.invite_id);
        }
    }

    // sender
    pub fn insert_job(&self, job: PushJob) {
        self.jobs.lock().insert(job.id.clone(), job);
    }

    pub fn update_status(&self, id: &str, status: PushStatus) {
        let is_progress = matches!(status, PushStatus::Downloading { .. });
        // 落盘要用的那份先取出来（别攥着 `jobs` 的锁去写几 MB 的 JSON）。
        let accepted_now = {
            let mut jobs = self.jobs.lock();
            let Some(job) = jobs.get_mut(id) else {
                return;
            };
            // "对面真的开始拉了"是**一次**状态跃迁（`Invited` → 之后）：这一刻把它落盘，
            // 重启后这条才能被重建出来（见 `push_out`）。之后的进度只挪 TTL 锚点（`touch_job`）。
            let was_invited = matches!(job.status, PushStatus::Invited);
            job.status = status;
            (was_invited && !matches!(job.status, PushStatus::Invited))
                .then(|| out_job_of(job, true))
        };
        if let Some(rec) = accepted_now {
            self.out.save_job(rec);
        }
        // 有字节在动 = 这条还活着：把 TTL 锚点往前挪（**不落盘**，攒批到 `TOUCH_FLUSH`）。
        // 少了这一句，"传了 6 天还在传"的那条会在第 7 天被自家 TTL 判过期。
        if is_progress {
            self.out.touch_job(id);
        }
    }

    pub fn job_status(&self, id: &str) -> Option<String> {
        self.jobs.lock().get(id).map(|j| match &j.status {
            PushStatus::Invited => "invited".into(),
            PushStatus::Accepted => "accepted".into(),
            PushStatus::Downloading { received, total } => {
                format!("downloading: {received}/{total}")
            }
            PushStatus::Completed => "completed".into(),
            PushStatus::Failed(r) => format!("failed: {r}"),
        })
    }

    pub fn remove_job(&self, id: &str) {
        // 终态 = 重启后不该再恢复：盘上那条一并删（这里是它唯一的出口，取消/完成/失败收尾
        // 全都走 `remove_job`）。
        self.out.drop_job(id);
        // 顺带置位"作废"标志：正在发的那条流是唯一看不到 `jobs` 变空的地方（它只按帧
        // 检查那个原子量），不置位的话取消一个大文件要等它自己读完。
        if let Some(job) = self.jobs.lock().remove(id) {
            job.cancel.store(true, Ordering::SeqCst);
        }
    }

    pub fn job(&self, id: &str) -> Option<PushJob> {
        self.jobs.lock().get(id).cloned()
    }

    /// 下载端点的一次取文件请求：按**清单里的相对路径**精确查表。
    ///
    /// 这条查找本身就是路径校验 —— 清单里没有的路径一律取不到，`../` 之类的越界路径
    /// 自然也不在表里（多选推送时清单是"几个源各占一个前缀"合成的一棵树，同样如此）。
    pub fn download_target(&self, id: &str, rel: &str) -> DownloadTarget {
        let jobs = self.jobs.lock();
        let Some(job) = jobs.get(id) else {
            return DownloadTarget::Missing;
        };
        match job.sources.get(rel) {
            Some(abs) => DownloadTarget::Found {
                path: abs.clone(),
                peer_paired: job.peer_paired,
                peer_id: job.peer_id.clone(),
                cancel: job.cancel.clone(),
            },
            None => DownloadTarget::NotInManifest,
        }
    }

    // receiver
    pub fn insert_invite(&self, inv: InboundInvite) {
        self.insert_invite_with_budget(inv, MAX_INBOUND_BYTES);
    }

    /// [`insert_invite`] 的预算参数版：生产用 [`MAX_INBOUND_BYTES`]，测试用小预算跑同一条
    /// 淘汰路径（不必真的分配 512 MiB）。
    fn insert_invite_with_budget(&self, inv: InboundInvite, max_bytes: usize) {
        let id = inv.invite_id.clone();
        let footprint = invite_footprint(&inv);
        // 锁序固定 `inbound` → `invite_seen`（`remove_invite` / `take_invite` 同序）。
        let mut inbound = self.inbound.lock();
        let mut seen = self.invite_seen.lock();
        // 顺序记账跟着表走：表里已经没有的键一律丢掉（它在 `remove_invite`/`take_invite` 里也删，
        // 这里是兜底 —— 两个入口漏一处，淘汰就会按一条早就不在的 id 去"最老"）。
        seen.retain(|k, _| inbound.contains_key(k));
        // 32 位手机上 128 × 64 MiB 会超过 `usize::MAX`：这里必须饱和加，不能 `.sum()`
        // （debug 下溢出 panic，release 下绕回一个小数，淘汰就不受控了）。
        let mut total: usize = inbound
            .values()
            .fold(0usize, |acc, inv| acc.saturating_add(invite_footprint(inv)));
        while !inbound.is_empty()
            && (inbound.len() >= MAX_INBOUND_INVITES
                || total.saturating_add(footprint) > max_bytes)
        {
            // 满了就挤掉**最老**的那条（与结局记录淘汰旧条同一套写法，见 `record_outcome`）。
            // 挤不动（记账与表不同步到一条都对不出来）就 break：宁可留一条多的，也不空转。
            let Some(oldest) = seen
                .iter()
                .min_by_key(|(_, at)| **at)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            if let Some(gone) = inbound.remove(&oldest) {
                total = total.saturating_sub(invite_footprint(&gone));
            }
            seen.remove(&oldest);
            warn!(
                invite = %oldest,
                limit = MAX_INBOUND_INVITES,
                bytes = max_bytes,
                "入站邀请到上限了，挤掉最老的一条（未鉴权的投递换新 id 也刷不出无上界的表）"
            );
        }
        seen.insert(id.clone(), Instant::now());
        inbound.insert(id, inv);
    }

    pub fn remove_invite(&self, id: &str) {
        self.inbound.lock().remove(id);
        self.invite_seen.lock().remove(id);
    }

    /// 取走一条邀请（拒绝/超时/取消）。取走后下载循环就会收手。
    pub fn take_invite(&self, id: &str) -> Option<InboundInvite> {
        let taken = self.inbound.lock().remove(id);
        self.invite_seen.lock().remove(id);
        taken
    }

    // 对账（回执丢了怎么办，见 `WsEvent::PushAsk` / `PushState`）
    /// 记下一条入站邀请的结局。**每条只记一次**（终局），条数按 `RECENT_OUTCOMES` 淘汰最旧的。
    pub fn record_outcome(
        &self,
        invite_id: &str,
        sender_id: &str,
        state: &'static str,
        received: u64,
        reason: Option<String>,
    ) {
        let mut recent = self.recent.lock();
        recent.insert(
            invite_id.to_string(),
            RecentOutcome {
                sender_id: sender_id.to_string(),
                state,
                received,
                reason,
                at: Instant::now(),
            },
        );
        while recent.len() > RECENT_OUTCOMES {
            let Some(oldest) = recent
                .iter()
                .min_by_key(|(_, o)| o.at)
                .map(|(k, _)| k.clone())
            else {
                break;
            };
            recent.remove(&oldest);
        }
    }

    /// 对账：这条邀请（如果是**这台**设备推来的）此刻什么状态。
    ///
    /// 先看还拿在手里的（`inbound` 里有 = 等用户处理，或下载循环还在跑）→ `waiting`；
    /// 再看结局记录；都没有 → `unknown`。
    pub fn answer_invite_state(&self, invite_id: &str, sender_id: &str) -> PushInviteStateWire {
        let held = self
            .inbound
            .lock()
            .get(invite_id)
            .is_some_and(|inv| inv.sender.id == sender_id);
        if held {
            return PushInviteStateWire {
                invite_id: invite_id.to_string(),
                state: "waiting".into(),
                received: 0,
                reason: None,
            };
        }
        if let Some(o) = self.recent.lock().get(invite_id) {
            if o.sender_id == sender_id {
                return PushInviteStateWire {
                    invite_id: invite_id.to_string(),
                    state: o.state.into(),
                    received: o.received,
                    reason: o.reason.clone(),
                };
            }
        }
        PushInviteStateWire {
            invite_id: invite_id.to_string(),
            state: "unknown".into(),
            received: 0,
            reason: None,
        }
    }

    /// 这条邀请的对端是不是"等报告"那一版（`await1`，见 [`CAP_AWAIT`]）——
    /// `schedule_invite_timeout` 靠它决定要不要排那个 60 秒作废。
    pub fn invite_supports_await(&self, id: &str) -> bool {
        self.inbound
            .lock()
            .get(id)
            .is_some_and(|inv| inv.supports_await())
    }

    /// 这批里还有没有"等对方点头"的台（`Invited`）。
    ///
    /// 源变化监视（`transfer::watch`）只在等待期间有意义：**都点头了（或都落了终态）就卸掉**，
    /// 正在传的时候不再盯 —— 那时改源是"传的东西变了"，由接收端按清单 `size`/`mtime` 对账。
    pub fn batch_has_invited(&self, batch_id: &str) -> bool {
        self.batches
            .lock()
            .get(batch_id)
            .is_some_and(|b| b.targets.iter().any(|t| t.state == TargetState::Invited))
    }

    /// 对账的闸门：这台设备现在可以问了吗（可以 = 顺带把"上次问的时刻"推到现在）。
    /// 回执丢了就靠下一拍再问，所以这里是**节流**而不是"只问一次"。
    pub fn ask_due(&self, peer_id: &str) -> bool {
        let mut asked = self.asked_at.lock();
        let due = asked
            .get(peer_id)
            .is_none_or(|t| t.elapsed() >= ASK_INTERVAL);
        if due {
            asked.insert(peer_id.to_string(), Instant::now());
        }
        due
    }

    /// 一批走完了：记下时刻，等宽限期到了再丢（见 `SETTLED_GRACE`）。
    pub fn settle_batch(&self, batch_id: &str) {
        let mut settled = self.settled.lock();
        if settled.contains_key(batch_id) {
            return; // 纠正之后可能又走一遍"全终态"，别把宽限期往后推
        }
        settled.insert(batch_id.to_string(), Instant::now());
    }

    /// 把过了宽限期的批次真丢掉（由后台那个一分钟一拍的小任务叫）。
    pub fn sweep_settled_batches(&self) {
        let due: Vec<String> = {
            let settled = self.settled.lock();
            settled
                .iter()
                .filter(|(_, at)| at.elapsed() >= SETTLED_GRACE)
                .map(|(id, _)| id.clone())
                .collect()
        };
        for id in due {
            self.settled.lock().remove(&id);
            self.drop_batch(&id);
        }
    }

    /// 这批里推给 `peer_id`、还**没落终态**的邀请号（发送方对账要问的就是它们）。
    pub fn waiting_invites_of(&self, peer_id: &str) -> Vec<String> {
        self.batches
            .lock()
            .values()
            .flat_map(|b| b.targets.iter())
            .filter(|t| t.peer_id == peer_id && t.state.is_waiting())
            .map(|t| t.invite_id.clone())
            .collect()
    }

    /// 这条邀请是哪一台的（对账答案落账前要先确认"确实是这台设备的"）。
    pub fn target_of_invite(&self, invite_id: &str) -> Option<BatchTarget> {
        self.batches
            .lock()
            .values()
            .flat_map(|b| b.targets.iter())
            .find(|t| t.invite_id == invite_id)
            .cloned()
    }
}

impl Default for PushStore {
    fn default() -> Self {
        Self::new()
    }
}

// ---------------------------------------------------------------------------
// Batch snapshots to the UI
// ---------------------------------------------------------------------------

/// 「终态只落一次」的**唯一例外**：本机猜的失败可以被对端更权威的终态改写。
///
/// 失败有两种来源：当事人明确表达的（用户取消 / 对端拒绝）与本机的**推测**（超时未响应、
/// 邀请送不出去、对账答 `unknown` —— 三种都可能只是因为"回执丢了"或"恰好撞上对账窗口"）。
/// 后一种一旦拿到对端的权威结局（`Done` = 收完了，`Rejected` = 它明确拒绝了）就是假账，必须改；
/// 前一种是明确意图，改了才是错的。
fn late_success(from: TargetState, to: TargetState) -> bool {
    from == TargetState::Failed && matches!(to, TargetState::Done | TargetState::Rejected)
}

/// 把一批里的某台目标推进到新状态，并把**整批快照**发给界面（`push://batch`）。
///
/// 这是发送侧卡片唯一的数据来源：调用点全部落在本来就有的状态变更处（对端回执、
/// 邀请送不出去、超时、取消），所以"界面看到的"和"后端记着的"仍然是同一个事实
/// （同 `report_to_sender` 那条纪律）。
///
/// - 终态只落一次：取消之后对端补报的失败不会把「已取消」改写成「失败」；
/// - 纯进度（`force = false`）按 `REPORT_INTERVAL` 合并 —— 8 台设备各 5 次/秒的回报
///   不该变成 40 条界面事件；状态跃迁一律立刻发（那正是"成功多少 / 还在等多少"要的实时性）；
/// - 整批没有一台还在路上时，快照发完就把批次丢掉。
pub fn batch_set(
    state: &AppState,
    invite_id: &str,
    target_state: TargetState,
    received: Option<u64>,
    error: Option<String>,
    force: bool,
) {
    let Some(batch_id) = state.push.batch_id_of(invite_id) else {
        return; // 不是本机发出去的推送（例如接收侧的回执），与批次无关
    };
    let (shot, out_record) = {
        let mut batches = state.push.batches.lock();
        let Some(batch) = batches.get_mut(&batch_id) else {
            return;
        };
        let per_target = batch.per_target_total;
        let Some(target) = batch.targets.iter_mut().find(|t| t.invite_id == invite_id) else {
            return;
        };
        if target.state.is_terminal() && !late_success(target.state, target_state) {
            return;
        }
        if late_success(target.state, target_state) {
            // 本机判的"失败"（超时 / 送不出去）被更权威的成功回执改写了 —— 对面确实收完了，
            // 本机之前那条是假账（回执丢了）。日志留一句，免得以后看到"失败变成功"以为是 bug。
            info!(%invite_id, "push marked failed before, but the peer reports it completed; correcting");
        }
        target.state = target_state;
        if target_state == TargetState::Done {
            target.received = per_target;
        } else if let Some(now) = received {
            target.received = now.min(per_target);
        }
        target.error = error;
        // 落到这里 = 这一台真的变了（换档，或进度往前挪）→ 把落盘那份做出来（纯计算），
        // **写盘在锁外**：`out` 自己有锁，而 `drop_batch` 是"先 out 再 batches"，
        // 两处顺序反过来就是死锁的料。
        let record = out_batch_of(batch, &[]);
        let emit = force || batch.last_emit.elapsed() >= REPORT_INTERVAL;
        if emit {
            batch.last_emit = Instant::now();
        }
        (emit.then(|| snapshot(batch)), record)
    };
    // 状态跃迁那一刻盘上必须有（哪怕这几拍的事件被节流了：`apply_push_states` 的
    // `Sending` 就是 `force = false`）。**LocalSend-only 的批次没有可恢复的 job**：
    // `out_batch_of` 会过滤掉它们，落一个空 targets 的批次既没用、又会每 200ms 写一次盘 ——
    // 所以只在真有 OrbDrop 目标时才写。
    if !out_record.targets.is_empty() {
        state.push.out.save_batch(out_record);
    }
    let Some(shot) = shot else {
        return; // 节流住的只是**事件**：既不发快照，也不做收尾（与改动前一致）
    };
    if let Ok(value) = serde_json::to_value(&shot) {
        ws::emit_ui(state, ws::events::ui::PUSH_BATCH, &value);
    }
    if shot.waiting == 0 {
        // 整批走完：**不立刻丢**。对端的回执可能还在路上（链路回来才对得上账），而
        // "本机推测的失败"要被纠正，就得先找得到这批 —— 留一段宽限期（`SETTLED_GRACE`），
        // 到点由 `sweep_settled_batches` 收掉。
        state.push.settle_batch(&batch_id);
    }
    // 源变化监视只在"还有台在等对方点头"时有意义：没人等了就卸掉（见 `transfer::watch`）。
    if !state.push.batch_has_invited(&batch_id) {
        crate::transfer::watch::unwatch(state, &batch_id);
    }
}

// ---------------------------------------------------------------------------
// 跨重启：落盘那份（`push_out`）与重建
// ---------------------------------------------------------------------------

/// `PushJob` → 落盘那份。`accepted` 由调用方给：建这条时是 `false`（对面还没开始拉），
/// 收到"同意"或第一条进度之后是 `true`（见 `push_out::OutJob::accepted`）。
///
/// `PushStatus` 不进那份：它是内存里的过程量，重建时按 `accepted` 与批次里那份进度拼回来。
pub fn out_job_of(job: &PushJob, accepted: bool) -> super::push_out::OutJob {
    super::push_out::OutJob {
        id: job.id.clone(),
        batch_id: job.batch_id.clone(),
        peer_id: job.peer_id.clone(),
        peer_name: job.peer_name.clone(),
        name: job.name.clone(),
        is_dir: job.is_dir,
        peer_paired: job.peer_paired,
        sources: job
            .sources
            .iter()
            .map(|(rel, abs)| (rel.clone(), abs.to_string_lossy().into_owned()))
            .collect(),
        accepted,
        updated_at: super::push_out::now_secs(),
    }
}

/// `PushBatch` → 落盘那份。`roots` 只有建这批的那一刻知道 —— 状态跃迁那一刻传空的，
/// `PushOutStore::save_batch` 会把盘上现有的那几项留着（见那里的注释）。
pub fn out_batch_of(batch: &PushBatch, roots: &[std::path::PathBuf]) -> super::push_out::OutBatch {
    super::push_out::OutBatch {
        id: batch.id.clone(),
        name: batch.name.clone(),
        count: batch.count,
        skipped: batch.skipped,
        per_target_total: batch.per_target_total,
        targets: batch
            .targets
            .iter()
            // LocalSend 目标没有可恢复的 `PushJob`（协议没有续传），不写进 `push_out`；
            // 混排批次里只持久化 OrbDrop 那几行。
            .filter(|t| t.transport == crate::config::PeerTransport::OrbDrop)
            .map(|t| super::push_out::OutTarget {
                peer_id: t.peer_id.clone(),
                peer_name: t.peer_name.clone(),
                invite_id: t.invite_id.clone(),
                paired: t.paired,
                state: t.state.key().to_string(),
                received: t.received,
                error: t.error.clone(),
            })
            .collect(),
        roots: roots
            .iter()
            .map(|p| p.to_string_lossy().into_owned())
            .collect(),
    }
}

/// 启动时把上次没走完的发送任务捡回来（`push_out.json`）。
///
/// **为什么只要这一份就够**：数据面是接收端主动来要块，发送端只要"取文件的凭据"还在就行 ——
/// 不用重发邀请、不用重问同意、也不碰协议（清单的身份不在这份里：`serve` 每次都现场 stat 源
/// 文件，接收端拿现场那份跟自己的清单对账）。
///
/// 规则：
/// - 批次照原样恢复（含还挂着的「等待对方同意」那一档：**对面那台可能还在跑**，它手上那张
///   邀请卡照样点得动 —— 恢复的 job 就是它来要块时能命中的那份凭据）；
/// - `accepted` 的那几条：邀请档强制升成「传输中」（对面明明已经在拉，别写「等待对方同意」）；
/// - 源监视按盘上那几项 `roots` 挂回去（"源一变就撤这次推送"这条承诺不能因为重启就没了）；
/// - 已经全是终态的批次不恢复（顺手把盘上那份也删掉）。
///
/// 返回恢复出来的任务条数（启动日志用）。
pub fn restore_outbound(state: &Arc<AppState>) -> usize {
    let (jobs, batches) = state.push.out.snapshot();
    if jobs.is_empty() && batches.is_empty() {
        return 0;
    }
    let accepted: HashMap<String, bool> = jobs.iter().map(|j| (j.id.clone(), j.accepted)).collect();
    // 每批的"每台字节总量"：恢复 job 的 `Downloading{received,total}` 要用它。
    let mut totals: HashMap<String, u64> = HashMap::new();

    for rec in batches {
        let mut targets: Vec<BatchTarget> = Vec::with_capacity(rec.targets.len());
        for t in &rec.targets {
            // 认不出来的档（以后加了新档又被降级读）当终态丢：宁可少一行，也不造一个假"还在等"。
            let Some(key) = TargetState::from_key(&t.state) else {
                warn!(invite = %t.invite_id, state = %t.state, "批次里有个认不出的状态，丢弃这一行");
                continue;
            };
            let is_accepted = accepted.get(&t.invite_id).copied().unwrap_or(false);
            targets.push(BatchTarget {
                peer_id: t.peer_id.clone(),
                peer_name: t.peer_name.clone(),
                invite_id: t.invite_id.clone(),
                transport: crate::config::PeerTransport::OrbDrop,
                paired: t.paired,
                // 对面已经点头、只是第一条进度还没到：别让恢复出来的卡片写「等待对方同意」。
                state: if key == TargetState::Invited && is_accepted {
                    TargetState::Sending
                } else {
                    key
                },
                received: t.received,
                error: t.error.clone(),
            });
        }
        if !targets.iter().any(|t| t.state.is_waiting()) {
            state.push.out.drop_batch(&rec.id);
            continue;
        }
        let batch = PushBatch {
            id: rec.id.clone(),
            name: rec.name,
            count: rec.count,
            skipped: rec.skipped,
            per_target_total: rec.per_target_total,
            targets,
            last_emit: Instant::now(),
        };
        totals.insert(batch.id.clone(), batch.per_target_total);
        let has_invited = batch
            .targets
            .iter()
            .any(|t| t.state == TargetState::Invited);
        state.push.insert_batch(batch);
        // 还有台在等对方点头 → 把源监视挂回去（不然"源变了就撤"这条只在重启前成立）。
        if has_invited {
            let roots: Vec<std::path::PathBuf> =
                rec.roots.iter().map(std::path::PathBuf::from).collect();
            if !roots.is_empty() {
                crate::transfer::watch::watch_sources(state, &rec.id, &roots);
            }
        }
    }

    let mut restored = 0usize;
    for rec in jobs {
        // 进度初值从批次那一行抄（落盘的粒度是"最后一次状态跃迁"，见 `push_out`）。
        let received = state
            .push
            .target_of_invite(&rec.id)
            .map(|t| t.received)
            .unwrap_or(0);
        let total = totals.get(&rec.batch_id).copied().unwrap_or(0);
        let status = match (rec.accepted, total) {
            (false, _) => PushStatus::Invited,
            (true, 0) => PushStatus::Accepted,
            (true, total) => PushStatus::Downloading { received, total },
        };
        let job = PushJob {
            id: rec.id.clone(),
            batch_id: rec.batch_id.clone(),
            peer_id: rec.peer_id.clone(),
            peer_name: rec.peer_name.clone(),
            name: rec.name.clone(),
            is_dir: rec.is_dir,
            sources: rec
                .sources
                .into_iter()
                .map(|(rel, abs)| (rel, std::path::PathBuf::from(abs)))
                .collect(),
            status,
            // `Instant` 落不了盘（也没人读它）→ 重启这一刻重新起算。
            created_at: Instant::now(),
            peer_paired: rec.peer_paired,
            cancel: Arc::new(AtomicBool::new(false)),
        };
        // 走 `insert_job`（**不碰盘**）：恢复不是"新建一次推送"，别把盘上那份的 `accepted`
        // 又写回 false（那会把"恢复成卡片"这件事弄丢）。
        state.push.insert_job(job);
        restored += 1;
    }
    if restored > 0 {
        info!(restored, "恢复了上次没走完的推送（对端来要块就能接着传）");
    }
    restored
}

/// 每一拍扫一遍"过期还没走完"的发送任务（发送端这一半的 TTL，见 `push_out::OUTBOUND_TTL_SECS`）。
///
/// 到期按**最后已知的进度**分两种收尾 —— 它们不是一回事：
/// - 已经报满 100%（最后一次进度就是全量）：**按「已送达」收尾**。完成回执是"三次尝试、尽力而为"
///   （`report_to_sender`），丢了就会走到这儿；把一条"对面把最后一个字节都报了"的推送说成
///   "对端一直没来续传"就是假话。
/// - 其余：按"对端一直没来续传，已放弃"落失败（复用既有收尾，卡片 3 秒后自己收）。
///
/// **它必须 ≥ 接收端那个窗口**（`resume::PENDING_TTL_SECS`），否则对面还在重试这边先答 410 了。
pub fn sweep_outbound(state: &AppState) {
    let now = super::push_out::now_secs();
    for (invite_id, batch_id) in state.push.out.expired(now) {
        let per_target = state.push.batch_per_target_total(&batch_id);
        let received = state
            .push
            .target_of_invite(&invite_id)
            .map(|t| t.received)
            .unwrap_or(0);
        if expired_target_state(received, per_target) == TargetState::Done {
            info!(%invite_id, "这条推送最后报的是全量，按已送达收尾");
            state.push.remove_job(&invite_id);
            batch_set(state, &invite_id, TargetState::Done, None, None, true);
            continue;
        }
        warn!(%invite_id, "这条推送等太久了（对端一直没来续传），放弃");
        // 放弃 = 这条邀请到此为止，**必须让对端知道**（与 F-54「任一端取消，另一端那张卡
        // 跟着收掉」同一条原则）：对面那条路上没有时钟（`await1`，见 `CAP_AWAIT`），本机不
        // 吭声的话它那张卡就一直挂着 —— 而卡里揣着它那份最大 64MB 的邀请清单。
        // 这是**发送方**这一侧，所以地址走地址簿（与 `cancel_push_with` 同一条路）；
        // 批次要**先**取 peer_id（`batch_set` 可能当场把走完的批次丢掉）。
        let peer_id = state.push.target_of_invite(&invite_id).map(|t| t.peer_id);
        state.push.remove_job(&invite_id);
        batch_set(
            state,
            &invite_id,
            TargetState::Failed,
            None,
            Some("对端一直没来续传，已放弃".into()),
            true,
        );
        if let Some(peer_id) = peer_id {
            super::notify_peer(
                state,
                &peer_id,
                WsEvent::PushDone {
                    invite_id,
                    status: "failed".into(),
                    reason: Some("发送方已放弃（你一直没来续传）".into()),
                },
            );
        }
    }
}

/// 过期那一条该落成什么（`sweep_outbound` 的判据，单拎出来是为了能直接测）：
/// **最后报满了全量 → 已送达**（完成回执是尽力而为的三次尝试，丢了就会走到这儿；
/// 把"对面把最后一个字节都报了"说成"对端一直没来续传"是假话）；否则 → 放弃。
fn expired_target_state(received: u64, per_target: u64) -> TargetState {
    if per_target > 0 && received >= per_target {
        TargetState::Done
    } else {
        TargetState::Failed
    }
}

// ---------------------------------------------------------------------------
// Reports back to the sender
// ---------------------------------------------------------------------------

/// 把一条推送状态回报给发送方（进度 / 完成 / 失败）。
///
/// 长连接在就**先试它**；"塞进队列"不等于"送出去了"（`ws::send` 只写进当前会话的 mpsc，
/// socket 写失败它并不知道），所以塞失败要**退回 HTTP**，而不是把这条回执丢掉 ——
/// 丢了这一条，发送方那台就会一直停在「等待」，而对面其实已经点头了 / 已经收完了。
/// HTTP 那一跳是最后一道兜底：**退避重试几次**，够覆盖对端重启/瞬时抖动。
/// 两条通道说的是同一个 `WsEvent`，发送方只有一份处理代码。
pub fn report_to_sender(
    state: &AppState,
    peer_id: &str,
    callback: &Option<(String, u16)>,
    event: WsEvent,
) {
    if state.ws.send(peer_id, event.clone()) {
        return;
    }
    let Some((ip, port)) = callback.clone() else {
        warn!(%peer_id, "推送回执无处可送：既没有长连接，也没有对方地址");
        return;
    };
    let url = format!("{}/push/report", crate::net::peer_base(&ip, port));
    // 认人：对端那边**已配对**时，这条回执必须带上它那枚 token（与它认我们的规则对称，
    // 见 `routes_push::authorize`）。手里有就带 —— 拿不到（未配对的推送）就照旧只靠邀请 id，
    // 那是"未绑定也能推"的前提。丢下这一枚的后果是回执被 401 掉，发送方一直把一次
    // 成功的传输写成「等待」（假账）。
    let token = crate::credential::get_peer_token(peer_id)
        .ok()
        .flatten()
        .filter(|t| !t.is_empty());
    // 带 token 的拨号按目标建表：同一地址上另一台已配对设备不能顶替它收走回执。
    let paired = state.get_peer(peer_id).is_some_and(|p| p.paired);
    let client = crate::net::dial_client(&ip, &crate::credential::dial_pin(peer_id, paired));
    crate::task::spawn("push report", async move {
        for (attempt, delay) in [
            Duration::ZERO,
            Duration::from_secs(1),
            Duration::from_secs(3),
        ]
        .into_iter()
        .enumerate()
        {
            if !delay.is_zero() {
                sleep(delay).await;
            }
            let mut req = client.post(&url).json(&event);
            if let Some(token) = token.as_deref() {
                req = req.bearer_auth(token);
            }
            match req.send().await {
                Ok(resp) if resp.status().is_success() => return,
                Ok(resp) => warn!(%url, attempt, status = %resp.status(), "推送回执被拒"),
                Err(e) => warn!(%url, attempt, %e, "推送回执发送失败"),
            }
        }
        // 三次都没成：这条回执就真没了。发送方那边有对账（`PushAsk`）兜底 ——
        // 链路回来时它会问一遍，接收方的结局记录会给出真实答案。
        error!(%url, "推送回执三次都发不出去，等发送方对账");
    });
}

// ---------------------------------------------------------------------------
// Receiver-side download loop
// ---------------------------------------------------------------------------

/// 接收方：开始把对端的文件拉下来（F-52 / F-60）。
///
/// 两条数据面在这里分派（见 `InboundInvite::supports_stream`）：
/// - 对端声明了 `stream1` → 新的分帧并发引擎（`transfer::download`）：块级并行 + 小文件打包；
/// - 否则 → 老的逐文件串行循环（`spawn_download_legacy`），一字未改。
///
/// 认人的前置检查（token、已配对必须带 token）两条路共用，所以放在分派**之前**。
pub fn spawn_download(state: Arc<AppState>, invite: InboundInvite, peer_id: String) {
    let invite_id = invite.invite_id.clone();
    let callback = state.dial_endpoint(&peer_id, invite.callback.clone());

    // "同意"这一条回执：带 `await1` 的对端要它（它的邀请投递时只回了"送到了、等着呢"，
    // 用户点的结果得有这么一条过去），长连接在就先走长连接，否则 POST 它的 `/push/report`。
    // 老对端照旧只走长连接 —— HTTP 那条路的决定是它自己那条同步响应（oneshot）。
    let accepted = WsEvent::PushResponse {
        invite_id: invite_id.clone(),
        accepted: true,
        reason: None,
    };
    if invite.supports_await() {
        report_to_sender(&state, &peer_id, &callback, accepted);
    } else if state.ws.has_connection(&peer_id) {
        ws::send_to_peer(&state, &peer_id, accepted);
    }

    // 下载要认人：已配对的对端必须带自己那枚 token；未绑定的推送没有 token 可带，
    // 下载端点就只认邀请 id（那个 id 只在这条邀请里出现过）。
    let token = match credential::get_peer_token(&peer_id) {
        Ok(t) => t,
        Err(e) => {
            fail_download(
                &state,
                &peer_id,
                &callback,
                &invite_id,
                format!("读取凭据失败: {e}"),
            );
            return;
        }
    };
    let paired = state.get_peer(&peer_id).map(|p| p.paired).unwrap_or(false);
    if paired && token.is_none() {
        fail_download(
            &state,
            &peer_id,
            &callback,
            &invite_id,
            "本机没有对端 token".into(),
        );
        return;
    }

    // 暂停闸在这一刻就登记（不是等任务跑起来）：界面那条消息一变成"接收中"，
    // 用户点右侧圆环就该找得到它。表项跟着 `ReceiveGate` 活，见它的说明。
    let control = Arc::new(super::browse_dl::DownloadControl::default());
    state
        .receive_downloads
        .lock()
        .insert(invite_id.clone(), control.clone());
    let gate = ReceiveGate::new(state.clone(), invite_id.clone());

    if invite.supports_stream() {
        super::download::spawn(state, invite, peer_id, token, control, gate);
        return;
    }
    spawn_download_legacy(state, invite, peer_id, token, control, gate);
}

/// 一个接收任务的暂停闸表项：**寿命 = 任务的寿命**。
///
/// 任务以任何方式退出（完成 / 失败 / 对端取消 / 本机取消）都会把 `AppState::receive_downloads`
/// 里那一项摘掉 —— 界面之后点圆环会拿到 `false`（"这条接收已经不在跑"），而不是对着一个
/// 早已没人等的闸门点头。`cancel_receive` 也会先摘表并**放开闸门**：暂停中的 worker 正等
/// 在 `wait_for` 上，不放它就永远醒不过来（表摘了、任务还在，比留着更难查）。
pub(crate) struct ReceiveGate {
    state: Arc<AppState>,
    invite_id: String,
}

impl ReceiveGate {
    fn new(state: Arc<AppState>, invite_id: String) -> Self {
        Self { state, invite_id }
    }
}

impl Drop for ReceiveGate {
    fn drop(&mut self) {
        self.state.receive_downloads.lock().remove(&self.invite_id);
    }
}

/// 老数据面：`GET /push/download`，一次一个文件、整份流、串行。
///
/// 保留它的唯一理由是**与老版本互通**（对端不在 `caps` 里声明 `stream1`）。新对端一律走
/// `transfer::download`：那条路把"每个文件的固定开销"摊掉了（批 + 并行）。
fn spawn_download_legacy(
    state: Arc<AppState>,
    invite: InboundInvite,
    peer_id: String,
    token: Option<String>,
    control: Arc<super::browse_dl::DownloadControl>,
    gate: ReceiveGate,
) {
    use futures_util::StreamExt;
    use tokio::io::{AsyncSeekExt, AsyncWriteExt};

    let invite_id = invite.invite_id.clone();
    let total_bytes = invite.total_bytes();
    let callback = state.dial_endpoint(&peer_id, invite.callback.clone());

    // 下载跑在运行时上：「同意接收」是 async 命令（`task::spawn` 另有"没有 reactor 就退到
    // Tauri 全局运行时"的兜底，见 task.rs）—— 这条任务绝不能被无声丢掉，否则两边都停在 0%。
    crate::task::spawn("push download", async move {
        // 暂停闸表项跟着这条任务活（完成 / 失败 / 取消都摘掉，见 `ReceiveGate`）。
        let _gate = gate;
        // 带 token 的拨号按目标建表：同一地址上另一台已配对设备不能顶替它。
        let paired = state.get_peer(&peer_id).is_some_and(|p| p.paired);
        let dial_host = callback
            .as_ref()
            .map(|(ip, _)| ip.clone())
            .unwrap_or_default();
        let client = crate::net::dial_client_streaming(
            &dial_host,
            &crate::credential::dial_pin(&peer_id, paired),
            Duration::from_secs(5),
            Duration::from_secs(30),
        );
        let config = state.config.lock().clone();
        // 落盘根 = `<落盘目录>/<发送方名>`（S-12 那颗开关关掉时就是落盘根本身；两条引擎共用
        // `resume::landing_dir`）。落盘目录默认是用户设的下载目录（S-01）；只有在
        // 「每次选择位置」下由用户亲自挑过的那些邀请才带自己的根（`invite.root`）——
        // 自动接收（已配对 + 开着「已配对设备自动接收」）不会有这个根。推送接收不再单独
        // 有「收件目录」。
        //
        // 对端只往落盘根下面写 —— **没有"推送名子目录"**：清单里的相对路径就是落盘后
        // 的样子（单文件 `A.txt`、单目录 `D/…`、多选 `甲目录/…` + `散文件.txt`），
        // 发送方在 `manifest_for_paths` 里已经把每个源的名字放进路径了。
        //
        // 名字必须先净化：它是报文里的自由字符串，`C:\Windows\Temp`、`..\..\..\Users\Public`
        // 之类会把基址顶掉（`PathBuf::push` 遇到带前缀的段会清空已累积的部分）。净化 +
        // `safe_join` 两道闸都在 `landing_root` 里（关掉那层时 `safe_join` 仍是唯一的围栏）。
        let base = invite
            .root
            .as_deref()
            .unwrap_or(config.download_dir.as_path());
        let sender_dir =
            super::resume::landing_dir(base, &invite.sender.name, config.device_subdir);
        let _ = tokio::fs::create_dir_all(&sender_dir).await;

        let mut total_received: u64 = 0;
        let mut last_report: Option<Instant> = None;
        // 已经建过的目录（见下面 `made_dirs.insert`）：目录嵌套深、文件多时，
        // "每文件问一次系统"是纯浪费。
        let mut made_dirs: HashSet<PathBuf> = HashSet::new();
        // 清单：目录推送就是清单本身，单文件推送那条路上对端只有名字与大小（合成的规则只在
        // `InboundInvite::manifest` 一处 —— 终结清理要按**同一份**清单判断"哪一个是我们写了一半的"）。
        let files = invite.manifest();
        // (相对路径, 总大小)：终结清理的判据（见 `discard_legacy`），循环里反复用。
        let sizes: Vec<(&str, u64)> = files.iter().map(|f| (f.path.as_str(), f.size)).collect();

        // 这一轮在本机这边**终结**（写不进去 / 对端撤了 / 卡住不动）：先松手，再把手里这个
        // 文件和这条邀请那份续传记录一起收掉，最后照原样报终态。
        //
        // 句柄按值传进来（这里 `drop`）：**Windows 上打开着的文件删不掉**（共享模式不含
        // `FILE_SHARE_DELETE` 时就是 ERROR_SHARING_VIOLATION），攥着句柄去删 = 清理白做。
        // 不能绕开它直接调 `fail_download`：记录会被清掉，那之后这半截文件就再没有第二次机会
        // （见 `discard_legacy` 那段判据）。
        let fail_file = |fh: tokio::fs::File, target: &std::path::Path, reason: String| {
            drop(fh);
            discard_legacy(&state, &invite_id, &sender_dir, &sizes, Some(target));
            fail_download(&state, &peer_id, &callback, &invite_id, reason);
        };
        // 用户取消（邀请被取走）：只收掉半截文件与记录，**不再回报** —— `cancel_receive`
        // 已经把话带给发送方了。放在闸门前后两处都能调（暂停期间被取消也要收干净）。
        let cancel_file = |fh: tokio::fs::File, target: &std::path::Path| {
            drop(fh);
            discard_legacy(&state, &invite_id, &sender_dir, &sizes, Some(target));
        };

        let base_url = format!(
            "{}/push/download",
            crate::net::peer_base(
                callback
                    .as_ref()
                    .map(|(ip, _)| ip.as_str())
                    .unwrap_or_default(),
                callback.as_ref().map(|(_, port)| *port).unwrap_or(0),
            )
        );

        for file in &files {
            let file_target = match super::resume::safe_join(&sender_dir, &file.path) {
                Some(p) => p,
                None => {
                    fail_download(
                        &state,
                        &peer_id,
                        &callback,
                        &invite_id,
                        format!("非法路径: {}", file.path),
                    );
                    return;
                }
            };
            if let Some(p) = file_target.parent() {
                // 已建过的目录不再问系统一遍：`create_dir_all` 对**存在**的目录照样要逐段
                // 打开+查询（Windows 上每段一次系统调用），2 万文件的目录就是 2 万次白干。
                if made_dirs.insert(p.to_path_buf()) {
                    let _ = tokio::fs::create_dir_all(p).await;
                }
            }

            let mut offset = state.push.resume.load(&invite_id, &file.path);
            let mut last_saved = offset;
            let mut last_persist_time = Instant::now();
            // 8MB / 5s 一拍：`save` 自身是节流落盘的（见 `ResumeStore::save`），这个粒度
            // 决定的是"崩溃后最多重传多少"，不再是磁盘写入次数。
            const PERSIST_CHUNK: u64 = 8 * 1024 * 1024;
            const PERSIST_INTERVAL: Duration = Duration::from_secs(5);
            // 连续几次"一个字节都没拿到"就判这条文件失败（见循环尾部的守卫）。
            const MAX_STALLS: u32 = 3;
            let mut stalled = 0u32;

            // 句柄**一次开到底**：续传时它跨多次尝试复用，收尾那次 `sync_data` 也用它，
            // 不必为了确认落盘再开一次。
            let mut fh = match tokio::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .open(&file_target)
                .await
            {
                Ok(f) => f,
                Err(e) => {
                    fail_download(
                        &state,
                        &peer_id,
                        &callback,
                        &invite_id,
                        format!("打开文件失败: {e}"),
                    );
                    return;
                }
            };
            // 记录长度以外的尾巴一律截掉：上次可能崩在"字节写完"与"进度记上"之间，
            // 那截尾巴没被记账，留着只会卡在文件中间当垃圾。
            if fh.metadata().await.map(|m| m.len()).unwrap_or(0) > offset {
                let _ = fh.set_len(offset).await;
            }

            'file: loop {
                if state.push.inbound.lock().get(&invite_id).is_none() {
                    // 邀请被取走了 = 用户点了取消（或拒绝）—— 话已经由 `cancel_receive` 带过去了，
                    // 这里静静收手。**但半截文件要收**：手里这个文件正在写（记录可能还没记上），
                    // 而记录键是邀请 id、取消之后这条邀请不会再被接手（`drop_pending` 已经在
                    // `cancel_receive` 里做了），重推一次又是新 id —— 留着它，用户目录里就多一个
                    // "名字对、内容截断"的假文件，而且永远没人清（见 `discard_legacy`）。
                    info!(%invite_id, "download cancelled");
                    cancel_file(fh, &file_target);
                    return;
                }
                // 这一份已经收完就不必再等闸门：用户恰好在收官时按暂停，不该把整条任务
                // 卡在"最后一块已经落完、只差报终态"之前。
                if offset >= file.size {
                    break;
                }
                // 界面点圆环暂停：停在闸门前，一个字节都不再向对端要。已经写进文件/临时文件
                // 的字节与 `offset` 都不动，继续之后从当前位置接着要（与浏览下载同一条语义）。
                control.wait_if_paused().await;
                // 等闸门期间可能被取消：`cancel_receive` 会先放开闸门把人叫醒，这里补看一眼。
                if state.push.inbound.lock().get(&invite_id).is_none() {
                    info!(%invite_id, "download cancelled while paused");
                    cancel_file(fh, &file_target);
                    return;
                }

                let mut req = client.get(&base_url).query(&[
                    ("id", invite_id.as_str()),
                    ("path", file.path.as_str()),
                    ("offset", &offset.to_string()),
                ]);
                if let Some(t) = &token {
                    req = req.bearer_auth(t);
                }
                let resp = match req.send().await {
                    Ok(r) if r.status().as_u16() == 410 => {
                        info!(%invite_id, %file.path, "cancelled by sender");
                        // 对端撤了 / 它那台重启过（`SENDER_GONE` 的语义）：**确定不会再续**，
                        // 手里这个文件与这条邀请的续传记录一起收掉，再照原样报终态。
                        // （原来这里单独一句 `resume.clear`：清记录与删它指着的文件不能分家，
                        // 见 `discard_legacy`，所以并到一条路上来。）
                        fail_file(fh, &file_target, super::SENDER_GONE.into());
                        return;
                    }
                    Ok(r) if r.status().as_u16() == 409 => {
                        // 源文件在建清单之后变小/被换掉了（对端语义见
                        // `routes_push::push_download_handler`）。这是明确的错误，
                        // 不能拿一个空包当"传完了"。
                        fail_file(
                            fh,
                            &file_target,
                            format!("源文件在传输中被修改: {}", file.path),
                        );
                        return;
                    }
                    Ok(r) if !r.status().is_success() => {
                        error!(%invite_id, status = %r.status(), "download failed");
                        sleep(Duration::from_secs(2)).await;
                        continue;
                    }
                    Ok(r) => r,
                    Err(e) => {
                        warn!(%invite_id, %e, "download error, retry");
                        sleep(Duration::from_secs(2)).await;
                        continue;
                    }
                };

                if let Err(e) = fh.seek(std::io::SeekFrom::Start(offset)).await {
                    fail_file(fh, &file_target, format!("定位写入位置失败: {e}"));
                    return;
                }
                let attempt_start = offset;
                // 一次尝试一个缓冲：客户端交给我们的块常常只有十几 KB（TLS 记录的粒度），
                // 逐块写就是每 16KB 一次阻塞任务派发；256KB 缓冲把写系统调用除十几倍。
                let mut out =
                    tokio::io::BufWriter::with_capacity(crate::net::STREAM_CHUNK, &mut fh);

                let mut stream = resp.bytes_stream();
                loop {
                    let Some(chunk) = stream.next().await else {
                        break;
                    };
                    let chunk = match chunk {
                        Ok(c) => c,
                        Err(e) => {
                            // 发送方中途取消时，流会就地断掉：退回外层重试，
                            // 下一次请求就会拿到 410，按"对端不再提供这条推送"收尾
                            // （`SENDER_GONE`：撤销与"它那台重启过"在线上分不出来）。
                            //
                            // **先把缓冲交出去再退**：`offset` 已经把缓冲里的字节算进去了，
                            // 丢掉它们就等于"以为收过、其实没写"（后续续传直接从洞后面接着写）。
                            let _ = out.flush().await;
                            warn!(%invite_id, %e, "stream error");
                            continue 'file;
                        }
                    };
                    // 老协议一次请求把整个文件发完：只在请求边界等的话，大文件要一直收完
                    // 才能暂停 —— 这里逐块看一眼。**放在读到这一块之后**：流到了 EOF 直接
                    // break，不会因为"暂停"把收尾卡住。暂停时先把缓冲交给盘（`offset` 已经
                    // 把这些字节算进去了），再停在闸门前；恢复后照常把这一块写进去。
                    if control.is_paused() {
                        if let Err(e) = out.flush().await {
                            drop(out);
                            fail_file(fh, &file_target, format!("写入失败: {e}"));
                            return;
                        }
                        control.wait_if_paused().await;
                    }
                    // 清单里的 `size` 是这一批的契约：正常对端只发 `size - offset` 字节。
                    // 多发出来的字节只可能来自"清单生成后源文件变大"或恶意对端 —— 照单全收
                    // 会写出比清单长的文件、还把它判成完成。宁可判失败，也不落一份长度错的。
                    if offset.saturating_add(chunk.len() as u64) > file.size {
                        drop(out);
                        fail_file(
                            fh,
                            &file_target,
                            format!("源文件在传输中被修改（比清单大）: {}", file.path),
                        );
                        return;
                    }
                    if let Err(e) = out.write_all(&chunk).await {
                        // 缓冲里那些字节本来就是"写不进去"的那一批，直接丢（文件要删）——
                        // 但得先把它对 `fh` 的借用交出来。
                        drop(out);
                        fail_file(fh, &file_target, format!("写入失败: {e}"));
                        return;
                    }
                    offset += chunk.len() as u64;
                    total_received += chunk.len() as u64;

                    if offset - last_saved >= PERSIST_CHUNK
                        || Instant::now().duration_since(last_persist_time) >= PERSIST_INTERVAL
                    {
                        // **先落盘、再记账**：记录里的 offset 必须是"盘上真有这些字节"。
                        // 缓冲里留着的字节不算数 —— 不 flush 就写记录，崩溃之后续传会从
                        // 一堆没写进去的"空位"后面接着写，最后交出一个长度对、内容是坏的
                        // 文件（正是"传过来是损坏的"那条红线）。
                        if let Err(e) = out.flush().await {
                            drop(out);
                            fail_file(fh, &file_target, format!("写入失败: {e}"));
                            return;
                        }
                        let _ = out.get_mut().sync_data().await;
                        state.push.resume.save(&invite_id, &file.path, offset);
                        last_persist_time = Instant::now();
                        last_saved = offset;
                    }

                    // 进度：回报发送方 + 推进本机那张接收卡片。
                    //
                    // 统一按 `REPORT_INTERVAL` 节流 —— 传输本身不再按块休眠（那是以前唯一
                    // 的"节流"，代价是把速度按死在几十 KB/s），所以这里必须自己限速：
                    // 不限的话一个 GB 级文件会以每秒上千条的频率刷事件。
                    let now = Instant::now();
                    if last_report.map_or(true, |t| now.duration_since(t) >= REPORT_INTERVAL) {
                        last_report = Some(now);
                        report_to_sender(
                            &state,
                            &peer_id,
                            &callback,
                            WsEvent::PushProgress {
                                invite_id: invite_id.clone(),
                                received: total_received,
                                total: total_bytes,
                            },
                        );
                        ws::emit_ui(
                            &state,
                            ws::events::ui::PUSH_PROGRESS,
                            &serde_json::json!({
                                "inviteId": invite_id,
                                "received": total_received,
                                "total": total_bytes,
                            }),
                        );
                    }
                }
                if let Err(e) = out.flush().await {
                    drop(out);
                    fail_file(fh, &file_target, format!("写入失败: {e}"));
                    return;
                }
                drop(out);

                // 一次尝试零字节：等一拍再来；连续 `MAX_STALLS` 次判这条邀请失败。
                //
                // 没有这道守卫，只要"对端能回 200 空包"而清单又说还有字节，两边就会进入
                // "狂发请求 / 狂回空包"的死循环 —— 谁也不报错，用户只看到永远停在某个百分比。
                if offset == attempt_start {
                    stalled += 1;
                    if stalled >= MAX_STALLS {
                        fail_file(
                            fh,
                            &file_target,
                            format!("传输没有进展（对端没有数据）: {}", file.path),
                        );
                        return;
                    }
                    sleep(Duration::from_secs(2)).await;
                } else {
                    stalled = 0;
                }
            }
            // 这个文件收完了：把最后的字节确认到盘上，再记终点（同上一条纪律）。
            let _ = fh.sync_data().await;
            state.push.resume.save(&invite_id, &file.path, file.size);
        }

        state.push.resume.flush();
        state.push.resume.clear(&invite_id);
        // 传完了：这条不用再接手了（重启后不该再来一遍）。
        state.push.resume.drop_pending(&invite_id);
        // 结局**先**落地、再摘邀请：对账可能正好撞在这一瞬（邀请已摘、记录还没写），
        // 那种窗口里答出去的是 `unknown`（"没送到"）——事实是"收到了"。
        state
            .push
            .record_outcome(&invite_id, &peer_id, "done", total_bytes, None);
        state.push.remove_invite(&invite_id);
        report_to_sender(
            &state,
            &peer_id,
            &callback,
            WsEvent::PushDone {
                invite_id: invite_id.clone(),
                status: "completed".into(),
                reason: None,
            },
        );
        ws::emit_ui(
            &state,
            ws::events::ui::PUSH_DONE,
            &serde_json::json!({
                "inviteId": invite_id,
                "status": "completed",
                // 落盘位置（<下载目录>/<发送方名>）：界面那条"已收到"的长通知要告诉用户
                // 东西到底在哪 —— 没有这个，通知只能说"收到了"，点完「知道了」就再也找不到。
                "dest": sender_dir.to_string_lossy(),
            }),
        );
        info!(%invite_id, "push download completed");
    });
}

/// 记录里这一项是不是**这次写了一半**的那个文件（`discard_legacy` 的判据，单拎出来是为了能直接测）。
///
/// `0 < offset < size`：写过（记录只在"字节确实落盘"之后才写，见 `ResumeStore::save`）、而且没写完。
/// 两侧的边界都不能算：
/// - `offset == 0` = 这一轮还没碰过的路径，盘上同名文件可能是用户自己的、或上一次收完的；
/// - `offset >= size` = 已经收完的（用户要的就是它）。
fn is_half_written(offset: u64, size: u64) -> bool {
    offset != 0 && offset < size
}

/// 一条**老协议**推送在本机**终结**（失败 / 用户取消 / 对端撤了）时的收尾清理。
///
/// 做两件事，**必须一起做**（分家就等于丢一半）：
/// - 删掉这次写出来的**半截文件**：`current` = 这一轮手里那个（它的记录可能还没记上 ——
///   8MB/5s 才记一次），再加上续传记录里 `0 < offset < size` 的那些。记录只在"字节确实落盘
///   之后"才写（见 `ResumeStore::save`），所以表里有它 = **本机动过**那个文件、而且没写完；
/// - 清掉这条邀请那份按**邀请 id** 存的续传记录（`ResumeStore::clear`，时间戳一并删）。
///
/// 为什么"保留"没有意义（这是与"重推接着传"那句注释的正面冲突，结论写在下面）：
/// 老协议的记录键是**邀请 id**，而用户"再推一次"拿到的是**新**邀请 id（`push_paths` 每次新造
/// 一个）；自动续传（`resume_pending`）用的又是**同一个** id。也就是说"接着传"只能靠"这份记录
/// + 这条邀请还活着"。一到终结态（`fail_download` 会 `drop_pending`、用户取消也是），这条记录
/// 这辈子不会有人再查：留着它，盘上那个**写在最终路径上**的半截文件就永远没人清 ——
/// 用户看到的是"名字对、内容截断"的假文件，点开就是坏内容（磁盘也一直占着）。
/// 新协议不受影响：那一路写 `.part`，记录在 `files` 里（键是文件身份、跨邀请复用），这里查不到。
///
/// 判据的**边界**（为什么不无脑删、为什么不用别的口径）：
/// - `offset == 0` 的路径不动：那是这一轮**还没碰过**的，盘上同名文件可能是用户自己的、或
///   上一次收完的完整文件 —— 删它才是真丢数据；
/// - `offset >= size` 的不动：那是已经收完的（用户要的就是它）；
/// - 只看"记录里有没有 + 有没有写完"，不看文件长度：长度分不出"我们写了一半"与"用户本来就
///   有这么个文件"，而记录是本机自己写的账。
///
/// **已知的边角**（不改口径的理由）：同一个落盘根上两条邀请同时写**同一个相对路径**时
/// （同一个发送方先后推两次、或关掉「按设备分目录」时两个同名设备推同名文件，再加"重启后
/// 接着收的那条撞上新推的一条"），这里会把对面那条的文件一起删掉。两条邀请写一个路径本来就是
/// 老协议没管过的情况（各写各的、最终内容"谁最后写完算谁的"），而**正常路径（一台对端、
/// 一次推送）碰不到**；要在这一层堵住，得先过滤"同一个根上还有谁在写哪些路径"，那是邀请那一层
/// 该做的事（去重），不是终结清理该猜的事。
pub(super) fn discard_legacy(
    state: &AppState,
    invite_id: &str,
    base: &std::path::Path,
    files: &[(&str, u64)],
    current: Option<&std::path::Path>,
) {
    if let Some(target) = current {
        let _ = std::fs::remove_file(target);
    }
    for &(rel, size) in files {
        let offset = state.push.resume.load(invite_id, rel);
        if !is_half_written(offset, size) {
            continue;
        }
        if let Some(target) = super::resume::safe_join(base, rel) {
            let _ = std::fs::remove_file(target);
        }
    }
    state.push.resume.clear(invite_id);
}

/// `fail_download` 的收尾清理：按**还挂着的邀请**算出落盘根与清单，交给
/// `transfer::discard_legacy_receive`。
///
/// 邀请已经不在了的那条路（`resume_pending` 判过期）由调用方自己清 —— 见 `resume_pending`。
fn discard_legacy_of_invite(state: &AppState, invite_id: &str) {
    let (sender_name, root, files) = {
        let inbound = state.push.inbound.lock();
        let Some(inv) = inbound.get(invite_id) else {
            return;
        };
        (inv.sender.name.clone(), inv.root.clone(), inv.manifest())
    };
    let sizes: Vec<(&str, u64)> = files.iter().map(|f| (f.path.as_str(), f.size)).collect();
    super::discard_legacy_receive(state, invite_id, &sender_name, root.as_deref(), &sizes);
}

/// 接收失败：告诉发送方，并让本机的接收卡片变成失败态。
///
/// **终态**（这条推送到此为止）：对端撤了（410）、源文件与清单对不上、用户取消/拒绝、
/// 待续传任务过期。落到这里就会把待续传任务删掉 —— 重启之后不会再自己接着收。
/// 中间态那次失败走 `fail_attempt`（保留邀请与任务，退避重试）。
///
/// `pub(super)`：新协议那条路（`transfer::download`）的会话级失败也走它 ——
/// 两条路对发送方说的话必须一模一样，不能各写一套。
pub(super) fn fail_download(
    state: &AppState,
    peer_id: &str,
    callback: &Option<(String, u16)>,
    invite_id: &str,
    reason: String,
) {
    error!(%invite_id, %reason, "push download failed");
    // 终态 = 不再接手这条：把待续传任务删掉（文件级位图**留着**，用户手动重推一次仍然省得下）。
    state.push.resume.drop_pending(invite_id);
    // 老协议那条路把半截文件写在**最终路径**上（新协议写 `.part`，归 `sweep_stale`/`finalize` 管）：
    // 这条推送到此为止，就把它连同那份按邀请 id 存的续传记录一起收掉（判据见 `discard_legacy`）。
    // 落盘根与清单从**还挂着的邀请**推出来（跑到这儿的都是"这一轮在跑"的邀请）；已经不在了的
    // 那条路（`resume_pending` 判过期）由它自己清，见 `transfer::resume_pending`。
    discard_legacy_of_invite(state, invite_id);
    // 同"完成"那条：先落结局再摘邀请（对账撞在中间那一瞬不该答 `unknown`）。
    //
    // 顺带把**文件级**那份记录落盘（`resume.json` 的 `files`，新协议按文件身份存的块位图）：
    // 失败之后留下的它才是"传失败再推一次接着传"的依据 —— 老协议那份按邀请 id 存的记录
    // 刚在上面被清掉了（它只对"同一条邀请继续续传"有用，见 `discard_legacy`）。光在内存里
    // 记着，进程一退就白记了。
    state.push.resume.flush();
    state
        .push
        .record_outcome(invite_id, peer_id, "failed", 0, Some(reason.clone()));
    state.push.remove_invite(invite_id);
    report_to_sender(
        state,
        peer_id,
        callback,
        WsEvent::PushDone {
            invite_id: invite_id.to_string(),
            status: "failed".into(),
            reason: Some(reason.clone()),
        },
    );
    ws::emit_ui(
        state,
        ws::events::ui::PUSH_DONE,
        &serde_json::json!({
            "inviteId": invite_id,
            "status": "failed",
            "reason": reason,
        }),
    );
}

/// **这一轮没成，但这条推送还能再试**：保留邀请与待续传任务，**不报终态**。
///
/// 为什么不报：报了 `PushDone{failed}`，接收端那张卡变成「接收失败」、发送端那张卡也被落成
/// 失败 —— 而失败是"只落一次"的终态（`late_success` 只认对端权威的 `Done`/`Rejected`），
/// 下一轮重试把字节接上时它回不来了。所以这里只留一行日志：
/// - 发送方对账问到时，本机答 `waiting`（邀请还在 `inbound` 里，见 `answer_invite_state`）；
/// - 两端界面都停在最后的进度上，等重试真的接上，或者由 `fail_download` 给个说法。
///
/// 邀请要从 `inbound` 里摘掉：它是"这一轮正在跑"的标志（`transfer::resume_pending` 拿它当
/// 闸门），留着下一轮就起不来了 —— 而重试的资格在**待续传任务**那一份里，不在邀请里。
///
/// 摘了邀请还得替它回答对账：**这条推送本机还拿着、正准备接着收**（`waiting`）。不写这一条，
/// `answer_invite_state` 会答 `unknown`（"没送到"），发送方那边就会把一条还在重试的推送
/// 记成失败 —— 而失败是它那份记录里的终态，重试成功也纠正不回来（见 `late_success`）。
pub(super) fn fail_attempt(state: &AppState, peer_id: &str, invite_id: &str, reason: String) {
    warn!(%invite_id, %reason, "这次接收没成，推送还在，等下一轮重试");
    state.push.remove_invite(invite_id);
    state
        .push
        .record_outcome(invite_id, peer_id, "waiting", 0, None);
    state.push.resume.flush();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn target(peer: &str, invite: &str, state: TargetState, received: u64) -> BatchTarget {
        BatchTarget {
            peer_id: peer.into(),
            peer_name: peer.into(),
            invite_id: invite.into(),
            transport: crate::config::PeerTransport::OrbDrop,
            paired: true,
            state,
            received,
            error: None,
        }
    }

    fn batch(targets: Vec<BatchTarget>) -> PushBatch {
        PushBatch {
            id: "b1".into(),
            name: "报告".into(),
            count: 2,
            skipped: 0,
            per_target_total: 10 * 1024 * 1024,
            targets,
            last_emit: Instant::now(),
        }
    }

    /// 过期的发送任务按**最后已知的进度**分两种收尾：报满了 = 已送达（完成回执丢了也会走到这儿），
    /// 否则才是"对端一直没来续传"。这条判据不能混：混了就是把一次成功的传输报成失败。
    #[test]
    fn an_expired_job_with_full_progress_lands_as_done() {
        let full = 10 * 1024 * 1024;
        assert_eq!(
            expired_target_state(full, full),
            TargetState::Done,
            "最后报的是全量"
        );
        assert_eq!(
            expired_target_state(full + 1, full),
            TargetState::Done,
            "报多了也算满"
        );
        assert_eq!(
            expired_target_state(full - 1, full),
            TargetState::Failed,
            "还差一块 = 放弃"
        );
        assert_eq!(
            expired_target_state(0, full),
            TargetState::Failed,
            "一个字节都没来过"
        );
        assert_eq!(
            expired_target_state(0, 0),
            TargetState::Failed,
            "总量都不知道（没批次）"
        );
    }

    /// 终结清理（`discard_legacy`）只看记录里那一条的 offset 与清单里的 size：
    /// `0 < offset < size` 才是"写过、没写完"。**两侧边界都要钉住** —— 判宽了删掉的是用户的文件
    /// 或已经收完的文件（真丢数据），判窄了就是半截文件永远留着。
    #[test]
    fn only_half_written_files_are_discarded() {
        assert!(is_half_written(1, 2), "写了一个字节、还差一个");
        assert!(is_half_written(1024, 10 * 1024 * 1024));
        assert!(
            !is_half_written(0, 10 * 1024 * 1024),
            "offset 0 = 这一轮还没碰过：盘上同名文件可能是用户自己的，删它才是真丢数据"
        );
        assert!(
            !is_half_written(10 * 1024 * 1024, 10 * 1024 * 1024),
            "已经收完的留着 —— 用户要的就是它"
        );
        assert!(!is_half_written(0, 0), "空文件、且没写过");
        assert!(
            !is_half_written(2, 1),
            "记录比清单还大（换过源）：不当成半截文件"
        );
    }

    /// 快照就是界面那张卡片的全部输入，所以它的**形状**（camelCase 字段名、state 字符串）
    /// 是一份必须钉住的契约：字段一改名，卡片的标题/计数/每行文案就静默变空。
    #[test]
    fn snapshot_shape_and_counts_are_the_card_contract() {
        let value = serde_json::to_value(snapshot(&batch(vec![
            target("PC-A", "i1", TargetState::Done, 10 * 1024 * 1024),
            target("PC-B", "i2", TargetState::Sending, 3 * 1024 * 1024),
            target("PC-C", "i3", TargetState::Failed, 0),
        ])))
        .unwrap();
        assert_eq!(
            value,
            serde_json::json!({
                "batchId": "b1",
                "name": "报告",
                "count": 2,
                // 清单里被跳过几项（链接/读不到）：卡片上那句"已跳过 N 项"读的就是它。
                "skipped": 0,
                // 成功 1 · 等待 1 · 失败 1 —— 卡片上那两个实时数字就是它们。
                "done": 1,
                "waiting": 1,
                "failed": 1,
                // 聚合进度：分子是各台已收之和，分母**只算还活着的那两台**（失败那台整条不计），
                // 所以有台失败时进度条也能走到 100%。
                "received": 13 * 1024 * 1024,
                "total": 20 * 1024 * 1024,
                "targets": [
                    { "peerId": "PC-A", "target": "PC-A", "inviteId": "i1", "paired": true,
                      "transport": "orbdrop", "state": "done", "received": 10 * 1024 * 1024, "error": null },
                    { "peerId": "PC-B", "target": "PC-B", "inviteId": "i2", "paired": true,
                      "transport": "orbdrop", "state": "sending", "received": 3 * 1024 * 1024, "error": null },
                    { "peerId": "PC-C", "target": "PC-C", "inviteId": "i3", "paired": true,
                      "transport": "orbdrop", "state": "failed", "received": 0, "error": null },
                ],
            })
        );
    }

    /// 刚建出来那一批：每台都在等（界面第一眼看到的是"等待 N"），进度分母是各台之和。
    #[test]
    fn a_fresh_batch_waits_on_every_target() {
        let mut b = batch(vec![
            target("PC-A", "i1", TargetState::Invited, 0),
            target("PC-B", "i2", TargetState::Invited, 0),
            target("PC-C", "i3", TargetState::Cancelled, 0),
        ]);
        b.skipped = 3;
        let value = serde_json::to_value(snapshot(&b)).unwrap();
        assert_eq!(
            value["skipped"].as_u64(),
            Some(3),
            "跳过的项数得原样带到卡片上 —— 数字对不上用户就不知道清单少了什么"
        );
        assert_eq!(
            (
                value["done"].as_u64(),
                value["waiting"].as_u64(),
                value["failed"].as_u64(),
                value["received"].as_u64(),
                value["total"].as_u64(),
            ),
            (Some(0), Some(2), Some(1), Some(0), Some(20 * 1024 * 1024)),
            "取消的那台不算在路上：等待 2 台，进度分母也只有它俩"
        );
    }
}

#[cfg(test)]
mod reconcile_tests {
    use super::*;

    fn wire(invite: &str, state: &str) -> PushInviteStateWire {
        PushInviteStateWire {
            invite_id: invite.into(),
            state: state.into(),
            received: 0,
            reason: None,
        }
    }

    fn invite_of(id: &str, sender: &str) -> InboundInvite {
        InboundInvite {
            invite_id: id.into(),
            sender: PeerInfo {
                id: sender.into(),
                name: sender.into(),
            },
            name: "a.txt".into(),
            size: 10,
            is_dir: false,
            count: 1,
            files: vec![],
            caps: Vec::new(),
            root: None,
            callback: None,
            responder: None,
        }
    }

    #[test]
    fn invite_limits_reject_oversized_and_overlong_manifests() {
        let mut inv = invite_of("i", "pc");
        inv.is_dir = true;
        inv.files = vec![crate::ws::events::FileEntry {
            path: "a.bin".into(),
            size: MAX_INVITE_FILE_BYTES + 1,
            mtime: 0,
        }];
        assert!(validate_invite(&inv).is_err(), "单文件超过上限必须拒绝");

        let mut inv = invite_of("i", "pc");
        inv.is_dir = true;
        inv.files = vec![crate::ws::events::FileEntry {
            path: "a".repeat(MAX_INVITE_PATH_BYTES + 1),
            size: 1,
            mtime: 0,
        }];
        assert!(validate_invite(&inv).is_err(), "超长路径必须拒绝");

        let mut inv = invite_of("i", "pc");
        inv.is_dir = true;
        inv.files = (0..=MAX_INVITE_FILES)
            .map(|i| crate::ws::events::FileEntry {
                path: format!("f{i}"),
                size: 1,
                mtime: 0,
            })
            .collect();
        assert!(validate_invite(&inv).is_err(), "文件数超过上限必须拒绝");

        let mut inv = invite_of("i", "pc");
        inv.size = MAX_INVITE_FILE_BYTES + 1;
        assert!(validate_invite(&inv).is_err(), "单文件推送超过上限必须拒绝");

        let ok = invite_of("i", "pc");
        assert!(validate_invite(&ok).is_ok(), "正常小清单必须放行");
    }

    /// 入站邀请有上界：`/push/invite` 是公开路由，第三方换个 invite id 就能再投一条（每条还带着
    /// 对端给的清单），带 `await1` 的邀请又**没有时钟**（F-51：不点就一直留着）—— 没有这个上限，
    /// 表就没有上界。挤掉的是**最老**那条，不是随机一条：正常用永远碰不到这个数，碰到的只有滥用。
    #[test]
    fn inbound_invites_are_bounded_and_drop_the_oldest() {
        let store = PushStore::new();
        for i in 0..MAX_INBOUND_INVITES + 5 {
            store.insert_invite(invite_of(&format!("i{i}"), "pc"));
        }
        assert_eq!(
            store.inbound.lock().len(),
            MAX_INBOUND_INVITES,
            "表刚好卡在上限"
        );
        for i in 0..5 {
            assert_eq!(
                store.answer_invite_state(&format!("i{i}"), "pc").state,
                "unknown",
                "最早那几条被挤掉了（它们连结局记录都没有）"
            );
        }
        assert_eq!(
            store.answer_invite_state("i5", "pc").state,
            "waiting",
            "第 6 条起留在表里 —— 未处理的卡片照样能点、能收"
        );
        // 取走一条就腾出一个位置（顺序记账跟着表走，不能自己涨）。
        assert!(store.take_invite("i5").is_some());
        store.insert_invite(invite_of("新的一条", "pc"));
        assert_eq!(store.answer_invite_state("新的一条", "pc").state, "waiting");
        assert_eq!(
            store.answer_invite_state("i6", "pc").state,
            "waiting",
            "还有一个空位：这时谁都不该被挤掉"
        );
        // 再满上一条 —— 被挤掉的必须是此刻**最老**的那条（`i6`），不是随手一条。
        store.insert_invite(invite_of("又一条", "pc"));
        assert_eq!(store.answer_invite_state("又一条", "pc").state, "waiting");
        assert_eq!(
            store.answer_invite_state("i6", "pc").state,
            "unknown",
            "满员时挤掉的是此刻最老的那条"
        );
        assert_eq!(store.inbound.lock().len(), MAX_INBOUND_INVITES);
    }

    /// 只限条数挡不住 128 × 64 MiB；这里用小预算跑同一条淘汰路径，证明**总字节**也会
    /// 触发挤掉最老的，而不是无限保留。
    #[test]
    fn inbound_invites_are_also_capped_by_total_bytes() {
        let store = PushStore::new();
        let budget = 450_000usize;
        let mut a = invite_of("a", "pc");
        a.name = "x".repeat(200_000);
        let mut b = invite_of("b", "pc");
        b.name = "x".repeat(200_000);
        let mut c = invite_of("c", "pc");
        c.name = "x".repeat(200_000);

        store.insert_invite_with_budget(a, budget);
        store.insert_invite_with_budget(b, budget);
        assert_eq!(
            store.answer_invite_state("a", "pc").state,
            "waiting",
            "两条 200 KB 在 450 KB 预算内"
        );
        store.insert_invite_with_budget(c, budget);
        assert_eq!(
            store.answer_invite_state("a", "pc").state,
            "unknown",
            "加入第三条时挤掉最老的"
        );
        assert_eq!(store.answer_invite_state("b", "pc").state, "waiting");
        assert_eq!(store.answer_invite_state("c", "pc").state, "waiting");
        let total: usize = store
            .inbound
            .lock()
            .values()
            .map(invite_footprint)
            .sum();
        assert!(
            total <= budget,
            "保留总量必须回到预算内，实际 {total} > {budget}"
        );
    }

    /// 单文件推送那条路上对端只给了名字与大小（没有清单），清单是现场**合成**的一条；
    /// 终结清理要按同一份清单判"哪一个是我们写了一半的"，合成规则差一点就是删错文件。
    #[test]
    fn a_single_file_invite_synthesizes_its_manifest() {
        let mut inv = invite_of("i1", "pc");
        inv.is_dir = false;
        // 单文件推送时 `name` 就是清单里的**相对路径**（`manifest_for_paths` 把源名放进了路径）。
        inv.name = "子目录/报告.pdf".into();
        inv.size = 4096;
        let m = inv.manifest();
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].path, "子目录/报告.pdf");
        assert_eq!(m[0].size, 4096);

        // 目录推送：清单是权威的，原样给出（不合成、不改写）。
        inv.is_dir = true;
        inv.files = vec![FileEntry {
            path: "a.bin".into(),
            size: 16,
            mtime: 7,
        }];
        let m = inv.manifest();
        assert_eq!(m.len(), 1);
        assert_eq!(
            (m[0].path.as_str(), m[0].size, m[0].mtime),
            ("a.bin", 16, 7)
        );
    }

    /// 还拿在手里的 = `waiting`（等用户处理或下载循环还在跑），处理过的报结局，
    /// 没见过的报 `unknown` —— 发送方据此区分"还在路上"和"根本没送到"。
    #[test]
    fn answer_reports_held_recorded_and_unknown() {
        let store = PushStore::new();
        store.insert_invite(invite_of("held", "pc"));
        store.record_outcome("done-1", "pc", "done", 1234, None);
        store.record_outcome("failed-1", "pc", "failed", 0, Some("磁盘满了".into()));

        assert_eq!(store.answer_invite_state("held", "pc").state, "waiting");
        let done = store.answer_invite_state("done-1", "pc");
        assert_eq!(done.state, "done");
        assert_eq!(done.received, 1234);
        let failed = store.answer_invite_state("failed-1", "pc");
        assert_eq!(failed.state, "failed");
        assert_eq!(failed.reason.as_deref(), Some("磁盘满了"));
        assert_eq!(store.answer_invite_state("nobody", "pc").state, "unknown");
    }

    /// 对端不能借别人的邀请 id 问出第三方的传输情况：发送方对不上就一律 `unknown`。
    #[test]
    fn answer_refuses_ids_from_another_sender() {
        let store = PushStore::new();
        store.insert_invite(invite_of("held", "pc"));
        store.record_outcome("done-1", "pc", "done", 10, None);

        assert_eq!(store.answer_invite_state("held", "别人").state, "unknown");
        assert_eq!(store.answer_invite_state("done-1", "别人").state, "unknown");
    }

    /// 结局记录有界：塞满之后淘汰最旧的那条（只留"最近几分钟"就够对账用）。
    #[test]
    fn outcomes_are_bounded() {
        let store = PushStore::new();
        for i in 0..RECENT_OUTCOMES + 5 {
            store.record_outcome(&format!("i{i}"), "pc", "done", 0, None);
        }
        let answered = (0..RECENT_OUTCOMES + 5)
            .filter(|i| store.answer_invite_state(&format!("i{i}"), "pc").state == "done")
            .count();
        assert_eq!(answered, RECENT_OUTCOMES, "只留最后 N 条");
    }

    /// 闸门：第一次放行、紧接着第二次不放行（链路每 5 秒一拍，对账按 20 秒节流）。
    #[test]
    fn ask_gate_throttles() {
        let store = PushStore::new();
        assert!(store.ask_due("pc"));
        assert!(!store.ask_due("pc"));
        assert!(store.ask_due("别的设备"), "闸门是按设备算的");
    }

    /// 「终态只落一次」的唯一例外：本机**猜的**失败可以被对端的权威终态改写，
    /// 明确意图不行。
    #[test]
    fn only_speculative_failures_can_be_overwritten() {
        assert!(late_success(TargetState::Failed, TargetState::Done));
        assert!(
            late_success(TargetState::Failed, TargetState::Rejected),
            "对账答 `unknown` 判的失败，随后收到对方明确的拒绝 —— 该改口"
        );
        for from in [
            TargetState::Rejected,
            TargetState::Cancelled,
            TargetState::Done,
            TargetState::Invited,
            TargetState::Sending,
        ] {
            assert!(
                !late_success(from, TargetState::Done),
                "{from:?} 不该被改成成功"
            );
        }
        assert!(
            !late_success(TargetState::Done, TargetState::Rejected),
            "成功不该变成被拒"
        );
        assert!(
            !late_success(TargetState::Failed, TargetState::Cancelled),
            "失败不该变成已取消"
        );
    }

    /// 待对账的邀请：只列这台设备、还没落终态的那些。
    #[test]
    fn waiting_invites_filter_by_peer_and_state() {
        let store = PushStore::new();
        let t = |peer: &str, invite: &str, state: TargetState| BatchTarget {
            peer_id: peer.into(),
            peer_name: peer.into(),
            invite_id: invite.into(),
            transport: crate::config::PeerTransport::OrbDrop,
            paired: true,
            state,
            received: 0,
            error: None,
        };
        store.insert_batch(PushBatch {
            id: "b".into(),
            name: "a.txt".into(),
            count: 1,
            skipped: 0,
            per_target_total: 10,
            targets: vec![
                t("pc", "w1", TargetState::Invited),
                t("pc", "w2", TargetState::Sending),
                t("pc", "d1", TargetState::Done),
                t("别的", "w3", TargetState::Invited),
            ],
            last_emit: Instant::now(),
        });

        let mut ids = store.waiting_invites_of("pc");
        ids.sort();
        assert_eq!(ids, vec!["w1".to_string(), "w2".to_string()]);
        assert_eq!(store.waiting_invites_of("别的"), vec!["w3".to_string()]);
        assert_eq!(store.target_of_invite("w3").unwrap().peer_id, "别的");
        assert!(store.target_of_invite("没有这条").is_none());
    }

    /// `Wire` 形状是对端能读的：camelCase 字段名（`PushState` 一上线就得两端一致）。
    #[test]
    fn wire_shape_is_camel_case() {
        let json = serde_json::to_value(wire("i1", "waiting")).unwrap();
        assert_eq!(json["inviteId"], "i1");
        assert_eq!(json["state"], "waiting");
        assert!(json.get("reason").is_some(), "字段在（值为 null 也要在）");
    }
}

#[cfg(test)]
mod receive_gate_tests {
    use super::*;

    /// 暂停闸表项的寿命 = 接收任务：`ReceiveGate` 一落，`receive_downloads` 里那条就该没了。
    /// 留着的后果是界面永远点得动一个没人等的闸门（命令回 `true`，进度却永远不动）。
    #[test]
    fn a_receive_gate_unregisters_when_the_task_ends() {
        let dir = std::env::temp_dir().join(format!("orbdrop_gate_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let state = Arc::new(AppState::new(
            crate::config::Config::default(),
            dir.join("config.toml"),
        ));
        let control = Arc::new(crate::transfer::browse_dl::DownloadControl::default());
        state
            .receive_downloads
            .lock()
            .insert("inv-1".into(), control.clone());
        let gate = ReceiveGate::new(state.clone(), "inv-1".into());
        assert!(state.receive_downloads.lock().contains_key("inv-1"));

        drop(gate);
        assert!(!state.receive_downloads.lock().contains_key("inv-1"));
        // 闸门本身还在任务手里（最后一个引用由任务持有）：暂停语义不受摘表影响。
        control.set_paused(true);
        assert!(control.is_paused());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
