//! 「让应用在后台活着」所需要的**系统那一层**（F-87，机制与真机观察见 `docs/p1-implementation.md` §22）。
//!
//! 为什么单独有这一块：前台服务（`OrbDropService`）加两把 Wi-Fi 锁在 AOSP 上就够了，但国产 ROM
//! 另有一层自己的权限位（华为「应用启动管理」、小米「自启动」、OPPO/vivo 同类开关）。那一层是
//! **ROM 私有的：应用既改不了，也读不到** —— 所以这里能做的只有两件：把用户送到那一页，
//! 以及读回**唯一能读到的那个状态**（电池优化白名单，Google 官方 API）。
//!
//! 真机证据（华为 ABC-AL00）：那三个开关留在默认的「自动管理」时，进程在后台被冻结 —— 通知栏
//! 那条常驻通知还挂着（它由系统栏画），而 mDNS 广播与 HTTP 应答一起停，对端看到的就是"这台离线
//! 了"。所以"用户没开那三个开关"这件事必须在界面上说清楚，而不是等用户来问"为什么收不到"。
//!
//! 与 `notify` 的分工：那边只管"打通知"（渠道、横幅、通知设置页），这边只管"系统让不让我活着"。
//! 桌面端没有实体：这一族的命令只在移动端登记（`lib.rs` 的 handler 列表里带 `#[cfg(mobile)]`）。

#[cfg(mobile)]
use std::sync::Arc;
#[cfg(mobile)]
use std::sync::OnceLock;

#[cfg(mobile)]
use serde::{Deserialize, Serialize};
#[cfg(mobile)]
use tauri::plugin::{PluginHandle, TauriPlugin};
#[cfg(mobile)]
use tauri::State;
#[cfg(mobile)]
use tracing::{info, warn};

#[cfg(mobile)]
use crate::config;
#[cfg(mobile)]
use crate::state::AppState;

/// 插件句柄（注册发生在 `run()` 挂插件那一刻，用它是在用户点按钮那一刻）。
#[cfg(mobile)]
static HANDLE: OnceLock<PluginHandle<tauri::Wry>> = OnceLock::new();

/// 保活设置插件（只在安卓构建里挂，见 `lib.rs`）。
#[cfg(mobile)]
pub fn plugin() -> TauriPlugin<tauri::Wry> {
    tauri::plugin::Builder::new("orbdrop-keepalive")
        .setup(|_app, api| {
            // 类名与 Kotlin 那边的 `@TauriPlugin class SystemSettingsPlugin` 对应。
            let handle = api.register_android_plugin("com.x177plus.orbdrop", "SystemSettingsPlugin")?;
            if HANDLE.set(handle).is_err() {
                warn!("保活设置插件重复注册：沿用第一份句柄");
            }
            Ok(())
        })
        .build()
}

#[cfg(mobile)]
fn handle() -> Result<&'static PluginHandle<tauri::Wry>, String> {
    HANDLE
        .get()
        .ok_or_else(|| "保活设置插件没注册上（内部错误）".to_string())
}

/// 「电池优化白名单在不在」—— 界面那一行"已允许 / 未允许"（唯一能读到的系统状态）。
#[cfg(mobile)]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct KeepAliveState {
    pub battery_optimization_ignored: bool,
}

/// 读回保活状态（Kotlin `SystemSettingsPlugin.keepAliveState`）。
#[cfg(mobile)]
#[tauri::command]
pub async fn keepalive_state() -> Result<KeepAliveState, String> {
    handle()?
        .run_mobile_plugin_async::<KeepAliveState>("keepAliveState", serde_json::json!({}))
        .await
        .map_err(|e| e.to_string())
}

/// 弹系统的"允许后台运行"对话框（Kotlin `requestIgnoreBatteryOptimizations`）。
///
/// 成败都不影响调用方（界面按钮那一条 toast 看结果）：ROM 不认那条 action 时会退到白名单列表页，
/// 两条都不成才会 reject。
#[cfg(mobile)]
#[tauri::command]
pub async fn request_ignore_battery_optimizations() -> Result<(), String> {
    handle()?
        .run_mobile_plugin_async::<serde_json::Value>(
            "requestIgnoreBatteryOptimizations",
            serde_json::json!({}),
        )
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// 跳到 ROM 的「自启动 / 后台活动」页；`opened` = 实际打开的是哪一页（见 Kotlin 那边的候选表）。
#[cfg(mobile)]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OpenedPage {
    pub opened: String,
}

#[cfg(mobile)]
#[tauri::command]
pub async fn open_autostart_settings() -> Result<OpenedPage, String> {
    handle()?
        .run_mobile_plugin_async::<OpenedPage>("openAutoStartSettings", serde_json::json!({}))
        .await
        .map_err(|e| e.to_string())
}

/// 「后台保活」首次引导给他看过了没有（F-87）：界面起来时问一次，没看过就开那张引导底板。
///
/// 落在配置文件里（`Config::keepalive_guided`）而不是界面内存：它要跨重启有效，且重装之后
/// 应当**重新问一次**（装完那三个开关又回到默认档了）。
#[cfg(mobile)]
#[tauri::command]
pub fn keepalive_guided(state: State<'_, Arc<AppState>>) -> bool {
    state.config.lock().keepalive_guided
}

/// 记一笔"引导已经给他看过"。幂等：已经记过就不再写盘。
#[cfg(mobile)]
#[tauri::command]
pub fn mark_keepalive_guided(state: State<'_, Arc<AppState>>) -> Result<bool, String> {
    let mut config = state.config.lock();
    if !config.keepalive_guided {
        config.keepalive_guided = true;
        config::save_config_at(&state.config_path, &config).map_err(|e| e.to_string())?;
        info!("后台保活引导已记为看过");
    }
    Ok(true)
}