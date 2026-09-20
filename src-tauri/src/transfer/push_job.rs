use std::collections::HashMap;
use std::path::PathBuf;
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
    pub sources: Vec<(String, PathBuf)>,
    pub status: PushStatus,
    pub created_at: Instant,
    /// 对端是否已配对：它决定了下载端点怎么认人（见 `routes_push`）。
    pub peer_paired: bool,
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
        sources: Vec<(String, PathBuf)>,
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

impl InboundInvite {
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
    Found { path: PathBuf, peer_paired: bool, peer_id: String },
}

// ---------------------------------------------------------------------------
// 推送批次（F-50 多目标）：一次操作 = 一批 = 界面上一张卡片
// ---------------------------------------------------------------------------

/// 一台目标设备在一批推送里的状态。
///
/// `Cancelled` 不并进 `Failed`：取消是**用户自己做的事**，行文案必须是「已取消」而不是
/// 「失败」；两者只在"这一批算不算全成"这件事上同类（卡片的收起规则见前端）。
#[derive(Clone, Copy, PartialEq, Eq)]
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
}

impl PushStore {
    pub fn new() -> Self {
        Self {
            jobs: Mutex::new(HashMap::new()),
            inbound: Mutex::new(HashMap::new()),
            resume: super::resume::ResumeStore::new(),
            batches: Mutex::new(HashMap::new()),
            batch_of_invite: Mutex::new(HashMap::new()),
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
        self.jobs.lock().remove(id);
    }

    pub fn job(&self, id: &str) -> Option<PushJob> {
        self.jobs.lock().get(id).cloned()
    }

    /// 这条推送还在不在。
    ///
    /// 下载端点的流式读取**每读一块**都要问一次（发送方一取消就当场断流），
    /// 所以它必须是查表，不能是 `job()` —— 那会把整份文件清单(`Vec<FileEntry>`)克隆一遍：
    /// 推一个几万文件的目录时，每 64KB 克隆一次清单，进度条会慢到看起来卡在 0%。
    pub fn has_job(&self, id: &str) -> bool {
        self.jobs.lock().contains_key(id)
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
        match job.sources.iter().find(|(p, _)| p == rel) {
            Some((_, abs)) => DownloadTarget::Found {
                path: abs.clone(),
                peer_paired: job.peer_paired,
                peer_id: job.peer_id.clone(),
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
}

impl Default for PushStore {
    fn default() -> Self { Self::new() }
}

// ---------------------------------------------------------------------------
// Batch snapshots to the UI
// ---------------------------------------------------------------------------

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
        if target.state.is_terminal() {
            return;
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
        state.push.drop_batch(&batch_id);
    }
}

// ---------------------------------------------------------------------------
// Reports back to the sender
// ---------------------------------------------------------------------------

/// 把一条推送状态回报给发送方（进度 / 完成 / 失败）。
///
/// 长连接在就直接走 WS；不在（已配对未连、未绑定）就 POST 回邀请上的 callback 地址，
/// 请求体仍是同一个 `WsEvent` —— 两条通道说同一套协议，发送方只有一份处理代码。
pub fn report_to_sender(
    state: &AppState,
    peer_id: &str,
    callback: &Option<(String, u16)>,
    event: WsEvent,
) {
    if state.ws.has_connection(peer_id) {
        ws::send_to_peer(state, peer_id, event);
        return;
    }
    let Some((ip, port)) = callback.clone() else {
        warn!(%peer_id, "推送回执无处可送：既没有长连接，也没有对方地址");
        return;
    };
    let url = format!("http://{ip}:{port}/push/report");
    crate::task::spawn("push report", async move {
        if let Err(e) = reqwest::Client::new().post(&url).json(&event).send().await {
            warn!(%url, %e, "推送回执发送失败");
        }
    });
}

// ---------------------------------------------------------------------------
// Receiver-side download loop
// ---------------------------------------------------------------------------

/// 接收方：开始把对端的文件拉下来（F-52 / F-60）。
pub fn spawn_download(state: Arc<AppState>, invite: InboundInvite, peer_id: String) {
    use futures_util::StreamExt;
    use tokio::io::{AsyncSeekExt, AsyncWriteExt};

    let invite_id = invite.invite_id.clone();
    let total_bytes = invite.total_bytes();
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

    // 下载跑在运行时上：「同意接收」是 async 命令（`task::spawn` 另有"没有 reactor 就退到
    // Tauri 全局运行时"的兜底，见 task.rs）—— 这条任务绝不能被无声丢掉，否则两边都停在 0%。
    crate::task::spawn("push download", async move {
        let client = reqwest::Client::new();
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
        let files = if invite.is_dir {
            invite.files.clone()
        } else {
            vec![FileEntry { path: invite.name.clone(), size: invite.size }]
        };

        let base_url = format!(
            "http://{}:{}/push/download",
            callback.as_ref().map(|(ip, _)| ip.clone()).unwrap_or_default(),
            callback.as_ref().map(|(_, port)| *port).unwrap_or(0)
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
                let _ = tokio::fs::create_dir_all(p).await;
            }

            let mut offset = state.push.resume.load(&invite_id, &file.path);
            let mut last_saved = offset;
            let mut last_persist_time = Instant::now();
            const PERSIST_CHUNK: u64 = 1_024_000;
            const PERSIST_INTERVAL: Duration = Duration::from_secs(5);

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

                let mut fh = match tokio::fs::OpenOptions::new()
                    .create(true).write(true).open(&file_target).await
                {
                    Ok(f) => f,
                    Err(e) => {
                        fail_download(&state, &peer_id, &callback, &invite_id, format!("打开文件失败: {e}"));
                        return;
                    }
                };
                if fh.metadata().await.map(|m| m.len()).unwrap_or(0) > offset {
                    let _ = fh.set_len(offset).await;
                }
                let _ = fh.seek(std::io::SeekFrom::Start(offset)).await;

                let mut stream = resp.bytes_stream();
                while let Some(chunk) = stream.next().await {
                    let chunk = match chunk {
                        Ok(c) => c,
                        Err(e) => {
                            // 发送方中途取消时，流会就地断掉：退回外层重试，
                            // 下一次请求就会拿到 410，按"发送方已取消"收尾。
                            warn!(%invite_id, %e, "stream error");
                            continue 'file;
                        }
                    };
                    if let Err(e) = fh.write_all(&chunk).await {
                        fail_download(&state, &peer_id, &callback, &invite_id, format!("写入失败: {e}"));
                        return;
                    }
                    offset += chunk.len() as u64;
                    total_received += chunk.len() as u64;

                    if offset - last_saved >= PERSIST_CHUNK
                        || Instant::now().duration_since(last_persist_time) >= PERSIST_INTERVAL
                    {
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
                let _ = fh.flush().await;
            }
            state.push.resume.save(&invite_id, &file.path, file.size);
        }

        state.push.resume.clear(&invite_id);
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
fn fail_download(
    state: &AppState,
    peer_id: &str,
    callback: &Option<(String, u16)>,
    invite_id: &str,
    reason: String,
) {
    error!(%invite_id, %reason, "push download failed");
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
