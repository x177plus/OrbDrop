//! 拖拽下载：把共享项里的任意文件/目录**拖到资源管理器/Finder 里**，落在拖到的那个目录。
//!
//! 网页里的 HTML5 拖拽出不了 webview —— 目的地是另一个进程，只有 OS 的原生拖放能跨过去。
//! 所以这里自己当 OLE 拖放源：`DoDragDrop`（见 `imp`）。
//!
//! 三种内容，各走各的路：
//!
//! - **本机共享项**：磁盘上本来就有这些文件，用 `CF_HDROP` 把真实路径交给资源管理器。
//!   只给 `DROPEFFECT_COPY`（外加 `Preferred DropEffect` 这道保险）—— 同一块盘内拖拽的
//!   默认动作是"移动"，不写死复制会把用户的源文件搬走。
//! - **对端项·小**（单文件 ≤ 100 MB、目录总量 ≤ 500 MB）：起拖前整份下到暂存目录
//!   （`<数据目录>/staging/<uuid>`），也走 `CF_HDROP` 真实路径。**为什么值得多下一次**：
//!   虚拟文件是 shell 协议，只有资源管理器 / Chromium 系 / QQ 这一小撮实现，而只认
//!   `CF_HDROP` 的应用（微信这类）根本不问 `FileContents` —— 真实路径才通吃。代价是起拖前
//!   要等内容下完（拖放循环跟着按住的主键走），所以用上限兜住等待时间。
//! - **对端项·大**：本机不落副本，用 OLE 的**虚拟文件**（`CFSTR_FILEDESCRIPTOR` +
//!   `CFSTR_FILECONTENTS` + 自定义 `IStream`）：资源管理器在落下时直接往目标目录写，字节由
//!   我们的流按需从对端拉（`GET /file?offset=&len=`）—— 拖一个 10GB 的目录也是秒起拖。
//!
//! 起拖前一律先备齐：本机项当场就有；对端项走一遍 `/tree` + `/stat`（把名字/大小/时间填进
//! 描述符），小项外加转存。
//!
//! 时序：界面按下鼠标移动出阈值就 `prepare`（登记票据，对端项同时开始走目录/转存），
//! 随后 `start` 等备齐再起拖 —— 拖放循环要跟着按住的主键走，所以主键还按着才起拖。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};
use tracing::{info, warn};
use uuid::Uuid;

use crate::credential;
use crate::state::AppState;
use crate::task;

/// 等元数据备齐的上限（只走 `/tree` + `/stat`，不拉内容；这条线是防挂死）。
const PREPARE_TIMEOUT: Duration = Duration::from_secs(120);
/// 票据在内存里留一会儿：界面万一起拖失败（手松早了），接着再拖一次还能命中同一张。
const TICKET_TTL: Duration = Duration::from_secs(600);

/// 拖放诊断的日志 target。从"界面请求起拖"到"目标问了什么格式"整条链都归它，
/// `RUST_LOG=orbdrop::dragprobe=off` 可关；分析完可整节删掉。
const PROBE: &str = "orbdrop::dragprobe";

// ---------------------------------------------------------------------------
// 提前转存：把"只认真实路径"的应用（微信这类从不问虚拟文件格式的）也覆盖进来
//
// 拖的是对端项时，若这一项**小到能兜住起拖前的等待**，就先下到暂存目录、给 `CF_HDROP`
// 真实路径；超过上限就退回虚拟文件（秒起拖、不落暂存盘，但只有支持虚拟文件的目标能收）。
// 上限是**等待时间**的换算：起拖必须等下载完（拖放循环跟着按住的主键走）。
// ---------------------------------------------------------------------------

/// 拖出的**单个文件**的上限。
const STAGE_MAX_FILE: u64 = 100 * 1024 * 1024;
/// 拖出的**目录**整棵树的总字节上限。
const STAGE_MAX_TOTAL: u64 = 500 * 1024 * 1024;
/// 暂存目录留多久：`CF_HDROP` 落下之后资源管理器可能**异步**复制（进度框一出来它就返回了），
/// 一起拖完就删会把还没拷完的东西抽走。留够时间，靠下次拖拽（与启动）按龄清扫。
const STAGE_TTL: Duration = Duration::from_secs(6 * 60 * 60);

/// 单次向对端要多少字节。资源管理器可能要一整块（比如 100MB），流允许"少给一点"，
/// 所以切成小块，内存不会被文件大小牵着走；提前转存也按这个粒度落盘。
const READ_CHUNK: u64 = 4 * 1024 * 1024;

/// 一次拖拽的源：本机共享项（`peer_id` 为空）或对端共享项。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DragOutRequest {
    pub peer_id: Option<String>,
    pub share_id: String,
    /// 共享项内相对路径（'' = 共享项根）
    pub path: String,
    /// 拖出去的那一项叫什么。对端共享项根的名字本机不知道（只有 id），只能由界面带下来
    /// —— 和 BC 比较位的 label 同一个理由。
    pub name: String,
    pub is_dir: bool,
}

/// 备好的拖拽票据：界面拿 `ticket` 去 `start`。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct DragTicket {
    pub ticket: String,
    pub name: String,
    /// 元数据已经备齐，可以起拖了。本机共享项恒为 true；对端项要先走一遍目录。
    pub ready: bool,
}

/// 拖出去的内容。
#[derive(Debug, Clone)]
pub enum Payload {
    /// 磁盘上的真实路径，交给资源管理器直接复制（`CF_HDROP`）：
    /// 本机共享项本来就是；**对端项是提前转存到暂存目录的那一份**（见 `stage_peer_item`）。
    /// 只认路径的应用（微信这类从不问虚拟文件的）只有这条路能收到东西。
    RealPaths(Vec<PathBuf>),
    /// 对端共享项：虚拟文件（落下时按需拉字节）。
    Virtual(Vec<VirtualItem>),
}

/// 一个虚拟项。`name` 是**相对落点**的路径（`\` 分隔），目录项也要列出来 ——
/// 拖一个目录出去时，目录本身、子目录、文件都是描述符里的一条。
#[derive(Debug, Clone)]
pub struct VirtualItem {
    pub name: String,
    pub is_dir: bool,
    pub size: u64,
    /// 大小是不是真的知道。对端是老版本、`/stat` 与"列父目录"两路都失败时是 `false`，
    /// 那时 `size` 记 0 —— "0 字节"和"不知道"必须分得开：提前转存要按大小决定要不要做，
    /// 照一个未知大小去下，可能等来几 GB。
    pub size_known: bool,
    /// 修改时间（Unix 秒；拿不到就是 None，那时不写 FD_WRITESTIME）。
    pub mtime: Option<i64>,
    /// 内容从哪来（目录为 None）。
    pub source: Option<PeerFile>,
}

/// 一个远端文件在服务端的坐标。
#[derive(Debug, Clone)]
pub struct PeerFile {
    /// `http://ip:port`
    pub base: String,
    pub token: String,
    pub share_id: String,
    /// 共享项内相对路径（`/` 分隔）
    pub path: String,
}

enum Staged {
    /// 正在走对端目录（只取元数据）。
    Preparing,
    /// 可以起拖了。
    Ready(Payload),
    Failed(String),
}

struct Entry {
    at: Instant,
    staged: Staged,
}

/// 拖拽票据表。放在 `AppState` 里，跟着进程活。
#[derive(Default)]
pub struct DragStore {
    entries: parking_lot::Mutex<HashMap<String, Entry>>,
}

impl DragStore {
    pub fn new() -> Self {
        Self::default()
    }

    /// 登记一次拖拽意图并返回票据。本机共享项当场可拖；对端共享项先在后台走一遍目录
    /// （`/tree` + `/stat`，只取名字/大小/时间）。
    pub fn prepare(&self, state: &Arc<AppState>, req: &DragOutRequest) -> Result<DragTicket, String> {
        let req = req.clone();
        let name = req.name.clone();
        // 顺手清掉过期暂存（不能拖完就删：资源管理器可能还在异步复制 `CF_HDROP`）。
        sweep_staging();
        {
            let mut entries = self.entries.lock();
            let expired: Vec<String> = entries
                .iter()
                .filter(|(_, e)| e.at.elapsed() >= TICKET_TTL)
                .map(|(t, _)| t.clone())
                .collect();
            for t in expired {
                entries.remove(&t);
            }
        }

        match req.peer_id.clone() {
            Some(peer_id) => {
                let ticket = Uuid::new_v4().to_string();
                self.entries
                    .lock()
                    .insert(ticket.clone(), Entry { at: Instant::now(), staged: Staged::Preparing });
                let (task_state, task_req, task_peer, id) =
                    (state.clone(), req.clone(), peer_id.clone(), ticket.clone());
                task::spawn("drag prepare", async move {
                    let result = prepare_peer_payload(&task_state, &task_req, &task_peer).await;
                    match &result {
                        Ok(Payload::Virtual(items)) => {
                            info!(ticket = %id, items = items.len(), "拖拽元数据已备好（虚拟文件）")
                        }
                        Ok(Payload::RealPaths(paths)) => {
                            info!(ticket = %id, paths = paths.len(), "拖拽内容已备好（提前转存）")
                        }
                        Err(e) => warn!(ticket = %id, error = %e, "拖拽内容准备失败"),
                    }
                    task_state.drag.finish(&id, result);
                });
                Ok(DragTicket { ticket, name: req.name, ready: false })
            }
            None => {
                let path = local_share_path(state, &req.share_id, &req.path)?;
                let ticket = Uuid::new_v4().to_string();
                self.entries.lock().insert(
                    ticket.clone(),
                    Entry {
                        at: Instant::now(),
                        staged: Staged::Ready(Payload::RealPaths(vec![path])),
                    },
                );
                Ok(DragTicket { ticket, name, ready: true })
            }
        }
    }

    /// 票据此刻的内容：`Ok(Some)` 可以起拖了，`Ok(None)` 还在走目录，`Err` 不用等了。
    pub fn payload(&self, ticket: &str) -> Result<Option<Payload>, String> {
        match self.entries.lock().get(ticket) {
            Some(Entry { staged: Staged::Ready(payload), .. }) => Ok(Some(payload.clone())),
            Some(Entry { staged: Staged::Failed(msg), .. }) => Err(msg.clone()),
            Some(_) => Ok(None),
            None => Err("这次拖拽已失效，请重新拖一次".into()),
        }
    }

    /// 后台准备（走目录 + 可能的提前转存）的收尾。
    fn finish(&self, ticket: &str, result: Result<Payload, String>) {
        if let Some(entry) = self.entries.lock().get_mut(ticket) {
            entry.staged = match result {
                Ok(payload) => Staged::Ready(payload),
                Err(msg) => Staged::Failed(msg),
            };
            entry.at = Instant::now();
        }
    }

    /// 拖完就把票据丢了：内容是按需拉的，留着也没用。
    fn forget(&self, ticket: &str) {
        self.entries.lock().remove(ticket);
    }
}

/// 等元数据备齐，然后在**主线程**上起 OLE 拖放。
///
/// `DoDragDrop` 自带模态循环，会一直阻塞到拖放结束（原生应用就是这么干的），所以必须跑在
/// 窗口所在线程上；对端项的目录遍历是异步的，先在 tokio 线程上等完再切过去。
pub async fn start(app: AppHandle, state: Arc<AppState>, ticket: String) -> Result<(), String> {
    // 记一行"界面确实发起了起拖"：以后某次拖拽整个日志里什么都没有时，这一行在不在
    // 就是"手势没触发"与"起拖被吃掉"的分界。
    tracing::info!(target: PROBE, ticket = %ticket, "拖放：界面请求起拖");
    let payload = wait_ready(&state, &ticket).await;
    let result = match payload {
        Err(e) => Err(e),
        Ok(payload) => {
            // 主键已经松开就别起拖了：拖放循环跟着按住的主键走，抬起状态下起拖会在当前位置
            // 立刻"落下"（可能正好落在面板自己身上）。内容留着，用户再拖一次即可。
            #[cfg(windows)]
            if !crate::orb_window::mouse_down() {
                state.drag.forget(&ticket);
                tracing::info!(target: PROBE, ticket = %ticket, "拖放：起拖前主键已松开，放弃本次");
                return Err("鼠标已松开：按住再拖一次即可".into());
            }
            let handle = tokio::runtime::Handle::current();
            // 起拖前把窗口带到前台（见 `orb_window::ensure_foreground`）：`DoDragDrop` 同样只在
            // 窗口是前台窗口时才真正吃住指针，而"点击激活"是异步的。
            if let Some(win) = app.get_webview_window("main") {
                let _ = crate::orb_window::ensure_foreground(&win);
            }
            let (tx, rx) = tokio::sync::oneshot::channel();
            app.run_on_main_thread(move || {
                let _ = tx.send(imp::drag(&payload, handle));
            })
            .map_err(|e| format!("无法进入主线程起拖: {e}"))?;
            rx.await.map_err(|_| "拖拽线程未响应".to_string())?
        }
    };
    state.drag.forget(&ticket);
    match &result {
        Ok(()) => tracing::info!(target: PROBE, ticket = %ticket, "拖放：本次起拖流程结束"),
        Err(e) => tracing::info!(target: PROBE, ticket = %ticket, error = %e, "拖放：起拖失败"),
    }
    result
}

async fn wait_ready(state: &Arc<AppState>, ticket: &str) -> Result<Payload, String> {
    let deadline = Instant::now() + PREPARE_TIMEOUT;
    loop {
        if let Some(payload) = state.drag.payload(ticket)? {
            return Ok(payload);
        }
        if Instant::now() >= deadline {
            return Err("准备超时：对端太慢或已离线（小项要下到暂存目录，大项只走元数据）".into());
        }
        tokio::time::sleep(Duration::from_millis(120)).await;
    }
}

/// 本机共享项的相对路径 → 磁盘上的绝对路径（越界防护与 HTTP 那侧同一套判据）。
fn local_share_path(state: &Arc<AppState>, share_id: &str, path_in_share: &str) -> Result<PathBuf, String> {
    let root = {
        let cfg = state.config.lock();
        cfg.shares
            .iter()
            .find(|s| s.id == share_id)
            .map(|s| s.path.clone())
            .ok_or_else(|| format!("共享项不存在: {share_id}"))?
    };
    let joined = if path_in_share.is_empty() {
        root.clone()
    } else {
        root.join(path_in_share.replace('/', std::path::MAIN_SEPARATOR_STR))
    };
    let canonical = joined.canonicalize().map_err(|e| format!("路径不可访问: {e}"))?;
    let canonical_root = root.canonicalize().map_err(|e| format!("共享项已不可访问: {e}"))?;
    if !canonical.starts_with(&canonical_root) {
        return Err("路径越界".into());
    }
    Ok(plain_path(&canonical))
}

// ---------------------------------------------------------------------------
// 对端共享项：只走元数据，内容等落下时再说
// ---------------------------------------------------------------------------

/// 走一遍对端的那一项，产出虚拟文件清单（顺序：父目录在前，资源管理器按这个顺序建目录）。
async fn walk_peer_item(
    state: &Arc<AppState>,
    req: &DragOutRequest,
    peer_id: &str,
    client: &reqwest::Client,
) -> Result<Vec<VirtualItem>, String> {
    let peer = state.get_peer(peer_id).ok_or_else(|| "设备不在列表中".to_string())?;
    let token = credential::get_peer_token(peer_id)
        .map_err(|e| e.to_string())?
        .ok_or_else(|| "未配对，无法读取对方的共享项".to_string())?;
    let base = format!("http://{}:{}", peer.ip, peer.port);

    let source = |path: String| PeerFile {
        base: base.clone(),
        token: token.clone(),
        share_id: req.share_id.clone(),
        path,
    };
    let root_name = plain_component(&req.name).unwrap_or_else(|| "共享项".into());

    // 先问对端"这一项到底是什么"：前端带来的 `is_dir` 是**别处**的判断（对端 `/shares`
    // 的 isDir、或树里那条记录），可能过时/猜错 —— 文件被当成目录走，用户只会看到一句
    // "Path is not a directory"。
    let meta = probe_peer_entry(client, &base, &token, &req.share_id, &req.path).await?;
    let is_dir = meta.map(|m| m.is_dir).unwrap_or(req.is_dir);

    if !is_dir {
        return Ok(vec![single_file_item(root_name, req.path.clone(), meta, source(req.path.clone()))]);
    }

    // 目录：自己那条先列上，然后逐层递归（`\` 是描述符里的分隔符）
    let mut items = vec![VirtualItem {
        name: root_name.clone(),
        is_dir: true,
        size: 0,
        size_known: true,
        mtime: None,
        source: None,
    }];
    let mut stack = vec![(req.path.clone(), root_name.clone())];
    while let Some((dir_path, dir_rel)) = stack.pop() {
        let entries = match fetch_tree(client, &base, &token, &req.share_id, &dir_path).await {
            Ok(entries) => entries,
            // 对端说它不是目录（前端/对端把它当目录了）：退化成"拖这一个文件"，
            // 别把用户卡在一句错误上。
            Err(e) if e.contains(NOT_A_DIRECTORY) => {
                warn!(path = %req.path, "对端说这不是目录，按单个文件拖");
                return Ok(vec![single_file_item(
                    root_name,
                    req.path.clone(),
                    meta,
                    source(req.path.clone()),
                )]);
            }
            Err(e) => return Err(e),
        };
        for entry in entries {
            let leaf = plain_component(&entry.name).unwrap_or_else(|| "_".into());
            let child_rel = format!("{dir_rel}\\{leaf}");
            let child_path = join_slash(&dir_path, &leaf);
            if entry.is_dir {
                items.push(VirtualItem {
                    name: child_rel.clone(),
                    is_dir: true,
                    size: 0,
                    size_known: true,
                    mtime: None,
                    source: None,
                });
                stack.push((child_path, child_rel));
            } else {
                items.push(VirtualItem {
                    name: child_rel,
                    is_dir: false,
                    size: entry.size,
                    size_known: true,
                    mtime: parse_rfc3339_secs(&entry.modified),
                    source: Some(source(child_path)),
                });
            }
        }
    }
    Ok(items)
}

/// 对端 `/tree` 回 400 = "这不是目录"（区别于"共享项不可见/不存在"的 404）。
///
/// 用它把"走目录走错了"识别出来，好退化成"拖这一个文件"而不是把用户卡住。
const NOT_A_DIRECTORY: &str = "对端说它不是目录";

// ---------------------------------------------------------------------------
// 提前转存：小项先下到暂存目录，按 `CF_HDROP` 真实路径拖出去
// ---------------------------------------------------------------------------

/// 问对端用的 HTTP 客户端（起拖前的元数据与提前转存共用）。
fn peer_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(Duration::from_secs(5))
        .timeout(Duration::from_secs(30))
        .build()
        .unwrap_or_default()
}

/// 这一项值不值得提前转存（上限与理由见文件头那一节）。
///
/// 大小**未知**的一律不转存：那时 `size` 记的是 0，"0 字节"和"不知道"分不开，
/// 照它判等于赌一把可能几 GB 的下载。
fn staging_eligible(items: &[VirtualItem]) -> bool {
    if items.iter().any(|i| !i.is_dir && !i.size_known) {
        return false;
    }
    let files: Vec<&VirtualItem> = items.iter().filter(|i| !i.is_dir).collect();
    if files.is_empty() {
        return false; // 空目录：没有内容要转存
    }
    if items.len() == 1 {
        // 拖的就是单个文件
        return files[0].size <= STAGE_MAX_FILE;
    }
    files.iter().map(|i| i.size).sum::<u64>() <= STAGE_MAX_TOTAL
}

/// 本次拖拽的暂存目录：`<数据目录>/staging/<uuid>`；条目名在它里面还原。
fn staging_dir() -> PathBuf {
    crate::config::data_dir().join("staging").join(Uuid::new_v4().to_string())
}

/// 把对端这一项整份下到暂存目录，返回**交给资源管理器的那个真实路径**。
///
/// `items` 的顺序是"父目录在前"，第一条就是拖出去的那一项本身；它的 `name` 是相对暂存根的
/// 路径（`\` 分隔），逐条还原成目录/文件即可。
async fn stage_peer_item(
    client: &reqwest::Client,
    dest_root: &Path,
    items: &[VirtualItem],
) -> Result<PathBuf, String> {
    tokio::fs::create_dir_all(dest_root)
        .await
        .map_err(|e| format!("建暂存目录失败: {e}"))?;

    for item in items {
        let rel = item.name.replace('\\', std::path::MAIN_SEPARATOR_STR);
        let dest = dest_root.join(&rel);
        if item.is_dir {
            tokio::fs::create_dir_all(&dest)
                .await
                .map_err(|e| format!("建暂存子目录失败（{rel}）: {e}"))?;
            continue;
        }
        if let Some(parent) = dest.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| format!("建暂存子目录失败（{rel}）: {e}"))?;
        }
        let source = item
            .source
            .as_ref()
            .ok_or_else(|| format!("「{rel}」没有内容来源"))?;
        download_peer_file(client, source, &dest).await?;

        // 字节数和元数据对不上 = 对端那边这一项变了或中途被截断。
        // 静默交出一份残缺的副本，比退回虚拟文件糟得多（用户以为拿全了）。
        if item.size_known {
            let got = tokio::fs::metadata(&dest).await.map(|m| m.len()).unwrap_or(0);
            if got != item.size {
                return Err(format!("「{rel}」下到的字节数不对：{got} ≠ {}", item.size));
            }
        }
    }

    let root_rel = items[0].name.replace('\\', std::path::MAIN_SEPARATOR_STR);
    Ok(dest_root.join(root_rel))
}

/// 把一个对端文件整份下到 `dest`：按 `READ_CHUNK` 分块，内存不随文件大小走。
async fn download_peer_file(client: &reqwest::Client, file: &PeerFile, dest: &Path) -> Result<(), String> {
    use tokio::io::AsyncWriteExt;

    let mut out = tokio::fs::File::create(dest)
        .await
        .map_err(|e| format!("建暂存文件失败: {e}"))?;
    let mut offset = 0u64;
    loop {
        let chunk = fetch_file_range(client, file, offset, READ_CHUNK).await?;
        if chunk.is_empty() {
            break;
        }
        out.write_all(&chunk)
            .await
            .map_err(|e| format!("写暂存文件失败: {e}"))?;
        offset += chunk.len() as u64;
        // 服务端按 `len` 截断：拿回来的比要的少 = 已经是文件末尾。
        if (chunk.len() as u64) < READ_CHUNK {
            break;
        }
    }
    out.flush().await.map_err(|e| format!("落盘失败: {e}"))
}

/// 清掉过期的暂存目录（按目录 mtime）。
///
/// **不能"拖完就删"**：`CF_HDROP` 落下后资源管理器可能仍在**异步**复制（进度框一出来
/// `DoDragDrop` 就返回了），删早了会把还没拷完的东西抽走。留 `STAGE_TTL`，下次拖拽与启动时扫。
pub fn sweep_staging() {
    let root = crate::config::data_dir().join("staging");
    let Ok(entries) = std::fs::read_dir(&root) else {
        return;
    };
    let now = std::time::SystemTime::now();
    for entry in entries.flatten() {
        let age = entry
            .metadata()
            .and_then(|m| m.modified())
            .ok()
            .and_then(|t| now.duration_since(t).ok());
        if age.is_some_and(|age| age >= STAGE_TTL) {
            let _ = std::fs::remove_dir_all(entry.path());
        }
    }
}

/// 对端项的准备：先只走元数据；**小到能兜住**的顺手下到暂存目录（真实路径，只认路径的
/// 应用才收得到），其余退回虚拟文件（秒起拖、不落暂存盘）。
async fn prepare_peer_payload(
    state: &Arc<AppState>,
    req: &DragOutRequest,
    peer_id: &str,
) -> Result<Payload, String> {
    let client = peer_client();
    let items = walk_peer_item(state, req, peer_id, &client).await?;
    // 非 Windows 平台还没有原生拖放源（`imp::drag` 直接报错）：别白下一趟。
    if !cfg!(windows) || !staging_eligible(&items) {
        return Ok(Payload::Virtual(items));
    }

    let dest = staging_dir();
    match stage_peer_item(&client, &dest, &items).await {
        Ok(path) => {
            info!(items = items.len(), "提前转存完成，按真实路径拖出");
            Ok(Payload::RealPaths(vec![path]))
        }
        Err(e) => {
            // 转存失败（对端掉线、写盘失败）不该让整次拖拽作废：退回虚拟文件，
            // 支持虚拟文件的目标（资源管理器 / Chromium / QQ）照旧能用。
            warn!(error = %e, "提前转存失败，退回虚拟文件");
            let _ = tokio::fs::remove_dir_all(&dest).await;
            Ok(Payload::Virtual(items))
        }
    }
}

/// 对端一项的元数据（`/stat` 或列父目录得来的都归到这里）。
#[derive(Debug, Clone, Copy)]
struct EntryMeta {
    is_dir: bool,
    size: u64,
    mtime: Option<i64>,
}

/// 问对端"这一项是什么、多大、什么时候改的"。
///
/// 优先 `/stat`（对端是新版本时一次请求搞定）；对端是老版本（没有 `/stat`）就退回
/// **列父目录**再挑出那一条 —— `/tree` 哪个版本都有。两条都不行返回 `None`，
/// 调用方按"大小未知"处理（描述符里不写 FD_FILESIZE，内容照旧读到末尾为止）。
async fn probe_peer_entry(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    share_id: &str,
    path: &str,
) -> Result<Option<EntryMeta>, String> {
    let stat_err = match fetch_stat(client, base, token, share_id, path).await {
        Ok(stat) => {
            return Ok(Some(EntryMeta {
                is_dir: stat.is_dir,
                size: stat.size,
                mtime: parse_rfc3339_secs(&stat.modified),
            }))
        }
        Err(e) => e,
    };
    let (parent, leaf) = match path.rsplit_once('/') {
        Some((parent, leaf)) => (parent, leaf),
        None => ("", path),
    };
    if leaf.is_empty() {
        // 共享项根：没有"父目录"可列。对端是老版本（没有 /stat）时只能按调用方的判断走，
        // 大小未知 —— 描述符里不写 FD_FILESIZE，内容照样读到流末尾为止。
        warn!(%stat_err, "问不到共享项根的元数据，按调用方的判断走（大小未知）");
        return Ok(None);
    }
    // 退回"列父目录再挑出那一条"（`/tree` 哪个版本都有）。挑不到就说明这一项压根不在，
    // 这时候必须报错 —— 比如越界路径（`../x`）：不能让拖拽"看着成功了"，到最后复制才失败。
    let entries = fetch_tree(client, base, token, share_id, parent)
        .await
        .map_err(|e| format!("{stat_err}；退回列父目录也失败：{e}"))?;
    match entries.iter().find(|e| e.name == leaf) {
        Some(entry) => Ok(Some(EntryMeta {
            is_dir: entry.is_dir,
            size: entry.size,
            mtime: parse_rfc3339_secs(&entry.modified),
        })),
        None => Err(format!("对端没有这一项：{path}")),
    }
}

/// 清单里"一个文件"那一条。`meta` 拿不到就不带大小（资源管理器照样读到流末尾）。
fn single_file_item(root_name: String, path: String, meta: Option<EntryMeta>, source: PeerFile) -> VirtualItem {
    let _ = path;
    VirtualItem {
        name: root_name,
        is_dir: false,
        size: meta.map(|m| m.size).unwrap_or(0),
        size_known: meta.is_some(),
        mtime: meta.and_then(|m| m.mtime),
        source: Some(source),
    }
}

fn join_slash(base: &str, name: &str) -> String {
    if base.is_empty() {
        name.to_string()
    } else {
        format!("{base}/{name}")
    }
}

fn parse_rfc3339_secs(text: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(text).ok().map(|t| t.timestamp())
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct TreeEntryDto {
    name: String,
    is_dir: bool,
    size: u64,
    modified: String,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct StatDto {
    is_dir: bool,
    size: u64,
    modified: String,
}

/// 向对端要一段文件字节（`GET /file?offset=&len=`）。
///
/// 服务端按 `len` 截断，所以**拿回来的比要的少 = 已经到文件末尾**：虚拟文件的流与提前转存
/// 都靠这条判结束，短读不是错误。
async fn fetch_file_range(
    client: &reqwest::Client,
    file: &PeerFile,
    offset: u64,
    want: u64,
) -> Result<Vec<u8>, String> {
    let url = format!("{}/file", file.base);
    let resp = client
        .get(&url)
        .query(&[
            ("share", file.share_id.as_str()),
            ("path", file.path.as_str()),
            ("offset", &offset.to_string()),
            ("len", &want.to_string()),
        ])
        .bearer_auth(&file.token)
        .send()
        .await
        .map_err(|e| format!("读取对端文件失败（share={} path={:?}）: {e}", file.share_id, file.path))?;
    if !resp.status().is_success() {
        let status = resp.status();
        // 把 share/path 与**响应体**都带上：403 只有两种来源，体里分得清清楚楚 ——
        // "Path access denied" = 路径解析不了（对端旧版本拖着"共享项根是文件"那一档时，
        // path 是空串、对端 `share_root.join("")` 多一个结尾分隔符）；
        // "Path traversal denied" = 越界。光看状态码没法区分是谁的错。
        let body = resp.text().await.unwrap_or_default();
        return Err(format!(
            "读取对端文件失败: HTTP {status}（share={} path={:?}）: {}",
            file.share_id,
            file.path,
            body.trim()
        ));
    }
    Ok(resp.bytes().await.map_err(|e| format!("读取对端文件中断: {e}"))?.to_vec())
}

async fn fetch_tree(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    share_id: &str,
    path: &str,
) -> Result<Vec<TreeEntryDto>, String> {
    let url = format!("{base}/tree");
    let resp = client
        .get(&url)
        .query(&[("share", share_id), ("path", path)])
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| format!("读取对端目录失败 ({url}): {e}"))?;
    if !resp.status().is_success() {
        if resp.status() == reqwest::StatusCode::BAD_REQUEST {
            return Err(format!("读取对端目录失败: {NOT_A_DIRECTORY}（{path}）"));
        }
        return Err(format!("读取对端目录失败: HTTP {}（{path}）", resp.status()));
    }
    let body: serde_json::Value = resp.json().await.map_err(|e| format!("解析目录失败: {e}"))?;
    let entries = body
        .get("entries")
        .and_then(|v| v.as_array())
        .ok_or_else(|| "对端目录响应异常".to_string())?;
    entries
        .iter()
        .map(|entry| {
            Ok(TreeEntryDto {
                name: entry.get("name").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
                is_dir: entry.get("isDir").and_then(|v| v.as_bool()).unwrap_or(false),
                size: entry.get("size").and_then(|v| v.as_u64()).unwrap_or(0),
                modified: entry.get("modified").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
            })
        })
        .collect()
}

async fn fetch_stat(
    client: &reqwest::Client,
    base: &str,
    token: &str,
    share_id: &str,
    path: &str,
) -> Result<StatDto, String> {
    let url = format!("{base}/stat");
    let resp = client
        .get(&url)
        .query(&[("share", share_id), ("path", path)])
        .bearer_auth(token)
        .send()
        .await
        .map_err(|e| format!("读取对端文件信息失败 ({url}): {e}"))?;
    if !resp.status().is_success() {
        return Err(format!("读取对端文件信息失败: HTTP {}", resp.status()));
    }
    let value: serde_json::Value = resp.json().await.map_err(|e| format!("解析文件信息失败: {e}"))?;
    Ok(StatDto {
        is_dir: value.get("isDir").and_then(|v| v.as_bool()).unwrap_or(false),
        size: value.get("size").and_then(|v| v.as_u64()).unwrap_or(0),
        modified: value.get("modified").and_then(|v| v.as_str()).unwrap_or_default().to_string(),
    })
}

// ---------------------------------------------------------------------------
// 名字与路径
// ---------------------------------------------------------------------------

/// 一段路径名：剔掉本机不接受的字符与结尾的空格/点（对端可能是 macOS，名字里会有 `:`）。
/// 整体非法（空、`.`、`..`）返回 None。
fn plain_component(name: &str) -> Option<String> {
    if name.is_empty() {
        return None;
    }
    let cleaned: String = name
        .chars()
        .map(|c| match c {
            '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|' => '_',
            c if c.is_control() => '_',
            c => c,
        })
        .collect();
    let cleaned = cleaned.trim_end_matches([' ', '.']);
    if cleaned.is_empty() || cleaned == "." || cleaned == ".." {
        return None;
    }
    Some(cleaned.to_string())
}

/// 去掉 Windows 的 `\\?\` 扩展长度前缀：交给资源管理器的是普通路径，那种写法在部分
/// shell API 里认不出来（`canonicalize` 一定会带上它）。
fn plain_path(path: &Path) -> PathBuf {
    let text = path.to_string_lossy();
    if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
        return PathBuf::from(format!(r"\\{rest}"));
    }
    if let Some(rest) = text.strip_prefix(r"\\?\") {
        return PathBuf::from(rest);
    }
    path.to_path_buf()
}

/// `CF_HDROP` 的 HGLOBAL 内容：一个 20 字节的 `DROPFILES` 头（`pFiles` = 20、`fWide` = TRUE）
/// + 一串以 `\0` 结尾的 UTF-16 路径，末尾再补一个 `\0` 收尾（双 `\0` = 列表结束）。
///
/// 手写字节而不是拼 `DROPFILES` 结构，是为了这份布局能在任何平台上被测试盯着
/// （`imp` 里有一条 `size_of::<DROPFILES>() == 20` 的静态断言给两边对表）。
fn hdrop_bytes(paths: &[PathBuf]) -> Vec<u8> {
    const HEADER_LEN: u32 = 20;
    let mut wide: Vec<u16> = Vec::new();
    for path in paths {
        wide.extend(path.to_string_lossy().encode_utf16());
        wide.push(0);
    }
    wide.push(0);

    let mut out = Vec::with_capacity(HEADER_LEN as usize + wide.len() * 2);
    out.extend_from_slice(&HEADER_LEN.to_le_bytes()); // pFiles：路径区相对这块内存的偏移
    out.extend_from_slice(&0i32.to_le_bytes()); // pt.x（落下点由 shell 自己填）
    out.extend_from_slice(&0i32.to_le_bytes()); // pt.y
    out.extend_from_slice(&0i32.to_le_bytes()); // fNC
    out.extend_from_slice(&1i32.to_le_bytes()); // fWide：路径是 UTF-16
    for unit in wide {
        out.extend_from_slice(&unit.to_le_bytes());
    }
    out
}

// ---------------------------------------------------------------------------
// OLE 拖放源（Windows）
// ---------------------------------------------------------------------------

#[cfg(windows)]
mod imp {
    use std::future::Future;
    use std::sync::atomic::{AtomicIsize, AtomicU32, AtomicU64, Ordering};
    use std::sync::LazyLock;

    use windows::core::{implement, w, Error, HRESULT, Result as WinResult, BOOL};
    use windows::Win32::Foundation::{
        CloseHandle, DV_E_FORMATETC, DRAGDROP_S_CANCEL, DRAGDROP_S_DROP,
        DRAGDROP_S_USEDEFAULTCURSORS, E_FAIL, E_NOTIMPL, E_OUTOFMEMORY,
        OLE_E_ADVISENOTSUPPORTED, POINT, S_OK,
    };
    use windows::Win32::Storage::FileSystem::{FILE_ATTRIBUTE_DIRECTORY, FILE_ATTRIBUTE_NORMAL};
    use windows::Win32::System::Com::{
        IAdviseSink, IDataObject, IDataObject_Impl, IEnumFORMATETC, IEnumFORMATETC_Impl,
        IEnumSTATDATA, ISequentialStream_Impl, IStream, IStream_Impl, DATADIR_GET, DVASPECT_CONTENT,
        FORMATETC, STGMEDIUM, STGMEDIUM_0, STGTY_STREAM, STREAM_SEEK, STREAM_SEEK_CUR,
        STREAM_SEEK_END, STREAM_SEEK_SET, STATFLAG, STATSTG, TYMED_HGLOBAL, TYMED_ISTREAM,
    };
    use windows::Win32::System::DataExchange::{GetClipboardFormatNameW, RegisterClipboardFormatW};
    use windows::Win32::System::Memory::{
        GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE, GMEM_ZEROINIT,
    };
    use windows::Win32::System::Ole::{
        DoDragDrop, IDropSource, IDropSource_Impl, OleInitialize, CF_HDROP, DROPEFFECT,
        DROPEFFECT_COPY,
    };
    use windows::Win32::System::SystemServices::{MK_LBUTTON, MODIFIERKEYS_FLAGS};
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };
    use windows::Win32::UI::WindowsAndMessaging::{
        GetCursorPos, GetWindowThreadProcessId, WindowFromPoint,
    };
    use windows::Win32::UI::Shell::{
        FILEDESCRIPTORW, FD_ATTRIBUTES, FD_FILESIZE, FD_PROGRESSUI, FD_UNICODE, FD_WRITESTIME,
    };

    use super::{fetch_file_range, hdrop_bytes, Payload, PeerFile, VirtualItem, PROBE, READ_CHUNK};

    /// 手写的 `CF_HDROP` 头必须和 shell 的 `DROPFILES` 一样大。
    const _: () = assert!(std::mem::size_of::<windows::Win32::UI::Shell::DROPFILES>() == 20);

    // -----------------------------------------------------------------------
    // 拖放诊断：非 shell 目标"提示可释放、落下却没东西"要靠这几行日志定位
    //
    // 记三件事：起拖时我们提供了什么格式、目标每次问了什么格式（我们给没给）、
    // 光标下是哪个应用。`target` 打了标，不需要时可以 `RUST_LOG=orbdrop::dragprobe=off`
    // 或直接删掉这一节。
    // -----------------------------------------------------------------------

    /// 剪贴板格式号 → 人能读的名字：标准格式查表，注册格式问系统。
    fn cf_name(cf: u16) -> String {
        let known = match cf {
            1 => "CF_TEXT",
            8 => "CF_DIB",
            13 => "CF_UNICODETEXT",
            15 => "CF_HDROP",
            17 => "CF_DIBV5",
            _ => "",
        };
        if !known.is_empty() {
            return format!("{known}({cf})");
        }
        let mut buf = [0u16; 128];
        let n = unsafe { GetClipboardFormatNameW(cf as u32, &mut buf) };
        if n > 0 {
            format!("{}({cf})", String::from_utf16_lossy(&buf[..n as usize]))
        } else {
            format!("(0x{cf:04x})")
        }
    }

    /// 起拖时写一行"我们提供了什么"：对照目标要什么，一眼看出是"它没要"还是"我们没给"。
    fn log_advertised(payload: &Payload) {
        match payload {
            Payload::RealPaths(paths) => tracing::info!(
                target: PROBE, kind = "real", paths = paths.len(),
                formats = %cf_name(CF_HDROP.0),
                "拖放：数据对象提供的格式"
            ),
            Payload::Virtual(items) => tracing::info!(
                target: PROBE, kind = "virtual", items = items.len(),
                formats = %format!(
                    "{}, {}, {}×{}",
                    cf_name(*FILE_DESCRIPTOR),
                    cf_name(*PREFERRED_DROP_EFFECT),
                    cf_name(*FILE_CONTENTS),
                    items.len()
                ),
                "拖放：数据对象提供的格式"
            ),
        }
    }

    /// 光标下那个窗口属于哪个进程：日志里"这一段是谁问的"靠它定位。
    ///
    /// 拖放期间每次 mouse-move 都会进 `QueryContinueDrag`，所以只在目标变了时才写一行；
    /// 自己（含 OLE 画的拖影窗口）不算目标。
    fn note_target_under_cursor(last: &AtomicIsize) {
        unsafe {
            let mut pt = POINT::default();
            if GetCursorPos(&mut pt).is_err() {
                return;
            }
            let hwnd = WindowFromPoint(pt);
            let key = hwnd.0 as isize;
            if last.swap(key, Ordering::Relaxed) == key {
                return;
            }
            if hwnd.0.is_null() {
                return;
            }
            let mut pid = 0u32;
            GetWindowThreadProcessId(hwnd, Some(&mut pid));
            if pid == std::process::id() {
                return;
            }
            tracing::info!(
                target: PROBE, pid, process = %process_name(pid),
                "拖放：光标下的目标窗口"
            );
        }
    }

    /// 进程 id → exe 文件名（拿不到就给 pid）：权限不够（对面是管理员进程）时也照常返回。
    fn process_name(pid: u32) -> String {
        unsafe {
            let Ok(handle) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
                return format!("pid:{pid}");
            };
            let mut buf = [0u16; 512];
            let mut len = buf.len() as u32;
            let ok = QueryFullProcessImageNameW(
                handle,
                PROCESS_NAME_WIN32,
                windows::core::PWSTR(buf.as_mut_ptr()),
                &mut len,
            );
            let _ = CloseHandle(handle);
            if ok.is_err() {
                return format!("pid:{pid}");
            }
            let full = String::from_utf16_lossy(&buf[..len as usize]);
            full.rsplit(['\\', '/']).next().unwrap_or(&full).to_string()
        }
    }

    /// 在本线程上起一次拖放，阻塞到用户松手/取消为止。
    ///
    /// `OleInitialize` 是每线程一次、可重复调用的（WebView2 早就初始化过 STA），这里只是
    /// 保证拖放线程自己有一份计数。`handle` 是给虚拟文件的流用的：流的 `Read` 会被 OLE 的
    /// 线程调用（跨进程时是本进程的 RPC 线程），那里没有运行时，得借这个把请求发回运行时。
    pub fn drag(payload: &Payload, handle: tokio::runtime::Handle) -> Result<(), String> {
        unsafe {
            let _ = OleInitialize(None);
        }
        log_advertised(payload);
        let data: IDataObject = DragDataObject {
            payload: payload.clone(),
            handle,
            client: reqwest::Client::builder()
                .connect_timeout(std::time::Duration::from_secs(5))
                .timeout(std::time::Duration::from_secs(30))
                .build()
                .unwrap_or_default(),
            seen: Default::default(),
        }
        .into();
        let source: IDropSource = CopyDropSource {
            last_target: AtomicIsize::new(0),
            last_effect: AtomicU32::new(u32::MAX),
        }
        .into();
        let mut effect = DROPEFFECT::default();
        // 只允许 COPY：本机共享项拖出去时源就是用户的真文件，落下只能是复制，不能是搬走。
        let hr = unsafe { DoDragDrop(&data, &source, DROPEFFECT_COPY, &mut effect) };
        if hr == DRAGDROP_S_DROP {
            // 用户把内容落在了某个目标上，目标给了什么效果由 `effect` 报账（COPY = 1）。
            tracing::info!(target: PROBE, effect = effect.0, "拖放：拖拽已落下");
        }
        hr.ok().map_err(|e| format!("拖放失败: {e}"))
    }

    /// 我们提供的格式。本机项只给 `CF_HDROP`；对端项给"虚拟文件"那一组。
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Format {
        FileDrop,
        FileDescriptor,
        FileContents(i32),
        PreferredDropEffect,
    }

    static FILE_DESCRIPTOR: LazyLock<u16> =
        LazyLock::new(|| unsafe { RegisterClipboardFormatW(w!("FileGroupDescriptorW")) as u16 });
    static FILE_CONTENTS: LazyLock<u16> =
        LazyLock::new(|| unsafe { RegisterClipboardFormatW(w!("FileContents")) as u16 });
    /// "只复制、别搬走"的保险 —— 同一块盘内拖拽的默认动作本来是"移动"。
    static PREFERRED_DROP_EFFECT: LazyLock<u16> =
        LazyLock::new(|| unsafe { RegisterClipboardFormatW(w!("Preferred DropEffect")) as u16 });

    fn format_of(payload: &Payload, pformatetc: *const FORMATETC) -> Option<Format> {
        let f = unsafe { pformatetc.as_ref() }?;
        if f.dwAspect != DVASPECT_CONTENT.0 as u32 {
            return None;
        }
        // `tymed` 是**位掩码**，不是枚举值：实测资源管理器要文件内容时给的是 13
        // （ISTREAM | ISTORAGE | HGLOBAL）。按等号判会把这种请求全部打回。
        let wants = |tymed: i32| f.tymed & tymed as u32 != 0;
        let cf = f.cfFormat;
        if cf == *PREFERRED_DROP_EFFECT {
            return wants(TYMED_HGLOBAL.0).then_some(Format::PreferredDropEffect);
        }
        match payload {
            Payload::RealPaths(_) => {
                (cf == CF_HDROP.0 && wants(TYMED_HGLOBAL.0)).then_some(Format::FileDrop)
            }
            Payload::Virtual(items) => {
                if cf == *FILE_DESCRIPTOR && wants(TYMED_HGLOBAL.0) {
                    return Some(Format::FileDescriptor);
                }
                if cf == *FILE_CONTENTS
                    && wants(TYMED_ISTREAM.0)
                    && f.lindex >= 0
                    && (f.lindex as usize) < items.len()
                {
                    return Some(Format::FileContents(f.lindex));
                }
                None
            }
        }
    }

    fn error(hr: HRESULT) -> Error {
        Error::new(hr, "")
    }

    /// `GMEM_MOVEABLE` 不是可选风格：OLE 要跨进程把这块内存交给资源管理器，
    /// 固定内存（`GMEM_FIXED`）编组不过去。
    fn hglobal(bytes: &[u8]) -> WinResult<windows::Win32::Foundation::HGLOBAL> {
        unsafe {
            let handle = GlobalAlloc(GMEM_MOVEABLE | GMEM_ZEROINIT, bytes.len())?;
            let dst = GlobalLock(handle) as *mut u8;
            if dst.is_null() {
                return Err(error(E_OUTOFMEMORY));
            }
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), dst, bytes.len());
            // 返回值 FALSE 且 GetLastError == NO_ERROR 表示"还锁着"，不是错误，直接忽略。
            let _ = GlobalUnlock(handle);
            Ok(handle)
        }
    }

    /// `FILEGROUPDESCRIPTORW`：`cItems` + N 个 `FILEDESCRIPTORW`（shell 头里是 packed(1)，
    /// 所以按 `size_of` 逐条拷字节，不假设对齐）。
    fn group_descriptor(items: &[VirtualItem]) -> Vec<u8> {
        let mut out = Vec::with_capacity(4 + items.len() * std::mem::size_of::<FILEDESCRIPTORW>());
        out.extend_from_slice(&(items.len() as u32).to_le_bytes());
        for item in items {
            let mut desc = FILEDESCRIPTORW::default();
            let mut flags = FD_ATTRIBUTES.0 | FD_UNICODE.0;
            if !item.is_dir {
                flags |= FD_PROGRESSUI.0;
                // 大小不知道就不报：报了 0 的话，认 `FD_FILESIZE` 的目标会拷出 0 字节的
                // 空文件。不报时目标照旧读到流末尾为止。
                if item.size_known {
                    flags |= FD_FILESIZE.0;
                }
                if item.mtime.is_some() {
                    flags |= FD_WRITESTIME.0;
                }
            }
            desc.dwFlags = flags as u32;
            desc.dwFileAttributes = if item.is_dir {
                FILE_ATTRIBUTE_DIRECTORY.0
            } else {
                FILE_ATTRIBUTE_NORMAL.0
            };
            desc.nFileSizeHigh = (item.size >> 32) as u32;
            desc.nFileSizeLow = (item.size & 0xffff_ffff) as u32;
            if let Some(secs) = item.mtime {
                desc.ftLastWriteTime = unix_to_filetime(secs);
            }
            for (i, unit) in item.name.encode_utf16().take(259).enumerate() {
                desc.cFileName[i] = unit;
            }
            let raw = unsafe {
                std::slice::from_raw_parts(
                    &desc as *const FILEDESCRIPTORW as *const u8,
                    std::mem::size_of::<FILEDESCRIPTORW>(),
                )
            };
            out.extend_from_slice(raw);
        }
        out
    }

    /// Unix 秒 → FILETIME（1601-01-01 起的 100ns）。
    fn unix_to_filetime(secs: i64) -> windows::Win32::Foundation::FILETIME {
        const EPOCH_DIFF: i64 = 11_644_473_600;
        let ticks = (secs.saturating_add(EPOCH_DIFF)) as u64 * 10_000_000;
        windows::Win32::Foundation::FILETIME {
            dwLowDateTime: (ticks & 0xffff_ffff) as u32,
            dwHighDateTime: (ticks >> 32) as u32,
        }
    }

    /// 对端文件的 `IStream`：`Read` 按需向对端要那一段字节（`GET /file?offset=&len=`）。
    ///
    /// 流的第一次读**发生在落下那一刻之后**（资源管理器开始复制时），所以拖一个 10GB 的
    /// 目录也是秒起拖，字节在复制过程中才流过去。
    #[implement(IStream)]
    struct PeerStream {
        /// 目录项没有内容来源：那时它是一个**空流**（枚举表里列了它，GetData 就必须给得出来，
        /// 否则跨进程编组会因为"枚举有、取不到"而卡住）。
        file: Option<PeerFile>,
        handle: tokio::runtime::Handle,
        client: reqwest::Client,
        /// 已知大小（拿不到就是 None，那时靠"短读 = 到末尾"判结束）。
        size: Option<u64>,
        pos: AtomicU64,
    }

    /// 在任意线程上同步等一个异步结果：把活派回运行时，用标准 channel 等回来。
    ///
    /// 不用 `Handle::block_on` —— `Read` 可能正好被运行时自己的线程调用，那样会卡死。
    fn blocking<T: Send + 'static>(
        handle: &tokio::runtime::Handle,
        fut: impl Future<Output = T> + Send + 'static,
    ) -> Option<T> {
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        handle.spawn(async move {
            let _ = tx.send(fut.await);
        });
        rx.recv().ok()
    }

    #[allow(non_snake_case)]
    impl ISequentialStream_Impl for PeerStream_Impl {
        fn Read(&self, pv: *mut core::ffi::c_void, cb: u32, pcbread: *mut u32) -> HRESULT {
            let pos = self.pos.load(Ordering::Relaxed);
            let mut want = (cb as u64).min(READ_CHUNK);
            if let Some(size) = self.size {
                want = want.min(size.saturating_sub(pos));
            }
            if want == 0 {
                unsafe { *pcbread = 0 };
                return S_OK;
            }
            let Some(file) = self.file.clone() else {
                unsafe { *pcbread = 0 };
                return S_OK;
            };
            let client = self.client.clone();
            let fetched = blocking(&self.handle, async move {
                fetch_file_range(&client, &file, pos, want).await
            });
            match fetched {
                Some(Ok(bytes)) => {
                    let n = bytes.len().min(want as usize);
                    unsafe {
                        std::ptr::copy_nonoverlapping(bytes.as_ptr(), pv as *mut u8, n);
                        *pcbread = n as u32;
                    }
                    self.pos.store(pos + n as u64, Ordering::Relaxed);
                    S_OK
                }
                Some(Err(e)) => {
                    tracing::warn!(error = %e, "拖拽读对端文件失败");
                    E_FAIL
                }
                None => E_FAIL,
            }
        }

        fn Write(&self, _pv: *const core::ffi::c_void, _cb: u32, _pcbwritten: *mut u32) -> HRESULT {
            E_NOTIMPL
        }
    }

    #[allow(non_snake_case)]
    impl IStream_Impl for PeerStream_Impl {
        fn Seek(&self, dlibmove: i64, dworigin: STREAM_SEEK, plibnewposition: *mut u64) -> WinResult<()> {
            let base = match dworigin {
                STREAM_SEEK_SET => 0,
                STREAM_SEEK_CUR => self.pos.load(Ordering::Relaxed) as i64,
                STREAM_SEEK_END => self.size.unwrap_or(0) as i64,
                _ => 0,
            };
            let next = (base + dlibmove).max(0) as u64;
            self.pos.store(next, Ordering::Relaxed);
            if !plibnewposition.is_null() {
                unsafe { *plibnewposition = next };
            }
            Ok(())
        }

        /// 大小是资源管理器显示进度用的（拿不到大小的项报 0，它照样读到末尾为止）。
        fn Stat(&self, pstatstg: *mut STATSTG, grfstatflag: &STATFLAG) -> WinResult<()> {
            unsafe {
                (*pstatstg).cbSize = self.size.unwrap_or(0);
                (*pstatstg).r#type = STGTY_STREAM.0 as u32;
                (*pstatstg).pwcsName = windows::core::PWSTR::null();
            }
            if grfstatflag.0 == 0 {
                // 要求给出名字：按 COM 规矩用 CoTaskMemAlloc 分配
                let name: Vec<u16> = self
                    .file
                    .as_ref()
                    .and_then(|f| f.path.rsplit('/').next())
                    .unwrap_or("file")
                    .encode_utf16()
                    .chain(std::iter::once(0))
                    .collect();
                let bytes = name.len() * 2;
                unsafe {
                    let ptr = windows::Win32::System::Com::CoTaskMemAlloc(bytes) as *mut u16;
                    if !ptr.is_null() {
                        std::ptr::copy_nonoverlapping(name.as_ptr(), ptr, name.len());
                        (*pstatstg).pwcsName = windows::core::PWSTR(ptr);
                    }
                }
            }
            Ok(())
        }

        fn Clone(&self) -> WinResult<IStream> {
            Ok(PeerStream {
                file: self.file.clone(),
                handle: self.handle.clone(),
                client: self.client.clone(),
                size: self.size,
                pos: AtomicU64::new(self.pos.load(Ordering::Relaxed)),
            }
            .into())
        }

        fn SetSize(&self, _libnewsize: u64) -> WinResult<()> {
            Err(error(E_NOTIMPL))
        }

        fn CopyTo(
            &self,
            _pstm: windows::core::Ref<'_, IStream>,
            _cb: u64,
            _pcbread: *mut u64,
            _pcbwritten: *mut u64,
        ) -> WinResult<()> {
            Err(error(E_NOTIMPL))
        }

        fn Commit(&self, _grfcommitflags: &windows::Win32::System::Com::STGC) -> WinResult<()> {
            Err(error(E_NOTIMPL))
        }

        fn Revert(&self) -> WinResult<()> {
            Err(error(E_NOTIMPL))
        }

        fn LockRegion(
            &self,
            _liboffset: u64,
            _cb: u64,
            _dwlocktype: &windows::Win32::System::Com::LOCKTYPE,
        ) -> WinResult<()> {
            Err(error(E_NOTIMPL))
        }

        fn UnlockRegion(&self, _liboffset: u64, _cb: u64, _dwlocktype: u32) -> WinResult<()> {
            Err(error(E_NOTIMPL))
        }
    }

    /// 诊断去重表的键：`(呼号, 格式, dwAspect, lindex, tymed)`。
    type QueryKey = (u16, u32, i32, u32, &'static str);

    #[implement(IDataObject)]
    struct DragDataObject {
        payload: Payload,
        handle: tokio::runtime::Handle,
        client: reqwest::Client,
        /// 诊断去重：同一个键一次拖拽只写一行 —— 资源管理器会把同一组格式问上很多遍。
        seen: parking_lot::Mutex<std::collections::HashSet<QueryKey>>,
    }

    impl DragDataObject {
        fn r#virtual(&self) -> Option<&Vec<VirtualItem>> {
            match &self.payload {
                Payload::Virtual(items) => Some(items),
                Payload::RealPaths(_) => None,
            }
        }

        /// 记一次目标的格式询问，并标出我们能不能应答（`supported`）——
        /// "它要了、我们没给"就是非 shell 目标落下没内容的根因。
        fn log_query(&self, call: &'static str, pformatetc: *const FORMATETC) {
            let Some(f) = (unsafe { pformatetc.as_ref() }) else {
                return;
            };
            if !self.seen.lock().insert((f.cfFormat, f.dwAspect, f.lindex, f.tymed, call)) {
                return;
            }
            tracing::info!(
                target: PROBE,
                call,
                supported = format_of(&self.payload, pformatetc).is_some(),
                format = %cf_name(f.cfFormat),
                tymed = f.tymed,
                lindex = f.lindex,
                aspect = f.dwAspect,
                "拖放：目标询问格式"
            );
        }
    }

    #[allow(non_snake_case)]
    impl IDataObject_Impl for DragDataObject_Impl {
        fn GetData(&self, pformatetc: *const FORMATETC) -> WinResult<STGMEDIUM> {
            self.log_query("GetData", pformatetc);
            let bytes = match format_of(&self.payload, pformatetc) {
                Some(Format::FileDrop) => match &self.payload {
                    Payload::RealPaths(paths) => hdrop_bytes(paths),
                    Payload::Virtual(_) => return Err(error(DV_E_FORMATETC)),
                },
                Some(Format::FileDescriptor) => group_descriptor(self.r#virtual().unwrap_or(&Vec::new())),
                Some(Format::PreferredDropEffect) => DROPEFFECT_COPY.0.to_le_bytes().to_vec(),
                Some(Format::FileContents(index)) => {
                    let item = &self.r#virtual().ok_or_else(|| error(DV_E_FORMATETC))?[index as usize];
                    let stream: IStream = PeerStream {
                        file: item.source.clone(),
                        handle: self.handle.clone(),
                        client: self.client.clone(),
                        size: (!item.is_dir).then_some(item.size),
                        pos: AtomicU64::new(0),
                    }
                    .into();
                    return Ok(STGMEDIUM {
                        tymed: TYMED_ISTREAM.0 as u32,
                        u: STGMEDIUM_0 { pstm: std::mem::ManuallyDrop::new(Some(stream)) },
                        pUnkForRelease: std::mem::ManuallyDrop::new(None),
                    });
                }
                None => return Err(error(DV_E_FORMATETC)),
            };
            Ok(STGMEDIUM {
                tymed: TYMED_HGLOBAL.0 as u32,
                u: STGMEDIUM_0 { hGlobal: hglobal(&bytes)? },
                pUnkForRelease: std::mem::ManuallyDrop::new(None),
            })
        }

        fn GetDataHere(&self, pformatetc: *const FORMATETC, _pmedium: *mut STGMEDIUM) -> WinResult<()> {
            self.log_query("GetDataHere", pformatetc);
            Err(error(DV_E_FORMATETC))
        }

        fn QueryGetData(&self, pformatetc: *const FORMATETC) -> HRESULT {
            self.log_query("QueryGetData", pformatetc);
            if format_of(&self.payload, pformatetc).is_some() {
                S_OK
            } else {
                DV_E_FORMATETC
            }
        }

        fn GetCanonicalFormatEtc(
            &self,
            _pformatectin: *const FORMATETC,
            pformatetcout: *mut FORMATETC,
        ) -> HRESULT {
            // 约定的"没有更规范的格式"：把 ptd 置空 + E_NOTIMPL。
            unsafe { (*pformatetcout).ptd = std::ptr::null_mut() };
            E_NOTIMPL
        }

        fn SetData(
            &self,
            _pformatetc: *const FORMATETC,
            _pmedium: *const STGMEDIUM,
            _frelease: BOOL,
        ) -> WinResult<()> {
            // 落下之后资源管理器会往源对象上写"Performed DropEffect"/"DropDescription"
            // 之类的账（实测会调 SetData + 忽略失败）。我们只做 COPY，不靠这些账决定
            // 删不删源文件，所以一律不支持 —— 实测复制本身照常完成。
            Err(error(E_NOTIMPL))
        }

        fn EnumFormatEtc(&self, dwdirection: u32) -> WinResult<IEnumFORMATETC> {
            tracing::info!(target: PROBE, direction = dwdirection, "拖放：目标枚举格式表");
            if dwdirection != DATADIR_GET.0 as u32 {
                return Err(error(E_NOTIMPL));
            }
            // 跨进程拖放就是照这张表逐个 GetData 编组的，给出去的东西必须都在表里。
            let mut formats = Vec::new();
            match &self.payload {
                Payload::RealPaths(_) => formats.push(formatetc(CF_HDROP.0, TYMED_HGLOBAL, -1)),
                Payload::Virtual(items) => {
                    formats.push(formatetc(*FILE_DESCRIPTOR, TYMED_HGLOBAL, -1));
                    formats.push(formatetc(*PREFERRED_DROP_EFFECT, TYMED_HGLOBAL, -1));
                    for index in 0..items.len() as i32 {
                        formats.push(formatetc(*FILE_CONTENTS, TYMED_ISTREAM, index));
                    }
                }
            }
            if !formats.iter().any(|f| f.cfFormat == *PREFERRED_DROP_EFFECT) {
                formats.push(formatetc(*PREFERRED_DROP_EFFECT, TYMED_HGLOBAL, -1));
            }
            Ok(FormatEnumerator { formats, next: AtomicU64::new(0) }.into())
        }

        fn DAdvise(
            &self,
            _pformatetc: *const FORMATETC,
            _advf: u32,
            _padvsink: windows::core::Ref<'_, IAdviseSink>,
        ) -> WinResult<u32> {
            Err(error(OLE_E_ADVISENOTSUPPORTED))
        }

        fn DUnadvise(&self, _dwconnection: u32) -> WinResult<()> {
            Err(error(OLE_E_ADVISENOTSUPPORTED))
        }

        fn EnumDAdvise(&self) -> WinResult<IEnumSTATDATA> {
            Err(error(OLE_E_ADVISENOTSUPPORTED))
        }
    }

    fn formatetc(cf: u16, tymed: windows::Win32::System::Com::TYMED, lindex: i32) -> FORMATETC {
        FORMATETC {
            cfFormat: cf,
            ptd: std::ptr::null_mut(),
            dwAspect: DVASPECT_CONTENT.0 as u32,
            lindex,
            tymed: tymed.0 as u32,
        }
    }

    #[implement(IEnumFORMATETC)]
    struct FormatEnumerator {
        formats: Vec<FORMATETC>,
        next: AtomicU64,
    }

    #[allow(non_snake_case)]
    impl IEnumFORMATETC_Impl for FormatEnumerator_Impl {
        fn Next(&self, celt: u32, rgelt: *mut FORMATETC, pceltfetched: *mut u32) -> HRESULT {
            let mut fetched = 0u32;
            let mut index = self.next.load(Ordering::Relaxed) as usize;
            while fetched < celt && index < self.formats.len() {
                unsafe { rgelt.add(fetched as usize).write(self.formats[index]) };
                fetched += 1;
                index += 1;
            }
            self.next.store(index as u64, Ordering::Relaxed);
            if !pceltfetched.is_null() {
                unsafe { *pceltfetched = fetched };
            }
            if fetched == celt {
                S_OK
            } else {
                windows::Win32::Foundation::S_FALSE
            }
        }

        fn Skip(&self, celt: u32) -> WinResult<()> {
            let index = (self.next.load(Ordering::Relaxed) as usize + celt as usize).min(self.formats.len());
            self.next.store(index as u64, Ordering::Relaxed);
            Ok(())
        }

        fn Reset(&self) -> WinResult<()> {
            self.next.store(0, Ordering::Relaxed);
            Ok(())
        }

        fn Clone(&self) -> WinResult<IEnumFORMATETC> {
            Ok(FormatEnumerator {
                formats: self.formats.clone(),
                next: AtomicU64::new(self.next.load(Ordering::Relaxed)),
            }
            .into())
        }
    }

    #[implement(IDropSource)]
    struct CopyDropSource {
        /// 诊断：光标下"上一次记过的目标窗口"。每次拖拽新建一个，所以换到同一个应用
        /// 也会重新记一行。
        last_target: AtomicIsize,
        /// 诊断：目标上一次报的落点效果（目标说"能收"才会是 COPY=1，0 = none）。
        last_effect: AtomicU32,
    }

    #[allow(non_snake_case)]
    impl IDropSource_Impl for CopyDropSource_Impl {
        /// MSDN 样例的写法：Esc 取消，主键松开即落下，其余继续。
        ///
        /// `grfKeyState` 是**本线程消息队列**里的键状态，按下没被处理过就会报 `0x0`
        /// （自测脚本合成输入时踩过这个坑：一进去就被判成"已经松手"，拖放当场结束）。
        fn QueryContinueDrag(&self, fescapepressed: BOOL, grfkeystate: MODIFIERKEYS_FLAGS) -> HRESULT {
            note_target_under_cursor(&self.last_target);
            if fescapepressed.as_bool() {
                DRAGDROP_S_CANCEL
            } else if (grfkeystate & MK_LBUTTON) == MODIFIERKEYS_FLAGS(0) {
                DRAGDROP_S_DROP
            } else {
                S_OK
            }
        }

        fn GiveFeedback(&self, dweffect: DROPEFFECT) -> HRESULT {
            if self.last_effect.swap(dweffect.0, Ordering::Relaxed) != dweffect.0 {
                tracing::info!(
                    target: PROBE, effect = dweffect.0,
                    "拖放：目标给出的落点效果（1 = 复制、2 = 移动、0 = 不收）"
                );
            }
            DRAGDROP_S_USEDEFAULTCURSORS
        }
    }

    /// 拖放数据对象的接口面：资源管理器（**另一个进程**）就是靠"枚举格式表 + 逐个
    /// GetData"把内容取过去的（`CF_HDROP` 与虚拟文件两条路都走这里）。
    #[cfg(test)]
    mod tests {
        use super::*;
        use std::path::PathBuf;

        use windows::Win32::System::Memory::GlobalSize;
        use windows::Win32::System::Ole::ReleaseStgMedium;

        fn object(payload: Payload) -> IDataObject {
            unsafe {
                let _ = OleInitialize(None);
            }
            DragDataObject {
                payload,
                handle: tokio::runtime::Handle::current(),
                client: reqwest::Client::new(),
                seen: Default::default(),
            }
            .into()
        }

        fn read_medium(medium: &STGMEDIUM) -> Vec<u8> {
            assert_eq!(medium.tymed, TYMED_HGLOBAL.0 as u32);
            unsafe {
                let handle = medium.u.hGlobal;
                let ptr = GlobalLock(handle) as *const u8;
                assert!(!ptr.is_null());
                let bytes = std::slice::from_raw_parts(ptr, GlobalSize(handle)).to_vec();
                let _ = GlobalUnlock(handle);
                bytes
            }
        }

        fn peer_file() -> PeerFile {
            PeerFile {
                base: "http://127.0.0.1:1".into(),
                token: "t".into(),
                share_id: "s".into(),
                path: "a/b.txt".into(),
            }
        }

        fn item(name: &str, is_dir: bool, size: u64) -> VirtualItem {
            VirtualItem {
                name: name.into(),
                is_dir,
                size,
                size_known: true,
                mtime: Some(1_700_000_000),
                source: (!is_dir).then(peer_file),
            }
        }

        /// 本机项：枚举里有 CF_HDROP，GetData 交出的就是那份路径列表。
        #[tokio::test]
        async fn real_paths_object_offers_cf_hdrop() {
            let paths = vec![PathBuf::from(r"C:\tmp\a.txt"), PathBuf::from(r"C:\tmp\中文 目录")];
            let object = object(Payload::RealPaths(paths.clone()));

            let enumerator = unsafe { object.EnumFormatEtc(DATADIR_GET.0 as u32) }.expect("EnumFormatEtc");
            let mut formats = [FORMATETC::default(); 2];
            assert_eq!(unsafe { enumerator.Next(&mut formats, None) }, S_OK);
            assert_eq!(formats[0].cfFormat, CF_HDROP.0);
            assert_eq!(formats[1].cfFormat, *PREFERRED_DROP_EFFECT);

            assert_eq!(unsafe { object.QueryGetData(&formatetc(CF_HDROP.0, TYMED_HGLOBAL, -1)) }, S_OK);
            let mut medium = unsafe { object.GetData(&formatetc(CF_HDROP.0, TYMED_HGLOBAL, -1)) }.expect("GetData");
            assert_eq!(read_medium(&medium), hdrop_bytes(&paths));
            unsafe { ReleaseStgMedium(&mut medium) };
        }

        /// 对端项：枚举里是"描述符 + 每个文件一条 FileContents"，描述符字节按 shell 的
        /// packed 布局；内容那条给的是 `IStream`（落下时才去读）。
        #[tokio::test]
        async fn virtual_object_offers_descriptor_and_contents() {
            let items = vec![item("目录", true, 0), item("目录\\a.txt", false, 5)];
            let object = object(Payload::Virtual(items.clone()));

            let enumerator = unsafe { object.EnumFormatEtc(DATADIR_GET.0 as u32) }.expect("EnumFormatEtc");
            let mut formats = [FORMATETC::default(); 4];
            assert_eq!(unsafe { enumerator.Next(&mut formats, None) }, S_OK, "四种格式一次取完");
            assert_eq!(formats[0].cfFormat, *FILE_DESCRIPTOR);
            assert_eq!(formats[1].cfFormat, *PREFERRED_DROP_EFFECT);
            assert_eq!(formats[2].cfFormat, *FILE_CONTENTS);
            assert_eq!(formats[2].lindex, 0);
            assert_eq!(formats[2].tymed, TYMED_ISTREAM.0 as u32);
            assert_eq!(formats[3].lindex, 1, "每个文件一条 FileContents");

            let mut descriptor =
                unsafe { object.GetData(&formatetc(*FILE_DESCRIPTOR, TYMED_HGLOBAL, -1)) }.expect("描述符");
            let blob = read_medium(&descriptor);
            unsafe { ReleaseStgMedium(&mut descriptor) };
            let one = std::mem::size_of::<FILEDESCRIPTORW>();
            assert_eq!(blob.len(), 4 + 2 * one);
            assert_eq!(u32::from_le_bytes(blob[0..4].try_into().unwrap()), 2, "cItems");
            let attr = |i: usize| u32::from_le_bytes(blob[4 + i * one + 36..4 + i * one + 40].try_into().unwrap());
            let flags = |i: usize| u32::from_le_bytes(blob[4 + i * one..4 + i * one + 4].try_into().unwrap());
            let size_low = |i: usize| u32::from_le_bytes(blob[4 + i * one + 68..4 + i * one + 72].try_into().unwrap());
            let name = |i: usize| {
                let mut units = Vec::new();
                for c in blob[4 + i * one + 72..4 + i * one + 72 + 40].chunks_exact(2) {
                    let u = u16::from_le_bytes([c[0], c[1]]);
                    if u == 0 {
                        break;
                    }
                    units.push(u);
                }
                String::from_utf16_lossy(&units)
            };

            assert_eq!(attr(0), FILE_ATTRIBUTE_DIRECTORY.0, "第一条是目录");
            assert_eq!(flags(0) & FD_FILESIZE.0 as u32, 0, "目录不报大小");
            assert_eq!(size_low(0), 0);
            assert_eq!(name(0), "目录");
            assert_eq!(attr(1), FILE_ATTRIBUTE_NORMAL.0, "第二条是文件");
            assert_ne!(flags(1) & FD_FILESIZE.0 as u32, 0, "文件要报大小");
            assert_ne!(flags(1) & FD_WRITESTIME.0 as u32, 0, "有修改时间就带上");
            assert_eq!(size_low(1), 5);
            assert_eq!(name(1), r"目录\a.txt", "名字是相对落点的路径，反斜杠分隔");
            assert_ne!(flags(1) & FD_UNICODE.0 as u32, 0, "名字是 UTF-16");

            // 内容：给的是流，且没读之前一次网络请求都不发（base 是个连不上的地址）
            assert_eq!(unsafe { object.QueryGetData(&formatetc(*FILE_CONTENTS, TYMED_ISTREAM, 1)) }, S_OK);
            assert_eq!(
                unsafe { object.QueryGetData(&formatetc(*FILE_CONTENTS, TYMED_ISTREAM, 9)) },
                DV_E_FORMATETC,
                "越界的 lindex 要拒绝"
            );
            let mut medium = unsafe { object.GetData(&formatetc(*FILE_CONTENTS, TYMED_ISTREAM, 1)) }.expect("内容");
            assert_eq!(medium.tymed, TYMED_ISTREAM.0 as u32);
            unsafe { ReleaseStgMedium(&mut medium) };
        }

        /// 诊断标签：注册格式必须解析成它们的名字——日志看不懂就等于没打。
        #[test]
        fn probe_labels_resolve_registered_formats() {
            assert_eq!(cf_name(CF_HDROP.0), "CF_HDROP(15)");
            assert_eq!(
                cf_name(*FILE_DESCRIPTOR),
                format!("FileGroupDescriptorW({})", *FILE_DESCRIPTOR)
            );
            assert_eq!(cf_name(*FILE_CONTENTS), format!("FileContents({})", *FILE_CONTENTS));
        }

        /// 大小**未知**的文件不能在描述符里报 0 字节：认 `FD_FILESIZE` 的目标会拷出一个空文件。
        /// 不报（清掉 `FD_FILESIZE`）时目标照旧读到流末尾为止。
        #[tokio::test]
        async fn unknown_size_file_omits_the_file_size_flag() {
            let mut unknown = item("未知大小.bin", false, 0);
            unknown.size_known = false;
            let object = object(Payload::Virtual(vec![unknown]));

            let mut descriptor =
                unsafe { object.GetData(&formatetc(*FILE_DESCRIPTOR, TYMED_HGLOBAL, -1)) }.expect("描述符");
            let blob = read_medium(&descriptor);
            unsafe { ReleaseStgMedium(&mut descriptor) };

            let flags = u32::from_le_bytes(blob[4..8].try_into().unwrap());
            assert_eq!(flags & FD_FILESIZE.0 as u32, 0, "大小未知就不报大小");
            assert_ne!(flags & FD_PROGRESSUI.0 as u32, 0, "照样要进度 UI");
            assert_ne!(flags & FD_UNICODE.0 as u32, 0, "名字仍是 UTF-16");
        }

        /// 本机项不提供虚拟文件那两种格式（反之亦然）：给错东西比不给更糟。
        #[tokio::test]
        async fn formats_do_not_leak_between_payload_kinds() {
            let real = object(Payload::RealPaths(vec![PathBuf::from(r"C:\tmp\a.txt")]));
            assert_eq!(
                unsafe { real.QueryGetData(&formatetc(*FILE_DESCRIPTOR, TYMED_HGLOBAL, -1)) },
                DV_E_FORMATETC
            );
            let virt = object(Payload::Virtual(vec![item("a.txt", false, 1)]));
            assert_eq!(
                unsafe { virt.QueryGetData(&formatetc(CF_HDROP.0, TYMED_HGLOBAL, -1)) },
                DV_E_FORMATETC
            );
        }
    }
}

/// 非 Windows 平台还没有原生拖放源（macOS 要走 `NSDraggingSession`）。
#[cfg(not(windows))]
mod imp {
    use super::Payload;

    pub fn drag(_payload: &Payload, _handle: tokio::runtime::Handle) -> Result<(), String> {
        Err("当前平台暂不支持把文件拖出到资源管理器".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一个够用的虚拟项（只关心"名字/是不是目录/大小/大小是否已知"）。
    fn virt(name: &str, is_dir: bool, size: u64) -> VirtualItem {
        VirtualItem {
            name: name.into(),
            is_dir,
            size,
            size_known: true,
            mtime: None,
            source: None,
        }
    }

    /// 提前转存的门槛：单文件一档、目录一档，且**大小未知一律不转存**。
    ///
    /// 这条判据同时决定两件事：只认 `CF_HDROP` 的应用（微信这类）收不收得到，
    /// 以及起拖前用户要按住鼠标等多久 —— 所以边界要钉死。
    #[test]
    fn staging_eligibility_follows_the_size_caps() {
        // 单个文件：按单文件上限
        assert!(staging_eligible(&[virt("a.bin", false, STAGE_MAX_FILE)]), "正好到上限要转存");
        assert!(
            !staging_eligible(&[virt("a.bin", false, STAGE_MAX_FILE + 1)]),
            "超一个字节就不转存"
        );

        // 目录：按总量上限（根目录那条 is_dir 不计入总量）
        let dir = virt("d", true, 0);
        let mut tree = vec![dir.clone()];
        tree.push(virt(r"d\a.bin", false, STAGE_MAX_TOTAL / 2));
        tree.push(virt(r"d\b.bin", false, STAGE_MAX_TOTAL / 2));
        assert!(staging_eligible(&tree), "总量正好到上限要转存");
        tree.push(virt(r"d\c.bin", false, 1));
        assert!(!staging_eligible(&tree), "总量超一个字节就不转存");

        // 目录档比单文件档宽：同一个超标文件，单独拖不转存、放进目录里转存
        let big = STAGE_MAX_FILE + 1;
        assert!(big <= STAGE_MAX_TOTAL, "两档的大小关系是这条判据的前提");
        assert!(!staging_eligible(&[virt("a.bin", false, big)]));
        assert!(staging_eligible(&[dir.clone(), virt(r"d\a.bin", false, big)]));

        // 大小未知：宁可退回虚拟文件，也不赌一个可能几 GB 的下载
        let mut unknown = virt("a.bin", false, 0);
        unknown.size_known = false;
        assert!(!staging_eligible(&[unknown]));

        // 空目录：没有内容可转存
        assert!(!staging_eligible(&[dir]));
    }

    fn request(peer_id: Option<&str>, path: &str) -> DragOutRequest {
        DragOutRequest {
            peer_id: peer_id.map(|p| p.to_string()),
            share_id: "s1".into(),
            path: path.into(),
            name: "项目".into(),
            is_dir: true,
        }
    }

    /// CF_HDROP 的字节布局：头 20 字节、路径 UTF-16、单 `\0` 分隔、双 `\0` 收尾。
    #[test]
    fn hdrop_layout_is_wide_paths_with_double_nul() {
        let paths = vec![PathBuf::from(r"C:\tmp\a.txt"), PathBuf::from(r"C:\tmp\中文 名")];
        let bytes = hdrop_bytes(&paths);

        assert_eq!(u32::from_le_bytes(bytes[0..4].try_into().unwrap()), 20, "pFiles 指向路径区");
        assert_eq!(&bytes[4..12], &[0u8; 8], "pt 归零");
        assert_eq!(i32::from_le_bytes(bytes[12..16].try_into().unwrap()), 0, "fNC");
        assert_eq!(i32::from_le_bytes(bytes[16..20].try_into().unwrap()), 1, "fWide");

        let units: Vec<u16> = bytes[20..]
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        assert_eq!(units[units.len() - 1], 0, "末尾一个 \\0 收尾");
        assert_eq!(units[units.len() - 2], 0, "紧挨着的是最后一条路径自己的 \\0");
        let text: String = String::from_utf16_lossy(&units[..units.len() - 2]);
        assert_eq!(text.split('\0').collect::<Vec<_>>(), vec![r"C:\tmp\a.txt", r"C:\tmp\中文 名"]);
    }

    /// 名字清洗：本机不收的字符替掉（对端可能是 macOS），结构性的名字直接不要。
    #[test]
    fn plain_component_cleans_and_rejects() {
        assert_eq!(plain_component("12:30.log"), Some("12_30.log".to_string()));
        assert_eq!(plain_component("尾 点. "), Some("尾 点".to_string()));
        assert_eq!(plain_component("a\\b"), Some("a_b".to_string()), "反斜杠是分隔符，不能当名字");
        assert_eq!(plain_component(""), None);
        assert_eq!(plain_component("."), None);
        assert_eq!(plain_component(".."), None);
        assert_eq!(plain_component("   "), None);
    }

    /// 交给 shell 的路径不能带 `\\?\`：那种写法在部分 shell API 里认不出来。
    #[test]
    fn plain_path_strips_verbatim_prefix() {
        assert_eq!(plain_path(Path::new(r"\\?\C:\Users\me\跟 目")), PathBuf::from(r"C:\Users\me\跟 目"));
        assert_eq!(plain_path(Path::new(r"\\?\UNC\nas\share\a")), PathBuf::from(r"\\nas\share\a"));
        assert_eq!(plain_path(Path::new(r"D:\普通\路径")), PathBuf::from(r"D:\普通\路径"));
    }

    /// 时间戳：描述符里的 FILETIME 是 1601 起的 100ns。
    #[test]
    fn rfc3339_round_trip_to_secs() {
        assert_eq!(parse_rfc3339_secs("2026-09-16T12:00:00+00:00"), Some(1_789_560_000));
        assert_eq!(parse_rfc3339_secs(""), None);
    }

    /// 本机共享项：拖的就是磁盘上那个路径（当场可拖），越界路径拖不出去。
    #[test]
    fn local_share_prepares_instantly() {
        let root = std::env::temp_dir().join(format!("orbdrop-drag-{}", Uuid::new_v4()));
        std::fs::create_dir_all(root.join("子目录")).unwrap();
        std::fs::write(root.join("子目录").join("a.txt"), b"x").unwrap();

        let share = crate::config::ShareItem::new(root.clone());
        let share_id = share.id.clone();
        let mut config = crate::config::Config::default();
        config.shares = vec![share];
        let state = Arc::new(AppState::new(config, root.join("config.toml")));

        let req = DragOutRequest {
            peer_id: None,
            share_id: share_id.clone(),
            path: "子目录/a.txt".into(),
            name: "a.txt".into(),
            is_dir: false,
        };
        let ticket = state.drag.prepare(&state, &req).expect("本机共享项当场可拖");
        assert!(ticket.ready, "本机路径不需要先走目录");
        let expect = plain_path(&root.join("子目录").join("a.txt").canonicalize().unwrap());
        match state.drag.payload(&ticket.ticket).unwrap().unwrap() {
            Payload::RealPaths(paths) => assert_eq!(paths, vec![expect]),
            other => panic!("本机项应该是真实路径: {other:?}"),
        }

        // 共享项根（''）也要能拖
        let root_req = DragOutRequest { path: String::new(), name: "根".into(), is_dir: true, ..req.clone() };
        let root_ticket = state.drag.prepare(&state, &root_req).unwrap();
        match state.drag.payload(&root_ticket.ticket).unwrap().unwrap() {
            Payload::RealPaths(paths) => {
                assert_eq!(paths, vec![plain_path(&root.canonicalize().unwrap())])
            }
            other => panic!("本机项应该是真实路径: {other:?}"),
        }

        // 越界：共享项之外的东西不给拖
        let escape = DragOutRequest { path: "../secret.txt".into(), ..req.clone() };
        assert!(state.drag.prepare(&state, &escape).is_err(), "共享项之外的路径必须拒绝");

        let _ = std::fs::remove_dir_all(&root);
    }

    /// 对端项在**没有 Tokio 运行时**的上下文里登记（同步 Tauri 命令踩过这个坑）：
    /// 后台活儿必须**真的跑起来**，不许被无声丢掉，也不许把票据永远留在「准备中」。
    ///
    /// 以前这里断言"当场判失败"—— 那是 `task::spawn` 在没运行时时"记日志跳过"的产物：
    /// 用户点一下什么都没发生。现在它会退到 Tauri 的全局运行时，所以对端项照常去拉；
    /// 这个用例里对端是编的地址，于是任务很快就会以"连不上"收尾 —— 关键是它**动了**。
    #[test]
    fn peer_prepare_without_runtime_still_runs_the_task() {
        let root = std::env::temp_dir().join(format!("orbdrop-drag-rt-{}", Uuid::new_v4()));
        std::fs::create_dir_all(&root).unwrap();
        let mut config = crate::config::Config::default();
        config.shares = vec![crate::config::ShareItem::new(root.clone())];
        let state = Arc::new(AppState::new(config, root.join("config.toml")));

        let req = request(Some("device-nowhere"), "");
        let ticket = state.drag.prepare(&state, &req).expect("登记不该失败");
        assert!(
            matches!(state.drag.payload(&ticket.ticket), Ok(None)),
            "刚登记时票据就该是「准备中」"
        );

        // 任务真的在跑：几秒内它必须自己收尾（对端地址是编的 → 连不上 → 判失败）。
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        loop {
            match state.drag.payload(&ticket.ticket) {
                Ok(None) => {}                     // 还在走目录：正常
                Ok(Some(_)) => panic!("对端地址是编的，不可能备好"),
                Err(msg) => {
                    assert!(!msg.is_empty(), "失败要带一句能看懂的原因");
                    break;
                }
            }
            assert!(
                std::time::Instant::now() < deadline,
                "票据不能永远停在「准备中」——那说明后台任务被丢掉了"
            );
            std::thread::sleep(std::time::Duration::from_millis(100));
        }

        let _ = std::fs::remove_dir_all(&root);
    }
}
