<script lang="ts">
  import * as chat from '$lib/stores/chat.svelte';
  import type { ChatItem } from '$lib/stores/chat.svelte';
  import { getDevices, deviceDotState, threadDotState, deviceShortName } from '$lib/stores/devices.svelte';
  import { getSettings } from '$lib/stores/settings.svelte';
  import { undeliverableText, type PendingGroup, type Undeliverable } from '$lib/types';
  import {
    adoptDomImages,
    applyComposer,
    imageFiles,
    insertImages,
    isImageName,
    serializeComposer,
    setCaretEnd,
  } from '$lib/composer';
  import ImagePreview from '$lib/components/ImagePreview.svelte';
  import ImageViewer from '$lib/components/ImageViewer.svelte';
  import { listen } from '@tauri-apps/api/event';
  import { onMount, untrack } from 'svelte';

  /**
   * 会话浮层（原型 `.convo-pop`）：两块视图共用一层 —— **会话列表 / 某个会话**。
   *
   * 这里**只管画**：三种会话的记录都在后端（`stores/chat` 拿着那份快照），"谁在跟你说话"在
   * 群聊里看消息自己的 `fromName`（颜色按**设备 id** 哈希，见 `chat.speakerIndex`），其余看
   * 设备表。需要 App 出面的事（开文件选择框、右键菜单）都走回调 —— 会话是面板里的一个视图，
   * 不是第二个 App。
   *
   * 三种身份（一对一 / 单向群发 / 共享群聊）在两块视图里各有一套画法：列表那头像与模式章、
   * 详情头部的模式条、收件人栏、输入区都跟着 `thread.mode` 走（原型的三张身份）。
   */
  let {
    pickedId,
    dropHot,
    onToast,
    onSendFiles,
    onAcceptInvite,
    onRejectInvite,
    onStopReceive,
    onConvoCtx,
    onMessageCtx,
    onOpenFile,
  }: {
    /** 右键菜单正指着的那条消息（高亮它，菜单收掉就清空）。 */
    pickedId: string | null;
    /** 拖进来的文件正落在这块浮层上（F-111）：描一圈主色 + 一句"松手会发生什么"。 */
    dropHot: boolean;
    onToast: (msg: string) => void;
    /** 「＋」：给这条会话的每台选文件发过去（App 开文件选择框，走推送那条路）。 */
    onSendFiles: () => void;
    onAcceptInvite: (inviteId: string) => void;
    onRejectInvite: (inviteId: string) => void;
    /** 正在接收的那颗停止：撤掉这条接收（保留续传记录，但**不再自动接着收** —— 用户喊停就是喊停）。 */
    onStopReceive: (inviteId: string) => void;
    /** 「⋮」：把会话菜单的锚点与**是哪个会话**交出去（菜单由 App 画，项按模式分）。 */
    onConvoCtx: (x: number, y: number, key: string) => void;
    /** 消息右键：坐标 + 这一条要复制的内容（整条 / 选中的那段）。 */
    onMessageCtx: (x: number, y: number, id: string, full: string, sel: string) => void;
    /** 点了一条**文件**消息（自己发的 / 对端推来且已收完的）：App 拿系统默认程序打开它。 */
    onOpenFile: (item: ChatItem) => void;
  } = $props();

  const open = $derived(chat.isChatOpen());
  /** 「每次询问」：接收一张需要同意的卡片时会先弹目录选择框（App 那边执行），文案跟着说清。 */
  const askEachTime = $derived(getSettings()?.downloadMode === 'AskEachTime');
  const thread = $derived(chat.activeThread());
  const rows = $derived(chat.convoRows());
  const devices = $derived(getDevices());
  const items = $derived<ChatItem[]>(thread ? chat.itemsOf(thread.key) : []);
  /** 输入区里有没有值得发的东西（文字或图，见 `lib/composer.ts`）。 */
  const canSend = $derived(thread ? chat.draftHasContent(thread.key) : false);
  /** 最后一条的 id：新消息进来就跳到底（按 id 而不是长度，编辑/替换也算变化）。 */
  const tail = $derived(items.length ? items[items.length - 1].id : '');
  const openDev = $derived(thread?.mode === 'direct' ? devices.find((d) => d.id === thread.participants[0]) : undefined);
  /**
   * 这个群里**明确拒绝加入**的那些成员（后端 `Group::declined`）：收件人那一排把它们染红，
   * 消息那行小字也改口说「已拒绝加入」。不是群 = 空表。
   */
  const declinedIds = $derived(thread?.mode === 'group' ? chat.groupDeclinedIds(thread.id) : []);
  /**
   * 一对一那条会话对着哪台设备（群发/群聊是空串）。
   *
   * 一对一那一屏里，"这条到没到"只说**这一台**：群发过去的副本同样躺在这条会话里
   * （群发是每台各存一份），但"几台送到了"是群发那条视图的事（`deliveryText` / `fileStatusText`）。
   */
  const directPeer = $derived(thread?.mode === 'direct' ? thread.participants[0] : '');

  /**
   * 这条群聊**已知缺**几条（后端算好，见 `ChatGroup.missing`）：> 0 就在消息流头顶如实说一句。
   * 只认本地已知的那个群（`groupOf`）：这条键指向的群要是已经不在了，就没得可缺。
   */
  const missing = $derived(thread?.mode === 'group' ? (chat.groupOf(thread.id)?.missing ?? 0) : 0);

  /**
   * 别的会话上还等着几条（当前这条不算，见 `chat.unreadElsewhere`）。
   *
   * 会话详情里**别处来消息原本没有任何提示**：底栏那颗角标在这一屏底下盖着、会话列表那条行
   * 也在浮层的另一屏里 —— 得退回去才知道。返回箭头右边这颗数就是它。
   */
  const elsewhere = $derived(chat.unreadElsewhere());

  let logEl = $state<HTMLDivElement | null>(null);
  let inputEl = $state<HTMLDivElement | null>(null);
  /** 正开着大图的那条（点缩略图进去，见 `ImageViewer`；`null` = 没开）。 */
  let viewing = $state<ChatItem | null>(null);

  /**
   * 正在认领浏览器塞进输入区的图（见 `composer.adoptDomImages`）：认领会改 DOM、而改 DOM 又会
   * 走进来的那次 `input`，用这个闸门挡住重入。
   */
  let adopting = false;

  /** 新消息进来、换会话、浮层重新展开 —— 都落到最底下（原型 `renderChatLog` 结尾那次滚动）。 */
  $effect(() => {
    void tail;
    void thread;
    void open;
    if (!open || !logEl) return;
    logEl.scrollTop = logEl.scrollHeight;
  });

  /**
   * 打开某个会话：把这条会话的草稿**重画**进输入区，光标放到末尾。
   *
   * `untrack` 不能省：输入区在编辑时**不归 Svelte 管**（DOM 就是编辑时的真相，否则每敲一个字
   * 都被重画一遍、光标丢到最前）。这个 effect 因此只认"哪条会话 / 哪个节点"，
   * 而**不能**跟着草稿的每一次变化跑 —— 读草稿那一下包在 `untrack` 里。
   */
  $effect(() => {
    const el = inputEl;
    const key = thread?.key ?? '';
    if (!el || !key) return;
    untrack(() => {
      applyComposer(el, chat.draftSegsOf(key));
      markEmpty();
    });
    // 换会话 / 关掉浮层：大图跟着收掉（它不是"第二个窗口"，只是这一条会话里的一次摊开）。
    viewing = null;
    el.focus();
    if (open) setCaretEnd(el);
  });

  /** 输入区那句占位文案的判据（`:empty` 认不出"只剩一个空文本节点"的框，见 `main.css`）。 */
  function markEmpty() {
    const el = inputEl;
    const t = thread;
    if (el && t) el.dataset.empty = chat.draftHasContent(t.key) ? '0' : '1';
  }

  /** 把输入区里此刻的样子存成草稿（每次 `input` 之后、认领完图之后都要叫一次）。 */
  function syncDraft() {
    const el = inputEl;
    const t = thread;
    if (!el || !t) return;
    chat.setDraftSegs(t.key, serializeComposer(el));
    markEmpty();
  }

  /**
   * `input` 事件：输入法剪贴板那条路（安卓那半靠的就是它）会把图**直接插进框里**，
   * 插进来的多半是一个 `<img>`（`data:` / `blob:` / `content://`）—— 先认领（落成暂存文件、
   * 换成我们自己那张有路径的图），再同步草稿。桌面端 `paste` 事件那条路见 `onPaste`。
   */
  async function onInput() {
    const el = inputEl;
    if (!el || adopting) return;
    adopting = true;
    try {
      await adoptDomImages(el, onToast);
    } finally {
      adopting = false;
    }
    syncDraft();
  }

  /** `Ctrl+V` 里带图：那几张图落盘后插到**光标处**（见 `composer.insertImages`）。 */
  function onPaste(e: ClipboardEvent) {
    const el = inputEl;
    if (!el) return;
    const files = imageFiles(e.clipboardData);
    if (!files.length) return; // 纯文字：交给默认粘贴，`input` 事件会把草稿同步过去
    e.preventDefault();
    void insertImages(el, files, onToast).then((ok) => {
      if (ok) syncDraft();
    });
  }

  /**
   * 一条消息暂时送不出去时那句解释（F-106，`chat://undeliverable`）：一行小字，可关闭、
   * 约 8s 自己消失。**不做成卡片、也不进右下角通知组** —— 它说的是"刚才那条为什么没反应"
   * （消息已经在排队，不丢），不是一件要用户处理的待办，摆着反而占地方。
   *
   * 新的一条顶掉旧的并重置计时：连着发几条时，用户要看到的是最新那一台/那个原因。
   */
  let undeliverable = $state<{ id: number; text: string } | null>(null);
  let undeliverableSeq = 0;
  let undeliverableTimer: ReturnType<typeof setTimeout> | undefined;

  function showUndeliverable(text: string) {
    const id = ++undeliverableSeq;
    undeliverable = { id, text };
    clearTimeout(undeliverableTimer);
    undeliverableTimer = setTimeout(() => {
      if (undeliverable?.id === id) undeliverable = null;
    }, 8000);
  }

  function dismissUndeliverable() {
    clearTimeout(undeliverableTimer);
    undeliverable = null;
  }

  onMount(() => {
    let off: (() => void) | undefined;
    void listen<Undeliverable>('chat://undeliverable', (ev) => {
      showUndeliverable(undeliverableText(ev.payload));
    }).then((f) => (off = f));
    return () => {
      clearTimeout(undeliverableTimer);
      off?.();
    };
  });

  /**
   * 这一条要不要在上头插一行发言人（群聊里他人发的、且与上一条不是同一个人说的）。
   *
   * 同一个人连着说几句只在头一条上面标一次（原型也是这么分组的，一屏里就不会满眼都是名字）。
   * 比的是**设备 id**：名字会变，拿名字比会把改过名的同一个人当成两个人。
   */
  function speakerOf(i: number): { cls: string; initial: string; name: string } | null {
    if (thread?.mode !== 'group') return null;
    const item = items[i];
    if (!item || item.fromMe || item.kind !== 'text' || !('from' in item)) return null;
    const prev = items[i - 1];
    if (prev && !prev.fromMe && prev.kind === 'text' && 'from' in prev && prev.from === item.from) return null;
    const name = item.fromName.trim() || chat.deviceNameOf(item.from);
    return { cls: `sp-${chat.speakerIndex(item.from)}`, initial: (name.charAt(0) || '?').toUpperCase(), name };
  }

  /**
   * 群邀请卡的「加入 / 不加入」：加入之后直接把这条群聊打开（点它的人就是要进去说话）；
   * 拒绝只是别再弹这张卡（本地记下"拒绝过"）。
   */
  function answerInvite(pg: PendingGroup, join: boolean) {
    void (join ? chat.joinGroup(pg.id) : chat.dismissGroup(pg.id)).catch((e) => onToast(String(e)));
  }

  const ICON_CHAT = 'M20 12a8 8 0 0 1-8 8H6.5L3 22V12a8 8 0 0 1 8-8h1a8 8 0 0 1 8 8Z M8.5 11h7 M8.5 14.5h4';
  const ICON_FILE = 'M6 3h8l5 5v13H6V3Z';
  const ICON_FOLDER = 'M3 7a2 2 0 0 1 2-2h4.2a2 2 0 0 1 1.4.6L12 7h7a2 2 0 0 1 2 2v9a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V7Z';

  /** 文件消息那颗图标：目录用文件夹，其余用文件（与共享项列表同一套图标）。 */
  function icPath(item: ChatItem & { kind: 'file' }): string {
    return item.isDir ? ICON_FOLDER : ICON_FILE;
  }

  /**
   * 一条消息那一刻（`stamp`：今天/昨天 + `HH:MM`，更早写日期）加上该说的话。
   *
   * 一对一：自己发的带**这台设备自己的**送达状态（群发过去的副本在这屏也只写这一台，
   * 见 `chat.deliveryText`），对方发的只写时刻 —— 标题已经写着是谁了。
   * 群发：只写时刻 —— 逐台战果由下面那块回执（`.chat-receipt`）逐台写清楚。
   * 群聊：自己发的写群里的账（「已读 3/4」，见 `chat.groupReceiptText`），别人发的只写时刻
   * —— 是谁说的，上一行那枚发言人标已经写了。
   */
  function metaOf(item: ChatItem): string {
    const when = chat.stamp(item.at);
    if (!item.fromMe) return when;
    if (directPeer) {
      const state = chat.deliveryText(directPeer, item);
      return state ? `${when} · ${state}` : when;
    }
    if (thread?.mode !== 'group') return when;
    const receipt = chat.groupReceiptText(item, declinedIds);
    return receipt ? `${when} · ${receipt}` : when;
  }

  /** 消息右键要复制的那串（文件消息 = `名字（大小）`，图片消息 = `[图片] 名字`，与原型同一套）。 */
  function copyTextOf(item: ChatItem): string {
    if (item.kind === 'file') return `${item.name}（${item.size}）`;
    if (item.image) return `[图片] ${item.name ?? ''}`.trim();
    return item.text;
  }

  /**
   * 这条文件消息是不是一张**能在会话里摊开看的图**：按扩展名判（与后端 `paste::mime_of`
   * 同一份名单）。文件夹当然不是；认不出来的扩展名也不是（那样的气泡还是老样式）。
   */
  function isImageMsg(item: ChatItem): boolean {
    return item.kind === 'file' && !item.isDir && isImageName(item.name);
  }

  /** 一条消息的显示名：文件消息与图片消息都有 `name`，都没有时写「图片」。 */
  function msgName(item: ChatItem): string {
    return item.kind === 'file' ? item.name : item.name ?? '图片';
  }

  function onLogContext(e: MouseEvent) {
    const msgEl = (e.target as HTMLElement | null)?.closest?.('.chat-msg') as HTMLElement | null;
    if (!msgEl) return;
    e.preventDefault();
    // 选中的那段文字（只在选区确实落在这一条里时才算）—— 原型同款判断。
    let sel = '';
    const selection = window.getSelection();
    if (selection && selection.rangeCount && !selection.isCollapsed) {
      const picked = selection.toString().trim();
      if (picked && msgEl.contains(selection.getRangeAt(0).commonAncestorContainer)) sel = picked;
    }
    const item = items.find((i) => i.id === msgEl.dataset.id);
    onMessageCtx(e.clientX, e.clientY, msgEl.dataset.id ?? '', item ? copyTextOf(item) : '', sel);
  }

  /**
   * 发出去这一整条。**顺序与分段都在 store 里**（`chat.sendComposer`）：输入区里可能是
   * "一段文字 / 一张图 / 又一段文字"，它按框里的顺序一条条落成消息，两端都排得对。
   *
   * 这里只管两件界面上的事：把输入区**重画**成 store 里剩下的那几段（发出去的不留、没发出去
   * 的原地留着），以及报结果 ——
   * 群发的提示当场给出逐台战果（刚发那条读出来的，不猜）：全到位只说台数；没到位的把
   * "几台送达 / 几台在等对端上线 / 几台没送到 / 几台发送中"一次说清 —— 随后
   * `chat://changed` 还会更新回执（对端上线后，等待发送的那几台会自己变成已送达）。
   */
  async function send() {
    const t = thread;
    if (!t) return;
    const res = await chat.sendComposer(t.key).catch((e) => {
      onToast(String(e));
      return null;
    });
    const el = inputEl;
    if (el) {
      applyComposer(el, chat.draftSegsOf(t.key));
      markEmpty();
      el.focus();
      setCaretEnd(el);
    }
    if (!res || res.failed) {
      if (res?.failed) onToast(`有 ${res.failed} 项没发出去：${res.error ?? '原因不明'}（还留在输入框里）`);
      return;
    }
    if (t.mode !== 'broadcast' || !res.textId) {
      if (res.images && !res.texts) onToast(`已发送 ${res.images} 张图片`);
      return;
    }
    const item = chat.itemById(t.key, res.textId);
    const receipt = item ? chat.receiptOf(item) : null;
    if (!item || !receipt) return;
    if (receipt.delivered === receipt.total) {
      onToast(`已群发至 ${receipt.total} 台设备`);
      return;
    }
    const tail = [`${receipt.delivered}/${receipt.total} 台送达`];
    if (receipt.waiting) tail.push(`${receipt.waiting} 台等对端上线`);
    if (receipt.offline) tail.push(`${receipt.offline} 台未送达`);
    const pending = chat.perDeviceOf(item).filter((p) => p.state === 'pending').length;
    if (pending) tail.push(`${pending} 台发送中`);
    onToast(`已群发 · ${tail.join('，')}`);
  }

  /** 收件人上那颗 ✕（原型 `removeRecipient`）：去掉一台；只剩一台就退回一对一会话。 */
  function dropRecipient(peerId: string) {
    const t = thread;
    if (!t) return;
    void chat
      .removeParticipant(t.key, peerId)
      .then((res) => {
        if (!res) return;
        if (!res.dissolved) {
          onToast(`已从收件人移除「${res.name}」（不是踢人，对方不会收到任何通知）`);
          return;
        }
        onToast(res.remaining ? '参与者只剩一台，已退回一对一会话' : '这台设备已经不在设备表里了，会话已关闭');
      })
      .catch((e) => onToast(String(e)));
  }

  /** 「⋮」：坐标与是哪个会话都交给 App，菜单内容也由它按模式决定。 */
  function convoMenu(e: MouseEvent) {
    e.stopPropagation();
    const rect = (e.currentTarget as HTMLElement).getBoundingClientRect();
    if (thread) onConvoCtx(rect.right, rect.bottom + 4, thread.key);
  }
</script>

{#snippet icBc()}
  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M18.5 5.5 6.5 9.5H5a1.5 1.5 0 0 0-1.5 1.5v2A1.5 1.5 0 0 0 5 14.5h1.5l12 4V5.5Z" /><path d="M8 14.8V18a1.6 1.6 0 0 0 1.6 1.6h1.2" /></svg>
{/snippet}

{#snippet icGrp()}
  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M16 19v-1a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v1" /><circle cx="9" cy="7" r="3" /><path d="M22 19v-1a4 4 0 0 0-3-3.87" /><path d="M16 4.13a4 4 0 0 1 0 7.75" /></svg>
{/snippet}

{#snippet icChat()}
  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><path d={ICON_CHAT} /></svg>
{/snippet}

{#snippet icCheck()}
  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.6" stroke-linecap="round" stroke-linejoin="round"><path d="m5 12.5 4.5 4.5L19 7" /></svg>
{/snippet}

{#snippet icMinus()}
  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4" stroke-linecap="round"><path d="M7 12h10" /></svg>
{/snippet}

{#snippet icWarn()}
  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M12 3.5 2.6 20h18.8L12 3.5Z" /><path d="M12 10v4" /><path d="M12 17.4h.01" /></svg>
{/snippet}

<!-- 推送卡头部那一行（图标 + 名字 + 大小）：待接收、收下的、以及它外面套的那颗「打开」按钮
     共用同一份 —— 三种状态的行长得必须一样，差别只在能不能点。 -->
{#snippet fileRow(item: ChatItem & { kind: 'file' })}
  <span class="f-icon"><svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6"><path d={icPath(item)} /></svg></span>
  <span class="ic-main">
    <span class="ic-name">{item.name}</span>
    <span class="ic-size">{item.size}</span>
  </span>
{/snippet}

<!-- 自己发出去那条气泡里的内容（名字 / 大小 / 状态行 / 进度条）：那颗「打开」按钮套在外面，
     没有路径时就裸着画（一个字都不差）。`status` 由调用处现算 —— 一对一那条只写这一台自己的
     状态，群发那条写整批（见 `fileStatusText`）。 -->
{#snippet fileBubbleBody(item: ChatItem & { kind: 'file' }, status: string)}
  <span class="cf-ico"><svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6"><path d={icPath(item)} /></svg></span>
  <span class="cf-main">
    <span class="cf-name">{item.name}</span>
    <span class="cf-size">{item.size}</span>
    {#if status}<span class="cf-status">{status}</span>{/if}
    {#if item.sending}<span class="cf-bar"><i style="width:{item.pct}%"></i></span>{/if}
  </span>
{/snippet}

<!-- 群邀请卡：**只画在会话侧**（列表头顶 + 会话详情顶部），不进右下角那组通知气泡 ——
     那组东西是"没有落点、有期限"的（配对邀请 60s 自动作废；推送邀请在对方点头前
     一直留着），而这张卡没有期限、处理完的落点就是它将要成为的那条会话。两处都渲染同一份
     （snippet），一处滚走另一处还在。 -->
{#snippet inviteStrip(inhead: boolean)}
  <div class="invite-strip" class:inhead={inhead} aria-label="群聊邀请">
    {#each chat.pendingGroups() as pg (pg.id)}
      {@const wait = pg.buffered > 0 ? ` · 已有 ${pg.buffered} 条消息在等你` : ''}
      <div class="invite-card">
        <div class="ic-row">
          <span class="f-icon">{@render icGrp()}</span>
          <span class="ic-main">
            <span class="ic-name">「{pg.fromName}」邀请你加入群聊「{pg.name.trim() || '未命名群聊'}」</span>
            <!-- 攒着几条是后端数的（`orphans`）：不说的话，用户不点「加入」就永远不知道
                 里面已经有东西在等他了。 -->
            <span class="ic-size">其他 {pg.members.length} 台设备{wait}</span>
          </span>
        </div>
        <div class="ic-acts">
          <button class="btn btn-secondary btn-sm" type="button" onclick={() => answerInvite(pg, false)}>不加入</button>
          <button class="btn btn-primary btn-sm" type="button" onclick={() => answerInvite(pg, true)}>加入</button>
        </div>
      </div>
    {/each}
  </div>
{/snippet}

<!-- 浮层常驻 DOM（`inert` 关掉交互）：收起 / 展开那一下得靠 `class:open` 触发过渡，
     要是按需挂载，挂上时就已经是终态，缩放那一动画就没了。 -->
<section
  class="convo-pop"
  class:open
  class:drop-hot={dropHot}
  aria-label="会话"
  aria-hidden={!open}
  inert={!open}
  data-od-id="convo-sheet"
>
  <div class="convos-list-view" hidden={!!thread}>
    <header class="convos-head">
      <span class="convos-title">会话</span>
      <div class="convos-head-acts">
        <button class="btn btn-secondary btn-sm" type="button" onclick={() => chat.askNewConvo()}>＋ 新建会话</button>
        <button class="icon-btn" type="button" aria-label="关闭会话" onclick={() => chat.setChatOpen(false)}>
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"><path d="M6 6l12 12M18 6 6 18" /></svg>
        </button>
      </div>
    </header>

    <!-- 群邀请：**收到但还没确认**的群就到这儿为止（用户没点「加入」就不算群，不占会话列表
         一行）。卡长在列表头顶、不跟着列表滚：邀请是"等你处理"的东西，滚走就等于没看见。
         详情里也有一份（见下面 `.conv-headgroup` 里那颗 `inviteStrip(true)`）：两处都看得见，
         点进哪条会话都不会让它离开视野。 -->
    {#if chat.pendingGroups().length > 0}
      {@render inviteStrip(false)}
    {/if}

    <div class="convo-list" aria-label="会话列表">
      {#if rows.length === 0}
        <div class="empty">
          {@render icChat()}
          <p>还没有会话</p>
          <p class="e-sub">用「＋ 新建会话」选设备开始，或到「设备」页那一行点「发消息」；对方先发来消息也会出现在这里。</p>
        </div>
      {:else}
        {#each rows as row (row.key)}
          <!-- 三种身份混排、行长得一样（头像 / 名字 / 模式章 / 预览 / 时间 / 未读）：头像与模式章区分身份。 -->
          <button class="convo-item" type="button" onclick={() => chat.openConversation(row.key)}>
            <span class="convo-avs">
              {#if row.mode === 'direct'}
                <span class="d-avatar">{row.initial}</span>
              {:else if row.mode === 'broadcast'}
                <span class="d-avatar bc-mark">{@render icBc()}</span>
              {:else}
                <span class="d-avatar grp-mark">{@render icGrp()}</span>
              {/if}
            </span>
            <span class="convo-main">
              <span class="convo-top">
                <span class="convo-name" title={row.label}>{row.label}</span>
                {#if row.mode === 'broadcast'}
                  <span class="convo-mode-chip bc">{@render icBc()}群发</span>
                {:else if row.mode === 'group'}
                  <span class="convo-mode-chip group">{@render icGrp()}群聊</span>
                {/if}
                <span class="convo-dot {row.dot}"></span>
                <span class="convo-time num">{row.time}</span>
              </span>
              <span class="convo-prev">{row.sub}</span>
            </span>
            {#if row.unread > 0}<span class="convo-unread">{row.unread}</span>{/if}
          </button>
        {/each}
      {/if}
    </div>
  </div>

  {#if thread}
    <div class="convo-detail">
      <div class="conv-headgroup">
        <header class="conv-top">
          <button class="conv-back" type="button" aria-label="返回会话列表" onclick={() => chat.backToList()}>
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="m15 6-6 6 6 6" /></svg>
          </button>
          <!-- 返回箭头**右边**：别的会话还等着几条（数与减掉当前这条的算法见 `chat.unreadElsewhere`）。
               不把返回键本身改成这颗数：让唯一那颗"退回去"要悬停才变得回来，是把主操作藏进指针里
               （键盘 Tab 到的还是同一个位置，屏幕上却写着 3）；点它是回列表，与返回键同一个落点。

               外面那层 `.conv-navslot` **一直在**（哪怕没有数）：数从无到有的时候不许把会话名顶走。
               槽宽按**最大值**（`99+` 那颗胶囊）钉死，1 位数与 3 位数都不动标题。 -->
          <span class="conv-navslot">
            {#if elsewhere}
              <button
                class="conv-elsewhere"
                type="button"
                title="还有 {elsewhere} 条来自其他会话的消息"
                aria-label="还有 {elsewhere} 条来自其他会话的消息，回会话列表"
                onclick={() => chat.backToList()}
              >{elsewhere > 99 ? '99+' : elsewhere}</button>
            {/if}
          </span>
          <!-- 名字 + 状态点 + 台数这一组**居中**（`.conv-headline`）：槽留出来的那格空位因此看不出
               来 —— 名字在中间，左边那段空白就只是"返回键旁边的空白"，而不是"名字前面多出来的一截"。
               分组里那颗点与台数跟着名字走（原型那三样本来就是一组）。 -->
          <div class="conv-headline">
            <span class="conv-title" title={thread.title}>{thread.title}</span>
            <!-- 标题旁**只有一枚状态点**（`docs/orb-functional-design.md` §12.2「状态栏」：header
                 除设备名外就带该设备的状态点）。未绑定的设备照样能收发（消息走"长连接优先 +
                 HTTP 兜底"），但它不需要再挂一枚「未绑定」章 —— 那会在标题旁并排摆出两颗点
                 （一枚裸的、一枚在胶囊里），看着就像画重了。 -->
            <span class="conv-status {directPeer ? deviceDotState(openDev) : threadDotState(thread.participants)}"></span>
            {#if thread.participants.length > 1}
              <span class="conv-count num">{thread.participants.length} 台</span>
            {/if}
          </div>
          <div class="conv-acts">
            <button class="icon-btn" type="button" aria-label="会话操作" onclick={convoMenu}>
              <svg viewBox="0 0 24 24" fill="currentColor"><circle cx="5" cy="12" r="1.6" /><circle cx="12" cy="12" r="1.6" /><circle cx="19" cy="12" r="1.6" /></svg>
            </button>
            <button class="icon-btn" type="button" aria-label="关闭会话" onclick={() => chat.setChatOpen(false)}>
              <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"><path d="M6 6l12 12M18 6 6 18" /></svg>
            </button>
          </div>
        </header>

        <!-- 模式条：一对一不画（它就是默认）；群发与群聊各说自己的规则，并各带一颗「编辑」。 -->
        {#if thread.mode !== 'direct'}
          <div class="convo-mode" data-mode={thread.mode}>
            <span class="cm-ic" aria-hidden="true">{#if thread.mode === 'broadcast'}{@render icBc()}{:else}{@render icGrp()}{/if}</span>
            <span class="cm-txt">
              <b>{thread.mode === 'broadcast' ? '群发模式' : '群聊模式'}</b>
              {thread.mode === 'broadcast' ? ' · 单向发送，对方各自单独收到' : ' · 任意参与者共享同一条消息流'}
            </span>
            <button class="cm-act" type="button" onclick={() => chat.askParticipants(thread.key)}>
              {thread.mode === 'broadcast' ? '编辑群发' : '编辑群聊'}
            </button>
          </div>
        {/if}

        <!-- 群邀请的"头"：它跟模式条同一层（不随消息流滚），因为待处理的邀请不该因为
             "我正在读别的会话"而消失 —— 列表那份看不到的时候，就是它在顶着。 -->
        {#if chat.pendingGroups().length > 0}
          {@render inviteStrip(true)}
        {/if}

        <!-- 「送不出去」的那句解释：贴在头部这一层（不随消息流滚），8s 自己消失、也能点掉。 -->
        {#if undeliverable}
          <div class="chat-note" role="status">
            <span class="cn-ic" aria-hidden="true">{@render icWarn()}</span>
            <span class="cn-txt">{undeliverable.text}</span>
            <button class="icon-btn cn-x" type="button" aria-label="关闭提示" onclick={dismissUndeliverable}>
              <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round"><path d="M6 6l12 12M18 6 6 18" /></svg>
            </button>
          </div>
        {/if}
      </div>

      <div class="recip">
        <span class="recip-label">{thread.mode === 'broadcast' ? '群发至' : '发送给'}</span>
        <div class="recip-chips">
          {#each thread.participants as peerId (peerId)}
            {@const dev = devices.find((d) => d.id === peerId)}
            <!-- 群成员不一定还在本机设备表里（被解除了配对、或只是没再加回来）：那时用名册上
                 随群带来的名字，别把一串设备 id 摆给用户看。一对一那台的设备行也可能已经不在
                 表里了（解绑又离线、会话还留着）：名字退回记录里那个（`deviceNameOf`）。 -->
            {@const rosterName = thread.mode === 'group' ? chat.groupOf(thread.id)?.members.find((m) => m.id === peerId)?.name.trim() : ''}
            <!-- 这一栏用的是**短名**（F-101）：有备注就只写备注，**不挂设备名** —— 一排胶囊
                 （群发/群聊常常好几台）里装不下「备注(设备名)」，用户起备注本来就是为了少看那
                 串机器名。头像字跟着同一个 label 走；名字长了悬停看全（`title`）。
                 本机只有**局域网设备列表**那一行是「备注(设备名)」（`deviceDisplayName`）。 -->
            {@const label = dev ? deviceShortName(dev) : rosterName || chat.deviceNameOf(peerId)}
            <!-- 明确拒绝加入这个群的那几台：整枚胶囊染红 + 悬停说明**为什么** —— 它们看着一切
                 正常（链路是通的、点名也在名册里），不说出来用户根本不知道这条消息它收不到。
                 旁边那颗状态点照旧画**链路**（绿 = 长连接…）：那是"拨得动它吗"，与"它收不收
                 本群的消息"是两件事（红的点在这个配色里已经被「要重新配对」占用）。 -->
            {@const refused = declinedIds.includes(peerId)}
            <span
              class="recip-chip"
              class:refused
              title={refused ? `${label} · 已拒绝加入这个群，收不到群消息` : label}
            >
              <span class="rc-av">{(label.trim().charAt(0) || '?').toUpperCase()}</span>
              <span class="rc-name">{label}</span>
              <span class="rc-st {deviceDotState(dev)}"></span>
              {#if thread.mode !== 'direct'}
                <button class="rc-x" type="button" aria-label="从收件人里移除" onclick={() => dropRecipient(peerId)}>
                  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><path d="M6 6l12 12M18 6 6 18" /></svg>
                </button>
              {/if}
            </span>
          {/each}
        </div>
        <button
          class="recip-add"
          type="button"
          aria-label={thread.mode === 'direct' ? '再加几台一起发' : '编辑收件人'}
          title={thread.mode === 'direct' ? '再加几台：选到两台以上就成为一个多台会话（群发或群聊）' : ''}
          onclick={() => chat.askParticipants(thread.key)}
        >
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round"><path d="M12 5v14M5 12h14" /></svg>
          添加
        </button>
      </div>

      <!-- 消息流：右键复制走 `onLogContext`（菜单由 App 画在 `.orb-stage` 之外，跨层叠上下文永远在最上）。 -->
      <div class="chat-log" bind:this={logEl} role="log" aria-label="会话记录" oncontextmenu={onLogContext}>
        <!-- 补拉够不着的那几条：群里明明已知缺着东西，就别只摆一条看着很完整的记录。
             一行小字，与群发那条单向台账同一副骨头（`.chat-ledger` 那一档），只在真缺时才在。 -->
        {#if missing > 0}
          <div class="chat-sync">
            <span class="cs-line"></span>
            <span class="cs-txt">有 {missing} 条消息未能同步（来源设备不在线）</span>
            <span class="cs-line"></span>
          </div>
        {/if}
        {#if items.length === 0 && missing === 0}
          <div class="chat-empty">
            {#if thread.mode === 'broadcast'}
              {@render icBc()}
              <p>还没有群发消息</p>
              <p class="ce-sub">
                你发出的每条消息都会作为单独的一对一消息，分别送达 {thread.participants.length} 台设备；这里只显示你发出的内容。
              </p>
            {:else if thread.mode === 'group'}
              {@render icGrp()}
              <p>还没有消息</p>
              <p class="ce-sub">在这里说一句，名册上的成员都会看到（同一条消息流）；传文件则按成员逐台推送。</p>
            {:else}
              {@render icChat()}
              <p>还没有消息</p>
              <p class="ce-sub">发一条消息或传一个文件，开始和设备对话。</p>
            {/if}
          </div>
        {:else}
          {#if thread.mode === 'broadcast'}
            <!-- 单向台账：这一条条都是我发出去的（对方的回复各自回到它们的 1:1，不在这儿）。 -->
            <div class="chat-ledger">
              <span class="cl-line"></span>
              <span class="cl-txt">已发送 · {items.length} 条 · 单向</span>
              <span class="cl-line"></span>
            </div>
          {/if}
          {#each items as item, i (item.id)}
            {@const who = speakerOf(i)}
            <div
              class="chat-msg"
              class:me={item.fromMe}
              class:peer={!item.fromMe}
              class:bc={thread.mode === 'broadcast'}
              class:picked={pickedId === item.id}
              data-id={item.id}
            >
              <!-- 群聊：换个人说话就在这条上面插一行发言人（头像字 + 名字，颜色按设备 id 哈希）——
                   同一个人连着说的几句共享头一行，一屏里不会满眼都是名字。 -->
              {#if who}
                <div class="chat-who {who.cls}">
                  <span class="cw-av">{who.initial}</span>
                  <span class="cw-nm">{who.name}</span>
                </div>
              {/if}
              {#if item.kind === 'text'}
                {#if item.image}
                  <!-- **图片消息**：图片是消息内容，不是文件 —— 对端直接就能看（没有"接收 /
                       同意"那一步），气泡里就是那张图，点开看大图。内容不在盘上（老版本留下的
                       记录、或文件被人删了）就退回一行名字，那一行点得动（用系统程序打开）。 -->
                  <div class="chat-bubble chat-bubble-file">
                    {#if item.path}
                      <ImagePreview path={item.path} label={item.name ?? '图片'} onopen={() => (viewing = item)}>
                        <span class="cf-main"><span class="cf-name">{item.name ?? '图片'}</span></span>
                      </ImagePreview>
                    {:else}
                      <span class="cf-main"><span class="cf-name">{item.name ?? '图片'}（读不出来）</span></span>
                    {/if}
                  </div>
                {:else}
                  <div class="chat-bubble"><span class="chat-text">{item.text}</span></div>
                {/if}
              {:else if !item.fromMe && item.inviteState === 'pending'}
                <!-- 对端推过来的那条：接收 / 拒绝（原型 `invite-card`） -->
                <div class="invite-card">
                  <div class="ic-row">{@render fileRow(item)}</div>
                  <div class="ic-sub">来自 {chat.deviceNameOf(item.peerId)} · {askEachTime ? '接收时选保存位置' : '接收后保存到下载目录'}</div>
                  <div class="ic-acts">
                    <button class="btn btn-secondary btn-sm" type="button" onclick={() => onRejectInvite(item.inviteId)}>拒绝</button>
                    <button class="btn btn-primary btn-sm" type="button" onclick={() => onAcceptInvite(item.inviteId)}>接收</button>
                  </div>
                </div>
              {:else if !item.fromMe}
                <!-- 收下的那条：进度 / 结果（原型里这一档走 `.cf-*` 那套零件）。
                     收完之后（`item.path` 有值了）那一行变成一颗按钮：点它用系统默认程序打开落盘
                     的那一份 —— 与文件标签页里双击一格同一个落点。还没收完就没有路径，那行照旧
                     是死的。 -->
                <div class="invite-card">
                  {#if item.path && isImageMsg(item)}
                    <!-- 收到的是一张图：气泡里摊开看它一眼（点开是大图）。读不出来就退回文件行。 -->
                    <ImagePreview path={item.path} label={item.name} onopen={() => (viewing = item)}>
                      <div class="ic-row">{@render fileRow(item)}</div>
                    </ImagePreview>
                  {:else if item.path}
                    <button
                      class="ic-row ic-open"
                      type="button"
                      title="打开「{item.name}」"
                      onclick={() => onOpenFile(item)}
                    >{@render fileRow(item)}</button>
                  {:else}
                    <div class="ic-row">{@render fileRow(item)}</div>
                  {/if}
                  <div class="ic-done">
                    <span>{chat.fileStatusText(item)}</span>
                    {#if item.inviteState === 'accepted'}
                      <!-- 正在接收：一颗停止（图标就够 —— 这一行本来就窄，写"停止"会把状态顶走）。
                           点了就撤掉这条接收，且**不再自动接着收**（见后端 `cancel_receive`）。 -->
                      <button
                        class="ic-stop"
                        type="button"
                        title="停止接收「{item.name}」"
                        aria-label="停止接收「{item.name}」"
                        onclick={() => onStopReceive(item.inviteId)}
                      >
                        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" aria-hidden="true">
                          <circle cx="12" cy="12" r="9" />
                          <path d="M9 9h6v6H9z" />
                        </svg>
                      </button>
                    {/if}
                  </div>
                  {#if item.inviteState === 'accepted'}
                    <span class="cf-bar"><i style="width:{item.bytes > 0 ? Math.min(100, Math.round((item.received / item.bytes) * 100)) : 0}%"></i></span>
                  {/if}
                </div>
              {:else}
                <!-- 自己发出去的文件：传输状态行（一对一 = 这台自己的状态；群发 = 整批的进度，
                     逐台状态另有一排回执。进度来自右下角那张推送卡）。
                     整块是一颗按钮：源文件就在这台机器上，点它直接开那一份（零拷贝）；
                     路径是选文件那一刻记下的，所以传着的时候也点得开。 -->
                {@const fState = chat.fileStatusText(item, directPeer)}
                <div class="chat-bubble chat-bubble-file">
                  {#if item.path && isImageMsg(item)}
                    <!-- 自己发出去的一张图：同样在气泡里摊开（点开是大图）。源文件就在这台机器上，
                         读不出来时退回"点一下用系统程序打开"那颗按钮。 -->
                    <ImagePreview path={item.path} label={item.name} onopen={() => (viewing = item)}>
                      <button
                        class="cf-open"
                        type="button"
                        title="打开本机的「{item.name}」"
                        onclick={() => onOpenFile(item)}
                      >{@render fileBubbleBody(item, fState)}</button>
                    </ImagePreview>
                  {:else if item.path}
                    <button
                      class="cf-open"
                      type="button"
                      title="打开本机的「{item.name}」"
                      onclick={() => onOpenFile(item)}
                    >{@render fileBubbleBody(item, fState)}</button>
                  {:else}
                    {@render fileBubbleBody(item, fState)}
                  {/if}
                </div>
              {/if}
              <div class="chat-meta">{metaOf(item)}</div>

              <!-- 群发：这条当时发给了谁、每台走到哪一步（逐台读副本的送达状态，不按当前名单猜）。 -->
              {#if thread.mode === 'broadcast' && item.fromMe}
                {@const per = chat.perDeviceOf(item)}
                {@const receipt = chat.receiptOf(item)}
                {#if receipt}
                  <div class="chat-receipt">
                    <span class="cr-head">已送达 {receipt.delivered}/{receipt.total} 台</span>
                    <span class="cr-list">
                      {#each per as p (p.peerId)}
                        <span
                          class="cr-dev"
                          class:off={p.state === 'off'}
                          class:pending={p.state === 'pending' || p.state === 'waiting'}
                          title="{chat.deviceNameOf(p.peerId)} · {p.label}"
                        >
                          <span class="cr-av">{chat.peerInitial(p.peerId)}</span>
                          <span class="cr-nm">{chat.deviceNameOf(p.peerId)}</span>
                          <span class="cr-ic">{#if p.state === 'ok'}{@render icCheck()}{:else}{@render icMinus()}{/if}</span>
                        </span>
                      {/each}
                    </span>
                  </div>
                {/if}
              {/if}
            </div>
          {/each}
        {/if}
      </div>

      <form
        class="chat-compose"
        class:bc={thread.mode === 'broadcast'}
        onsubmit={(e) => {
          e.preventDefault();
          void send();
        }}
      >
        <!-- 富输入区（`contenteditable`）：文字与图片**在同一行流里**，图可以夹在前后两段文字
             中间（需求要的就是这个顺序）。交给 Svelte 管的只有"哪个会话 / 哪个节点"，
             里面的内容由 `lib/composer.ts` 直接读写 DOM —— 受控的话每敲一个字光标都会被拽回最前。
             图片那一份（`.cx-img`）由那段代码用 `document.createElement` 造，样式在 `main.css`。 -->
        <div
          class="chat-input cx"
          bind:this={inputEl}
          contenteditable="true"
          role="textbox"
          tabindex="0"
          aria-multiline="true"
          spellcheck="false"
          data-empty="1"
          data-placeholder={thread.mode === 'broadcast'
            ? `群发给 ${thread.participants.length} 台设备…`
            : thread.mode === 'group'
              ? '在群里说一句…'
              : '输入消息…'}
          aria-label={thread.mode === 'broadcast' ? '输入要群发的消息' : thread.mode === 'group' ? '输入群消息' : '发送消息'}
          title={thread.mode === 'group' ? '群里说的话，名册上的成员都会看到；图片也可以直接粘进来' : '图片可以直接粘进来（Ctrl+V），前后还能继续写文字'}
          oninput={() => void onInput()}
          onpaste={onPaste}
          onkeydown={(e) => {
            // Shift+回车留给换行（这一版输入区支持多行），回车是发送。
            if (e.key === 'Enter' && !e.shiftKey && !e.isComposing) {
              e.preventDefault();
              void send();
            }
          }}
        ></div>
        <button
          class="chat-plus"
          class:bc={thread.mode === 'broadcast'}
          type="button"
          aria-label={thread.mode === 'broadcast' ? '群发文件（即推送）' : thread.mode === 'group' ? '发送文件到群里' : '发送文件（即推送）'}
          title={thread.mode === 'group' ? '选文件发到群里：按成员逐台推送（未配对的成员各自点一次同意）' : '选文件发过去就是推送'}
          onclick={onSendFiles}
        >
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round"><path d="M12 5v14M5 12h14" /></svg>
        </button>
        <button
          class="chat-send"
          class:bc={thread.mode === 'broadcast'}
          type="submit"
          aria-label={thread.mode === 'broadcast' ? '群发消息' : '发送消息'}
          title={thread.mode === 'group' ? '发送到群里' : ''}
          disabled={!canSend}
        >
          {#if thread.mode === 'broadcast'}
            {@render icBc()}
          {:else}
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M12 19V6" /><path d="M6 12l6-6 6 6" /></svg>
          {/if}
        </button>
      </form>
    </div>
  {/if}

  <!-- 拖入预告（F-111）：落点就是这条会话 —— 松手即推送，不再登记成共享项。
       盖一层浅底把底下的消息流压下去：这句话要读得清，`pointer-events: none` 让它不吃任何
       指针事件（拖动期间本来也收不到）。它只在该出现时挂上（`dropHot` 只在浮层开着、
       且已经进了某条会话时才为真，见 `App.dropInConvo`）。 -->
  {#if dropHot}
    <div class="drop-hint"><b>松手即发送到这条会话</b></div>
  {/if}

  <!-- 「看大图」：会话里点一张图的缩略图就在这一层上摊开它（`viewing` 跟着会话走 ——
       换会话时上面那个 effect 会把它清掉，一张已经不属于这条会话的图不该还挂在屏幕上）。 -->
  {#if viewing && viewing.path !== undefined && viewing.path !== ''}
    <ImageViewer
      path={viewing.path}
      name={msgName(viewing)}
      onclose={() => (viewing = null)}
      onopenfile={() => {
        if (viewing) onOpenFile(viewing);
      }}
    />
  {/if}
</section>

<style>
  /* 群聊的补拉缺口：与群发那条单向台账（全局 `.chat-ledger`）同一副骨头 —— 两侧细线夹一行
     等宽小字 —— 但不借它的颜色：那套橙色是"群发"的语义色，这里只是"少了几条"的如实交代，
     用中性灰，读作提示而不是模式标记。 */
  .chat-sync { display: flex; align-items: center; gap: 8px; padding: 4px 2px 2px; }
  .chat-sync .cs-line { flex: 1; height: 1px; background: var(--border); }
  .chat-sync .cs-txt {
    flex: none; font-family: var(--font-mono); font-size: 10px;
    letter-spacing: 0.06em; color: var(--muted);
  }

  /* 群邀请卡长在列表头顶：`.invite-card`（全局）自带边框与底色，这里只补它需要的落位与
     内边距 —— 贴着浮层左右边缘会长得像个横幅。 */
  .invite-strip { flex: none; display: grid; gap: 8px; padding: 10px 12px 0; }
  /* 详情里那一份（`.conv-headgroup` 内）：下面是收件人栏，补掉底边距，别两张卡贴在一起。 */
  .invite-strip.inhead { padding-bottom: 10px; }

  /* 群聊的发言人行：只在换人说话时插一行（头像字 + 名字）。三种语义色按设备 id 哈希取，
     用的是本机主色 / 成功色 / 群发暖色（`--accent` / `--ok` / `--bc`）—— 与"这是谁"无关，
     只负责把几个发言人一眼分开，且改名字不会换色。 */
  .chat-who { display: flex; align-items: center; gap: 5px; margin: 1px 2px 4px; }
  .chat-who .cw-av {
    flex: none; width: 18px; height: 18px; border-radius: 6px;
    display: grid; place-items: center;
    font-family: var(--font-display); font-size: 9px; font-weight: 650;
    color: var(--surface);
  }
  .chat-who .cw-nm { font-family: var(--font-display); font-size: var(--fs-xxs); font-weight: 620; }
  .chat-who.sp-0 .cw-av { background: var(--accent-deep); }
  .chat-who.sp-0 .cw-nm { color: var(--accent-deep); }
  .chat-who.sp-1 .cw-av { background: var(--ok); }
  .chat-who.sp-1 .cw-nm { color: color-mix(in oklch, var(--ok) 78%, black); }
  .chat-who.sp-2 .cw-av { background: var(--bc-deep); }
  .chat-who.sp-2 .cw-nm { color: var(--bc-deep); }
</style>