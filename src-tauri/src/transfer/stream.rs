//! 推送数据面的**分帧协议**（`POST /push/stream`）：一次请求带走多条任务，响应是一串记录。
//!
//! ## 为什么要另起一条协议
//!
//! 老路子是"一个文件一次 `GET /push/download`，串行"：2 万个文件的目录 = 2 万次串行往返，
//! 每次往返还要在发送端打开文件、解析路径、查凭据 —— 时间全花在**每个文件的固定开销**上，
//! 带宽反而闲着。这条新路子把两件事同时解决：
//!
//! - **一个请求装多条任务**：小文件按批（≤ `BUNDLE_MAX_TASKS` 条 / ≤ `BUNDLE_MAX_BYTES`）合并，
//!   一次往返摊到几百个文件上；
//! - **大文件切成块**：块任务之间可以并行（多个 worker、多条连接），大文件不再独占整条流。
//!
//! ## 记录格式（都是小端）
//!
//! ```text
//! u8 kind
//!   0x01 FILE   : path | size(u64) | mtime(i64) | block_size(u32) | block_count(u32)
//!   0x02 BLOCK  : path | index(u32) | offset(u64) | len(u32) | payload[len] | sha256(32)
//!   0x03 SKIP   : path | reason(u8)
//!   0x04 ERR    : path | msg
//!   0x05 SOURCE_CHANGED : path
//!   0x00 END
//! path / msg := u32 字节长度 + UTF-8 字节
//! ```
//!
//! ## 三条不可动摇的约定
//!
//! 1. **哈希在载荷后面**（trickle 尾随），不是前面：发送端一边读文件一边算，读到多少发多少，
//!    不必先把一整块（4MB）攒在内存里；接收端一边写盘一边算，读完 `len` 字节再读 32 字节
//!    摘要比对。两边都不需要缓冲整块。
//! 2. **块的边界由接收方定**（请求里的 `block_size`），发送端只按 `index` 算
//!    `offset = index * block_size`。接收端对回来的 `offset`/`len` 做**硬校验**
//!    （见 [`Record::BlockHeader`] 的注释）：发过来的东西必须正好落在它能落的那块上。
//! 3. **`FILE` 记录是变更判据**：`size`/`mtime` 与清单不一致 = 源文件在传输期间变了，
//!    接收端据此判这条文件失败，而不是把一份"半新半旧"的字节当成品。
//!
//! 分帧本身不做版本协商：邀请里的 `caps` 里没有 `stream1` 就整体退回老端点
//! （`GET /push/download`），老端点一字未改。

use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

/// 记录类型。
pub const KIND_END: u8 = 0x00;
pub const KIND_FILE: u8 = 0x01;
pub const KIND_BLOCK: u8 = 0x02;
pub const KIND_SKIP: u8 = 0x03;
pub const KIND_ERR: u8 = 0x04;
pub const KIND_SOURCE_CHANGED: u8 = 0x05;

/// `SKIP` 的三种理由。
pub const SKIP_COMPLETE: u8 = 0; // 这文件对端已经有了（offset == size）
pub const SKIP_NOT_IN_MANIFEST: u8 = 1; // 清单里没有这条路径
pub const SKIP_SOURCE_CHANGED: u8 = 2; // 源文件在清单之后变过（大小对不上）

/// 一个块默认多大。
///
/// 4MB：与 `peer_fs::READ_CHUNK`（镜像水合那条路）同一个量级 —— 千兆下一块约 33ms，
/// 取消/失败的可感知延迟就在这个尺度；内存上，发送端一边读一边发（不攒整块），
/// 接收端只按 256KB 的读写缓冲走，所以块大小并不直接等于内存占用。
pub const DEFAULT_BLOCK_SIZE: u32 = 4 * 1024 * 1024;

/// 一个请求里最多装多少条任务（小文件批的上限）。
pub const BUNDLE_MAX_TASKS: usize = 256;
/// 一个请求里最多装多少**预期字节**（小文件批的上限；按清单里的 size 估）。
pub const BUNDLE_MAX_BYTES: u64 = 8 * 1024 * 1024;
/// 小于等于这个大小的文件走"一批一个请求"，比它大的按块走（块内并行才有意义）。
pub const BUNDLE_FILE_MAX: u64 = 1024 * 1024;

/// 一次 `POST /push/stream` 的请求体。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamRequest {
    /// 邀请 id（认人与老端点同一套）。
    pub id: String,
    /// 块的粒度：块序号 = `offset / block_size`。**由接收方定**，发送端照它算/校验。
    pub block_size: u32,
    pub tasks: Vec<StreamTask>,
}

/// 一条任务：文件里的第 `index` 块。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamTask {
    /// 清单里的相对路径（落盘后的样子）。
    pub path: String,
    /// 块序号（小文件永远是 0）。
    pub index: u32,
}

/// 把一个文件的块布局算出来（两边**必须**用同一个算法）。
///
/// `size == 0` 时 `block_count == 0`：空文件没有块，接收端直接建一个空文件就当收完了
/// （不走网络 —— 请求一个 0 字节的块只会平白多一次往返）。
pub fn block_count(size: u64, block_size: u32) -> u32 {
    if size == 0 || block_size == 0 {
        return 0;
    }
    size.div_ceil(block_size as u64) as u32
}

/// 第 `index` 块的 `(offset, len)`。
///
/// `None` = 这个序号根本不存在（越界）—— 接收端拿它校验对端发回来的块头。
pub fn block_range(size: u64, block_size: u32, index: u32) -> Option<(u64, u32)> {
    if block_size == 0 {
        return None;
    }
    let count = block_count(size, block_size);
    if index >= count {
        return None;
    }
    let offset = index as u64 * block_size as u64;
    let len = (size - offset).min(block_size as u64) as u32;
    Some((offset, len))
}

/// 还缺哪些块（位图里没置位的）。**续传的全部逻辑就在这一句里**：队列只排它返回的块，
/// 已经落盘并校验过的那些一块都不会再要。
pub fn missing_blocks(size: u64, block_size: u32, bits: &str) -> Vec<u32> {
    let count = block_count(size, block_size);
    (0..count)
        .filter(|i| !crate::transfer::resume::bitmap_has(bits, *i))
        .collect()
}

/// 这条路径的稳定身份（**不含邀请 id**）：同一台对端的同一个文件，跨邀请、跨会话都是它。
///
/// 续传记录按它存：用户"传失败了再推一次"走的是新邀请、新 invite id，只有身份是稳定的 ——
/// 按 invite id 存的话，重推一次就是从零重传（老实现就是这个毛病）。
///
/// 含 `mtime`：源文件改过（时间戳变了）就不该复用旧记录，否则会拿着旧字节"续"出一个
/// 新旧拼接的文件。
///
/// **含落盘目录**（`dir` = 这个文件真正要写进去的那一层目录）：身份是"**对端的哪个文件 ×
/// 我把它收到哪个目录下**"。少了后半截，同一个文件先后落到两个目录时（用户改了下载目录、
/// "每次询问"下换了保存位置）会共用一个键 —— 按着旧目录的位图去新目录找 `.part`（找不到 →
/// 从零），顺手还把旧记录**覆盖**成空位图（那批已经在盘上的字节谁也认不出来了）、把旧目录
/// 里的 `.part` 变成**没人认领**的垃圾（`sweep_stale` 只清"还有记录指着"的那些）。
/// 并进来之后：两个目录就是两份互不干扰的记录，旧的照旧能接着用、过期了也能被清掉。
pub fn resume_key(peer_id: &str, dir: &Path, path: &str, size: u64, mtime: i64) -> String {
    let mut h = Sha256::new();
    h.update(peer_id.as_bytes());
    h.update([0]);
    h.update(identity_dir(dir).as_bytes());
    h.update([0]);
    h.update(path.as_bytes());
    h.update([0]);
    h.update(size.to_le_bytes());
    h.update(mtime.to_le_bytes());
    let digest = h.finalize();
    // 只留前 16 字节（128 位）：够当身份，短到能进文件名。
    let mut out = String::with_capacity(32);
    for b in &digest[..16] {
        out.push_str(&format!("{b:02x}"));
    }
    out
}

/// 落盘目录进身份时的写法：分隔符统一成 `/`、去掉尾部多余的 `/`；Windows 上再按小写。
///
/// 为什么不能原样拼：同一个目录会有好几种说法（用户在挑目录的框里重挑一次拿到的串、
/// 手改过配置文件的串、Windows 上大小写不同的串）。说法一变键就变 —— 那些**已经落盘的字节**
/// 就白传了。这是"把目录并进身份"这一改的代价，得在这儿兜住。
fn identity_dir(dir: &Path) -> String {
    let mut s = dir.to_string_lossy().replace('\\', "/");
    while s.len() > 1 && s.ends_with('/') {
        s.pop();
    }
    if cfg!(windows) {
        s = s.to_lowercase();
    }
    s
}

/// 由 `resume_key` 派生的临时文件名：同一个目标文件在同一目录下只有一个 `.part`。
pub fn part_name(key: &str) -> String {
    format!("{key}.orbdrop-part")
}

/// `SystemTime` → unix 秒（取不到就 0 = "时间戳未知"）。
///
/// 清单与 `FILE` 记录两边都用它：**必须同一个函数**，否则两边算出来的秒数会在边界上差 1
/// （一边进位、一边截断），成批文件被判成"源已变"。
pub fn mtime_secs(meta: &std::fs::Metadata) -> i64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// 块载荷的摘要（发送端与接收端各算一次，比对）。
pub fn digest(data: &[u8]) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(data);
    h.finalize().into()
}

/// 摘要的比较：**定长比较，不做提前返回**（同 `constant_time_eq` 的道理 —— 这里的对手
/// 不是网络攻击者，但"比较函数写错"是最经典的一类 bug，保持完全相同的写法最省心）。
pub fn digest_eq(a: &[u8; 32], b: &[u8; 32]) -> bool {
    let mut diff = 0u8;
    for i in 0..32 {
        diff |= a[i] ^ b[i];
    }
    diff == 0
}

// ---------------------------------------------------------------------------
// 发送端：写记录
// ---------------------------------------------------------------------------

async fn write_bytes<W: AsyncWrite + Unpin>(w: &mut W, bytes: &[u8]) -> io::Result<()> {
    w.write_all(bytes).await
}

/// `path | msg` 那一段的公共写法：u32 长度 + UTF-8 字节。
async fn write_str<W: AsyncWrite + Unpin>(w: &mut W, s: &str) -> io::Result<()> {
    let bytes = s.as_bytes();
    write_bytes(w, &(bytes.len() as u32).to_le_bytes()).await?;
    write_bytes(w, bytes).await
}

/// `FILE` 记录：这条文件的当前身份（大小/时间戳）+ 块布局。
pub async fn write_file<W: AsyncWrite + Unpin>(
    w: &mut W,
    path: &str,
    size: u64,
    mtime: i64,
    block_size: u32,
    block_count: u32,
) -> io::Result<()> {
    write_bytes(w, &[KIND_FILE]).await?;
    write_str(w, path).await?;
    write_bytes(w, &size.to_le_bytes()).await?;
    write_bytes(w, &mtime.to_le_bytes()).await?;
    write_bytes(w, &block_size.to_le_bytes()).await?;
    write_bytes(w, &block_count.to_le_bytes()).await
}

/// `BLOCK` 记录的头（载荷与摘要由调用方紧接着写）。
pub async fn write_block_header<W: AsyncWrite + Unpin>(
    w: &mut W,
    path: &str,
    index: u32,
    offset: u64,
    len: u32,
) -> io::Result<()> {
    write_bytes(w, &[KIND_BLOCK]).await?;
    write_str(w, path).await?;
    write_bytes(w, &index.to_le_bytes()).await?;
    write_bytes(w, &offset.to_le_bytes()).await?;
    write_bytes(w, &len.to_le_bytes()).await
}

/// `SKIP`：这条文件这次不传，理由见 `SKIP_*` 常量。
pub async fn write_skip<W: AsyncWrite + Unpin>(
    w: &mut W,
    path: &str,
    reason: u8,
) -> io::Result<()> {
    write_bytes(w, &[KIND_SKIP]).await?;
    write_str(w, path).await?;
    write_bytes(w, &[reason]).await
}

/// `ERR`：**单条文件**失败（其余任务照常继续）。整条连接级的问题不用它表达 ——
/// 那种情况直接断流，接收端按"这次尝试失败"重试。
pub async fn write_err<W: AsyncWrite + Unpin>(w: &mut W, path: &str, msg: &str) -> io::Result<()> {
    write_bytes(w, &[KIND_ERR]).await?;
    write_str(w, path).await?;
    write_str(w, msg).await
}

/// `SOURCE_CHANGED`：这条文件的字节这次一条都不发，接收端应当把它判失败。
pub async fn write_source_changed<W: AsyncWrite + Unpin>(w: &mut W, path: &str) -> io::Result<()> {
    write_bytes(w, &[KIND_SOURCE_CHANGED]).await?;
    write_str(w, path).await
}

/// 收尾。**每条响应都必须以它结束**：接收端把"读到 END"当作"这一批任务都交代过了"，
/// 少了它就只能当成断流（= 重试），重试本身没错，但白费一次往返。
pub async fn write_end<W: AsyncWrite + Unpin>(w: &mut W) -> io::Result<()> {
    write_bytes(w, &[KIND_END]).await
}

// ---------------------------------------------------------------------------
// 接收端：读记录
// ---------------------------------------------------------------------------

/// 一条读出来的记录（`BLOCK` 只有头 —— 载荷由调用方按 `len` 读，摘要随后读）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Record {
    File {
        path: String,
        size: u64,
        mtime: i64,
        block_size: u32,
        block_count: u32,
    },
    /// 块头。**接收端必须校验**：`offset % block_size == 0`、
    /// `index == offset / block_size`、`offset + len <= size` —— 三条里任何一条不成立就是
    /// 对端在往我们文件里的别处写字节，直接判这次尝试失败。
    BlockHeader {
        path: String,
        index: u32,
        offset: u64,
        len: u32,
    },
    Skip {
        path: String,
        reason: u8,
    },
    Err {
        path: String,
        message: String,
    },
    SourceChanged {
        path: String,
    },
    End,
}

/// 记录读取器：包一层 `AsyncRead`（响应体 → `StreamReader` → `BufReader`）。
pub struct RecordReader<R> {
    inner: R,
}

impl<R: AsyncRead + Unpin> RecordReader<R> {
    pub fn new(inner: R) -> Self {
        Self { inner }
    }

    /// 读下一条记录。`io::ErrorKind::UnexpectedEof` = 流在记录中间断了（对端收手/网络断了），
    /// 调用方按"这次尝试失败"处理即可 —— 不要在这里把它变成"传完了"。
    pub async fn next(&mut self) -> io::Result<Record> {
        let kind = self.u8().await?;
        match kind {
            KIND_END => Ok(Record::End),
            KIND_FILE => {
                let path = self.string().await?;
                let size = self.u64().await?;
                let mtime = self.i64().await?;
                let block_size = self.u32().await?;
                let block_count = self.u32().await?;
                Ok(Record::File {
                    path,
                    size,
                    mtime,
                    block_size,
                    block_count,
                })
            }
            KIND_BLOCK => {
                let path = self.string().await?;
                let index = self.u32().await?;
                let offset = self.u64().await?;
                let len = self.u32().await?;
                Ok(Record::BlockHeader {
                    path,
                    index,
                    offset,
                    len,
                })
            }
            KIND_SKIP => {
                let path = self.string().await?;
                let reason = self.u8().await?;
                Ok(Record::Skip { path, reason })
            }
            KIND_ERR => {
                let path = self.string().await?;
                let message = self.string().await?;
                Ok(Record::Err { path, message })
            }
            KIND_SOURCE_CHANGED => Ok(Record::SourceChanged {
                path: self.string().await?,
            }),
            other => Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("未知的记录类型: {other:#04x}"),
            )),
        }
    }

    /// 紧跟在块载荷后面的 32 字节摘要。
    pub async fn digest(&mut self) -> io::Result<[u8; 32]> {
        let mut out = [0u8; 32];
        self.inner.read_exact(&mut out).await?;
        Ok(out)
    }

    /// 从流里读满 `buf`（块载荷的搬运就用它）。
    ///
    /// 注：tokio 1.53 起 `read_exact` 返回读到的字节数，这里统一丢掉计数 —— 调用方要的语义
    /// 就是"恰好填满 buf，否则报错"。
    pub async fn read_exact(&mut self, buf: &mut [u8]) -> io::Result<()> {
        self.inner.read_exact(buf).await.map(|_| ())
    }

    async fn u8(&mut self) -> io::Result<u8> {
        let mut b = [0u8; 1];
        self.inner.read_exact(&mut b).await?;
        Ok(b[0])
    }

    async fn u32(&mut self) -> io::Result<u32> {
        let mut b = [0u8; 4];
        self.inner.read_exact(&mut b).await?;
        Ok(u32::from_le_bytes(b))
    }

    async fn u64(&mut self) -> io::Result<u64> {
        let mut b = [0u8; 8];
        self.inner.read_exact(&mut b).await?;
        Ok(u64::from_le_bytes(b))
    }

    async fn i64(&mut self) -> io::Result<i64> {
        let mut b = [0u8; 8];
        self.inner.read_exact(&mut b).await?;
        Ok(i64::from_le_bytes(b))
    }

    /// 带长度前缀的字符串。**上限是硬约束**（`MAX_STRING`）：长度字段来自网络，不设上限
    /// 就等于让对端一句话让我们分配任意大小的内存。
    async fn string(&mut self) -> io::Result<String> {
        const MAX_STRING: u32 = 64 * 1024;
        let len = self.u32().await?;
        if len > MAX_STRING {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("字符串字段过长: {len}"),
            ));
        }
        let mut buf = vec![0u8; len as usize];
        self.inner.read_exact(&mut buf).await?;
        Ok(String::from_utf8_lossy(&buf).into_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn block_math_and_layout() {
        assert_eq!(block_count(0, 1024), 0, "空文件没有块");
        assert_eq!(block_count(1, 1024), 1);
        assert_eq!(block_count(1024, 1024), 1, "正好一整块还是一块");
        assert_eq!(block_count(1025, 1024), 2);

        assert_eq!(block_range(1025, 1024, 0), Some((0, 1024)));
        assert_eq!(block_range(1025, 1024, 1), Some((1024, 1)));
        assert_eq!(block_range(1025, 1024, 2), None, "越界块没有区间");
        assert_eq!(block_range(0, 1024, 0), None);
    }

    #[test]
    fn missing_blocks_skips_what_is_already_there() {
        let size = DEFAULT_BLOCK_SIZE as u64 * 4;
        let count = block_count(size, DEFAULT_BLOCK_SIZE);
        assert_eq!(count, 4);
        assert_eq!(
            missing_blocks(size, DEFAULT_BLOCK_SIZE, ""),
            vec![0, 1, 2, 3],
            "空位图 = 全都要"
        );

        let mut bits = String::new();
        crate::transfer::resume::bitmap_set(&mut bits, 0);
        crate::transfer::resume::bitmap_set(&mut bits, 2);
        assert_eq!(
            missing_blocks(size, DEFAULT_BLOCK_SIZE, &bits),
            vec![1, 3],
            "已置位的块不再要（这才是续传省钱的地方）"
        );

        for i in 0..count {
            crate::transfer::resume::bitmap_set(&mut bits, i);
        }
        assert!(
            missing_blocks(size, DEFAULT_BLOCK_SIZE, &bits).is_empty(),
            "全齐了就不再排队"
        );
    }

    #[test]
    fn resume_key_is_stable_and_identity_sensitive() {
        let dir = Path::new("C:/Users/u/Downloads/PC-A");
        let a = resume_key("peer", dir, "D/x.txt", 10, 5);
        assert_eq!(
            a,
            resume_key("peer", dir, "D/x.txt", 10, 5),
            "同一身份 → 同一个键"
        );
        assert_ne!(
            a,
            resume_key("peer", dir, "D/x.txt", 11, 5),
            "大小变了 → 换键"
        );
        assert_ne!(
            a,
            resume_key("peer", dir, "D/x.txt", 10, 6),
            "时间戳变了 → 换键"
        );
        assert_ne!(
            a,
            resume_key("other", dir, "D/x.txt", 10, 5),
            "换了一台对端 → 换键"
        );
        // 落盘目录也是身份的一半：同一个文件收在两个目录下，两份记录各算各的
        // （共用的话，按着 A 的位图去 B 找 `.part` 只会从零，还顺手砸掉 A 的记录）。
        assert_ne!(
            a,
            resume_key("peer", Path::new("D:/另一块盘/PC-A"), "D/x.txt", 10, 5),
            "落盘目录变了 → 换键"
        );
        // 同一个目录的几种说法必须认成同一个 —— 说法一变就白传（见 `identity_dir`）。
        assert_eq!(
            a,
            resume_key(
                "peer",
                Path::new("C:/Users/u/Downloads/PC-A/"),
                "D/x.txt",
                10,
                5
            )
        );
        assert_eq!(
            a,
            resume_key(
                "peer",
                Path::new("C:\\Users\\u\\Downloads\\PC-A"),
                "D/x.txt",
                10,
                5
            ),
            "反斜杠/正斜杠是同一个目录"
        );
        if cfg!(windows) {
            assert_eq!(
                a,
                resume_key(
                    "peer",
                    Path::new("c:/users/u/downloads/pc-a"),
                    "D/x.txt",
                    10,
                    5
                ),
                "Windows 上大小写不同是同一个目录"
            );
        }
        assert_eq!(a.len(), 32, "16 字节的 hex");
        assert!(part_name(&a).ends_with(".orbdrop-part"));
    }

    #[tokio::test]
    async fn records_round_trip() {
        let mut wire: Vec<u8> = Vec::new();
        write_file(&mut wire, "a/文 件.txt", 4096, -7, DEFAULT_BLOCK_SIZE, 1)
            .await
            .unwrap();
        write_block_header(&mut wire, "a/文 件.txt", 0, 0, 4096)
            .await
            .unwrap();
        wire.extend_from_slice(&[7u8; 4096]); // 载荷
        wire.extend_from_slice(&[9u8; 32]); // 尾随摘要
        write_skip(&mut wire, "b.txt", SKIP_COMPLETE).await.unwrap();
        write_err(&mut wire, "c.txt", "打不开").await.unwrap();
        write_source_changed(&mut wire, "d.txt").await.unwrap();
        write_end(&mut wire).await.unwrap();

        let mut r = RecordReader::new(&wire[..]);
        assert_eq!(
            r.next().await.unwrap(),
            Record::File {
                path: "a/文 件.txt".into(),
                size: 4096,
                mtime: -7,
                block_size: DEFAULT_BLOCK_SIZE,
                block_count: 1,
            }
        );
        assert_eq!(
            r.next().await.unwrap(),
            Record::BlockHeader {
                path: "a/文 件.txt".into(),
                index: 0,
                offset: 0,
                len: 4096,
            }
        );
        // 载荷由调用方搬，摘要在后。
        let mut payload = vec![0u8; 4096];
        r.read_exact(&mut payload).await.unwrap();
        assert_eq!(payload, vec![7u8; 4096]);
        assert_eq!(r.digest().await.unwrap(), [9u8; 32]);
        assert_eq!(
            r.next().await.unwrap(),
            Record::Skip {
                path: "b.txt".into(),
                reason: SKIP_COMPLETE
            }
        );
        assert_eq!(
            r.next().await.unwrap(),
            Record::Err {
                path: "c.txt".into(),
                message: "打不开".into()
            }
        );
        assert_eq!(
            r.next().await.unwrap(),
            Record::SourceChanged {
                path: "d.txt".into()
            }
        );
        assert_eq!(r.next().await.unwrap(), Record::End);
    }

    #[tokio::test]
    async fn truncated_stream_is_unexpected_eof_not_end() {
        let mut wire: Vec<u8> = Vec::new();
        write_block_header(&mut wire, "x.txt", 0, 0, 8)
            .await
            .unwrap();
        // 载荷没写全就断了。
        wire.extend_from_slice(&[1, 2, 3]);
        let mut r = RecordReader::new(&wire[..]);
        r.next().await.unwrap();
        let mut payload = vec![0u8; 8];
        let err = r.read_exact(&mut payload).await.unwrap_err();
        assert_eq!(
            err.kind(),
            io::ErrorKind::UnexpectedEof,
            "断在中间 = 重试，不是传完"
        );
    }

    #[tokio::test]
    async fn unknown_record_and_giant_string_are_rejected() {
        let mut r = RecordReader::new(&[0x7fu8][..]);
        assert!(r.next().await.is_err(), "未知记录类型要报错");

        let mut wire = vec![KIND_ERR];
        wire.extend_from_slice(&(u32::MAX).to_le_bytes());
        let mut r = RecordReader::new(&wire[..]);
        let err = r.next().await.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::InvalidData, "长度字段不能无穷大");
    }

    #[test]
    fn digest_helpers() {
        let d = digest(b"hello");
        assert!(digest_eq(&d, &digest(b"hello")));
        assert!(!digest_eq(&d, &digest(b"hellp")));
    }
}
