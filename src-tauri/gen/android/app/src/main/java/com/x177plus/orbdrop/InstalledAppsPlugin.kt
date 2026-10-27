package com.x177plus.orbdrop

import android.app.Activity
import android.content.Intent
import android.content.pm.PackageInfo
import android.graphics.Bitmap
import android.graphics.Canvas
import android.graphics.Path
import android.graphics.drawable.AdaptiveIconDrawable
import android.os.Build
import android.util.Base64
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSArray
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import java.io.ByteArrayOutputStream
import java.io.File
import java.text.Collator
import java.util.Locale

/**
 * 「发送应用」那张表的数据来源（F-119，后端 `mobile::list_installed_apps` 那条命令）。
 *
 * **只报事实，不拼名字**：这里交回标签 / 包名 / 版本名 / 版本号 / APK 路径 / 大小 / 是不是
 * 拆包安装，落盘名（`应用名_versionName(versionCode)_包名.apk`）由 Rust 那边拼
 * （`transfer::resume::apk_file_name`）。理由：名字要先过**接收侧那份净化判据**（`safe_join`
 * 那一关遇到 `:` 之类是整次接收判失败），而那份判据在 Rust 里；两处各写一套迟早分叉。
 *
 * **为什么读得到别人的 APK**：`ApplicationInfo.sourceDir` 就是那份 `base.apk`
 * （`/data/app/~~…/base.apk`）。它在 Linux 这一层是世界可读的，SELinux 也给所有 app 域放行
 * （AOSP `public/app.te`：`allow appdomain apk_data_file:dir r_dir_perms` +
 * `allow appdomain apk_data_file:file rx_file_perms`），所以**不需要 root、也不需要任何
 * 运行时权限** —— 与"APK 提取器"那类应用是同一个机制。
 *
 * **为什么要声明 `<queries>`**（见 `AndroidManifest.xml`）：Android 11 起包可见性过滤，
 * 没声明的话 `queryIntentActivities` 只回极少数几项（常常只有本应用自己），这张表就是空的。
 * 用的是 intent 签名（MAIN + LAUNCHER）而不是 `QUERY_ALL_PACKAGES`：用户说"应用"指的就是
 * 有桌面图标的那些。
 *
 * **这一趟是只读的**：一次 `queryIntentActivities` + 每包一次 `getPackageInfo` + 一次 `stat`，
 * 不读字节、不拷文件 —— 与 `PickFilePlugin`（选中的东西要整份拷进缓存）不是一回事。
 *
 * 一个包可能有多个入口 Activity（`queryIntentActivities` 会各回一条）：按**包名去重**，
 * 表里一行一个应用，与用户在系统"设置 → 应用"里看到的口径一致。
 */
@TauriPlugin
class InstalledAppsPlugin(private val activity: Activity) : Plugin(activity) {

  /**
   * 一行图标的参数（字段名与 Rust 那边 `serde_json::json!` 的键一一对应）。
   */
  @InvokeArg
  class AppIconArgs {
    var packageName: String? = null
  }

  /**
   * 包名 → 图标的 `data:` URL（进程内缓存）。
   *
   * 为什么要缓存：界面按行取（滑动、搜索都会重建行），而画一张图标要 `getApplicationIcon` +
   * 画布 + PNG 压缩 —— 同一条重复取就是白花。缓存的规模上限就是"这台设备上的应用数"
   * （几十到几百张几 KB 的小图），不需要淘汰。
   */
  private val iconCache = HashMap<String, String>()

  /** 列出这台设备上**有桌面图标**的应用（按标签排序，中文按拼音）。 */
  @Command
  fun list(invoke: Invoke) {
    try {
      val pm = activity.packageManager
      val intent = Intent(Intent.ACTION_MAIN).addCategory(Intent.CATEGORY_LAUNCHER)
      val rows = JSArray()
      val seen = HashSet<String>()
      for (resolved in pm.queryIntentActivities(intent, 0)) {
        val info = resolved.activityInfo ?: continue
        val pkg = info.packageName ?: continue
        if (!seen.add(pkg)) continue
        val app = info.applicationInfo ?: continue
        // 正在被卸载/停用的包在这一刻会抛 NameNotFoundException：跳过它，整张表照出。
        val version = try {
          pm.getPackageInfo(pkg, 0)
        } catch (e: Exception) {
          continue
        }
        val apk = File(app.sourceDir ?: continue)
        val row = JSObject()
        // 标签是**别的应用自己声明的**字符串（`"X: 编辑器"` 这种真的存在），这里原样交给
        // 后端去净化 —— 这一层不做任何字符处理，免得出现"两处各有一套净化规则"。
        row.put("label", app.loadLabel(pm).toString())
        row.put("packageName", pkg)
        row.put("versionName", version.versionName ?: "")
        row.put("versionCode", versionCodeOf(version))
        row.put("path", apk.absolutePath)
        row.put("size", apk.length())
        // 拆包安装（AAB：base.apk + config.*.apk）：单发 base.apk 对端多半装不上，
        // 界面据此在那一行标注出来（发不发、要不要摊开说，是界面的事）。
        row.put("hasSplits", !app.splitSourceDirs.isNullOrEmpty())
        rows.put(row)
      }

      // 排序放在这一层：`Collator` 是按**系统语言**排的，中文标签按拼音而不是按码位
      // （按码位排出来是"乱"的 —— 界面那边只能拿到已经排好的一列）。
      val collator = Collator.getInstance(Locale.getDefault())
      val sorted = (0 until rows.length())
        .map { rows.getJSONObject(it) }
        .sortedWith { a, b -> collator.compare(a.optString("label"), b.optString("label")) }

      val out = JSObject()
      val list = JSArray()
      sorted.forEach { list.put(it) }
      out.put("apps", list)
      invoke.resolve(out)
    } catch (e: Exception) {
      invoke.reject(e.message ?: e.toString())
    }
  }

  /**
   * 版本号：API 28 起有 64 位的 `longVersionCode`（老的 `versionCode` 在 2.0 那种量级上会
   * 溢出成负数），更老的系统只能读那个 32 位字段。minSdk 是 24，两个分支都得留着。
   */
  private fun versionCodeOf(info: PackageInfo): Long =
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.P) {
      info.longVersionCode
    } else {
      @Suppress("DEPRECATION")
      info.versionCode.toLong()
    }

  /**
   * 一个应用的图标（`data:image/png;base64,…`）—— **界面按行按需取**，不是列表那一趟一起给。
   *
   * 为什么分开：一百多个应用的图标一起画、再一起塞进一次 IPC 的返回值，会让"打开这张表"变成
   * 一次几百毫秒的等待；而图标是"看得见才需要"的东西。分开之后表本身立刻就出来（那一行先显示
   * 首字母），图标一张张补上；取过的进 [iconCache]，重开这张表就不再走这一趟。
   *
   * 失败（正在卸载、图标坏了）如实 `reject`：界面那一行退回首字母，不会空一块 —— 所以**失败
   * 不进缓存**（下次还可能好）。
   */
  @Command
  fun icon(invoke: Invoke) {
    val pkg = invoke.parseArgs(AppIconArgs::class.java).packageName
    if (pkg.isNullOrEmpty()) {
      invoke.reject(activity.getString(R.string.apps_missing_package))
      return
    }
    iconCache[pkg]?.let {
      respond(invoke, it)
      return
    }
    try {
      val url = renderIcon(pkg)
      iconCache[pkg] = url
      respond(invoke, url)
    } catch (e: Exception) {
      invoke.reject(e.message ?: e.toString())
    }
  }

  private fun respond(invoke: Invoke, url: String) {
    val out = JSObject()
    out.put("icon", url)
    invoke.resolve(out)
  }

  /**
   * 画一张正方形的 PNG 图标。
   *
   * 自适应图标（API 26+）要多做两件事：`AdaptiveIconDrawable` **自己不带遮罩** —— 直接铺满
   * 会把"打底色"也画出来（前景看着比别处小一圈，与系统里、与别的图标都对不齐），而系统那边
   * 可见的是 108 帧里中间那块 72（再放大到图标尺寸）。所以先把整帧放大到让中间 72 正好铺满
   * 目标尺寸，再按系统那套比例裁一圈圆角。
   *
   * 普通图标照原样铺满（它们本身已经是"画好的样子"）。
   */
  private fun renderIcon(pkg: String): String {
    val drawable = activity.packageManager.getApplicationIcon(pkg)
    val size = ICON_PX
    val bitmap = Bitmap.createBitmap(size, size, Bitmap.Config.ARGB_8888)
    val canvas = Canvas(bitmap)
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.O && drawable is AdaptiveIconDrawable) {
      val box = size * 108f / 72f
      val off = (size - box) / 2f
      drawable.setBounds(off.toInt(), off.toInt(), (off + box).toInt(), (off + box).toInt())
      canvas.clipPath(roundedSquare(size, 0.22f))
    } else {
      drawable.setBounds(0, 0, size, size)
    }
    drawable.draw(canvas)

    val bytes = ByteArrayOutputStream()
    bitmap.compress(Bitmap.CompressFormat.PNG, 100, bytes)
    return "data:image/png;base64," + Base64.encodeToString(bytes.toByteArray(), Base64.NO_WRAP)
  }

  /** 圆角矩形路径（半径按边长的比例给）。 */
  private fun roundedSquare(size: Int, ratio: Float): Path {
    val r = size * ratio
    val path = Path()
    path.addRoundRect(0f, 0f, size.toFloat(), size.toFloat(), r, r, Path.Direction.CW)
    return path
  }

  private companion object {
    /** 画多大：列表那一行显示 34–40dp，144px 在任何密度下都够清楚，PNG 也就几 KB。 */
    const val ICON_PX = 144
  }
}