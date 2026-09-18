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

/// Sender-side state of a push job (F-50 ~ F-52).
#[derive(Clone)]
pub struct PushJob {
    pub id: String,
    pub peer_id: String,
    pub peer_name: String,
    pub name: String,
    pub is_dir: bool,
    pub files: Vec<FileEntry>,
    /// 清单相对路径 → 本机绝对路径（服务端按它取文件，见 `download_target`）。
    pub sources: Vec<(String, PathBuf)>,
    pub total_size: u64,
    pub status: PushStatus,
    pub created_at: Instant,
    /// 对端是否已配对：它决定了下载端点怎么认人（见 `routes_push`）。
    pub peer_paired: bool,
}

impl PushJob {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        id: String,
        peer_id: String,
        peer_name: String,
        name: String,
        is_dir: bool,
        files: Vec<FileEntry>,
        sources: Vec<(String, PathBuf)>,
        total_size: u64,
        peer_paired: bool,
    ) -> Self {
        Self {
            id,
            peer_id,
            peer_name,
            name,
            is_dir,
            files,
            sources,
            total_size,
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

pub struct PushStore {
    pub jobs: Mutex<HashMap<String, PushJob>>,
    pub inbound: Mutex<HashMap<String, InboundInvite>>,
    pub resume: super::resume::ResumeStore,
}

impl PushStore {
    pub fn new() -> Self {
        Self {
            jobs: Mutex::new(HashMap::new()),
            inbound: Mutex::new(HashMap::new()),
            resume: super::resume::ResumeStore::new(),
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