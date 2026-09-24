<script lang="ts">
  import type { PeerDevice, TabKey } from '$lib/types';
  import { tick } from 'svelte';
  import { tabKeyEquals } from '$lib/types';
  import { scrollRail } from '$lib/scrollRail';
  import { unreadForPeer } from '$lib/stores/chat.svelte';
  import { deviceDisplayName } from '$lib/stores/devices.svelte';

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

  let tabsEl = $state<HTMLDivElement | undefined>();
  let peerEls: Record<string, HTMLButtonElement | undefined> = {};

  /* 名字超出标签宽度就加一档右端渐隐（与悬浮框列表的 `.rq-iname` 同一套：不加省略号，
     还看得到后面的字 —— 备注（F-101）把显示名变成「备注(设备名)」之后，一行装不下是常态）。
     触发点两条：标签集合变（长连接进出）、某个显示名变（改了备注）；首次渲染也要量一遍。 */
  function markClipped() {
    for (const el of tabsEl?.querySelectorAll<HTMLElement>('.tab-name') ?? []) {
      el.classList.toggle('is-clipped', el.scrollWidth > el.clientWidth + 0.5);
    }
  }
  $effect(() => {
    void browsers.map((d) => deviceDisplayName(d)).join('\u0000');
    void tick().then(markClipped);
  });

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
      title="{deviceDisplayName(device)} · {device.ip}:{device.port} · 长连接"
      bind:this={peerEls[device.id]}
      onclick={() => onSelect(tab)}
    >
      <!-- 没有状态点：这一栏只给**长连接**设备开（`App.svelte` 的 `browsers`），
           每一条都是绿的，一枚常量点等于没说 —— 标签在不在本身就是那一档状态。
           全名与地址在 `title` 里，页头那颗胶囊（`PeerTab`）也写着「长连接」。 -->
      <span class="tab-name">{deviceDisplayName(device)}</span>
      {#if unreadForPeer(device.id) > 0}
        <span class="tab-dot" title={`${unreadForPeer(device.id)} 条未读消息`}></span>
      {/if}
    </button>
  {/each}
</div>