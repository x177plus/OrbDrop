//! 悬浮窗的「可点区域」：窗口比看得见的那件东西大一圈，那圈余量不该吃点击。
//!
//! 两种形态的窗口都是「悬浮体 + 四周余量」，余量留的是 `--shadow` 的可见半径
//! （`orbShape.RECT_SLACK` 左 14 / 上 10 / 右 16 / 下 18，面板是 `PANEL_SLACK` 20）。
//! 阴影是**墨迹**不是实体，可 Windows 上**透明像素照样算命中**：点那圈余量就是在点我们的窗口
//! —— 压在下面的窗口收不到这一下（连焦点都被抢走），悬停、右键一并吃掉。
//!
//! 让它穿透的路子试过两条：
//!
//! - `WM_NCHITTEST` 返回 `HTTRANSPARENT`：**不行**。`WindowFromPoint` 认它（实测返回的是下面
//!   那个窗口），但真实的鼠标消息不认 —— 跨线程/跨进程时消息照旧落在我们窗口上（文档里那句
//!   "in the same thread" 是字面意思）。实测：下面那个窗口一次 mousedown 都没收到。
//! - `WS_EX_TRANSPARENT`：**可行，但必须连 `WS_EX_LAYERED` 一起加**。只加前者，点击照样被吃掉；
//!   两个一起加，指针底下那个窗口就收得到（实测两个点位的 mousedown 都落到了下面那个窗口）。
//!   这正是 Tauri 的 `set_ignore_cursor_events` —— 所以这里用它，不自己写窗口过程（tao 的
//!   窗口样式缓存得跟着一起变，绕过去加样式，下次它自己改样式时会被冲掉）。
//!   加 `WS_EX_LAYERED` 不影响画面：改前/改后抓屏逐像素比过，完全一致（窗口本来就是 DWM 合成
//!   的那套，不是老式 GDI 分层）。
//!
//! 这两样都是**整窗**级的，所以只能按指针位置开关：指针在「可点区域」里 → 关掉穿透，在余量里
//! → 打开穿透。而判定只能在这边轮询 —— 窗口一旦穿透，页面就收不到 mousemove 了，前端无从知道
//! 指针什么时候回来（拿 `WindowFromPoint`/LL 钩子去补，代价与复杂度都不划算）。
//!
//! 三件事决定了这里的写法：
//!
//! - **轮询不能停**：两个方向都得由我们发起，停掉的那一刻指针若在余量里，窗口就一直穿透着
//!   （连悬浮球都点不着）。间隔 20ms：人从余量移进悬浮体再按下主键远不止这个数；而空闲时
//!   这就是唯一的心跳 —— `GetCursorPos` + 一次区间判定，量级是微秒（N-04「CPU 空闲近 0」）。
//! - **主键按住时一律不穿透**：OS 的窗口拖动（`WM_NCLBUTTONDOWN` 的模态循环）期间，落位判定、
//!   「球滑回完整位」这些动作会让可点区域当场变样（`resetOrbDom` 把热区收回悬浮体本身）；
//!   按下那一刻起就不该再按区域判定 —— 否则拖动中途指针只要跨出区域一次，窗口就吃不到鼠标，
//!   拖着的东西当场卡住。按下与抬起也要落在同一个窗口上（否则 `Orb.svelte` 的 `pressed`
//!   会卡住），这条一并管住。
//! - **区域由前端给**（`orb_set_hit_region`，CSS px、窗口左上角为原点）：只有它知道当前该吃
//!   指针的是哪几个盒子 —— 折叠期是悬停热区（`--zone-*`），面板期是面板（右键菜单是
//!   `position: fixed` 且夹取按窗口算，可能压在那圈余量上，得一并报上来）。这边只做
//!   「点在不在其中」的判定，并按当前 DPI 把 CSS px 换算成物理像素 —— 区域不随 DPI 变化，
//!   换显示器/改缩放都不用重报。
//!
//! 非 Windows 不动（见下面 `imp`）：macOS 那边是整窗级的 `ignoresMouseEvents`，做不到「只让
//! 余量穿透」，于是余量照旧吃点击 —— 与改造前一致。

use parking_lot::Mutex;
use serde::Deserialize;
use std::sync::atomic::AtomicBool;
use std::sync::Once;
use std::time::Duration;
use tauri::WebviewWindow;

/// 一块「可点区域」（CSS px，窗口左上角为原点）。
#[derive(Debug, Clone, Copy, Deserialize)]
pub struct RegionRect {
    pub x: f64,
    pub y: f64,
    pub w: f64,
    pub h: f64,
}

/// 指针位置轮询间隔（见文件头：这是空闲时唯一的心跳）。
const POLL: Duration = Duration::from_millis(20);

/// 当前可点区域（CSS px）。**空 = 还没接上**（界面还没报过来，或异常）：一律按整窗可点处理，
/// 退回改造前的行为 —— 区域为空就整窗穿透的话，悬浮球自己都点不着。
static REGION: Mutex<Vec<RegionRect>> = Mutex::new(Vec::new());
/// 窗口此刻是不是穿透态（= `set_ignore_cursor_events` 的当前值）：只在真的变了才下发。
static IGNORING: AtomicBool = AtomicBool::new(false);
/// 轮询线程只起一次。
static POLLER: Once = Once::new();

/// 更新可点区域（界面每次几何/状态变化时调用），并按需启动轮询线程。
pub fn set_region(window: &WebviewWindow, rects: Vec<RegionRect>) -> Result<(), String> {
    *REGION.lock() = rects;
    POLLER.call_once(|| imp::start(window.clone()));
    Ok(())
}

/// 点 `(x, y)` 在不在区域里（CSS px，半开区间：右下边界不算在里）。
fn inside(rects: &[RegionRect], x: f64, y: f64) -> bool {
    rects
        .iter()
        .any(|r| x >= r.x && x < r.x + r.w && y >= r.y && y < r.y + r.h)
}

#[cfg(windows)]
mod imp {
    use super::{inside, IGNORING, POLL, REGION};
    use std::ffi::c_void;
    use std::sync::atomic::Ordering;
    use tauri::WebviewWindow;
    use tracing::warn;
    use windows::Win32::Foundation::{HWND, POINT};
    use windows::Win32::Graphics::Gdi::ScreenToClient;
    use windows::Win32::UI::HiDpi::GetDpiForWindow;
    use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

    pub fn start(window: WebviewWindow) {
        let Ok(hwnd) = window.hwnd() else {
            warn!("拿不到窗口句柄，点击穿透不生效");
            return;
        };
        // 句柄按 `isize` 过线程边界：窗口句柄本身没有生命周期问题（窗口关了就失效，
        // 那之后这里读到的点也一律算「在里面」，不会误开穿透）。
        let raw = hwnd.0 as isize;
        std::thread::spawn(move || loop {
            std::thread::sleep(POLL);
            let hwnd = HWND(raw as *mut c_void);
            let want = !(live(hwnd) || crate::orb_window::mouse_down());
            if want != IGNORING.load(Ordering::Relaxed) {
                match window.set_ignore_cursor_events(want) {
                    Ok(()) => IGNORING.store(want, Ordering::Relaxed),
                    Err(e) => warn!("切换窗口点击穿透失败: {e}"),
                }
            }
        });
    }

    /// 指针此刻是否落在可点区域里。
    ///
    /// 读不到指针（句柄失效/API 失败）就当「在里面」：维持窗口可点的现状，比把窗口点不着强。
    /// 区域还没报来时同理（见 `REGION` 的说明）。
    fn live(hwnd: HWND) -> bool {
        let mut cur = POINT::default();
        unsafe {
            if GetCursorPos(&mut cur).is_err() || !ScreenToClient(hwnd, &mut cur).as_bool() {
                return true;
            }
            let region = REGION.lock();
            if region.is_empty() {
                return true;
            }
            let dpi = GetDpiForWindow(hwnd);
            let sf = if dpi == 0 { 1.0 } else { f64::from(dpi) / 96.0 };
            inside(&region, f64::from(cur.x) / sf, f64::from(cur.y) / sf)
        }
    }
}

/// 非 Windows：没有「只让余量穿透」的对应机制（macOS 的 `ignoresMouseEvents` 是整窗级的），
/// 于是什么都不做 —— 余量照旧吃点击，与改造前一致。
#[cfg(not(windows))]
mod imp {
    use tauri::WebviewWindow;

    pub fn start(_window: WebviewWindow) {}
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(x: f64, y: f64, w: f64, h: f64) -> RegionRect {
        RegionRect { x, y, w, h }
    }

    /// 区域是半开区间（右下边界不算），多块矩形取并集 —— 折叠期的热区与面板期的
    /// 「面板 + 右键菜单」都靠这一条。
    #[test]
    fn inside_is_half_open_and_unions_rects() {
        let rects = [r(20.0, 20.0, 448.0, 620.0), r(8.0, 300.0, 172.0, 120.0)];
        assert!(inside(&rects, 20.0, 20.0), "左上角在内");
        assert!(inside(&rects, 467.9, 639.9), "右下内侧在内");
        assert!(!inside(&rects, 468.0, 640.0), "右下边界算在外");
        // 两个"在外"的点必须落在**并集**之外：面板那块（448×620）把第二块矩形的整片
        // 区域都盖住了，所以"面板左缘外 1px"（x=19）和"第二块矩形右缘外"（x=180, y=419）
        // 其实都在面板里 —— 并集判定当然算它们在内。左外侧取两块都够不着的 x，
        // 右外侧取面板上沿之上（y < 20）那一行。
        assert!(!inside(&rects, 7.0, 300.0), "面板左外侧不在内");
        assert!(inside(&rects, 8.0, 300.0), "第二块矩形自己那块在内");
        assert!(inside(&rects, 179.0, 419.0), "第二块矩形右下内侧在内");
        assert!(!inside(&rects, 180.0, 10.0), "第二块矩形右侧在外");
        assert!(!inside(&[], 0.0, 0.0), "空区域什么都不算在内");
    }
}