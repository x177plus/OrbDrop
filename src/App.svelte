<script lang="ts">
  import { getCurrentWebviewWindow } from '@tauri-apps/api/webviewWindow';
  import { LogicalSize } from '@tauri-apps/api/dpi';
  import { onMount } from 'svelte';
  import { listen } from '@tauri-apps/api/event';
  import { getDevices, loadDevices } from '$lib/stores/devices.svelte';
  import { getServerPort, setServerPort, loadShares, addShare, removeShare, getShares } from '$lib/stores/shares.svelte';
  import { loadSettings } from '$lib/stores/settings.svelte';
  import { getBcStatus, loadBcStatus, setBcSide, clearSide, compare } from '$lib/stores/bc.svelte';
  import { getActualPort, startHttpServer, acceptPush, rejectPush, orbScreenMetrics, orbSetRect, orbMouseDown, orbSavePos, addPeer, pairStart, pairConfirm, connectPeer, disconnectPeer, removePeer, downloadFromPeer } from '$lib/api';
  import type { PushInvite, PeerDevice, TabKey, BcTarget } from '$lib/types';
  import { planExpansion, planCollapse, planDock, type DockPlan, type ScreenRect } from '$lib/orbLayout';
  import Orb from '$lib/components/Orb.svelte';
  import TabBar from '$lib/components/TabBar.svelte';
  import TabContent from '$lib/components/TabContent.svelte';
  import SettingsPanel from '$lib/components/SettingsPanel.svelte';
  import ContextMenu from '$lib/components/ContextMenu.svelte';

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

  // Push card state (sending progress)
  let pushCardState = $state<{ title: string; target: string; total: string; progress: number; done: boolean; received: number } | null>(null);
  // Consent card state (incoming push)
  let consentState = $state<{ from: string; name: string; size: string; bytes: number } | null>(null);
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
  // Push invitation overlay (real incoming)
  let pushInvite = $state<PushInvite | null>(null);

  let connectedCount = $derived(getDevices().filter(d => d.connected).length);
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

  // Push progress card (simulation when backend progress events absent)
  function startPush(name: string, target: string, total: string) {
    pushCardState = { title: `正在发送 · ${name}`, target, total, progress: 0, done: false, received: 0 };
    const totalMB = parseFloat(total);
    const totalBytes = total.indexOf('M') > -1 ? totalMB * 1024 * 1024 : totalMB * 1024;
    let received = 0;
    const interval = setInterval(() => {
      received += totalBytes * 0.15;
      const pct = Math.min(100, Math.round((received / totalBytes) * 100));
      pushCardState = { ...pushCardState!, progress: pct, received, done: pct >= 100 };
      if (pct >= 100) {
        clearInterval(interval);
        pushCardState = { ...pushCardState!, title: `已发送完成 · ${name}`, done: true };
        toast(`「${name}」已成功发送至 ${target}`);
        setTimeout(() => { pushCardState = null; }, 3000);
      }
    }, 500);
  }

  function requestPush(device: PeerDevice) {
    if (device.connected) {
      startPush('项目设计稿', device.name, '12.6 MB');
    } else {
      toast(`已向「${device.name}」发起推送，等待对方同意`);
      consentState = { from: device.name, name: '项目设计稿', size: '12.6 MB', bytes: 12.6 * 1024 * 1024 };
    }
  }

  function acceptConsent() {
    if (!consentState) return;
    const target = consentState.from;
    consentState = null;
    toast(`「${target}」已同意接收，开始传输`);
    startPush('项目设计稿', target, '12.6 MB');
  }

  function declineConsent() {
    if (!consentState) return;
    toast(`「${consentState.from}」拒绝了接收`);
    consentState = null;
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

  function ctxPush() {
    hideCtx();
    toast('已向该设备发起推送');
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

  async function handleAcceptPush() {
    if (!pushInvite) return;
    try {
      await acceptPush(pushInvite.inviteId);
      toast(`正在接收「${pushInvite.name}」`);
    } catch (e) { console.error('Accept failed:', e); }
    pushInvite = null;
  }

  async function handleRejectPush() {
    if (!pushInvite) return;
    try { await rejectPush(pushInvite.inviteId); } catch (e) { /* ignore */ }
    pushInvite = null;
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
        if (paths.length > 0) {
          for (const p of paths) {
            addShare(p).catch((e) => console.error('添加共享项失败:', e));
          }
          const connected = getDevices().filter(d => d.connected);
          if (connected.length > 0) {
            toast(`已拖入文件并自动广播至 ${connected.map(d => d.name).join('、')}`);
          } else {
            toast('已添加共享项');
          }
        }
      }
    });

    // 拖动结束 → 判定吸附。OS 拖动期间 move 事件成串到达（且可能在拖动循环结束后才处理），
    // 所以停手 DOCK_SETTLE_MS 之后才算一次，避免每帧都发 SetWindowPos。
    let moveTimer: ReturnType<typeof setTimeout> | null = null;
    const unMoved = appWindow.onMoved(() => {
      if (moveTimer) clearTimeout(moveTimer);
      moveTimer = setTimeout(() => { moveTimer = null; void settleDrag(); }, DOCK_SETTLE_MS);
    });

    const unPushInvite = listen<PushInvite>('push://invite', (ev) => {
      if (!expanded) setOpen(true);
      pushInvite = ev.payload;
    });

    const unPaired = listen('devices://changed', () => {
      loadDevices().catch(console.error);
    });

    // 用户又点了一次应用图标 / 启动了第二份进程：把面板展开，而不是让新进程悄悄消失。
    const unOrbShow = listen('orb://show', () => {
      setOpen(true);
    });

    // 对方输入了本机显示的配对码 → 本机验证通过、给出 token（list 由 devices://changed 刷新）
    const unPairDone = listen<{ peerId: string; peerName?: string | null }>('pair://completed', (ev) => {
      if (pairConfirming) return;   // 本机自己发起的确认，toast 由 confirmPair 给出
      const ps = pairState;
      if (ps?.peerId === ev.payload.peerId) {
        const name = ps.peerName ?? ev.payload.peerName ?? '设备';
        closePair();
        toast(`已与「${name}」配对成功（点「连接」建立长连接）`);
      }
    });

    const unProgress = listen<{ invite_id: string; received: number; total: number }>('push://progress', (ev) => {
      const { received, total } = ev.payload;
      if (total <= 0) return;
      const pct = Math.min(100, Math.round((received / total) * 100));
      if (pushCardState) {
        pushCardState = { ...pushCardState, progress: pct, received, done: pct >= 100 };
        if (pct >= 100) {
          pushCardState = { ...pushCardState, title: '已发送完成', done: true };
          setTimeout(() => { pushCardState = null; }, 3000);
        }
      }
    });

    // Context menu from child components
    const handleCtx = (e: Event) => {
      const detail = (e as CustomEvent).detail as any;
      if (detail && detail.kind) {
        ctxState = { x: detail.x, y: detail.y, el: detail.el, kind: detail.kind, shareId: detail.shareId, path: detail.path, isDir: detail.isDir, deviceState: detail.deviceState, deviceOnline: detail.online, deviceNeedsPair: detail.needsPair, deviceId: detail.deviceId, peerId: detail.peerId, name: detail.name };
      }
    };
    document.addEventListener('od:ctx', handleCtx);

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
      unPaired.then(fn => fn());
      unOrbShow.then(fn => fn());
      unProgress.then(fn => fn());
      document.removeEventListener('od:ctx', handleCtx);
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
  style="--panel-slack: {PANEL_SLACK}px; --orb-inset: {ORB_INSET}px; --orb-x: {orbOffset.x}px; --orb-y: {orbOffset.y}px; --zone-x: {orbZone.x}px; --zone-y: {orbZone.y}px; --zone-w: {orbZone.w}px; --zone-h: {orbZone.h}px"
>
  <!-- 悬停热区：贴边收起半颗后，指针落在热区（露出的半颗 ~ 完整球那一带）就把球滑出来 -->
  <div class="orb-zone">
    <Orb
      badgeCount={connectedCount}
      expanded={expanded}
      onToggle={() => setOpen(!expanded)}
    />
  </div>

  <!-- Main Panel -->
  <section class="panel" aria-label="OrbDrop 主面板">
    <header class="panel-head" data-tauri-drag-region>
      <span class="grip" aria-hidden="true"><i></i><i></i><i></i><i></i><i></i><i></i></span>
      <span class="panel-brand"><span class="mark-sm"><i></i></span>OrbDrop</span>
      <div class="panel-head-right">
        <span class="ch-inst">
          <span class="dot ok"></span>
          <span>在线 · 长连接 {connectedCount}</span>
        </span>
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

    <div class="panel-body">
      <TabContent tab={activeTab} devices={getDevices()} onToast={toast} onRequestPush={requestPush} onOpenPair={openPair} onUnpair={(d) => void unpairDevice(d.id)} />
    </div>

    <footer class="panel-foot">
      <span class="num">端口 {serverPortVal || 3890}</span>
      <span>OrbDrop v0.1.0</span>
    </footer>

    <!-- Push card (sending progress) -->
    {#if pushCardState}
      <div class="push-card show" class:done={pushCardState.done}>
        <div class="p-top">
          <span>{pushCardState.title}</span>
          <span class="p-per num">{pushCardState.progress}%</span>
        </div>
        <div class="p-bar"><i style="width:{pushCardState.progress}%"></i></div>
        <div class="p-meta">
          <span>→ {pushCardState.target} · 局域网</span>
          <span class="num">{(pushCardState.received / 1024 / 1024).toFixed(1)} / {pushCardState.total}</span>
        </div>
      </div>
    {/if}

    <!-- Consent card (incoming push) -->
    {#if consentState}
      <div class="push-card show" id="consentCard">
        <div class="p-top">
          <span>接收推送邀请</span>
          <span class="mono">来自 {consentState.from}</span>
        </div>
        <div class="consent-info">
          <span class="f-icon">
            <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6"><path d="M6 3h8l5 5v13H6V3Z"/></svg>
          </span>
          <span class="c-name">{consentState.name}</span>
          <span class="c-size num">{consentState.size}</span>
        </div>
        <div class="consent-acts">
          <button class="btn btn-secondary" onclick={declineConsent}>拒绝</button>
          <button class="btn btn-primary" onclick={acceptConsent}>同意</button>
        </div>
      </div>
    {/if}

    <!-- Settings sheet -->
    <SettingsPanel open={showSettings} onClose={closeSettings} onToast={toast} />

    <!-- Pair dialog overlay -->
    {#if pairState}
      <div class="overlay open" id="pairDialog">
        <div class="dialog">
          {#if pairState.peerId}
            <h3>与 {pairState.peerName} 配对</h3>
            <p class="d-desc">在对方设备的 OrbDrop 中输入下方配对码，即可完成配对。</p>
            <p class="d-target num">输入到「{pairState.peerName}」的 OrbDrop</p>
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
              <label for="pairCodeInput">或：输入对方设备上显示的配对码</label>
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
              <button class="btn btn-secondary" onclick={closePair}>完成</button>
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

    <!-- Push invite dialog (real incoming) -->
    {#if pushInvite}
      <div class="overlay open">
        <div class="dialog">
          <h3>收到推送</h3>
          <p class="d-desc">
            <strong>{pushInvite.sender.name}</strong> 发来：{pushInvite.name}
            ({pushInvite.isDir ? '目录' : `${(pushInvite.size / 1024 / 1024).toFixed(1)} MB`})
          </p>
          <div class="d-acts">
            <button class="btn btn-secondary" onclick={handleRejectPush}>拒绝</button>
            <button class="btn btn-primary" onclick={handleAcceptPush}>同意</button>
          </div>
        </div>
      </div>
    {/if}

    <!-- Toast layer：提示属于面板（提示出现时收起面板，它就随面板一起退场）。
         放在 panel 里而不是顶层：面板收起时 opacity:0，提示结构上就不可能画进那个 90px 的悬浮球窗口。 -->
    <div class="toast-layer">
      {#each toasts as t (t.id)}
        <div class="toast" class:leaving={t.leaving}>
          <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round">
            <path d="M12 3v12M7 10l5 5 5-5M5 19h14"/>
          </svg>
          {t.msg}
        </div>
      {/each}
    </div>
  </section>
</aside>

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
