<script lang="ts">
  import { tick, untrack, onDestroy } from 'svelte';
  import { dragOut } from '$lib/dragOut';
  import { RECT_CARD_W } from '$lib/orbShape';
  import Logo from '$lib/components/Logo.svelte';
  import type { OrbDragInfo, RectPage } from '$lib/types';

  /**
   * 圆角矩形悬浮框（`design/orbdrop-rect-orb.html`）：收起态的另一种形态。
   *
   * 尺寸、间距、时长、缓动逐值照原型：卡片 176×134、两列列表可视 2.5 行、右端渐隐代替省略号、
   * 右侧分页点列、翻页时整卡"被抽出/被扫过"的 400ms 动画、角标随翻页轻微呼吸。
   *
   * 分页：**第一页永远是本机**（本机的共享项），其后每台长连接设备一页；只有一页时不画右边的
   * 分页点列（没得翻）——但那一列的位置照旧占着：收起态是定尺（`orbShape.ts` 的 `rectLayout`），
   * 窗口宽度不随设备接入/断开变化。
   *
   * 与圆球（`Orb.svelte`）的分工：
   *  - 内容格 = 拖出源（原生 OLE 拖出，复用 `drag.rs` 那条路；本机项给磁盘真实路径、对端项给
   *    镜像里的真实路径，字节等目标读了才拉）；**单击不做事**，**双击** = 目录钻进去（整张列表换成那一层的内容）、文件打开
   *    （策略在 App 的 `activateRectSlot`）；**右键 = 那一格的菜单**（本机页最外那一层的共享项
   *    才有，见 `onContext`）；「↰ 上一级」那格没有菜单 —— 它是出口，不是某一层里的东西；
   *  - header / 分页点 / 卡内空白 = 窗口拖动（与 `Orb.svelte` 同一套手动拖拽：位移过阈值
   *    才交给 OS 拖窗口，否则松手当单击 = 展开/收起面板）；
   *  - 三种意图靠**空间与手势**分开：内容格（含「↰ 上一级」那格）不参与移窗、也不吃单击
   *    （按下就 `stopPropagation`），拖动它们只会拖出/无动作；要移窗就抓 header、点列或空白。
   *  - 右键菜单开着时，**第一下按下只关菜单**：那一下由菜单自己在捕获阶段吃掉（`ContextMenu`），
   *    卡片这边收不到、也就不会顺手展开面板或起拖。
   *
   * 形状本身不在这里算：窗口多大、悬浮框在窗口里的哪一块由 `orbShape.ts` 给，`App.svelte`
   * 换算成 CSS 变量传下来（与圆球共用同一套变量）。
   */
  let {
    pages = [],
    deviceCount = 0,
    notifCount = 0,
    expanded = false,
    docked = false,
    onToggle,
    onDragStart,
    onActivate,
    onUp,
    onContext,
    onToast,
  }: {
    /** 第一页永远是本机；空 = 数据还没到位（渲染一张本机空态卡）。 */
    pages?: RectPage[];
    deviceCount?: number;
    /** 左上角那颗待处理数（面板里那组通知 —— **不含本机在途的推送卡** —— **加上会话未读与还没确认的群邀请**）。 */
    notifCount?: number;
    expanded?: boolean;
    /** 贴边收起态（`App` 的 `rectDock !== 0`）：卡片已滑出窗口、露在屏幕边上的换成那条竖条。
     *  竖条的显隐全在 CSS 里（`.orb-stage.docked`），这里只用来**让两枚角标放大一次**。 */
    docked?: boolean;
    /** 单击卡内空白：展开 / 收起面板 */
    onToggle: () => void;
    /** 起拖：位移过阈值，把窗口拖动交给 OS（`App.beginOrbDrag`） */
    onDragStart: (info: OrbDragInfo) => void;
    /** 双击内容格（第几页的第几格）：目录钻进去、文件打开 —— 策略在 App 的 `activateRectSlot` */
    onActivate: (pageIdx: number, slotIdx: number) => void;
    /** 双击「↰ 上一级」那一格、或按 Esc：回上一层（`App.upRect`） */
    onUp: (pageIdx: number) => void;
    /**
     * 右键**内容格**（第几页的第几格 + 指针在窗口里的坐标）：菜单长不长、长什么，全在 App 的
     * `openRectCtx` —— 卡片只负责说"哪一格被右键了"（它不知道共享项、也不知道折叠窗口只有
     * 236×162 那点地方）。「↰ 上一级」那格、空态与卡内空白都不报（它们不是某一层的条目）。
     */
    onContext: (pageIdx: number, slotIdx: number, x: number, y: number) => void;
    /** 拖出准备/失败时的提示 */
    onToast: (msg: string) => void;
  } = $props();

  const ICON_FOLDER = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linejoin="round"><path d="M3 7a2 2 0 0 1 2-2h4.2a2 2 0 0 1 1.4.6L12 7h7a2 2 0 0 1 2 2v9a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V7Z"/></svg>';
  const ICON_FILE = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6"><path d="M6 3h8l5 5v13H6V3Z"/></svg>';
  const ICON_EMPTY = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><rect x="4" y="3" width="16" height="14" rx="2"/><path d="M8 21h8M12 17v4"/></svg>';
  /** 「↰ 上一级」那格的图标：朝上的箭头 —— 与文件夹/文件靠**形状**分开（卡上的彩色只有
   *  黄/橙/绿三支，都各有含义，不为此再加一支）。 */
  const ICON_UP = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="M12 19V6M6 12l6-6 6 6"/></svg>';

  /** 数据还没到位时的一张本机空态卡（`App` 一上来就会给出本机页，这里只是兜底）。 */
  const FALLBACK_PAGE: RectPage = { id: 'local', kind: 'local', name: 'OrbDrop', ip: '', loading: false, error: null, dir: null, up: null, items: [] };

  /** 与 `main.css` 的 `--rq-flip` 同一个数：整卡翻页动画的时长。 */
  const FLIP_MS = 400;
  /** 滚轮累积到这么多像素才翻页（原型同值）：先让列表自己滚，到底/到顶才换页。 */
  const FLIP_AT = 40;
  /** 一次翻页后多久才接下一次滚轮翻页（原型同值）：免得滚轮一甩飞过去好几页。 */
  const LOCK_MS = 520;
  const DRAG_THRESHOLD_PX = 4;
  const REDUCED = typeof matchMedia === 'function' && matchMedia('(prefers-reduced-motion: reduce)').matches;

  let wrapEl = $state<HTMLElement | null>(null);
  let dotListEl = $state<HTMLElement | null>(null);
  let devBadgeEl = $state<HTMLElement | null>(null);
  let alertBadgeEl = $state<HTMLElement | null>(null);
  /** 卡片 / 列表滚动容器 / 分页点：`{#each}` 里按页下标 bind:this，只读不参与渲染。 */
  const cardEls: (HTMLElement | undefined)[] = [];
  const scrollers: (HTMLElement | undefined)[] = [];
  const dotEls: (HTMLButtonElement | undefined)[] = [];

  /** 当前页（用 id 而不是下标：设备列表会随长连接增减/重排，下标会指到另一台设备去）。 */
  let activeId = $state<string | null>(null);
  /** 翻页动画的两段状态：`entering` = 新卡还在起点（`from-*`），`leaving` = 旧卡正被扫过。 */
  let entering = $state<{ idx: number; dir: number } | null>(null);
  let leaving = $state<{ idx: number; dir: number } | null>(null);
  let dotShift = $state(0);

  /** 非响应式：拖拽/翻页手势的现场，模板不读它（读了会跟着每帧重渲染）。 */
  let downX = 0;
  let downY = 0;
  let pressed = false;
  let moved = false;
  let started = false;
  let pendingDot = -1;
  let flipTimer: ReturnType<typeof setTimeout> | null = null;
  /** 翻页后的那一次"别再接着翻"的锁：也留个句柄，销毁时一并收掉（见 `onDestroy`）。 */
  let lockTimer: ReturnType<typeof setTimeout> | null = null;
  let acc = 0;
  let lastDir = 0;
  let locking = false;
  let rafId: number | null = null;
  /** 每一页列表的滚动目标（缓动滚动的落点）；`scrollers` 与它同下标。 */
  const targets: number[] = [];
  let rowStep = 33;   // 行高 + 行距，挂载时从 CSS 变量读（原型同样是从 `--rq-row-*` 算）
  let fallback = 0;   // 当前页被撤掉时（设备断开）按它就近落位

  const shown = $derived(pages.length === 0 ? [FALLBACK_PAGE] : pages);
  const idx = $derived.by(() => {
    const i = pages.findIndex((p) => p.id === activeId);
    return i >= 0 ? i : 0;
  });
  /** 分页点的透明度：越远越淡（原型同一条梯度），当前页最实。 */
  const dotOpacities = $derived.by(() =>
    pages.map((_, i) => {
      const dist = Math.abs(i - idx);
      return dist === 0 ? 1 : dist === 1 ? 0.6 : dist === 2 ? 0.34 : dist === 3 ? 0.16 : 0.07;
    }),
  );
  const activePage = $derived(shown[idx] ?? FALLBACK_PAGE);

  // 页数变化（设备接入/断开）：当前页没了就就近落位；顺带把被裁的名字重新打标。
  $effect(() => {
    const ids = pages.map((p) => p.id);
    if (ids.length > 0) {
      if (activeId === null || !ids.includes(activeId)) {
        activeId = ids[Math.min(fallback, ids.length - 1)];
      } else {
        fallback = ids.indexOf(activeId);
      }
    }
    void tick().then(markClipped);
  });

  // 分页点列整体上下居中（`.rq-dotlist` 用 transform 平移，不做滚动）
  $effect(() => {
    void idx;
    void pages.length;
    void tick().then(centerDots);
  });

  // 行高 + 行距从 CSS 变量读（原型同样是从 `--rq-row-*` 算）：键盘上下键一次滚一行。
  $effect(() => {
    const el = wrapEl;
    if (!el) return;
    const cs = getComputedStyle(el);
    rowStep =
      (parseFloat(cs.getPropertyValue('--rq-row-h')) || 30) +
      (parseFloat(cs.getPropertyValue('--rq-row-gap')) || 3);
  });

  // 列宽跟着字号/DPI 变（窗口尺寸是定尺，不再随内容改）：被裁的名字要重新打标。
  $effect(() => {
    globalThis.addEventListener('resize', markClipped);
    return () => globalThis.removeEventListener('resize', markClipped);
  });

  // 贴边收起 / 滑回来时，两枚角标放大一次（与翻页那一次同一个动画，见 main.css 的 rq-badge-tick）。
  // 该动的是它：收起态**换掉的是内容**（卡片 ↔ 竖条），而角标是留在原处的那件东西 ——
  // 它动一下正好把"整块换过一档"说出来，也就是圆球那边"角标沿球缘转 90°"在矩形上的对应物。
  // 起点只取首次渲染那一格（`untrack` 明说"初始值就够了"，不然编译器会按漏引用报一句）
  let wasDocked = untrack(() => docked);
  $effect(() => {
    if (docked === wasDocked) return;
    wasDocked = docked;
    pulseBadges();
  });

  /** 页内列表滚动到哪儿了：动画滚动期间不动 target，免得和缓动打架。 */
  function onScroll(i: number) {
    const el = scrollers[i];
    if (el && rafId === null && i === idx) targets[i] = el.scrollTop;
  }

  function maxScroll(el: HTMLElement) {
    return Math.max(0, el.scrollHeight - el.clientHeight);
  }

  function animateScroll() {
    const el = scrollers[idx];
    if (!el) { rafId = null; return; }
    const diff = (targets[idx] ?? 0) - el.scrollTop;
    if (Math.abs(diff) < 0.5) {
      el.scrollTop = targets[idx] ?? 0;
      rafId = null;
      return;
    }
    el.scrollTop += diff * 0.24;
    rafId = window.requestAnimationFrame(animateScroll);
  }

  function scrollPageBy(d: number) {
    const el = scrollers[idx];
    if (!el) return;
    targets[idx] = Math.min(maxScroll(el), Math.max(0, (targets[idx] ?? el.scrollTop) + d));
    if (REDUCED) { el.scrollTop = targets[idx]; return; }
    if (rafId === null) rafId = window.requestAnimationFrame(animateScroll);
  }

  /** 列表是不是已经滚到头 / 到顶了：到了才把这一下滚轮让给翻页。 */
  function pageAtEdge(dir: number) {
    const el = scrollers[idx];
    if (!el) return true;
    const max = maxScroll(el);
    const target = targets[idx] ?? el.scrollTop;
    if (dir > 0) return target >= max - 0.5 && el.scrollTop >= max - 1.5;
    return target <= 0.5 && el.scrollTop <= 1.5;
  }

  function queueFlip(dir: number, raw: number) {
    if (dir > 0 && idx >= pages.length - 1) { acc = 0; lastDir = 0; return; }
    if (dir < 0 && idx <= 0) { acc = 0; lastDir = 0; return; }
    if (dir !== lastDir) { acc = 0; lastDir = dir; }
    acc += Math.abs(raw);
    if (locking) return;
    if (acc >= FLIP_AT) {
      acc = 0;
      locking = true;
      lockTimer = window.setTimeout(() => { locking = false; lockTimer = null; }, LOCK_MS);
      void goto(idx + dir, dir);
    }
  }

  function normDelta(e: WheelEvent) {
    const el = scrollers[idx];
    if (e.deltaMode === 1) return e.deltaY * 16;
    if (e.deltaMode === 2) return e.deltaY * (el ? el.clientHeight : 100);
    return e.deltaY;
  }

  function onWheel(e: WheelEvent) {
    if (e.deltaY === 0) return;
    e.preventDefault();
    const d = normDelta(e);
    const dir = d > 0 ? 1 : -1;
    if (!pageAtEdge(dir)) {
      scrollPageBy(d);
      acc = 0;
      lastDir = 0;
      return;
    }
    queueFlip(dir, d);
  }

  /**
   * 翻页（原型的两段式）：新卡先以「从牌堆方向抽出」的起点（略小、带一点倾斜）无过渡落位，
   * 逼一次样式重算把起点定死，再放开过渡铺平盖住旧卡；旧卡原地不动、被扫过之后才淡出 ——
   * 这样才看得见"盖住"。
   */
  async function goto(next: number, dir: number) {
    if (next < 0 || next >= pages.length || next === idx) return;
    const prev = idx;
    if (flipTimer !== null) { clearTimeout(flipTimer); flipTimer = null; }
    const target = pages[next];
    if (REDUCED || !cardEls[prev] || !cardEls[next]) {
      entering = null;
      leaving = null;
      activeId = target.id;
      fallback = next;
      pulseBadges();
      return;
    }
    entering = { idx: next, dir };
    await tick();
    void cardEls[next]?.offsetWidth;    // 起点定成 `from-*` 那一格
    entering = null;
    activeId = target.id;
    fallback = next;
    leaving = { idx: prev, dir };
    await tick();
    flipTimer = window.setTimeout(() => { leaving = null; flipTimer = null; }, FLIP_MS + 80);
    pulseBadges();
  }

  /** 计数是全局值，翻页不会变；这一步只说明"页换了"（原型同款）。 */
  function pulseBadges() {
    if (REDUCED) return;
    for (const el of [devBadgeEl, alertBadgeEl]) {
      if (!el) continue;
      el.classList.remove('is-ticking');
      void el.offsetWidth;
      el.classList.add('is-ticking');
    }
  }

  function centerDots() {
    const dot = dotEls[idx];
    if (!dotListEl || !dot) return;
    dotShift = dotListEl.offsetHeight / 2 - (dot.offsetTop + dot.offsetHeight / 2);
  }

  /** 名字超出列宽时改用右端渐隐（原型同款：不加省略号，仍看得到后面的字）。 */
  function markClipped() {
    for (const card of cardEls) {
      if (!card) continue;
      for (const el of card.querySelectorAll<HTMLElement>('.rq-iname')) {
        el.classList.toggle('is-clipped', el.scrollWidth > el.clientWidth + 0.5);
      }
    }
  }

  function onKeydown(e: KeyboardEvent, i: number) {
    if (i !== idx) return;
    switch (e.key) {
      case 'ArrowDown':
        e.preventDefault();
        if (!pageAtEdge(1)) scrollPageBy(rowStep);
        else void goto(idx + 1, 1);
        break;
      case 'ArrowUp':
        e.preventDefault();
        if (!pageAtEdge(-1)) scrollPageBy(-rowStep);
        else void goto(idx - 1, -1);
        break;
      case 'PageDown': case 'ArrowRight': e.preventDefault(); void goto(idx + 1, 1); break;
      case 'PageUp': case 'ArrowLeft': e.preventDefault(); void goto(idx - 1, -1); break;
      case 'Escape':
        // 钻进去了才有的出口：Esc = 回上一层（鼠标那套是双击列表头上那格「↰ 上一级」）。
        if (!shown[i]?.dir) break;
        e.preventDefault();
        onUp(i);
        break;
      case 'Home': e.preventDefault(); void goto(0, -1); break;
      case 'End': e.preventDefault(); void goto(pages.length - 1, 1); break;
    }
  }

  /**
   * 右键一个内容格：位置（指针在**窗口**里的坐标，菜单是 `position: fixed`、夹取也按窗口算）
   * 与"第几页第几格"一起交出去。
   *
   * `preventDefault` 关掉 webview 自带的那份右键菜单（App 在捕获阶段已经全局关过一次，这里
   * 再关一次是为了这两条路各自自洽）；`stopPropagation` 与内容格那套「按下即断」同一条口径 ——
   * 右键不该被卡上任何手势（移窗/单击展开都只认主键，见 `onPointerDown`）。
   */
  function onContextMenu(e: MouseEvent, i: number, j: number) {
    e.preventDefault();
    e.stopPropagation();
    onContext(i, j, e.clientX, e.clientY);
  }

  /* ─── 手势：移窗 / 单击 / 拖出 ───────────────────────────────────────
     与 `Orb.svelte` 同一套手动拖拽（不能用 `data-tauri-drag-region`：Tauri 的拖拽脚本在
     mousedown 当场上交 OS，本次按下的 pointerup/click 全丢，单击就永远不生效）。
     内容格不参与移窗（它是拖出源，自己 `stopPropagation`）—— 两种意图靠空间区分。 */
  function onPointerDown(e: PointerEvent) {
    if (e.button !== 0) return;
    downX = e.clientX;
    downY = e.clientY;
    pressed = true;
    moved = false;
    started = false;
    const dot = (e.target as HTMLElement | null)?.closest<HTMLElement>('.rq-dot');
    pendingDot = dot?.dataset.dot ? Number(dot.dataset.dot) : -1;
    wrapEl?.setPointerCapture(e.pointerId);
  }

  function onPointerMove(e: PointerEvent) {
    if (!pressed || started || e.buttons !== 1) return;
    const dx = e.clientX - downX;
    const dy = e.clientY - downY;
    if (!moved) {
      if (dx * dx + dy * dy <= DRAG_THRESHOLD_PX * DRAG_THRESHOLD_PX) return;
      moved = true;   // 到此为止这一按已经算拖动，松手不再当单击
    }
    started = true;
    /* 交给 OS 之前**必须放掉指针捕获**。`start_dragging` 之后页面再也收不到 pointerup
       （见上面那段），捕获就没法在 `onPointerUp` 里释放了 —— 而**留着的捕获 = 留着的 hover**：
       Chromium 把捕获目标当作"指针正悬停在它上面"，于是 `.rq:hover` 不再随指针离开而消失，
       hover 显隐的分页点胶囊就永远挂着。捕获的用处只在阈值判定之前（那时任务已经完成），
       移交之后一点用都没有。 */
    if (wrapEl?.hasPointerCapture(e.pointerId)) wrapEl.releasePointerCapture(e.pointerId);
    onDragStart({ pressX: downX, pressY: downY });
  }

  function onPointerUp(e: PointerEvent) {
    if (e.button !== 0) return;
    // 这一按不是起在悬浮框身上的（内容格的 pointerdown 已经 stopPropagation 掉了）：
    // 内容格有自己的"单击 = 跳标签页"，别在这里再当成"单击 = 开面板"。
    if (!pressed) return;
    pressed = false;
    if (wrapEl?.hasPointerCapture(e.pointerId)) wrapEl.releasePointerCapture(e.pointerId);
    const dot = pendingDot;
    pendingDot = -1;
    if (moved) return;
    if (dot >= 0 && dot < pages.length) {
      void goto(dot, dot > idx ? 1 : -1);
      return;
    }
    onToggle();
  }

  /**
   * 一页在朗读文本里的说法。三处（卡片 aria-label、列表 aria-label、翻页朗读）必须一致，
   * 所以收在一个地方：本机那页说的就是「本机」（`page.name` 本来也是它，见 App 的 `rectPages`），
   * 设备那页说的是"对本机可见的共享项"（对端的可见范围由广播开关与长连接决定，F-25）。
   * 钻进去之后说的就是"这个文件夹里有什么"。
   */
  function pageLabel(page: RectPage): string {
    if (page.dir) return `${page.name}，${page.dir.full}，${page.items.length} 项`;
    return page.kind === 'local'
      ? `${page.name}，${page.items.length} 个共享项`
      : `${page.name}，${page.items.length} 个可见共享项`;
  }

  /**
   * 组件被销毁：切形态（`App.svelte` 的 `{#if orbShape === 'rect'}`）时整棵卡片被摘掉。
   * 这里不 cancel 的话，那个自续期的 `requestAnimationFrame`（`animateScroll`）会一直排下一帧 ——
   * 元素已经不在文档里，`scrollTop` 恒为 0、与目标永远差着，收敛不了就是一辈子的 60Hz 空转，
   * 顺带把整棵 DOM（连同 `bind:this` 的那几个数组）钉在内存里。两个定时器同样收掉。
   */
  onDestroy(() => {
    if (rafId !== null) { cancelAnimationFrame(rafId); rafId = null; }
    if (flipTimer !== null) { clearTimeout(flipTimer); flipTimer = null; }
    if (lockTimer !== null) { clearTimeout(lockTimer); lockTimer = null; }
  });
</script>

<div
  class="rq"
  bind:this={wrapEl}
  inert={expanded}
  role="group"
  aria-label="OrbDrop 悬浮框：设备分页与共享项"
  style="--rq-w: {RECT_CARD_W}px"
  onwheel={onWheel}
  onpointerdown={onPointerDown}
  onpointermove={onPointerMove}
  onpointerup={onPointerUp}
  onpointercancel={() => { pressed = false; moved = true; }}
>
  <div class="rq-stack">
    <!-- 两枚角标骑在卡片的两个上角（原型 `.rq-shellbadge`：28px 高的宽体圆角矩形、2px 卡底
         描边、等宽数字）。**含义、角位、配色都与悬浮球逐项一致**：收起态从球换成这张卡时，
         同一个数还在同一个角、还是同一个颜色，只是外壳从圆球变成卡片。
         左 = 待处理数（橙，同球左上那颗：通知 + 发送侧"有台没成"那张卡 + 会话未读 + 群邀请；
         **本机在途的推送卡不算** —— 那是自己在做事，不是有事等你）；右 = 长连接设备数（绿，同球右上那颗）。
         翻页时轻微呼吸一下，和翻页动作产生关联。 -->
    <span class="rq-shellbadge is-alert num" bind:this={alertBadgeEl} role="img" hidden={notifCount === 0}
      aria-label="待处理 {notifCount} 条：通知、未读消息与群邀请">{notifCount}</span>
    <span class="rq-shellbadge is-devices num" bind:this={devBadgeEl} role="img" hidden={deviceCount === 0}
      aria-label="长连接设备 {deviceCount} 台">{deviceCount}</span>

    <!-- 贴边收起态的内容（`.rq-dock`）在每张卡**里面**（见下面 `article` 的末尾）：卡片滑出屏幕时
         被屏幕边裁掉的是卡片自己，所以露在屏内的那一条左侧是卡片自己的圆角、右侧是切口 ——
         不是另画一条"条"出来。 -->

    {#each shown as page, i (page.id)}
      <article
        class="rq-card"
        class:is-active={i === idx}
        class:is-leaving-next={leaving?.idx === i && leaving.dir > 0}
        class:is-leaving-prev={leaving?.idx === i && leaving.dir < 0}
        class:from-next={entering?.idx === i && entering.dir > 0}
        class:from-prev={entering?.idx === i && entering.dir < 0}
bind:this={cardEls[i]}
        aria-label={pageLabel(page)}
      >
        <header class="rq-head">
          <div class="rq-headtop">
            <!-- 卡片左上角这一格是**常驻位**：没通知时是项目 logo（与球的球心、面板左上角同一枚），
                 有通知时让位给左上那颗角标 —— 角标落点正是这一行的让位区，一出现就**整个盖住**
                 logo，这是刻意的（见 `main.css` 的 `.rq-logo`）。尺寸 12 + 上提 1px 是实测值：
                 大了会从角标右下圆弧漏出来。 -->
            <span class="rq-logo"><Logo size={12} /></span>
            <span class="rq-devname" title={page.name}>{page.name}</span>
          </div>
          <div class="rq-headmeta">
            <!-- 第二行：没钻入时是 `ip:port`（"这台是谁"），钻进去之后换成**现在在哪**
                 （`共享项名/子路径`；长了从头上砍，`title` 里给全）。项数照旧贴右缘。 -->
            {#if page.dir}
              <span class="rq-ip num is-path" title={page.dir.full}>{page.dir.label}</span>
            {:else}
              <span class="rq-ip num" title={page.ip}>{page.ip}</span>
            {/if}
            <span class="rq-count num">
              {page.loading ? '…' : page.error ? '—' : `${page.items.length} 项`}
            </span>
          </div>
        </header>

        <div class="rq-viewport">
          <!-- 列表容器刻意可聚焦：方向键在卡内滚列表、滚到底继续按即翻页（原型同一套键盘规则）。
               它不是「可交互控件」，所以这两条 a11y 提示在这里是有意为之。 -->
          <!-- svelte-ignore a11y_no_noninteractive_tabindex -->
          <!-- svelte-ignore a11y_no_noninteractive_element_interactions -->
          <div
            class="rq-scroll"
            bind:this={scrollers[i]}
            tabindex="0"
            role="group"
            aria-label="{pageLabel(page)}：上下方向键滚动列表，滚到底后继续即翻页"
            onkeydown={(e) => onKeydown(e, i)}
            onscroll={() => onScroll(i)}
          >
            {#if page.loading && page.items.length === 0}
              <div class="rq-empty">
                <p>{page.dir ? `正在读取「${page.dir.label}」…` : `正在读取「${page.name}」的共享项…`}</p>
              </div>
            {:else if page.error}
              <div class="rq-empty">
                {@html ICON_EMPTY}
                <p>{page.error}</p>
                <p class="rq-empty-sub">
                  {page.dir
                    ? '回上一层再试一次'
                    : page.kind === 'local' ? '确认共享项还在磁盘上' : '确认双方仍在长连接状态后重试'}
                </p>
              </div>
            {:else if page.items.length === 0 && !page.up}
              <!-- 空态按"是谁"分开说：本机那页要教怎么加共享项，设备那页要教对方怎么开广播
                   （两条都是用户唯一能采取的动作，换一句空话等于让人对着卡片发呆）。
                   钻进去之后的空目录**不走这里** —— 那时头上那格「↰ 上一级」必须留着，
                   它是唯一的出口（空目录由列表里那格「空文件夹」说明）。 -->
              <div class="rq-empty">
                {@html ICON_EMPTY}
                {#if page.kind === 'local'}
                  <p>还没有共享项</p>
                  <p class="rq-empty-sub">拖入文件或文件夹到悬浮框即可添加</p>
                {:else}
                  <p>对方还没有向本机广播共享项</p>
                  <p class="rq-empty-sub">对方打开共享项的广播开关后会自动出现</p>
                {/if}
              </div>
            {:else}
              <ul class="rq-grid">
                {#if page.up}
                  <!-- 列表头那格「↰ 上一级」：**双击**回上一层（单击无效果 —— 与内容格同一套礼仪）。
                       它不参与移窗（按下就 `stopPropagation`），也不是拖出源（不挂 `dragOut`）：
                       它是这张列表的出口，不是某一层里的东西。 -->
                  <li
                    class="rq-item is-up"
                    title="回到「{page.up.name}」"
                    aria-label="返回上一级：{page.up.name}"
                    onpointerdown={(e) => e.stopPropagation()}
                    ondblclick={() => onUp(i)}
                  >
                    <span class="rq-ic" aria-hidden="true">{@html ICON_UP}</span>
                    <span class="rq-itxt">
                      <span class="rq-iname">{page.up.name}</span>
                      <span class="rq-imeta">{page.up.meta}</span>
                    </span>
                  </li>
                {/if}
                {#each page.items as it, j (`${it.id}|${it.path}`)}
                  <li
                    class="rq-item"
                    class:is-folder={it.isDir}
                    title="{it.name} · {it.meta}"
                    use:dragOut={{
                      source: {
                        peerId: page.kind === 'peer' ? page.id : undefined,
                        shareId: it.id,
                        path: it.path,
                        name: it.name,
                        isDir: it.isDir,
                      },
                      onMessage: onToast,
                    }}
                    onpointerdown={(e) => e.stopPropagation()}
                    ondblclick={() => onActivate(i, j)}
                    oncontextmenu={(e) => onContextMenu(e, i, j)}
                  >
                    <span class="rq-ic" aria-hidden="true">{@html it.isDir ? ICON_FOLDER : ICON_FILE}</span>
                    <span class="rq-itxt">
                      <span class="rq-iname">{it.name}</span>
                      <span class="rq-imeta">{it.meta}</span>
                    </span>
                  </li>
                {/each}
                {#if page.up && page.items.length === 0}
                  <!-- 空目录：只说明"这里没东西"。不是条目、不吃指针（`is-void`）。 -->
                  <li class="rq-item is-void" aria-hidden="true">
                    <span class="rq-itxt"><span class="rq-iname">空文件夹</span></span>
                  </li>
                {/if}
              </ul>
            {/if}
          </div>
        </div>

        <!-- 贴边收起时这一张卡的内容：卡片整张滑出屏幕、只留靠边那一条时，露在屏内的就是它。
             两列：**图标 + 设备名同一列**（图标在这一列顶端）／ `ip:port` 一列。
             列宽固定（竖排一列只占一个行高），所以露出宽度是定数（`orbShape.RECT_DOCK_W` = 53）。
             横向锚在卡片贴边的那一端：贴右从左边排起，贴左整块镜像（图标落到右上角）；
             两列文字跟着左右换转向（右侧顺时针、左侧逆时针，见 main.css）。
             高度：设备名那一列**封顶 84**（图标压在角标带里，名落在上下两条角标带之间），
             `ip:port` 那一列在角标横向范围之外，可以用满条高。超了都由省略号收尾。
             它不是控件（`pointer-events: none`），悬停那一层是 `.orb-zone`。 -->
        <div class="rq-dock" aria-hidden="true">
          <span class="rq-dock-id">
            <span class="rq-dock-ic"><Logo size={14} /></span>
            <span class="rq-dock-name">{page.name}</span>
          </span>
          {#if page.ip}<span class="rq-dock-ip num">{page.ip}</span>{/if}
        </div>
      </article>
    {/each}
  </div>

  <!-- 分页点只在**有得翻**的时候出现：本机一页、以及后面每台长连接设备一页；只有一页时
       整列不画（没得翻）。位置照旧占着 —— 窗口尺寸是定尺（`orbShape.ts` 的 `rectLayout`），
       不因为这列画不画而变。 -->
  {#if pages.length > 1}
    <nav class="rq-dots" aria-label="设备分页">
      <div class="rq-dotlist" bind:this={dotListEl} style="--rq-dot-shift: {dotShift}px">
        {#each pages as page, i (page.id)}
          <button
            type="button"
            class="rq-dot"
            class:is-current={i === idx}
            bind:this={dotEls[i]}
            data-dot={i}
            style="opacity: {dotOpacities[i] ?? 0.07}"
            aria-label="切换到 {page.name}"
            aria-current={i === idx ? 'true' : undefined}
            onkeydown={(e) => {
              if (e.key !== 'Enter' && e.key !== ' ') return;
              e.preventDefault();
              void goto(i, i > idx ? 1 : -1);
            }}
          ></button>
        {/each}
      </div>
    </nav>
  {/if}
</div>

<p class="rq-sr" aria-live="polite">
  {pageLabel(activePage)}，第 {idx + 1} 页，共 {pages.length} 页
</p>