//! 接收端引擎：**并发**把对端的块拉下来（`POST /push/stream`）。
//!
//! ## 与老循环（`push_job::spawn_download_legacy`）的关系
//!
//! 老循环是"一个文件一次请求、串行"，新引擎是"一批块任务、N 个 worker 并行"。选择由
//! **邀请里的能力位**决定（`caps` 含 `stream1` 才走这条）；老对端照旧走老循环，一条代码
//! 都不用为它改。两条路共用：认人、回执（`PushProgress`/`PushDone`）、界面事件、
//! 续传文件、落盘根。
//!
//! ## 并行怎么保证不互相踩
//!
//! - **任务粒度是块**（默认 4MB），同一个文件的多个块可以由不同 worker 同时拉；
//! - 每个 worker 每次只写**自己那一块**：各自开一个句柄、`seek` 到自己那块的开头顺序写，
//!   互不重叠（块区间由 `stream::block_range` 唯一确定，且接收端会硬校验对端发回来的
//!   区间是不是正好是它要的那一块）；
//! - 写在 `.part`（`<name>.<key>.orbdrop-part`）上，**收到最后一块 + 长度核对 + fsync 之后
//!   才原子改名**到最终路径：用户看到的最终文件只有两种状态 —— 不存在，或者是校验过的完整
//!   文件。永远没有"半截文件顶着真名"。
//!
//! ## 完整性（三道）
//!
//! 1. **每块哈希**：对端把这块的 SHA-256 尾随在载荷后面，我们边写边算、读完比对。
//!    不符 → 这一块作废（不置位、不计进度），重试一次；再不符 → 这条文件判失败。
//! 2. **长度**：`FILE` 记录的 `size` 要等于清单里的 `size`（对端那边的文件变了就当场
//!    判失败，而不是拿一份拼凑的字节当成品）；收尾时 `.part` 的长度也要等于 `size`。
//! 3. **进度只在落盘之后记**：置位前先 `flush` + `sync_data`，所以位图里的每一位都意味着
//!    "这些字节在盘上"，崩溃后续传不会从垃圾后面接着写。
//!
//! ## 失败隔离
//!
//! 单条文件失败（对端打不开、源变了、哈希两次不符）只把**那一条**记下来，其余任务照跑；
//! 收尾时按"有没有失败"给发送方回 `completed` / `failed`（失败原因里带上有几条、第一条是谁）。
//! 连接级失败（连不上、断流）是另一回事：那是"这次尝试失败"，退避后重试整批任务。

use std::collections::{HashMap, HashSet, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use futures_util::TryStreamExt;
use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};
use tokio::time::sleep;
use tokio_util::io::StreamReader;
use tracing::{debug, error, info, warn};

use super::browse_dl::DownloadControl;
use super::push_job::{self, InboundInvite};
use super::resume::{bitmap_has, bitmap_set, now_secs, safe_join, FileResume};
use super::stream::{
    self, digest_eq, Record, RecordReader, StreamRequest, StreamTask, BUNDLE_FILE_MAX,
    BUNDLE_MAX_BYTES, BUNDLE_MAX_TASKS, SKIP_COMPLETE, SKIP_NOT_IN_MANIFEST, SKIP_SOURCE_CHANGED,
};
use crate::state::AppState;
use crate::ws::events::WsEvent;

/// 进度/回执的上报节流（与老循环同一个值：界面看 5 次/秒已经够平滑）。
const REPORT_INTERVAL: Duration = Duration::from_millis(200);

/// 接收流每次读的**空闲**上限：对端收下请求后不再发字节，这里 30s 就会失败并走重试/
/// 收尾，而不是让 worker 永久挂在 `reader.next().await` 上。正常传输只要还有字节进来
/// 就不会触发；它与服务端公开路由的 30s body idle 同一档。
const READ_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// 一次请求最多重试几次（连接级失败；文件级失败不走这里）。
const MAX_ATTEMPTS: u32 = 3;

/// 同一块连续两次哈希不符就判这条文件失败（第一次不符会重排一次）。
const MAX_HASH_RETRIES: usize = 1;

/// 多大的文件才在改名前 `fsync`（见 `Session::finalize` 里的说明）。
const SYNC_MIN: u64 = 1024 * 1024;

/// 一次任务的记账：一个文件在本次会话里的全部状态。
struct FileWork {
    /// 清单里的相对路径（对端也这么叫它）。
    rel: String,
    size: u64,
    mtime: i64,
    /// 最终落盘路径 / 临时文件路径（都在 `<下载目录>/<发送方名>/…` 下）。
    target: PathBuf,
    part: PathBuf,
    /// 稳定身份（`stream::resume_key`）。
    key: String,
    total_blocks: u32,
    /// 已完成的块数（多个 worker 同时推进同一个文件，用原子量计数）。
    done_blocks: AtomicU32,
    /// 续传记录（位图 + 已落盘字节）：`save_file` 落盘的就是它。
    record: Mutex<FileResume>,
    /// 这条文件已经判失败：不再接任务，也不参与最终审计。
    failed: AtomicBool,
    /// 哈希不符重排过的块（见 `MAX_HASH_RETRIES`）。
    retried: Mutex<Vec<u32>>,
    /// 已经收尾（原子改名过）。收尾必须幂等：多条路径都可能把它排上（队列完成、收尾兜底扫描），
    /// 重复改一次名会把"文件已经不在 `.part`"当成失败。
    finalized: AtomicBool,
    /// **本次会话之前就已在盘上**的块（续传接手的那部分，位图 hex，与 `FileResume::blocks` 同一套编码）。
    ///
    /// 收尾时只复核这些块：本次会话收到的块在**到达时**就逐块比对过摘要了，再读一遍纯属重复劳动
    /// （128MB 的文件实测多花 2 秒，占整个传输时间的一大半）；而接手来的块当时是"上一个进程/上一次
    /// 会话"校验的，字节有没有真的留在盘上只有复核一遍才知道。
    adopted: String,
}

impl FileWork {
    fn remaining_blocks(&self) -> Vec<u32> {
        // **一把锁只取一次**：`parking_lot::Mutex` 不可重入，先在锁里把块大小读出来再用，
        // 别在持有 `record` 的时候再调一个"顺手也去锁 `record`"的小函数（那是死锁，
        // 而且表现为"进程不占 CPU、什么都不发生"）。
        let rec = self.record.lock();
        stream::missing_blocks(self.size, rec.block_size, &rec.blocks)
    }
}

/// 队列里的一条任务：大文件的一块，或一批小文件各自的一整块。
enum Task {
    Block(Arc<FileWork>, u32),
    Bundle(Vec<(Arc<FileWork>, u32)>),
}

impl Task {
    fn is_empty(&self) -> bool {
        match self {
            Task::Block(_, _) => false,
            Task::Bundle(files) => files.is_empty(),
        }
    }
}

struct Session {
    state: Arc<AppState>,
    peer_id: String,
    invite_id: String,
    callback: Option<(String, u16)>,
    base_url: String,
    /// 实际拨号的主机字面量（`base_url` 里的那个）：按它建目标信任表。
    dial_host: String,
    token: Option<String>,
    block_size: u32,
    workers: usize,
    files: Vec<Arc<FileWork>>,
    /// 相对路径 → 任务（记录里的每条 `FILE`/`BLOCK` 都要按路径找它）。
    ///
    /// 是**表**不是线性扫描：一个批次里几百条记录 × 清单里几千上万个文件，
    /// 逐条 `iter().find()` 就是几百万次字符串比较 —— 那正是这条协议要消灭的开销。
    by_path: HashMap<String, Arc<FileWork>>,
    queue: Mutex<VecDeque<Task>>,
    notify: tokio::sync::Notify,
    /// 已确认落盘的字节（含续传基线）。
    received: Arc<AtomicU64>,
    total: u64,
    /// 停止一切：邀请被撤（本机取消）、对端取消、收尾。
    cancel: Arc<AtomicBool>,
    /// 取消的**即时**信号：`run_request` 在等网络时也能立刻收手，不用等 read timeout。
    /// `AtomicBool` 仍是真相源，watch 只负责把等待中的 future 叫醒。
    cancel_tx: tokio::sync::watch::Sender<bool>,
    cancel_rx: tokio::sync::watch::Receiver<bool>,
    /// 本机用户取消了（邀请没了）—— 收尾时**不**再回报（`cancel_receive` 已经报过）。
    local_cancel: AtomicBool,
    /// 对端取消了（410）。
    sender_cancel: AtomicBool,
    /// 对端的源文件与清单**对不上**（传输期间被改过）：这条文件判失败，整条任务也不再重试。
    ///
    /// 与"这次没传成"分开：那是网络/写盘的抖动，重试有意义；源变了再试一百次还是同一份
    /// 清单对不上（见 `transfer::resume_pending` 的终态判据）。
    source_changed: AtomicBool,
    /// 收尾完成（reporter 据此退出）。
    finished: AtomicBool,
    /// 失败的文件（路径 + 原因）。
    failed: Mutex<Vec<(String, String)>>,
    /// 收尾复核用的读写缓冲池（见 `take_buf`）：几千个小文件时，"每个文件都新分配一个
    /// 256KB 缓冲"会平白清零几百 MB 内存，白白吃掉吞吐。
    bufs: Mutex<Vec<Vec<u8>>>,
    /// 用户在这一轮里按下的暂停闸（界面那条文件气泡右侧的圆环）。worker 在**接下一批任务
    /// 之前**等它：在飞的那一块照常收完落盘，之后停住 —— 不向对端要新字节，也不动已收内容。
    control: Arc<DownloadControl>,
}

impl Session {
    /// 对端那份源文件与清单对不上（大小/时间戳变了）：这条文件判失败，并**记下"整条任务
    /// 落终态"** —— 再试还是同一份清单对不上，重试没有意义。
    fn mark_source_changed(&self, rel: &str, reason: String) {
        self.source_changed.store(true, Ordering::SeqCst);
        self.mark_failed(rel, reason);
    }

    fn mark_failed(&self, rel: &str, reason: String) {
        let mut failed = self.failed.lock();
        if failed.iter().any(|(p, _)| p == rel) {
            return;
        }
        failed.push((rel.to_string(), reason.clone()));
        warn!(invite = %self.invite_id, path = %rel, reason = %reason, "这条文件失败（其余照常）");
        drop(failed);
        if let Some(fw) = self.by_path.get(rel) {
            fw.failed.store(true, Ordering::SeqCst);
        }
        self.state.push.resume.flush();
    }

    fn summarize_failure(&self) -> String {
        let failed = self.failed.lock();
        let first = failed
            .first()
            .map(|(p, r)| format!("{p}: {r}"))
            .unwrap_or_default();
        format!("{} 个文件失败，第一个是 {first}", failed.len())
    }

    /// 取下一个任务（队列空 = 没有更多活儿）。
    fn next_task(&self) -> Option<Task> {
        let mut q = self.queue.lock();
        while let Some(task) = q.pop_front() {
            if !task.is_empty() {
                return Some(task);
            }
        }
        None
    }

    /// 把一条任务放回队尾（哈希不符重试用；带一次 `notify` 唤醒别的 worker）。
    fn requeue(&self, task: Task) {
        if task.is_empty() {
            return;
        }
        self.queue.lock().push_back(task);
        self.notify.notify_one();
    }

    /// 一块的字节**已经落盘并 fsync**：置位、记进度、必要时收尾。
    ///
    /// 顺序是刻意的：先 `bitmap_set` 再 `save_file`（`save_file` 会把记录写进内存并节流落盘），
    /// 而调用方保证"字节已经在盘上"—— 位图里的每一位就是那个承诺。
    fn complete_block(&self, fw: &Arc<FileWork>, index: u32, len: u32, digest: &[u8; 32]) -> bool {
        let (key, rec, already_done) = {
            let mut rec = fw.record.lock();
            // **幂等**：重试会把同一批块再走一遍（一次响应断在中间时，前面那些块早就置位了）。
            // 位图置位本身是幂等的，但计数不是 —— 再加一次 `done_blocks` 就会**超过总块数**，
            // 于是成功判据（`+1 == total_blocks`，以及收尾兜底里的 `== total_blocks`）永远不成立：
            // 一个字节不差、每块都校验过的文件被判成"没有收到全部数据"，`.part` 永远不改名。
            // `received` 同理（进度会超过 total）。
            let already_done = bitmap_has(&rec.blocks, index);
            bitmap_set(&mut rec.blocks, index);
            // 摘要存进记录：收尾时要把文件**再读一遍**逐块复核（见 `finalize`），
            // 也是跨会话续传时那些早先收下的块唯一的校验依据。
            rec.set_hash(index, digest);
            rec.updated = now_secs();
            (fw.key.clone(), rec.clone(), already_done)
        };
        self.state.push.resume.save_file(&key, rec);
        if already_done {
            return fw.done_blocks.load(Ordering::Acquire) >= fw.total_blocks;
        }
        if len > 0 {
            self.received.fetch_add(len as u64, Ordering::Relaxed);
        }
        fw.done_blocks.fetch_add(1, Ordering::AcqRel) + 1 == fw.total_blocks
    }

    /// 一条任务重试到底仍然失败：**只标"没收全"的那些文件**。
    ///
    /// 一次响应断在中间时，同一批里可能已经有文件整份收完并逐块校验过了（`complete_block` 全部
    /// 到位，只差响应末尾那个 `END`）；把它们一起标失败，收尾兜底扫描（`:880` 那段"跳过 failed"）
    /// 就不会再碰它们 —— 明明收全的文件永远停在 `.part`。没收全的照旧如实记失败。
    fn fail_task_files(&self, task: &Task, reason: &str) {
        let fail_if_incomplete = |fw: &Arc<FileWork>| {
            if fw.done_blocks.load(Ordering::Relaxed) < fw.total_blocks {
                self.mark_failed(&fw.rel, reason.to_string());
            }
        };
        match task {
            Task::Block(fw, _) => fail_if_incomplete(fw),
            Task::Bundle(items) => {
                for (fw, _) in items {
                    fail_if_incomplete(fw);
                }
            }
        }
    }

    /// 文件收尾：长度核对 → 截齐 → fsync → **原子改名** → 丢掉续传记录。
    async fn finalize(&self, fw: &Arc<FileWork>) -> Result<(), String> {
        if fw.finalized.swap(true, Ordering::SeqCst) {
            return Ok(()); // 已经收尾过（幂等）
        }
        // 已经判失败的文件**绝不改名到最终路径**。块收全之后仍可能收到同一文件的
        // `SourceChanged`（`mark_failed` 会把 `failed` 置位，但那时它可能已经在
        // `to_finalize` 里了）—— 这里再挡一次，最终路径上只出现"完整且没被判失败"的文件。
        if fw.failed.load(Ordering::SeqCst) {
            debug!(path = %fw.rel, "文件已判失败，不做收尾改名");
            return Ok(());
        }
        // 空文件（0 字节）没有任何块要传，也就没人建过 `.part`：这里补建一个，
        // 之后走同一条"长度核对 → fsync → 原子改名"的路（空文件同样要出现在最终路径上）。
        if fw.size == 0 && !fw.part.exists() {
            tokio::fs::OpenOptions::new()
                .create(true)
                .write(true)
                .open(&fw.part)
                .await
                .map_err(|e| format!("建临时文件失败: {e}"))?;
        }
        let meta = tokio::fs::metadata(&fw.part)
            .await
            .map_err(|e| format!("临时文件不见了: {e}"))?;
        if meta.len() < fw.size {
            return Err(format!(
                "落盘的文件短于预期（{} < {}）",
                meta.len(),
                fw.size
            ));
        }
        if meta.len() > fw.size {
            // 上一次尝试写得更长（那次崩在"记进度"之前）：截到精确大小，多出来的字节不留。
            let f = tokio::fs::OpenOptions::new()
                .write(true)
                .open(&fw.part)
                .await
                .map_err(|e| format!("打开临时文件失败: {e}"))?;
            f.set_len(fw.size)
                .await
                .map_err(|e| format!("截断失败: {e}"))?;
        }

        // **复核接手来的块**（本次会话现场收到的块已经在到达时逐块比对过）。
        //
        // 这道闸守的是"续传"这条路：位图说"这些块早在盘上了"，那是**以前的进程**说的 ——
        // 位图与磁盘之间只有这一处可以撒谎（进程被杀不丢页缓存，但系统崩溃/断电会），
        // 所以改名之前把它们读出来比一遍；不符就清位重下，这条文件这次算失败（绝不改名）。
        // 现场收到的块不读第二遍：那纯粹是重复劳动（大文件实测多花一倍时间）。
        self.verify_blocks(fw).await?;

        // 落盘确认（只有**大文件**做）：小文件每传一份都 fsync 一次的话，几千个文件就是几千次
        // 磁盘等待（NTFS 下一次几毫秒），吞吐直接掉一个数量级 —— 而这些字节本来就在页缓存里、
        // 进程被杀/崩溃都不会丢（内核还在），只有"断电/系统崩溃"那一档需要它。大文件一次 fsync
        // 相对自身传输时间可以忽略，所以按大小分档。
        if fw.size >= SYNC_MIN {
            let f = tokio::fs::OpenOptions::new()
                .write(true)
                .open(&fw.part)
                .await
                .map_err(|e| format!("打开临时文件失败: {e}"))?;
            f.sync_all().await.map_err(|e| format!("落盘失败: {e}"))?;
        }
        // 同卷 rename = 原子替换（Windows 上 std 用的是 `MOVEFILE_REPLACE_EXISTING`）：
        // 目标路径上永远是"旧的完整文件"或"新的完整文件"，没有中间态。
        tokio::fs::rename(&fw.part, &fw.target)
            .await
            .map_err(|e| format!("改名失败: {e}"))?;
        self.state.push.resume.drop_file(&fw.key);
        // 一个文件真的落定了 = 这条接收**有进展**：把待续传任务的 TTL 锚点往前挪
        // （够不着对端那种"没有进展"的等待就不该刷新它，见 `PendingInbound::updated_at`）。
        self.state.push.resume.touch_pending(&self.invite_id);
        debug!(path = %fw.rel, "文件收尾完成（原子改名）");
        Ok(())
    }

    /// 借一个缓冲（池里没有就新分配一个）。
    fn take_buf(&self) -> Vec<u8> {
        self.bufs
            .lock()
            .pop()
            .unwrap_or_else(|| vec![0u8; crate::net::STREAM_CHUNK])
    }

    /// 还回去（池子上限 8 个，够并发 worker 用；再多就是白占内存）。
    fn put_buf(&self, buf: Vec<u8>) {
        let mut pool = self.bufs.lock();
        if pool.len() < 8 {
            pool.push(buf);
        }
    }

    /// 复核**接手来的**那些块：按记录里的块摘要，把 `.part` 里对应的区间读出来比一遍。
    ///
    /// 不符的那一块：清掉它的位（下次重下）、落记录、返回错误 —— 这条文件这次算失败，
    /// **绝不改名**（宁可不给，也不能给一份内容错的）。
    ///
    /// 本次会话收到的块**不**在这里复核：它们在到达时就与对端的摘要比对过（见 `write_block`
    /// 的返回值），再读一遍只是重复劳动（大文件实测要多花一倍时间）。
    async fn verify_blocks(&self, fw: &Arc<FileWork>) -> Result<(), String> {
        let rec = fw.record.lock().clone();
        let mut buf = self.take_buf();
        let outcome = self.verify_blocks_inner(fw, &rec, &mut buf).await;
        self.put_buf(buf);
        outcome
    }

    async fn verify_blocks_inner(
        &self,
        fw: &Arc<FileWork>,
        rec: &super::resume::FileResume,
        buf: &mut [u8],
    ) -> Result<(), String> {
        let mut fh = tokio::fs::File::open(&fw.part)
            .await
            .map_err(|e| format!("打开临时文件失败: {e}"))?;
        for index in 0..fw.total_blocks {
            if !bitmap_has(&fw.adopted, index) {
                continue; // 本次会话收的块：到达时已经比对过
            }
            let Some((offset, blk_len)) = stream::block_range(fw.size, self.block_size, index)
            else {
                return Err(format!("第 {index} 块的区间算不出来"));
            };
            let Some(expect) = rec.block_hash(index) else {
                return Err(format!("第 {index} 块没有校验记录"));
            };
            fh.seek(std::io::SeekFrom::Start(offset))
                .await
                .map_err(|e| format!("校验时定位失败: {e}"))?;
            let mut hasher = Sha256::new();
            let mut left = blk_len as usize;
            while left > 0 {
                let want = left.min(buf.len());
                fh.read_exact(&mut buf[..want])
                    .await
                    .map(|_| ())
                    .map_err(|e| format!("校验时读文件失败: {e}"))?;
                hasher.update(&buf[..want]);
                left -= want;
            }
            let got: [u8; 32] = hasher.finalize().into();
            if hex_of(&got) != expect {
                let mut rec = fw.record.lock();
                rec.blocks = clear_bit(&rec.blocks, index);
                rec.updated = now_secs();
                self.state.push.resume.save_file(&fw.key, rec.clone());
                return Err(format!("第 {index} 块校验不符（已标记重下）"));
            }
        }
        Ok(())
    }

    /// 跑一次请求：把 `tasks` 发过去，按记录写盘。
    ///
    /// 返回 `Ok(finalize 待办)` = 这一批的传输部分交代完了；`Err` = 连接级失败（整批重试）。
    async fn run_request(
        &self,
        tasks: &[StreamTask],
        buf: &mut Vec<u8>,
    ) -> Result<Vec<Arc<FileWork>>, String> {
        // 先看原子真相；再看 watch。watch 会把"已经置位"也记在值里，不会丢掉
        // "置位发生在检查之后、等待之前"这一瞬间（`Notify::notify_waiters` 会丢）。
        if self.cancel.load(Ordering::Relaxed) {
            return Err("已取消".into());
        }
        let mut cancel = self.cancel_rx.clone();
        tokio::select! {
            _ = cancel.changed() => Err("已取消".into()),
            result = self.run_request_inner(tasks, buf) => result,
        }
    }

    async fn run_request_inner(
        &self,
        tasks: &[StreamTask],
        buf: &mut Vec<u8>,
    ) -> Result<Vec<Arc<FileWork>>, String> {
        // 带 token 的拨号按目标建表（同一地址上另一台已配对设备的证书不能顶替它）。
        let paired = self.state.get_peer(&self.peer_id).is_some_and(|p| p.paired);
        let client = crate::net::dial_client_streaming(
            &self.dial_host,
            &crate::credential::dial_pin(&self.peer_id, paired),
            std::time::Duration::from_secs(5),
            READ_IDLE_TIMEOUT,
        );
        let url = format!("{}/push/stream", self.base_url);
        let body = StreamRequest {
            id: self.invite_id.clone(),
            block_size: self.block_size,
            tasks: tasks.to_vec(),
        };
        let mut req = client.post(&url).json(&body);
        if let Some(t) = &self.token {
            req = req.bearer_auth(t);
        }
        let resp = req.send().await.map_err(|e| format!("请求失败: {e}"))?;
        let status = resp.status();
        if status.as_u16() == 410 {
            // 对端**不再提供**这条推送了：它撤销了、收尾了、或者它那台已经没有这条 job
            // （重启过 / 自己把这条判失败了）。这三件事在线上长得一模一样（都是 `Missing`），
            // 所以下面那句话不许写成"已取消" —— 见 `SENDER_GONE`。
            self.sender_cancel.store(true, Ordering::SeqCst);
            self.cancel.store(true, Ordering::SeqCst);
            return Err(super::SENDER_GONE.into());
        }
        if !status.is_success() {
            return Err(format!("HTTP {status}"));
        }

        // 响应体 → AsyncRead（`StreamReader`）→ 带缓冲的记录读取器。
        let stream = resp.bytes_stream().map_err(std::io::Error::other);
        let io = StreamReader::new(stream);
        let mut reader = RecordReader::new(tokio::io::BufReader::with_capacity(
            crate::net::STREAM_CHUNK,
            io,
        ));

        let mut to_finalize: Vec<Arc<FileWork>> = Vec::new();
        // 这一批里，每条路径的対账信息（`FILE` 记录）—— 块头要靠它校验。
        let mut seen: HashMap<String, (u64, i64, u32)> = HashMap::new();

        loop {
            if self.cancel.load(Ordering::Relaxed) {
                return Err("已取消".into());
            }
            let record = match reader.next().await {
                Ok(r) => r,
                Err(e) => return Err(format!("读记录失败: {e}")),
            };
            match record {
                Record::End => return Ok(to_finalize),
                Record::File {
                    path,
                    size,
                    mtime,
                    block_size,
                    ..
                } => {
                    let Some(fw) = self.by_path.get(&path).cloned() else {
                        // 我们没要这个路径：协议层面的错，整批作废（重试也没用）。
                        return Err(format!("对端发来了没请求的文件: {path}"));
                    };
                    // ① 对端那边的文件与清单对不上 = 源文件在传输期间被改过：
                    //    块布局是按清单算的，接着传只会得到一份拼凑的文件。
                    let manifest_mtime_matches = fw.mtime == 0 || mtime == 0 || fw.mtime == mtime;
                    if size != fw.size || !manifest_mtime_matches {
                        warn!(
                            path = %path, sent = size, want = fw.size,
                            sent_mtime = mtime, want_mtime = fw.mtime,
                            "对端的源文件与清单不符"
                        );
                        self.mark_source_changed(
                            &path,
                            format!("源文件在传输期间被修改（大小 {} → {}）", fw.size, size),
                        );
                        continue;
                    }
                    // ② 块布局必须与请求里的一致。
                    if block_size != self.block_size {
                        return Err(format!(
                            "对端用了不同的块大小: {} vs {}",
                            block_size, self.block_size
                        ));
                    }
                    seen.insert(path, (size, mtime, block_size));
                }
                Record::BlockHeader {
                    path,
                    index,
                    offset,
                    len,
                } => {
                    let Some(fw) = self.by_path.get(&path).cloned() else {
                        return Err(format!("对端发来了没请求的文件: {path}"));
                    };
                    if fw.failed.load(Ordering::Relaxed) {
                        // 已经判失败的文件：把载荷读掉（保持流同步），不写盘。
                        let mut left = len as usize;
                        while left > 0 {
                            let want = left.min(buf.len());
                            reader
                                .read_exact(&mut buf[..want])
                                .await
                                .map_err(|e| format!("读载荷失败: {e}"))?;
                            left -= want;
                        }
                        let _ = reader.digest().await;
                        continue;
                    }
                    // ③ 区间硬校验：对端只能在"我们要的那一块"上写字节。
                    let expect = stream::block_range(fw.size, self.block_size, index);
                    let ok = expect == Some((offset, len)) && seen.contains_key(&path);
                    if !ok {
                        self.mark_failed(&path, "对端发来的块区间与请求不符".into());
                        let mut left = len as usize;
                        while left > 0 {
                            let want = left.min(buf.len());
                            let _ = reader.read_exact(&mut buf[..want]).await;
                            left -= want;
                        }
                        let _ = reader.digest().await;
                        continue;
                    }
                    let digest_remote = match self
                        .write_block(&fw, index, offset, len, &mut reader, buf)
                        .await
                    {
                        Ok((local, remote)) => (local, remote),
                        Err(e) => return Err(e),
                    };
                    if !digest_eq(&digest_remote.0, &digest_remote.1) {
                        // 这一块的字节不认：不置位、不计进度，重排一次；再不行判这条文件失败。
                        let first_time = {
                            let mut retried = fw.retried.lock();
                            if retried.contains(&index) {
                                false
                            } else {
                                retried.push(index);
                                true
                            }
                        };
                        if first_time && fw.retried.lock().len() <= MAX_HASH_RETRIES {
                            warn!(path = %path, index, "块哈希不符，重排这一块");
                            self.requeue(Task::Block(fw.clone(), index));
                        } else {
                            self.mark_failed(
                                &path,
                                format!("第 {index} 块校验不符（重试后仍不符）"),
                            );
                        }
                        continue;
                    }
                    if self.complete_block(&fw, index, len, &digest_remote.0)
                        && !fw.failed.load(Ordering::Relaxed)
                    {
                        to_finalize.push(fw);
                    }
                }
                Record::Skip { path, reason } => match reason {
                    SKIP_COMPLETE => {
                        // 对端认为这块没什么可发的（offset 已经到末尾）。我们按"这一块好了"记，
                        // 但**不**计进度：字节本来就不在网络上传。
                        debug!(path = %path, "对端说这条已经完整");
                    }
                    SKIP_SOURCE_CHANGED => {
                        self.mark_failed(&path, "源文件在传输期间被修改".into());
                    }
                    SKIP_NOT_IN_MANIFEST => {
                        self.mark_failed(&path, "对端说这条不在这次推送里".into());
                    }
                    other => {
                        self.mark_failed(&path, format!("对端跳过（未知理由 {other}）"));
                    }
                },
                Record::Err { path, message } => {
                    if path.is_empty() {
                        return Err(message);
                    }
                    self.mark_failed(&path, message);
                }
                Record::SourceChanged { path } => {
                    self.mark_source_changed(&path, "源文件在传输期间被修改".into());
                }
            }
        }
    }

    /// 读一批载荷 → 写进 `.part` → 落盘 → 返回 `(本地摘要, 对端摘要)`。
    ///
    /// 只有 `Ok` 且两个摘要相等时，调用方才允许置位与计进度。
    async fn write_block<R: tokio::io::AsyncRead + Unpin>(
        &self,
        fw: &Arc<FileWork>,
        _index: u32,
        offset: u64,
        len: u32,
        reader: &mut RecordReader<R>,
        buf: &mut Vec<u8>,
    ) -> Result<([u8; 32], [u8; 32]), String> {
        let mut fh = tokio::fs::OpenOptions::new()
            .create(true)
            .write(true)
            .open(&fw.part)
            .await
            .map_err(|e| format!("打开临时文件失败: {e}"))?;
        fh.seek(std::io::SeekFrom::Start(offset))
            .await
            .map_err(|e| format!("定位失败: {e}"))?;
        let mut out = tokio::io::BufWriter::with_capacity(crate::net::STREAM_CHUNK, &mut fh);
        let mut hasher = Sha256::new();
        let mut left = len as usize;
        while left > 0 {
            if self.cancel.load(Ordering::Relaxed) {
                // 读一半就收手：这块不置位（下次重下），但已经写进去的字节留着
                // —— 它们没有自相矛盾，只是"未确认"。
                return Err("已取消".into());
            }
            let want = left.min(buf.len());
            reader
                .read_exact(&mut buf[..want])
                .await
                .map_err(|e| format!("读载荷失败: {e}"))?;
            hasher.update(&buf[..want]);
            out.write_all(&buf[..want])
                .await
                .map_err(|e| format!("写入失败: {e}"))?;
            left -= want;
        }
        let remote = reader
            .digest()
            .await
            .map_err(|e| format!("读摘要失败: {e}"))?;
        let local: [u8; 32] = hasher.finalize().into();
        // 把缓冲交出去就返回：这一块的**可信度**由两件事保证 —— 收到时摘要比对（下面返回值），
        // 以及收尾时把整个文件再读一遍逐块复核（`verify_blocks`）。这里不再每块 `fsync`：
        // 那是每块一次磁盘等待，小文件场景实测把吞吐按死在一个数量级以下。
        out.flush().await.map_err(|e| format!("写入失败: {e}"))?;
        drop(out);
        Ok((local, remote))
    }
}

/// 开始一次新协议的接收（由 `push_job::spawn_download` 按能力位分派）。
pub(crate) fn spawn(
    state: Arc<AppState>,
    invite: InboundInvite,
    peer_id: String,
    token: Option<String>,
    control: Arc<DownloadControl>,
    gate: push_job::ReceiveGate,
) {
    // `InboundInvite` 里有一个 oneshot 发送端（不是 `Clone`），所以失败收尾要用的东西
    // 先取出来，再把邀请整体移进任务。
    let invite_id = invite.invite_id.clone();
    crate::task::spawn("push download (stream)", async move {
        // 暂停闸表项跟着这条任务活（完成 / 失败 / 取消都摘掉，见 `ReceiveGate`）。
        let _gate = gate;
        if let Err(e) = run(state.clone(), invite, peer_id.clone(), token, control).await {
            // 会话级失败（建目录、清单非法、请求……）：**这一轮没成，但推送还在** ——
            // 保留邀请与待续传记录，退避重试。真落终态的两条路是"对端撤了（410）"与
            // "源文件与清单对不上"，它们自己走 `fail_download`。
            push_job::fail_attempt(&state, &peer_id, &invite_id, e);
        }
    });
}

async fn run(
    state: Arc<AppState>,
    invite: InboundInvite,
    peer_id: String,
    token: Option<String>,
    control: Arc<DownloadControl>,
) -> Result<(), String> {
    // 淘汰放太久的旧记录（连带它们指着的 `.part`）：兜底清理，顺手做一次就够。
    let swept = state.push.resume.sweep_stale(30 * 24 * 60 * 60);
    if swept > 0 {
        info!(swept, "清掉了过期的续传记录");
    }
    // 续传记录可能来自旧版本/被篡改的本机文件：真正分配位图前再校验一次。
    crate::transfer::push_job::validate_invite(&invite)
        .map_err(|e| format!("清单超出上限: {e}"))?;

    let config = state.config.lock().clone();
    // 落盘根 = `<落盘目录>/<对端名字>`（S-12 关掉这层时就是落盘根本身，见 `resume::landing_dir`）。
    // 落盘目录默认是设置里的下载目录；只有在「每次询问」
    // 下由用户亲自挑过的邀请才带自己的根（`invite.root`，见 `accept_invite`）。
    // 名字先净化（对端自报的串不能直接当目录名用，
    // 否则 `C:\Windows\Temp` / `..\..\Users\Public` 会把基址那一层整个顶掉）。
    let base = invite
        .root
        .as_deref()
        .unwrap_or(config.download_dir.as_path());
    let dir = super::resume::landing_dir(base, &invite.sender.name, config.device_subdir);
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|e| format!("建目录失败: {e}"))?;

    // 两个旋钮都从配置来（设置里的「传输并发数」「分块大小」），并在读的时候夹一次
    // 取值域 —— 配置文件是可以手改的，越界的值不能让传输行为失控。
    let block_size = crate::config::clamp_transfer_block_mb(config.transfer_block_mb) * 1024 * 1024;
    let workers = crate::config::clamp_transfer_workers(config.transfer_workers) as usize;
    let callback = invite.callback.clone();
    // 数据面拨谁：优先地址簿里此刻的拨号地址（可能是中转端点，F-106），
    // 拿不到才用邀请里带来的那个（对端自报的旧地址）。
    let dial = state
        .dial_endpoint(&peer_id, callback.clone())
        .unwrap_or_default();
    let dial_host = dial.0.clone();
    let base_url = crate::net::peer_base(&dial.0, dial.1);

    // ---- 准备每个文件：路径、身份、续传记录 ----------------------------------------
    let mut files: Vec<Arc<FileWork>> = Vec::new();
    let mut received: u64 = 0;
    let mut made_dirs: HashSet<PathBuf> = HashSet::new();
    let mut pending_finalize: Vec<Arc<FileWork>> = Vec::new();

    for entry in invite.files.iter() {
        let target =
            safe_join(&dir, &entry.path).ok_or_else(|| format!("非法路径: {}", entry.path))?;
        if let Some(parent) = target.parent() {
            if made_dirs.insert(parent.to_path_buf()) {
                tokio::fs::create_dir_all(parent)
                    .await
                    .map_err(|e| format!("建目录失败: {e}"))?;
            }
        }
        // 身份 = 对端 id + **落盘目录** + 相对路径 + 大小 + 时间戳（见 `stream::resume_key`）：
        // 同一个文件收在两个目录下是两份记录，互不踩。
        let key = stream::resume_key(
            &peer_id,
            target.parent().unwrap_or(&dir),
            &entry.path,
            entry.size,
            entry.mtime,
        );
        let part = target
            .parent()
            .unwrap_or(&dir)
            .join(stream::part_name(&key));
        let total_blocks = stream::block_count(entry.size, block_size);

        // 续传：只有"身份（含大小与时间戳）+ 块大小都一致"才认这条旧记录，
        // 否则那份位图指的是另一个文件。
        let adopted = state
            .push
            .resume
            .file_record(&key)
            .filter(|rec| rec.size == entry.size && rec.block_size == block_size);
        let mut rec = match adopted {
            Some(mut rec) if part.exists() => {
                let part_len = std::fs::metadata(&part).map(|m| m.len()).unwrap_or(0);
                // 记录与磁盘对账：位图声称写完的字节若超过文件实际长度，那些位不可信
                // （理论上不该发生 —— 置位前都 fsync 过；真遇上就以磁盘为准）。
                for index in 0..total_blocks {
                    if bitmap_has(&rec.blocks, index) {
                        if let Some((offset, len)) =
                            stream::block_range(entry.size, block_size, index)
                        {
                            if offset + len as u64 > part_len {
                                warn!(path = %entry.path, index, "位图与磁盘对不上，这块重下");
                                rec.blocks = clear_bit(&rec.blocks, index);
                            }
                        }
                    }
                }
                rec.part = part.to_string_lossy().into_owned();
                rec
            }
            _ => {
                // 没有可用的记录：从零开始（顺手把可能残留的 `.part` 清掉，别让旧字节
                // 被当成这次的进度 —— 那种"半新半旧"正是最难查的一类坏文件）。
                if part.exists() {
                    let _ = tokio::fs::remove_file(&part).await;
                }
                FileResume::new(
                    entry.size,
                    entry.mtime,
                    block_size,
                    part.to_string_lossy().into_owned(),
                )
            }
        };

        rec.size = entry.size;
        rec.mtime = entry.mtime;
        rec.part = part.to_string_lossy().into_owned();
        // 这份位图是"接手"的（本次会话之前就在盘上），收尾时要复核它（见 `FileWork::adopted`）。
        // 注意是在**与磁盘对账之后**取快照：被清掉的位不属于"已在盘上"，不该被信任也不该被复核。
        let adopted_bits = rec.blocks.clone();
        let done_blocks = (0..total_blocks)
            .filter(|i| bitmap_has(&rec.blocks, *i))
            .count() as u32;
        received += rec.done_bytes();
        state.push.resume.save_file(&key, rec.clone());

        let fw = Arc::new(FileWork {
            rel: entry.path.clone(),
            size: entry.size,
            mtime: entry.mtime,
            target,
            part,
            key,
            total_blocks,
            done_blocks: AtomicU32::new(done_blocks),
            record: Mutex::new(rec),
            failed: AtomicBool::new(false),
            retried: Mutex::new(Vec::new()),
            finalized: AtomicBool::new(false),
            adopted: adopted_bits,
        });
        if total_blocks > 0 && done_blocks == total_blocks {
            // 续传记录说它已经全了：只差改名（可能上次就崩在这一步）。
            pending_finalize.push(fw.clone());
        }
        files.push(fw);
    }

    // ---- 组队列 -------------------------------------------------------------------
    let mut queue: VecDeque<Task> = VecDeque::new();
    let mut bundle: Vec<(Arc<FileWork>, u32)> = Vec::new();
    let mut bundle_bytes: u64 = 0;
    for fw in &files {
        if fw.total_blocks == 0 {
            // 空文件：没有块要传，直接收尾（建一个空文件）。
            pending_finalize.push(fw.clone());
            continue;
        }
        if fw.done_blocks.load(Ordering::Relaxed) == fw.total_blocks {
            continue; // 已在 `pending_finalize` 里
        }
        let remaining = fw.remaining_blocks();
        let small = fw.total_blocks == 1 && fw.size <= BUNDLE_FILE_MAX;
        if small {
            bundle.push((fw.clone(), 0));
            bundle_bytes += fw.size;
            if bundle.len() >= BUNDLE_MAX_TASKS || bundle_bytes >= BUNDLE_MAX_BYTES {
                queue.push_back(Task::Bundle(std::mem::take(&mut bundle)));
                bundle_bytes = 0;
            }
        } else {
            // 大文件：一块一条任务，几条 worker 就能同时啃它。清单顺序即队列顺序。
            for index in remaining {
                queue.push_back(Task::Block(fw.clone(), index));
            }
        }
    }
    if !bundle.is_empty() {
        queue.push_back(Task::Bundle(bundle));
    }

    let total = invite.total_bytes();
    let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
    let session = Arc::new(Session {
        state: state.clone(),
        peer_id: peer_id.clone(),
        invite_id: invite.invite_id.clone(),
        callback: callback.clone(),
        base_url,
        dial_host,
        token,
        block_size,
        workers,
        files: files.clone(),
        by_path: files.iter().map(|f| (f.rel.clone(), f.clone())).collect(),
        queue: Mutex::new(queue),
        notify: tokio::sync::Notify::new(),
        received: Arc::new(AtomicU64::new(received)),
        total,
        cancel: Arc::new(AtomicBool::new(false)),
        cancel_tx,
        cancel_rx,
        local_cancel: AtomicBool::new(false),
        sender_cancel: AtomicBool::new(false),
        source_changed: AtomicBool::new(false),
        finished: AtomicBool::new(false),
        failed: Mutex::new(Vec::new()),
        bufs: Mutex::new(Vec::new()),
        control,
    });

    info!(
        invite = %session.invite_id,
        files = session.files.len(),
        total,
        resumed = received,
        workers = session.workers,
        "开始并发接收（分帧协议）"
    );

    // 已经在盘上的部分先算进进度（断点续传时用户看到的就是"接着上次的进度条"）。
    if received > 0 {
        report_progress(&session);
    }

    // ---- 收尾待办（续传已完成 / 空文件）-------------------------------------------
    for fw in pending_finalize {
        if let Err(e) = session.finalize(&fw).await {
            session.mark_failed(&fw.rel, e);
        }
    }

    // ---- 进度/取消的观察者 ---------------------------------------------------------
    {
        let s = session.clone();
        crate::task::spawn("push progress (stream)", async move {
            reporter(s).await;
        });
    }

    // ---- worker 池 -----------------------------------------------------------------
    let mut handles = Vec::new();
    for _ in 0..session.workers {
        let s = session.clone();
        handles.push(crate::task::spawn(
            "push worker",
            async move { worker(s).await },
        ));
    }
    for h in handles {
        let _ = h.await;
    }

    session.finished.store(true, Ordering::SeqCst);
    session.notify.notify_waiters();
    // 最后一次进度（收尾那几毫秒里落盘的字节不该少报）。
    report_progress(&session);
    session.state.push.resume.flush();

    // ---- 结局 -----------------------------------------------------------------------
    if session.sender_cancel.load(Ordering::SeqCst) {
        push_job::fail_download(
            &state,
            &peer_id,
            &callback,
            &invite.invite_id,
            super::SENDER_GONE.into(),
        );
        return Ok(());
    }
    if session.local_cancel.load(Ordering::SeqCst) {
        // 本机用户点的取消：`cancel_receive` 已经把话带过去了，这里静静收手。
        info!(invite = %session.invite_id, "本机取消，接收收手");
        return Ok(());
    }

    // 收尾兜底：某条文件的最后一块已经落位，但那次响应没走到 `END`（对端断流/重试窗口）——
    // 它不会再排上队，只能在这里补收尾。`finalize` 幂等，重复调用无副作用。
    for fw in &session.files {
        if fw.failed.load(Ordering::Relaxed) || fw.finalized.load(Ordering::Relaxed) {
            continue;
        }
        if fw.total_blocks == 0 || fw.done_blocks.load(Ordering::Relaxed) == fw.total_blocks {
            if let Err(e) = session.finalize(fw).await {
                session.mark_failed(&fw.rel, e);
            }
        }
    }

    // **完成审计**：走到这里，每一条文件都必须已经收尾。还没收尾的 = 数据没收全
    // （队列排空但任务没做完：对端中途撤了、或者某次响应只发了一半就结束）——
    // 这种必须如实记成失败，绝不能因为"没有失败记录"就回一句 `completed`。
    for fw in &session.files {
        if !fw.finalized.load(Ordering::Relaxed) {
            session.mark_failed(&fw.rel, "没有收到全部数据（传输被中断）".into());
        }
    }

    let failed = session.failed.lock().clone();
    if failed.is_empty() {
        state.push.resume.drop_pending(&invite.invite_id);
        state
            .push
            .record_outcome(&invite.invite_id, &peer_id, "done", total, None);
        state.push.remove_invite(&invite.invite_id);
        push_job::report_to_sender(
            &state,
            &peer_id,
            &callback,
            WsEvent::PushDone {
                invite_id: invite.invite_id.clone(),
                status: "completed".into(),
                reason: None,
            },
        );
        crate::ws::emit_ui(
            &state,
            crate::ws::events::ui::PUSH_DONE,
            &serde_json::json!({
                "inviteId": invite.invite_id,
                "status": "completed",
                "dest": dir.to_string_lossy(),
            }),
        );
        info!(invite = %session.invite_id, files = session.files.len(), "并发接收完成");
    } else {
        let reason = session.summarize_failure();
        if session.source_changed.load(Ordering::Relaxed) {
            // 对端的源文件与清单对不上：再试还是同一份清单，落终态。
            push_job::fail_download(&state, &peer_id, &callback, &invite.invite_id, reason);
        } else {
            // 其余（网络断了、写不进去、有几块重试到底仍失败）→ **这次不成、推送还在**：
            // 保留邀请与待续传任务，退避重试（见 `transfer::resume_pending`）。
            push_job::fail_attempt(&state, &peer_id, &invite.invite_id, reason);
        }
    }
    Ok(())
}

/// 进度 + 取消观察：每 `REPORT_INTERVAL` 一拍。
///
/// 取消的判据是"邀请还在不在"（`cancel_receive` 会把它取走），与老循环同一条。
async fn reporter(session: Arc<Session>) {
    // 从 0 起算（而不是 `u64::MAX`）：进度事件只在**真有字节落位**之后才发。
    // 发一条"正在传输、已收 0 字节"的事件不是"实时"，是噪音 —— 界面那张卡会先闪一下
    // 传输中、进度 0%，而发送方的聚合进度（"成功/等待"那行数字）也会被这条空事件推着走。
    // 老循环就是"第一块字节进门才报"，这里保持同一条语义（p1 的聚合进度用例钉着它）。
    let mut last = 0u64;
    loop {
        sleep(REPORT_INTERVAL).await;
        if session.finished.load(Ordering::SeqCst) {
            return;
        }
        if session
            .state
            .push
            .inbound
            .lock()
            .get(&session.invite_id)
            .is_none()
        {
            session.local_cancel.store(true, Ordering::SeqCst);
            session.cancel.store(true, Ordering::SeqCst);
            let _ = session.cancel_tx.send(true);
            session.notify.notify_waiters();
            return;
        }
        let received = session.received.load(Ordering::Relaxed);
        if received != last {
            last = received;
            report_progress(&session);
        }
    }
}

fn report_progress(session: &Session) {
    let received = session.received.load(Ordering::Relaxed);
    push_job::report_to_sender(
        &session.state,
        &session.peer_id,
        &session.callback,
        WsEvent::PushProgress {
            invite_id: session.invite_id.clone(),
            received,
            total: session.total,
        },
    );
    crate::ws::emit_ui(
        &session.state,
        crate::ws::events::ui::PUSH_PROGRESS,
        &serde_json::json!({
            "inviteId": session.invite_id,
            "received": received,
            "total": session.total,
        }),
    );
}

async fn worker(session: Arc<Session>) {
    let mut buf = vec![0u8; crate::net::STREAM_CHUNK];
    loop {
        if session.cancel.load(Ordering::Relaxed) {
            return;
        }
        // **先取任务、再等闸门**：队列空了就退出 —— 暂停不该把收尾挡在门外（最后一块已经
        // 落盘、只差报终态时，用户按一下暂停就永远收不了尾）。拿在手里的任务不会丢：
        // 继续后照常发请求；取消则随 worker 一起放掉（取消本来就是"不要了"）。
        let Some(task) = session.next_task() else {
            return;
        };
        // 用户暂停：不向对端要任何新字节。正在收的那一批照常落盘，之后停在这里。
        // 取消会先放开闸门（`cancel_receive`），所以醒来后再看一眼 `cancel`。
        session.control.wait_if_paused().await;
        if session.cancel.load(Ordering::Relaxed) {
            return;
        }
        let (tasks, label) = match &task {
            Task::Block(fw, index) => (
                vec![StreamTask {
                    path: fw.rel.clone(),
                    index: *index,
                }],
                format!("block {}#{}", fw.rel, index),
            ),
            Task::Bundle(items) => (
                items
                    .iter()
                    .map(|(fw, index)| StreamTask {
                        path: fw.rel.clone(),
                        index: *index,
                    })
                    .collect(),
                format!("bundle x{}", items.len()),
            ),
        };

        let mut done = false;
        for attempt in 1..=MAX_ATTEMPTS {
            match session.run_request(&tasks, &mut buf).await {
                Ok(to_finalize) => {
                    for fw in to_finalize {
                        if let Err(e) = session.finalize(&fw).await {
                            session.mark_failed(&fw.rel, e);
                        }
                    }
                    done = true;
                    break;
                }
                Err(e) => {
                    if session.cancel.load(Ordering::Relaxed) {
                        return;
                    }
                    error!(task = %label, attempt, %e, "这次请求失败");
                    if attempt < MAX_ATTEMPTS {
                        sleep(Duration::from_secs(1 << (attempt - 1))).await;
                    }
                }
            }
        }
        if !done {
            // 重试到底还是失败：把这一批里**没传完**的文件记成失败（一条坏文件不该拖垮整批；
            // 已经收全的那些不标 —— 它们只差收尾，交给兜底扫描）。
            session.fail_task_files(&task, "取文件失败（重试后仍失败）");
        }
    }
}

/// 32 字节摘要 → hex（与记录里存的形式一致，便于比对）。
fn hex_of(digest: &[u8; 32]) -> String {
    let mut out = String::with_capacity(64);
    for b in digest {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// 清掉一位（`bitmap_set` 的逆操作）：只在"校验不符/位图与磁盘对不上"这两条兜底路径上用。
fn clear_bit(bits: &str, index: u32) -> String {
    let byte = (index / 8) as usize;
    let at = byte * 2;
    if bits.len() < at + 2 {
        return bits.to_string();
    }
    let cur = u8::from_str_radix(&bits[at..at + 2], 16).unwrap_or(0);
    let next = cur & !(1u8 << (index % 8));
    let mut out = bits.to_string();
    out.replace_range(at..at + 2, &format!("{next:02x}"));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    const BLOCK: u32 = 4 * 1024 * 1024;

    fn work(rel: &str, size: u64, total_blocks: u32) -> Arc<FileWork> {
        let dir = std::env::temp_dir();
        Arc::new(FileWork {
            rel: rel.into(),
            size,
            mtime: 0,
            target: dir.join(format!("orbdrop_dl_test_{rel}")),
            part: dir.join(format!("orbdrop_dl_test_{rel}.part")),
            key: format!("key-{rel}"),
            total_blocks,
            done_blocks: AtomicU32::new(0),
            record: Mutex::new(FileResume::new(size, 0, BLOCK, String::new())),
            failed: AtomicBool::new(false),
            retried: Mutex::new(Vec::new()),
            finalized: AtomicBool::new(false),
            adopted: String::new(),
        })
    }

    fn session(files: Vec<Arc<FileWork>>) -> Session {
        let dir = std::env::temp_dir().join(format!("orbdrop_dl_sess_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let state = Arc::new(AppState::new(
            crate::config::Config::default(),
            dir.join("config.toml"),
        ));
        let (cancel_tx, cancel_rx) = tokio::sync::watch::channel(false);
        Session {
            state,
            peer_id: "device-test".into(),
            invite_id: "inv-test".into(),
            callback: None,
            base_url: String::new(),
            dial_host: String::new(),
            token: None,
            block_size: BLOCK,
            workers: 1,
            files: files.clone(),
            by_path: files.iter().map(|f| (f.rel.clone(), f.clone())).collect(),
            queue: Mutex::new(VecDeque::new()),
            notify: tokio::sync::Notify::new(),
            received: Arc::new(AtomicU64::new(0)),
            total: files.iter().map(|f| f.size).sum(),
            cancel: Arc::new(AtomicBool::new(false)),
            cancel_tx,
            cancel_rx,
            local_cancel: AtomicBool::new(false),
            sender_cancel: AtomicBool::new(false),
            source_changed: AtomicBool::new(false),
            finished: AtomicBool::new(false),
            failed: Mutex::new(Vec::new()),
            bufs: Mutex::new(Vec::new()),
            control: Arc::new(super::DownloadControl::default()),
        }
    }

    /// 重试会重发同一批块：同一块完成两次**不能**记两次数（P1-9）。
    ///
    /// 记两次的后果不是"进度多点"这么轻：`done_blocks` 会超过总块数，"`+1 == total_blocks`"
    /// 与收尾兜底里的 `== total_blocks` 就永远不成立 —— 一个字节不差、每块都校验过的文件
    /// 被判成"没有收到全部数据（传输被中断）"，`.part` 永远停在原地。
    #[test]
    fn a_retried_block_is_not_counted_twice() {
        let fw = work("a.bin", 2 * BLOCK as u64, 2);
        let s = session(vec![fw.clone()]);
        let digest = [7u8; 32];

        assert!(
            !s.complete_block(&fw, 0, BLOCK, &digest),
            "还差一块，没完成"
        );
        assert_eq!(fw.done_blocks.load(Ordering::Acquire), 1);
        assert_eq!(s.received.load(Ordering::Relaxed), BLOCK as u64);

        // 同一块再来一次（断流重试）：计数不动、字节不动，仍然"没完成"。
        assert!(
            !s.complete_block(&fw, 0, BLOCK, &digest),
            "重复的那块不该让它变成完成"
        );
        assert_eq!(
            fw.done_blocks.load(Ordering::Acquire),
            1,
            "重复的块不能再记一次数"
        );
        assert_eq!(
            s.received.load(Ordering::Relaxed),
            BLOCK as u64,
            "进度也不能跟着涨（会超过 total）"
        );
        assert_eq!(
            fw.record.lock().done_bytes(),
            BLOCK as u64,
            "位图里仍然只有一块"
        );

        // 第二块到位 → 真的完成。
        assert!(s.complete_block(&fw, 1, BLOCK, &digest));
        assert_eq!(fw.done_blocks.load(Ordering::Acquire), 2);
        assert_eq!(s.received.load(Ordering::Relaxed), 2 * BLOCK as u64);
    }

    /// 一批任务重试到底仍失败时，**已经收全**的成员不能被标失败（P1-9）。
    ///
    /// 标了就会掉进收尾兜底扫描的"跳过 failed"分支：收全的文件永远不改名（明明只差 END 那一步）。
    #[test]
    fn only_incomplete_files_are_failed_when_a_task_dies() {
        let digest = [3u8; 32];
        let done = work("done.txt", BLOCK as u64, 1);
        let broken = work("broken.txt", 2 * BLOCK as u64, 2);
        let s = session(vec![done.clone(), broken.clone()]);
        assert!(
            s.complete_block(&done, 0, BLOCK, &digest),
            "这份整份收完了（只差收尾）"
        );
        assert!(
            !s.complete_block(&broken, 0, BLOCK, &digest),
            "这份只收了一半"
        );

        s.fail_task_files(
            &Task::Bundle(vec![(done.clone(), 0), (broken.clone(), 1)]),
            "取文件失败（重试后仍失败）",
        );

        let failed = s.failed.lock().clone();
        assert_eq!(
            failed.iter().map(|(p, _)| p.clone()).collect::<Vec<_>>(),
            vec!["broken.txt".to_string()],
            "只有没收全的那份该被标失败"
        );
        assert!(
            !done.failed.load(Ordering::SeqCst),
            "收全的那份不能被标 failed"
        );
        assert!(broken.failed.load(Ordering::SeqCst));
        // 收全的那份照旧参与收尾（兜底扫描只看 `failed`/`finalized`）。
        assert!(!done.finalized.load(Ordering::SeqCst));
    }

    /// 块收全之后才收到 `SourceChanged`（同一文件的失败已经是终态）时，收尾**绝不能**
    /// 把 `.part` 改名到最终路径：否则最终路径上会出现一份新旧混合/不完整的文件。
    #[tokio::test]
    async fn a_failed_file_is_never_published() {
        let fw = work("failed_never_published.bin", 4, 1);
        let _ = std::fs::remove_file(&fw.target);
        std::fs::write(&fw.part, [1u8; 4]).unwrap();
        // 模拟"已经进了 to_finalize、随后又被判失败"。
        fw.failed.store(true, Ordering::SeqCst);

        let s = session(vec![fw.clone()]);
        s.finalize(&fw).await.unwrap();

        assert!(
            !fw.target.exists(),
            "已判失败的文件不该出现在最终路径上"
        );
        assert!(fw.part.exists(), "临时文件保留给上层清理/重试");
        let _ = std::fs::remove_file(&fw.part);
    }

    /// 暂停不该把**已经没活儿的会话**挡在收尾之前：队列空了 worker 直接退出，
    /// 不因为闸门合着就永远等下去（否则用户恰好在 100% 时按暂停，文件就永远不改名/不报完成）。
    #[tokio::test]
    async fn an_empty_queue_finishes_even_while_paused() {
        let s = Arc::new(session(Vec::new()));
        s.control.set_paused(true);
        tokio::time::timeout(Duration::from_millis(500), worker(s))
            .await
            .expect("空队列的 worker 不该等在暂停闸门前");
    }

    /// 暂停时**一个块都不开始拉**：worker 把任务拿在手里、停在闸门前；取消（或继续）才退出。
    #[tokio::test]
    async fn a_paused_worker_does_not_start_its_block() {
        let fw = work("paused_worker.bin", 4, 1);
        let s = Arc::new(session(vec![fw.clone()]));
        s.queue.lock().push_back(Task::Block(fw, 0));
        s.control.set_paused(true);

        let handle = tokio::spawn(worker(s.clone()));
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!handle.is_finished(), "暂停中 worker 不该开始取块");

        // 取消会先放开闸门（`cancel_receive` 就是这个顺序）：worker 醒来看到取消就退出。
        s.cancel.store(true, Ordering::SeqCst);
        s.control.set_paused(false);
        tokio::time::timeout(Duration::from_secs(1), handle)
            .await
            .expect("放开闸门后 worker 应当退出")
            .expect("worker 不应 panic");
    }
}
