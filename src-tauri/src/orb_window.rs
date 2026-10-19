//! 悬浮球窗口几何：判定展开方向所需的显示器工作区，以及原子的移动 + 缩放。
//!
//! 展开方向必须知道「悬浮球当前在哪、四周还剩多少可显示区域」，而 Tauri 的
//! `Monitor` 只给出整块显示器矩形（不含任务栏/Dock），所以工作区直接取
//! Win32 `GetMonitorInfoW(...).rcWork`；移动+缩放用一次 `SetWindowPos` 完成，
//! 避免「先移动后缩放」产生中间帧（透明窗口会看到悬浮球瞬移）。

use serde::Serialize;
use tauri::WebviewWindow;

/// 屏幕矩形（物理像素，虚拟屏坐标）
#[derive(Debug, Clone, Copy, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ScreenMetrics {
    /// 悬浮球所在显示器的工作区（已扣除任务栏 / Dock）
    pub work_area: Rect,
    /// 该显示器整屏矩形
    pub monitor: Rect,
    pub scale_factor: f64,
}

/// 查询悬浮球所在显示器的工作区（物理像素）。
pub fn metrics(window: &WebviewWindow) -> Result<ScreenMetrics, String> {
    imp::metrics(window)
}

/// 一次性移动 + 缩放窗口：保持 Z 序（始终置顶）、不抢焦点。
pub fn set_rect(window: &WebviewWindow, rect: Rect) -> Result<(), String> {
    // 每次改窗口尺寸前后 DWM 都可能重画系统边框，所以顺手再抹一次（失败不影响移动/缩放）
    let _ = strip_os_frame(window);
    imp::set_rect(window, rect)
}

/// 开始拖动悬浮球窗口：先把抓取锚点校正回按下点，再当场把拖动交给 OS。
///
/// OS 的窗口拖动（`WM_NCLBUTTONDOWN`/HTCAPTION 模态循环）是**一次性**把窗口钉在指针上的：
/// `start_dragging` 那一刻「指针在窗口里的位置」就成了整个拖动过程的不变量。于是从按下点走到
/// 那一刻的位移永久丢失 —— 球在指针旁边偏着走，松手时指针落在球上的相对位置也不是按下时那个。
/// （按住面板头部拖动没有这个问题：那条路径在 mousedown 当场上交，锚点天然就是按下点。）
///
/// 校正与起拖**必须在同一次调用里**完成：中间哪怕只多回一趟前端，指针就又走了一段，那段同样
/// 会偏掉。指针位置也在这里现读（`imp::drag_align`），不用前端事件里的坐标 —— pointermove
/// 最多滞后一帧，高速拖动时那一帧就是十几个像素，而 OS 锚定时读的是物理指针位置。
pub fn drag_start(window: &WebviewWindow, press_x: f64, press_y: f64) -> Result<(), String> {
    imp::drag_align(window, press_x, press_y)?;
    ensure_foreground(window)?;
    window.start_dragging().map_err(|e| format!("start_dragging 失败: {e}"))
}

/// 起拖之前先确保窗口在前台。
///
/// OS 的两条拖拽路径（窗口的模态移动循环、OLE 的 `DoDragDrop`）都只在窗口是**前台窗口**时
/// 才真正吃住指针。而"点击把窗口带到前台"是异步的：指针动得快的时候，起拖请求可能赶在
/// 激活落地之前交出去 —— 那一次拖拽安静地不成立，用户看到的正是"第一次拖没用，再拖一次就好"。
/// 点击本来就会把窗口带到前台，这里只是把它**提前到交出去之前**，不改变用户能观察到的东西。
pub fn ensure_foreground(window: &WebviewWindow) -> Result<(), String> {
    imp::ensure_foreground(window)
}

/// 鼠标主键此刻是否按下。
///
/// 拖动悬浮球走的是 OS 的模态移动循环：窗口一直在动，但页面收不到 mouseup（被拖拽
/// 循环吃掉）。吸附判定若只看 move 事件的间隔，用户拖到一半停一下就会被判定成「松手」，
/// 球在手里被抢到贴边位置。所以落位判定前先问 OS 键状态，按住就不动。
pub fn mouse_down() -> bool {
    imp::mouse_down()
}

/// 这个位置（连同窗口当前尺寸）是否还落在某块显示器上。
///
/// 复原悬浮球位置前必须问一句：显示器被拔掉 / 分辨率变了之后，上次的落点可能已经在
/// 屏幕外 —— 透明无边框窗口没有标题栏可以拖回来，用户只能去删配置文件。
///
/// 判据是「与显示器矩形有正面积相交」而不是「球完整可见」：贴边收起时窗口整个留在
/// 工作区内（球是被窗口裁掉的），所以正常落位必然相交；而两块显示器之间的接缝、
/// 任务栏下方这些地方也不该算失联。
pub fn visible_on_some_monitor(window: &WebviewWindow, x: i32, y: i32) -> bool {
    let size = match window.outer_size() {
        Ok(s) => s,
        // 问不出尺寸就没有判据，按「可见」处理：宁可复原到原位，也别把用户记下的位置丢掉
        Err(_) => return true,
    };
    let rect = Rect { x, y, w: size.width as i32, h: size.height as i32 };
    let monitors: Vec<Rect> = window
        .available_monitors()
        .unwrap_or_default()
        .iter()
        .map(|m| {
            let p = m.position();
            let s = m.size();
            Rect { x: p.x, y: p.y, w: s.width as i32, h: s.height as i32 }
        })
        .collect();
    if monitors.is_empty() {
        return true;   // 拿不到显示器列表（远程会话等）：无据可依，不拦
    }
    overlaps_any(rect, &monitors)
}

/// 矩形是否与 `monitors` 中任一块有正面积交集。
fn overlaps_any(rect: Rect, monitors: &[Rect]) -> bool {
    monitors.iter().any(|m| {
        rect.x < m.x + m.w && m.x < rect.x + rect.w && rect.y < m.y + m.h && m.y < rect.y + rect.h
    })
}

// ---------------------------------------------------------------------------
// 运行中兜底：窗口被藏起来 / 被搬出所有显示器 / 显示器拓扑变了
// ---------------------------------------------------------------------------
//
// 上面那些（`visible_on_some_monitor` / `default_pos`）都只服务于**启动那一刻**：启动时能重来，
// 而运行中发生同一件事时，用户看到的是"窗口没了、托盘还在" —— 无边框窗口没有标题栏可以拖回来，
// 前端也不知道显示器动过（它只在动手那一刻现读 `scaleFactor`）。所以这里补上运行时的判据与动作，
// 由 `lib.rs` 的看门狗按 2s 一拍驱动（见 `start_orb_watchdog`）。

/// 所有显示器的**工作区**（物理像素）。
///
/// 判据与 `visible_on_some_monitor` 同源，但那条只回答"还在不在某块屏上"，这条回答"该搬到哪去"。
/// 必须用工作区而不是整屏矩形：贴边收起时窗口整个落在工作区内（球是被窗口裁掉的），
/// 搬进整屏范围可能正好压在任务栏底下 —— 那等于还是看不见。
fn work_areas(window: &WebviewWindow) -> Vec<Rect> {
    window
        .available_monitors()
        .unwrap_or_default()
        .iter()
        .map(|m| {
            let w = m.work_area();
            Rect {
                x: w.position.x,
                y: w.position.y,
                w: w.size.width as i32,
                h: w.size.height as i32,
            }
        })
        .collect()
}

/// 窗口此刻是否还与某块显示器有正面积交集（判据与启动那次一致，见 `visible_on_some_monitor`）。
pub fn on_some_monitor(window: &WebviewWindow) -> bool {
    let (pos, size) = match (window.outer_position(), window.outer_size()) {
        (Ok(p), Ok(s)) => (p, s),
        // 问不出来就按"在屏上"处理：不搬比乱搬强（搬错地方同样看不见）
        _ => return true,
    };
    let areas = work_areas(window);
    if areas.is_empty() {
        return true; // 拿不到显示器列表（某些远程会话）：无据可依，不搬
    }
    overlaps_any(Rect { x: pos.x, y: pos.y, w: size.width as i32, h: size.height as i32 }, &areas)
}

/// 把窗口搬回**最近的**那块显示器的工作区里（尺寸不动）。返回是否真搬过。
///
/// 只在"已经落在所有显示器之外"时才动：正常贴边、用户故意拖到两屏接缝上都不算失联。
pub fn clamp_into_work_area(window: &WebviewWindow) -> Result<bool, String> {
    let pos = window.outer_position().map_err(|e| e.to_string())?;
    let size = window.outer_size().map_err(|e| e.to_string())?;
    let areas = work_areas(window);
    if areas.is_empty() {
        return Ok(false);
    }
    let rect = Rect { x: pos.x, y: pos.y, w: size.width as i32, h: size.height as i32 };
    if overlaps_any(rect, &areas) {
        return Ok(false);
    }
    let target = nearest_area(rect, &areas);
    let moved = Rect {
        x: clamp_axis(rect.x, target.x, target.x + target.w - rect.w),
        y: clamp_axis(rect.y, target.y, target.y + target.h - rect.h),
        ..rect
    };
    set_rect(window, moved)?;
    Ok(true)
}

/// 与 `rect` 中心最近的那块工作区（已经不在任何屏上时的搬运目标）。
fn nearest_area(rect: Rect, areas: &[Rect]) -> Rect {
    let (cx, cy) = (rect.x + rect.w / 2, rect.y + rect.h / 2);
    let dist = |a: &Rect| {
        let (ax, ay) = (a.x + a.w / 2, a.y + a.h / 2);
        let (dx, dy) = (i64::from(cx - ax), i64::from(cy - ay));
        dx * dx + dy * dy
    };
    areas.iter().copied().min_by_key(dist).unwrap_or(areas[0])
}

/// 把一维坐标夹进 `[min, max]`；`max < min`（工作区比窗口还小）时取 `min` —— 宁可让窗口朝外
/// 溢出一截，也别把它推到工作区另一侧去（`planRectDock` 里同一条取舍）。
fn clamp_axis(v: i32, min: i32, max: i32) -> i32 {
    if max < min {
        min
    } else {
        v.clamp(min, max)
    }
}

/// 显示器工作区集合 + 缩放因子的指纹：变了就说明拓扑/DPI 动过（拔显示器、改分辨率、
/// 远控的虚拟屏、缩放从 150% 变 100%…）。
///
/// 为什么要一个"指纹"而不是在窗口过程里接 `WM_DISPLAYCHANGE`：拓扑变化有好几条来源
/// （`WM_DISPLAYCHANGE`、`WM_DPICHANGED`、虚拟显示器驱动的注册/注销），一个个接容易漏；
/// 而"工作区集合 + 缩放"这一份状态本身就是判据 —— 它变了，界面那份几何（吸附偏移、
/// 展开方向、命中区域）就全过时了。
pub fn topology_fingerprint(window: &WebviewWindow) -> u64 {
    fingerprint_of(&work_areas(window), window.scale_factor().unwrap_or(1.0))
}

/// `topology_fingerprint` 的纯部分（可测）：逐字段 FNV-1a。
///
/// 只判"变没变"，所以枚举顺序不稳定也无所谓（同一份拓扑在两次查询之间是稳定的）。
fn fingerprint_of(areas: &[Rect], scale_factor: f64) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    let mut mix = |v: u64| {
        h ^= v;
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    };
    for a in areas {
        for v in [a.x, a.y, a.w, a.h] {
            mix(v as i64 as u64);
        }
    }
    mix((scale_factor * 1000.0).round() as u64);
    h
}

/// 窗口此刻在 OS 眼里可见吗、被最小化了吗。
///
/// **不能问 Tauri 的 `is_visible()`**：那是 tao 自己缓存的标志位 —— 窗口被系统藏起来
/// （"显示桌面"、别的程序、远控会话切换）时它一声不响，缓存里照样是 `true`，
/// 看门狗就永远发现不了。
pub fn os_state(window: &WebviewWindow) -> (bool, bool) {
    imp::os_state(window)
}

/// 强行让窗口重新画一遍。
///
/// 透明窗口"什么都没画"时 OS 侧一切正常（可见、在屏上、没最小化），没有任何状态可以判据 ——
/// 那种时候能做的只有把它逼着重画一次。只在看门狗已经发现异常时调用（空闲时不碰窗口）。
pub fn force_repaint(window: &WebviewWindow) {
    imp::force_repaint(window)
}

/// 把窗口叫回台面：置顶 + 显示 + 未最小化 + 前台。
///
/// 三个调用点：单实例（第二份进程被启动）、托盘左键（单击/双击）、看门狗。
/// **托盘那条是这里的关键**：托盘图标是唯一"进程还在台上就一定看得见"的东西（它由资源管理器
/// 画），所以它那一下必须真的能把窗口叫回来 —— 只给前端发一条 `orb://show` 是不够的：
/// 前端若正卡在一次展开里（`opening`）那就是空操作，而"窗口被系统藏起来"这种事前端根本不知道。
///
/// "显示"这一步不走 `window.show()`：tao 的 `set_visible(true)` 在它缓存里已经是可见时是
/// **空操作**（diff 为空直接返回），而这里要修的恰恰是"缓存说可见、OS 说不可见"。所以直接
/// `ShowWindow`，并且不激活 —— 前台那一步明着由 `set_focus` 给。
pub fn raise(window: &WebviewWindow) -> Result<(), String> {
    let _ = strip_os_frame(window); // 置顶/显示之后 DWM 可能把那条 1px 系统边框画回来
    imp::force_show(window);
    let _ = window.unminimize();
    window.set_always_on_top(true).map_err(|e| format!("重新置顶失败: {e}"))?;
    window.set_focus().map_err(|e| format!("带到前台失败: {e}"))
}

/// 看门狗轮询间隔。
///
/// 2s：这类事故（窗口被藏 / 被搬走 / 拓扑变了）不需要毫秒级反应；而空闲时这就是它多出来的
/// 全部开销 —— 几次 `IsWindowVisible` / `GetMonitorInfo` 级别的查询，够不着 N-04 的"CPU 空闲近 0"。
pub const WATCHDOG_TICK: std::time::Duration = std::time::Duration::from_secs(2);

/// 首启（配置里还没有落位记录）时悬浮球离工作区右边缘的余量（CSS px）。
///
/// 必须大于前端的吸附阈值（`App.svelte` 的 `DOCK_THRESHOLD` = 20）：默认落位是「靠边但
/// **不吸附**」，落在阈值内的话启动时那次落位判定会当场把球吸到边上、收起大半颗。
const DEFAULT_ORB_MARGIN: f64 = 50.0;

/// 没记过位置时的默认落位（物理像素）：工作区右上、垂直 1/3 处，右边留 `DEFAULT_ORB_MARGIN`。
///
/// 边长取**标称值**（`collapsed_logical` = tauri.conf 里 main 窗口的 width，乘当前 DPI），
/// 不问窗口当前的 `outer_size()`：启动那一刻那个外框还带着 DWM 阴影余量（实测 90 → 136），
/// 照它算出来的落位离右边缘有 84px —— 「没贴边」但也不「靠边」。
///
/// 按悬浮球**窗口**算而不是按球算：球在窗口里只内缩 14 CSS px，两者看起来是同一个位置。
pub fn default_pos(window: &WebviewWindow, collapsed_logical: f64) -> Result<(i32, i32), String> {
    let m = metrics(window)?;
    let side = (collapsed_logical * m.scale_factor).round() as i32;
    let margin = (DEFAULT_ORB_MARGIN * m.scale_factor).round() as i32;
    Ok(default_pos_in(m.work_area, side, margin))
}

/// `default_pos` 的纯几何部分：右上角内缩 `margin`、垂直线 1/3 处（工作区左上角不为 0 时也算对）。
fn default_pos_in(work: Rect, win_w: i32, margin: i32) -> (i32, i32) {
    (work.x + work.w - win_w - margin, work.y + work.h / 3)
}

/// 抹掉 Windows 给顶层窗口自动绘制的 1px 边框与圆角。
///
/// 我们的窗口是透明无边框的，那层 OS 边框会沿着 OS 自己的圆角露在面板之外
/// （面板圆角与窗口圆角之间的缝隙里），且每次改窗口尺寸都会重画一次 ——
/// 表现为窗口最外缘一条闪烁的线。
pub fn strip_os_frame(window: &WebviewWindow) -> Result<(), String> {
    imp::strip_os_frame(window)
}

#[cfg(windows)]
mod imp {
    use super::{Rect, ScreenMetrics};
    use tauri::WebviewWindow;
    use windows::Win32::Foundation::{POINT, RECT};
    use windows::Win32::Graphics::Gdi::{
        ClientToScreen, GetMonitorInfoW, MonitorFromWindow, RedrawWindow, MONITOR_DEFAULTTONEAREST,
        MONITORINFO, RDW_ALLCHILDREN, RDW_INVALIDATE, RDW_UPDATENOW,
    };
    use windows::Win32::UI::HiDpi::GetDpiForWindow;
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LBUTTON};
    use windows::Win32::UI::WindowsAndMessaging::{
        GetCursorPos, GetForegroundWindow, GetWindowRect, IsIconic, IsWindowVisible,
        SetForegroundWindow, SetWindowPos, ShowWindow, SW_SHOWNOACTIVATE, SWP_NOACTIVATE,
        SWP_NOZORDER, SWP_NOSIZE,
    };

    fn to_rect(r: RECT) -> Rect {
        Rect { x: r.left, y: r.top, w: r.right - r.left, h: r.bottom - r.top }
    }

    pub fn mouse_down() -> bool {
        // 高位 = 当前是否按下（低位是「自上次调用以来按过」，这里不关心）
        unsafe { GetAsyncKeyState(VK_LBUTTON.0 as i32) as u16 & 0x8000 != 0 }
    }

    pub fn ensure_foreground(window: &WebviewWindow) -> Result<(), String> {
        let hwnd = window.hwnd().map_err(|e| format!("取窗口句柄失败: {e}"))?;
        unsafe {
            if GetForegroundWindow() == hwnd {
                return Ok(());
            }
            // 用户刚点过我们，前台权限就在本进程手上，这一下一般都会成功；
            // 失败也不报错：拖拽那一步自己会以"没动"告终，不该为此弹一条错。
            let _ = SetForegroundWindow(hwnd);
        }
        Ok(())
    }

    pub fn metrics(window: &WebviewWindow) -> Result<ScreenMetrics, String> {
        let hwnd = window.hwnd().map_err(|e| format!("取窗口句柄失败: {e}"))?;
        unsafe {
            let monitor = MonitorFromWindow(hwnd, MONITOR_DEFAULTTONEAREST);
            if monitor.is_invalid() {
                return Err("MonitorFromWindow 未返回有效期显示器".into());
            }
            let mut info = MONITORINFO {
                cbSize: std::mem::size_of::<MONITORINFO>() as u32,
                ..Default::default()
            };
            if !GetMonitorInfoW(monitor, &mut info).as_bool() {
                return Err("GetMonitorInfoW 失败".into());
            }
            let dpi = GetDpiForWindow(hwnd);
            Ok(ScreenMetrics {
                work_area: to_rect(info.rcWork),
                monitor: to_rect(info.rcMonitor),
                scale_factor: if dpi == 0 { 1.0 } else { f64::from(dpi) / 96.0 },
            })
        }
    }

    pub fn set_rect(window: &WebviewWindow, rect: Rect) -> Result<(), String> {
        let hwnd = window.hwnd().map_err(|e| format!("取窗口句柄失败: {e}"))?;
        unsafe {
            SetWindowPos(hwnd, None, rect.x, rect.y, rect.w, rect.h, SWP_NOACTIVATE | SWP_NOZORDER)
                .map_err(|e| format!("SetWindowPos 失败: {e}"))
        }
    }

    /// 把窗口挪到「按下点仍在指针底下」的位置：锚点该是按下点，而 OS 锚定时读的是此刻的指针
    /// 位置，所以这里现读指针、当场把窗口平移过去（见外层 `drag_start` 的说明）。
    pub fn drag_align(window: &WebviewWindow, press_x: f64, press_y: f64) -> Result<(), String> {
        let hwnd = window.hwnd().map_err(|e| format!("取窗口句柄失败: {e}"))?;
        unsafe {
            // 指针在窗口内的位置必须相对**客户端区**算：Windows 上外框带 DWM 阴影余量，
            // 外框比客户端大一圈（实测 90 → 136），拿外框原点算会整体偏进来。
            let mut origin = POINT::default();
            if !ClientToScreen(hwnd, &mut origin).as_bool() {
                return Err("ClientToScreen 失败".into());
            }
            let mut cur = POINT::default();
            GetCursorPos(&mut cur).map_err(|e| format!("GetCursorPos 失败: {e}"))?;
            // 外框位置只用来当平移基准（要挪的是外框），所以这里可以用 GetWindowRect
            let mut outer = RECT::default();
            GetWindowRect(hwnd, &mut outer).map_err(|e| format!("GetWindowRect 失败: {e}"))?;
            let dpi = GetDpiForWindow(hwnd);
            let sf = if dpi == 0 { 1.0 } else { f64::from(dpi) / 96.0 };
            let sx = f64::from(cur.x - origin.x) - press_x * sf;
            let sy = f64::from(cur.y - origin.y) - press_y * sf;
            if sx.round() == 0.0 && sy.round() == 0.0 {
                return Ok(());
            }
            SetWindowPos(
                hwnd,
                None,
                outer.left + sx.round() as i32,
                outer.top + sy.round() as i32,
                0,
                0,
                SWP_NOACTIVATE | SWP_NOZORDER | SWP_NOSIZE,
            )
            .map_err(|e| format!("SetWindowPos 失败: {e}"))
        }
    }

    pub fn strip_os_frame(window: &WebviewWindow) -> Result<(), String> {
        use std::ffi::c_void;
        use windows::Win32::Graphics::Dwm::{
            DwmSetWindowAttribute, DWMWA_BORDER_COLOR, DWMWA_COLOR_NONE,
            DWMWA_WINDOW_CORNER_PREFERENCE, DWMWCP_DONOTROUND,
        };

        let hwnd = window.hwnd().map_err(|e| format!("取窗口句柄失败: {e}"))?;
        let border: u32 = DWMWA_COLOR_NONE;
        let corner: i32 = DWMWCP_DONOTROUND.0;
        unsafe {
            DwmSetWindowAttribute(
                hwnd,
                DWMWA_BORDER_COLOR,
                &border as *const u32 as *const c_void,
                std::mem::size_of::<u32>() as u32,
            )
            .map_err(|e| format!("DwmSetWindowAttribute(BORDER_COLOR) 失败: {e}"))?;
            DwmSetWindowAttribute(
                hwnd,
                DWMWA_WINDOW_CORNER_PREFERENCE,
                &corner as *const i32 as *const c_void,
                std::mem::size_of::<i32>() as u32,
            )
            .map_err(|e| format!("DwmSetWindowAttribute(CORNER_PREFERENCE) 失败: {e}"))?;
        }
        Ok(())
    }

    /// `IsWindowVisible` + `IsIconic`：OS 眼里的真实状态（不是 tao 的那份缓存）。
    ///
    /// 取不到句柄时按"一切正常"处理 —— 宁可不动窗口，也别凭一个坏句柄去搬它。
    pub fn os_state(window: &WebviewWindow) -> (bool, bool) {
        let Ok(hwnd) = window.hwnd() else {
            return (true, false);
        };
        unsafe { (IsWindowVisible(hwnd).as_bool(), IsIconic(hwnd).as_bool()) }
    }

    /// `RedrawWindow`：连子窗口一起失效并立刻重画（WebView2 是子窗口，只失效父窗口不够）。
    pub fn force_repaint(window: &WebviewWindow) {
        if let Ok(hwnd) = window.hwnd() {
            unsafe {
                let _ = RedrawWindow(
                    Some(hwnd),
                    None,
                    None,
                    RDW_INVALIDATE | RDW_UPDATENOW | RDW_ALLCHILDREN,
                );
            }
        }
    }

    /// 直接 `ShowWindow(SW_SHOWNOACTIVATE)`：跨过 tao 那份缓存，且不抢前台。
    ///
    /// 窗口本来就在屏幕上时这一下是空操作（`ShowWindow` 对已显示的窗口不改状态），
    /// 所以它只在"缓存说可见、OS 说不可见"那条缝里起作用 —— 那正是要修的那条缝。
    pub fn force_show(window: &WebviewWindow) {
        if let Ok(hwnd) = window.hwnd() {
            unsafe {
                let _ = ShowWindow(hwnd, SW_SHOWNOACTIVATE);
            }
        }
    }
}

/// 非 Windows 平台没有这层系统边框。
#[cfg(not(windows))]
mod imp {
    use super::{Rect, ScreenMetrics};
    use tauri::{PhysicalPosition, PhysicalSize, WebviewWindow};

    pub fn strip_os_frame(_window: &WebviewWindow) -> Result<(), String> {
        Ok(())
    }

    /// 非 Windows 拿不到全局键状态：一律当作「没按住」，落位判定退回按 move 事件间隔触发。
    pub fn mouse_down() -> bool {
        false
    }

    /// 非 Windows 没有"被系统藏起来而 Tauri 自己不知道"这条路（macOS 的隐藏是应用级的，
    /// Tauri 自己记账），于是退回 Tauri 的判据。
    pub fn os_state(window: &WebviewWindow) -> (bool, bool) {
        (
            window.is_visible().unwrap_or(true),
            window.is_minimized().unwrap_or(false),
        )
    }

    /// 非 Windows 没有等价的"逼一次重画"（macOS 由窗口服务器管，没有 Win32 那种残帧），空实现。
    pub fn force_repaint(_window: &WebviewWindow) {}

    pub fn force_show(window: &WebviewWindow) {
        let _ = window.show();
    }

    /// 非 Windows 用 Tauri 自己的前台请求（macOS 上是 `makeKeyAndOrderFront` 那一套）。
    pub fn ensure_foreground(window: &WebviewWindow) -> Result<(), String> {
        window.set_focus().map_err(|e| e.to_string())
    }

    pub fn metrics(window: &WebviewWindow) -> Result<ScreenMetrics, String> {
        let monitor = window
            .current_monitor()
            .map_err(|e| e.to_string())?
            .ok_or_else(|| "无法确定悬浮球所在显示器".to_string())?;
        let pos = monitor.position();
        let size = monitor.size();
        let rect = Rect { x: pos.x, y: pos.y, w: size.width as i32, h: size.height as i32 };
        Ok(ScreenMetrics { work_area: rect, monitor: rect, scale_factor: monitor.scale_factor() })
    }

    pub fn set_rect(window: &WebviewWindow, rect: Rect) -> Result<(), String> {
        window.set_position(PhysicalPosition::new(rect.x, rect.y)).map_err(|e| e.to_string())?;
        window.set_size(PhysicalSize::new(rect.w.max(1) as u32, rect.h.max(1) as u32)).map_err(|e| e.to_string())
    }

    /// 把窗口挪到「按下点仍在指针底下」的位置（非 Windows 用 Tauri 的窗口内指针位置）。
    pub fn drag_align(window: &WebviewWindow, press_x: f64, press_y: f64) -> Result<(), String> {
        let sf = window.scale_factor().map_err(|e| e.to_string())?;
        let pos = window.outer_position().map_err(|e| e.to_string())?;
        let cur = window.cursor_position().map_err(|e| e.to_string())?;
        let x = pos.x + (cur.x - press_x * sf).round() as i32;
        let y = pos.y + (cur.y - press_y * sf).round() as i32;
        window.set_position(PhysicalPosition::new(x, y)).map_err(|e| e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 一块 1920x1080 主屏 + 它左边一块 1280x1024（负坐标）的副屏。
    fn monitors() -> Vec<Rect> {
        vec![
            Rect { x: 0, y: 0, w: 1920, h: 1080 },
            Rect { x: -1280, y: 0, w: 1280, h: 1024 },
        ]
    }

    /// 复原判据的核心：落点在屏上就别拦，落在屏外（显示器拔了/分辨率变了）就该回居中。
    #[test]
    fn overlaps_only_inside_monitors() {
        let ms = monitors();
        // 主屏正中、主屏左下角（贴边收起：窗口贴住工作区边缘，仍在屏内）
        assert!(overlaps_any(Rect { x: 800, y: 400, w: 90, h: 90 }, &ms));
        assert!(overlaps_any(Rect { x: 0, y: 990, w: 90, h: 90 }, &ms));
        // 左侧副屏（负坐标）
        assert!(overlaps_any(Rect { x: -1280, y: 100, w: 90, h: 90 }, &ms));
        // 跨屏接缝：窗口一半在副屏、一半在主屏
        assert!(overlaps_any(Rect { x: -45, y: 100, w: 90, h: 90 }, &ms));
        // 副屏被拔掉之后：原来的落点整块在主屏左侧之外
        assert!(
            !overlaps_any(Rect { x: -1280, y: 100, w: 90, h: 90 }, &[ms[0]]),
            "只剩主屏时，记在副屏上的位置必须判为不可见（否则球复原到屏幕外，用户拖不回来）"
        );
        // 主屏分辨率变小（改成 1600x900）：原来的右下角落点已经出界
        let small = [Rect { x: 0, y: 0, w: 1600, h: 900 }];
        assert!(!overlaps_any(Rect { x: 1600, y: 100, w: 90, h: 90 }, &small), "右侧出界");
        assert!(!overlaps_any(Rect { x: 100, y: 900, w: 90, h: 90 }, &small), "下方出界");
        // 恰好贴边（无正面积交集）也算出界：窗口与显示器边界只相接时球一个像素都看不到
        assert!(!overlaps_any(Rect { x: 1920, y: 0, w: 90, h: 90 }, &[ms[0]]));
    }

    /// 默认落位在右上、垂直线 1/3 处，且右边余量大于前端吸附阈值（20 CSS px）——
    /// 否则首启那次落位判定会立刻把球吸到边上，「靠边但不吸附」当场失效。
    #[test]
    fn default_pos_is_upper_right_and_not_dockable() {
        let work = Rect { x: 0, y: 0, w: 1920, h: 1032 };
        let (x, y) = default_pos_in(work, 90, 24);
        assert_eq!(x, 1920 - 90 - 24, "窗口右边贴着工作区右边的内侧");
        assert!(work.x + work.w - (x + 90) > 20, "余量必须超过吸附阈值");
        assert!(y > work.y && y < work.y + work.h / 2, "垂直落在上半部的 1/3 处");

        // 工作区左上角不在原点（副屏 / 左侧任务栏）也按绝对坐标算
        let left = Rect { x: -1280, y: 0, w: 1280, h: 1024 };
        assert_eq!(default_pos_in(left, 90, 24), (-1280 + 1280 - 90 - 24, 1024 / 3));
    }

    /// 指纹只判"变没变"：拓扑或缩放动过就必须变（否则界面那份几何不会被重排），
    /// 同一份拓扑两次查询必须一样（否则看门狗每 2s 都会让界面白重排一次）。
    #[test]
    fn fingerprint_follows_topology_and_scale() {
        let base = monitors();
        let f = fingerprint_of(&base, 1.5);
        assert_eq!(f, fingerprint_of(&base, 1.5), "同一份拓扑两次查询必须一致");
        assert_ne!(f, fingerprint_of(&base, 1.0), "缩放变了要算变");
        assert_ne!(f, fingerprint_of(&base, 1.25), "缩放变了要算变（非线性缩放也一样）");
        // 远控的虚拟屏：多出来一块
        let mut extra = base.clone();
        extra.push(Rect { x: 1920, y: 0, w: 1080, h: 1920 });
        assert_ne!(f, fingerprint_of(&extra, 1.5), "多了一块屏要算变");
        // 分辨率变小 / 任务栏换边：工作区矩形变了
        assert_ne!(f, fingerprint_of(&[Rect { x: 0, y: 0, w: 1600, h: 900 }], 1.5), "工作区变了要算变");
        // 副屏被拔掉
        assert_ne!(f, fingerprint_of(&base[..1], 1.5), "少了一块屏要算变");
        assert_eq!(fingerprint_of(&[], 1.0), fingerprint_of(&[], 1.0), "空集合也得稳定");
    }

    /// 搬回工作区：挑最近的一块、夹进工作区；已经在某块屏上时一动不动。
    #[test]
    fn clamp_targets_the_nearest_work_area() {
        let main = Rect { x: 0, y: 0, w: 1920, h: 1032 };
        let left = Rect { x: -1280, y: 0, w: 1280, h: 984 };
        let areas = [main, left];
        // 贴右的球（实测落点就在 x≈1783 那一带）：主屏分辨率变成 1600 宽之后它整块出界
        let shrunk = [Rect { x: 0, y: 0, w: 1600, h: 900 }];
        let gone = Rect { x: 1830, y: 400, w: 90, h: 90 };
        assert!(!overlaps_any(gone, &shrunk), "分辨率变小之后它必须判为出界");
        let t = nearest_area(gone, &shrunk);
        assert_eq!((t.x, t.y), (shrunk[0].x, shrunk[0].y), "目标是剩下那块屏的工作区");
        assert_eq!(clamp_axis(gone.x, t.x, t.x + t.w - gone.w), 1600 - 90, "夹到工作区右缘内侧");
        // 副屏被拔掉：原落点落在主屏左侧之外（负坐标）→ 目标是主屏，夹回主屏左上
        let gone_left = Rect { x: -1280, y: 100, w: 90, h: 90 };
        assert!(!overlaps_any(gone_left, &[main]), "只剩主屏时它必须判为出界");
        let t2 = nearest_area(gone_left, &[main]);
        assert_eq!((t2.x, t2.y), (main.x, main.y));
        assert_eq!(clamp_axis(gone_left.x, t2.x, t2.x + t2.w - gone_left.w), 0);
        // 离左边那块副屏更近时，目标是副屏
        let off_left = Rect { x: -1400, y: 100, w: 90, h: 90 };
        let t3 = nearest_area(off_left, &areas);
        assert_eq!((t3.x, t3.y), (left.x, left.y), "离副屏更近时目标是副屏");
        // 工作区比窗口还小（极小屏/极端缩放）：取 min，不许推到另一侧去
        assert_eq!(clamp_axis(-5000, 10, -20), 10);
        assert_eq!(clamp_axis(600, 0, 1000), 600, "范围内原样不动");
    }
}