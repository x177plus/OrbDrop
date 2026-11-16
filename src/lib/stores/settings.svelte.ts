import type { AppSettings } from '$lib/types';
import * as api from '$lib/api';

let settings = $state<AppSettings | null>(null);
let loading = $state(false);

export function getSettings() { return settings; }
export function isSettingsLoading() { return loading; }

export async function loadSettings() {
  loading = true;
  try {
    settings = await api.getSettings();
  } catch (e) {
    console.error('Failed to load settings:', e);
  } finally {
    loading = false;
  }
}

export async function updateSettings(
  s: Parameters<typeof api.updateSettings>[0],
): Promise<AppSettings> {
  loading = true;
  try {
    // 后端的回包是**落盘后的真值**（取值夹住、语言规范化）：交给调用方写回表单，
    // 不然自动保存后面板还显示用户手打的那个数。
    const next = await api.updateSettings(s);
    settings = next;
    return next;
  } catch (e) {
    console.error('Failed to update settings:', e);
    throw e;
  } finally {
    loading = false;
  }
}