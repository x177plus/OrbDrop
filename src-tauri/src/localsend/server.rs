//! LocalSend v2.2 的**接收侧** HTTP 面。
//!
//! 挂在 `/api/localsend/v1|v2/*`（`server/mod.rs` 负责 mount；v1 只是别名）。
//! 与 OrbDrop 自己的推送不同：LocalSend 没有 token/签名体系，安全边界只有三条 ——
//! ① 默认关闭 + 离线即 503；② 每个文件的上传 token（`prepare-upload` 回给发送方）；
//! ③ token 绑来源 IP + 客户端证书指纹。文件名一律过 [`paths::safe_relative`]。
//!
//! 事件走 OrbDrop 现有的 `push://invite` / `push://progress` / `push://done`，
//! 额外带 `transport: "localsend"`，前端据此显示 LocalSend 徽标。

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::body::Body;
use axum::extract::{ConnectInfo, Extension, Query, Request, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use futures_util::StreamExt;
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::io::AsyncWriteExt;
use tracing::{debug, info, warn};

use crate::localsend::dto::{self, FileDto, PrepareUploadRequest, PrepareUploadResponse};
use crate::localsend::session::{self, Pending, SessionSummary, UploadDenied, UploadTicket};
use crate::localsend::{self, paths};
use crate::state::AppState;

/// 一条 `prepare-upload` 最多带多少个文件（LocalSend 自己也不设上限）。
const MAX_FILES: usize = 10_000;
/// 单个文件上限。LocalSend 允许任意大；接收侧必须有闸门。
const MAX_FILE_BYTES: u64 = 1 << 40; // 1 TiB
/// 一次会话总量上限。
const MAX_TOTAL_BYTES: u64 = 10 * (1 << 40); // 10 TiB
/// 文本消息的 `preview` 上限（LocalSend 的文本消息正文就塞在这里）。
pub(crate) const MAX_PREVIEW_BYTES: usize = 256 * 1024;
/// 自动接收图片的单张上限。比 OrbDrop 聊天图片的 8 MB 宽一点（手机照片常见更大），
/// 但不能无限：自动接收意味着没有用户点击那一步。
const MAX_AUTO_IMAGE_BYTES: u64 = 12 * 1024 * 1024;
/// 一批自动接收图片的条数/总量上限。
const MAX_AUTO_IMAGES: usize = 16;
const MAX_AUTO_IMAGE_TOTAL: u64 = 48 * 1024 * 1024;
/// `fileId` / `sessionId` / `token` 的长度上限：它们只是表键，不该是几 MB 的串。
const MAX_KEY_LEN: usize = 256;
/// 进度事件的节流间隔。
const PROGRESS_INTERVAL: Duration = Duration::from_millis(250);

/// 所有 LocalSend 路由共用的闸门：关闭 → 404；离线 → 503。
fn gate(state: &AppState) -> Option<Response> {
    if !crate::localsend::enabled(state) {
        return Some((StatusCode::NOT_FOUND, "Not Found").into_response());
    }
    if !state.is_online() {
        return Some((StatusCode::SERVICE_UNAVAILABLE, "Offline").into_response());
    }
    None
}

fn client_fingerprint(cert: &Option<Extension<crate::net::ClientCertFingerprint>>) -> Option<String> {
    cert.as_ref()
        .and_then(|Extension(fp)| fp.0.as_deref())
        .map(|s| s.to_ascii_lowercase())
}

fn bad_request(msg: &str) -> Response {
    (StatusCode::BAD_REQUEST, msg.to_string()).into_response()
}

fn query_value(params: &HashMap<String, String>, camel: &str, snake: &str) -> Option<String> {
    params
        .get(camel)
        .or_else(|| params.get(snake))
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && s.len() <= MAX_KEY_LEN)
}

/// 对端自报的别名：去掉控制字符并限长（它会进事件、日志与落盘目录名）。
fn clean_alias(raw: &str) -> String {
    let alias: String = raw
        .trim()
        .chars()
        .filter(|c| !c.is_control())
        .take(64)
        .collect();
    if alias.is_empty() {
        "LocalSend".to_string()
    } else {
        alias
    }
}

fn summary_error_status(status: StatusCode, msg: &str) -> Response {
    (status, msg.to_string()).into_response()
}

// ---------------------------------------------------------------------------
// info
// ---------------------------------------------------------------------------

/// GET /api/localsend/v1|v2/info
pub async fn info(State(state): State<Arc<AppState>>) -> Response {
    if let Some(r) = gate(&state) {
        return r;
    }
    Json(localsend::info_response(&state)).into_response()
}

// ---------------------------------------------------------------------------
// register
// ---------------------------------------------------------------------------

/// POST /api/localsend/v1|v2/register —— 对端公告自己，也用来探活。
///
/// 与 LocalSend 一致：指纹对不上**只忽略登记**，响应仍是本机的 info（200）——
/// 回 4xx 会让对端把"我们"从它的设备表里删掉。
pub async fn register(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    client_cert: Option<Extension<crate::net::ClientCertFingerprint>>,
    Json(body): Json<dto::DeviceInfo>,
) -> Response {
    if let Some(r) = gate(&state) {
        return r;
    }
    if body.port == 0 || !body.protocol.eq_ignore_ascii_case("https") {
        // 形状不对：同样只忽略，不建行（LocalSend 也没有明文 HTTP 传输）。
        info!(port = body.port, protocol = %body.protocol, "LocalSend register 被忽略：端口/协议不合法");
        return Json(localsend::info_response(&state)).into_response();
    }
    if !state
        .localsend
        .rate
        .lock()
        .allow(addr.ip(), 60, Duration::from_secs(10))
    {
        return summary_error_status(StatusCode::TOO_MANY_REQUESTS, "Too many requests");
    }

    let cert = client_fingerprint(&client_cert);
    let claimed = localsend::clean_fingerprint(&body.fingerprint);
    let sender_ip = addr.ip().to_string();
    // 出示了证书时以 TLS 握手证书为准；自报指纹只是"旧客户端/明文"的兜底。
    // 两样都没有时（第三方实现：不交客户端证书、自报值是设备 UUID），如果这个
    // `ip:port` 已经通过公告被我方登记过，沿用那一行的身份 —— 它还是同一台设备；
    // 没登记过就忽略（响应照旧是本机 info，不能让对端把我们从它的表里删掉）。
    let identity = match (&cert, &claimed) {
        (Some(fp), Some(body_fp)) if fp != body_fp => {
            info!(claimed = %body_fp, cert = %fp, "LocalSend register 被忽略：指纹与证书不符");
            return Json(localsend::info_response(&state)).into_response();
        }
        (Some(fp), _) => fp.clone(),
        // 自报指纹不能把一台**还在线**的设备从别的地址上顶掉（公告里明文广播，谁都能抄）。
        (None, Some(fp)) if state.localsend.claim_is_plausible(fp, &sender_ip) => fp.clone(),
        (None, Some(_)) => {
            info!(peer = %sender_ip, "LocalSend register 被忽略：自报指纹与在线设备冲突");
            return Json(localsend::info_response(&state)).into_response();
        }
        (None, None) => match state.localsend.find_by_endpoint(&sender_ip, body.port) {
            Some(peer) => peer.fingerprint,
            None => {
                info!("LocalSend register 被忽略：没有可用的指纹");
                return Json(localsend::info_response(&state)).into_response();
            }
        },
    };

    // 同机共存时，我们自己的独立监听也可能收到一条“来自本机”的 register
    // （例如本机第三方 LocalSend 的公告被我们拨回了自己）：不登记，免得设备列表里
    // 多出一台自己。响应照旧是本机 info。
    if identity == localsend::own_fingerprint() {
        info!("LocalSend register 来自本机自身，忽略登记");
        return Json(localsend::info_response(&state)).into_response();
    }

    let now = Instant::now();
    let peer = localsend::LocalSendPeer {
        id: localsend::id_for_fingerprint(&identity),
        fingerprint: identity,
        alias: clean_alias(&body.alias),
        device_model: body.device_model.clone(),
        device_type: body.device_type.clone(),
        ip: addr.ip().to_string(),
        port: body.port,
        protocol: body.protocol.clone(),
        download: body.download,
        online: true,
        seen_at: now,
        last_seen: now,
        certified: cert.is_some(),
    };
    let known = state.localsend.get(&peer.id).is_some();
    let peer = state.localsend.upsert(peer);
    // 对端会周期性公告（本机也把它当探活使）：首次登记是事件，之后每次刷新只是
    // 同一台设备"还在"，按 INFO 记就是 5 秒一条的刷屏。
    if known {
        debug!(peer = %peer.id, ip = %peer.ip, port = peer.port, "LocalSend register（刷新）");
    } else {
        info!(peer = %peer.id, ip = %peer.ip, port = peer.port, "LocalSend register");
    }
    state.emit_devices_changed();
    Json(localsend::info_response(&state)).into_response()
}

// ---------------------------------------------------------------------------
// prepare-upload
// ---------------------------------------------------------------------------

/// 文本消息判定（LocalSend 的约定：单文件 + `text/*` + `preview` 里放正文）。
///
/// 返回 `Some(正文)` 才是文本消息；`preview` 超过上限就按普通文件处理，
/// 免得一条几 MB 的"文本"被塞进事件载荷。
fn text_message(files: &HashMap<String, FileDto>) -> Option<String> {
    if files.len() != 1 {
        return None;
    }
    let f = files.values().next()?;
    if !f.file_type.to_ascii_lowercase().starts_with("text/") {
        return None;
    }
    if f.file_name.contains('/') {
        return None;
    }
    let preview = f.preview.as_ref()?;
    if preview.len() > MAX_PREVIEW_BYTES || preview.is_empty() {
        return None;
    }
    Some(preview.clone())
}

/// 这一批是不是**可以自动接收的图片**。
///
/// 条件刻意收得很紧：
/// - 整批每一件都是图片 MIME + 我们认识的图片扩展名（`blobs`/前端预览共用同一份名单）；
/// - 不含路径分隔符（图片就是图片，不是目录树）；
/// - 单张/整批大小与条数都有上限。
///
/// 混进任意一个非图片文件就整批走普通文件确认 —— 不在一个 `prepare-upload` 里拆分，
/// 否则对端的“接受/拒绝”语义会对不上。
fn auto_image_batch(files: &HashMap<String, FileDto>) -> bool {
    !files.is_empty()
        && files.len() <= MAX_AUTO_IMAGES
        && files.values().all(|f| {
            f.file_type.to_ascii_lowercase().starts_with("image/")
                && !f.file_name.contains('/')
                && !f.file_name.contains('\\')
                && f.size > 0
                && f.size <= MAX_AUTO_IMAGE_BYTES
                && crate::paste::image_ext_of(&f.file_name).is_some()
        })
        && files.values().map(|f| f.size).sum::<u64>() <= MAX_AUTO_IMAGE_TOTAL
}

/// 校验并规整一份清单。`Err` = 整条请求该被拒绝。
fn validate_files(
    files: &HashMap<String, FileDto>,
) -> Result<HashMap<String, FileDto>, (StatusCode, String)> {
    if files.is_empty() {
        return Err((StatusCode::BAD_REQUEST, "没有文件".into()));
    }
    if files.len() > MAX_FILES {
        return Err((
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("文件太多（上限 {MAX_FILES}）"),
        ));
    }
    let mut total: u64 = 0;
    let mut out = HashMap::with_capacity(files.len());
    for (id, f) in files.iter() {
        if id.is_empty() || id.len() > MAX_KEY_LEN || f.id.is_empty() || f.id.len() > MAX_KEY_LEN {
            return Err((StatusCode::BAD_REQUEST, "文件 id 不合法".into()));
        }
        if f.size > MAX_FILE_BYTES {
            return Err((StatusCode::PAYLOAD_TOO_LARGE, "单个文件太大".into()));
        }
        total = total
            .checked_add(f.size)
            .filter(|t| *t <= MAX_TOTAL_BYTES)
            .ok_or_else(|| (StatusCode::PAYLOAD_TOO_LARGE, "总大小超限".to_string()))?;
        // 文件名是唯一的落点来源：不安全就整条拒绝（静默改名会让两边的清单对不上）。
        if paths::safe_relative(&f.file_name).is_none() {
            return Err((StatusCode::BAD_REQUEST, "文件名不合法".into()));
        }
        let mut f = f.clone();
        if let Some(sha) = f.sha256.as_ref() {
            if sha.len() != 64 || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
                return Err((StatusCode::BAD_REQUEST, "sha256 格式不合法".into()));
            }
            f.sha256 = Some(sha.to_ascii_lowercase());
        }
        out.insert(id.clone(), f);
    }
    Ok(out)
}

/// 用户点了「拒绝」/ 发送方取消 / 请求被丢掉时，把还挂着的 pending 清掉。
struct PendingGuard {
    state: Arc<AppState>,
    invite_id: String,
    armed: bool,
}

impl PendingGuard {
    fn new(state: &Arc<AppState>, invite_id: &str) -> Self {
        Self {
            state: state.clone(),
            invite_id: invite_id.to_string(),
            armed: true,
        }
    }

    fn disarm(mut self) {
        self.armed = false;
    }
}

impl Drop for PendingGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if self.state.localsend.sessions.remove_pending(&self.invite_id) {
            info!(invite = %self.invite_id, "LocalSend 邀请随连接一起作废");
        }
    }
}

/// POST /api/localsend/v1|v2/prepare-upload
///
/// 这条请求**一直挂着等用户决定**（与 LocalSend App 的行为一致）：没有总超时，
/// 只靠发送方取消（`/cancel`）或连接断开（`PendingGuard`）收场。
pub async fn prepare_upload(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    client_cert: Option<Extension<crate::net::ClientCertFingerprint>>,
    Json(body): Json<PrepareUploadRequest>,
) -> Response {
    if let Some(r) = gate(&state) {
        return r;
    }
    if !state
        .localsend
        .rate
        .lock()
        .allow(addr.ip(), 20, Duration::from_secs(10))
    {
        return summary_error_status(StatusCode::TOO_MANY_REQUESTS, "Too many requests");
    }

    let mut files = match validate_files(&body.files) {
        Ok(files) => files,
        Err((status, msg)) => {
            info!(%msg, "LocalSend prepare-upload 被拒");
            return (status, msg).into_response();
        }
    };

    let sender_ip = addr.ip().to_string();
    let cert = client_fingerprint(&client_cert);
    // 身份优先级：客户端证书 > 合规且不冲突的自报指纹 > **同端点已登记的行** > 来源 IP。
    // 第三方实现（不交客户端证书、自报值是设备 UUID）靠第三档归到公告登记的那台设备，
    // 否则通知/会话会凭空多出一个以 IP 为 id 的"第二台设备"。
    //
    // 自报指纹是明文广播的，谁都能抄：它只在这一行**不是从别的地址**冒出来时才算数
    // （见 `LocalSendState::claim_is_plausible`）。不这么收，一段里任何机器都能顶着
    // 别人的身份往它的会话里塞文本/图片。
    let claimed = localsend::clean_fingerprint(&body.info.fingerprint)
        .filter(|fp| state.localsend.claim_is_plausible(fp, &sender_ip));
    let identity = cert
        .clone()
        .or(claimed)
        .or_else(|| {
            state
                .localsend
                .find_by_endpoint(&sender_ip, body.info.port)
                .map(|peer| peer.fingerprint)
        })
        .unwrap_or_else(|| sender_ip.clone());
    // 本机指纹是公开的（公告与 `/info` 都带）：不让任何人用它造出一台"自己"。
    if identity == localsend::own_fingerprint() {
        info!("LocalSend prepare-upload 用本机指纹自称，已拒绝");
        return summary_error_status(StatusCode::FORBIDDEN, "Self");
    }
    let sender_id = localsend::id_for_fingerprint(&identity);
    let alias = clean_alias(&body.info.alias);

    // 对端可能直接调 prepare-upload（手动 IP / 没先 register）：把这条发送方也登记进
    // LocalSend 表，否则会话/通知里会指向一个设备列表里没有的 id。
    {
        let now = Instant::now();
        state.localsend.upsert(localsend::LocalSendPeer {
            id: sender_id.clone(),
            fingerprint: identity.clone(),
            alias: alias.clone(),
            device_model: body.info.device_model.clone(),
            device_type: body.info.device_type.clone(),
            ip: sender_ip.clone(),
            port: body.info.port,
            protocol: body.info.protocol.clone(),
            download: body.info.download,
            online: true,
            seen_at: now,
            last_seen: now,
            certified: cert.is_some(),
        });
        state.emit_devices_changed();
    }

    let message = text_message(&files);
    if message.is_none() {
        // `preview` 只是文本消息的正文，只有下面那条路用。普通文件/图片把对端塞进来的
        // 整串预览原样留在内存里没有用处（一份清单最多 64MB，还会跟着会话活到回收）。
        for f in files.values_mut() {
            f.preview = None;
        }
    }

    // 文本消息：始终直接落一条 LocalSend 消息并回 204（没有「先同意」这一档）。
    // 不建 Pending、不占单会话槽 —— 文件正在传的时候收文字也不会 409。
    if let Some(text) = message.as_ref() {
        let file_id = files.keys().next().cloned().unwrap_or_default();
        let message_id = format!("ls:{file_id}");
        let (fresh, _) = state.chat.record_localsend_inbound_text(
            &sender_id,
            &alias,
            &message_id,
            text,
            crate::chat::now_ms(),
        );
        crate::group::emit(&state);
        if fresh {
            crate::notify::message(
                &crate::notify::Convo::Direct(sender_id.clone()),
                &alias,
                text,
            )
            .await;
        }
        info!(sender = %alias, bytes = text.len(), "LocalSend 文本消息已自动接收");
        return StatusCode::NO_CONTENT.into_response();
    }

    let count = files.len();
    let total: u64 = files.values().map(|f| f.size).sum();
    let name = if count == 1 {
        files
            .values()
            .next()
            .map(|f| f.file_name.clone())
            .unwrap_or_else(|| "1 个文件".to_string())
    } else {
        format!("{count} 个文件")
    };
    let is_dir = files.values().any(|f| f.file_name.contains('/'));
    let mut auto_images = auto_image_batch(&files);

    let invite_id = uuid::Uuid::new_v4().to_string();
    let session_id = uuid::Uuid::new_v4().to_string();

    // 全图片批次 + 自动接收：直接建 active 会话并回 token。没有通知卡、没有「接收」，
    // 也不占单会话槽（普通文件正在传时图片照样能进来）。上传完成后由 `upload` 把文件
    // 封成图片消息（`localsend_messages`）。
    //
    // 静默落盘有小时预算（跨发送方）：超了的那一批降级成普通文件、等用户点「接收」。
    if auto_images && !state.localsend.take_auto_image_budget(total) {
        info!(sender = %alias, total, "LocalSend 自动接收图片超出小时预算，改为等待用户确认");
        auto_images = false;
    }
    if auto_images {
        let tokens = match state.localsend.sessions.start_auto_images(
            &state,
            invite_id.clone(),
            session_id.clone(),
            sender_ip.clone(),
            cert.clone(),
            sender_id.clone(),
            alias.clone(),
            files.clone(),
        ) {
            Ok(tokens) => tokens,
            Err(e) => {
                warn!(%invite_id, %e, "LocalSend 自动接收图片失败");
                return summary_error_status(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "Cannot accept images",
                );
            }
        };
        return Json(PrepareUploadResponse {
            session_id,
            files: tokens,
        })
        .into_response();
    }

    let (tx, rx) = tokio::sync::oneshot::channel::<session::Decision>();
    let cancel = tokio_util::sync::CancellationToken::new();

    let pending = Pending {
        invite_id: invite_id.clone(),
        session_id: session_id.clone(),
        sender_ip: sender_ip.clone(),
        fingerprint: cert.clone(),
        sender_id: sender_id.clone(),
        alias: alias.clone(),
        files: files.clone(),
        decision: Some(tx),
        cancel: cancel.clone(),
        root: None,
    };

    // 单会话槽：一条还没决定完 / 一条还在传，都回 409（LocalSend 原话）。
    if state.localsend.sessions.claim_slot(pending).is_err() {
        return summary_error_status(StatusCode::CONFLICT, "Blocked by another session");
    }

    let guard = PendingGuard::new(&state, &invite_id);

    let event = json!({
        "inviteId": invite_id,
        "sender": { "id": sender_id, "name": alias },
        "name": name,
        "size": total,
        "isDir": is_dir,
        "count": count,
        "auto": false,
        "transport": "localsend",
    });
    crate::ws::emit_ui(&state, crate::ws::events::ui::PUSH_INVITE, &event);
    info!(%invite_id, sender = %alias, count, total, "LocalSend prepare-upload 等待用户决定");

    let decision = tokio::select! {
        d = rx => d.ok(),
        _ = cancel.cancelled() => None,
    };

    match decision {
        Some(session::Decision::Accept) => {
            guard.disarm();
            match state.localsend.sessions.tokens_for(&session_id) {
                Some(files) => {
                    Json(PrepareUploadResponse { session_id, files }).into_response()
                }
                None => {
                    // 接受了但会话不见了（用户在此期间取消/离线）—— 如实报错，别让对端挂死。
                    warn!(%invite_id, "LocalSend 会话在接受后消失");
                    summary_error_status(StatusCode::CONFLICT, "Session cancelled")
                }
            }
        }
        // 拒绝、发送方取消、pending 被外部删掉（离线清理）。
        _ => {
            guard.disarm();
            summary_error_status(StatusCode::FORBIDDEN, "Rejected")
        }
    }
}

// ---------------------------------------------------------------------------
// upload
// ---------------------------------------------------------------------------

fn emit_progress(state: &AppState, session_id: &str, invite_id: &str) {
    if let Some((received, total)) = state.localsend.sessions.progress(session_id) {
        crate::ws::emit_ui(
            state,
            crate::ws::events::ui::PUSH_PROGRESS,
            &json!({
                "inviteId": invite_id,
                "received": received,
                "total": total,
                "transport": "localsend",
            }),
        );
    }
}

fn emit_done(state: &AppState, summary: &SessionSummary) {
    // 自动接收的图片不是文件卡：收尾只落图片消息（在 `upload` 里做），不发 `push://done`。
    if summary.auto_images {
        return;
    }
    let status = if summary.completed { "completed" } else { "failed" };
    let mut payload = json!({
        "inviteId": summary.invite_id,
        "status": status,
        "dest": summary.root.to_string_lossy(),
        "total": summary.total,
        "transport": "localsend",
    });
    if !summary.completed {
        payload["reason"] = json!(format!("部分文件失败: {}", summary.failed.join(", ")));
    }
    crate::ws::emit_ui(state, crate::ws::events::ui::PUSH_DONE, &payload);
}

/// 一次上传尝试失败后的收尾（删残片、记状态、必要时发 `PUSH_DONE`）。
fn fail_upload(state: &AppState, ticket: &UploadTicket, retryable: bool) {
    let _ = std::fs::remove_file(&ticket.part);
    if let Some(summary) =
        state
            .localsend
            .sessions
            .mark_failed(&ticket.session_id, &ticket.file_id, retryable)
    {
        emit_done(state, &summary);
    }
}

fn hex_lower(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0f) as usize] as char);
    }
    out
}

/// POST /api/localsend/v1|v2/upload?sessionId=&fileId=&token=
///
/// 流式写 `.part`，字节数必须**精确**等于清单里的 `size`；有 `sha256` 就边写边算。
/// 成功后 `rename` 到最终路径（同目录，不跨卷），失败删残片、允许同一 token 重试（≤3 次）。
pub async fn upload(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    client_cert: Option<Extension<crate::net::ClientCertFingerprint>>,
    Query(params): Query<HashMap<String, String>>,
    req: Request<Body>,
) -> Response {
    if let Some(r) = gate(&state) {
        return r;
    }
    let Some(session_id) = query_value(&params, "sessionId", "session_id") else {
        return bad_request("Missing 'sessionId'");
    };
    let Some(file_id) = query_value(&params, "fileId", "file_id") else {
        return bad_request("Missing 'fileId'");
    };
    let Some(token) = query_value(&params, "token", "token") else {
        return bad_request("Missing 'token'");
    };

    let sender_ip = addr.ip().to_string();
    let cert = client_fingerprint(&client_cert);
    let ticket = match state.localsend.sessions.begin_upload(
        &session_id,
        &file_id,
        &token,
        &sender_ip,
        cert.as_deref(),
    ) {
        Ok(t) => t,
        Err(UploadDenied::NotFound) => {
            return summary_error_status(StatusCode::NOT_FOUND, "Unknown session or file")
        }
        Err(UploadDenied::Forbidden) => {
            return summary_error_status(StatusCode::FORBIDDEN, "Invalid token or IP address")
        }
        Err(UploadDenied::Exhausted) => {
            return summary_error_status(StatusCode::CONFLICT, "Too many attempts")
        }
    };

    // 声明长度能提前拒绝一个明显超大的请求，但**不能**当作校验依据：只有实际写到的
    // 字节数才算数。
    if let Some(len) = req
        .headers()
        .get(header::CONTENT_LENGTH)
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.parse::<u64>().ok())
    {
        if len > ticket.size {
            fail_upload(&state, &ticket, ticket.attempt < session::MAX_ATTEMPTS);
            return summary_error_status(StatusCode::BAD_REQUEST, "Content-Length exceeds declared size");
        }
    }

    let mut file = match tokio::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&ticket.part)
        .await
    {
        Ok(f) => f,
        Err(e) => {
            warn!(part = %ticket.part.display(), %e, "LocalSend 上传：临时文件打不开");
            fail_upload(&state, &ticket, ticket.attempt < session::MAX_ATTEMPTS);
            return summary_error_status(StatusCode::INTERNAL_SERVER_ERROR, "Cannot create file");
        }
    };

    let mut hasher = Sha256::new();
    let mut written: u64 = 0;
    let mut last_emit = Instant::now();
    let mut stream = std::pin::pin!(req.into_body().into_data_stream());

    loop {
        let next = tokio::select! {
            n = stream.next() => n,
            _ = ticket.cancel.cancelled() => {
                drop(file);
                fail_upload(&state, &ticket, ticket.attempt < session::MAX_ATTEMPTS);
                return summary_error_status(StatusCode::FORBIDDEN, "Cancelled");
            }
        };
        let Some(chunk) = next else { break };
        let chunk = match chunk {
            Ok(c) => c,
            Err(e) => {
                warn!(%session_id, %file_id, %e, "LocalSend 上传：读请求体失败");
                drop(file);
                fail_upload(&state, &ticket, ticket.attempt < session::MAX_ATTEMPTS);
                return summary_error_status(StatusCode::BAD_REQUEST, "Broken body");
            }
        };
        if chunk.is_empty() {
            continue;
        }
        written += chunk.len() as u64;
        if written > ticket.size {
            drop(file);
            fail_upload(&state, &ticket, ticket.attempt < session::MAX_ATTEMPTS);
            return summary_error_status(StatusCode::PAYLOAD_TOO_LARGE, "Body larger than declared size");
        }
        hasher.update(&chunk);
        if let Err(e) = file.write_all(&chunk).await {
            warn!(%session_id, %file_id, %e, "LocalSend 上传：写盘失败");
            drop(file);
            fail_upload(&state, &ticket, ticket.attempt < session::MAX_ATTEMPTS);
            return summary_error_status(StatusCode::INSUFFICIENT_STORAGE, "Write failed");
        }
        if last_emit.elapsed() >= PROGRESS_INTERVAL {
            state
                .localsend
                .sessions
                .set_received(&ticket.session_id, &ticket.file_id, written);
            if !ticket.auto_image {
                emit_progress(&state, &ticket.session_id, &ticket.invite_id);
            }
            last_emit = Instant::now();
        }
    }

    if let Err(e) = file.flush().await {
        warn!(%session_id, %file_id, %e, "LocalSend 上传：flush 失败");
        drop(file);
        fail_upload(&state, &ticket, ticket.attempt < session::MAX_ATTEMPTS);
        return summary_error_status(StatusCode::INSUFFICIENT_STORAGE, "Write failed");
    }
    // Windows 上必须先关句柄再 rename。
    drop(file);

    if written != ticket.size {
        fail_upload(&state, &ticket, ticket.attempt < session::MAX_ATTEMPTS);
        return summary_error_status(StatusCode::BAD_REQUEST, "Size mismatch");
    }

    if let Some(expected) = ticket.sha256.as_deref() {
        let got = hex_lower(&hasher.finalize());
        if !expected.eq_ignore_ascii_case(&got) {
            warn!(%session_id, %file_id, "LocalSend 上传：sha256 校验失败");
            fail_upload(&state, &ticket, ticket.attempt < session::MAX_ATTEMPTS);
            return summary_error_status(StatusCode::UNPROCESSABLE_ENTITY, "Checksum mismatch");
        }
    }

    if let Some(parent) = ticket.target.parent() {
        if let Err(e) = tokio::fs::create_dir_all(parent).await {
            warn!(part = %ticket.part.display(), %e, "LocalSend 上传：创建目录失败");
            fail_upload(&state, &ticket, ticket.attempt < session::MAX_ATTEMPTS);
            return summary_error_status(StatusCode::INTERNAL_SERVER_ERROR, "Cannot create directory");
        }
    }
    if let Err(e) = tokio::fs::rename(&ticket.part, &ticket.target).await {
        warn!(part = %ticket.part.display(), target = %ticket.target.display(), %e, "LocalSend 上传：rename 失败");
        fail_upload(&state, &ticket, ticket.attempt < session::MAX_ATTEMPTS);
        return summary_error_status(StatusCode::INTERNAL_SERVER_ERROR, "Cannot finalize file");
    }

    // 自动接收的图片：把刚落盘的原始字节封成 blobs 内容，落一条图片消息。
    // 注意顺序：先 `save_in`（它自己会原子重写目标路径），再记账；任何一步失败都
    // 删掉这份半成品并让上传走可重试的失败路径。
    if ticket.auto_image {
        let root = state.chat.blobs_root().to_path_buf();
        let raw = match tokio::fs::read(&ticket.target).await {
            Ok(raw) => raw,
            Err(e) => {
                warn!(target = %ticket.target.display(), %e, "LocalSend 自动图片：读回落盘文件失败");
                let _ = tokio::fs::remove_file(&ticket.target).await;
                fail_upload(&state, &ticket, ticket.attempt < session::MAX_ATTEMPTS);
                return summary_error_status(StatusCode::INTERNAL_SERVER_ERROR, "Cannot read image");
            }
        };
        if let Err(e) =
            crate::blobs::save_in(&root, &ticket.message_id, &ticket.file_name, &raw)
        {
            warn!(target = %ticket.target.display(), %e, "LocalSend 自动图片：写入图片内容失败");
            let _ = tokio::fs::remove_file(&ticket.target).await;
            fail_upload(&state, &ticket, ticket.attempt < session::MAX_ATTEMPTS);
            return summary_error_status(StatusCode::INTERNAL_SERVER_ERROR, "Cannot store image");
        }
        let (fresh, _) = state.chat.record_localsend_inbound_image(
            &ticket.sender_id,
            &ticket.alias,
            &ticket.message_id,
            &ticket.file_name,
            ticket.size,
            crate::chat::now_ms(),
        );
        crate::group::emit(&state);
        if fresh {
            crate::notify::message(
                &crate::notify::Convo::Direct(ticket.sender_id.clone()),
                &ticket.alias,
                crate::i18n::t("notify.image"),
            )
            .await;
        }
    }

    state
        .localsend
        .sessions
        .set_received(&ticket.session_id, &ticket.file_id, ticket.size);
    if !ticket.auto_image {
        emit_progress(&state, &ticket.session_id, &ticket.invite_id);
    }
    if let Some(summary) = state
        .localsend
        .sessions
        .mark_finished(&ticket.session_id, &ticket.file_id)
    {
        emit_done(&state, &summary);
    }
    info!(%session_id, %file_id, size = ticket.size, "LocalSend 上传完成");
    StatusCode::OK.into_response()
}

// ---------------------------------------------------------------------------
// cancel
// ---------------------------------------------------------------------------

/// POST /api/localsend/v1|v2/cancel?sessionId=
///
/// 与 LocalSend 一致：认不认识都回 200（对方只看"我这边可以放手了"）。
pub async fn cancel(
    State(state): State<Arc<AppState>>,
    ConnectInfo(addr): ConnectInfo<SocketAddr>,
    client_cert: Option<Extension<crate::net::ClientCertFingerprint>>,
    Query(params): Query<HashMap<String, String>>,
) -> Response {
    if let Some(r) = gate(&state) {
        return r;
    }
    let session_id = query_value(&params, "sessionId", "session_id");
    let sender_ip = addr.ip().to_string();
    let cert = client_fingerprint(&client_cert);

    let pending_cancelled = state.localsend.sessions.cancel_pending_from(
        session_id.as_deref(),
        &sender_ip,
        cert.as_deref(),
    );
    if !pending_cancelled {
        if let Some(sid) = session_id.as_deref() {
            // 会话 id 是不可猜的 UUID；到了这一层才允许凭它整条撤回。
            if state.localsend.sessions.cancel_active(sid) {
                info!(session = %sid, "LocalSend 会话被发送方取消");
            }
        }
    }
    StatusCode::OK.into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(name: &str, size: u64, file_type: &str, preview: Option<&str>) -> FileDto {
        FileDto {
            id: "f".to_string(),
            file_name: name.to_string(),
            size,
            file_type: file_type.to_string(),
            sha256: None,
            preview: preview.map(|s| s.to_string()),
            metadata: None,
        }
    }

    #[test]
    fn text_message_requires_single_plain_file_with_preview() {
        let mut files = HashMap::new();
        files.insert(
            "f".to_string(),
            file("msg.txt", 3, "text/plain", Some("你好")),
        );
        assert_eq!(text_message(&files).as_deref(), Some("你好"));

        // 多个文件不是文本消息
        let mut many = files.clone();
        many.insert("g".to_string(), file("b.txt", 1, "text/plain", Some("x")));
        assert_eq!(text_message(&many), None);

        // 非 text/* 不是
        let mut binary = HashMap::new();
        binary.insert("f".to_string(), file("a.png", 3, "image/png", Some("hi")));
        assert_eq!(text_message(&binary), None);

        // 没有 preview 不是
        let mut no_preview = HashMap::new();
        no_preview.insert("f".to_string(), file("a.txt", 3, "text/plain", None));
        assert_eq!(text_message(&no_preview), None);

        // 超过上限按普通文件
        let big = "x".repeat(MAX_PREVIEW_BYTES + 1);
        let mut too_big = HashMap::new();
        too_big.insert("f".to_string(), file("a.txt", 3, "text/plain", Some(&big)));
        assert_eq!(text_message(&too_big), None);

        // 带目录的文本按文件收
        let mut nested = HashMap::new();
        nested.insert("f".to_string(), file("dir/a.txt", 3, "text/plain", Some("x")));
        assert_eq!(text_message(&nested), None);
    }

    #[test]
    fn manifest_validation_rejects_hostile_input() {
        // 空清单
        assert!(validate_files(&HashMap::new()).is_err());

        // 路径穿越
        let mut evil = HashMap::new();
        evil.insert("f".to_string(), file("../etc/passwd", 1, "text/plain", None));
        assert!(validate_files(&evil).is_err());

        // 绝对路径
        let mut abs = HashMap::new();
        abs.insert("f".to_string(), file("/etc/passwd", 1, "text/plain", None));
        assert!(validate_files(&abs).is_err());

        // 超长 id
        let mut big_id = HashMap::new();
        big_id.insert("i".repeat(MAX_KEY_LEN + 1), file("a.bin", 1, "application/octet-stream", None));
        assert!(validate_files(&big_id).is_err());

        // sha256 形状
        let mut bad_sha = HashMap::new();
        let mut f = file("a.bin", 1, "application/octet-stream", None);
        f.sha256 = Some("zz".to_string());
        bad_sha.insert("f".to_string(), f);
        assert!(validate_files(&bad_sha).is_err());

        // 合法（大小写归一）
        let mut ok = HashMap::new();
        let mut f = file("a.bin", 1, "application/octet-stream", None);
        f.sha256 = Some("AB".repeat(32));
        ok.insert("f".to_string(), f);
        let expected = "ab".repeat(32);
        let out = validate_files(&ok).unwrap();
        assert_eq!(out["f"].sha256.as_deref(), Some(expected.as_str()));
    }

    #[test]
    fn manifest_limits_are_enforced() {
        let mut too_big = HashMap::new();
        too_big.insert("f".to_string(), file("a.bin", MAX_FILE_BYTES + 1, "application/octet-stream", None));
        assert_eq!(
            validate_files(&too_big).unwrap_err().0,
            StatusCode::PAYLOAD_TOO_LARGE
        );
    }

    #[test]
    fn hex_encoding_matches_sha256_text() {
        assert_eq!(hex_lower(&[0x00, 0x0f, 0xff]), "000fff");
    }

    #[test]
    fn auto_image_batch_requires_all_safe_images() {
        let mut only_images = HashMap::new();
        only_images.insert("a".to_string(), file("a.png", 10, "image/png", None));
        only_images.insert("b".to_string(), file("b.jpg", 20, "image/jpeg", None));
        assert!(auto_image_batch(&only_images));

        let mut mixed = only_images.clone();
        mixed.insert("c".to_string(), file("c.txt", 3, "text/plain", None));
        assert!(!auto_image_batch(&mixed));

        let mut svg = HashMap::new();
        svg.insert("a".to_string(), file("a.svg", 10, "image/svg+xml", None));
        assert!(!auto_image_batch(&svg));

        let mut too_big = HashMap::new();
        too_big.insert(
            "a".to_string(),
            file("a.png", MAX_AUTO_IMAGE_BYTES + 1, "image/png", None),
        );
        assert!(!auto_image_batch(&too_big));
    }
}