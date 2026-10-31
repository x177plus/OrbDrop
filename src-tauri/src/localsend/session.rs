//! LocalSend 上传会话表（pending / active）。
//!
//! 由 `server.rs` 的 `prepare-upload` / `upload` / `cancel` 驱动；由 `lib.rs` 的
//! `accept_push` / `reject_push` 命令收尾。**只在内存里**：LocalSend 协议没有续传，
//! 进程重启后没有可恢复的会话。
//!
//! 安全边界：`sessionId` / `fileId` / `token` 都是不可信输入；token 绑定
//! `sessionId + fileId + 来源 IP + 客户端证书指纹`，比较用常数时间。
//!
//! 所有状态迁移都在 [`parking_lot::Mutex`] 里同步做完；这里**没有任何 await**，
//! 所以 `accept_pending` 可以从同步的 Tauri 命令路径调用。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::sync::oneshot;
use tokio_util::sync::CancellationToken;

use crate::localsend::dto::FileDto;
use crate::localsend::paths;
use crate::state::AppState;

/// 同一个 token 最多允许几次上传尝试（断连/校验失败后对端会重试同一条）。
pub(crate) const MAX_ATTEMPTS: u8 = 3;

/// 同一时刻允许挂着的**自动接收图片**会话数。
///
/// 它们不占单会话槽（见 `claim_slot`），但每条都揣着一份对端给的清单；没有上限时，
/// 一段里谁都能用 `prepare-upload` 不停堆会话、一条都不上传。
const MAX_AUTO_SESSIONS: usize = 8;

#[derive(Default)]
pub struct SessionStore {
    /// 键 = `invite_id`（界面里的那张邀请卡）。
    pub(crate) pending: Mutex<HashMap<String, Pending>>,
    /// 键 = `session_id`（LocalSend 协议里的会话）。
    pub(crate) active: Mutex<HashMap<String, Active>>,
}

/// `prepare-upload` 挂着等用户决定的那一条。
pub(crate) struct Pending {
    pub(crate) invite_id: String,
    pub(crate) session_id: String,
    pub(crate) sender_ip: String,
    pub(crate) fingerprint: Option<String>,
    pub(crate) sender_id: String,
    pub(crate) alias: String,
    pub(crate) files: HashMap<String, FileDto>,
    pub(crate) decision: Option<oneshot::Sender<Decision>>,
    pub(crate) cancel: CancellationToken,
    pub(crate) root: Option<PathBuf>,
}

pub(crate) enum Decision {
    Accept,
    Reject,
}

pub(crate) struct Active {
    pub(crate) invite_id: String,
    pub(crate) session_id: String,
    pub(crate) sender_ip: String,
    pub(crate) fingerprint: Option<String>,
    pub(crate) sender_id: String,
    pub(crate) alias: String,
    pub(crate) files: HashMap<String, ActiveFile>,
    pub(crate) cancel: CancellationToken,
    pub(crate) root: PathBuf,
    /// 这条会话是“整批图片自动接收”：上传完成后按图片消息落账，不发文件卡。
    pub(crate) auto_images: bool,
    /// 最后一次有动静（建立 / 开始上传 / 有字节 / 收尾）。空转的会话靠它被回收。
    pub(crate) last_activity: Instant,
}

pub(crate) struct ActiveFile {
    pub(crate) dto: FileDto,
    pub(crate) token: String,
    pub(crate) target: PathBuf,
    pub(crate) part: PathBuf,
    pub(crate) status: FileStatus,
    pub(crate) attempts: u8,
    pub(crate) received: u64,
    /// 自动接收图片时的会话消息 id（普通文件为空）。落盘目标是 blobs 的 `<id>.bin`。
    pub(crate) message_id: String,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum FileStatus {
    Pending,
    InProgress,
    Finished,
    Failed,
}

/// `upload` 通过校验后拿到的那张票：目标/临时路径与取消令牌的**快照**。
///
/// 拿到它之后 handler 不再持有会话锁（流式传输可能持续很久）；收尾时用
/// [`SessionStore::mark_finished`] / [`SessionStore::mark_failed`] 重新进锁。
#[derive(Debug)]
pub(crate) struct UploadTicket {
    pub(crate) invite_id: String,
    pub(crate) session_id: String,
    pub(crate) file_id: String,
    pub(crate) target: PathBuf,
    pub(crate) part: PathBuf,
    pub(crate) size: u64,
    pub(crate) sha256: Option<String>,
    pub(crate) cancel: CancellationToken,
    pub(crate) attempt: u8,
    /// 自动接收图片：上传完成后写入 blobs 并落一条图片消息，而不是文件卡。
    pub(crate) auto_image: bool,
    pub(crate) sender_id: String,
    pub(crate) alias: String,
    pub(crate) message_id: String,
    pub(crate) file_name: String,
}

/// `upload` 被拒的三种原因（分别映射 404 / 403 / 409）。
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum UploadDenied {
    /// 会话或文件不存在（已经收完/被取消）。
    NotFound,
    /// 来源 IP、证书指纹或 token 对不上。
    Forbidden,
    /// 这个 token 的尝试次数用完，或文件已经收完。
    Exhausted,
}

/// 一次会话的收尾快照（全部文件都有终态时才产生）。
pub(crate) struct SessionSummary {
    pub(crate) invite_id: String,
    pub(crate) root: PathBuf,
    pub(crate) total: u64,
    /// 没有失败文件。
    pub(crate) completed: bool,
    /// 失败文件的文件名（给 `PUSH_DONE` 的 reason 用）。
    pub(crate) failed: Vec<String>,
    /// 自动接收的图片会话：收尾时不要发文件卡事件。
    pub(crate) auto_images: bool,
}

impl SessionStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// 模式关闭 / 离线 / 退出：取消所有挂着和正在跑的。
    ///
    /// 返回**有界面卡**的那几条（pending + 非自动图片的 active）：调用方据此补一条
    /// `push://done / cancelled`。不补的话，接收卡会一直停在"接收中 x%"（或一张点了
    /// 「接收」就报"邀请不存在"的卡），直到应用重启。
    pub fn cancel_all(&self) -> Vec<String> {
        let mut cancelled = Vec::new();
        for (_, p) in self.pending.lock().drain() {
            p.cancel.cancel();
            if let Some(tx) = p.decision {
                let _ = tx.send(Decision::Reject);
            }
            cancelled.push(p.invite_id);
        }
        for (_, a) in self.active.lock().drain() {
            a.cancel.cancel();
            cleanup_parts(&a);
            if !a.auto_images {
                cancelled.push(a.invite_id);
            }
        }
        cancelled
    }

    pub fn has_pending(&self, invite_id: &str) -> bool {
        self.pending.lock().contains_key(invite_id)
    }

    /// 单会话槽：普通文件/手动文本同一时刻只处理一条 LocalSend 传输。
    ///
    /// `Err` = 已有 pending 或 active（`prepare-upload` 回 409）。
    /// **自动图片不走这里**（`start_auto_images` 直接建 active）：文本/图片不该被一条
    /// 正在传的文件挡住。
    pub(crate) fn claim_slot(&self, pending: Pending) -> Result<(), ()> {
        let mut pending_map = self.pending.lock();
        // 自动接收的图片**不占**这个槽：它没有界面卡，卡住时用户连取消的入口都没有。
        // 只按"普通文件/待决定的邀请"判忙（见 `start_auto_images` 的说明）。
        let busy = !pending_map.is_empty()
            || self
                .active
                .lock()
                .values()
                .any(|a| !a.auto_images);
        if busy {
            return Err(());
        }
        pending_map.insert(pending.invite_id.clone(), pending);
        Ok(())
    }

    /// 取走一条 pending（不发决定；`Drop` guard / 断连清理用）。
    pub(crate) fn remove_pending(&self, invite_id: &str) -> bool {
        let Some(p) = self.pending.lock().remove(invite_id) else {
            return false;
        };
        p.cancel.cancel();
        true
    }

    /// 自动接收一整批图片：直接建 active 会话，**不占单会话槽**。
    ///
    /// 图片在上传完成时落成会话消息，不需要用户在界面点任何东西；允许它和普通文件
    /// 传输并行（两者的 `sessionId` 各走各的，`active` 本来就是一张表）。文本路径更轻，
    /// 在 `server::prepare_upload` 里直接落账、连会话都不建。
    pub(crate) fn start_auto_images(
        &self,
        state: &Arc<AppState>,
        invite_id: String,
        session_id: String,
        sender_ip: String,
        fingerprint: Option<String>,
        sender_id: String,
        alias: String,
        files: HashMap<String, FileDto>,
    ) -> Result<HashMap<String, String>, String> {
        let root = state.chat.blobs_root().to_path_buf();
        let mut active_files = HashMap::new();
        let mut tokens = HashMap::new();
        {
            // 先量一下手上有几条自动图片会话：这个入口不需要任何人点同意，必须自己封顶。
            let active = self.active.lock();
            if active.values().filter(|a| a.auto_images).count() >= MAX_AUTO_SESSIONS {
                return Err("太多待接收的图片会话".to_string());
            }
        }
        for (id, dto) in files.iter() {
            let message_id = uuid::Uuid::new_v4().to_string();
            let target = crate::blobs::path_in(&root, &message_id, &dto.file_name)
                .ok_or_else(|| format!("图片文件名不可用: {}", dto.file_name))?;
            let part = part_path(&target);
            let _ = std::fs::remove_file(&part);
            let token = new_token();
            tokens.insert(id.clone(), token.clone());
            active_files.insert(
                id.clone(),
                ActiveFile {
                    dto: dto.clone(),
                    token,
                    target,
                    part,
                    status: FileStatus::Pending,
                    attempts: 0,
                    received: 0,
                    message_id,
                },
            );
        }
        if active_files.is_empty() {
            return Err("没有可接收的图片".to_string());
        }
        let active = Active {
            invite_id,
            session_id: session_id.clone(),
            sender_ip,
            fingerprint,
            sender_id,
            alias,
            files: active_files,
            cancel: CancellationToken::new(),
            root,
            auto_images: true,
            last_activity: Instant::now(),
        };
        self.active.lock().insert(session_id, active);
        Ok(tokens)
    }

    /// 界面点「接收」。由 `lib.rs::accept_push` 调。
    ///
    /// **同步**：只做状态迁移与目录创建，不 await 任何东西。
    pub fn accept_pending(
        &self,
        state: &Arc<AppState>,
        invite_id: &str,
        root: Option<PathBuf>,
    ) -> Result<(), String> {
        let Some(mut p) = self.pending.lock().remove(invite_id) else {
            return Err("LocalSend 邀请不存在或已处理".to_string());
        };

        let (download_dir, device_subdir) = {
            let cfg = state.config.lock();
            (cfg.download_dir.clone(), cfg.device_subdir)
        };
        if let Some(r) = &root {
            if !r.is_absolute() {
                return Err("落盘目录必须是绝对路径".to_string());
            }
        }
        let base = root
            .or(p.root.clone())
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(download_dir);
        let dir = paths::landing_root(&base, &p.alias, device_subdir);

        let mut files = HashMap::new();
        let mut reason: Option<String> = None;
        // 同一份清单里可能有两个同名 `fileName`（LocalSend 多选时是裸文件名）：
        // `unique_path` 只看盘上有没有，这里再记一份"本批已分出去的落点"。
        let mut taken: std::collections::HashSet<PathBuf> = std::collections::HashSet::new();
        for (id, dto) in p.files.iter() {
            // `prepare-upload` 已经过了一遍；这里再走一次是因为落点决定**只能**发生在
            // `safe_relative` 之后（防止将来有人从别处塞 Pending 进来）。
            let Some(rel) = paths::safe_relative(&dto.file_name) else {
                reason = Some(format!("文件名不安全: {}", dto.file_name));
                continue;
            };
            let target = paths::unique_path(&dir, &rel, &taken);
            taken.insert(target.clone());
            if let Some(parent) = target.parent() {
                if let Err(e) = std::fs::create_dir_all(parent) {
                    reason = Some(format!("创建目录失败: {e}"));
                    continue;
                }
            }
            let part = part_path(&target);
            // 上一次进程留下的同名残片不该被当成"已经写了一部分"。
            let _ = std::fs::remove_file(&part);
            files.insert(
                id.clone(),
                ActiveFile {
                    dto: dto.clone(),
                    token: new_token(),
                    target,
                    part,
                    status: FileStatus::Pending,
                    attempts: 0,
                    received: 0,
                    message_id: String::new(),
                },
            );
        }

        if files.is_empty() {
            // 一个都落不了：告诉等着的 handler 这是拒绝，别挂在那儿。
            if let Some(tx) = p.decision.take() {
                let _ = tx.send(Decision::Reject);
            }
            return Err(reason.unwrap_or_else(|| "没有可接收的文件".to_string()));
        }

        let session_id = p.session_id.clone();
        let active = Active {
            invite_id: p.invite_id.clone(),
            session_id: session_id.clone(),
            sender_ip: p.sender_ip.clone(),
            fingerprint: p.fingerprint.clone(),
            sender_id: p.sender_id.clone(),
            alias: p.alias.clone(),
            files,
            cancel: CancellationToken::new(),
            root: dir,
            auto_images: false,
            last_activity: Instant::now(),
        };
        self.active.lock().insert(session_id, active);
        if let Some(tx) = p.decision.take() {
            let _ = tx.send(Decision::Accept);
        }
        if let Some(reason) = reason {
            // 部分文件被排除：协议上合法（只有回包里的 files 才会被上传），但要留痕。
            tracing::warn!(invite = %p.invite_id, %reason, "LocalSend 接收：部分文件被排除");
        }
        Ok(())
    }

    /// 界面点「拒绝」。由 `lib.rs::reject_push` 调。
    pub fn reject_pending(&self, invite_id: &str) -> bool {
        let mut pending = self.pending.lock();
        let Some(p) = pending.remove(invite_id) else {
            return false;
        };
        p.cancel.cancel();
        if let Some(tx) = p.decision {
            let _ = tx.send(Decision::Reject);
        }
        true
    }

    /// 接受的 token 表（回给发送方的 `PrepareUploadResponse.files`）。
    pub(crate) fn tokens_for(&self, session_id: &str) -> Option<HashMap<String, String>> {
        let active = self.active.lock();
        let a = active.get(session_id)?;
        Some(
            a.files
                .iter()
                .map(|(id, f)| (id.clone(), f.token.clone()))
                .collect(),
        )
    }

    /// `upload` 的认证 + 状态迁移（Pending → InProgress，`attempts += 1`）。
    pub(crate) fn begin_upload(
        &self,
        session_id: &str,
        file_id: &str,
        token: &str,
        sender_ip: &str,
        fingerprint: Option<&str>,
    ) -> Result<UploadTicket, UploadDenied> {
        let mut active = self.active.lock();
        let Some(a) = active.get_mut(session_id) else {
            return Err(UploadDenied::NotFound);
        };
        // 来源必须是**同一个对端**：IP 与客户端证书指纹都要对得上。
        if a.sender_ip != sender_ip || a.fingerprint.as_deref() != fingerprint {
            return Err(UploadDenied::Forbidden);
        }
        let invite_id = a.invite_id.clone();
        let session_id = a.session_id.clone();
        let cancel = a.cancel.clone();
        let auto_image = a.auto_images;
        let sender_id = a.sender_id.clone();
        let alias = a.alias.clone();
        a.last_activity = Instant::now();
        let Some(f) = a.files.get_mut(file_id) else {
            return Err(UploadDenied::NotFound);
        };
        if !ct_eq(&f.token, token) {
            return Err(UploadDenied::Forbidden);
        }
        if f.status == FileStatus::Finished || f.attempts >= MAX_ATTEMPTS {
            return Err(UploadDenied::Exhausted);
        }
        f.attempts += 1;
        f.status = FileStatus::InProgress;
        f.received = 0;
        let _ = std::fs::remove_file(&f.part);
        Ok(UploadTicket {
            invite_id,
            session_id,
            file_id: file_id.to_string(),
            target: f.target.clone(),
            part: f.part.clone(),
            size: f.dto.size,
            sha256: f.dto.sha256.clone(),
            cancel,
            attempt: f.attempts,
            auto_image,
            sender_id,
            alias,
            message_id: f.message_id.clone(),
            file_name: f.dto.file_name.clone(),
        })
    }

    /// 流式写盘期间更新已收字节（只影响进度事件，不参与安全判断）。
    pub(crate) fn set_received(&self, session_id: &str, file_id: &str, received: u64) {
        let mut active = self.active.lock();
        if let Some(a) = active.get_mut(session_id) {
            if let Some(f) = a.files.get_mut(file_id) {
                if f.status == FileStatus::InProgress {
                    f.received = received;
                    a.last_activity = Instant::now();
                }
            }
        }
    }

    /// 会话进度 `(已收, 总量)`。
    pub(crate) fn progress(&self, session_id: &str) -> Option<(u64, u64)> {
        let active = self.active.lock();
        let a = active.get(session_id)?;
        let total = a.files.values().map(|f| f.dto.size).sum();
        let received = a
            .files
            .values()
            .map(|f| match f.status {
                FileStatus::Finished => f.dto.size,
                _ => f.received,
            })
            .sum();
        Some((received, total))
    }

    /// 文件已经 `rename` 到最终路径：记终态，必要时把整条会话摘掉。
    pub(crate) fn mark_finished(
        &self,
        session_id: &str,
        file_id: &str,
    ) -> Option<SessionSummary> {
        let mut active = self.active.lock();
        if let Some(a) = active.get_mut(session_id) {
            a.last_activity = Instant::now();
            if let Some(f) = a.files.get_mut(file_id) {
                f.status = FileStatus::Finished;
                f.received = f.dto.size;
            }
        }
        summarize(&mut active, session_id)
    }

    /// 本次尝试失败：删残片；还有重试次数就退回 [`FileStatus::Pending`]。
    pub(crate) fn mark_failed(
        &self,
        session_id: &str,
        file_id: &str,
        attempts_left: bool,
    ) -> Option<SessionSummary> {
        let mut active = self.active.lock();
        if let Some(a) = active.get_mut(session_id) {
            a.last_activity = Instant::now();
            if let Some(f) = a.files.get_mut(file_id) {
                let _ = std::fs::remove_file(&f.part);
                f.received = 0;
                f.status = if attempts_left {
                    FileStatus::Pending
                } else {
                    FileStatus::Failed
                };
            }
        }
        summarize(&mut active, session_id)
    }

    /// 回收空转的会话（**自动图片**和普通文件都算）。
    ///
    /// 协议没有续传，一条 `Active` 的寿命本来就该跟着这次上传走：对端在 `prepare-upload`
    /// 之后一声不吭（或重试用完就消失）时，没人会再来碰它。留着它的代价不只是内存 ——
    /// 普通文件会话会一直占着单会话槽（`claim_slot`），此后**所有**对端的文件传输都回 409，
    /// 而界面上没有任何入口能清掉它。返回回收条数（调用方按需记一条日志）。
    pub(crate) fn reap_idle(&self, idle: Duration) -> usize {
        let mut active = self.active.lock();
        let now = Instant::now();
        let victims: Vec<String> = active
            .iter()
            .filter(|(_, a)| now.duration_since(a.last_activity) >= idle)
            .map(|(id, _)| id.clone())
            .collect();
        for id in &victims {
            if let Some(a) = active.remove(id) {
                a.cancel.cancel();
                cleanup_parts(&a);
            }
        }
        victims.len()
    }

    /// 发送方 `POST /cancel`：整条 active 会话作废。
    pub(crate) fn cancel_active(&self, session_id: &str) -> bool {
        let mut active = self.active.lock();
        let Some(a) = active.remove(session_id) else {
            return false;
        };
        a.cancel.cancel();
        cleanup_parts(&a);
        true
    }

    /// 界面按「取消」时只知道 `inviteId`：在 active 里按它找到 session 再收。
    pub fn cancel_by_invite(&self, invite_id: &str) -> bool {
        let mut active = self.active.lock();
        let Some(session_id) = active
            .iter()
            .find(|(_, a)| a.invite_id == invite_id)
            .map(|(id, _)| id.clone())
        else {
            return false;
        };
        let Some(a) = active.remove(&session_id) else {
            return false;
        };
        a.cancel.cancel();
        cleanup_parts(&a);
        true
    }

    /// 发送方 `POST /cancel`（还没决定的那条）：来源匹配才取消。
    pub(crate) fn cancel_pending_from(
        &self,
        session_id: Option<&str>,
        sender_ip: &str,
        fingerprint: Option<&str>,
    ) -> bool {
        let mut pending = self.pending.lock();
        let victim = pending
            .iter()
            .find(|(_, p)| {
                session_id.map_or(true, |s| p.session_id == s)
                    && p.sender_ip == sender_ip
                    // 证书指纹只在对端**有**证书时强制；明文回环两边都是 None。
                    && (p.fingerprint.is_none() || p.fingerprint.as_deref() == fingerprint)
            })
            .map(|(k, _)| k.clone());
        let Some(key) = victim else {
            return false;
        };
        let Some(p) = pending.remove(&key) else {
            return false;
        };
        p.cancel.cancel();
        if let Some(tx) = p.decision {
            let _ = tx.send(Decision::Reject);
        }
        true
    }
}

/// 全部文件都有终态时把会话摘掉，并给出收尾快照。
fn summarize(active: &mut HashMap<String, Active>, session_id: &str) -> Option<SessionSummary> {
    let done = active.get(session_id).is_some_and(|a| {
        a.files
            .values()
            .all(|f| matches!(f.status, FileStatus::Finished | FileStatus::Failed))
    });
    if !done {
        return None;
    }
    let a = active.remove(session_id)?;
    let total: u64 = a.files.values().map(|f| f.dto.size).sum();
    let failed: Vec<String> = a
        .files
        .values()
        .filter(|f| f.status == FileStatus::Failed)
        .map(|f| f.dto.file_name.clone())
        .collect();
    Some(SessionSummary {
        invite_id: a.invite_id,
        root: a.root,
        total,
        completed: failed.is_empty(),
        failed,
        auto_images: a.auto_images,
    })
}

/// 最终路径旁边的临时文件：`<name>.part`（与最终路径同一目录，rename 不跨卷）。
fn part_path(target: &std::path::Path) -> PathBuf {
    let mut os = target.as_os_str().to_os_string();
    os.push(".part");
    PathBuf::from(os)
}

fn new_token() -> String {
    // 32 字节随机（两枚 v4 UUID），URL-safe 且不需要额外依赖。
    let a = uuid::Uuid::new_v4().simple().to_string();
    let b = uuid::Uuid::new_v4().simple().to_string();
    format!("{a}{b}")
}

/// 常数时间字符串比较（长度不同时立刻返回，长度本身不是秘密）。
fn ct_eq(a: &str, b: &str) -> bool {
    let a = a.as_bytes();
    let b = b.as_bytes();
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn cleanup_parts(active: &Active) {
    for f in active.files.values() {
        let _ = std::fs::remove_file(&f.part);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(id: &str, name: &str, size: u64) -> ActiveFile {
        let target = PathBuf::from("C:/dl").join(name);
        ActiveFile {
            dto: FileDto {
                id: id.to_string(),
                file_name: name.to_string(),
                size,
                file_type: "application/octet-stream".to_string(),
                sha256: None,
                preview: None,
                metadata: None,
            },
            token: "tok".to_string(),
            part: part_path(&target),
            target,
            status: FileStatus::Pending,
            attempts: 0,
            received: 0,
            message_id: String::new(),
        }
    }

    fn store_with_session() -> SessionStore {
        let s = SessionStore::new();
        s.active.lock().insert(
            "sess".to_string(),
            Active {
                invite_id: "inv".to_string(),
                session_id: "sess".to_string(),
                sender_ip: "10.0.0.2".to_string(),
                fingerprint: Some("abc".to_string()),
                sender_id: "ls-abc".to_string(),
                alias: "Test".to_string(),
                files: HashMap::from([("f1".to_string(), file("f1", "a.bin", 10))]),
                cancel: CancellationToken::new(),
                root: PathBuf::from("C:/dl"),
                auto_images: false,
                last_activity: Instant::now(),
            },
        );
        s
    }

    fn active_session(id: &str, auto_images: bool) -> Active {
        Active {
            invite_id: id.to_string(),
            session_id: id.to_string(),
            sender_ip: "10.0.0.2".to_string(),
            fingerprint: Some("abc".to_string()),
            sender_id: "ls-abc".to_string(),
            alias: "Test".to_string(),
            files: HashMap::from([("f1".to_string(), file("f1", "a.bin", 10))]),
            cancel: CancellationToken::new(),
            root: PathBuf::from("C:/dl"),
            auto_images,
            last_activity: Instant::now(),
        }
    }

    #[test]
    fn upload_requires_matching_token_ip_and_cert() {
        let s = store_with_session();
        // token 不对
        assert_eq!(
            s.begin_upload("sess", "f1", "bad", "10.0.0.2", Some("abc")).unwrap_err(),
            UploadDenied::Forbidden
        );
        // IP 不对
        assert_eq!(
            s.begin_upload("sess", "f1", "tok", "10.0.0.3", Some("abc")).unwrap_err(),
            UploadDenied::Forbidden
        );
        // 证书指纹不对（对端用另一把私钥拨进来）
        assert_eq!(
            s.begin_upload("sess", "f1", "tok", "10.0.0.2", Some("def")).unwrap_err(),
            UploadDenied::Forbidden
        );
        // 会话不存在
        assert_eq!(
            s.begin_upload("nope", "f1", "tok", "10.0.0.2", Some("abc")).unwrap_err(),
            UploadDenied::NotFound
        );
        // 全对
        assert_eq!(s.begin_upload("sess", "f1", "tok", "10.0.0.2", Some("abc")).unwrap().attempt, 1);
    }

    #[test]
    fn attempts_are_capped_and_success_is_terminal() {
        let s = store_with_session();
        for n in 1..=MAX_ATTEMPTS {
            let t = s.begin_upload("sess", "f1", "tok", "10.0.0.2", Some("abc")).unwrap();
            assert_eq!(t.attempt, n);
            if n < MAX_ATTEMPTS {
                // 失败且还有次数 → 退回 Pending 可重试
                assert!(s.mark_failed("sess", "f1", true).is_none());
            } else {
                assert!(s.mark_failed("sess", "f1", false).is_some());
            }
        }
        assert_eq!(
            s.begin_upload("sess", "f1", "tok", "10.0.0.2", Some("abc")).unwrap_err(),
            UploadDenied::NotFound
        );
    }

    #[test]
    fn finishing_the_last_file_removes_the_session() {
        let s = store_with_session();
        s.begin_upload("sess", "f1", "tok", "10.0.0.2", Some("abc")).unwrap();
        assert_eq!(s.progress("sess"), Some((0, 10)));
        s.set_received("sess", "f1", 4);
        assert_eq!(s.progress("sess"), Some((4, 10)));
        let summary = s.mark_finished("sess", "f1").expect("会话应已收尾");
        assert!(summary.completed);
        assert_eq!(summary.total, 10);
        assert_eq!(summary.invite_id, "inv");
        assert!(s.progress("sess").is_none());
    }

    #[test]
    fn token_compare_is_exact() {
        assert!(ct_eq("abc", "abc"));
        assert!(!ct_eq("abc", "abd"));
        assert!(!ct_eq("abc", "ab"));
    }

    fn pending(id: &str) -> Pending {
        Pending {
            invite_id: id.to_string(),
            session_id: format!("{id}-sess"),
            sender_ip: "10.0.0.9".to_string(),
            fingerprint: None,
            sender_id: "ls-x".to_string(),
            alias: "Other".to_string(),
            files: HashMap::new(),
            decision: None,
            cancel: CancellationToken::new(),
            root: None,
        }
    }

    /// 自动接收的图片会话没有界面卡；它挂住时必须**不占**单会话槽，否则普通文件接收
    /// 会永久 409，而用户没有任何入口能清掉它。
    #[test]
    fn an_auto_image_session_does_not_claim_the_slot() {
        let s = store_with_session();
        {
            let mut active = s.active.lock();
            let mut a = active.remove("sess").expect("会话存在");
            a.auto_images = true;
            active.insert("sess".to_string(), a);
        }
        assert!(s.claim_slot(pending("inv2")).is_ok(), "自动图片不该挡普通文件");
    }

    /// 普通文件会话（含还在等用户决定的那条）仍然只允许一条。
    #[test]
    fn a_normal_active_session_or_pending_blocks_the_slot() {
        let s = store_with_session();
        assert!(s.claim_slot(pending("inv2")).is_err());
        s.active.lock().clear();
        assert!(s.claim_slot(pending("inv2")).is_ok());
        assert!(s.claim_slot(pending("inv3")).is_err(), "pending 也占槽");
    }

    /// 空转（对端 `prepare-upload` 之后再也不上传）的会话会被回收；有动静的不动。
    #[test]
    fn idle_sessions_are_reaped_and_fresh_ones_are_kept() {
        let s = store_with_session();
        {
            let mut active = s.active.lock();
            active.get_mut("sess").unwrap().last_activity = Instant::now() - Duration::from_secs(600);
        }
        assert_eq!(s.reap_idle(Duration::from_secs(120)), 1);
        assert!(s.progress("sess").is_none());
        assert_eq!(s.reap_idle(Duration::from_secs(120)), 0);
    }

    /// 关开关 / 切离线时的收尾：把**有界面卡**的那几条交回去（调用方据此发
    /// `push://done / cancelled`）；自动图片没有卡，不该混进去。
    #[test]
    fn cancel_all_reports_only_the_sessions_with_a_card() {
        let s = SessionStore::new();
        s.pending
            .lock()
            .insert("inv2".to_string(), pending("inv2"));
        s.active.lock().insert(
            "sess".to_string(),
            active_session("sess", false),
        );
        s.active
            .lock()
            .insert("auto".to_string(), active_session("auto", true));

        let mut ids = s.cancel_all();
        ids.sort();
        assert_eq!(ids, vec!["inv2".to_string(), "sess".to_string()]);
        assert!(s.pending.lock().is_empty());
        assert!(s.active.lock().is_empty());
    }
}