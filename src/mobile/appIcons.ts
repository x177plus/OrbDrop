import * as api from "$lib/api";

/**
 * 「发送应用」那张表里**每一行的图标**（F-119）。
 *
 * 取值走 `api.appIcon`（原生画一张 144px 的 PNG，进程内也缓存一份，见
 * `InstalledAppsPlugin.kt`），这里再缓存**一层**：搜索框每敲一个字、每来回切一次会话，整张表的
 * 行都会被重建（`{#each}`），没有这一层就是同一个包名反复走一趟 IPC。这与会话图片那条
 * （`lib/images.ts`）是同一个理由：**缓存是必需的，不是优化**。
 *
 * 不需要淘汰：缓存的规模上限就是"这台设备上的应用数"（几十到几百张几 KB 的小图），
 * 比设一个上限再淘汰更简单，也不会把用户刚看过的那几张又洗掉。
 */
const cache = new Map<string, string>();

/** 已经在飞的那几张：同一行连着挂两次（换搜索词再换回来）只飞一趟。 */
const inflight = new Map<string, Promise<string>>();

/**
 * 同时在飞的请求条数上限。
 *
 * 一张表有一百多行，一挂上就是一百多个请求；原生的画图在主线程上，一次全放开就是让那一屏
 * 卡一下。四条一池：按行出现的顺序取（上面的行先拿到），既不会排队排到最后一个才出现，
 * 也不会把主线程喂满。
 */
const MAX_INFLIGHT = 4;
let running = 0;
const waiting: Array<() => void> = [];

async function acquire(): Promise<void> {
  if (running < MAX_INFLIGHT) {
    running += 1;
    return;
  }
  await new Promise<void>((resolve) => waiting.push(resolve));
  running += 1;
}

function release(): void {
  running -= 1;
  waiting.shift()?.();
}

/** 已经取到的图标（没有就返回 `undefined`）—— 同步问一句，用来在挂载那一刻直接画出来。 */
export function peekAppIcon(packageName: string): string | undefined {
  return cache.get(packageName);
}

/**
 * 取一个应用的图标；取不到（正在卸载、图标坏了、原生报错）会 reject —— 调用方自己决定
 * 退成什么（那一行是首字母）。
 */
export async function loadAppIcon(packageName: string): Promise<string> {
  const done = cache.get(packageName);
  if (done) return done;
  const flying = inflight.get(packageName);
  if (flying) return flying;

  const task = (async () => {
    await acquire();
    try {
      const url = await api.appIcon(packageName);
      cache.set(packageName, url);
      return url;
    } finally {
      release();
      inflight.delete(packageName);
    }
  })();
  inflight.set(packageName, task);
  return task;
}