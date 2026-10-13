/**
 * 会话里那些图片的 `data:` URL 缓存。
 *
 * 一个会话里同一条图片消息会被渲染很多次（滚动、状态行每 200ms 一跳都会碰到那一条），而每次
 * 读它都是一次 IPC + 整份文件 base64 —— 缓存是必需的，不是优化。
 *
 * **为什么不做成响应式**（`$state` 的模块级缓存）：那样读取发生在渲染期，而缓存又有上限，
 * 淘汰之后下一次渲染又会去读 → 读回来又写 → 又渲染，转成一个停不下来的取数循环。
 * 这里的取数一律由组件自己的 `$effect` 驱动（`ImagePreview.svelte`），组件里那份 `$state`
 * 才是渲染的依据 —— 淘汰最多让下次挂载重读一次，不会自激。
 *
 * 上限按**条数**算（`MAX_CACHE`）：读得到的那几张是用户真看过的，条目少而大；不设上限就会
 * 攒住一整个会话的图。
 */

import { readImageDataUrl } from './api';

const MAX_CACHE = 8;

/** `路径 → data URL`。`Map` 的插入顺序就是淘汰顺序（读一次踢到队尾 = LRU）。 */
const cache = new Map<string, string>();

/** 一条图片消息的预览 URL（同一路径重复问只读一次盘）。 */
export async function imageDataUrl(path: string): Promise<string> {
  const hit = cache.get(path);
  if (hit !== undefined) {
    cache.delete(path);
    cache.set(path, hit);
    return hit;
  }
  const url = await readImageDataUrl(path);
  cache.set(path, url);
  while (cache.size > MAX_CACHE) {
    const oldest = cache.keys().next().value;
    if (oldest === undefined) break;
    cache.delete(oldest);
  }
  return url;
}

/** 那条文件记录没了（清空聊天记录）时把它从缓存里摘掉。 */
export function forgetImage(path: string): void {
  cache.delete(path);
}