<script lang="ts">
  import { onMount, tick } from 'svelte';
  import { fsDrives, fsList, listPickerFavorites, setPickerFavorites } from '$lib/api';
  import type { FsCrumb, FsDrive, FsEntry, FsError, LocalPick, PickerFavorite } from '$lib/types';
  import { scrollRail } from '$lib/scrollRail';
  import { ctxPlacement } from '$lib/ctxPlacement';
  import Hint from '$lib/components/Hint.svelte';

  /**
   * 本机文件选择框：**文件和文件夹在同一个框里混选**。
   *
   * 为什么要自绘：系统原生选择框在 Windows/Linux 上只能二选一（Win32 只有
   * `FOS_PICKFOLDERS` 这一个开关，`rfd` 也只给 `pick_file(s) / pick_folder(s)`），
   * 而推送要的正是"文件 + 文件夹混选"。自绘还顺带拿到原生框给不了的两件事：
   * 选中的项**跨目录累积**（原生框一换目录，之前选的就没了），以及顶部「常用」。
   *
   * 一律多选：多选也能只挑一个（挑一个就是单发），没有额外操作成本。
   *
   * 选中集**只有一个呈现处**：底部角标点开的那辆购物车（不管选中的项在不在当前目录）。
   * 跨目录累积的代价正是"换了目录之后，之前选的在列表里一个像素都不占"，所以车必须能
   * 看到全量、能逐项摘掉、能全部取消 —— 这三件事只有这一份实现。
   */
  let {
    title,
    sub,
    confirmLabel = '选择',
    onConfirm,
    onClose,
  }: {
    /** 标题（如「推送到「XX」」）。 */
    title: string;
    /** 标题旁「?」里的说明。 */
    sub: string;
    confirmLabel?: string;
    onConfirm: (items: LocalPick[]) => void;
    onClose: () => void;
  } = $props();

  /** 一行：目录项或盘符，渲染前统一成同一个形状。 */
  type Row = { name: string; path: string; isDir: boolean; size: number; drive: FsDrive | null };
  /** 车里的项：交给调用方的 `LocalPick` 再加**勾选时所在的位置**。
   *  位置只能在勾选那一刻记（`cwd` / `crumbs` 是现成的）—— 提交时不许从 `path` 反推
   *  （见 p1：前端不拼路径）。车上每行都要显示它：两个目录里的同名文件只靠名字分不开。 */
  type Pick = LocalPick & { from: string; fromName: string };
  /** 右键菜单里的"对象"：一个目录/文件行，或一个常用位置。`x/y` 是这一层内的坐标，
   *  `bw/bh` 是这一层的大小（菜单要留在里面）。 */
  type CtxState = { x: number; y: number; bw: number; bh: number; row: Row | null; fav: PickerFavorite | null };

  let layerEl = $state<HTMLElement | null>(null);
  let listEl = $state<HTMLElement | null>(null);
  let railEl = $state<HTMLElement | null>(null);
  /** 底部角标（购物车的开关）与展开的那块面板：摘完一项要把焦点接回车上。 */
  let cartEl = $state<HTMLElement | null>(null);
  let cartPanelEl = $state<HTMLElement | null>(null);

  /** 设备与驱动器（后端 `fs_drives`）：「此电脑」那一层只列这些。 */
  let drives = $state<FsDrive[] | null>(null);
  /** 当前目录的绝对路径；`''` = 「此电脑」（那一层只列盘符，不走 `fs_list`）。 */
  let cwd = $state('');
  /** 面包屑（含「此电脑」前面那几段；失败时后端也会带回来，见 `failed`）。 */
  let crumbs = $state<FsCrumb[]>([]);
  let entries = $state<FsEntry[]>([]);
  let loading = $state(true);
  let failed = $state<FsError | null>(null);
  /** 选中的项：按 `path` 去重，**不随导航清空**（这是自绘框相对原生框的主要增益）。 */
  let selection = $state<Pick[]>([]);
  /** 车展开着没有。**只在点角标时开**：换目录本来就是为了接着选东西，自动展开会挡道。 */
  let cartOpen = $state(false);
  /** 顶部「常用」：用户右键目录攒出来的，**存在配置文件里**（重启后还在）；默认一条都没有。 */
  let favorites = $state<PickerFavorite[]>([]);
  let ctx = $state<CtxState | null>(null);
  /** 入场动画用的一位：挂载时先不带 `.open`，下一帧再加 —— 直接带着它挂载就没有过渡了。 */
  let entered = $state(false);

  let rows = $derived<Row[]>(
    cwd === ''
      ? (drives ?? []).map((d) => ({ name: d.name, path: d.path, isDir: true, size: 0, drive: d }))
      : entries.map((e) => ({ name: e.name, path: e.path, isDir: e.isDir, size: e.size, drive: null })),
  );

  // ── 导航 ────────────────────────────────────────────────────────────────
  /**
   * 进入某个绝对路径（`''` = 此电脑）。
   *
   * 失败不退回上一级：面包屑由错误里的 `crumbs` 摆好，用户看到的是"这个目录打不开"，
   * 而不是"我明明点了它却还停在上一个地方"。
   */
  async function go(path: string) {
    hideCtx();
    // 换目录 = 接着选东西：车收起来（选中集不变，角标照样挂着；要看得再点一下）。
    cartOpen = false;
    if (path === '') {
      cwd = '';
      crumbs = [];
      entries = [];
      failed = null;
      loading = false;
      await focusFirst();
      return;
    }
    loading = true;
    failed = null;
    try {
      const listing = await fsList(path);
      cwd = listing.path;
      crumbs = listing.crumbs;
      entries = listing.entries;
    } catch (e) {
      const err = e as FsError;
      failed = { kind: err?.kind ?? 'other', message: err?.message ?? String(e), crumbs: err?.crumbs ?? [] };
      cwd = path;
      crumbs = failed.crumbs;
      entries = [];
    } finally {
      loading = false;
    }
    await focusFirst();
  }

  /** 返回上一级：面包屑的倒数第二段（`''` = 此电脑）。 */
  function up() {
    if (cwd === '') return;
    const parent = crumbs.length >= 2 ? crumbs[crumbs.length - 2].path : '';
    void go(parent);
  }

  async function focusFirst() {
    await tick();
    const first = listEl?.querySelector<HTMLElement>('.picker-row');
    if (first) first.focus();
    else listEl?.focus();
  }

  onMount(() => {
    requestAnimationFrame(() => (entered = true));
    void (async () => {
      try {
        drives = await fsDrives();
      } catch (e) {
        const err = e as FsError;
        failed = { kind: err?.kind ?? 'other', message: err?.message ?? String(e), crumbs: [] };
      } finally {
        loading = false;
      }
      try {
        favorites = await listPickerFavorites();
      } catch {
        // 读不到就当空：这一栏是快捷入口，不该拦住整个选择框。
        favorites = [];
      }
      await focusFirst();
    })();
  });

  // ── 选中 ────────────────────────────────────────────────────────────────
  function isSelected(path: string): boolean {
    return selection.some((s) => s.path === path);
  }

  /** 改选中集的唯一入口。空车就顺手把车收掉：车开着、里面什么都没有是个没有意义的抽屉，
   *  不收的话下次勾一个项会看到它**已经开着**（用户没点过）。 */
  function setSelection(next: Pick[]) {
    selection = next;
    if (!next.length) cartOpen = false;
  }

  /** 当前所在位置（给车上的"在哪选的"用）：`''` = 此电脑；否则 crumbs 末段就是当前目录
   *  （后端保证，见 p1 的路径语义）。 */
  function here(): { from: string; fromName: string } {
    return { from: cwd, fromName: cwd === '' ? '此电脑' : crumbs[crumbs.length - 1].name };
  }

  function toggle(row: Row) {
    if (isSelected(row.path)) {
      setSelection(selection.filter((s) => s.path !== row.path));
      return;
    }
    setSelection([
      ...selection,
      { name: row.name, path: row.path, isDir: row.isDir, size: row.size, ...here() },
    ]);
  }

  function toggleCart() {
    cartOpen = !cartOpen;
  }

  /** 从车上摘一项：列表里那一行的勾会跟着灭（`aria-selected` 派生自 `selection`）。
   *  摘完把焦点交给同一位置的下一个 ✕ —— 那个按钮自己刚被卸载，不接一把焦点就掉到
   *  body 上，键盘用户连摘几项会在这里断掉。 */
  function removePick(path: string, index: number) {
    setSelection(selection.filter((s) => s.path !== path));
    void (async () => {
      await tick();
      const xs = cartPanelEl?.querySelectorAll<HTMLElement>('.cart-x');
      if (xs?.length) xs[Math.min(index, xs.length - 1)].focus();
      else await focusFirst();
    })();
  }

  function clearSelection() {
    setSelection([]);
    void focusFirst();
  }

  /** 交给调用方时把账本摘掉：`from` / `fromName` 只在这一层用。 */
  function confirm() {
    if (!selection.length) return;
    onConfirm(selection.map(({ name, path, isDir, size }) => ({ name, path, isDir, size })));
  }

  function meta(r: Row): string {
    if (r.drive) {
      const kind = r.drive.removable ? '可移动磁盘' : '本地磁盘';
      if (r.drive.total <= 0) return kind;
      return `${kind} · ${formatSize(r.drive.free)} 可用 / ${formatSize(r.drive.total)}`;
    }
    return r.isDir ? '文件夹' : `文件 · ${formatSize(r.size)}`;
  }

  /** 盘占用条：画"已用"的比例（与原型一致）。 */
  function usedPct(d: FsDrive): number {
    if (d.total <= 0) return 0;
    return Math.max(0, Math.min(100, Math.round((1 - d.free / d.total) * 100)));
  }

  function formatSize(bytes: number): string {
    if (bytes <= 0) return '0 B';
    const units = ['B', 'KB', 'MB', 'GB', 'TB'];
    const i = Math.min(units.length - 1, Math.floor(Math.log(bytes) / Math.log(1024)));
    return `${(bytes / Math.pow(1024, i)).toFixed(i > 0 ? 1 : 0)} ${units[i]}`;
  }

  // ── 常用位置 ────────────────────────────────────────────────────────────
  /**
   * 写回常用列表（整份替换）。
   *
   * **先改界面再落盘**：右键加一下要立刻看到芯片。落盘失败就退回原样并说一声 ——
   * 挂着芯片、下次打开却没了，用户只会以为"时灵时不灵"。
   * 写成功时以后端返回的那份为准（它会去重），两边不会分叉。
   */
  async function persistFavorites(next: PickerFavorite[]) {
    const before = favorites;
    favorites = next;
    try {
      favorites = await setPickerFavorites(next);
    } catch (e) {
      favorites = before;
      document.dispatchEvent(new CustomEvent('od:toast', { detail: `常用位置没能保存：${e}` }));
    }
  }

  function isFav(path: string): boolean {
    return favorites.some((f) => f.path === path);
  }

  function addFav(row: Row) {
    if (!row.isDir || isFav(row.path)) return;
    void persistFavorites([...favorites, { name: row.name, path: row.path }]);
  }

  /* 新加的那条滚进视野。否则芯片静默加在末尾、留在可视区外 —— 右键加完"看着什么都没发生"，
     和标签栏里新设备接入是同一件事（TabBar 也这么做）。
     首次铺开不算"新加"：那是打开这一刻读到的那批，别让常用栏自己跳到最右。 */
  let knownFavCount = 0;
  $effect(() => {
    const count = favorites.length;
    const firstPopulation = knownFavCount === 0;
    const grew = count > knownFavCount;
    knownFavCount = count;
    if (firstPopulation || !grew) return;
    railEl?.lastElementChild?.scrollIntoView({ inline: 'nearest', block: 'nearest' });
  });

  function removeFav(path: string) {
    void persistFavorites(favorites.filter((f) => f.path !== path));
  }

  // ── 右键菜单 ────────────────────────────────────────────────────────────
  // 坐标必须相对**这一层**：`position: fixed` 会被 `.picker-layer` 的 backdrop-filter
  // 改成相对它定位（见 main.css 里的层叠说明），直接送 clientX/clientY 会整体偏移。
  // 翻边/夹取交给 `ctxPlacement`（面板里的那几个菜单用的是同一份实现），这里只报
  // 指针位置和这一层有多大 —— 菜单顶出这一层同样点不到。
  function layerPointer(e: MouseEvent): { x: number; y: number; bw: number; bh: number } | null {
    const box = layerEl?.getBoundingClientRect();
    if (!box) return null;
    return { x: e.clientX - box.left, y: e.clientY - box.top, bw: box.width, bh: box.height };
  }

  function openCtx(e: MouseEvent, row: Row) {
    e.preventDefault();
    e.stopPropagation();
    const at = layerPointer(e);
    if (!at) return;
    ctx = { ...at, row, fav: null };
  }

  function openFavCtx(e: MouseEvent, fav: PickerFavorite) {
    e.preventDefault();
    e.stopPropagation();
    const at = layerPointer(e);
    if (!at) return;
    ctx = { ...at, row: null, fav };
  }

  function hideCtx() {
    ctx = null;
  }

  // ── 列表交互 ────────────────────────────────────────────────────────────
  function rowOf(el: HTMLElement): Row | undefined {
    return rows.find((r) => r.path === el.dataset.path);
  }

  function onListClick(e: MouseEvent) {
    const target = e.target as HTMLElement;
    const el = target.closest<HTMLElement>('.picker-row');
    if (!el) return;
    const row = rowOf(el);
    if (!row) return;
    // 行尾那个箭头是"进去"，点行本身是"选它" —— 两个动作挤在一行里，得分清
    if (target.closest('.pr-enter')) {
      if (row.isDir) void go(row.path);
      return;
    }
    toggle(row);
  }

  function onListDblClick(e: MouseEvent) {
    const el = (e.target as HTMLElement).closest<HTMLElement>('.picker-row');
    if (!el) return;
    const row = rowOf(el);
    if (row?.isDir) void go(row.path);
  }

  function onListKey(e: KeyboardEvent) {
    const items = Array.from(listEl?.querySelectorAll<HTMLElement>('.picker-row') ?? []);
    const at = items.indexOf(document.activeElement as HTMLElement);
    const focusAt = (i: number) => {
      const el = items[Math.max(0, Math.min(items.length - 1, i))];
      if (el) el.focus();
    };

    if (e.key === 'ArrowDown') {
      e.preventDefault();
      focusAt(at + 1);
    } else if (e.key === 'ArrowUp') {
      e.preventDefault();
      focusAt(Math.max(0, at - 1));
    } else if (e.key === 'Home') {
      e.preventDefault();
      focusAt(0);
    } else if (e.key === 'End') {
      e.preventDefault();
      focusAt(items.length - 1);
    } else if (e.key === ' ' || e.key === 'Spacebar') {
      const row = rowOf(document.activeElement as HTMLElement);
      if (row) {
        e.preventDefault();
        toggle(row);
      }
    } else if (e.key === 'Enter') {
      const row = rowOf(document.activeElement as HTMLElement);
      if (!row) return;
      e.preventDefault();
      // 目录上的回车 = 进去（与双击一致）；文件上的回车 = 就这么定了
      if (row.isDir) void go(row.path);
      else confirm();
    } else if (e.key === 'Backspace') {
      e.preventDefault();
      up();
    }
  }

  /** Esc 关闭 / Tab 圈在这一层里（面板其余部分被这层盖着，焦点跑出去等于键盘用户点不到东西）。 */
  function onWindowKey(e: KeyboardEvent) {
    if (e.key === 'Escape') {
      e.preventDefault();
      if (ctx) {
        hideCtx();
        return;
      }
      if (cartOpen) {
        // 先收车、再关框（和右键菜单同一个顺序）：Esc 一键把整批选择丢掉太狠。
        cartOpen = false;
        cartEl?.focus();
        return;
      }
      onClose();
      return;
    }
    if (e.key !== 'Tab' || !layerEl) return;
    const items = Array.from(
      layerEl.querySelectorAll<HTMLElement>('button:not([disabled]), .picker-row[tabindex="0"]'),
    ).filter((el) => el.offsetParent !== null);
    if (!items.length) return;
    const first = items[0];
    const last = items[items.length - 1];
    if (e.shiftKey && document.activeElement === first) {
      e.preventDefault();
      last.focus();
    } else if (!e.shiftKey && document.activeElement === last) {
      e.preventDefault();
      first.focus();
    }
  }

  const ICON_FOLDER = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linejoin="round"><path d="M3 7a2 2 0 0 1 2-2h4.2a2 2 0 0 1 1.4.6L12 7h7a2 2 0 0 1 2 2v9a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V7Z"/></svg>';
  const ICON_FILE = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6"><path d="M6 3h8l5 5v13H6V3Z"/></svg>';
  const ICON_DRIVE = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><rect x="3" y="5" width="18" height="11" rx="2"/><path d="M7 19h10M12 16v3"/></svg>';
  const ICON_CHECK = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.4" stroke-linecap="round" stroke-linejoin="round"><path d="m5 12 5 5 9-9"/></svg>';
  const ICON_CHEVRON = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="m9 6 6 6-6 6"/></svg>';
  const ICON_UP = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="m15 6-6 6 6 6"/></svg>';
  const ICON_WARN = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><circle cx="12" cy="12" r="9"/><path d="M12 8v5M12 16h.01"/></svg>';
  const ICON_LOCK = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.5" stroke-linecap="round" stroke-linejoin="round"><rect x="5" y="10" width="14" height="10" rx="2"/><path d="M8 10V7a4 4 0 0 1 8 0v3"/></svg>';
  const ICON_SPIN = '<svg class="pk-spin" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round"><path d="M12 3a9 9 0 1 0 9 9"/></svg>';
  const ICON_STAR = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M12 4l2.4 4.9 5.4.8-3.9 3.8.9 5.4-4.8-2.5-4.8 2.5.9-5.4L4.2 9.7l5.4-.8L12 4Z"/></svg>';
  const ICON_TRASH = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M4 7h16M9 7V4h6v3M6 7l1 13h10l1-13M10 11v6M14 11v6"/></svg>';
  const ICON_PICK = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M12 5v14M5 12l7 7 7-7"/></svg>';
  const ICON_X = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round"><path d="M6 6l12 12M18 6 6 18"/></svg>';
</script>

<svelte:window onkeydown={onWindowKey} onresize={hideCtx} onclick={(e) => { if (ctx && !(e.target as HTMLElement).closest('.picker-ctx')) hideCtx(); }} />

<div class="picker-layer" class:open={entered} bind:this={layerEl} aria-hidden="false">
  <div class="picker" role="dialog" aria-modal="true" aria-label={title}>
    <header class="picker-head">
      <h3>{title}</h3>
      <Hint>{sub}</Hint>
      <button class="icon-btn" id="pickerCloseBtn" onclick={onClose} aria-label="关闭选择框">
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round"><path d="M6 6l12 12M18 6 6 18"/></svg>
      </button>
    </header>

    <div class="picker-places">
      <div class="picker-rail" aria-label="常用位置" bind:this={railEl} use:scrollRail>
        {#if favorites.length === 0}
          <span class="picker-empty-fav">还没有常用位置 · 右键目录即可添加到常用</span>
        {:else}
          {#each favorites as f (f.path)}
            <button
              class="place"
              class:active={f.path === cwd}
              title={`${f.path} · 右键可移出常用`}
              onclick={() => void go(f.path)}
              oncontextmenu={(e) => openFavCtx(e, f)}
            >
              <span class="f-icon">{@html ICON_FOLDER}</span>{f.name}
            </button>
          {/each}
        {/if}
      </div>
    </div>

    <div class="picker-crumbs">
      <button class="crumb-up" onclick={up} disabled={cwd === ''} aria-label="返回上一级">{@html ICON_UP}</button>
      <div class="crumb-trail">
        <button class="crumb" class:current={cwd === ''} onclick={() => void go('')}>此电脑</button>
        {#each crumbs as c, i (c.path)}
          <span class="crumb-sep">/</span>
          <button class="crumb" class:current={i === crumbs.length - 1} onclick={() => void go(c.path)}>{c.name}</button>
        {/each}
      </div>
    </div>

    <!-- 右键菜单开在列表里、点空白就关：滚动时它必须跟着走（跟着的代价是位置会飘） -->
    <div class="picker-body" onscroll={hideCtx}>
      {#if loading}
        <div class="picker-state">{@html ICON_SPIN}<p>正在读取…</p></div>
      {:else if failed}
        <div class="picker-state">
          {@html failed.kind === 'denied' ? ICON_LOCK : ICON_WARN}
          <p>{failed.kind === 'denied' ? '无权访问' : failed.kind === 'missing' ? '找不到这个位置' : '打不开这个位置'}</p>
          <p class="ps-sub">
            {failed.kind === 'denied'
              ? '这个文件夹需要更高的系统权限，请选择一个你有权限的文件夹。'
              : failed.kind === 'missing'
                ? '它可能被移走或改名了。请返回上一级后重试。'
                : failed.message}
          </p>
        </div>
      {:else if rows.length === 0}
        <div class="picker-state">
          {@html ICON_FOLDER}
          <p>{cwd === '' ? '没有找到磁盘' : '这个文件夹是空的'}</p>
          <p class="ps-sub">{cwd === '' ? '系统没有返回任何可用的驱动器。' : '返回上一级，或选择其他文件夹。'}</p>
        </div>
      {:else}
        <div
          class="picker-list"
          role="listbox"
          aria-multiselectable="true"
          aria-label="当前位置内容"
          tabindex="-1"
          bind:this={listEl}
          onclick={onListClick}
          ondblclick={onListDblClick}
          onkeydown={onListKey}
          oncontextmenu={(e) => {
            const el = (e.target as HTMLElement).closest<HTMLElement>('.picker-row');
            if (!el) {
              hideCtx();
              return;
            }
            const row = rowOf(el);
            if (row) openCtx(e, row);
          }}
        >
          {#if cwd === ''}
            <div class="picker-group" role="presentation">设备和驱动器</div>
          {/if}
          {#each rows as r (r.path)}
            <div
              class="picker-row"
              class:folder={r.isDir}
              class:drive={!!r.drive}
              role="option"
              aria-selected={isSelected(r.path)}
              tabindex="0"
              data-path={r.path}
              title={r.isDir ? (isFav(r.path) ? '右键可移出常用' : '右键可添加到常用') : ''}
            >
              <span class="pr-check">{@html ICON_CHECK}</span>
              <span class="f-icon">{@html r.drive ? ICON_DRIVE : r.isDir ? ICON_FOLDER : ICON_FILE}</span>
              <span class="pr-main">
                <span class="pr-name">{r.name}</span>
                <span class="pr-meta">{meta(r)}</span>
                {#if r.drive && r.drive.total > 0}
                  <span class="pr-bar"><i style="width:{usedPct(r.drive)}%"></i></span>
                {/if}
              </span>
              {#if r.isDir}
                <span class="pr-enter" aria-hidden="true">{@html ICON_CHEVRON}</span>
              {/if}
            </div>
          {/each}
        </div>
      {/if}
    </div>

    <!-- 购物车：点底部角标向上展开，**占布局高度**（上面的列表 `flex: 1` 自己让位）——
        不做浮在列表上的弹层：那会盖住最后几行，而且 `.picker` 是 `overflow: hidden`，
        挂到框外必被裁。它是选中集的**唯一**呈现处：选中的项可能不在当前目录、在列表里
        一个像素都不占，只有这儿能看到全量、能逐项摘、能全部取消。 -->
    {#if cartOpen && selection.length > 0}
      <div class="picker-cart" id="pickerCart" bind:this={cartPanelEl} aria-label="已选项目">
        <div class="cart-head">
          <span class="cart-count">已选 <b>{selection.length}</b> 项</span>
          <button class="cart-clear" onclick={clearSelection}>全部取消</button>
        </div>
        <div class="cart-list">
          {#each selection as s, i (s.path)}
            <div class="cart-row" class:folder={s.isDir}>
              <span class="f-icon">{@html s.isDir ? ICON_FOLDER : ICON_FILE}</span>
              <span class="cart-name">{s.name}</span>
              <span class="cart-from" title={s.from}>{s.fromName}</span>
              <span class="cart-meta">{s.isDir ? '文件夹' : formatSize(s.size)}</span>
              <button
                class="cart-x"
                onclick={() => removePick(s.path, i)}
                aria-label="取消选择 {s.name}"
                title="取消选择「{s.name}」"
              >{@html ICON_X}</button>
            </div>
          {/each}
        </div>
      </div>
    {/if}

    <footer class="picker-foot">
      {#if selection.length === 0}
        <div class="picker-sel">未选择任何项目</div>
      {:else}
        <!-- 角标 + 首项名字 + 箭头，整块是车的开关。收起时箭头朝上（点开是往上长） -->
        <button
          class="cart-btn"
          class:open={cartOpen}
          bind:this={cartEl}
          onclick={toggleCart}
          aria-expanded={cartOpen}
          aria-controls="pickerCart"
          title={selection.map((s) => s.path).join('\n')}
        >
          <span class="cart-badge num">{selection.length}</span>
          <span class="cart-label">{selection[0].name}{#if selection.length > 1}<span class="cart-etc">等 {selection.length} 项</span>{/if}</span>
          <span class="cart-caret">{@html ICON_CHEVRON}</span>
        </button>
      {/if}
      <button class="btn btn-secondary btn-sm" onclick={onClose}>取消</button>
      <button class="btn btn-primary btn-sm" onclick={confirm} disabled={selection.length === 0}>{confirmLabel}</button>
    </footer>
  </div>

  {#if ctx}
    {@const cur = ctx}
    <!-- svelte-ignore a11y_no_static_element_interactions -->
    <div
      class="picker-ctx ctx show"
      role="menu"
      tabindex="-1"
      use:ctxPlacement={{ x: cur.x, y: cur.y, bounds: { width: cur.bw, height: cur.bh } }}
    >
      {#if cur.row?.isDir}
        <button class="ctx-item" onclick={() => { hideCtx(); void go(cur.row!.path); }}>
          {@html ICON_CHEVRON}进入此文件夹
        </button>
        <div class="ctx-sep"></div>
        {#if isFav(cur.row!.path)}
          <button class="ctx-item danger" onclick={() => { const p = cur.row!.path; hideCtx(); removeFav(p); }}>
            {@html ICON_TRASH}从常用中移除
          </button>
        {:else}
          <button class="ctx-item" onclick={() => { const r = cur.row!; hideCtx(); addFav(r); }}>
            {@html ICON_STAR}添加到常用
          </button>
        {/if}
      {:else if cur.row}
        <button class="ctx-item" onclick={() => { const r = cur.row!; hideCtx(); toggle(r); }}>
          {@html ICON_PICK}选择此文件
        </button>
      {:else if cur.fav}
        <button class="ctx-item" onclick={() => { const p = cur.fav!.path; hideCtx(); void go(p); }}>
          {@html ICON_CHEVRON}进入「{cur.fav!.name}」
        </button>
        <div class="ctx-sep"></div>
        <button class="ctx-item danger" onclick={() => { const p = cur.fav!.path; hideCtx(); removeFav(p); }}>
          {@html ICON_TRASH}从常用中移除
        </button>
      {/if}
    </div>
  {/if}
</div>