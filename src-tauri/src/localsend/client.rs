//! LocalSend 出站发送（`prepare-upload` + `/upload`）。
//!
//! 与 OrbDrop 自己的推送数据面完全独立：文件推送**不建 `PushJob`**，进度/终态只走
//! `transfer::push_job::batch_set`（批次卡那一行），文件体由本模块流式推给对方。
//! 文本消息走 `send_text`：官方约定下只需 `prepare-upload` 回 `204`，不建会话/不推进度。
//!
//! 要点：
//! - 身份仍按证书指纹钉死（[`crate::net::DialPin::Fingerprint`]，小写）；
//! - `prepare-upload` **不设总超时**（对端可能要等用户点「接受」），只给连接 5s；
//! - `/upload` 用流式客户端：不设总时长，但每次 socket 读有 30s 空闲上限，避免
//!   对端收下请求后不再动、把这台 worker 永久挂住；
//! - 进度最多每 ~200ms 报一次（四路并发共用同一个节流点），取消靠 `CancellationToken`
//!   直接丢弃请求 future（连接随即断开），并在已知 `sessionId` 时补一条 `/cancel`。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use parking_lot::Mutex;
use tokio::io::AsyncReadExt;
use tokio_util::io::ReaderStream;
use tokio_util::sync::CancellationToken;
use tracing::{debug, warn};
use uuid::Uuid;

use crate::localsend::dto;
use crate::localsend::LocalSendPeer;
use crate::net::{DialPin, Trust};
use crate::state::AppState;
use crate::transfer::push_job::TargetState;
use crate::transfer::PushSource;

/// 建连上限。`prepare-upload` 的响应可能等几分钟（用户还没点），所以只有这一条时限。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// `/upload` 的单次 socket 读空闲上限：传大文件不设总时长，但不能无限等。
const UPLOAD_READ_TIMEOUT: Duration = Duration::from_secs(30);
/// 进度上报节流（四路上传共用一瓶）。
const PROGRESS_INTERVAL: Duration = Duration::from_millis(200);
/// 同时最多几路上传。LocalSend 官方接收端也是逐条处理，四路足够吃满局域网。
const MAX_PARALLEL_UPLOADS: usize = 4;
/// `prepare-upload` 回话上限：一万个文件的 token 表也就几百 KB，留足余量。
const PREPARE_BODY_MAX: usize = 8 * 1024 * 1024;

/// 发送失败的种类：界面那一行的终态与错误文案各不相同。
enum Failure {
    Cancelled,
    Rejected(String),
    Failed(String),
}

impl From<String> for Failure {
    fn from(value: String) -> Self {
        Failure::Failed(value)
    }
}

/// 一次在途的出站会话。不落盘，只服务于 `cancel()`（它拿不到 `run_send` 的局部变量）。
#[derive(Clone)]
struct OutboundMeta {
    ip: String,
    port: u16,
    fingerprint: String,
    session_id: Option<String>,
}

fn outbound_meta() -> &'static Mutex<HashMap<String, OutboundMeta>> {
    static META: LazyLock<Mutex<HashMap<String, OutboundMeta>>> =
        LazyLock::new(|| Mutex::new(HashMap::new()));
    &META
}

fn set_session_id(invite_id: &str, session_id: &str) {
    if let Some(meta) = outbound_meta().lock().get_mut(invite_id) {
        meta.session_id = Some(session_id.to_string());
    }
}

fn clear_meta(invite_id: &str) {
    outbound_meta().lock().remove(invite_id);
}

/// 把文件发到一台 LocalSend 对端。
///
/// `batch_id` 只用于日志；界面上那条目标行由 `batch_set` 按 `invite_id` 推进。
pub async fn send_files(
    state: Arc<AppState>,
    peer: LocalSendPeer,
    batch_id: String,
    invite_id: String,
    source: PushSource,
    cancel: CancellationToken,
) -> Result<(), String> {
    debug!(
        batch = %batch_id,
        invite = %invite_id,
        peer = %peer.alias,
        files = source.files.len(),
        total = source.total,
        "localsend: 开始出站发送"
    );
    if !crate::localsend::enabled(&state) || !state.is_online() {
        return Err("LocalSend 兼容已关闭".to_string());
    }

    // 令牌由调用方建（排进 lane 的那一刻就登记），排队期间点取消同样能收掉。
    state
        .localsend
        .register_outbound(invite_id.clone(), cancel.clone());
    outbound_meta().lock().insert(
        invite_id.clone(),
        OutboundMeta {
            ip: peer.ip.clone(),
            port: peer.port,
            fingerprint: peer.fingerprint.clone(),
            session_id: None,
        },
    );

    let outcome = tokio::select! {
        _ = cancel.cancelled() => Err(Failure::Cancelled),
        r = run_send(&state, &peer, &invite_id, &source) => r,
    };

    // 先摘掉登记再动批次：`cancel()` 之后再来一次也只会看到空表。
    state.localsend.unregister_outbound(&invite_id);
    clear_meta(&invite_id);

    match outcome {
        Ok(()) => {
            crate::transfer::push_job::batch_set(
                &state,
                &invite_id,
                TargetState::Done,
                Some(source.total),
                None,
                true,
            );
            Ok(())
        }
        Err(Failure::Cancelled) => {
            crate::transfer::push_job::batch_set(
                &state,
                &invite_id,
                TargetState::Cancelled,
                None,
                Some("已取消".to_string()),
                true,
            );
            Ok(())
        }
        Err(Failure::Rejected(reason)) => {
            crate::transfer::push_job::batch_set(
                &state,
                &invite_id,
                TargetState::Rejected,
                None,
                Some(reason.clone()),
                true,
            );
            Err(reason)
        }
        Err(Failure::Failed(reason)) => {
            crate::transfer::push_job::batch_set(
                &state,
                &invite_id,
                TargetState::Failed,
                None,
                Some(reason.clone()),
                true,
            );
            Err(reason)
        }
    }
}

/// 发一条 LocalSend **文本消息**（官方约定：单文件 + `text/plain` + `preview`）。
///
/// 正常路径只需要 `prepare-upload` 回 `204`：对端把 `preview` 当消息，不需要任何
/// `/upload`。少数第三方实现会把它当普通文件回 `200` 并给 token；这时把正文原样
/// 上传成 `.txt`，保证对方至少能收到内容（但对面看到的是文件，不是消息）。
///
/// 这条路径**不建 PushJob / 不推进度**：文本消息没有文件卡，终态由调用方写回
/// `chat.localsend_messages`。
pub async fn send_text(
    state: &Arc<AppState>,
    peer: &LocalSendPeer,
    text: &str,
) -> Result<(), String> {
    // 排队期间用户可能已经把开关关掉 / 切到离线：排队的那几条不该在"关掉之后"还发出去。
    if !crate::localsend::enabled(state) || !state.is_online() {
        return Err("LocalSend 兼容已关闭".to_string());
    }
    let pin = pin_for(&peer.fingerprint);
    let client = new_client(&peer.ip, &pin).map_err(failure_text)?;
    let base = format!("https://{}", authority(&peer.ip, peer.port));
    let id = Uuid::new_v4().to_string();
    let size = text.as_bytes().len() as u64;
    let file = dto::FileDto {
        id: id.clone(),
        // 官方客户端也是 `<uuid>.txt`；接收端真正显示的是 `preview`。
        file_name: format!("{id}.txt"),
        size,
        file_type: "text/plain".to_string(),
        sha256: None,
        preview: Some(text.to_string()),
        metadata: None,
    };
    let request = dto::PrepareUploadRequest {
        info: crate::localsend::local_info(state),
        files: HashMap::from([(id, file)]),
    };
    let response = client
        .post(format!("{base}/api/localsend/v2/prepare-upload"))
        .json(&request)
        // 对面要么马上收下，要么挂着等它的用户点「接受」。没有总超时的话，一条没人
        // 处理的请求会把这台设备的出站 lane 永久占住（后面每条消息都排在它后面）；
        // 超时就落「未送达」，对方那边的邀请也会随连接断开作废。
        .timeout(Duration::from_secs(90))
        .send()
        .await
        .map_err(|e| format!("联系不上「{}」：{e}", peer.alias))?;

    match response.status() {
        reqwest::StatusCode::NO_CONTENT => Ok(()),
        reqwest::StatusCode::OK => {
            // 第三方实现把文本当文件：按它给的 token 把正文上传过去。
            let accepted: dto::PrepareUploadResponse =
                crate::net::read_json_capped(response, PREPARE_BODY_MAX).await?;
            let (file_id, token) = accepted
                .files
                .into_iter()
                .next()
                .ok_or_else(|| "对方没有接受这条文本消息".to_string())?;
            let upload_client = new_upload_client(&peer.ip, &pin).map_err(failure_text)?;
            let uploaded = upload_client
                .post(format!("{base}/api/localsend/v2/upload"))
                .query(&[
                    ("sessionId", accepted.session_id.as_str()),
                    ("fileId", file_id.as_str()),
                    ("token", token.as_str()),
                ])
                .header(reqwest::header::CONTENT_LENGTH, size.to_string())
                .header(reqwest::header::CONTENT_TYPE, "text/plain; charset=utf-8")
                .body(text.to_string())
                .send()
                .await
                .map_err(|e| format!("上传文本消息失败：{e}"))?;
            if uploaded.status().is_success() {
                Ok(())
            } else {
                Err(format!(
                    "对方没有接受这条文本消息（HTTP {}）",
                    uploaded.status().as_u16()
                ))
            }
        }
        reqwest::StatusCode::UNAUTHORIZED => Err("对方需要 PIN（暂不支持）".to_string()),
        reqwest::StatusCode::FORBIDDEN => Err("对方拒绝了这条消息".to_string()),
        reqwest::StatusCode::CONFLICT => Err("对方正忙，请稍后再试".to_string()),
        other => Err(format!(
            "对方没能接收这条消息（HTTP {}）",
            other.as_u16()
        )),
    }
}

/// 把一张图片按 LocalSend 文件传输发给对端（**不建 PushJob、不显示文件卡**）。
///
/// 图片在本机会话里是一条 `localsend_messages` 图片消息，状态由调用方记
/// `Sending → Sent/Failed`；这里只负责线上的 `prepare-upload` + `/upload`。
pub async fn send_image(
    state: &Arc<AppState>,
    peer: &LocalSendPeer,
    name: &str,
    bytes: &[u8],
) -> Result<(), String> {
    if !crate::localsend::enabled(state) || !state.is_online() {
        return Err("LocalSend 兼容已关闭".to_string());
    }
    if bytes.is_empty() {
        return Err("图片内容为空".to_string());
    }
    let pin = pin_for(&peer.fingerprint);
    let client = new_client(&peer.ip, &pin).map_err(failure_text)?;
    let base = format!("https://{}", authority(&peer.ip, peer.port));
    let id = Uuid::new_v4().to_string();
    let size = bytes.len() as u64;
    let mime = file_type(name).to_string();
    let file = dto::FileDto {
        id: id.clone(),
        file_name: name.to_string(),
        size,
        file_type: mime.clone(),
        sha256: None,
        preview: None,
        metadata: None,
    };
    let request = dto::PrepareUploadRequest {
        info: crate::localsend::local_info(state),
        files: HashMap::from([(id.clone(), file)]),
    };
    let response = client
        .post(format!("{base}/api/localsend/v2/prepare-upload"))
        .json(&request)
        // 对面要么马上收下，要么挂着等它的用户点「接受」。没有总超时的话，一条没人
        // 处理的请求会把这台设备的出站 lane 永久占住（后面每条消息都排在它后面）；
        // 超时就落「未送达」，对方那边的邀请也会随连接断开作废。
        .timeout(Duration::from_secs(90))
        .send()
        .await
        .map_err(|e| format!("联系不上「{}」：{e}", peer.alias))?;

    match response.status() {
        reqwest::StatusCode::NO_CONTENT => Ok(()),
        reqwest::StatusCode::OK => {
            let accepted: dto::PrepareUploadResponse =
                crate::net::read_json_capped(response, PREPARE_BODY_MAX).await?;
            let token = accepted
                .files
                .get(&id)
                .or_else(|| accepted.files.values().next())
                .cloned()
                .ok_or_else(|| "对方没有接受这张图片".to_string())?;
            let upload_client = new_upload_client(&peer.ip, &pin).map_err(failure_text)?;
            let uploaded = upload_client
                .post(format!("{base}/api/localsend/v2/upload"))
                .query(&[
                    ("sessionId", accepted.session_id.as_str()),
                    ("fileId", id.as_str()),
                    ("token", token.as_str()),
                ])
                .header(reqwest::header::CONTENT_LENGTH, size.to_string())
                .header(reqwest::header::CONTENT_TYPE, mime)
                .body(bytes.to_vec())
                .send()
                .await
                .map_err(|e| format!("上传图片失败：{e}"))?;
            if uploaded.status().is_success() {
                Ok(())
            } else {
                Err(format!(
                    "对方没有接受这张图片（HTTP {}）",
                    uploaded.status().as_u16()
                ))
            }
        }
        reqwest::StatusCode::UNAUTHORIZED => Err("对方需要 PIN（暂不支持）".to_string()),
        reqwest::StatusCode::FORBIDDEN => Err("对方拒绝了这张图片".to_string()),
        reqwest::StatusCode::CONFLICT => Err("对方正忙，请稍后再试".to_string()),
        other => Err(format!("对方没能接收这张图片（HTTP {}）", other.as_u16())),
    }
}

fn failure_text(failure: Failure) -> String {
    match failure {
        Failure::Cancelled => "已取消".to_string(),
        Failure::Rejected(reason) | Failure::Failed(reason) => reason,
    }
}

/// 取消一条出站发送。幂等：令牌不在了也照样把界面那一行落成 `Cancelled`。
pub fn cancel(state: &AppState, invite_id: &str) {
    if let Some(token) = state.localsend.outbound_cancel(invite_id) {
        token.cancel();
    }
    let meta = outbound_meta().lock().remove(invite_id);
    if let Some(meta) = meta {
        // 会话已经建立时通知对端收手；还在等用户点的那一段没有 sessionId，只能断连。
        if let Some(session_id) = meta.session_id.clone() {
            let _ = crate::task::spawn("localsend cancel", async move {
                notify_cancel(meta, session_id).await;
            });
        }
    }
    crate::transfer::push_job::batch_set(
        state,
        invite_id,
        TargetState::Cancelled,
        None,
        Some("已取消".to_string()),
        true,
    );
}

async fn notify_cancel(meta: OutboundMeta, session_id: String) {
    let pin = pin_for(&meta.fingerprint);
    let Ok(client) = new_client(&meta.ip, &pin) else {
        return;
    };
    let url = format!(
        "https://{}/api/localsend/v2/cancel",
        authority(&meta.ip, meta.port)
    );
    let _ = client
        .post(url)
        .query(&[("sessionId", session_id.as_str())])
        .timeout(Duration::from_secs(5))
        .send()
        .await;
}

async fn run_send(
    state: &Arc<AppState>,
    peer: &LocalSendPeer,
    invite_id: &str,
    source: &PushSource,
) -> Result<(), Failure> {
    if source.files.is_empty() {
        return Err(Failure::Failed("这次推送里没有可发送的文件".to_string()));
    }

    let pin = pin_for(&peer.fingerprint);
    let prepare_client = new_client(&peer.ip, &pin)?;

    // 清单：file id 是对端眼里的不透明键；本机用一张表映射回真实路径（绝不把 id 当路径）。
    let info = crate::localsend::local_info(state);
    let mut files: HashMap<String, dto::FileDto> = HashMap::new();
    let mut plan: Vec<(String, UploadJob)> = Vec::new();
    let mut missing = 0usize;
    for entry in &source.files {
        let Some(path) = source.sources.get(&entry.path) else {
            missing += 1;
            continue;
        };
        let id = Uuid::new_v4().to_string();
        files.insert(
            id.clone(),
            dto::FileDto {
                id: id.clone(),
                file_name: entry.path.clone(),
                size: entry.size,
                file_type: file_type(&entry.path).to_string(),
                sha256: None,
                preview: None,
                metadata: None,
            },
        );
        plan.push((
            id,
            UploadJob {
                file_id: String::new(), // 下面按响应里的 key 回填
                token: String::new(),
                name: entry.path.clone(),
                path: path.clone(),
                size: entry.size,
            },
        ));
    }
    if missing > 0 {
        warn!(missing, "localsend: 清单里有条目没有本机路径，已跳过");
    }
    if plan.is_empty() {
        return Err(Failure::Failed("这次推送里没有可发送的文件".to_string()));
    }

    let base = format!("https://{}", authority(&peer.ip, peer.port));
    let request = dto::PrepareUploadRequest { info, files };
    let response = prepare_client
        .post(format!("{base}/api/localsend/v2/prepare-upload"))
        .json(&request)
        .send()
        .await
        .map_err(|e| Failure::Failed(format!("联系不上「{}」：{e}", peer.alias)))?;

    match response.status() {
        reqwest::StatusCode::OK => {}
        // 204 = 对端已收下（文本消息/无需上传）；没有 token 要处理。
        reqwest::StatusCode::NO_CONTENT => return Ok(()),
        reqwest::StatusCode::UNAUTHORIZED => {
            return Err(Failure::Failed("对方需要 PIN（暂不支持）".to_string()));
        }
        reqwest::StatusCode::FORBIDDEN => {
            return Err(Failure::Rejected("对方拒绝了这次发送".to_string()));
        }
        reqwest::StatusCode::CONFLICT => {
            return Err(Failure::Failed("对方正忙，请稍后再试".to_string()));
        }
        other => {
            return Err(Failure::Failed(format!(
                "对方没能接收这次发送（HTTP {}）",
                other.as_u16()
            )));
        }
    }

    let accepted: dto::PrepareUploadResponse =
        crate::net::read_json_capped(response, PREPARE_BODY_MAX).await?;
    let dto::PrepareUploadResponse {
        session_id,
        files: tokens,
    } = accepted;
    set_session_id(invite_id, &session_id);

    let mut by_id: HashMap<String, UploadJob> = plan.into_iter().collect();
    let mut jobs: Vec<UploadJob> = Vec::new();
    for (file_id, token) in tokens {
        let Some(mut job) = by_id.remove(&file_id) else {
            continue; // 对端接受了一个我们没报过的 id：忽略
        };
        job.file_id = file_id;
        job.token = token;
        jobs.push(job);
    }
    if jobs.is_empty() {
        return Ok(());
    }

    // 对端已经点头：把卡片从「邀请中」推到「发送中」（终态之前的第一条进度）。
    crate::transfer::push_job::batch_set(
        state.as_ref(),
        invite_id,
        TargetState::Sending,
        Some(0),
        None,
        false,
    );

    let upload_client = crate::net::dial_client_streaming_isolated(
        &peer.ip,
        &pin,
        CONNECT_TIMEOUT,
        UPLOAD_READ_TIMEOUT,
    )?;
    let received = Arc::new(AtomicU64::new(0));
    let last_emit: Arc<Mutex<Option<Instant>>> = Arc::new(Mutex::new(None));

    // 至多四路并发；某一路失败会 `?` 掉整条流，其余在途的 future 随流一起 drop（连接断开）。
    let mut uploads = futures_util::stream::iter(jobs.into_iter().map(|job| {
        let client = upload_client.clone();
        let base = base.clone();
        let session_id = session_id.clone();
        let state = state.clone();
        let invite_id = invite_id.to_string();
        let received = received.clone();
        let last_emit = last_emit.clone();
        async move {
            upload_file(UploadArgs {
                client,
                base,
                session_id,
                job,
                state,
                invite_id,
                received,
                last_emit,
            })
            .await
        }
    }))
    .buffer_unordered(MAX_PARALLEL_UPLOADS);

    while let Some(result) = uploads.next().await {
        result?;
    }
    Ok(())
}

/// 一个待上传文件（file id / token 由 `prepare-upload` 的回包决定）。
struct UploadJob {
    file_id: String,
    token: String,
    name: String,
    path: PathBuf,
    size: u64,
}

struct UploadArgs {
    client: reqwest::Client,
    /// `https://ip:port`
    base: String,
    session_id: String,
    job: UploadJob,
    state: Arc<AppState>,
    invite_id: String,
    received: Arc<AtomicU64>,
    last_emit: Arc<Mutex<Option<Instant>>>,
}

async fn upload_file(args: UploadArgs) -> Result<(), Failure> {
    let UploadArgs {
        client,
        base,
        session_id,
        job,
        state,
        invite_id,
        received,
        last_emit,
    } = args;

    let file = tokio::fs::File::open(&job.path)
        .await
        .map_err(|e| Failure::Failed(format!("读不到「{}」：{e}", job.name)))?;
    let reader = file.take(job.size);

    // 边读边计数：节流点在四路之间共享，所以整批最多 ~5 条/秒。
    let prog_state = state.clone();
    let prog_invite = invite_id.clone();
    let stream = ReaderStream::with_capacity(reader, crate::net::STREAM_CHUNK).map(move |item| {
        if let Ok(chunk) = &item {
            let len = chunk.len() as u64;
            let now = received.fetch_add(len, Ordering::Relaxed) + len;
            let due = {
                let mut last = last_emit.lock();
                let due = match *last {
                    Some(t) => t.elapsed() >= PROGRESS_INTERVAL,
                    None => true,
                };
                if due {
                    *last = Some(Instant::now());
                }
                due
            };
            if due {
                crate::transfer::push_job::batch_set(
                    prog_state.as_ref(),
                    &prog_invite,
                    TargetState::Sending,
                    Some(now),
                    None,
                    false,
                );
            }
        }
        item
    });

    let request = client
        .post(format!("{base}/api/localsend/v2/upload"))
        .query(&[
            ("sessionId", session_id.as_str()),
            ("fileId", job.file_id.as_str()),
            ("token", job.token.as_str()),
        ])
        .header(reqwest::header::CONTENT_LENGTH, job.size.to_string())
        .header(reqwest::header::CONTENT_TYPE, "application/octet-stream")
        .body(reqwest::Body::wrap_stream(stream));

    let response = request
        .send()
        .await
        .map_err(|e| Failure::Failed(format!("上传「{}」失败：{e}", job.name)))?;
    let status = response.status();
    match status {
        s if s.is_success() => Ok(()),
        reqwest::StatusCode::UNPROCESSABLE_ENTITY => Err(Failure::Failed(format!(
            "「{}」校验失败（源文件可能已变化）",
            job.name
        ))),
        other => Err(Failure::Failed(format!(
            "「{}」上传失败（HTTP {}）",
            job.name,
            other.as_u16()
        ))),
    }
}

/// 对端地址的 URL authority（IPv6 要加方括号）。
fn authority(ip: &str, port: u16) -> String {
    if ip.contains(':') && !ip.starts_with('[') {
        format!("[{ip}]:{port}")
    } else {
        format!("{ip}:{port}")
    }
}

/// 按目标钉死的指纹。
///
/// 只有 64 位 hex（证书 SHA-256）才是可钉的身份。第三方实现会把设备 UUID、甚至来源 IP
/// 填进 `fingerprint`（见 `super::clean_fingerprint`），拿它去钉 TLS 每次必被拒；
/// 这一档退回 TOFU —— 拒绝发送没有意义，对端是合法的 LocalSend。
fn pin_for(fingerprint: &str) -> DialPin {
    match super::clean_fingerprint(fingerprint) {
        Some(fp) => DialPin::Fingerprint(fp),
        None => DialPin::Tofu,
    }
}

/// 只给连接时限、**不给总时长**的客户端（`net` 里没有这条组合；`prepare-upload`
/// 要一直挂着等用户点接受，不能有总超时）。
///
/// 信任表用 [`net::Trust::isolated`] 而不是 `scoped`：LocalSend 的握手见到的证书不能写进
/// OrbDrop 的进程级观察值 —— 同一地址上可能同时住着一台 OrbDrop 设备（真机场景：对端
/// 既跑 OrbDrop 又开着第三方 LocalSend）。
fn new_client(host: &str, pin: &DialPin) -> Result<reqwest::Client, Failure> {
    crate::net::client_builder_with_trust(Trust::isolated(host, pin))
        .connect_timeout(CONNECT_TIMEOUT)
        .build()
        .map_err(|e| Failure::Failed(format!("建 HTTPS 客户端失败：{e}")))
}

/// `/upload` 那一跳的客户端：不设总时长（正文大小由对方清单决定），但读不动就收手。
///
/// `prepare-upload` 可能要挂着等对端用户点「接受」，所以不能整体套 `read_timeout`；
/// 上传这一步没有"等人"的语义，卡住只可能是链路死了。
fn new_upload_client(host: &str, pin: &DialPin) -> Result<reqwest::Client, Failure> {
    crate::net::client_builder_with_trust(Trust::isolated(host, pin))
        .connect_timeout(CONNECT_TIMEOUT)
        .read_timeout(UPLOAD_READ_TIMEOUT)
        .build()
        .map_err(|e| Failure::Failed(format!("建 HTTPS 客户端失败：{e}")))
}

/// 按扩展名给 MIME。LocalSend 的接收端只用它决定预览/图标，兜底是二进制。
pub(crate) fn file_type(name: &str) -> &'static str {
    let ext = std::path::Path::new(name)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match ext.as_str() {
        // 文本（协议约定 text/plain）
        "txt" | "log" | "md" | "markdown" | "csv" | "tsv" | "ini" | "cfg" | "conf" | "toml"
        | "yaml" | "yml" | "json" | "xml" | "html" | "htm" | "css" | "js" | "mjs" | "cjs"
        | "ts" | "tsx" | "jsx" | "rs" | "c" | "h" | "cc" | "cpp" | "hpp" | "py" | "java" | "kt"
        | "go" | "rb" | "php" | "sh" | "bash" | "zsh" | "ps1" | "bat" | "cmd" | "sql" | "srt"
        | "vtt" => "text/plain",
        // 图片
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "bmp" => "image/bmp",
        "svg" => "image/svg+xml",
        "ico" => "image/x-icon",
        "tif" | "tiff" => "image/tiff",
        "heic" => "image/heic",
        "heif" => "image/heif",
        "avif" => "image/avif",
        // 视频
        "mp4" | "m4v" => "video/mp4",
        "mkv" => "video/x-matroska",
        "mov" => "video/quicktime",
        "webm" => "video/webm",
        "avi" => "video/x-msvideo",
        "wmv" => "video/x-ms-wmv",
        "flv" => "video/x-flv",
        "mpg" | "mpeg" => "video/mpeg",
        "3gp" => "video/3gpp",
        // 明确点名的两类
        "pdf" => "application/pdf",
        "apk" => "application/vnd.android.package-archive",
        _ => "application/octet-stream",
    }
}
