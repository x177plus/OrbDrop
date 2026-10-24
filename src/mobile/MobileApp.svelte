<script lang="ts">
  import "./mobile.css";
  // 后端事件的移动端登记处（推送/配对这几族在桌面端写在 `App.svelte` 里，移动端原先一条没接，
  // 见 `events.ts` 的说明）。模块级副作用，引一次就够。
  import "./events";
  import { onMount } from "svelte";
  import Icon from "./Icon.svelte";
  import Toasts from "./components/Toasts.svelte";
  import SheetHost from "./SheetHost.svelte";
  import ConvosTab from "./tabs/ConvosTab.svelte";
  import FilesTab from "./tabs/FilesTab.svelte";
  import DevicesTab from "./tabs/DevicesTab.svelte";
  import SettingsTab from "./tabs/SettingsTab.svelte";
  import ChatScreen from "./ChatScreen.svelte";
  import * as store from "./store.svelte";
  import * as chat from "$lib/stores/chat.svelte";
  import * as devices from "$lib/stores/devices.svelte";
  import * as upd from "$lib/stores/update.svelte";
  import type { IconName } from "./icons";

  const tab = $derived(store.currentTab());
  const chatKey = $derived(store.openChatKey());
  const sheet = $derived(store.currentSheet());

  // 「会话」页签那颗角标 = 未读 + 还没确认的群邀请（见 `convoBadgeTotal`）：邀请卡只画在
  // 「会话」页里，不计数的话停在别的页时它就完全没有提示。
  const convoBadge = $derived(chat.convoBadgeTotal());
  const pending = $derived(store.pendingInvites().length);
  const inboxCount = $derived(store.getInbox().length);
  const onlineCount = $derived(devices.getDevices().filter((d) => d.connected).length);
  const deviceCount = $derived(devices.getDevices().length);
  const addr = $derived(store.getLocalAddr());
  /**
   * 「设置」页签的更新角标：有可用新版就挂一颗 `1`。与设置页里那格「版本与更新」同一份状态；
   * 按过「稍后」也照挂（那是"不再自动弹"，不是"没有更新"）。
   */
  const updateBadge = $derived(upd.updateBadge());

  /**
   * 启动时的静默检查发现新版 → **主动弹一张底板**（后端 `check(force = false)`，起来 8 秒后跑，
   * 见 `src-tauri/src/update/mod.rs` 的 `init`/`check`）。移动端没有桌面那组右下角气泡，
   * 这张板就是它的通知位；设置页「版本与更新」那格照旧是同一份状态。
   *
   * **只在没有别的底板时弹**：配对邀请（有效期 60 秒）也占着同一个槽位，那种时候它优先 ——
   * 这张板等它关掉之后自己会补上（`currentSheet` 一变这个 effect 就重跑）。
   * 「稍后」按过之后 `updatePrompt` 就空了，本次运行内不再弹。
   */
  $effect(() => {
    if (upd.updatePrompt() && !store.currentSheet()) store.openSheet({ kind: "update" });
  });

  /** 顶部那一行：标题、副标题、右侧动作 —— 四个 tab 各一份，与原型一致。 */
  const bar = $derived.by((): { title: string; sub: string; act?: { icon: IconName; label: string; run: () => void } } => {
    if (tab === "convos") {
      return {
        title: "会话",
        sub: `${onlineCount} 台在线 · ${deviceCount} 台设备`,
        act: { icon: "plus", label: "新建会话", run: () => store.openSheet({ kind: "convo" }) },
      };
    }
    if (tab === "files") {
      return {
        title: "文件",
        sub: `${pending} 个待接收 · ${inboxCount} 个已接收`,
        act: { icon: "refresh", label: "刷新", run: () => void refreshAll() },
      };
    }
    if (tab === "devices") {
      return {
        title: "设备",
        sub: addr ? `本机 · ${addr.ip || "离线"}:${addr.port}` : "本机",
        act: { icon: "plus", label: "手动添加设备", run: () => store.openSheet({ kind: "addDevice" }) },
      };
    }
    return { title: "设置", sub: "OrbDrop 移动端 · 局域网文件共享" };
  });

  const tabs = [
    { id: "convos", label: "会话", icon: "chat" },
    { id: "files", label: "文件", icon: "files" },
    { id: "devices", label: "设备", icon: "devices" },
    { id: "settings", label: "设置", icon: "settings" },
  ] as const;

  /** 一次把三处数据拉齐（下拉刷新没有：桌面端也没有，靠这颗按钮与后端事件）。 */
  async function refreshAll(): Promise<void> {
    await Promise.all([devices.loadDevices(), chat.refresh(), store.loadInbox()]);
    store.toast("已刷新设备与共享项", "refresh");
  }

  onMount(() => {
    // 路由真相就是 hash：安卓的返回键走 webview 历史栈时，这一条链不用另写一套。
    // 安卓返回键：MainActivity 注册的回调会调 `window.__orbdropBack`（见那个 Kotlin 文件）。
    // 顺序就是"从最上面一层开始收"：底板 → 会话子屏 → 非首层 tab；都没有才返回 false，
    // 让原生那边按平台惯例退出应用。桌面端永远不会调用它（没有那段 Kotlin）。
    (window as unknown as Record<string, unknown>).__orbdropBack = () => {
      if (store.currentSheet()) {
        store.closeSheet();
        return true;
      }
      if (store.openChatKey()) {
        store.navigateBackToList();
        return true;
      }
      if (store.currentTab() !== "convos") {
        store.navigateTab("convos");
        return true;
      }
      return false;
    };

    /**
     * 点**消息通知**进来的落点（`MainActivity.deliverPendingConvo` 调它，见那一段 Kotlin）：
     * 返回 `true` = 进去了，别的地方不用管；`false` = 这条键现在进不去（后端那份 id 对应的
     * 会话不在了），原生那边会隔一会儿再试。
     *
     * 后端只给 `kind` + `id`（键的拼法是这边的事，见 `chat.convoKeyOf`）。先把 tab 拨到「会话」：
     * 从「文件」页点了通知进来，返回键该退回会话列表，而不是退回文件页。
     */
    (window as unknown as Record<string, unknown>).__orbdropOpen = (kind: string, id: string) => {
      const key = chat.convoKeyOf(kind, id);
      if (!key) return false;
      store.navigateTab("convos");
      store.navigateChat(key);
      return true;
    };

    const sync = () => {
      const h = location.hash || "#/convos";
      const chatPrefix = "#/chat/";
      if (h.startsWith(chatPrefix)) {
        let key = h.slice(chatPrefix.length);
        try {
          key = decodeURIComponent(key);
        } catch {
          // 键里的 % 不是转义就走原样：非法键会由 `chat.activeThread` 判空，退回列表。
        }
        chat.openConversation(key);
        return;
      }
      store.setTab(h.replace(/^#\//, ""));
      chat.backToList();
    };
    window.addEventListener("hashchange", sync);
    if (!location.hash) location.hash = "#/convos";
    sync();

    void devices.loadDevices();
    void chat.refresh();
    void store.loadInbox();
    void store.loadLocalAddr();
    // 自更新那份状态：**从启动就订阅**（设置页挂载时也会叫一次，那个是幂等的）——
    // 启动那次静默检查的结论要能落在这张板上，而不是等用户先点进设置页才第一次听说。
    void upd.watchUpdate();
    // 首次运行引导（F-87）：装完那一刻国产 ROM 的「应用启动管理」会在后台把进程冻掉，而通知栏
    // 那条常驻通知照旧挂着（用户看不出毛病）。只问一次，记在配置里 —— 见 `store.offerKeepAliveGuide`。
    void store.offerKeepAliveGuide();
    return () => {
      window.removeEventListener("hashchange", sync);
      delete (window as unknown as Record<string, unknown>).__orbdropBack;
      delete (window as unknown as Record<string, unknown>).__orbdropOpen;
    };
  });
</script>

<div class="app">
  <header class="appbar">
    <div class="ab-main">
      <h1 class="ab-title">{bar.title}</h1>
      <p class="ab-sub">{bar.sub}</p>
    </div>
    {#if bar.act}
      <div class="ab-acts">
        <button class="icon-btn" type="button" aria-label={bar.act.label} onclick={bar.act.run}>
          <Icon name={bar.act.icon} />
        </button>
      </div>
    {/if}
  </header>

  <div class="appmain">
    {#if tab === "convos"}
      <ConvosTab />
    {:else if tab === "files"}
      <FilesTab />
    {:else if tab === "devices"}
      <DevicesTab />
    {:else}
      <SettingsTab />
    {/if}
  </div>

  <!-- 用 div 而不是 nav：`role="tablist"` 会顶掉 `<nav>` 自己的 landmark 语义，屏幕阅读器
       里就少了一个导航地标（a11y 检查也会报）。原型把它写在 `<nav>` 上，这里改掉。 -->
  <div class="tabbar" role="tablist" aria-label="主导航">
    {#each tabs as t (t.id)}
      <button
        class="tab-item"
        class:active={tab === t.id}
        type="button"
        role="tab"
        aria-selected={tab === t.id}
        onclick={() => store.navigateTab(t.id)}
      >
        <span class="ti-ic">
          <Icon name={t.icon} />
          {#if t.id === "convos" && convoBadge > 0}
            <span class="ti-badge">{convoBadge}</span>
          {:else if t.id === "files" && pending > 0}
            <span class="ti-badge hot">{pending}</span>
          {:else if t.id === "settings" && updateBadge > 0}
            <span class="ti-badge">{updateBadge}</span>
          {/if}
        </span>
        <span class="ti-lb">{t.label}</span>
      </button>
    {/each}
  </div>

  <!-- 会话子屏：盖在 tab 之上（原型 `.sub`），开着时底下的列表不再可点。 -->
  <section class="sub" class:open={!!chatKey} aria-hidden={!chatKey} aria-label="会话详情">
    {#if chatKey}
      <ChatScreen convoKey={chatKey} />
    {/if}
  </section>

  {#if sheet}
    <SheetHost {sheet} onClose={() => store.closeSheet()} />
  {/if}

  <Toasts />
</div>