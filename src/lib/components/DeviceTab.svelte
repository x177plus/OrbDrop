<script lang="ts">
  import type { PeerDevice } from '$lib/types';
  import { getDevices, loadDevices } from '$lib/stores/devices.svelte';
  import { connectPeer, disconnectPeer } from '$lib/api';
  import { onMount } from 'svelte';

  let {
    onToast,
    onRequestPush,
    onOpenPair,
  }: {
    onToast: (msg: string) => void;
    onRequestPush?: (device: PeerDevice) => void;
    onOpenPair?: (device: PeerDevice | null) => void;
  } = $props();

  onMount(() => {
    loadDevices().catch(console.error);
  });

  let devices = $derived(getDevices());
  let connectedCount = $derived(devices.filter(d => d.connected).length);
  let devCountStr = $derived(`${devices.length} 台 · ${connectedCount} 长连接`);

  function deviceInitial(name: string): string {
    return name.charAt(0).toUpperCase();
  }

  function stateChipClass(d: PeerDevice): string {
    if (d.connected) return 'state-chip connected';
    if (d.paired) return 'state-chip';
    return 'state-chip unbound';
  }

  function stateLabel(d: PeerDevice): string {
    if (d.connected) return '长连接';
    if (d.paired) return '已配对';
    return '未绑定';
  }

  function stateDotClass(d: PeerDevice): string {
    if (d.connected) return 'dot ok';
    if (d.paired) return 'dot off';
    return 'dot warn';
  }

  /** 手动断开/连接：长连接只由用户点击发起（配对不再自动建链）。 */
  async function setLink(d: PeerDevice, connect: boolean) {
    try {
      if (connect) {
        await connectPeer(d.id);
        onToast(`正在与「${d.name}」建立长连接`);
      } else {
        await disconnectPeer(d.id);
        onToast(`已断开与「${d.name}」的长连接（对端的重连会被拒，直到任一方点「连接」）`);
      }
    } catch (e) {
      onToast(`${connect ? '建立长连接' : '断开'}失败：${e}`);
    }
  }

  function handleDeviceAction(e: Event) {
    const btn = (e.target as HTMLElement).closest('button');
    const row = (e.target as HTMLElement).closest('.device-row') as HTMLElement | null;
    if (!btn || !row) return;
    const device = devices.find(d => d.id === row.dataset.id);
    if (!device) return;
    if (btn.hasAttribute('data-connect')) {
      if (!device.paired) {
        onOpenPair?.(device);
      } else {
        void setLink(device, true);
      }
    } else if (btn.hasAttribute('data-disconnect')) {
      void setLink(device, false);
    } else if (btn.hasAttribute('data-pair')) {
      onOpenPair?.(device);
    } else if (btn.hasAttribute('data-push')) {
      onRequestPush?.(device);
    }
  }

  function openCtx(e: MouseEvent, d: PeerDevice) {
    e.preventDefault();
    const el = (e.target as HTMLElement).closest('.device-row') as HTMLElement | null;
    if (!el) return;
    document.dispatchEvent(new CustomEvent('od:ctx', {
      detail: {
        x: e.clientX, y: e.clientY,
        el, kind: 'device',
        deviceState: d.connected ? 'connected' : d.paired ? 'paired' : 'unbound',
        name: d.name,
        deviceId: d.id,
      }
    }));
  }
</script>

<button class="btn btn-secondary btn-block" id="pairBtn" style="height:var(--control-h)" onclick={() => onOpenPair?.(null)}>
  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round">
    <path d="M12 5v14M5 12h14"/>
  </svg>
  添加设备 · 配对码
</button>

<div class="list-label">
  <span>局域网设备</span>
  <span class="num" id="devCount">{devCountStr}</span>
</div>

{#if devices.length === 0}
  <div class="empty">
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
      <rect x="4" y="3" width="16" height="14" rx="2"/><path d="M8 21h8M12 17v4"/>
    </svg>
    <p>还没有已配对设备</p>
    <p class="e-sub">点击上方「添加设备 · 配对码」，在对方设备的 OrbDrop 中输入本机配对码即可完成配对。</p>
  </div>
{:else}
  <!-- svelte-ignore a11y_click_events_have_key_events -->
  <!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
  <ul class="device-list" id="deviceList" onclick={handleDeviceAction}>
    {#each devices as d (d.id)}
      <li
        class="device-row"
        class:online={d.online}
        data-id={d.id}
        data-state={d.connected ? 'connected' : d.paired ? 'paired' : 'unbound'}
        data-ip={`${d.ip}:${d.port}`}
        oncontextmenu={(e) => openCtx(e, d)}
      >
        <span class="d-avatar">{deviceInitial(d.name)}</span>
        <div class="d-main">
          <div class="d-name">
            {d.name}
            <span class={stateChipClass(d)}>
              <span class={stateDotClass(d)}></span>{stateLabel(d)}
            </span>
            <span class="auto-tag">自动广播</span>
          </div>
          <div class="d-ip">{d.ip}:{d.port}</div>
        </div>
        <div class="d-acts">
          {#if d.connected}
            <button class="btn btn-ghost btn-sm" data-disconnect>断开</button>
            <button class="btn btn-ghost btn-sm" data-push>推送</button>
          {:else if d.paired}
            <!-- 已配对：配对只确立关系，长连接要用户自己点「连接」建立 -->
            <button class="btn btn-primary btn-sm" data-connect>连接</button>
            <button class="btn btn-ghost btn-sm" data-push>推送</button>
          {:else}
            <button class="btn btn-primary btn-sm" data-pair>配对</button>
            <button class="btn btn-ghost btn-sm" data-push>推送</button>
          {/if}
        </div>
      </li>
    {/each}
  </ul>
{/if}

<p style="margin:14px 2px 0; font-size:var(--fs-xs); color:var(--muted); line-height:1.6;">
  <span class="hl">未绑定</span>设备可点击「推送」直接发送（接收方需同意）；
  <span class="hl">已配对</span>设备点「连接」建立长连接（配对本身不建链）；
  处于<span class="hl">长连接</span>的设备会自动收到你拖入悬浮球文件的广播，无需逐条操作。
</p>