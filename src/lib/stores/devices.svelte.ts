import { listen } from '@tauri-apps/api/event';
import type { PeerDevice } from '$lib/types';
import * as api from '$lib/api';

let devices = $state<PeerDevice[]>([]);
let loading = $state(false);

export function getDevices() { return devices; }
export function isDevicesLoading() { return loading; }

export async function loadDevices() {
  loading = true;
  try {
    // 只把在线设备交给界面：对方下线（mDNS 移除）后立刻从列表消失；
    // 地址簿/凭据仍留在后端，对方再上线会自动重新出现。
    devices = (await api.listDevices()).filter((d) => d.online);
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