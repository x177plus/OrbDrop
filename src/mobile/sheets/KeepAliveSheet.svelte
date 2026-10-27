<script lang="ts">
  import { onMount } from "svelte";
  import Sheet from "../components/Sheet.svelte";
  import * as api from "$lib/api";
  import type { KeepAliveState, NotifyState } from "$lib/types";
  import { t } from "$lib/i18n.svelte";
  import * as store from "../store.svelte";

  // 「后台与通知」引导（F-87 + F-108 的横幅那半，机制与真机观察见 `docs/p1-implementation.md` §22）。
  //
  // 这一页只说两件事，都是**我们在应用里做不到、只能引导用户去做**的：
  //  1. 系统那一层（电池优化白名单、ROM 的「应用启动管理 / 自启动」、部分厂商的
  //     「后台耗电管理」）—— 真机实测：这些开关留在默认档时进程会在后台被冻掉，
  //     通知栏那条常驻通知还挂着，而对端看到的是"这台离线了"。部分厂商的后台耗电
  //     管理甚至会把前台服务进程直接写进 `cgroup.freeze`（详见 docs §22）；只开自启动
  //     与白名单挡不住，所以这一页单独有一格。
  //  2. 通知能不能弹横幅 —— 横幅不是权限，是"渠道重要程度 ≥ 高 且 这条通知真的会响/震"；
  //     ROM 自己那层（部分厂商的「静默通知」/「横幅」开关）读不到，只能把用户送到那一页自己看。
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
      store.failed(t("读取后台保活状态"), e);
    }
    try {
      notify = await api.notificationState();
    } catch (e) {
      store.failed(t("读取通知状态"), e);
    }
  }

  /** 渠道重要性的中文档位（后端给的是 `NotificationManager.IMPORTANCE_*`；`-1` = 渠道还没建）。 */
  function importanceText(v: number | undefined): string {
    if (v === undefined) return t("读取中…");
    if (v < 0) return t("还没建（先给通知权限）");
    if (v >= 5) return t("紧急 · 会弹横幅");
    if (v >= 4) return t("高 · 会弹横幅");
    if (v >= 3) return t("默认 · 不弹横幅");
    if (v >= 2) return t("低 · 不弹横幅");
    return t("已关闭");
  }

  async function allowBackground(): Promise<void> {
    try {
      await api.requestIgnoreBatteryOptimizations();
    } catch (e) {
      store.failed(t("打开电池优化设置"), e);
      return;
    }
    store.toast(t("在系统弹窗里点「允许」"), "check");
  }

  async function openAutoStart(): Promise<void> {
    try {
      const page = await api.openAutoStartSettings();
      store.toast(t("已打开「{page}」：把自启动/关联启动/后台活动都打开", { page: t(page.opened) }), "check");
    } catch (e) {
      store.failed(t("打开系统设置"), e);
    }
  }

  async function openBackgroundPower(): Promise<void> {
    try {
      const page = await api.openBackgroundPowerSettings();
      // 没找到厂商那一页时插件退到应用详情：文案别硬说「允许后台高耗电」（各家叫法不同）。
      store.toast(
        page.opened === "应用详情"
          ? t("没找到「后台耗电管理」页，已打开应用详情：请在电池 / 耗电管理里允许后台运行")
          : t("已打开「{page}」：找到 OrbDrop，选择「允许后台运行 / 允许后台高耗电」", { page: t(page.opened) }),
        "check",
      );
    } catch (e) {
      store.failed(t("打开后台耗电设置"), e);
    }
  }

  async function openNotifySettings(): Promise<void> {
    try {
      await api.openNotificationSettings();
    } catch (e) {
      store.failed(t("打开通知设置"), e);
    }
  }

  async function openChannelSettings(): Promise<void> {
    try {
      await api.openMessageChannelSettings();
    } catch (e) {
      store.failed(t("打开通知渠道设置"), e);
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
  title={t("后台与通知")}
  desc={t("系统会不会允许 OrbDrop 在后台一直在线、通知能不能弹出来。")}
  onClose={onClose}
>
  <div class="sec">
    <div class="sec-label"><span>{t("后台保活")}</span></div>
    <div class="card gcard">
      <div class="srow">
        <div class="srow-top" style="justify-content:space-between;">
          <span class="slabel">{t("电池优化白名单")}</span>
          <span
            class="slabel"
            style="flex:none; color:{keepAlive === null
              ? "var(--muted)"
              : keepAlive.batteryOptimizationIgnored
                ? "var(--accent)"
                : "var(--warn)"};"
          >
            {keepAlive === null ? t("读取中…") : keepAlive.batteryOptimizationIgnored ? t("已允许") : t("未允许")}
          </span>
        </div>
        <p class="shelp">
          {t("没在白名单里时，系统在省电/待机状态会掐断应用的后台网络。系统弹窗里点一次「允许」即可。")}
        </p>
        {#if keepAlive && !keepAlive.batteryOptimizationIgnored}
          <button class="btn btn-primary btn-sm" type="button" onclick={() => void allowBackground()}>
            {t("允许后台运行")}
          </button>
        {/if}
      </div>

      <div class="srow">
        <div class="srow-top" style="justify-content:space-between;">
          <span class="slabel">{t("自启动 / 关联启动 / 后台活动")}</span>
        </div>
        <p class="shelp">
          {t("这一层是各手机厂商自己的开关，应用读不到也改不了。到系统设置的「应用启动管理 / 自启动」里，把自启动、关联启动、后台活动都打开。留在默认档时，应用在后台会被冻结 —— 通知栏那条「OrbDrop 正在运行」还挂着，但对端已经看不到你，别人推过来的东西也收不到。")}
        </p>
        <button class="btn btn-secondary btn-sm" type="button" onclick={() => void openAutoStart()}>
          {t("打开系统设置")}
        </button>
      </div>

      <div class="srow">
        <div class="srow-top" style="justify-content:space-between;">
          <span class="slabel">{t("后台耗电管理")}</span>
        </div>
        <p class="shelp">
          {t("部分厂商还有一层不在自启动页上：它会连前台服务一起冻住 —— 熄屏后进程停摆，通知栏还在，对端却看到离线。到系统设置的「电池 / 耗电管理」里找到 OrbDrop，选「允许后台运行」或「允许后台高耗电」（各家叫法不同）。")}
        </p>
        <button class="btn btn-secondary btn-sm" type="button" onclick={() => void openBackgroundPower()}>
          {t("打开后台耗电设置")}
        </button>
      </div>
    </div>
  </div>

  <div class="sec">
    <div class="sec-label"><span>{t("通知")}</span></div>
    <div class="card gcard">
      <div class="srow">
        <div class="srow-top" style="justify-content:space-between;">
          <span class="slabel">{t("允许通知")}</span>
          <span
            class="slabel"
            style="flex:none; color:{notify === null
              ? "var(--muted)"
              : notify.enabled
                ? "var(--accent)"
                : "var(--warn)"};"
          >
            {notify === null ? t("读取中…") : notify.enabled ? t("已允许") : t("未允许")}
          </span>
        </div>
        <p class="shelp">{t("收消息时的提醒。关掉它不影响后台在线，但你不会知道有人找你。")}</p>
        <button class="btn btn-secondary btn-sm" type="button" onclick={() => void openNotifySettings()}>
          {t("通知设置")}
        </button>
      </div>

      <div class="srow">
        <div class="srow-top" style="justify-content:space-between;">
          <span class="slabel">{t("「消息通知」渠道")}</span>
          <span class="slabel" style="flex:none; color:var(--muted);">
            {importanceText(notify?.messageChannelImportance)}
          </span>
        </div>
        <p class="shelp">
          {t("横幅要求这条渠道是\"高\"及以上（只有\"高\"才会在屏幕顶部弹出来）。部分手机厂商还有一层自己的开关会把它按住（如「静默通知」「横幅」），那一层应用读不到 —— 进去看一眼就能确认。")}
        </p>
        <button class="btn btn-secondary btn-sm" type="button" onclick={() => void openChannelSettings()}>
          {t("打开渠道设置")}
        </button>
      </div>
    </div>
  </div>

  {#snippet foot()}
    <button class="btn btn-primary btn-block" type="button" onclick={onClose}>{t("知道了")}</button>
  {/snippet}
</Sheet>