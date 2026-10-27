package com.x177plus.orbdrop

import android.app.Activity
import android.net.Uri
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.JSObject
import app.tauri.plugin.Plugin
import java.io.File
import java.util.UUID

/**
 * 「输入框里粘进来的那张图」在安卓上的落地（后端 `resolve_image_uri` 那条命令）。
 *
 * **为什么非要有它**：手机上把一张图从输入法（Gboard 那类）的剪贴板粘进输入框，WebView 往
 * `contenteditable` 里插的是一个 `<img src="content://…">` —— 那串 URI 属于 ContentResolver，
 * **网页里的 JS 读不出来**（不是 http、不是 data、不是 blob）。而会话里"发图片"走的是推送，
 * 推送只认本机**真实路径**（清单要按路径列、对端要按路径分块取字节，见 `transfer`）。
 * 所以这一跳必须回原生：把 URI 后面的字节拷成一份暂存文件，把**路径**交回界面 ——
 * 从那一步起，安卓与桌面端粘贴进来的东西是同一种东西（都只是一个本机文件）。
 *
 * 落点：`cacheDir/paste/`（应用私有缓存，系统在空间紧张时可以自己回收）。
 * **`cache-path` 早就在 FileProvider 的根里**（`res/xml/file_paths.xml`），所以这份暂存文件
 * 后面"点一下用系统程序打开"照样走得通。
 *
 * 每一句会端到界面上的话（`invoke.reject` 的字符串）都按"给人看"写：它会在移动端变成一条 toast。
 */
@InvokeArg
class ResolveImageArgs {
  lateinit var uri: String
}

@TauriPlugin
class PasteImagePlugin(private val activity: Activity) : Plugin(activity) {

  @Command
  fun resolveImage(invoke: Invoke) {
    val args = invoke.parseArgs(ResolveImageArgs::class.java)
    try {
      // 回的是一个对象（`{path, size}`）而不是裸字符串：`Plugin.resolve` 收的就是 `JSObject?`，
      // 塞一个 String 进去 Kotlin 直接编译不过（这一条是 android 构建当场抓出来的）。
      // `size` 一并带回去：界面那条文件气泡要显示"多大"，而这一层是唯一知道的地方。
      val file = copyToCache(args.uri.trim())
      val out = JSObject()
      out.put("path", file.absolutePath)
      out.put("size", file.length())
      invoke.resolve(out)
    } catch (e: Exception) {
      invoke.reject(e.message ?: e.toString())
    }
  }

  /** 把那串 URI 指的内容拷成 `cacheDir/paste/` 下的一份文件。 */
  private fun copyToCache(uri: String): File {
    val parsed = Uri.parse(uri)
    val resolver = activity.contentResolver
    val dir = File(activity.cacheDir, "paste")
    if (!dir.exists() && !dir.mkdirs()) {
      throw IllegalStateException(activity.getString(R.string.paste_cache_dir_failed, dir.absolutePath))
    }
    // 名字带时间戳 + 一串随机数：同一秒里粘两张也不会互相盖掉；扩展名按 URI 声明的 MIME 取
    // （取不到就按 png —— 后端 `paste::read_data_url` 只认那几个图片扩展名，不认识的会被它挡掉）。
    val stamp = java.text.SimpleDateFormat("yyyyMMdd-HHmmss", java.util.Locale.US).format(java.util.Date())
    val name = "图片-$stamp-${UUID.randomUUID().toString().take(4)}.${extOf(resolver.getType(parsed))}"
    val out = File(dir, name)
    val input = resolver.openInputStream(parsed)
      ?: throw IllegalArgumentException(activity.getString(R.string.paste_read_failed, uri))
    input.use { ins ->
      out.outputStream().use { outs -> ins.copyTo(outs) }
    }
    if (out.length() <= 0L) {
      out.delete()
      throw IllegalArgumentException(activity.getString(R.string.paste_empty))
    }
    return out
  }

  /**
   * MIME → 扩展名。这一份名单**必须与后端 `paste::mime_of` 对齐**（那边认不出来的扩展名
   * 一律拒），所以这里也只映射同样那几个；其余（含 `image/tiff` 这类我们不解码的）一律落成 png。
   *
   * 为什么退回 png 而不报错：`<img>` 自己会去解码，WebView 认得出来的格式就画得出来 ——
   * 落一个错的扩展名，至少让"发出去"这件事先成，缩略图那一层再如实说读不出来。
   */
  private fun extOf(mime: String?): String {
    return when (mime?.lowercase()?.substringBefore(';')?.trim()) {
      "image/jpeg" -> "jpg"
      "image/gif" -> "gif"
      "image/webp" -> "webp"
      "image/bmp", "image/x-ms-bmp" -> "bmp"
      "image/avif" -> "avif"
      else -> "png"
    }
  }
}