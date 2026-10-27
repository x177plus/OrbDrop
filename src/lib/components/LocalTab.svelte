<script lang="ts">
  import { onMount } from 'svelte';
  import { getShares, loadShares, toggleBroadcast } from '$lib/stores/shares.svelte';
  import { getDevices, deviceShortName } from '$lib/stores/devices.svelte';
  import { getServerPort, setServerPort } from '$lib/stores/shares.svelte';
  import { getActualPort } from '$lib/api';
  import TreeNode from './TreeNode.svelte';
  import { dragOut } from '$lib/dragOut';
  import type { TreeEntry } from '$lib/types';
  import { fetchTree } from '$lib/api';
  import type { ShareItem } from '$lib/types';
  import { t } from '$lib/i18n.svelte';

  let {
    onToast,
    onAddShare,
  }: {
    onToast: (msg: string) => void;
    /** 点那一大块「拖入文件或文件夹，点击添加共享项」：开文件选择框挑本机内容（F-20）。 */
    onAddShare: () => void;
  } = $props();

  interface TreeState { expanded: boolean; entries: TreeEntry[]; loaded: boolean; loading: boolean; error: string | null; }
  let shareTreeStates = $state<Record<string, TreeState>>({});
  // 尚未展开过的共享项用这个只读占位：模板里只能读状态，建条目必须发生在事件处理里，
  // 否则等于在渲染期写 $state（Svelte 5 抛 state_unsafe_mutation，整块列表渲染中断）。
  const EMPTY_TREE_STATE: TreeState = Object.freeze({ expanded: false, entries: [], loaded: false, loading: false, error: null });

  onMount(() => {
    loadShares();
    getActualPort().then(setServerPort).catch(console.error);
  });

  let shares = $derived(getShares());
  let connectedDevices = $derived(getDevices().filter(d => d.connected));
  let totalShares = $derived(shares.length);

  function treeState(shareId: string): TreeState {
    return shareTreeStates[shareId] ?? EMPTY_TREE_STATE;
  }

  async function toggleTree(shareId: string) {
    let st = treeState(shareId);
    if (st === EMPTY_TREE_STATE) {
      shareTreeStates = { ...shareTreeStates, [shareId]: { ...EMPTY_TREE_STATE, entries: [] } };
      st = shareTreeStates[shareId];
    }
    if (!st.loaded && !st.loading) {
      st.loading = true;
      try {
        st.entries = await fetchTree(getServerPort(), shareId, '');
        st.loaded = true;
      } catch (e: any) {
        st.error = e?.message ?? t('加载失败');
      } finally {
        st.loading = false;
      }
    }
    st.expanded = !st.expanded;
  }

  function handleBroadcastToggle(shareId: string, checked: boolean) {
    toggleBroadcast(shareId, checked).catch(console.error);
    onToast(checked
      ? (connectedDevices.length > 0
        ? t('已开启广播 · 自动广播至 {names}', { names: connectedDevices.map(deviceShortName).join(t('、')) })
        : t('已开启广播'))
      : t('已停止广播该共享项'));
  }

  function openCtx(e: MouseEvent, el: HTMLElement, share: ShareItem) {
    e.preventDefault();
    e.stopPropagation();
    const item = el.closest('.share-item') as HTMLElement | null;
    document.dispatchEvent(new CustomEvent('od:ctx', {
      detail: { x: e.clientX, y: e.clientY, el: item, kind: 'share', shareId: share.id, name: share.name }
    }));
  }

  const ICON_CHEVRON = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.8" stroke-linecap="round" stroke-linejoin="round"><path d="m9 6 6 6-6 6"/></svg>';
  const ICON_FOLDER = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linejoin="round"><path d="M3 7a2 2 0 0 1 2-2h4.2a2 2 0 0 1 1.4.6L12 7h7a2 2 0 0 1 2 2v9a2 2 0 0 1-2 2H5a2 2 0 0 1-2-2V7Z"/></svg>';
  const ICON_FILE = '<svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6"><path d="M6 3h8l5 5v13H6V3Z"/></svg>';
</script>

<button class="dropzone" onclick={onAddShare}>
  <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round">
    <path d="M12 3v12M6 9l6 6 6-6M5 19h14"/>
  </svg>
  {t('拖入文件或文件夹，点击添加共享项')}
</button>

<!-- 文案按**实际行为**写：`add_share` 落盘时 `broadcastEnabled` 是 false（`config.rs` 的
     `ShareItem::new`），所以"添加即自动广播"是不成立的 —— 要让人看到得自己打开那一项的开关。 -->
{#if connectedDevices.length > 0}
  <p class="bcast-hint">
    <span class="dot live"></span>
    <span>{t('{names} 已连接：在共享项上打开广播开关，对端才能看到', { names: connectedDevices.map(d => t('「{name}」', { name: deviceShortName(d) })).join(t('、')) })}</span>
  </p>
{/if}

<div class="list-label">
  <span>{t('共享项')}</span>
  <span class="num">{t('{n} 项', { n: totalShares })}</span>
</div>

{#if totalShares === 0}
  <div class="empty">
    <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="1.4" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true">
      <path d="M22 19a2 2 0 0 1-2 2H4a2 2 0 0 1-2-2V5a2 2 0 0 1 2-2h5l2 3h9a2 2 0 0 1 2 2v11Z"/>
    </svg>
    <p>{t('还没有共享项')}</p>
    <p class="e-sub">{t('拖入文件或文件夹到悬浮球即可添加共享项，并可选择广播至长连接设备。')}</p>
  </div>
{:else}
  <ul class="share-list">
    {#each shares as share (share.id)}
      {@const dir = share.isDir}
      {@const ts = treeState(share.id)}
      <li
        class="share-item"
        class:folder={dir}
        class:open={ts.expanded}
        data-id={share.id}
        oncontextmenu={(e) => openCtx(e, e.currentTarget, share)}
      >
        <div class="share-row" use:dragOut={{ source: { shareId: share.id, path: '', name: share.name, isDir: dir }, onMessage: onToast }}>
          {#if dir}
            <button class="chev" onclick={() => toggleTree(share.id)} aria-label={t('展开')}>{@html ICON_CHEVRON}</button>
          {:else}
            <button class="chev" style="visibility:hidden">{@html ICON_CHEVRON}</button>
          {/if}
          <span class="f-icon">{@html dir ? ICON_FOLDER : ICON_FILE}</span>
          <div class="s-main">
            <div class="s-name">{share.name}</div>
            <div class="s-meta">
              <span class="num">{share.path}</span>
            </div>
          </div>
          <div class="s-acts">
            {#if connectedDevices.length > 0 && share.broadcastEnabled}
              <span class="bcast-chip" title={t('自动广播至 {names}', { names: connectedDevices.map(d => t('「{name}」', { name: deviceShortName(d) })).join(t('、')) })}>
                <span class="dot live"></span>{t('{n}台设备', { n: connectedDevices.length })}
              </span>
            {/if}
            <label class="switch" title={t('向设备广播')}>
              <input
                type="checkbox"
                checked={share.broadcastEnabled}
                onchange={(e) => handleBroadcastToggle(share.id, (e.currentTarget as HTMLInputElement).checked)}
              />
              <span class="track"></span>
            </label>
          </div>
        </div>

        {#if dir}
          <div class="tree">
            <div class="tree-inner">
              {#if ts.loading}
                <div class="node"><span class="n-name" style="color:var(--muted)">{t('加载中…')}</span></div>
              {:else if ts.error}
                <div class="node"><span class="n-name" style="color:var(--warn)">{ts.error}</span></div>
              {:else if ts.expanded && ts.entries.length === 0}
                <div class="node"><span class="n-name" style="color:var(--muted)">{t('空目录')}</span></div>
              {:else if ts.expanded}
                {#each ts.entries as entry (entry.name)}
                  <TreeNode
                    shareId={share.id}
                    {entry}
                    level={1}
                  />
                {/each}
              {/if}
            </div>
          </div>
        {/if}
      </li>
    {/each}
  </ul>
{/if}