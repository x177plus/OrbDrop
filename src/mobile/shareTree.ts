import * as api from "$lib/api";
import type { ShareItem, TreeEntry } from "$lib/types";

/**
 * 本机共享项的目录树。
 *
 * 走 Tauri IPC（`local_tree`），**不走页面里的 `fetch`**：安卓 release WebView 的
 * `usesCleartextTraffic=false` 会拦下 `http://127.0.0.1`，前端只拿得到
 * `TypeError: Failed to fetch`。命令内部与 `/tree` 共用同一份列举实现（见后端
 * `server::routes_tree::list_entries`）。桌面端自己的 `LocalTab` 仍走本机 HTTP，
 * 两条路的数据格式一致。
 */
export function fetchLocalTree(shareId: string, path: string): Promise<TreeEntry[]> {
  return api.localTree(shareId, path);
}

/** 共享项内相对路径 → 手机磁盘上的绝对路径（与桌面端 `openLocalEntry` 同一口径）。 */
export function shareAbsPath(share: ShareItem, rel: string): string {
  const root = share.path.replace(/[\\/]+$/, "");
  const clean = rel.replace(/\\/g, "/");
  return clean ? `${root}/${clean}` : share.path;
}

/** 用系统应用打开共享项里的一个文件（`.apk` 落到系统安装器）。 */
export async function openLocalShareEntry(share: ShareItem, rel: string): Promise<void> {
  await api.openPath(shareAbsPath(share, rel));
}