package com.x177plus.orbdrop

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.Context
import android.content.Intent
import android.net.wifi.WifiManager
import android.os.Build
import android.os.IBinder
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
 * 通知**不带按钮**：它的作用是"让系统别杀我"，不是给用户操作入口；点一下把应用调回前台即可。
 * 渠道重要性用 LOW：不响、不震、不在锁屏上亮 —— 一条常驻的"正在运行"提示而已。
 */
class OrbDropService : Service() {
  /** Wi-Fi 组播锁（P1-13①）：生命周期 = "应用在局域网上活着"，正是这条服务的意义。 */
  private var multicastLock: WifiManager.MulticastLock? = null

  override fun onBind(intent: Intent?): IBinder? = null

  override fun onCreate() {
    super.onCreate()
    ensureChannel()
    acquireMulticastLock()
  }

  override fun onDestroy() {
    // 服务没了（用户退应用/被系统回收）才还回去：它是**进程内**的资源，留着不放会让 Wi-Fi
    // 芯片一直收组播（耗电）。
    multicastLock?.let { lock ->
      if (lock.isHeld) runCatching { lock.release() }
    }
    multicastLock = null
    super.onDestroy()
  }

  /**
   * 拿 Wi-Fi 组播锁（P1-13①）。
   *
   * 安卓的 Wi-Fi 驱动默认**过滤组播帧**（省电），而 mDNS 的发现应答全走组播 ——
   * 没有这把锁时本机发得出去、收不回来，表现就是设备列表永远空着（只能手动敲 IP）。
   * 权限是 `CHANGE_WIFI_MULTICAST_STATE`（清单里已声明）。
   *
   * **为什么在服务里而不是 `MainActivity`**：锁要的是"应用在网上的这段时间"，而界面走得
   * 比进程早 —— 界面一销毁（从最近任务里划掉、被 ROM 回收）而进程靠这条前台服务继续活着时，
   * 锁跟着界面一放，mDNS 就再也收不到任何东西，而且**看不出任何异常**：设备表停在原地、
   * 地址学不到、面板永远显示旧状态。真机上"明明能发消息，设备行却一直是 `:0`"就是这一条。
   *
   * Wi-Fi 关着时 `acquire()` 不会报错（等于没拿），用户打开 Wi-Fi 之后需要重新触发 ——
   * 所以这里对失败/无 Wi-Fi 都只记一行日志，不打断启动。
   */
  private fun acquireMulticastLock() {
    if (multicastLock?.isHeld == true) return
    val wifi = applicationContext.getSystemService(Context.WIFI_SERVICE) as? WifiManager
    if (wifi == null) {
      Log.w(LOCK_TAG, "没有 WIFI_SERVICE：跳过组播锁")
      return
    }
    multicastLock = wifi.createMulticastLock(LOCK_TAG).apply {
      // 不去数引用：拿一次、放一次，重复调 `acquire()` 不该需要配平。
      setReferenceCounted(false)
      runCatching { acquire() }.onFailure { Log.w(LOCK_TAG, "组播锁获取失败：${it.message}") }
    }
  }

  override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
    startForeground(NOTIFICATION_ID, buildNotification())
    // 被系统回收之后自己回来：这条服务的全部意义就是"别让进程死掉"，START_STICKY 是它的前提。
    return START_STICKY
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
    /** 组播锁的名字（`dumpsys wifi` 里能看到它；与 `MainActivity` 的日志 tag 同一个串）。 */
    const val LOCK_TAG = "orbdrop-mdns"
    private const val CHANNEL_ID = "orbdrop.keepalive"
    private const val NOTIFICATION_ID = 1701

    /** 起服务（幂等：重复调不会叠出第二条通知）。 */
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