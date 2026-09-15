<script lang="ts">
  import type { PeerDevice, TabKey } from '$lib/types';
  import { tabKeyEquals } from '$lib/types';

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
</script>

<div class="tabs" role="tablist">
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
      onclick={() => onSelect(tab)}
    >
      <span class="dot ok"></span>
      {device.name}
    </button>
  {/each}
</div>