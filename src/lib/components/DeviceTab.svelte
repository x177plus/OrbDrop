<script lang="ts">
  import type { PeerDevice } from '$lib/types';
  import { getDevices, loadDevices, deviceState, needsRepair, deviceDisplayName, deviceShortName, unicastBlockedTip } from '$lib/stores/devices.svelte';
  import { connectPeer, disconnectPeer } from '$lib/api';
  import * as chat from '$lib/stores/chat.svelte';
  import Hint from '$lib/components/Hint.svelte';
  import { onMount, tick } from 'svelte';
  import { t } from '$lib/i18n.svelte';

  let {
    onToast,
    onMessage,
    onOpenPair,
    onUnpair,
    noticeCount,
  }: {
    onToast: (msg: string) => void;
    /** 行上那颗「发消息」：打开与这台设备的会话。 */
    onMessage?: (device: PeerDevice) => void;
    onOpenPair?: (device: PeerDevice | null) => void;
    onUnpair?: (device: PeerDevice) => void;
    /** 这台设备此刻还挂着几件待处理的通知（F-112）：由 `App` 从右下角那组气泡里数出来。 */
    noticeCount?: (peerId: string) => number;
  } = $props();

  onMount(() => {
    loadDevices().catch(console.error);
  });

  let devices = $derived(getDevices());

  /**
   * 前端筛选（纯本地，不发请求）：局域网里设备一多，靠滚动找一台太慢。
   * 匹配**显示名**（F-101：设了备注就是「备注(设备名)」，所以备注也搜得到）与 **IP**
   * （大小写不敏感）。空串 / 纯空白 = 不过滤。
   *
   * 只过滤显示，不动数据：右键菜单与动作处理都按 `devices` 查表，筛只是少画几行。
   */
  let query = $state('');
  let filtered = $derived.by(() => {
    const q = query.trim().toLowerCase();
    if (!q) return devices;
    return devices.filter((d) => deviceDisplayName(d).toLowerCase().includes(q) || d.ip.includes(q));
  });

  let connectedCount = $derived(devices.filter(d => d.connected).length);
  let offlineCount = $derived(devices.filter(d => !d.online).length);
  /** 筛选时换成"匹配 x / 共 y 台"：一眼看出是筛剩这么多，而不是设备真的只有这么几台。 */
  let devCountStr = $derived(
    query.trim()
      ? t('匹配 {matched} / {total} 台', { matched: filtered.length, total: devices.length })
      : offlineCount
        ? t('{total} 台 · {connected} 长连接 · {offline} 离线', { total: devices.length, connected: connectedCount, offline: offlineCount })
        : t('{total} 台 · {connected} 长连接', { total: devices.length, connected: connectedCount })
  );

  function deviceInitial(name: string): string {
    return name.charAt(0).toUpperCase();
  }

  /**
   * 头像上那颗角标的悬停 / 读屏文案（数字是两件事加起来的，只说一个数说不清是哪一类：
   * 与底栏「会话」那颗角标同一条规矩）。两半都为空时不会走到这里（角标那时根本不画）。
   */
  function pendingLabel(unread: number, notice: number): string {
    return [
      unread ? t('{n} 条未读消息', { n: unread }) : '',
      notice ? t('{n} 条待处理通知', { n: notice }) : '',
    ].filter(Boolean).join(' · ');
  }

  let listEl = $state<HTMLUListElement | undefined>();

  /* 名字超出就加一档右端渐隐（与标签栏、悬浮框列表同一套）。**只淡名字，不碰状态章**：
     名字与胶囊在同一个 flex 行里，容器上写 `text-overflow` 那种做法会把胶囊一起裁掉 ——
     长连接/已配对那几个字必须完整（那是这一行的结论）。触发点：行集合变（进出/筛选）、
     某个显示名变（改了备注）；首次渲染也要量一遍。 */
  function markClipped() {
    for (const el of listEl?.querySelectorAll<HTMLElement>('.d-name-txt') ?? []) {
      el.classList.toggle('is-clipped', el.scrollWidth > el.clientWidth + 0.5);
    }
  }
  $effect(() => {
    void filtered.map((d) => deviceDisplayName(d)).join('\u0000');
    void tick().then(markClipped);
  });

  /** 地址未知的设备（只有凭据、还没学到过地址）：别显示成 `:0`。 */
  function addressLabel(d: PeerDevice): string {
    return d.ip && d.port ? `${d.ip}:${d.port}` : t('地址未知');
  }

  /**
   * 「新建会话」：选设备、选模式（一对一 / 单向群发 / 共享群聊）、起名字 ——
   * 与会话浮层右上那颗「＋ 新建会话」是同一条路（`stores/chat` 里的收件人选择框）。
   */
  function openNewConvo() {
    chat.askNewConvo();
  }

  /**
   * 悬停说明：这一行现在到底在做什么（排障时唯一能自证的东西）。
   *
   * 胶囊上那几个字（长连接 / 已配对 / 未绑定 / 离线 / 重连中 / 需配对）由 `stores/devices`
   * 的 `deviceState` 给；这里补的是**为什么**与**该做什么**（`waiting` 那类"本机还在等它"
   * 的意思也只有这里说得下）。
   */
  function linkHint(d: PeerDevice): string {
    if (d.paired && !d.verified) {
      return t('没有钉住这台设备的证书指纹（老记录或凭据缺失）：需要重新配对才能验证身份。');
    }
    if (!d.online) {
      if (!d.paired) {
        // 未绑定又离线：只有"看得见"过的设备才留得到这一行（见 `visibleDevices`）——
        // 它只是最近还在这片网上的痕迹。"为什么连不上"（单播被 AP 隔开那类）不写在这里，
        // 那是发起连接时的一句话（`unicastBlockedTip`）。
        return t('对方此刻联系不上（探活不成）。这一行只是它最近还在这片网上的痕迹；它上线后就能「配对」或「发消息」（消息会排队等它上线）。');
      }
      // 「等上线」并进这一档：拨号待命 / 正在退避都只是"本机在等它"，pill 仍是「离线」。
      const waiting = d.linkState && (d.linkState.kind === 'waiting' || d.linkState.kind === 'retrying');
      const tail = waiting
        ? t('本机还在等它：一出现在网上就自动接回长连接。')
        : t('它上线后这一行会回到在线，长连接是否接回仍按本机「连接」记忆走。');
      return t('对方此刻不在网上（探活看不到它）。这一行留在列表里是为了让你能「解除配对」（本机动作，不需要对方在线）；{tail}', { tail });
    }
    const s = d.linkState;
    if (d.connected || !s || s.kind === 'connected') return '';
    switch (s.kind) {
      case 'retrying': return t('正在尝试建立长连接（失败后按 5→60s 退避重试）');
      case 'waiting': return t('对端此刻不在网上：不再空发连接，等它出现（探活/mDNS）或对方自己拨过来');
      case 'stopped':
        switch (s.reason) {
          case 'peerDisconnected': return t('对端点过「断开」：它只是临时暂停这条链路，不是解除配对 —— 自动重拨会被暂时挡住，谁点一次「连接」就恢复');
          case 'tokenRejected': return t('对端不认本机的凭据（它清过数据或已解除配对）：需要重新配对');
          case 'identityChanged': return t('对端证书与本机记住的不一致（对方重装过 / 换过机器？）：需要重新配对才连得上');
          case 'localDisconnected': return t('本机点过「断开」：重启后也不会自动连，点「连接」即可恢复');
          case 'peerRejected': return t('对方拒绝了本机的连接请求：想再试一次就再点一次「连接」（那会重新发一次请求）');
        }
    }
    return '';
  }

  /** 手动断开/连接：长连接只由用户点击发起（配对不再自动建链）。 */
  async function setLink(d: PeerDevice, connect: boolean) {
    try {
      if (connect) {
        // 「连接」= 先向对端发一条**请求**（F-16 的同意制）：对方在它自己的通知里点
        // 「接受」才建链。等待那张卡片由 `link://request_sent` 事件挂上（App 那边），
        // 这里只说清"球踢给对方了"。
        const outcome = await connectPeer(d.id);
        if (outcome.kind === 'asked') {
          onToast(t('已把连接请求发给「{name}」：等它在自己的通知里点「接受」', { name: deviceShortName(d) }));
        } else {
          // 这一台此刻"看得见却直连不通"时，那句解释在这里说 —— 它是"这一次为什么连不上"，
          // 不该做成设备行上的一个常驻状态（见 `unicastBlocked`）。
          onToast(unicastBlockedTip(d) ?? t('正在与「{name}」建立长连接', { name: deviceShortName(d) }));
        }
      } else {
        await disconnectPeer(d.id);
        onToast(t('已与「{name}」断开长连接（临时暂停，不是解除配对）：本机不再自动接回，对方点「连接」可立刻恢复', { name: deviceShortName(d) }));
      }
    } catch (e) {
      onToast(connect ? t('建立长连接失败：{error}', { error: String(e) }) : t('断开失败：{error}', { error: String(e) }));
    }
  }

  function handleDeviceAction(e: Event) {
    const row = (e.target as HTMLElement).closest('.device-row') as HTMLElement | null;
    if (!row) return;
    const device = devices.find(d => d.id === row.dataset.id);
    if (!device) return;
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
    } else if (btn.hasAttribute('data-message')) {
      onMessage?.(device);
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
        needsPair: needsRepair(d) || !d.verified,
        name: deviceDisplayName(d),
        deviceId: d.id,
      }
    }));
  }
</script>

<!-- 行上那颗「发消息」：三档在线状态（长连接 / 已配对 / 未绑定）右下角都是它。
     图标是纸飞机（发出动作用它，与圆圈气泡那类"会话入口"区分开）。 -->
{#snippet messageBtn()}
  <button class="btn btn-ghost btn-sm" data-message>
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M22 2 11 13"/><path d="M22 2 15 22 11 13 2 9 22 2Z"/></svg>
    {t('发消息')}
  </button>
{/snippet}

<button class="btn btn-secondary btn-block" id="pairBtn" style="height:var(--control-h)" onclick={() => onOpenPair?.(null)}>
  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round">
    <path d="M12 5v14M5 12h14"/>
  </svg>
  {t('添加设备 · 配对码')}
</button>
<!-- 新建会话入口：选几台设备、选模式（一对一 / 单向群发 / 共享群聊）、起名字。
     与「设备」页其它按钮同宽同高的一组。 -->
<button class="btn btn-secondary btn-block" style="height:var(--control-h); margin-top:6px" onclick={openNewConvo}>
  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round">
    <path d="M16 19v-1a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v1"/><circle cx="9" cy="7" r="3"/>
    <path d="M22 19v-1a4 4 0 0 0-3-3.87"/><path d="M16 4.13a4 4 0 0 1 0 7.75"/>
  </svg>
  {t('新建会话 · 选择多台设备')}
</button>

<div class="list-label dev-label">
  <span class="ll-name"><span class="ll-text">{t('局域网设备')}</span><Hint>
    <p>
      {t('状态点：实心=已配对且指纹已核对（信任设备），空心=未配对/未验证；颜色：实心绿=长连接（会收到你拖入悬浮球的广播的就是这几台）、蓝=在线（能收到，但不会收到自动广播）、灰=离线、琥珀=本机在等、红=需重新配对（红永远空心：关系失效就不算信任设备）。')}
      {t('胶囊上那几个字说的是关系与链路，与点不重复 —— 行上不再另挂一枚「自动广播」标签，要一眼看出哪几台收得到广播，看状态点的颜色就够（绿只给长连接）。')}
      {t('发消息按钮打开与这台设备的会话 —— 消息长连接优先、HTTP 兜底：已配对的不点「连接」也能收发，未配对设备也能收能发（对端名字是它自报的）；对端不在网上时那条消息排着队，它一上线就自动发出去（界面那一刻写「等待发送」）；已配对设备点「连接」建立长连接（配对本身不建链）；处于长连接的设备会自动收到你拖入悬浮球文件的广播，无需逐条操作；关机时连着长连接的设备，下次启动 OrbDrop 时会自动接回来（点过「断开」的不会）。')}
      {t('已配对但离线的设备排在列表下方（在线的在上），可以「解除配对」—— 解除是本机动作，不需要对方在线。')}
      {t('推送在会话里（输入框左边那颗「＋」）：选文件发过去就是推送 —— 一对一发给这一台；「群发」那条会话里同样有那颗「＋」，一次发给名单上的所有设备（各自邀请、各自进度）。上面的新建会话一次选几台、选模式：单向群发是每台各自收到一条单独消息，共享群聊是所有人的同一条消息流（收到邀请的人点「加入」才算入群）；群那条会话里也有那颗「＋」，按名册逐台推过去。')}
      {t('搜索框只在这里筛显示（显示名或 IP），不影响长连接与消息。')}
      {t('右键设备行可以「修改备注」：备注只存在本机（对端看不到），设好之后这一行显示成「备注(设备名)」—— 其余地方（会话、标签栏、收件人选择框…）只写备注本身；名字太长时悬停能看到完整的。')}
    </p>
  </Hint></span>
  {#if devices.length > 0}
    <!-- 纯前端筛选：只改列表显示，不发请求、不碰设备状态。type=search 自带一键清空（✕）。 -->
    <input
      class="dev-search"
      type="search"
      bind:value={query}
      placeholder={t('搜索名称 / IP')}
      aria-label={t('搜索局域网设备（设备名或 IP）')}
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
    <p>{t('还没有已配对设备')}</p>
    <p class="e-sub">{t('点击上方「添加设备 · 配对码」，在对方设备的 OrbDrop 中输入本机配对码即可完成配对。')}</p>
  </div>
{:else if filtered.length === 0}
  <!-- 设备在、只是没匹配上：别摆"还没有已配对设备"那张空态（会像是设备丢了）。 -->
  <div class="empty">
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
      <circle cx="11" cy="11" r="7"/><path d="m20 20-3.5-3.5"/>
    </svg>
    <p>{t('没有匹配「{query}」的设备', { query: query.trim() })}</p>
    <p class="e-sub">{t('搜索只匹配设备名与 IP（{n} 台都没命中）。', { n: devices.length })}</p>
    <button class="btn btn-secondary btn-sm" onclick={() => (query = '')}>{t('清除搜索')}</button>
  </div>
{:else}
  <!-- svelte-ignore a11y_click_events_have_key_events -->
  <!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
  <ul class="device-list" id="deviceList" bind:this={listEl} onclick={handleDeviceAction}>
    {#each filtered as d (d.id)}
      <!-- 状态点与状态章同一处判据（`stores/devices` 的 `deviceState`）：颜色=链路，填充=是否已验证（实心/空心）。 -->
      {@const st = deviceState(d)}
      <!-- 这台设备"还有几件等你处理"（F-112）：未读消息（会话 store 算好的）＋ 它那边挂着的
           通知（右下角那组气泡里属于它的几条）。两处都有角标了，头像上再来一颗 —— 设备列表
           一眼扫过去就知道该点哪一台，不必先想"它是不是跟我说过话"。 -->
      {@const unread = chat.unreadForPeer(d.id)}
      {@const notice = noticeCount?.(d.id) ?? 0}
      {@const pending = unread + notice}
      <li
        class="device-row"
        class:online={d.online}
        data-id={d.id}
        data-state={d.connected ? 'connected' : d.paired ? 'paired' : 'unbound'}
        data-ip={`${d.ip}:${d.port}`}
        oncontextmenu={(e) => openCtx(e, d)}
      >
        <span class="d-avatar">
          {deviceInitial(deviceDisplayName(d))}
          {#if pending > 0}
            <!-- 一颗数字角标（不是一颗点）：设备多起来时"哪台最要紧"要靠数字读，点在扫视时
                 都一样大。文案把两半分开报（见 `pendingLabel`）。 -->
            <span
              class="d-av-badge num"
              role="img"
              aria-label={t('等你处理 {n} 件：{label}', { n: pending, label: pendingLabel(unread, notice) })}
              title={pendingLabel(unread, notice)}
            >{pending}</span>
          {/if}
        </span>
        <div class="d-main">
          <div class="d-name">
            <!-- 名字长了这一行只淡不截断（右端渐隐）：悬停给全名（这一栏的全名就是「备注(设备名)」）。 -->
            <span class="d-name-txt" title={deviceDisplayName(d)}>{deviceDisplayName(d)}</span>
            <span class="state-chip {st.chip}" title={linkHint(d)}>
              <span class="dot {st.dot} {st.trusted ? 'solid' : 'hollow'}"></span>{st.label}
            </span>
          </div>
          <div class="d-ip">{addressLabel(d)}</div>
        </div>
        <div class="d-acts">
          {#if !d.online && d.paired}
            <!-- 已配对但离线：唯一能做的本机动作是「解除配对」（后端不依赖长连接，见 unpair_peer）。
                 「连接」在这里不提供：对端不在网上，只会开一个空转的拨号循环；
                 「发消息」也不给（同 F-14 的离线行规则）——设备页这一行不是消息的入口，
                 会话列表里与它的那条会话一直在（那里的输入框照样能用，消息会排队等它上线）。
                 **注意这里是"且已配对"**：未绑定的那一档（含"看得见却直连不通"的，F-106）
                 走下面的未绑定那一档 —— 它保留「配对」与「发消息」，但同样不给依赖链路的动作。 -->
            <button class="btn btn-ghost btn-sm" data-unpair>{t('解除配对')}</button>
          {:else if d.connected}
            <button class="btn btn-ghost btn-sm" data-disconnect>{t('断开')}</button>
            {@render messageBtn()}
          {:else if d.paired}
            {#if needsRepair(d)}
              <!-- token 已失效：再点「连接」只会 401，得重新配对换一枚新 token -->
              <button class="btn btn-primary btn-sm" data-pair>{t('配对')}</button>
            {:else}
              <!-- 已配对：配对只确立关系，长连接要用户自己点「连接」建立 -->
              <button class="btn btn-primary btn-sm" data-connect>{t('长连接')}</button>
            {/if}
            {@render messageBtn()}
          {:else}
            <button class="btn btn-primary btn-sm" data-pair>{t('配对')}</button>
            {@render messageBtn()}
          {/if}
        </div>
      </li>
    {/each}
  </ul>
{/if}