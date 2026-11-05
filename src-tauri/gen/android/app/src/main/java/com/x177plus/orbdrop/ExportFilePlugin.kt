package com.x177plus.orbdrop

import android.app.Activity
import android.content.ContentValues
import android.content.Intent
import android.net.Uri
import android.os.Build
import android.os.Environment
import android.provider.MediaStore
import android.webkit.MimeTypeMap
import androidx.activity.result.ActivityResult
import androidx.annotation.RequiresApi
import app.tauri.annotation.ActivityCallback
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import java.io.File

/**
 * 「把收件目录里的一份文件存进系统共享存储」在安卓上的落点（后端 `save_file_to_phone` 那条命令）。
 *
 * **为什么需要它**：本应用的收件、会话图片、粘贴/选择暂存全在应用私有数据目录里
 * （`dataDir/OrbDrop`，见 `config.rs::default_download_dir` 与 `lib.rs` 里写 `ORBDROP_DATA_DIR`
 * 那一段）—— 那个目录**别的应用看不到**：相册里搜不到收到的图，微信/邮件这类"挑一张图 / 挑一个
 * 文件"的选择器也挑不到它。`FileOpenPlugin` 解决的是"用别的应用**打开**这一份"，解决不了
 * "让别的应用**看见并拿去用**"。所以这里复制一份进系统媒体库 / 下载目录。
 *
 * **两条路，都免存储权限**：
 *
 * 1. Android 10（API 29）起直接写 `MediaStore`（`RELATIVE_PATH` + `IS_PENDING`）：不需要
 *    `WRITE_EXTERNAL_STORAGE`，也不需要用户点任何东西 —— 收件里那张图一按就出现在相册的
 *    `Pictures/OrbDrop` 里，视频进 `Movies/OrbDrop`，其余文件进 `Download/OrbDrop`。
 * 2. Android 9 及以下没有 `RELATIVE_PATH`，按路径写公共目录要 `WRITE_EXTERNAL_STORAGE`
 *    （运行时权限）；本应用一直不申请存储权限（见 `docs/p1-implementation.md` §44 里
 *    "不需要任何存储权限"那套取舍），所以这一档改开系统的「保存到」对话框
 *    （`ACTION_CREATE_DOCUMENT`，SAF）：用户自己挑落点，系统把那个 URI 的写授权随结果给我们，
 *    同样免权限。用户在对话框里按返回 = **取消**，回 `{saved:false}`，不是错误。
 *
 * **为什么不是"直接落到公共目录，收件也放那儿"**：分区存储下应用按路径写公共目录在
 * Android 11 起就不成立（要 `MANAGE_EXTERNAL_STORAGE` 或整条 MediaStore 落盘层）；收件链路
 * 只认真实路径（清单、分块读字节、续传），改不动也不该为此改。复制一份是这里唯一干净的做法。
 *
 * **落点与重名**：落点是固定子目录（`Pictures|Movies|Music|Download` 下的 `OrbDrop/`），
 * 重名先问媒体库里有没有（同 `DISPLAY_NAME` + `RELATIVE_PATH`），有就按 `名字 (1).ext` 往后排
 * —— 与桌面端落盘的 S-01 口径一致，用户看到的是两份不同的文件，而不是一条被覆盖的消息。
 */
@InvokeArg
class ExportArgs {
  lateinit var path: String
  /**
   * 界面上的显示名（收件项的文件名 / 会话里那张图的名字）。
   *
   * 会话图片的路径是 `chat-vault/blobs/<消息 id>.bin`（封装格式，Rust 侧已经解成一份原始图片的
   * 临时文件），**名字必须由界面给**：拿路径的文件名会得到 `<id>.bin` —— 相册里一个打不开的
   * 二进制文件，这正是真机反馈的那个 bug。缺省时退回文件自己的名字。
   */
  var name: String? = null
}

@TauriPlugin
class ExportFilePlugin(private val activity: Activity) : Plugin(activity) {

  @Command
  fun saveToPublic(invoke: Invoke) {
    val args = try {
      invoke.parseArgs(ExportArgs::class.java)
    } catch (e: Exception) {
      invoke.reject(e.message ?: e.toString())
      return
    }
    val file = try {
      val owned = AppFiles.requireOwned(activity, args.path, R.string.export_not_owned)
      if (!owned.exists()) throw IllegalArgumentException(activity.getString(R.string.export_missing, args.path))
      if (owned.isDirectory) throw IllegalArgumentException(activity.getString(R.string.export_is_directory))
      owned
    } catch (e: Exception) {
      invoke.reject(e.message ?: e.toString())
      return
    }
    val display = displayName(args.name, file)
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.Q) {
      // 一个 1 GB 的视频不该占着 UI 线程（同 `PickFilePlugin.picked` 的取舍）。
      Thread {
        try {
          val out = saveToMediaStore(file, display)
          activity.runOnUiThread { invoke.resolve(out) }
        } catch (e: Exception) {
          activity.runOnUiThread { invoke.reject(e.message ?: e.toString()) }
        }
      }.start()
      return
    }
    // 老系统：交给系统「保存到」对话框（SAF）。打不开就是真的没有可用的文件管理器。
    try {
      startActivityForResult(invoke, createDocumentIntent(display, mimeOf(display, file)), "created")
    } catch (e: Exception) {
      invoke.reject(e.message ?: activity.getString(R.string.export_open_failed))
    }
  }

  /** 老系统那条 SAF 路的结果：`RESULT_OK` = 用户挑好了落点，取消 = `{saved:false}`。 */
  @ActivityCallback
  fun created(invoke: Invoke, result: ActivityResult) {
    val target = result.data?.data
    if (result.resultCode != Activity.RESULT_OK || target == null) {
      resolveSaved(invoke, false, "chosen")
      return
    }
    // 源路径从这次调用的原始参数里再读一次（`Invoke` 一直带着 argsJson，见 `Invoke.getArgs`）。
    val source = try {
      AppFiles.requireOwned(
        activity,
        invoke.parseArgs(ExportArgs::class.java).path,
        R.string.export_not_owned,
      )
    } catch (e: Exception) {
      invoke.reject(e.message ?: e.toString())
      return
    }
    Thread {
      try {
        copyInto(target, source)
        activity.runOnUiThread { resolveSaved(invoke, true, "chosen") }
      } catch (e: Exception) {
        activity.runOnUiThread { invoke.reject(e.message ?: e.toString()) }
      }
    }.start()
  }

  /**
   * Android 10+ 直写媒体库。返回前把 `IS_PENDING` 清掉 —— 不清的话这一条对相册/选择器
   * **永远不可见**，用户只会看到一句"已保存"却哪儿都找不到。
   */
  @RequiresApi(Build.VERSION_CODES.Q)
  private fun saveToMediaStore(file: File, display: String): JSObject {
    val resolver = activity.contentResolver
    val mime = mimeOf(display, file)
    val collection: Uri
    val dir: String
    val place: String
    when {
      mime.startsWith("image/") -> {
        collection = MediaStore.Images.Media.EXTERNAL_CONTENT_URI
        dir = Environment.DIRECTORY_PICTURES
        place = "gallery"
      }
      mime.startsWith("video/") -> {
        collection = MediaStore.Video.Media.EXTERNAL_CONTENT_URI
        dir = Environment.DIRECTORY_MOVIES
        place = "gallery"
      }
      mime.startsWith("audio/") -> {
        collection = MediaStore.Audio.Media.EXTERNAL_CONTENT_URI
        dir = Environment.DIRECTORY_MUSIC
        place = "music"
      }
      else -> {
        collection = MediaStore.Downloads.EXTERNAL_CONTENT_URI
        dir = Environment.DIRECTORY_DOWNLOADS
        place = "downloads"
      }
    }
    // 末尾的斜杠是 `RELATIVE_PATH` 的规范形状（媒体库自己写回来的也是这样），重名查询要用它。
    val relative = "$dir/OrbDrop/"
    val values = ContentValues().apply {
      put(MediaStore.MediaColumns.DISPLAY_NAME, uniqueName(collection, relative, display))
      put(MediaStore.MediaColumns.MIME_TYPE, mime)
      put(MediaStore.MediaColumns.RELATIVE_PATH, relative)
      put(MediaStore.MediaColumns.IS_PENDING, 1)
    }
    val uri = resolver.insert(collection, values)
      ?: throw IllegalStateException(activity.getString(R.string.export_no_output))
    try {
      copyInto(uri, file)
      values.clear()
      values.put(MediaStore.MediaColumns.IS_PENDING, 0)
      resolver.update(uri, values, null, null)
    } catch (e: Exception) {
      // 失败就删掉那条占位记录，别在相册里留一条 0 字节、永远打不开的残骸。
      runCatching { resolver.delete(uri, null, null) }
      throw e
    }
    return savedObject(true, place)
  }

  /** 把源文件的字节写进已经拿到的目标 URI（两条路共用）。 */
  private fun copyInto(target: Uri, source: File) {
    val out = activity.contentResolver.openOutputStream(target)
      ?: throw IllegalStateException(activity.getString(R.string.export_no_output))
    out.use { sink ->
      source.inputStream().use { it.copyTo(sink) }
    }
  }

  /**
   * 同一目录下不重名的显示名：`名字.ext` → `名字 (1).ext` → …（最多问 999 次，之后带时间戳，
   * 免得在一个几千项的相册里排到天荒地老）。
   */
  @RequiresApi(Build.VERSION_CODES.Q)
  private fun uniqueName(collection: Uri, relative: String, name: String): String {
    val dot = name.lastIndexOf('.')
    val stem = if (dot > 0) name.substring(0, dot) else name
    val ext = if (dot > 0) name.substring(dot) else ""
    var n = 0
    while (n < 999) {
      val candidate = if (n == 0) name else "$stem ($n)$ext"
      if (!exists(collection, relative, candidate)) return candidate
      n += 1
    }
    return "$stem (${System.currentTimeMillis()})$ext"
  }

  /** 媒体库里同目录、同名的那一项在不在（`RELATIVE_PATH` 是 API 29 才有的列）。 */
  @RequiresApi(Build.VERSION_CODES.Q)
  private fun exists(collection: Uri, relative: String, name: String): Boolean {
    val selection =
      "${MediaStore.MediaColumns.DISPLAY_NAME} = ? AND ${MediaStore.MediaColumns.RELATIVE_PATH} = ?"
    val args = arrayOf(name, relative)
    activity.contentResolver
      .query(collection, arrayOf(MediaStore.MediaColumns._ID), selection, args, null)
      ?.use { cursor -> return cursor.moveToFirst() }
    return false
  }

  /** 老系统那条：系统的「保存到」对话框，带一个像样的默认名字与 MIME。 */
  private fun createDocumentIntent(display: String, mime: String): Intent =
    Intent(Intent.ACTION_CREATE_DOCUMENT).apply {
      addCategory(Intent.CATEGORY_OPENABLE)
      type = mime
      putExtra(Intent.EXTRA_TITLE, display)
    }

  /**
   * 共享存储里该用的名字：界面给的显示名优先，缺省退回文件自己的名字。
   *
   * **只留最后一段**：显示名可能来自对端（会话图片的显示名就是一条消息里的字符串），带
   * `/` 或 `\` 时不能让它变成子目录。Rust 那一侧对封装内容已经净化过一次，这里是第二道。
   */
  private fun displayName(requested: String?, file: File): String {
    val raw = requested?.trim()?.takeIf { it.isNotEmpty() } ?: file.name
    val leaf = raw.substringAfterLast('/').substringAfterLast('\\').trim()
    return leaf.ifEmpty { file.name }
  }

  /**
   * 按**显示名**的扩展名问系统要 MIME（问不到再退回源文件的扩展名，最后是通用的二进制
   * 类型）。名字优先：会话图片的源文件是解封装出来的临时文件，名字才是对用户有意义的那份；
   * 而且媒体库按 MIME 决定进相册还是下载。
   *
   * `apk` 先钉死：系统那张表里不一定有它，而这是一个会出现在收件里的常见类型。
   */
  private fun mimeOf(name: String, file: File): String {
    val ext = name.substringAfterLast('.', "").lowercase()
    if (ext == "apk") return "application/vnd.android.package-archive"
    MimeTypeMap.getSingleton().getMimeTypeFromExtension(ext)?.let { return it }
    val fileExt = file.extension.lowercase()
    if (fileExt == "apk") return "application/vnd.android.package-archive"
    return MimeTypeMap.getSingleton().getMimeTypeFromExtension(fileExt)
      ?: "application/octet-stream"
  }

  private fun resolveSaved(invoke: Invoke, saved: Boolean, place: String) {
    invoke.resolve(savedObject(saved, place))
  }

  private fun savedObject(saved: Boolean, place: String): JSObject = JSObject().apply {
    put("saved", saved)
    put("place", place)
  }
}