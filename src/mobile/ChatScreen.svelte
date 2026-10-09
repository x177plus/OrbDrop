<script lang="ts">
  import * as chat from "$lib/stores/chat.svelte";
  import type { ChatItem, Receipt } from "$lib/stores/chat.svelte";
  import * as devices from "$lib/stores/devices.svelte";
  import Icon from "./Icon.svelte";
  import * as store from "./store.svelte";

  let { convoKey }: { convoKey: string } = $props();

  // 外壳只在 `activeThread()` 非空时才挂这个组件（`store.openChatKey()` 就是它），所以这里把
  // 那几个字段拆开用 —— 模板里不用到处判空；键失效时它们落到"空会话"的默认值，界面自己退回列表。
  const thread = $derived(chat.activeThread());
  const mode = $derived(thread?.mode ?? "direct");
  const participants = $derived(thread?.participants ?? []);
  const title = $derived(thread?.title ?? "");
  const items = $derived(chat.itemsOf(convoKey));

  let logEl = $state<HTMLDivElement | null>(null);
  let inputEl = $state<HTMLInputElement | null>(null);

  const draft = $derived(chat.draftOf(convoKey));
  const dot = $derived(devices.threadDotState(participants));
  const peers = $derived(devices.getDevices().filter((d) => participants.includes(d.id)));
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
  const relayed = $derived(peers.find((d) => d.relayVia));
  /** 原型 `subText`：一对一按链路/可达性写，多台会话写"几台送得到"。 */
  const sub = $derived(
    mode === "direct"
      ? linked > 0
        ? "长连接 · 在线"
        : reachable > 0
          ? relayed
            ? `经 ${relayed.relayVia} 中转 · 在线`
            : "在线"
          : "离线 · 消息不会送达"
      : `${reachable}/${participants.length} 台在线`,
  );

  /** 发言人的设备 id：群消息带 `from`，本机内存里的文件消息带 `peerId`（群里的文件只会是本机发的）。 */
  function speakerOf(item: ChatItem): string {
    if (item.kind === "file") return item.peerId;
    return "from" in item ? item.from : "";
  }

  /**
   * 发言人的显示名：颜色**不**跟着它走（名字可改），这里只是给人看的。
   * 名册里的成员可能不在本机设备表里，那时退回设备表里的名字 / 设备 id（`deviceNameOf` 兜底）。
   */
  function speakerName(item: ChatItem): string {
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
      const status = mode === "group" ? chat.groupReceiptText(item) : chat.deliveryText(participants[0], item);
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

  async function send(): Promise<void> {
    const text = chat.draftOf(convoKey).trim();
    if (!text) return;
    try {
      const res = await chat.sendText(convoKey, text);
      if (!res) return;
    } catch (e) {
      // 发失败**不清草稿**：原型是 mock 不会失败，真发不出去还把刚打的字抹掉，用户得重打一遍。
      store.failed("发送消息", e);
      return;
    }
    chat.setDraftOf(convoKey, "");
    inputEl?.focus();
  }

  function onKey(e: KeyboardEvent): void {
    // 中文/日文输入法里那下回车是"上屏"，不是"发送"（原型同款判断）。
    if (e.key !== "Enter" || e.isComposing) return;
    e.preventDefault();
    void send();
  }

  // 来了新消息就贴到底（读 `items.length` 而不是 `items`：那条文件消息的进度每秒都在变，
  // 每变一次就把滚动位置拽走会让人没法往上翻）。`convoKey` 也要读 —— 换一条会话时哪怕条数一样，
  // 也得先贴到底；`logEl` 从 null 变成节点那一下同理。
  $effect(() => {
    void convoKey;
    void items.length;
    if (logEl) logEl.scrollTop = logEl.scrollHeight;
  });
</script>

<header class="chatbar">
  <button class="icon-btn" type="button" aria-label="返回会话列表" onclick={() => store.navigateBackToList()}>
    <Icon name="back" />
  </button>
  <div class="cb-main">
    <span class="cb-title">{title}</span>
    <span class="cb-sub"><i class="cv-dot {dot}"></i>{sub}</span>
  </div>
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

<div class="chatlog" bind:this={logEl} role="log" aria-label="会话记录">
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
        <div class="bubble">
          {#if item.kind === "file"}
            <span class="fbub">
              <span class="fio"><Icon name="file" /></span>
              <span class="fmain">
                <span class="fname">{item.name}</span>
                <span class="fsize num">{item.size || "文件"}</span>
                {#if item.sending}
                  <span class="fstat">传输中 {Math.round(item.pct)}%</span>
                  <span class="fbar"><i style="width:{Math.round(item.pct)}%"></i></span>
                {:else if fileStatus(item)}
                  <span class="fstat">{fileStatus(item)}</span>
                {/if}
              </span>
            </span>
          {:else}
            <span class="ctext">{item.text}</span>
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

<!-- 底部那颗 `＋`（发送文件）本阶段不放：手机选文件要走 content URI 导入，还没做。 -->
<form
  class="compose"
  onsubmit={(e) => {
    e.preventDefault();
    void send();
  }}
>
  <input
    class="cinput"
    type="text"
    placeholder="发送消息…"
    autocomplete="off"
    aria-label="消息内容"
    bind:this={inputEl}
    value={draft}
    oninput={(e) => chat.setDraftOf(convoKey, e.currentTarget.value)}
    onkeydown={onKey}
  />
  <button class="cbtn csend" type="submit" aria-label="发送消息" disabled={!draft.trim()}>
    <Icon name="send" />
  </button>
</form>