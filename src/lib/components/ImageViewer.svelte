<script lang="ts">
  import { imageDataUrl } from '$lib/images';

  /**
   * 「看大图」：会话里点一下那张缩略图，就在这一层上把它整张摊开（桌面端与移动端共用）。
   *
   * 两件事必须留着：**关得掉**（点背景 / Esc / 那颗「关闭」）与**还能交给系统**（这颗按钮与
   * 会话里点一条普通文件消息同一个落点 `open_path`）—— 图片在应用里看得见，不代表"用别的
   * 程序打开它"这条老路就该没了。
   *
   * 尺寸上限在**读盘那一层**（`paste::MAX_PREVIEW_BYTES`）：超过它的图这里也读不出来，
   * 那时只显示一句原话 + 「用系统程序打开」。
   */
  let {
    path,
    name,
    onclose,
    onopenfile,
  }: {
    path: string;
    name: string;
    onclose: () => void;
    /** 用系统默认程序打开这一份（与点普通文件消息同一件事）。 */
    onopenfile: () => void;
  } = $props();

  let url = $state('');
  let error = $state('');

  $effect(() => {
    const target = path;
    let live = true;
    url = '';
    error = '';
    void imageDataUrl(target)
      .then((next) => {
        if (live) url = next;
      })
      .catch((e) => {
        if (live) error = e instanceof Error ? e.message : String(e);
      });
    return () => {
      live = false;
    };
  });
</script>

<svelte:window
  onkeydown={(e) => {
    if (e.key === 'Escape') onclose();
  }}
/>

<!-- 点背景关掉：这一层是"看它一眼"，不是第二个窗口。背景那一层做成**真的 button**
     （铺满整层、`aria-label` 说明它是干什么的），而不是给一个 `div` 挂 `onclick` ——
     前者键盘/读屏都能用，后者只是把"能点"画出来。 -->
<div class="iv-layer" role="dialog" aria-modal="true" tabindex="-1" aria-label="查看图片：{name}">
  <button class="iv-scrim" type="button" aria-label="关闭大图" onclick={onclose}></button>
  <div class="iv-body">
    {#if url}
      <img class="iv-img" src={url} alt={name} draggable="false" />
    {:else if error}
      <p class="iv-err">{error}</p>
    {:else}
      <p class="iv-err">正在读这张图…</p>
    {/if}
    <div class="iv-bar">
      <span class="iv-name" title={name}>{name}</span>
      <button class="iv-btn" type="button" onclick={onopenfile}>用系统程序打开</button>
      <button class="iv-btn" type="button" onclick={onclose}>关闭</button>
    </div>
  </div>
</div>

<style>
  /* 只用基础色板里的变量与 rgba（不写 `color-mix()` / `oklch()`）：这份样式进产物那份 CSS，
     移动端产物要过 Chrome 90 的兼容门禁，见 `scripts/check-compat.mjs`。 */
  .iv-layer {
    position: absolute;
    inset: 0;
    z-index: 95;
    display: grid;
    place-items: center;
    padding: 14px;
    background: rgba(0, 0, 0, 0.62);
  }
  /* 铺满整层的关闭按钮（在 `.iv-body` 底下）：点哪儿都算关，但它是真的可聚焦元素。 */
  .iv-scrim {
    position: absolute;
    inset: 0;
    z-index: 0;
    padding: 0;
    border: 0;
    background: transparent;
    cursor: zoom-out;
  }
  .iv-body {
    position: relative;
    z-index: 1;
    display: flex;
    flex-direction: column;
    gap: 8px;
    max-width: 100%;
    max-height: 100%;
  }
  .iv-img {
    display: block;
    max-width: 100%;
    max-height: 72vh;
    object-fit: contain;
    border-radius: 10px;
    background: var(--surface);
  }
  .iv-err {
    margin: 0;
    padding: 10px 12px;
    border-radius: 10px;
    background: var(--surface);
    color: var(--muted);
    font-size: var(--fs-xs);
    line-height: 1.6;
  }
  .iv-bar {
    display: flex;
    align-items: center;
    gap: 6px;
    padding: 6px 8px;
    border-radius: 10px;
    background: var(--surface);
  }
  .iv-name {
    flex: 1;
    min-width: 0;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    font-family: var(--font-display);
    font-size: var(--fs-xs);
    color: var(--fg);
  }
  .iv-btn {
    flex: none;
    padding: 4px 9px;
    border: 1px solid var(--border);
    border-radius: 8px;
    background: var(--surface);
    color: var(--fg);
    font-size: var(--fs-xs);
    cursor: pointer;
  }
</style>