<script lang="ts">
  import { onMount } from 'svelte';

  let {
    x, y,
    kind,
    shareId, path, isDir,
    deviceState,
    deviceOnline = true,
    deviceNeedsPair = false,
    peerName,
    bcLeftLabel = null,
    bcRightLabel = null,
    bcCompareLabel = '',
    bcCompareEnabled = true,
    bcThisStaged = false,
    onClose,
    onToast,
    onDownload,
    onOpen,
    onCopy,
    onRemove,
    onConnect,
    onDisconnect,
    onPair,
    onUnpair,
    onPush,
    onPeerDownload,
    onBcCompare,
    onBcAddLeft,
    onBcAddRight,
    onBcClear,
  }: {
    x: number; y: number;
    kind: 'share' | 'device' | 'peer';
    shareId?: string;
    path?: string;
    isDir?: boolean;
    deviceState?: string;
    /** 右击的设备此刻是否在线：离线时只剩「解除配对」是本机动作。 */
    deviceOnline?: boolean;
    /** token 已失效（对端不认我们）：这一档的动作是「配对」，不是「连接」。 */
    deviceNeedsPair?: boolean;
    peerName?: string;
    /** 已在比较位上的项（null = 空），文案由后端给的 label 提供。 */
    bcLeftLabel?: string | null;
    bcRightLabel?: string | null;
    /** 追加在「用 BeyondCompare 比较」后的对手方说明；'' = 无可比对象。 */
    bcCompareLabel?: string;
    bcCompareEnabled?: boolean;
    /** 当前右键的这一项是否已在某个比较位上。 */
    bcThisStaged?: boolean;
    onClose: () => void;
    onToast: (msg: string) => void;
    onDownload: () => void;
    onOpen: () => void;
    onCopy: () => void;
    onRemove: () => void;
    onConnect: () => void;
    onDisconnect: () => void;
    onPair: () => void;
    onUnpair: () => void;
    onPush: () => void;
    onPeerDownload: () => void;
    onBcCompare: () => void;
    onBcAddLeft: () => void;
    onBcAddRight: () => void;
    onBcClear: () => void;
  } = $props();

  function handleItem(fn: () => void) {
    fn();
  }

  let root = $state<HTMLDivElement | null>(null);

  /**
   * 菜单在挂载期间独占「点击任意位置关闭」的职责：组件只在 ctxState 非空时存在，
   * 因此这里的监听生命周期就等于菜单的可见期。
   *
   * 用 pointerdown（捕获阶段）而不是 click：列表行会 stopPropagation 或消费 click，
   * 只有 pointerdown 能保证任何位置按下都先到这里。
   */
  onMount(() => {
    function onPointerDown(e: PointerEvent) {
      if (!root?.contains(e.target as Node)) onClose();
    }
    function onKeyDown(e: KeyboardEvent) {
      if (e.key === 'Escape') onClose();
    }
    document.addEventListener('pointerdown', onPointerDown, true);
    document.addEventListener('keydown', onKeyDown);
    return () => {
      document.removeEventListener('pointerdown', onPointerDown, true);
      document.removeEventListener('keydown', onKeyDown);
    };
  });

  // Adjust position to stay within viewport
  let style = $derived(() => {
    const vw = window.innerWidth;
    const vh = window.innerHeight;
    let left = x;
    let top = y;
    // rough adjustment — the ctx menu is ~200px wide, ~200px tall max
    if (left + 200 > vw) left = Math.max(8, vw - 200 - 8);
    if (top + 250 > vh) top = Math.max(8, vh - 250 - 8);
    return `left:${left}px; top:${top}px`;
  });
</script>

<!-- BeyondCompare 菜单区：本机共享项与对端共享项共用同一份结构 -->
{#snippet bcSection()}
  <div class="ctx-sep"></div>
  {#if bcLeftLabel}
    <button class="ctx-item" disabled>
      <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6"><circle cx="12" cy="12" r="3"/></svg>
      左：{bcLeftLabel}
    </button>
  {/if}
  {#if bcRightLabel}
    <button class="ctx-item" disabled>
      <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6"><circle cx="12" cy="12" r="3"/></svg>
      右：{bcRightLabel}
    </button>
  {/if}
  <button class="ctx-item" disabled={!bcCompareEnabled} onclick={() => handleItem(onBcCompare)}>
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linejoin="round"><rect x="3" y="4" width="7" height="16" rx="1.5"/><rect x="14" y="4" width="7" height="16" rx="1.5"/></svg>
    用 BeyondCompare 比较{bcCompareLabel ? ` · ${bcCompareLabel}` : ''}
  </button>
  <button class="ctx-item" onclick={() => handleItem(onBcAddLeft)}>
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M20 12H4M10 6l-6 6 6 6"/></svg>
    添加到左侧比较位
  </button>
  <button class="ctx-item" onclick={() => handleItem(onBcAddRight)}>
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M4 12h16M14 6l6 6-6 6"/></svg>
    添加到右侧比较位
  </button>
  {#if bcThisStaged}
    <button class="ctx-item" onclick={() => handleItem(onBcClear)}>
      <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="9"/><path d="M9 9l6 6M15 9l-6 6"/></svg>
      清除比较位
    </button>
  {/if}
{/snippet}

<!-- svelte-ignore a11y_click_events_have_key_events a11y_no_static_element_interactions -->
<div class="ctx show" bind:this={root} style={style()} role="menu" tabindex="-1" onclick={onClose} oncontextmenu={(e) => e.preventDefault()}>
  {#if kind === 'peer'}
    <button class="ctx-item" onclick={() => handleItem(onPeerDownload)}>
      <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M12 3v12M7 10l5 5 5-5M5 19h14"/></svg>
      下载到本地{peerName ? ` · 来自${peerName}` : ''}
    </button>
    {@render bcSection()}
  {:else if kind === 'share'}
    <button class="ctx-item" onclick={() => handleItem(onDownload)}>
      <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M12 3v12M7 10l5 5 5-5M5 19h14"/></svg>
      下载到本地
    </button>
    <button class="ctx-item" onclick={() => handleItem(onOpen)}>
      <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M3 7a2 2 0 0 1 2-2h4.2a2 2 0 0 1 1.4.6L12 7h7a2 2 0 0 1 2 2v9a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V7Z"/></svg>
      打开所在文件夹
    </button>
    <button class="ctx-item" onclick={() => handleItem(onCopy)}>
      <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M4 7h11v13H4z"/><path d="M8 4h12v12"/></svg>
      复制共享路径
    </button>
    <div class="ctx-sep"></div>
    <button class="ctx-item danger" onclick={() => handleItem(onRemove)}>
      <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M4 7h16M9 7V4h6v3M6 7l1 13h10l1-13M10 11v6M14 11v6"/></svg>
      移除共享项
    </button>
    {@render bcSection()}
  {:else}
    {#if !deviceOnline}
      <!-- 离线设备：连接/推送的目标都不在网上（拨号只会空转、推送邀请是 HTTP 直达），
           只有「解除配对」是本机动作，所以这里什么都不给。 -->
    {:else if deviceState === 'connected'}
      <button class="ctx-item" onclick={() => handleItem(onDisconnect)}>
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M10 13a5 5 0 0 0 7 0l3-3a5 5 0 0 0-7-7l-1.5 1.5"/><path d="M14 11a5 5 0 0 0-7 0l-3 3a5 5 0 0 0 7 7l1.5-1.5"/><path d="M3 3l18 18"/></svg>
        断开长连接
      </button>
    {:else if deviceNeedsPair}
      <!-- token 已失效：拨号必然 401，唯一出路是重新配对 -->
      <button class="ctx-item" onclick={() => handleItem(onPair)}>
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M12 5v14M5 12h14"/></svg>
        重新配对
      </button>
    {:else if deviceState === 'unbound'}
      <!-- 未绑定：只做两件事 —— 配对，或直接推送（推送邀请不需要预先配对）。
           没有「立即连接」（未配对拨不动），也没有「解除配对」（没有关系可解）。 -->
      <button class="ctx-item" onclick={() => handleItem(onPair)}>
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M12 5v14M5 12h14"/></svg>
        配对
      </button>
    {:else}
      <button class="ctx-item" onclick={() => handleItem(onConnect)}>
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M10 13a5 5 0 0 0 7 0l3-3a5 5 0 0 0-7-7l-1.5 1.5"/><path d="M14 11a5 5 0 0 0-7 0l-3 3a5 5 0 0 0 7 7l1.5-1.5"/></svg>
        立即连接
      </button>
    {/if}
    {#if deviceOnline}
      <button class="ctx-item" onclick={() => handleItem(onPush)}>
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M12 3v12M7 10l5 5 5-5M5 19h14"/></svg>
        推送文件
      </button>
    {/if}
    {#if deviceState !== 'unbound'}
      <div class="ctx-sep"></div>
      <button class="ctx-item danger" onclick={() => handleItem(onUnpair)}>
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M4 7h16M9 7V4h6v3M6 7l1 13h10l1-13M10 11v6M14 11v6"/></svg>
        解除配对
      </button>
    {/if}
  {/if}
</div>

<style>
  /* 比较位提示行 / 无可比对象的比较项：沿用 .ctx-item 排版，但不可点也不高亮。
     pointer-events:none 让点击落到菜单根节点上，等于「点空白关闭菜单」。 */
  .ctx-item:disabled {
    opacity: 0.55;
    color: var(--muted);
    cursor: default;
    pointer-events: none;
  }
</style>