<script lang="ts">
  import { imageDataUrl } from '$lib/images';
  import type { Snippet } from 'svelte';

  /**
   * 会话里一张图片的缩略图（桌面端与移动端共用）。
   *
   * 读不出来（文件太大 / 已经不在盘上 / 不是图片）就渲染调用方给的 `fallback` —— 那条气泡
   * 退回原来的"文件图标 + 名字"样式，用户至少还点得开「用系统程序打开」。所以**加载中也是
   * 画 fallback**：闪一下文件行总比留一块空白好。
   *
   * 取数由这里的 `$effect` 驱动（不是渲染期读缓存）：缓存有上限，淘汰之后若还在渲染期读，
   * 就会"读 → 淘汰 → 再读"转成停不下来的循环，见 `lib/images.ts` 的文件头。
   */
  let {
    path,
    label = '图片',
    onopen,
    children,
  }: {
    /** 本机上那份图片的绝对路径。 */
    path: string;
    /** 无障碍名称 / 悬停提示（一般是这条文件消息的名字）。 */
    label?: string;
    /** 点缩略图（看大图）。没给就只是个不可点的预览。 */
    onopen?: () => void;
    /** 读不出来时画它（调用方给的是那条气泡原来的"文件图标 + 名字"样式）。 */
    children?: Snippet;
  } = $props();

  let url = $state('');

  $effect(() => {
    const target = path;
    let live = true;
    url = '';
    if (!target) return;
    void imageDataUrl(target)
      .then((next) => {
        if (live) url = next;
      })
      .catch(() => {
        // 读不出来就走 fallback：那句话（"太大，不在会话里预览"之类）在气泡上没有地方写，
        // 点了「用系统程序打开」自然就看到了。
      });
    return () => {
      live = false;
    };
  });
</script>

{#if url}
  {#if onopen}
    <button class="ip-thumb" type="button" title="点一下看大图" aria-label="看大图：{label}" onclick={onopen}>
      <img src={url} alt={label} draggable="false" />
    </button>
  {:else}
    <span class="ip-thumb static"><img src={url} alt={label} draggable="false" /></span>
  {/if}
{:else}
  {@render children?.()}
{/if}

<style>
  /* 只用基础色板里的变量（不写 `color-mix()` / `oklch()`）：这份样式会进产物那份 CSS，
     而移动端产物要过 Chrome 90 的兼容门禁，见 `scripts/check-compat.mjs`。 */
  .ip-thumb {
    display: block;
    padding: 0;
    border: 1px solid var(--border);
    border-radius: 10px;
    background: var(--fg-soft);
    overflow: hidden;
    cursor: zoom-in;
    line-height: 0;
    max-width: 168px;
  }
  .ip-thumb.static { cursor: default; }
  .ip-thumb img {
    display: block;
    width: auto;
    height: auto;
    max-width: 168px;
    max-height: 168px;
    object-fit: contain;
  }
</style>