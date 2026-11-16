package com.x177plus.orbdrop

import android.app.Activity
import android.os.Build
import android.os.Environment
import android.os.storage.StorageManager
import java.io.File

/**
 * 「所有文件访问」与共享存储卷的唯一判据。
 *
 * `ShareStoragePlugin`（选择器列根目录 / 列目录）与 `FileOpenPlugin`（打开本机共享项里的
 * 文件）共用：前者用 [roots] 给界面，后者用 [contains] 判断一个真实路径是否在用户能读的
 * 共享存储里。判据只有这一份，避免"选择器能列出来、打开却说不许"。
 */
internal object SharedStorage {
  /** Android 11+ 且用户授予了「所有文件访问」。 */
  fun granted(): Boolean =
    Build.VERSION.SDK_INT >= Build.VERSION_CODES.R && Environment.isExternalStorageManager()

  /**
   * 当前挂载的共享存储卷。只收 `MEDIA_MOUNTED` / `MEDIA_MOUNTED_READ_ONLY`，并去重。
   *
   * 主存储排第一（界面默认落在那里）。`StorageVolume.getDirectory()` 是 API 30，调用方已经
   * 用 [granted] 把入口卡在 Android 11+，因此这里不需要再处理更老的分支。
   */
  @Suppress("NewApi")
  fun roots(activity: Activity): List<Pair<String, File>> {
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

  /** `path` 是不是 `root` 本身，或落在它下面（两边都先 canonical 过）。 */
  fun isUnder(path: File, root: File): Boolean {
    val p = runCatching { path.canonicalPath }.getOrElse { return false }
    val r = runCatching { root.canonicalPath }.getOrElse { return false }
    return p == r || p.startsWith(r + File.separator)
  }

  /** `file` 是否落在当前任一共享存储卷里（先要求已授权，调用方负责）。 */
  fun contains(activity: Activity, file: File): Boolean =
    roots(activity).any { isUnder(file, it.second) }
}