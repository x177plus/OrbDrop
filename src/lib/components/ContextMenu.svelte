<script lang="ts">
  import { onMount } from 'svelte';
  import { ctxPlacement } from '$lib/ctxPlacement';
  import { t } from '$lib/i18n.svelte';

  let {
    x, y,
    kind,
    shareId, path, isDir,
    deviceState,
    deviceOnline = true,
    deviceNeedsPair = false,
    deviceRemark = null,
    peerName,
    bcAvailable = true,
    bcLeftLabel = null,
    bcLeftExists = true,
    bcCompareLabel = '',
    bcCompareEnabled = true,
    chatSel = false,
    chatQuote = false,
    convoBrowse = false,
    convoThread = false,
    convoBc = false,
    compact = false,
    pending = false,
    pinned = false,
    onConvoUpgrade,
    onClose,
    onToast,
    onDownload,
    onOpen,
    /** 共享项（本机）文件：用系统默认程序打开这一项自己。不传 = 不画「打开」。 */
    onOpenLocal,
    /** 共享项（本机/对端）文件：删除磁盘上的这一份。不传 = 不画「删除文件」。 */
    onDeleteEntry,
    onCopy,
    onCopyUrl,
    onRemove,
    onConnect,
    onDisconnect,
    onPair,
    onUnpair,
    onForget,
    onRemark,
    onPeerDownload,
    /** 设备页的对端项：用系统默认程序打开（Windows 是 CFAPI 占位符，按需水合）。 */
    onPeerOpen,
    /** 设备页的文件：打开本机工作副本所在的文件夹（与「打开」同一份本地副本）。不传 = 不画。 */
    onPeerOpenFolder,
    /** 设备页的文件：强制写回。只在冲突/自动写回停住时给；跳过冲突核对直接覆盖对端。 */
    onPeerForceWriteback,
    /** 设备页的目录：把本机文件/目录上传到这一项（只对目录给；`onPeerUpload` 不传就不画）。 */
    onPeerUpload,
    /** 共享项（本机或对端）文件/目录：查看更新记录。 */
    onShareHistory,
    onBcCompare,
    onBcAddLeft,
    onChatCopy,
    onChatQuote,
    onConvoFiles,
    onConvoEdit,
    onConvoClear,
    onConvoExit,
    onMessage,
  }: {
    x: number; y: number;
    /** `chat` = 会话里那条消息（复制）；`convo` = 会话本身的菜单。 */
    kind: 'share' | 'device' | 'peer' | 'chat' | 'convo';
    shareId?: string;
    path?: string;
    isDir?: boolean;
    /**
     * 设备/会话菜单：这台设备的状态档（`connected` / `paired` / `unbound`）。
     * **一对一会话菜单也填它**（那两项「修改备注 / 配对」的判据就在这里，见 `App.openConvoCtx`）：
     * 留空 = 这台设备已经不在设备表里，会话菜单不摆那两项。
     */
    deviceState?: string;
    /** 右击的设备此刻是否在线：离线时只剩「解除配对」是本机动作。 */
    deviceOnline?: boolean;
    /** token 已失效（对端不认我们）：这一档的动作是「配对」，不是「连接」。 */
    deviceNeedsPair?: boolean;
    /** 这台设备当前的备注（F-101）；`null`/缺省 = 没备注。菜单里顺手把它摆出来。 */
    deviceRemark?: string | null;
    peerName?: string;
    /** 本机是否探测到 BeyondCompare（后端 `bc_status.exePath` 非空）；false = 整个 BC 菜单区不显示。 */
    bcAvailable?: boolean;
    /** 左侧槽里的那一项（null = 还没人放过左侧），文案由后端给的 label 提供。 */
    bcLeftLabel?: string | null;
    /** 槽里那条此刻还在不在（本机路径被删/改名 → 把那行标出来）。 */
    bcLeftExists?: boolean;
    /** 追加在「用 BeyondCompare 比较」后的对手方说明；'' = 没有对手方（槽空，或右键的正是槽里那一项）。 */
    bcCompareLabel?: string;
    bcCompareEnabled?: boolean;
    /** 消息右键时选区里那段可复制的文字（空 = 不显示「复制所选文字」）。 */
    chatSel?: boolean;
    /**
     * 消息右键时给不给「引用」：LocalSend 会话没有这套线上协议，调用方传 false。
     * `onChatQuote` 不传 = 不画引用项。
     */
    chatQuote?: boolean;
    /** 会话菜单：「查看该设备文件」只在对方**正处在长连接**时给 —— 对端浏览标签只存在于长连接设备。 */
    convoBrowse?: boolean;
    /** 会话菜单：这是不是一个**多台会话**（群发 / 群聊）—— 决定有哪些项、又是哪几句话。 */
    convoThread?: boolean;
    /** 会话菜单：多台会话里这是**群发**（否则是群聊）—— 只影响文案。 */
    convoBc?: boolean;
    /**
     * 卡片上开的那一份菜单（`ctxState.orb`）：本机页与设备页共用这一档 —— 行距、字号与文案
     * 都收紧。它会**按菜单实际尺寸把折叠态窗口撑大**（`App.placeOrbMenu`），所以 BC 区
     * （装了 BC 才画）照摆；本机共享项那一份仍不摆「下载到本地」（目录复制到下载目录是另一个
     * 语义，见 `ctxDownload`），设备页那一份的「下载到本地」不受影响。
     * 也**不摆「更新记录…」与「上传到此文件夹…」**：历史浮层（`UpdateHistory`）与本机文件
     * 选择框（`FilePicker`）都只长在展开的面板里，折起来时窗口装不下 —— 要用先展开主界面。
     */
    compact?: boolean;
    /**
     * 还没定好落点：菜单**已经渲染、但先不显形**（`opacity: 0`），只为让人量出它真实多大
     * （折叠态那一档要按它把窗口撑到装得下，见 `App.placeOrbMenu`）。定好落点后再放开显形，
     * 于是那 0.12s 的淡入/缩放是从**最终位置**开始的，用户看不到菜单先盖在卡片上再挪开。
     */
    pending?: boolean;
    /**
     * 落点已由调用方按**屏幕工作区**定好（折叠态卡片那一档）：`ctxPlacement` 一个像素都不动。
     * 那一档的窗口是照着菜单撑大的，拿窗口当夹取框只会把菜单又推回卡片上。
     */
    pinned?: boolean;
    /**
     * 会话菜单：把这份**群发**升级成群聊（拿那份名单建群，历史不迁）。不传 = 不画这一项 ——
     * 菜单里不该出现"点了没反应"的项，只有调用方真接得上这条路时才长出来。
     */
    onConvoUpgrade?: () => void;
    onClose: () => void;
    onToast: (msg: string) => void;
    onDownload: () => void;
    onOpen: () => void;
    /** 共享项（本机）文件：打开文件自己（不是所在文件夹）。 */
    onOpenLocal?: () => void;
    /** 共享项（本机/对端）文件：删除这一份（目录与共享项根不画）。 */
    onDeleteEntry?: () => void;
    onCopy: () => void;
    /**
     * 共享项菜单：「复制下载链接」—— 只有共享项**开着广播开关**时调用方才传这个回调
     * （后端 `/s/<shareId>` 的 gate 就是同一个字段）。不传 = 不画这一项。
     */
    onCopyUrl?: () => void;
    onRemove: () => void;
    onConnect: () => void;
    onDisconnect: () => void;
    onPair: () => void;
    onUnpair: () => void;
    /** 未配对设备：忘掉它（只清本机记录，不通知对端）。不传 = 不画这一项。 */
    onForget?: () => void;
    /** 设备 / 一对一会话菜单：改本机给这台设备起的备注（F-101）—— 纯本机动作，任何状态的行都给。 */
    onRemark: () => void;
    onPeerDownload: () => void;
    /** 设备页的对端项：打开（返回本地路径后由调用方交给系统）。不传 = 不画这一项。 */
    onPeerOpen?: () => void;
    /** 设备页的文件：打开本机工作副本所在的文件夹。不传 = 不画这一项。 */
    onPeerOpenFolder?: () => void;
    /**
     * 设备页的文件：强制写回。跳过"对端在打开后变过"的核对，直接用本地副本覆盖；
     * 仍拒绝目录。不传 = 不画这一项。
     */
    onPeerForceWriteback?: () => void;
    /** 共享项（本机或对端）文件/目录：查看更新记录。不传 = 不画。 */
    onShareHistory?: () => void;
    /** 设备页的目录：上传到这一项。不传 = 不画这一项（文件行没有"上传到文件里"这回事）。 */
    onPeerUpload?: () => void;
    onBcCompare: () => void;
    onBcAddLeft: () => void;
    /** 复制会话里的一条消息：`all` = 整条，`sel` = 只复制选中的那段。 */
    onChatCopy: (which: 'all' | 'sel') => void;
    /** 引用会话里的一条消息：`all` = 整条，`sel` = 只引用选中的那段。不传 = 不画。 */
    onChatQuote?: (which: 'all' | 'sel') => void;
    /** 会话菜单：切到那台设备的文件标签页（一对一才有）。 */
    onConvoFiles: () => void;
    /** 会话菜单：编辑收件人与名字（多台会话才有）。 */
    onConvoEdit: () => void;
    /** 会话菜单：清空群聊的本地记录（**不退群**）。 */
    onConvoClear: () => void;
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
   *
   * 按在菜单**外面**的那一下在关掉菜单之后**就地吃掉**（`stopPropagation`，捕获阶段挡住整条
   * 下行/冒泡）：悬浮框上"第一下只关菜单"就靠它 —— 不然这一下会顺手把面板展开 / 把页翻过去，
   * 而卡片侧没法自己判断"这一下是来关菜单的"（菜单已经关了，它读到的状态是关着的）。
   * 只挡事件传播，不 `preventDefault`：焦点那类默认动作照旧。
   */
  onMount(() => {
    function onPointerDown(e: PointerEvent) {
      if (root?.contains(e.target as Node)) return;
      onClose();
      e.stopPropagation();
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
   *
   * 这两个数**每次落点都现读**（别抄成常量）：折叠态那一档菜单一开，窗口就先被撑大
   * （`App.placeOrbMenu`），落点要用撑大之后的窗口算。
   */
  const bounds = {
    get width() { return window.innerWidth; },
    get height() { return window.innerHeight; },
  };
</script>

<!-- BeyondCompare 菜单区：本机共享项与对端共享项共用同一份结构。
     左侧槽只有一个（BC 自己的 `Saved*Left`）：那两行读同一个槽 —— 资源管理器里放的左侧
     在这里照样显示，「添加到左侧比较位」就是覆盖它。 -->
{#snippet bcSection()}
  <div class="ctx-sep"></div>
  {#if bcLeftLabel}
    {@const leftText = t('左：{label}', { label: bcLeftLabel }) + (bcLeftExists ? '' : t('（已被移动或删除）'))}
    <button class="ctx-item" disabled title={compact ? leftText : undefined}>
      <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6"><circle cx="12" cy="12" r="3"/></svg>
      {#if compact}
        <span class="ctx-elide">{leftText}</span>
      {:else}
        {leftText}
      {/if}
    </button>
  {/if}
  <button class="ctx-item" disabled={!bcCompareEnabled} onclick={() => handleItem(onBcCompare)}>
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linejoin="round"><rect x="3" y="4" width="7" height="16" rx="1.5"/><rect x="14" y="4" width="7" height="16" rx="1.5"/></svg>
    {compact
      ? t('BC 比较')
      : bcCompareLabel
        ? t('用 BeyondCompare 比较 · 与左侧：{label}', { label: bcCompareLabel })
        : t('用 BeyondCompare 比较')}
  </button>
  <button class="ctx-item" onclick={() => handleItem(onBcAddLeft)}>
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M20 12H4M10 6l-6 6 6 6"/></svg>
    {compact ? t('BC 加到左侧') : t('添加到左侧比较位')}
  </button>
{/snippet}

<!-- svelte-ignore a11y_click_events_have_key_events a11y_no_static_element_interactions -->
<div class="ctx" class:show={!pending} class:is-compact={compact} bind:this={root} data-od-id="ctx-menu" use:ctxPlacement={{ x, y, bounds: pinned ? null : bounds }} role="menu" tabindex="-1" onclick={onClose}>
  {#if kind === 'chat'}
    {#if chatQuote && onChatQuote}
      {#if chatSel}
        <button class="ctx-item" onclick={() => handleItem(() => onChatQuote('sel'))}>
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M9 7H5v5h3c0 2-1 3-3 3v2c3 0 5-2 5-5V7Z"/><path d="M19 7h-4v5h3c0 2-1 3-3 3v2c3 0 5-2 5-5V7Z"/></svg>
          {t('引用所选文字')}
        </button>
      {/if}
      <button class="ctx-item" onclick={() => handleItem(() => onChatQuote('all'))}>
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M9 7H5v5h3c0 2-1 3-3 3v2c3 0 5-2 5-5V7Z"/><path d="M19 7h-4v5h3c0 2-1 3-3 3v2c3 0 5-2 5-5V7Z"/></svg>
        {t('引用整条消息')}
      </button>
      <div class="ctx-sep"></div>
    {/if}
    {#if chatSel}
      <button class="ctx-item" onclick={() => handleItem(() => onChatCopy('sel'))}>
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M4 7h11v13H4z"/><path d="M8 4h12v12"/></svg>
        {t('复制所选文字')}
      </button>
    {/if}
    <button class="ctx-item" onclick={() => handleItem(() => onChatCopy('all'))}>
      <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M4 7h11v13H4z"/><path d="M8 4h12v12"/></svg>
      {t('复制整条消息')}
    </button>
  {:else if kind === 'convo'}
    <!-- 一对一那条会话也摆着**设备那两项**（修改备注 / 配对·重新配对），与设备右键菜单
         同一份处理函数（`onRemark` / `onPair` 读的是 `ctxState.deviceId`，见 `App.openConvoCtx`）：
         会话正是"看着这台设备"的地方，要改对它的叫法或重新配对，不该先退回设备页去右键那一行。
         分档与设备菜单逐条同规 —— 备注与在线状态无关（纯本机数据，离线会话照样能改）；
         配对项只给「未绑定」与「凭据失效」两档（已配对的一对一会话没有配对可言）。
         `deviceState` 为空 = 这台设备已经不在设备表里（会话还留着，但那两项都没有主语）：不摆。 -->
    {#if !convoThread && deviceState}
      <button class="ctx-item" title={deviceRemark ?? undefined} onclick={() => handleItem(onRemark)}>
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M4 5h16M4 10h10M4 15h16M4 20h7"/></svg>
        {t('修改备注')}
        {#if deviceRemark}<span class="ctx-current">{deviceRemark}</span>{/if}
      </button>
      {#if deviceState === 'unbound' || deviceNeedsPair}
        <button class="ctx-item" onclick={() => handleItem(onPair)}>
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M12 5v14M5 12h14"/></svg>
          {deviceState === 'unbound' ? t('配对') : t('重新配对')}
        </button>
      {/if}
      <div class="ctx-sep"></div>
    {/if}
    {#if !convoThread && convoBrowse}
      <button class="ctx-item" onclick={() => handleItem(onConvoFiles)}>
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M3 7a2 2 0 0 1 2-2h4.2a2 2 0 0 1 1.4.6L12 7h7a2 2 0 0 1 2 2v9a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V7Z"/></svg>
        {t('查看该设备文件')}
      </button>
    {/if}
    {#if convoThread}
      <button class="ctx-item" onclick={() => handleItem(onConvoEdit)}>
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M16 19v-1a4 4 0 0 0-4-4H6a4 4 0 0 0-4 4v1"/><circle cx="9" cy="7" r="3"/><path d="M22 19v-1a4 4 0 0 0-3-3.87"/><path d="M16 4.13a4 4 0 0 1 0 7.75"/></svg>
        {convoBc ? t('编辑收件人与群发名称') : t('编辑参与者与群聊名称')}
      </button>
    {/if}
    <!-- 群发 → 群聊：拿同一份名单建一个群（没有新后端命令，就是 `group_create`）。
         `title` 把"不迁历史"摆在明面上：那些消息本来就在各端 1:1 里，对端也从没见过它属于一个群。 -->
    {#if convoBc && onConvoUpgrade}
      <button
        class="ctx-item"
        title={t('拿这份名单建一个群，之后的讨论共享同一条消息流。历史不迁移：已经发出的消息仍在各设备的单独会话里（对端也从不知道它属于一个群）。')}
        onclick={() => handleItem(onConvoUpgrade)}
      >
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.7" stroke-linecap="round" stroke-linejoin="round"><path d="M12 18V8"/><path d="m8 12 4-4 4 4"/><path d="M5 20h14"/></svg>
        {t('升级为群聊')}
      </button>
    {/if}
    {#if convoThread && !convoBc}
      <button class="ctx-item" onclick={() => handleItem(onConvoClear)}>
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M4 7h16M9 7V4h6v3M6 7l1 13h10l1-13"/><path d="M10 11v5M14 11v5"/></svg>
        {t('清空聊天记录（不退群）')}
      </button>
    {/if}
    <button class="ctx-item danger" onclick={() => handleItem(onConvoExit)}>
      <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M4 7h16M9 7V4h6v3M6 7l1 13h10l1-13M10 11v6M14 11v6"/></svg>
      {convoThread ? (convoBc ? t('删除群发（消息留在各设备会话）') : t('退出群聊（本地删除并广播离开）')) : t('清空聊天记录')}
    </button>
  {:else if kind === 'peer'}
    <!-- 第一项永远是「打开」（树根文件与目录都适用）；随后是"拿到本地"的操作。 -->
    {#if onPeerOpen}
      <button class="ctx-item" onclick={() => handleItem(onPeerOpen)}>
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M14 3h7v7"/><path d="M10 14 21 3"/><path d="M21 14v5a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5"/></svg>
        {t('打开')}
      </button>
    {/if}
    <!-- 文件：打开本机工作副本所在的文件夹（与「打开」同一份本地副本）；目录的「打开」
         就是进它自己，不再重复一项。 -->
    {#if onPeerOpenFolder && !isDir}
      <button class="ctx-item" onclick={() => handleItem(onPeerOpenFolder)}>
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M3 7a2 2 0 0 1 2-2h4.2a2 2 0 0 1 1.4.6L12 7h7a2 2 0 0 1 2 2v9a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V7Z"/></svg>
        {compact ? t('打开文件夹') : t('打开所在文件夹')}
      </button>
    {/if}
    <!-- 卡片上的这一档已经由页头写着设备名（那一页就是它），文案不再重复一次。 -->
    <button class="ctx-item" onclick={() => handleItem(onPeerDownload)}>
      <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M12 3v12M7 10l5 5 5-5M5 19h14"/></svg>
      {compact ? t('下载到本地') : peerName ? t('下载到本地 · 来自「{name}」', { name: peerName }) : t('下载到本地')}
    </button>
    <!-- 卡片这一档不摆「上传到此文件夹…」与「更新记录…」：挑本机文件/目录的选择框、历史浮层
         都长在**展开的面板**里，折起来时 236×162 的窗口装不下（点了界面也出不来）—— 要用就
         先展开主界面。面板里那两处入口照旧。 -->
    {#if onPeerUpload && isDir && !compact}
      <button class="ctx-item" onclick={() => handleItem(onPeerUpload)}>
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M12 21V9M7 14l5-5 5 5M5 5h14"/></svg>
        {t('上传到此文件夹…')}
      </button>
    {/if}
    <!-- 自动写回负责正常保存；手动只留冲突时的「强制写回」。普通写回和"用远端覆盖"
         不再占菜单：前者由 `mirror::auto` 自动做，后者直接用「下载到本地」取一份即可。 -->
    {#if onPeerForceWriteback && !isDir && !compact}
      <button class="ctx-item danger" title={t('不核对对端是否在打开后变过；用本地副本直接覆盖（可能丢掉对方的修改）')} onclick={() => handleItem(onPeerForceWriteback)}>
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M12 3 2 21h20L12 3Z"/><path d="M12 9v6M12 18h.01"/></svg>
        {compact ? t('强制写回') : t('强制写回（覆盖远端更新）')}
      </button>
    {/if}
    {#if onShareHistory && !compact}
      <button class="ctx-item" onclick={() => handleItem(onShareHistory)}>
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="9"/><path d="M12 7v5l3 2"/></svg>
        {t('更新记录…')}
      </button>
    {/if}
    <!-- 删除放第一组的最后（本机 / 对端同一位置），红色。共享项根（单文件共享 / 树根，
         path 为空）不给 —— 那等于删掉用户磁盘上的源文件，和「移除共享项」是两回事。
         目录递归删除由后端执行，确认框会明确写出"及其全部内容"。 -->
    {#if onDeleteEntry && path}
      <button
        class="ctx-item delete"
        title={isDir ? t('从对方设备的共享目录中删除这个文件夹及其全部内容，无法恢复') : t('从对方设备的共享目录中删除这个文件，无法恢复')}
        onclick={() => handleItem(onDeleteEntry)}
      >
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M4 7h16M9 7V4h6v3M6 7l1 13h10l1-13M10 11v6M14 11v6"/></svg>
        {isDir
          ? (compact ? t('删除文件夹') : t('删除文件夹…'))
          : (compact ? t('删除文件') : t('删除文件…'))}
      </button>
    {/if}
    {#if bcAvailable}{@render bcSection()}{/if}
  {:else if kind === 'share'}
    <!-- 第一项永远是「打开」：文件用系统默认程序打开自己，目录进文件管理器。
         「打开所在文件夹」只对文件给（目录的「打开」就是进它自己，不再重复一项）。 -->
    {#if onOpenLocal}
      <button class="ctx-item" onclick={() => handleItem(onOpenLocal)}>
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M14 3h7v7"/><path d="M10 14 21 3"/><path d="M21 14v5a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5"/></svg>
        {t('打开')}
      </button>
    {/if}
    {#if !isDir}
      <button class="ctx-item" onclick={() => handleItem(onOpen)}>
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M3 7a2 2 0 0 1 2-2h4.2a2 2 0 0 1 1.4.6L12 7h7a2 2 0 0 1 2 2v9a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V7Z"/></svg>
        {compact ? t('打开文件夹') : t('打开所在文件夹')}
      </button>
    {/if}
    {#if !compact && !isDir}
      <button class="ctx-item" onclick={() => handleItem(onDownload)}>
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M12 3v12M7 10l5 5 5-5M5 19h14"/></svg>
        {t('下载到本地')}
      </button>
    {/if}
    <button class="ctx-item" onclick={() => handleItem(onCopy)}>
      <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M4 7h11v13H4z"/><path d="M8 4h12v12"/></svg>
      {compact ? t('复制路径') : t('复制共享路径')}
    </button>
    {#if onCopyUrl}
      <button class="ctx-item" title={t('复制到其他设备的下载链接；用浏览器打开即开始下载')} onclick={() => onCopyUrl?.()}>
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="9"/><path d="M3 12h18M12 3c2.5 2.6 3.8 5.7 3.8 9S14.5 18.4 12 21c-2.5-2.6-3.8-5.7-3.8-9S9.5 5.6 12 3Z"/></svg>
        {compact ? t('复制链接') : t('复制下载链接')}
      </button>
    {/if}
    {#if onShareHistory && !compact}
      <button class="ctx-item" onclick={() => handleItem(onShareHistory)}>
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="9"/><path d="M12 7v5l3 2"/></svg>
        {t('更新记录…')}
      </button>
    {/if}
    <!-- 删除放第一组的最后，红色；共享项根（path 为空）不给。 -->
    {#if onDeleteEntry && path}
      <button
        class="ctx-item delete"
        title={isDir ? t('从本机磁盘上删除这个文件夹及其全部内容，无法恢复') : t('从本机磁盘上删除这个文件，无法恢复')}
        onclick={() => handleItem(onDeleteEntry)}
      >
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M4 7h16M9 7V4h6v3M6 7l1 13h10l1-13M10 11v6M14 11v6"/></svg>
        {isDir
          ? (compact ? t('删除文件夹') : t('删除文件夹…'))
          : (compact ? t('删除文件') : t('删除文件…'))}
      </button>
    {/if}
    <div class="ctx-sep"></div>
    <button class="ctx-item danger" onclick={() => handleItem(onRemove)}>
      <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M4 7h16M9 7V4h6v3M6 7l1 13h10l1-13M10 11v6M14 11v6"/></svg>
      {compact ? t('移除共享') : t('移除共享项')}
    </button>
    {#if bcAvailable}{@render bcSection()}{/if}
  {:else}
    <!-- 备注（F-101）**与在线状态无关**：它是纯本机数据（写进本机 `config.toml`，
         不发给对端），离线行、未绑定的行照样能改 —— 所以它排在下面那段状态分档之前，
         不受任何条件影响。当前备注就写在菜单里（`title` 给全，行内截断），
         用户不用先点进去才知道自己起过什么名。 -->
    <button class="ctx-item" title={deviceRemark ?? undefined} onclick={() => handleItem(onRemark)}>
      <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M4 5h16M4 10h10M4 15h16M4 20h7"/></svg>
      {t('修改备注')}
      {#if deviceRemark}<span class="ctx-current">{deviceRemark}</span>{/if}
    </button>
    <div class="ctx-sep"></div>
    {#if deviceState === 'unbound'}
      <!-- 未绑定：只做两件事 —— 配对，或开会话发消息（消息走"长连接优先 + HTTP 兜底"，
           未建链时对端还收不到就排队等它上线）；推送在会话里那颗「＋」，这里不再重复摆一份。
           没有「立即连接」（未配对拨不动），也没有「解除配对」（没有关系可解）。
           **排在"离线"之前**：未绑定的行（含只被 mDNS 看到过、还没连上的，F-106）
           在列表里照样有一行，菜单就该给得出这一行能做的事（配对 / 发消息），
           而不是只剩「修改备注」。 -->
      <button class="ctx-item" onclick={() => handleItem(onPair)}>
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M12 5v14M5 12h14"/></svg>
        {t('配对')}
      </button>
    {:else if !deviceOnline}
      <!-- 已配对但离线：只有「解除配对」是本机动作（它不依赖长连接）。会话仍然打得开、消息也发得
           出去（排着队等它上线，见 chat::flush_queued），但设备页这一行按 F-14 的离线行规则
           不给这两个动作。 -->
    {:else if deviceState === 'connected'}
      <button class="ctx-item" onclick={() => handleItem(onDisconnect)}>
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M10 13a5 5 0 0 0 7 0l3-3a5 5 0 0 0-7-7l-1.5 1.5"/><path d="M14 11a5 5 0 0 0-7 0l-3 3a5 5 0 0 0 7 7l1.5-1.5"/><path d="M3 3l18 18"/></svg>
        {t('断开长连接')}
      </button>
    {:else if deviceNeedsPair}
      <!-- token 已失效：拨号必然 401，唯一出路是重新配对 -->
      <button class="ctx-item" onclick={() => handleItem(onPair)}>
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M12 5v14M5 12h14"/></svg>
        {t('重新配对')}
      </button>
    {:else}
      <button class="ctx-item" onclick={() => handleItem(onConnect)}>
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M10 13a5 5 0 0 0 7 0l3-3a5 5 0 0 0-7-7l-1.5 1.5"/><path d="M14 11a5 5 0 0 0-7 0l-3 3a5 5 0 0 0 7 7l1.5-1.5"/></svg>
        {t('立即连接')}
      </button>
    {/if}
    {#if deviceOnline || deviceState === 'unbound'}
      <button class="ctx-item" onclick={() => handleItem(onMessage)}>
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M22 2 11 13"/><path d="M22 2 15 22 11 13 2 9 22 2Z"/></svg>
        {t('发消息')}
      </button>
    {/if}
    {#if deviceState !== 'unbound'}
      <div class="ctx-sep"></div>
      <button class="ctx-item danger" onclick={() => handleItem(onUnpair)}>
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M4 7h16M9 7V4h6v3M6 7l1 13h10l1-13M10 11v6M14 11v6"/></svg>
        {t('解除配对')}
      </button>
    {:else if onForget}
      <div class="ctx-sep"></div>
      <button
        class="ctx-item danger"
        title={t('只清本机记录：设备行、地址、备注与观察指纹；对端不受影响。它下次广播时还会重新出现。')}
        onclick={() => handleItem(onForget)}
      >
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M4 7h16M9 7V4h6v3M6 7l1 13h10l1-13M10 11v6M14 11v6"/></svg>
        {t('忘记设备')}
      </button>
    {/if}
  {/if}
</div>

<style>
  /* ── 折叠态卡片上的那一份（`.is-compact`）─────────────────────────────
     它长在桌面上、贴着悬浮框，比面板里的菜单要"小一号"：面板那份是 13px/7×10 的行，
     这一份与**卡片里的目录名同一号**（`--fs-xxs`，见 `.rq-iname`），行距与内边距一并收紧。
     三个数（字号 / 行内边距 / 图标）是这一档的全部：宽度由文字算（`min-width: 0` 撤掉
     面板那份的 172px 下限），所以菜单整体比面板那份小一圈。
     代价是**每一项的文案也得短**（打开文件夹 / 复制路径 / 移除共享）：11px 的行里摆
     "打开所在文件夹" 这种长句，菜单又会被撑回去。
     宽度**必须写成 `max-content`**：`.ctx` 是 `position: fixed` 且只给了 `left`，宽度默认是
     shrink-to-fit —— 菜单还没定好落点时（`pending`）正好摆在指针那一格，右边剩多少它就缩多窄，
     量出来的尺寸是"被窗口挤过的那一份"，撑窗与落点全跟着错（实测：97px 的菜单量成 63×272）。
     `max-content` 与摆在哪儿无关。 */
  .ctx.is-compact { min-width: 0; width: max-content; padding: 4px; }
  .ctx.is-compact .ctx-item { gap: 6px; padding: 4px 7px; font-size: var(--fs-xxs); }
  .ctx.is-compact .ctx-item svg { width: 12px; height: 12px; }
  /* 左槽那行的文件名：卡片这一档只有 236px 窗口，菜单必须窄到能整条摆在指针左侧
     （`placeOrbMenu` 才不会去搬窗口原点 —— 那一下正是闪烁的来源）。整行截断，全名在 title 里；
     面板那一档不截断。 */
  .ctx.is-compact .ctx-elide {
    min-width: 0;
    max-width: 40px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }
  .ctx.is-compact .ctx-sep { margin: 4px 5px; }

  /* 删除：红色（`.danger` 是这套配色里"关系出问题"的琥珀色；删除这一条用户要求用红）。
     图标也要跟着红：`.ctx-item svg` 默认是 muted。 */
  .ctx-item.delete { color: var(--danger); }
  .ctx-item.delete svg { color: var(--danger); }
  /* 走令牌而不是组件内写 `color-mix()`：令牌自带 sRGB/现代两档回退，老 WebView 不会整条失效
     （兼容性门禁也会把组件 CSS 里的裸 `color-mix()` 算作问题）。 */
  .ctx-item.delete:hover { background: var(--danger-soft); }

  /* 比较位提示行 / 无可比对象的比较项：沿用 .ctx-item 排版，但不可点也不高亮。
     pointer-events:none 让点击落到菜单根节点上，等于「点空白关闭菜单」。 */
  .ctx-item:disabled {
    opacity: 0.55;
    color: var(--muted);
    cursor: default;
    pointer-events: none;
  }

  /* 菜单行里跟在文字后面的当前备注：菜单宽度不该被一句备注拉长（备注最长 24 个字），
     所以这一行自己截断，全名在按钮的 `title` 里。 */
  .ctx-current {
    flex: 1 1 auto;
    min-width: 0;
    max-width: 132px;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    color: var(--muted);
    text-align: right;
  }
</style>