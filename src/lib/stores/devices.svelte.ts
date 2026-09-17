import { listen } from '@tauri-apps/api/event';
import type { PeerDevice } from '$lib/types';
import * as api from '$lib/api';

let devices = $state<PeerDevice[]>([]);
let loading = $state(false);

export function getDevices() { return devices; }
export function isDevicesLoading() { return loading; }

/**
 * 交给界面的设备行：在线设备全列（长连接 / 已配对 / 未绑定），**已配对但离线的也列**
 * —— 对方不在线时才是真正需要「解除配对」的场合，而解除配对是本机动作
 * （`unpair_peer` 只是"尽力通知对端"，对端离线照样清干净本机 token）。
 *
 * 离线且未绑定的不列：它没有任何可做的动作，只会堆积曾经扫到的设备。
 *
 * 排序（用户要求）：**在线在前、离线在后**；在线组内 长连接 → 已配对 → 未绑定
 * （离线组内同理，只是没有长连接可言）。同级按名字排，免得每次刷新行序乱跳。
 */
export function visibleDevices(all: PeerDevice[]): PeerDevice[] {
  const group = (d: PeerDevice) => (d.online ? 0 : 1);
  const rank = (d: PeerDevice) => (d.connected ? 0 : d.paired ? 1 : 2);
  return all
    .filter((d) => d.online || d.paired)
    .sort((a, b) => group(a) - group(b) || rank(a) - rank(b) || a.name.localeCompare(b.name));
}

export async function loadDevices() {
  loading = true;
  try {
    devices = visibleDevices(await api.listDevices());
  } catch (e) {
    console.error('Failed to load devices:', e);
  } finally {
    loading = false;
  }
}

export function addDevice(d: PeerDevice) {
  devices = [...devices, d];
}

// Watch for live peer state changes from the Rust backend.
const initListeners: () => Promise<void> = (() => {
  // Return a cleanup function (called once on module init).
  const unlisteners: Promise<() => void>[] = [];

  unlisteners.push(
    listen<{ peerId: string }>('devices://changed', () => {
      loadDevices().catch(console.error);
    })
  );
  unlisteners.push(
    listen<{ peerId: string; name: string }>('peer://online', () => {
      loadDevices().catch(console.error);
    })
  );
  unlisteners.push(
    listen<{ peerId: string }>('peer://offline', () => {
      loadDevices().catch(console.error);
    })
  );

  return async () => {
    for (const u of unlisteners) {
      (await u)();
    }
  };
})();

export function cleanup() {
  void initListeners();
}

// Export event channel for use by other stores / components.
export const eventChannels = {
  listen: listen,
};