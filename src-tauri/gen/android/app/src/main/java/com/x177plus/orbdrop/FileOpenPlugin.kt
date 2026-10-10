package com.x177plus.orbdrop

import android.app.Activity
import android.content.Intent
import android.webkit.MimeTypeMap
import androidx.core.content.FileProvider
import app.tauri.annotation.Command
import app.tauri.annotation.InvokeArg
import app.tauri.annotation.TauriPlugin
import app.tauri.plugin.Invoke
import app.tauri.plugin.Plugin
import java.io.File

/**
 * 「打开一个文件」在安卓上的落地（后端 `open_path` 那条命令，见 `docs/p1-implementation.md` §20）。
 *
 * **为什么不用 `tauri-plugin-opener` 的 `open_path`**（它还在依赖里，桌面端那半继续用它）：
 *
 * 1. 它移动端那一条把路径当**裸字符串**发给 Kotlin（`run_mobile_plugin("open", path.into())`），
 *    而 `OpenerPlugin` 的 `OpenArgs` 要的是 `{url, with}` 这个对象 —— Jackson 当场反序列化失败
 *    （实测：`MismatchedInputException: Cannot construct instance of app.tauri.opener.OpenArgs
 *    … from String value`）。也就是安卓上"打开"任何文件都只会得到一句报错；
 * 2. 就算参数形状对上，它交给 `ACTION_VIEW` 的是**没有 scheme 的路径**、也不带 MIME ——
 *    系统解析不出任何 activity。而私有文件本来就递不出去：安卓 7 起把 `file://` 交给别的应用
 *    直接抛 `FileUriExposedException`（本应用所有收件都在私有数据目录里）。
 *
 * 所以这里自己走一遍：`FileProvider` 的 `content://`（根见 `res/xml/file_paths.xml`）+ 按扩展名
 * 给的 MIME + `FLAG_GRANT_READ_URI_PERMISSION`（不授这个读权限，接收方读不了，安装器会当场失败）。
 *
 * **`.apk` 那一档就是"点一下安装"**：`application/vnd.android.package-archive` 正是系统安装器
 * （PackageInstaller）认领的那个 MIME，`ACTION_VIEW` 一到就进安装界面 —— 这是侧载的标准姿势，
 * 不需要（也不该）自己去开 `PackageInstaller` 会话。清单里声明 `REQUEST_INSTALL_PACKAGES` 是为了
 * 让「安装未知应用」那张特殊权限列表里**能出现 OrbDrop**（只声明了这个权限的应用才在列表里），
 * 首次点安装时系统会自己引用户去打开它一次。
 *
 * 打不开时抛出的那句话会原样端到界面上（`打开失败：…`，见 `store.svelte.ts::failed`），
 * 所以下面每一句都写成给人看的。
 */
@InvokeArg
class OpenFileArgs {
  lateinit var path: String
}

@TauriPlugin
class FileOpenPlugin(private val activity: Activity) : Plugin(activity) {

  @Command
  fun openFile(invoke: Invoke) {
    val args = invoke.parseArgs(OpenFileArgs::class.java)
    try {
      activity.startActivity(intentFor(args.path))
      invoke.resolve()
    } catch (e: Exception) {
      invoke.reject(e.message ?: e.toString())
    }
  }

  private fun intentFor(path: String): Intent {
    val file = File(path)
    if (!file.exists()) throw IllegalArgumentException("文件不在了：$path")
    // 文件夹这一档没有出路：收件落在应用私有目录里，`content://` 也换不来一个"能浏览它"的应用
    // （FileProvider 给不出目录枚举）。与其丢一个系统解析不出的 Intent 让用户看到
    // "没有应用可执行此操作"，不如把话说明白。
    if (file.isDirectory) {
      throw IllegalArgumentException("这是文件夹：安卓上没有能打开应用私有目录的应用")
    }
    val uri = FileProvider.getUriForFile(activity, "${activity.packageName}.fileprovider", file)
    return Intent(Intent.ACTION_VIEW).apply {
      setDataAndType(uri, mimeOf(file) ?: "*/*")
      addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
    }
  }

  /**
   * 按扩展名问系统要 MIME；问不到就给通配类型（下面代码里那个"星号斜杠星号"，块注释里写不出
   * 这个字面量 —— 它会把注释提前闭合），也就是交给用户自己选"用什么打开"，与安卓惯例一致。
   *
   * `apk` 先钉死：这一条是"点一下安装"的全部依据，不能依赖 ROM 自带那张表里有没有它。
   */
  private fun mimeOf(file: File): String? {
    val ext = file.extension.lowercase()
    if (ext == "apk") return "application/vnd.android.package-archive"
    return MimeTypeMap.getSingleton().getMimeTypeFromExtension(ext)
  }
}