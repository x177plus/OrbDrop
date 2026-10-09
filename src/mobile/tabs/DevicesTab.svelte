<script lang="ts">
  import { onMount } from "svelte";
  import Icon from "../Icon.svelte";
  import StateChip from "../components/StateChip.svelte";
  import * as store from "../store.svelte";
  import * as devices from "$lib/stores/devices.svelte";
  import * as settings from "$lib/stores/settings.svelte";

  // 哪些行该列、按什么序，交给 `visibleDevices`（离线且未绑定的不列：它没有任何可做的动作；
  // 在线在前、同级按名字）。后端给的是"发现的先后"，不是给人看的顺序，界面自己不排。
  const rows = $derived(devices.visibleDevices(devices.getDevices()));
  const addr = $derived(store.getLocalAddr());

  // 本机名字与桌面那份「本机名称」是同一份设置。设备页可能是开机后第一个进的页，
  // 那时还没人拉过设置，缺了就补一次；已经拉到就沿用，不重复请求。
  const name = $derived(settings.getSettings()?.deviceName || "本机");
  onMount(() => {
    if (!settings.getSettings()) void settings.loadSettings();
  });
</script>

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
          <div class="drow" class:connected={d.connected}>
            <span class="dav2">{d.name.charAt(0)}</span>
            <span class="dmain">
              <span class="dname">{d.name}</span>
              <span class="dip num">{d.ip}:{d.port}</span>
              {#if d.relayVia}
                <!-- 这条路不是直连：此刻经这台设备中转（后端自动接管/自动收起）。 -->
                <span class="drelay">经 {d.relayVia} 中转</span>
              {/if}
              <StateChip state={devices.deviceState(d)} />
            </span>
            <span class="dacts">
              <!-- 主按钮三档（原型）+「需配对」一档：token 被对端清掉 / 换了机器之后，
                   长连接会被一直拒（见 `needsRepair`），那时点「连接」是死路，该重新配对换一枚新 token。 -->
              {#if d.connected}
                <button class="btn btn-secondary btn-sm" type="button" onclick={() => void store.toggleConnection(d.id, true)}>断开</button>
              {:else if !d.paired || devices.needsRepair(d)}
                <!-- 目标设备在配对表里选：`pair_start` 要 peerId，而这个 tab 只知道自己按了哪一行。 -->
                <button class="btn btn-primary btn-sm" type="button" onclick={() => store.openSheet({ kind: "pair" })}>配对</button>
              {:else}
                <button class="btn btn-primary btn-sm" type="button" onclick={() => void store.toggleConnection(d.id, false)}>连接</button>
              {/if}
              <button class="icon-btn" type="button" aria-label="设备操作" onclick={() => store.openSheet({ kind: "deviceActions", peerId: d.id })}>
                <Icon name="more" cls="ic-fill" />
              </button>
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