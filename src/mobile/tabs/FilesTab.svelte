<script lang="ts">
  import Icon from "../Icon.svelte";
  import FileTile from "../components/FileTile.svelte";
  import DownloadRing from "../components/DownloadRing.svelte";
  import * as store from "../store.svelte";
  import * as chat from "$lib/stores/chat.svelte";
  import * as devices from "$lib/stores/devices.svelte";
  import { fileIcon } from "../util";
  import { t } from "$lib/i18n.svelte";
  import type { BrowseDownload, FilesSeg } from "../store.svelte";
  import type { InboxEntry, PeerShareItem } from "$lib/types";

  const seg = $derived(store.currentSeg());
  const inbox = $derived(store.getInbox());
  const invites = $derived(store.pendingInvites());
  const transfers = $derived(store.activeTransfers());
  const activeDownloads = $derived(store.activeBrowseDownloads());
  /** 「传输」页签数的是**两路**：会话里的推送 + 浏览里正在下的对端文件。 */
  const transferCount = $derived(transfers.length + activeDownloads.length);
  const inboxState = $derived(store.inboxState());

  const SEGS: { id: FilesSeg; label: string }[] = [
    { id: "inbox", label: "收件" },
    { id: "transfer", label: "传输" },
    { id: "browse", label: "浏览" },
  ];

  /** 段角标：收件把「待接收」一起数（它们也是没处理完的文件）；浏览恒 0（原型就是 0）。 */
  function segCount(id: FilesSeg): number {
    if (id === "inbox") return inbox.length + invites.length;
    if (id === "transfer") return transferCount;
    return 0;
  }

  /** 字节 → 短文本（与桌面端 `fmtBytes` 同口径：收件行里只要够短）。 */
  function fmtBytes(n: number): string {
    if (n >= 1024 * 1024 * 1024) return `${(n / 1024 / 1024 / 1024).toFixed(2)} GB`;
    if (n >= 1024 * 1024) return `${(n / 1024 / 1024).toFixed(1)} MB`;
    if (n >= 1024) return `${(n / 1024).toFixed(0)} KB`;
    return `${n} B`;
  }

  /**
   * 收件行的时间：原型那份是 mock 里写死的「今天 10:22」，真数据只给一个 epoch 毫秒，
   * 所以按本地时区现算（今天 / 昨天给短时，更早给月-日）。
   */
  function shortTime(at: number): string {
    const d = new Date(at);
    const now = new Date();
    const hm = `${String(d.getHours()).padStart(2, "0")}:${String(d.getMinutes()).padStart(2, "0")}`;
    if (d.toDateString() === now.toDateString()) return t("今天 {time}", { time: hm });
    const yesterday = new Date(now);
    yesterday.setDate(now.getDate() - 1);
    if (d.toDateString() === yesterday.toDateString()) return t("昨天 {time}", { time: hm });
    return t("{month}月{day}日", { month: d.getMonth() + 1, day: d.getDate() });
  }

  /**
   * 收件行的副行。`from` 在本机自己下载的那些上是空串（后端只在推送落盘时写发送方目录名），
   * 空的那一段就地省掉 —— 原型里这一列永远有值，真数据不一定。
   */
  function inboxMeta(f: InboxEntry): string {
    return [f.from, f.isDir ? t("文件夹") : fmtBytes(f.size), shortTime(f.modified)]
      .filter((s) => s !== "")
      .join(" · ");
  }

  /** 进度条宽度：后端给的是 0–100 的百分数，夹一道免得越界把条画穿。 */
  function pctOf(pct: number): number {
    return Math.min(100, Math.max(0, Math.round(pct)));
  }

  /** 浏览下载的百分比：对端没报大小时不编数字（那一档只显示已收字节 / 「下载中」）。 */
  function downloadPct(d: BrowseDownload): number {
    return d.total > 0 ? pctOf((d.received / d.total) * 100) : 0;
  }

  // ── 浏览段 ────────────────────────────────────────────────────────────────

  // 只有**长连接**设备能浏览（F-25）：对端的共享项只发给与它保持长连接的设备，没建链的
  // 设备根本没有可列的东西。判据与桌面端 `App.svelte` 那个 `browsers` 完全一样 ——
  // 两端说同一句话：芯片/标签在不在，本身就是那一档状态（`connected` 已蕴含 `paired`）。
  const browseDevices = $derived(devices.getDevices().filter((d) => d.connected));
  const browsePeer = $derived(store.getBrowsePeer());
  const shares = $derived(store.browseSharesOf());
  const browseState = $derived(store.browseState());

  // 正在浏览的那台断开长连接（掉线 / 手动断开）后就不在可浏览集合里了：清掉选中，
  // 别让底下列着已经读不到的旧共享项（桌面端同样自动回落，见 App.svelte 那条 $effect）。
  $effect(() => {
    const peer = browsePeer;
    const live = browseDevices;
    if (peer !== null && !live.some((d) => d.id === peer)) store.clearBrowsePeer();
  });

  // 进浏览段时替用户挑第一台：原型 `browseDev` 一开始就指着 d1。不挑的话底下列的是
  // 「对方还没有共享项」—— 看着像对端没共享，其实是本机还没选设备（store 里没选就拉不了）。
  $effect(() => {
    if (seg !== "browse" || browsePeer !== null) return;
    const first = browseDevices[0];
    if (first) void store.selectBrowsePeer(first.id);
  });

  /** 共享项那一行的副行：目录给直接子项数（老版本对端不带这个字段时干脆不写），文件给大小。 */
  function shareMeta(s: PeerShareItem): string {
    if (!s.isDir) return fmtBytes(s.size ?? 0);
    return s.itemCount == null ? "" : t("{n} 项", { n: s.itemCount });
  }

  /**
   * 共享项被点。还没展开过就去拉一层；拉回来是空的（或对端自己报了 0 个子项）就照原型
   * 的 `browseNoop` 给同一句提示 —— 空目录在界面上与"点坏了"长得一样，得有句话。
   */
  function toggleShare(s: PeerShareItem): void {
    const kids = store.browseChildrenOf(s.id);
    if (kids === undefined) {
      if (s.itemCount === 0) {
        store.toast(t("这是一个空目录"));
        return;
      }
      void store.toggleShare(s.id);
      return;
    }
    if (kids.length === 0) {
      store.toast(t("这是一个空目录"));
      return;
    }
    void store.toggleShare(s.id);
  }

  /**
   * 第二层点不动：store 的 `browseChildren` 以共享项 id 为键（一层就够移动端用），
   * 往下再钻没有数据可拿，所以照原型 `browseNoop` 那一档给提示。
   */
  function nestedNoop(): void {
    store.toast(t("这是一个空目录"));
  }

  /** 嵌套行的缩进：原型 `flattenNodes` 是按深度算的（12 + depth * 20）。 */
  function indent(depth: number): string {
    return `padding-left:${12 + depth * 20}px`;
  }
</script>

<div class="seg" role="tablist" aria-label={t("文件视图")}>
  {#each SEGS as s (s.id)}
    <button
      class="seg-item"
      class:active={seg === s.id}
      type="button"
      role="tab"
      aria-selected={seg === s.id}
      onclick={() => store.setSeg(s.id)}
    >
      {t(s.label)}
      {#if segCount(s.id) > 0}<span class="segn num">{segCount(s.id)}</span>{/if}
    </button>
  {/each}
</div>

{#if seg === "inbox"}
  <div class="body">
    {#if invites.length}
      <div class="sec">
        <div class="sec-label"><span>{t("待接收")}</span><span class="num">{t("{n} 个", { n: invites.length })}</span></div>
        <div class="stack">
          {#each invites as inv (inv.msg.id)}
            <!-- 发送方名字取设备名：原型那条 mock 带了 `fromName`，真实的文件消息里只有 peerId。 -->
            <div class="pend">
              <div class="pend-top">
                <span class="pend-chip"><Icon name="download" />{t("待接收")}</span>
                <span class="pend-who">{t("来自 {name}", { name: chat.deviceNameOf(inv.peerId) })}</span>
              </div>
              <div class="pend-body">
                <FileTile name={inv.msg.name} isDir={inv.msg.isDir} />
                <span class="fmain">
                  <span class="fname2">{inv.msg.name}</span>
                  <span class="fmeta">{t("{size} · 保存到收件目录", { size: inv.msg.size || t("文件") })}</span>
                </span>
              </div>
              <div class="pend-acts">
                <button
                  class="btn btn-secondary"
                  type="button"
                  onclick={() => void store.answerInvite(inv.msg, false)}>{t("拒绝")}</button
                >
                <button
                  class="btn btn-primary"
                  type="button"
                  onclick={() => void store.answerInvite(inv.msg, true)}>{t("接收")}</button
                >
              </div>
            </div>
          {/each}
        </div>
      </div>
    {/if}

    <div class="sec">
      <div class="sec-label"><span>{t("已接收文件")}</span><span class="num">{t("{n} 个", { n: inbox.length })}</span></div>
      <div class="card list">
        {#if inboxState.error}
          <!-- 原型这一档没有异步（列表是 mock）；真数据在后端失败时要如实露出原话，不吞。 -->
          <div class="empty">
            <Icon name="x" />
            <p>{t("读取收件目录失败")}</p>
            <p class="es">{inboxState.error}</p>
          </div>
        {:else if inboxState.loading && !inbox.length}
          <div class="empty"><Icon name="inbox" /><p>{t("正在读取收件目录…")}</p></div>
        {:else if !inbox.length}
          <div class="empty">
            <Icon name="inbox" />
            <p>{t("还没有收到的文件")}</p>
            <p class="es">{t("收到推送并同意后，文件会落到收件目录并出现在这里。")}</p>
          </div>
        {:else}
          {#each inbox as f (f.path)}
            <button
              class="frow"
              type="button"
              onclick={() => store.openSheet({ kind: "fileActions", entry: f })}
            >
              <FileTile name={f.name} isDir={f.isDir} />
              <span class="fmain">
                <span class="fname2">{f.name}</span>
                <span class="fmeta">{inboxMeta(f)}</span>
              </span>
              <span class="fgo" aria-hidden="true"><Icon name="more" cls="ic-fill" /></span>
            </button>
          {/each}
        {/if}
      </div>
    </div>
  </div>
{:else if seg === "transfer"}
  <div class="body">
    <div class="sec">
      <div class="sec-label"><span>{t("进行中")}</span><span class="num">{t("{n} 个", { n: transferCount })}</span></div>
      <div class="card list">
        {#if transferCount}
          {#each transfers as item (item.id)}
            <div class="trow">
              <div class="t-top">
                <FileTile name={item.name} isDir={item.isDir} />
                <span class="t-name">{item.name}</span>
                <span class="t-pct num">{pctOf(item.pct)}%</span>
              </div>
              <div class="t-bar"><i style="width:{pctOf(item.pct)}%"></i></div>
              <div class="t-meta">
                <span>
                  {item.dir === "out"
                    ? t("发送至 {name}", { name: item.who })
                    : t("接收自 {name}", { name: item.who })}
                </span>
                <!-- 原型这里按状态给「取消」按钮或「已完成」；取消整批要 batchId（这条路上拿不到，
                     本阶段不做），而 activeTransfers 只给还在动的行，所以右边换 store 写好的状态文案。 -->
                <span class="num">{item.status}</span>
              </div>
            </div>
          {/each}
          <!-- 「浏览」里点下来的对端文件也在这张表里：它们不在会话的消息里，进度由
               `browse://download` 事件推来（`activeBrowseDownloads`）。 -->
          {#each activeDownloads as d (d.key)}
            <div class="trow">
              <div class="t-top">
                <FileTile name={d.name} isDir={false} />
                <span class="t-name">{d.name}</span>
                <span class="t-pct num">{d.total > 0 ? `${downloadPct(d)}%` : ""}</span>
              </div>
              <div class="t-bar" class:indet={d.total <= 0} class:paused={d.status === "paused"}>
                <i style="width:{d.total > 0 ? downloadPct(d) : 35}%"></i>
              </div>
              <div class="t-meta">
                <span>{t("下载自 {name}", { name: devices.peerShortName(d.peerId) })}</span>
                <span class="num">
                  {d.status === "paused" ? t("已暂停") + " · " : ""}{d.total > 0
                    ? `${fmtBytes(d.received)} / ${fmtBytes(d.total)}`
                    : fmtBytes(d.received)}
                </span>
              </div>
            </div>
          {/each}
        {:else}
          <div class="empty">
            <Icon name="files" />
            <p>{t("当前没有进行中的传输")}</p>
            <p class="es">{t("推送或下载文件时会在这里显示进度。")}</p>
          </div>
        {/if}
      </div>
    </div>
    <!-- 原型的「最近完成」分组不画：`activeTransfers()` / `activeBrowseDownloads()` 都只给
         还在动的行，做完的那些这里列不出来。 -->
  </div>
{:else if !browseDevices.length}
  <div class="body">
    <div class="empty">
      <Icon name="devices" />
      <p>{t("没有处于长连接的设备")}</p>
      <p class="es">{t("在「设备」页选一台已配对的 PC 点「连接」，建立长连接后就能浏览它共享的目录。")}</p>
    </div>
  </div>
{:else}
  <div class="body">
    <div class="chips">
      {#each browseDevices as d (d.id)}
        <button
          class="chip"
          class:active={browsePeer === d.id}
          type="button"
          onclick={() => void store.selectBrowsePeer(d.id)}
        >
          <Icon name="devices" />{devices.deviceShortName(d)}
        </button>
      {/each}
    </div>
    <!-- 原型这里的内联 margin 原样搬：`.chips` 下方没有留白，去掉就贴在一起了。 -->
    <div class="card list" style="margin-top:calc(8 * var(--u))">
      {#if browseState.error}
        <div class="empty">
          <Icon name="x" />
          <p>{t("读取共享项失败")}</p>
          <p class="es">{browseState.error}</p>
        </div>
      {:else if browseState.loading || browsePeer === null}
        <!-- 还没选中任何设备时也按"读取中"画：这一帧之后 `$effect` 就会替用户选第一台。 -->
        <div class="empty"><Icon name="folder" /><p>{t("正在读取共享项…")}</p></div>
      {:else if !shares.length}
        <div class="empty">
          <Icon name="folder" />
          <p>{t("对方还没有共享项")}</p>
          <p class="es">{t("对方把目录拖入 OrbDrop 并开启广播后，就会出现在这里。")}</p>
        </div>
      {:else}
        {#each shares as s (s.id)}
          {@const kids = store.browseChildrenOf(s.id)}
          {@const open = !!kids && kids.length > 0}
          {@const dl = store.browseDownloadOf(s.id, "")}
          <div class="noderow" class:has-dl={!s.isDir}>
            <button
              class="node"
              class:open
              type="button"
              style={indent(0)}
              aria-expanded={s.isDir ? open : undefined}
              onclick={() =>
                s.isDir
                  ? toggleShare(s)
                  : void store.activateBrowseEntry(s.id, "", s.name, s.size)}
            >
              <span class="ntile" class:folder={s.isDir}>
                <Icon name={s.isDir ? "folder" : fileIcon(s.name, false)} />
              </span>
              <span class="fmain">
                <span class="fname2">{s.name}</span>
                <span class="fmeta">{shareMeta(s)}</span>
              </span>
              {#if s.isDir}
                <span class="chev" aria-hidden="true"><Icon name="chev" /></span>
              {/if}
            </button>
            {#if !s.isDir}
              <DownloadRing
                status={dl?.status ?? "idle"}
                received={dl?.received ?? 0}
                total={dl?.total ?? 0}
                error={dl?.error ?? null}
                name={s.name}
                onToggle={() => void store.toggleBrowseDownload(s.id, "", s.name, s.size)}
              />
            {/if}
          </div>
          {#if kids && kids.length > 0}
            {#each kids as c (c.name)}
              {@const cdl = store.browseDownloadOf(s.id, c.name)}
              <div class="noderow" class:has-dl={!c.isDir}>
                <button
                  class="node"
                  type="button"
                  style={indent(1)}
                  onclick={() =>
                    c.isDir
                      ? nestedNoop()
                      : void store.activateBrowseEntry(s.id, c.name, c.name, c.size)}
                >
                  <span class="ntile" class:folder={c.isDir}>
                    <Icon name={c.isDir ? "folder" : fileIcon(c.name, false)} />
                  </span>
                  <span class="fmain">
                    <span class="fname2">{c.name}</span>
                    <span class="fmeta">{c.isDir ? "" : fmtBytes(c.size)}</span>
                  </span>
                  {#if c.isDir}
                    <span class="chev" aria-hidden="true"><Icon name="chev" /></span>
                  {/if}
                </button>
                {#if !c.isDir}
                  <DownloadRing
                    status={cdl?.status ?? "idle"}
                    received={cdl?.received ?? 0}
                    total={cdl?.total ?? 0}
                    error={cdl?.error ?? null}
                    name={c.name}
                    onToggle={() => void store.toggleBrowseDownload(s.id, c.name, c.name, c.size)}
                  />
                {/if}
              </div>
            {/each}
          {/if}
        {/each}
      {/if}
    </div>
  </div>
{/if}