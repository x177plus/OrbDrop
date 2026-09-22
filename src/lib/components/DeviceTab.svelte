<script lang="ts">
  import type { PeerDevice } from '$lib/types';
  import { getDevices, loadDevices } from '$lib/stores/devices.svelte';
  import { connectPeer, disconnectPeer } from '$lib/api';
  import Hint from '$lib/components/Hint.svelte';
  import { onMount } from 'svelte';

  let {
    onToast,
    onRequestPush,
    onRequestPushMany,
    onOpenPair,
    onUnpair,
  }: {
    onToast: (msg: string) => void;
    onRequestPush?: (device: PeerDevice) => void;
    /** 多选之后一次推给多台（App 会开**一次**文件选择框，同一份内容分头发出去）。 */
    onRequestPushMany?: (devices: PeerDevice[]) => void;
    onOpenPair?: (device: PeerDevice | null) => void;
    onUnpair?: (device: PeerDevice) => void;
  } = $props();

  /**
   * 多选推送模式。
   *
   * 单台推送是高频路径，「推送」按钮就长在每一行上（点一下直接开选择框）；多台推送是
   * 低频但更重的一次操作，所以它另起一个模式：先在设备页把目标勾齐（**只在这里能勾** ——
   * 设备候选、在线状态、能不能收都只有这个列表知道），再开一次选择框挑文件。
   *
   * 入口是**长按任意可勾的设备行**（见 `onRowPress`），它**只进模式、不预选长按的那一台**，
   * 勾谁由用户接着自己点。这里刻意不摆一颗「多选」按钮：工具条那一行（`.list-label`）
   * 常驻的东西在多选态里一件不多、一件不少（含搜索框），多出/少掉一件带 padding 与 border
   * 的控件就会把那一行从 17.6px 顶到 23.6px，下面的列表整条跳 6px（实测值，
   * 见 main.css 里「设备页多选」那段注释）。
   * 勾选期间每一行的动作按钮收起来（点整行 = 勾/取消），顶部那颗块状按钮原地换成
   * 动作条 —— 同高、不跳版，也不会被右下角那组通知气泡盖住（那块是面板底部）。
   */
  let selectMode = $state(false);
  /** 勾选的设备 id，保持勾选顺序（推送顺序就是用户点的顺序）。 */
  let selected = $state<string[]>([]);

  onMount(() => {
    loadDevices().catch(console.error);
  });

  let devices = $derived(getDevices());

  /**
   * 前端筛选（纯本地，不发请求）：局域网里设备一多，靠滚动找一台太慢。
   * 只匹配**设备名**与 **IP**（大小写不敏感）。空串 / 纯空白 = 不过滤。
   *
   * 只过滤显示，不动数据：勾选集（`selected`）、右键菜单、长按判定都仍按 `devices`
   * 查表，所以筛掉的行不会留下"看不见但已勾上"的错位 —— 已选台数由顶部动作条明示。
   */
  let query = $state('');
  let filtered = $derived.by(() => {
    const q = query.trim().toLowerCase();
    if (!q) return devices;
    return devices.filter((d) => d.name.toLowerCase().includes(q) || d.ip.includes(q));
  });

  let connectedCount = $derived(devices.filter(d => d.connected).length);
  let offlineCount = $derived(devices.filter(d => !d.online).length);
  /** 筛选时换成"匹配 x / 共 y 台"：一眼看出是筛剩这么多，而不是设备真的只有这么几台。 */
  let devCountStr = $derived(
    query.trim()
      ? `匹配 ${filtered.length} / ${devices.length} 台`
      : `${devices.length} 台 · ${connectedCount} 长连接${offlineCount ? ` · ${offlineCount} 离线` : ''}`
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
    // 两种都归这里：token 它不认（清过数据/解除了配对）、证书指纹对不上（它换了机器）。
    // 对用户是同一个动作 —— 重新配对。
    return !!s && s.kind === 'stopped' && (s.reason === 'tokenRejected' || s.reason === 'identityChanged');
  }

  /** 能收推送的才可勾：离线的一台**就地失败、不排队**（F-53），token 失效的一台必然 401。 */
  function selectable(d: PeerDevice): boolean {
    return d.online && !needsRepair(d);
  }

  /**
   * 长按进入多选 —— 这是多选**唯一**的入口。
   *
   * 按住不放 450ms；期间指针挪窝超过 6px 就作废（那是拖动/误触，不是长按）。
   * 长按**只进模式，不替你勾任何一台**：谁要推是用户接下来自己点的。代价是必须吞掉
   * 紧随其后的那一次 click —— 松手时 `selectMode` 已经是 true，整行成了开关，不拦这一下
   * 等于"长按哪台就顺手勾上哪台"。吞掉的标记由**下一次 pointerdown** 复位：那是新一次
   * 手势的开始（长按后松手落在列表外、根本没派发 click 的那种情况，也不会留下哑火）。
   */
  const LONG_PRESS_MS = 450;
  const LONG_PRESS_SLOP = 6;
  /** 按下点（null = 现在没有在计时）。 */
  let pressAt: { x: number; y: number } | null = null;
  let pressTimer: ReturnType<typeof setTimeout> | null = null;
  /** 长按这一下还要吞掉多少次 click（见上面的说明）。 */
  let swallowClick = false;

  function cancelPress() {
    if (pressTimer) clearTimeout(pressTimer);
    pressTimer = null;
    pressAt = null;
  }

  function onRowPress(e: PointerEvent) {
    swallowClick = false;   // 新一次按下 = 上一次手势早已结束，标记作废
    if (selectMode || e.button !== 0) return;
    const row = (e.target as HTMLElement).closest('.device-row') as HTMLElement | null;
    const device = row && devices.find((d) => d.id === row.dataset.id);
    // 不可勾的行不响应长按：从一台"离线/需配对"的设备进多选，只会让人以为它能推。
    if (!device || !selectable(device)) return;
    pressAt = { x: e.clientX, y: e.clientY };
    pressTimer = setTimeout(() => {
      pressTimer = null;
      swallowClick = true;
      selectMode = true;
    }, LONG_PRESS_MS);
  }

  function onRowPressMove(e: PointerEvent) {
    if (!pressAt) return;
    if (Math.abs(e.clientX - pressAt.x) > LONG_PRESS_SLOP || Math.abs(e.clientY - pressAt.y) > LONG_PRESS_SLOP) {
      cancelPress();
    }
  }

  /** 不可勾的原因 —— 置灰的行光看状态 pill 不够，得说清"为什么不能选它"。 */
  function unselectableReason(d: PeerDevice): string {
    if (!d.online) return '「离线」：推送是 HTTP 直达对端，它不在网上只会就地失败（不排队）';
    return '「需配对」：对端不认本机凭据（或它换了机器），推送会被拒 —— 重新配对后再推';
  }

  function toggleSelect(d: PeerDevice) {
    if (!selectable(d)) return;
    selected = selected.includes(d.id) ? selected.filter((id) => id !== d.id) : [...selected, d.id];
  }

  function exitSelect() {
    selectMode = false;
    selected = [];
    swallowClick = false;   // 模式都退出了，没有"下一次 click"要拦
  }

  /** 勾好了 → 把这几台交给下一步（选择框），列表现场归位：一步一件事。 */
  function confirmSelect() {
    const picks = devices.filter((d) => selected.includes(d.id));
    if (!picks.length) return;
    exitSelect();
    onRequestPushMany?.(picks);
  }

  function onKeydown(e: KeyboardEvent) {
    if (!selectMode) return;
    if (e.key === 'Escape') {
      e.stopPropagation();
      exitSelect();
      return;
    }
    if (e.key !== ' ' && e.key !== 'Enter') return;
    // 空格/回车 = 勾选当前聚焦的那台（Esc 退出、Tab 在行间走 —— 都不用鼠标）。
    const row = (e.target as HTMLElement).closest('.device-row') as HTMLElement | null;
    const device = row && devices.find((d) => d.id === row.dataset.id);
    if (!device) return;
    e.preventDefault();
    toggleSelect(device);
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
          case 'tokenRejected':
          case 'identityChanged': return '需配对';
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
          case 'identityChanged': return '对端证书与本机记住的不一致（对方重装过 / 换过机器？）：需要重新配对才连得上';
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
    // 长按进来的那一下：还没到"勾选"这一步，先把它吃掉（否则长按哪台就顺手勾上哪台）。
    if (swallowClick) {
      swallowClick = false;
      return;
    }
    const row = (e.target as HTMLElement).closest('.device-row') as HTMLElement | null;
    if (!row) return;
    const device = devices.find(d => d.id === row.dataset.id);
    if (!device) return;
    // 多选模式下整行就是勾选开关（行里的动作按钮这时已经收起来了）。
    if (selectMode) {
      toggleSelect(device);
      return;
    }
    const btn = (e.target as HTMLElement).closest('button');
    if (!btn) return;
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
    // 勾选期间右键菜单一律不弹：此刻每一行的含义是"勾它/取消它"，弹一份动作菜单
    // （连接/解除配对/推送）只会让人以为多选没生效。
    if (selectMode) return;
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

{#if selectMode}
  <!-- 勾选期间顶部那颗块状按钮原地换成动作条（同高，不跳版；放在顶部是因为底部
       会被右下角那组通知气泡压住）。 -->
  <div class="dev-select-bar" style="height:var(--control-h)">
    <span class="num">已选 <b>{selected.length}</b> 台</span>
    <div class="d-acts">
      <button class="btn btn-secondary btn-sm" onclick={exitSelect}>取消</button>
      <button class="btn btn-primary btn-sm" onclick={confirmSelect} disabled={selected.length === 0}>推送…</button>
    </div>
  </div>
{:else}
  <button class="btn btn-secondary btn-block" id="pairBtn" style="height:var(--control-h)" onclick={() => onOpenPair?.(null)}>
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round">
      <path d="M12 5v14M5 12h14"/>
    </svg>
    添加设备 · 配对码
  </button>
{/if}

<div class="list-label dev-label">
  <span class="ll-name"><span class="ll-text">局域网设备</span><Hint>
    <p>
      <span class="hl">未绑定</span>设备可点击「推送」直接发送（接收方需同意）；
      <span class="hl">已配对</span>设备点「连接」建立长连接（配对本身不建链）；
      处于<span class="hl">长连接</span>的设备会自动收到你拖入悬浮球文件的广播，无需逐条操作；
      关机时连着长连接的设备，下次启动 OrbDrop 时会自动接回来（点过「断开」的不会）。
      <span class="hl">已配对但离线</span>的设备排在列表下方（在线的在上），可以「解除配对」
      —— 解除是本机动作，不需要对方在线。
      <span class="hl">多选</span>推送：长按任意设备进入多选（离线/需配对的行不响应），勾好几台再挑一次文件，
      同一份内容分头发出去，进度与成败都记在右下角那张卡片上。
      <span class="hl">搜索框</span>只在这里筛显示（设备名或 IP），不影响长连接与推送。
    </p>
  </Hint></span>
  {#if devices.length > 0}
    <!-- 纯前端筛选：只改列表显示，不发请求、不碰设备状态。type=search 自带一键清空（✕）。 -->
    <input
      class="dev-search"
      type="search"
      bind:value={query}
      placeholder="搜索名称 / IP"
      aria-label="搜索局域网设备（设备名或 IP）"
      autocomplete="off"
      autocapitalize="off"
      spellcheck="false"
    />
  {/if}
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
{:else if filtered.length === 0}
  <!-- 设备在、只是没匹配上：别摆"还没有已配对设备"那张空态（会像是设备丢了）。 -->
  <div class="empty">
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
      <circle cx="11" cy="11" r="7"/><path d="m20 20-3.5-3.5"/>
    </svg>
    <p>没有匹配「{query.trim()}」的设备</p>
    <p class="e-sub">搜索只匹配设备名与 IP（{devices.length} 台都没命中）。</p>
    <button class="btn btn-secondary btn-sm" onclick={() => (query = '')}>清除搜索</button>
  </div>
{:else}
  <!-- svelte-ignore a11y_click_events_have_key_events -->
  <!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
  <ul
    class="device-list"
    id="deviceList"
    onclick={handleDeviceAction}
    onkeydown={onKeydown}
    onpointerdown={onRowPress}
    onpointermove={onRowPressMove}
    onpointerup={cancelPress}
    onpointercancel={cancelPress}
  >
    {#each filtered as d (d.id)}
      <!-- svelte-ignore a11y_no_noninteractive_tabindex -->
      <li
        class="device-row"
        class:online={d.online}
        class:selecting={selectMode}
        class:selected={selected.includes(d.id)}
        class:locked={selectMode && !selectable(d)}
        data-id={d.id}
        data-state={d.connected ? 'connected' : d.paired ? 'paired' : 'unbound'}
        data-ip={`${d.ip}:${d.port}`}
        role={selectMode ? 'checkbox' : undefined}
        aria-checked={selectMode ? selected.includes(d.id) : undefined}
        aria-disabled={selectMode && !selectable(d) ? 'true' : undefined}
        tabindex={selectMode ? 0 : undefined}
        title={selectMode && !selectable(d) ? unselectableReason(d) : undefined}
        oncontextmenu={(e) => openCtx(e, d)}
      >
        {#if selectMode}
          <!-- 勾选框是画出来的（不是 <input>）：点整行就是开关，多一个真 input 会在
               冒泡里被点两次，勾了又取消。尺寸与文件选择框里的那颗一致（16px），
               但**槽位仍占头像那 34px** —— 换进来/换出去都不能挪动后面的设备名与 ip。 -->
          <span class="d-check" class:on={selected.includes(d.id)} aria-hidden="true">
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4" stroke-linecap="round" stroke-linejoin="round"><path d="m5 12 5 5 9-9"/></svg>
          </span>
        {:else}
          <span class="d-avatar">{deviceInitial(d.name)}</span>
        {/if}
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
              <button class="btn btn-primary btn-sm" data-connect>长连接</button>
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