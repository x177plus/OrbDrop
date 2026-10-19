package com.x177plus.orbdrop

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.content.pm.ServiceInfo
import android.net.ConnectivityManager
import android.net.Network
import android.net.NetworkCapabilities
import android.net.NetworkRequest
import android.net.wifi.WifiManager
import android.os.Build
import android.os.Handler
import android.os.IBinder
import android.os.Looper
import android.os.SystemClock
import android.util.Log
import androidx.core.app.NotificationCompat

/**
 * 常驻前台服务（F-81）：**只要应用在，这台设备就得在局域网上活着**。
 *
 * 为什么必须有它：安卓会在界面切到后台之后冻结/回收进程（Doze、后台限制），而 OrbDrop 的
 * 服务器与长连接跑在 Rust 那一侧 —— 进程一停，对端 5 秒一次的探活就判本机离线、
 * 谁推过来的文件都收不到（还不会补投：推送不排队，见 F-53）。前台服务 + 一条常驻通知
 * 是安卓给"我正在为用户做一件持续的事"的标准答案，也是唯一能把进程留在后台的办法。
 *
 * 这一版把"尽量活着"这件事做到底，四件事缺一不可：
 *  1. **类型选 `connectedDevice`**（清单里）：`dataSync` 在 Android 15 上每 24 小时只给 6 小时，
 *     到点被系统停掉；`connectedDevice` 没有这个上限。
 *  2. **`START_STICKY`**：被系统回收后自己回来。
 *  3. **两把 Wi-Fi 锁**（组播锁 + 高性能锁）：灭屏、后台时不让 Wi-Fi 芯片睡过去 ——
 *     组播锁是 mDNS 收得到的前提，高性能锁是连接不掉的前提。
 *  4. **网络回调**：Wi-Fi 关掉再打开时系统会把锁收回去，得在网络恢复的那一刻重新拿。
 *
 * 通知**不带按钮**：它的作用是"让系统别杀我"，不是给用户操作入口；点一下把应用调回前台即可。
 * 渠道重要性用 LOW：不响、不震、不在锁屏上亮 —— 一条常驻的"正在运行"提示而已。
 *
 * **这四条只够 AOSP**：国产 ROM 另有一层自己的权限位（华为「应用启动管理」、小米「自启动」…），
 * 留在默认档时进程会在后台被**冻结** —— 实测（华为 ABC-AL00）表现是通知栏那条常驻通知还在，
 * 而 mDNS 与 HTTP 一起停。那一层是 ROM 私有的，应用改不了也读不到，只能引导用户去开：
 * 引导与状态读取在界面那一侧（`SystemSettingsPlugin` + 移动端「后台与通知」，F-87，
 * 见 `docs/p1-implementation.md` §22）。这里只做"能做的都做了"，并留一条可查的证据 ——
 * [watchdog]（进程被冻过就会在日志里留下间隔异常）。
 */
class OrbDropService : Service() {
  /** Wi-Fi 组播锁（P1-13①）：生命周期 = "应用在局域网上活着"，正是这条服务的意义。 */
  private var multicastLock: WifiManager.MulticastLock? = null

  /** Wi-Fi 高性能锁：灭屏/后台时别让 Wi-Fi 进省电档（掉包、断连的常见根因）。 */
  private var wifiLock: WifiManager.WifiLock? = null

  /** Wi-Fi 网络回调：关开 Wi-Fi 后重新拿锁（见 [watchWifi]）。 */
  private var networkCallback: ConnectivityManager.NetworkCallback? = null

  /** 冻结看门狗的时钟（主线程 Handler；见 [watchdog]）。 */
  private val handler = Handler(Looper.getMainLooper())

  /** 上一拍的时刻（`elapsedRealtime`：含休眠时间，正因为它才量得出"进程中间没跑"）。 */
  private var lastTick = 0L

  /**
   * 冻结看门狗：**"用户没开 ROM 那层开关"唯一可查的证据**（F-87，见 `docs/p1-implementation.md` §22）。
   *
   * 为什么需要：华为「应用启动管理」留在默认的"自动管理"时，进程会在后台被**冻结** —— 而冻结的
   * 表现与"网络被切"一模一样（mDNS 收不到、HTTP 不答话），通知栏那条常驻通知还挂着（它由系统栏
   * 画，进程冻了它也在），从外面看不出区别。那三个开关是 ROM 私有的，应用读不到，所以只能从
   * "我们本该在跑、却没跑"反推：每一拍记一次时刻，下一拍真的回来时看中间隔了多久 —— 被冻过的话
   * 这个间隔会**远大于**定时器间隔（定时器挂在主线程，进程一冻它跟着停）。
   *
   * 一拍只做一次减法、一分钟唤醒一次主线程，不持任何唤醒锁（持了反而会改变被观察的行为）——
   * 与 N-04「常驻低资源占用」不冲突。
   */
  private val watchdog = object : Runnable {
    override fun run() {
      val now = SystemClock.elapsedRealtime()
      if (lastTick != 0L && now - lastTick > WATCHDOG_TRIP_MS) {
        Log.w(
          TAG,
          "进程疑似被系统冻结过：上一拍在 ${(now - lastTick) / 1000}s 前" +
            "（定时器 ${WATCHDOG_INTERVAL_MS / 1000}s 一拍）。" +
            "若对端在此期间看到本机离线，去系统设置里打开自启动/关联启动/后台活动",
        )
      }
      lastTick = now
      handler.postDelayed(this, WATCHDOG_INTERVAL_MS)
    }
  }

  override fun onBind(intent: Intent?): IBinder? = null

  override fun onCreate() {
    super.onCreate()
    ensureChannel()
    acquireLocks()
    watchWifi()
    handler.postDelayed(watchdog, WATCHDOG_INTERVAL_MS)
  }

  override fun onDestroy() {
    // 服务没了（用户退应用/被系统回收）才还回去：它们是**进程内**的资源，留着不放会让 Wi-Fi
    // 芯片一直全速（耗电）。
    handler.removeCallbacks(watchdog)
    unwatchWifi()
    releaseLocks()
    super.onDestroy()
  }

  /**
   * 拿两把 Wi-Fi 锁（幂等：已经握着就跳过）。
   *
   * 安卓的 Wi-Fi 驱动默认**过滤组播帧**（省电），而 mDNS 的发现应答全走组播 ——
   * 没有组播锁时本机发得出去、收不回来，表现就是设备列表永远空着（只能手动敲 IP）。
   * 权限是 `CHANGE_WIFI_MULTICAST_STATE`（清单里已声明）。
   *
   * **为什么在服务里而不是 `MainActivity`**：锁要的是"应用在网上的这段时间"，而界面走得
   * 比进程早 —— 界面一销毁（从最近任务里划掉、被 ROM 回收）而进程靠这条前台服务继续活着时，
   * 锁跟着界面一放，mDNS 就再也收不到任何东西，而且**看不出任何异常**：设备表停在原地、
   * 地址学不到、面板永远显示旧状态。真机上"明明能发消息，设备行却一直是 `:0`"就是这一条。
   *
   * Wi-Fi 关着时 `acquire()` 不会报错（等于没拿）；Wi-Fi 关掉时系统会把这两把锁**自动收回**，
   * 所以恢复联网后必须重拿 —— 由 [watchWifi] 的回调负责。这里对失败只记一行日志，不打断启动。
   */
  private fun acquireLocks() {
    val wifi = applicationContext.getSystemService(Context.WIFI_SERVICE) as? WifiManager
    if (wifi == null) {
      Log.w(TAG, "没有 WIFI_SERVICE：跳过 Wi-Fi 锁")
      return
    }
    // 组播锁。对象只造一次，之后按 isHeld 补拿（系统在 Wi-Fi 关闭时收回后 isHeld 会变回 false）。
    if (multicastLock == null) {
      multicastLock = wifi.createMulticastLock(LOCK_TAG).apply {
        // 不去数引用：拿一次、放一次，重复调 `acquire()` 不该需要配平。
        setReferenceCounted(false)
      }
    }
    multicastLock?.let { lock ->
      if (!lock.isHeld) {
        runCatching { lock.acquire() }.onFailure { Log.w(TAG, "组播锁获取失败：${it.message}") }
      }
    }

    // 高性能 Wi-Fi 锁。`WIFI_MODE_FULL_HIGH_PERF` 自 API 29 起标记废弃，但语义（"别让 Wi-Fi
    // 睡过去"）正是我们要的，且仍然生效；替代项 LOW_LATENCY 是给实时音视频的，语义不符。
    if (wifiLock == null) {
      @Suppress("DEPRECATION")
      wifiLock = wifi.createWifiLock(WifiManager.WIFI_MODE_FULL_HIGH_PERF, WIFI_LOCK_TAG).apply {
        setReferenceCounted(false)
      }
    }
    wifiLock?.let { lock ->
      if (!lock.isHeld) {
        runCatching { lock.acquire() }.onFailure { Log.w(TAG, "Wi-Fi 锁获取失败：${it.message}") }
      }
    }
  }

  /** 放掉 [acquireLocks] 拿的两把锁。 */
  private fun releaseLocks() {
    multicastLock?.let { if (it.isHeld) runCatching { it.release() } }
    multicastLock = null
    wifiLock?.let { if (it.isHeld) runCatching { it.release() } }
    wifiLock = null
  }

  /**
   * 监听 Wi-Fi 的可用性，恢复时重新拿锁。
   *
   * 为什么需要：用户关掉 Wi-Fi 再打开（或换热点）时，系统会把上一轮的组播锁/Wi-Fi 锁收回 ——
   * 现在没有这一步的话，服务活着但 mDNS 从此收不到任何东西，**且不报任何错**（设备表停住、
   * 地址学不到）。`onAvailable` 是"Wi-Fi 又通了"的时刻，在这里重拿即可。
   *
   * 只关心 `TRANSPORT_WIFI`：以太网/蜂窝不在本应用的场景内。
   */
  private fun watchWifi() {
    if (networkCallback != null) return
    val cm = applicationContext.getSystemService(Context.CONNECTIVITY_SERVICE) as? ConnectivityManager
    if (cm == null) {
      Log.w(TAG, "没有 CONNECTIVITY_SERVICE：跳过网络回调")
      return
    }
    val cb = object : ConnectivityManager.NetworkCallback() {
      override fun onAvailable(network: Network) {
        acquireLocks()
        Log.i(TAG, "Wi-Fi 又通了：重新拿锁（组播锁 + 高性能锁）")
      }

      /**
       * Wi-Fi 没了（关掉、掉线、被 ROM 在熄屏时收走）。**只记一行日志**，不做别的。
       *
       * 为什么留着这一行：真机排障时"对端看到本机离线"有两种完全不同的成因 —— 进程被冻结
       * （[watchdog] 那一路）与**本机离开了这个网段**（ROM 的 WLAN+ 智能切到移动数据、或熄屏
       * 断 WLAN）。后者的第一手证据就是这一行：Wi-Fi 没了。日志里两条时间线并排一看就知道
       * 是哪一种，不必再靠 ping/curl 反推。
       */
      override fun onLost(network: Network) {
        Log.w(TAG, "Wi-Fi 断了：本机的组播锁/Wi-Fi 锁会随之下线，对端在此期间会看到本机离线")
      }
    }
    val request = NetworkRequest.Builder()
      .addTransportType(NetworkCapabilities.TRANSPORT_WIFI)
      .build()
    runCatching { cm.registerNetworkCallback(request, cb) }
      .onSuccess { networkCallback = cb }
      .onFailure { Log.w(TAG, "网络回调注册失败：${it.message}") }
  }

  /** 撤掉 [watchWifi] 注册的回调。 */
  private fun unwatchWifi() {
    val cb = networkCallback ?: return
    networkCallback = null
    val cm = applicationContext.getSystemService(Context.CONNECTIVITY_SERVICE) as? ConnectivityManager ?: return
    runCatching { cm.unregisterNetworkCallback(cb) }
  }

  override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
    val notification = buildNotification()
    try {
      if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
        // Android 14+ 起 `startForeground` 必须带上类型，且要与清单里声明的一致。
        startForeground(
          NOTIFICATION_ID,
          notification,
          ServiceInfo.FOREGROUND_SERVICE_TYPE_CONNECTED_DEVICE,
        )
      } else {
        startForeground(NOTIFICATION_ID, notification)
      }
    } catch (e: Exception) {
      // 极端情况下（后台启动限制）会抛 `ForegroundServiceStartNotAllowedException`。那就别赖着
      // 不走 —— 被判"没在 5 秒内 startForeground"会崩掉整个进程，比丢连接更糟；交给系统别的
      // 重启策略（START_STICKY）去处理。
      Log.w(TAG, "startForeground 失败：${e.message}")
      stopSelf()
    }
    // 被系统回收之后自己回来：这条服务的全部意义就是"别让进程死掉"，START_STICKY 是它的前提。
    return START_STICKY
  }

  /**
   * Android 15 的前台服务超时回调。
   *
   * 只对 `dataSync` / `mediaProcessing` / `shortService` 计时，我们用 `connectedDevice`，
   * 正常永远不会触发。留着是防呆：一旦哪天类型被改回计时的那几种，这里若不主动 `stopSelf()`，
   * 会被系统判成"超时未停"并抛 `RemoteServiceException` 崩掉整个进程。
   */
  override fun onTimeout(startId: Int, fgsType: Int) {
    Log.w(TAG, "前台服务超时（type=$fgsType）：主动停止，避免进程崩溃")
    stopSelf(startId)
  }

  private fun ensureChannel() {
    if (Build.VERSION.SDK_INT < Build.VERSION_CODES.O) return
    val manager = getSystemService(NotificationManager::class.java) ?: return
    if (manager.getNotificationChannel(CHANNEL_ID) != null) return
    val channel = NotificationChannel(
      CHANNEL_ID,
      getString(R.string.notif_channel_keepalive),
      NotificationManager.IMPORTANCE_LOW,
    ).apply {
      description = getString(R.string.notif_channel_keepalive_desc)
      setShowBadge(false)
    }
    manager.createNotificationChannel(channel)
  }

  private fun buildNotification(): Notification {
    // 点一下回到应用（`singleTask` 的 MainActivity：不会再多开一个实例）。
    val open = Intent(this, MainActivity::class.java).apply {
      flags = Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_SINGLE_TOP
    }
    val pending = PendingIntent.getActivity(
      this,
      0,
      open,
      PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
    )
    return NotificationCompat.Builder(this, CHANNEL_ID)
      .setSmallIcon(R.mipmap.ic_launcher)
      .setContentTitle(getString(R.string.notif_keepalive_title))
      .setContentText(getString(R.string.notif_keepalive_text))
      .setContentIntent(pending)
      .setOngoing(true)
      .setShowWhen(false)
      .setPriority(NotificationCompat.PRIORITY_LOW)
      .build()
  }

  companion object {
    /** 日志 tag；与 `MainActivity.TAG` 同一串（`adb logcat -s orbdrop-mdns` 一处看全）。 */
    private const val TAG = "orbdrop-mdns"

    /** 组播锁的名字（`dumpsys wifi` 里能看到它；与上面的日志 tag 同一个串）。 */
    const val LOCK_TAG = TAG

    /** 高性能 Wi-Fi 锁的名字（`dumpsys wifi` 里能看到它）。 */
    const val WIFI_LOCK_TAG = "orbdrop-wifi"

    private const val CHANNEL_ID = "orbdrop.keepalive"
    private const val NOTIFICATION_ID = 1701

    /** 冻结看门狗（[watchdog]）：一拍多久、间隔超出多少算"中间没跑"。 */
    private const val WATCHDOG_INTERVAL_MS = 60_000L
    private const val WATCHDOG_TRIP_MS = 180_000L

    /** 起服务（幂等：重复调不会叠出第二条通知）。由 `MainActivity.onCreate` 调用。 */
    fun start(context: Context) {
      val intent = Intent(context, OrbDropService::class.java)
      if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
        context.startForegroundService(intent)
      } else {
        context.startService(intent)
      }
    }
  }
}