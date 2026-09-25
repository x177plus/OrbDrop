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

impl InboundInvite {
    /// 走新协议还是老循环。判据只有一个：**对端说自己支持**（老对端不带这个位）。
    pub fn supports_stream(&self) -> bool {
        self.caps.iter().any(|c| c == CAP_STREAM)
    }

    pub fn total_bytes(&self) -> u64 {
        if self.is_dir {
            self.files.iter().map(|f| f.size).sum()
        } else {
            self.size
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
    /// 邀请已发出、还没有任何回执（已配对的目标会很快转 `Sending`）。
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
            TargetState::Done | TargetState::Cancelled | TargetState::Rejected | TargetState::Failed
        )
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
    /// 这一台对应的邀请 id（= 它那条 `PushJob.id`）。
    pub invite_id: String,
    /// 已配对 = 对方**不问同意**直接收：行文案别说「等待对方同意」（对面没点过任何东西）。
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
    /// 发送方就只能等它那 5 分钟超时，然后把一条已经成功的传输记成失败。
    recent: Mutex<HashMap<String, RecentOutcome>>,
    /// 对账的节流：peer_id → 上次问的时刻（一台设备别每 5 秒问一遍）。
    asked_at: Mutex<HashMap<String, Instant>>,
    /// 已经走完、但还在宽限期的批次：batch_id → 走完的时刻（见 `SETTLED_GRACE`）。
    settled: Mutex<HashMap<String, Instant>>,
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
            resume: super::resume::ResumeStore::new(),
            batches: Mutex::new(HashMap::new()),
            batch_of_invite: Mutex::new(HashMap::new()),
            recent: Mutex::new(HashMap::new()),
            asked_at: Mutex::new(HashMap::new()),
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

    /// 整批走完就丢掉：界面手里那份最后的快照已经是全量。
    pub fn drop_batch(&self, batch_id: &str) {
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
        if let Some(job) = self.jobs.lock().get_mut(id) {
            job.status = status;
        }
    }

    pub fn job_status(&self, id: &str) -> Option<String> {
        self.jobs.lock().get(id).map(|j| match &j.status {
            PushStatus::Invited => "invited".into(),
            PushStatus::Accepted => "accepted".into(),
            PushStatus::Downloading { received, total } => format!("downloading: {received}/{total}"),
            PushStatus::Completed => "completed".into(),
            PushStatus::Failed(r) => format!("failed: {r}"),
        })
    }

    pub fn remove_job(&self, id: &str) {
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
        self.inbound.lock().insert(inv.invite_id.clone(), inv);
    }

    pub fn remove_invite(&self, id: &str) {
        self.inbound.lock().remove(id);
    }

    /// 取走一条邀请（拒绝/超时/取消）。取走后下载循环就会收手。
    pub fn take_invite(&self, id: &str) -> Option<InboundInvite> {
        self.inbound.lock().remove(id)
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
    fn default() -> Self { Self::new() }
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
    let shot = {
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
        if !force && batch.last_emit.elapsed() < REPORT_INTERVAL {
            return;
        }
        batch.last_emit = Instant::now();
        snapshot(batch)
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
}

// ---------------------------------------------------------------------------
// Reports back to the sender
// ---------------------------------------------------------------------------

/// 把一条推送状态回报给发送方（进度 / 完成 / 失败）。
///
/// 长连接在就**先试它**；"塞进队列"不等于"送出去了"（`ws::send` 只写进当前会话的 mpsc，
/// socket 写失败它并不知道），所以塞失败要**退回 HTTP**，而不是把这条回执丢掉 ——
/// 丢了这一条，发送方那台就会在 5 分钟后把一条已经成功的传输记成失败（假账）。
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
            match crate::net::client().post(&url).json(&event).send().await {
                Ok(_) => return,
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
    let callback = invite.callback.clone();

    // 长连接那条路上的"同意"回执（HTTP 那条路走邀请请求的同步响应）。
    if state.ws.has_connection(&peer_id) {
        ws::send_to_peer(&state, &peer_id, WsEvent::PushResponse {
            invite_id: invite_id.clone(),
            accepted: true,
        });
    }

    // 下载要认人：已配对的对端必须带自己那枚 token；未绑定的推送没有 token 可带，
    // 下载端点就只认邀请 id（那个 id 只在这条邀请里出现过）。
    let token = match credential::get_peer_token(&peer_id) {
        Ok(t) => t,
        Err(e) => {
            fail_download(&state, &peer_id, &callback, &invite_id, format!("读取凭据失败: {e}"));
            return;
        }
    };
    let paired = state.get_peer(&peer_id).map(|p| p.paired).unwrap_or(false);
    if paired && token.is_none() {
        fail_download(&state, &peer_id, &callback, &invite_id, "本机没有对端 token".into());
        return;
    }

    if invite.supports_stream() {
        super::download::spawn(state, invite, peer_id, token);
        return;
    }
    spawn_download_legacy(state, invite, peer_id, token);
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
) {
    use futures_util::StreamExt;
    use tokio::io::{AsyncSeekExt, AsyncWriteExt};

    let invite_id = invite.invite_id.clone();
    let total_bytes = invite.total_bytes();
    let callback = invite.callback.clone();

    // 下载跑在运行时上：「同意接收」是 async 命令（`task::spawn` 另有"没有 reactor 就退到
    // Tauri 全局运行时"的兜底，见 task.rs）—— 这条任务绝不能被无声丢掉，否则两边都停在 0%。
    crate::task::spawn("push download", async move {
        let client = crate::net::client();
        let config = state.config.lock().clone();
        // 落盘根 = 用户设的下载目录（S-01）：推送接收不再单独有「收件目录」。
        //
        // 对端只往 <发送方名>/ 下面写 —— **没有"推送名子目录"**：清单里的相对路径就是落盘后
        // 的样子（单文件 `A.txt`、单目录 `D/…`、多选 `甲目录/…` + `散文件.txt`），
        // 发送方在 `manifest_for_paths` 里已经把每个源的名字放进路径了。
        let sender_dir = config.download_dir.join(&invite.sender.name);
        let _ = tokio::fs::create_dir_all(&sender_dir).await;

        let mut total_received: u64 = 0;
        let mut last_report: Option<Instant> = None;
        // 已经建过的目录（见下面 `made_dirs.insert`）：目录嵌套深、文件多时，
        // "每文件问一次系统"是纯浪费。
        let mut made_dirs: HashSet<PathBuf> = HashSet::new();
        let files = if invite.is_dir {
            invite.files.clone()
        } else {
            vec![FileEntry {
                path: invite.name.clone(),
                size: invite.size,
                // 单文件推送这条路上对端只有名字与大小（没有清单），时间戳无从谈起。
                mtime: 0,
            }]
        };

        let base_url = format!(
            "{}/push/download",
            crate::net::peer_base(
                callback.as_ref().map(|(ip, _)| ip.as_str()).unwrap_or_default(),
                callback.as_ref().map(|(_, port)| *port).unwrap_or(0),
            )
        );

        for file in &files {
            let file_target = match super::resume::safe_join(&sender_dir, &file.path) {
                Some(p) => p,
                None => {
                    fail_download(&state, &peer_id, &callback, &invite_id, format!("非法路径: {}", file.path));
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
                .create(true).write(true).open(&file_target).await
            {
                Ok(f) => f,
                Err(e) => {
                    fail_download(&state, &peer_id, &callback, &invite_id, format!("打开文件失败: {e}"));
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
                    // 接收方自己点了取消：话已经由 cancel_receive 带过去了，这里静静收手。
                    info!(%invite_id, "download cancelled");
                    return;
                }
                if offset >= file.size { break; }

                let mut req = client
                    .get(&base_url)
                    .query(&[
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
                        fail_download(&state, &peer_id, &callback, &invite_id, "发送方已取消".into());
                        state.push.resume.clear(&invite_id);
                        return;
                    }
                    Ok(r) if r.status().as_u16() == 409 => {
                        // 源文件在建清单之后变小/被换掉了（对端语义见
                        // `routes_push::push_download_handler`）。这是明确的错误，
                        // 不能拿一个空包当"传完了"。
                        fail_download(
                            &state, &peer_id, &callback, &invite_id,
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
                    fail_download(&state, &peer_id, &callback, &invite_id, format!("定位写入位置失败: {e}"));
                    return;
                }
                let attempt_start = offset;
                // 一次尝试一个缓冲：客户端交给我们的块常常只有十几 KB（TLS 记录的粒度），
                // 逐块写就是每 16KB 一次阻塞任务派发；256KB 缓冲把写系统调用除十几倍。
                let mut out =
                    tokio::io::BufWriter::with_capacity(crate::net::STREAM_CHUNK, &mut fh);

                let mut stream = resp.bytes_stream();
                while let Some(chunk) = stream.next().await {
                    let chunk = match chunk {
                        Ok(c) => c,
                        Err(e) => {
                            // 发送方中途取消时，流会就地断掉：退回外层重试，
                            // 下一次请求就会拿到 410，按"发送方已取消"收尾。
                            //
                            // **先把缓冲交出去再退**：`offset` 已经把缓冲里的字节算进去了，
                            // 丢掉它们就等于"以为收过、其实没写"（后续续传直接从洞后面接着写）。
                            let _ = out.flush().await;
                            warn!(%invite_id, %e, "stream error");
                            continue 'file;
                        }
                    };
                    if let Err(e) = out.write_all(&chunk).await {
                        fail_download(&state, &peer_id, &callback, &invite_id, format!("写入失败: {e}"));
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
                            fail_download(&state, &peer_id, &callback, &invite_id, format!("写入失败: {e}"));
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
                        report_to_sender(&state, &peer_id, &callback, WsEvent::PushProgress {
                            invite_id: invite_id.clone(),
                            received: total_received,
                            total: total_bytes,
                        });
                        ws::emit_ui(&state, ws::events::ui::PUSH_PROGRESS, &serde_json::json!({
                            "inviteId": invite_id,
                            "received": total_received,
                            "total": total_bytes,
                        }));
                    }
                }
                if let Err(e) = out.flush().await {
                    fail_download(&state, &peer_id, &callback, &invite_id, format!("写入失败: {e}"));
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
                        fail_download(
                            &state, &peer_id, &callback, &invite_id,
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
        // 结局**先**落地、再摘邀请：对账可能正好撞在这一瞬（邀请已摘、记录还没写），
        // 那种窗口里答出去的是 `unknown`（"没送到"）——事实是"收到了"。
        state
            .push
            .record_outcome(&invite_id, &peer_id, "done", total_bytes, None);
        state.push.remove_invite(&invite_id);
        report_to_sender(&state, &peer_id, &callback, WsEvent::PushDone {
            invite_id: invite_id.clone(),
            status: "completed".into(),
            reason: None,
        });
        ws::emit_ui(&state, ws::events::ui::PUSH_DONE, &serde_json::json!({
            "inviteId": invite_id,
            "status": "completed",
            // 落盘位置（<下载目录>/<发送方名>）：界面那条"已收到"的长通知要告诉用户
            // 东西到底在哪 —— 没有这个，通知只能说"收到了"，点完「知道了」就再也找不到。
            "dest": sender_dir.to_string_lossy(),
        }));
        info!(%invite_id, "push download completed");
    });
}

/// 接收失败：告诉发送方，并让本机的接收卡片变成失败态。
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
    // 同"完成"那条：先落结局再摘邀请（对账撞在中间那一瞬不该答 `unknown`）。
    //
    // 顺带把续传记录落盘：失败之后**留下**的记录就是"下次接着传"的全部依据，
    // 光在内存里记着，进程一退就白记了。
    state.push.resume.flush();
    state
        .push
        .record_outcome(invite_id, peer_id, "failed", 0, Some(reason.clone()));
    state.push.remove_invite(invite_id);
    report_to_sender(state, peer_id, callback, WsEvent::PushDone {
        invite_id: invite_id.to_string(),
        status: "failed".into(),
        reason: Some(reason.clone()),
    });
    ws::emit_ui(state, ws::events::ui::PUSH_DONE, &serde_json::json!({
        "inviteId": invite_id,
        "status": "failed",
        "reason": reason,
    }));
}
#[cfg(test)]
mod tests {
    use super::*;

    fn target(peer: &str, invite: &str, state: TargetState, received: u64) -> BatchTarget {
        BatchTarget {
            peer_id: peer.into(),
            peer_name: peer.into(),
            invite_id: invite.into(),
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
                      "state": "done", "received": 10 * 1024 * 1024, "error": null },
                    { "peerId": "PC-B", "target": "PC-B", "inviteId": "i2", "paired": true,
                      "state": "sending", "received": 3 * 1024 * 1024, "error": null },
                    { "peerId": "PC-C", "target": "PC-C", "inviteId": "i3", "paired": true,
                      "state": "failed", "received": 0, "error": null },
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
            sender: PeerInfo { id: sender.into(), name: sender.into() },
            name: "a.txt".into(),
            size: 10,
            is_dir: false,
            count: 1,
            files: vec![],
            caps: Vec::new(),
            callback: None,
            responder: None,
        }
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
            assert!(!late_success(from, TargetState::Done), "{from:?} 不该被改成成功");
        }
        assert!(!late_success(TargetState::Done, TargetState::Rejected), "成功不该变成被拒");
        assert!(!late_success(TargetState::Failed, TargetState::Cancelled), "失败不该变成已取消");
    }

    /// 待对账的邀请：只列这台设备、还没落终态的那些。
    #[test]
    fn waiting_invites_filter_by_peer_and_state() {
        let store = PushStore::new();
        let t = |peer: &str, invite: &str, state: TargetState| BatchTarget {
            peer_id: peer.into(),
            peer_name: peer.into(),
            invite_id: invite.into(),
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
