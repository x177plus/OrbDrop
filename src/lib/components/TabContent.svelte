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
    onAddShare,
    onOpenPair,
    onUnpair,
  }: {
    tab: TabKey;
    /** 设备标签靠它取到 ip/port 与长连接状态。 */
    devices?: PeerDevice[];
    onToast: (msg: string) => void;
    onRequestPush?: (device: PeerDevice) => void;
    /** 本机标签的「添加共享项」：开文件选择框挑本机内容。 */
    onAddShare: () => void;
    onOpenPair?: (device: PeerDevice | null) => void;
    onUnpair?: (device: PeerDevice) => void;
  } = $props();

  // 设备列表里**已配对但离线**的行也在（为了能解除配对），但对端离线时浏览标签没有
  // 意义（树要走长连接）—— 所以这里仍按"在线"筛选，离线时落到下面的空态。
  let peer = $derived(
    tab.kind === 'peer' ? devices.find((d) => d.id === tab.peerId && d.online) : undefined
  );
</script>

{#if tab.kind === 'local'}
  <LocalTab {onToast} {onAddShare} />
{:else if tab.kind === 'devices'}
  <DeviceTab {onToast} {onRequestPush} {onOpenPair} {onUnpair} />
{:else if peer}
  {#key peer.id}
    <PeerTab device={peer} {onToast} />
  {/key}
{:else}
  <div class="empty"><p>设备已离线</p><p class="e-sub">对方不在线或已解除配对，切回「设备」标签可以重新连接。</p></div>
{/if}