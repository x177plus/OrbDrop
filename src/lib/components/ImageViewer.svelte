<script lang="ts">
  import { imageDataUrl } from '$lib/images';
  import { t } from '$lib/i18n.svelte';

  /**
   * 「看大图」：会话里点一下那张缩略图，就在这一层上把它整张摊开（桌面端与移动端共用）。
   *
   * 两件事必须留着：**关得掉**（点图片 / 点背景 / Esc / 那颗「关闭」）与**还能交给系统**（这颗按钮与
   * 会话里点一条普通文件消息同一个落点 `open_path`）—— 图片在应用里看得见，不代表"用别的
   * 程序打开它"这条老路就该没了。
   *
   * 尺寸由**这一层自己**决定，不跟视口走：这一层常常只挂在会话浮层里（桌面端那块 320×420），
   * 用 `72vh` 量出来的图会比浮层高，下半截与底下那排按钮一起被裁掉。所以图片区吃满剩余高度、
   * 自己滚，操作条永远是最后一行。长图按容器宽度铺开、纵向滚动（鼠标滚轮 / 触屏拖动 / 按住
   * 鼠标拖），不再为了"整张塞进来"缩成一条。
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

  /** 图片那块（可滚的按钮）：拖拽平移直接改它的 `scrollLeft` / `scrollTop`。 */
  let stageEl = $state<HTMLButtonElement | null>(null);
  /** 鼠标按住的起点；`null` = 没在拖。 */
  let pan: { x: number; y: number; left: number; top: number; moved: boolean } | null = null;
  /** 刚拖完的那一次 `click` 不能当成"点一下收起"（拖动松手浏览器照样派发 click）。 */
  let afterPan = false;

  /** 只有"图比这块大、确实滚得动"时才接管鼠标拖动；否则让 click 直接走收起。 */
  function onStageDown(e: PointerEvent) {
    afterPan = false;
    const stage = stageEl;
    if (!stage || e.pointerType !== 'mouse' || e.button !== 0) return;
    if (stage.scrollHeight <= stage.clientHeight && stage.scrollWidth <= stage.clientWidth) return;
    pan = { x: e.clientX, y: e.clientY, left: stage.scrollLeft, top: stage.scrollTop, moved: false };
    stage.setPointerCapture(e.pointerId);
  }

  function onStageMove(e: PointerEvent) {
    const stage = stageEl;
    if (!stage || !pan) return;
    const dx = e.clientX - pan.x;
    const dy = e.clientY - pan.y;
    // 手指/鼠标抖一两像素不算拖：那还是"点一下"。
    if (!pan.moved && Math.abs(dx) + Math.abs(dy) < 4) return;
    pan.moved = true;
    stage.scrollLeft = pan.left - dx;
    stage.scrollTop = pan.top - dy;
  }

  function onStageUp(e: PointerEvent) {
    const stage = stageEl;
    if (!pan) return;
    const moved = pan.moved;
    pan = null;
    if (stage?.hasPointerCapture(e.pointerId)) stage.releasePointerCapture(e.pointerId);
    if (moved) afterPan = true;
  }

  function onStageClick() {
    if (afterPan) {
      afterPan = false;
      return;
    }
    onclose();
  }

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
<div class="iv-layer" role="dialog" aria-modal="true" tabindex="-1" aria-label={t('查看图片：{name}', { name })}>
  <button class="iv-scrim" type="button" aria-label={t('关闭大图')} onclick={onclose}></button>
  <div class="iv-body">
    <!-- 图片区本身就是那颗"收起"按钮：点图、点图旁边的空处都收，键盘回车上也一样。
         它同时是滚动容器 —— 长图按容器宽度铺开，超出高度就滚；操作条在它外面，永远看得见。
         图区与图的点击都指向同一个动作，"点一次放大、再点一次缩小"就是这一下。 -->
    <button
      class="iv-stage"
      type="button"
      bind:this={stageEl}
      aria-label={t('收起大图')}
      title={t('点一下收起大图')}
      onclick={onStageClick}
      onpointerdown={onStageDown}
      onpointermove={onStageMove}
      onpointerup={onStageUp}
      onpointercancel={onStageUp}
    >
      {#if url}
        <img class="iv-img" src={url} alt={name} draggable="false" />
      {:else if error}
        <span class="iv-err">{error}</span>
      {:else}
        <span class="iv-err">{t('正在读这张图…')}</span>
      {/if}
    </button>
    <div class="iv-bar">
      <span class="iv-name" title={name}>{name}</span>
      <button class="iv-btn" type="button" onclick={onopenfile}>{t('用系统程序打开')}</button>
      <button class="iv-btn" type="button" onclick={onclose}>{t('关闭')}</button>
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
    display: flex;
    flex-direction: column;
    padding: 12px;
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
    flex: 1 1 auto;
    min-height: 0;
    display: flex;
    flex-direction: column;
    gap: 8px;
    width: 100%;
    /* 只有图区和操作条吃指针：它们之间的缝、图区外的边，点下去落到背后那颗关闭按钮上。 */
    pointer-events: none;
  }
  /* 图片区：滚的是这一块，不是整层 —— 图再长也顶不走下面那条操作栏。
     它自己就是颗按钮（点图 / 点图旁空处都收起），所以平时看起来要完全不像按钮。 */
  .iv-stage {
    flex: 1 1 auto;
    min-height: 0;
    display: flex;
    width: 100%;
    padding: 0;
    border: 0;
    background: transparent;
    overflow: auto;
    overscroll-behavior: contain;
    cursor: zoom-out;
    pointer-events: auto;
    /* 滚动条不画出来（长图照样滚）：一条灰杠贴在图片右边，比图本身还显眼。
       三种写法覆盖 Chromium/WebView、Firefox 与旧 Edge —— 与 `.appmain` / `.chatlog` 同一套。 */
    scrollbar-width: none;
    -ms-overflow-style: none;
  }
  .iv-stage::-webkit-scrollbar {
    display: none;
  }
  /* 长图按容器宽度铺开（`max-width` 不是 `max-height`）：超出那一截交给上面的滚动，
     不为了"整张塞进来"把图缩成一条。`margin: auto` 让装得下的图居中；装不下时
     auto 外边距按 0 算，起点仍在左上角，滚得到。 */
  .iv-img {
    display: block;
    margin: auto;
    max-width: 100%;
    height: auto;
    border-radius: 10px;
    background: var(--surface);
  }
  .iv-err {
    display: block;
    margin: auto;
    max-width: 100%;
    padding: 10px 12px;
    border-radius: 10px;
    background: var(--surface);
    color: var(--muted);
    font-size: var(--fs-xs);
    line-height: 1.6;
  }
  .iv-bar {
    flex: none;
    display: flex;
    align-items: center;
    gap: 6px;
    padding: 6px 8px;
    border-radius: 10px;
    background: var(--surface);
    pointer-events: auto;
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