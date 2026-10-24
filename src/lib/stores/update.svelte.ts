import { listen } from '@tauri-apps/api/event';
import * as api from '$lib/api';
import type { UpdateState } from '$lib/types';

/**
 * 自更新的**唯一**状态源（桌面设置面板/通知气泡与移动设置页/底板共用）。
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
/**
 * 用户把这一格按下去了（「稍后」/关掉那张底板）：**本次运行内不再自动弹**。
 *
 * 下次启动是另一个进程，那份静默检查会重新弹一次 —— "每次启动都提示"就是这个意思
 * （状态本来就不落盘，`Ctl` 是内存里的）。
 */
let dismissed = $state(false);
/**
 * 「更新这件事」已经在动手了（下载/校验/落地已经露过头）。
 *
 * 有了它，"提示"才能在用户点下「立即更新」之后**就地变成进度**：不然点完那一下这一格就没了，
 * 界面上剩下的只有一个两分钟后才出现的系统安装器（手机），或一次突然的重启（桌面）。
 */
let engaged = $state(false);
let watching = false;

/**
 * 所有状态都从这一个口子进来（命令的返回、事件、初值三条路）。
 *
 * 除了写 `state`，它还要维护 [`engaged`] —— 那是"这一格该不该留着"的判据，只有一处能算。
 */
function setState(next: UpdateState): void {
  // 下载/校验/落地一露头就记下"动手了"；回到待决定/收工则清掉，下一件事重新弹。
  if (next.phase === 'downloading' || next.phase === 'verifying' || next.phase === 'applying') {
    engaged = true;
  } else if (next.phase === 'available' || next.phase === 'uptodate' || next.phase === 'idle') {
    engaged = false;
  }
  state = next;
}

export function getUpdateState(): UpdateState | null {
  return state;
}

export function isApplying(): boolean {
  return applying;
}

/**
 * 该不该**主动提示**：启动时的静默检查（后端 `check(force = false)`，起来 8 秒后跑）发现了
 * 新版，而且用户还没按掉它。
 *
 * `silent` 那道门是必须的：用户自己点「检查更新」得来的结论已经有设置面板在显示，
 * 再弹一张板/一个气泡就是把同一件事说两遍。
 */
export function updatePrompt(): UpdateState | null {
  const s = state;
  if (!s || dismissed) return null;
  return s.phase === 'available' && s.silent ? s : null;
}

/**
 * 「有更新待处理」的角标数字：手机底栏「设置」那颗用。
 *
 * 与 [`updatePrompt`] 不同，**不看 `silent`、也不看 `dismissed`**：按过「稍后」只是不再
 * 自动弹底板，更新本身还在那儿；角标是让用户随时找得到它的那条线索。
 */
export function updateBadge(): number {
  return state?.phase === 'available' ? 1 : 0;
}

/**
 * 「更新这件事」该不该在界面上占一格：**任何一次**检查发现的可用更新（启动静默检查或
 * 用户手动检查都算），外加已经点过「立即更新」之后的进度与结果（那时这一格从"提示"变成
 * "进度"，见 [`engaged`]）。
 *
 * 桌面端那条通知气泡按它决定显不显示；手机那边弹的是底板，弹出去之后就由 [`getUpdateState`]
 * 接着画进度（底板不会因为阶段变了就自己消失）。
 */
export function updateTask(): UpdateState | null {
  const s = state;
  if (!s || dismissed) return null;
  return engaged || s.phase === 'available' ? s : null;
}

/** 用户按掉了这次提示（「稍后」/关掉那张底板）。 */
export function dismissUpdatePrompt(): void {
  dismissed = true;
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
    if (!state) setState(s);
  } catch (e) {
    console.error('读取更新状态失败:', e);
  }
  await listen<UpdateState>('update://state', (e) => {
    setState(e.payload);
  });
}

/** 检查更新（失败时把后端的报错原样抛给调用方，界面自己决定怎么显示）。 */
export async function checkUpdate(force = true): Promise<void> {
  const next = await api.updateCheck(force);
  // 用户主动点的那次是明确想看结论：之前按过「稍后」也重新给一次右下角那格。
  if (force) dismissed = false;
  setState(next);
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