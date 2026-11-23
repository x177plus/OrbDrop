import type { ShareItem } from '$lib/types';
import * as api from '$lib/api';

// Svelte 5 rune-based store: reactive at module level.
let items = $state<ShareItem[]>([]);
let loading = $state(false);
let error = $state<string | null>(null);
let serverPort = $state(0);
/** 请求序号：多选添加会并发触发多次 `loadShares`，旧响应不能覆盖新列表。 */
let loadSeq = 0;

export function getShares() { return items; }
export function isSharesLoading() { return loading; }
/** 最近一次 `list_shares` 的失败原因；成功一次就清空。桌面端只是多一种可读的状态。 */
export function getSharesError() { return error; }
export function getServerPort() { return serverPort; }
export function setServerPort(p: number) { serverPort = p; }

export async function loadShares() {
  const seq = ++loadSeq;
  loading = true;
  error = null;
  try {
    const next = await api.listShares();
    if (seq === loadSeq) items = next;
  } catch (e) {
    if (seq === loadSeq) {
      error = e instanceof Error ? e.message : String(e);
      console.error('Failed to load shares:', e);
    }
  } finally {
    if (seq === loadSeq) loading = false;
  }
}

/**
 * 登记一条共享项。返回后端那一份 —— **重复登记同一条路径时返回的是已经存在的那一项**
 * （`add_share` 按路径去重，见其注释）：调用方靠"回来的 id 是不是本来就有的"区分
 * 「已添加」与「已在共享项里」。
 */
export async function addShare(path: string): Promise<ShareItem> {
  const item = await api.addShare(path);
  await loadShares();
  return item;
}

export async function removeShare(id: string) {
  await api.removeShare(id);
  await loadShares();
}

export async function renameShare(id: string, name: string) {
  await api.renameShare(id, name);
  await loadShares();
}

export async function toggleBroadcast(id: string, enabled: boolean) {
  await api.toggleBroadcast(id, enabled);
  await loadShares();
}