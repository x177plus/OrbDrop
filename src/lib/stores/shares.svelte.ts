import type { ShareItem } from '$lib/types';
import * as api from '$lib/api';

// Svelte 5 rune-based store: reactive at module level.
let items = $state<ShareItem[]>([]);
let loading = $state(false);
let serverPort = $state(0);

export function getShares() { return items; }
export function isSharesLoading() { return loading; }
export function getServerPort() { return serverPort; }
export function setServerPort(p: number) { serverPort = p; }

export async function loadShares() {
  loading = true;
  try {
    items = await api.listShares();
  } catch (e) {
    console.error('Failed to load shares:', e);
  } finally {
    loading = false;
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