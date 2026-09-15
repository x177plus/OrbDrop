<script lang="ts">
  import type { TreeEntry } from '$lib/types';
  import { untrack } from 'svelte';
  import { fetchTree, fetchPeerTree } from '$lib/api';
  import { getServerPort } from '$lib/stores/shares.svelte';
  import TreeNode from './TreeNode.svelte';

  let {
    shareId,
    entry,
    level = 1,
    // Remote browsing (P1): when set, fetches from a peer's HTTP API.
    baseUrl,
    token,
    /** 远程模式下的设备 id：右键菜单要按「对端的文件」而不是「本机共享项」处理。 */
    peerId,
    /** 对端目录变更信号：值变化时重拉本节点已加载的那一层（F-33）。 */
    refreshSignal = 0,
  }: {
    shareId: string;
    entry: TreeEntry;
    level?: number;
    baseUrl?: string;
    token?: string;
    peerId?: string;
    refreshSignal?: number;
  } = $props();

  let expanded = $state(false);
  let children = $state<TreeEntry[]>([]);
  let loaded = $state(false);
  let loading = $state(false);
  let error = $state<string | null>(null);

  async function loadChildren() {
    loading = true;
    error = null;
    try {
      if (baseUrl && token) {
        children = await fetchPeerTree(baseUrl, token, shareId, entry.name);
      } else {
        children = await fetchTree(getServerPort(), shareId, entry.name);
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
        ? { x: e.clientX, y: e.clientY, kind: 'peer', peerId, shareId, path: entry.name, isDir: entry.isDir, name: entry.name }
        : { x: e.clientX, y: e.clientY, kind: 'share', shareId, path: entry.name, isDir: entry.isDir, name: entry.name }
    }));
  }

  const ICON_FOLDER = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linejoin="round"><path d="M3 7a2 2 0 0 1 2-2h4.2a2 2 0 0 1 1.4.6L12 7h7a2 2 0 0 1 2 2v9a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V7Z"/></svg>';
  const ICON_FILE = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6"><path d="M6 3h8l5 5v13H6V3Z"/></svg>';
  const ICON_CHEVRON = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="m9 6 6 6-6 6"/></svg>';

  function formatSize(bytes: number): string {
    if (bytes <= 0) return '';
    const units = ['B', 'KB', 'MB', 'GB'];
    const i = Math.floor(Math.log(bytes) / Math.log(1024));
    const v = bytes / Math.pow(1024, i);
    return `${v.toFixed(i > 0 ? 1 : 0)} ${units[i]}`;
  }
</script>

{#if entry.isDir}
  <!-- svelte-ignore a11y_no_static_element_interactions -->
  <div class="node" class:sub={level > 1} oncontextmenu={openCtx}>
    <button class="chev" style="width:18px; height:18px; flex:none; margin-left:-22px" onclick={toggle} aria-label="展开">
      {@html ICON_CHEVRON}
    </button>
    <span class="f-icon">{@html ICON_FOLDER}</span>
    <span class="n-name">{entry.name}</span>
    <span class="n-meta num">{entry.size > 0 ? `${entry.size} 个文件` : ''}</span>
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
          <TreeNode {shareId} entry={child} level={level + 1} {baseUrl} {token} {peerId} />
        {/each}
      {/if}
    </div>
  {/if}
{:else}
  <!-- svelte-ignore a11y_no_static_element_interactions -->
  <div class="node" class:sub={level > 1} oncontextmenu={openCtx}>
    <span class="f-icon">{@html ICON_FILE}</span>
    <span class="n-name">{entry.name}</span>
    <span class="n-meta num">{formatSize(entry.size)}</span>
  </div>
{/if}