package com.x177plus.orbdrop

import android.app.Activity
import android.content.ClipData
import android.content.ClipboardManager
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.Plugin

/**
 * 把一段文字写进系统剪贴板（后端 `clipboard::copy_text`）。
 *
 * **为什么必须有它**：Android WebView 里 `navigator.clipboard.writeText()` 永远失败 ——
 * `NotAllowedError: Write permission denied.`，`clipboard-write` 这个 Web 权限在 WebView 里
 * 没有实现，`WebChromeClient.onPermissionRequest` 也不会被调用（Chromium issue 40205793）。
 * 界面上看到的"需要写入权限"就是这一句：**不是安卓权限的问题**，是 WebView 不给这条 Web API。
 *
 * 原生这一跳不需要任何权限：Android 10 起被限制的是**读**剪贴板（只有前台应用 / 默认输入法
 * 读得到全文），写一直对前台应用开放；`setPrimaryClip` 不弹权限对话框，也不要求 WebView 有焦点。
 *
 * 标签（[ClipData] 的 label）用应用名：部分 ROM 的剪贴板浮窗会显示它。
 */
@InvokeArg
class CopyTextArgs {
  lateinit var text: String
}

@TauriPlugin
class ClipboardPlugin(private val activity: Activity) : Plugin(activity) {

  @Command
  fun writeText(invoke: Invoke) {
    val args = invoke.parseArgs(CopyTextArgs::class.java)
    val clipboard = activity.getSystemService(ClipboardManager::class.java)
    if (clipboard == null) {
      invoke.reject(activity.getString(R.string.clipboard_unavailable))
      return
    }
    try {
      clipboard.setPrimaryClip(ClipData.newPlainText(activity.getString(R.string.app_name), args.text))
      invoke.resolve()
    } catch (e: Exception) {
      // 正常机型走不到这里；写失败时把系统原话交给界面（那里会变成「复制失败：…」）。
      invoke.reject(e.message ?: e.toString())
    }
  }
}