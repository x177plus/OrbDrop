//! 托盘图标：唯一的窗口外入口。
//!
//! 悬浮球窗口已经不在任务栏露面（`tauri.conf.json` 的 `skipTaskbar`），托盘于是成了这台
//! 进程在"窗口之外"的唯一把手：右键菜单里必须有退出，否则用户只能去任务管理器杀进程
//! （悬浮球无边框，连个关闭按钮都没有）。

use tauri::menu::{Menu, MenuItem};
use tauri::AppHandle;
use tracing::info;

use crate::log;

/// 配置里写了 `trayIcon`，Tauri 在 `Builder::build()` 阶段就把它建出来了；id 没指定时
/// 默认 `main`（tauri `app.rs`：`tray_config.id.clone().unwrap_or_else(|| "main".into())`）。
/// 这里取的就是同一个 id —— 改配置里的 `trayIcon.id` 必须同步改这里。
const TRAY_ID: &str = "main";

/// 退出菜单项的 id，只在建菜单与认事件之间用。
const MENU_QUIT: &str = "quit";

/// 给配置里那个托盘图标挂上右键菜单（退出）。
///
/// 不重建图标：`TrayIconBuilder` 那条路要自己把配置里的图标、tooltip 重新加载一遍，
/// 而托盘 id 是稳定的，直接取回来挂菜单即可。
pub fn setup(app: &AppHandle) -> Result<(), String> {
    let quit = MenuItem::with_id(app, MENU_QUIT, "退出", true, None::<&str>)
        .map_err(|e| format!("建托盘菜单项失败: {e}"))?;
    let menu = Menu::with_items(app, &[&quit]).map_err(|e| format!("建托盘菜单失败: {e}"))?;

    let tray = app.tray_by_id(TRAY_ID).ok_or_else(|| {
        format!("未找到 id={TRAY_ID} 的托盘图标（tauri.conf.json 里还有 trayIcon 吗？）")
    })?;
    tray.set_menu(Some(menu))
        .map_err(|e| format!("给托盘挂菜单失败: {e}"))?;

    // 菜单事件监听器是**全局**的（Tauri 把任何菜单的事件都往这一组监听器里送），
    // 应用里目前只有托盘这一个菜单，所以按 id 认一下就够。
    tray.on_menu_event(|app, event| {
        if event.id() == MENU_QUIT {
            info!("托盘菜单：退出 OrbDrop");
            // `exit` 最后是 `std::process::exit`，静态量不 drop、日志不会自己刷盘，
            // 所以上面那行"谁把进程关掉的"必须先落盘再走。
            log::flush();
            app.exit(0);
        }
    });

    Ok(())
}