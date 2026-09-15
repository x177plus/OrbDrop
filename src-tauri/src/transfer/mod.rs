pub mod push_job;
pub mod resume;

use std::path::PathBuf;
use std::sync::Arc;

use tracing::{info, warn};

use crate::config::ShareItem;
use crate::state::AppState;
use crate::ws;
use crate::ws::events::{FileEntry, PeerInfo, WsEvent};

/// Recursively collect the file manifest for a push (file or directory).
/// Returns (files, total_size, is_dir).
pub async fn build_manifest(share: &ShareItem, path_in_share: &str) -> Result<(Vec<FileEntry>, u64, bool), String> {
    let root = share.path.join(path_in_share);
    let meta = tokio::fs::metadata(&root).await.map_err(|e| format!("无法访问: {e}"))?;

    if meta.is_file() {
        let name = root.file_name().map(|s| s.to_string_lossy().to_string()).unwrap_or_else(|| "file".into());
        let files = vec![FileEntry { path: name.clone(), size: meta.len() }];
        Ok((files, meta.len(), false))
    } else {
        let mut files = Vec::new();
        let mut total = 0u64;
        collect_dir(&root, &mut files, &mut total, &root).await?;
        Ok((files, total, true))
    }
}

async fn collect_dir(
    dir: &PathBuf,
    files: &mut Vec<FileEntry>,
    total: &mut u64,
    base: &PathBuf,
) -> Result<(), String> {
    let mut stack = vec![dir.clone()];
    while let Some(current) = stack.pop() {
        let mut rd = tokio::fs::read_dir(&current).await.map_err(|e| format!("读取目录失败: {e}"))?;
        while let Some(entry) = rd.next_entry().await.map_err(|e| format!("读取目录失败: {e}"))? {
            let path = entry.path();
            let meta = entry.metadata().await.map_err(|e| format!("元数据失败: {e}"))?;
            if meta.is_dir() {
                stack.push(path);
            } else {
                let rel = path.strip_prefix(base)
                    .map(|p| p.to_string_lossy().replace('\\', "/"))
                    .unwrap_or_default();
                *total += meta.len();
                files.push(FileEntry { path: rel, size: meta.len() });
            }
        }
    }
    Ok(())
}

/// Sender side: create a push invite and send it over WS.
/// Returns the invite id.
pub async fn invite_push(
    state: Arc<AppState>,
    share_id: &str,
    path_in_share: &str,
    peer_id: &str,
) -> Result<String, String> {
    let (share, peer) = {
        let config = state.config.lock();
        let share = config.shares.iter().find(|s| s.id == share_id)
            .ok_or_else(|| format!("共享项不存在: {share_id}"))?
            .clone();
        let peer = config.peers.iter().find(|p| p.id == peer_id)
            .cloned()
            .ok_or_else(|| format!("设备不存在: {peer_id}"))?;
        (share, peer)
    };

    if !state.ws.has_connection(peer_id) {
        return Err("对方离线".into());
    }

    let (files, total, is_dir) = build_manifest(&share, path_in_share).await?;
    if files.is_empty() {
        return Err("没有可推送的文件".into());
    }

    let invite_id = uuid::Uuid::new_v4().to_string();
    let name = PathBuf::from(path_in_share)
        .file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| share.name.clone());

    let self_name = state.config.lock().device_name.clone();

    let job = push_job::PushJob::new(
        invite_id.clone(),
        peer_id.to_string(),
        peer.name.clone(),
        name.clone(),
        is_dir,
        files.clone(),
        total,
        share.path.clone(),
    );
    state.push.insert_job(job);

    let ok = state.ws.send(peer_id, WsEvent::PushInvite {
        invite_id: invite_id.clone(),
        sender: PeerInfo { id: state.config.lock().device_id.clone(), name: self_name },
        name,
        size: total,
        is_dir,
        files,
    });

    if !ok {
        state.push.remove_job(&invite_id);
        return Err("对方离线".into());
    }

    // 5-minute invite timeout: no response → auto-cancel.
    let state2 = state.clone();
    let id2 = invite_id.clone();
    crate::task::spawn("push invite timeout", async move {
        tokio::time::sleep(std::time::Duration::from_secs(300)).await;
        if let Some(status) = state2.push.job_status(&id2) {
            if status == "invited" {
                info!(%id2, "invite timed out, cancelling");
                state2.push.update_status(&id2, push_job::PushStatus::Cancelled);
                state2.push.remove_job(&id2);
                ws::emit_ui(&state2, ws::events::ui::PUSH_DONE, &serde_json::json!({
                    "inviteId": id2,
                    "status": "failed",
                    "reason": "对方未响应 (5分钟超时)",
                }));
            }
        }
    });

    Ok(invite_id)
}

/// Sender side: cancel a pending push.
pub fn cancel_push(state: &AppState, invite_id: &str) -> Result<(), String> {
    let job = state.push.jobs.lock().get(invite_id).map(|j| j.peer_id.clone())
        .ok_or_else(|| "推送不存在".to_string())?;
    state.push.update_status(invite_id, push_job::PushStatus::Cancelled);
    state.push.remove_job(invite_id);
    ws::emit_ui(state, ws::events::ui::PUSH_DONE, &serde_json::json!({
        "inviteId": invite_id,
        "status": "cancelled",
    }));
    // Receiver will get 410 on its next download request.
    let _ = job;
    Ok(())
}

/// Receiver side: accept an invite and start the download loop.
pub fn accept_invite(state: Arc<AppState>, invite_id: &str) -> Result<(), String> {
    let (invite, peer) = {
        let inbound_guard = state.push.inbound.lock();
        let inv = inbound_guard.get(invite_id).ok_or_else(|| "推送不存在或已超时".to_string())?;
        let sender_id = inv.sender.id.clone();
        let sender_address = state.peers.lock().iter()
            .find(|p| p.id == sender_id)
            .map(|p| (p.ip.clone(), p.port))
            .ok_or_else(|| "发送方信息缺失".to_string())?;
        let invite = push_job::InboundInvite {
            invite_id: inv.invite_id.clone(),
            sender: inv.sender.clone(),
            name: inv.name.clone(),
            size: inv.size,
            is_dir: inv.is_dir,
            files: inv.files.clone(),
        };
        // Don't remove from inbound yet — the download loop checks it for cancellation.
        (invite, (sender_id, sender_address))
    };

    push_job::spawn_download(state.clone(), invite, peer.0, (peer.1).0, (peer.1).1);
    Ok(())
}

/// Receiver side: reject an invite.
pub fn reject_invite(state: &AppState, invite_id: &str) {
    let removed = state.push.remove_invite_if_present(invite_id);
    if removed {
        ws::emit_ui(state, ws::events::ui::PUSH_RESPONSE, &serde_json::json!({
            "inviteId": invite_id,
            "accepted": false,
        }));
    }
}

/// Receiver side: cancel an in-flight download (keep resume record).
pub fn cancel_receive(state: &AppState, invite_id: &str) {
    state.push.remove_invite(invite_id);
    ws::emit_ui(state, ws::events::ui::PUSH_DONE, &serde_json::json!({
        "inviteId": invite_id,
        "status": "cancelled",
    }));
    warn!(%invite_id, "receive cancelled; resume record kept");
}