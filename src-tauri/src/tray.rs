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
//!
//! 左键与它上面那颗红点（F-03 的另一半）：
//!
//! ```text
//! 左键单击   有事等处理 → 落到"该看的那一件事"上（未读会话 / 群邀请 / 通知气泡）
//!            没有事      → 什么都不做
//! 左键双击   打开主界面（面板展开 —— 与"第二份进程被启动"同一条路）
//! 右键       菜单（状态头 + 在线/离线 + 退出）
//! ```
//!
//! **托盘只回答"有没有事"和"左键被点了"**：`tray_set_alert` 收一个布尔（有事 / 没事），
//! 有事时图标**整帧闪动**（亮拍 = 底图 + 右上角一颗红点，灭拍 = 一张看不见的空白；见
//! `TrayFrame`）；左键真被点了就发一条 `orb://tray_alert` 给界面，
//! 由界面自己决定落到哪个窗口 —— "是哪一件事"（未读消息 / 群邀请 / 待确认通知 / 新版本）
//! 只活在界面那一层，后端再存一份就是两处真相，迟早分叉。
//!
//! 那个布尔与**悬浮球左上角那颗角标是同一个数**（界面 `orbNotifCount`）：球上有数 ⇔ 托盘上
//! 有红点。事情处理完（读过消息、点掉通知）角标掉回 0，红点与闪动自己就停了。

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, LazyLock, Mutex};
use std::time::{Duration, Instant};

use tauri::image::Image;
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu};
use tauri::tray::{MouseButton, MouseButtonState, TrayIcon, TrayIconEvent};
use tauri::{AppHandle, Emitter, Listener, Manager};
use tracing::{info, warn};

use crate::log;
use crate::state::AppState;
use crate::ws::events::ui::{LOCAL_ONLINE, ORB_SHOW, TRAY_ALERT};

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

/// 有事那一档那颗红点的颜色：**独立的一支红**。
///
/// 不取面板里的 `--notif` 橙（那是面板自己浅底上的角标色），也不借 `--danger`（那是面板里
/// "关系出问题"的语义色，红点说的是另一件事）。红点画在**任务栏**上：那里只有一种共识
/// （红 = 有提醒），底色还随系统浅/深主题变，所以取一支明度顶到 62% 的红
/// （`oklch(62% 0.21 25)` ≈ `#ea3c3f`）—— 深底浅底上都跳得出来。
const DOT_ALERT: (u8, u8, u8) = (0xea, 0x3c, 0x3f);
/// 红点直径 / 画布边长；以及它离画布右上角两条边的留白比例。
const BADGE_RATIO: f64 = 0.44;
const BADGE_MARGIN: f64 = 0.03;

/// 闪动的一拍：亮 → 空白 → 亮 …… 每拍 500ms（一秒一个来回）。
///
/// "灭"是**整张空白**（图标消失一拍），不是"红点自己一明一灭"—— 见 `TrayFrame`。
const BLINK_MS: u64 = 500;
/// 双击判定的窗口：两次"左键抬起"的间隔 ≤ 它就算双击。
///
/// 系统默认的双击间隔是 500ms（用户可调），这里取 400ms —— 托盘那两下是"连点两下"，
/// 比列表里双击条目更随意；窗口放宽到系统值反而会把"点两下但没想成双击"也吃进来。
const DOUBLE_CLICK_WINDOW: Duration = Duration::from_millis(400);

/// 有事等处理吗（未读消息 / 群邀请 / 待确认通知 / 在途推送 / 发现新版本）。**界面说了算**
/// （`tray_set_alert`）：它与悬浮球左上角那颗角标是同一个数，两处显示一起动。
static ALERT: AtomicBool = AtomicBool::new(false);
/// 闪动此刻在哪一拍：`true` = 亮（"底图 + 红点"），`false` = 灭（**整张空白帧**）。
/// `ALERT` 为假时一律按底图画。
///
/// 图标是**每一拍现算**的（`paint_tray_icon` 同时读 `ALERT` 与 `LIT`），所以"刚关上灯又有一拍
/// 冲进来"这种时序缝不会留下一张错的帧：那时候 `ALERT` 已经是假，算出来就是底图。
static LIT: AtomicBool = AtomicBool::new(false);
/// 闪动线程的代号：状态每变一次 +1，旧线程看到代号变了就自己退场（换挡不能有两个线程同闪）。
static GEN: AtomicU64 = AtomicU64::new(0);
/// 上一次"左键抬起"的时刻（连点判定用）。只装一个 `Instant`，取值即放 —— 不做任何跨线程等待。
static LAST_LEFT_UP: Mutex<Option<Instant>> = Mutex::new(None);

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
    // 左键不再是"打开菜单"：菜单搬到右键，左键留给"单击看事 / 双击开面板"（见文件头）。
    // tray-icon 的默认值是**左键也弹菜单**（`menu_on_left_click: true`），不显式关掉的话
    // 点一下左键会弹出一个菜单，那个菜单还会挡在双击判定前面。
    tray.set_show_menu_on_left_click(false)
        .map_err(|e| format!("关闭左键弹菜单失败: {e}"))?;

    // 左键抬起 → 单击 / 双击（自己按时间差判，见 `on_left_up`）。菜单事件另有一个监听器，
    // 两者互不影响：菜单只在右键弹、也从右键里来。
    tray.on_tray_icon_event(|tray, event| {
        if let TrayIconEvent::Click { button: MouseButton::Left, button_state: MouseButtonState::Up, .. } = event {
            on_left_up(tray.app_handle());
        }
    });

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

// ── 左键：单击看事、双击开面板 ──────────────────────────────────────────

/// 左键抬起：按与上一次抬起的间隔分出"单击 / 双击"，再各办各的事。
///
/// **不用 `TrayIconEvent::DoubleClick`**：托盘回调窗口的窗口类没带 `CS_DBLCLKS`
/// （tray-icon 的 `RegisterClassW` 里 `style = 0`），Explorer 送不送 `WM_LBUTTONDBLCLK` 不由我们
/// 决定。按两次"抬起"的时间差认，送不送那条消息都得到同一个结果 —— 而实测这台机器上它是**会**送
/// 的（顺序是 Down / Up / DoubleClick / Up）：我们只认抬起，那条 `DoubleClick` 落在第二次抬起
/// 之前、被忽略掉，所以**不会**重复触发一次开面板。
///
/// 单击**不等**（不能为了区分双击先压 400ms 再做）：有事的时候点一下要当场落地。代价是
/// 双击时第一下已经按单击办过一次事 —— 而单击做的正是"打开该看的那件事"，紧接着的
/// `orb://show` 只是把同一个面板展开，用户看到的仍是"主界面打开了"。
fn on_left_up(app: &AppHandle) {
    let now = Instant::now();
    let double = take_double_click(&LAST_LEFT_UP, now);

    if double {
        // 与"第二份进程被启动"同一条路（`lib.rs` 的单实例分支）：界面收到就展开面板；
        // 面板已经开着时那次展开是空操作（`expand` 一进门就早退）。
        info!("托盘：左键双击 → 展开面板");
        if let Err(e) = app.emit(ORB_SHOW, ()) {
            warn!("托盘双击的展开事件发不出去: {e}");
        }
        return;
    }

    // 单击：有事才动。没有事的时候什么都不做 —— "打开主界面"是双击那一档的事
    // （这正是这次改动要的：左键从"弹菜单"变成"单击看事、双击开面板"）。
    if ALERT.load(Ordering::SeqCst) {
        info!("托盘：左键单击 → 界面落到该看的那件事上");
        if let Err(e) = app.emit(TRAY_ALERT, ()) {
            warn!("托盘单击的事件发不出去: {e}");
        }
    }
}

/// 这一次"左键抬起"是双击吗？顺带把连点状态往前推一格。
///
/// 规则只有两条：与**上一次**抬起相差不超过 `DOUBLE_CLICK_WINDOW` 就是双击；双击之后把基准
/// **清零**（否则连点第三下会被算成又一次双击，而不是新的第一下）。
///
/// 拿走锁只为了读写那一个 `Instant`，**绝不在持有它的时候做任何跨线程调用**（见 `handles` 那段
/// 同一个道理）；也正因为这样，这条判定能被单独测（见 `third_click_starts_over`）。
fn take_double_click(slot: &Mutex<Option<Instant>>, now: Instant) -> bool {
    let Ok(mut last) = slot.lock() else {
        // 锁中毒（只可能是别的线程在持锁时 panic）：按单击处理，别把托盘整个卡住。
        return false;
    };
    let double = last.is_some_and(|t| now.duration_since(t) <= DOUBLE_CLICK_WINDOW);
    *last = if double { None } else { Some(now) };
    double
}

// ── 有事那一档：红点 + 闪动 ────────────────────────────────────────────

/// 界面告诉托盘：此刻**有没有事等处理**（未读消息 / 群邀请 / 待确认通知 / 在途推送 / 新版本）。
///
/// 只收一个布尔：托盘要的是"挂不挂红点、闪不闪"，"是哪一件事"由界面在单击那一刻自己决定
/// （见 `TRAY_ALERT`）。同一个值重复上报直接早退 —— 界面那边的 `$effect` 每回角标一变就调
/// 一次这条命令，而角标常常在同一个数上来回报（push 进度一拍一条）。
#[cfg(desktop)]
#[tauri::command]
pub fn tray_set_alert(app: AppHandle, active: bool) -> Result<(), String> {
    if ALERT.swap(active, Ordering::SeqCst) == active {
        return Ok(());
    }
    // 代号**先**推进：上一档的闪动线程看到代号变了就退场。反过来的话，旧线程可能在新一档
    // 画完之后又画回自己那一拍，图标会停在闪动的中间。
    let gen = GEN.fetch_add(1, Ordering::SeqCst) + 1;
    // 亮灭由 `ALERT` 与 `LIT` 一起决定，所以这里只把相位摆到该有的那一拍上。
    LIT.store(active, Ordering::SeqCst);
    paint_tray_icon(&app);
    if active {
        info!("托盘：有事等处理 → 图标挂红点并开始闪动");
        spawn_blink(app, gen);
    } else {
        info!("托盘：事情处理完了 → 图标回到常亮（红点收掉）");
    }
    Ok(())
}

/// 闪动线程：每 `BLINK_MS` 换一拍，直到"没事了"或自己的代号被换掉。
///
/// 只碰那两个原子量，**绝不拿 `HANDLES` 的锁**（`set_icon` 内部要等主线程回执，握着锁等
/// 主线程 = 主线程反过来等这把锁就死锁，见 `handles` 那段）；也不缓存任何状态 ——
/// 每一拍都按当前真值重画。
fn spawn_blink(app: AppHandle, gen: u64) {
    let spawned = std::thread::Builder::new().name("tray-blink".into()).spawn(move || {
        // 起点是"灭"那一拍：指挥官（`tray_set_alert`）刚画过"亮"，接着就该灭。
        let mut lit = false;
        while ALERT.load(Ordering::SeqCst) && GEN.load(Ordering::SeqCst) == gen {
            LIT.store(lit, Ordering::SeqCst);
            paint_tray_icon(&app);
            lit = !lit;
            std::thread::sleep(Duration::from_millis(BLINK_MS));
        }
    });
    if let Err(e) = spawned {
        // 线程起不来只是不闪，红点还在（当前那一拍是"亮"）—— 别把这一档整个丢掉。
        warn!("托盘闪动线程起不来（图标停在当前那一拍）: {e}");
    }
}

/// 按当前真值重画托盘图标：没事给底图（在线 / 离线），有事给"底图 + 红点"或空白帧（见
/// `TrayFrame`）。
///
/// 与 `refresh` 里那几样一样，一律现读、不信缓存：这条路径同时服务"有事了 / 没事了"、
/// "闪动的一拍"与"在线状态变了"三种来源，只有真值不会分叉。
fn paint_tray_icon(app: &AppHandle) {
    let Some(h) = handles() else { return };
    let online = local_online(app);
    // 亮灭由两个原子量一起决定；灭拍给的是**空白帧**（见 `TrayFrame`）。
    let frame = tray_frame(ALERT.load(Ordering::SeqCst), LIT.load(Ordering::SeqCst));
    if let Err(e) = h.tray.set_icon(Some(tray_icon(online, frame))) {
        warn!("托盘图标更新失败: {e}");
    }
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
    // 图标走 `paint_tray_icon`：它按当前那一拍画（有事那一档正在闪，直接画底图会打断它，
    // 下一拍还得由闪动线程补回来 —— 一次闪断）。
    paint_tray_icon(app);
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

/// 这一拍该画哪一张图。
///
/// 闪动是**整帧交替**：亮拍是"底图 + 红点"，灭拍是**一张看不见的空白**（图标整个消失一拍）。
/// 不是"红点自己一明一灭"—— 那样在任务栏上只是一颗点在抖，扫一眼看不出"有事"；整帧亮灭
/// 才是"这块地方在闪"（与手机/系统那些"图标闪烁提醒"同一种语言）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TrayFrame {
    /// 没事：底图本身（在线 / 离线）。
    Base,
    /// 有事 · 亮拍：底图 + 右上角那颗红点。
    Badged,
    /// 有事 · 灭拍：**看不见的空白帧**（图标消失一拍）。
    Blank,
}

/// 当前真值 → 这一拍画哪张。
fn tray_frame(alert: bool, lit: bool) -> TrayFrame {
    match (alert, lit) {
        (false, _) => TrayFrame::Base,
        (true, true) => TrayFrame::Badged,
        (true, false) => TrayFrame::Blank,
    }
}

/// 托盘图标：在线 = `tauri.conf.json` 的 `trayIcon.iconPath` 同一张图；离线 = 整颗转灰
/// （与球心 logo 在离线时转 `--muted` 是同一套语言，形状不动、只换色）。
///
/// 两张都是**编译期**解码成 RGBA（`include_image!` 走 tauri-codegen），所以运行时不需要
/// `image-png` feature；代价是这里必须与配置里的 iconPath 保持一致。
///
/// 有事那一档的两张（`Badged` / `Blank`）是**运行时合成**的：`badged_rgba` 在底图右上角盖一颗
/// 红点，`blank_rgba` 给一张同尺寸、看不见的空白帧 —— 都省掉跟着底图一起改的资源文件。
fn tray_icon(online: bool, frame: TrayFrame) -> Image<'static> {
    let base = base_icon(online);
    let (w, h) = (base.width(), base.height());
    match frame {
        TrayFrame::Base => base.clone(),
        TrayFrame::Badged => Image::new_owned(badged_rgba(base.rgba(), w, h), w, h),
        TrayFrame::Blank => Image::new_owned(blank_rgba(w, h), w, h),
    }
}

/// 灭拍的空白帧：**整张透明，只留一个 α=1 的像素**（同尺寸）。
///
/// **为什么是"一张全透明的图"而不是 `set_icon(None)`**：那一条是把图标从通知区**摘掉**，托盘会
/// 当场少一格、下一下再插回来（还可能重排），闪动就成了"图标跳来跳去"；而且摘掉的那半秒里
/// 点击/悬停也落不到这颗图标上（`Shell_NotifyIconGetRect` 问不到位置）。画一张全透明的，图标
/// 一直在、位置一直对，只是这半秒里什么都看不见 —— 这正是要的效果。
///
/// **为什么还要留一个 α=1 的像素**（实测，Win11 26200）：给 Explorer 一张**整张全透明**的图标，
/// 它不画空，而是画一个 16×16 的**占位黑块**（点阵/黑方块，取决于尺寸）—— 闪起来就成了
/// "图标 ↔ 黑方块"。留一个 α=1 的点（渲染上等于看不见：实测那一帧除任务栏底色外一个像素都
/// 不差）之后它就不算"空图标"了。1×1 的全透明图也一样不行（一样吃占位黑块）。
fn blank_rgba(w: u32, h: u32) -> Vec<u8> {
    let mut rgba = vec![0u8; (w as usize) * (h as usize) * 4];
    // 只留一个 α=1 的像素（渲染上等于看不见）。
    let keep = ((h / 2 * w + w / 2) * 4) as usize;
    rgba[keep + 3] = 1;
    rgba
}

/// 底色那张图（在线 / 离线）放在静态格里 —— 初始化就地写在这里，第二张也是。
///
/// 为什么非得过静态格：`Image::rgba` 的签名是 `fn rgba(&'a self) -> &'a [u8]`
/// （`impl<'a> Image<'a>`），对 `Image<'static>` 来说它要一个 `&'static self`
/// —— 临时值上根本调不出来。
static ICON_ONLINE: LazyLock<Image<'static>> = LazyLock::new(|| tauri::include_image!("icons/32x32.png"));
static ICON_OFFLINE: LazyLock<Image<'static>> =
    LazyLock::new(|| tauri::include_image!("icons/32x32-offline.png"));

fn base_icon(online: bool) -> &'static Image<'static> {
    if online {
        &ICON_ONLINE
    } else {
        &ICON_OFFLINE
    }
}

/// 有事那一档的图标：**底图一个像素都不动**，只在右上角盖一颗红点。
///
/// 红点是"盖在图标上"的角标 —— 与系统在任务栏按钮、头像上盖角标是同一套语言：底图尺寸、
/// 位置全不变，只是右上角被盖住一部分（所以红点落在底图自己那块墨迹上，是**有意**的）。
/// 底色（在线 / 离线）也照旧看得见：点只盖住右上那一角。
fn badged_rgba(base: &[u8], w: u32, h: u32) -> Vec<u8> {
    debug_assert_eq!(
        base.len(),
        (w as usize) * (h as usize) * 4,
        "底图必须正好是画布的 RGBA"
    );
    let mut canvas = base.to_vec();

    let dot = ratio_side(w, BADGE_RATIO);
    let margin = (w as f64 * BADGE_MARGIN).round() as u32;
    let circle = circle_rgba(DOT_ALERT, dot, 1.0);
    over_into(
        &mut canvas,
        w,
        w.saturating_sub(margin + dot),
        margin,
        &circle,
        dot,
    );
    canvas
}

/// 比例 → 像素边长（至少 1px：0 边长会把那一档整个画没）。
fn ratio_side(canvas: u32, ratio: f64) -> u32 {
    ((canvas as f64 * ratio).round() as u32).max(1)
}

/// source-over：把 `src`（side×side RGBA）叠到画布的 (dx,dy)。
///
/// 红点那一块底下是底图自己的墨迹（角标就盖在图标上），所以这里必须按混合算而不是覆盖：
/// `out_a = sa + da·(1-sa)`，颜色按各自的 alpha 加权 —— 红点边缘那圈软边压在球面上才不会
/// 切出一条硬边。
fn over_into(dst: &mut [u8], cw: u32, dx: u32, dy: u32, src: &[u8], side: u32) {
    for y in 0..side {
        for x in 0..side {
            let s = ((y * side + x) * 4) as usize;
            let sa = u32::from(src[s + 3]);
            if sa == 0 {
                continue;
            }
            let d = ((dy + y) * cw + dx + x) as usize * 4;
            let da = u32::from(dst[d + 3]) * (255 - sa) / 255;
            let out_a = sa + da;
            for c in 0..3 {
                dst[d + c] = ((u32::from(src[s + c]) * sa + u32::from(dst[d + c]) * da) / out_a) as u8;
            }
            dst[d + 3] = out_a as u8;
        }
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

/// 菜单里那颗状态点：直径 = 图标位的一半（`DOT_RATIO`）。
fn dot_rgba(rgb: (u8, u8, u8), slot: u32) -> Vec<u8> {
    circle_rgba(rgb, slot, DOT_RATIO)
}

/// 实心圆的 RGBA 缓冲：圆居中、直径 = 边长 × `ratio`，边缘 4×4 超采样抗锯齿 ——
/// 就十几像素的东西，硬边一眼能看出锯齿。菜单里的状态点（`DOT_RATIO`）与托盘上的红点
/// （`1.0` = 铺满）共用它。
fn circle_rgba(rgb: (u8, u8, u8), slot: u32, ratio: f64) -> Vec<u8> {
    const SUB: u32 = 4;
    let radius = slot as f64 * ratio / 2.0;
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

    /// 渲染出来的图必须正好是画布的 RGBA（尺寸/形状都不许变）：托盘图标按它自己的尺寸上屏，
    /// 少一个字节就是一张错位的图。
    #[test]
    fn badged_icon_keeps_the_base_canvas_and_the_alert_dot_lands_top_right() {
        let base = base_icon(true);
        let (w, h) = (base.width(), base.height());
        let badged = badged_rgba(base.rgba(), w, h);
        assert_eq!(badged.len(), (w * h * 4) as usize, "必须是画布的 RGBA");

        let dot = ratio_side(w, BADGE_RATIO);
        let margin = (w as f64 * BADGE_MARGIN).round() as u32;
        let (cx, cy) = (w - margin - dot / 2, margin + dot / 2);
        let c = ((cy * w + cx) * 4) as usize;
        assert_eq!(badged[c + 3], 255, "红点圆心应完全不透明");
        assert_eq!(
            &badged[c..c + 3],
            [DOT_ALERT.0, DOT_ALERT.1, DOT_ALERT.2],
            "圆心应是那颗红（这一块底图是空的，红点不该被稀释）"
        );

        // 红点整颗都在画布内：右、上两条边各留了 margin 的空白。
        for (x, y) in [(w - 1, cy), (cx, 0)] {
            assert_eq!(alpha_at(&badged, w, x, y), 0, "红点越出画布或贴到边上");
        }
    }

    /// 底图**一个像素都不动**：红点只盖在右上角那块，图标本体（尺寸、位置、颜色）照旧 ——
    /// 用一张纯白不透明的假底图，断言就只关于我们的几何：点之外处处还是那片白。
    #[test]
    fn badge_only_covers_the_top_right_corner() {
        let (w, h) = (32u32, 32u32);
        let white: Vec<u8> = [255u8, 255, 255, 255].repeat((w * h) as usize);
        let badged = badged_rgba(&white, w, h);

        let dot = ratio_side(w, BADGE_RATIO);
        let margin = (w as f64 * BADGE_MARGIN).round() as u32;
        // 点占的那块：x ∈ [w-margin-dot, w-margin)，y ∈ [margin, margin+dot)
        let (x0, x1) = (w - margin - dot, w - margin);
        let (y0, y1) = (margin, margin + dot);

        for y in 0..h {
            for x in 0..w {
                let inside = (x0..x1).contains(&x) && (y0..y1).contains(&y);
                if inside {
                    continue;
                }
                let i = ((y * w + x) * 4) as usize;
                assert_eq!(
                    &badged[i..i + 4],
                    &white[i..i + 4],
                    "点之外（{x},{y}）不该被动过"
                );
            }
        }
        // 左上角也原样是那片白 —— 没有任何"挪走底图 / 让位"的动作。
        assert_eq!(alpha_at(&badged, w, 0, 0), 255, "白底图左上角应原样保留");
        // 而那片墨迹的右上角确实被盖住了（这正是"红点压在图标上"）。
        let c = (((y0 + dot / 2) * w + x0 + dot / 2) * 4) as usize;
        assert_eq!(&badged[c..c + 3], [DOT_ALERT.0, DOT_ALERT.1, DOT_ALERT.2]);
    }

    /// 双击是"时间差"这一件事：窗口内算双击、窗口外算单击、第一下永远是单击。
    /// 判定单拎出来测（`take_double_click`），是因为它是这条路上唯一有分支的真逻辑。
    #[test]
    fn double_click_window_is_a_timing_question() {
        let t0 = Instant::now();

        assert!(!take_double_click(&Mutex::new(None), t0), "第一下永远是单击");
        assert!(
            take_double_click(&Mutex::new(Some(t0)), t0 + DOUBLE_CLICK_WINDOW),
            "间隔正好等于窗口也算双击（边界含在内）"
        );
        assert!(
            !take_double_click(&Mutex::new(Some(t0)), t0 + DOUBLE_CLICK_WINDOW + Duration::from_millis(1)),
            "越过窗口一格就是单击"
        );
    }

    /// 双击之后基准清零：连点的第三下要重新算作"第一下"，否则用户"点-点-点"会被读成一串双击，
    /// 每一下都去开面板。
    #[test]
    fn third_click_starts_over() {
        let slot = Mutex::new(None);
        let t0 = Instant::now();

        assert!(!take_double_click(&slot, t0), "1: 单击");
        assert!(take_double_click(&slot, t0 + Duration::from_millis(100)), "2: 双击（基准清零）");
        assert!(!take_double_click(&slot, t0 + Duration::from_millis(200)), "3: 新的第一下 = 单击");
        assert!(take_double_click(&slot, t0 + Duration::from_millis(250)), "4: 又成对 = 双击");
    }

    /// 没事的时候那张图就是底图本身；有事那一档才动笔（亮拍合成、灭拍看不见）。
    ///
    /// 读字节要一个 `&'static self`（见 `base_icon`），所以这几张放进静态格，而不是本地变量。
    #[test]
    fn plain_icon_is_the_base_untouched() {
        static PLAIN_ONLINE: LazyLock<Image<'static>> = LazyLock::new(|| tray_icon(true, TrayFrame::Base));
        static PLAIN_OFFLINE: LazyLock<Image<'static>> =
            LazyLock::new(|| tray_icon(false, TrayFrame::Base));
        static BADGED_ONLINE: LazyLock<Image<'static>> =
            LazyLock::new(|| tray_icon(true, TrayFrame::Badged));

        assert_eq!(PLAIN_ONLINE.rgba(), base_icon(true).rgba(), "没事那一档不该被改过");
        assert_eq!(PLAIN_OFFLINE.rgba(), base_icon(false).rgba(), "离线那张同理");
        assert_ne!(
            BADGED_ONLINE.rgba(),
            base_icon(true).rgba(),
            "有事那一档必须是合出来的"
        );
    }

    /// 灭拍是**整张看不见**：尺寸照旧（托盘那一格不跟着变），所有像素都透明 —— 只留一个 α=1
    /// 的点（那一个点是给 Explorer 看的，见 `blank_rgba`；渲染上等于看不见）。
    #[test]
    fn blank_frame_is_invisible_and_keeps_the_canvas() {
        static BLANK: LazyLock<Image<'static>> = LazyLock::new(|| tray_icon(true, TrayFrame::Blank));
        let base = base_icon(true);

        assert_eq!((BLANK.width(), BLANK.height()), (base.width(), base.height()), "尺寸不能变");

        let alphas: Vec<u8> = BLANK.rgba().iter().skip(3).step_by(4).copied().collect();
        let kept = alphas.iter().filter(|&&a| a > 0).count();
        assert_eq!(kept, 1, "只该留一个非透明像素（否则 Explorer 画占位黑块）");
        assert_eq!(alphas.iter().copied().max(), Some(1), "留的那个点也必须是 α=1（看不见）");
        assert!(
            BLANK.rgba().iter().step_by(4).all(|&r| r == 0),
            "空白帧不该有任何 RGB 墨迹"
        );
    }

    /// 三个档位各自的取值：没事一律底图；有事亮拍是合成的那张、灭拍是空白。
    #[test]
    fn frames_follow_the_two_flags() {
        assert_eq!(tray_frame(false, false), TrayFrame::Base);
        assert_eq!(tray_frame(false, true), TrayFrame::Base, "没事时亮灭都画底图");
        assert_eq!(tray_frame(true, true), TrayFrame::Badged);
        assert_eq!(tray_frame(true, false), TrayFrame::Blank);
    }

    /// source-over：半透明的红叠在透明底上 = 红 + 一半的 α；不透明的源直接盖住底。
    #[test]
    fn over_blends_by_alpha() {
        let mut dst = [0u8; 4];
        over_into(&mut dst, 1, 0, 0, &[255, 0, 0, 128], 1);
        assert_eq!(&dst[..3], &[255, 0, 0]);
        assert!((128..=130).contains(&dst[3]), "α 应是一半: {}", dst[3]);

        let mut dst = [0u8, 0, 255, 255];
        over_into(&mut dst, 1, 0, 0, &[255, 0, 0, 255], 1);
        assert_eq!(dst, [255, 0, 0, 255], "完全不透明的源直接盖住底");
    }
}