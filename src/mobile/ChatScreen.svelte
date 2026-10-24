<script lang="ts">
  import * as chat from "$lib/stores/chat.svelte";
  import type { ChatItem, Receipt } from "$lib/stores/chat.svelte";
  import * as devices from "$lib/stores/devices.svelte";
  import Icon from "./Icon.svelte";
  import * as store from "./store.svelte";
  import * as api from "$lib/api";
  import {
    adoptDomImages,
    applyComposer,
    imageFiles,
    insertImagePicks,
    insertImages,
    isImageName,
    serializeComposer,
    setCaretEnd,
  } from "$lib/composer";
  import ImagePreview from "$lib/components/ImagePreview.svelte";
  import ImageViewer from "$lib/components/ImageViewer.svelte";
  import TransferRing from "$lib/components/TransferRing.svelte";
  import { untrack } from "svelte";

  let { convoKey }: { convoKey: string } = $props();

  // 外壳只在 `activeThread()` 非空时才挂这个组件（`store.openChatKey()` 就是它），所以这里把
  // 那几个字段拆开用 —— 模板里不用到处判空；键失效时它们落到"空会话"的默认值，界面自己退回列表。
  const thread = $derived(chat.activeThread());
  const mode = $derived(thread?.mode ?? "direct");
  const participants = $derived(thread?.participants ?? []);
  const title = $derived(thread?.title ?? "");
  const items = $derived(chat.itemsOf(convoKey));
  /**
   * 最后一条的 id。吸底只认它：`items` 每次快照都是新数组，读 `.length` 看着像“只在条数变了
   * 才跑”，实际依赖的是数组身份 —— 文件进度、图片路径这些同长度更新也会把滚动拽回底部。
   */
  const tail = $derived(items.length ? items[items.length - 1].id : "");
  /**
   * 别的会话上还等着几条（当前这条不算，见 `chat.unreadElsewhere`）：正停在这条会话里、
   * 别处来了消息时，返回箭头右边那颗角标记的就是它 —— 不开着这条会话时那个数是看不见的
   * （底栏角标与「会话」页签都在这条会话底下盖着）。
   */
  const elsewhere = $derived(chat.unreadElsewhere());

  let logEl = $state<HTMLDivElement | null>(null);
  let inputEl = $state<HTMLDivElement | null>(null);
  /** 正开着大图的那条（点缩略图进去，见 `ImageViewer`；`null` = 没开）。 */
  let viewing = $state<ChatItem | null>(null);
  /** 正在认领输入法剪贴板塞进输入框的图（见 `composer.adoptDomImages`）—— 挡住重入。 */
  let adopting = false;

  /** 输入区里有没有值得发的东西（文字或图，见 `lib/composer.ts`）。 */
  const canSend = $derived(chat.draftHasContent(convoKey));

  const dot = $derived(devices.threadDotState(participants));
  /** 这条会话里的设备是否都验证过：决定标题那颗点实心/空心。 */
  const trusted = $derived(devices.threadTrusted(participants));
  // 这一份查**两份表**（`deviceById`）：设备页列不出来的那几台（离线 + 未绑定 + 没被 mDNS
  // 看见，可会话还在）也得算进来 —— 只查可见那份时这里会变成空表，于是"0 台在线 / 离线 · 消息
  // 不会送达"顶在一台其实刚发过消息的设备上（同一行那颗点已经按后端真实状态画了，两句自相矛盾）。
  const peers = $derived(devices.devicesById(participants));
  /** 有长连接的那几台（"长连接"是这一档唯一的判据）。 */
  const linked = $derived(peers.filter((d) => d.connected).length);
  /**
   * **送得到吗**：与设备页那颗点同一档 —— 在线（探活过的）与经中转都算送得到，
   * 只有"离线・无路"才算送不到（那时消息会排队，见 F-95）。
   *
   * 这里以前只认 `connected`（长连接），于是"经中转够得着、消息真的送得到"的一对一会话
   * 顶着「离线 · 消息不会送达」—— 与同一行那颗绿点自相矛盾，也正是真机上"明明能发消息，
   * 却写着离线"的那一句。
   */
  const reachable = $derived(peers.filter((d) => devices.deviceDotState(d) !== "offline").length);
  /** 原型 `subText`：一对一按链路/可达性写，多台会话写"几台送得到"。 */
  const sub = $derived(
    mode === "direct"
      ? linked > 0
        ? "长连接 · 在线"
        : reachable > 0
          ? "在线"
          : "离线 · 消息不会送达"
      : `${reachable}/${participants.length} 台在线`,
  );

  /** 发言人的设备 id：群消息带 `from`，本机内存里的文件消息带 `peerId`（群里的文件只会是本机发的）。 */
  function speakerOf(item: ChatItem): string {
    if (item.kind === "file") return item.peerId;
    return "from" in item ? item.from : "";
  }

  /**
   * 发言人的显示名（F-101：**备注优先**）：有备注就是备注本身，没有备注才用消息里那个
   * 对端自报的名字（群成员不一定在本机设备表里，`deviceNameOf` 兜底）。
   * 颜色**不**跟着它走（名字可改），见 `speakerIndex`。
   */
  function speakerName(item: ChatItem): string {
    const remark = devices.deviceRemarkOf(speakerOf(item));
    if (remark) return remark;
    if (item.kind === "text" && item.fromName) return item.fromName;
    return chat.deviceNameOf(speakerOf(item));
  }

  /** 一条消息底下那行小字（原型 `msgHTML` 里的 `meta`；`chat.stamp` = 日期 + 时刻）。 */
  function metaOf(item: ChatItem): string {
    const time = chat.stamp(item.at);
    if (item.fromMe) {
      // 群发那条视图里只有"我发出去的"：没有"某台的送达状态"可言，逐台都在下面那张回执卡里。
      if (mode === "broadcast") return time;
      // 群聊是共享的一条消息流，没有"各台各一份副本"那套账 —— 逐台走群自己那行小字。
      const status = mode === "group"
        ? chat.groupReceiptText(item, chat.groupDeclinedIds(thread?.id ?? ""))
        : chat.deliveryText(participants[0], item);
      return [time, status].filter(Boolean).join(" · ");
    }
    // 群聊里发言人已经写在上面的 `.who` 行了，这里只写时刻（原型同款）。
    if (mode === "group") return time;
    return [speakerName(item), time].filter(Boolean).join(" · ");
  }

  /** 文件气泡里那句状态（原型 `fileStatus`）。 */
  function fileStatus(item: ChatItem): string {
    if (item.kind !== "file") return "";
    // 一对一那条视图只写"这一台自己的"状态；群发那条视图写整批（逐台另有回执卡）。
    return chat.fileStatusText(item, mode === "direct" ? participants[0] : "");
  }

  /**
   * 这条文件消息是不是一张**能在会话里摊开看的图**：按扩展名判（与后端 `paste::mime_of`
   * 同一份名单）。文件夹不是；认不出来的扩展名也不是（那种气泡还是老样式）。
   */
  function isImageMsg(item: ChatItem): boolean {
    return item.kind === "file" && !item.isDir && isImageName(item.name);
  }

  /** 一条消息的显示名：文件消息与图片消息都有 `name`，都没有时写「图片」。 */
  function msgName(item: ChatItem): string {
    return item.kind === "file" ? item.name : item.name ?? "图片";
  }

  /**
   * 回执卡第一行：原型是 `已送达 n/m 台 · k 台离线`。
   * 真实的"排队等对端上线"（`waiting`）与"没成"（`offline`）是两档 —— 不单列一档就会漏报，
   * 让人以为剩下那几台都送到了。
   */
  function receiptHead(r: Receipt): string {
    const parts = [`已送达 ${r.delivered}/${r.total} 台`];
    if (r.offline) parts.push(`${r.offline} 台离线`);
    if (r.waiting) parts.push(`${r.waiting} 台等待发送`);
    return parts.join(" · ");
  }

  /**
   * 发出去这一整条。**顺序与分段都在 store 里**（`chat.sendComposer`）：输入区里可能是
   * "一段文字 / 一张图 / 又一段文字"，它按框里的顺序一条条落成消息 —— 图片夹在中间时，
   * 发送方与接收方排出来的顺序是同一个（见那段说明里的"为什么不能让后端各取各的现在"）。
   *
   * 这里只管界面上的两件事：把输入区重画成 store 里剩下的那几段，以及报结果。
   */
  async function send(): Promise<void> {
    try {
      const res = await chat.sendComposer(convoKey);
      const el = inputEl;
      if (el) {
        applyComposer(el, chat.draftSegsOf(convoKey));
        markEmpty();
        el.focus();
        setCaretEnd(el);
      }
      if (!res) return;
      if (res.failed) {
        // **发失败不清草稿**：没发出去的那几段原样还在输入区里，用户不用重打一遍
        // （原型是 mock 不会失败，真发不出去还把刚打的字抹掉，用户得重打一遍）。
        store.toast(`有 ${res.failed} 项没发出去：${res.error ?? "原因不明"}`, "x");
        return;
      }
      if (res.images && !res.texts) store.toast(`已发送 ${res.images} 张图片`, "file");
    } catch (e) {
      store.failed("发送消息", e);
    }
  }

  function onKey(e: KeyboardEvent): void {
    // 中文/日文输入法里那下回车是"上屏"，不是"发送"（原型同款判断）。
    // Shift+回车留给换行（这一版输入区支持多行）。
    if (e.key !== "Enter" || e.shiftKey || e.isComposing) return;
    e.preventDefault();
    void send();
  }

  /** 输入区那句占位文案的判据（`:empty` 认不出"只剩一个空文本节点"的框，见 `mobile.css`）。 */
  function markEmpty(): void {
    const el = inputEl;
    if (el) el.dataset.empty = chat.draftHasContent(convoKey) ? "0" : "1";
  }

  /** 把输入区里此刻的样子存成草稿（每次 `input` 之后、认领完图之后都要叫一次）。 */
  function syncDraft(): void {
    const el = inputEl;
    if (!el) return;
    chat.setDraftSegs(convoKey, serializeComposer(el));
    markEmpty();
  }

  /**
   * `input`：**手机这条路上，图就是从这儿进来的** —— 输入法剪贴板往输入框里插图时不会走
   * `paste` 事件，它是直接往 `contenteditable` 里插一个 `<img>`（`content://` / `data:` /
   * `blob:` 都见过）。先认领（落成暂存文件、换成我们自己那张有路径的图），再同步草稿。
   *
   * 光标位置也在这里照看：认领会替换掉那个节点，替换完把光标放回它后面 —— 不然用户接着打的
   * 字会跑到框首去（图前那一段文字就废了）。
   */
  async function onInput(): Promise<void> {
    const el = inputEl;
    if (!el || adopting) return;
    adopting = true;
    try {
      if (await adoptDomImages(el, (msg) => store.toast(msg, "x"))) setCaretEnd(el);
    } finally {
      adopting = false;
    }
    syncDraft();
  }

  /** `Ctrl+V` / 长按粘贴里带图时（浏览器那条路给的是字节，安卓给的多半是 `<img>`）：两条都认。 */
  function onPaste(e: ClipboardEvent): void {
    const el = inputEl;
    if (!el) return;
    const files = imageFiles(e.clipboardData);
    if (!files.length) return; // 纯文字：交给默认粘贴，`input` 会同步草稿
    e.preventDefault();
    void insertImages(el, files, (msg) => store.toast(msg, "x")).then((ok) => {
      if (ok) syncDraft();
    });
  }

  /**
   * 点一条收完的文件气泡 = 交给系统打开它（`.apk` 落到系统安装器上 —— 后端按扩展名给 MIME，
   * 见 `docs/p1-implementation.md` §20）。
   *
   * 与「文件」页收件行那张操作表里的「打开」是同一个落点（`api.openPath`），只是这里少走一层
   * 表：在会话里点那一下就是想开它。还没有落盘路径的（在传、拒了、失败了）点不动 —— 气泡压根
   * 不是按钮（见模板里那个 `{#if item.path}`）。
   */
  async function openFile(item: ChatItem): Promise<void> {
    // 文件消息与**图片消息**（图片是消息内容那一档）都有 `path`：交给系统打开本机这一份。
    if (!item.path) return;
    try {
      await api.openPath(item.path);
    } catch (e) {
      store.failed("打开", e);
    }
  }

  /**
   * 点自己发出去那条文件气泡右侧的停止圆环：取消这条推送（`store.stopOutgoingFile`）。
   * 界面不用自己改这条消息：随后那条 `push://batch` 会把这一台落成「已取消」。
   */
  async function stopSend(item: ChatItem): Promise<void> {
    if (item.kind !== "file") return;
    await store.stopOutgoingFile(item);
  }

  /**
   * 用户此刻是不是贴着底（阈值 2px 给像素取整留余量）。
   *
   * 图片的缩略图要等 `ImagePreview` 把 `<img>` 挂上才有最终高度 —— 那一下晚于吸底，
   * 只有当时确实在底部才该跟着再贴一次；用户已经翻上去看历史时不许把他拽走。
   */
  let pinned = true;

  /** 滚到最底下。**没吸住之前不许动 `pinned`**：吸底本身触发的那次 `scroll` 会把它算回 true。 */
  function pinBottom(): void {
    const el = logEl;
    if (!el) return;
    el.scrollTop = el.scrollHeight;
    pinned = true;
  }

  /** 用户滚过：只有仍在底部附近才算“还贴着底”。 */
  function onLogScroll(): void {
    const el = logEl;
    if (!el) return;
    pinned = el.scrollHeight - el.clientHeight - el.scrollTop <= 2;
  }

  /** 图片缩略图挂上来了：高度差是这一刻才出现的，当时在底部就补一次吸底。 */
  function onImageLayout(): void {
    if (!pinned) return;
    requestAnimationFrame(() => {
      if (pinned) pinBottom();
    });
  }

  // 来了新消息就贴到底。依赖的是**最后一条的 id**，不是数组长度/数组身份：文件进度、
  // 图片路径这些同长度更新不该把正在翻历史的滚动位置拽走（见 `tail` 的说明）。
  // `convoKey` 也要读 —— 换一条会话时哪怕最后一条一样，也得先贴到底；`logEl` 从 null 变成
  // 节点那一下同理。
  $effect(() => {
    void convoKey;
    void tail;
    if (logEl) pinBottom();
  });

  /**
   * 换会话：把这条会话的草稿**重画**进输入区（图片是 `blob:` 预览，重画就是重新挂上去）。
   *
   * `untrack` 不能省：输入区在编辑时**不归 Svelte 管**（DOM 就是编辑时的真相 —— 否则每敲一个
   * 字都被重画一遍、光标丢到最前）。这个 effect 只认"哪条会话 / 哪个节点"，读草稿那一下包在
   * `untrack` 里。`viewing` 一起收掉：一张已经不属于这条会话的图不该还挂在屏幕上。
   */
  $effect(() => {
    const el = inputEl;
    void convoKey;
    if (!el) return;
    untrack(() => {
      applyComposer(el, chat.draftSegsOf(convoKey));
      markEmpty();
    });
    viewing = null;
  });

  /**
   * 底板上刚选好的那几张图：从 store 里取过来插进输入区（F-118）。
   *
   * **要核对会话键**：选图那一步发生在系统选择器里，用户可能已经换到别的会话去了 —— 那几张图
   * 只属于当时那条会话，键对不上就谁也不插（`takePendingImages` 原封不动地留着，别的会话也
   * 拿不走）。`inputEl` 还没挂上时不取：一取就等于把图从队列里扔了，等它挂上这一条会再跑一次。
   *
   * 插完与粘贴那条路收尾一致：把输入区同步进草稿（否则切会话一回来图就没了），光标放回图后面，
   * 焦点回到输入区 —— 用户接着就能打字配文字。
   */
  $effect(() => {
    const el = inputEl;
    if (!el) return;
    const picks = store.takePendingImages(convoKey);
    if (!picks || picks.length === 0) return;
    void (async () => {
      const ok = await insertImagePicks(el, picks, (msg) => store.toast(msg, "x"));
      if (!ok) return;
      syncDraft();
      el.focus();
      setCaretEnd(el);
    })();
  });
</script>

<!-- 一条文件气泡的内容（图标 / 名字 / 大小 / 状态行）：外面套不套那颗按钮由调用处定
     （有落盘路径才点得开），右侧那颗暂停/停止圆环由调用处另摆 —— 它不能嵌在打开按钮里。
     状态那一行在点得开时补一句"点一下打开" —— 手机上这是唯一的提示，没有悬停可看。 -->
{#snippet fileBub(item: ChatItem & { kind: "file" })}
  <span class="fio"><Icon name="file" /></span>
  <span class="fmain">
    <span class="fname">{item.name}</span>
    <span class="fsize num">{item.size || "文件"}</span>
    {#if item.sending}
      <span class="fstat">传输中 {Math.round(item.pct)}%</span>
    {:else if fileStatus(item)}
      <span class="fstat">{fileStatus(item)}{item.path ? " · 点一下打开" : ""}</span>
    {/if}
  </span>
{/snippet}

<header class="chatbar">
  <button class="icon-btn" type="button" aria-label="返回会话列表" onclick={() => store.navigateBackToList()}>
    <Icon name="back" />
  </button>
  <!-- 返回箭头**右边**：别的会话还等着几条。不摆在返回键本身上 —— 那位置让给角标就得靠悬停
       才能变回返回键，而这里没有悬停（手指没有 hover），唯一的"退回去"会变得不确定；也不并进
       返回键画成一颗挂着数的按钮：那会让"返回"这个动作看起来像"去看那几条消息"。
       点它 = 回会话列表（那几条就在列表里），与返回键同一个落点。

       外面那层 `.cb-navslot` **一直在**（哪怕没有数）：数从无到有的时候不许把设备名顶走 ——
       会话标题是这一屏上唯一告诉用户"这是谁的会话"的东西，每次来条消息就横移一截，比多留一格
       空位难看得多。槽的宽度按**最大值**（`99+` 那颗胶囊）钉死，所以 1 位数、3 位数都不动。 -->
  <span class="cb-navslot">
    {#if elsewhere}
      <button
        class="cb-elsewhere"
        type="button"
        aria-label="还有 {elsewhere} 条来自其他会话的消息，回会话列表"
        onclick={() => store.navigateBackToList()}
      >{elsewhere > 99 ? "99+" : elsewhere}</button>
    {/if}
  </span>
  <div class="cb-main">
    <span class="cb-title">{title}</span>
    <span class="cb-sub"><i class="cv-dot {dot} {trusted ? 'solid' : 'hollow'}"></i>{sub}</span>
  </div>
  <!-- 左边那格槽的**镜像**：会话名是在「左右两格之间的剩余空间」里居中的，左边多出 34u 的话
       名的中心就会比屏幕中心偏右 21u（393 宽下实测）。右边补上同样宽的空格，名的中心才落在
       正中间。空的、也不给读屏看（`aria-hidden`）—— 它只是一格宽度。 -->
  <span class="cb-navslot" aria-hidden="true"></span>
  <button
    class="icon-btn"
    type="button"
    aria-label="会话操作"
    onclick={() => store.openSheet({ kind: "chatActions", key: convoKey })}
  >
    <Icon name="more" cls="ic-fill" />
  </button>
</header>

{#if mode === "broadcast"}
  <div class="chat-banner bc">
    <Icon name="bc" />
    <span>群发（单向）：每条内容都会作为一条独立的一对一消息分别发给 {participants.length} 台设备，这里只显示你发出的部分。</span>
  </div>
{:else if mode === "group"}
  <div class="chat-banner grp">
    <Icon name="group" />
    <span>群聊：{participants.length} 台设备共享此会话，任何设备发送的消息所有人都能看到。</span>
  </div>
{/if}

<div class="chatlog" bind:this={logEl} role="log" aria-label="会话记录" onscroll={onLogScroll}>
  {#if !items.length}
    <div class="empty">
      <Icon name="chat" />
      <p>还没有消息</p>
      <p class="es">发一条消息或传一个文件，开始和设备对话。</p>
    </div>
  {/if}
  {#each items as item (item.id)}
    {@const receipt = mode === "broadcast" && item.fromMe ? chat.receiptOf(item) : null}
    {#if item.kind === "file" && item.inviteState === "pending"}
      <!-- 收进来的推送邀请：接收 / 拒绝要落到后端（`answerInvite`），这张卡才会跟着事件变成
           "接收中 / 已接收"（这条消息的状态是推送事件写的，不是这里自己记的）。 -->
      <div class="invite">
        <div class="inv-head">
          <Icon name="file" />
          <span class="cv-main">
            <span class="fname">{item.name}</span>
            <span class="fmeta">{item.size || "文件"}</span>
          </span>
        </div>
        <!-- 原型写的是 `fromName`（mock 才有），真实那条消息只带发送方设备 id。 -->
        <div class="inv-sub">来自 {chat.deviceNameOf(item.peerId)} · 接收后保存到收件目录</div>
        <div class="inv-acts">
          <button class="btn btn-secondary" type="button" onclick={() => void store.answerInvite(item, false)}>
            拒绝
          </button>
          <button class="btn btn-primary" type="button" onclick={() => void store.answerInvite(item, true)}>
            接收
          </button>
        </div>
      </div>
    {:else}
      <div class="msg" class:me={item.fromMe} class:peer={!item.fromMe} class:bc={mode === "broadcast"}>
        {#if mode === "group" && !item.fromMe}
          <!-- 颜色档按**设备 id**（`speakerIndex` 本来就是按 id 哈希的）：跟着名字走的话，对方改
               一次名颜色就跳，同一个人的两句话会被认成两个人。 -->
          <div class="who sp-{chat.speakerIndex(speakerOf(item)) + 1}">
            <span class="who-av">{speakerName(item).charAt(0)}</span>
            <span class="who-nm">{speakerName(item)}</span>
          </div>
        {/if}
        <div
          class="bubble"
          class:media={!!item.path && (item.kind === "file" ? isImageMsg(item) : !!item.image)}
        >
          {#if item.kind === "file"}
            {@const stoppable = item.fromMe && chat.outgoingStoppable(item, mode === "direct" ? participants[0] : "")}
            {#if item.path && isImageMsg(item)}
              <!-- 一张图：气泡里摊开看它一眼（点开是大图）。读不出来退回原来那条文件行；
                   还在发送时右侧那颗停止圆环留在图外面。 -->
              <span class="fbub-row">
                <ImagePreview
                  path={item.path}
                  label={item.name}
                  flush
                  onopen={() => (viewing = item)}
                  onlayout={onImageLayout}
                >
                  <button class="fbub is-open" type="button" onclick={() => void openFile(item)}>
                    {@render fileBub(item)}
                  </button>
                </ImagePreview>
                {#if stoppable}
                  <span class="fring">
                    <TransferRing
                      mode="send"
                      pct={item.pct}
                      name={item.name}
                      onclick={() => void stopSend(item)}
                    />
                  </span>
                {/if}
              </span>
            {:else if item.path}
              <!-- 收完的那条：整块是一颗按钮，点它交给系统打开（`openFile`）。状态那一行顺带
                   说一句"点一下打开"（手机上只有这一处文字提示 —— 没有悬停可看）。 -->
              <span class="fbub-row">
                <button class="fbub is-open" type="button" onclick={() => void openFile(item)}>
                  {@render fileBub(item)}
                </button>
                {#if stoppable}
                  <span class="fring">
                    <TransferRing
                      mode="send"
                      pct={item.pct}
                      name={item.name}
                      onclick={() => void stopSend(item)}
                    />
                  </span>
                {/if}
              </span>
            {:else}
              <!-- 还没有落盘路径 = 还在传（或已经被拒/失败）。**正在接收**时这一行右端那颗圆环
                   与「浏览」里下载同一套：进行中 = 暂停键，已暂停 = 继续键（点它走
                   `store.toggleReceivePause`）。暂停不丢已收字节与临时文件；暂停之后右端
                   仍然只有这颗圆环，不再多出一颗停止。 -->
              <span class="fbub">
                {@render fileBub(item)}
                {#if item.inviteState === "accepted"}
                  {@const paused = chat.invitePaused(item.inviteId)}
                  <TransferRing
                    mode="receive"
                    status={paused ? "paused" : "running"}
                    pct={item.pct}
                    name={item.name}
                    onclick={() => void store.toggleReceivePause(item)}
                  />
                {/if}
              </span>
            {/if}
          {:else}
            {#if item.image}
              <!-- **图片消息**：图片是消息内容，不是文件 —— 对端直接就能看（没有"接收 / 同意"
                   那一步），气泡里就是那张图，点开看大图。内容不在盘上就退回一行名字。 -->
              {#if item.path}
                <ImagePreview
                  path={item.path}
                  label={item.name ?? "图片"}
                  flush
                  onopen={() => (viewing = item)}
                  onlayout={onImageLayout}
                >
                  <span class="fbub"><span class="fname">{item.name ?? "图片"}</span></span>
                </ImagePreview>
              {:else}
                <span class="ctext">{item.name ?? "图片"}（读不出来）</span>
              {/if}
            {:else}
              <span class="ctext">{item.text}</span>
            {/if}
          {/if}
        </div>
        <span class="meta num">{metaOf(item)}</span>
      </div>
    {/if}
    {#if receipt}
      <div class="receipt">
        <span class="rc-head">{receiptHead(receipt)}</span>
        <span class="rc-list">
          {#each chat.perDeviceOf(item) as per (per.peerId)}
            <span class="rc-dev {per.state}">
              <span class="rc-av">{chat.peerInitial(per.peerId)}</span>
              <span class="rc-nm">{chat.deviceNameOf(per.peerId)}</span>
              <span class="rc-ic"><Icon name={per.state === "ok" ? "check" : "x"} /></span>
            </span>
          {/each}
        </span>
      </div>
    {/if}
  {/each}
</div>

<!-- 输入区是**富文本**（`contenteditable`）：文字与图片在同一条流里，图能夹在前后两段文字中间。
     手机这一端图片是**从输入法的剪贴板进来的**（长按/剪贴板里那张图 → 粘进框里），
     进来的那个 `<img>` 由 `onInput` 认领成一份本机暂存文件（见 `lib/composer.ts`）。
     交给 Svelte 管的只有"哪条会话 / 哪个节点"，框里的内容由 `lib/composer.ts` 直接读写 DOM
     —— 受控的话每敲一个字光标都会被拽回最前。 -->
<form
  class="compose"
  onsubmit={(e) => {
    e.preventDefault();
    void send();
  }}
>
  <div
    class="cinput cx"
    contenteditable="true"
    role="textbox"
    tabindex="0"
    aria-multiline="true"
    spellcheck="false"
    data-empty="1"
    data-placeholder="发送消息…"
    aria-label="消息内容"
    bind:this={inputEl}
    oninput={() => void onInput()}
    onpaste={onPaste}
    onkeydown={onKey}
  ></div>
  <!-- 那颗「＋」（F-118 / F-119）：桌面端它就在输入框与发送键之间（`ConvoPanel.svelte` 的 `chat-plus`），
       安卓端同一个位置、同一个意思 —— 点开一张底板选「图片 / 文件 / 应用」。手机上没有桌面那种自绘的
       本机文件浏览器可用（分区存储不给"按路径列共享存储"，非媒体文件连读都读不到），所以图片与文件借系统的
       照片选择器与文件选择器，选中的东西由原生那一跳拷成暂存文件（见 `PickFilePlugin.kt`）；
       「应用」不用借谁 —— 包管理器直接查得到，那张表由我们自己画（见 `AppPickerSheet.svelte`）。 -->
  <button
    class="cbtn cplus"
    type="button"
    aria-label="发送文件、图片或应用"
    title="选图片（落进输入区）、选文件（当场推送）或发送一个应用"
    onclick={() => store.openSheet({ kind: "attach" })}
  >
    <Icon name="plus" />
  </button>
  <button class="cbtn csend" type="submit" aria-label="发送消息" disabled={!canSend}>
    <Icon name="send" />
  </button>
</form>

<!-- 「看大图」：会话里点一张图的缩略图就在这一层上摊开它（换会话时上面那个 effect 会清掉）。 -->
{#if viewing && viewing.path !== undefined && viewing.path !== ""}
  <ImageViewer
    path={viewing.path}
    name={msgName(viewing)}
    onclose={() => (viewing = null)}
    onopenfile={() => {
      if (viewing) void openFile(viewing);
    }}
  />
{/if}