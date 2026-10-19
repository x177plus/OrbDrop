import type { BcSlot, BcStatus, BcTarget } from '$lib/types';
import * as api from '$lib/api';

// Svelte 5 rune-based store: reactive at module level.
// 左侧槽只有一处存储（BC 自己的 `Saved*Left` 注册表值），资源管理器那套右键写的就是它 ——
// 所以这里一律整体替换后端给的状态，不在前端推导，也不缓存（槽可能在资源管理器里刚被改过）。
let status = $state<BcStatus>({ exePath: null, slotWritable: false, left: null });

export function getBcStatus() { return status; }

/**
 * 现读一次槽。`slotFor` = 右键菜单里那一项是文件还是文件夹：BC 按类型取槽，我们也按类型取，
 * 好让菜单里那行与真正要比的那一档一致（不传 = 启动/设置面板那一档，文件夹槽优先）。
 */
export async function loadBcStatus(slotFor?: 'file' | 'folder') {
  try {
    status = await api.bcStatus(slotFor);
  } catch (e) {
    console.error('Failed to load BeyondCompare status:', e);
  }
}

/** 放入左侧槽（覆盖原有项）；返回槽里现在的这一项（label 用后端算好的），reject 时抛出。 */
export async function setLeft(target: BcTarget): Promise<BcSlot | null> {
  status = await api.bcSetLeft(target);
  return status.left;
}

/** 与左侧比对：左边是槽里的原文，右边是这一项。 */
export async function compareWithLeft(target: BcTarget) {
  await api.bcCompareWithLeft(target);
}