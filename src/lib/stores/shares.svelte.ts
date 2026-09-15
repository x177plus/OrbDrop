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

export async function addShare(path: string) {
  await api.addShare(path);
  await loadShares();
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