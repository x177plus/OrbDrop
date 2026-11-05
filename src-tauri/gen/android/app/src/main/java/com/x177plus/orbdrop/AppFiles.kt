package com.x177plus.orbdrop

import android.app.Activity
import java.io.File

/**
 * 「这只能是本应用自己的文件」这道闸的**唯一实现** —— `FileOpenPlugin`（交给别的应用打开）
 * 与 `ExportFilePlugin`（复制进系统共享存储）共用同一份判据。
 *
 * 为什么必须收口：`file_paths.xml` 的 `root-path` 是 `/`（安卓的预定义根覆盖不到
 * `app_data_dir/OrbDrop`），而在新的那条路里**文件内容会被写进系统相册 / 下载** —— 一旦
 * 放开成任意绝对路径，界面传来的一个字符串就能把凭据库之类的私有文件复制到别的应用看得见
 * 的地方。正常收件、聊天图片、粘贴/选择暂存都在 `dataDir/OrbDrop` 或 `cacheDir` 下；别的
 * 绝对路径一律拒绝。用 canonical path 判，符号链接跳出去也会被拒。
 *
 * `notOwnedRes` 是那条拒绝话术的字符串资源：两条命令对自己的描述不一样（「打开」/「保存」），
 * 但判据只有这一份。
 */
internal object AppFiles {
  /**
   * 校验并返回 canonical 之后的文件（**不检查存在性**：调用方各自有更贴切的缺失话术）。
   *
   * @throws IllegalArgumentException 路径落在应用私有目录之外（话术来自 [notOwnedRes]）。
   */
  fun requireOwned(activity: Activity, path: String, notOwnedRes: Int): File {
    val file = File(path).canonicalFile
    val roots = listOf(
      File(activity.applicationInfo.dataDir, "OrbDrop").canonicalFile,
      activity.cacheDir.canonicalFile,
    )
    val allowed = roots.any { root ->
      file == root || file.path.startsWith(root.path + File.separator)
    }
    if (!allowed) throw IllegalArgumentException(activity.getString(notOwnedRes, path))
    return file
  }
}