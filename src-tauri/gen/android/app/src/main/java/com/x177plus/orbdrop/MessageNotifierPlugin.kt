package com.x177plus.orbdrop

import android.app.Activity
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Build
import android.provider.Settings
import android.util.Log
import androidx.core.app.NotificationCompat
import androidx.core.app.NotificationManagerCompat
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin

/**
 * 界面此刻是不是正在前面（`MainActivity` 的 `onResume` / `onPause` 写它）。
 *
 * 通知栏那一条是给"人没看着屏幕"的时候用的：界面就在眼前时，消息本来就已经划进聊天记录、
 * 角标也跟着跳了，再多响一声只是噪音。这个标记也是**唯一**能判断这件事的地方 ——
 * 后端只知道自己收到了消息，不知道用户在看哪一屏。
 */
object AppVisibility {
  @Volatile
  var foreground = false
}

/** `notifyMessage` 的参数（字段名与 Rust 那边 `serde_json::json!` 的键一一对应）。 */
@InvokeArg
class NotifyMessageArgs {
  lateinit var kind: String
  lateinit var id: String
  lateinit var title: String
  lateinit var body: String
}

/**
 * 收到对端消息的通知（F-108）。由 Rust 那侧调用（`tauri-plugin` 的 `run_mobile_plugin`，
 * 见 `src-tauri/src/notify.rs`）：**消息落账那一刻**就发，不经过界面。
 *
 * 为什么要绕过界面：应用在后台时 `WryActivity.onPause` 会 `webView.onPause()`（JS 的定时器
 * 全停），而界面本身也可能早被系统回收 —— 那种时候前端发不出任何东西，后端却一直在收消息。
 * （界面真的已经被销毁的那一档，Rust 那边会先跳过这次调用：见 `notify::set_ui_alive`。）
 *
 * 与 `OrbDropService` 那条常驻通知分开：那条的活是"别让系统杀我"（`IMPORTANCE_LOW`、不可撤、
 * 点一下回应用），这条的活是"有人找你"（`IMPORTANCE_HIGH`、可撤、点一下**直接进那条会话**）。
 * 两个渠道也是**故意分开**的：用户可以在系统设置里单独关掉消息通知，而保住后台在线。
 *
 * 这条「消息通知」渠道不是在第一次发通知时才建的 —— 装完（通知授权落定）那一刻
 * `MainActivity` 就会调这儿把它建出来，理由见 [ensureChannel]（用户在系统设置里得先看得见它，
 * 才有得开横幅）。这里的调用只是兜底。
 */
@TauriPlugin
class MessageNotifierPlugin(private val activity: Activity) : Plugin(activity) {

  @Command
  fun notifyMessage(invoke: Invoke) {
    val args = invoke.parseArgs(NotifyMessageArgs::class.java)
    // 人正看着屏幕：界面自己会更新（聊天记录、未读角标），这里再响一声只是噪音。
    if (AppVisibility.foreground) {
      invoke.resolve()
      return
    }
    // 没给通知权限（Android 13+ 的那一次询问被拒）：post 下去也不会显示，直接算了。
    val manager = NotificationManagerCompat.from(activity)
    if (!manager.areNotificationsEnabled()) {
      invoke.resolve()
      return
    }
    // 兜底：正常情况下这条渠道在装完（授权落定）那一刻就由 `MainActivity` 建好了（见
    // `ensureChannel` 的说明），这里再叫一次只是保证"通知这条路永远不依赖界面跑没跑过"。
    ensureChannel(activity)
    manager.notify(notificationId(args.kind, args.id), buildNotification(args))
    invoke.resolve()
  }

  /**
   * 通知这一层此刻的真实状态：**总开关** + 「消息通知」渠道的重要程度（F-108 的自查，见
   * `docs/p1-implementation.md` §19）。
   *
   * 为什么值得读：横幅不是权限，是"渠道重要程度 ≥ 高 且 这条通知真的会响/震"的派生表现
   * （AOSP `NotificationInterruptStateProviderImpl.shouldHeadsUpWhenAwake` 里那道
   * `entry.getImportance() < IMPORTANCE_HIGH → 不弹`）。而渠道的重要性**建完只能降不能升**，
   * 所以要区分"我们声明得不对"与"系统/用户在别处关了它"，界面只能拿这个值说真话。
   *
   * `messageChannelImportance` 用 `-1` 表示"渠道还没建"（`NotificationManager.IMPORTANCE_*`
   * 的取值是 0–5，没有负数）—— 那说明用户还没给通知权限、或者渠道被系统清过。
   */
  @Command
  fun notificationState(invoke: Invoke) {
    val manager = NotificationManagerCompat.from(activity)
    val channel = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
      activity.getSystemService(NotificationManager::class.java)?.getNotificationChannel(CHANNEL_MESSAGES)
    } else {
      null
    }
    val out = JSObject()
    out.put("enabled", manager.areNotificationsEnabled())
    out.put("messageChannelImportance", channel?.importance ?: -1)
    invoke.resolve(out)
  }

  /** 跳到本应用的通知设置页（`ACTION_APP_NOTIFICATION_SETTINGS`；Android 8 以下只有应用详情页）。 */
  @Command
  fun openNotificationSettings(invoke: Invoke) {
    val intent = if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O) {
      Intent(Settings.ACTION_APP_NOTIFICATION_SETTINGS)
        .putExtra(Settings.EXTRA_APP_PACKAGE, activity.packageName)
    } else {
      Intent(Settings.ACTION_APPLICATION_DETAILS_SETTINGS, Uri.parse("package:${activity.packageName}"))
    }
    try {
      activity.startActivity(intent)
      invoke.resolve()
    } catch (e: Exception) {
      invoke.reject(activity.getString(R.string.notif_settings_failed))
    }
  }

  /**
   * 跳到「消息通知」**那一条渠道**自己的设置页：横幅、铃声、锁屏、勿扰，全挂在这一页上
   * （部分厂商 ROM 会把渠道映射成"通知类型"，同一批开关换了个名字）。
   *
   * 先 `ensureChannel` 再跳：渠道不在时那一页是空的 —— 用户被送到一个什么都没有的页面上，
   * 比不引导更糟。渠道建不出来（没给通知权限）时退回应用通知页，好歹能看到总开关。
   */
  @Command
  fun openChannelSettings(invoke: Invoke) {
    try {
      if (Build.VERSION.SDK_INT < Build.VERSION_CODES.O) {
        activity.startActivity(
          Intent(Settings.ACTION_APPLICATION_DETAILS_SETTINGS, Uri.parse("package:${activity.packageName}")),
        )
        invoke.resolve()
        return
      }
      ensureChannel(activity)
      val system = activity.getSystemService(NotificationManager::class.java)
      val hasChannel = system?.getNotificationChannel(CHANNEL_MESSAGES) != null
      activity.startActivity(
        if (hasChannel) {
          Intent(Settings.ACTION_CHANNEL_NOTIFICATION_SETTINGS)
            .putExtra(Settings.EXTRA_APP_PACKAGE, activity.packageName)
            .putExtra(Settings.EXTRA_CHANNEL_ID, CHANNEL_MESSAGES)
        } else {
          Intent(Settings.ACTION_APP_NOTIFICATION_SETTINGS)
            .putExtra(Settings.EXTRA_APP_PACKAGE, activity.packageName)
        },
      )
      invoke.resolve()
    } catch (e: Exception) {
      invoke.reject(activity.getString(R.string.notif_settings_failed))
    }
  }

  private fun buildNotification(args: NotifyMessageArgs): Notification {
    // 点一下回到应用，并带上"要进哪条会话"——`singleTask` 的 MainActivity 会把这份 intent
    // 交给已经开着的那个实例（`onNewIntent`），会话由它转交界面（`window.__orbdropOpen`）。
    val open = Intent(activity, MainActivity::class.java).apply {
      flags = Intent.FLAG_ACTIVITY_NEW_TASK or Intent.FLAG_ACTIVITY_SINGLE_TOP
      putExtra(MainActivity.EXTRA_CONVO_KIND, args.kind)
      putExtra(MainActivity.EXTRA_CONVO_ID, args.id)
    }
    val pending = PendingIntent.getActivity(
      activity,
      notificationId(args.kind, args.id),
      open,
      PendingIntent.FLAG_UPDATE_CURRENT or PendingIntent.FLAG_IMMUTABLE,
    )
    return NotificationCompat.Builder(activity, CHANNEL_MESSAGES)
      .setSmallIcon(R.mipmap.ic_launcher)
      .setContentTitle(args.title)
      .setContentText(args.body)
      // 消息常常比一行长：展开成两三行，省得点进去才看得到后半句。
      .setStyle(NotificationCompat.BigTextStyle().bigText(args.body))
      .setContentIntent(pending)
      .setAutoCancel(true)
      .setCategory(NotificationCompat.CATEGORY_MESSAGE)
      .setPriority(NotificationCompat.PRIORITY_HIGH)
      .build()
  }

  /**
   * 一条会话一个**固定**的 id：同一条会话连着来几条消息，只更新那一张卡（而不是堆一排），
   * 内容永远是最新那条 —— 通知栏不是聊天记录，堆一排会把别的应用的通知挤下去。
   *
   * 取负：`OrbDropService` 那条常驻通知用的是正数 id，撞上了会把"我正在运行"那条顶掉
   * （系统不容两条同 id 的通知，后一条替换前一条）。
   */
  private fun notificationId(kind: String, id: String): Int {
    return -kotlin.math.abs("orbdrop.msg:$kind:$id".hashCode())
  }

  companion object {
    /** 消息通知的渠道（与 `OrbDropService` 那条 `orbdrop.keepalive` 分开，见类注释）。 */
    const val CHANNEL_MESSAGES = "orbdrop.messages"

    /**
     * 建「消息通知」这条渠道（幂等：已存在就直接返回）。
     *
     * **为什么是 companion、为什么要在界面起来那一刻就建**（调用点：`MainActivity`）：
     * 渠道不只是"发通知时的路由"，它同时是**用户在系统设置里唯一能看到、能调的东西** ——
     * 只按需在建在 `notifyMessage` 里的话，装完那一刻渠道表是空的（另一条 `orbdrop.keepalive`
     * 是 `IMPORTANCE_LOW`、而且它在系统那一页里本来也没什么可调的），于是
     * "设置 → 通知和状态栏 → OrbDrop"里看不到「消息通知」这一条：**横幅/提醒方式无从开起**
     * （部分厂商 ROM 的默认值也是在"这个应用有哪些通知类型"的那一刻定下的，见 docs/p1-implementation.md §19）。
     *
     * 重要性只有**建渠道那一刻**能定：AOSP 之后只允许应用把它**降**下去，不允许升
     * （`PreferencesHelper.createNotificationChannel`：`existing.getUserLockedFields() == 0 &&
     * channel.getImportance() < existing.getImportance()` 才写，且删了再用同一个 id 重建
     * 会走"反删除"、保留原设置）——所以这条渠道要么一开始就是 HIGH，要么永远不是。
     */
    fun ensureChannel(context: Context) {
      if (Build.VERSION.SDK_INT < Build.VERSION_CODES.O) return
      val system = context.getSystemService(NotificationManager::class.java) ?: return
      if (system.getNotificationChannel(CHANNEL_MESSAGES) != null) return
      val channel = NotificationChannel(
        CHANNEL_MESSAGES,
        context.getString(R.string.notif_channel_messages),
        NotificationManager.IMPORTANCE_HIGH,
      ).apply {
        description = context.getString(R.string.notif_channel_messages_desc)
        setShowBadge(true)
      }
      system.createNotificationChannel(channel)
      Log.i(TAG, "消息通知渠道已建：$CHANNEL_MESSAGES（importance=HIGH）")
    }

    /** 日志 tag；与 `MainActivity` / `OrbDropService` 同一串（`adb logcat -s orbdrop-mdns` 一处看全）。 */
    private const val TAG = "orbdrop-mdns"
  }
}