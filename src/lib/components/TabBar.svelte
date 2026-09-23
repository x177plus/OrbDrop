<script lang="ts">
  import type { PeerDevice, TabKey } from '$lib/types';
  import { tabKeyEquals } from '$lib/types';
  import { scrollRail } from '$lib/scrollRail';
  import { unreadForPeer } from '$lib/stores/chat.svelte';

  let {
    localCount = 0,
    deviceCount = 0,
    /** 已建立长连接的设备：每台一个浏览标签（F-24 / F-30）。 */
    browsers = [],
    activeTab,
    onSelect,
  }: {
    localCount?: number;
    deviceCount?: number;
    browsers?: PeerDevice[];
    activeTab: TabKey;
    onSelect: (tab: TabKey) => void;
  } = $props();

  const LOCAL: TabKey = { kind: 'local' };
  const DEVICES: TabKey = { kind: 'devices' };

  function isActive(tab: TabKey): boolean {
    return tabKeyEquals(tab, activeTab);
  }

  /* ─── 溢出后怎么够到后方标签 ──────────────────────────────────────
   长连接设备一多，标签栏（`.tabs`）就横向溢出。拖动横滚 + 滚轮横滚由 `scrollRail`
   统一提供（选择框的「常用」栏用的是同一份实现，见 `$lib/scrollRail.ts`）。 */

  let tabsEl: HTMLDivElement | undefined;
  let peerEls: Record<string, HTMLButtonElement | undefined> = {};

  /* 新设备接入：把它滚进视野。否则标签静默加在末尾、留在可视区外，
     用户根本不知道多了一台（activeTab 只在点击和对端消失时才写）。
     首次有对端不算"新接入"：那是启动时批量接回长连接，别一开面板标签栏就自己跳走。
     inline:'nearest' 只滚够看到它为止，不把整条标签甩到最右。 */
  let knownPeerIds: string[] = [];
  $effect(() => {
    const ids = browsers.map((d) => d.id);
    const added = ids.filter((id) => !knownPeerIds.includes(id));
    const firstPopulation = knownPeerIds.length === 0;
    knownPeerIds = ids;
    if (firstPopulation || added.length === 0) return;
    peerEls[added[added.length - 1]]?.scrollIntoView({ inline: 'nearest', block: 'nearest' });
  });
</script>

<!-- svelte-ignore a11y_no_static_element_interactions -->
<div
  class="tabs"
  role="tablist"
  tabindex="-1"
  bind:this={tabsEl}
  use:scrollRail
>
  <button
    class="tab"
    class:active={isActive(LOCAL)}
    role="tab"
    aria-selected={isActive(LOCAL)}
    onclick={() => onSelect(LOCAL)}
  >
    本机 <span class="count num">{localCount}</span>
  </button>
  <button
    class="tab"
    class:active={isActive(DEVICES)}
    role="tab"
    aria-selected={isActive(DEVICES)}
    onclick={() => onSelect(DEVICES)}
  >
    设备 <span class="count num">{deviceCount}</span>
  </button>
  {#each browsers as device (device.id)}
    {@const tab: TabKey = { kind: 'peer', peerId: device.id }}
    <button
      class="tab peer-tab"
      class:active={isActive(tab)}
      role="tab"
      aria-selected={isActive(tab)}
      title="{device.name} · {device.ip}:{device.port} · 长连接"
      bind:this={peerEls[device.id]}
      onclick={() => onSelect(tab)}
    >
      <span class="dot live"></span>
      {device.name}
      {#if unreadForPeer(device.id) > 0}
        <span class="tab-dot" title={`${unreadForPeer(device.id)} 条未读消息`}></span>
      {/if}
    </button>
  {/each}
</div>