//! 托盘图标与它的右键菜单：唯一的窗口外入口，也是「在线 / 离线」的第二处开关。
//!
//! 悬浮球窗口不在任务栏露面（`tauri.conf.json` 的 `skipTaskbar`），托盘于是成了这台进程在
//! "窗口之外"的唯一把手。菜单形态（见 `docs/p1-implementation.md` §16）：
//!
//! ```text
//! ● 状态 ▸              ← 状态头：标题只说明"这行是什么"（名词），状态由点色给
//!      ✔ 在线           ← 子项是**选项列表**，原生勾选 = 当前是哪个
//!        离线
//! ──────────────
//!     退出
//! ```
//!
//! 状态头不写状态名（点已经在说同一件事，子菜单里还有一遍勾选）；托盘图标与 tooltip
//! 也都带着状态，所以这一行可以只留"名词 + 点"。
//!
//! 为什么把"头"做成子菜单标题、而不是一行禁用的菜单项：Windows 只把菜单项的**文字**画灰，
//! 位图不跟着灰（muda 走原生 `MIIM_BITMAP`，不做 owner-draw），"灰字 + 鲜绿点"是两套打架
//! 的信号；而且禁用行点了没反应，是个死点击。子菜单标题永远正常色、点它=展开，没有这两个问题。
//!
//! 状态本身在**不打开菜单**时也看得见：托盘图标离线时整颗转灰、tooltip 写 `OrbDrop · 离线`。
//! 所以菜单里那两行可以纯当选项，不存在"这行在告诉我什么"的歧义。

use std::sync::{Arc, Mutex};

use tauri::image::Image;
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu};
use tauri::tray::TrayIcon;
use tauri::{AppHandle, Listener, Manager};
use tracing::{info, warn};

use crate::log;
use crate::state::AppState;
use crate::ws::events::ui::LOCAL_ONLINE;

/// 配置里写了 `trayIcon`，Tauri 在 `Builder::build()` 阶段就把它建出来了；id 没指定时
/// 默认 `main`（tauri `app.rs`：`tray_config.id.clone().unwrap_or_else(|| "main".into())`）。
/// 这里取的就是同一个 id —— 改配置里的 `trayIcon.id` 必须同步改这里。
const TRAY_ID: &str = "main";

// 菜单项 id：只在建菜单与认事件之间用。
const MENU_PRESENCE: &str = "presence";
const MENU_ONLINE: &str = "online";
const MENU_OFFLINE: &str = "offline";
const MENU_QUIT: &str = "quit";

/// 状态头的标题：**固定文案，不随状态变** —— 状态由那颗点的颜色说。
/// 顺带 `refresh` 里少一次 `set_text`（标题永远不变，就没有要改的东西）。
const TITLE_PRESENCE: &str = "状态";

/// 在线点：面板 `--ok`（`src/styles/main.css` 的 `oklch(58% 0.16 158)`）换算到 sRGB。
const DOT_ONLINE: (u8, u8, u8) = (0x00, 0x95, 0x54);
/// 离线点：面板 `--muted`（`oklch(54% 0.012 250)`）同一个灰。
///
/// 刻意**不**用面板里那枚「离线」点（那里是 `color-mix(in oklch, var(--fg) 22%, transparent)`）：
/// 那个值是为面板自己的浅色底配的，而菜单底色跟随**系统浅/深主题** —— 一个几乎透明的深灰
/// 放到深色菜单上会直接消失。中灰在白底、黑底上都看得见。
const DOT_OFFLINE: (u8, u8, u8) = (0x6a, 0x6f, 0x76);

/// 点位在菜单里的逻辑边长（Windows 的菜单图标位就是 16px），点直径取它的一半。
const DOT_SLOT: f64 = 16.0;
const DOT_RATIO: f64 = 0.5;

/// 菜单项句柄：状态一变（不管从托盘还是面板）要用它们**就地**改标题/勾选/点色。
/// 重建菜单也行，但 Linux 上 `set_menu` 之后换不了菜单，就地改是唯一到处都对的路子。
///
/// `Submenu` / `CheckMenuItem` 没有 `default_runtime` 别名（只有 `TrayIcon` 有），
/// 所以这里显式带上运行时参数 —— 本应用只有 Wry 一个运行时。
#[derive(Clone)]
struct TrayHandles {
    tray: TrayIcon,
    presence: Submenu<tauri::Wry>,
    online: CheckMenuItem<tauri::Wry>,
    offline: CheckMenuItem<tauri::Wry>,
}

/// 句柄存放处。`manage` 要求 `Send + Sync + 'static`：这几个类型都是引用计数的句柄，
/// Tauri 对它们有 `unsafe impl Send/Sync`（都只在主线程上用）。
static HANDLES: Mutex<Option<TrayHandles>> = Mutex::new(None);

/// 取一份句柄的**副本**（不是锁）。
///
/// 绝不能把 guard 带到菜单调用之后：`set_text`/`set_checked`/`set_icon` 内部是
/// `run_on_main_thread` + 等回执，而刷新的触发点既可能在主线程（托盘点击）、也可能在
/// 别的线程（命令 → 发事件 → 监听器同步跑）。握着锁等主线程 = 主线程反过来等这把锁
/// 就死锁了。句柄本身是引用计数的，复制一份的代价可以忽略。
fn handles() -> Option<TrayHandles> {
    HANDLES.lock().ok().and_then(|slot| slot.clone())
}

/// 给配置里那个托盘图标挂上右键菜单（状态头 + 退出）。
///
/// 不重建图标：`TrayIconBuilder` 那条路要自己把配置里的图标、tooltip 重新加载一遍，
/// 而托盘 id 是稳定的，直接取回来挂菜单即可。
pub fn setup(app: &AppHandle) -> Result<(), String> {
    let online_now = local_online(app);

    // 状态头：标题只说明"这行是什么"（名词），**状态由那颗点的颜色给** ——
    // 子菜单里还有一遍勾选，标题再写一遍状态名纯属重复（也把"状态/动作"的歧义请回来）。
    let presence = Submenu::with_id_and_icon(
        app,
        MENU_PRESENCE,
        TITLE_PRESENCE,
        true,
        Some(dot_image(for_state(online_now), app)),
    )
    .map_err(|e| format!("建状态子菜单失败: {e}"))?;

    let online = CheckMenuItem::with_id(app, MENU_ONLINE, "在线", true, online_now, None::<&str>)
        .map_err(|e| format!("建「在线」菜单项失败: {e}"))?;
    let offline = CheckMenuItem::with_id(app, MENU_OFFLINE, "离线", true, !online_now, None::<&str>)
        .map_err(|e| format!("建「离线」菜单项失败: {e}"))?;
    presence
        .append_items(&[&online, &offline])
        .map_err(|e| format!("状态子菜单装配失败: {e}"))?;

    let sep = PredefinedMenuItem::separator(app).map_err(|e| format!("建分隔线失败: {e}"))?;
    let quit = MenuItem::with_id(app, MENU_QUIT, "退出", true, None::<&str>)
        .map_err(|e| format!("建托盘菜单项失败: {e}"))?;
    let menu = Menu::with_items(app, &[&presence, &sep, &quit])
        .map_err(|e| format!("建托盘菜单失败: {e}"))?;

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
            // `exit` 最后是 `std::process::exit`：静态量不 drop、日志不会自己刷盘，
            // 所以上面那行"谁把进程关掉的"必须先落盘再走。
            log::flush();
            app.exit(0);
            return;
        }
        let online = match event.id().as_ref() {
            MENU_ONLINE => true,
            MENU_OFFLINE => false,
            // 状态头是子菜单标题，点它只展开菜单，不会走到这里
            _ => return,
        };
        let arc = local_state(app);
        if let Err(e) = crate::apply_online(&arc, app, online) {
            warn!("托盘切换在线状态失败: {e}");
        }
        // **无条件**重刷：状态没变时 `apply_online` 早退、也就不会发事件，而 muda 的
        // Check 项被点时会**先自己反选**（`menu_selected` 里 `set_checked(!item.checked)`）
        // 才派发事件 —— 不重刷的话，点"当前那一项"会留下一个假的空勾。
        refresh(app);
    });

    // 面板右上角那个状态框也能切状态（`set_online` 命令）。不跟着它刷新的话，托盘菜单
    // 会停在旧状态上 —— 同一份状态，两处显示必须一起动（`emit` 同时投递给 webview 与
    // Rust 监听器，所以这里收得到）。
    let listen_app = app.clone();
    let _ = app.listen(LOCAL_ONLINE, move |_| refresh(&listen_app));

    if let Ok(mut slot) = HANDLES.lock() {
        *slot = Some(TrayHandles { tray, presence, online, offline });
    }
    // 初次落位：图标/tooltip/勾选按真值刷一遍。离线是**落盘状态**，重启后如果仍是离线，
    // 托盘在启动那一刻就该是灰的 —— 不能等用户第一次切换。
    refresh(app);
    Ok(())
}

fn local_state(app: &AppHandle) -> Arc<AppState> {
    app.state::<Arc<AppState>>().inner().clone()
}

fn local_online(app: &AppHandle) -> bool {
    app.state::<Arc<AppState>>().is_online()
}

/// 把状态点、勾选、托盘图标、tooltip 一起刷成当前真值的样子。
///
/// 一律从 `state.is_online()` 现读，不信任何一处缓存：这条路径同时服务"托盘切的"、
/// "面板切的"与"启动时是什么"三种来源，只有真值不会分叉。标题是固定文案，不用改。
fn refresh(app: &AppHandle) {
    let Some(h) = handles() else { return };
    let online = local_online(app);

    // 就地改：状态头的点色；两个子项用原生勾选表示"当前是哪个"
    if let Err(e) = h.presence.set_icon(Some(dot_image(for_state(online), app))) {
        warn!("托盘状态点更新失败: {e}");
    }
    if let Err(e) = h
        .online
        .set_checked(online)
        .and_then(|()| h.offline.set_checked(!online))
    {
        warn!("托盘勾选更新失败: {e}");
    }
    if let Err(e) = h.tray.set_icon(Some(tray_icon(online))) {
        warn!("托盘图标更新失败: {e}");
    }
    if let Err(e) = h.tray.set_tooltip(Some(tooltip_text(online))) {
        warn!("托盘提示更新失败: {e}");
    }
}

fn for_state(online: bool) -> (u8, u8, u8) {
    if online {
        DOT_ONLINE
    } else {
        DOT_OFFLINE
    }
}

fn tooltip_text(online: bool) -> &'static str {
    if online {
        "OrbDrop · 在线"
    } else {
        "OrbDrop · 离线（对局域网隐身）"
    }
}

/// 托盘图标：在线 = `tauri.conf.json` 的 `trayIcon.iconPath` 同一张图；离线 = 整颗转灰
/// （与球心 logo 在离线时转 `--muted` 是同一套语言，形状不动、只换色）。
///
/// 两张都是**编译期**解码成 RGBA（`include_image!` 走 tauri-codegen），所以运行时不需要
/// `image-png` feature；代价是这里必须与配置里的 iconPath 保持一致。
fn tray_icon(online: bool) -> Image<'static> {
    if online {
        tauri::include_image!("icons/32x32.png")
    } else {
        tauri::include_image!("icons/32x32-offline.png")
    }
}

/// 画一颗实心圆点当菜单图标。
///
/// 尺寸按窗口 DPI 缩放（菜单图标位是逻辑 16px，1:1 画出来在 125%/150% 上会小一圈）。
fn dot_image(rgb: (u8, u8, u8), app: &AppHandle) -> Image<'static> {
    let scale = app
        .get_webview_window("main")
        .and_then(|w| w.scale_factor().ok())
        .unwrap_or(1.0);
    let slot = dot_slot(scale);
    Image::new_owned(dot_rgba(rgb, slot), slot, slot)
}

/// 菜单图标位的物理边长：逻辑 16px × 缩放。下限 8px —— 缩放出个极小的值时，
/// 半径会小到一颗都画不出来（`radius < 1` 时圆心判定恒假），宁可略大。
fn dot_slot(scale: f64) -> u32 {
    (DOT_SLOT * scale).round().max(8.0) as u32
}

/// 圆点的 RGBA 缓冲：点居中、直径 = 边长的一半，边缘 4×4 超采样抗锯齿 ——
/// 就十几像素的东西，硬边一眼能看出锯齿。
fn dot_rgba(rgb: (u8, u8, u8), slot: u32) -> Vec<u8> {
    const SUB: u32 = 4;
    let radius = slot as f64 * DOT_RATIO / 2.0;
    let mid = slot as f64 / 2.0;

    let mut rgba = Vec::with_capacity((slot * slot * 4) as usize);
    for y in 0..slot {
        for x in 0..slot {
            let mut inside = 0;
            for sy in 0..SUB {
                for sx in 0..SUB {
                    let px = x as f64 + (sx as f64 + 0.5) / SUB as f64 - mid;
                    let py = y as f64 + (sy as f64 + 0.5) / SUB as f64 - mid;
                    if px * px + py * py <= radius * radius {
                        inside += 1;
                    }
                }
            }
            rgba.extend_from_slice(&[rgb.0, rgb.1, rgb.2, (inside * 255 / (SUB * SUB)) as u8]);
        }
    }
    rgba
}

#[cfg(test)]
mod tests {
    use super::*;

    fn alpha_at(buf: &[u8], slot: u32, x: u32, y: u32) -> u8 {
        buf[((y * slot + x) * 4 + 3) as usize]
    }

    /// 菜单图标位必须跟着 DPI 走：不缩放的话在 125%/150% 上点会小一圈；
    /// 极小/异常的缩放值要落在下限上，而不是画出一颗看不见的点（半径为 0 → 全透明）。
    #[test]
    fn dot_slot_follows_dpi_and_keeps_a_floor() {
        assert_eq!(dot_slot(1.0), 16);
        assert_eq!(dot_slot(1.25), 20);
        assert_eq!(dot_slot(1.5), 24);
        assert_eq!(dot_slot(0.1), 8, "退化缩放走下限，别产出半径 0 的空图");
    }

    /// 缓冲区必须正好是边长的 RGBA 方阵，圆心不透明、四角全透明、上色用的是给的那个色。
    #[test]
    fn dot_is_centred_opaque_and_leaves_the_corners_empty() {
        let slot = 16;
        let buf = dot_rgba(DOT_ONLINE, slot);
        assert_eq!(buf.len(), (slot * slot * 4) as usize, "必须是 slot² 的 RGBA");

        let mid = slot / 2;
        assert_eq!(alpha_at(&buf, slot, mid, mid), 255, "圆心应完全不透明");
        assert_eq!(&buf[((mid * slot + mid) * 4) as usize..][..3], &[DOT_ONLINE.0, DOT_ONLINE.1, DOT_ONLINE.2]);
        for (x, y) in [(0, 0), (slot - 1, 0), (0, slot - 1), (slot - 1, slot - 1)] {
            assert_eq!(alpha_at(&buf, slot, x, y), 0, "四角在圆外");
        }
    }

    /// 边缘是软的（抗锯齿），而点的大小就是它声称的直径：
    /// 半透明像素必须存在，且**覆盖度之和**（α/255 累加）应接近直径 `slot/2` 的圆面积
    /// —— 只数"完全不透明"的像素会漏掉整圈软边，比圆面积明显偏小。
    #[test]
    fn dot_edge_is_antialiased_and_area_matches_its_diameter() {
        let slot = 16;
        let buf = dot_rgba(DOT_OFFLINE, slot);
        let alphas: Vec<u8> = (0..slot * slot).map(|i| buf[(i * 4 + 3) as usize]).collect();
        assert!(
            alphas.iter().any(|&a| a > 0 && a < 255),
            "边缘应有半透明像素（否则就是硬边）"
        );

        let coverage: f64 = alphas.iter().map(|&a| f64::from(a) / 255.0).sum();
        let r = slot as f64 * DOT_RATIO / 2.0;
        let area = std::f64::consts::PI * r * r;
        assert!(
            (coverage - area).abs() < area * 0.08,
            "覆盖度之和 {coverage} 应接近半径 {r} 的圆面积 {area}"
        );
    }
}