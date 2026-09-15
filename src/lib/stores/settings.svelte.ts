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

export async function updateSettings(s: Parameters<typeof api.updateSettings>[0]) {
  loading = true;
  try {
    settings = await api.updateSettings(s);
  } catch (e) {
    console.error('Failed to update settings:', e);
    throw e;
  } finally {
    loading = false;
  }
}