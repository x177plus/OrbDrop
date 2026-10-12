package com.x177plus.orbdrop

import android.Manifest
import android.content.Intent
import android.content.pm.PackageManager
import android.os.Build
import android.os.Bundle
import android.util.Log
import android.view.ViewGroup
import android.webkit.WebView
import androidx.activity.OnBackPressedCallback
import androidx.activity.enableEdgeToEdge
import androidx.core.app.NotificationManagerCompat
import androidx.core.view.ViewCompat
import androidx.core.view.WindowInsetsCompat
import org.json.JSONObject

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

  /**
   * 点**消息通知**进来时要落在哪条会话（`MessageNotifierPlugin` 写进 intent 的 extras）。
   *
   * 交付成功（界面回 `true`）就清空；交付不了（界面还没起来）就留着，下一次 `onResume`
   * 或新的通知再试 —— 见 [deliverPendingConvo]。
   */
  private var pendingConvo: Pair<String, String>? = null

  /** [deliverPendingConvo] 已经试了几次（每个新的触发点重置，见那里的说明）。 */
  private var convoAttempts = 0

  override fun onCreate(savedInstanceState: Bundle?) {
    enableEdgeToEdge()
    super.onCreate(savedInstanceState)

    // 前台保活要它，Android 13+ 的通知权限要它（见各自的函数）。
    // **组播锁不在这里**：它属于"应用在局域网上活着"这件事，归 `OrbDropService` —— 界面
    // 走得比进程早（常驻服务还在，界面已经销毁），锁跟着界面走就会在那一刻默默失效
    // （表现：设备列表不再更新、地址再也学不到；真机上查了半天就是这个）。
    OrbDropService.start(this)
    requestNotificationPermissionIfNeeded()

    // 点消息通知把应用拉起来的那一次：通知里带着"要进哪条会话"。冷启动时界面（WebView 与
    // `MobileApp`）都还在路上，所以这里只记下来，由 `onResume` 去交付（见 `deliverPendingConvo`）。
    pendingConvo = convoOf(intent)

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
    // 界面刚出现：点通知进来的那条会话此刻才交得出去（`MobileApp` 还没挂上时它会自己重试）。
    deliverPendingConvo()
  }

  /**
   * 提醒那一条要不要出，看的就是这个标记（见 `AppVisibility`）：界面在眼前时不响。
   *
   * 交付"点通知要进的会话"也挂在 `onResume`：冷启动与从最近任务回来都会经过它，
   * 而那时界面才可能已经挂好。
   */
  override fun onResume() {
    super.onResume()
    AppVisibility.foreground = true
    ensureMessageChannel()
    convoAttempts = 0
    deliverPendingConvo()
  }

  /**
   * 让「消息通知」这条渠道**装完就存在**（F-108；渠道本身在 `MessageNotifierPlugin.ensureChannel`）。
   *
   * 为什么挂在这里、为什么必须挂：
   *
   * - **挂在 `onResume` 而不是 `onCreate`**：Android 13+ 的通知授权是一个对话框，它让本 Activity
   *   `onPause`，用户点完之后再 `onResume` —— 那一下正好是"权限已经落定"的时刻。比它早建没有意义
   *   （`areNotificationsEnabled()` 还是 false，渠道建出来也是关着的、用户白看），比它晚（原来那样：
   *   等后台第一条消息）用户就**在系统设置里看不到这条渠道**，"横幅/提醒方式"也就无从开起 ——
   *   华为那一层的默认值正是在"这个应用有哪些通知类型"的那一刻定下的（见 `docs/p1-implementation.md` §19）。
   * - **幂等**：渠道已存在时 `ensureChannel` 直接返回，所以每次回到前台叫一次没有代价。
   * - 没给权限时**不建**：那时用户看不见通知，渠道建出来只会让系统设置页多一条"关着的"；
   *   等他在系统设置里把通知打开、再回到应用这一刻，这里就会把它建好。
   */
  private fun ensureMessageChannel() {
    if (!NotificationManagerCompat.from(this).areNotificationsEnabled()) return
    MessageNotifierPlugin.ensureChannel(this)
  }

  override fun onPause() {
    AppVisibility.foreground = false
    super.onPause()
  }

  /**
   * `singleTask` 的实例已经在运行时，点通知走的是这里（而不是 `onCreate`）：
   * 应用开着（哪怕在别的 tab）、用户在通知栏里点了那条消息。
   */
  override fun onNewIntent(intent: Intent) {
    super.onNewIntent(intent)
    convoOf(intent)?.let {
      pendingConvo = it
      convoAttempts = 0
      deliverPendingConvo()
    }
  }

  override fun onDestroy() {
    super.onDestroy()
  }

  /** intent 里那份"要进哪条会话"（没有 = 不是从消息通知进来的）。 */
  private fun convoOf(intent: Intent?): Pair<String, String>? {
    val kind = intent?.getStringExtra(EXTRA_CONVO_KIND) ?: return null
    val id = intent.getStringExtra(EXTRA_CONVO_ID) ?: return null
    return kind to id
  }

  /**
   * 把"点通知要进的那条会话"交给界面（`window.__orbdropOpen`，与返回键那条路同一个套路：
   * 原生问界面，界面回 `true` 表示它办到了）。
   *
   * **为什么要重试**：点通知那一刻界面可能还不存在（进程刚被拉起，Rust 那侧才刚开始建 WebView），
   * 而 `window.__orbdropOpen` 是 `MobileApp` 挂上去的 —— 一次调用会石沉大海，用户看到的就是
   * "点了通知，应用开了，却停在列表里"。所以每 300ms 问一次，最多约 6 秒；界面一旦站起来
   * 就当场交付。试完还没成也**不清掉**：下一次 `onResume` 或新的通知会重新试。
   */
  private fun deliverPendingConvo() {
    val target = pendingConvo ?: return
    val (kind, id) = target
    val view = webView ?: return
    val js = "window.__orbdropOpen ? window.__orbdropOpen(${JSONObject.quote(kind)}, ${JSONObject.quote(id)}) : false"
    view.evaluateJavascript(js) { result ->
      // 一轮跑完时目标可能已经不是它了：`onNewIntent` 与 `onResume` 会各叫一次（点通知那一刻
      // 两个回调都在），晚到的那次回调不该再动（也不该把新目标当成自己交付过了）。
      if (pendingConvo != target) return@evaluateJavascript
      if (result?.trim() == "true") {
        Log.i(TAG, "通知那条会话已交给界面：$kind/$id")
        pendingConvo = null
        convoAttempts = 0
        return@evaluateJavascript
      }
      if (++convoAttempts <= CONVO_DELIVER_ATTEMPTS) {
        view.postDelayed({ deliverPendingConvo() }, CONVO_DELIVER_INTERVAL_MS)
      } else {
        // 试满一轮还交不出去：应用开了却停在列表上，是用户唯一看得见的现象 —— 日志里留一句，
        // 免得下次只能在"点了没反应"和"界面没起来"之间猜。
        Log.w(TAG, "通知那条会话没交出去（界面还没起来？）：$kind/$id")
      }
    }
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

  companion object {
    /**
     * 消息通知带过来的"要进哪条会话"（`MessageNotifierPlugin` 写、这里读）。
     *
     * 只带"哪一种 + 后端那份 id"，不带界面那串会话键（`dev:` / `grp:`）—— 键是界面的约定，
     * 交给 `window.__orbdropOpen` 那一侧去拼（`chat.convoKeyOf`）。
     */
    const val EXTRA_CONVO_KIND = "orbdrop.convo.kind"
    const val EXTRA_CONVO_ID = "orbdrop.convo.id"

    private const val TAG = "orbdrop-mdns"
    private const val REQ_NOTIFICATIONS = 1701

    /** 交付那条会话最多试几次、每次隔多久（见 [deliverPendingConvo]）。 */
    private const val CONVO_DELIVER_ATTEMPTS = 20
    private const val CONVO_DELIVER_INTERVAL_MS = 300L
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