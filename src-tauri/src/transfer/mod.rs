pub mod download;
pub mod push_job;
pub mod resume;
pub mod stream;
pub mod stream_send;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use tracing::{debug, info, warn};

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
    ///
    /// 是**表**而不是列表：下载端点每次请求都要按路径找一条（见 `download_target`），
    /// 列表就是每请求一次线性扫描 —— 2 万文件的目录配 2 万次请求，累计约 2×10⁸ 次
    /// 字符串比较。路径在清单里唯一（同名去重保证了这一点），所以键不会撞。
    pub sources: HashMap<String, PathBuf>,
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
    let mut sources: HashMap<String, PathBuf> = HashMap::new();
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
        if crate::unsendable::is_mirror_path(path, &crate::mirror::root()) {
            return Err("镜像目录（拖放用的本地占位符）不能推送".into());
        }
        // 数据目录**整棵子树**都不行：只挡"数据目录本身"会漏掉 `credentials.dat` 这类
        // （见 `unsendable::is_own_data_path`），那等于直接把本机凭据推给对端。
        if crate::unsendable::is_own_data_path(path, &own_data_dir) {
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
            files.push(FileEntry {
                path: prefix.clone(),
                size: meta.len(),
                mtime: stream::mtime_secs(&meta),
            });
            // 清单里存**已经解析过的绝对路径**：下载端点每次请求都要拿它开文件，而
            // `canonicalize` 在 Windows 上是逐段解析（20 万次请求就是白白烧掉的时间）。
            // 解析失败（清单建好前被删/ACL 挡住）退回原路径：这条文件照样进清单，
            // 真取不到时由下载端点如实报"文件不存在"，而不是在这里静默把它抹掉。
            let abs = tokio::fs::canonicalize(path)
                .await
                .unwrap_or_else(|_| path.clone());
            sources.insert(prefix, abs);
            total += meta.len();
        } else {
            let (sub, sub_total, sub_skipped) = collect_dir_manifest(path).await?;
            skipped += sub_skipped;
            // 目录**只解析根一次**：走查交回来的相对路径都是 `read_dir` 亲眼见过的条目，
            // 逐条再 `canonicalize` 一遍（2 万条 = 2 万次逐段解析）换不到任何东西。
            let root = tokio::fs::canonicalize(path)
                .await
                .unwrap_or_else(|_| path.clone());
            for f in sub {
                let rel = format!("{prefix}/{}", f.path);
                sources.insert(rel.clone(), root.join(&f.path));
                files.push(FileEntry { path: rel, size: f.size, mtime: f.mtime });
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
            if crate::unsendable::is_link(&ft)
                || crate::unsendable::is_own_data_path(&path, &own_data_dir)
            {
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
            local.push(FileEntry {
                path: rel,
                size: meta.len(),
                mtime: stream::mtime_secs(&meta),
            });
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
            caps: vec![push_job::CAP_STREAM.to_string()],
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
                caps: inv.caps.clone(),
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
    // 拒绝也是个结局：这条 `PushResponse` 现在只在有长连接时发得出去（没有 HTTP 兜底），
    // 丢了的话发送方只能靠超时 —— 对账时把真话说出去。
    state.push.record_outcome(
        &invite.invite_id,
        &invite.sender.id,
        "rejected",
        0,
        Some(reason.to_string()),
    );
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
    // 这条 `PushDone` 走的是"尽力而为"的回执路（链路断了就没了），对账时也要答得出来。
    state.push.record_outcome(
        &invite.invite_id,
        &invite.sender.id,
        "cancelled",
        0,
        Some("接收方已取消".into()),
    );
    // 取消 = "下次接着传"，所以那些还没落盘的进度必须现在就落地（见 `ResumeStore::flush`）。
    state.push.resume.flush();
    warn!(%invite_id, "receive cancelled; resume record kept");
}

// ---------------------------------------------------------------------------
// 对账：回执丢了怎么办（`PushAsk` / `PushState`）
// ---------------------------------------------------------------------------

/// 发送方：把本机发出去的、**还没落终态**的邀请问一遍对端。
///
/// 调用点与文本消息的"排队补发"（`chat::flush_queued`）是同一批，理由也一样：
/// **链路在的时候不主动对账，回执丢了就永远对不上账**。回执（进度/完成/拒绝）是"一次、
/// 尽力而为"的 —— 长连接断在"检查有没有会话"与"真的发出去"之间、或那一跳 HTTP 补发失败，
/// 发送方那一台就会一直停在「等待」，5 分钟后被判成「对方未响应」，而对面其实早就收完了。
///
/// 幂等 + 节流（`PushStore::ask_due`），所以每 5 秒一拍地叫它不会刷屏；没有待对账的邀请
/// 就什么都不做（连闸门都不动）。只走长连接：对账的前提就是"链路在"。
pub fn reconcile(state: &Arc<AppState>, peer_id: &str) {
    if !state.ws.has_connection(peer_id) {
        return;
    }
    let invite_ids = state.push.waiting_invites_of(peer_id);
    if invite_ids.is_empty() || !state.push.ask_due(peer_id) {
        return;
    }
    let ask_id = uuid::Uuid::new_v4().to_string();
    debug!(%peer_id, count = invite_ids.len(), %ask_id, "推送对账：问对端这些邀请的结果");
    if !state.ws.send(peer_id, WsEvent::PushAsk { ask_id, invite_ids }) {
        debug!(%peer_id, "推送对账：问不出去（会话刚断），等下一拍");
    }
}

/// 接收方：如实回答一条对账（`PushAsk`）—— 本机还拿着的答 `waiting`，
/// 处理过的按结局答，没见过的答 `unknown`（= 那条邀请根本没送到）。
pub fn answer_ask(state: &AppState, peer_id: &str, ask_id: &str, invite_ids: &[String]) -> WsEvent {
    let states = invite_ids
        .iter()
        .map(|id| state.push.answer_invite_state(id, peer_id))
        .collect::<Vec<_>>();
    debug!(%peer_id, %ask_id, count = invite_ids.len(), "推送对账：答对端");
    WsEvent::PushState {
        ask_id: ask_id.to_string(),
        states,
    }
}

/// 发送方：把对账的答案落到批次上 —— 这就是"回执丢了"的补漏。
///
/// 只认**属于这台设备**的邀请（答案里塞了别人的 id 就记一句、不落账）。落到批次上的写法
/// 与真实回执完全同路（`batch_set`），所以"本机推测的失败"能被对端的成功纠正，
/// 而"用户取消 / 对端拒绝"这些明确意图改不动（见 `push_job::late_success`）。
pub fn apply_push_states(
    state: &AppState,
    peer_id: &str,
    ask_id: &str,
    states: &[push_job::PushInviteStateWire],
) {
    use push_job::{batch_set, PushStatus, TargetState};
    for s in states {
        let Some(target) = state.push.target_of_invite(&s.invite_id) else {
            // 批次已经收起（整批走完就丢了）或者这条不属于本机发出去的推送。
            continue;
        };
        if target.peer_id != peer_id {
            warn!(%peer_id, invite_id = %s.invite_id, "对账答案里混进了别的设备的邀请，忽略");
            continue;
        }
        match s.state.as_str() {
            "done" => {
                state.push.remove_job(&s.invite_id);
                batch_set(state, &s.invite_id, TargetState::Done, None, None, true);
            }
            "failed" => {
                state.push.remove_job(&s.invite_id);
                batch_set(
                    state,
                    &s.invite_id,
                    TargetState::Failed,
                    None,
                    Some(s.reason.clone().unwrap_or_else(|| "对端接收失败".into())),
                    true,
                );
            }
            "rejected" => {
                state.push.remove_job(&s.invite_id);
                batch_set(
                    state,
                    &s.invite_id,
                    TargetState::Rejected,
                    None,
                    // 用对端给的原因：同样是 `rejected`，"对方拒绝"和"超时未处理"是两件事，
                    // 卡片上那句话该说准（`reject_shared` 两种都走这一档）。
                    Some(
                        s.reason
                            .clone()
                            .unwrap_or_else(|| "对方拒绝了这次推送".into()),
                    ),
                    true,
                );
            }
            "cancelled" => {
                state.push.remove_job(&s.invite_id);
                batch_set(
                    state,
                    &s.invite_id,
                    TargetState::Failed,
                    None,
                    Some("对方取消了接收".into()),
                    true,
                );
            }
            "waiting" => {
                // 对端还拿着这条邀请：未绑定的是"等它点同意"（界面别改口），已配对的它自己
                // 就开收了 —— 那就是"传输中"，卡片该从「等待开始」挪到进度那一档。
                state.push.update_status(&s.invite_id, PushStatus::Accepted);
                if target.paired {
                    batch_set(state, &s.invite_id, TargetState::Sending, Some(s.received), None, false);
                }
            }
            // 对端没有这条邀请（本机没见过 / 记录已过期）：它根本没送到 —— 当场收手，
            // 不必再等满 5 分钟。发送方随后重试时对端会正常收到。
            other => {
                debug!(invite_id = %s.invite_id, state = %other, "对账：对端没有这条邀请");
                state.push.remove_job(&s.invite_id);
                batch_set(
                    state,
                    &s.invite_id,
                    TargetState::Failed,
                    None,
                    Some("对端没有这条邀请（没送达）".into()),
                    true,
                );
            }
        }
    }
    debug!(%peer_id, %ask_id, "推送对账答案已落账");
}

// ---------------------------------------------------------------------------
// tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::sync::mpsc;

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

    // ── 对账：回执丢了怎么办（`PushAsk` / `PushState`）────────────────────────

    /// 一台临时状态：对账的落账要真的批次与真的 store。
    fn reconcile_state() -> Arc<AppState> {
        let dir = std::env::temp_dir().join(format!(
            "orbdrop_push_reconcile_{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&dir).unwrap();
        Arc::new(AppState::new(crate::config::Config::default(), dir.join("config.toml")))
    }

    fn batch_with(targets: Vec<push_job::BatchTarget>) -> push_job::PushBatch {
        push_job::PushBatch {
            id: "b1".into(),
            name: "a.txt".into(),
            count: 1,
            skipped: 0,
            per_target_total: 10,
            targets,
            last_emit: Instant::now(),
        }
    }

    fn target_of(peer: &str, invite: &str, state: push_job::TargetState) -> push_job::BatchTarget {
        push_job::BatchTarget {
            peer_id: peer.into(),
            peer_name: peer.into(),
            invite_id: invite.into(),
            paired: true,
            state,
            received: 0,
            error: None,
        }
    }

    /// 一整个来回：发送方问 → 接收方按**自己的记录**答 → 发送方落账 —— 这就是"回执丢了"的补漏。
    /// 两端在这一条里是同一个进程里的两个 `AppState`（生产里是两个进程，协议一样）。
    #[tokio::test]
    async fn reconcile_round_trip_lands_the_receivers_answer() {
        // 发送方：一条还没落终态的邀请（回执丢了，界面还停在「等待」）
        let sender = reconcile_state();
        sender.push.insert_batch(batch_with(vec![target_of("p1", "i1", push_job::TargetState::Invited)]));
        let ids = sender.push.waiting_invites_of("p1");
        assert_eq!(ids, vec!["i1".to_string()], "要问的就是它");

        // 接收方：这条邀请它早就处理完了（结局记录在案）
        let receiver = reconcile_state();
        receiver.push.record_outcome("i1", "p1", "done", 1024, None);

        // 接收方答（对端 id 逐条校验）
        let answer = answer_ask(&receiver, "p1", "ask-1", &ids);
        let WsEvent::PushState { states, ask_id } = answer else {
            panic!("对账的答案是 PushState");
        };
        assert_eq!(ask_id, "ask-1");
        assert_eq!(states.len(), 1);
        assert_eq!(states[0].state, "done");
        assert_eq!(states[0].received, 1024);

        // 发送方落账：那条「等待」变成「已成功」，会话气泡跟着改口（`push://batch` 全量快照）
        apply_push_states(&sender, "p1", "ask-1", &states);
        let target = sender.push.target_of_invite("i1").unwrap();
        assert_eq!(target.state, push_job::TargetState::Done);
        assert!(sender.push.batch_snapshot("b1").is_some(), "卡片还在（宽限期）");
    }

    /// 本机判的"失败"（超时/送不出去）是**推测**，对账拿到 `done` 就该改口 ——
    /// 对面收完了、本机写着失败，那是假账。
    #[tokio::test]
    async fn reconcile_corrects_a_speculative_failure() {
        let state = reconcile_state();
        state.push.insert_batch(batch_with(vec![target_of("p1", "i1", push_job::TargetState::Invited)]));
        // 5 分钟超时：本机先记成失败
        push_job::batch_set(&state, "i1", push_job::TargetState::Failed, None, Some("对方未响应".into()), true);
        assert_eq!(state.push.target_of_invite("i1").unwrap().state, push_job::TargetState::Failed);

        // 对端其实收完了：对账答案（正常情况下这条是回执，丢了才需要问）
        apply_push_states(
            &state,
            "p1",
            "ask-1",
            &[push_job::PushInviteStateWire {
                invite_id: "i1".into(),
                state: "done".into(),
                received: 10,
                reason: None,
            }],
        );

        let target = state.push.target_of_invite("i1").unwrap();
        assert_eq!(target.state, push_job::TargetState::Done, "推测的失败被对端的成功改写");
        assert!(target.error.is_none(), "改口时要把上一次的原因抹掉");
    }

    /// 但"当事人明确表达的"不能被改写：对面拒绝过的那台，不会因为一条 `done` 就变成成功
    /// （终态只落一次的纪律，见 `push_job::late_success`）。
    #[tokio::test]
    async fn reconcile_never_overwrites_an_explicit_rejection() {
        let state = reconcile_state();
        state.push.insert_batch(batch_with(vec![target_of("p1", "i1", push_job::TargetState::Invited)]));
        push_job::batch_set(&state, "i1", push_job::TargetState::Rejected, None, Some("对方拒绝".into()), true);

        apply_push_states(
            &state,
            "p1",
            "ask-1",
            &[push_job::PushInviteStateWire {
                invite_id: "i1".into(),
                state: "done".into(),
                received: 10,
                reason: None,
            }],
        );

        assert_eq!(
            state.push.target_of_invite("i1").unwrap().state,
            push_job::TargetState::Rejected
        );
    }

    /// 对端说"没见过这条邀请" = 那条邀请根本没送到：当场收手，不用等满 5 分钟。
    #[tokio::test]
    async fn reconcile_fails_an_invite_the_peer_never_saw() {
        let state = reconcile_state();
        state.push.insert_batch(batch_with(vec![target_of("p1", "i1", push_job::TargetState::Invited)]));
        apply_push_states(
            &state,
            "p1",
            "ask-1",
            &[push_job::PushInviteStateWire {
                invite_id: "i1".into(),
                state: "unknown".into(),
                received: 0,
                reason: None,
            }],
        );
        let target = state.push.target_of_invite("i1").unwrap();
        assert_eq!(target.state, push_job::TargetState::Failed);
        assert!(target.error.unwrap().contains("没送达"));
    }

    /// 答案里混进别的设备的邀请：不落账（对端的答案不能动到第三台那行）。
    #[tokio::test]
    async fn reconcile_ignores_invites_of_other_peers() {
        let state = reconcile_state();
        state.push.insert_batch(batch_with(vec![target_of("p2", "i1", push_job::TargetState::Invited)]));
        apply_push_states(
            &state,
            "p1",
            "ask-1",
            &[push_job::PushInviteStateWire {
                invite_id: "i1".into(),
                state: "done".into(),
                received: 10,
                reason: None,
            }],
        );
        assert_eq!(
            state.push.target_of_invite("i1").unwrap().state,
            push_job::TargetState::Invited,
            "不是这台设备的答案，一个字都不该动"
        );
    }

    /// 只问"还没落终态"的那几条，而且同一台设备有节流（每 5 秒一拍地叫不会刷屏）。
    #[tokio::test]
    async fn reconcile_asks_only_about_waiting_invites_and_is_throttled() {
        let state = reconcile_state();
        state.push.insert_batch(batch_with(vec![
            target_of("p1", "waiting", push_job::TargetState::Invited),
            target_of("p1", "done", push_job::TargetState::Done),
            target_of("p2", "other-peer", push_job::TargetState::Invited),
        ]));
        let (tx, mut rx) = mpsc::unbounded_channel();
        state.ws.register("p1", tx);

        reconcile(&state, "p1");
        let event = rx.try_recv().expect("问出去了一条 PushAsk");
        match event {
            WsEvent::PushAsk { invite_ids, .. } => {
                assert_eq!(invite_ids, vec!["waiting".to_string()], "只问还没落终态的那条");
            }
            other => panic!("期望 PushAsk，得到 {}", other.type_name()),
        }
        // 节流：立刻再叫一次不该再问（回执丢了靠下一拍，20 秒后才又问）
        reconcile(&state, "p1");
        assert!(rx.try_recv().is_err(), "20 秒内不重复问");
    }

    /// 没有长连接就不问：对账的前提是"链路在"（问出去也得有人答）。
    #[tokio::test]
    async fn reconcile_does_nothing_without_a_link() {
        let state = reconcile_state();
        state.push.insert_batch(batch_with(vec![target_of("p1", "i1", push_job::TargetState::Invited)]));
        reconcile(&state, "p1"); // 没注册会话 → 早退，不发也不占闸门
        assert!(state.push.ask_due("p1"), "没问过就不该占掉闸门");
    }

    /// 回执的最后一道兜底：**"塞进 WS 队列"失败也要退回 HTTP**（以前只看"有没有会话"，
    /// 于是"会话还在但已经写不进去"这一档会静默丢回执）。这里用"注册过、但接收端已经丢掉"
    /// 的会话把这个分支逼出来。
    ///
    /// 对端地址是 `https://`（`net::peer_base`），所以监听端看到的是 TLS 握手的第一帧 ——
    /// 这已经足够证明"回执确实往那个地址发了一次"（要连出握手，先得拨号成功）。
    #[tokio::test]
    async fn report_falls_back_to_http_when_the_ws_queue_is_dead() {
        let state = reconcile_state();
        let (tx, rx) = mpsc::unbounded_channel();
        state.ws.register("p1", tx);
        drop(rx); // 会话还在册，但队列已经没人接

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let seen = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 5];
            let n = tokio::io::AsyncReadExt::read(&mut sock, &mut buf).await.unwrap();
            buf[..n].to_vec()
        });

        push_job::report_to_sender(
            &state,
            "p1",
            &Some(("127.0.0.1".into(), port)),
            WsEvent::PushDone { invite_id: "i1".into(), status: "completed".into(), reason: None },
        );

        let head = tokio::time::timeout(Duration::from_secs(5), seen)
            .await
            .expect("5 秒内该向回调地址发出回执")
            .unwrap();
        assert_eq!(head.first(), Some(&0x16), "TLS 握手：确实拨到了那个地址，head={head:?}");

        // 有活儿干的会话不该走这条路：回执进的是 WS 队列。
        let (tx2, mut rx2) = mpsc::unbounded_channel();
        state.ws.register("p2", tx2);
        push_job::report_to_sender(
            &state,
            "p2",
            &Some(("127.0.0.1".into(), 1)), // 故意指向一个没人听的端口：走了 HTTP 就会连不上
            WsEvent::PushDone { invite_id: "i2".into(), status: "completed".into(), reason: None },
        );
        assert!(matches!(rx2.try_recv(), Ok(WsEvent::PushDone { .. })), "有会话就走 WS");
    }
}