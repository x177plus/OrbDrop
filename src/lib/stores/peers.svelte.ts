import { listen } from '@tauri-apps/api/event';
import type { PeerDevice, PeerShareItem } from '$lib/types';
import * as api from '$lib/api';
import { getDevices } from './devices.svelte';

/**
 * 对端设备标签的数据源：某一台设备「广播给本机」的共享项列表。
 *
 * 列表内容完全由服务端的可见性规则决定（开播开关 + 本机此刻是否与它保持长连接），
 * 前端不做二次过滤——否则两边规则一旦漂移，界面会显示读不到的共享项。
 */
export interface PeerSharesState {
  items: PeerShareItem[];
  loaded: boolean;
  loading: boolean;
  error: string | null;
}

/**
 * 还没拉过的设备读这个只读占位：模板里只能读状态，
 * 建条目必须发生在事件处理里，否则等于渲染期写 $state（state_unsafe_mutation）。
 */
const EMPTY: PeerSharesState = Object.freeze({ items: [], loaded: false, loading: false, error: null });

let states = $state<Record<string, PeerSharesState>>({});

export function getPeerSharesState(peerId: string): PeerSharesState {
  return states[peerId] ?? EMPTY;
}

function patch(peerId: string, next: Partial<PeerSharesState>) {
  states = { ...states, [peerId]: { ...(states[peerId] ?? EMPTY), ...next } };
}

/** 对端 HTTP 入口：ip/port 都来自设备列表（mDNS/手动添加/配对请求三处合并的结果）。 */
export function peerBaseUrl(peer: PeerDevice): string {
  return `http://${peer.ip}:${peer.port}`;
}

/** 拉取（或刷新）某台设备对本机可见的共享项。 */
export async function loadPeerShares(peerId: string): Promise<void> {
  const peer = getDevices().find((d) => d.id === peerId);
  if (!peer) {
    patch(peerId, { items: [], loaded: true, loading: false, error: '设备不在列表中' });
    return;
  }
  patch(peerId, { loading: true, error: null });
  try {
    const token = await api.getPeerToken(peerId);
    if (!token) throw new Error('未配对，无法读取对方的共享项');
    const items = await api.fetchPeerShares(peerBaseUrl(peer), token);
    patch(peerId, { items, loaded: true, loading: false, error: null });
  } catch (e: unknown) {
    const msg = e instanceof Error ? e.message : '加载失败';
    patch(peerId, { items: [], loaded: true, loading: false, error: `读取失败：${msg}` });
  }
}

/** 设备已不在列表（离线/解除配对）时清掉缓存，避免下次上线显示过期内容。 */
export function forgetPeerShares(peerId: string) {
  if (!(peerId in states)) return;
  const next = { ...states };
  delete next[peerId];
  states = next;
}

async function reloadIfLoaded(peerId: string) {
  if (states[peerId]?.loaded) await loadPeerShares(peerId);
}

// 对端共享项清单变化（新增/移除/改名/开播）→ 重新拉；长连接状态变化会改变可见范围 → 同样重拉。
const initListeners: Promise<void> = (async () => {
  await listen<{ peerId: string }>('shares://changed', (ev) => {
    void reloadIfLoaded(ev.payload.peerId);
  });
  await listen('devices://changed', () => {
    for (const peerId of Object.keys(states)) void reloadIfLoaded(peerId);
  });
})();

export function cleanup() {
  void initListeners;
}