package com.x177plus.orbdrop

import android.app.Activity
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.Intent
import android.os.Build
import androidx.core.app.NotificationCompat
import androidx.core.app.NotificationManagerCompat
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
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
    ensureChannel()
    manager.notify(notificationId(args.kind, args.id), buildNotification(args))
    invoke.resolve()
  }

  private fun ensureChannel() {
    if (Build.VERSION.SDK_INT < Build.VERSION_CODES.O) return
    val system = activity.getSystemService(NotificationManager::class.java) ?: return
    if (system.getNotificationChannel(CHANNEL_MESSAGES) != null) return
    val channel = NotificationChannel(
      CHANNEL_MESSAGES,
      activity.getString(R.string.notif_channel_messages),
      NotificationManager.IMPORTANCE_HIGH,
    ).apply {
      description = activity.getString(R.string.notif_channel_messages_desc)
      setShowBadge(true)
    }
    system.createNotificationChannel(channel)
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

  private companion object {
    /** 消息通知的渠道（与 `OrbDropService` 那条 `orbdrop.keepalive` 分开，见类注释）。 */
    const val CHANNEL_MESSAGES = "orbdrop.messages"
  }
}