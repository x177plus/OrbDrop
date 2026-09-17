<script lang="ts">
  import type { PeerDevice } from '$lib/types';
  import { getDevices, loadDevices } from '$lib/stores/devices.svelte';
  import { connectPeer, disconnectPeer } from '$lib/api';
  import { onMount } from 'svelte';

  let {
    onToast,
    onRequestPush,
    onOpenPair,
    onUnpair,
  }: {
    onToast: (msg: string) => void;
    onRequestPush?: (device: PeerDevice) => void;
    onOpenPair?: (device: PeerDevice | null) => void;
    onUnpair?: (device: PeerDevice) => void;
  } = $props();

  onMount(() => {
    loadDevices().catch(console.error);
  });

  let devices = $derived(getDevices());
  let connectedCount = $derived(devices.filter(d => d.connected).length);
  let offlineCount = $derived(devices.filter(d => !d.online).length);
  let devCountStr = $derived(
    `${devices.length} 台 · ${connectedCount} 长连接${offlineCount ? ` · ${offlineCount} 离线` : ''}`
  );

  function deviceInitial(name: string): string {
    return name.charAt(0).toUpperCase();
  }

  /** 地址未知的设备（只有凭据、还没学到过地址）：别显示成 `:0`。 */
  function addressLabel(d: PeerDevice): string {
    return d.ip && d.port ? `${d.ip}:${d.port}` : '地址未知';
  }

  /**
   * 这台设备是不是"我们的 token 已经不作数了"（对端清过数据 / 它把我们解除配对了）。
   *
   * 这一档**不能给「连接」**：拨号必然 401，`stop_reason_for` 会立刻让循环停手
   * （`LinkState::Stopped{TokenRejected}`），点多少次都一样。唯一出路是重新配对 ——
   * 走一遍配对流程，对端会发一枚新 token，本机存的那枚（和 BC 里那份）跟着更新。
   *
   * 它也是唯一用红的档：本机断开、对端断开都是**有人主动做的正常动作**（界面回到
   * 「已配对」），链路被网络打断是自愈的「重连中」—— 只有"关系已经不成立"才算异常。
   */
  function needsRepair(d: PeerDevice): boolean {
    const s = d.linkState;
    return !!s && s.kind === 'stopped' && s.reason === 'tokenRejected';
  }

  /**
   * 离线优先判定：已配对但对方此刻不在网上（mDNS/探活都看不到）。它留在列表里是为了
   * 「解除配对」——那是纯本机动作；长连接状态（重连中/等对端上线）对一个不在网上的
   * 设备没有意义，所以这一档不看 linkState。
   */
  function stateChipClass(d: PeerDevice): string {
    if (!d.online) return 'state-chip offline';
    if (d.connected) return 'state-chip connected';
    if (d.paired) {
      if (needsRepair(d)) return 'state-chip danger';
      // 已配对：关系正常、随时可用 —— 蓝色（项目主色）。
      return linkChipExtra(d) ? 'state-chip pending' : 'state-chip paired';
    }
    return 'state-chip unbound';
  }

  function stateLabel(d: PeerDevice): string {
    if (!d.online) return '离线';
    if (d.connected) return '长连接';
    if (d.paired) return linkLabel(d) ?? '已配对';
    return '未绑定';
  }

  function stateDotClass(d: PeerDevice): string {
    if (!d.online) return 'dot off';
    if (d.connected) return 'dot ok';
    if (d.paired) {
      if (needsRepair(d)) return 'dot danger';
      return linkChipExtra(d) ? 'dot warn' : 'dot off';
    }
    // 未绑定 = 局域网设备的常态（不配对也能推送），不是告警：灰点，别和"要处理"的黄点抢注意力。
    return 'dot off';
  }

  /**
   * 已配对但没连着的时候，后端会说明"为什么"（重连中 / 已停止）。把它显示出来，用户
   * 才知道该等、该点「连接」、还是该重新配对 —— 静默的失败是最糟的一种。
   *
   * 文案一律 3 个字：完整原因放在悬停说明里（`linkHint`），pill 只负责"一眼分类"。
   *
   * `waiting`（对端不在网上、拨号转入待命）**不单独成档**：它蕴含"对端离线"，pill 归
   * 「离线」（见 stateLabel 的离线优先判定），"本机还在等它上线"这层意思挪进悬停说明。
   */
  function linkLabel(d: PeerDevice): string | null {
    const s = d.linkState;
    // 没有拨号意图、或会话本来就在 → 不用多说，界面上"长连接/已配对"就够了。
    if (!s || s.kind === 'connected') return null;
    switch (s.kind) {
      case 'retrying': return '重连中';
      case 'stopped':
        switch (s.reason) {
          // 本机点过「断开」：**这是用户自己做的事，不是故障** —— 行回到「已配对」（蓝），
          // 原因只留在悬停说明里。否则每次主动断开都像出了事，pill 也就失去信号价值。
          case 'localDisconnected': return null;
          // 对端点过「断开」：**对端主动做的事，同样属于正常断开**（重拨会被它拒绝，
          // 但它随时会拨回来或本机再点「连接」）—— 也回到「已配对」，不是"意外断开"。
          case 'peerDisconnected': return null;
          case 'tokenRejected': return '需配对';
        }
    }
    // waiting 与任何未知 kind 都落到这里：不额外占一档。
    return null;
  }

  function linkChipExtra(d: PeerDevice): boolean {
    return !d.connected && !!linkLabel(d);
  }

  /** 悬停说明：这一行现在到底在做什么（排障时唯一能自证的东西）。 */
  function linkHint(d: PeerDevice): string {
    if (!d.online) {
      // 「等上线」并进这一档：拨号待命 / 正在退避都只是"本机在等它"，pill 仍是「离线」。
      const waiting = d.linkState && (d.linkState.kind === 'waiting' || d.linkState.kind === 'retrying');
      const tail = waiting
        ? '本机还在等它：一出现在网上就自动接回长连接。'
        : '它上线后这一行会回到在线，长连接是否接回仍按本机「连接」记忆走。';
      return `对方此刻不在网上（mDNS 和探活都看不到它）。这一行留在列表里是为了让你能「解除配对」（本机动作，不需要对方在线）；${tail}`;
    }
    const s = d.linkState;
    if (d.connected || !s || s.kind === 'connected') return '';
    switch (s.kind) {
      case 'retrying': return '正在尝试建立长连接（失败后按 5→60s 退避重试）';
      case 'waiting': return '对端此刻不在网上：不再空发连接，等它出现（探活/mDNS）或对方自己拨过来';
      case 'stopped':
        switch (s.reason) {
          case 'peerDisconnected': return '对端点过「断开」：它只是临时暂停这条链路，不是解除配对 —— 自动重拨会被暂时挡住，谁点一次「连接」就恢复';
          case 'tokenRejected': return '对端不认本机的凭据（它清过数据或已解除配对）：需要重新配对';
          case 'localDisconnected': return '本机点过「断开」：重启后也不会自动连，点「连接」即可恢复';
        }
    }
    return '';
  }

  /** 手动断开/连接：长连接只由用户点击发起（配对不再自动建链）。 */
  async function setLink(d: PeerDevice, connect: boolean) {
    try {
      if (connect) {
        await connectPeer(d.id);
        onToast(`正在与「${d.name}」建立长连接`);
      } else {
        await disconnectPeer(d.id);
        onToast(`已与「${d.name}」断开长连接（临时暂停，不是解除配对）：本机不再自动接回，对方点「连接」可立刻恢复`);
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
    if (btn.hasAttribute('data-unpair')) {
      onUnpair?.(device);
    } else if (btn.hasAttribute('data-connect')) {
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
        online: d.online,
        needsPair: needsRepair(d),
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
            <span class={stateChipClass(d)} title={linkHint(d)}>
              <span class={stateDotClass(d)}></span>{stateLabel(d)}
            </span>
            <span class="auto-tag">自动广播</span>
          </div>
          <div class="d-ip">{addressLabel(d)}</div>
        </div>
        <div class="d-acts">
          {#if !d.online}
            <!-- 离线设备唯一能做的本机动作：解除配对（后端不依赖长连接，见 unpair_peer）。
                 「连接」在这里不提供：对端不在网上，只会开一个空转的拨号循环；
                 「推送」也不行——推送邀请是 HTTP 直达对端。 -->
            <button class="btn btn-ghost btn-sm" data-unpair>解除配对</button>
          {:else if d.connected}
            <button class="btn btn-ghost btn-sm" data-disconnect>断开</button>
            <button class="btn btn-ghost btn-sm" data-push>推送</button>
          {:else if d.paired}
            {#if needsRepair(d)}
              <!-- token 已失效：再点「连接」只会 401，得重新配对换一枚新 token -->
              <button class="btn btn-primary btn-sm" data-pair>配对</button>
            {:else}
              <!-- 已配对：配对只确立关系，长连接要用户自己点「连接」建立 -->
              <button class="btn btn-primary btn-sm" data-connect>连接</button>
            {/if}
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
  处于<span class="hl">长连接</span>的设备会自动收到你拖入悬浮球文件的广播，无需逐条操作；
  关机时连着长连接的设备，下次启动 OrbDrop 时会自动接回来（点过「断开」的不会）。
  <span class="hl">已配对但离线</span>的设备排在列表下方（在线的在上），可以「解除配对」
  —— 解除是本机动作，不需要对方在线。
</p>