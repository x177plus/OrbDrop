//! 发送端：把一批块任务变成一串分帧记录（`POST /push/stream` 的响应体）。
//!
//! 与老端点（`GET /push/download`，一个文件一次请求）的关系：**并存**。老端点在能力协商
//! 里没有 `stream1` 的对端照旧用它，这里这条只服务新对端。两边共用同一份"清单白名单 +
//! 认人"规则（`PushStore::download_target` / `routes_push::authorize`），区别只在搬运方式：
//!
//! - 老端点：一次一个文件、整份流；
//! - 这条：一次一批**块**（大文件的一块，或一批小文件的各自一整块），块之间可以并行。
//!
//! ## 为什么要在这里做"边读边算边发"
//!
//! 块的摘要写在**载荷后面**（尾随）。这样发送端不需要先把一块（4MB）攒在内存里：
//! 读 256KB → 立刻发出去 → 喂哈希，一直到块尾把 32 字节摘要补上。接收端同理：边写盘
//! 边算，读完载荷再读摘要比对。两边的内存都只与 256KB 的缓冲挂钩，与块大小、文件大小
//! 都无关。
//!
//! ## 源文件在传输期间被改了怎么办
//!
//! 三条都做，成本都只有一次 `stat`：
//! 1. 块序号越界（对端按**旧**大小算出来的块，现在文件变小了）→ `SKIP(SOURCE_CHANGED)`；
//! 2. `FILE` 记录带上当前的 `size`/`mtime` —— 接收端拿它与自己那份清单比对，不一致就判
//!    这条文件失败（它那边的块布局也是按旧大小算的，接着传只会得到一份拼凑的文件）；
//! 3. 一块发完之后复核一次 `size`/`mtime`，变了就补一条 `SOURCE_CHANGED` ——
//!    这是"传到一半文件被改"唯一的探测手段（块哈希只能证明"发出去的字节 = 读到的字节"）。

use std::sync::atomic::Ordering;

use tokio::fs::File;
use tokio::io::{AsyncReadExt, AsyncSeekExt};
use tokio::sync::mpsc::Sender;
use tracing::{debug, info, warn};

use super::stream::{self, StreamRequest, SKIP_NOT_IN_MANIFEST};
use super::push_job::DownloadTarget;

/// 一次读取多大再发（与帧容量同量级；再大也换不来什么，只会多占内存）。
const READ_CHUNK: usize = crate::net::STREAM_CHUNK;

/// 执行一批任务，把记录写进 `tx`，直到写不动（对端断了）或任务做完。
///
/// **没有返回值**：任务级的失败已经用 `ERR`/`SKIP` 记录说清楚了（那才是接收端看的东西），
/// 连接级的失败由 `tx.send` 的 `Err` 表达（对端断流）。两条语义都在协议里，不再另设一个
/// 没人消费的布尔量。
pub async fn serve(
    state: &std::sync::Arc<crate::state::AppState>,
    req: &StreamRequest,
    tx: &Sender<Vec<u8>>,
) {
    let mut buf = vec![0u8; READ_CHUNK];
    let mut header: Vec<u8> = Vec::with_capacity(256);

    for task in &req.tasks {
        // 用户取消 / 这条推送已经收尾：整条停下来（接收端会看到断流并重试，下一次请求
        // 拿到 410 就知道是"发送方已取消"）。
        let (abs, cancelled) = match state.push.download_target(&req.id, &task.path) {
            DownloadTarget::Missing => {
                debug!(id = %req.id, path = %task.path, "推送已不在，停止发流");
                break;
            }
            DownloadTarget::NotInManifest => {
                header.clear();
                if stream::write_skip(&mut header, &task.path, SKIP_NOT_IN_MANIFEST)
                    .await
                    .is_err()
                    || tx.send(std::mem::take(&mut header)).await.is_err()
                {
                    return;
                }
                warn!(id = %req.id, path = %task.path, "请求的路径不在清单里");
                continue;
            }
            DownloadTarget::Found { path, cancel, .. } => (path, cancel),
        };
        if cancelled.load(Ordering::Relaxed) {
            debug!(id = %req.id, "推送已作废，停止发流");
            break;
        }

        let mut file = match File::open(&abs).await {
            Ok(f) => f,
            Err(e) => {
                if send_err(tx, &mut header, &task.path, &format!("打开失败: {e}"))
                    .await
                    .is_err()
                {
                    return;
                }
                continue;
            }
        };
        let meta = match file.metadata().await {
            Ok(m) => m,
            Err(e) => {
                if send_err(tx, &mut header, &task.path, &format!("读取元数据失败: {e}"))
                    .await
                    .is_err()
                {
                    return;
                }
                continue;
            }
        };
        if !meta.is_file() {
            if send_err(tx, &mut header, &task.path, "源不是一个普通文件")
                .await
                .is_err()
            {
                return;
            }
            continue;
        }

        let size = meta.len();
        let mtime = stream::mtime_secs(&meta);
        let count = stream::block_count(size, req.block_size);
        let Some((offset, len)) = stream::block_range(size, req.block_size, task.index) else {
            // 对端按旧的大小算出来的块，现在这个文件已经没有那么长了。
            header.clear();
            if stream::write_source_changed(&mut header, &task.path).await.is_err()
                || tx.send(std::mem::take(&mut header)).await.is_err()
            {
                return;
            }
            info!(path = %task.path, index = task.index, size, "块序号越界：源文件变小了");
            continue;
        };

        // `FILE` 记录每条任务都发一次：批次里每条路径各有一份身份，接收端拿它对账
        // （大小/时间戳与清单不符就判这条文件失败）。开销是每条几十字节。
        //
        // `FILE` 与 `BLOCK` 头在同一个缓冲里攒好再发一次：两条都是小记录，合成一次
        // `tx.send` 少一次唤醒，也不会出现"发了 FILE 却在 BLOCK 前被打断"的缝隙。
        header.clear();
        if stream::write_file(&mut header, &task.path, size, mtime, req.block_size, count)
            .await
            .is_err()
            || stream::write_block_header(&mut header, &task.path, task.index, offset, len)
                .await
                .is_err()
            || tx.send(std::mem::take(&mut header)).await.is_err()
        {
            return;
        }

        // 载荷：一块一块读、一块一块发，同时喂哈希。摘要最后补。
        if let Err(e) = file.seek(std::io::SeekFrom::Start(offset)).await {
            warn!(path = %task.path, %e, "seek 失败");
            // 头已经发出去了：这条只能靠"断流"收场（接收端按这次尝试失败处理）。
            return;
        }
        let mut hasher = sha2::Sha256::new();
        use sha2::Digest as _;
        let mut sent: u64 = 0;
        while sent < len as u64 {
            let want = ((len as u64 - sent) as usize).min(READ_CHUNK);
            let got = match file.read(&mut buf[..want]).await {
                Ok(0) => break,
                Ok(n) => n,
                Err(e) => {
                    warn!(path = %task.path, %e, "读源文件失败");
                    return; // 头已发、载荷不完整 → 断流
                }
            };
            hasher.update(&buf[..got]);
            if tx.send(buf[..got].to_vec()).await.is_err() {
                return; // 对端断了
            }
            sent += got as u64;
        }
        if sent != len as u64 {
            // 声明了 len 却读不满 = 文件在读取期间被截短。断流（接收端重试/判失败），
            // 绝不能补个零凑数 —— 那就是把坏字节当好的交出去。
            warn!(path = %task.path, want = len, got = sent, "源文件读不满一块，断流");
            return;
        }
        let digest: [u8; 32] = hasher.finalize().into();
        if tx.send(digest.to_vec()).await.is_err() {
            return;
        }

        // 这一块发完之后源文件变了没有（大小/时间戳）：变了就补一条明确的话，
        // 而不是让接收端拿着一份"半新半旧"的字节当成功。
        if let Ok(now) = file.metadata().await {
            if now.len() != size || stream::mtime_secs(&now) != mtime {
                header.clear();
                let _ = stream::write_source_changed(&mut header, &task.path).await;
                if tx.send(std::mem::take(&mut header)).await.is_err() {
                    return;
                }
                info!(path = %task.path, "源文件在传输期间被修改");
            }
        }
    }

    header.clear();
    if stream::write_end(&mut header).await.is_ok() {
        let _ = tx.send(std::mem::take(&mut header)).await;
    }
}

async fn send_err(
    tx: &Sender<Vec<u8>>,
    header: &mut Vec<u8>,
    path: &str,
    msg: &str,
) -> Result<(), ()> {
    header.clear();
    if stream::write_err(header, path, msg).await.is_err() {
        return Err(());
    }
    tx.send(std::mem::take(header)).await.map_err(|_| ())
}


