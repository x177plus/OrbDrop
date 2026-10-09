package com.x177plus.orbdrop

import android.Manifest
import android.content.pm.PackageManager
import android.os.Build
import android.os.Bundle
import android.util.Log
import android.view.ViewGroup
import android.webkit.WebView
import androidx.activity.OnBackPressedCallback
import androidx.activity.enableEdgeToEdge
import androidx.core.view.ViewCompat
import androidx.core.view.WindowInsetsCompat

/**
 * 返回键先问界面，界面说"没有了"才退出应用。
 *
 * 为什么需要这一段：Tauri 的 `TauriActivity` 把 `handleBackNavigation` 覆盖成 `false`
 * （见生成目录里的 `generated/TauriActivity.kt`），也就是**它不接管返回键** —— 默认行为
 * 是直接结束 Activity。而移动端界面里"层"是应用自己的状态（会话子屏、底板、非首层的 tab），
 * 它们不落在 webview 历史上，所以返回键一按就把应用关掉：在会话里按返回＝退出应用，
 * 而不是回到会话列表。
 *
 * 这里注册的回调按 LIFO 最先执行（晚注册的先跑），把决定权交给前端
 * （`src/mobile/MobileApp.svelte` 挂的 `window.__orbdropBack`）：它返回 `true` 表示
 * "我收掉了一层，别退应用"，返回 `false` 表示已经在最外层 —— 那时才 `finish()`，
 * 与安卓上"退无可退才退出"的惯例一致。
 *
 * **另外一件事：系统栏与软键盘的 inset 也归这一层。**
 * `enableEdgeToEdge()`（Tauri 模板就带着）＝ `decorFitsSystemWindows(false)`，也就是窗口
 * **不再**为状态栏/导航栏/键盘让出空间，全交给应用自己处理。页面那边认不出这些东西：它有
 * 的只是"视口多大"（`innerHeight`），而 `env(safe-area-inset-*)` 在 Android 上只反映屏幕
 * **挖孔**，不反映状态栏与键盘。所以：
 *
 * - 不处理的话，状态栏的时钟压在标题上、导航条压着底栏，键盘弹起时输入框被键盘盖住
 *   （实测：键盘弹出前后 `innerHeight` 一点没变）；
 * - 处理方式是让 WebView **真的变矮**（改 margin），而不是给它加 padding —— 加 padding
 *   页面视口未必跟着缩，页面里的 `height: 100%` 会照旧铺到键盘底下。
 */
class MainActivity : TauriActivity() {
  private var webView: WebView? = null

  override fun onCreate(savedInstanceState: Bundle?) {
    enableEdgeToEdge()
    super.onCreate(savedInstanceState)

    // 前台保活要它，Android 13+ 的通知权限要它（见各自的函数）。
    // **组播锁不在这里**：它属于"应用在局域网上活着"这件事，归 `OrbDropService` —— 界面
    // 走得比进程早（常驻服务还在，界面已经销毁），锁跟着界面走就会在那一刻默默失效
    // （表现：设备列表不再更新、地址再也学不到；真机上查了半天就是这个）。
    OrbDropService.start(this)
    requestNotificationPermissionIfNeeded()

    // 系统栏/软键盘的 inset 从 decorView 上读（值还没被下游消费，见 `applyInsetsAsMargins`）。
    // **不吃掉**（返回原 insets）：系统的与 AppCompat 自己的那套处理照旧。
    ViewCompat.setOnApplyWindowInsetsListener(window.decorView) { _, insets ->
      applyInsetsAsMargins(insets)
      insets
    }

    onBackPressedDispatcher.addCallback(
      this,
      object : OnBackPressedCallback(true) {
        override fun handleOnBackPressed() {
          val view = webView
          if (view == null) {
            // webview 还没建起来（启动那一瞬）：没什么可问的，按平台惯例退出。
            finish()
            return
          }
          // `evaluateJavascript` 回的是 JSON 字面量：JS 的 `true` → `"true"`。
          view.evaluateJavascript("window.__orbdropBack ? window.__orbdropBack() : false") { result ->
            if (result?.trim() != "true") finish()
          }
        }
      },
    )
  }

  override fun onWebViewCreate(webView: WebView) {
    super.onWebViewCreate(webView)
    this.webView = webView
    // WebView 是建在这里的（Rust 那边 `setContentView`），而 inset 是在窗口刚起来时就派发的 ——
    // 补一次派发，免得第一帧少了状态栏那份留白。
    ViewCompat.requestApplyInsets(window.decorView)
  }

  override fun onDestroy() {
    super.onDestroy()
  }

  /**
   * Android 13+ 的通知权限（P1-13④）。
   *
   * 前台服务在 13+ 上**没有**这个权限也照旧运行（系统不会杀它），只是那一条常驻通知不显示 ——
   * 也就是说"保活"本身不依赖用户的这一次授权。这里仍要问一句：不给的话用户就看不到
   * "OrbDrop 正在后台运行"这件事，卸载/排查时会一头雾水。
   */
  private fun requestNotificationPermissionIfNeeded() {
    if (Build.VERSION.SDK_INT < Build.VERSION_CODES.TIRAMISU) return
    if (checkSelfPermission(Manifest.permission.POST_NOTIFICATIONS) == PackageManager.PERMISSION_GRANTED) return
    runCatching { requestPermissions(arrayOf(Manifest.permission.POST_NOTIFICATIONS), REQ_NOTIFICATIONS) }
      .onFailure { Log.w(TAG, "申请通知权限失败：${it.message}") }
  }

  private companion object {
    const val TAG = "orbdrop-mdns"
    const val REQ_NOTIFICATIONS = 1701
  }

  /**
   * 把系统栏 + 软键盘的 inset 做成 WebView 的 margin（见类注释）。
   *
   * **读的是 `decorView` 上的 inset，不是 WebView 自己的**：挂在 WebView 上实测读到
   * `imeVisible=true` 而 `ime.bottom=0` —— 值在到达它之前就被下游（AppCompat/WebView 自己
   * 的 insets 处理）消费掉了。decorView 是窗口的第一站，那里的值还是原始值。
   */
  private fun applyInsetsAsMargins(insets: WindowInsetsCompat) {
    val view = webView ?: return
    val bars = insets.getInsets(WindowInsetsCompat.Type.systemBars())
    val ime = insets.getInsets(WindowInsetsCompat.Type.ime())
    val lp = view.layoutParams as? ViewGroup.MarginLayoutParams ?: return
    // 键盘比导航条高，取两者的较大值；左右/上一起给，带挖孔的机器与分屏时同样对得上。
    val bottom = maxOf(bars.bottom, ime.bottom)
    if (lp.leftMargin == bars.left && lp.topMargin == bars.top &&
      lp.rightMargin == bars.right && lp.bottomMargin == bottom
    ) {
      return
    }
    lp.setMargins(bars.left, bars.top, bars.right, bottom)
    view.layoutParams = lp
  }
}