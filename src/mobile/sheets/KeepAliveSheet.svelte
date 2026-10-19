<script lang="ts">
  import { onMount } from "svelte";
  import Sheet from "../components/Sheet.svelte";
  import * as api from "$lib/api";
  import type { KeepAliveState, NotifyState } from "$lib/types";
  import * as store from "../store.svelte";

  // 「后台与通知」引导（F-87 + F-108 的横幅那半，机制与真机观察见 `docs/p1-implementation.md` §22）。
  //
  // 这一页只说两件事，都是**我们在应用里做不到、只能引导用户去做**的：
  //  1. 系统那一层（电池优化白名单、ROM 的「应用启动管理」）—— 真机实测（华为 ABC-AL00）：
  //     那三个开关留在默认的「自动管理」时进程会在后台被冻掉，通知栏那条常驻通知还挂着，
  //     而对端看到的是"这台离线了"。
  //  2. 通知能不能弹横幅 —— 横幅不是权限，是"渠道重要程度 ≥ 高 且 这条通知真的会响/震"；
  //     ROM 自己那层（华为「静默通知」/「横幅」开关）读不到，只能把用户送到那一页自己看。
  //
  // 所以整页的原则是：**能读的读真话**（电池优化白名单、渠道重要性），**读不到的把路指清楚**
  // （跳系统页 + 说开哪几个开关），一句"可能""也许"都不写。
  let { onClose }: { onClose: () => void } = $props();

  let keepAlive = $state<KeepAliveState | null>(null);
  let notify = $state<NotifyState | null>(null);

  /** 读回两层状态。能读的只有这两样（见上），读不到就保持上一次的值。 */
  async function refresh(): Promise<void> {
    try {
      keepAlive = await api.keepAliveState();
    } catch (e) {
      store.failed("读取后台保活状态", e);
    }
    try {
      notify = await api.notificationState();
    } catch (e) {
      store.failed("读取通知状态", e);
    }
  }

  /** 渠道重要性的中文档位（后端给的是 `NotificationManager.IMPORTANCE_*`；`-1` = 渠道还没建）。 */
  function importanceText(v: number | undefined): string {
    if (v === undefined) return "读取中…";
    if (v < 0) return "还没建（先给通知权限）";
    if (v >= 5) return "紧急 · 会弹横幅";
    if (v >= 4) return "高 · 会弹横幅";
    if (v >= 3) return "默认 · 不弹横幅";
    if (v >= 2) return "低 · 不弹横幅";
    return "已关闭";
  }

  async function allowBackground(): Promise<void> {
    try {
      await api.requestIgnoreBatteryOptimizations();
    } catch (e) {
      store.failed("打开电池优化设置", e);
      return;
    }
    store.toast("在系统弹窗里点「允许」", "check");
  }

  async function openAutoStart(): Promise<void> {
    try {
      const page = await api.openAutoStartSettings();
      store.toast(`已打开「${page.opened}」：把自启动/关联启动/后台活动都打开`, "check");
    } catch (e) {
      store.failed("打开系统设置", e);
    }
  }

  async function openNotifySettings(): Promise<void> {
    try {
      await api.openNotificationSettings();
    } catch (e) {
      store.failed("打开通知设置", e);
    }
  }

  async function openChannelSettings(): Promise<void> {
    try {
      await api.openMessageChannelSettings();
    } catch (e) {
      store.failed("打开通知渠道设置", e);
    }
  }

  onMount(() => {
    void refresh();
    // 从系统设置页回来时（WebView 重新可见）状态可能已经变了 —— 重读一次，
    // 用户看到的"已允许"才是他刚才那一下的结果。
    const onVisible = () => {
      if (document.visibilityState === "visible") void refresh();
    };
    document.addEventListener("visibilitychange", onVisible);
    return () => document.removeEventListener("visibilitychange", onVisible);
  });
</script>

<Sheet
  title="后台与通知"
  desc="系统会不会允许 OrbDrop 在后台一直在线、通知能不能弹出来。"
  onClose={onClose}
>
  <div class="sec">
    <div class="sec-label"><span>后台保活</span></div>
    <div class="card gcard">
      <div class="srow">
        <div class="srow-top" style="justify-content:space-between;">
          <span class="slabel">电池优化白名单</span>
          <span
            class="slabel"
            style="flex:none; color:{keepAlive === null
              ? "var(--muted)"
              : keepAlive.batteryOptimizationIgnored
                ? "var(--accent)"
                : "var(--warn)"};"
          >
            {keepAlive === null ? "读取中…" : keepAlive.batteryOptimizationIgnored ? "已允许" : "未允许"}
          </span>
        </div>
        <p class="shelp">
          没在白名单里时，系统在省电/待机状态会掐断应用的后台网络。系统弹窗里点一次「允许」即可。
        </p>
        {#if keepAlive && !keepAlive.batteryOptimizationIgnored}
          <button class="btn btn-primary btn-sm" type="button" onclick={() => void allowBackground()}>
            允许后台运行
          </button>
        {/if}
      </div>

      <div class="srow">
        <div class="srow-top" style="justify-content:space-between;">
          <span class="slabel">自启动 / 关联启动 / 后台活动</span>
        </div>
        <p class="shelp">
          这一层是各手机厂商自己的开关，应用读不到也改不了（华为：「应用启动管理」关掉
          「自动管理」，再把手动管理里那三个开关都打开）。留在默认档时，应用在后台会被冻结 ——
          通知栏那条「OrbDrop 正在运行」还挂着，但对端已经看不到你，别人推过来的东西也收不到。
        </p>
        <button class="btn btn-secondary btn-sm" type="button" onclick={() => void openAutoStart()}>
          打开系统设置
        </button>
      </div>
    </div>
  </div>

  <div class="sec">
    <div class="sec-label"><span>通知</span></div>
    <div class="card gcard">
      <div class="srow">
        <div class="srow-top" style="justify-content:space-between;">
          <span class="slabel">允许通知</span>
          <span
            class="slabel"
            style="flex:none; color:{notify === null
              ? "var(--muted)"
              : notify.enabled
                ? "var(--accent)"
                : "var(--warn)"};"
          >
            {notify === null ? "读取中…" : notify.enabled ? "已允许" : "未允许"}
          </span>
        </div>
        <p class="shelp">收消息时的提醒。关掉它不影响后台在线，但你不会知道有人找你。</p>
        <button class="btn btn-secondary btn-sm" type="button" onclick={() => void openNotifySettings()}>
          通知设置
        </button>
      </div>

      <div class="srow">
        <div class="srow-top" style="justify-content:space-between;">
          <span class="slabel">「消息通知」渠道</span>
          <span class="slabel" style="flex:none; color:var(--muted);">
            {importanceText(notify?.messageChannelImportance)}
          </span>
        </div>
        <p class="shelp">
          横幅要求这条渠道是"高"及以上（只有"高"才会在屏幕顶部弹出来）。部分手机厂商还有一层
          自己的开关会把它按住（华为：「静默通知」要关掉、横幅要打开），那一层应用读不到 ——
          进去看一眼就能确认。
        </p>
        <button class="btn btn-secondary btn-sm" type="button" onclick={() => void openChannelSettings()}>
          打开渠道设置
        </button>
      </div>
    </div>
  </div>

  {#snippet foot()}
    <button class="btn btn-primary btn-block" type="button" onclick={onClose}>知道了</button>
  {/snippet}
</Sheet>