use std::collections::HashMap;
use std::net::SocketAddr;
use std::pin::Pin;
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

use crate::state::AppState;
use crate::transfer::push_job::{DownloadTarget, InboundInvite};
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
}

fn one_item() -> usize {
    1
}

#[derive(serde::Serialize)]
pub struct InviteResponse {
    accepted: bool,
}

/// POST /push/invite —— 接收方收到一条推送邀请（F-50 / F-51）。
///
/// 这条路**不要求预先配对**：未绑定设备也能推（需求 F-50）。安全靠两点：
/// ① 同意制 —— 用户点了才会开始传；② 若 sender 自称的是本机**已配对**的设备，
/// 就必须出示那枚 token，否则等于谁都能冒充你的老搭档给你塞文件。
///
/// 这是个**同步**接口：**未绑定**设备的推送在用户做决定之前，请求一直挂着，决定一下来
/// 就作为响应回给发送方（长连接那条路走 WS `push_response`）；**已绑定**（已配对）设备
/// 不问同意，当场开收并立刻回 `accepted: true`。
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
    info!(%invite_id, sender = %body.sender.name, name = %body.name, paired, "inbound push invite (http)");

    crate::ws::emit_ui(&state, crate::ws::events::ui::PUSH_INVITE, &serde_json::json!({
        "inviteId": invite_id,
        "sender": { "id": body.sender.id, "name": body.sender.name },
        "name": body.name,
        "size": body.size,
        "isDir": body.is_dir,
        "count": body.count,
        "auto": paired,
    }));

    let mut invite = InboundInvite {
        invite_id: invite_id.clone(),
        sender: body.sender,
        name: body.name,
        size: body.size,
        is_dir: body.is_dir,
        count: body.count,
        files: body.files,
        // ip 取自 TCP 来源（不可伪造），端口用发送方自报的服务端口。
        callback: Some((addr.ip().to_string(), body.port)),
        responder: None,
    };

    // 已绑定的设备推送**不需要接收方同意**：直接开收，并用这次同步响应告诉发送方
    // "收下了"。未绑定仍然走同意制（挂一张卡片等用户点）—— 那是"不配对也能推"的
    // 唯一安全兜底，不能一起取消。
    if paired {
        state.push.insert_invite(invite);
        if let Err(reason) = accept_invite(state.clone(), &invite_id) {
            warn!(%invite_id, %reason, "auto-accept failed");
            // 本机那条"正在接收"的通知得收尾（`reject_invite_with` 会把它变成一条
            // 失败通知），再把原因作为这次同步响应回给发送方。
            reject_invite_with(&state, &invite_id, &reason);
            return Err((StatusCode::BAD_REQUEST, reason));
        }
        return Ok(Json(InviteResponse { accepted: true }));
    }

    let (tx, rx) = tokio::sync::oneshot::channel::<bool>();
    invite.responder = Some(tx);
    state.push.insert_invite(invite);
    schedule_invite_timeout(state.clone(), invite_id.clone());

    // 等用户在通知卡片上做决定（对方自己的超时到点会先填上 false）。
    let accepted = match tokio::time::timeout(INVITE_TTL + std::time::Duration::from_secs(10), rx).await {
        Ok(Ok(v)) => v,
        _ => false,
    };
    Ok(Json(InviteResponse { accepted }))
}

fn bearer_token(headers: &HeaderMap) -> Option<String> {
    headers
        .get("Authorization")?
        .to_str()
        .ok()?
        .strip_prefix("Bearer ")
        .map(|t| t.to_string())
}

// ---------------------------------------------------------------------------
// Sender side: reports from the receiver
// ---------------------------------------------------------------------------

/// POST /push/report —— 接收方回报进度/完成/失败。
///
/// 没有长连接时（已配对未连、未绑定）走这条路：请求体就是 WS 那条路上的
/// `push_progress` / `push_done` 事件，发送方用同一个 dispatcher 处理。
pub async fn push_report_handler(
    State(state): State<Arc<AppState>>,
    Json(event): Json<WsEvent>,
) -> Result<StatusCode, (StatusCode, String)> {
    let (invite_id, kind) = match &event {
        WsEvent::PushProgress { invite_id, .. } => (invite_id.clone(), "push_progress"),
        WsEvent::PushDone { invite_id, .. } => (invite_id.clone(), "push_done"),
        _ => return Err((StatusCode::BAD_REQUEST, "只接受 push_progress / push_done".into())),
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
    info!(%invite_id, kind, "push report (http)");
    crate::ws::session::handle_inbound(&state, &sender_id, event).await;
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

    let (source_path, peer_paired, peer_id) = match state.push.download_target(id, file_path) {
        // 这条推送已经被发送方取消 / 传完了。
        DownloadTarget::Missing => {
            return Err((StatusCode::GONE, "推送不存在或已取消".into()));
        }
        // `path` 是清单里的相对路径：清单里没有的取不到 —— 越界路径（`../` 之类）
        // 自然也不在表里，因为这张表是发送方按自己选中的那些源建的。
        DownloadTarget::NotInManifest => {
            return Err((StatusCode::NOT_FOUND, "文件不在这次推送里".into()));
        }
        DownloadTarget::Found { path, peer_paired, peer_id } => (path, peer_paired, peer_id),
    };
    authorize(&state, &headers, peer_paired, &peer_id)?;

    let canonical = source_path
        .canonicalize()
        .map_err(|_| (StatusCode::NOT_FOUND, "文件不存在".into()))?;
    if !canonical.is_file() {
        return Err((StatusCode::NOT_FOUND, "文件不存在".into()));
    }

    let mut file = tokio::fs::File::open(&canonical)
        .await
        .map_err(|e| (StatusCode::NOT_FOUND, format!("文件打开失败: {e}")))?;

    let file_size = file
        .metadata()
        .await
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, format!("元数据读取失败: {e}")))?
        .len();

    if offset > 0 {
        use tokio::io::AsyncSeekExt;
        if let Err(e) = file.seek(std::io::SeekFrom::Start(offset)).await {
            return Err((StatusCode::INTERNAL_SERVER_ERROR, format!("Seek失败: {e}")));
        }
    }

    use tokio::io::AsyncReadExt;
    let remaining = file_size.saturating_sub(offset);
    let limited = file.take(remaining);
    let reader = CancelAwareReader {
        inner: limited,
        state: state.clone(),
        invite_id: id.clone(),
    };
    Ok(Body::from_stream(ReaderStream::new(reader)))
}

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

/// 流式读文件的包装：推送被取消时**就地断流**（返回 EOF），而不是把剩下的字节读完。
///
/// 接收方看到的是"响应提前结束"，它会退回重试并拿到 410，于是两边在对端取消后
/// 几百毫秒内一起收手；不这么做的话，取消一个正在传的大文件要等它传完才见效。
struct CancelAwareReader {
    inner: tokio::io::Take<tokio::fs::File>,
    state: Arc<AppState>,
    invite_id: String,
}

impl AsyncRead for CancelAwareReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if !self.state.push.has_job(&self.invite_id) {
            return Poll::Ready(Ok(())); // EOF
        }
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}