package com.x177plus.orbdrop

import android.app.Activity
import android.content.ComponentName
import android.content.Intent
import android.net.Uri
import android.os.PowerManager
import android.provider.Settings
import android.util.Log
import app.tauri.annotation.Command
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin

/**
 * 「让本应用在后台活着」所需要的**系统那一层**（F-87，机制与取舍见 `docs/p1-implementation.md` §22）。
 *
 * 为什么非要有它：前台服务（`OrbDropService`）+ 两把 Wi-Fi 锁在 AOSP 上够用，但国产 ROM 另有一层
 * 自己的权限位 —— 华为「应用启动管理」（自启动 / 关联启动 / 后台活动）、小米「自启动」、OPPO/vivo
 * 同类开关。**那一层是 ROM 私有的：应用既改不了，也读不到**，只能把用户送到那一页、并说清楚
 * 要开哪几个开关。这不是纸上的担心：本仓在华为 JAD-AL80 上实测过 —— 那三个开关留在默认的
 * 「自动管理」时，进程在后台被冻结：通知栏那条常驻通知还在（它由系统栏画），而 mDNS 广播与
 * HTTP 应答一起停，对端看到的就是"这台离线了"，且**没有任何异常可看**。
 *
 * 这一层里**能读**的只有一样：电池优化白名单（`PowerManager.isIgnoringBatteryOptimizations`，
 * Google 官方 API）。所以界面上"已允许 / 未允许"那一行是真的，其余只能"跳过去 + 让用户自己看"。
 *
 * 三件事写在一个插件里，因为它们都是"跳系统页 + 读回那一个能读到的状态"，没有业务逻辑；
 * 通知那一侧（渠道、横幅、通知设置页）归 `MessageNotifierPlugin`，这里一行都不碰。
 *
 * 每一句会端到界面上的话（`invoke.reject` 的字符串）都按"给人看"写：它会在移动端变成一条 toast。
 */
@TauriPlugin
class SystemSettingsPlugin(private val activity: Activity) : Plugin(activity) {

  /**
   * 本机在不在电池优化白名单里。**唯一能读的系统状态**（见类注释）。
   *
   * 拿不到 `PowerManager` 时按"没在白名单"报（宁可多引导一次，也不要谎报"已允许"）。
   */
  @Command
  fun keepAliveState(invoke: Invoke) {
    val pm = activity.getSystemService(PowerManager::class.java)
    val ignoring = pm?.isIgnoringBatteryOptimizations(activity.packageName) ?: false
    val out = JSObject()
    out.put("batteryOptimizationIgnored", ignoring)
    invoke.resolve(out)
  }

  /**
   * 弹系统那条"是否允许后台运行"（`ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS`）。
   *
   * 这条 action 要求清单里声明 `REQUEST_IGNORE_BATTERY_OPTIMIZATIONS`，否则当场
   * `SecurityException`（本仓库已经声明）。它在 Google Play 上属于**受限用法**（政策要求应用确实
   * 需要常驻），本应用不经商店分发、且承诺就是"后台保持在线"，所以直接用。
   *
   * ROM 不认这条 action 时（个别机型把它收进了白名单列表页）退到
   * `ACTION_IGNORE_BATTERY_OPTIMIZATION_SETTINGS`：那一页让用户自己在列表里找到 OrbDrop 打开。
   * 两条都不成才 reject —— 界面上那句话是"请手动进 设置 → 电池 → 更多电池设置"。
   */
  @Command
  fun requestIgnoreBatteryOptimizations(invoke: Invoke) {
    val direct = Intent(
      Settings.ACTION_REQUEST_IGNORE_BATTERY_OPTIMIZATIONS,
      Uri.parse("package:${activity.packageName}"),
    )
    if (tryStart(direct)) {
      invoke.resolve()
      return
    }
    if (tryStart(Intent(Settings.ACTION_IGNORE_BATTERY_OPTIMIZATION_SETTINGS))) {
      invoke.resolve()
      return
    }
    invoke.reject("打不开电池优化设置：请手动进入 设置 → 电池 → 应用启动管理/更多电池设置")
  }

  /**
   * 跳到 ROM 的「自启动 / 后台活动」页（国产 ROM 才有的那一层，见类注释）。
   *
   * 候选组件是**社区长期在用**的那几个类名，但因为两件事必须逐个试、不能只试一个：
   * ① 同一个 ROM 的不同版本会在几个类名之间来回换（EMUI → HarmonyOS 换过，小米也换过）；
   * ② 新机型上这些组件**未导出**，`startActivity` 直接抛 `SecurityException`
   *    （`Permission Denial: starting Intent { cmp=com.huawei.systemmanager/.startupmgr.ui.… }`，
   *    见 AutoStarter 等库的 issue）。
   *
   * 全都不成时退回**本应用详情页**：那一页一定进得去，用户从「应用信息 → 耗电管理 / 权限」一般
   * 也能走到同一批开关 —— 比丢一句"没有应用可执行此操作"强。
   *
   * 返回值里带上是哪一页（`opened`）：界面据此说"已经帮你打开「华为·应用启动管理」"，
   * 而不是笼统的"已跳转"。
   */
  @Command
  fun openAutoStartSettings(invoke: Invoke) {
    for ((label, component) in AUTO_START_PAGES) {
      if (tryStart(Intent().setComponent(component))) {
        val out = JSObject()
        out.put("opened", label)
        invoke.resolve(out)
        return
      }
    }
    if (tryStart(appDetails())) {
      val out = JSObject()
      out.put("opened", "应用详情")
      invoke.resolve(out)
      return
    }
    invoke.reject("打不开系统设置页：请手动进入 设置 → 应用 → OrbDrop，打开自启动/关联启动/后台活动")
  }

  /** 本应用在系统设置里的详情页（`ACTION_APPLICATION_DETAILS_SETTINGS`）。 */
  private fun appDetails(): Intent =
    Intent(Settings.ACTION_APPLICATION_DETAILS_SETTINGS, Uri.parse("package:${activity.packageName}"))

  /** 试开一个系统页：成不成都别抛出去 —— 每一次失败都只记一行日志，由调用方决定下一步。 */
  private fun tryStart(intent: Intent): Boolean = try {
    activity.startActivity(intent)
    true
  } catch (e: Exception) {
    Log.i(TAG, "系统设置页跳不过去（${intent.component?.flattenToShortString() ?: intent.action}）：${e.message}")
    false
  }

  private companion object {
    /** 日志 tag；与 `MainActivity` / `OrbDropService` 同一串（`adb logcat -s orbdrop-mdns` 一处看全）。 */
    const val TAG = "orbdrop-mdns"

    /**
     * 「自启动 / 后台活动」页的候选（标签 → 组件）。顺序就是尝试顺序：同一家的版本之间从新到旧。
     *
     * 华为那三个开关（自启动 / 关联启动 / 后台活动）在**同一个页面**上，所以它一个标签下挂三条候选。
     */
    val AUTO_START_PAGES: List<Pair<String, ComponentName>> = listOf(
      "华为·应用启动管理" to ComponentName(
        "com.huawei.systemmanager",
        "com.huawei.systemmanager.startupmgr.ui.StartupNormalAppListActivity",
      ),
      "华为·应用启动管理" to ComponentName(
        "com.huawei.systemmanager",
        "com.huawei.systemmanager.appcontrol.activity.StartupAppControlActivity",
      ),
      "华为·应用启动管理" to ComponentName(
        "com.huawei.systemmanager",
        "com.huawei.systemmanager.optimize.process.ProtectActivity",
      ),
      "小米·自启动管理" to ComponentName(
        "com.miui.securitycenter",
        "com.miui.permcenter.autostart.AutoStartManagementActivity",
      ),
      "OPPO·自启动管理" to ComponentName(
        "com.coloros.safecenter",
        "com.coloros.safecenter.permission.startup.StartupAppListActivity",
      ),
      "OPPO·自启动管理" to ComponentName(
        "com.oppo.safe",
        "com.oppo.safe.permission.startup.StartupAppListActivity",
      ),
      "vivo·自启动管理" to ComponentName(
        "com.vivo.permissionmanager",
        "com.vivo.permissionmanager.activity.BgStartUpManagerActivity",
      ),
      "vivo·自启动管理" to ComponentName(
        "com.iqoo.secure",
        "com.iqoo.secure.ui.phoneoptimize.AddWhiteListActivity",
      ),
      "三星·电池与后台" to ComponentName(
        "com.samsung.android.lool",
        "com.samsung.android.sm.ui.battery.BatteryActivity",
      ),
      "魅族·权限管理" to ComponentName(
        "com.meizu.safe",
        "com.meizu.safe.permission.SmartBGActivity",
      ),
    )
  }
}