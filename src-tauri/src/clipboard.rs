//! 系统剪贴板：安卓上能用的**写**这一条腿（命令 `copy_text`）。
//!
//! **为什么不用 `navigator.clipboard`**：Android WebView 里异步剪贴板 API 永远拒绝写入 ——
//! `navigator.clipboard.writeText()` 抛 `NotAllowedError: Write permission denied.`，
//! 而 `clipboard-write` 这个 Web 权限在 WebView 里根本没有实现，`WebChromeClient.onPermissionRequest`
//! 也不会被调用（Chromium issue 40205793；真机同一句）。所以界面那边
//! （`components/MessageActionsPopover` 的「复制整条消息」、`sheets/FileActionsSheet` 的「复制路径」）
//! 都走这条命令。
//!
//! **不需要任何安卓权限**：Android 10 起限制的是**读**剪贴板（只有前台应用 / 默认输入法读得到全文），
//! 写一直对前台应用开放，`ClipboardManager.setPrimaryClip` 不触发权限对话框、也不要求文档焦点 ——
//! 上一版那句"`Document is not focused`"在 Web 那条路上存在，这条路上不存在。
//!
//! 桌面端没有这条命令，也照旧用 `navigator.clipboard`（WebView2 / WKWebView 是通的）；
//! 命令只在移动端登记，见 `lib.rs`。

#[cfg(mobile)]
use std::sync::OnceLock;
#[cfg(mobile)]
use tauri::plugin::{PluginHandle, TauriPlugin};
#[cfg(mobile)]
use tracing::warn;

/// 插件句柄（注册发生在 `run()` 挂插件那一刻，用它是在用户点复制那一刻）。
#[cfg(mobile)]
static HANDLE: OnceLock<PluginHandle<tauri::Wry>> = OnceLock::new();

/// 剪贴板插件（只在安卓构建里挂，见 `lib.rs`）。
#[cfg(mobile)]
pub fn plugin() -> TauriPlugin<tauri::Wry> {
    tauri::plugin::Builder::new("orbdrop-clipboard")
        .setup(|_app, api| {
            // 类名与 Kotlin 那边的 `@TauriPlugin class ClipboardPlugin` 对应。
            let handle = api.register_android_plugin("com.x177plus.orbdrop", "ClipboardPlugin")?;
            if HANDLE.set(handle).is_err() {
                warn!("剪贴板插件重复注册：沿用第一份句柄");
            }
            Ok(())
        })
        .build()
}

/// 把一段文字写进系统剪贴板（Kotlin `ClipboardPlugin.writeText`）。
///
/// 失败时把原话交给调用方 —— 它会在界面上变成一条「复制失败：…」的 toast。
#[cfg(mobile)]
#[tauri::command]
pub async fn copy_text(text: String) -> Result<(), String> {
    let Some(handle) = HANDLE.get() else {
        return Err("剪贴板插件没注册上（内部错误）".into());
    };
    handle
        .run_mobile_plugin_async::<serde_json::Value>(
            "writeText",
            serde_json::json!({ "text": text }),
        )
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}