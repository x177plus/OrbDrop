<script lang="ts">
  import type { TreeEntry } from '$lib/types';
  import { formatFileSize } from '$lib/types';
  import { untrack } from 'svelte';
  import { fetchTree, peerTree } from '$lib/api';
  import { getServerPort } from '$lib/stores/shares.svelte';
  import { dragOut } from '$lib/dragOut';
  import { peerDownloadActive, peerDownloadOf, peerDownloadPct, togglePeerDownloadPause } from '$lib/stores/peerDownloads.svelte';
  import TreeNode from './TreeNode.svelte';

  let {
    shareId,
    entry,
    level = 1,
    /**
     * 远程模式下的设备 id：右键菜单要按「对端的文件」而不是「本机共享项」处理，
     * 也决定这一层是问对端要还是问本机要。
     *
     * 对端的地址与凭据**不从前端传**：到对端那一跳是 TLS + 自签证书，WebView 既不会认
     * 我们的证书也不该认 —— 所以整件事走后端命令（`api.peerTree`）。
     */
    peerId,
    /** 对端目录变更信号：值变化时重拉本节点已加载的那一层（F-33）。 */
    refreshSignal = 0,
    /**
     * 本节点所在目录的共享项内相对路径（共享项根 = ''）。
     * 必须由父节点累加传下来：`entry.name` 只是叶子名，深层节点拿它当路径会指错目录
     * （展开子层、下载、BeyondCompare 比较全靠这个相对路径）。
     */
    parentPath = '',
  }: {
    shareId: string;
    entry: TreeEntry;
    level?: number;
    peerId?: string;
    refreshSignal?: number;
    parentPath?: string;
  } = $props();

  /** 本节点的共享项内相对路径（父目录路径 + 自己这一层名字）。 */
  let ownPath = $derived(parentPath ? `${parentPath}/${entry.name}` : entry.name);

  /** 拖出去的那一项（F-46）：整棵树上的每个节点都能拖。 */
  let dragSource = $derived({ peerId, shareId, path: ownPath, name: entry.name, isDir: entry.isDir });

  /** 对端这一项的手动下载（右键「下载到本地」）：行内背景底色按 `received / total` 推进。 */
  let download = $derived(peerDownloadOf(peerId, shareId, ownPath));
  let downloading = $derived(peerDownloadActive(download));
  let downloadPct = $derived(peerDownloadPct(download));

  /** 拖拽的准备进度/失败原因冒到 App 的浮层：树节点不知道自己在哪个面板上。 */
  function notify(msg: string) {
    document.dispatchEvent(new CustomEvent('od:toast', { detail: msg }));
  }

  /**
   * 双击正在下载的那一行 = 暂停 / 继续。
   *
   * 只有对端项、且这条下载真在动（或已暂停）时才接：别的行双击不做事（文件夹展开仍走
   * 那颗箭头，本机项没有下载）。后端那道闸只停流，不丢 `.orbdrop-part` 与已收字节。
   */
  async function toggleDownload(e: MouseEvent) {
    if (!peerId || !download || (download.status !== 'running' && download.status !== 'paused')) return;
    e.preventDefault();
    e.stopPropagation();
    const result = await togglePeerDownloadPause(peerId, shareId, ownPath);
    if (result === 'missing') notify('这项已不在下载中');
    else if (result === 'error') notify('暂停 / 继续下载失败');
  }

  let expanded = $state(false);
  let children = $state<TreeEntry[]>([]);
  let loaded = $state(false);
  let loading = $state(false);
  let error = $state<string | null>(null);

  async function loadChildren() {
    loading = true;
    error = null;
    try {
      if (peerId) {
        children = await peerTree(peerId, shareId, ownPath);
      } else {
        children = await fetchTree(getServerPort(), shareId, ownPath);
      }
      loaded = true;
    } catch (e: any) {
      error = e?.message ?? '加载失败';
    } finally {
      loading = false;
    }
  }

  async function toggle() {
    if (!entry.isDir) return;
    if (!loaded) {
      await loadChildren();
    }
    expanded = !expanded;
  }

  // 对端目录变更（F-33）：已经加载过的那一层重拉，展开状态保持不变。
  // 依赖只取 refreshSignal —— 读 loaded/entry 会把自己变成响应式依赖，形成多余往返。
  $effect(() => {
    const signal = refreshSignal;
    if (signal === 0) return;             // 初值：本地树与首次挂载都不触发
    untrack(() => {
      if (!entry.isDir || !loaded) return;
      void loadChildren();
    });
  });

  function openCtx(e: MouseEvent) {
    e.preventDefault();
    e.stopPropagation();
    document.dispatchEvent(new CustomEvent('od:ctx', {
      detail: peerId
        ? { x: e.clientX, y: e.clientY, kind: 'peer', peerId, shareId, path: ownPath, isDir: entry.isDir, size: entry.size, name: entry.name }
        : { x: e.clientX, y: e.clientY, kind: 'share', shareId, path: ownPath, isDir: entry.isDir, name: entry.name }
    }));
  }

  const ICON_FOLDER = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linejoin="round"><path d="M3 7a2 2 0 0 1 2-2h4.2a2 2 0 0 1 1.4.6L12 7h7a2 2 0 0 1 2 2v9a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V7Z"/></svg>';
  const ICON_FILE = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6"><path d="M6 3h8l5 5v13H6V3Z"/></svg>';
  const ICON_CHEVRON = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="m9 6 6 6-6 6"/></svg>';
</script>

{#if entry.isDir}
  <!-- svelte-ignore a11y_no_static_element_interactions -->
  <div class="node" class:sub={level > 1} oncontextmenu={openCtx} use:dragOut={{ source: dragSource, onMessage: notify }}>
    <button class="chev" style="width:18px; height:18px; flex:none; margin-left:-22px" onclick={toggle} aria-label="展开">
      {@html ICON_CHEVRON}
    </button>
    <span class="f-icon">{@html ICON_FOLDER}</span>
    <span class="n-name">{entry.name}</span>
  </div>
  {#if expanded}
    <div style="padding-left:16px">
      {#if loading}
        <div class="node sub"><span class="n-name" style="color:var(--muted)">加载中…</span></div>
      {:else if error}
        <div class="node sub"><span class="n-name" style="color:var(--warn)">{error}</span></div>
      {:else if children.length === 0}
        <div class="node sub"><span class="n-name" style="color:var(--muted)">空目录</span></div>
      {:else}
        {#each children as child (child.name)}
          <TreeNode {shareId} entry={child} level={level + 1} parentPath={ownPath} {peerId} {refreshSignal} />
        {/each}
      {/if}
    </div>
  {/if}
{:else}
  <!-- svelte-ignore a11y_no_static_element_interactions -->
  <div
    class="node"
    class:sub={level > 1}
    class:dl-active={downloading}
    class:dl-paused={download?.status === 'paused'}
    style="--dl-pct: {downloadPct / 100}"
    title={download?.status === 'paused'
      ? `已暂停 · 双击继续「${entry.name}」`
      : download?.status === 'running' ? `双击暂停「${entry.name}」` : undefined}
    ondblclick={toggleDownload}
    oncontextmenu={openCtx}
    use:dragOut={{ source: dragSource, onMessage: notify }}
  >
    <span class="f-icon">{@html ICON_FILE}</span>
    <span class="n-name">{entry.name}</span>
    <span class="n-meta num">{formatFileSize(entry.size)}</span>
  </div>
{/if}