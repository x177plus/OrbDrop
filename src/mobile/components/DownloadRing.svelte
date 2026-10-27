<script lang="ts">
  import Icon from "../Icon.svelte";
  import { t } from "$lib/i18n.svelte";

  /**
   * 文件行右侧那一圈：**行高不变**，下载键原地变成进度环。
   *
   * - 没下过 / 失败：下载箭头，点一下开始（与点整行同一条动作）。
   * - 下载中：圆环按 `received / total` 走，中心是暂停键；点一下暂停。
   * - 已暂停：圆环停在原处、颜色转灰，中心是继续键；点一下接着下。
   * - 已完成：一圈对勾，不再响应点击（点整行 = 打开已落盘的那份）。
   *
   * 对端没报大小时不编百分比：圆环里跑一段短弧（与传输页的 indeterminate 同一个意思）。
   *
   * 两处用它：「文件 → 浏览」的对端文件行（`inline = false`，绝对定位在行右端）与会话里
   * **正在接收**的那条文件气泡（`inline = true`，作为气泡行里的一个 flex 项）。显示与机制
   * （点环暂停 / 继续、暂停不丢已收字节）两处一模一样，差别只在动词（下载 / 接收）。
   */
  let {
    status = "idle",
    received = 0,
    total = 0,
    error = null,
    name,
    verb = "下载",
    inline = false,
    onToggle,
  }: {
    status?: "idle" | "running" | "paused" | "done" | "failed";
    /** 已收字节。 */
    received?: number;
    /** 对端报的总大小；0 = 不知道（不编百分比，走 indeterminate 短弧）。 */
    total?: number;
    /** 失败原因（只有 `status === "failed"` 时有）。 */
    error?: string | null;
    name: string;
    /** 动作动词：浏览里是「下载」，会话接收是「接收」。 */
    verb?: string;
    /** `true` = 作为行内 flex 项摆（不绝对定位），会话气泡用。 */
    inline?: boolean;
    onToggle: () => void;
  } = $props();

  const R = 15;
  /** 动作动词按 msgid 传进来（默认「下载」，会话接收是「接收」）—— 跟着界面语言一起翻。 */
  const verbText = $derived(t(verb));
  const C = 2 * Math.PI * R;

  const knownTotal = $derived(total > 0);
  const pct = $derived(
    total > 0 ? Math.min(100, Math.max(0, Math.round((received / total) * 100))) : 0,
  );
  const label = $derived(
    status === "running"
      ? t("暂停{verb}「{name}」", { verb: verbText.toLowerCase(), name })
      : status === "paused"
        ? t("继续{verb}「{name}」", { verb: verbText.toLowerCase(), name })
        : status === "failed"
          ? t("{verb}失败：{error}，点击重试", { verb: verbText, error: error ?? t("未知原因") })
          : t("{verb}「{name}」", { verb: verbText, name }),
  );

  function toggle(e: MouseEvent): void {
    // 环盖在整行按钮上面：不拦住就会同时触发整行的「打开 / 开始下载」。
    e.stopPropagation();
    if (status === "done") return;
    onToggle();
  }
</script>

{#if status === "done"}
  <span class="dlring done" class:inline={inline} aria-hidden="true"><Icon name="check" /></span>
{:else}
  <button
    class="dlring"
    class:paused={status === "paused"}
    class:failed={status === "failed"}
    class:inline={inline}
    type="button"
    aria-label={label}
    title={label}
    onclick={toggle}
  >
    {#if status === "running" || status === "paused"}
      <svg class="ring" class:indet={!knownTotal && status === "running"} viewBox="0 0 36 36" aria-hidden="true">
        <circle class="track" cx="18" cy="18" r={R} />
        <circle
          class="bar"
          cx="18"
          cy="18"
          r={R}
          stroke-dasharray={knownTotal ? C : "24 70"}
          stroke-dashoffset={knownTotal ? C * (1 - pct / 100) : 0}
        />
      </svg>
      <span class="glyph" aria-hidden="true">
        {#if status === "paused"}
          <svg viewBox="0 0 24 24"><path d="M8 5.5v13l10-6.5z" /></svg>
        {:else}
          <svg viewBox="0 0 24 24">
            <rect x="7" y="5.5" width="3.5" height="13" rx="1.2" />
            <rect x="13.5" y="5.5" width="3.5" height="13" rx="1.2" />
          </svg>
        {/if}
      </span>
    {:else}
      <svg class="ring" viewBox="0 0 36 36" aria-hidden="true">
        <circle class="track" cx="18" cy="18" r={R} />
      </svg>
      <Icon name="download" />
    {/if}
  </button>
{/if}

<style>
  .dlring {
    position: absolute;
    right: calc(12 * var(--u));
    top: 50%;
    transform: translateY(-50%);
    width: calc(36 * var(--u));
    height: calc(36 * var(--u));
    border-radius: 50%;
    display: grid;
    place-items: center;
    color: var(--muted);
  }
  .dlring:hover { background: var(--fg-soft); color: var(--fg); }
  /* 行内那一档（会话里正在接收的文件气泡）：不绝对定位，作为 `.fbub` 的一个 flex 项贴在
     文字右侧 —— 与浏览那一行同一颗圆环、同一套点击语义，只是位置跟着气泡走。 */
  .dlring.inline {
    position: static;
    transform: none;
    flex: none;
    margin-left: calc(2 * var(--u));
  }
  /* 完成的对勾只是标记：让它对点击透明，点到它上面也算点到整行（打开文件）。 */
  .dlring.done { color: var(--ok); pointer-events: none; }
  .dlring.failed { color: var(--warn); }
  .dlring.failed .ring .track { stroke: var(--warn); }
  .dlring :global(.ic) { width: calc(18 * var(--u)); height: calc(18 * var(--u)); }
  .ring { position: absolute; inset: 0; width: 100%; height: 100%; transform: rotate(-90deg); }
  .ring .track { fill: none; stroke: var(--border); stroke-width: 3; }
  .ring .bar {
    fill: none;
    stroke: var(--accent);
    stroke-width: 3;
    stroke-linecap: round;
    transition: stroke-dashoffset .3s linear;
  }
  .dlring.paused .ring .bar { stroke: var(--muted); transition: none; }
  .ring.indet .bar {
    transform-box: fill-box;
    transform-origin: center;
    animation: dl-spin 1.1s linear infinite;
  }
  @keyframes dl-spin { to { transform: rotate(360deg); } }
  .glyph { width: calc(15 * var(--u)); height: calc(15 * var(--u)); display: grid; place-items: center; }
  .glyph svg { width: 100%; height: 100%; fill: currentColor; }
  .dlring.paused .glyph { color: var(--muted); }
  .dlring:not(.paused) .glyph { color: var(--accent-deep); }
</style>