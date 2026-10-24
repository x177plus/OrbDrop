<script lang="ts">
  import { onMount } from 'svelte';
  import { listen } from '@tauri-apps/api/event';
  import { peerTree } from '$lib/api';
  import { getPeerSharesState, loadPeerShares } from '$lib/stores/peers.svelte';
  import { peerDownloadActive, peerDownloadOf, peerDownloadPct, togglePeerDownloadPause } from '$lib/stores/peerDownloads.svelte';
  import { deviceShortName } from '$lib/stores/devices.svelte';
  import type { PeerDevice, PeerShareItem, TreeEntry } from '$lib/types';
  import { dragOut } from '$lib/dragOut';
  import TreeNode from './TreeNode.svelte';

  let {
    device,
    onToast,
  }: {
    device: PeerDevice;
    onToast: (msg: string) => void;
  } = $props();

  interface ShareTree { expanded: boolean; entries: TreeEntry[]; loaded: boolean; loading: boolean; error: string | null; }
  let treeStates = $state<Record<string, ShareTree>>({});
  // 未展开过的共享项用只读占位：渲染期只能读状态，建条目必须发生在事件处理里。
  const EMPTY_TREE: ShareTree = Object.freeze({ expanded: false, entries: [], loaded: false, loading: false, error: null });

  /** 每次收到对端目录变更就自增，驱动 TreeNode 重拉自己已加载的那一层（F-33）。 */
  let refreshSignal = $state(0);

  /** 短名（F-101）：有备注就只写备注（没备注才是设备名）—— 会话、标签栏、页头都用它。 */
  const deviceName = $derived(deviceShortName(device));

  const shares = $derived(getPeerSharesState(device.id));

  function treeState(shareId: string): ShareTree {
    return treeStates[shareId] ?? EMPTY_TREE;
  }

  function patchTree(shareId: string, next: Partial<ShareTree>) {
    treeStates = { ...treeStates, [shareId]: { ...treeState(shareId), ...next } };
  }

  onMount(() => {
    void loadPeerShares(device.id);

    // 对端目录变更（F-33）：根层就地重拉，展开中的子层靠 refreshSignal 各自重拉。
    // **节流 + 尾沿**（见 `queueShareRefresh`）：逐条刷会把对端与本机界面一起压住。
    const changed = listen<{ shareId: string; path: string; kind: string }>('share://changed', (ev) => {
      const shareId = ev.payload.shareId;
      if (!treeState(shareId).loaded) return;      // 没打开过的共享项不用管
      queueShareRefresh(shareId);
    });

    return () => {
      if (changedTimer !== null) { clearTimeout(changedTimer); changedTimer = null; }
      pendingChanged.clear();
      void changed.then((fn) => fn());
    };
  });

  /**
   * 一轮变更之后多久再刷一次（尾沿也算一次）。
   *
   * 为什么必须节流：对端一次构建能在几百毫秒里连着报几百条变更，而这里每收一条就
   * `refreshSignal += 1`（展开中的每一层树各自重拉一遍）+ 重拉一次根层 —— 1300 条/秒
   * 就是一场请求风暴（打的是对端的 HTTP，卡的是本机界面）。**节流而不是单纯防抖**：
   * 持续爆发期间仍然每 `CHANGED_REFRESH_MS` 刷一次（"远端在跟着刷新"这件事不能丢），
   * 而**尾沿**保证爆发停下来之后还会再刷最后一次 —— 不会停在中间态。
   */
  const CHANGED_REFRESH_MS = 400;
  let changedTimer: ReturnType<typeof setTimeout> | null = null;
  let changedAt = 0;
  const pendingChanged = new Set<string>();

  function queueShareRefresh(shareId: string) {
    pendingChanged.add(shareId);
    if (changedTimer !== null) clearTimeout(changedTimer);
    const wait = Math.max(0, CHANGED_REFRESH_MS - (Date.now() - changedAt));
    changedTimer = setTimeout(() => {
      changedTimer = null;
      changedAt = Date.now();
      const ids = [...pendingChanged];
      pendingChanged.clear();
      refreshSignal += 1;                          // 展开中的子层各自重拉
      for (const id of ids) {
        if (treeState(id).expanded) void loadShareRoot(id, true);
      }
    }, wait);
  }

  async function loadShareRoot(shareId: string, inPlace = false) {
    // 刷新已展开的层时保持原样（不显示"加载中…"、不清空条目）：否则树会被卸载重建，
    // 展开状态和子层缓存全丢，反而看不出"自动刷新"。
    patchTree(shareId, inPlace ? { error: null } : { loading: true, error: null });
    try {
      const entries = await peerTree(device.id, shareId, '');
      patchTree(shareId, { entries, loaded: true });
    } catch (e: unknown) {
      patchTree(shareId, { error: e instanceof Error ? e.message : '加载失败' });
    } finally {
      patchTree(shareId, { loading: false });
    }
  }

  async function toggleShare(share: PeerShareItem) {
    const st = treeState(share.id);
    if (share.isDir && !st.loaded && !st.loading) {
      await loadShareRoot(share.id);
    }
    patchTree(share.id, { expanded: !treeState(share.id).expanded });
  }

  /** 手动刷新兜底（F-33）：重拉共享项清单 + 已加载的根层，展开状态保持不变。 */
  async function refresh() {
    refreshSignal += 1;
    for (const [shareId, st] of Object.entries(treeStates)) {
      if (st.loaded) void loadShareRoot(shareId, true);
    }
    await loadPeerShares(device.id);
    onToast(`已刷新「${deviceName}」的共享项`);
  }

  function openCtx(e: MouseEvent, share: PeerShareItem) {
    e.preventDefault();
    e.stopPropagation();
    dispatchCtx(e, { shareId: share.id, path: '', isDir: share.isDir, size: share.size ?? 0, name: share.name });
  }

  function dispatchCtx(e: MouseEvent, item: { shareId: string; path: string; isDir: boolean; size: number; name: string }) {
    document.dispatchEvent(new CustomEvent('od:ctx', {
      detail: { x: e.clientX, y: e.clientY, kind: 'peer', peerId: device.id, ...item }
    }));
  }

  /** 双击共享项根那一行（只对文件）：下载中 → 暂停，已暂停 → 继续。 */
  async function toggleShareDownload(e: MouseEvent, share: PeerShareItem) {
    if (share.isDir) return;
    const dl = peerDownloadOf(device.id, share.id, '');
    if (!dl || (dl.status !== 'running' && dl.status !== 'paused')) return;
    e.preventDefault();
    e.stopPropagation();
    const result = await togglePeerDownloadPause(device.id, share.id, '');
    if (result === 'missing') onToast('这项已不在下载中');
    else if (result === 'error') onToast('暂停 / 继续下载失败');
  }

  const ICON_CHEVRON = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="m9 6 6 6-6 6"/></svg>';
  const ICON_FOLDER = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linejoin="round"><path d="M3 7a2 2 0 0 1 2-2h4.2a2 2 0 0 1 1.4.6L12 7h7a2 2 0 0 1 2 2v9a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V7Z"/></svg>';
  const ICON_FILE = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6"><path d="M6 3h8l5 5v13H6V3Z"/></svg>';
</script>

<div class="peer-head">
  <span class="d-avatar">{deviceName.charAt(0).toUpperCase()}</span>
  <div class="d-main">
    <div class="d-name" title={deviceName}>{deviceName}</div>
    <div class="d-ip num">{device.ip}:{device.port}</div>
  </div>
  <span class="state-chip live"><span class="dot live"></span>长连接</span>
  <button class="icon-btn" onclick={refresh} aria-label="刷新共享项" title="刷新共享项">
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"><path d="M21 12a9 9 0 1 1-2.6-6.4M21 4v5h-5"/></svg>
  </button>
</div>

<div class="list-label">
  <span>共享项</span>
  <span class="num">{shares.items.length} 项</span>
</div>

{#if shares.error}
  <div class="empty">
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
      <path d="M12 9v4M12 17h.01M10.3 3.9 1.8 18a2 2 0 0 0 1.7 3h17a2 2 0 0 0 1.7-3L13.7 3.9a2 2 0 0 0-3.4 0Z"/>
    </svg>
    <p>{shares.error}</p>
    <p class="e-sub">对方只把开了广播的共享项发给长连接设备；确认双方仍在长连接状态后重试。</p>
  </div>
{:else if shares.loading && !shares.loaded}
  <div class="empty"><p>正在读取「{deviceName}」的共享项…</p></div>
{:else if shares.items.length === 0}
  <div class="empty">
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
      <path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2v11Z"/>
    </svg>
    <p>对方还没有向本机广播共享项</p>
    <p class="e-sub">对方在「本机」标签里打开某个共享项的广播开关后，这里会自动出现。</p>
  </div>
{:else}
  <ul class="share-list">
    {#each shares.items as share (share.id)}
      {@const ts = treeState(share.id)}
      {@const dl = peerDownloadOf(device.id, share.id, '')}
      {@const dlActive = !share.isDir && peerDownloadActive(dl)}
      {@const dlPaused = !share.isDir && dl?.status === 'paused'}
      {@const dlPct = peerDownloadPct(dl)}
      <li
        class="share-item"
        class:folder={share.isDir}
        class:open={ts.expanded}
        oncontextmenu={(e) => openCtx(e, share)}
      >
        <!-- svelte-ignore a11y_no_static_element_interactions -->
        <div
          class="share-row"
          class:dl-active={dlActive}
          class:dl-paused={dlPaused}
          style="--dl-pct: {dlPct / 100}"
          title={dlPaused
            ? `已暂停 · 双击继续「${share.name}」`
            : dl?.status === 'running' ? `双击暂停「${share.name}」` : undefined}
          ondblclick={(e) => void toggleShareDownload(e, share)}
          use:dragOut={{ source: { peerId: device.id, shareId: share.id, path: '', name: share.name, isDir: share.isDir }, onMessage: onToast }}
        >
          {#if share.isDir}
            <button class="chev" onclick={() => toggleShare(share)} aria-label="展开">{@html ICON_CHEVRON}</button>
          {:else}
            <button class="chev" style="visibility:hidden">{@html ICON_CHEVRON}</button>
          {/if}
          <span class="f-icon">{@html share.isDir ? ICON_FOLDER : ICON_FILE}</span>
          <div class="s-main">
            <div class="s-name">{share.name}</div>
            <div class="s-meta"><span class="num">来自「{deviceName}」</span></div>
          </div>
        </div>

        {#if share.isDir}
          <div class="tree">
            <div class="tree-inner">
              {#if ts.loading}
                <div class="node"><span class="n-name" style="color:var(--muted)">加载中…</span></div>
              {:else if ts.error}
                <div class="node"><span class="n-name" style="color:var(--warn)">{ts.error}</span></div>
              {:else if ts.expanded && ts.entries.length === 0}
                <div class="node"><span class="n-name" style="color:var(--muted)">空目录</span></div>
              {:else if ts.expanded}
                {#each ts.entries as entry (entry.name)}
                  <TreeNode shareId={share.id} {entry} level={1} peerId={device.id} {refreshSignal} />
                {/each}
              {/if}
            </div>
          </div>
        {/if}
      </li>
    {/each}
  </ul>
{/if}