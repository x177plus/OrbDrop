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
    use windows::Win32::Foundation::RECT;
    use windows::Win32::Graphics::Gdi::{
        GetMonitorInfoW, MonitorFromWindow, MONITOR_DEFAULTTONEAREST, MONITORINFO,
    };
    use windows::Win32::UI::HiDpi::GetDpiForWindow;
    use windows::Win32::UI::WindowsAndMessaging::{SetWindowPos, SWP_NOACTIVATE, SWP_NOZORDER};

    fn to_rect(r: RECT) -> Rect {
        Rect { x: r.left, y: r.top, w: r.right - r.left, h: r.bottom - r.top }
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
}

/// 非 Windows 平台没有这层系统边框。
#[cfg(not(windows))]
mod imp {
    use super::{Rect, ScreenMetrics};
    use tauri::{PhysicalPosition, PhysicalSize, WebviewWindow};

    pub fn strip_os_frame(_window: &WebviewWindow) -> Result<(), String> {
        Ok(())
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
}