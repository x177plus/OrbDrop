<script lang="ts">
  import { getCurrentWebviewWindow } from '@tauri-apps/api/webviewWindow';
  import { LogicalSize } from '@tauri-apps/api/dpi';
  import { onMount } from 'svelte';
  import { fly } from 'svelte/transition';
  import { cubicOut } from 'svelte/easing';
  import { listen } from '@tauri-apps/api/event';
  import { getDevices, loadDevices } from '$lib/stores/devices.svelte';
  import { getServerPort, setServerPort, loadShares, addShare, removeShare, getShares } from '$lib/stores/shares.svelte';
  import { loadSettings } from '$lib/stores/settings.svelte';
  import { getBcStatus, loadBcStatus, setBcSide, clearSide, compare } from '$lib/stores/bc.svelte';
  import { getActualPort, startHttpServer, acceptPush, rejectPush, cancelPush, cancelReceive, pushPaths, localAddresses, orbScreenMetrics, orbSetRect, orbDragStart, orbMouseDown, orbSavePos, addPeer, pairStart, pairConfirm, pairInvite, cancelPairInvite, acceptPairInvite, rejectPairInvite, connectPeer, disconnectPeer, removePeer, downloadFromPeer } from '$lib/api';
  import type { PushInvite, PeerDevice, TabKey, BcTarget, OrbDragInfo, LocalPick } from '$lib/types';
  import { planExpansion, planCollapse, planDock, type DockPlan, type ScreenRect } from '$lib/orbLayout';
  import Orb from '$lib/components/Orb.svelte';
  import FilePicker from '$lib/components/FilePicker.svelte';
  import Logo from '$lib/components/Logo.svelte';
  import TabBar from '$lib/components/TabBar.svelte';
  import TabContent from '$lib/components/TabContent.svelte';
  import SettingsPanel from '$lib/components/SettingsPanel.svelte';
  import ContextMenu from '$lib/components/ContextMenu.svelte';
  import Hint from '$lib/components/Hint.svelte';
  import HintLayer from '$lib/components/HintLayer.svelte';

  const appWindow = getCurrentWebviewWindow();
  const COLLAPSED = new LogicalSize(90, 90);
  const ORB_SIZE = 62;                                             // 悬浮球直径（CSS px）
  const PANEL = { w: 448, h: 620 };                                // 面板标称尺寸（CSS px）
  const PANEL_SLACK = 20;                                          // 面板四周留给阴影的余量（CSS px）：必须 ≥ --shadow 的可见半径（main.css 里有实测值），否则阴影被窗口边裁出切线
  const EXPANDED = new LogicalSize(PANEL.w + PANEL_SLACK * 2, PANEL.h + PANEL_SLACK * 2);
  const ORB_INSET = (COLLAPSED.width - ORB_SIZE) / 2;              // 折叠态悬浮球在窗口内的内缩
  const WORK_MARGIN = 8;                                           // 展开窗口与工作区边缘的最小间距（CSS px）
  const FOLD_MS = 180;                                             // 面板退场 0.16s 走完（已完全不可见）即可缩窗口
  const DOCK_THRESHOLD = 20;                                       // 悬浮球边缘距工作区边缘 ≤ 此值即贴边收起（CSS px）
  const DOCK_EXPOSE = 1 / 3;                                       // 贴边收起后仍露在屏幕内的比例（其余推到屏幕外）
  const DOCK_SETTLE_MS = 160;                                      // 拖动停手多久后判定吸附（OS 拖动期间 move 事件成串到达）
  const DRAG_POLL_MS = 60;                                         // 还按着鼠标时轮询「松手了没有」的间隔
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
  // 悬浮球在窗口内的绝对偏移（CSS px）：切换窗口时由 JS 更新，配合 holdOrb 隐藏保证不错位
  let orbOffset = $state({ x: ORB_INSET, y: ORB_INSET });
  // 悬停热区（窗口内偏移 + 尺寸，CSS px）：贴边时覆盖「露出的半颗」到「完整球」的并集，
  // 由 CSS :hover 触发球滑出/滑回；未贴边时就是球本身
  let orbZone = $state<ScreenRect>({ x: ORB_INSET, y: ORB_INSET, w: ORB_SIZE, h: ORB_SIZE });
  // true = 悬浮球强制隐藏（切窗口期间）；用 transition:none 立即生效，不能让它带过渡露脸
  let holdOrb = $state(false);
  let ballRect = $state<ScreenRect | null>(null);
  // 展开态窗口矩形（物理像素）：收起时与「当前窗口位置」作差，得到窗口被拖动的位移
  let expandedRect = $state<ScreenRect | null>(null);
  let foldTimer: ReturnType<typeof setTimeout> | null = null;
  let opening = false;
  let settling = false;   // 落位判定进行中（含「等松手」轮询）：防止 move 事件重复起一轮

  // 发送侧：一条推送从"等对方同意"到传完，全程只有这一张卡片。
  // 是个列表而不是单张：同时在推两个目标（或同一目标的两个文件）时，两张卡各自走完，
  // 不会被后来者顶掉 —— 顶掉等于把前一条的进度条凭空抹掉。
  type PushCard = {
    inviteId: string;
    target: string;      // 目标设备名
    name: string;        // 文件/目录名（多选时是容器名）
    /** 这次推了几项（>1 = 多选）。 */
    count: number;
    total: number;       // 总字节数
    received: number;
    phase: 'waiting' | 'sending' | 'done';
    /** 目标已绑定：对方**不需要点同意**，所以"等待对方同意"那一段只是短暂过场。 */
    auto: boolean;
  };
  let pushCards = $state<PushCard[]>([]);
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
  };
  let notices = $state<PushNotice[]>([]);
  // 铃铛：展开/收起右下角那组"等你确认"的气泡。收起 ≠ 已读 —— 角标照旧挂着。
  let notifOpen = $state(true);
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
  // Toast messages
  let toasts = $state<{ id: number; msg: string; leaving: boolean }[]>([]);
  let toastId = $state(0);
  // Context menu
  type CtxState = { x: number; y: number; kind: 'share' | 'device' | 'peer'; el?: HTMLElement; shareId?: string; path?: string; isDir?: boolean; deviceState?: string; deviceOnline?: boolean; deviceNeedsPair?: boolean; deviceId?: string; peerId?: string; peerName?: string; name?: string };
  let ctxState = $state<CtxState | null>(null);

  let connectedCount = $derived(getDevices().filter(d => d.connected).length);
  // 悬浮球左上角 / 铃铛的角标：这一组里还有几件事（通知 + 在途推送）。
  // 铃铛收起的不只是"通知"—— 发送方的进度卡也在同一组里，所以角标得一起算，
  // 否则会出现"收起后角标还在、但组里明明有东西"的错位。
  let notifCount = $derived(notices.length + pushCards.length);
  // 通知（面板右下角那组气泡）：未绑定的邀请、正在接收、已收到待确认、失败 —— **全都在
  // 这一组里**，形状统一（右对齐、2/3 面板宽，不占满）。要当场点的按钮（接收/拒绝/取消）
  // 就长在气泡里，所以它们不单独占一层。
  // 气泡组容器：新气泡总是贴在最下缘（就是面板右下角），所以每次都把它滚到底。
  let notifEl = $state<HTMLDivElement | null>(null);
  $effect(() => {
    void notices.length;
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
  // 本机内网地址（左下角「端口 · IP」）：多网卡时只显示首个，其余在悬停提示里列全 ——
  // 一块 footer 装不下三四个地址，但"我还有别的地址"必须让人看得见（手动添加设备要用）。
  let localAddrs = $state<{ primary: string | null; all: string[] }>({ primary: null, all: [] });
  let addrTitle = $derived(
    localAddrs.all.length > 1
      ? `本机有 ${localAddrs.all.length} 个内网地址（点一下复制第一个）：\n${localAddrs.all.join('\n')}`
      : '点一下复制「IP:端口」',
  );

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
      const dev = getDevices().find((d) => d.id === c.peerId)?.name ?? c.peerId;
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

  // BC 菜单的显示条件：右键项与比较位的组合决定文案与可用性
  let bcMenu = $derived.by(() => {
    const st = getBcStatus();
    const target = ctxState ? ctxBcTarget(ctxState) : null;
    const thisLeft = !!(target && st.left && sameBcTarget(st.left.target, target));
    const thisRight = !!(target && st.right && sameBcTarget(st.right.target, target));
    const otherLeft = !!(target && st.left && !thisLeft);           // 对手方在左侧
    const otherRight = !!(target && st.right && !thisRight);
    return {
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
   * 展开：先立即藏住悬浮球，再算方向/落位，一次 SetWindowPos 原子切换窗口，
   * 最后才让面板从悬浮球那一侧长出。悬浮球在切窗口期间绝不参与渲染，
   * 因此不存在「画在旧窗口坐标/旧偏移」的错位帧。
   */
  async function expand() {
    if (opening || expanded) return;
    opening = true;
    holdOrb = true;
    try {
      const sf = await appWindow.scaleFactor();
      const pos = await appWindow.outerPosition();
      const size = await appWindow.outerSize();
      const ball: ScreenRect = { x: pos.x, y: pos.y, w: size.width, h: size.height };
      const metrics = await orbScreenMetrics();
      const plan = planExpansion({
        ball,
        work: metrics.workArea,
        expanded: { w: Math.round(EXPANDED.width * sf), h: Math.round(EXPANDED.height * sf) },
        orbInset: Math.round(ORB_INSET * sf),
        margin: Math.round(WORK_MARGIN * sf),
      });

      if (foldTimer) { clearTimeout(foldTimer); foldTimer = null; }
      ballRect = ball;
      expandedRect = plan.window;
      const off = { x: plan.orbOffset.x / sf, y: plan.orbOffset.y / sf };
      orbOffset = off;
      orbZone = { ...off, w: ORB_SIZE, h: ORB_SIZE };
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

  /** 未贴边的折叠态：球居中内缩、热区就是球自己 */
  function resetOrbDom() {
    orbOffset = { x: ORB_INSET, y: ORB_INSET };
    orbZone = { x: ORB_INSET, y: ORB_INSET, w: ORB_SIZE, h: ORB_SIZE };
  }

  /**
   * 落位到吸附结果：先把窗口（连同球的裁剪边界）挪到位，再更新球偏移与热区 ——
   * 顺序不能反，否则球会在旧窗口里先滑一次、再随窗口跳一下。
   */
  async function applyDock(plan: DockPlan, sf: number) {
    await orbSetRect(plan.window);
    setDockDom(plan, sf);
  }

  /** 球偏移与悬停热区（窗口内 CSS px） */
  function setDockDom(plan: DockPlan, sf: number) {
    orbZone = { x: plan.zone.x / sf, y: plan.zone.y / sf, w: plan.zone.w / sf, h: plan.zone.h / sf };
    orbOffset = { x: plan.orb.x / sf, y: plan.orb.y / sf };
  }

  /** 吸附判定：自由落位的折叠态窗口 → 贴边收起大半颗（未贴边的轴保持原样） */
  function dockPlan(ball: ScreenRect, sf: number, work: ScreenRect): DockPlan {
    return planDock({
      ball,
      work,
      orbSize: Math.round(ORB_SIZE * sf),
      orbInset: Math.round(ORB_INSET * sf),
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
    try {
      await orbDragStart(info.pressX, info.pressY);
    } catch (e) {
      console.error('悬浮球起拖失败:', e);   // 起不来就是这一次拖不动，别把异常抛进事件处理里
    }
  }

  /**
   * 记录悬浮球落位（折叠态窗口左上角，物理像素）：下次启动回到这里（F-74）。
   *
   * 记的永远是**球**的落位，不是窗口的：折叠态窗口就是球；展开态窗口是面板，得先把球按
   * 窗口位移算回折叠态矩形（见 `rememberBallUnderPanel`）。调用点只在「几何落定之后」：
   * settleDrag（拖动结束、吸附判定跑完）与 settleCollapse（收起落位完）。
   */
  async function rememberOrbPos(rect: ScreenRect) {
    try {
      await orbSavePos(Math.round(rect.x), Math.round(rect.y));
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
      const plan = dockPlan({ x: pos.x, y: pos.y, w: size.width, h: size.height }, metrics.scaleFactor, metrics.workArea);
      if (plan.window.x !== pos.x || plan.window.y !== pos.y) await orbSetRect(plan.window);
      setDockDom(plan, metrics.scaleFactor);
      await rememberOrbPos(plan.window);
    } finally {
      settling = false;
    }
  }

  /**
   * 展开态拖动面板之后：算出球现在在哪并记下来。
   *
   * 与收起落位是同一套算法（`planCollapse` + `dockPlan`），所以这里记下的就是「此刻点收起，
   * 球会落到哪」—— 拖完面板直接关掉应用，下次启动也在那儿（否则记的还是展开前的老位置）。
   */
  async function rememberBallUnderPanel() {
    if (!ballRect || !expandedRect) return;   // 异常路径：没有展开前的矩形，算不出球在哪
    const metrics = await orbScreenMetrics();
    const free = planCollapse({ ball: ballRect, expanded: expandedRect, current: await appWindow.outerPosition() });
    await rememberOrbPos(dockPlan(free, metrics.scaleFactor, metrics.workArea).window);
  }

  /**
   * 收起落位：先把悬浮球原矩形按窗口位移平移（球贴回面板里它原来所在的那一角），
   * 再判吸附 —— 面板被拖到屏幕边上时，收起就是贴边收起半颗。
   */
  async function settleCollapse(ball: ScreenRect, expanded: ScreenRect | null) {
    const metrics = await orbScreenMetrics();
    const sf = metrics.scaleFactor;
    const free = expanded
      ? planCollapse({ ball, expanded, current: await appWindow.outerPosition() })
      : ball;   // 没记下展开矩形（异常路径）：按原样缩回
    const plan = dockPlan(free, sf, metrics.workArea);
    await applyDock(plan, sf);
    await rememberOrbPos(plan.window);   // 收起后的落位才是球下次该出现的地方
  }

  /**
   * 收起：面板先折叠淡出（此时窗口未变、球也没露脸），随后把窗口与球偏移一起切到
   * 吸附后的落位，切完才让悬浮球淡入 —— 悬浮球只在几何完全 settle 之后才可见。
   */
  function collapse() {
    expanded = false;
    showSettings = false;
    ctxState = null;
    picker = null;        // 选择框和右键菜单一样属于面板：收起即作废
    toasts = [];          // 提示是面板上的浮层：收起即作废，别等下次展开又冒出来
    holdOrb = true;
    if (foldTimer) clearTimeout(foldTimer);
    foldTimer = setTimeout(() => {
      foldTimer = null;
      const ball = ballRect;
      const expanded = expandedRect;
      ballRect = null;
      expandedRect = null;
      void (async () => {
        try {
          if (ball) await settleCollapse(ball, expanded);
          else resetOrbDom();   // 没记下展开矩形（异常路径）：回到未贴边的折叠态
        } catch (e) {
          console.error('收起悬浮球失败:', e);
        } finally {
          holdOrb = false;
        }
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

  /**
   * 发送卡的标题。
   *
   * 多选时**只说项数不说名字**：那个"名字"是分组用的标签（共同父目录名或「N 项」），
   * 而落盘时它并不参与路径（见 `manifest_for_paths`）—— 写成"正在发送 · 3 项（3 项）"
   * 是把同一件事说了两遍。
   */
  function pushCardTitle(c: PushCard): string {
    if (c.count > 1) {
      const what = `（${c.count} 项）`;
      return c.phase === 'done' ? `已发送${what}` : c.phase === 'sending' ? `正在发送${what}` : `等待对方同意${what}`;
    }
    if (c.phase === 'done') return `已发送 · ${c.name}`;
    if (c.phase === 'sending') return `正在发送 · ${c.name}`;
    return '等待对方同意';
  }

  /**
   * 点「推送」：直接开选择框（文件与文件夹混选、可跨目录累积）。
   *
   * 以前这里还有一层"先问推文件还是文件夹"——那是原生框的能力限制逼出来的：Windows 的
   * `IFileOpenDialog` 只有 `FOS_PICKFOLDERS` 一个开关（非此即彼），rfd 也只给
   * `pick_file(s) / pick_folder(s)`，没有混合模式。选择框自绘之后这一层就不需要了，
   * 用户可以先把一个文件勾上、再进另一层目录把整个文件夹勾上，一次推过去。
   */
  function requestPush(device: PeerDevice) {
    if (!device.online) {
      toast(`「${device.name}」不在网上，推送未发送`);
      return;
    }
    picker = {
      title: `推送到「${device.name}」`,
      sub: '文件与文件夹都可以选，双击文件夹进入，选中项跨目录累积；已选项目在底部角标里可查看、逐项取消或全部取消。对方同意后开始传输，多项合并成一条推送，只需同意一次。',
      confirmLabel: '推送',
      onConfirm: (items) => void pushPicked(items, device),
    };
  }

  function closePicker() {
    picker = null;
  }

  /** 推送确认：关框 → 把那批路径交给后端去送（取消时这里根本不会调到）。 */
  async function pushPicked(items: LocalPick[], device: PeerDevice) {
    closePicker();
    if (items.length === 0) return;
    try {
      // 多选合并成**一条**推送：对端一张卡、一次同意、一条进度。
      await pushPaths(items.map((i) => i.path), device.id);
    } catch (e) {
      toast(String(e));    // 对方离线等：就地失败，不排队（F-53）
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
   * 返回真正登记上的条数：坏路径（不存在、已被删）吞在这一层并进控制台，
   * 一条坏的没道理让整批一起消失。
   */
  async function addShares(paths: string[]): Promise<number> {
    const settled = await Promise.all(
      paths.map((p) =>
        addShare(p).then(
          () => true,
          (e) => {
            console.error('添加共享项失败:', p, e);
            return false;
          },
        ),
      ),
    );
    const added = settled.filter(Boolean).length;
    if (added === 0) toast('没有添加任何共享项');
    else toast(`已添加 ${added} 个共享项`);
    return added;
  }

  async function cancelSending(inviteId: string) {
    try { await cancelPush(inviteId); } catch (e) { toast(String(e)); }
  }

  function dropNotice(inviteId: string) {
    notices = notices.filter((n) => n.inviteId !== inviteId);
  }

  /** 气泡上的「知道了」：只撤这一条 —— 悬浮球与铃铛上的数字跟着减。 */
  function confirmNotice(inviteId: string) {
    dropNotice(inviteId);
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
      return '接受后两台设备互为「已配对」（互相交换凭据），之后就能建立长连接、互相推送；60 秒内未处理自动作废。';
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

  /** 同意接收：卡片立刻转成"正在接收"，进度随后由 push://progress 推进。 */
  async function acceptNotice(n: PushNotice) {
    try {
      await acceptPush(n.inviteId);
    } catch (e) {
      toast(String(e));
      dropNotice(n.inviteId);
      return;
    }
    notices = notices.map((x) => (x.inviteId === n.inviteId ? { ...x, kind: 'receiving' } : x));
  }

  async function rejectNotice(n: PushNotice) {
    dropNotice(n.inviteId);
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
    if (device) { void issuePairCode(device.id, device.name); return; }
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
      toast(`已添加设备「${device.name}」`);
      await issuePairCode(device.id, device.name);
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
      toast('配对失败，请核对配对码');
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
  function closeSettings() { showSettings = false; }

  function hideCtx() { ctxState = null; }

  async function ctxDownload() {
    const c = ctxState;
    if (!c) return;
    toast('已开始下载到下载目录');
    hideCtx();
  }

  async function ctxOpen() {
    hideCtx();
    toast('已在访达中打开所在文件夹');
  }

  /** 对端文件下载：走后端命令（token 取凭据库、落盘按下载目录设置都在 Rust 侧）。 */
  async function ctxPeerDownload() {
    const c = ctxState;
    hideCtx();
    if (c?.kind !== 'peer' || !c.peerId || !c.shareId) return;
    try {
      const saved = await downloadFromPeer(c.peerId, c.shareId, c.path ?? '', c.name);
      toast(`已下载到 ${saved}`);
    } catch (e) {
      toast(`下载失败：${e}`);
    }
  }

  function ctxCopy() {
    hideCtx();
    toast('已复制共享路径');
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
      toast('正在建立长连接，拖入文件将自动广播');
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

  /** 右键「推送」：与设备行同一个入口（同一个文件选择框）。 */
  function ctxPush() {
    const deviceId = ctxState?.deviceId;
    hideCtx();
    const device = deviceId ? getDevices().find((d) => d.id === deviceId) : undefined;
    if (device) requestPush(device);
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
    // 折叠态窗口 = 悬浮球 + 四周余量：尺寸在这里归位，**位置**由后端在首帧之前按上次的
    // 落位写好（lib.rs `restore_orb_pos`）—— 等界面起来再挪会看到球从屏幕中央跳过去。
    // 位置复原之后还得按同一个矩形重算贴边形态：上次是贴边收起的话，球要滑到被窗口裁掉
    // 的那一位、热区跟着贴边，否则球会以「未贴边」的样子完整站在屏幕边上（而窗口已经贴住
    // 边缘，看上去像被谁挪出来了一截）。这正是 `settleDrag` 对落位矩形做的事，直接复用。
    void (async () => {
      try {
        await appWindow.setSize(COLLAPSED);
        await settleDrag();
      } catch (e) {
        console.error('复原悬浮球落位失败:', e);
      }
    })();

    // Init data + server
    void (async () => {
      try {
        await Promise.all([loadSettings(), loadDevices(), loadBcStatus()]);
        const port = await getActualPort();
        if (port === 0) {
          const p = await startHttpServer();
          setServerPort(p);
        } else {
          setServerPort(port);
        }
        await loadShares();
        // 左下角「端口 · 本机 IP」：内网地址启动时取一次 —— 换网/插网线属于罕见情形，
        // 重启 App 就刷新了，不值得为它在界面上常驻一个网卡监听。
        localAddrs = await localAddresses();
      } catch (e) { console.error('Init failed:', e); }
    })();

    // Tauri drag-drop
    const unlisten = appWindow.onDragDropEvent((event) => {
      if (event.payload.type === 'over') {
        dragging = true;
      } else if (event.payload.type === 'leave') {
        dragging = false;
      } else if (event.payload.type === 'drop') {
        dragging = false;
        const paths = event.payload.paths;
        if (paths.length > 0) void addShares(paths);
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
        from: inv.sender.name,
        name: inv.name,
        size: inv.size,
        isDir: inv.isDir,
        count: inv.count ?? 1,
        received: 0,
      }];
    });

    // 邀请已发出：等对方同意（长连接与 HTTP 两条通道都是这一条事件）。
    // 目标已绑定的话对方不会问人（后端直接收），所以这张卡上的"等待对方同意"只是一瞬。
    const unPushSent = listen<{ inviteId: string; peerId: string; target: string; name: string; total: number; count: number }>('push://sent', (ev) => {
      const p = ev.payload;
      const auto = getDevices().find((d) => d.id === p.peerId)?.paired ?? false;
      pushCards = [...pushCards, { inviteId: p.inviteId, target: p.target, name: p.name, total: p.total, received: 0, phase: 'waiting', count: p.count ?? 1, auto }];
    });

    const unPushResponse = listen<{ inviteId: string; accepted: boolean; reason?: string | null }>('push://response', (ev) => {
      const { inviteId, accepted, reason } = ev.payload;
      const notice = notices.find((n) => n.inviteId === inviteId);
      const card = pushCards.find((c) => c.inviteId === inviteId);
      if (accepted) {
        if (notice) notices = notices.map((n) => (n.inviteId === inviteId ? { ...n, kind: 'receiving' } : n));
        if (card) {
          pushCards = pushCards.map((c) => (c.inviteId === inviteId ? { ...c, phase: 'sending' } : c));
          // 已绑定设备那条路上没有"对方同意"这一步，说"已同意"会让人以为对面点过。
          toast(card.auto ? `开始向「${card.target}」传输` : `「${card.target}」已同意接收，开始传输`);
        }
        return;
      }
      // 对方拒绝 / 60s 没人处理而作废 / 已绑定设备**自动收下时本机放行失败**：本机挂着
      // 的那条通知必须撤掉，否则它会一直留在那儿，点「接收」才发现邀请早就没了（后面那种
      // 情况下 kind 已经是 'receiving' —— 后端这时补一条 accepted:false 回来）。
      // 发送方那张进度卡由随后的 push://done 收尾（两条通道都会跟一条）。
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
      // 发送侧：进度条。
      pushCards = pushCards.map((c) => (c.inviteId === inviteId ? { ...c, received, total, phase: 'sending' } : c));
    });

    const unPushDone = listen<{ inviteId: string; status: string; reason?: string | null; dest?: string }>('push://done', (ev) => {
      const { inviteId, status, reason, dest } = ev.payload;
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
      const card = pushCards.find((c) => c.inviteId === inviteId);
      if (card) {
        if (status === 'completed') {
          pushCards = pushCards.map((c) => (c.inviteId === inviteId ? { ...c, phase: 'done', received: c.total } : c));
          toast(`「${card.name}」已发送完成`);
          setTimeout(() => { pushCards = pushCards.filter((c) => c.inviteId !== inviteId); }, 3000);
        } else {
          toast(status === 'cancelled' ? '已取消推送' : (reason ?? '推送失败'));
          pushCards = pushCards.filter((c) => c.inviteId !== inviteId);
        }
      }
    });

    const unPaired = listen('devices://changed', () => {
      loadDevices().catch(console.error);
    });

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
        const name = ps.peerName ?? ev.payload.peerName ?? '设备';
        closePair();
        toast(`已与「${name}」配对成功（点「连接」建立长连接）`);
      }
    });

    // 收到一条配对邀请 → 挂一条通知（带「接收/拒绝」），悬浮球左上角的数字 +1。
    // **不主动弹面板**：与推送邀请同一套约定（F-51），人来点那颗球。
    const unPairInvite = listen<{ inviteId: string; peerId: string; peerName: string }>('pair://invite', (ev) => {
      const p = ev.payload;
      notices = [...notices, {
        inviteId: p.inviteId,
        kind: 'pair',
        peerId: p.peerId,
        from: p.peerName,
        name: '',
        size: 0,
        isDir: false,
        count: 1,
        received: 0,
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
      unPushSent.then(fn => fn());
      unPushResponse.then(fn => fn());
      unPushProgress.then(fn => fn());
      unPushDone.then(fn => fn());
      unPaired.then(fn => fn());
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
  class:drag={dragging}
  class:hold={holdOrb}
  style="--panel-slack: {PANEL_SLACK}px; --orb-inset: {ORB_INSET}px; --foot-h: {footH}px; --notif-max-h: {notifMaxH}px; --orb-x: {orbOffset.x}px; --orb-y: {orbOffset.y}px; --zone-x: {orbZone.x}px; --zone-y: {orbZone.y}px; --zone-w: {orbZone.w}px; --zone-h: {orbZone.h}px"
>
  <!-- 悬停热区：贴边收起半颗后，指针落在热区（露出的半颗 ~ 完整球那一带）就把球滑出来 -->
  <div class="orb-zone">
    <Orb
      badgeCount={connectedCount}
      notifCount={notifCount}
      expanded={expanded}
      onToggle={() => setOpen(!expanded)}
      onDragStart={beginOrbDrag}
    />
  </div>

  <!-- Main Panel -->
  <section class="panel" aria-label="OrbDrop 主面板">
    <!-- `deep`：Tauri 的拖拽脚本只在**命中元素自己**带属性时才拖（`composedPath[0]`），
         logo/文字这些子元素会把 mousedown 从 header 上挡掉，于是只有空隙能拖。`deep` 让整棵
         子树里的非可点击元素都触发拖拽；按钮仍照旧拦截（脚本先判 clickable 再看属性）。 -->
    <header class="panel-head" data-tauri-drag-region="deep">
      <span class="panel-brand"><Logo size={20} />OrbDrop</span>
      <div class="panel-head-right">
        <span class="ch-inst">
          <span class="dot ok"></span>
          <span>在线 · 长连接 {connectedCount}</span>
        </span>
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
      <TabContent tab={activeTab} devices={getDevices()} onToast={toast} onRequestPush={requestPush} onAddShare={requestAddShare} onOpenPair={openPair} onUnpair={(d) => void unpairDevice(d.id)} />
    </div>

    <footer class="panel-foot" bind:this={footEl}>
      <span class="foot-left">
        <span class="num">端口 {serverPortVal || 3890}</span>
        {#if localAddrs.primary}
          <!-- 本机内网 IP：多网卡时这里只显示首个，悬停列出全部（跨网段手动添加要用） -->
          <button class="foot-addr num" onclick={copyAddr} title={addrTitle}>{localAddrs.primary}</button>
        {/if}
      </span>
      <span class="foot-right">
        <span>OrbDrop v0.1.0</span>
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
           已收到待确认、接收失败，外加**发送侧的进度卡**（"等待对方同意"要等很久，
           它也是右下角的一件事）—— 全都同一形状：右对齐、2/3 面板宽（不占满）。
           来一条甩入一条、收起时整组甩回右侧（`fly`）；footer 最右侧的铃铛整体收起/展开它，
           **收起 ≠ 已读**（角标照旧挂着，与悬浮球左上角黄标同一个数）。 -->
      {#if notifOpen && (notices.length > 0 || pushCards.length > 0)}
        <div class="notif-group" bind:this={notifEl}>
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
                  <button class="btn btn-secondary btn-sm" onclick={() => confirmNotice(n.inviteId)}>知道了</button>
                </div>
              {/if}
            </div>
          {/each}

          <!-- 发送侧：每条在途的推送一张卡 —— 和通知同一个形状、同一组、同一个铃铛管，
               因为它也是"右下角的一件事"（"等待对方同意"可以等很久；已绑定目标那一档
               只是一瞬）。它排在通知前面：通知贴最下缘（要你看一眼的事优先）。 -->
          {#each pushCards as c (c.inviteId)}
            <div
              class="notif-bubble"
              class:done={c.phase === 'done'}
              in:fly={{ x: 32, duration: MOTION, easing: cubicOut }}
              out:fly={{ x: 32, duration: MOTION }}
            >
              <div class="p-top">
                <span class="nb-title">{pushCardTitle(c)}</span>
                <span class="nb-size num">{pushPercent(c.received, c.total)}%</span>
              </div>
              <div class="p-bar"><i style="width:{pushPercent(c.received, c.total)}%"></i></div>
              <div class="p-meta">
                <span>→「{c.target}」· 局域网</span>
                <span class="num">{fmtBytes(c.received)} / {fmtBytes(c.total)}</span>
              </div>
              {#if c.phase !== 'done'}
                <div class="nb-acts">
                  <button class="btn btn-secondary btn-sm" onclick={() => void cancelSending(c.inviteId)}>取消</button>
                </div>
              {/if}
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
    peerName={ctxState.peerId ? getDevices().find((d) => d.id === ctxState?.peerId)?.name : undefined}
    bcLeftLabel={bcMenu.leftLabel}
    bcRightLabel={bcMenu.rightLabel}
    bcCompareLabel={bcMenu.compareLabel}
    bcCompareEnabled={bcMenu.compareEnabled}
    bcThisStaged={bcMenu.thisStaged}
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
    onPush={ctxPush}
    onPeerDownload={ctxPeerDownload}
    onBcCompare={ctxBcCompare}
    onBcAddLeft={() => ctxBcPlace('left')}
    onBcAddRight={() => ctxBcPlace('right')}
    onBcClear={ctxBcClear}
  />
{/if}
