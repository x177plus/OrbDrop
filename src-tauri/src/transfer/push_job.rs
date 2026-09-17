use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::time::sleep;
use tracing::{error, info, warn};

use crate::credential;
use crate::state::AppState;
use crate::ws;
use crate::ws::events::{FileEntry, PeerInfo, WsEvent};

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
    Cancelled,
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
    pub total_size: u64,
    pub status: PushStatus,
    pub created_at: Instant,
    pub source_dir: PathBuf,
}

impl PushJob {
    pub fn new(
        id: String,
        peer_id: String,
        peer_name: String,
        name: String,
        is_dir: bool,
        files: Vec<FileEntry>,
        total_size: u64,
        source_dir: PathBuf,
    ) -> Self {
        Self {
            id,
            peer_id,
            peer_name,
            name,
            is_dir,
            files,
            total_size,
            status: PushStatus::Invited,
            created_at: Instant::now(),
            source_dir,
        }
    }

    pub fn file_count(&self) -> usize {
        if self.is_dir { self.files.len() } else { 1 }
    }
}

/// Receiver-side state for an incoming push invitation.
pub struct InboundInvite {
    pub invite_id: String,
    pub sender: PeerInfo,
    pub name: String,
    pub size: u64,
    pub is_dir: bool,
    pub files: Vec<FileEntry>,
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
            PushStatus::Cancelled => "cancelled".into(),
        })
    }

    pub fn remove_job(&self, id: &str) {
        self.jobs.lock().remove(id);
    }

    pub fn job(&self, id: &str) -> Option<PushJob> {
        self.jobs.lock().get(id).cloned()
    }

    // receiver
    pub fn insert_invite(&self, inv: InboundInvite) {
        self.inbound.lock().insert(inv.invite_id.clone(), inv);
    }

    pub fn remove_invite(&self, id: &str) {
        self.inbound.lock().remove(id);
    }

    pub fn remove_invite_if_present(&self, id: &str) -> bool {
        self.inbound.lock().remove(id).is_some()
    }
}

impl Default for PushStore {
    fn default() -> Self { Self::new() }
}

// ---------------------------------------------------------------------------
// Receiver-side download loop
// ---------------------------------------------------------------------------

pub fn spawn_download(
    state: Arc<AppState>,
    invite: InboundInvite,
    peer_id: String,
    peer_ip: String,
    peer_port: u16,
) {
    use tokio::io::{AsyncSeekExt, AsyncWriteExt};
    use futures_util::StreamExt;

    let invite_id = invite.invite_id.clone();
    let total_bytes = invite.total_bytes();

    // Notify sender: accepted
    ws::send_to_peer(&state, &peer_id, WsEvent::PushResponse {
        invite_id: invite_id.clone(),
        accepted: true,
    });
    ws::emit_ui(&state, ws::events::ui::PUSH_RESPONSE, &serde_json::json!({
        "inviteId": invite_id,
        "accepted": true,
    }));

    let token = credential::get_peer_token(&peer_id)
        .ok().flatten().unwrap_or_default();
    if token.is_empty() {
        error!(%peer_id, "no token for push download");
        ws::send_to_peer(&state, &peer_id, WsEvent::PushDone {
            invite_id: invite_id.clone(),
            status: "failed".into(),
            reason: Some("no auth token".into()),
        });
        return;
    }

    // 接收方点「同意」走的是同步命令（主线程、无 reactor），必须走统一收口，
    // 否则同一个 panic 会以"点同意就闪退"的形式复现。
    crate::task::spawn("push download", async move {
        let client = reqwest::Client::new();
        let config = state.config.lock().clone();
        // 落盘根 = 用户设的下载目录（S-01）：推送接收不再单独有「收件目录」。
        // 下面按 <发送方名>/<推送名>/… 建子目录，所以推送进来的东西不会和手动下载的文件混在
        // 同一层，也不会互相覆盖；`safe_join` 的根就是这里。
        let save_root = config.download_dir.clone();
        let sender_dir = save_root.join(&invite.sender.name).join(&invite.name);
        let _ = tokio::fs::create_dir_all(&sender_dir).await;

        let mut total_received: u64 = 0;
        let files = if invite.is_dir {
            invite.files.clone()
        } else {
            vec![FileEntry { path: invite.name.clone(), size: invite.size }]
        };

        let base_url = format!("http://{peer_ip}:{peer_port}/push/download");

        for file in &files {
            let file_target = match super::resume::safe_join(&sender_dir, &file.path) {
                Some(p) => p,
                None => {
                    ws::send_to_peer(&state, &peer_id, WsEvent::PushDone {
                        invite_id: invite_id.clone(),
                        status: "failed".into(),
                        reason: Some(format!("invalid path: {}", file.path)),
                    });
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
                    info!(%invite_id, "download cancelled");
                    return;
                }
                if offset >= file.size { break; }

                let resp = match client
                    .get(&base_url)
                    .query(&[
                        ("id", invite_id.as_str()),
                        ("path", file.path.as_str()),
                        ("offset", &offset.to_string()),
                    ])
                    .bearer_auth(&token)
                    .send().await
                {
                    Ok(r) if r.status().as_u16() == 410 => {
                        info!(%invite_id, %file.path, "cancelled by sender");
                        ws::send_to_peer(&state, &peer_id, WsEvent::PushDone {
                            invite_id: invite_id.clone(),
                            status: "failed".into(),
                            reason: Some("cancelled by sender".into()),
                        });
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
                        error!(%invite_id, %e, "open file failed");
                        ws::send_to_peer(&state, &peer_id, WsEvent::PushDone {
                            invite_id: invite_id.clone(),
                            status: "failed".into(),
                            reason: Some(format!("open error: {e}")),
                        });
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
                            warn!(%invite_id, %e, "stream error");
                            continue 'file;
                        }
                    };
                    if let Err(e) = fh.write_all(&chunk).await {
                        error!(%invite_id, %e, "write error");
                        ws::send_to_peer(&state, &peer_id, WsEvent::PushDone {
                            invite_id: invite_id.clone(),
                            status: "failed".into(),
                            reason: Some(format!("write error: {e}")),
                        });
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

                    let progress = serde_json::json!({
                        "inviteId": invite_id,
                        "received": total_received,
                        "total": total_bytes,
                    });
                    ws::send_to_peer(&state, &peer_id, WsEvent::PushProgress {
                        invite_id: invite_id.clone(),
                        received: total_received,
                        total: total_bytes,
                    });
                    state.push.update_status(&invite_id, PushStatus::Downloading {
                        received: total_received,
                        total: total_bytes,
                    });
                    ws::emit_ui(&state, ws::events::ui::PUSH_PROGRESS, &progress);

                    sleep(Duration::from_millis(50)).await;
                }
                let _ = fh.flush().await;
            }
            state.push.resume.save(&invite_id, &file.path, file.size);
        }

        state.push.resume.clear(&invite_id);
        state.push.remove_invite(&invite_id);
        ws::send_to_peer(&state, &peer_id, WsEvent::PushDone {
            invite_id: invite_id.clone(),
            status: "completed".into(),
            reason: None,
        });
        ws::emit_ui(&state, ws::events::ui::PUSH_DONE, &serde_json::json!({
            "inviteId": invite_id,
            "status": "completed",
        }));
        info!(%invite_id, "push download completed");
    });
}