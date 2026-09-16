<script lang="ts">
  import { onMount } from 'svelte';

  let {
    x, y,
    kind,
    shareId, path, isDir,
    deviceState,
    peerName,
    onClose,
    onToast,
    onDownload,
    onOpen,
    onCopy,
    onRemove,
    onConnect,
    onDisconnect,
    onUnpair,
    onPush,
    onPeerDownload,
  }: {
    x: number; y: number;
    kind: 'share' | 'device' | 'peer';
    shareId?: string;
    path?: string;
    isDir?: boolean;
    deviceState?: string;
    peerName?: string;
    onClose: () => void;
    onToast: (msg: string) => void;
    onDownload: () => void;
    onOpen: () => void;
    onCopy: () => void;
    onRemove: () => void;
    onConnect: () => void;
    onDisconnect: () => void;
    onUnpair: () => void;
    onPush: () => void;
    onPeerDownload: () => void;
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

<!-- svelte-ignore a11y_click_events_have_key_events a11y_no_static_element_interactions -->
<div class="ctx show" bind:this={root} style={style()} role="menu" tabindex="-1" onclick={onClose} oncontextmenu={(e) => e.preventDefault()}>
  {#if kind === 'peer'}
    <button class="ctx-item" onclick={() => handleItem(onPeerDownload)}>
      <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M12 3v12M7 10l5 5 5-5M5 19h14"/></svg>
      下载到本地{peerName ? ` · 来自${peerName}` : ''}
    </button>
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
  {:else}
    {#if deviceState === 'connected'}
      <button class="ctx-item" onclick={() => handleItem(onDisconnect)}>
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M10 13a5 5 0 0 0 7 0l3-3a5 5 0 0 0-7-7l-1.5 1.5"/><path d="M14 11a5 5 0 0 0-7 0l-3 3a5 5 0 0 0 7 7l1.5-1.5"/><path d="M3 3l18 18"/></svg>
        断开长连接
      </button>
    {:else}
      <button class="ctx-item" onclick={() => handleItem(onConnect)}>
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M10 13a5 5 0 0 0 7 0l3-3a5 5 0 0 0-7-7l-1.5 1.5"/><path d="M14 11a5 5 0 0 0-7 0l-3 3a5 5 0 0 0 7 7l1.5-1.5"/></svg>
        立即连接
      </button>
    {/if}
    <button class="ctx-item" onclick={() => handleItem(onPush)}>
      <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M12 3v12M7 10l5 5 5-5M5 19h14"/></svg>
      推送文件
    </button>
    <div class="ctx-sep"></div>
    <button class="ctx-item danger" onclick={() => handleItem(onUnpair)}>
      <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M4 7h16M9 7V4h6v3M6 7l1 13h10l1-13M10 11v6M14 11v6"/></svg>
      解除配对
    </button>
  {/if}
</div>