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
 * 为什么非要有它：前台服务（`OrbDropService`）+ 两把 Wi-Fi 锁在 AOSP 上够用，但部分厂商 ROM
 * 另有一层自己的权限位（自启动 / 关联启动 / 后台活动 / 后台耗电管理）。**那一层是 ROM 私有的：
 * 应用既改不了，也读不到**，只能把用户送到那一页、并说清楚要开哪几个开关。这不是纸上的担心：
 * 真机实测过 —— 这些开关留在默认档时，进程在后台被冻结：通知栏那条常驻通知还在（它由系统栏画），
 * 而 mDNS 广播与 HTTP 应答一起停，对端看到的就是"这台离线了"，且**没有任何异常可看**。
 *
 * 还有一层**不在自启动页上**的开关：部分厂商的「后台耗电管理」会在熄屏后把前台服务进程直接
 * 放进 freezer（真机实测：熄屏后 `/sys/fs/cgroup/uid_<uid>/pid_<pid>/cgroup.freeze=1`，
 * Rust 服务器与 5s 探活一起停，对端判本机离线；`OrbDropService` 的 watchdog 留下
 * "上一拍在 1573s 前"）。所以自启动那套候选之外另有一条 [openBackgroundPowerSettings]，
 * 界面也单独有一格说明。
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
    invoke.reject(activity.getString(R.string.settings_battery_optimization_failed))
  }

  /**
   * 跳到 ROM 的「自启动 / 后台活动」页（部分厂商 ROM 才有的那一层，见类注释）。
   *
   * 候选组件是**社区长期在用**的那几个类名，但因为两件事必须逐个试、不能只试一个：
   * ① 同一厂商的不同大版本会在几个类名之间来回换；
   * ② 新机型上这些组件**未导出**，`startActivity` 直接抛 `SecurityException`
   *    （`Permission Denial: starting Intent { cmp=<vendor>/... }`，见 AutoStarter 等库的 issue）。
   *
   * 候选表覆盖主流 ROM 的已知页面；不在表里、或类名失效的，一律退到**本应用详情页**：
   * 那一页一定进得去，用户从「应用信息 → 耗电管理 / 权限」一般也能走到同一批开关 ——
   * 比丢一句"没有应用可执行此操作"强。
   *
   * 返回值里带上是哪一页（`opened`）：界面据此说"已经帮你打开「系统启动管理」"，
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
    invoke.reject(activity.getString(R.string.settings_autostart_failed))
  }

  /**
   * 跳到「后台耗电管理」页（「允许后台运行 / 允许后台高耗电」通常在那里）。
   *
   * 为什么单独一条：这一层不在「自启动管理」页上，也不是电池优化白名单 —— 部分厂商 ROM 用它
   * 在熄屏后把前台服务进程整个冻住（见类注释里的真机证据）。先试已知的公开 action，
   * 再试组件名；都不成退到应用详情页，让用户自己找到「电池 / 耗电管理」。
   *
   * 和自启动那一格一样，**开关状态读不到**：命令只负责把用户送到那一页，界面文案说明选哪一项。
   */
  @Command
  fun openBackgroundPowerSettings(invoke: Invoke) {
    // 有的页面要把本应用带进去才直接落在它那一行（小米的 HiddenAppsConfigActivity 就是这种）。
    val appLabel = activity.applicationInfo.loadLabel(activity.packageManager).toString()
    val candidates = listOf(
      "系统后台耗电管理" to Intent().setComponent(
        ComponentName("com.miui.powerkeeper", "com.miui.powerkeeper.ui.HiddenAppsConfigActivity"),
      ).apply {
        putExtra("package_name", activity.packageName)
        putExtra("package_label", appLabel)
      },
    ) + BACKGROUND_POWER_PAGES
    for ((label, intent) in candidates) {
      if (tryStart(intent)) {
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
    invoke.reject(activity.getString(R.string.settings_background_power_failed))
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
     * 有的 ROM 把自启动 / 关联启动 / 后台活动放在**同一个页面**上，所以同一标签下会挂多条候选。
     */
    val AUTO_START_PAGES: List<Pair<String, ComponentName>> = listOf(
      "系统启动管理" to ComponentName(
        "com.huawei.systemmanager",
        "com.huawei.systemmanager.startupmgr.ui.StartupNormalAppListActivity",
      ),
      "系统启动管理" to ComponentName(
        "com.huawei.systemmanager",
        "com.huawei.systemmanager.appcontrol.activity.StartupAppControlActivity",
      ),
      "系统启动管理" to ComponentName(
        "com.huawei.systemmanager",
        "com.huawei.systemmanager.optimize.process.ProtectActivity",
      ),
      "系统启动管理" to ComponentName(
        "com.miui.securitycenter",
        "com.miui.permcenter.autostart.AutoStartManagementActivity",
      ),
      "系统启动管理" to ComponentName(
        "com.coloros.safecenter",
        "com.coloros.safecenter.permission.startup.StartupAppListActivity",
      ),
      "系统启动管理" to ComponentName(
        "com.oppo.safe",
        "com.oppo.safe.permission.startup.StartupAppListActivity",
      ),
      "系统启动管理" to ComponentName(
        "com.coloros.safecenter",
        "com.coloros.safecenter.startupapp.StartupAppListActivity",
      ),
      "系统启动管理" to ComponentName(
        "com.vivo.permissionmanager",
        "com.vivo.permissionmanager.activity.BgStartUpManagerActivity",
      ),
      "系统启动管理" to ComponentName(
        "com.iqoo.secure",
        "com.iqoo.secure.ui.phoneoptimize.AddWhiteListActivity",
      ),
      "系统启动管理" to ComponentName(
        "com.iqoo.secure",
        "com.iqoo.secure.ui.phoneoptimize.BgStartUpManager",
      ),
      "系统电池设置" to ComponentName(
        "com.samsung.android.lool",
        "com.samsung.android.sm.ui.battery.BatteryActivity",
      ),
      "系统电池设置" to ComponentName(
        "com.samsung.android.lool",
        "com.samsung.android.sm.battery.ui.usage.CheckableAppListActivity",
      ),
      "系统电池设置" to ComponentName(
        "com.samsung.android.lool",
        "com.samsung.android.sm.battery.ui.BatteryActivity",
      ),
      "系统启动管理" to ComponentName(
        "com.asus.mobilemanager",
        "com.asus.mobilemanager.powersaver.PowerSaverSettings",
      ),
      "系统启动管理" to ComponentName(
        "com.asus.mobilemanager",
        "com.asus.mobilemanager.autostart.AutoStartActivity",
      ),
      "系统启动管理" to ComponentName(
        "com.oneplus.security",
        "com.oneplus.security.chainlaunch.view.ChainLaunchAppListActivity",
      ),
      "系统启动管理" to ComponentName(
        "com.evenwell.powersaving.g3",
        "com.evenwell.powersaving.g3.exception.PowerSaverExceptionActivity",
      ),
      "系统启动管理" to ComponentName(
        "com.letv.android.letvsafe",
        "com.letv.android.letvsafe.AutobootManageActivity",
      ),
      "系统权限设置" to ComponentName(
        "com.meizu.safe",
        "com.meizu.safe.permission.SmartBGActivity",
      ),
    )

    /**
     * 「后台耗电管理」页的候选。
     *
     * 公开 action（`exported=true`、没有额外权限）优先，组件名是给同一家旧版本 / 换类名留的
     * 退路；两条都不成时调用方退到应用详情页。类名是厂商私有实现，**只作为运行时标识**，
     * 不面向用户展示。
     *
     * **不要**把它塞进 [AUTO_START_PAGES]：自启动页上没有这个开关，用户会以为已经开完了。
     */
    val BACKGROUND_POWER_PAGES: List<Pair<String, Intent>> = listOf(
      // 公开 action 优先，组件名兜底
      "系统后台耗电管理" to Intent("com.iqoo.powersaving.battery.high.power.jump"),
      "系统后台耗电管理" to Intent().setComponent(
        ComponentName(
          "com.iqoo.powersaving",
          "com.iqoo.powersaving.BackgroundHighUsageActivity",
        ),
      ),
      // 系统后台优化页（通用 action，放在最后，避免抢先于厂商专属页面）
      "系统后台耗电管理" to Intent().setComponent(
        ComponentName(
          "com.oneplus.security",
          "com.oneplus.security.chainlaunch.view.ChainLaunchAppListActivity",
        ),
      ),
      // 电池 / 休眠应用页
      "系统后台耗电管理" to Intent().setComponent(
        ComponentName(
          "com.samsung.android.lool",
          "com.samsung.android.sm.battery.ui.usage.CheckableAppListActivity",
        ),
      ),
      "系统后台耗电管理" to Intent().setComponent(
        ComponentName(
          "com.samsung.android.lool",
          "com.samsung.android.sm.battery.ui.BatteryActivity",
        ),
      ),
      // 应用启动管理页也管后台活动
      "系统后台耗电管理" to Intent().setComponent(
        ComponentName(
          "com.huawei.systemmanager",
          "com.huawei.systemmanager.optimize.process.ProtectActivity",
        ),
      ),
      "系统后台耗电管理" to Intent().setComponent(
        ComponentName(
          "com.huawei.systemmanager",
          "com.huawei.systemmanager.startupmgr.ui.StartupNormalAppListActivity",
        ),
      ),
      // 自启动页通常也带后台管理
      "系统后台耗电管理" to Intent().setComponent(
        ComponentName(
          "com.coloros.safecenter",
          "com.coloros.safecenter.permission.startup.StartupAppListActivity",
        ),
      ),
      // 各自的省电 / 后台页
      "系统后台耗电管理" to Intent().setComponent(
        ComponentName(
          "com.asus.mobilemanager",
          "com.asus.mobilemanager.powersaver.PowerSaverSettings",
        ),
      ),
      "系统后台耗电管理" to Intent().setComponent(
        ComponentName(
          "com.evenwell.powersaving.g3",
          "com.evenwell.powersaving.g3.exception.PowerSaverExceptionActivity",
        ),
      ),
      "系统后台耗电管理" to Intent().setComponent(
        ComponentName(
          "com.meizu.safe",
          "com.meizu.safe.permission.SmartBGActivity",
        ),
      ),
      // 通用后台优化 action：上面都不成时再试（避免抢先于厂商专属页）
      "系统后台耗电管理" to Intent("com.android.settings.action.BACKGROUND_OPTIMIZE"),
    )
  }
}