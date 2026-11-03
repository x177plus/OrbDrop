//! 底板遮罩后面露出来的那一层窗口底色：打开时跟着变暗（主要是最上面的状态栏）。
//!
//! **为什么需要这一条**：移动端是 edge-to-edge 布局，但 WebView 被 `MainActivity` 按
//! `systemBars` 的 inset 缩了边（见那个文件头的说明）—— 状态栏那一条**不在 WebView 里**，
//! 露出来的是窗口底色（`orbdrop_bg`，白色）。底板遮罩（`.scrim`）画在 WebView 内部，
//! `position: absolute; inset: 0` 也只能盖到 WebView 的边界，于是遮罩打开时最上面那条
//! 白色状态栏会显得像"没盖住"。
//!
//! 能改的只有**窗口那一层**：`Window.setBackgroundDrawable` 换成"遮罩色叠在 `orbdrop_bg`
//! 上"的等效不透明色（`orbdrop_scrim_bg`）。**不碰 `statusBarColor`**：targetSdk 35+ 的
//! edge-to-edge 应用上它已经无效，真正露出来的就是窗口底色；改窗口底色在新旧系统上都成立。
//!
//! 导航栏在部分 ROM（真机 Android 12 / EMUI 实测）上是系统自己的 `NAVIGATION_BAR_PANEL`，
//! 应用改 `navigationBarColor` 或对比度保护都动不了它 —— 那一条不归应用管，这里只保证
//! 窗口底色（状态栏）跟着遮罩走。
//!
//! 状态栏图标保持深色：遮罩只有 32% 黑，叠出来的底色是中间灰（#AFB1B3），深色图标对比度
//! 足够（约 9:1）；切浅色图标反而会在变暗的一瞬间闪一下，没有收益。
//!
//! 前端在 `MobileApp.svelte` 里跟着 `store.currentSheet()` 的开关状态调这一条；命令只在
//! 移动端登记（`lib.rs` 的 handler 列表里带 `#[cfg(mobile)]`）。

#[cfg(mobile)]
use std::sync::OnceLock;
#[cfg(mobile)]
use tauri::plugin::{PluginHandle, TauriPlugin};
#[cfg(mobile)]
use tracing::warn;

/// 插件句柄（注册发生在 `run()` 挂插件那一刻，用它是在底板开关那一刻）。
#[cfg(mobile)]
static HANDLE: OnceLock<PluginHandle<tauri::Wry>> = OnceLock::new();

/// 底板遮罩底色插件（只在安卓构建里挂，见 `lib.rs`）。
#[cfg(mobile)]
pub fn plugin() -> TauriPlugin<tauri::Wry> {
    tauri::plugin::Builder::new("orbdrop-sheet-scrim")
        .setup(|_app, api| {
            // 类名与 Kotlin 那边的 `@TauriPlugin class SheetScrimPlugin` 对应。
            let handle = api.register_android_plugin("com.x177plus.orbdrop", "SheetScrimPlugin")?;
            if HANDLE.set(handle).is_err() {
                warn!("遮罩底色插件重复注册：沿用第一份句柄");
            }
            Ok(())
        })
        .build()
}

/// 底板打开 / 关闭时切换窗口底色（Kotlin `SheetScrimPlugin.setSheetScrim`）。
///
/// `open = true` 用 `orbdrop_scrim_bg`（与 `.scrim` 叠在页面底色上的效果等效），`false` 换回
/// `orbdrop_bg`。命令失败只是"状态栏没跟着暗"，不影响底板本身 —— 前端静默吞掉。
#[cfg(mobile)]
#[tauri::command]
pub async fn set_sheet_scrim(open: bool) -> Result<(), String> {
    let Some(handle) = HANDLE.get() else {
        return Err("遮罩底色插件没注册上（内部错误）".into());
    };
    handle
        .run_mobile_plugin_async::<serde_json::Value>(
            "setSheetScrim",
            serde_json::json!({ "open": open }),
        )
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}