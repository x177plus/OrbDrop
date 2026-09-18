<script lang="ts">
  import { getHint, hideHint } from '$lib/stores/hint.svelte';

  const GAP = 8;     // 触发点与气泡的间距
  /** 气泡与窗口边缘的最小间距 = `App.svelte` 的 `PANEL_SLACK`（面板四周那圈透明余量）：
      夹在这圈里面，气泡既不会探出面板的圆角边框挂到桌面上，`--shadow-bubble` 的可见半径
      （实测下 9.6）也不会越过窗口边被硬裁成一条切线（同 `:root` 里量过的那些数）。 */
  const MARGIN = 20;

  let bubble = $state<HTMLDivElement | null>(null);
  let pos = $state({ x: 0, y: 0 });
  /** 滚动计数：气泡要跟着触发点走，不能按弹出来那一刻的旧矩形钉在原地 ——
      键盘 Tab 到「?」时浏览器会先把它滚进视野，而那一下滚动发生在气泡弹出之后，
      钉住旧位置就会错位。 */
  let scrollTick = $state(0);

  let hint = $derived(getHint());

  /**
   * 落位：气泡多大只有渲染出来才知道，所以挂载后量一次再定。
   * 默认落在触发点下方；下方放不下就翻到上方；水平方向夹在 `MARGIN` 那圈里。
   */
  $effect(() => {
    const h = hint;
    const node = bubble;
    scrollTick;   // 依赖：滚动后重新落位
    if (!h || !node) return;
    if (!h.trigger.isConnected) { hideHint(); return; }
    const rect = h.trigger.getBoundingClientRect();
    const w = node.offsetWidth;
    const ht = node.offsetHeight;
    const vw = window.innerWidth;
    const vh = window.innerHeight;
    const x = Math.min(Math.max(rect.left, MARGIN), Math.max(MARGIN, vw - w - MARGIN));
    const below = rect.bottom + GAP;
    const above = rect.top - GAP - ht;
    const y = below + ht <= vh - MARGIN
      ? below
      : above >= MARGIN
        ? above
        : Math.max(MARGIN, vh - ht - MARGIN);
    pos = { x, y };
  });

  // `scroll` 事件不冒泡，所以用捕获阶段挂在 window 上：面板/设置里的滚动也收得到。
  $effect(() => {
    const onScroll = () => scrollTick++;
    window.addEventListener('scroll', onScroll, true);
    return () => window.removeEventListener('scroll', onScroll, true);
  });
</script>

{#if hint}
  <div class="hint-tip" id="hint-tip" role="tooltip" bind:this={bubble} style="left:{pos.x}px; top:{pos.y}px">
    {@render hint.content()}
  </div>
{/if}