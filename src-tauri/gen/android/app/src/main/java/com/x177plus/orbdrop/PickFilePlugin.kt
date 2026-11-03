package com.x177plus.orbdrop

import android.app.Activity
import android.content.Intent
import android.net.Uri
import android.os.Build
import android.provider.DocumentsContract
import android.provider.MediaStore
import android.provider.OpenableColumns
import android.webkit.MimeTypeMap
import androidx.activity.result.ActivityResult
import app.tauri.annotation.ActivityCallback
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSArray
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import java.io.File
import java.text.SimpleDateFormat
import java.util.ArrayDeque
import java.util.Date
import java.util.HashSet
import java.util.Locale
import java.util.UUID

/**
 * 会话里那颗「＋」在安卓上的落点（后端 `mobile::pick_files` 那条命令）。
 *
 * **为什么要有它**：桌面端那颗「＋」开的是自绘的本机文件浏览器（`FilePicker.svelte` + `fs_browse`
 * 那几条命令），安卓上这套走不通 —— 没有盘符，分区存储也不给"按路径列共享存储"（非媒体文件
 * 连读都读不到，只能走 SAF 的 URI 读授权）。所以这里换成**系统选择器**：图片走照片选择器
 * （Android 13 起；老系统退回 `ACTION_GET_CONTENT`），文件走 `ACTION_GET_CONTENT`。两者都
 * **不需要任何存储权限** —— 系统把选中那几个 URI 的读授权随结果一起给我们。
 *
 * **为什么回来还得拷一份**：`content://` 那串 URI 在 WebView 的 JS 里读不出字节（它属于
 * ContentResolver），而发文件走的是推送、发图片走的是会话消息，两条都只认本机**真实路径**
 * （清单要按路径列、对端要按路径分块取字节，见 `transfer`）。所以这一跳与剪贴板那条
 * （`PasteImagePlugin`）是同一件事：把 URI 后面的字节拷成一份暂存文件、把**路径**交回界面。
 * 落点 `cacheDir/pick/`：应用私有缓存（系统空间紧张时可回收），而且**不在**自家数据目录
 * （`/data/user/0/<包名>/OrbDrop`）那棵子树里 —— 那棵子树会被 `unsafe` 判成"本机凭据"、
 * 当场拒掉推送（见 `unsafe::is_own_data_path` 与 `paste.rs` 的同一段说明）。
 *
 * 取消 = 空列表（**不是**错误）：用户按返回键退出选择器是正常操作，界面那边什么都不该发生。
 *
 * 每一句会端到界面上的话（`invoke.reject` 的字符串）都按"给人看"写：它会在移动端变成一条 toast。
 */
@InvokeArg
class PickArgs {
  /** `"image"`、`"file"` 或 `"folder"`（后端只放这三个值过来；别的按文件处理）。 */
  var kind: String? = null
}

@TauriPlugin
class PickFilePlugin(private val activity: Activity) : Plugin(activity) {

  @Command
  fun pick(invoke: Invoke) {
    val args = invoke.parseArgs(PickArgs::class.java)
    val kind = args.kind
    val intent = when (kind) {
      "image" -> imageIntent()
      // 目录只能一次选一棵（系统没有"多目录树"这回事），所以单独一支。
      "folder" -> treeIntent()
      else -> fileIntent()
    }
    if (kind != "folder") intent.putExtra(Intent.EXTRA_ALLOW_MULTIPLE, true)
    try {
      startActivityForResult(invoke, intent, "picked")
    } catch (e: Exception) {
      invoke.reject(e.message ?: activity.getString(R.string.pick_open_failed))
    }
  }

  /** 选择器回来了（`RESULT_OK` = 选了几项，取消 = `RESULT_CANCELED`）。 */
  @ActivityCallback
  fun picked(invoke: Invoke, result: ActivityResult) {
    if (result.resultCode != Activity.RESULT_OK) {
      // 取消：回一份空表。界面据此直接返回，不提示、不报错。
      val empty = JSObject()
      empty.put("files", JSArray())
      invoke.resolve(empty)
      return
    }
    // `@ActivityCallback` 在主线程上执行；这里可能是一整个 500 MB–1 GB 的视频/安装包
    // （或一整棵目录），`copyToCache` 的 `ins.copyTo` 不能占着 UI 线程（否则就是 ANR）。
    // URI 先在主线程取好：目录树那一支拿的是 `data.data` 本身（它才是 tree URI），
    // 普通文件那一支拿 `clipData` / `data`；两种解析都在后台线程里完成。
    val tree = result.data?.data?.takeIf { DocumentsContract.isTreeUri(it) }
    val data = result.data
    Thread {
      try {
        val files = JSArray()
        if (tree != null) {
          files.put(copyTreeToCache(tree))
        } else {
          for (uri in urisOf(data)) {
            files.put(copyToCache(uri))
          }
        }
        val out = JSObject()
        out.put("files", files)
        activity.runOnUiThread { invoke.resolve(out) }
      } catch (e: Exception) {
        activity.runOnUiThread { invoke.reject(e.message ?: e.toString()) }
      }
    }.start()
  }

  /**
   * 选图片的那一支：**先请系统的照片选择器**（Android 13 起，`ACTION_PICK_IMAGES`）——
   * 它只列媒体库里的图与视频、响应快，而且同样免权限；这台机器上没有它（老系统，
   * `resolveActivity` 返回 null）就退回 `ACTION_GET_CONTENT`，那一条在任何一个安卓 ROM 上
   * 都有人接手（通常就是"文件"应用，也能选到图片）。
   */
  private fun imageIntent(): Intent {
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.TIRAMISU) {
      val picker = Intent(MediaStore.ACTION_PICK_IMAGES)
      picker.type = "image/*"
      if (picker.resolveActivity(activity.packageManager) != null) return picker
    }
    return openable("image/*")
  }

  /** 选文件的那一支：任何类型都列出来（通配符见 `ANY_TYPE`；用户要发什么由他挑，不按扩展名收窄）。 */
  private fun fileIntent(): Intent = openable(ANY_TYPE)

  /**
   * 选目录的那一支：系统的目录选择器（SAF，`ACTION_OPEN_DOCUMENT_TREE`）。
   *
   * **为什么不是 `ACTION_GET_CONTENT`**：那条路给的是"一个文件"，目录根本不作为一个可选项
   * 出现；`ACTION_OPEN_DOCUMENT_TREE` 是系统给"把一整棵目录的读权限交出去"的唯一入口。
   * 同样不要任何存储权限 —— 返回的 tree URI 自带对整棵树的读授权（含之后新建的子项）。
   * 它**只支持一次一棵**（没有多选）：手机上一次选两个目录只能分两次发。
   */
  private fun treeIntent(): Intent = Intent(Intent.ACTION_OPEN_DOCUMENT_TREE)

  private fun openable(type: String): Intent {
    val intent = Intent(Intent.ACTION_GET_CONTENT)
    intent.addCategory(Intent.CATEGORY_OPENABLE)
    intent.type = type
    return intent
  }

  /** 结果里的 URI：多选在 `clipData` 里，单选在 `data` 里（两者都可能有，一个都没选就是空的）。 */
  private fun urisOf(data: Intent?): List<Uri> {
    if (data == null) return emptyList()
    val out = mutableListOf<Uri>()
    val clip = data.clipData
    if (clip != null) {
      for (i in 0 until clip.itemCount) {
        clip.getItemAt(i)?.uri?.let { out.add(it) }
      }
    } else {
      data.data?.let { out.add(it) }
    }
    return out
  }

  /**
   * 一棵目录树（`ACTION_OPEN_DOCUMENT_TREE` 交回的 tree URI）→ 缓存里的一整棵目录。
   *
   * **为什么必须整棵拷下来**：推送那条路只认本机真实路径（清单按路径列、下载端点按路径分块
   * 取字节，见 `transfer`），而 SAF 给的是 `content://` —— Rust 侧没有 ContentResolver，
   * WebView 里也读不出字节。所以和 [`copyToCache`] 是同一条取舍：先用系统授权把字节搬进
   * `cacheDir/pick/`，再把**路径**交给推送。目录因此是"先整棵拷、再按目录推"，不是边传边读。
   *
   * **保留目录结构**：每一项按 provider 给的 `DISPLAY_NAME` 落成同名子目录/文件，相对路径
   * 就是用户看到的那棵树（对端收到的清单也是它）。名字仍过 [`stagedName`]：provider 给的名字
   * 是**别的应用的数据**，不能让它拼出 `../`。
   *
   * **读不到的子项跳过并计数**（`skipped`，界面会说一声）：一棵几千项的相册里有一张已经被删、
   * 或某个云端占位符取不下来，不该让整次发送失败 —— 这与桌面端 `collect_dir_manifest` 对
   * 读不到的项"跳过 + 计数"是同一条取舍。只有两种情况整条失败：根目录读不出来（用户点的就是
   * 它），以及**空间不足**（继续"跳过"只会假装发成功，还留下半棵目录）。一个文件都没有的
   * 目录也当场拒绝：清单里没有东西可推，`push_paths` 只会回一句更含糊的话。
   *
   * 空文件**不拒绝**（与 [`copyToCache`] 的单文件那条不同）：一个目录里放几个 0 字节的占位
   * 文件是正常内容，跳过它们等于悄悄改了用户要发的东西。
   */
  private fun copyTreeToCache(tree: Uri): JSObject {
    val resolver = activity.contentResolver
    val staging = File(activity.cacheDir, "pick")
    if (!staging.exists() && !staging.mkdirs()) {
      throw IllegalStateException(activity.getString(R.string.pick_cache_dir_failed, staging.absolutePath))
    }
    val rootId = try {
      DocumentsContract.getTreeDocumentId(tree)
    } catch (e: Exception) {
      throw IllegalStateException(activity.getString(R.string.pick_folder_read_failed, tree.toString()), e)
    }
    val rootUri = DocumentsContract.buildDocumentUriUsingTree(tree, rootId)
    val root = File(staging, uniqueName(staging, stagedName(displayNameOf(rootUri), null, "文件夹")))
    if (!root.mkdirs()) {
      throw IllegalStateException(activity.getString(R.string.pick_cache_dir_failed, root.absolutePath))
    }

    var total = 0L
    var files = 0
    var skipped = 0
    val pending = ArrayDeque<Pair<String, File>>()
    // 有环 / 自引用的 provider（文档 id 指回祖先）：同一个 id 只展开一次，否则会无限深。
    val seen = HashSet<String>()
    pending.add(rootId to root)
    while (pending.isNotEmpty()) {
      val (docId, outDir) = pending.removeLast()
      if (!seen.add(docId)) {
        skipped++
        continue
      }
      val children = DocumentsContract.buildChildDocumentsUriUsingTree(tree, docId)
      val cursor = try {
        resolver.query(children, CHILD_COLUMNS, null, null, null)
      } catch (_: Exception) {
        null
      }
      if (cursor == null) {
        if (docId == rootId) {
          root.deleteRecursively()
          throw IllegalStateException(activity.getString(R.string.pick_folder_read_failed, tree.toString()))
        }
        skipped++
        continue
      }
      try {
        val idCol = cursor.getColumnIndex(DocumentsContract.Document.COLUMN_DOCUMENT_ID)
        if (idCol < 0) {
          if (docId == rootId) {
            root.deleteRecursively()
            throw IllegalStateException(activity.getString(R.string.pick_folder_read_failed, tree.toString()))
          }
          skipped++
          continue
        }
        val nameCol = cursor.getColumnIndex(DocumentsContract.Document.COLUMN_DISPLAY_NAME)
        val mimeCol = cursor.getColumnIndex(DocumentsContract.Document.COLUMN_MIME_TYPE)
        while (cursor.moveToNext()) {
          val childId = cursor.getString(idCol)
          if (childId == null) {
            skipped++
            continue
          }
          val rawName = if (nameCol >= 0 && !cursor.isNull(nameCol)) cursor.getString(nameCol) ?: "" else ""
          val mime = if (mimeCol >= 0 && !cursor.isNull(mimeCol)) cursor.getString(mimeCol) else null
          if (mime == DocumentsContract.Document.MIME_TYPE_DIR) {
            val sub = File(outDir, uniqueName(outDir, stagedName(rawName, null, "文件夹")))
            if (sub.mkdirs() || sub.isDirectory) {
              pending.add(childId to sub)
            } else {
              skipped++
            }
            continue
          }
          val out = File(outDir, uniqueName(outDir, stagedName(rawName, null, "文件")))
          try {
            val docUri = DocumentsContract.buildDocumentUriUsingTree(tree, childId)
            val input = resolver.openInputStream(docUri)
              ?: throw IllegalArgumentException(activity.getString(R.string.pick_read_failed, tree.toString()))
            input.use { ins ->
              out.outputStream().use { outs -> ins.copyTo(outs, COPY_BUFFER) }
            }
            total += out.length()
            files++
          } catch (e: Exception) {
            out.delete()
            if (isNoSpace(e)) {
              root.deleteRecursively()
              throw IllegalStateException(
                activity.getString(
                  R.string.pick_folder_copy_failed,
                  rawName.ifBlank { childId },
                  e.message ?: e.toString(),
                ),
                e,
              )
            }
            skipped++
          }
        }
      } finally {
        cursor.close()
      }
    }
    if (files == 0) {
      root.deleteRecursively()
      throw IllegalStateException(activity.getString(R.string.pick_folder_empty))
    }
    val obj = JSObject()
    obj.put("path", root.absolutePath)
    obj.put("name", root.name)
    obj.put("size", total)
    obj.put("mime", "")
    obj.put("isDir", true)
    obj.put("skipped", skipped)
    return obj
  }

  /** 一个 URI 的 `DISPLAY_NAME`（provider 不给就是空串）。目录根的名字就走这里。 */
  private fun displayNameOf(uri: Uri): String {
    val cols = arrayOf(DocumentsContract.Document.COLUMN_DISPLAY_NAME)
    return try {
      activity.contentResolver.query(uri, cols, null, null, null)?.use { c ->
        if (!c.moveToFirst()) {
          ""
        } else {
          val i = c.getColumnIndex(DocumentsContract.Document.COLUMN_DISPLAY_NAME)
          if (i >= 0 && !c.isNull(i)) c.getString(i) ?: "" else ""
        }
      } ?: ""
    } catch (_: Exception) {
      ""
    }
  }

  /**
   * 这次失败是不是"存储空间不够"。这种错误不能按"跳过这一项"处理：后面的每一项都会同样
   * 失败，最后只会剩半棵目录。消息通常是 `ENOSPC` / `No space left on device`。
   */
  private fun isNoSpace(e: Throwable): Boolean {
    var cur: Throwable? = e
    while (cur != null) {
      val msg = cur.message.orEmpty()
      if (msg.contains("ENOSPC", ignoreCase = true) || msg.contains("No space left", ignoreCase = true)) {
        return true
      }
      cur = cur.cause
    }
    return false
  }

  /**
   * 一个 URI → 缓存里的一份文件：拷字节、量大小、把真实名字带上。
   *
   * 名字取系统给的那一个（`OpenableColumns.DISPLAY_NAME`）—— 会话里那条气泡、对端落盘的那份
   * 都写它，用户一眼认得出"这是我刚发的那张图/那份表格"。`SIZE` 只当参考（有些 provider 不给
   * 或给 0），**交回界面的大小一律以拷好的那份文件为准**。
   */
  private fun copyToCache(uri: Uri): JSObject {
    val resolver = activity.contentResolver
    val mime = resolver.getType(uri)
    val (rawName, _) = metaOf(uri)
    val dir = File(activity.cacheDir, "pick")
    if (!dir.exists() && !dir.mkdirs()) {
      throw IllegalStateException(activity.getString(R.string.pick_cache_dir_failed, dir.absolutePath))
    }
    val name = uniqueName(dir, stagedName(rawName, mime))
    val out = File(dir, name)
    val input = resolver.openInputStream(uri)
      ?: throw IllegalArgumentException(activity.getString(R.string.pick_read_failed, uri.toString()))
    input.use { ins ->
      out.outputStream().use { outs -> ins.copyTo(outs) }
    }
    if (out.length() <= 0L) {
      out.delete()
      throw IllegalArgumentException(activity.getString(R.string.pick_empty, stagedName(rawName, mime)))
    }
    val obj = JSObject()
    obj.put("path", out.absolutePath)
    obj.put("name", name)
    obj.put("size", out.length())
    obj.put("mime", mime ?: "")
    return obj
  }

  /** `DISPLAY_NAME` 与 `SIZE`（provider 不给就是空名 / 0）。 */
  private fun metaOf(uri: Uri): Pair<String, Long> {
    var name = ""
    var size = 0L
    val cols = arrayOf(OpenableColumns.DISPLAY_NAME, OpenableColumns.SIZE)
    activity.contentResolver.query(uri, cols, null, null, null)?.use { c ->
      if (c.moveToFirst()) {
        val ni = c.getColumnIndex(OpenableColumns.DISPLAY_NAME)
        if (ni >= 0 && !c.isNull(ni)) name = c.getString(ni) ?: ""
        val si = c.getColumnIndex(OpenableColumns.SIZE)
        if (si >= 0 && !c.isNull(si)) size = c.getLong(si)
      }
    }
    return name to size
  }

  /**
   * 暂存文件叫什么。
   *
   * 净化：路径分隔符与控制字符换成下划线（名字是**别的应用给的**，不能让它拼出目录来 ——
   * 落点始终是上面那个 `pick/`）、去掉首尾的点与空白（`..` / `.hidden` 这类名字在别的工具里
   * 会变成别的东西）、空名兜一个带时间戳的。
   *
   * **扩展名那一档是图片专有的**：一条图片消息要能在两端都读得出来，落盘的扩展名必须落在
   * `paste::mime_of` 那份名单里（png/jpg/jpeg/gif/webp/bmp/avif）—— 不在名单里的（真机上的
   * `IMG_20261014.heic`、`screenshot.qoi`……）按 URI 声明的 MIME 换一个，MIME 也说不上来就按
   * png。这与 `PasteImagePlugin.extOf` 是同一条取舍：扩展名只是"让这条路先走通"，真解不出来
   * 的那一层会如实说读不出来。**文件那一档不动名字**：推送本来就是"用户要发什么就发什么"，
   * 对端落盘用的也是同一个名字。
   *
   * `fallback` 只决定空名兜底的前缀（文件是「文件-…」、目录是「文件夹-…」）：目录树里每一层
   * 都可能遇到 provider 不给名字的项，两档兜底混用会让用户分不清那是什么。
   */
  private fun stagedName(raw: String, mime: String?, fallback: String = "文件"): String {
    val clean = raw
      .replace('\u0000', ' ')
      .replace('/', '_')
      .replace('\\', '_')
      .trim()
      .trimStart('.')
      .trim()
    if (clean.isEmpty()) return "$fallback-${stamp()}-${UUID.randomUUID().toString().take(4)}"
    if (!mime.orEmpty().startsWith("image/")) return clean
    val dot = clean.lastIndexOf('.')
    val base = if (dot > 0) clean.substring(0, dot) else clean
    val current = if (dot > 0) clean.substring(dot + 1).lowercase(Locale.US) else ""
    return "$base.${imageExt(current, mime)}"
  }

  /** 认得的图片扩展名原样保留；认不出按 MIME 映射；都不行落 png（与 `PasteImagePlugin.extOf` 同一份名单）。 */
  private fun imageExt(current: String, mime: String?): String {
    if (IMAGE_EXTS.contains(current)) return current
    return when (mime?.lowercase(Locale.US)?.substringBefore(';')?.trim()) {
      "image/jpeg" -> "jpg"
      "image/gif" -> "gif"
      "image/webp" -> "webp"
      "image/bmp", "image/x-ms-bmp" -> "bmp"
      "image/avif" -> "avif"
      "image/png" -> "png"
      else -> extFromMime(mime?.lowercase(Locale.US)?.substringBefore(';')?.trim(), "png")
    }
  }

  /** 系统那份 MIME→扩展名表（认不出来用 `fallback`）。 */
  private fun extFromMime(mime: String?, fallback: String): String {
    if (mime == null) return fallback
    return MimeTypeMap.getSingleton().getExtensionFromMimeType(mime) ?: fallback
  }

  /**
   * 同一个名字已经躺在暂存目录里时，插一个序号（`清单.xlsx` → `清单-1.xlsx`）。
   *
   * 缓存目录是"这次发的东西"的落脚点，同名覆盖等于**把用户刚选的那份换成另一份**：
   * 界面手里那条路径会指向新字节，而他以为发的是第一份。
   */
  private fun uniqueName(dir: File, name: String): String {
    if (!File(dir, name).exists()) return name
    val dot = name.lastIndexOf('.')
    val base = if (dot > 0) name.substring(0, dot) else name
    val ext = if (dot > 0) name.substring(dot) else ""
    for (i in 1 until 1000) {
      val candidate = "$base-$i$ext"
      if (!File(dir, candidate).exists()) return candidate
    }
    return "$base-${UUID.randomUUID().toString().take(4)}$ext"
  }

  private fun stamp(): String =
    SimpleDateFormat("yyyyMMdd-HHmmss", Locale.US).format(Date())

  private companion object {
    /** 后端 `paste::mime_of` 认得的那几个扩展名（那边认不出来的一律拒），两处必须同一份。 */
    val IMAGE_EXTS = setOf("png", "jpg", "jpeg", "gif", "webp", "bmp", "avif")

    /** 任意类型（`Intent.type` 的那两个通配符；写成常量是因为它出现在注释里会提前结束块注释）。 */
    const val ANY_TYPE = "*/*"

    /**
     * 列一层目录时要的几列：文档 id（继续往下走 / 开流）、显示名（落盘名）、MIME（分辨目录）。
     * `SIZE` 不要 —— 有些 provider 报 0 或干脆不给，大小一律以拷好的那份文件为准。
     */
    val CHILD_COLUMNS = arrayOf(
      DocumentsContract.Document.COLUMN_DOCUMENT_ID,
      DocumentsContract.Document.COLUMN_DISPLAY_NAME,
      DocumentsContract.Document.COLUMN_MIME_TYPE,
    )

    /** 目录树拷贝的缓冲区：provider 的 `openInputStream` 一次读大块比逐 8 KB 少很多往返。 */
    const val COPY_BUFFER = 64 * 1024
  }
}