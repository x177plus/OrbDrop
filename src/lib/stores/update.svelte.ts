import { listen } from '@tauri-apps/api/event';
import * as api from '$lib/api';
import type { UpdateState } from '$lib/types';

/**
 * 自更新的**唯一**状态源（桌面设置面板与移动设置页共用）。
 *
 * 状态本身归后端（`src-tauri/src/update/`），这里只做两件事：拿一次初值、然后跟着
 * `update://state` 事件走。所以两端都不需要自己攒状态，"桌面看到的进度"和"手机看到的进度"
 * 天然是同一份。
 *
 * 用模块级 `$state` + 取值函数（而不是导出变量）：Svelte 5 里跨模块导出被重新赋值的变量不会
 * 触发更新，取值函数每次读的都是当前信号值 —— 与 `stores/settings.svelte.ts` 同一个写法。
 */
let state = $state<UpdateState | null>(null);
/** "安装"这条命令还没返回（桌面端点下去进程就要重启了，按钮得先按住）。 */
let applying = $state(false);
let watching = false;

export function getUpdateState(): UpdateState | null {
  return state;
}

export function isApplying(): boolean {
  return applying;
}

/**
 * 订阅状态（两端挂载时都叫一次，重复调用没有副作用）。
 *
 * 初值取命令、之后靠事件：命令是"此刻"，事件是"之后每次变化"。两者顺序不保证（事件可能
 * 比命令的返回先到），所以**只在还没有值时**用命令的结果填初值，别把新事件盖回旧的。
 */
export async function watchUpdate(): Promise<void> {
  if (watching) return;
  watching = true;
  try {
    const s = await api.updateState();
    if (!state) state = s;
  } catch (e) {
    console.error('读取更新状态失败:', e);
  }
  await listen<UpdateState>('update://state', (e) => {
    state = e.payload;
  });
}

/** 检查更新（失败时把后端的报错原样抛给调用方，界面自己决定怎么显示）。 */
export async function checkUpdate(force = true): Promise<void> {
  state = await api.updateCheck(force);
}

/** 下载并安装。桌面端这条命令返回后进程很快会重启（helper 覆盖 exe）。 */
export async function installUpdate(): Promise<void> {
  applying = true;
  try {
    await api.updateApply();
  } finally {
    applying = false;
  }
}