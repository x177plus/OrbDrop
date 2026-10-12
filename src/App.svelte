<script lang="ts">
  // 桌面端样式表从**桌面入口**引：`index.html` 是平台共用的，放在那儿的 `<link>` 会被
  // 移动端产物一并打包（实测 `dist-mobile` 里混进过整份 67KB 桌面样式）。各端只带走自己那份。
  import "./styles/main.css";
  import { getCurrentWebviewWindow } from '@tauri-apps/api/webviewWindow';
  import { LogicalSize } from '@tauri-apps/api/dpi';
  import { onMount, tick } from 'svelte';
  import { fly } from 'svelte/transition';
  import { cubicOut } from 'svelte/easing';
  import { listen } from '@tauri-apps/api/event';
  import { open as openDialog } from '@tauri-apps/plugin-dialog';
  import { getDevices, loadDevices, isLocalOnline, noteLocalOnline, deviceDisplayName, peerDisplayName, saveDeviceRemark, unicastBlockedTip } from '$lib/stores/devices.svelte';
  import { getServerPort, setServerPort, loadShares, addShare, removeShare, getShares } from '$lib/stores/shares.svelte';
  import { loadSettings, getSettings } from '$lib/stores/settings.svelte';
  import { getPeerSharesState, loadPeerShares } from '$lib/stores/peers.svelte';
  import { getBcStatus, loadBcStatus, setBcSide, clearSide, compare } from '$lib/stores/bc.svelte';
  import * as chat from '$lib/stores/chat.svelte';
  import type { ConvoThread } from '$lib/stores/chat.svelte';
  import * as upd from '$lib/stores/update.svelte';
  import { getActualPort, startHttpServer, setOnline, acceptPush, rejectPush, cancelPush, cancelPushBatch, cancelReceive, pushPaths, pathPicks, localAddresses, orbScreenMetrics, orbSetRect, orbSetHitRegion, orbDragStart, orbMouseDown, orbSaveCenter, addPeer, pairStart, pairConfirm, pairInvite, cancelPairInvite, acceptPairInvite, rejectPairInvite, connectPeer, disconnectPeer, removePeer, downloadFromPeer, downloadFile, openPath, fetchTree, peerTree } from '$lib/api';
  import type { PushInvite, PushBatch, PushTarget, PeerDevice, TabKey, BcTarget, OrbDragInfo, LocalPick, RectPage, RectSlot, TreeEntry, OrbShape, ShareItem } from '$lib/types';
  import { shareMeta, updateStatusText, updateTitleText } from '$lib/types';
  import { planExpansion, planCollapse, planDock, planRectDock, spinForDock, type BadgeSpin, type DockAxis, type DockPlan, type ScreenRect } from '$lib/orbLayout';
  import { circleLayout, collapsedLayout, ORB_SIZE, RECT_CARD_W, RECT_DOCK_W } from '$lib/orbShape';
  import Orb from '$lib/components/Orb.svelte';
  import RectOrb from '$lib/components/RectOrb.svelte';
  import FilePicker from '$lib/components/FilePicker.svelte';
  import Logo from '$lib/components/Logo.svelte';
  import TabBar from '$lib/components/TabBar.svelte';
  import TabContent from '$lib/components/TabContent.svelte';
  import SettingsPanel from '$lib/components/SettingsPanel.svelte';
  import ContextMenu from '$lib/components/ContextMenu.svelte';
  import ConvoPanel from '$lib/components/ConvoPanel.svelte';
  import RecipientDialog from '$lib/components/RecipientDialog.svelte';
  import Hint from '$lib/components/Hint.svelte';
  import HintLayer from '$lib/components/HintLayer.svelte';

  const appWindow = getCurrentWebviewWindow();
  const PANEL = { w: 448, h: 620 };                                // 面板标称尺寸（CSS px）
  const PANEL_SLACK = 20;                                          // 面板四周留给阴影的余量（CSS px）：必须 ≥ --shadow 的可见半径（main.css 里有实测值），否则阴影被窗口边裁出切线
  const EXPANDED = new LogicalSize(PANEL.w + PANEL_SLACK * 2, PANEL.h + PANEL_SLACK * 2);
  const WORK_MARGIN = 8;                                           // 展开窗口与工作区边缘的最小间距（CSS px）
  /** 卡头那行路径最多放这么多个**等宽字符**（`.rq-ip` 是 10px 等宽，正文到项数之间约 20 格）：
   *  超了从头上砍 —— 尾巴才是"我在哪"，共享项名砍掉不心疼（`title` 里给全）。 */
  const RECT_DIR_MAX = 20;
  const FOLD_MS = 180;                                             // 面板退场 0.16s 走完（已完全不可见）即可缩窗口
  const DOCK_THRESHOLD = 20;                                       // 悬浮球边缘距工作区边缘 ≤ 此值即贴边收起（CSS px）
  const DOCK_EXPOSE = 1 / 3;                                       // 贴边收起后仍露在屏幕内的比例（其余推到屏幕外）
  const DOCK_SETTLE_MS = 160;                                      // 拖动停手多久后判定吸附（OS 拖动期间 move 事件成串到达）
  const DRAG_POLL_MS = 60;                                         // 还按着鼠标时轮询「松手了没有」的间隔
  const START = circleLayout();                                    // 折叠态几何的起始值（形态未知前按圆球那一档，见下面的 orbOffset）
  // 通知气泡甩入/甩出的时长。`prefers-reduced-motion` 下归零：动效可以省，功能不能省
  // （收/展、确认都照旧）。
  const MOTION = typeof matchMedia === 'function' && matchMedia('(prefers-reduced-motion: reduce)').matches ? 0 : 200;

  /**
   * 等 WebView2 的 CSS 视口跟上 OS 窗口尺寸。
   * SetWindowPos 之后视口尺寸会滞后 1~2 帧，此时布局仍按旧宽度算 ——
   * 面板若在这期间开始淡入，就会先以窄条出现再跳宽（右边缘闪一下）。
   */
  async function waitForViewport(width: number, height: number) {
    for (let i = 0; i < 6; i++) {
      if (Math.abs(window.innerWidth - width) <= 1 && Math.abs(window.innerHeight - height) <= 1) return;
      await new Promise(requestAnimationFrame);
    }
    console.warn('展开：视口尺寸未在预期帧数内跟上窗口，按当前尺寸继续');
  }

  let expanded = $state(false);
  let showSettings = $state(false);
  let activeTab = $state<TabKey>({ kind: 'local' });
  let dragging = $state(false);
  /**
   * 拖入的落点正在**那条开着的会话**上（`dropInConvo`）：松手即推送给这条会话的收件人，
   * 不再登记成共享项（F-111）。
   *
   * 只用来画预告（会话浮层描一圈主色 + 一句提示）。**真正走哪条路不读它** —— `drop` 那一刻
   * 按坐标现判：`over` 与 `drop` 之间指针还在动，拿上一次 `over` 的结果去决定结果是错的。
   */
  let convoDropHot = $state(false);
  // 折叠态悬浮形态（设置里选的那一档，落盘在后端 `AppSettings.orbShape`）：
  // circle = 62px 圆球（`Orb.svelte`），rect = 圆角矩形悬浮框（`RectOrb.svelte`）。
  let orbShape = $derived(getSettings()?.orbShape ?? 'circle');
  // 悬浮体在窗口内的绝对偏移（CSS px）：切换窗口时由 JS 更新，配合 holdOrb 隐藏保证不错位。
  // 起始值取圆球那一档（首帧时设置还没回来，形态未知）；形态一到位，下面那个改窗口尺寸的
  // `$effect` 会把偏移与热区按真实形态重设一遍（`resetOrbDom`）。
  let orbOffset = $state({ x: START.widget.x, y: START.widget.y });
  // 悬停热区（窗口内偏移 + 尺寸，CSS px）：**收起态屏上看得见的那一条** —— 贴边时是露出那截
  // 弧的方框 / 那条竖条，呼出阈值就是它（见 `planDock` / `planRectDock` 的 `zone`）；悬浮体滑回
  // 完整位之后由 `collapsedHitRects` 把它并进来。未贴边时就是悬浮体本身
  let orbZone = $state<ScreenRect>({ x: START.widget.x, y: START.widget.y, w: START.widget.w, h: START.widget.h });
  // 贴边收起时两颗角标各自绕球心的转角（度）：左 = 黄（通知数）、右 = 绿（长连接设备数）。
  // 数值由贴边方向定（`spinForDock`）：同号 = 整对一起扫、一正一负 = 两条臂各自荡开（贴上）
  let badgeSpin = $state<BadgeSpin>({ left: 0, right: 0 });
  // 圆角矩形悬浮框的贴边方向（-1 贴左 / 1 贴右 / 0 未贴）：收起态的竖条钉在悬浮框哪一端、
  // 两枚角标落在竖条的哪两条边、卡与竖条谁渐显，全由它定。是 `badgeSpin` 在那一档的对应物。
  let rectDock = $state<DockAxis>(0);
  // 右键菜单：开在哪（窗口 CSS px）、开的是什么（`kind` 决定菜单画哪一组），外加一个"这次是
  // 折叠态卡片上开的那一份"的标记（`orb`，见 `orbCtxOpen`）。开与关只有两条路：面板里的各行
  // 派 `od:ctx` 事件（见 `handleCtx`），卡片那一档直接调 `openRectCtx`。
  type CtxState = { x: number; y: number; kind: 'share' | 'device' | 'peer' | 'chat' | 'convo'; el?: HTMLElement; shareId?: string; path?: string; isDir?: boolean; deviceState?: string; deviceOnline?: boolean; deviceNeedsPair?: boolean; deviceId?: string; peerId?: string; name?: string; chatId?: string; chatFull?: string; chatSel?: string; convoKey?: string; convoPeerId?: string; convoThread?: boolean; convoBc?: boolean; convoBrowse?: boolean; orb?: boolean; placed?: boolean; menuW?: number; menuH?: number };
  let ctxState = $state<CtxState | null>(null);
  /**
   * 折叠态卡片上那个右键菜单是不是开着（`ctxState.orb`，见 `openRectCtx`）。
   *
   * 它只有一个用处，且只在**贴边收起**那一档：**保持 `reveal`**（指针从卡片挪到菜单上时，
   * 卡片不许缩回竖条 —— 缩回去的同一刻可点区域也只剩那条竖条，菜单当场变得点不着，见
   * `collapsedHitRects`）。未贴边时卡片本来就一直在，这个标记等于没作用。
   */
  let orbCtxOpen = $derived(ctxState?.orb === true);
  /**
   * 菜单那一档的**窗口账**：菜单要长在指针的右下（右边不够就左下），而折叠态窗口只有 236×162 ——
   * 只能先把窗口撑大（见 `placeOrbMenu`）。`grown` = 撑过（关菜单时要缩回去）；`shift` = 窗口原点
   * 反向挪过多少（CSS px）：窗口内的东西要整体平移这个量才在原地不动，值交给 `--menu-dx/dy`
   * （`.orb-stage.menu` 的 transform，见 main.css）。菜单**不在** stage 里，坐标不受它影响。
   */
  let orbMenuGeo = $state<{ grown: boolean; shift: { x: number; y: number } }>({ grown: false, shift: { x: 0, y: 0 } });
  /** 撑窗那一两帧里卡片先不出镜：原点在挪，画面上会跳（见 main.css 的 `.menu-busy`）。 */
  let orbMenuBusy = $state(false);
  /** 缩窗那一趟还在飞（窗口还没回到折叠态那一档）：新一次落点得先等它 —— 两次 `SetWindowPos`
   *  同时在飞，后到的说了算，窗口会停在错的那一档。 */
  let orbMenuRestoring: Promise<void> | null = null;
  // 贴边态"悬停显示"的两个开关（`--orb-zone` 的 `.reveal`，见 `setDockOrb` 与 markup）：
  // `zoneHot` = 指针在热区内、且这次进入允许显示；`zoneHold` = 落位之后压住的那一次。
  let zoneHot = $state(false);
  let zoneHold = $state(false);
  /**
   * `.reveal` 的最终开关 = 悬停呼出 **或** 拖着东西进来。
   *
   * 拖入这一路必须单独接：OS 的文件拖动（OLE 拖放）走的是自己的模态循环，WebView 收到的是
   * Tauri 转来的 `onDragDropEvent`，**收不到 pointerenter / `:hover`** —— 只认指针的那套显示逻辑
   * 在拖动下整个是哑的。而贴边收起时屏上看得见的只有那条竖条 / 露出的那截弧，用户拖过去
   * 什么也看不到（`.orb-stage.drag` 那圈高亮描在球与卡片上，两者此刻都在窗口外），松手却真的
   * 会登记成共享项 —— 有结果、没预告。
   *
   * 于是拖入沿用**与悬停同一档显示**：悬浮体滑回完整可见 + 那圈主色高亮。语义也对得上 ——
   * 落下 = 登记为本机共享项，落点就是悬浮体本身。
   *
   * 但**不展开面板**：展开要在拖动模态循环中途 `SetWindowPos`，落点目标（`IDropTarget`）在
   * 拖动期间被搬走/改尺寸，抖动与丢目标都是自找的；而"面板上的落点"只有会话浮层那一块
   * （见 `dropInConvo`），它本来就只在面板**已经展开**时才看得见、才谈得上落点，所以这一路
   * 永远不会因为"面板收着"而缺预告。
   *
   * 第三路是**卡片上开着的右键菜单**（`orbCtxOpen`）：菜单画在卡片外面（窗口坐标，可能压着
   * 卡片右下那圈余量），指针从卡片挪到菜单上那一刻 `pointerleave` 就把这一档关掉了 ——
   * 卡片缩回竖条的同时可点区域也只剩那条竖条，菜单落在那块「不吃了」的区域里，点下去穿到桌面。
   * 菜单开着 = 这次交互还在悬浮体上，所以它也算一档"呼出"。
   */
  let reveal = $derived(zoneHot || dragging || orbCtxOpen);
  // 折叠态贴边方向（最近一次落位判定的结果，`{0,0}` = 未贴边）。只有展开时用一次：
  // 贴边那一档的中心点多半在屏幕外（悬浮体大半被推出去了），展开要改成"窗口贴住工作区边"
  // —— 见 `expand` 里给 `planExpansion` 的 margin。
  let dockAxis = $state<{ x: DockAxis; y: DockAxis }>({ x: 0, y: 0 });
  // true = 悬浮球强制隐藏（切窗口期间）；用 transition:none 立即生效，不能让它带过渡露脸
  let holdOrb = $state(false);
  // 展开时记下的**悬浮体本体中心**（球心 / 卡片白底中心，屏幕物理像素）：展开/收起唯一的参考点。
  // 面板的可视盒角落在它上面；记它而不是 OS 窗口左上角 —— 窗口 = 悬浮体 + 四周余量，两种形态的
  // 余量与尺寸都不同，记窗口的话同一个悬浮体位置会展开出两个地方（见 `orbLayout.planExpansion`）。
  let orbAnchor = $state<{ x: number; y: number } | null>(null);
  // 展开态窗口矩形（物理像素）：收起时与「当前窗口位置」作差，得到窗口被拖动的位移
  let expandedRect = $state<ScreenRect | null>(null);
  // 折叠态窗口尺寸已经按哪一档形态归过位（`resetOrbDom` 那一档）：见下面改窗口尺寸的 `$effect`
  let sizedShape = $state<OrbShape>('circle');
  let foldTimer: ReturnType<typeof setTimeout> | null = null;
  let opening = false;
  let settling = false;   // 落位判定进行中（含「等松手」轮询）：防止 move 事件重复起一轮

  // 发送侧：一次推送操作（可能覆盖多台设备）= 一张卡。是个列表而不是单张：同时在推两批
  // （或一边推一边还在等上一批）时，两张卡各自走完，不会被后来者顶掉 —— 顶掉等于把前一条
  // 的进度条凭空抹掉。
  //
  // 数据全部来自后端的 `push://batch` 快照（`BatchView`）：每台的成败、还差几台、
  // 聚合进度都是后端算好的 —— 界面不做"把 N 条邀请事件拼成一张卡"的聚合运算，
  // 否则同一张卡就有两处真相（谁先到先算谁的）。单台推送也是一批（只有一条目标）。
  let pushBatches = $state<PushBatch[]>([]);
  /** 一批超过 5 台时列表先只列 4 行：展开了的批次记在这里（按 batchId）。 */
  let expandedBatches = $state<Record<string, boolean>>({});
  /** 已经收过尾的批次（toast/定时收起只做一次，快照可能连着来好几条终态）。 */
  const settledBatches = new Set<string>();
  // 接收侧：待处理的通知（推送邀请 / 接收中 / 已收到待确认 / 接收失败）。
  // 悬浮球左上角的数字就是这个数组的长度 —— 卡片/气泡撤掉，数字就减。
  type PushNotice = {
    inviteId: string;
    /**
     * invite    未绑定设备发来的邀请：要用户点「接收/拒绝」（底部那张全宽卡）
     * receiving 传输进行中（底部那张全宽卡，带进度与「取消」）
     * done      已经收完，**等用户确认**（右下角气泡，点了「知道了」才消失）
     * failed    失败，同样等用户确认（右下角气泡）
     * pair      对端发来的**配对邀请**：点「接收」即配对（免输入配对码，F-12）
     * pairWait  本机**发出去**、还在等对端在它的通知里处理的配对邀请（带「取消」）
     */
    kind: 'invite' | 'receiving' | 'done' | 'failed' | 'pair' | 'pairWait';
    from: string;        // 对方设备名
    name: string;        // 文件/目录名（多选时是容器名）
    size: number;
    isDir: boolean;
    /** 对方这次选了几项（>1 = 多选；通知标题按它换文案）。 */
    count: number;
    received: number;
    error?: string;
    /** 落盘位置（`<下载目录>/<发送方名>`），只有 done 有。 */
    dest?: string;
    /** 配对邀请的发起方 id（kind = 'pair'）。 */
    peerId?: string;
    /**
     * 配对邀请（kind = 'pair'）来自"本机记录里已配对、但没带上本机发给它的凭据"的设备
     * （后端 `pair://invite` 的 `unproven`）。只在本机已知需重新配对时才会放行到这一档，
     * 正文要提醒用户"它自称是老搭档，但凭据对不上"。
     */
    unproven?: boolean;
  };
  let notices = $state<PushNotice[]>([]);
  // 铃铛：展开/收起右下角那组"等你确认"的气泡。收起 ≠ 已读 —— 角标照旧挂着。
  let notifOpen = $state(true);
  // 本机「在线 / 离线」状态框（面板右上角）：展开态 + 当前值。值来自后端（`AppSettings.online`
  // 落盘、跨重启有效），这里只做镜像；切换时先乐观改本地、失败再翻回去（后端回话要几十毫秒，
  // 中间那一下"点了没反应"最像卡死）。
  let statusOpen = $state(false);
  let statusEl = $state<HTMLElement | null>(null);
  // 本机「在线 / 离线」的当前值住在 `stores/devices` 里（设备/会话的状态点要看它：隐身时后端
  // 拒发，那些点一律按"送不到"画）—— 这里只读它、只把它报回去，不再自己存一份。

  /**
   * 点状态框里的一行。
   *
   * 点是**当前值**那一行时它是"开关"：收起时展开列表、展开时收起（不重复切状态）。
   * 收起态下另一行是 `visibility: hidden` 的（不可点），所以这个开关只可能落在当前值那行。
   * 点另一行就是切过去，顺带收起列表。
   */
  function pickOnline(on: boolean) {
    if (on === isLocalOnline()) statusOpen = !statusOpen;
    else void setLocalOnline(on);
  }

  /**
   * 切「在线 / 离线」：离线 = 隐身（不广播 mDNS、断全部长连接、拒收一切入站请求），
   * 配对关系与共享项都不动。**先收起列表再切** —— 与点图标按钮同一套手感，
   * 也让"框长回去"和"状态变了"是同一段动画里的事。
   */
  async function setLocalOnline(on: boolean) {
    statusOpen = false;
    if (on === isLocalOnline()) return;
    const prev = isLocalOnline();
    noteLocalOnline(on);
    try {
      await setOnline(on);
      toast(on ? '已切回「在线」，重新对局域网可见' : '已切到「离线」，本机对局域网隐身');
    } catch (e) {
      noteLocalOnline(prev);
      toast(typeof e === 'string' ? e : '切换在线状态失败');
    }
  }
  // 文件选择框的请求：标题/说明/确认按钮文案 + 确认动作。推送与「添加共享项」都用它 ——
  // 同一个选择框只该有一套开关方式，所以这里存的是**要开成什么样**，而不是某个具体用途。
  let picker = $state<{
    title: string;
    sub: string;
    confirmLabel: string;
    onConfirm: (items: LocalPick[]) => void;
  } | null>(null);
  // Pair dialog state: we display our own code (issued by the backend) and
  // accept the code the other side displays in the same dialog.
  type PairDialog = {
    peerId: string | null;   // null = still picking a target device
    peerName: string | null;
    code: string;            // our code, shown for the peer to type in
    remaining: number;       // seconds left on `code`; 0 while refreshing
    input: string;           // the code shown on the peer's screen
    busy: boolean;
    error: string | null;
    ip: string;              // manual-add target (F-11)
    port: string;
  };
  let pairState = $state<PairDialog | null>(null);
  let pairTimer: ReturnType<typeof setInterval> | null = null;
  /**
   * 设备备注对话框（F-101）：右键设备行 →「修改备注」开的那一个。
   *
   * `saved` 是**打开那一刻后端里存着的备注**（'' = 没备注）：用它决定画不画「清除备注」
   * 那颗按钮，以及保存后该说"已设置"还是"已清除"——判断依据是后端给的值，不是输入框里
   * 用户刚敲的东西。
   *
   * `rawName` 是对端自己的设备名（**不含**备注）：对话框里"当前设备名"与保存后的提示都报它。
   * 报显示名的话，改备注这件事就变成"把「客厅台式机(DESKTOP-ABC)」的备注设为「书房笔记本」"
   * —— 一句里两半都在说备注，反而看不出改的是哪台机器。
   */
  let remarkState = $state<{ peerId: string; rawName: string; value: string; saved: string; busy: boolean; error: string | null } | null>(null);
  // Toast messages
  let toasts = $state<{ id: number; msg: string; leaving: boolean }[]>([]);
  let toastId = $state(0);

  let connectedCount = $derived(getDevices().filter(d => d.connected).length);
  // 铃铛的角标：面板右下角那组气泡里还有几件事（通知 + 在途推送）。
  // 铃铛收起的不只是"通知"—— 发送方的进度卡也在同一组里，所以角标得一起算，
  // 否则会出现"收起后角标还在、但组里明明有东西"的错位。
  // 「更新这件事」此刻该不该占一格（提示 → 点过「立即更新」之后的进度/失败）：后端那份
  // 状态归 store，这里只读。`silent` 那道门在 store 里 —— 用户自己检查来的结论不进这一格。
  let updateTask = $derived(upd.updateTask());
  /** 下载进度百分比（不知道总长时是 0，那一格退化成"正在下载 + 已下多少"）。 */
  let updatePct = $derived(
    updateTask?.size ? Math.min(100, Math.round((updateTask.received / updateTask.size) * 100)) : 0,
  );
  // **一批多设备推送算一件**：用户点一次就得到一张卡，角标自然跟着"几次操作"走。
  // 「更新这件事」也算一件：启动时的静默检查发现新版（或已经点过「立即更新」）时，
  // 它就和那些"等你确认"的通知一样占着右下角一格（见上面的 `upd.updateTask`）。
  let notifCount = $derived(notices.length + pushBatches.length + (updateTask ? 1 : 0));
  // 底栏右下角那格版本号：**与设置里「当前版本」同一个源**（后端 `UpdateState.current`，
  // 也就是 `tauri.conf.json` 的 version）。写死字面量的话，`npm run bump` 之后底栏就停在
  // 上一版上，跟设置里那格对不上 —— 版本号只该有一处真源。
  // 还没读到（后端那条命令没回来）时跟设置里一样显示 `…`。
  let appVersion = $derived(upd.getUpdateState()?.current ?? '…');
  // 会话侧"等你处理"的数：未读 + 还没确认的群邀请（见 store 里的 `convoBadgeTotal`）。
  // 邀请卡只画在会话列表那层 —— 不计数的话面板收着时它就完全没有提示。
  let convoBadgeCount = $derived(chat.convoBadgeTotal());
  // 底栏「会话」那颗角标的读屏文案：未读与群邀请是两个数，合成一个数字就说不清是哪一类。
  let convoBadgeLabel = $derived(
    [
      chat.unreadTotal() > 0 ? `${chat.unreadTotal()} 条未读` : '',
      chat.pendingGroups().length > 0 ? `${chat.pendingGroups().length} 个群邀请` : '',
    ]
      .filter(Boolean)
      .join('，'),
  );
  // 悬浮球左上角那颗角标：**同一组东西再加上会话侧那一堆**。未读消息与群邀请也是"有事等你"，
  // 而面板收着的时候，那颗球是唯一还看得见的提示（铃铛只数它自己那组气泡里的东西 —— 会话
  // 不进那一组，所以铃铛角标跟着涨就会「说 3 件、点开只有 2 件」）。打开那个会话即已读、
  // 群邀请点了「加入/不加入」即消失，数自己就掉。
  let orbNotifCount = $derived(notifCount + convoBadgeCount);
  // 通知（面板右下角那组气泡）：未绑定的邀请、正在接收、已收到待确认、失败 —— **全都在
  // 这一组里**，形状统一（右对齐、2/3 面板宽，不占满）。要当场点的按钮（接收/拒绝/取消）
  // 就长在气泡里，所以它们不单独占一层。
  // 气泡组容器：新气泡总是贴在最下缘（就是面板右下角），所以每次都把它滚到底。
  let notifEl = $state<HTMLDivElement | null>(null);
  $effect(() => {
    void notices.length;
    void pushBatches.length;
    void updateTask;
    if (notifEl) notifEl.scrollTop = notifEl.scrollHeight;
  });
  // footer 的实际高度 + 通知组能用的最大高度：两个都量，不写死数字。
  // 通知组的可用区间 = **标签栏下方 → footer 上缘**（`.panel-body` 的顶边正好是标签栏的
  // 下缘），上下各留 12px、与左右一致；标签栏会随长连接设备增减、footer 会随地址换行变高，
  // 所以挂 ResizeObserver 跟着重算。
  let footEl = $state<HTMLElement | null>(null);
  let bodyEl = $state<HTMLElement | null>(null);
  let footH = $state(40);
  let notifMaxH = $state(320);
  $effect(() => {
    const foot = footEl;
    const body = bodyEl;
    if (!foot) return;
    const measure = () => {
      footH = foot.offsetHeight;
      if (!body) return;
      // 通知组盒子的高度预算 = **标签栏下缘 → footer 上缘**（盒子的上下边就是这两条横线，
      // 滚动区域正好夹在中间，内容越不过线去）。阴影余量在盒子内部的 padding 里让，
      // 不从这里扣 —— 所以这里不减任何数字。
      const avail = foot.getBoundingClientRect().top - body.getBoundingClientRect().top;
      notifMaxH = Math.max(140, Math.round(avail));
    };
    const ro = new ResizeObserver(measure);
    ro.observe(foot);
    if (body) ro.observe(body);
    measure();
    return () => ro.disconnect();
  });
  // 每台长连接设备一个浏览标签：长连接是「对方此刻能看到/能被看到」的边界（F-25）。
  let browsers = $derived(getDevices().filter(d => d.connected));
  // 正在浏览的设备掉线/断开长连接后，标签会消失：自动回落，别把用户留在空白页。
  $effect(() => {
    const tab = activeTab;
    if (tab.kind === 'peer' && !browsers.some((d) => d.id === tab.peerId)) {
      activeTab = { kind: 'devices' };
    }
  });
  let localCount = $derived(getShares().length);
  let deviceCount = $derived(getDevices().length);
  let serverPortVal = $derived(getServerPort());
  // 本机内网地址（左下角「IP:端口」）：多网卡时只显示首个，其余在悬停提示里列全 ——
  // 一块 footer 装不下三四个地址，但"我还有别的地址"必须让人看得见（手动添加设备要用）。
  let localAddrs = $state<{ primary: string | null; all: string[] }>({ primary: null, all: [] });
  let addrTitle = $derived(
    localAddrs.all.length > 1
      ? `本机有 ${localAddrs.all.length} 个内网地址（点一下复制第一个）：\n${localAddrs.all.join('\n')}`
      : '点一下复制「IP:端口」',
  );

  /** 左下角「端口 · IP」的地址：后端枚举，本机在**启动**与**网卡变化**时各拉一次
   *  （`local://addresses` 只在真的变了的时候发，见 lib.rs 那条 5s 网卡巡检）。 */
  async function refreshLocalAddrs() {
    try {
      localAddrs = await localAddresses();
    } catch (e) {
      console.error('本机内网地址获取失败:', e);
    }
  }

  /** 复制「IP:端口」—— 手动添加设备（F-11）时直接粘到对面就行。 */
  function copyAddr() {
    const ip = localAddrs.primary;
    if (!ip) return;
    const text = `${ip}:${serverPortVal || 3890}`;
    navigator.clipboard.writeText(text).then(
      () => toast(`已复制 ${text}`),
      () => toast('复制失败，请手动输入'),
    );
  }

  /** ctxState → BcTarget。label 是给人看的名字（后端拼不出对端共享项的名字），后端优先采用。 */
  function ctxBcTarget(c: CtxState): BcTarget | null {
    if (!c.shareId) return null;                                    // 没有共享项 id 就没有可比较的对象
    const path = c.path ?? '';
    if (c.kind === 'peer') {
      if (!c.peerId) return null;
      const dev = peerDisplayName(c.peerId);
      const item = path || c.name;                                  // 树内节点用累积相对路径，共享项根用共享项名
      return { kind: 'peer', peerId: c.peerId, shareId: c.shareId, path, label: item ? `${dev} · ${item}` : dev };
    }
    const share = getShares().find((s) => s.id === c.shareId)?.name ?? '';
    const label = path ? (share ? `${share}/${path}` : path) : (share || c.name || '');
    return { kind: 'share', shareId: c.shareId, path, label };
  }

  /** 同一个比较对象（label 只是显示名，不参与比较）。 */
  function sameBcTarget(a: BcTarget, b: BcTarget): boolean {
    if (a.kind !== b.kind || a.shareId !== b.shareId || a.path !== b.path) return false;
    return a.kind === 'peer' && b.kind === 'peer' ? a.peerId === b.peerId : true;
  }

  // BC 菜单的显示条件：本机没装 BC（exePath 为空）时整区隐藏；装了则由右键项与比较位的组合决定文案与可用性
  let bcMenu = $derived.by(() => {
    const st = getBcStatus();
    const target = ctxState ? ctxBcTarget(ctxState) : null;
    const thisLeft = !!(target && st.left && sameBcTarget(st.left.target, target));
    const thisRight = !!(target && st.right && sameBcTarget(st.right.target, target));
    const otherLeft = !!(target && st.left && !thisLeft);           // 对手方在左侧
    const otherRight = !!(target && st.right && !thisRight);
    return {
      available: st.exePath !== null,
      leftLabel: st.left?.label ?? null,
      rightLabel: st.right?.label ?? null,
      compareLabel: otherLeft && otherRight ? '左右比较位' : otherLeft ? '与左侧' : otherRight ? '与右侧' : '',
      compareEnabled: otherLeft || otherRight,
      thisStaged: thisLeft || thisRight,
    };
  });

  function setOpen(v: boolean) {
    if (v) void expand();
    else collapse();
  }

  /**
   * 卡上"钻进了哪一个子目录"：**每一页各自独立**（本机页与每台设备页各记自己那一层），
   * `null` = 这一页还在看共享项清单。键是 `RectPage.id`（本机 = `'local'`，设备 = peerId）。
   *
   * `rel` 是**相对共享项根**的路径（`''` = 就在共享项根那一层）：卡上的条目与拖出源都按
   * 「共享项 id + 相对路径」定位，所以进/出只需要一个字符串。共享项自己的名字只在最外那一层
   * 拿得到（子项只给相对路径），于是它跟着往下带（`shareName`），头那行才说得出完整位置。
   */
  type RectDir = { kind: 'local' | 'peer'; shareId: string; shareName: string; rel: string };
  let rectDirs = $state<Record<string, RectDir | null>>({});
  /** 钻进去那一层的内容：键 = `pageId|shareId|rel`。进去时拉一次，**回上一级命中缓存**（不闪加载态）。 */
  type RectKids = { loading: boolean; error: string | null; entries: TreeEntry[] };
  let rectKids = $state<Record<string, RectKids>>({});

  /** 子目录缓存的键：三样都得进键 —— 同一个 shareId 在不同设备页上是两份东西。 */
  function rectKidsKey(pageId: string, loc: RectDir): string {
    return `${pageId}|${loc.shareId}|${loc.rel}`;
  }

  /**
   * 拉一层目录的内容：本机走自己的 HTTP（`fetchTree`）、对端走后端命令（`peerTree`），
   * 与面板里展开那棵树走的是同一条路（同一份后端、同一套越界防护）。
   */
  async function loadRectKids(pageId: string, loc: RectDir) {
    const key = rectKidsKey(pageId, loc);
    rectKids[key] = { loading: true, error: null, entries: [] };
    try {
      const entries = loc.kind === 'local'
        ? await fetchTree(serverPortVal || 3890, loc.shareId, loc.rel)
        : await peerTree(pageId, loc.shareId, loc.rel);
      rectKids[key] = { loading: false, error: null, entries };
    } catch (e) {
      rectKids[key] = { loading: false, error: e instanceof Error ? e.message : '读取失败', entries: [] };
    }
  }

  /**
   * 一页要显示的内容：没钻入就是共享项清单（`root`），钻入了就是那一层的内容 + 头上那格
   * 「↰ 上一级」。两种视图共用同一份 `RectPage` 字段（`dir` / `up` / `items` / `loading` / `error`），
   * 卡片那边不用分支。
   *
   * 子项的第二行用 `shareMeta(isDir, null, e.size)`：树里只有大小、没有子项数（那要递归遍历，
   * 面板里那棵树也不显示它），所以目录一律说「文件夹」—— 与"宁可不说，也不报一个没根据的数"同一口径。
   */
  function rectView(pageId: string, root: RectSlot[]): { view: Pick<RectPage, 'dir' | 'up' | 'items'>; loading: boolean | null; error: string | null } {
    const loc = rectDirs[pageId];
    if (!loc) return { view: { dir: null, up: null, items: root }, loading: null, error: null };
    const kids = rectKids[rectKidsKey(pageId, loc)] ?? { loading: true, error: null, entries: [] };
    const full = loc.rel ? `${loc.shareName}/${loc.rel}` : loc.shareName;
    // 上一格是**父目录**：'a/b' 里回到 'a'，'a' 里回到共享项自己，共享项根再往上就是清单。
    const parentRel = loc.rel.slice(0, Math.max(0, loc.rel.lastIndexOf('/')));
    const upName = loc.rel === '' ? '共享项'
      : parentRel === '' ? loc.shareName
      : parentRel.slice(parentRel.lastIndexOf('/') + 1);
    return {
      view: {
        dir: { label: full.length > RECT_DIR_MAX ? `…${full.slice(-(RECT_DIR_MAX - 1))}` : full, full },
        up: { name: upName, meta: '上一级' },
        items: kids.entries.map((e) => ({
          id: loc.shareId,
          path: loc.rel ? `${loc.rel}/${e.name}` : e.name,
          name: e.name,
          isDir: e.isDir,
          meta: shareMeta(e.isDir, null, e.size),
        })),
      },
      loading: kids.loading,
      error: kids.error,
    };
  }

  /**
   * 圆角矩形悬浮框那几页：**第一页永远是本机**（本机的共享项，与「本机」标签页同一份
   * `getShares()`；`isDir` / 大小 / 子项数都由后端问来的真元数据），其后每台长连接设备一页
   * （它对本机可见的共享项 —— F-25 的广播范围：只有长连接设备才拿得到开播的那些项）。
   *
   * 本机页的内容与「本机」标签页一致：共享项清单不按广播开关过滤（那是给对端看的口径），
   * 卡上这一页回答的是"我这台机器装了什么"。
   *
   * 每一页还能**钻进去**（双击目录）：那时这一页显示的是那一层的内容，`loading`/`error` 换成
   * 那一层自己的（拉到就换回来，见 `rectView`）。
   */
  let rectPages = $derived.by<RectPage[]>(() => {
    if (orbShape !== 'rect') return [];
    const localRoot: RectSlot[] = getShares().map((s) => ({
      id: s.id,
      path: '',
      name: s.name,
      isDir: s.isDir,
      meta: shareMeta(s.isDir, s.itemCount, s.size),
    }));
    const localDrill = rectView('local', localRoot);
    const local: RectPage = {
      id: 'local',
      kind: 'local',
      // 本机页写「本机」，不写设置里那个设备名：卡片是**一眼看是哪台机器**的地方，
      // 「本机」这一个词就把"这是我的"说完了；设备名（常是 DESKTOP-XXXX 这种主机名）反而要
      // 用户自己回忆，且它在设置里、给对端看的配对弹窗里都有，不差这一处。
      // 具体身份由下面那行地址（本机 IP:端口）承担 —— 那才是别人拿来连你的东西。
      name: '本机',
      ip: localAddrs.primary ? `${localAddrs.primary}:${serverPortVal || 3890}` : '',
      loading: localDrill.loading ?? false,
      error: localDrill.error,
      ...localDrill.view,
    };
    const peers = getDevices()
      .filter((d) => d.connected)
      .map((d): RectPage => {
        const st = getPeerSharesState(d.id);
        const root: RectSlot[] = st.items.map((it) => ({
          id: it.id,
          path: '',
          name: it.name,
          isDir: it.isDir,
          // 老版本对端不带 size/itemCount（`shareMeta` 认 `undefined`），那时只说类型名 ——
          // 不像上一版那样回退成登记日期（对端看到的是"别人哪天加的"，没有意义）。
          meta: shareMeta(it.isDir, it.itemCount, it.size),
        }));
        const drill = rectView(d.id, root);
        return {
          id: d.id,
          kind: 'peer',
          // 卡片上的设备名用显示名（F-101：设了备注就是「备注(设备名)」）—— 与本机页写
          // 「本机」同一个目的：一眼看出这是哪一台。
          name: deviceDisplayName(d),
          ip: `${d.ip}:${d.port}`,
          loading: drill.loading ?? st.loading,
          error: drill.error ?? st.error,
          ...drill.view,
        };
      });
    return [local, ...peers];
  });

  // 钻进去的目录要拉内容：缓存里没有就拉一次（进去/返回上一级都是改 `rectDirs`，这里只负责补数据）。
  $effect(() => {
    for (const [pageId, loc] of Object.entries(rectDirs)) {
      if (!loc || rectKids[rectKidsKey(pageId, loc)]) continue;
      void loadRectKids(pageId, loc);
    }
  });

  // 折叠态窗口的几何：窗口多大、悬浮体（球 / 框）在窗口里的哪一块。全部 CSS px，见 orbShape.ts。
  // 圆角矩形那一档是**定尺**：设备接入/断开（点列画不画）、卡片里 IP 多长都不改窗口宽度。
  const layout = $derived(collapsedLayout(orbShape));

  // 卡上那一页的共享项：长连接设备一出现就拉回来（与打开它的标签页走同一条路）。
  $effect(() => {
    if (orbShape !== 'rect') return;
    for (const d of getDevices()) {
      if (!d.connected) continue;
      const st = getPeerSharesState(d.id);
      if (!st.loaded && !st.loading) void loadPeerShares(d.id);
    }
  });

  // 形态一变（且此刻是折叠态），窗口尺寸就跟着归到这一档。位置不动：悬浮体在窗口里的左上角
  // 是固定余量，改尺寸不会让它跳。
  //
  // **`sizedShape` 是那道闩**：形态只能在面板开着时改（「悬浮形态」在设置里，而设置只在展开态
  // 可达），所以收起那条路（`settleCollapse`）天然已经按当前形态归过位、并判过一次吸附 ——
  // 形态一换、`holdOrb` 一落，这个 `$effect` 就会重跑一次，顺手 `resetOrbDom()` 把刚贴好的边
  // 清掉：卡片从贴边竖条弹回完整位。归过位的那一档不再跑，就只剩"启动时设置回来"那一趟。
  $effect(() => {
    const size = layout.size;
    if (orbShape === sizedShape || expanded || holdOrb || opening) return;
    void (async () => {
      try {
        const pos = await appWindow.outerPosition();
        const sf = await appWindow.scaleFactor();
        await orbSetRect({ x: pos.x, y: pos.y, w: Math.round(size.w * sf), h: Math.round(size.h * sf) });
        sizedShape = orbShape;
        resetOrbDom();
      } catch (e) {
        console.error('按形态改折叠态窗口尺寸失败:', e);
      }
    })();
  });

  /**
   * 右键菜单自己的盒子（窗口坐标，CSS px）。菜单是 `position: fixed`，而夹取按**窗口**算
   * （`ctxPlacement` 的 `bounds` = 视口，见 ContextMenu.svelte）—— 所以它可能压在那圈阴影余量上，
   * 报可点区域时得把它并进来，否则探进余量的那一截点不着（点下去穿到桌面去了）。两档都要并：
   * 面板里那一份（余量在面板四周）与**折叠态卡片上那一份**（窗口只有 236×162，菜单很容易探出
   * 卡片本身那几块）。
   *
   * 量 `offset*` 而不是 `getBoundingClientRect()`：菜单出现时带 0.12s 的 `scale(0.96)`
   * （转心在左上角），实时盒子会是缩小后的那一格；`offset*` 就是 `ctxPlacement` 写下的定位值与
   * 菜单的固有尺寸 —— 稳定之后的那一格。
   */
  function ctxMenuRects(): ScreenRect[] {
    if (!ctxState) return [];
    // 折叠态那一份：落点与尺寸都是 App 自己算出来的（`placeOrbMenu`），直接用，**不去 DOM 里读** ——
    // action 把新的 left/top 写进元素与这个 `$effect` 跑在同一批更新里，读 DOM 会读到**上一格**
    // （菜单还没搬过去，报出去的却是旧位置：那一小段屏幕就点不着了）。还没定好落点时它不可见，
    // 也就没有要吃指针的盒子。
    if (ctxState.orb) {
      const w = ctxState.menuW ?? 0, h = ctxState.menuH ?? 0;
      return ctxState.placed && w > 0 && h > 0 ? [{ x: ctxState.x, y: ctxState.y, w, h }] : [];
    }
    const el = document.querySelector<HTMLElement>('[data-od-id="ctx-menu"]');
    return el ? [{ x: el.offsetLeft, y: el.offsetTop, w: el.offsetWidth, h: el.offsetHeight }] : [];
  }

  /**
   * 折叠期该吃指针的那一块（CSS px，窗口内）。
   *
   * **未贴边**时就是"画出来的那件东西"：圆球 = 球本身；圆角矩形 = 卡片，**有点列时**才并上右侧
   * 那 30px（间距 10 + 点列 20）。只有一页时点列不画（`RectOrb` 的 `{#if pages.length > 1}`，
   * 它拿的就是同一个 `rectPages`），那一列就一并交给桌面穿透 —— 不画也不吃指针的一列还占着，
   * 用户看到的就是"点空处没反应"。反过来，点列一画出来就必须留在区域内（指针得能挪上去点它），
   * 所以这个"收窄"只在**没有点列**时发生：`layout.body` 正是 `layout.widget` 减去那 30px
   * （圆球那一档两者相同，`rectPages` 也是空的，同一条式子照样对）。
   *
   * **贴边收起**时只吃**屏上看得见的那一条**（`--zone-*` = 露出的半颗 / 竖条）：呼出阈值就是
   * 看得见的那点儿 —— 悬浮体收进去之后**才会**占的那一大片（圆球一整颗的方框 / 卡片整张，
   * 见 `planDock` / `planRectDock` 的 `zone`）也算进来的话，收起态就在屏幕边上圈着一大块：
   * 指针从旁边路过就把悬浮体呼出来，压在下面的窗口也点不着。
   * 呼出**之后**（`reveal`：悬停，或拖着东西进来 —— 见它的说明）才把悬浮体本体那一块并进来：
   * 那时它已经滑回完整位、指针就落在它上面 —— 不含它的话，指针一离开那一条，窗口就当场穿透、
   * hover 丢掉，滑回来的悬浮体又收回去，一进一出抖个不停。并集里两块都不动，只有悬浮体在中间滑。
   */
  function collapsedHitRects(): ScreenRect[] {
    // 菜单撑窗那一档：窗口原点挪过，窗口内的东西整体反向平移过（`.orb-stage` 的 `--menu-dx/dy`）——
    // 这里报的是**窗口**坐标，所以几块都得跟着挪；不挪的话卡片画在一处、吃点击的却是另一处
    // （整张卡都不响应，点下去穿到桌面）。关着菜单时 `d` 是零，同一条式子照走。
    const d = orbMenuGeo.shift;
    if (dockAxis.x !== 0 || dockAxis.y !== 0) {
      const card = bodyRect({ x: layout.widget.x, y: layout.widget.y });
      const rects = reveal ? [{ ...orbZone }, card] : [{ ...orbZone }];
      return rects.map((r) => ({ ...r, x: r.x + d.x, y: r.y + d.y }));
    }
    const body = bodyRect(orbOffset);
    return [{ x: body.x + d.x, y: body.y + d.y, w: body.w, h: body.h }];
  }

  /** 悬浮体**本体**在窗口里占的那一块（`at` = 它在窗口内的左上角）：
   *  未贴边时它就是可点区域，贴边呼出后是并集里的第二块（那时它停在 `--orb-inset`，
   *  也就是 `layout.widget` 的位置）。有点列时并上右侧那 30px，见 `collapsedHitRects`。 */
  function bodyRect(at: { x: number; y: number }): ScreenRect {
    return { x: at.x, y: at.y, w: rectPages.length > 1 ? layout.widget.w : layout.body.w, h: layout.body.h };
  }

  /**
   * 报给后端的「可点区域」：窗口里该吃点击的那几个盒子（`hit_region.rs` 据此按指针位置开关
   * 整窗穿透）。窗口比看得见的那件东西大一圈，而**透明像素照样算命中** —— 不报的话，那圈余量
   * 会把点击全吃掉（压在下面的窗口收不到，焦点还会被抢走）。
   *
   * 取「当前会吃指针的东西」：
   * - 面板期（含切窗口那一小段 `holdOrb`：面板还画着、窗口还没缩回去）：面板的可视盒，外加开着的
   *   右键菜单（见 `ctxMenuRects`）。面板里的对话框/设置/选择框都挂在面板里，被它的
   *   `overflow: hidden` 裁在面板内，不用另算。
   * - 折叠期：见 `collapsedHitRects`，**同样并上开着的右键菜单** —— 卡片那一档的菜单可能压着
   *   `collapsedHitRects` 那几块之外的余量（菜单比卡片宽/靠下时都会），不并进来，探出卡片的那一截
   *   点下去就穿到桌面了。
   *
   * 这个 `$effect` 会在它读到的每一个状态变化时重跑（形态/偏移/贴边方向/页数/展开/右键菜单…），
   * 所以区域是**动态**的：点列随设备增减出现或消失，那一列的可点性当场跟着变；贴边那一档收起时
   * 只报看得见的那一条，指针落上来（`zoneHot`）再当场把悬浮体本体并进来。
   */
  $effect(() => {
    const rects: ScreenRect[] = expanded || holdOrb
      ? [{ x: PANEL_SLACK, y: PANEL_SLACK, w: PANEL.w, h: PANEL.h }, ...ctxMenuRects()]
      : [...collapsedHitRects(), ...ctxMenuRects()];
    void orbSetHitRegion(rects).catch((e) => console.error('设置可点区域失败:', e));
  });

  /**
   * 折叠态菜单的落点：菜单先渲染出来（`pending`，不可见）→ 量出它多大 → 撑窗 + 定位 →
   * 放开显形（`placeOrbMenu`）。这个 `$effect` 读的是 `ctxState`，所以它跑在 DOM 更新之后 ——
   * 那一刻菜单已经在文档里，量得到。
   */
  $effect(() => {
    const c = ctxState;
    if (!c?.orb || c.placed !== false) return;
    void placeOrbMenu(c);
  });

  /**
   * 菜单关掉（或被面板那一份顶掉）之后，窗口缩回折叠态那一档。读 `orbCtxOpen` 即可：
   * 它一变就说明这一档结束了，而 `restoreOrbWindow` 自己会看 `grown` 决定要不要动手。
   */
  $effect(() => {
    if (orbCtxOpen) return;
    void restoreOrbWindow();
  });

  /**
   * 双击卡上的一格：**目录 = 钻进去，文件 = 打开**。策略全在这里，卡片只负责说"第几页的第几格
   * 被双击了"（它不知道共享项、路径与打开方式）。
   *
   * 共享项的名字只在最外那一层取得到（子项只给相对路径），所以进第二层时从当前那层继承 ——
   * 头那行路径要一直是"共享项名/子路径"。
   */
  function activateRectSlot(page: RectPage, slot: RectSlot) {
    if (!slot.isDir) {
      void openRectFile(page, slot);
      return;
    }
    const cur = rectDirs[page.id];
    rectDirs[page.id] = {
      kind: page.kind,
      shareId: slot.id,
      shareName: cur?.shareName ?? slot.name,
      rel: slot.path,
    };
  }

  /** 回上一层：子目录 → 父目录 → 共享项根 → 共享项清单（`null`）。 */
  function upRect(pageId: string) {
    const loc = rectDirs[pageId];
    if (!loc) return;
    const i = loc.rel.lastIndexOf('/');
    rectDirs[pageId] = loc.rel === '' ? null : { ...loc, rel: i < 0 ? '' : loc.rel.slice(0, i) };
  }

  /**
   * 右键卡上的一格：与面板里右键一个共享项**同一个菜单、同一套处理**（`kind: 'share'`，
   * 于是「打开所在文件夹 / 复制共享路径 / 移除共享项」全落到已有的那几个 `ctx*` 上）。
   *
   * **只有本机页最外那一层**才长菜单：钻进去之后列的是共享项**里面**的东西（那是路径，
   * 不是登记项 —— 没有"移除"这回事），设备页列的是别人的共享项（本机删不掉它）。两处右键
   * 都不做事，与卡片上"单击不做事"同一套克制。
   *
   * `orb: true` 是"这次开在折叠态卡片上"的标记，它带来三件事：`.reveal` 那一档要多算一路
   * （`orbCtxOpen`）、可点区域要把菜单盒并进来、以及**落点由 `placeOrbMenu` 现算**（先撑窗，
   * 再把菜单摆到指针的右下/左下）—— 所以这里先给 `placed: false`：菜单先渲染成不可见，
   * 等落点定好再显形（否则用户会看到它先盖在卡片上、再跳到旁边去）。
   */
  function openRectCtx(pageIdx: number, slotIdx: number, x: number, y: number) {
    const page = rectPages[pageIdx];
    const slot = page?.items[slotIdx];
    if (!page || page.kind !== 'local' || page.dir || !slot) return;
    ctxState = { x, y, kind: 'share', shareId: slot.id, name: slot.name, orb: true, placed: false };
  }

  /** 菜单与屏幕边留的余量（CSS px）：与 `ctxPlacement` 的 `MARGIN` 同一个数。 */
  const MENU_MARGIN = 8;
  /** 撑窗时额外给菜单阴影留的余量（CSS px）：`--shadow` 的可见半径左右 12 / 上 7 / 下 17，
   *  取 16 —— 正好贴着窗口边的话阴影会被硬裁成一条切线。 */
  const MENU_SLACK = 16;

  /**
   * 折叠态那份菜单的**落点与撑窗**：指针的右下（右边的屏幕位置不够就翻到指针左侧，下边同理
   * 翻上去），再按工作区夹一次；随后**把折叠态窗口撑到装得下这一枚菜单**。
   *
   * 为什么要撑窗：菜单是 `position: fixed`，超出窗口的部分会被窗口边硬裁 —— 夹在那 236×162 里
   * 就只能压着卡片本身（用户看到的是"菜单盖住了悬浮框"）。撑出来的那块是**透明**的，也不吃点击
   * （可点区域只报卡片与菜单，见 `collapsedHitRects` / `ctxMenuRects`），所以它只是"菜单够得着
   * 的地方"，不是第二件看得见的东西。
   *
   * 窗口只长大、不缩小：新矩形 = **当前窗口** ∪ 菜单盒，于是卡片、分页点、阴影余量一个都不会
   * 被挤出窗口。原点则**尽量不动**（右下展开那一档根本不动）—— 只有翻到左侧那一档需要把原点
   * 往左挪，而窗口内坐标得反向平移同样的量才在原地不动（`.orb-stage` 的 `--menu-dx/dy`，
   * 见 main.css）；菜单挂在 stage 外面，坐标不受那次平移影响，所以它按**真窗口坐标**摆就是了。
   *
   * 原点要挪的那一两帧里卡片先不出镜（`orbMenuBusy`）：窗口已经到新位置、内容还是旧的窗口内
   * 坐标，不藏就是"卡片横跳一次"。那几帧里菜单也还没显形，所以整扇窗口是空的 —— 换位置看不见。
   */
  async function placeOrbMenu(c: CtxState) {
    const el = document.querySelector<HTMLElement>('[data-od-id="ctx-menu"]');
    if (!el) return;                       // 菜单已经不在了（刚被关掉）：无事可做
    const mw = el.offsetWidth, mh = el.offsetHeight;   // 尺寸随字号/文本缩放变，量出来才算数
    if (orbMenuRestoring) await orbMenuRestoring;      // 上一趟缩窗还没落地：等它，否则两次 SetWindowPos 打架
    try {
      const sf = await appWindow.scaleFactor();
      const pos = await appWindow.outerPosition();
      const work = (await orbScreenMetrics()).workArea;
      const m = Math.round(MENU_MARGIN * sf);
      const sl = Math.round(MENU_SLACK * sf);
      const pw = Math.round(mw * sf), ph = Math.round(mh * sf);
      const cw = Math.round(window.innerWidth * sf), ch = Math.round(window.innerHeight * sf);
      // 指针在**屏幕**上的位置（`c.x/y` 是右键那一刻报上来的窗口内坐标）
      const px = pos.x + Math.round(c.x * sf);
      const py = pos.y + Math.round(c.y * sf);
      let mx = px + pw + m > work.x + work.w ? px - pw : px;      // 右边装不下 → 翻到指针左侧
      let my = py + ph + m > work.y + work.h ? py - ph : py;      // 下边装不下 → 翻到指针上边
      mx = Math.max(work.x + m, Math.min(mx, work.x + work.w - pw - m));   // 翻过去还越界（工作区比菜单还小）：夹住
      my = Math.max(work.y + m, Math.min(my, work.y + work.h - ph - m));
      // 窗口还要给菜单的**阴影**留出可见半径（`--shadow` 左右 12 / 上 7 / 下 17）：正好贴着窗口边
      // 的话，阴影会被窗口边硬裁成一条切线。这块多出来的透明区域不吃点击（可点区域只报菜单盒本身）。
      const rect = {
        x: Math.min(mx - sl, pos.x),
        y: Math.min(my - sl, pos.y),
        w: Math.max(mx + pw + sl, pos.x + cw) - Math.min(mx - sl, pos.x),
        h: Math.max(my + ph + sl, pos.y + ch) - Math.min(my - sl, pos.y),
      };
      const dx = (rect.x - pos.x) / sf, dy = (rect.y - pos.y) / sf;   // 原点挪了多少（往左/上是负）
      if (rect.x !== pos.x || rect.y !== pos.y || rect.w !== cw || rect.h !== ch) {
        orbMenuBusy = dx !== 0 || dy !== 0;    // 只往右下长时画面上本来就看不出来，不必藏
        await orbSetRect(rect);
        // **累加**：右键点另一格时窗口多半已经撑过一次（原点也挪过），这次挪的是相对**当前**原点的
        // 增量 —— 把它加到已有的那一段上，不然 stage 的平移会被清零，卡片当场横跳一段。
        orbMenuGeo = { grown: true, shift: { x: orbMenuGeo.shift.x - dx, y: orbMenuGeo.shift.y - dy } };
      }
      // 落点换算到（新的）窗口坐标里：窗口原点就是 `rect.x/y`，菜单不受 stage 那个平移影响。
      // 这里**不等** `waitForViewport`：菜单是 `pinned` 的（`ctxPlacement` 一个像素都不动），
      // 落点与它无关；而那个等待是数 rAF 的 —— 窗口被遮住/后台时 rAF 会被节流，菜单就迟迟不显形。
      const left = (mx - rect.x) / sf, top = (my - rect.y) / sf;
      if (ctxState?.orb) {
        ctxState.x = left;
        ctxState.y = top;
        ctxState.menuW = mw;
        ctxState.menuH = mh;
        ctxState.placed = true;
      }
    } catch (e) {
      console.error('右键菜单落点/撑窗失败:', e);
      if (ctxState?.orb) ctxState.placed = true;   // 撑不动就按现状显形（夹在窗口里，总比不显示强）
    } finally {
      orbMenuBusy = false;
    }
  }

  /**
   * 菜单关掉之后，把窗口缩回折叠态该有的那一档：尺寸回到布局尺寸、原点回到"没挪过"的位置。
   *
   * 两个一起改，卡片在屏幕上原地不动（原点回到基础位 ⇔ 窗口内坐标去掉那次平移）。`shift` 要等
   * 窗口真的挪回去之后再清 —— 先清的话，卡片会带着还没归位的窗口先跳一下。
   */
  async function restoreOrbWindow() {
    if (orbMenuRestoring) return orbMenuRestoring;
    if (!orbMenuGeo.grown) return;
    const shift = orbMenuGeo.shift;
    orbMenuRestoring = (async () => {
      try {
        // 面板接管了几何（展开那一趟自己会把窗口摆正）时不用再缩 —— `expand` 已经把账结清了
        if (!(expanded || holdOrb || opening)) {
          const sf = await appWindow.scaleFactor();
          const pos = await appWindow.outerPosition();
          await orbSetRect({
            x: pos.x + Math.round(shift.x * sf),
            y: pos.y + Math.round(shift.y * sf),
            w: Math.round(layout.size.w * sf),
            h: Math.round(layout.size.h * sf),
          });
        }
      } catch (e) {
        console.error('缩回折叠态窗口失败:', e);
      } finally {
        orbMenuGeo = { grown: false, shift: { x: 0, y: 0 } };
        orbMenuRestoring = null;
      }
    })();
    await orbMenuRestoring;
  }

  /**
   * 双击一个文件 = 用系统默认程序打开它。
   *
   * 本机项**直接开原文件**（零拷贝：文件本来就在这台机器上，开一份副本只会让人对着两份文件
   * 发愣）；对端项的文件不在本机，先按 S-02 的下载模式取一份到下载目录再打开 —— 之后想再开
   * 一次、或想留着这份，都能从落盘那份走（所以提示里要说清"是下载来的副本"）。
   */
  async function openRectFile(page: RectPage, slot: RectSlot) {
    try {
      if (page.kind === 'local') {
        const share = getShares().find((s) => s.id === slot.id);
        if (!share) throw new Error('共享项已不存在');
        await openPath(slot.path ? `${share.path}/${slot.path}` : share.path);
        return;
      }
      const copy = await downloadFromPeer(page.id, slot.id, slot.path, slot.name);
      // 「每次询问」下用户在保存框里取消：什么都没发生，别去打开一个不存在的路径。
      if (!copy) return;
      await openPath(copy);
      toast(`已下载「${slot.name}」的副本并打开`);
    } catch (e) {
      toast(`打开失败：${e instanceof Error ? e.message : String(e)}`);
    }
  }

  /**
   * 展开：先立即藏住悬浮球，再算方向/落位，一次 SetWindowPos 原子切换窗口，
   * 最后才让面板从悬浮球那一侧长出。悬浮球在切窗口期间绝不参与渲染，
   * 因此不存在「画在旧窗口坐标/旧偏移」的错位帧。
   *
   * 方向/落位以**悬浮体本体中心**（球心 / 卡片白底中心）为参考点，面板的可视盒角落在它上面：
   * 同一个中心，两种形态展开出的面板逐像素相同（见 `orbLayout.planExpansion`）。
   */
  async function expand() {
    if (opening || expanded) return;
    opening = true;
    holdOrb = true;
    // 面板要长出来了，折叠态那份菜单（如果开着）当场作废：窗口马上从 236×162 变成面板，
    // 菜单还按旧坐标飘在卡片原位就成了面板上的一块残影。菜单那一档的撑窗账也当场结清
    // （下面算参考点要用它，算完就得清零 —— 平移留在 stage 上会把面板也画歪；卡片此刻已被
    // `holdOrb` 藏住，所以这一步看得见的是"面板摆正"，不是"卡片跳一下"）。
    ctxState = null;
    const menuShift = orbMenuGeo.shift;
    orbMenuGeo = { grown: false, shift: { x: 0, y: 0 } };
    try {
      const sf = await appWindow.scaleFactor();
      const pos = await appWindow.outerPosition();
      // 参考点 = 悬浮体本体中心此刻在屏幕上的位置 = 窗口左上角 + 它在窗口内的偏移 + 半个本体。
      // 偏移用 `orbOffset`（贴边收起时是贴边那一档的偏移 —— 悬浮体大半在窗口外，中心也就在
      // 屏幕外一点；下面按贴边档给 margin 0，让面板贴住屏幕边）。`menuShift` 那一段必须算进去：
      // 菜单撑窗时窗口原点挪过、窗口内坐标反向平移过，`orbOffset` 本身不变。
      const center = {
        x: pos.x + Math.round((orbOffset.x + menuShift.x + layout.body.w / 2) * sf),
        y: pos.y + Math.round((orbOffset.y + menuShift.y + layout.body.h / 2) * sf),
      };
      const metrics = await orbScreenMetrics();
      const plan = planExpansion({
        center,
        work: metrics.workArea,
        box: { w: Math.round(PANEL.w * sf), h: Math.round(PANEL.h * sf) },
        slack: Math.round(PANEL_SLACK * sf),
        // 贴边那一档：该轴不再留边距，窗口贴住工作区边（面板因此贴着屏幕边）
        margin: {
          x: Math.round((dockAxis.x === 0 ? WORK_MARGIN : 0) * sf),
          y: Math.round((dockAxis.y === 0 ? WORK_MARGIN : 0) * sf),
        },
      });

      if (foldTimer) { clearTimeout(foldTimer); foldTimer = null; }
      orbAnchor = center;
      expandedRect = plan.window;
      // 展开态悬浮体在窗口里的偏移（CSS 变量仍是**本体左上角**：CSS 按它定位 `.orb` / `.rq`）
      setOrbPos({
        x: (plan.anchorOffset.x - Math.round(layout.body.w * sf / 2)) / sf,
        y: (plan.anchorOffset.y - Math.round(layout.body.h * sf / 2)) / sf,
      });
      await orbSetRect(plan.window);
      await waitForViewport(EXPANDED.width, EXPANDED.height);
      expanded = true;
      holdOrb = false; // 悬浮球继续由 .open 隐藏，直到收起
    } catch (e) {
      console.error('展开悬浮球失败:', e);
      resetOrbDom();   // 窗口没长开，球别按展开偏移留在一个看不见的位置
      holdOrb = false;
      toast('展开失败，请重试');
    } finally {
      opening = false;
    }
  }

  /**
   * 悬浮体回到「未贴边」的折叠态：居中内缩、热区就是悬浮体自己、角标不转、竖条收起。
   * 四个调用点：展开失败的回退、**起拖那一刻**（拖动期间不能挂着上一次的贴边偏移，
   * 见 `beginOrbDrag`）、形态/尺寸刚变过的那一次重设。
   */
  function resetOrbDom() {
    setOrbPos({ x: layout.widget.x, y: layout.widget.y });
    badgeSpin = { left: 0, right: 0 };
    rectDock = 0;
    dockAxis = { x: 0, y: 0 };
  }

  /** 悬浮体（球 / 卡片）在窗口内的偏移 + 热区（热区尺寸跟着形态走：圆球 62、悬浮框 卡片+点列） */
  function setOrbPos(off: { x: number; y: number }) {
    orbOffset = off;
    orbZone = { x: off.x, y: off.y, w: layout.widget.w, h: layout.widget.h };
  }

  /**
   * 折叠态位置在两种坐标之间换算，参考点是**悬浮体本体中心**（屏幕物理像素）。
   *
   * 展开时参考点 = 窗口左上角 + `--orb-x/y` + 半个本体（见 `expand`）；收起时反过来 ——
   * 用本体中心在窗口内的偏移反推出**未贴边**的窗口矩形，再交给贴边判定复现贴边
   * （吸附幂等：同一个矩形再判一次得到同样的窗口与偏移，见 `planDock` / `planRectDock`）。
   *
   * 本体中心是唯一参考点，窗口左上角只是它减去"本体中心到窗口左上角"的那一段
   * （球 45/45、卡片 102/77，由 `orbShape.ts` 的 `body` 决定）—— 切形态时窗口跟着让位，
   * 悬浮体以中心为锚原地长大/缩小，位置本身一动不动。
   */
  function bodyCenterInWindow() {
    const b = layout.body;
    return { x: b.x + b.w / 2, y: b.y + b.h / 2 };
  }
  function windowFromCenter(center: { x: number; y: number }, sf: number) {
    const c = bodyCenterInWindow();
    return { x: center.x - Math.round(c.x * sf), y: center.y - Math.round(c.y * sf) };
  }
  /** 一个贴边/落位结果里的本体中心在哪（物理像素）：窗口 + 该档的偏移（贴边时大半在屏外）+ 半个本体 */
  function centerOf(plan: DockPlan, sf: number) {
    return {
      x: plan.window.x + plan.orb.x + Math.round(layout.body.w * sf / 2),
      y: plan.window.y + plan.orb.y + Math.round(layout.body.h * sf / 2),
    };
  }

  /** 圆角矩形悬浮框的落位：离左右工作区边缘够近就贴边收起成一条竖条（`planRectDock`） */
  function rectDockPlan(free: ScreenRect, sf: number, work: ScreenRect): DockPlan {
    return planRectDock({
      window: free,
      work,
      peek: Math.round(RECT_DOCK_W * sf),
      widget: {
        x: Math.round(layout.widget.x * sf),
        y: Math.round(layout.widget.y * sf),
        w: Math.round(layout.widget.w * sf),
        h: Math.round(layout.widget.h * sf),
      },
      cardW: Math.round(RECT_CARD_W * sf),
      threshold: Math.round(DOCK_THRESHOLD * sf),
      margin: Math.round(WORK_MARGIN * sf),
    });
  }

  /** 悬停热区（窗口内 CSS px）= **收起态屏上看得见的那一条**；贴边方向一并定下，球还没滑出去时
   *  它就先就位了。注意它不是呼出之后的热区 —— 那个是它 ∪ 悬浮体本体，由 `collapsedHitRects`
   *  按 `zoneHot` 现拼（悬浮体滑回完整位后指针落在的那块在这儿之外）。 */
  function setDockZone(plan: DockPlan, sf: number) {
    orbZone = { x: plan.zone.x / sf, y: plan.zone.y / sf, w: plan.zone.w / sf, h: plan.zone.h / sf };
  }

  /**
   * 悬浮体偏移 + 贴边朝向 = 贴边收起后的最终形态。
   *
   * 圆球：偏移 + 两颗角标绕球心的转角（`spinForDock`），这两样是同一段动画的两半。
   * 悬浮框：偏移 + **贴边方向**（`rectDock`）—— 竖条钉在哪一端、哪一枚角标往里收、
   * 卡与竖条谁渐显，全由 CSS 按这个方向选；角标不转（矩形没有弧可绕）。
   *
   * 分两个入口只是为了控制它从哪一刻开始跑：悬浮体此刻可见（拖动落位）→ `setDockDom`
   * 一次到位；还藏在 `holdOrb` 里（收起）→ 见 `revealDock`。
   */
  function setDockOrb(plan: DockPlan, sf: number) {
    orbOffset = { x: plan.orb.x / sf, y: plan.orb.y / sf };
    dockAxis = plan.dock;     // 展开时要知道贴边方向（贴边档不给边距，窗口贴住工作区边）
    if (orbShape === 'rect') {
      rectDock = plan.dock.x;
      badgeSpin = { left: 0, right: 0 };
      // 贴边落位那一瞬间指针还压在悬浮体上（拖动落位必然如此：松手点就在卡片上）。照悬停直接
      // 显示的话，"把卡片拖到边上"这一步**看不到任何结果** —— 竖条被滑回来的卡片盖着、卡片又
      // 与贴边前一模一样，只有指针离开热区之后竖条才现身。所以落位时先压住这一次显示：
      // 指针离开热区才解除，之后进出与圆球那套一致（`zoneHold` 见 markup 的两个指针事件）。
      zoneHold = zoneHot;
      zoneHot = false;
    } else {
      badgeSpin = spinForDock(plan.dock);   // 角标转到哪：规则与实测都在 `orbLayout` 里
    }
  }

  /** 落位结果一次性生效（拖动落位用） */
  function setDockDom(plan: DockPlan, sf: number) {
    setDockZone(plan, sf);
    setDockOrb(plan, sf);
  }

  /** 把悬浮体停在「完整可见」位、角标不转、不贴边：收起时先这样露面，再收进去（见 `revealDock`） */
  function parkOrb() {
    orbOffset = { x: layout.widget.x, y: layout.widget.y };
    badgeSpin = { left: 0, right: 0 };
    rectDock = 0;
    dockAxis = { x: 0, y: 0 };
  }

  /**
   * 逼一次样式重算（读一次布局即触发）。
   *
   * `holdOrb` 期间 `.orb` 是 `transition: none` —— 这一帧算出来的值，正是之后那段过渡的
   * **起点**。不逼这一下，等会儿「解除 hold」与「改球偏移」有可能落进同一次样式重算，
   * 起点就退回更早的旧偏移（球会从它展开时所在的那一格一路飞过来）。见 `revealDock`。
   *
   * 调用前要先 `await tick()`：`parkOrb` 写的是 `$state`，Svelte 的 DOM 更新排在同一段微任务里，
   * 同步读布局读到的还是改之前的值，等于没逼。
   */
  function flushLayout() {
    void document.body.offsetHeight;
  }

  /**
   * 贴边收起那段**给人看**的动画：球从完整位滑出屏幕外（贴边侧探出 `overhang`），两个角标
   * 同时沿球缘逆时针转 90°。
   *
   * 排在球露面之后（`holdOrb = false`）再等一帧：隐藏期间球的位置换了两次（展开位 → 完整位
   * 由 `parkOrb` 落定，`flushLayout` 已把起点算出来），这一帧要先把「球完整可见」画出来，
   * 下一帧才改偏移 —— 于是滑动是从屏幕边滑进去的，而不是瞬移。
   */
  async function revealDock(plan: DockPlan, sf: number) {
    if (!plan.dock.x && !plan.dock.y) return;   // 未贴边：悬浮体本来就停在完整位
    await new Promise(requestAnimationFrame);
    setDockOrb(plan, sf);
  }

  /** 吸附判定：自由落位的折叠态窗口 → 贴边收起大半颗（未贴边的轴保持原样）。
   *  只有圆球走这条路：圆角矩形悬浮框的落位是 `rectDockPlan`（收起成竖条，见 `planRectDock`）。 */
  function dockPlan(ball: ScreenRect, sf: number, work: ScreenRect): DockPlan {
    return planDock({
      ball,
      work,
      orbSize: Math.round(ORB_SIZE * sf),
      orbInset: Math.round(layout.widget.x * sf),
      overhang: Math.round(ORB_SIZE * (1 - DOCK_EXPOSE) * sf),
      threshold: Math.round(DOCK_THRESHOLD * sf),
    });
  }

  /**
   * 把悬浮球窗口的拖动交给 OS（`Orb.svelte` 越过阈值、且球没有正在滑动时调用）。
   *
   * OS 的窗口拖动（`startDragging` → `WM_NCLBUTTONDOWN`/HTCAPTION 模态循环）在**起拖那一刻**
   * 把窗口钉在指针上：此后「指针在窗口里的位置」就是整个拖动的不变量。于是从按下点走到那一刻
   * 的位移会永久丢失 —— 球在指针旁边偏着走，松手时指针落在球上的相对位置也不是按下时那个
   * （快速甩鼠标时一个 pointermove 就有十几到几十像素，偏得肉眼可见）。按住面板头部拖动没这
   * 问题：那条路径在 mousedown 当场上交，锚点天然就是按下点。
   *
   * 校正与起拖必须在**同一瞬间**发生：指针位置在后端现读（不用事件坐标 —— pointermove 最多
   * 滞后一帧，高速拖动时那一帧就是十几个像素），挪完窗口当场 `start_dragging`，中间不回前端。
   * 「球正在悬停滑入」那一段由 `Orb` 挡在起拖之前（等滑动结束），所以这里只需算指针那一段。
   */
  async function beginOrbDrag(info: OrbDragInfo) {
    // 起拖当场把球放回「完整可见」位（内缩 + 角标不转），热区也收回球自己身上。
    // 右键菜单也当场作废：它是按**窗口**坐标画在那一刻的位置上的（折叠态那一份的锚点就是
    // 卡片上被右键的那一格），窗口一被拖走，菜单要么跟着卡片飘、要么指着一个已经不存在的锚点 ——
    // 与"收起面板时菜单作废"同一条口径（见 `collapse`）。
    //
    // 拖动期间球是跟着窗口（指针）走的，而球偏移只有落位判定（`settleDrag`）才会更新 ——
    // 上一次贴边收起的偏移（`--orb-x` = 69 那种）会一直挂在球上：指针一旦离开球，悬停那层
    // 遮不住，球就带着贴边偏移画到窗口外，屏幕上就是「边缘被切一刀」；松手后按新落点重新
    // 贴边，看起来就成了「先播了一次旧方向的贴边动画，再播新方向的」（拖到上边却先出现
    // 右侧那道裁切线，就是这么来的）。
    // 热区收回球自己身上还消掉一个抖动源：拖动期间悬停与不悬停同形，指针相对球怎么漂都一样。
    // 窗口对齐（`orbDragStart` → Rust 的 `drag_align`）也是按「球在完整位」算的：按下点本就
    // 落在完整位上的球上，偏移一清，球才真钉在指针下。
    resetOrbDom();
    ctxState = null;
    try {
      await orbDragStart(info.pressX, info.pressY);
    } catch (e) {
      console.error('悬浮球起拖失败:', e);   // 起不来就是这一次拖不动，别把异常抛进事件处理里
      void settleDrag();                     // 球已放回完整位：按当前窗口位置重判一次落位
    }
  }

  /**
   * 记录悬浮体本体中心（物理像素）：下次启动回到这里（F-74）。
   *
   * 记的永远是**悬浮体**的位置（它的中心），不是窗口的：窗口左上角 = 中心 − 本体内缩
   * （`windowFromCenter`），随形态与 DPI 变。调用点都在「几何落定之后」：settleDrag
   * （拖动结束、吸附判定跑完）、settleCollapse（收起落位完）、rememberBallUnderPanel
   * （展开态拖完面板）。
   */
  async function rememberOrbCenter(center: { x: number; y: number }) {
    try {
      await orbSaveCenter(Math.round(center.x), Math.round(center.y));
    } catch (e) {
      console.error('记录悬浮球位置失败:', e);   // 记不上只是下次回不到原位，不打断交互
    }
  }

  /**
   * 拖动结束后的落位判定。
   *
   * 折叠态：窗口就是悬浮球本身，拿它当前矩形当自由落位再判一次吸附，吸附结果落到窗口上。
   * 展开态：窗口是面板，拖动它时球跟着面板走 —— 球的新位置**算**出来（按窗口位移平移展开前
   *   记下的原矩形，再判一次吸附），只记录、不动窗口：球要贴边就把面板拽走显然不对。
   *
   * 判定前先等鼠标真正松开 —— OS 拖动期间窗口一直在动、页面却收不到 mouseup，只看 move
   * 事件的间隔会把「拖到一半停一下」当成松手：球会在手里被抢到贴边位置。松手那一刻的
   * 窗口位置才是落点，球从那儿滑出去。
   *
   * 贴边结果与当前位置一致时不发 SetWindowPos —— 否则这次移动又会触发 move 事件，
   * 判定再跑一遍，空转成环。
   */
  async function settleDrag() {
    if (settling || holdOrb || opening) return;
    // 菜单撑窗期间窗口的几何是菜单说了算（那一趟 `orbSetRect` 自己也会触发一次 `onMoved`）：
    // 这一档不能按"折叠态窗口"去重判落位 —— 撑出来的那一块会被当成用户拖出来的位置。
    if (orbMenuGeo.grown) return;
    settling = true;
    const from = expanded;
    try {
      while (await orbMouseDown()) {
        await new Promise((r) => setTimeout(r, DRAG_POLL_MS));
        if (holdOrb || opening || expanded !== from) return;   // 期间展开/收起/切窗口：这轮作废
      }
      if (from) {
        await rememberBallUnderPanel();
        return;
      }
      const pos = await appWindow.outerPosition();
      const size = await appWindow.outerSize();
      const metrics = await orbScreenMetrics();
      // 折叠态窗口的尺寸永远由**当前形态**定（不是此刻 OS 窗口测出来的那个）：形态刚换过时
      // OS 那边还是旧尺寸，照它判吸附会把窗口的宽高带错（圆球的贴边偏移是按球径与内缩算的）。
      const free: ScreenRect = {
        x: pos.x,
        y: pos.y,
        w: Math.round(layout.size.w * metrics.scaleFactor),
        h: Math.round(layout.size.h * metrics.scaleFactor),
      };
      sizedShape = orbShape;   // 这一趟落位之后窗口就是当前形态的尺寸了（见改尺寸那个 `$effect`）
      if (orbShape === 'rect') {
        // 悬浮框：离左右工作区边缘够近就贴边收起成一条竖条（上下两条边只夹进工作区）
        const plan = rectDockPlan(free, metrics.scaleFactor, metrics.workArea);
        if (plan.window.x !== pos.x || plan.window.y !== pos.y || plan.window.w !== size.width || plan.window.h !== size.height) {
          await orbSetRect(plan.window);
        }
        setDockDom(plan, metrics.scaleFactor);
        await rememberOrbCenter(centerOf(plan, metrics.scaleFactor));
        return;
      }
      const plan = dockPlan(free, metrics.scaleFactor, metrics.workArea);
      if (plan.window.x !== pos.x || plan.window.y !== pos.y || plan.window.w !== size.width || plan.window.h !== size.height) {
        await orbSetRect(plan.window);
      }
      setDockDom(plan, metrics.scaleFactor);
      await rememberOrbCenter(centerOf(plan, metrics.scaleFactor));
    } finally {
      settling = false;
    }
  }

  /**
   * 展开态拖动面板之后：算出悬浮体现在在哪、把它下次该出现的地方记下来。
   *
   * 与收起落位是同一套算法（`planCollapse` + `windowFromCenter` + `dockPlan`），所以这里记下的
   * 就是「此刻点收起，悬浮体会落到哪」—— 拖完面板直接关掉应用，下次启动也在那儿
   * （否则记的还是展开前的老位置）。
   */
  async function rememberBallUnderPanel() {
    if (!orbAnchor || !expandedRect) return;   // 异常路径：没有展开前的参考点，算不出球在哪
    const metrics = await orbScreenMetrics();
    const sf = metrics.scaleFactor;
    const center = planCollapse({ anchor: orbAnchor, expanded: expandedRect, current: await appWindow.outerPosition() });
    const at = windowFromCenter(center, sf);
    const free: ScreenRect = { x: at.x, y: at.y, w: Math.round(layout.size.w * sf), h: Math.round(layout.size.h * sf) };
    const plan = orbShape === 'rect'
      ? rectDockPlan(free, sf, metrics.workArea)
      : dockPlan(free, sf, metrics.workArea);
    await rememberOrbCenter(centerOf(plan, sf));
  }

  /**
   * 收起落位：先把悬浮体的参考点按窗口位移平移（悬浮体贴回面板里它原来所在的那一角），
   * 再按当前形态换算成折叠态窗口、判吸附 —— 面板被拖到屏幕边缘时，收起就是贴边收起半颗。
   *
   * 窗口（连同球的裁剪边界）与热区当场到位，**球偏移与角标转不转先押后**：它们要等球露面
   * 之后才切（`revealDock`），否则这段动画会在 `holdOrb` 的隐藏期间跑完 —— 球一露面就已经
   * 收进去了、角标也已经转好了，等于没做动画。
   */
  async function settleCollapse(anchor: { x: number; y: number }, expanded: ScreenRect | null): Promise<{ plan: DockPlan | null; sf: number }> {
    const metrics = await orbScreenMetrics();
    const sf = metrics.scaleFactor;
    const center = expanded
      ? planCollapse({ anchor, expanded, current: await appWindow.outerPosition() })
      : anchor;   // 没记下展开矩形（异常路径）：按原样缩回
    // 折叠态窗口由**当前形态**从本体中心反推（`anchor` 是展开那一刻记的：面板开着时用户改过
    // 悬浮形态的话，它的尺寸/中心偏移属于上一档）—— 于是切形态只挪窗口，悬浮体以中心为锚
    // 原地长大/缩小，位置一动不动。
    const at = windowFromCenter(center, sf);
    const free: ScreenRect = { x: at.x, y: at.y, w: Math.round(layout.size.w * sf), h: Math.round(layout.size.h * sf) };
    let plan: DockPlan | null = null;
    if (orbShape === 'rect') {
      plan = rectDockPlan(free, sf, metrics.workArea);
      await orbSetRect(plan.window);
      sizedShape = orbShape;      // 窗口已归到当前形态的尺寸，改尺寸那个 `$effect` 别再跑一趟
      setDockZone(plan, sf);      // 热区先按「看得见的那条竖条」就位（卡片还没滑出去时它就在了）
      parkOrb();                  // 先按"未贴边"露面：滑出去 + 卡片渐隐交给 `revealDock`
      await rememberOrbCenter(centerOf(plan, sf));   // 收起后的落位才是悬浮框下次该出现的地方
    } else {
      plan = dockPlan(free, sf, metrics.workArea);
      await orbSetRect(plan.window);
      sizedShape = orbShape;
      setDockZone(plan, sf);
      parkOrb();
      await rememberOrbCenter(centerOf(plan, sf));
    }
    await tick();                  // 等 park 落进 DOM（`$state` 的更新在微任务里）
    flushLayout();                 // 把「球完整可见」定成过渡起点（`holdOrb` 里 transition 是关的）
    return { plan, sf };
  }

  /**
   * 收起：面板先折叠淡出（此时窗口未变、球也没露脸），随后把窗口切到吸附后的落位 ——
   * 切完让悬浮球露面（**停在完整可见位**），露面那一帧之后才让它收进去、角标同时转 90°。
   * 于是「收进去」这段动画是看得见的：球只在几何完全 settle 之后才可见，但可见之后的
   * 那一段滑动/旋转留在明处，不在 `holdOrb` 的隐藏期间偷偷跑完。
   */
  function collapse() {
    expanded = false;
    showSettings = false;
    ctxState = null;
    picker = null;        // 选择框和右键菜单一样属于面板：收起即作废
    chat.setChatOpen(false);
    chat.closeRecipients();
    toasts = [];          // 提示是面板上的浮层：收起即作废，别等下次展开又冒出来
    holdOrb = true;
    if (foldTimer) clearTimeout(foldTimer);
    foldTimer = setTimeout(() => {
      foldTimer = null;
      const anchor = orbAnchor;
      const expanded = expandedRect;
      orbAnchor = null;
      expandedRect = null;
      void (async () => {
        let settled: { plan: DockPlan | null; sf: number } | null = null;
        try {
          if (anchor) settled = await settleCollapse(anchor, expanded);
          else resetOrbDom();   // 没记下参考点（异常路径）：回到未贴边的折叠态
        } catch (e) {
          console.error('收起悬浮球失败:', e);
        } finally {
          holdOrb = false;      // 几何已 settle，球可以露面了
        }
        // 露面之后才滑出去 / 收进去 + 转角标（两档形态都有贴边这一档，`plan` 为 null 时才是异常路径）
        if (settled?.plan) await revealDock(settled.plan, settled.sf);
      })();
    }, FOLD_MS);
  }

  // Toast
  function toast(msg: string) {
    const id = ++toastId;
    toasts = [...toasts, { id, msg, leaving: false }];
    setTimeout(() => {
      toasts = toasts.map(t => (t.id === id ? { ...t, leaving: true } : t));
      setTimeout(() => { toasts = toasts.filter(t => t.id !== id); }, 260);
    }, 2200);
  }

  // ── 自更新（F-110）─────────────────────────────────────────────────────
  // 启动时的静默检查发现新版（后端 `state.silent` 认这一档）时，右下角那组通知里就多一格
  // 等用户决定 —— 见 `upd.updateTask` 与上面 `notifCount` 那一带的注释。桌面端点「立即更新」
  // = 下载 → 校验 → 新 exe 覆盖当前 exe 并重启（进程随之中断，进度只在这一格与设置面板里说）。
  async function installUpdateNow() {
    try {
      await upd.installUpdate();
    } catch (e) {
      // 失败必须说出来：那条命令的报错讲的是真因（镜像都不通 / 校验不过 / helper 起不来）。
      toast(`更新失败：${e instanceof Error ? e.message : String(e)}`);
    }
  }

  // ── 推送（F-50 ~ F-54）────────────────────────────────────────────────
  /** 字节数 → 人看的短文本（卡片/通知里都够短）。 */
  function fmtBytes(n: number): string {
    if (n >= 1024 * 1024 * 1024) return `${(n / 1024 / 1024 / 1024).toFixed(2)} GB`;
    if (n >= 1024 * 1024) return `${(n / 1024 / 1024).toFixed(1)} MB`;
    if (n >= 1024) return `${(n / 1024).toFixed(0)} KB`;
    return `${n} B`;
  }

  function pushPercent(received: number, total: number): number {
    return total > 0 ? Math.min(100, Math.round((received / total) * 100)) : 0;
  }

  /** 这一批还有几件"在路上"（邀请挂着或正在传）—— 卡片能不能取消、要不要粘住都看它。 */
  function batchLive(b: PushBatch): boolean {
    return b.waiting > 0;
  }

  function batchAllDone(b: PushBatch): boolean {
    return b.waiting === 0 && b.failed === 0;
  }

  /**
   * 有台没成、且这一批不止一台 → 卡片**粘住**，点「知道了」才消失。
   *
   * 一条 toast 说不清"是 3 台里的哪一台失败了"，而单台推送那条路的收尾方式（toast +
   * 自动收掉）已经够用且是用户熟悉的 —— 所以只有多设备这一档需要留一张能逐行看的卡。
   */
  function batchSticky(b: PushBatch): boolean {
    return b.targets.length > 1 && !batchLive(b) && !batchAllDone(b);
  }

  /**
   * 发送卡的标题。
   *
   * 多台设备：`推送给「A」「B」等 3 台设备`（用户一眼看到"这次推给了谁、几台"），
   * 名字超过两个就省略，完整名单在悬停里（`batchTitleFull`）。
   *
   * 单台：沿用原来那套标题。多选时**只说项数不说名字**：那个"名字"是分组用的标签
   * （共同父目录名或「N 项」），而落盘时它并不参与路径（见 `manifest_for_paths`）——
   * 写成"正在发送 · 3 项（3 项）"是把同一件事说了两遍。
   */
  function pushBatchTitle(b: PushBatch): string {
    const live = batchLive(b);
    const sending = b.targets.some((t) => t.state === 'sending');
    if (b.targets.length > 1) {
      const names = b.targets.map((t) => `「${targetName(t)}」`);
      const head = names.length <= 2 ? names.join('') : `${names[0]}${names[1]}等 ${names.length} 台设备`;
      return `推送给${head}`;
    }
    if (b.count > 1) {
      const what = `（${b.count} 项）`;
      return !live ? `已发送${what}` : sending ? `正在发送${what}` : `等待对方同意${what}`;
    }
    if (!live) return `已发送 · ${b.name}`;
    return sending ? `正在发送 · ${b.name}` : '等待对方同意';
  }

  /**
   * 一批推送里一台目标的**显示名**（F-101）：快照里的 `target` 是**发起那一刻**对端自报的
   * 设备名，本机现在若给这台设备起了备注，就该按备注显示（`peerDisplayName` 找不到这台设备时
   * 才回落到快照里那个名字 —— 设备可能已经被移除了）。
   */
  function targetName(t: PushTarget): string {
    return peerDisplayName(t.peerId, t.target);
  }

  /** 悬停里的完整名单（标题只放得下两个名字）。 */
  function batchTitleFull(b: PushBatch): string {
    if (b.targets.length < 3) return '';
    return b.targets.map(targetName).join('、');
  }

  /**
   * 计数行 = 用户要的那两个数（还有失败时补一个）：成功 x · 等待 y · 失败 z。
   * 全成时换成一句结论 —— 那时候数字已经没有信息量了。
   */
  function batchCounts(b: PushBatch): string {
    if (batchAllDone(b)) {
      return b.targets.length > 1 ? `${b.targets.length} 台全部完成` : '已发送完成';
    }
    const failed = b.failed > 0 ? ` · 失败 ${b.failed}` : '';
    return `成功 ${b.done} · 等待 ${b.waiting}${failed}`;
  }

  /**
   * 一行目标设备的行内文案。
   *
   * 已配对的目标**不问同意**直接收（token 就是那份信任），所以它那一行不能说
   * 「等待对方同意」—— 对面根本没点过任何东西，说成"等同意"是在编一个没发生的事。
   */
  function targetLabel(t: PushTarget, total: number): string {
    switch (t.state) {
      case 'invited': return t.paired ? '等待开始（对方自动接收）' : '等待对方同意';
      case 'sending': return `传输中 ${pushPercent(t.received, total)}%`;
      case 'done': return `已发送 ${fmtBytes(t.received)}`;
      case 'cancelled': return '已取消';
      case 'rejected': return '对方拒绝';
      default: return `失败：${t.error ?? '未知原因'}`;
    }
  }

  /**
   * 每一台的字节总量。
   *
   * 整批的 `total` 是所有**还在算的台**之和（失败/取消的整条不计），而各台都是同一份清单
   * —— 除以"还在算的台数"就得到每一台的总量。分母必须用 `done + waiting`：
   * 已经完成的那几台仍在 `total` 里，只用 `waiting` 会把它们的量摊到剩下这几台上，
   * 于是某台传到一半却显示成 15%（实际 30%）。
   */
  function batchPerTarget(b: PushBatch): number {
    const live = b.done + b.waiting;
    return live > 0 ? Math.round(b.total / live) : 0;
  }

  /** 行首那颗点：沿用设备行那套视觉（绿=活的/成了，灰=在等，红=没成）。 */
  function targetDot(t: PushTarget): string {
    switch (t.state) {
      case 'done': return 'dot ok';
      case 'sending': return 'dot ok';
      case 'invited': return 'dot off';
      default: return 'dot danger';
    }
  }

  /** 一批里现在要看的那几行：>5 台时先列 4 行，其余在「还有 N 台」后面（点开就地展开）。 */
  function visibleTargets(b: PushBatch): PushTarget[] {
    if (b.targets.length <= 5 || expandedBatches[b.batchId]) return b.targets;
    return b.targets.slice(0, 4);
  }

  /** 这一条邀请属于哪一批（回执只知道邀请 id）。 */
  function batchForInvite(inviteId: string): PushBatch | undefined {
    return pushBatches.find((b) => b.targets.some((t) => t.inviteId === inviteId));
  }

  function dropBatch(batchId: string) {
    // 收尾标记跟着卡片一起清：它只在"卡片还挂着、终态快照可能连着来"的那段时间有用
    // （后端在这一批没有在路上之后就不再发事件了，所以清掉不会让谁重新收一次尾）。
    settledBatches.delete(batchId);
    pushBatches = pushBatches.filter((b) => b.batchId !== batchId);
  }

  /**
   * 一台目标都不在网上：这句话说一次。「＋」选文件那条路（开框之前先挡）与拖入会话浮层
   * 那条路（`sendDroppedToConvo`）共用它 —— 同一件事在两处各写一句，文案迟早会分叉。
   */
  function toastNoLiveTarget(devices: PeerDevice[]) {
    toast(devices.length === 1
      ? `「${deviceDisplayName(devices[0])}」不在网上，推送未发送`
      : '所选设备都不在网上，推送未发送');
  }

  /**
   * 推送（唯一入口：会话里那颗「＋」）：给这个会话的每台各开一条 —— **只开一次选择框**
   * （文件与文件夹混选、可跨目录累积），同一批文件并发给每台（各台一条邀请、各自同意、
   * 各自进度；清单只建一次）。1:1 会话就是单台推送。
   *
   * 以前这里还有一层"先问推文件还是文件夹"——那是原生框的能力限制逼出来的：Windows 的
   * `IFileOpenDialog` 只有 `FOS_PICKFOLDERS` 一个开关（非此即彼），rfd 也只给
   * `pick_file(s) / pick_folder(s)`，没有混合模式。选择框自绘之后这一层就不需要了，
   * 用户可以先把一个文件勾上、再进另一层目录把整个文件夹勾上，一次推过去。
   */
  function requestPushMany(devices: PeerDevice[], chatBatchId = '', groupId = '') {
    const live = devices.filter((d) => d.online);
    if (live.length === 0) {
      toastNoLiveTarget(devices);
      return;
    }
    picker = {
      title: `推送到${pushTargetsLabel(live)}`,
      sub: '文件与文件夹都可以选，双击文件夹进入，选中项跨目录累积；已选项目在底部角标里可查看、逐项取消或全部取消。同一份内容会分别发给每台设备：已配对的直接收，未绑定的各自点同意，进度与成败都记在右下角那张卡片上。',
      confirmLabel: live.length > 1 ? `推送到 ${live.length} 台设备` : '推送',
      onConfirm: (items) => void pushPicked(items, live, chatBatchId, groupId),
    };
  }

  /** 「A」「B」等 3 台设备 —— 与卡片标题同一个规则（设备名一律用「」框住）。 */
  function pushTargetsLabel(devices: PeerDevice[]): string {
    const names = devices.map((d) => `「${deviceDisplayName(d)}」`);
    if (names.length === 1) return names[0];
    if (names.length === 2) return names.join('');
    return `${names[0]}${names[1]}等 ${names.length} 台设备`;
  }

  function closePicker() {
    picker = null;
  }

  /** 推送确认：关框 → 把这批路径交给后端去送（取消时这里根本不会调到）。 */
  async function pushPicked(items: LocalPick[], devices: PeerDevice[], chatBatchId: string, groupId = '') {
    closePicker();
    if (items.length === 0) return;
    try {
      // 同一份清单发给每台：多选合并成**每台一条**推送（对端一张卡、一次同意、一条进度）。
      const batch = await pushPaths(items.map((i) => i.path), devices.map((d) => d.id));
      // 会话里也留一条（文件消息就是这次推送的状态行）—— 推了哪些项、走到哪一步，
      // 落点只有一个：会话。`chatBatchId` 非空时它归到那条「群发」的记录里，`groupId`
      // 非空时归到群那条视图里（两者互斥）。
      chat.noteOutgoingFiles(devices.map((d) => d.id), items, batch, chatBatchId, groupId);
    } catch (e) {
      toast(String(e));    // 全都不可达等：就地失败，不排队（F-53）
    }
  }

  /** 本机页点「拖入文件或文件夹，点击添加共享项」：开同一个选择框挑本机内容（F-20）。 */
  function requestAddShare() {
    picker = {
      title: '添加共享项',
      sub: '文件与文件夹都可以选，双击文件夹进入，选中项跨目录累积；已选项目在底部角标里可查看、逐项取消或全部取消。登记后可在本机标签里对每项开关广播。',
      confirmLabel: '添加共享项',
      onConfirm: (items) => {
        closePicker();
        void addShares(items.map((i) => i.path));
      },
    };
  }

  /**
   * 一批本机路径登记成共享项：**拖入悬浮球与「添加共享项」选择框共用这一个落点**
   * —— 两条路各写一套的话，失败处理、列表刷新、提示文案迟早会分叉。
   *
   * **同一条路径重复登记不算"添加"**（F-21）：后端按路径去重，回来的 id 要是本来就有的，
   * 就是"已经在共享项里"那一类。同一批里的重复（选择框里同时挑中 `E:\a\b` 与 `E:\a\b\`）也算
   * —— 所以批内见过的 id 也要记住。
   *
   * 返回真正登记上的条数：坏路径（不存在、已被删）吞在这一层并进控制台，
   * 一条坏的没道理让整批一起消失。
   */
  async function addShares(paths: string[]): Promise<number> {
    const known = new Set(getShares().map((s) => s.id));
    const seen = new Set<string>();
    // 失败原因留一份：一条坏的没道理让整批一起消失（下面照旧只算成功的），但**一条都没成**时
    // 必须说清为什么 —— 比如"镜像目录不能添加为共享项"（`unsendable::is_mirror_path`）。
    // 只说"没有添加任何共享项"等于让用户对着一次沉默的拒绝发呆。
    const failures: string[] = [];
    const results = await Promise.all(
      paths.map(async (p) => {
        try {
          const item = await addShare(p);
          if (known.has(item.id) || seen.has(item.id)) return 'dup' as const;
          seen.add(item.id);
          return 'added' as const;
        } catch (e) {
          console.error('添加共享项失败:', p, e);
          failures.push(e instanceof Error ? e.message : String(e));
          return 'failed' as const;
        }
      }),
    );
    const added = results.filter((r) => r === 'added').length;
    const skipped = results.filter((r) => r === 'dup').length;
    if (added > 0 && skipped > 0) toast(`已添加 ${added} 个共享项（${skipped} 项已在共享项里）`);
    else if (added > 0) toast(`已添加 ${added} 个共享项`);
    else if (skipped > 0) toast(skipped === 1 ? '这项已经在共享项里了' : `这 ${skipped} 项已经在共享项里了`);
    else if (failures.length > 0)
      toast(failures.length === 1 ? failures[0] : `${failures.length} 项没能添加：${failures[0]}`);
    else toast('没有添加任何共享项');
    return added;
  }

  async function cancelSending(inviteId: string) {
    try { await cancelPush(inviteId); } catch (e) { toast(String(e)); }
  }

  /** 取消整批（卡片上的「取消全部」）：各台各撤一条，任何一台撤不动都不影响其余几台。 */
  async function cancelBatch(batchId: string) {
    try { await cancelPushBatch(batchId); } catch (e) { toast(String(e)); }
  }

  // ── 会话（聊天）：面板底栏那颗入口、会话里的文件发送、会话菜单 ────────────
  // 会话记录在后端（`stores/chat` 拿着那份快照），这里只做"面板级"的事：开合浮层、
  // 开文件选择框、画右键菜单、把右下角那条推送通知同步进会话。

  /** 底栏那颗「会话」：开合浮层。展开时拉一次快照 —— 收起期间收到的消息得补上。 */
  function toggleChat() {
    const next = !chat.isChatOpen();
    chat.setChatOpen(next);
    if (next) {
      void chat.refresh();
    } else {
      chat.closeRecipients();
    }
  }

  /**
   * 设备页的「发消息」（行上的按钮与右键菜单同一条路）：开一台设备的会话 ——
   * 设备不在了就说清楚，别开出一个空会话。
   */
  function openDeviceChat(peerId: string) {
    if (!getDevices().some((d) => d.id === peerId)) {
      toast('该设备已被移除');
      return;
    }
    chat.openDeviceChat(peerId);
  }

  /**
   * 这条会话该发给谁、那批记录归到哪里 —— 「＋」选文件与拖入浮层（F-111）两条路共用这一份。
   *
   * `chatBatchId` = 群发那份名单的 id（这批文件记进那份群发的记录），`groupId` = 群聊的 id
   * （记进群那条视图），两者互斥，一对一都为空。名册成员要还在本机设备表里才推得动
   * （推送是按设备寻址的），不在的那些在这里就掉出去。
   */
  function convoSendTargets(t: ConvoThread) {
    return {
      peers: getDevices().filter((d) => t.participants.includes(d.id)),
      chatBatchId: t.mode === 'broadcast' ? t.id : '',
      groupId: t.mode === 'group' ? t.id : '',
    };
  }

  /**
   * 会话里「＋」：选文件就是推送（`requestPushMany` 那条路，一次选、分头发）。
   *
   * 目标是**当前开着的那条会话**的参与者 —— 一对一发给这一台；群发那条会话发给名单上的
   * 每台，并把这批文件记到那份群发的记录里（`chatBatchId`）；群聊也是同一招：**拿群成员当
   * 收件人跑一次多目标推送**（群里发文件没有单独的后端命令），本机这条记录挂上 `groupId`
   * 就落在群那条视图里。谁在名单里、记录挂哪儿，见上面那份 `convoSendTargets`。
   */
  function chatSendFiles() {
    const t = chat.activeThread();
    if (!t) return;
    const { peers, chatBatchId, groupId } = convoSendTargets(t);
    requestPushMany(peers, chatBatchId, groupId);
  }

  /**
   * OS 拖放的落点（窗口内的**物理像素**坐标，Tauri 转来的）是不是落在会话浮层上。
   *
   * 两条都要成立：会话浮层开着、**且已经进了某一条会话**。列表那一屏没有"发给谁"可言
   * （一条都没选），落上去照旧登记成共享项 —— 那正是这里返回 false 的情形。
   *
   * 坐标得除以 `devicePixelRatio`：`position` 是物理像素（Windows 的 OLE 落点，
   * wry 在 `DragOver`/`Drop` 里做的 `ScreenToClient`），而 `getBoundingClientRect()` 是 CSS
   * 像素。不除这一下，缩放不是 100% 的机器上落点判定整体偏移（150% 时偏出去一半）。
   */
  function dropInConvo(pos: { x: number; y: number }): boolean {
    if (!chat.isChatOpen() || !chat.activeThread()) return false;
    // 会话浮层的根节点（`ConvoPanel` 上那个 `data-od-id`）。关着时它 `inert` 且
    // `pointer-events: none`，但上面两条已经挡掉了那种情形。
    const el = document.querySelector<HTMLElement>('[data-od-id="convo-sheet"]');
    if (!el) return false;
    const r = el.getBoundingClientRect();
    const dpr = window.devicePixelRatio || 1;
    const x = pos.x / dpr;
    const y = pos.y / dpr;
    return x >= r.left && x <= r.right && y >= r.top && y <= r.bottom;
  }

  /**
   * 拖进会话浮层的那批文件：**当场按这条会话推过去**（F-111），与「＋」同一套落点 ——
   * 同一份收件人、同一处会话记录、同一批推荐逻辑，只是清单来自拖放而不是选择框。
   * **不再弹一次选择框**：用户用拖放表达完了"发什么"，再问一遍是白费一遍手。
   *
   * 名字 / 大小 / 是否目录得问后端（拖放只给路径，`pathPicks`）；能不能推（链接、系统目录、
   * 自家数据目录、镜像）不在前端判 —— `push_paths` 那条路会报出原话，判据只有那一份。
   */
  async function sendDroppedToConvo(paths: string[]) {
    const t = chat.activeThread();
    if (!t) return;
    const { peers, chatBatchId, groupId } = convoSendTargets(t);
    const live = peers.filter((d) => d.online);
    if (live.length === 0) {
      toastNoLiveTarget(peers);
      return;
    }
    try {
      const items = await pathPicks(paths);
      const batch = await pushPaths(paths, live.map((d) => d.id));
      chat.noteOutgoingFiles(live.map((d) => d.id), items, batch, chatBatchId, groupId);
    } catch (e) {
      toast(String(e));    // 清单建不起来（链接 / 数据目录 / 路径没了）：就地失败，不登记共享项
    }
  }

  /** 会话菜单的「查看该设备文件」：切到那台设备的浏览标签（标签只给长连接设备开）。 */
  function chatOpenDeviceFiles(peerId: string) {
    activeTab = { kind: 'peer', peerId };
  }

  function openMessageCtx(x: number, y: number, id: string, full: string, sel: string) {
    ctxState = { x, y, kind: 'chat', chatId: id, chatFull: full, chatSel: sel };
  }

  /**
   * 会话菜单（详情右上那颗「⋮」）：锚点 + **是哪个会话**交出去，菜单由 `ContextMenu`
   * 按模式分（一对一：查看该设备文件 / 清空聊天记录；群发与群聊：编辑那份名单 / 删掉它）。
   */
  function openConvoCtx(x: number, y: number, key: string) {
    const t = chat.activeThread();
    if (!t || t.key !== key) return;
    // 「查看该设备文件」要有对方的**浏览标签**才成立（那个标签只给长连接设备开）：
    // 没长连接时挂上去只是个点了没反应的项。
    const browse = getDevices().find((d) => d.id === t.participants[0])?.connected === true;
    ctxState = {
      x,
      y,
      kind: 'convo',
      convoKey: t.key,
      convoPeerId: t.mode === 'direct' ? t.participants[0] : undefined,
      convoThread: t.mode !== 'direct',
      convoBc: t.mode === 'broadcast',
      convoBrowse: t.mode === 'direct' && browse,
    };
  }

  /** 消息菜单：复制整条 / 只复制选中的那段（与别处复制同一套：剪贴板 API + 失败提示）。 */
  function ctxChatCopy(which: 'all' | 'sel') {
    const c = ctxState;
    hideCtx();
    const text = (which === 'sel' ? c?.chatSel : c?.chatFull) ?? '';
    if (!text) return;
    navigator.clipboard.writeText(text).then(
      () => toast(which === 'sel' ? '已复制所选文字' : '已复制整条消息'),
      () => toast('复制失败，请手动选中文字后复制'),
    );
  }

  /** 设备菜单的「发消息」：把 ctxState 里那台设备的会话开出来。 */
  function ctxMessage() {
    const id = ctxState?.deviceId;
    hideCtx();
    if (id) openDeviceChat(id);
  }

  /**
   * 设备菜单的「修改备注」（F-101）：开对话框，预填当前备注。
   *
   * **不看在线状态**（与设备行状态分档无关）：备注只写本机配置，离线/未绑定的设备也能改。
   * 设备已经不在列表里（右键后列表刚刷新过）就说清楚，别开出一个改不到任何东西的框。
   */
  function ctxRemark() {
    const id = ctxState?.deviceId;
    hideCtx();
    if (!id) return;
    const device = getDevices().find((d) => d.id === id);
    if (!device) {
      toast('该设备已被移除');
      return;
    }
    const saved = device.remark ?? '';
    remarkState = { peerId: id, rawName: device.name, value: saved, saved, busy: false, error: null };
  }

  /** 备注输入框一挂上就聚焦并**全选**：这一栏多半是来改的，全选省一次手动清空。 */
  function focusAndSelect(node: HTMLInputElement) {
    node.focus();
    node.select();
  }

  function closeRemark() {
    remarkState = null;
  }

  /** 保存（空串即清除）：提示里说的是**这台设备**的备注改成了什么，用户得看得出改的是哪台。 */
  async function saveRemark() {
    const rs = remarkState;
    if (!rs?.peerId || rs.busy) return;
    remarkState = { ...rs, busy: true, error: null };
    try {
      const stored = await saveDeviceRemark(rs.peerId, rs.value);
      closeRemark();
      toast(stored ? `已把「${rs.rawName}」的备注设为「${stored}」` : `已清除「${rs.rawName}」的备注`);
    } catch (e) {
      remarkState = { ...rs, busy: false, error: String(e) };
    }
  }

  /** 「清除备注」：只清这一条，与"把输入框留空再保存"是同一条路（后端不认识空串以外的清除）。 */
  async function clearRemark() {
    const rs = remarkState;
    if (!rs?.peerId || rs.busy) return;
    // 先把输入框置空再走保存：清空动作与"手动删光再点保存"必须落到**同一段代码**上，
    // 否则两条路的失败处理、提示文案迟早分叉（这里不要自己点 `busy` —— `saveRemark`
    // 进门就查它，点上等于当场把自己挡回去）。
    remarkState = { ...rs, value: '' };
    await saveRemark();
  }

  /** 会话菜单的「查看该设备文件」：切到那台设备的浏览标签。 */
  function ctxConvoFiles() {
    const id = ctxState?.convoPeerId;
    hideCtx();
    if (id) chatOpenDeviceFiles(id);
  }

  /** 会话菜单的「编辑收件人与群发名称 / 编辑参与者与群聊名称」：打开收件人选择框（模式锁定）。 */
  function ctxConvoEdit() {
    const key = ctxState?.convoKey;
    hideCtx();
    if (key) chat.askParticipants(key);
  }

  /**
   * 群发菜单的「升级为群聊」：拿这份名单的成员建一个**共享群**（之后任何成员发的消息，
   * 所有成员都看得到 —— 群发做不到这件事）。
   *
   * **历史不迁**：群发那些消息在各端都只存在于 1:1 会话里，对端也从没见过"它们属于一个群"，
   * 搬过来只会造出一份只有本机认的历史。新群从空开始（菜单项自己的 `title` 里写了这句）。
   */
  async function ctxConvoUpgrade() {
    const key = ctxState?.convoKey;
    hideCtx();
    if (!key) return;
    try {
      const id = await chat.upgradeBatch(key);
      if (id) toast('已升级为群聊：邀请已发出去，对方点「加入」后才算进群');
    } catch (e) {
      toast(String(e));
    }
  }

  /**
   * 会话菜单最后那一项：
   * - 一对一 = 「清空聊天记录」：只清这一台那条记录，别的设备那份不受影响；
   * - 群发 = 「删除群发」：删的是这份名单，**消息一条不动**（它们本来就在各台会话里）；
   * - 群聊 = 「退出群聊」：本机从名册里退掉（后端收掉本机与这个群的关系），其余成员不受影响。
   */
  async function ctxConvoExit() {
    const c = ctxState;
    hideCtx();
    if (!c?.convoKey) return;
    if (!c.convoThread) {
      const peerId = c.convoPeerId;
      if (!peerId) return;
      const title = chat.deviceNameOf(peerId);
      try {
        await chat.clearPeer(peerId);
      } catch (e) {
        toast(String(e));
        return;
      }
      chat.backToList();
      toast(`已清空与「${title}」的聊天记录`);
      return;
    }
    try {
      const gone = await chat.exitThread(c.convoKey);
      if (!gone) return;
      toast(gone.mode === 'broadcast'
        ? `已删除「${gone.title}」（消息仍留在各台设备的会话里）`
        : `已退出「${gone.title}」`);
    } catch (e) {
      toast(String(e));
    }
  }

  /**
   * 会话里那张卡上的「接收 / 拒绝」：右下角那条通知还在就交给同一条处理路径
   * （两条路各写一套的话，"接收中"的进度与成败会各说各的），通知已经被处理过就只调后端。
   */
  async function chatAcceptInvite(inviteId: string) {
    const notice = notices.find((n) => n.inviteId === inviteId);
    if (notice) {
      await acceptNotice(notice);
      return;
    }
    // 通知不在了（用户手动清过）也走同一套：该问目录就问，取消就拒绝。
    let ok = false;
    try {
      ok = await acceptInviteWithAsk(inviteId);
    } catch (e) {
      toast(String(e));
      chat.noteInviteDone(inviteId, 'failed');
      return;
    }
    if (!ok) {
      await chatRejectInvite(inviteId);
      return;
    }
    chat.noteInviteAccepted(inviteId);
  }

  async function chatRejectInvite(inviteId: string) {
    const notice = notices.find((n) => n.inviteId === inviteId);
    if (notice) {
      await rejectNotice(notice);
      return;
    }
    try {
      await rejectPush(inviteId);
    } catch (e) {
      toast(String(e));
    }
    chat.noteInviteDone(inviteId, 'rejected');
  }

  function dropNotice(inviteId: string) {
    notices = notices.filter((n) => n.inviteId !== inviteId);
  }

  /** 气泡上的「知道了」：只撤这一条 —— 悬浮球与铃铛上的数字跟着减。 */
  function confirmNotice(inviteId: string) {
    dropNotice(inviteId);
  }

  /**
   * 气泡上的「打开目录」：把落盘位置交给系统（目录 = 文件管理器里打开），随后也撤掉这一条 ——
   * 按它和按「知道了」一样是"这条我处理完了"，不会再有一条气泡挂着同一个目录。
   *
   * 开不起来（目录已被移走/删掉）就**留住**这条并说明原因：这条通知存在的意义就是
   * "东西落在哪"，不能在什么都没打开的情况下把它抹掉。
   */
  async function openNoticeDir(n: PushNotice) {
    const dest = n.dest;
    if (!dest) return;
    try {
      await openPath(dest);
      dropNotice(n.inviteId);
    } catch (e) {
      toast(`打开目录失败：${e}`);
    }
  }

  /** 通知标题（与发送卡同一套规则：多项只说项数，不说那个分组标签）。 */
  function noticeTitle(n: PushNotice): string {
    // 配对邀请与推送无关：既没有文件也没有项数，只说"谁在等你的决定"。
    if (n.kind === 'pair') return `「${n.from}」请求与你配对`;
    // 本机发出去的那条：说清"在等谁" —— 卡片挂着就代表对面还没处理。
    if (n.kind === 'pairWait') return `等待「${n.from}」确认配对`;
    if (n.count > 1) {
      const what = `（${n.count} 项）`;
      if (n.kind === 'invite') return `收到「${n.from}」的推送${what}`;
      if (n.kind === 'done') return `已收到「${n.from}」的推送${what}`;
      return n.kind === 'receiving' ? `正在接收${what}` : `接收失败${what}`;
    }
    if (n.kind === 'invite') return `收到「${n.from}」的推送${n.isDir ? '目录' : '文件'}`;
    if (n.kind === 'done') return `已收到「${n.from}」的推送${n.isDir ? '目录' : '文件'}`;
    return n.kind === 'receiving' ? `正在接收 · ${n.name}` : `接收失败 · ${n.name}`;
  }

  /** 气泡正文：收完的说清"东西在哪"（`dest` 由后端给），失败的说清原因。 */
  function noticeBody(n: PushNotice): string {
    // 配对邀请：说清"点下去会发生什么" —— 接受即交换凭据、互为「已配对」。
    if (n.kind === 'pair') {
      // 没带凭据的那种（后端 `unproven`）要当场说清：用户是在"它可能真是那台老设备、
      // 也可能不是"的前提下按「接收」的。
      const warn = n.unproven
        ? '注意：它自称是本机已配对的设备，却没带上本机发给它的凭据（它重新装过 / 解除过配对？）。接收即按它现在这个地址重新配对。'
        : '';
      return `${warn}接受后两台设备互为「已配对」（互相交换凭据），之后就能建立长连接、互相推送；60 秒内未处理自动作废。`;
    }
    if (n.kind === 'pairWait') {
      return `已把邀请推到「${n.from}」：它在自己的通知里点「接收」即配对成功。60 秒内没人处理自动作废。`;
    }
    if (n.kind === 'done') {
      const what = n.count > 1 ? `${n.count} 项` : n.name;
      return n.dest ? `${what} 已保存到 ${n.dest}` : `${what} 已保存到下载目录`;
    }
    return n.error ?? '接收失败';
  }

  /**
   * 接收一条**需要同意**的推送（未绑定设备那条）：设置了「每次询问」就先问落盘目录。
   *
   * - 目录选择框打开即设置里的下载目录（`defaultPath`），省掉一层层点；
   * - 框里取消 = **拒绝**这次接收（返回 false）—— 发送方那边正挂着等回执，不能让它干等到
   *   60 秒超时；调用方接着走 `rejectNotice` / `chatRejectInvite` 把话带回去；
   * - 「直接下载」那一档照旧不问，`root` 不带 → 后端落在设置里的下载目录。
   *
   * **已配对设备那条路根本不会走到这里**：它免同意自动收，界面上连「接收」按钮都没有。
   */
  async function acceptInviteWithAsk(inviteId: string): Promise<boolean> {
    const s = getSettings();
    let root: string | null = null;
    if (s?.downloadMode === 'AskEachTime') {
      const picked = await openDialog({
        title: '选择接收位置',
        directory: true,
        multiple: false,
        defaultPath: s.downloadDir || undefined,
      });
      const dir = Array.isArray(picked) ? picked[0] : picked;
      if (!dir) return false;
      root = dir;
    }
    await acceptPush(inviteId, root ?? undefined);
    return true;
  }

  async function acceptNotice(n: PushNotice) {
    let ok = false;
    try {
      ok = await acceptInviteWithAsk(n.inviteId);
    } catch (e) {
      toast(String(e));
      dropNotice(n.inviteId);
      return;
    }
    if (!ok) {
      await rejectNotice(n);
      return;
    }
    notices = notices.map((x) => (x.inviteId === n.inviteId ? { ...x, kind: 'receiving' } : x));
    chat.noteInviteAccepted(n.inviteId);
  }

  async function rejectNotice(n: PushNotice) {
    dropNotice(n.inviteId);
    chat.noteInviteDone(n.inviteId, 'rejected');
    try { await rejectPush(n.inviteId); } catch (e) { toast(String(e)); }
  }

  async function cancelNotice(n: PushNotice) {
    dropNotice(n.inviteId);
    try { await cancelReceive(n.inviteId); } catch (e) { toast(String(e)); }
  }

  /**
   * 接受一条配对邀请：凭据在后端交换（本机铸一枚 token 放在那条挂着 HTTP 请求的响应里
   * 交给对方，同时把对方那枚存下来）——回到这里的已经是"配对完成"。
   */
  async function acceptPairNotice(n: PushNotice) {
    dropNotice(n.inviteId);
    try {
      const name = await acceptPairInvite(n.inviteId);
      toast(`已与「${name}」配对成功（点「连接」建立长连接）`);
    } catch (e) {
      toast(`配对未完成：${e}`);
    }
  }

  /** 拒绝一条配对邀请：对端会收到明确的"对方拒绝"，本机什么都不写。 */
  async function rejectPairNotice(n: PushNotice) {
    dropNotice(n.inviteId);
    try { await rejectPairInvite(n.inviteId); } catch (e) { toast(String(e)); }
  }

  /**
   * 取消本机发出去、还挂着的配对邀请：撤回对端界面上那条通知，本机这张卡收掉。
   * 撤回失败（对端已经处理/不在网上）不是错误 —— 它那条邀请的 TTL 会兜底。
   */
  async function cancelPairWaitNotice(n: PushNotice) {
    dropNotice(n.inviteId);
    try { await cancelPairInvite(n.inviteId); } catch (e) { toast(String(e)); }
    toast('已取消配对邀请');
  }

  /** 配对码有效期（秒），与后端 PairStore::CODE_TTL 一致。 */
  const PAIR_TTL = 120;

  function stopPairTimer() {
    if (pairTimer) { clearInterval(pairTimer); pairTimer = null; }
  }

  /**
   * 生成本机配对码：对方把屏幕上这串数字输入到它自己的 OrbDrop 里，由它向本机
   * POST /pair 换取 token。
   *
   * 刻意不自动轮换：曾按「过期自动刷新」每 2 分钟换码，对方正照着屏幕输入时被换掉，
   * 就会报「配对码错误或已过期」。现在只倒计时，过期后由用户点「刷新配对码」。
   */
  async function issuePairCode(peerId: string, peerName: string) {
    let code: string;
    try {
      code = await pairStart(peerId);
    } catch (e) {
      toast(`生成配对码失败：${e}`);
      return;
    }
    pairState = { peerId, peerName, code, remaining: PAIR_TTL, input: '', busy: false, error: null, ip: '', port: '' };
    startPairCountdown(peerId);
  }

  function startPairCountdown(peerId: string) {
    stopPairTimer();
    pairTimer = setInterval(() => {
      const ps = pairState;
      if (!ps || ps.peerId !== peerId) { stopPairTimer(); return; }
      if (ps.remaining > 1) { pairState = { ...ps, remaining: ps.remaining - 1 }; return; }
      stopPairTimer();
      pairState = { ...ps, remaining: 0 };
    }, 1000);
  }

  /** 旧码过期后手动换新码（后端同一时间只保留未过期的码）。 */
  async function refreshPairCode() {
    const ps = pairState;
    if (!ps?.peerId) return;
    pairState = { ...ps, busy: true, error: null };
    try {
      const code = await pairStart(ps.peerId);
      pairState = { ...ps, code, remaining: PAIR_TTL, busy: false };
      startPairCountdown(ps.peerId);
      toast('已换新配对码');
    } catch (e) {
      if (pairState) pairState = { ...pairState, busy: false, error: String(e) };
    }
  }

  function openPair(device: PeerDevice | null) {
    if (device) {
      // 发起配对 = 发起与这一台的连接：它此刻"看得见却直连不通"时，那句话在这里说
      // （配对码由本机持有、对方拨进来，拨不动这张网就是白等）。设备行本身不带这个状态。
      const tip = unicastBlockedTip(device);
      if (tip) toast(tip);
      void issuePairCode(device.id, deviceDisplayName(device));
      return;
    }
    // 还没选中设备：先手动添加（F-11），探测到身份后直接进入配对
    stopPairTimer();
    pairState = {
      peerId: null, peerName: null, code: '', remaining: 0, input: '', busy: false, error: null,
      ip: '', port: serverPortVal ? String(serverPortVal) : '',
    };
  }

  let pairConfirming = false;  // 本机正在用对方的码配对：收尾由 confirmPair 自己做

  function closePair() { stopPairTimer(); pairState = null; }

  /** 手动添加设备：探测对方 /pair/info 拿身份，随后进入配对。 */
  async function addAndPair() {
    const ps = pairState;
    if (!ps) return;
    let ip = ps.ip.trim();
    let port = ps.port.trim();
    const colon = ip.lastIndexOf(':');
    if (colon > 0) { port = ip.slice(colon + 1); ip = ip.slice(0, colon); }
    const portNum = Number(port);
    if (!ip || !Number.isInteger(portNum) || portNum < 1 || portNum > 65535) {
      pairState = { ...ps, error: '请填写对方 IP 与端口（1-65535）' };
      return;
    }
    pairState = { ...ps, busy: true, error: null };
    try {
      const device = await addPeer(ip, portNum);
      await loadDevices();
      toast(`已添加设备「${peerDisplayName(device.id, device.name)}」`);
      await issuePairCode(device.id, peerDisplayName(device.id, device.name));
    } catch (e) {
      if (pairState) pairState = { ...pairState, busy: false, error: String(e) };
    }
  }

  /** 输入对方设备上显示的配对码完成配对：由对方验证本机、交换 token。 */
  async function confirmPair() {
    const ps = pairState;
    if (!ps?.peerId) return;
    if (!/^\d{6}$/.test(ps.input)) {
      pairState = { ...ps, error: '请输入 6 位数字配对码' };
      return;
    }
    pairState = { ...ps, busy: true, error: null };
    pairConfirming = true;
    try {
      await pairConfirm(ps.peerId, ps.input);
      await loadDevices();
      closePair();
      toast(`已与「${ps.peerName ?? '设备'}」配对成功（点「连接」建立长连接）`);
    } catch (e) {
      if (pairState) pairState = { ...pairState, busy: false, error: String(e) };
      // 拨过去就没通的：那一台此刻"看得见却直连不通"时，这句解释比"请核对配对码"有用
      // （码很可能是对的，是这张网不让两个客户端互相说话）。
      const tip = unicastBlockedTip(getDevices().find((d) => d.id === ps.peerId));
      toast(tip ?? '配对失败，请核对配对码');
    } finally {
      pairConfirming = false;
    }
  }

  /**
   * 发起配对邀请（F-12 免配对码）：本机点一下按钮，对端在**它自己的通知里**点「接收」
   * 即可配对成功 —— 谁都不用读屏、也不用输码。
   *
   * 邀请是后端的一次 HTTP POST（`/pair/invite`），对端在等它自己的用户做决定（最长 60s）。
   * **本机这边不等在弹窗里**：弹窗当场收起，那条邀请变成右下角通知组里的一张卡
   * （与"等待对方同意"的推送卡同一个位置、同一套收尾），对端接受/拒绝/超时都落到它身上。
   */
  async function invitePair() {
    const ps = pairState;
    if (!ps?.peerId) return;
    pairState = { ...ps, busy: true, error: null };
    // 同一台设备再发一次：先把上一条还挂着的邀请撤回来，别让对端界面上堆两条一样的。
    for (const old of notices.filter((n) => n.kind === 'pairWait' && n.peerId === ps.peerId)) {
      dropNotice(old.inviteId);
      void cancelPairInvite(old.inviteId).catch(() => {});
    }
    let inviteId: string;
    try {
      inviteId = await pairInvite(ps.peerId);
    } catch (e) {
      if (pairState) pairState = { ...pairState, busy: false, error: String(e) };
      // 邀请就是一次到对端的 POST：投不出去时，这一台是不是"看得见却直连不通"要说清楚。
      const tip = unicastBlockedTip(getDevices().find((d) => d.id === ps.peerId));
      if (tip) toast(tip);
      return;
    }
    const name = ps.peerName ?? '设备';
    closePair();
    notices = [...notices, {
      inviteId,
      kind: 'pairWait',
      peerId: ps.peerId,
      from: name,
      name: '', size: 0, isDir: false, count: 1, received: 0,
    }];
    toast(`已向「${name}」发起配对邀请：对方在通知里点「接收」即配对成功`);
  }

  function copyCode() {
    const ps = pairState;
    if (!ps?.code) return;
    navigator.clipboard.writeText(ps.code).then(
      () => toast(`已复制配对码 ${ps.code}`),
      () => toast('复制失败，请手动输入')
    );
  }

  function openSettings() { showSettings = true; }
  /**
   * 设置面板保存/取消后都要重探一次 BC：菜单里的 BC 区跟着 `bc_status.exePath` 走，
   * 用户刚在设置里填了 BC 路径（或清空），右键菜单必须立刻反映，不能等下次启动。
   */
  function closeSettings() {
    showSettings = false;
    void loadBcStatus();
  }

  function hideCtx() { ctxState = null; }

  /** 本机共享项在磁盘上的真实位置（绝对路径 + 是不是目录）。
 *
 *  树节点传的是**共享项内**的相对路径（用 `/` 分隔，见 `TreeNode` 的 `ownPath`），共享项根
 *  那一档没有 `path`；把两者拼起来才是磁盘上的那个东西。拼法与后端一致（`/file` 也是
 *  `share_root.join(rel)`）。 */
  function localShareItem(c: CtxState): { abs: string; isDir: boolean; share: ShareItem } | null {
    if (c.kind !== 'share' || !c.shareId) return null;
    const share = getShares().find((s) => s.id === c.shareId);
    if (!share) return null;
    const rel = (c.path ?? '').replace(/\\/g, '/');
    const root = share.path.replace(/[\\/]+$/, '');
    return {
      abs: rel ? `${root}/${rel}` : share.path,
      // 根那一档看共享项自己的元数据，树节点看它自己的。
      isDir: rel ? c.isDir === true : share.isDir,
      share,
    };
  }

  /** 一个路径的所在文件夹（两种分隔符都要认）。 */
  function parentDir(path: string): string {
    const cut = Math.max(path.lastIndexOf('/'), path.lastIndexOf('\\'));
    return cut > 0 ? path.slice(0, cut) : path;
  }

  /** 「下载到本地」（本机共享项）：真的复制进下载目录，落盘名与位置都由后端给。
   *
   *  **目录项不复制**：本机目录已经在磁盘上，复制一份整棵树是另一个语义（而且 S-02 的
   *  "选位置"是文件保存框，对目录无从下手）。这里如实说明并把用户引到「打开所在文件夹」，
   *  而不是像从前那样报一句"已开始下载"却什么都不做。 */
  async function ctxDownload() {
    const c = ctxState;
    hideCtx();
    if (!c || c.kind !== 'share' || !c.shareId) return;
    const item = localShareItem(c);
    if (item?.isDir) {
      toast('本机目录不复制到下载目录（它已经在磁盘上）——用「打开所在文件夹」查看');
      return;
    }
    try {
      const saved = await downloadFile(c.shareId, c.path ?? '');
      // 保存框里取消 = 不落盘：静默收场（不是失败，所以不弹提示）。
      if (!saved) return;
      toast(`已下载到 ${saved}`);
    } catch (e) {
      toast(`下载失败：${e}`);
    }
  }

  /** 「打开所在文件夹」：目录直接打开它自己，文件打开它所在的文件夹。
   *  走系统的默认打开方式（`open_path`），失败时报错而不是假装成功。 */
  async function ctxOpen() {
    const c = ctxState;
    hideCtx();
    const item = c ? localShareItem(c) : null;
    if (!item) return;
    const target = item.isDir ? item.abs : parentDir(item.abs);
    try {
      await openPath(target);
    } catch (e) {
      toast(`打开失败：${e}`);
    }
  }

  /** 对端文件下载：走后端命令（token 取凭据库、落盘按下载目录设置都在 Rust 侧）。 */
  async function ctxPeerDownload() {
    const c = ctxState;
    hideCtx();
    if (c?.kind !== 'peer' || !c.peerId || !c.shareId) return;
    try {
      const saved = await downloadFromPeer(c.peerId, c.shareId, c.path ?? '', c.name);
      // 保存框里取消 = 不落盘（与「本机共享项下载」同一个口径）。
      if (!saved) return;
      toast(`已下载到 ${saved}`);
    } catch (e) {
      toast(`下载失败：${e}`);
    }
  }

  /** 「复制共享路径」：真的写剪贴板（从前只弹一句"已复制"，粘出来还是旧内容）。
   *  复制的是这项在磁盘上的**绝对路径** —— 拿它喂脚本/工具链才有用。 */
  async function ctxCopy() {
    const c = ctxState;
    hideCtx();
    const item = c ? localShareItem(c) : null;
    if (!item) return;
    try {
      await navigator.clipboard.writeText(item.abs);
      toast('已复制共享路径');
    } catch (e) {
      toast(`复制失败：${e}`);
    }
  }

  async function ctxRemove() {
    const c = ctxState;
    if (c?.kind === 'share' && c.shareId) {
      await removeShare(c.shareId);
      toast('已移除共享项');
    }
    hideCtx();
  }

  async function ctxConnect() {
    const deviceId = ctxState?.deviceId;
    hideCtx();
    if (!deviceId) return;
    try {
      await connectPeer(deviceId);
      // 与行内那颗「长连接」同一句话（`DeviceTab.setLink`）：拨之前先看这一台此刻通不通。
      const tip = unicastBlockedTip(getDevices().find((d) => d.id === deviceId));
      toast(tip ?? '正在建立长连接，拖入文件将自动广播');
    } catch (e) {
      toast(`建立长连接失败：${e}`);
    }
  }

  async function ctxDisconnect() {
    const deviceId = ctxState?.deviceId;
    hideCtx();
    if (!deviceId) return;
    try {
      await disconnectPeer(deviceId);
      toast('已断开长连接');
    } catch (e) {
      toast(`断开失败：${e}`);
    }
  }

  /** 解除配对：本机动作，**不需要对端在线**（后端只尽力通知它一声）。
   * 设备行不立刻消失：对端还在网上就继续以「未绑定」列出；它离线时这一行本来就不会显示。 */
  async function unpairDevice(deviceId: string) {
    try {
      await removePeer(deviceId);
      toast('已解除配对（对方在线时会继续显示为「未绑定」）');
    } catch (e) {
      toast(`解除配对失败：${e}`);
    }
  }

  async function ctxUnpair() {
    const deviceId = ctxState?.deviceId;
    hideCtx();
    if (!deviceId) return;
    await unpairDevice(deviceId);
  }

  /** 右键「重新配对」：token 已失效，走一遍配对流程换新 token（与行内「配对」同一个入口）。 */
  function ctxPair() {
    const deviceId = ctxState?.deviceId;
    hideCtx();
    const device = deviceId ? getDevices().find((d) => d.id === deviceId) : undefined;
    if (device) openPair(device);
  }

  /** 放入比较位：side 是 'left' | 'right'，文案沿用同一套。 */
  async function ctxBcPlace(side: 'left' | 'right') {
    const c = ctxState;
    hideCtx();
    const target = c ? ctxBcTarget(c) : null;
    if (!target) {
      toast('这项没有可比较的共享路径');
      return;
    }
    try {
      const s = await setBcSide(side, target);
      toast(`已放入${side === 'left' ? '左' : '右'}侧比较位：${s?.label ?? target.label ?? ''}`);
    } catch (e) {
      toast(`设置比较位失败：${e}`);
    }
  }

  async function ctxBcClear() {
    const c = ctxState;
    hideCtx();
    const target = c ? ctxBcTarget(c) : null;
    if (!target) return;
    const st = getBcStatus();
    try {
      // 同一项可能同时占着左右两侧，两边都要清
      for (const side of ['left', 'right'] as const) {
        const s = st[side];
        if (s && sameBcTarget(s.target, target)) await clearSide(side);
      }
      toast('已清除比较位');
    } catch (e) {
      toast(`清除比较位失败：${e}`);
    }
  }

  async function ctxBcCompare() {
    const c = ctxState;
    hideCtx();
    const target = c ? ctxBcTarget(c) : null;
    if (!target) {
      toast('这项没有可比较的共享路径');
      return;
    }
    const st = getBcStatus();
    const { left, right } = st;
    const thisLeft = !!left && sameBcTarget(left.target, target);
    const thisRight = !!right && sameBcTarget(right.target, target);
    // 当前项占位时与另一侧比；没占位时用它补空位（只有一侧占位）；两侧都占位就直接比两侧
    const pair: { a: BcTarget; b: BcTarget; la: string; lb: string } | null =
      left && right ? { a: left.target, b: right.target, la: left.label, lb: right.label }
      : left && !thisLeft ? { a: left.target, b: target, la: left.label, lb: target.label ?? '' }
      : right && !thisRight ? { a: target, b: right.target, la: target.label ?? '', lb: right.label }
      : null;
    if (!pair) {
      toast('请先把要比较的另一项「添加到左侧/右侧比较位」');
      return;
    }
    try {
      await compare(pair.a, pair.b);
      toast(`已在 BeyondCompare 中打开比较：${pair.la} ↔ ${pair.lb}`);
    } catch (e) {
      toast(`BeyondCompare 启动失败：${e}`);
    }
  }

  onMount(() => {
    // 底栏那格版本号要靠这份状态（与设置里「当前版本」同一个源）；订阅是幂等的，
    // 设置面板挂载时也会叫一次 —— 谁先叫都行，但不能只靠它：面板收着时底栏照样要显示。
    void upd.watchUpdate();
    // 设置只拉一次：形态（折叠态尺寸）与其余初始化都等这一份。
    const settingsReady = loadSettings();
    // 折叠态窗口 = 悬浮球/悬浮框 + 四周余量：尺寸在这里归位，**位置**由后端在首帧之前按上次的
    // 落位写好（lib.rs `place_orb_on_startup`）—— 等界面起来再挪会看到球从屏幕中央跳过去。
    // 尺寸也归一次位：形态（设置）决定它，而后端只知道落盘的形态、不知道卡片被 IP 撑宽后的宽度。
    // 位置复原之后还得按同一个矩形重算落位：上次是贴边收起的话，球要滑到被窗口裁掉
    // 的那一位、热区跟着贴边，否则球会以「未贴边」的样子完整站在屏幕边上（而窗口已经贴住
    // 边缘，看上去像被谁挪出来了一截）。这正是 `settleDrag` 对落位矩形做的事，直接复用。
    void (async () => {
      try {
        await settingsReady;   // 形态在设置里：先拿到它再定折叠态尺寸
        sizedShape = orbShape; // 尺寸这一趟就归到位了：别让改尺寸那个 `$effect` 再跑一趟清掉落位
        await appWindow.setSize(new LogicalSize(layout.size.w, layout.size.h));
        await settleDrag();
      } catch (e) {
        console.error('复原悬浮球落位失败:', e);
      }
    })();

    // Init data + server
    void (async () => {
      try {
        await Promise.all([settingsReady, loadDevices(), loadBcStatus()]);
        // 状态框的初始值跟着落盘的设置走（上次退出时是「离线」就还是「离线」）。
        noteLocalOnline(getSettings()?.online ?? true);
        const port = await getActualPort();
        if (port === 0) {
          const p = await startHttpServer();
          setServerPort(p);
        } else {
          setServerPort(port);
        }
        await loadShares();
        // 左下角「端口 · 本机 IP」：启动时取一次，之后只在**网卡真的变了**的时候
        // 由 `local://addresses` 叫一次（见下面的监听）。
        await refreshLocalAddrs();
      } catch (e) { console.error('Init failed:', e); }
    })();

    // Tauri drag-drop
    const unlisten = appWindow.onDragDropEvent((event) => {
      if (event.payload.type === 'over' || event.payload.type === 'enter') {
        dragging = true;
        // 预告：落点是不是会话浮层（一眼看出松手会发生什么，见 `dropInConvo`）。
        convoDropHot = dropInConvo(event.payload.position);
      } else if (event.payload.type === 'leave') {
        dragging = false;
        convoDropHot = false;
      } else if (event.payload.type === 'drop') {
        dragging = false;
        convoDropHot = false;
        const paths = event.payload.paths;
        if (paths.length === 0) return;
        // 落点在**开着的那条会话**上 = 往这条会话推（F-111，与「＋」同一条路）；
        // 其余落点照旧登记成共享项（F-20）。现判坐标：这一次落点的位置只有 `drop` 自己知道。
        if (dropInConvo(event.payload.position)) void sendDroppedToConvo(paths);
        else void addShares(paths);
      }
    });

    // 拖动结束 → 判定吸附。OS 拖动期间 move 事件成串到达（且可能在拖动循环结束后才处理），
    // 所以停手 DOCK_SETTLE_MS 之后才算一次，避免每帧都发 SetWindowPos。
    let moveTimer: ReturnType<typeof setTimeout> | null = null;
    const unMoved = appWindow.onMoved(() => {
      if (moveTimer) clearTimeout(moveTimer);
      moveTimer = setTimeout(() => { moveTimer = null; void settleDrag(); }, DOCK_SETTLE_MS);
    });

    // ── 推送事件：发送侧一张进度卡，接收侧一串通知（面板右下角的气泡组）──────
    // 邀请：接收方收到别人的推送 → 挂一条通知（悬浮球左上角的数字 +1）。
    // **不主动弹面板**：人来点，球上那个黄色数字就是"有事等你"的信号。
    // 未绑定设备发的邀请带「接收/拒绝」按钮，已绑定（`auto`）的已经直接开收、只显示进度。
    const unPushInvite = listen<PushInvite>('push://invite', (ev) => {
      const inv = ev.payload;
      notices = [...notices, {
        inviteId: inv.inviteId,
        kind: inv.auto ? 'receiving' : 'invite',
        from: peerDisplayName(inv.sender.id, inv.sender.name),
        name: inv.name,
        size: inv.size,
        isDir: inv.isDir,
        count: inv.count ?? 1,
        received: 0,
      }];
      // 会话里也落一张卡（同一件事的另一处呈现：右下角是"顺带看一眼"，会话是"跟这台设备的往来"）。
      chat.noteInboundInvite(inv);
    });

    // 一批推送的快照：**发送侧卡片唯一的数据来源**（创建、每台进度、成功/失败/取消）。
    // 单台推送也是一批（只有一条目标）—— 所以这里同时要负责"单台那套收尾方式"（toast +
    // 自动/立刻收掉，与以前一模一样）和"多台的收尾方式"（全成自动收；有失败就粘住等确认）。
    const unPushBatch = listen<PushBatch>('push://batch', (ev) => {
      const b = ev.payload;
      pushBatches = pushBatches.some((x) => x.batchId === b.batchId)
        ? pushBatches.map((x) => (x.batchId === b.batchId ? b : x))
        : [...pushBatches, b];
      // 会话里那几条文件消息的进度与成败，抄的就是这张快照（唯一的进度来源）。
      chat.noteBatch(b);

      // 单台那条路由 push://done 收尾（toast 文案与原来一字不差），这里不插手。
      if (b.targets.length === 1 || settledBatches.has(b.batchId)) return;
      if (batchLive(b)) return;
      settledBatches.add(b.batchId);
      if (batchAllDone(b)) {
        toast(`已推送给 ${b.targets.length} 台设备`);
        setTimeout(() => dropBatch(b.batchId), 3000);
      } else if (b.targets.every((t) => t.state === 'cancelled')) {
        // 整批取消：卡片立刻收掉（一次确认过的操作已经是终局，没有"哪台没成"要交代）。
        toast('已取消推送');
        dropBatch(b.batchId);
      } else if (b.done === 0) {
        // 一台没成：没有"部分成功"要交代，一条提示 + 收起（与单台失败同一个手感）。
        toast(`推送未成功：${b.targets.find((t) => t.error)?.error ?? '全部失败'}`);
        dropBatch(b.batchId);
      } else {
        toast(`推送完成：成功 ${b.done} · 失败 ${b.failed}`);
        // 剩下的交给卡片上的「知道了」——一条 toast 说不清是哪几台。
      }
    });

    const unPushResponse = listen<{ inviteId: string; accepted: boolean; reason?: string | null }>('push://response', (ev) => {
      const { inviteId, accepted, reason } = ev.payload;
      const notice = notices.find((n) => n.inviteId === inviteId);
      if (accepted) {
        chat.noteInviteAccepted(inviteId);
        if (notice) notices = notices.map((n) => (n.inviteId === inviteId ? { ...n, kind: 'receiving' } : n));
        const b = batchForInvite(inviteId);
        if (b && b.targets.length === 1) {
          // 已绑定设备那条路上没有"对方同意"这一步，说"已同意"会让人以为对面点过。
          const t = b.targets[0];
          toast(t.paired ? `开始向「${targetName(t)}」传输` : `「${targetName(t)}」已同意接收，开始传输`);
        }
        return;
      }
      // 对方拒绝 / 60s 没人处理而作废 / 已绑定设备**自动收下时本机放行失败**：本机挂着
      // 的那条通知必须撤掉，否则它会一直留在那儿，点「接收」才发现邀请早就没了（后面那种
      // 情况下 kind 已经是 'receiving' —— 后端这时补一条 accepted:false 回来）。
      // 发送方那张进度卡由随后的 push://done 收尾（两条通道都会跟一条）。
      chat.noteInviteDone(inviteId, 'rejected');
      if (notice && (notice.kind === 'invite' || notice.kind === 'receiving')) {
        dropNotice(inviteId);
        toast(
          notice.kind === 'invite'
            ? `推送邀请已作废：${reason ?? '对方拒绝'}`
            : `接收没能开始：${reason ?? '未知原因'}`,
        );
      }
    });

    const unPushProgress = listen<{ inviteId: string; received: number; total: number }>('push://progress', (ev) => {
      const { inviteId, received, total } = ev.payload;
      if (total <= 0) return;
      // 接收侧：卡片就地变成"正在接收 xx%"。
      notices = notices.map((n) => (n.inviteId === inviteId ? { ...n, kind: 'receiving', received, size: total } : n));
      chat.noteInviteProgress(inviteId, received, total);
      // 发送侧那张卡的进度由 `push://batch` 快照推进（同一份数据只有一个来源）。
    });

    const unPushDone = listen<{ inviteId: string; status: string; reason?: string | null; dest?: string }>('push://done', (ev) => {
      const { inviteId, status, reason, dest } = ev.payload;
      chat.noteInviteDone(inviteId, status === 'completed' ? 'received' : 'failed');
      const notice = notices.find((n) => n.inviteId === inviteId);
      if (notice) {
        if (status === 'completed') {
          toast(`已收到「${notice.name}」`);
          // 收完 ≠ 处理完：转成一条**待确认**的通知挂在右下角气泡里，直到用户点
          // 「知道了」才消失 —— 否则"东西落在哪"会跟着 toast 一起蒸发掉。
          notices = notices.map((n) => (n.inviteId === inviteId ? { ...n, kind: 'done', received: n.size, dest } : n));
        } else if (status === 'cancelled') {
          dropNotice(inviteId);
        } else {
          notices = notices.map((n) => (n.inviteId === inviteId ? { ...n, kind: 'failed', error: reason ?? '接收失败' } : n));
        }
      }
      // 发送侧：**只有单台那一批**在这里收尾（toast 文案与原来一字不差）；多台的收尾
      // 归 `push://batch`（那里才知道整批成了几台、还有没有在等的）。
      const b = batchForInvite(inviteId);
      if (!b || b.targets.length !== 1) return;
      if (status === 'completed') {
        toast(`「${b.name}」已发送完成`);
        setTimeout(() => dropBatch(b.batchId), 3000);
      } else {
        toast(status === 'cancelled' ? '已取消推送' : (reason ?? '推送失败'));
        dropBatch(b.batchId);
      }
    });

    const unPaired = listen('devices://changed', () => {
      loadDevices().catch(console.error);
    });

    // 本机「在线 / 离线」变了：状态框是这份状态的镜像，事件到哪儿它跟到哪儿
    // （`setOnline` 的返回路径与这里都会走同一份值，重复设置是幂等的）。
    const unLocalOnline = listen<{ online: boolean }>('local://online', (ev) => {
      noteLocalOnline(ev.payload.online);
    });

    // 本机网卡地址变了（插网线、开/关热点、切 Wi-Fi、VPN 起来）：左下角「端口 · IP」
    // 重拉一次。后端只在**变化时**发这条（5s 巡检一次网卡），值仍由这里自己拉。
    const unLocalAddrs = listen('local://addresses', () => {
      void refreshLocalAddrs();
    });

    // 状态框展开后，点别处 / 按 Esc 收起。用 **捕获阶段的 pointerdown**：标签栏、
    // 列表里的按钮都可能 `stopPropagation`，冒泡阶段会漏掉一半的"点别处"。
    const closeStatus = (e: PointerEvent) => {
      if (!statusOpen) return;
      if (statusEl && e.target instanceof Node && statusEl.contains(e.target)) return;
      statusOpen = false;
    };
    const escStatus = (e: KeyboardEvent) => {
      if (e.key === 'Escape' && statusOpen) statusOpen = false;
    };
    document.addEventListener('pointerdown', closeStatus, true);
    document.addEventListener('keydown', escStatus);

    // Esc 的层级：收件人选择框 → 会话菜单 → 会话浮层（原型同款顺序）。
    // 会话菜单那一下必须由这里先处理：菜单自己的 Esc 监听是**后挂**的（组件随菜单挂载），
    // 它只会把菜单关掉；先一步把菜单关掉并 return，就不会连带把浮层也关了。
    const escChat = (e: KeyboardEvent) => {
      if (e.key !== 'Escape' || e.defaultPrevented) return;
      if (chat.getRecipientReq()) { chat.closeRecipients(); return; }
      if (ctxState?.kind === 'chat' || ctxState?.kind === 'convo') { hideCtx(); return; }
      if (chat.isChatOpen()) chat.setChatOpen(false);
    };
    document.addEventListener('keydown', escChat);

    // 用户又点了一次应用图标 / 启动了第二份进程：把面板展开，而不是让新进程悄悄消失。
    const unOrbShow = listen('orb://show', () => {
      setOpen(true);
    });

    // 对方输入了本机显示的配对码 → 本机验证通过、给出 token（list 由 devices://changed 刷新）
    const unPairDone = listen<{ peerId: string; peerName?: string | null; inviteId?: string | null }>('pair://completed', (ev) => {
      if (pairConfirming) return;   // 本机自己发起的确认，toast 由 confirmPair 给出
      // 本机发出去的那条邀请被对方接受了：右下角那张"等待确认"的卡收掉 + 提示。
      const waited = notices.find((n) => n.kind === 'pairWait' && n.inviteId === ev.payload.inviteId);
      if (waited) {
        dropNotice(waited.inviteId);
        toast(`已与「${waited.from}」配对成功（点「连接」建立长连接）`);
        return;
      }
      const ps = pairState;
      if (ps?.peerId === ev.payload.peerId) {
        const name = peerDisplayName(ev.payload.peerId, ps.peerName ?? ev.payload.peerName ?? '设备');
        closePair();
        toast(`已与「${name}」配对成功（点「连接」建立长连接）`);
      }
    });

    // 收到一条配对邀请 → 挂一条通知（带「接收/拒绝」），悬浮球左上角的数字 +1。
    // **不主动弹面板**：与推送邀请同一套约定（F-51），人来点那颗球。
    const unPairInvite = listen<{ inviteId: string; peerId: string; peerName: string; unproven?: boolean }>('pair://invite', (ev) => {
      const p = ev.payload;
      notices = [...notices, {
        inviteId: p.inviteId,
        kind: 'pair',
        peerId: p.peerId,
        from: peerDisplayName(p.peerId, p.peerName),
        name: '',
        size: 0,
        isDir: false,
        count: 1,
        received: 0,
        unproven: p.unproven === true,
      }];
    });

    // 一条进来的邀请作废了（本机 60s 没处理 / 对方把邀请撤回了）：撤掉那条通知并说明原因。
    const unPairInviteClosed = listen<{ inviteId: string; reason?: string | null }>('pair://invite_closed', (ev) => {
      const { inviteId, reason } = ev.payload;
      if (!notices.some((n) => n.inviteId === inviteId)) return;
      dropNotice(inviteId);
      toast(`配对邀请已作废：${reason ?? '超时未处理'}`);
    });

    // 本机发起的邀请没成（对方拒绝 / 超时 / 已撤回 / 联系不上）：把右下角那张卡收掉并说明原因。
    const unPairInviteFailed = listen<{ inviteId: string; reason?: string | null }>('pair://invite_failed', (ev) => {
      const { inviteId, reason } = ev.payload;
      if (!notices.some((n) => n.kind === 'pairWait' && n.inviteId === inviteId)) return;
      dropNotice(inviteId);
      // 用户自己点「取消」时那张卡已经收掉了（撤回来的 403 落到这里时什么都不用说）。
      const why = reason ?? '对方未接受';
      toast(why.includes('取消') ? '配对邀请已取消' : `配对邀请未完成：${why}`);
    });

    // Context menu from child components
    const handleCtx = (e: Event) => {
      const detail = (e as CustomEvent).detail as any;
      if (detail && detail.kind) {
        ctxState = { x: detail.x, y: detail.y, el: detail.el, kind: detail.kind, shareId: detail.shareId, path: detail.path, isDir: detail.isDir, deviceState: detail.deviceState, deviceOnline: detail.online, deviceNeedsPair: detail.needsPair, deviceId: detail.deviceId, peerId: detail.peerId, name: detail.name };
      }
    };
    document.addEventListener('od:ctx', handleCtx);

    // 右键全局兜底：webview 自带一份**默认右键菜单**（wry 打开 WebView2 的
    // AreDefaultContextMenusEnabled；dev 构建里还带「检查」），任何没被拦下的 contextmenu
    // 都会把它叫出来。这里一律 preventDefault 把它关掉 —— 需要右键的地方（设备行/共享项/
    // 树节点/通知条）各自在自己的处理里 preventDefault + 开菜单，走的是同一份事件，不受影响。
    // 用捕获阶段：这些行会 stopPropagation，冒泡阶段兜不到。
    const killDefaultCtx = (e: Event) => e.preventDefault();
    document.addEventListener('contextmenu', killDefaultCtx, true);

    // 子组件要提示（拖拽下载的准备进度/失败原因）时冒上来：浮层只有这一层有。
    const handleToast = (e: Event) => {
      const msg = (e as CustomEvent<string>).detail;
      if (msg) toast(msg);
    };
    document.addEventListener('od:toast', handleToast);

    return () => {
      unlisten.then(fn => fn());
      unMoved.then(fn => fn());
      if (moveTimer) clearTimeout(moveTimer);
      unPushInvite.then(fn => fn());
      unPushBatch.then(fn => fn());
      unPushResponse.then(fn => fn());
      unPushProgress.then(fn => fn());
      unPushDone.then(fn => fn());
      unPaired.then(fn => fn());
      unLocalOnline.then(fn => fn());
      unLocalAddrs.then(fn => fn());
      document.removeEventListener('pointerdown', closeStatus, true);
      document.removeEventListener('keydown', escStatus);
      document.removeEventListener('keydown', escChat);
      unOrbShow.then(fn => fn());
      unPairDone.then(fn => fn());
      unPairInvite.then(fn => fn());
      unPairInviteClosed.then(fn => fn());
      unPairInviteFailed.then(fn => fn());
      document.removeEventListener('od:ctx', handleCtx);
      document.removeEventListener('contextmenu', killDefaultCtx, true);
      document.removeEventListener('od:toast', handleToast);
    };
  });
</script>

<!-- Orb Stage -->
<aside
  class="orb-stage"
  class:open={expanded}
  class:live={connectedCount > 0}
  class:offline={!isLocalOnline()}
  class:drag={dragging}
  class:hold={holdOrb}
  class:docked={rectDock !== 0}
  class:docked-left={rectDock === -1}
  class:docked-right={rectDock === 1}
  class:menu={orbMenuGeo.shift.x !== 0 || orbMenuGeo.shift.y !== 0}
  class:menu-busy={orbMenuBusy}
  style="--panel-slack: {PANEL_SLACK}px; --orb-inset: {layout.widget.x}px; --orb-inset-y: {layout.widget.y}px; --foot-h: {footH}px; --notif-max-h: {notifMaxH}px; --orb-x: {orbOffset.x}px; --orb-y: {orbOffset.y}px; --zone-x: {orbZone.x}px; --zone-y: {orbZone.y}px; --zone-w: {orbZone.w}px; --zone-h: {orbZone.h}px; --spin-l: {badgeSpin.left}deg; --spin-r: {badgeSpin.right}deg; --menu-dx: {orbMenuGeo.shift.x}px; --menu-dy: {orbMenuGeo.shift.y}px"
>
  <!-- 悬停热区：**只有屏上看得见的那一条**（贴边收起后 = 露出那截弧的方框 / 那条竖条，
       见 orbLayout 的 planDock / planRectDock 的 `zone`）—— 指针落在它上面才把悬浮体呼出来。
       呼出之后指针落在滑回完整位的悬浮体上，而悬浮体在这块 div **里面**（子元素）：指针在子元素
       上时 `pointerleave` 不会发给这里，所以这一条 `reveal` 挂得住，`collapsedHitRects` 也据此
       把悬浮体本体并进可点区域（不含它的话，指针一离开那一条，窗口就当场穿透、hover 丢掉，
       悬浮体又收回去）。圆球那一档的显隐还是 CSS 的 `:hover`（滑出来的是同一颗球，指针在不在
       上面差别不大；球滑走后指针仍压在这块 div 上，`:hover` 也不会丢）；
       圆角矩形那一档走 `.reveal`（App 按这两个指针事件维护）：贴边落位时指针本来就压在上面，
       照 `:hover` 直接显示的话"贴到边上"这一步看不到结果（`zoneHold` 压住的那一次，
       见 `setDockOrb`）。
       两个形态在**拖入**时也走 `.reveal`（`reveal` = `zoneHot || dragging`，见它的说明）：
       OS 的文件拖动收不到任何指针事件，贴边收起时那条竖条上给不出半点预告，而松手真的会登记
       共享项 —— 拖入因此复用悬停那一档显示（滑回完整位 + 拖入高亮），只是**不展开面板**。 -->
  <div
    class="orb-zone"
    class:reveal={reveal}
    role="presentation"
    onpointerenter={() => { if (zoneHold) return; zoneHot = true; }}
    onpointerleave={() => { zoneHot = false; zoneHold = false; }}
  >
    {#if orbShape === 'rect'}
      <RectOrb
        pages={rectPages}
        deviceCount={connectedCount}
        notifCount={orbNotifCount}
        {expanded}
        docked={rectDock !== 0}
        onToggle={() => setOpen(!expanded)}
        onDragStart={beginOrbDrag}
        onActivate={(i, j) => {
          const page = rectPages[i];
          const slot = page?.items[j];
          if (page && slot) activateRectSlot(page, slot);
        }}
        onUp={(i) => {
          const page = rectPages[i];
          if (page) upRect(page.id);
        }}
        onContext={openRectCtx}
        onToast={toast}
      />
    {:else}
      <Orb
        badgeCount={connectedCount}
        notifCount={orbNotifCount}
        expanded={expanded}
        onToggle={() => setOpen(!expanded)}
        onDragStart={beginOrbDrag}
      />
    {/if}
  </div>

  <!-- Main Panel -->
  <section class="panel" aria-label="OrbDrop 主面板">
    <!-- `deep`：Tauri 的拖拽脚本只在**命中元素自己**带属性时才拖（`composedPath[0]`），
         logo/文字这些子元素会把 mousedown 从 header 上挡掉，于是只有空隙能拖。`deep` 让整棵
         子树里的非可点击元素都触发拖拽；按钮仍照旧拦截（脚本先判 clickable 再看属性）。 -->
    <header class="panel-head" data-tauri-drag-region="deep">
      <span class="panel-brand"><Logo size={20} />OrbDrop</span>
      <div class="panel-head-right">
        <!-- 本机「在线 / 离线」（面板右上角）。点一下**向下长出**选项列表：框只动高度
             （行数 = 选项条数），圆角一个像素都不变 —— 收起时框高 = 一行，14px 圆角正好是
             左右半圆；展开时是高两倍的矩形、同一个 14px 圆角。展开时框盖在标签栏上，
             面板头自己不跟着变高（框绝对定位贴着头部的上沿长）。 -->
        <div class="ch-wrap" bind:this={statusEl}>
          <div class="ch-inst" class:open={statusOpen} class:off={!isLocalOnline()}>
            <div class="ch-list">
              <button class="ch-row online" class:active={isLocalOnline()} aria-expanded={isLocalOnline() ? statusOpen : undefined} onclick={() => pickOnline(true)}>
                <span class="dot ok"></span>
                <span>在线</span>
                <!-- 与改前那颗胶囊同款文案：`在线 · 长连接 N`。本机离线时那台设备一条长连接
                     都没有，写出来只是噪音 —— 所以整段**藏起来**（`visibility`）而不是删掉：
                     元素仍占着同样的宽度，切到离线时框不会缩一截。 -->
                <span class="ch-meta" class:gone={!isLocalOnline()}>· 长连接 {connectedCount}</span>
                <svg class="ch-caret" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="m6 9 6 6 6-6"/></svg>
              </button>
              <button class="ch-row offline" class:active={!isLocalOnline()} aria-expanded={!isLocalOnline() ? statusOpen : undefined} onclick={() => pickOnline(false)}>
                <span class="dot off"></span>
                <span>离线</span>
                <svg class="ch-caret" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="m6 9 6 6 6-6"/></svg>
              </button>
            </div>
          </div>
        </div>
        <!-- 通知铃铛搬到 footer 最右侧（版本号右边）—— 面板头部只留窗口级的动作。 -->
        <button class="icon-btn" onclick={openSettings} aria-label="打开设置">
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"><circle cx="12" cy="12" r="3"/><path d="M19.4 15a1.7 1.7 0 0 0 .34 1.87l.06.06a2 2 0 1 1-2.83 2.83l-.06-.06a1.7 1.7 0 0 0-1.87-.34 1.7 1.7 0 0 0-1 1.55V21a2 2 0 1 1-4 0v-.09a1.7 1.7 0 0 0-1-1.55 1.7 1.7 0 0 0-1.87.34l-.06.06a2 2 0 1 1-2.83-2.83l.06-.06a1.7 1.7 0 0 0 .34-1.87 1.7 1.7 0 0 0-1.55-1H3a2 2 0 1 1 0-4h.09a1.7 1.7 0 0 0 1.55-1 1.7 1.7 0 0 0-.34-1.87l-.06-.06a2 2 0 1 1 2.83-2.83l.06.06a1.7 1.7 0 0 0 1.87.34h.01a1.7 1.7 0 0 0 1-1.55V3a2 2 0 1 1 4 0v.09a1.7 1.7 0 0 0 1 1.55 1.7 1.7 0 0 0 1.87-.34l.06-.06a2 2 0 1 1 2.83 2.83l-.06.06a1.7 1.7 0 0 0-.34 1.87v.01a1.7 1.7 0 0 0 1.55 1H21a2 2 0 1 1 0 4h-.09a1.7 1.7 0 0 0-1.55 1Z"/></svg>
        </button>
        <button class="icon-btn" onclick={() => setOpen(false)} aria-label="收起面板">
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="m6 9 6 6 6-6"/></svg>
        </button>
      </div>
    </header>

    <TabBar
      localCount={localCount}
      deviceCount={deviceCount}
      {browsers}
      {activeTab}
      onSelect={(tab: TabKey) => activeTab = tab}
    />

    <div class="panel-body" bind:this={bodyEl}>
      <TabContent tab={activeTab} devices={getDevices()} onToast={toast} onMessage={(d) => openDeviceChat(d.id)} onAddShare={requestAddShare} onOpenPair={openPair} onUnpair={(d) => void unpairDevice(d.id)} />
    </div>

    <footer class="panel-foot" bind:this={footEl}>
      <span class="foot-left">
        <!-- 会话入口：常驻、不遮挡内容（原型 `.foot-msg`）。角标 = 会话侧待处理总数
             （未读 + 还没确认的群邀请，与悬浮球左上角那颗同一个数）——「收起 ≠ 已读」：
             收起浮层不清未读，只有真打开那个会话才算读过；群邀请只能靠点「加入/不加入」消掉。 -->
        <button
          class="foot-msg"
          class:open={chat.isChatOpen()}
          type="button"
          aria-label={convoBadgeLabel ? `会话，${convoBadgeLabel}` : '会话'}
          aria-expanded={chat.isChatOpen()}
          onclick={toggleChat}
        >
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><rect x="3.75" y="5.25" width="16.5" height="13.5" rx="4.25"/><circle cx="8.6" cy="12" r="1.15" fill="currentColor" stroke="none"/><circle cx="12" cy="12" r="1.15" fill="currentColor" stroke="none"/><circle cx="15.4" cy="12" r="1.15" fill="currentColor" stroke="none"/></svg>
          <span>会话</span>
          {#if convoBadgeCount > 0}<span class="msg-fab-badge num">{convoBadgeCount > 99 ? '99+' : convoBadgeCount}</span>{/if}
        </button>
        {#if localAddrs.primary}
          <!-- 本机内网「IP:端口」：多网卡时只显示首个，悬停列出全部（跨网段手动添加要用）；点一下整串复制。 -->
          <button class="foot-addr num" onclick={copyAddr} title={addrTitle}>{localAddrs.primary}:{serverPortVal || 3890}</button>
        {:else}
          <span class="num">端口 {serverPortVal || 3890}</span>
        {/if}
      </span>
      <span class="foot-right">
        <span>OrbDrop v{appVersion}</span>
        <!-- 通知铃铛（最右下角、版本号右边）：点一下收起/展开右下角那组通知气泡。
             角标 = 未处理的通知数，与悬浮球左上角黄标是同一个数 —— **收起 ≠ 已读**。 -->
        <button
          class="icon-btn bell"
          class:active={notifOpen}
          class:has={notifCount > 0}
          onclick={() => (notifOpen = !notifOpen)}
          aria-label={notifOpen ? '收起通知' : '展开通知'}
          aria-expanded={notifOpen}
        >
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M18 8a6 6 0 1 0-12 0c0 7-3 9-3 9h18s-3-2-3-9"/><path d="M13.7 21a2 2 0 0 1-3.4 0"/></svg>
          {#if notifCount > 0}<span class="bell-badge num">{notifCount}</span>{/if}
        </button>
      </span>
    </footer>

    <!-- 面板底部堆叠层（`column-reverse`：**DOM 里靠前的元素贴底** —— 谁写在前面，谁最靠近
         footer）。顺序 = 通知气泡 → 发送进度卡 → 提示：通知要贴右下角、就落在 footer 上缘；
         进度卡与瞬时提示往上叠。放在 panel 里而不是顶层：面板收起时 opacity:0，
         这一列结构上就不可能画进那个 90px 的悬浮球窗口。 -->
    <div class="panel-bottom">
      <!-- 通知组（面板右下角的气泡组）：未绑定的邀请（带接收/拒绝）、正在接收（进度 + 取消）、
           已收到待确认、接收失败，外加**发送侧的进度卡**与**「更新这件事」**（启动时的静默
           检查发现新版）—— 全都同一形状：右对齐、2/3 面板宽（不占满）。
           来一条甩入一条、收起时整组甩回右侧（`fly`）；footer 最右侧的铃铛整体收起/展开它，
           **收起 ≠ 已读**（角标照旧挂着，与悬浮球左上角黄标同一个数）。 -->
      {#if notifOpen && (notices.length > 0 || pushBatches.length > 0 || updateTask)}
        <div class="notif-group" bind:this={notifEl}>
          <!-- 「更新这件事」（F-110）：启动时的静默检查发现新版时它就在这儿等用户决定，
               点过「立即更新」之后**就地变成进度**（见 store 的 `updateTask`）—— 这一格是
               "提示 → 进度 → 失败"同一格三种画法，与那些等你确认的通知同一个形状、同一个
               铃铛与角标。面板收着时，悬浮球左上角那颗黄标就是唯一还看得见的提示。 -->
          {#if updateTask}
            <div
              class="notif-bubble"
              class:fail={updateTask.phase === 'error'}
              in:fly={{ x: 32, duration: MOTION, easing: cubicOut }}
              out:fly={{ x: 32, duration: MOTION }}
            >
              <div class="p-top">
                <span class="nb-title">{updateTitleText(updateTask)}</span>
                <span class="nb-size num">
                  {#if updateTask.phase === 'downloading' || updateTask.phase === 'verifying'}
                    {updatePct}%
                  {:else if updateTask.size}
                    <!-- 提示那一档给"要下多大"；正在收尾/已经失败时不报百分比 -->
                    {fmtBytes(updateTask.size)}
                  {/if}
                </span>
              </div>

              {#if updateTask.phase === 'downloading' || updateTask.phase === 'verifying'}
                <div class="p-bar"><i style="width:{updatePct}%"></i></div>
                <p class="nb-body">{updateStatusText(updateTask)}</p>
              {:else if updateTask.phase === 'available'}
                <!-- 版本号已经写在标题里了，正文说"更新什么 / 点下去会发生什么"。 -->
                <p class="nb-body">{updateTask.notes ?? '下载完会用新版本覆盖当前程序并重启。'}</p>
              {:else}
                <p class="nb-body">{updateStatusText(updateTask)}</p>
              {/if}

              <div class="nb-acts">
                {#if updateTask.phase === 'available'}
                  <button class="btn btn-secondary btn-sm" onclick={() => upd.dismissUpdatePrompt()}>稍后</button>
                  <button
                    class="btn btn-primary btn-sm"
                    disabled={upd.isApplying()}
                    onclick={() => void installUpdateNow()}
                  >
                    {upd.isApplying() ? '准备中…' : '立即更新'}
                  </button>
                {:else if updateTask.phase === 'error'}
                  <button class="btn btn-secondary btn-sm" onclick={() => upd.dismissUpdatePrompt()}>知道了</button>
                  <button
                    class="btn btn-primary btn-sm"
                    disabled={upd.isApplying()}
                    onclick={() => void installUpdateNow()}
                  >
                    重试
                  </button>
                {/if}
              </div>
            </div>
          {/if}

          {#each notices as n, i (n.inviteId)}
            <div
              class="notif-bubble"
              class:fail={n.kind === 'failed'}
              in:fly={{ x: 32, duration: MOTION, delay: Math.min(i, 4) * 30, easing: cubicOut }}
              out:fly={{ x: 32, duration: MOTION }}
            >
              <div class="p-top">
                <span class="nb-title">{noticeTitle(n)}</span>
                <span class="nb-size num">
                  {#if n.kind === 'pair'}{:else if n.kind === 'receiving'}{pushPercent(n.received, n.size)}%{:else}{fmtBytes(n.size)}{/if}
                </span>
              </div>

              {#if n.kind === 'receiving'}
                <div class="p-bar"><i style="width:{pushPercent(n.received, n.size)}%"></i></div>
                <div class="p-meta">
                  <span>来自「{n.from}」</span>
                  <span class="num">{fmtBytes(n.received)} / {fmtBytes(n.size)}</span>
                </div>
                <div class="nb-acts">
                  <button class="btn btn-secondary btn-sm" onclick={() => void cancelNotice(n)}>取消</button>
                </div>
              {:else if n.kind === 'pair'}
                <div class="consent-info">
                  <span class="f-icon">
                    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M10 13a5 5 0 0 0 7 0l3-3a5 5 0 0 0-7-7l-1.5 1.5"/><path d="M14 11a5 5 0 0 0-7 0l-3 3a5 5 0 0 0 7 7l1.5-1.5"/></svg>
                  </span>
                  <span class="c-name">{n.from}</span>
                </div>
                <p class="nb-body">{noticeBody(n)}</p>
                <div class="nb-acts">
                  <button class="btn btn-secondary btn-sm" onclick={() => void rejectPairNotice(n)}>拒绝</button>
                  <button class="btn btn-primary btn-sm" onclick={() => void acceptPairNotice(n)}>接收</button>
                </div>
              {:else if n.kind === 'pairWait'}
                <p class="nb-body">{noticeBody(n)}</p>
                <div class="nb-acts">
                  <button class="btn btn-secondary btn-sm" onclick={() => void cancelPairWaitNotice(n)}>取消</button>
                </div>
              {:else if n.kind === 'invite'}
                <div class="consent-info">
                  <span class="f-icon">
                    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6"><path d="M6 3h8l5 5v13H6V3Z"/></svg>
                  </span>
                  <span class="c-name">{n.name}</span>
                </div>
                <div class="nb-acts">
                  <button class="btn btn-secondary btn-sm" onclick={() => void rejectNotice(n)}>拒绝</button>
                  <button class="btn btn-primary btn-sm" onclick={() => void acceptNotice(n)}>接收</button>
                </div>
              {:else}
                <p class="nb-body">{noticeBody(n)}</p>
                <div class="nb-acts">
                  {#if n.dest}
                    <button class="btn btn-secondary btn-sm" onclick={() => void openNoticeDir(n)}>打开目录</button>
                  {/if}
                  <button class="btn btn-secondary btn-sm" onclick={() => confirmNotice(n.inviteId)}>知道了</button>
                </div>
              {/if}
            </div>
          {/each}

          <!-- 发送侧：一次推送操作（可能覆盖多台设备）一张卡 —— 和通知同一个形状、同一组、
               同一个铃铛管，因为它也是"右下角的一件事"（"等待对方同意"可以等很久；已绑定
               目标那一档只是一瞬）。它排在通知前面：通知贴最下缘（要你看一眼的事优先）。

               多台时多两样东西：**每台一行**（谁成了、谁还在等、谁没成，都逐行写清楚），
               以及那行计数 —— 数字全部来自后端快照（见 `push://batch`），界面不算账。 -->
          {#each pushBatches as b (b.batchId)}
            <div
              class="notif-bubble"
              class:done={batchAllDone(b)}
              in:fly={{ x: 32, duration: MOTION, easing: cubicOut }}
              out:fly={{ x: 32, duration: MOTION }}
            >
              <div class="p-top">
                <span class="nb-title" title={batchTitleFull(b)}>{pushBatchTitle(b)}</span>
                <span class="nb-size num">{pushPercent(b.received, b.total)}%</span>
              </div>
              <div class="p-bar"><i style="width:{pushPercent(b.received, b.total)}%"></i></div>
              <div class="p-meta">
                {#if b.targets.length > 1}
                  <span>{batchCounts(b)}</span>
                {:else}
                  <span>→「{b.targets[0].target}」· 局域网</span>
                {/if}
                <span class="num">{fmtBytes(b.received)} / {fmtBytes(b.total)}</span>
              </div>

              <!-- 清单里少了的项（链接 / 读不到）：卡片上必须写出来 —— 清单少了东西却不说，
                   用户会以为"就这些"然后推漏（同 `fs_browse::list_dir` 那条注释的道理）。 -->
              {#if b.skipped > 0}
                <div class="p-skip">已跳过 {b.skipped} 项（链接 / 读不到）</div>
              {/if}

              {#if b.targets.length > 1}
                <div class="batch-rows">
                  {#each visibleTargets(b) as t (t.inviteId || t.peerId)}
                    <div class="batch-row" class:bad={t.state === 'failed' || t.state === 'rejected'}>
                      <span class={targetDot(t)}></span>
                      <span class="br-name" title={targetName(t)}>{targetName(t)}</span>
                      <span class="br-state">{targetLabel(t, batchPerTarget(b))}</span>
                      {#if t.state === 'invited' || t.state === 'sending'}
                        <button
                          class="br-x"
                          onclick={() => void cancelSending(t.inviteId)}
                          aria-label="取消推送给「{targetName(t)}」"
                          title="取消这一台（其余几台照常）">✕</button>
                      {/if}
                    </div>
                  {/each}
                  {#if b.targets.length > 5 && !expandedBatches[b.batchId]}
                    <button class="br-more" onclick={() => (expandedBatches = { ...expandedBatches, [b.batchId]: true })}>
                      还有 {b.targets.length - 4} 台 ▾
                    </button>
                  {/if}
                </div>
              {/if}

              <div class="nb-acts">
                {#if batchLive(b)}
                  <button class="btn btn-secondary btn-sm" onclick={() => void cancelBatch(b.batchId)}>
                    {b.targets.length > 1 ? '取消全部' : '取消'}
                  </button>
                {:else if batchSticky(b)}
                  <button class="btn btn-secondary btn-sm" onclick={() => dropBatch(b.batchId)}>知道了</button>
                {/if}
              </div>
            </div>
          {/each}
        </div>
      {/if}

      <div class="toast-stack">
        {#each toasts as t (t.id)}
          <div class="toast" class:leaving={t.leaving}>
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round">
              <path d="M12 3v12M7 10l5 5 5-5M5 19h14"/>
            </svg>
            {t.msg}
          </div>
        {/each}
      </div>
    </div>

    <!-- Settings sheet -->
    <SettingsPanel open={showSettings} onClose={closeSettings} onToast={toast} />

    <!-- Pair dialog overlay -->
    {#if pairState}
      <div class="overlay open" id="pairDialog">
        <div class="dialog">
          {#if pairState.peerId}
            <!-- 设备名一律「」框住（本文件里的 toast / 通知标题同此），并排的「推送到「X」」同理 -->
            <h3 class="d-head">
              与「{pairState.peerName}」配对<Hint>
                两种方式任选：「发起邀请」由对方在它自己的通知里点「接收」（不需要输入配对码）；
                或让对方把下方这串码输入到它的 OrbDrop 里。
              </Hint>
            </h3>
            <button class="btn btn-primary btn-block pair-invite" onclick={() => void invitePair()} disabled={pairState.busy}>
              发起邀请
            </button>
            <p class="d-target num">或：输入到「{pairState.peerName}」的 OrbDrop</p>
            <div class="code num" class:expired={pairState.remaining === 0}>
              {#each pairState.code as digit}
                <b>{digit}</b>
              {/each}
            </div>
            {#if pairState.remaining > 0}
              <p class="code-hint num">剩余 {pairState.remaining}s · 过期后点「刷新配对码」换新码</p>
            {:else}
              <p class="code-hint num">已过期，旧码已作废</p>
              <button class="btn btn-secondary btn-sm btn-block" onclick={refreshPairCode} disabled={pairState.busy}>刷新配对码</button>
            {/if}
            <div class="field pair-enter">
              <label for="pairCodeInput">输入对方设备上显示的配对码</label>
              <div class="path-input">
                <input
                  id="pairCodeInput"
                  class="input num"
                  bind:value={pairState.input}
                  maxlength="6"
                  inputmode="numeric"
                  autocomplete="off"
                  placeholder="6 位数字"
                />
                <button class="btn btn-primary" onclick={confirmPair} disabled={pairState.busy}>确认配对</button>
              </div>
            </div>
            {#if pairState.error}<p class="d-err">{pairState.error}</p>{/if}
            <div class="d-acts">
              <button class="btn btn-secondary" onclick={copyCode}>复制配对码</button>
              <button class="btn btn-secondary" onclick={closePair}>取消</button>
            </div>
          {:else}
            <h3>添加设备</h3>
            <p class="d-desc">输入对方设备的 IP 与端口，添加后即可与它配对（跨网段场景）。</p>
            <div class="field">
              <label for="pairIp">对方 IP / IP:端口</label>
              <input id="pairIp" class="input num" bind:value={pairState.ip} placeholder="192.168.1.4:3890" />
            </div>
            <div class="field pair-port">
              <label for="pairPort">端口</label>
              <input id="pairPort" class="input num" bind:value={pairState.port} inputmode="numeric" placeholder="3890" />
            </div>
            {#if pairState.error}<p class="d-err">{pairState.error}</p>{/if}
            <div class="d-acts">
              <button class="btn btn-secondary" onclick={closePair}>取消</button>
              <button class="btn btn-primary" onclick={addAndPair} disabled={pairState.busy}>添加并配对</button>
            </div>
          {/if}
        </div>
      </div>
    {/if}

    <!-- 设备备注（F-101）：右键设备行 →「修改备注」。备注**只写本机**
         （`config.toml` 的 `deviceRemarks`），对端看不到、也不改对方的设备名；
         设了备注之后，本机界面上这台设备的名字显示成「备注(设备名)」。
         留空保存 = 清除这一条（与「清除备注」同一件事）。 -->
    {#if remarkState}
      <div class="overlay open" id="remarkDialog">
        <div class="dialog">
          <h3 class="d-head">设备备注<Hint>
            备注是<b>你</b>给这台设备起的名字，只保存在本机（`config.toml`），
            <span class="hl">不会发给对端</span>，也不改对方自己的设备名。
            设好之后本机到处都用「<b>备注(设备名)</b>」显示这台设备（设备行、标签栏、会话标题、
            收件人选择框、推送卡都算）。备注跟着<b>设备</b>走，不跟地址走 ——
            换网段、重新配对之后它还在；留空保存即清除。
          </Hint></h3>
          <p class="d-desc">当前设备名：{remarkState.rawName}</p>
          <div class="field">
            <label for="remarkInput">备注</label>
            <input
              id="remarkInput"
              class="input"
              bind:value={remarkState.value}
              maxlength="24"
              placeholder="例如：客厅台式机"
              autocomplete="off"
              spellcheck="false"
              use:focusAndSelect
              onkeydown={(e) => { if (e.key === 'Enter') void saveRemark(); }}
            />
          </div>
          {#if remarkState.error}<p class="d-err">{remarkState.error}</p>{/if}
          <div class="d-acts">
            {#if remarkState.saved}
              <button class="btn btn-secondary" onclick={() => void clearRemark()} disabled={remarkState.busy}>清除备注</button>
            {/if}
            <button class="btn btn-secondary" onclick={closeRemark}>取消</button>
            <button class="btn btn-primary" onclick={() => void saveRemark()} disabled={remarkState.busy}>保存</button>
          </div>
        </div>
      </div>
    {/if}

    <!-- 会话浮层（左下角展开）与收件人选择框：与设置、配对、选择框同一套层叠，都在面板里 ——
         面板收起时整块跟着消失（浮层由 `collapse()` 一起关掉）。会话记录在后端，
         这里只画；浮层内部自己管列表 ↔ 详情两屏。 -->
    <ConvoPanel
      pickedId={ctxState?.kind === 'chat' ? ctxState.chatId ?? null : null}
      dropHot={convoDropHot}
      onToast={toast}
      onSendFiles={chatSendFiles}
      onAcceptInvite={chatAcceptInvite}
      onRejectInvite={chatRejectInvite}
      onConvoCtx={openConvoCtx}
      onMessageCtx={openMessageCtx}
    />
    {#if chat.getRecipientReq()}
      <RecipientDialog />
    {/if}

    <!-- 文件选择框：文件与文件夹在同一个框里混选、选中项跨目录累积。推送与「添加共享项」
         都用它 —— 开成什么样由 `picker` 这个请求对象决定（`closePicker` 关掉它）。 -->
    {#if picker}
      <FilePicker
        title={picker.title}
        sub={picker.sub}
        confirmLabel={picker.confirmLabel}
        onConfirm={picker.onConfirm}
        onClose={closePicker}
      />
    {/if}

  </section>
</aside>

<!-- 悬停说明的气泡层：必须挂在 `.orb-stage` 之外 —— 面板里的滚动容器会裁掉它，
     对话框的 `backdrop-filter` 还会把 `position: fixed` 改成相对对话框定位。 -->
<HintLayer />

<!-- Context menu -->
{#if ctxState}
  <ContextMenu
    x={ctxState.x}
    y={ctxState.y}
    kind={ctxState.kind}
    shareId={ctxState.shareId}
    path={ctxState.path}
    isDir={ctxState.isDir}
    deviceState={ctxState.deviceState}
    deviceOnline={ctxState.deviceOnline}
    deviceNeedsPair={ctxState.deviceNeedsPair}
    deviceRemark={ctxState.deviceId ? getDevices().find((d) => d.id === ctxState?.deviceId)?.remark ?? null : null}
    peerName={ctxState.peerId ? peerDisplayName(ctxState.peerId) : undefined}
    bcAvailable={bcMenu.available}
    bcLeftLabel={bcMenu.leftLabel}
    bcRightLabel={bcMenu.rightLabel}
    bcCompareLabel={bcMenu.compareLabel}
    bcCompareEnabled={bcMenu.compareEnabled}
    bcThisStaged={bcMenu.thisStaged}
    chatSel={!!ctxState.chatSel}
    convoBrowse={ctxState.convoBrowse === true}
    convoThread={ctxState.convoThread === true}
    convoBc={ctxState.convoBc === true}
    compact={ctxState.orb === true}
    pending={ctxState.orb === true && ctxState.placed === false}
    pinned={ctxState.orb === true}
    onClose={hideCtx}
    onToast={toast}
    onDownload={ctxDownload}
    onOpen={ctxOpen}
    onCopy={ctxCopy}
    onRemove={ctxRemove}
    onConnect={ctxConnect}
    onDisconnect={ctxDisconnect}
    onPair={ctxPair}
    onUnpair={ctxUnpair}
    onRemark={ctxRemark}
    onMessage={ctxMessage}
    onChatCopy={ctxChatCopy}
    onConvoFiles={ctxConvoFiles}
    onConvoEdit={ctxConvoEdit}
    onConvoUpgrade={() => void ctxConvoUpgrade()}
    onConvoExit={() => void ctxConvoExit()}
    onPeerDownload={ctxPeerDownload}
    onBcCompare={ctxBcCompare}
    onBcAddLeft={() => ctxBcPlace('left')}
    onBcAddRight={() => ctxBcPlace('right')}
    onBcClear={ctxBcClear}
  />
{/if}
