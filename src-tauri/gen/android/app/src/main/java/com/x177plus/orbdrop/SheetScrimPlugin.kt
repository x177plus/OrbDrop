package com.x177plus.orbdrop

import android.app.Activity
import android.graphics.drawable.ColorDrawable
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.Plugin

/**
 * 底板遮罩后面露出来的那一层窗口底色（后端 `sheet_scrim::set_sheet_scrim`）。
 *
 * **为什么需要它**：应用是 edge-to-edge 的，但 WebView 按 `MainActivity` 里的
 * `systemBars` inset 缩了边 —— 状态栏那一条**不在 WebView 里**，露出来的是窗口底色
 * （`orbdrop_bg`，白色）。底板遮罩（`.scrim`）画在 WebView 内部，`inset: 0` 也只能盖到
 * WebView 的边界，所以底板打开时最上面那一条会保持白色，看起来像"遮罩没盖住"。
 *
 * **改的是窗口底色，不是 `statusBarColor`**：targetSdk 35+ 的 edge-to-edge 应用上
 * `statusBarColor` 已经无效（系统强制透明），真正露出来的是窗口底色；`windowBackground`
 * 从 API 24 到 35 都成立。颜色是 `.scrim` 的 `rgba(14,18,23,.32)` 叠在 `orbdrop_bg`
 * 上算出的等效不透明值，见 `values/colors.xml` 的 `orbdrop_scrim_bg`。
 *
 * 导航栏在部分 ROM（真机 Android 12 / EMUI 实测）上是系统自己的
 * `NAVIGATION_BAR_PANEL`：纯白，`navigationBarColor` 与对比度保护都动不了它 —— 那一条
 * 不归应用管，这里只保证状态栏那一条跟着遮罩走。
 *
 * **不切图标深浅**：32% 黑叠出来是中间灰（#AFB1B3），深色时间/电量图标对比度约 9:1；
 * 切浅色会在变暗的一瞬间换一整套系统图标，反而闪。关闭时换回 `orbdrop_bg`。
 */
@InvokeArg
class SheetScrimArgs {
  var open: Boolean = false
}

@TauriPlugin
class SheetScrimPlugin(private val activity: Activity) : Plugin(activity) {

  @Command
  fun setSheetScrim(invoke: Invoke) {
    val args = invoke.parseArgs(SheetScrimArgs::class.java)
    activity.runOnUiThread {
      try {
        val color = activity.getColor(
          if (args.open) R.color.orbdrop_scrim_bg else R.color.orbdrop_bg,
        )
        val window = activity.window
        // 状态栏后面露出来的就是这一层窗口底色；导航栏那一条在部分 ROM 上是系统自己画的，
        // 不在应用能改的范围内（见类注释）。
        window.setBackgroundDrawable(ColorDrawable(color))
        // 背景换过之后让 DecorView 立刻重画一帧；不然状态栏那一条可能等下一次布局才更新。
        window.decorView.invalidate()
        invoke.resolve()
      } catch (e: Exception) {
        invoke.reject(e.message ?: e.toString())
      }
    }
  }
}