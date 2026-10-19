use std::collections::HashMap;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll};

use axum::body::Body;
use axum::extract::{ConnectInfo, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::Json;
use tokio::io::{AsyncRead, ReadBuf};
use tokio_util::io::ReaderStream;
use tracing::{info, warn};

use crate::net;
use crate::server::auth::bearer_token;
use crate::state::AppState;
use crate::transfer::push_job::{DownloadTarget, InboundInvite};
use crate::transfer::stream::StreamRequest;
use crate::transfer::{accept_invite, reject_invite_with, schedule_invite_timeout, INVITE_TTL};
use crate::ws::events::{FileEntry, PeerInfo, WsEvent};
use crate::ws::session::peer_id_from_token;

// ---------------------------------------------------------------------------
// Receiver side: inbound invitation
// ---------------------------------------------------------------------------

/// 对端 POST 过来的邀请体。
///
/// 字段就是 `WsEvent::PushInvite` 那套（`type` 字段一并送过来，这里忽略掉），
/// 只多一个 `port`：**发送方的服务端口**。请求的源端口是一次性的，回执
/// （进度/完成）要发回它真正的服务端口。
#[derive(serde::Deserialize)]
pub struct InviteBody {
    invite_id: String,
    sender: PeerInfo,
    port: u16,
    name: String,
    size: u64,
    is_dir: bool,
    /// 发送方选了几项（>1 = 多选合并成的一条推送）。老对端不带就按一项算。
    #[serde(default = "one_item")]
    count: usize,
    files: Vec<FileEntry>,
    /// 发送方支持的能力位（见 `WsEvent::PushInvite::caps`）。老对端不带 → 空 → 走老循环。
    #[serde(default)]
    caps: Vec<String>,
    /// 这批文件在会话时间线上的位置（发送方的墙上时间；见 `WsEvent::PushInvite::at`）。
    /// 老对端不带 → 0 → 接收侧退回"收到的那一刻"。
    #[serde(default)]
    at: u64,
}

fn one_item() -> usize {
    1
}

#[derive(serde::Serialize)]
pub struct InviteResponse {
    accepted: bool,
    /// **邀请已送达、本机等用户点头**（对端带 `await1` 时才有这一档，见
    /// `push_job::CAP_AWAIT`）：发送方据此继续等，别当拒绝 —— 决定稍后从回执通道
    /// （`POST /push/report` 或长连接 `push_response`）过去。
    ///
    /// 老对端不认识这个字段（也永远不会收到 true）：它那条请求照旧挂着等用户决定。
    pending: bool,
    /// 拒绝 / 收不下的原因（原话），发送方卡片上就写这句。老对端忽略。
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<String>,
}

/// POST /push/invite —— 接收方收到一条推送邀请（F-50 / F-51）。
///
/// 这条路**不要求预先配对**：未绑定设备也能推（需求 F-50）。安全靠两点：
/// ① 同意制 —— 用户点了才会开始传；② 若 sender 自称的是本机**已配对**的设备，
/// 就必须出示那枚 token，否则等于谁都能冒充你的老搭档给你塞文件。
///
/// 决定什么时候回来，看对端是哪一版（邀请体里的 `caps`，见 `push_job::CAP_AWAIT`）：
/// - 带 `await1`（新版本）→ **投递即回**：这张邀请挂在本机通知列表里等用户点，响应只是
///   一句"送到了"（`pending`），用户点完的结果从回执通道过去。**没有超时**；
/// - 老对端 → 这条请求一直挂着等用户决定，到点自动作废，响应就是回执。
///
/// **已绑定**（已配对）设备不问同意：当场开收并立刻回 `accepted: true`（两条路一样）。
pub async fn push_invite_handler(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    headers: HeaderMap,
    Json(body): Json<InviteBody>,
) -> Result<Json<InviteResponse>, (StatusCode, String)> {
    // 本机地址簿里这台是不是"已绑定"（已配对）。
    let paired = state.get_peer(&body.sender.id).is_some_and(|p| p.paired);
    // 冒充已配对设备：拿不出 token 就拒 —— 走得过这一关的 paired = 身份确认过的老搭档。
    if paired {
        let claimed = bearer_token(&headers)
            .and_then(|t| peer_id_from_token(&state, &t));
        if claimed.as_deref() != Some(body.sender.id.as_str()) {
            info!(sender = %body.sender.id, "推送邀请被拒：自称已配对设备但 token 不认");
            return Err((StatusCode::UNAUTHORIZED, "身份校验失败".into()));
        }
    }

    let invite_id = body.invite_id;
    // 一条推送邀请也是"**从它那边收到了东西**"：对端能拨进来、本机拨不回去时（AP 把单播
    // 隔开、跨段、地址过期），会话/邀请这类入站报文是它还在网上的唯一证据（见
    // `AppState::heard`）。只对设备表里已有的行生效 —— 载荷里的 id 不配凭空造出"在线设备"。
    state.note_heard_online(&body.sender.id);
    // `await1` = 发送方是"投递即回"那一版（见 `push_job::CAP_AWAIT`）。哪一端老，看这一行
    // 最快：**没有**这个位就是对面老 —— 那这条邀请只能按老规矩（60 秒没人处理就作废）办。
    let await_capable = body.caps.iter().any(|c| c == crate::transfer::push_job::CAP_AWAIT);
    info!(
        %invite_id, sender = %body.sender.name, name = %body.name, paired, await1 = await_capable,
        "inbound push invite (http)"
    );

    crate::ws::emit_ui(&state, crate::ws::events::ui::PUSH_INVITE, &serde_json::json!({
        "inviteId": invite_id,
        "sender": { "id": body.sender.id, "name": body.sender.name },
        "name": body.name,
        "size": body.size,
        "isDir": body.is_dir,
        "count": body.count,
        "auto": paired,
        // 这条文件消息在会话时间线上的位置（发送方点的）。
        "at": body.at,
    }));

    let mut invite = InboundInvite {
        invite_id: invite_id.clone(),
        sender: body.sender,
        name: body.name,
        size: body.size,
        is_dir: body.is_dir,
        count: body.count,
        files: body.files,
        caps: body.caps,
        // 落盘根由用户在界面上定（「每次询问」时那颗目录选择框）—— 邀请到达时还不知道，
        // 所以这里恒为 `None`，真要挑就等 `accept_invite` 带的那个 root。
        root: None,
        // ip 取自 TCP 来源（不可伪造），端口用发送方自报的服务端口 —— 但**本机自己的监听
        // 端点不能记**：NAT 后面的对端（安卓模拟器）回连进来时，来源地址在**本机**这一侧
        // 就是本机自己（见 `net::observed_dial_target`）。留一个指向自己的回执地址，等于
        // 把"接收/进度/完成"回报 POST 给本机自己（对端永远收不到，日志里还看不出是谁的错）。
        // 判定为"自己"时给 `None`：回执改走长连接，没有长连接就如实报"无处可送"。
        callback: net::observed_dial_target(addr.ip(), body.port, state.listening_port()),
        responder: None,
    };

    // 已绑定的设备推送**不需要接收方同意**：直接开收，并用这次同步响应告诉发送方
    // "收下了"。未绑定仍然走同意制（挂一张卡片等用户点）—— 那是"不配对也能推"的
    // 唯一安全兜底，不能一起取消。
    if paired {
        state.push.insert_invite(invite);
        if let Err(reason) = accept_invite(state.clone(), &invite_id, None) {
            warn!(%invite_id, %reason, "auto-accept failed");
            // 本机那条"正在接收"的通知得收尾（`reject_invite_with` 会把它变成一条
            // 失败通知），再把原因作为这次同步响应回给发送方。
            reject_invite_with(&state, &invite_id, &reason);
            return Err((StatusCode::BAD_REQUEST, reason));
        }
        return Ok(Json(InviteResponse { accepted: true, pending: false, reason: None }));
    }

    // 带 `await1` 的对端（新版本）：**投递即算送达**，不在这儿等用户 ——
    // 它那边一直挂着等回执的话，用户一直不点就等于把一条 HTTP 请求吊死在那儿，
    // 而"等多久"这件事本来就该由人来定。用户点完，那道决定从回执通道过去
    // （`/push/report` 或长连接 `push_response`，见 `transfer::reject_shared`）。
    if invite.supports_await() {
        state.push.insert_invite(invite);
        return Ok(Json(InviteResponse { accepted: false, pending: true, reason: None }));
    }

    // 老对端：那条邀请请求挂着等本机用户的决定，同步响应就是回执（到点自动作废，
    // 两边必须同时收手 —— 见 `transfer::schedule_invite_timeout`）。
    info!(%invite_id, "对端没带 await1（老版本）：这条邀请按老规矩办 —— 60 秒没人处理就自动作废");
    let (tx, rx) = tokio::sync::oneshot::channel::<bool>();
    invite.responder = Some(tx);
    state.push.insert_invite(invite);
    schedule_invite_timeout(state.clone(), invite_id.clone());

    // 等用户在通知卡片上做决定（对方自己的超时到点会先填上 false）。
    let accepted = match tokio::time::timeout(INVITE_TTL + std::time::Duration::from_secs(10), rx).await {
        Ok(Ok(v)) => v,
        _ => false,
    };
    Ok(Json(InviteResponse { accepted, pending: false, reason: None }))
}

// ---------------------------------------------------------------------------
// Sender side: reports from the receiver
// ---------------------------------------------------------------------------

/// POST /push/report —— 接收方回报**决定**（接收/拒绝）与进度/完成/失败。
///
/// 没有长连接时（已配对未连、未绑定）走这条路：请求体就是 WS 那条路上的
/// `push_response` / `push_progress` / `push_done` 事件，发送方用同一个 dispatcher 处理。
///
/// `push_response` 是**等报告那一版对端的主要出口**（见 `push_job::CAP_AWAIT`）：邀请投递
/// 时那条同步响应只说"送到了"，用户点完的结果从这里回来。
pub async fn push_report_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(event): Json<WsEvent>,
) -> Result<StatusCode, (StatusCode, String)> {
    let (invite_id, kind) = match &event {
        WsEvent::PushResponse { invite_id, .. } => (invite_id.clone(), "push_response"),
        WsEvent::PushProgress { invite_id, .. } => (invite_id.clone(), "push_progress"),
        WsEvent::PushDone { invite_id, .. } => (invite_id.clone(), "push_done"),
        _ => {
            return Err((
                StatusCode::BAD_REQUEST,
                "只接受 push_response / push_progress / push_done".into(),
            ))
        }
    };
    // 本机可能按两种身份收到这条消息：
    // - 发送方：这条推送是我发出去的（job 在）—— 进度/完成；
    // - 接收方：这条邀请还挂在通知列表里，对方把它撤了（invite 在）—— 撤卡。
    // 两者都没有 = 本来就不是给我们的（或早已作废）。
    let sender_id = match state.push.job(&invite_id) {
        Some(job) => job.peer_id.clone(),
        None => {
            let pending = state
                .push
                .inbound
                .lock()
                .get(&invite_id)
                .map(|inv| inv.sender.id.clone());
            match pending {
                Some(id) => id,
                None => return Err((StatusCode::GONE, "推送不存在或已取消".to_string())),
            }
        }
    };
    // 认人：与 `/push/invite`、`/push/download` 同一套 —— 这台对端是**已配对**设备时，
    // 必须出示它的 token。否则只要知道 invite_id（v4 UUID，可能从日志/界面/别处漏出去），
    // 任何人都能伪造一条 `push_done` 把在途传输判完成或判失败。
    // 未绑定的对端手里没有 token（这正是"未绑定也能推"的前提），只能靠 invite_id。
    let peer_paired = state.get_peer(&sender_id).is_some_and(|p| p.paired);
    authorize(&state, &headers, peer_paired, &sender_id)?;
    info!(%invite_id, kind, "push report (http)");
    let _ = crate::ws::session::handle_inbound(&state, &sender_id, event).await;
    Ok(StatusCode::NO_CONTENT)
}

// ---------------------------------------------------------------------------
// Sender side: file streaming
// ---------------------------------------------------------------------------

/// GET /push/download?id=<invite_id>&path=<rel_path>&offset=<u64>
///
/// Streams the requested file from the sender's source directory, starting at
/// `offset`.  This is the HTTP Range-style endpoint used by the receiver's
/// download loop (F-52 / F-60).
///
/// 挂在下沉过的**公开**路由上（不带 auth 中间件）：未绑定的接收方没有 token 可带。
/// 认人就地做，见 `authorize`；路径只认邀请清单里列过的那几条。
///
/// Returns 410 Gone if the job was cancelled or doesn't exist.
pub async fn push_download_handler(
    State(state): State<Arc<AppState>>,
    Query(params): Query<HashMap<String, String>>,
    headers: HeaderMap,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let id = params
        .get("id")
        .ok_or_else(|| (StatusCode::BAD_REQUEST, "Missing 'id' parameter".to_string()))?;
    let file_path = params
        .get("path")
        .ok_or_else(|| (StatusCode::BAD_REQUEST, "Missing 'path' parameter".to_string()))?;
    let offset = params
        .get("offset")
        .and_then(|v| v.parse::<u64>().ok())
        .unwrap_or(0);

    let (source_path, peer_paired, peer_id, cancel) = match state.push.download_target(id, file_path) {
        // 这条推送已经被发送方取消 / 传完了。
        DownloadTarget::Missing => {
            return Err((StatusCode::GONE, "推送不存在或已取消".into()));
        }
        // `path` 是清单里的相对路径：清单里没有的取不到 —— 越界路径（`../` 之类）
        // 自然也不在表里，因为这张表是发送方按自己选中的那些源建的。
        DownloadTarget::NotInManifest => {
            return Err((StatusCode::NOT_FOUND, "文件不在这次推送里".into()));
        }
        DownloadTarget::Found { path, peer_paired, peer_id, cancel } => {
            (path, peer_paired, peer_id, cancel)
        }
    };
    authorize(&state, &headers, peer_paired, &peer_id)?;

    // 清单里存的**已经是解析过的绝对路径**（见 `manifest_for_paths`）：这里不再
    // `canonicalize()`。那是每请求一次逐段路径解析（Windows 上一次几毫秒），而路径的
    // 安全边界本来就是"清单白名单"，不是"再解析一遍" —— 清单里没有的路径一行都取不到。
    let mut file = tokio::fs::File::open(&source_path)
        .await
        .map_err(|e| (StatusCode::NOT_FOUND, format!("文件打开失败: {e}")))?;

    let meta = file
        .metadata()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("元数据读取失败: {e}")))?;
    // 打开之后再看一次类型：源目录被换成一个同名目录时，`open` 在 Windows 上可能成功。
    if !meta.is_file() {
        return Err((StatusCode::NOT_FOUND, "文件不存在".into()));
    }
    let file_size = meta.len();

    // `offset` 越过当前长度 = 源文件在清单建好之后变小或被换掉了。
    //
    // **必须回一个明确的错**：老写法会返回一段 0 字节的 200，而接收端那边的循环是
    // "没到 size 就再要一次"，于是两边进入死循环（对端狂发请求、我们狂回空包，谁也不报错）。
    // 409 让接收端当场把这条文件判成"源已变"。
    if offset > 0 && offset >= file_size {
        return Err((StatusCode::CONFLICT, "源文件在传输中被修改".into()));
    }

    if offset > 0 {
        use tokio::io::AsyncSeekExt;
        if let Err(e) = file.seek(std::io::SeekFrom::Start(offset)).await {
            return Err((StatusCode::INTERNAL_SERVER_ERROR, format!("Seek失败: {e}")));
        }
    }

    use tokio::io::AsyncReadExt;
    let remaining = file_size.saturating_sub(offset);
    let limited = file.take(remaining);
    let reader = CancelAwareReader { inner: limited, cancel };
    Ok(Body::from_stream(ReaderStream::with_capacity(
        reader,
        crate::net::STREAM_CHUNK,
    )))
}

/// POST /push/stream —— 新数据面（`caps` 里有 `stream1` 的对端才走这条）。
///
/// 一次请求带一批**块任务**（大文件的一块，或一批小文件各自的一整块），响应是一串分帧记录
/// （见 `transfer::stream` 的模块注释）。认人与 `/push/download` 同一套：拿第一条任务定身份，
/// 之后的每条任务再各自查一次清单白名单（发送端逐条查，见 `stream_send::serve`）。
///
/// 与老端点一样挂在**公开**路由上（未绑定的接收方没有 token 可带），认人就地做。
pub async fn push_stream_handler(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    Json(req): Json<StreamRequest>,
) -> Result<impl IntoResponse, (StatusCode, String)> {
    let Some(first) = req.tasks.first() else {
        return Err((StatusCode::BAD_REQUEST, "没有任务".into()));
    };
    let (peer_paired, peer_id) = match state.push.download_target(&req.id, &first.path) {
        DownloadTarget::Missing => {
            return Err((StatusCode::GONE, "推送不存在或已取消".into()));
        }
        DownloadTarget::NotInManifest => {
            return Err((StatusCode::NOT_FOUND, "文件不在这次推送里".into()));
        }
        DownloadTarget::Found { peer_paired, peer_id, .. } => (peer_paired, peer_id),
    };
    authorize(&state, &headers, peer_paired, &peer_id)?;

    // 参数护栏：块大小与任务条数都来自对端，**必须有上限** ——
    // 块大小决定接收端要预留/校验的区间（也是发送端要一口气算完的哈希范围），
    // 任务条数决定这一条请求要活多久。
    if req.block_size == 0 || req.block_size > MAX_STREAM_BLOCK {
        return Err((StatusCode::BAD_REQUEST, "块大小非法".into()));
    }
    if req.tasks.len() > MAX_STREAM_TASKS {
        return Err((StatusCode::BAD_REQUEST, "任务太多".into()));
    }

    // 响应体由一条后台任务喂：它按任务顺序读源文件、边读边发（不把整块攒在内存里）。
    // 通道有界（8 帧 ≈ 2MB）：对端读得慢时，这里就自然地压住读盘速度，不会为了"发得快"
    // 把内存吃掉。
    let (tx, rx) = tokio::sync::mpsc::channel::<Vec<u8>>(8);
    let serve_state = state.clone();
    crate::task::spawn("push stream", async move {
        crate::transfer::stream_send::serve(&serve_state, &req, &tx).await;
    });

    let body = Body::from_stream(futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv()
            .await
            .map(|chunk| (Ok::<_, std::io::Error>(axum::body::Bytes::from(chunk)), rx))
    }));
    Ok(body)
}

/// 一次请求最多几条任务（小文件批的上限 + 余量）。
const MAX_STREAM_TASKS: usize = 1024;
/// 单块上限：块大小由接收方定，但**发送方也要守一道**（对端不正常时别把这一条请求拖成小时级）。
const MAX_STREAM_BLOCK: u32 = 64 * 1024 * 1024;

/// 下载端点的认人规则。
///
/// - 这条推送是发给**已配对**对端的：必须出示能反解成它的那枚 token。
/// - 未绑定的推送：邀请 id 本身就是能力（一次性的 v4 uuid，只在这条邀请里出现过，
///   对端也是刚凭它同意的）。局域网明文 HTTP 里再套一层 token 换不到什么，
///   需求本来就把安全性放在"接收方同意"这一步上。
fn authorize(
    state: &AppState,
    headers: &HeaderMap,
    peer_paired: bool,
    peer_id: &str,
) -> Result<(), (StatusCode, String)> {
    if !peer_paired {
        return Ok(());
    }
    let caller = bearer_token(headers).and_then(|t| peer_id_from_token(state, &t));
    if caller.as_deref() == Some(peer_id) {
        Ok(())
    } else {
        Err((StatusCode::UNAUTHORIZED, "请带上对端 token".into()))
    }
}

/// 流式读文件的包装：推送被取消（或这条 job 已经收尾被清掉）时**就地断流**（返回 EOF），
/// 而不是把剩下的字节读完。
///
/// 接收方看到的是"响应提前结束"，它会退回重试并拿到 410，于是两边在对端取消后
/// 几百毫秒内一起收手；不这么做的话，取消一个正在传的大文件要等它传完才见效。
///
/// 判据是**原子量**（`PushJob::cancel`，由 `remove_job` 置位）而不是 `has_job()`：
/// 这里每读一帧都要问一次（256KB 帧 = 千兆下每秒几百次），而 `has_job` 每次都要抢
/// 全局互斥锁；原子读不碰锁，语义还更全（取消 / 完成 / 失败收尾全都是 `remove_job`）。
struct CancelAwareReader {
    inner: tokio::io::Take<tokio::fs::File>,
    cancel: Arc<AtomicBool>,
}

impl AsyncRead for CancelAwareReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if self.cancel.load(Ordering::Relaxed) {
            return Poll::Ready(Ok(())); // EOF
        }
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    fn state() -> Arc<AppState> {
        let dir = std::env::temp_dir().join(format!("orbdrop_push_rep_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        Arc::new(AppState::new(crate::config::test_config(), dir.join("config.toml")))
    }

    fn inbound_invite(id: &str, sender: &str) -> crate::transfer::push_job::InboundInvite {
        crate::transfer::push_job::InboundInvite {
            invite_id: id.into(),
            sender: PeerInfo { id: sender.into(), name: "PC-B".into() },
            name: "a.txt".into(),
            size: 3,
            is_dir: false,
            count: 1,
            files: Vec::new(),
            caps: Vec::new(),
            root: None,
            callback: None,
            responder: None,
        }
    }

    /// `/push/report` 不是"知道 invite_id 就能改结果"（P1-4）。
    ///
    /// 已配对的对端必须出示它那枚 token（与 `/push/invite`、`/push/download` 同一道关）；
    /// 未绑定的对端手里没有 token —— 那是"未绑定也能推"的前提，只能靠 invite_id。
    #[tokio::test]
    async fn push_report_needs_the_token_of_a_paired_peer() {
        let st = state();
        let port = crate::server::start_server_loopback(st.clone(), 4320)
            .await
            .expect("server start");
        crate::credential::set_peer_credential("peer-paired", "tok-paired", &"c".repeat(64))
            .unwrap();
        st.mark_paired(&crate::config::PeerDevice {
            id: "peer-paired".into(),
            name: "PC-B".into(),
            ip: "127.0.0.1".into(),
            port: 1,
            online: true,
            paired: true,
            ..Default::default()
        });
        st.push.insert_invite(inbound_invite("inv-paired", "peer-paired"));
        st.push.insert_invite(inbound_invite("inv-unpaired", "peer-new"));

        let url = format!("http://127.0.0.1:{port}/push/report");
        let body = |invite: &str| {
            serde_json::json!({
                "type": "push_done", "invite_id": invite,
                "status": "completed", "reason": null,
            })
        };
        let client = reqwest::Client::new();

        // 已配对 + 无凭据 → 401（原来这里只要 invite_id 对就能改结果）
        let bare = client.post(&url).json(&body("inv-paired")).send().await.unwrap();
        assert_eq!(bare.status(), StatusCode::UNAUTHORIZED);
        // 已配对 + 别人的 token → 401
        let wrong = client
            .post(&url)
            .bearer_auth("tok-other")
            .json(&body("inv-paired"))
            .send()
            .await
            .unwrap();
        assert_eq!(wrong.status(), StatusCode::UNAUTHORIZED);
        // 已配对 + 自己的 token → 收下
        let ok = client
            .post(&url)
            .bearer_auth("tok-paired")
            .json(&body("inv-paired"))
            .send()
            .await
            .unwrap();
        assert_eq!(ok.status(), StatusCode::NO_CONTENT);

        // 未绑定：没有 token 可带，仍按 invite_id 收下（否则"未绑定也能推"就断了）
        let unpaired = client.post(&url).json(&body("inv-unpaired")).send().await.unwrap();
        assert_eq!(unpaired.status(), StatusCode::NO_CONTENT);

        crate::server::stop_server(&st).await;
        crate::credential::remove_peer("peer-paired").ok();
    }
}

