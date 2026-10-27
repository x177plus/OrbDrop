package com.x177plus.orbdrop

import android.app.Activity
import android.content.Intent
import android.net.Uri
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.Plugin

@InvokeArg
class OpenStoreArgs {
  lateinit var url: String
}

/**
 * 「去应用商店更新系统 WebView」在安卓上的落地（导航钩子在 `mobile::store_plugin`）。
 *
 * **为什么这个按钮非得由后端来点**：需要它的那一刻，前端是死的 —— 引擎低于下限（Chrome 90）时入口
 * module 连解析都过不去（产物里带 `??=`，Chrome 85 起的语法），解析失败就是整块脚本作废，界面里那套
 * IPC 根本不存在。页面上唯一还能点的东西是 `static/engine-floor.js`（ES5）画出来的浮层，而它只能发出
 * 一次**导航**（`market://details?id=com.google.android.webview`）。wry 的 WebViewClient 把每一次导航
 * 都交给 Rust（`shouldOverride` → 插件的 `on_navigation`），于是那次导航在 Rust 侧被截下来、换成一个
 * 系统 Intent 由这里发出去 —— WebView 自己加载不了 `market://`（unknown scheme，放行只会得到空白页）。
 *
 * 不硬指定商店、但**先点名 Play**：WebView 的更新就在 Play 里，而 `market://` 可能同时被
 * 多个应用商店认领 —— 不点名的话每次都得先过一张"打开方式"选择框（真机实测：选择框里会出现
 * 多家）。Play 不在（部分地区的机器很常见）才退回让系统挑：谁在谁接，总比按钮没反应强。
 *
 * 打不开时 `invoke.reject` 的那句话会进 Rust 日志（`打开应用商店失败：…`）：那种情况下已经没有任何
 * 界面能显示它了（浮层是静态的，页面上另有"没反应就自己搜一下"的文字指引），所以日志就是唯一出口。
 */
@TauriPlugin
class StoreOpenPlugin(private val activity: Activity) : Plugin(activity) {

  @Command
  fun openStore(invoke: Invoke) {
    val args = invoke.parseArgs(OpenStoreArgs::class.java)
    val uri = Uri.parse(args.url)
    if (tryOpen(uri, "com.android.vending")) {
      invoke.resolve()
      return
    }
    if (tryOpen(uri, null)) {
      invoke.resolve()
      return
    }
    invoke.reject(activity.getString(R.string.store_no_handler, args.url))
  }

  /** 交给系统打开。这个包名（或任意应用）接不了就回 `false` —— 由调用方决定退到哪一档。 */
  private fun tryOpen(uri: Uri, pkg: String?): Boolean {
    val intent = Intent(Intent.ACTION_VIEW, uri)
    if (pkg != null) intent.setPackage(pkg)
    return try {
      activity.startActivity(intent)
      true
    } catch (e: Exception) {
      false
    }
  }
}