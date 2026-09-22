pub mod push_job;
pub mod resume;

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

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
    /// 清单里**被跳过的项数**（链接、读不到的项）。>0 时界面必须说出来 —— 清单少了东西却
    /// 不说，用户会以为"就这些"然后推漏（同一份道理见 `fs_browse::list_dir`）。
    pub skipped: usize,
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
    let mut skipped = 0usize;
    let mut used: Vec<String> = Vec::new();
    let mut single_file = false;
    // 算一次：`data_dir()` 要读环境变量，别在走查里每个文件项都问一遍。
    let own_data_dir = crate::config::data_dir();

    for path in paths {
        let base = file_name_of(path);
        // 顶层源是用户**明确点的**：链接/系统目录/自家数据目录一律**当场报错**，不静默跳过 ——
        // 静默跳到最后一个文件都不剩，用户只会看到一句"没有可推送的文件"，根本不知道自己
        // 做错了什么。（走查里遇到的那些才跳过 + 计数：那是清单少一项，不是选错了源。）
        // 用 `symlink_metadata`：必须分辨"这一项本身是链接"，`metadata` 会跟过去看目标。
        let meta = tokio::fs::symlink_metadata(path)
            .await
            .map_err(|e| format!("无法访问 {}: {e}", path.display()))?;
        if crate::unsendable::is_link(&meta.file_type()) {
            return Err(format!("「{base}」是一个链接，不能作为推送源"));
        }
        if crate::unsendable::is_system_name(&base, crate::unsendable::sits_in_volume_root(path)) {
            return Err(format!("「{base}」是系统自己的目录/文件，不能推送"));
        }
        if crate::unsendable::is_own_data_dir(path, &own_data_dir) {
            return Err("OrbDrop 自己的数据目录不能推送（里面是本机的设置与凭据）".into());
        }
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
            let (sub, sub_total, sub_skipped) = collect_dir_manifest(path).await?;
            skipped += sub_skipped;
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
        skipped,
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

/// 一个目录下的**非空文件**清单（相对该目录，'/' 分隔）+ 总字节 + **跳过的项数**。
///
/// 跳过谁、为什么不整条失败：
/// - **链接**（符号链接/junction）：会回环（`%LOCALAPPDATA%\Application Data` 指向自己的
///   父目录，几百层地重复同一棵树），也会把同一份内容按两条路径列两遍；
/// - **读不到的项**（ACL 拒绝、正好被删、网络盘中途掉线）：一个子目录挡不住整批。
///
/// 这两类都**计数**回报给界面 —— 清单少了东西却不说，用户会以为"就这些"然后推漏（这就是
/// `list_dir` 那条"静默少几项比报错更坏"的另一面）。计数的是**项**：一个链接、一个读不到的
/// 目录各算一项，不是它里面有多少个文件。
///
/// 系统自己的名字（`System Volume Information`、`pagefile.sys`…）**不进这个计数**：选择框
/// 里本来就不列它们，用户没指望过；推 `C:\` 时天天弹一句"跳过了 20 项"只会把真话淹掉。
/// 顶层源撞上它们则是另一回事 —— 那是用户明确点的，`manifest_for_paths` 当场报错。
///
/// **根目录自己**读不到仍然整条失败：那是用户点的那个源，界面得说出"这个目录读不了"。
async fn collect_dir_manifest(dir: &Path) -> Result<(Vec<FileEntry>, u64, usize), String> {
    let mut files = Vec::new();
    let mut total = 0u64;
    let mut skipped = 0usize;
    let own_data_dir = crate::config::data_dir();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let mut rd = match tokio::fs::read_dir(&current).await {
            Ok(rd) => rd,
            Err(e) if current == dir => return Err(format!("读取目录失败: {e}")),
            Err(_) => {
                skipped += 1;
                continue;
            }
        };
        // 这一层先收在局部，成功了再并进总表：中途列举失败 = 这一层整个不可信，已经收到的
        // 那半份宁可全丢（当作整层跳过）—— 交出一份"少了一半"的清单正是上面要防的事。
        let mut local: Vec<FileEntry> = Vec::new();
        let mut local_total = 0u64;
        let mut subdirs: Vec<PathBuf> = Vec::new();
        let mut intact = true;
        loop {
            let entry = match rd.next_entry().await {
                Ok(Some(entry)) => entry,
                Ok(None) => break,
                Err(_) => {
                    intact = false;
                    break;
                }
            };
            let path = entry.path();
            let name = entry.file_name().to_string_lossy().to_string();
            let Ok(ft) = entry.file_type().await else {
                skipped += 1;
                continue;
            };
            if crate::unsendable::is_link(&ft) || crate::unsendable::is_own_data_dir(&path, &own_data_dir) {
                skipped += 1;
                continue;
            }
            if crate::unsendable::is_system_name(&name, crate::unsendable::sits_in_volume_root(&path)) {
                continue;
            }
            if ft.is_dir() {
                subdirs.push(path);
                continue;
            }
            let Ok(meta) = entry.metadata().await else {
                skipped += 1;
                continue;
            };
            let rel = path
                .strip_prefix(dir)
                .map(|p| p.to_string_lossy().replace('\\', "/"))
                .unwrap_or_default();
            local_total += meta.len();
            local.push(FileEntry { path: rel, size: meta.len() });
        }
        if !intact {
            skipped += 1;
            continue;
        }
        files.extend(local);
        total += local_total;
        stack.extend(subdirs);
    }
    Ok((files, total, skipped))
}

// ---------------------------------------------------------------------------
// Sender side
// ---------------------------------------------------------------------------

/// 发送方：把本机一批文件/目录推给**一台或多台**设备（F-50）。
///
/// 一次操作 = 一批，只多两件事，其余与单台推送完全一致：
/// - **清单只建一次**（目录只走一遍，各台拿到的清单与同名去重编号一模一样）；
/// - 每台各有一条独立的邀请/job —— 接收侧因此毫无差别（已配对不问同意、未绑定弹卡等点头、
///   60s 超时、双向取消、断点续传全部照旧），批次只多一张聚合卡片 + **失败隔离**
///   （一台送不到不影响其余几台；但一台都送不到时整批就地失败，等价于单台那条 F-53）。
///
/// 通道按对端状态逐台选（`docs/p1-implementation.md` §7）：
/// - 长连接 → WS 事件 `push_invite`；其余在线设备（已配对未连、未绑定）→ HTTP `POST /push/invite`。
///
/// 返回整批快照；界面同时还会收到 `push://batch` —— 同一份数据，界面按 `batchId` 合并。
pub async fn push_paths(
    state: Arc<AppState>,
    paths: Vec<String>,
    peer_ids: Vec<String>,
) -> Result<push_job::BatchView, String> {
    // 同一台设备勾两次只发一条（保持勾选顺序）。
    let mut ids: Vec<String> = Vec::new();
    for id in peer_ids {
        if !ids.contains(&id) {
            ids.push(id);
        }
    }
    if ids.is_empty() {
        return Err("没有选择目标设备".into());
    }

    let roots: Vec<PathBuf> = paths.iter().map(PathBuf::from).collect();
    let source = manifest_for_paths(&roots).await?;
    if source.files.is_empty() {
        return Err("没有可推送的文件".into());
    }

    let (device_id, self_name) = {
        let config = state.config.lock();
        (config.device_id.clone(), config.device_name.clone())
    };
    let server_port = state.server_port.load(std::sync::atomic::Ordering::SeqCst);
    let batch_id = uuid::Uuid::new_v4().to_string();

    // 先给每台定一行：不在网上的这台**就地失败、不排队**（F-53），能派的进 `pending` 去发邀请。
    let mut targets: Vec<push_job::BatchTarget> = Vec::new();
    let mut pending: Vec<(crate::config::PeerDevice, String)> = Vec::new();
    let mut first_reason: Option<String> = None;
    for peer_id in &ids {
        let Some(peer) = state.get_peer(peer_id) else {
            let reason = format!("设备不存在: {peer_id}");
            first_reason.get_or_insert(reason.clone());
            targets.push(push_job::BatchTarget {
                peer_id: peer_id.clone(),
                peer_name: peer_id.clone(),
                invite_id: String::new(),
                paired: false,
                state: push_job::TargetState::Failed,
                received: 0,
                error: Some(reason),
            });
            continue;
        };
        if !peer.online || peer.ip.is_empty() || peer.port == 0 {
            let reason = format!("「{}」不在网上，推送未发送", peer.name);
            first_reason.get_or_insert(reason.clone());
            targets.push(push_job::BatchTarget {
                peer_id: peer.id.clone(),
                peer_name: peer.name.clone(),
                invite_id: String::new(),
                paired: peer.paired,
                state: push_job::TargetState::Failed,
                received: 0,
                error: Some(reason),
            });
            continue;
        }
        let invite_id = uuid::Uuid::new_v4().to_string();
        state.push.insert_job(PushJob::new(
            invite_id.clone(),
            batch_id.clone(),
            peer.id.clone(),
            peer.name.clone(),
            source.name.clone(),
            source.is_dir,
            source.sources.clone(),
            peer.paired,
        ));
        targets.push(push_job::BatchTarget {
            peer_id: peer.id.clone(),
            peer_name: peer.name.clone(),
            invite_id: invite_id.clone(),
            paired: peer.paired,
            state: push_job::TargetState::Invited,
            received: 0,
            error: None,
        });
        pending.push((peer, invite_id));
    }

    if pending.is_empty() {
        // 一台都派不出去 = 单台推送那条"就地失败"（F-53）：界面弹一条提示，不建卡片。
        // 单台时沿用原来那句原话，多台时给一句能概括的。
        return Err(if ids.len() == 1 {
            first_reason.unwrap_or_else(|| "推送未发送".into())
        } else {
            format!("所选 {} 台设备都收不了（不在网上或设备不存在），推送未发送", ids.len())
        });
    }

    let view = state.push.insert_batch(push_job::PushBatch {
        id: batch_id.clone(),
        name: source.name.clone(),
        count: source.count,
        skipped: source.skipped,
        per_target_total: source.total,
        targets,
        last_emit: Instant::now(),
    });
    if let Ok(value) = serde_json::to_value(&view) {
        ws::emit_ui(&state, ws::events::ui::PUSH_BATCH, &value);
    }

    for (peer, invite_id) in pending {
        let event = WsEvent::PushInvite {
            invite_id: invite_id.clone(),
            sender: PeerInfo { id: device_id.clone(), name: self_name.clone() },
            name: source.name.clone(),
            size: source.total,
            is_dir: source.is_dir,
            count: source.count,
            files: source.files.clone(),
        };
        spawn_invite_delivery(state.clone(), peer, invite_id, event, server_port);
    }

    Ok(view)
}

/// 把一条邀请送出去：长连接走 WS，其余在线设备走 HTTP `POST /push/invite`。
///
/// 一台一条任务：任何一台的失败都只落在它自己那一行上（`batch_set(.., Failed, ..)`），
/// 牵不动整批 —— 推 5 台、其中 1 台掉线，另外 4 台照走。
fn spawn_invite_delivery(
    state: Arc<AppState>,
    peer: crate::config::PeerDevice,
    invite_id: String,
    event: WsEvent,
    server_port: u16,
) {
    let peer_id = peer.id.clone();
    let peer_name = peer.name.clone();
    let paired = peer.paired;
    crate::task::spawn("push invite", async move {
        if state.ws.has_connection(&peer_id) {
            if state.ws.send(&peer_id, event) {
                schedule_ws_invite_timeout(state.clone(), invite_id.clone());
            } else {
                fail_invite(&state, &invite_id, "对方离线");
            }
            return;
        }
        match deliver_invite_http(&peer, server_port, &event, paired).await {
            Ok(true) => {
                state.push.update_status(&invite_id, PushStatus::Accepted);
                ws::emit_ui(&state, ws::events::ui::PUSH_RESPONSE, &serde_json::json!({
                    "inviteId": invite_id,
                    "accepted": true,
                }));
            }
            Ok(false) => {
                state.push.remove_job(&invite_id);
                push_job::batch_set(
                    &state,
                    &invite_id,
                    push_job::TargetState::Rejected,
                    None,
                    Some("对方拒绝了这次推送".into()),
                    true,
                );
                ws::emit_ui(&state, ws::events::ui::PUSH_RESPONSE, &serde_json::json!({
                    "inviteId": invite_id,
                    "accepted": false,
                }));
                ws::emit_ui(&state, ws::events::ui::PUSH_DONE, &serde_json::json!({
                    "inviteId": invite_id,
                    "status": "failed",
                    "reason": format!("「{peer_name}」拒绝了这次推送"),
                }));
            }
            Err(reason) => fail_invite(&state, &invite_id, &reason),
        }
    });
}

/// 邀请没能送到（对方不在网上/请求失败）：清掉 job 并把这一台标成失败。
fn fail_invite(state: &AppState, invite_id: &str, reason: &str) {
    state.push.remove_job(invite_id);
    push_job::batch_set(
        state,
        invite_id,
        push_job::TargetState::Failed,
        None,
        Some(reason.to_string()),
        true,
    );
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
            push_job::batch_set(
                &state,
                &invite_id,
                push_job::TargetState::Failed,
                None,
                Some("对方未响应（5 分钟超时）".into()),
                true,
            );
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

    let url = format!(
        "{}/push/invite",
        crate::net::peer_base(&peer.ip, peer.port)
    );
    let mut req = crate::net::client()
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
    push_job::batch_set(
        state,
        invite_id,
        push_job::TargetState::Cancelled,
        None,
        Some("已取消".into()),
        true,
    );
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

/// 发送方：取消一整批推送 —— 每台各撤一条（两个阶段各自的理由见 `cancel_push`）。
///
/// 单台的失败（对端已经不在网上、这条早就结束了）**不该挡住其余几台的取消**，
/// 所以逐台各自尽力、结果不带出来（对端那条邀请照样会因为它自己的 60s 超时作废）。
pub fn cancel_push_batch(state: &AppState, batch_id: &str) -> Result<(), String> {
    let targets = state
        .push
        .batch_targets(batch_id)
        .ok_or_else(|| "推送不存在".to_string())?;
    for target in targets {
        if target.state.is_waiting() {
            let _ = cancel_push(state, &target.invite_id);
        }
    }
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

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("orbdrop_manifest_{tag}_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    /// 链接不进去、也不数它里面有多少东西：junction 回环会让走查把同一棵树翻几百遍
    /// （`%LOCALAPPDATA%\Application Data` 就是指向自己父目录的那种），
    /// 而"跳过了 N 项"里的 N 数的是**项**（这个链接本身），不是它背后的文件数。
    #[tokio::test]
    async fn links_are_skipped_not_followed() {
        let dir = temp_dir("link");
        std::fs::create_dir_all(dir.join("真").join("深")).unwrap();
        std::fs::write(dir.join("真").join("深").join("a.txt"), b"hi").unwrap();
        std::fs::write(dir.join("散.txt"), b"xy").unwrap();
        let link = dir.join("回环");
        // 指向自己的父目录 = 真正的回环：跟随它，走查就永远走不完。
        if !crate::unsendable::make_link_for_test(&link, &dir) {
            eprintln!("这台机器造不出链接，跳过这条");
            std::fs::remove_dir_all(&dir).unwrap();
            return;
        }

        let (files, total, skipped) = collect_dir_manifest(&dir).await.unwrap();
        let mut names: Vec<&str> = files.iter().map(|f| f.path.as_str()).collect();
        names.sort();
        assert_eq!(names, vec!["散.txt", "真/深/a.txt"], "只该有真实文件，没有回环翻出来的副本");
        assert_eq!(total, 4);
        assert_eq!(skipped, 1, "跳过的是那个链接本身");

        std::fs::remove_dir(&link).unwrap_or_else(|_| std::fs::remove_file(&link).unwrap());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 顶层源是用户**明确点的**：是链接就当场报错，别静默跳成一个空清单
    /// （那样用户只会看到"没有可推送的文件"，根本不知道自己选错了什么）。
    #[tokio::test]
    async fn top_level_link_source_is_refused() {
        let dir = temp_dir("toplink");
        std::fs::create_dir_all(dir.join("真")).unwrap();
        let link = dir.join("链接");
        if !crate::unsendable::make_link_for_test(&link, &dir.join("真")) {
            eprintln!("这台机器造不出链接，跳过这条");
            std::fs::remove_dir_all(&dir).unwrap();
            return;
        }

        let Err(err) = manifest_for_paths(&[link.clone()]).await else {
            panic!("顶层是链接的源必须当场拒绝");
        };
        assert!(err.contains("链接"), "该说清是链接的问题，实际：{err}");

        std::fs::remove_dir(&link).unwrap_or_else(|_| std::fs::remove_file(&link).unwrap());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 卷根上的系统文件（`pagefile.sys`…）不能被当成推送源 —— 它读不了，也不该送。
    #[cfg(windows)]
    #[tokio::test]
    async fn root_level_system_file_is_refused() {
        let pagefile = PathBuf::from("C:\\pagefile.sys");
        if tokio::fs::symlink_metadata(&pagefile).await.is_err() {
            return; // 这台机器关掉了分页文件：没有可测的
        }
        let Err(err) = manifest_for_paths(&[pagefile]).await else {
            panic!("卷根上的系统文件必须当场拒绝");
        };
        assert!(err.contains("系统"), "该说清是系统自己的东西，实际：{err}");
    }

    /// 读不到的项**跳过 + 计数**，不能把整批拖垮：用户的目录树里有一个打不开的
    /// 子目录是常事（权限、刚好被删），为此拒绝整批等于什么都传不了。
    #[cfg(unix)]
    #[tokio::test]
    async fn unreadable_subdir_is_skipped_and_counted() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("denied");
        std::fs::create_dir_all(dir.join("看得见")).unwrap();
        std::fs::write(dir.join("看得见").join("a.txt"), b"x").unwrap();
        let shut = dir.join("打不开");
        std::fs::create_dir_all(&shut).unwrap();
        std::fs::set_permissions(&shut, std::fs::Permissions::from_mode(0o000)).unwrap();

        let (files, _, skipped) = collect_dir_manifest(&dir).await.unwrap();
        assert_eq!(files.len(), 1, "看得见的那份照常进清单");
        assert_eq!(skipped, 1, "打不开的那个子目录算一项");

        std::fs::set_permissions(&shut, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 但**用户点的那个源**读不到仍然整条失败：界面得说出"这个目录读不了"，
    /// 而不是推一个空清单出去。
    #[cfg(unix)]
    #[tokio::test]
    async fn unreadable_root_still_fails_the_whole_source() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_dir("denied_root");
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o000)).unwrap();

        let Err(err) = manifest_for_paths(&[dir.clone()]).await else {
            panic!("源目录读不到必须整条失败");
        };
        assert!(err.contains("读取目录失败"), "实际：{err}");

        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o755)).unwrap();
        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 清单的形状：目录里混着链接、读不到的项时，**计数照实回到 `PushSource`** ——
    /// 卡片上"已跳过 N 项"读的就是它，数字错了用户就不知道清单少了什么。
    #[tokio::test]
    async fn skipped_survives_into_the_source() {
        let dir = temp_dir("count");
        std::fs::create_dir_all(dir.join("子")).unwrap();
        std::fs::write(dir.join("子").join("a.txt"), b"x").unwrap();
        let link = dir.join("链接");
        if !crate::unsendable::make_link_for_test(&link, &dir.join("子")) {
            eprintln!("这台机器造不出链接，跳过这条");
            std::fs::remove_dir_all(&dir).unwrap();
            return;
        }

        let source = manifest_for_paths(&[dir.clone()]).await.unwrap();
        assert_eq!(source.skipped, 1);
        assert_eq!(source.files.len(), 1);
        assert!(source.is_dir, "有清单的推送才是目录语义");

        std::fs::remove_dir(&link).unwrap_or_else(|_| std::fs::remove_file(&link).unwrap());
        std::fs::remove_dir_all(&dir).unwrap();
    }
}