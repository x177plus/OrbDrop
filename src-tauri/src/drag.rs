//! 拖拽下载：把共享项里的任意文件/目录**拖到资源管理器/Finder 里**，落在拖到的那个目录。
//!
//! 网页里的 HTML5 拖拽出不了 webview —— 目的地是另一个进程，只有 OS 的原生拖放能跨过去。
//! 所以这里自己当 OLE 拖放源：`DoDragDrop`（见 `imp`）。
//!
//! 两种内容，都走 `CF_HDROP` **真实路径**：
//!
//! - **本机共享项**：磁盘上本来就有这些文件，把真实路径交给资源管理器。只给 `DROPEFFECT_COPY`
//!   （外加 `Preferred DropEffect` 这道保险）—— 同一块盘内拖拽的默认动作是"移动"，不写死复制
//!   会把用户的源文件搬走。
//! - **对端共享项**：本地建一个 CFAPI **占位符**就把路径交出去，字节等目标真去读它时才从对端拉
//!   （`crate::mirror`）—— 起拖零等待、零下载，而且只对真被消费的部分付钱。
//!
//! 曾经还有两条路（小项"提前转存"到 `<数据目录>/staging`、大项走 OLE 虚拟文件 + 自定义
//! `IStream`），镜像实机验证通过后已整体删除；为什么与实测数值见 `docs/cfapi-mirror.md`。
//!
//! 时序：界面按下鼠标移动出阈值就 `prepare`（登记票据，对端项同时去建占位符），
//! 随后 `start` 等备齐再起拖 —— 拖放循环要跟着按住的主键走，所以主键还按着才起拖。

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Manager};
use tracing::{info, warn};
use uuid::Uuid;

use crate::state::AppState;
use crate::task;

/// 等对端项备齐（问一次元数据 + 建占位符）的上限；这条线是防挂死。
const PREPARE_TIMEOUT: Duration = Duration::from_secs(120);
/// 票据在内存里留一会儿：界面万一起拖失败（手松早了），接着再拖一次还能命中同一张。
const TICKET_TTL: Duration = Duration::from_secs(600);

/// 拖放诊断的日志 target。从"界面请求起拖"到"目标问了什么格式"整条链都归它，
/// `RUST_LOG=orbdrop::dragprobe=off` 可关；分析完可整节删掉。
const PROBE: &str = "orbdrop::dragprobe";

// ---------------------------------------------------------------------------
// 对端项：本地建占位符，字节等目标读的时候才拉（见 `crate::mirror`）
// ---------------------------------------------------------------------------

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

/// 拖出去的内容：**交给资源管理器的真实路径**。
///
/// 本机共享项本来就是磁盘上的真文件；对端项是镜像里的占位符（`crate::mirror`，真实路径、
/// 按需从对端拉字节）—— 两者对 `CF_HDROP` 来说都是路径，所以这里不再区分。
pub type Payload = Vec<PathBuf>;

enum Staged {
    /// 正在准备对端项（问元数据 + 建占位符）。
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

    /// 登记一次拖拽意图并返回票据。
    ///
    /// 本机共享项当场可拖；对端项先在后台建占位符（问一次元数据 + `CfCreatePlaceholders`），
    /// **不下载任何字节**。
    pub fn prepare(&self, state: &Arc<AppState>, req: &DragOutRequest) -> Result<DragTicket, String> {
        let req = req.clone();
        let name = req.name.clone();
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
                        Ok(paths) => info!(ticket = %id, paths = paths.len(), "拖拽内容已备好"),
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
                    Entry { at: Instant::now(), staged: Staged::Ready(vec![path]) },
                );
                Ok(DragTicket { ticket, name, ready: true })
            }
        }
    }

    /// 票据此刻的内容：`Ok(Some)` 可以起拖了，`Ok(None)` 还在准备，`Err` 不用等了。
    pub fn payload(&self, ticket: &str) -> Result<Option<Payload>, String> {
        match self.entries.lock().get(ticket) {
            Some(Entry { staged: Staged::Ready(payload), .. }) => Ok(Some(payload.clone())),
            Some(Entry { staged: Staged::Failed(msg), .. }) => Err(msg.clone()),
            Some(_) => Ok(None),
            None => Err("这次拖拽已失效，请重新拖一次".into()),
        }
    }

    /// 后台准备（建占位符）的收尾。
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
/// 窗口所在线程上；对端项的建占位符是异步的，先在 tokio 线程上等完再切过去。
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
            // 起拖前把窗口带到前台（见 `orb_window::ensure_foreground`）：`DoDragDrop` 同样只在
            // 窗口是前台窗口时才真正吃住指针，而"点击激活"是异步的。
            if let Some(win) = app.get_webview_window("main") {
                let _ = crate::orb_window::ensure_foreground(&win);
            }
            let (tx, rx) = tokio::sync::oneshot::channel();
            app.run_on_main_thread(move || {
                let _ = tx.send(imp::drag(&payload));
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
            return Err("准备超时：对端太慢或已离线".into());
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

/// 对端项的准备：本地建一个占位符就完事（**不下载任何字节**）。
///
/// 交给 `CF_HDROP` 的是镜像里的真实路径 —— 只认路径的应用（微信这类）与认虚拟文件的目标走
/// 同一条路；字节等目标真去读的时候才从对端拉（见 `crate::mirror`）。
async fn prepare_peer_payload(
    state: &Arc<AppState>,
    req: &DragOutRequest,
    peer_id: &str,
) -> Result<Payload, String> {
    // 顺手按龄回收上次留下的镜像项（与旧的 sweep_staging 同一个时机：每次拖拽 + 启动）。
    crate::mirror::sweep();
    let item = crate::mirror::MirroredItem {
        peer_id: peer_id.to_string(),
        share_id: req.share_id.clone(),
        path: req.path.clone(),
        name: req.name.clone(),
        is_dir: req.is_dir,
    };
    let path = crate::mirror::place(state, &item).await?;
    info!(path = %path.display(), "镜像就绪，按本地真实路径拖出");
    Ok(vec![path])
}

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
    use std::sync::atomic::{AtomicIsize, AtomicU32, AtomicU64, Ordering};
    use std::sync::LazyLock;

    use windows::core::{implement, w, Error, HRESULT, Result as WinResult, BOOL};
    use windows::Win32::Foundation::{
        CloseHandle, DV_E_FORMATETC, DRAGDROP_S_CANCEL, DRAGDROP_S_DROP,
        DRAGDROP_S_USEDEFAULTCURSORS, E_NOTIMPL, E_OUTOFMEMORY,
        OLE_E_ADVISENOTSUPPORTED, POINT, S_OK,
    };
    use windows::Win32::System::Com::{
        IAdviseSink, IDataObject, IDataObject_Impl, IEnumFORMATETC, IEnumFORMATETC_Impl,
        IEnumSTATDATA, DATADIR_GET, DVASPECT_CONTENT, FORMATETC, STGMEDIUM, STGMEDIUM_0,
        TYMED_HGLOBAL,
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
    use std::path::PathBuf;

    use super::{hdrop_bytes, PROBE};

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
    ///
    /// 两种内容（本机真文件、对端项的占位符）对 `CF_HDROP` 来说没区别，所以这里只有一种。
    fn log_advertised(paths: &[PathBuf]) {
        tracing::info!(
            target: PROBE, paths = paths.len(),
            formats = %cf_name(CF_HDROP.0),
            "拖放：数据对象提供的格式"
        );
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
    /// 保证拖放线程自己有一份调用计数。
    pub fn drag(paths: &[PathBuf]) -> Result<(), String> {
        unsafe {
            let _ = OleInitialize(None);
        }
        log_advertised(paths);
        let data: IDataObject = DragDataObject { paths: paths.to_vec(), seen: Default::default() }.into();
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

    /// 我们提供的格式：只有 `CF_HDROP`（真实路径）与那道保险。
    #[derive(Clone, Copy, PartialEq, Eq)]
    enum Format {
        FileDrop,
        PreferredDropEffect,
    }

    /// "只复制、别搬走"的保险 —— 同一块盘内拖拽的默认动作本来是"移动"。
    static PREFERRED_DROP_EFFECT: LazyLock<u16> =
        LazyLock::new(|| unsafe { RegisterClipboardFormatW(w!("Preferred DropEffect")) as u16 });

    /// 目标问的这个格式我们给不给。
    fn format_of(pformatetc: *const FORMATETC) -> Option<Format> {
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
        (cf == CF_HDROP.0 && wants(TYMED_HGLOBAL.0)).then_some(Format::FileDrop)
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

    /// 诊断去重表的键：`(呼号, 格式, dwAspect, lindex, tymed)`。
    type QueryKey = (u16, u32, i32, u32, &'static str);

    #[implement(IDataObject)]
    struct DragDataObject {
        /// 交给资源管理器的真实路径（本机真文件，或镜像里的占位符）。
        paths: Vec<PathBuf>,
        /// 诊断去重：同一个键一次拖拽只写一行 —— 资源管理器会把同一组格式问上很多遍。
        seen: parking_lot::Mutex<std::collections::HashSet<QueryKey>>,
    }

    impl DragDataObject {
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
                supported = format_of(pformatetc).is_some(),
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
            let bytes = match format_of(pformatetc) {
                Some(Format::FileDrop) => hdrop_bytes(&self.paths),
                Some(Format::PreferredDropEffect) => DROPEFFECT_COPY.0.to_le_bytes().to_vec(),
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
            if format_of(pformatetc).is_some() {
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
            let formats = vec![
                formatetc(CF_HDROP.0, TYMED_HGLOBAL, -1),
                formatetc(*PREFERRED_DROP_EFFECT, TYMED_HGLOBAL, -1),
            ];
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
    /// GetData"把 `CF_HDROP` 那份路径列表取过去的。
    #[cfg(test)]
    mod tests {
        use super::*;
        use std::path::PathBuf;

        use windows::Win32::System::Memory::GlobalSize;
        use windows::Win32::System::Ole::ReleaseStgMedium;

        fn object(paths: Vec<PathBuf>) -> IDataObject {
            unsafe {
                let _ = OleInitialize(None);
            }
            DragDataObject { paths, seen: Default::default() }.into()
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

        /// 只给 `CF_HDROP`（本机真文件与镜像占位符对目标来说没区别），外加"只复制"那道保险。
        #[tokio::test]
        async fn object_offers_cf_hdrop_and_nothing_else() {
            let paths = vec![PathBuf::from(r"C:\tmp\a.txt"), PathBuf::from(r"C:\tmp\中文 目录")];
            let object = object(paths.clone());

            let enumerator = unsafe { object.EnumFormatEtc(DATADIR_GET.0 as u32) }.expect("EnumFormatEtc");
            let mut formats = [FORMATETC::default(); 2];
            assert_eq!(unsafe { enumerator.Next(&mut formats, None) }, S_OK, "正好两种格式");
            assert_eq!(formats[0].cfFormat, CF_HDROP.0);
            assert_eq!(formats[1].cfFormat, *PREFERRED_DROP_EFFECT);

            assert_eq!(unsafe { object.QueryGetData(&formatetc(CF_HDROP.0, TYMED_HGLOBAL, -1)) }, S_OK);
            let mut medium = unsafe { object.GetData(&formatetc(CF_HDROP.0, TYMED_HGLOBAL, -1)) }.expect("GetData");
            assert_eq!(read_medium(&medium), hdrop_bytes(&paths));
            unsafe { ReleaseStgMedium(&mut medium) };
        }

        /// 诊断标签：`CF_HDROP` 与那道保险都必须解析成人看得懂的名字 —— 日志看不懂等于没打。
        #[test]
        fn probe_labels_resolve_registered_formats() {
            assert_eq!(cf_name(CF_HDROP.0), "CF_HDROP(15)");
            assert_eq!(
                cf_name(*PREFERRED_DROP_EFFECT),
                format!("Preferred DropEffect({})", *PREFERRED_DROP_EFFECT)
            );
        }
    }
}

/// 非 Windows 平台还没有原生拖放源（macOS 要走 `NSDraggingSession`）。
#[cfg(not(windows))]
mod imp {
    pub fn drag(_paths: &[std::path::PathBuf]) -> Result<(), String> {
        Err("当前平台暂不支持把文件拖出到资源管理器".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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

    /// 交给 shell 的路径不能带 `\\?\`：那种写法在部分 shell API 里认不出来。
    #[test]
    fn plain_path_strips_verbatim_prefix() {
        assert_eq!(plain_path(Path::new(r"\\?\C:\Users\me\跟 目")), PathBuf::from(r"C:\Users\me\跟 目"));
        assert_eq!(plain_path(Path::new(r"\\?\UNC\nas\share\a")), PathBuf::from(r"\\nas\share\a"));
        assert_eq!(plain_path(Path::new(r"D:\普通\路径")), PathBuf::from(r"D:\普通\路径"));
    }

    /// 时间戳：描述符里的 FILETIME 是 1601 起的 100ns。

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
        let paths = state.drag.payload(&ticket.ticket).unwrap().unwrap();
        assert_eq!(paths, vec![expect], "本机项给的就是磁盘上的真实路径");

        // 共享项根（''）也要能拖
        let root_req = DragOutRequest { path: String::new(), name: "根".into(), is_dir: true, ..req.clone() };
        let root_ticket = state.drag.prepare(&state, &root_req).unwrap();
        let paths = state.drag.payload(&root_ticket.ticket).unwrap().unwrap();
        assert_eq!(paths, vec![plain_path(&root.canonicalize().unwrap())]);

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
                Ok(None) => {}                     // 还在建占位符：正常
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
