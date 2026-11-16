package com.x177plus.orbdrop

import android.app.Activity
import java.io.File

/**
 * 应用文件路径的两道闸。
 *
 * - [requireOwned]：只认 `dataDir/OrbDrop` 与 `cacheDir`。`ExportFilePlugin`（把内容复制进
 *   系统相册 / 下载）必须走这一档 —— 一旦放开成任意绝对路径，界面传来的一个字符串就能把
 *   凭据库之类的私有文件复制到别的应用看得见的地方。
 * - [requireOpenable]：「打开」用。在本机私有文件之外，额外允许共享存储卷里的真实文件
 *   （本机共享项的 `/storage/...`）—— 共享方已经授了「所有文件访问」，用户点的也是自己
 *   登记的那一项。只走 [requireOwned] 的话，表现就是"共享的 APK 点不开"。
 *
 * 两档都用 canonical path 判，符号链接跳出去也会被拒。
 */
internal object AppFiles {
  /**
   * 校验并返回 canonical 之后的文件（**不检查存在性**：调用方各自有更贴切的缺失话术）。
   *
   * @throws IllegalArgumentException 路径落在应用私有目录之外（话术来自 `notOwnedRes`）。
   */
  fun requireOwned(activity: Activity, path: String, notOwnedRes: Int): File {
    val file = File(path).canonicalFile
    if (!isOwned(activity, file)) {
      throw IllegalArgumentException(activity.getString(notOwnedRes, path))
    }
    return file
  }

  /**
   * 「打开一个文件」的入口闸：应用私有文件照旧放行；本机共享项里的文件落在共享存储卷下，
   * 在用户已授予「所有文件访问」时也放行。
   *
   * 不放开「保存到手机」：那条命令会把内容复制到系统相册 / 下载，输入仍只认应用私有文件。
   */
  fun requireOpenable(activity: Activity, path: String): File {
    val file = File(path).canonicalFile
    if (isOwned(activity, file)) return file
    if (!SharedStorage.granted()) {
      throw IllegalArgumentException(activity.getString(R.string.share_storage_not_granted))
    }
    if (!SharedStorage.contains(activity, file)) {
      throw IllegalArgumentException(activity.getString(R.string.share_storage_out_of_root))
    }
    return file
  }

  /** 应用私有数据目录 / 缓存目录之下（两边都先 canonical 过）。 */
  private fun isOwned(activity: Activity, file: File): Boolean {
    val roots = listOf(
      File(activity.applicationInfo.dataDir, "OrbDrop").canonicalFile,
      activity.cacheDir.canonicalFile,
    )
    return roots.any { root ->
      file == root || file.path.startsWith(root.path + File.separator)
    }
  }
}