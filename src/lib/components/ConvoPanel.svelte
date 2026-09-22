<script lang="ts">
  import * as chat from '$lib/stores/chat.svelte';
  import type { ChatItem } from '$lib/stores/chat.svelte';
  import { getDevices } from '$lib/stores/devices.svelte';

  /**
   * 会话浮层（原型 `.convo-pop`）：三块视图共用一层 —— **会话列表 / 与某台设备的会话 / 群发框**。
   *
   * 这里**只管画**：会话记录在后端（`stores/chat` 拿着那份快照），"谁在跟你说话"看设备表。
   * 需要 App 出面的事（开文件选择框、右下角那条邀请、右键菜单、切到设备标签）都走回调 ——
   * 会话是面板里的一个视图，不是第二个 App。
   *
   * **群发框是临时载体**：它自己不是会话、不留历史（记录落在各台设备的会话里），所以它是
   * 一块独立的视图，而不是"某个会话"。
   */
  let {
    pickedId,
    onToast,
    onSendFiles,
    onAcceptInvite,
    onRejectInvite,
    onConvoCtx,
    onMessageCtx,
  }: {
    /** 右键菜单正指着的那条消息（高亮它，菜单收掉就清空）。 */
    pickedId: string | null;
    onToast: (msg: string) => void;
    /** 「＋」：给这几台选文件发过去（App 开文件选择框，走推送那条路；1:1 会话就是单台）。 */
    onSendFiles: (peerIds: string[]) => void;
    onAcceptInvite: (inviteId: string) => void;
    onRejectInvite: (inviteId: string) => void;
    /** 「与这台设备的会话」右上角「⋮」：把会话菜单的锚点交出去（菜单由 App 画）。 */
    onConvoCtx: (x: number, y: number) => void;
    /** 消息右键：坐标 + 这一条要复制的内容（整条 / 选中的那段）。 */
    onMessageCtx: (x: number, y: number, id: string, full: string, sel: string) => void;
  } = $props();

  const open = $derived(chat.isChatOpen());
  const batch = $derived(chat.getBatch());
  const openPeer = $derived(chat.getOpenPeer());
  const peers = $derived(chat.convoPeers());
  const devices = $derived(getDevices());
  const items = $derived<ChatItem[]>(openPeer ? chat.itemsOf(openPeer) : []);
  const openDev = $derived(devices.find((d) => d.id === openPeer));
  const tail = $derived(items.length ? items[items.length - 1].id : '');

  let logEl = $state<HTMLDivElement | null>(null);
  let inputEl = $state<HTMLInputElement | null>(null);
  let batchInputEl = $state<HTMLInputElement | null>(null);

  /** 新消息进来、换会话、浮层重新展开 —— 都落到最底下（原型 `renderChatLog` 结尾那次滚动）。 */
  $effect(() => {
    const el = logEl;
    if (!el) return;
    void tail;
    void openPeer;
    void open;
    if (open && openPeer) el.scrollTop = el.scrollHeight;
  });

  /** 打开某个会话 / 群发框就把光标放进输入框（点「发消息」「群发」进来的人下一步一定是打字）。 */
  $effect(() => {
    if (!open) return;
    if (batch) batchInputEl?.focus();
    else if (openPeer) inputEl?.focus();
  });

  const ICON_CHAT = 'M20 12a8 8 0 0 1-8 8H6.5L3 22V12a8 8 0 0 1 8-8h1a8 8 0 0 1 8 8Z M8.5 11h7 M8.5 14.5h4';
  const ICON_FILE = 'M6 3h8l5 5v13H6V3Z';
  const ICON_FOLDER = 'M3 7a2 2 0 0 1 2-2h4.2a2 2 0 0 1 1.4.6L12 7h7a2 2 0 0 1 2 2v9a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V7Z';

  /** 文件消息那颗图标：目录用文件夹，其余用文件（与共享项列表同一套图标）。 */
  function icPath(item: ChatItem & { kind: 'file' }): string {
    return item.isDir ? ICON_FOLDER : ICON_FILE;
  }

  /**
   * 一条消息那一刻（`HH:MM`）加上该说的话。
   *
   * 自己发的带送达状态；对方发的**只写时刻** —— 会话的标题已经写着是谁了，不必每行再报一遍
   * 名字（原型的 `chat-meta` 也是这个逻辑：只有群里的参与者消息才带名字，而这里没有群）。
   */
  function metaOf(item: ChatItem): string {
    const when = chat.clock(item.at);
    if (!item.fromMe) return when;
    const state = chat.deliveryText(item);
    return state ? `${when} · ${state}` : when;
  }

  /** 消息右键要复制的那串（文件消息 = `名字（大小）`，与原型 `data-copy` 同一套）。 */
  function copyTextOf(item: ChatItem): string {
    return item.kind === 'file' ? `${item.name}（${item.size}）` : item.text;
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

  function send(peerId: string, text: string) {
    if (!text.trim()) return;
    void chat
      .sendText(peerId, text)
      .then((ok) => {
        if (ok) chat.setDraftOf(peerId, '');
        else onToast('这台设备已经被移除了');
      })
      .catch((e) => onToast(String(e)));
  }

  /**
   * 群发框里的那条发出去：框自己会关掉（`sendBatch` 里 `batch = null`），这里只给一句话 ——
   * 逐台成败由刚发那条的 `id` 读出来（不猜），列表里那几台的最近记录随即变成这条。
   */
  function sendBatch() {
    void chat
      .sendBatch()
      .then((res) => {
        if (!res) return;
        if (!res.failed.length) onToast(`已群发给${chat.namesLabel(res.sent)}`);
        else if (!res.sent.length) onToast(`群发未送达：${chat.namesLabel(res.failed)}没有长连接`);
        else onToast(`已群发给${chat.namesLabel(res.sent)} · ${res.failed.length} 台未送达`);
      })
      .catch((e) => onToast(String(e)));
  }

  /**
   * 与某台设备的会话里点「添加」：这里没有"群会话"可加 —— 加人就是**改成群发**
   * （把当前这台带去群发框，选完设备一起发，记录各落各的会话）。
   */
  function addToBatch(seed: string[]) {
    chat.askRecipients(seed, (ids) => chat.openBatch(ids));
  }

  /** 「与这台设备的会话」右上角「⋮」：坐标交给 App，菜单内容也由它决定。 */
  function convoMenu(e: MouseEvent) {
    e.stopPropagation();
    const rect = (e.currentTarget as HTMLElement).getBoundingClientRect();
    onConvoCtx(rect.right, rect.bottom + 4);
  }
</script>

<!-- 浮层常驻 DOM（`inert` 关掉交互）：收起 / 展开那一下得靠 `class:open` 触发过渡，
     要是按需挂载，挂上时就已经是终态，缩放那一动画就没了。 -->
<section
  class="convo-pop"
  class:open
  aria-label="会话"
  aria-hidden={!open}
  inert={!open}
  data-od-id="convo-sheet"
>
  <div class="convos-list-view" hidden={!!openPeer || !!batch}>
    <header class="convos-head">
      <span class="convos-title">会话</span>
      <div class="convos-head-acts">
        <button class="btn btn-secondary btn-sm" type="button" onclick={() => chat.askRecipients([], (ids) => chat.openBatch(ids))}>＋ 群发</button>
        <button class="icon-btn" type="button" aria-label="关闭会话" onclick={() => chat.setChatOpen(false)}>
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"><path d="M6 6l12 12M18 6 6 18" /></svg>
        </button>
      </div>
    </header>

    <div class="convo-list" aria-label="会话列表">
      {#if peers.length === 0}
        <div class="empty">
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><path d={ICON_CHAT} /></svg>
          <p>还没有会话</p>
          <p class="e-sub">到「设备」页配对或建立长连接后，每台设备就是一条会话；要同时发几台就用「＋ 群发」。</p>
        </div>
      {:else}
        {#each peers as peerId (peerId)}
          {@const unread = chat.unreadForPeer(peerId)}
          <button class="convo-item" type="button" onclick={() => chat.openConversation(peerId)}>
            <span class="convo-avs"><span class="d-avatar">{chat.peerInitial(peerId)}</span></span>
            <span class="convo-main">
              <span class="convo-top">
                <span class="convo-name">{chat.deviceNameOf(peerId)}</span>
                <span class="convo-dot {chat.peerDotState(peerId)}"></span>
                <span class="convo-time num">{chat.timeOf(peerId)}</span>
              </span>
              <span class="convo-prev">{chat.summaryOf(peerId)}</span>
            </span>
            {#if unread > 0}<span class="convo-unread">{unread}</span>{/if}
          </button>
        {/each}
      {/if}
    </div>
  </div>

  {#if openPeer}
    <div class="convo-detail">
      <header class="conv-top">
        <button class="conv-back" type="button" aria-label="返回会话列表" onclick={() => chat.backToList()}>
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="m15 6-6 6 6 6" /></svg>
        </button>
        <span class="conv-title">{chat.deviceNameOf(openPeer)}</span>
        <span class="conv-status {chat.peerDotState(openPeer)}"></span>
        <div class="conv-acts">
          <button class="icon-btn" type="button" aria-label="会话操作" onclick={convoMenu}>
            <svg viewBox="0 0 24 24" fill="currentColor"><circle cx="5" cy="12" r="1.6" /><circle cx="12" cy="12" r="1.6" /><circle cx="19" cy="12" r="1.6" /></svg>
          </button>
          <button class="icon-btn" type="button" aria-label="关闭会话" onclick={() => chat.setChatOpen(false)}>
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"><path d="M6 6l12 12M18 6 6 18" /></svg>
          </button>
        </div>
      </header>

      <div class="recip">
        <span class="recip-label">发送给</span>
        <div class="recip-chips">
          <span class="recip-chip">
            <span class="rc-av">{chat.peerInitial(openPeer)}</span>
            <span class="rc-name">{openDev?.name ?? openPeer}</span>
            <span class="rc-st" class:ok={openDev?.connected}></span>
          </span>
        </div>
        <button class="recip-add" type="button" aria-label="再加几台一起发（群发）" onclick={() => addToBatch([openPeer])}>
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round"><path d="M12 5v14M5 12h14" /></svg>
          添加
        </button>
      </div>

      <!-- 消息流：右键复制走 `onLogContext`（菜单由 App 画在 `.orb-stage` 之外，跨层叠上下文永远在最上）。 -->
      <div class="chat-log" bind:this={logEl} role="log" aria-label="会话记录" oncontextmenu={onLogContext}>
        {#if items.length === 0}
          <div class="chat-empty">
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><path d={ICON_CHAT} /></svg>
            <p>还没有消息</p>
            <p class="ce-sub">发一条消息或传一个文件，开始和设备对话。</p>
          </div>
        {:else}
          {#each items as item (item.id)}
            <div
              class="chat-msg"
              class:me={item.fromMe}
              class:peer={!item.fromMe}
              class:picked={pickedId === item.id}
              data-id={item.id}
            >
              {#if item.kind === 'text'}
                <div class="chat-bubble"><span class="chat-text">{item.text}</span></div>
              {:else if !item.fromMe && item.inviteState === 'pending'}
                <!-- 对端推过来的那条：接收 / 拒绝（原型 `invite-card`） -->
                <div class="invite-card">
                  <div class="ic-row">
                    <span class="f-icon"><svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6"><path d={icPath(item)} /></svg></span>
                    <span class="ic-main">
                      <span class="ic-name">{item.name}</span>
                      <span class="ic-size">{item.size}</span>
                    </span>
                  </div>
                  <div class="ic-sub">来自 {chat.deviceNameOf(item.peerId)} · 接收后保存到下载目录</div>
                  <div class="ic-acts">
                    <button class="btn btn-secondary btn-sm" type="button" onclick={() => onRejectInvite(item.inviteId)}>拒绝</button>
                    <button class="btn btn-primary btn-sm" type="button" onclick={() => onAcceptInvite(item.inviteId)}>接收</button>
                  </div>
                </div>
              {:else if !item.fromMe}
                <!-- 收下的那条：进度 / 结果（原型里这一档走 `.cf-*` 那套零件） -->
                <div class="invite-card">
                  <div class="ic-row">
                    <span class="f-icon"><svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6"><path d={icPath(item)} /></svg></span>
                    <span class="ic-main">
                      <span class="ic-name">{item.name}</span>
                      <span class="ic-size">{item.size}</span>
                    </span>
                  </div>
                  <div class="ic-done">
                    <span>{chat.fileStatusText(item)}</span>
                  </div>
                  {#if item.inviteState === 'accepted'}
                    <span class="cf-bar"><i style="width:{item.bytes > 0 ? Math.min(100, Math.round((item.received / item.bytes) * 100)) : 0}%"></i></span>
                  {/if}
                </div>
              {:else}
                <!-- 自己发出去的文件：传输状态行（进度来自右下角那张推送卡） -->
                <div class="chat-bubble chat-bubble-file">
                  <span class="cf-ico"><svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6"><path d={icPath(item)} /></svg></span>
                  <span class="cf-main">
                    <span class="cf-name">{item.name}</span>
                    <span class="cf-size">{item.size}</span>
                    {#if chat.fileStatusText(item)}<span class="cf-status">{chat.fileStatusText(item)}</span>{/if}
                    {#if item.sending}<span class="cf-bar"><i style="width:{item.pct}%"></i></span>{/if}
                  </span>
                </div>
              {/if}
              <div class="chat-meta">{metaOf(item)}</div>
            </div>
          {/each}
        {/if}
      </div>

      <form
        class="chat-compose"
        onsubmit={(e) => {
          e.preventDefault();
          if (openPeer) send(openPeer, chat.draftOf(openPeer));
        }}
      >
        <input
          class="chat-input"
          bind:this={inputEl}
          type="text"
          placeholder="输入消息…"
          autocomplete="off"
          aria-label="发送消息"
          value={chat.draftOf(openPeer)}
          oninput={(e) => chat.setDraftOf(openPeer, e.currentTarget.value)}
          onkeydown={(e) => {
            if (e.key === 'Enter' && !e.shiftKey && !e.isComposing) {
              e.preventDefault();
              send(openPeer, chat.draftOf(openPeer));
            }
          }}
        />
        <button class="chat-plus" type="button" aria-label="发送文件（即推送）" onclick={() => onSendFiles([openPeer])}>
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round"><path d="M12 5v14M5 12h14" /></svg>
        </button>
        <button class="chat-send" type="submit" aria-label="发送消息" disabled={!chat.draftOf(openPeer).trim()}>
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M12 19V6" /><path d="M6 12l6-6 6 6" /></svg>
        </button>
      </form>
    </div>
  {/if}

  {#if batch}
    <!-- 群发框：**一次投递的载体**（标题就叫「群发」）。收件人在这里选、可以再加，但这里
         不存历史 —— 发出去之后记录落在各台设备的会话里，这个框随即消失。 -->
    <div class="convo-detail">
      <header class="conv-top">
        <span class="conv-title">群发</span>
        <span class="conv-count num">{batch.recipients.length} 台</span>
        <div class="conv-acts">
          <button class="icon-btn" type="button" aria-label="关闭群发" onclick={() => chat.closeBatch()}>
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"><path d="M6 6l12 12M18 6 6 18" /></svg>
          </button>
        </div>
      </header>

      <div class="recip">
        <span class="recip-label">发送给</span>
        <div class="recip-chips">
          {#each batch.recipients as peerId (peerId)}
            {@const dev = devices.find((d) => d.id === peerId)}
            <span class="recip-chip">
              <span class="rc-av">{chat.peerInitial(peerId)}</span>
              <span class="rc-name">{dev?.name ?? peerId}</span>
              <span class="rc-st" class:ok={dev?.connected}></span>
              <button
                class="rc-x"
                type="button"
                aria-label="从收件人里移除"
                onclick={() => chat.setBatchRecipients(batch.recipients.filter((id) => id !== peerId))}
              >
                <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><path d="M6 6l12 12M18 6 6 18" /></svg>
              </button>
            </span>
          {/each}
        </div>
        <button class="recip-add" type="button" aria-label="添加收件人" onclick={() => chat.askRecipients(batch.recipients, (ids) => chat.setBatchRecipients(ids))}>
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round"><path d="M12 5v14M5 12h14" /></svg>
          添加
        </button>
      </div>

      <div class="chat-log">
        <div class="chat-empty">
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><path d={ICON_CHAT} /></svg>
          <p>群发不留历史</p>
          <p class="ce-sub">这条会分别落到每台设备的会话里 —— 之后在各自的会话里继续聊。</p>
        </div>
      </div>

      <form
        class="chat-compose"
        onsubmit={(e) => {
          e.preventDefault();
          sendBatch();
        }}
      >
        <input
          class="chat-input"
          bind:this={batchInputEl}
          type="text"
          placeholder="输入要群发的消息…"
          autocomplete="off"
          aria-label="输入要群发的消息"
          value={batch.draft}
          oninput={(e) => chat.setBatchDraft(e.currentTarget.value)}
          onkeydown={(e) => {
            if (e.key === 'Enter' && !e.shiftKey && !e.isComposing) {
              e.preventDefault();
              sendBatch();
            }
          }}
        />
        <!-- 「＋」＝选文件推送给这几台（各自一条邀请、各自一条进度；记录一样各落各的会话）。
             推完**框不关**：文件有它自己那张进度卡在管，而这个框是"我正在选的收件人"，
             关它该由用户决定（✕ / Esc / 发完那句话）。 -->
        <button class="chat-plus" type="button" aria-label="发送文件（即推送）" onclick={() => onSendFiles(batch.recipients)}>
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round"><path d="M12 5v14M5 12h14" /></svg>
        </button>
        <button
          class="chat-send"
          type="submit"
          aria-label="群发"
          disabled={batch.recipients.length === 0 || !batch.draft.trim()}
        >
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="M12 19V6" /><path d="M6 12l6-6 6 6" /></svg>
        </button>
      </form>
    </div>
  {/if}
</section>