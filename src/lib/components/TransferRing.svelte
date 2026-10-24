<script lang="ts">
  /**
   * 会话里文件气泡右端那颗**大图标 + 圆环**（桌面与安卓共用）。
   *
   * - 接收中：环按已收 / 总大小走，中心是暂停键；暂停后中心是继续键。
   * - 发送中：环按这批的进度走，中心是停止键 —— 按下去就是取消这条推送。
   *
   * **两端同一颗、同一套样式**，直径由调用方的字号环境决定：默认 36u（安卓按 `--u` 缩放；
   * 桌面在 `main.css` 的 `.chat-msg` 上把它定成 32px —— 那边的气泡文字是 13px、安卓是 15u，
   * 36 的环在桌面这一行里会显得比安卓大一圈，32 才是与安卓同一个观感）。中心图标**不另着色**
   * —— 跟着气泡文字走（`currentColor`）：蓝气泡上是白的、浅底卡片上是 `--fg`，两端都一样。
   * 进度弧保留主色；暂停时整圈转灰（那是状态，不是图标）。弧是**开路径 + 圆头端帽**
   * （不是闭合圆切虚线 —— 那种画法在各引擎上不一定给端帽圆角），0% 时整条弧不画。
   *
   * 进度来源由调用方给：接收侧是 `pct` 现算的那一份，发送侧是 `push://batch` 的聚合进度。
   */
  let {
    mode,
    status = 'running',
    pct = 0,
    name,
    onclick,
  }: {
    mode: 'receive' | 'send';
    /** 接收中 / 已暂停；发送侧恒为 `running`。 */
    status?: 'running' | 'paused';
    /** 0–100（调用方已经算好并夹过界）。 */
    pct?: number;
    name: string;
    onclick: (e: MouseEvent) => void;
  } = $props();

  const R = 15;
  const clamped = $derived(Math.min(100, Math.max(0, pct)));
  const paused = $derived(mode === 'receive' && status === 'paused');
  const label = $derived(
    mode === 'send'
      ? `停止发送「${name}」`
      : paused
        ? `继续接收「${name}」`
        : `暂停接收「${name}」`,
  );

  function activate(e: MouseEvent): void {
    // 环常常贴在整行按钮边上：拦住冒泡，别让"暂停/停止"顺手触发"打开文件"。
    e.stopPropagation();
    onclick(e);
  }
</script>

<button
  class="xring"
  class:paused
  type="button"
  aria-label={label}
  title={label}
  onclick={activate}
>
  <svg class="ring" viewBox="0 0 36 36" aria-hidden="true">
    <circle class="track" cx="18" cy="18" r={R} />
    {#if clamped > 0}
      <!-- **开**弧（不是一个闭合圆 + 虚线）：圆头端帽只保证画在开放子路径上，闭合圆切出来
           的两端在不同引擎/版本上不保证圆角 —— 不赌这个，改成开弧。
           `pathLength="100"` 把长度归一成百分比，位移就是 `100 - pct`，和原来那份数学一一对应。 -->
      <path
        class="bar"
        d="M18 3 A15 15 0 1 1 17.999 3"
        pathLength="100"
        stroke-dasharray="100"
        stroke-dashoffset={100 - clamped}
        stroke-linecap="round"
        stroke-linejoin="round"
      />
    {/if}
  </svg>
  <span class="glyph" aria-hidden="true">
    {#if mode === 'send'}
      <svg viewBox="0 0 24 24"><rect x="7.5" y="7.5" width="9" height="9" rx="1.5" /></svg>
    {:else if paused}
      <svg viewBox="0 0 24 24"><path d="M8 5.5v13l10-6.5z" /></svg>
    {:else}
      <svg viewBox="0 0 24 24">
        <rect x="7" y="5.5" width="3.5" height="13" rx="1.2" />
        <rect x="13.5" y="5.5" width="3.5" height="13" rx="1.2" />
      </svg>
    {/if}
  </span>
</button>

<style>
  .xring {
    /* 直径：默认 36u（安卓那套字号）；桌面由 `main.css` 的 `.chat-msg` 覆盖成 32px。 */
    --ring: var(--xring-size, calc(36 * var(--u, 1px)));
    /* 线宽（用户单位）：默认 3（36u 里就是 3px）。桌面环缩到 32px 后线也跟着缩成 2.67px，
       圆头端帽就几乎看不出了 —— 那边把它补成 3.375（= 3px × 36 / 32），两端线的**绝对**
       粗细与端帽半径一致。 */
    --stroke: var(--xring-stroke, 3);
    position: relative;
    flex: none;
    width: var(--ring);
    height: var(--ring);
    border-radius: 50%;
    display: grid;
    place-items: center;
    /* 不指定颜色 = 继承气泡的文字色（`currentColor`），与左侧文件图标同一个"默认色"。 */
    color: inherit;
    background: transparent;
  }
  .xring:hover {
    background: var(--fg-soft);
  }
  .ring {
    position: absolute;
    inset: 0;
    width: 100%;
    height: 100%;
  }
  .ring .track {
    fill: none;
    stroke: var(--border);
    stroke-width: var(--stroke);
  }
  .ring .bar {
    fill: none;
    stroke: var(--accent);
    stroke-width: var(--stroke);
    stroke-linecap: round;
    transition: stroke-dashoffset 0.3s linear;
  }
  .xring.paused .ring .bar {
    stroke: var(--muted);
    transition: none;
  }
  .glyph {
    width: calc(var(--ring) * 0.42);
    height: calc(var(--ring) * 0.42);
    display: grid;
    place-items: center;
  }
  .glyph svg {
    width: 100%;
    height: 100%;
    fill: currentColor;
  }
</style>