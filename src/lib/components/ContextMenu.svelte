<script lang="ts">
  import { onMount } from 'svelte';
  import { ctxPlacement } from '$lib/ctxPlacement';

  let {
    x, y,
    kind,
    shareId, path, isDir,
    deviceState,
    deviceOnline = true,
    deviceNeedsPair = false,
    peerName,
    bcAvailable = true,
    bcLeftLabel = null,
    bcRightLabel = null,
    bcCompareLabel = '',
    bcCompareEnabled = true,
    bcThisStaged = false,
    chatSel = false,
    convoBrowse = false,
    convoThread = false,
    convoBc = false,
    onConvoUpgrade,
    onClose,
    onToast,
    onDownload,
    onOpen,
    onCopy,
    onRemove,
    onConnect,
    onDisconnect,
    onPair,
    onUnpair,
    onPeerDownload,
    onBcCompare,
    onBcAddLeft,
    onBcAddRight,
    onBcClear,
    onChatCopy,
    onConvoFiles,
    onConvoEdit,
    onConvoExit,
    onMessage,
  }: {
    x: number; y: number;
    /** `chat` = 会话里那条消息（复制）；`convo` = 会话本身的菜单。 */
    kind: 'share' | 'device' | 'peer' | 'chat' | 'convo';
    shareId?: string;
    path?: string;
    isDir?: boolean;
    deviceState?: string;
    /** 右击的设备此刻是否在线：离线时只剩「解除配对」是本机动作。 */
    deviceOnline?: boolean;
    /** token 已失效（对端不认我们）：这一档的动作是「配对」，不是「连接」。 */
    deviceNeedsPair?: boolean;
    peerName?: string;
    /** 本机是否探测到 BeyondCompare（后端 `bc_status.exePath` 非空）；false = 整个 BC 菜单区不显示。 */
    bcAvailable?: boolean;
    /** 已在比较位上的项（null = 空），文案由后端给的 label 提供。 */
    bcLeftLabel?: string | null;
    bcRightLabel?: string | null;
    /** 追加在「用 BeyondCompare 比较」后的对手方说明；'' = 无可比对象。 */
    bcCompareLabel?: string;
    bcCompareEnabled?: boolean;
    /** 当前右键的这一项是否已在某个比较位上。 */
    bcThisStaged?: boolean;
    /** 消息右键时选区里那段可复制的文字（空 = 不显示「复制所选文字」）。 */
    chatSel?: boolean;
    /** 会话菜单：「查看该设备文件」只在对方**正处在长连接**时给 —— 对端浏览标签只存在于长连接设备。 */
    convoBrowse?: boolean;
    /** 会话菜单：这是不是一个**多台会话**（群发 / 群聊）—— 决定有哪些项、又是哪几句话。 */
    convoThread?: boolean;
    /** 会话菜单：多台会话里这是**群发**（否则是群聊）—— 只影响文案。 */
    convoBc?: boolean;
    /**
     * 会话菜单：把这份**群发**升级成群聊（拿那份名单建群，历史不迁）。不传 = 不画这一项 ——
     * 菜单里不该出现"点了没反应"的项，只有调用方真接得上这条路时才长出来。
     */
    onConvoUpgrade?: () => void;
    onClose: () => void;
    onToast: (msg: string) => void;
    onDownload: () => void;
    onOpen: () => void;
    onCopy: () => void;
    onRemove: () => void;
    onConnect: () => void;
    onDisconnect: () => void;
    onPair: () => void;
    onUnpair: () => void;
    onPeerDownload: () => void;
    onBcCompare: () => void;
    onBcAddLeft: () => void;
    onBcAddRight: () => void;
    onBcClear: () => void;
    /** 复制会话里的一条消息：`all` = 整条，`sel` = 只复制选中的那段。 */
    onChatCopy: (which: 'all' | 'sel') => void;
    /** 会话菜单：切到那台设备的文件标签页（一对一才有）。 */
    onConvoFiles: () => void;
    /** 会话菜单：编辑收件人与名字（多台会话才有）。 */
    onConvoEdit: () => void;
    /** 会话菜单：删除群发（消息留在各台会话里）/ 退出群聊（本地删除，并把"没有本机了"的名册广播给其他成员）。 */
    onConvoExit: () => void;
    /** 设备菜单：打开与这台设备的会话。 */
    onMessage: () => void;
  } = $props();

  function handleItem(fn: () => void) {
    fn();
  }

  let root = $state<HTMLDivElement | null>(null);

  /**
   * 菜单在挂载期间独占「点击任意位置关闭」的职责：组件只在 ctxState 非空时存在，
   * 因此这里的监听生命周期就等于菜单的可见期。
   *
   * 用 pointerdown（捕获阶段）而不是 click：列表行会 stopPropagation 或消费 click，
   * 只有 pointerdown 能保证任何位置按下都先到这里。
   */
  onMount(() => {
    function onPointerDown(e: PointerEvent) {
      if (!root?.contains(e.target as Node)) onClose();
    }
    function onKeyDown(e: KeyboardEvent) {
      if (e.key === 'Escape') onClose();
    }
    document.addEventListener('pointerdown', onPointerDown, true);
    document.addEventListener('keydown', onKeyDown);
    return () => {
      document.removeEventListener('pointerdown', onPointerDown, true);
      document.removeEventListener('keydown', onKeyDown);
    };
  });

  /**
   * 菜单是 `position: fixed` 且挂在 `.orb-stage` 之外（跨层叠上下文永远在最上），
   * 参照系就是视口；面板窗口本身即视口，所以直接量 `innerWidth/innerHeight`。
   * 每次打开都是一次新挂载，所以这个值跟着当前窗口大小走。
   */
  const bounds = { width: window.innerWidth, height: window.innerHeight };
</script>

<!-- BeyondCompare 菜单区：本机共享项与对端共享项共用同一份结构 -->
{#snippet bcSection()}
  <div class="ctx-sep"></div>
  {#if bcLeftLabel}
    <button class="ctx-item" disabled>
      <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6"><circle cx="12" cy="12" r="3"/></svg>
      左：{bcLeftLabel}
    </button>
  {/if}
  {#if bcRightLabel}
    <button class="ctx-item" disabled>
      <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6"><circle cx="12" cy="12" r="3"/></svg>
      右：{bcRightLabel}
    </button>
  {/if}
  <button class="ctx-item" disabled={!bcCompareEnabled} onclick={() => handleItem(onBcCompare)}>
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linejoin="round"><rect x="3" y="4" width="7" height="16" rx="1.5"/><rect x="14" y="4" width="7" height="16" rx="1.5"/></svg>
    用 BeyondCompare 比较{bcCompareLabel ? ` · ${bcCompareLabel}` : ''}
  </button>
  <button class="ctx-item" onclick={() => handleItem(onBcAddLeft)}>
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M20 12H4M10 6l-6 6 6 6"/></svg>
    添加到左侧比较位
  </button>
  <button class="ctx-item" onclick={() => handleItem(onBcAddRight)}>
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M4 12h16M14 6l6 6-6 6"/></svg>
    添加到右侧比较位
  </button>
  {#if bcThisStaged}
    <button class="ctx-item" onclick={() => handleItem(onBcClear)}>
      <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="9"/><path d="M9 9l6 6M15 9l-6 6"/></svg>
      清除比较位
    </button>
  {/if}
{/snippet}

<!-- svelte-ignore a11y_click_events_have_key_events a11y_no_static_element_interactions -->
<div class="ctx show" bind:this={root} data-od-id="ctx-menu" use:ctxPlacement={{ x, y, bounds }} role="menu" tabindex="-1" onclick={onClose}>
  {#if kind === 'chat'}
    {#if chatSel}
      <button class="ctx-item" onclick={() => handleItem(() => onChatCopy('sel'))}>
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M4 7h11v13H4z"/><path d="M8 4h12v12"/></svg>
        复制所选文字
      </button>
    {/if}
    <button class="ctx-item" onclick={() => handleItem(() => onChatCopy('all'))}>
      <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M4 7h11v13H4z"/><path d="M8 4h12v12"/></svg>
      复制整条消息
    </button>
  {:else if kind === 'convo'}
    {#if !convoThread && convoBrowse}
      <button class="ctx-item" onclick={() => handleItem(onConvoFiles)}>
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M3 7a2 2 0 0 1 2-2h4.2a2 2 0 0 1 1.4.6L12 7h7a2 2 0 0 1 2 2v9a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V7Z"/></svg>
        查看该设备文件
      </button>
    {/if}
    {#if convoThread}
      <button class="ctx-item" onclick={() => handleItem(onConvoEdit)}>
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M16 19v-1a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v1"/><circle cx="9" cy="7" r="3"/><path d="M22 19v-1a4 4 0 0 0-3-3.87"/><path d="M16 4.13a4 4 0 0 1 0 7.75"/></svg>
        {convoBc ? '编辑收件人与群发名称' : '编辑参与者与群聊名称'}
      </button>
    {/if}
    <!-- 群发 → 群聊：拿同一份名单建一个群（没有新后端命令，就是 `group_create`）。
         `title` 把"不迁历史"摆在明面上：那些消息本来就在各端 1:1 里，对端也从没见过它属于一个群。 -->
    {#if convoBc && onConvoUpgrade}
      <button
        class="ctx-item"
        title="拿这份名单建一个群，之后的讨论共享同一条消息流。历史不迁移：已经发出的消息仍在各设备的单独会话里（对端也从不知道它属于一个群）。"
        onclick={() => handleItem(onConvoUpgrade)}
      >
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"><path d="M12 18V8"/><path d="m8 12 4-4 4 4"/><path d="M5 20h14"/></svg>
        升级为群聊
      </button>
    {/if}
    <button class="ctx-item danger" onclick={() => handleItem(onConvoExit)}>
      <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M4 7h16M9 7V4h6v3M6 7l1 13h10l1-13M10 11v6M14 11v6"/></svg>
      {convoThread ? (convoBc ? '删除群发（消息留在各设备会话）' : '退出群聊（本地删除并广播离开）') : '清空聊天记录'}
    </button>
  {:else if kind === 'peer'}
    <button class="ctx-item" onclick={() => handleItem(onPeerDownload)}>
      <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M12 3v12M7 10l5 5 5-5M5 19h14"/></svg>
      下载到本地{peerName ? ` · 来自「${peerName}」` : ''}
    </button>
    {#if bcAvailable}{@render bcSection()}{/if}
  {:else if kind === 'share'}
    <button class="ctx-item" onclick={() => handleItem(onDownload)}>
      <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M12 3v12M7 10l5 5 5-5M5 19h14"/></svg>
      下载到本地
    </button>
    <button class="ctx-item" onclick={() => handleItem(onOpen)}>
      <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M3 7a2 2 0 0 1 2-2h4.2a2 2 0 0 1 1.4.6L12 7h7a2 2 0 0 1 2 2v9a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V7Z"/></svg>
      打开所在文件夹
    </button>
    <button class="ctx-item" onclick={() => handleItem(onCopy)}>
      <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M4 7h11v13H4z"/><path d="M8 4h12v12"/></svg>
      复制共享路径
    </button>
    <div class="ctx-sep"></div>
    <button class="ctx-item danger" onclick={() => handleItem(onRemove)}>
      <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M4 7h16M9 7V4h6v3M6 7l1 13h10l1-13M10 11v6M14 11v6"/></svg>
      移除共享项
    </button>
    {#if bcAvailable}{@render bcSection()}{/if}
  {:else}
    {#if !deviceOnline}
      <!-- 离线设备：只有「解除配对」是本机动作（它不依赖长连接）。会话仍然打得开、消息也发得
           出去（排着队等它上线，见 chat::flush_queued），但设备页这一行按 F-14 的离线行规则
           不给这两个动作。 -->
    {:else if deviceState === 'connected'}
      <button class="ctx-item" onclick={() => handleItem(onDisconnect)}>
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M10 13a5 5 0 0 0 7 0l3-3a5 5 0 0 0-7-7l-1.5 1.5"/><path d="M14 11a5 5 0 0 0-7 0l-3 3a5 5 0 0 0 7 7l1.5-1.5"/><path d="M3 3l18 18"/></svg>
        断开长连接
      </button>
    {:else if deviceNeedsPair}
      <!-- token 已失效：拨号必然 401，唯一出路是重新配对 -->
      <button class="ctx-item" onclick={() => handleItem(onPair)}>
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M12 5v14M5 12h14"/></svg>
        重新配对
      </button>
    {:else if deviceState === 'unbound'}
      <!-- 未绑定：只做两件事 —— 配对，或开会话发消息（消息走"长连接优先 + HTTP 兜底"，
           未建链时对端还收不到就排队等它上线）；推送在会话里那颗「＋」，这里不再重复摆一份。
           没有「立即连接」（未配对拨不动），也没有「解除配对」（没有关系可解）。 -->
      <button class="ctx-item" onclick={() => handleItem(onPair)}>
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M12 5v14M5 12h14"/></svg>
        配对
      </button>
    {:else}
      <button class="ctx-item" onclick={() => handleItem(onConnect)}>
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M10 13a5 5 0 0 0 7 0l3-3a5 5 0 0 0-7-7l-1.5 1.5"/><path d="M14 11a5 5 0 0 0-7 0l-3 3a5 5 0 0 0 7 7l1.5-1.5"/></svg>
        立即连接
      </button>
    {/if}
    {#if deviceOnline}
      <button class="ctx-item" onclick={() => handleItem(onMessage)}>
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M22 2 11 13"/><path d="M22 2 15 22 11 13 2 9 22 2Z"/></svg>
        发消息
      </button>
    {/if}
    {#if deviceState !== 'unbound'}
      <div class="ctx-sep"></div>
      <button class="ctx-item danger" onclick={() => handleItem(onUnpair)}>
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M4 7h16M9 7V4h6v3M6 7l1 13h10l1-13M10 11v6M14 11v6"/></svg>
        解除配对
      </button>
    {/if}
  {/if}
</div>

<style>
  /* 比较位提示行 / 无可比对象的比较项：沿用 .ctx-item 排版，但不可点也不高亮。
     pointer-events:none 让点击落到菜单根节点上，等于「点空白关闭菜单」。 */
  .ctx-item:disabled {
    opacity: 0.55;
    color: var(--muted);
    cursor: default;
    pointer-events: none;
  }
</style>