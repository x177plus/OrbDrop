import type { BcSide, BcStatus, BcTarget } from '$lib/types';
import * as api from '$lib/api';

// Svelte 5 rune-based store: reactive at module level.
// 后端每个比较位命令都返回最新状态，这里一律整体替换，不在前端推导状态。
let status = $state<BcStatus>({ exePath: null, left: null, right: null });

export function getBcStatus() { return status; }

export async function loadBcStatus() {
  try {
    status = await api.bcStatus();
  } catch (e) {
    console.error('Failed to load BeyondCompare status:', e);
  }
}

/** 放入比较位；返回该侧（label 用后端算好的那个），后端 reject 时抛出。 */
export async function setBcSide(side: 'left' | 'right', target: BcTarget): Promise<BcSide | null> {
  status = await api.bcSetSide(side, target);
  return side === 'left' ? status.left : status.right;
}

export async function clearSide(side: 'left' | 'right') {
  status = await api.bcClearSide(side);
}

export async function compare(a: BcTarget, b: BcTarget) {
  await api.bcCompare(a, b);
}