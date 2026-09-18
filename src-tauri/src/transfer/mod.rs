pub mod push_job;
pub mod resume;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use tracing::{info, warn};

use crate::credential;
use crate::state::AppState;
use crate::ws;
use crate::ws::events::{FileEntry, PeerInfo, WsEvent};

use push_job::{InboundInvite, PushJob, PushStatus};

/// 一条邀请在接收方屏幕上停留多久：到点自动拒绝。
///
/// 发送方在等（HTTP 邀请是同步等回执的），接收方这边没人管的话，那条邀请会一直挂在
/// 通知列表里，而发送方早就超时走人了 —— 两边状态必须同时作废。
pub const INVITE_TTL: Duration = Duration::from_secs(60);

/// 发送方等 HTTP 邀请回执的时限：比 `INVITE_TTL` 多一点余量，让"对方没点、自动拒绝"
/// 这条正常路径能走到回执，而不是本机先断在网上。
const HTTP_INVITE_TIMEOUT: Duration = Duration::from_secs(75);

/// 长连接那条路上没有同步回执，等不到人处理就作废（P1 原有行为）。
const WS_INVITE_TTL: Duration = Duration::from_secs(300);

// ---------------------------------------------------------------------------
// Source manifest
// ---------------------------------------------------------------------------

/// 一份待推送的源：清单 + **每条清单路径对应的本机绝对路径**（F-50 / F-52）。
///
/// 清单里的相对路径**就是落盘后的样子**（`A.txt` / `D/…` / `甲目录/…` + `散文件.txt`），
/// 对端只把它接到 `<下载目录>/<发送方名>/` 后面；`sources` 是本机侧的翻译表。
/// 多选推送之所以能落成"多文件就是多文件"，靠的就是这两件事：发送方在生成时就决定了
/// 每条路径的顶层名字（各源自己的名字），对端不需要再猜、也不再套"推送名"那一层。
pub struct PushSource {
    /// 只用于显示（通知/卡片里"推的是什么"）；**不参与落盘路径**。
    pub name: String,
    pub files: Vec<FileEntry>,
    /// 清单相对路径 → 本机绝对路径（与 `files` 一一对应）。
    pub sources: Vec<(String, PathBuf)>,
    pub total: u64,
    /// 清单是否**权威**（下载端点按清单取文件）。单文件推送是唯一的例外：
    /// 对端只有一条名字，没有清单。
    pub is_dir: bool,
    /// 用户这次选中了几项（1 = 单文件/单目录；>1 = 多选）。
    pub count: usize,
}

/// 把用户选中的一批文件/目录整理成推送清单。
///
/// **清单里的路径就是"对端落盘后的样子"**：每个源以**自己的名字**当顶层路径 ——
/// 单文件的 `A.txt`、单目录的 `D/…`、多选的 `甲目录/…` + `散文件.txt`；对端只把它们
/// 原样落到 `<下载目录>/<发送方名>/` 下面。**没有"推送名子目录"这一层**：
/// `A.txt/A.txt`、`3 项/a.txt` 那种都是多余嵌套 —— 直接传多文件就是多文件，不是在目录里。
///
/// `name` 只剩下显示用途（通知/卡片里"推的是什么"）：单源 = 它自己，
/// 多源 = 这批源的共同父目录名，跨目录/跨盘取不到就用「N 项」。
pub async fn manifest_for_paths(paths: &[PathBuf]) -> Result<PushSource, String> {
    if paths.is_empty() {
        return Err("没有可推送的文件".into());
    }

    let name = if paths.len() == 1 {
        file_name_of(&paths[0])
    } else {
        common_parent_name(paths).unwrap_or_else(|| format!("{} 项", paths.len()))
    };

    let mut files: Vec<FileEntry> = Vec::new();
    let mut sources: Vec<(String, PathBuf)> = Vec::new();
    let mut total = 0u64;
    let mut used: Vec<String> = Vec::new();
    let mut single_file = false;

    for path in paths {
        let meta = tokio::fs::metadata(path)
            .await
            .map_err(|e| format!("无法访问 {}: {e}", path.display()))?;
        let base = file_name_of(path);
        // 同名去重：`C:\a\报告.txt` 与 `D:\b\报告.txt` 不能在清单里撞成同一条路径
        // （Windows 的文件名比较本来就大小写不敏感，比对时照此办理）。
        let mut prefix = base.clone();
        let mut n = 2;
        while used.iter().any(|u| u.eq_ignore_ascii_case(&prefix)) {
            prefix = dedupe_name(&base, n);
            n += 1;
        }
        used.push(prefix.clone());

        if meta.is_file() {
            single_file = paths.len() == 1;
            files.push(FileEntry { path: prefix.clone(), size: meta.len() });
            sources.push((prefix, path.clone()));
            total += meta.len();
        } else {
            let (sub, sub_total) = collect_dir_manifest(path).await?;
            for f in sub {
                let rel = format!("{prefix}/{}", f.path);
                sources.push((rel.clone(), path.join(&f.path)));
                files.push(FileEntry { path: rel, size: f.size });
            }
            total += sub_total;
        }
    }

    Ok(PushSource {
        name,
        files,
        sources,
        total,
        // 单文件推送没有清单可言（对端只有一条名字，见 `spawn_download`）——
        // 这就是 `is_dir` 的真正含义：**清单是否权威**。
        is_dir: !single_file,
        count: paths.len(),
    })
}

fn file_name_of(path: &Path) -> String {
    path.file_name()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "未命名".into())
}

/// 同名源的去重名：编号插在扩展名**前面**（`报告.txt` → `报告 (2).txt`），
/// 扩展名保持不变 —— 对端落盘后仍然是"同一种文件"。
fn dedupe_name(base: &str, n: usize) -> String {
    match base.rsplit_once('.') {
        Some((stem, ext)) if !stem.is_empty() && !ext.is_empty() => format!("{stem} ({n}).{ext}"),
        _ => format!("{base} ({n})"),
    }
}

/// 这批路径的共同父目录名（都同一个父目录才算；跨盘/跨目录返回 None）。
fn common_parent_name(paths: &[PathBuf]) -> Option<String> {
    let first = paths.first()?.parent()?;
    if paths.iter().skip(1).any(|p| p.parent() != Some(first)) {
        return None;
    }
    first.file_name().map(|s| s.to_string_lossy().to_string())
}

/// 一个目录下的**非空文件**清单（相对该目录，'/' 分隔）。
async fn collect_dir_manifest(dir: &Path) -> Result<(Vec<FileEntry>, u64), String> {
    let mut files = Vec::new();
    let mut total = 0u64;
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let mut rd = tokio::fs::read_dir(&current).await.map_err(|e| format!("读取目录失败: {e}"))?;
        while let Some(entry) = rd.next_entry().await.map_err(|e| format!("读取目录失败: {e}"))? {
            let path = entry.path();
            let meta = entry.metadata().await.map_err(|e| format!("元数据失败: {e}"))?;
            if meta.is_dir() {
                stack.push(path);
            } else {
                let rel = path
                    .strip_prefix(dir)
                    .map(|p| p.to_string_lossy().replace('\\', "/"))
                    .unwrap_or_default();
                total += meta.len();
                files.push(FileEntry { path: rel, size: meta.len() });
            }
        }
    }
    Ok((files, total))
}

// ---------------------------------------------------------------------------
// Sender side
// ---------------------------------------------------------------------------

/// 发送方：把本机某个文件/目录推给某台设备（F-50）。
///
/// 通道按对端状态选（`docs/p1-implementation.md` §7）：
/// - 长连接 → WS 事件 `push_invite`；
/// - 其余在线设备（已配对未连、未绑定）→ HTTP `POST /push/invite`。
///
/// 推送**不要求预先配对**：邀请就是一次普通 HTTP 请求，接收方同意才传输，
/// 同意制兜底安全（未绑定时下载端点靠邀请 id 本身当能力，见 `routes_push`）。
///
/// 返回邀请 id；对方同意与否走 `push://response` / `push://done` 事件回来 ——
/// 两条通道对界面是同一个形状，UI 不需要知道邀请是怎么送出去的。
pub async fn push_paths(
    state: Arc<AppState>,
    paths: Vec<String>,
    peer_id: &str,
) -> Result<String, String> {
    let peer = state
        .get_peer(peer_id)
        .ok_or_else(|| format!("设备不存在: {peer_id}"))?;
    // F-53：目标不在网上就地失败，不排队。
    if !peer.online || peer.ip.is_empty() || peer.port == 0 {
        return Err(format!("「{}」不在网上，推送未发送", peer.name));
    }

    let roots: Vec<PathBuf> = paths.iter().map(PathBuf::from).collect();
    let source = manifest_for_paths(&roots).await?;
    if source.files.is_empty() {
        return Err("没有可推送的文件".into());
    }

    let invite_id = uuid::Uuid::new_v4().to_string();
    let (device_id, self_name) = {
        let config = state.config.lock();
        (config.device_id.clone(), config.device_name.clone())
    };
    let server_port = state.server_port.load(std::sync::atomic::Ordering::SeqCst);
    let paired = peer.paired;

    state.push.insert_job(PushJob::new(
        invite_id.clone(),
        peer_id.to_string(),
        peer.name.clone(),
        source.name.clone(),
        source.is_dir,
        source.files.clone(),
        source.sources.clone(),
        source.total,
        paired,
    ));

    ws::emit_ui(&state, ws::events::ui::PUSH_SENT, &serde_json::json!({
        "inviteId": invite_id,
        "peerId": peer_id,
        "target": peer.name,
        "name": source.name,
        "total": source.total,
        "isDir": source.is_dir,
        "count": source.count,
    }));

    let event = WsEvent::PushInvite {
        invite_id: invite_id.clone(),
        sender: PeerInfo { id: device_id, name: self_name },
        name: source.name.clone(),
        size: source.total,
        is_dir: source.is_dir,
        count: source.count,
        files: source.files.clone(),
    };

    let state2 = state.clone();
    let id2 = invite_id.clone();
    let peer_name = peer.name.clone();
    let peer_id = peer_id.to_string();
    crate::task::spawn("push invite", async move {
        if state2.ws.has_connection(&peer_id) {
            if state2.ws.send(&peer_id, event) {
                schedule_ws_invite_timeout(state2.clone(), id2.clone());
            } else {
                fail_invite(&state2, &id2, "对方离线");
            }
            return;
        }
        match deliver_invite_http(&peer, server_port, &event, paired).await {
            Ok(true) => {
                state2.push.update_status(&id2, PushStatus::Accepted);
                ws::emit_ui(&state2, ws::events::ui::PUSH_RESPONSE, &serde_json::json!({
                    "inviteId": id2,
                    "accepted": true,
                }));
            }
            Ok(false) => {
                state2.push.remove_job(&id2);
                ws::emit_ui(&state2, ws::events::ui::PUSH_RESPONSE, &serde_json::json!({
                    "inviteId": id2,
                    "accepted": false,
                }));
                ws::emit_ui(&state2, ws::events::ui::PUSH_DONE, &serde_json::json!({
                    "inviteId": id2,
                    "status": "failed",
                    "reason": format!("「{peer_name}」拒绝了这次推送"),
                }));
            }
            Err(reason) => fail_invite(&state2, &id2, &reason),
        }
    });

    Ok(invite_id)
}

/// 邀请没能送到（对方不在网上/请求失败）：清掉 job 并让界面给出失败提示。
fn fail_invite(state: &AppState, invite_id: &str, reason: &str) {
    state.push.remove_job(invite_id);
    ws::emit_ui(state, ws::events::ui::PUSH_DONE, &serde_json::json!({
        "inviteId": invite_id,
        "status": "failed",
        "reason": reason,
    }));
}

/// 长连接邀请没人处理就作废（发送方等 5 分钟）。
fn schedule_ws_invite_timeout(state: Arc<AppState>, invite_id: String) {
    crate::task::spawn("push invite timeout", async move {
        tokio::time::sleep(WS_INVITE_TTL).await;
        if state.push.job_status(&invite_id).as_deref() == Some("invited") {
            info!(%invite_id, "invite timed out, cancelling");
            state.push.remove_job(&invite_id);
            ws::emit_ui(&state, ws::events::ui::PUSH_DONE, &serde_json::json!({
                "inviteId": invite_id,
                "status": "failed",
                "reason": "对方未响应（5 分钟超时）",
            }));
        }
    });
}

/// 把邀请 POST 到对端的 `/push/invite`。返回对方是否同意。
///
/// 请求体就是 WS 那条路的 `push_invite` 事件（同一个序列化结构，多带一个 `port`）——
/// 两条通道说的是同一套协议，接收方只有一份解析代码。`port` 是本机**服务端口**：
/// 这个 HTTP 请求的源端口是一次性的，对端要回执（进度/完成）得发回真正的服务端口。
async fn deliver_invite_http(
    peer: &crate::config::PeerDevice,
    server_port: u16,
    event: &WsEvent,
    paired: bool,
) -> Result<bool, String> {
    let mut body = serde_json::to_value(event).map_err(|e| format!("邀请序列化失败: {e}"))?;
    body["port"] = serde_json::json!(server_port);

    let url = format!("http://{}:{}/push/invite", peer.ip, peer.port);
    let mut req = reqwest::Client::new()
        .post(&url)
        .timeout(HTTP_INVITE_TIMEOUT)
        .json(&body);
    if paired {
        if let Ok(Some(token)) = credential::get_peer_token(&peer.id) {
            req = req.bearer_auth(token);
        }
    }

    let resp = req
        .send()
        .await
        .map_err(|e| format!("联系不上「{}」：{e}", peer.name))?;
    if !resp.status().is_success() {
        return Err(format!(
            "「{}」没能接收这次推送（HTTP {}）",
            peer.name,
            resp.status().as_u16()
        ));
    }
    let decision: InviteDecision = resp
        .json()
        .await
        .map_err(|e| format!("「{}」的回应无法解析：{e}", peer.name))?;
    Ok(decision.accepted)
}

#[derive(serde::Deserialize)]
struct InviteDecision {
    accepted: bool,
}

/// 发送方：取消一条推送（F-54）。
///
/// 对端必须知道，两个阶段各有各的理由：
/// - 邀请还挂着（对方还没点）→ 那条邀请卡必须撤掉，否则它一直挂在对面通知列表里，
///   点「接收」才发现早就没了；
/// - 已经在传 → 下载端点会当场断流、下一次请求拿到 410，收手更快。
pub fn cancel_push(state: &AppState, invite_id: &str) -> Result<(), String> {
    let job = state
        .push
        .job(invite_id)
        .ok_or_else(|| "推送不存在".to_string())?;
    state.push.remove_job(invite_id);
    notify_peer(state, &job.peer_id, WsEvent::PushDone {
        invite_id: invite_id.to_string(),
        status: "cancelled".into(),
        reason: Some("发送方已取消".into()),
    });
    ws::emit_ui(state, ws::events::ui::PUSH_DONE, &serde_json::json!({
        "inviteId": invite_id,
        "status": "cancelled",
    }));
    Ok(())
}

/// 发送方给对端捎一条推送状态（目前只有「取消」）：长连接走 WS，否则 POST 它的 `/push/report`。
///
/// 地址取自地址簿 —— 发送方手里没有对端的回调地址，但推送本来就要求对端在线（有地址）。
pub fn notify_peer(state: &AppState, peer_id: &str, event: WsEvent) {
    let callback = state
        .get_peer(peer_id)
        .and_then(|p| (!p.ip.is_empty() && p.port != 0).then(|| (p.ip.clone(), p.port)));
    push_job::report_to_sender(state, peer_id, &callback, event);
}

// ---------------------------------------------------------------------------
// Receiver side
// ---------------------------------------------------------------------------

/// 接收方：邀请到点没被处理就自动拒绝（两条通道共用）。
///
/// 发送方在等回执：HTTP 那条路上等的是这个 oneshot，长连接那条路上等的是
/// `push_response` —— 所以这里两个出口都要走到。
pub fn schedule_invite_timeout(state: Arc<AppState>, invite_id: String) {
    crate::task::spawn("push invite decision timeout", async move {
        tokio::time::sleep(INVITE_TTL).await;
        let Some(invite) = state.push.take_invite(&invite_id) else {
            return; // 已经处理过了
        };
        info!(%invite_id, "invite expired, auto-rejecting");
        reject_shared(&state, invite, "超时未处理");
    });
}

/// 接收方：同意一条邀请 → 开始拉取（F-52）。
pub fn accept_invite(state: Arc<AppState>, invite_id: &str) -> Result<(), String> {
    let (invite, sender_id) = {
        let mut inbound = state.push.inbound.lock();
        let inv = inbound
            .get_mut(invite_id)
            .ok_or_else(|| "推送不存在或已超时".to_string())?;
        // 下载要连的地址：邀请带来的地址，或现从地址簿里补一个 —— 补不上就不能开始
        // （缺了它只会空转到天荒地老），也就还没到"放行"的时候。
        let callback = inv
            .callback
            .clone()
            .or_else(|| {
                state
                    .get_peer(&inv.sender.id)
                    .and_then(|p| (!p.ip.is_empty() && p.port != 0).then(|| (p.ip.clone(), p.port)))
            })
            .ok_or_else(|| "发送方地址未知，无法接收".to_string())?;
        if let Some(tx) = inv.responder.take() {
            // 放行发送方；但**保留**这条邀请：下载循环拿它当"没被取消"的标志。
            let _ = tx.send(true);
        }
        (
            InboundInvite {
                invite_id: inv.invite_id.clone(),
                sender: inv.sender.clone(),
                name: inv.name.clone(),
                size: inv.size,
                is_dir: inv.is_dir,
                count: inv.count,
                files: inv.files.clone(),
                callback: Some(callback),
                responder: None,
            },
            inv.sender.id.clone(),
        )
    };

    push_job::spawn_download(state, invite, sender_id);
    Ok(())
}

/// 接收方：拒绝一条邀请 —— 界面上的按钮走这里。
pub fn reject_invite(state: &AppState, invite_id: &str) {
    reject_invite_with(state, invite_id, "对方拒绝");
}

/// 同上，但把原因带到本机那条通知上。
///
/// 自动放行（已绑定设备）失败时用它：那种场合**没有人"拒绝"**，是本机收不下
/// （地址未知之类）—— 原因照实说，别让用户以为对面点了拒绝。
pub fn reject_invite_with(state: &AppState, invite_id: &str, reason: &str) {
    let Some(mut invite) = state.push.take_invite(invite_id) else {
        return;
    };
    if let Some(tx) = invite.responder.take() {
        let _ = tx.send(false);
    }
    reject_shared(state, invite, reason);
}

/// 拒绝的共同出口：回执 + 本机通知列表撤掉这张卡。
fn reject_shared(state: &AppState, invite: InboundInvite, reason: &str) {
    if state.ws.has_connection(&invite.sender.id) {
        ws::send_to_peer(
            state,
            &invite.sender.id,
            WsEvent::PushResponse { invite_id: invite.invite_id.clone(), accepted: false },
        );
    }
    ws::emit_ui(state, ws::events::ui::PUSH_RESPONSE, &serde_json::json!({
        "inviteId": invite.invite_id,
        "accepted": false,
        "reason": reason,
    }));
}

/// 接收方：取消正在进行的接收（保留续传记录，下次可接着拉）。
pub fn cancel_receive(state: &AppState, invite_id: &str) {
    let Some(invite) = state.push.take_invite(invite_id) else {
        return;
    };
    // 下载循环发现邀请没了就会收手；这里替它把话带到发送方 ——
    // 否则对面永远停在"正在传输"，那条 job 没有超时兜底。
    push_job::report_to_sender(
        state,
        &invite.sender.id,
        &invite.callback,
        WsEvent::PushDone {
            invite_id: invite.invite_id.clone(),
            status: "failed".into(),
            reason: Some("接收方已取消".into()),
        },
    );
    ws::emit_ui(state, ws::events::ui::PUSH_DONE, &serde_json::json!({
        "inviteId": invite.invite_id,
        "status": "cancelled",
    }));
    warn!(%invite_id, "receive cancelled; resume record kept");
}