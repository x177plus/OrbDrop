<script lang="ts">
  import LocalTab from './LocalTab.svelte';
  import DeviceTab from './DeviceTab.svelte';
  import PeerTab from './PeerTab.svelte';
  import type { PeerDevice, TabKey } from '$lib/types';

  let {
    tab,
    devices = [],
    onToast,
    onRequestPush,
    onOpenPair,
  }: {
    tab: TabKey;
    /** 设备标签靠它取到 ip/port 与长连接状态。 */
    devices?: PeerDevice[];
    onToast: (msg: string) => void;
    onRequestPush?: (device: PeerDevice) => void;
    onOpenPair?: (device: PeerDevice | null) => void;
  } = $props();

  let peer = $derived(tab.kind === 'peer' ? devices.find((d) => d.id === tab.peerId) : undefined);
</script>

{#if tab.kind === 'local'}
  <LocalTab {onToast} />
{:else if tab.kind === 'devices'}
  <DeviceTab {onToast} {onRequestPush} {onOpenPair} />
{:else if peer}
  {#key peer.id}
    <PeerTab device={peer} {onToast} />
  {/key}
{:else}
  <div class="empty"><p>设备已离线</p><p class="e-sub">对方不在线或已解除配对，切回「设备」标签可以重新连接。</p></div>
{/if}