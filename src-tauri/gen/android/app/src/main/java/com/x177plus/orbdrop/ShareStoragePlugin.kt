package com.x177plus.orbdrop

import android.app.Activity
import android.content.Intent
import android.net.Uri
import android.os.Build
import android.os.Environment
import android.os.storage.StorageManager
import android.provider.Settings
import android.util.Log
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSArray
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import java.io.File
import java.util.Locale

/**
 * 安卓端作为**共享方**时用的系统那一层：申请「所有文件访问」、列出共享存储的根目录与目录内容。
 *
 * 为什么需要它：共享项在后端是一个**真实路径**（`ShareItem.path: PathBuf`，见 `config.rs`），
 * 而分区存储下应用默认只能读媒体，拿不到 zip/pdf/apk 这类非媒体文件的路径。SAF 的 URI 能读，
 * 但 URI 不是路径，后端整套 `/shares` `/tree` `/file` `/dav` 都按 `std::fs` 读；要把 URI 接进去
 * 就得重写一条虚拟文件系统。这里走另一条：用户在**第一次添加共享项时**按需授予
 * `MANAGE_EXTERNAL_STORAGE`，之后 `File` / Rust `std::fs` 直接读真实路径，现有共享链路一个字不改。
 *
 * **权限与共享范围是两件事**：权限只是"能读共享存储"，真正登记哪些路径仍由用户在选择器里一项项
 * 决定（见 `ShareStoragePlugin.list` + 移动端「本机」段）。所以这里不扫全盘、不自动共享任何东西。
 *
 * 仅 Android 11+（API 30）有这条特殊权限；更老的系统上 `state.supported = false`，界面明确提示
 * 不支持，而不是退到一个行为不一致的 legacy 分支。
 */
@InvokeArg
class ShareListArgs {
  var path: String? = null
}

@TauriPlugin
class ShareStoragePlugin(private val activity: Activity) : Plugin(activity) {

  /** 当前系统是否支持这条特殊权限，以及用户授没授。 */
  @Command
  fun state(invoke: Invoke) {
    val supported = Build.VERSION.SDK_INT >= Build.VERSION_CODES.R
    val out = JSObject()
    out.put("supported", supported)
    out.put("granted", supported && Environment.isExternalStorageManager())
    out.put("sdk", Build.VERSION.SDK_INT)
    invoke.resolve(out)
  }

  /**
   * 打开系统的「所有文件访问」页。
   *
   * 这是特殊应用访问，不是运行时弹框：`startActivity` 之后用户要自己把 OrbDrop 那一行打开，
   * 回来时界面在 `visibilitychange` 里重读 [state]。个别 ROM 不认带包名的那条 action，退到
   * 通用列表页；再不行退到应用详情页，至少让用户能自己找到入口。
   */
  @Command
  fun request(invoke: Invoke) {
    if (Build.VERSION.SDK_INT < Build.VERSION_CODES.R) {
      invoke.reject(activity.getString(R.string.share_storage_unsupported))
      return
    }
    val direct = Intent(
      Settings.ACTION_MANAGE_APP_ALL_FILES_ACCESS_PERMISSION,
      Uri.parse("package:${activity.packageName}"),
    )
    if (tryStart(direct) || tryStart(Intent(Settings.ACTION_MANAGE_ALL_FILES_ACCESS_PERMISSION))) {
      invoke.resolve()
      return
    }
    if (tryStart(appDetails())) {
      invoke.resolve()
      return
    }
    invoke.reject(activity.getString(R.string.share_storage_request_failed))
  }

  /** 共享存储的根（内部存储 / SD 卡…）。没授权时直接拒绝，界面据此引导。 */
  @Command
  fun roots(invoke: Invoke) {
    if (!granted()) {
      invoke.reject(activity.getString(R.string.share_storage_not_granted))
      return
    }
    val out = JSObject()
    val array = JSArray()
    for ((label, dir) in storageRoots()) {
      val item = JSObject()
      item.put("name", label)
      item.put("path", dir.absolutePath)
      array.put(item)
    }
    out.put("roots", array)
    invoke.resolve(out)
  }

  /**
   * 列一层目录。**在后台线程读**：共享存储可能是 SD 卡或几千项的相册目录，主线程上 `listFiles`
   * 足够卡出 ANR（与 `PickFilePlugin` 的拷贝同一套取舍）。
   */
  @Command
  fun list(invoke: Invoke) {
    val path = invoke.parseArgs(ShareListArgs::class.java).path
    if (path.isNullOrBlank()) {
      invoke.reject(activity.getString(R.string.share_storage_read_failed, ""))
      return
    }
    if (!granted()) {
      invoke.reject(activity.getString(R.string.share_storage_not_granted))
      return
    }
    Thread {
      try {
        val result = listDir(path)
        activity.runOnUiThread { invoke.resolve(result) }
      } catch (e: Exception) {
        Log.i(TAG, "列共享目录失败（$path）：${e.message}")
        activity.runOnUiThread {
          invoke.reject(e.message ?: activity.getString(R.string.share_storage_read_failed, path))
        }
      }
    }.start()
  }

  private fun granted(): Boolean =
    Build.VERSION.SDK_INT >= Build.VERSION_CODES.R && Environment.isExternalStorageManager()

  /**
   * 当前挂载的共享存储卷。只收 `MEDIA_MOUNTED` / `MEDIA_MOUNTED_READ_ONLY`，并去重。
   *
   * 主存储排第一（界面默认落在那里）。`StorageVolume.getDirectory()` 是 API 30，调用方已经
   * 用 `granted()` 把入口卡在 Android 11+，因此这里不需要再处理更老的分支。
   */
  @Suppress("NewApi")
  private fun storageRoots(): List<Pair<String, File>> {
    val manager = activity.getSystemService(StorageManager::class.java)
      ?: return emptyList()
    val primary = Environment.getExternalStorageDirectory().absolutePath
    val seen = HashSet<String>()
    val roots = ArrayList<Pair<String, File>>()
    for (volume in manager.storageVolumes) {
      if (volume.state != Environment.MEDIA_MOUNTED &&
        volume.state != Environment.MEDIA_MOUNTED_READ_ONLY
      ) {
        continue
      }
      val dir = volume.directory ?: continue
      if (!dir.canRead()) continue
      val canonical = runCatching { dir.canonicalPath }.getOrNull() ?: continue
      if (!seen.add(canonical)) continue
      val label = runCatching { volume.getDescription(activity) }
        .getOrNull()
        ?.takeUnless { it.isBlank() }
        ?: canonical
      roots.add(label to File(canonical))
    }
    // 极少数 ROM 的 StorageManager 不报卷：退回 Environment 给的那一条，保证选择器不是空的。
    if (roots.isEmpty()) {
      val fallback = Environment.getExternalStorageDirectory()
      val canonical = runCatching { fallback.canonicalPath }.getOrNull()
      if (canonical != null && fallback.canRead()) roots.add(canonical to File(canonical))
    }
    roots.sortBy { (_, dir) -> if (dir.absolutePath == primary) 0 else 1 }
    return roots
  }

  /**
   * 读一层目录，返回 `{ entries, parent? }`。
   *
   * 路径必须先落在某个共享存储根下：前端传上来的字符串不可信，`..`、`/data/...` 或别的
   * provider 的 URI 一律拒掉。隐藏项跳过 —— `routes_tree` 与共享项卡片的子项数也只数非隐藏项，
   * 两处口径必须一致（否则用户会以为漏了东西）。
   */
  @Suppress("NewApi")
  private fun listDir(rawPath: String): JSObject {
    val roots = storageRoots()
    if (roots.isEmpty()) {
      throw IllegalStateException(activity.getString(R.string.share_storage_read_failed, rawPath))
    }
    val dir = File(rawPath)
    val canonical = dir.canonicalFile
    val root = roots.firstOrNull { isUnder(canonical, it.second) }
      ?: throw IllegalArgumentException(activity.getString(R.string.share_storage_out_of_root))
    if (!canonical.isDirectory) {
      throw IllegalArgumentException(activity.getString(R.string.share_storage_read_failed, rawPath))
    }
    if (!canonical.canRead()) {
      throw IllegalArgumentException(activity.getString(R.string.share_storage_read_failed, rawPath))
    }
    val files = canonical.listFiles()
      ?: throw IllegalStateException(activity.getString(R.string.share_storage_read_failed, rawPath))

    val sorted = files
      .asSequence()
      .filter { !it.name.startsWith(".") }
      .sortedWith(
        compareByDescending<File> { it.isDirectory }
          .thenBy { it.name.lowercase(Locale.getDefault()) },
      )
      .toList()
    // 一个几千项的相册目录一次性铺进 WebView 会把界面拖死；这里只给前 500 项，
    // 并明确告诉界面"还有更多"（用户可进入子目录继续找，不会静默少东西）。
    // 先截断再建 JSObject：大目录里那几千个对象本身也是白花的。
    val truncated = sorted.size > MAX_ENTRIES
    val entries = sorted.take(MAX_ENTRIES).map { file ->
      val item = JSObject()
      item.put("name", file.name)
      item.put("path", file.absolutePath)
      item.put("isDir", file.isDirectory)
      // 目录的大小没有意义（也不该为它递归求和）；与后端 `ShareItemDto` 的约定一致。
      item.put("size", if (file.isDirectory) 0L else file.length())
      item.put("modified", file.lastModified())
      item
    }

    val out = JSObject()
    val array = JSArray()
    for (item in entries) array.put(item)
    out.put("entries", array)
    out.put("truncated", truncated)
    val parent = canonical.parentFile
    if (parent != null && isUnder(parent, root.second)) {
      out.put("parent", parent.absolutePath)
    }
    return out
  }

  /** `path` 是不是 `root` 本身，或落在它下面（两边都先 canonical 过）。 */
  private fun isUnder(path: File, root: File): Boolean {
    val p = runCatching { path.canonicalPath }.getOrElse { return false }
    val r = runCatching { root.canonicalPath }.getOrElse { return false }
    return p == r || p.startsWith(r + File.separator)
  }

  private fun appDetails(): Intent =
    Intent(Settings.ACTION_APPLICATION_DETAILS_SETTINGS, Uri.parse("package:${activity.packageName}"))

  /** 试开一个系统页：失败只记日志，由调用方决定下一步。 */
  private fun tryStart(intent: Intent): Boolean = try {
    activity.startActivity(intent)
    true
  } catch (e: Exception) {
    Log.i(TAG, "系统设置页跳不过去（${intent.action}）：${e.message}")
    false
  }

  private companion object {
    const val TAG = "orbdrop-share"

    /** 一层目录最多铺这么多行（见 `listDir` 的说明）。 */
    const val MAX_ENTRIES = 500
  }
}