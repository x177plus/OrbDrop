<script lang="ts">
  import { onMount } from "svelte";
  import Icon from "../Icon.svelte";
  import StateChip from "../components/StateChip.svelte";
  import * as store from "../store.svelte";
  import * as devices from "$lib/stores/devices.svelte";
  import * as settings from "$lib/stores/settings.svelte";
  import * as chat from "$lib/stores/chat.svelte";
  import type { PeerDevice } from "$lib/types";

  // 哪些行该列、按什么序，交给 `visibleDevices`（离线且未绑定的不列：它没有任何可做的动作；
  // 在线在前、同级按名字）。后端给的是"发现的先后"，不是给人看的顺序，界面自己不排。
  const rows = $derived(devices.visibleDevices(devices.getDevices()));
  const addr = $derived(store.getLocalAddr());
  // 本机刚发出去、还在等对方点头的连接请求（F-16 同意制）：有它就在页面上停一档可取消的等待态。
  const linkWait = $derived(store.pendingLinkRequest());
  // 还没处理的推送邀请（会话 store 里那些 `pending` 的文件消息，见 `store.pendingInvites`）：
  // 行上那颗待处理角标（F-112）的一半，另一半是未读消息。
  const invites = $derived(store.pendingInvites());

  /** 这台设备发来的、等本机接收/拒绝的推送有几条。 */
  function invitesOf(peerId: string): number {
    return invites.filter((p) => p.peerId === peerId).length;
  }

  /**
   * 角标的悬停 / 读屏文案：未读消息与待接收邀请是两个数，合成一个就说不清是哪一类
   * （与桌面端 `DeviceTab` 那颗角标同一套说法）。
   */
  function pendingLabel(unread: number, pending: number): string {
    return [
      unread ? `${unread} 条未读消息` : '',
      pending ? `${pending} 条待接收推送` : '',
    ].filter(Boolean).join(" · ");
  }

  // 本机名字与桌面那份「本机名称」是同一份设置。设备页可能是开机后第一个进的页，
  // 那时还没人拉过设置，缺了就补一次；已经拉到就沿用，不重复请求。
  const name = $derived(settings.getSettings()?.deviceName || "本机");
  onMount(() => {
    if (!settings.getSettings()) void settings.loadSettings();
  });

  // ── 长按整行 = 设备操作 ────────────────────────────────────────────────────
  //
  // 与桌面端"右键设备行"是同一件事（都落到 `DeviceActionsSheet` 那张表上）：行内因此不再放
  // 「⋯」按钮 —— 省下来的 44px 连同头像的 54px 一起给名字与地址（98 → 197px，见 §21）。
  //
  // **自己计时，不用 `contextmenu`**：行里全是文字，WebView 的长按会先走到系统的文本选择 /
  // 长按弹窗上去（`mobile.css` 里那两条 `user-select` / `-webkit-touch-callout` 就是为挡它）。
  // 指针挪动超过 slop 就当成在滑列表，撤销这一次按压。
  const LONG_PRESS_MS = 500;
  const LONG_PRESS_SLOP = 8;

  /** 正在按压的那一行（画一层浅底）：按住要有反应，否则这条手势没人会发现。 */
  let pressing = $state<string | null>(null);
  let pressTimer: ReturnType<typeof setTimeout> | null = null;
  let pressFrom = { x: 0, y: 0 };

  function pressStart(e: PointerEvent, peerId: string): void {
    // 行动作那两颗按钮自己的按压不算长按（它们在 `.dacts` 里，各有各的动作）。
    if ((e.target as HTMLElement).closest(".dacts")) return;
    pressFrom = { x: e.clientX, y: e.clientY };
    pressing = peerId;
    pressTimer = setTimeout(() => {
      pressTimer = null;
      pressing = null;
      store.openSheet({ kind: "deviceActions", peerId });
    }, LONG_PRESS_MS);
  }

  function pressMove(e: PointerEvent): void {
    if (!pressTimer) return;
    if (
      Math.abs(e.clientX - pressFrom.x) > LONG_PRESS_SLOP ||
      Math.abs(e.clientY - pressFrom.y) > LONG_PRESS_SLOP
    ) {
      pressEnd();
    }
  }

  function pressEnd(): void {
    if (pressTimer) {
      clearTimeout(pressTimer);
      pressTimer = null;
    }
    pressing = null;
  }
</script>

<!-- 行上那颗「发消息」（F-59）：打开与这台设备的会话 —— 消息**长连接优先、HTTP 兜底**，
     两条都够不着就排队等对端上线（见 F-95）；未绑定的对端也收得下（名字是它自报的）。
     离线行不给它（同 F-14 的离线行规则），所以只在那两档渲染。
     **不带图标**（桌面端那颗是"纸飞机 + 发消息"）：一行里挤着两颗按钮时，纸飞机要占掉 23px，
     而设备名只有一行可显示 —— 名字比图标重要（底板里那一项仍旧带图标）。
     次要样式（`.btn-ghost`）：同一行里主按钮只该有一颗（配对 / 连接 / 断开）。 -->
{#snippet messageBtn(d: PeerDevice)}
  <button class="btn btn-ghost btn-sm" type="button" onclick={() => store.openChat(d.id)}>发消息</button>
{/snippet}

<div class="body">
  <div class="sec">
    <div class="card list">
      <div class="dcard">
        <span class="dav"><Icon name="phone" /></span>
        <span class="dmain">
          <span class="dname">{name}</span>
          <!-- 没有内网地址 = 本机现在谁也找不到（原型写死了一个地址，这里按真实情况说）。 -->
          <span class="dip num">{addr?.ip ? `${addr.ip}:${addr.port}` : "离线"}</span>
        </span>
        <!-- 原型这里用的是 mock 才有那档 `.state-chip.connected`；真样式表只留五档，
             本机自己那档取 `live`（实心绿点 = 有实时链路，与原型那颗点同形）。 -->
        <span class="state-chip live"><i class="dot live"></i>本机</span>
      </div>
    </div>
  </div>

  <div class="sec">
    <button class="btn btn-primary btn-block" type="button" onclick={() => store.openSheet({ kind: "pair" })}>
      <Icon name="link" />配对设备
    </button>
  </div>

  <!-- 本机发出的连接请求还在等对方点头（F-16 同意制）：停在"等谁同意"那一档，可当场取消 ——
       写法与配对表里的「取消邀请」同款（`.opt` + 一颗次级整宽按钮），不另造一套视觉。
       `link://request_result` / `link://request_closed` 回来时这一档自己会收掉。 -->
  {#if linkWait}
    <div class="sec">
      <div class="opt">
        <span class="oi"><Icon name="link" /></span>
        <span class="om">
          <span class="on">等待「{linkWait.peerName}」同意</span>
          <span class="od">连接请求已发出，等它在自己的通知里点「接受」。</span>
        </span>
      </div>
      <button class="btn btn-secondary btn-block" type="button" onclick={() => void store.cancelPendingLinkRequest()}>
        <Icon name="x" />取消请求
      </button>
    </div>
  {/if}

  <div class="sec">
    <div class="sec-label"><span>已发现设备</span><span class="num">{rows.length} 台</span></div>
    {#if rows.length === 0}
      <div class="card list">
        <div class="empty">
          <Icon name="devices" />
          <p>还没有发现设备</p>
          <p class="es">确保两台设备在同一局域网，或手动添加 IP:端口。</p>
        </div>
      </div>
    {:else}
      <div class="card list">
        {#each rows as d (d.id)}
          <!-- 长按整行 = 设备操作（桌面端是右键那一套）：短按不做任何事 —— 行只有信息，
               动作是右边那两颗按钮。`is-pressing` 是按住 500ms 期间的浅底反馈。

               这几个 pointer 处理器不是"这个 div 是颗按钮"：它只在**按住不放**时才做事，
               给不了 `role="button"`（短按什么都不发生，加了就是骗读屏），键盘也没有等价操作 ——
               设备操作的键盘/读屏入口因此**没有**（已知取舍，见 §21 那条记录）。 -->
          <!-- 行上的待处理角标（F-112）：这台设备还有几件等你处理 —— 未读消息（会话 store
               算好的）＋ 还没接收/拒绝的推送邀请。移动端的设备行**没有头像**（§21 把那一格
               还给了名字与地址），所以角标落在名字那一排、状态胶囊的右边：数字紧挨着
               "这台现在怎么样"，读作"它那边还有几件等你"。两行高度不变。 -->
          {@const unread = chat.unreadForPeer(d.id)}
          {@const pending = invitesOf(d.id)}
          <!-- svelte-ignore a11y_no_static_element_interactions -->
          <div
            class="drow"
            class:is-pressing={pressing === d.id}
            onpointerdown={(e) => pressStart(e, d.id)}
            onpointermove={pressMove}
            onpointerup={pressEnd}
            onpointercancel={pressEnd}
            onpointerleave={pressEnd}
            oncontextmenu={(e) => e.preventDefault()}
          >
            <span class="dmain">
              <!-- 名字与状态胶囊**同排**（胶囊跟着名字走）：胶囊原来自己占一整行，还被 grid 拉成
                   整列宽 —— 列一宽就是一条空壳。同排之后行少一行（无中转的行 92 → 71），
                   胶囊也回到自适应宽度；名字长到装不下时先省略号，胶囊**不**被挤掉。 -->
              <span class="dnrow">
                <!-- 名字走**显示名**（F-101：有备注就是「备注(设备名)」）—— 设备列表是"挑哪一台机器"
                     的地方，只写备注认不出是哪台（桌面端 `deviceDisplayName` 只给这一处用）。
                     名字长了这一行省略号截断，`title` 给全（桌面端同款）。 -->
                <span class="dname" title={devices.deviceDisplayName(d)}>{devices.deviceDisplayName(d)}</span>
                <StateChip state={devices.deviceState(d)} />
                {#if unread + pending > 0}
                  <span
                    class="dn-pending num"
                    role="img"
                    aria-label="等你处理 {unread + pending} 件：{pendingLabel(unread, pending)}"
                    title={pendingLabel(unread, pending)}
                  >{unread + pending}</span>
                {/if}
              </span>
              <!-- 第二行 = "这台现在**怎么到的**"：直连写地址；经中转就写「经 X 中转」。
                   中转那一档原来显示的是对方**自报、而这跳已经不通**的地址（写在那一行反而误导 ——
                   用户会以为这就是路），真实地址在 ⋯ 菜单标题与会话页标题旁都有。
                   两行恒定 → 行高恒定 68（原来是 68 / 90 两档）。 -->
              {#if d.relayVia}
                <span class="drelay">经 {d.relayVia} 中转</span>
              {:else}
                <span class="dip num">{d.ip}:{d.port}</span>
              {/if}
            </span>
            <span class="dacts">
              <!-- 行内只放"这一刻能做的事"，与桌面端设备行的档位逐条同规（F-59 / F-14）：
                   在线三档 = 一颗主按钮（断开 / 配对 / 连接）+「发消息」；
                   离线未绑定 = 「配对」+「发消息」（消息排着队等它上线，见 F-95）；
                   离线已配对 = 一颗不放（F-14 那颗「解除配对」走长按菜单，与其它设备操作同路）。 -->
              {#if d.online}
                {#if d.connected}
                  <button class="btn btn-secondary btn-sm" type="button" onclick={() => void store.toggleConnection(d.id, true)}>断开</button>
                {:else if !d.paired || devices.needsRepair(d)}
                  <!-- 目标设备在配对表里选：`pair_start` 要 peerId，而这个 tab 只知道自己按了哪一行。 -->
                  <button class="btn btn-primary btn-sm" type="button" onclick={() => store.openSheet({ kind: "pair" })}>配对</button>
                {:else}
                  <button class="btn btn-primary btn-sm" type="button" onclick={() => void store.toggleConnection(d.id, false)}>连接</button>
                {/if}
                {@render messageBtn(d)}
              {:else if !d.paired}
                <button class="btn btn-primary btn-sm" type="button" onclick={() => store.openSheet({ kind: "pair" })}>配对</button>
                {@render messageBtn(d)}
              {/if}
            </span>
          </div>
        {/each}
      </div>
    {/if}
  </div>

  <div class="sec">
    <button class="btn btn-secondary btn-block" type="button" onclick={() => store.openSheet({ kind: "addDevice" })}>
      <Icon name="plus" />手动添加设备
    </button>
  </div>
</div>