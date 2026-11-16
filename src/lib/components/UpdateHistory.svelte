<script lang="ts">
  import { t } from '$lib/i18n.svelte';
  import type { ShareUpdateEntry } from '$lib/types';

  let {
    open,
    title,
    subtitle,
    isDir,
    loading,
    error,
    entries,
    onClose,
    onSelect,
    onBack,
  }: {
    open: boolean;
    title: string;
    subtitle: string;
    isDir: boolean;
    loading: boolean;
    error: string | null;
    entries: ShareUpdateEntry[];
    onClose: () => void;
    onSelect?: (path: string, name: string) => void;
    onBack?: () => void;
  } = $props();

  const latestMine = $derived(entries.find((entry) => entry.mine));
  const latestPeer = $derived(entries.find((entry) => !entry.mine));

  function when(at: number): string {
    const date = new Date(at);
    return date.toLocaleString();
  }

  function kindLabel(kind: string): string {
    if (kind === 'created') return t('创建');
    if (kind === 'deleted') return t('删除');
    return t('修改');
  }

  function fileName(path: string): string {
    const at = path.lastIndexOf('/');
    return at >= 0 ? path.slice(at + 1) : path;
  }
</script>

{#if open}
  <!-- svelte-ignore a11y_click_events_have_key_events a11y_no_static_element_interactions -->
  <div
    class="overlay open history-overlay"
    role="dialog"
    aria-modal="true"
    aria-label={t('更新记录：{name}', { name: title })}
    tabindex="-1"
    onclick={(e) => {
      if (e.target === e.currentTarget) onClose();
    }}
  >
    <div class="dialog history-dialog">
      <header class="history-head">
        <div>
          <h3>{title}</h3>
          <p class="history-sub">{subtitle || t('共享项根目录')}</p>
        </div>
        {#if onBack}
          <button class="history-back" onclick={onBack}>{t('返回目录')}</button>
        {/if}
      </header>

      {#if loading}
        <p class="history-note">{t('正在读取更新记录…')}</p>
      {:else if error}
        <p class="history-error">{error}</p>
      {:else if entries.length === 0}
        <p class="history-note">{t('还没有更新记录')}</p>
      {:else if isDir}
        <p class="history-note">{t('显示有变化的文件（每个文件的最新一条）')}</p>
        <div class="history-list">
          {#each entries as entry (entry.path)}
            <button class="history-row" onclick={() => onSelect?.(entry.path, fileName(entry.path))}>
              <span class="history-name">{entry.path}</span>
              <span class="history-meta">
                <span class="history-badge" class:mine={entry.mine}>{entry.mine ? t('本机') : t('对端')}</span>
                <span>{entry.actorName}</span>
                <span class="history-kind">{kindLabel(entry.kind)}</span>
                <time>{when(entry.at)}</time>
              </span>
            </button>
          {/each}
        </div>
      {:else}
        <div class="history-summary">
          <div class="history-card">
            <span class="history-label">{t('本机最近更新')}</span>
            {#if latestMine}
              <strong>{when(latestMine.at)}</strong>
              <span>{latestMine.actorName} · {kindLabel(latestMine.kind)}</span>
            {:else}
              <span class="history-empty">{t('暂无')}</span>
            {/if}
          </div>
          <div class="history-card">
            <span class="history-label">{t('对端最近更新')}</span>
            {#if latestPeer}
              <strong>{when(latestPeer.at)}</strong>
              <span>{latestPeer.actorName} · {kindLabel(latestPeer.kind)}</span>
            {:else}
              <span class="history-empty">{t('暂无')}</span>
            {/if}
          </div>
        </div>
        <div class="history-list">
          {#each entries as entry, index (entry.at + entry.path + entry.actorId)}
            <div class="history-row static" class:latest={index === 0}>
              <span class="history-meta">
                <span class="history-badge" class:mine={entry.mine}>{entry.mine ? t('本机') : t('对端')}</span>
                <span>{entry.actorName}</span>
                <span class="history-kind">{kindLabel(entry.kind)}</span>
                <time>{when(entry.at)}</time>
              </span>
            </div>
          {/each}
        </div>
      {/if}

      <footer class="d-acts">
        <button class="btn btn-secondary" type="button" onclick={onClose}>{t('关闭')}</button>
      </footer>
    </div>
  </div>
{/if}

<style>
  /* 固定高度的卡片：外层遮罩不滚，只有中间的 `.history-list` 滚。高度同时受视口约束，
     小窗口上也不会把卡片顶出遮罩（`min-height: 0` 是给 grid/flex 的自动最小尺寸兜底）。 */
  .overlay.history-overlay {
    overflow: hidden;
  }
  .history-dialog {
    width: min(460px, calc(100% - 32px));
    height: min(560px, calc(100% - 32px));
    max-height: calc(100% - 32px);
    min-height: 0;
    padding: 18px;
    display: flex;
    flex-direction: column;
    gap: 10px;
    overflow: hidden;
  }
  .history-dialog > :not(.history-list) {
    flex: none;
  }
  .history-head {
    display: flex;
    align-items: flex-start;
    justify-content: space-between;
    gap: 12px;
  }
  .history-head h3 {
    margin: 0;
    word-break: break-all;
  }
  .history-sub {
    margin: 4px 0 0;
    font-family: var(--font-mono);
    font-size: var(--fs-xs);
    color: var(--muted);
    word-break: break-all;
  }
  .history-back {
    flex: none;
    padding: 5px 9px;
    border-radius: 7px;
    font-size: var(--fs-xs);
    color: var(--muted);
    border: 1px solid var(--border);
  }
  .history-note {
    margin: 0;
    font-size: var(--fs-sm);
    color: var(--muted);
  }
  .history-error {
    margin: 0;
    font-size: var(--fs-sm);
    color: rgb(134, 71, 0);
    word-break: break-word;
  }
  .history-summary {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 8px;
  }
  .history-card {
    display: flex;
    flex-direction: column;
    gap: 3px;
    padding: 10px;
    border: 1px solid var(--border);
    border-radius: var(--radius);
    background: var(--fg-soft);
    font-size: var(--fs-xs);
    color: var(--muted);
    word-break: break-word;
  }
  .history-card strong {
    color: var(--fg);
    font-size: var(--fs-sm);
    font-weight: 600;
  }
  .history-label {
    font-size: var(--fs-xs);
    color: var(--muted);
  }
  .history-empty {
    color: var(--muted);
  }
  .history-list {
    display: flex;
    flex-direction: column;
    gap: 2px;
    flex: 1 1 auto;
    min-height: 0;
    overflow-y: auto;
    padding-right: 2px;
  }
  /* 固定卡片里没有列表时（加载中 / 空 / 出错），关闭键仍贴在卡片底部。
     写成 `.dialog.history-dialog` 是为了压过全局 `.dialog .d-acts { margin-top: 18px }`。 */
  .dialog.history-dialog .d-acts {
    margin-top: auto;
  }
  .history-row {
    display: flex;
    flex-direction: column;
    gap: 3px;
    width: 100%;
    padding: 8px 9px;
    border-radius: 8px;
    text-align: left;
    color: var(--fg);
    font-size: var(--fs-sm);
  }
  .history-row:not(.static):hover {
    background: var(--fg-soft);
  }
  .history-row.static {
    cursor: default;
  }
  .history-name {
    font-family: var(--font-mono);
    font-size: var(--fs-xs);
    word-break: break-all;
  }
  .history-meta {
    display: flex;
    align-items: center;
    flex-wrap: wrap;
    gap: 7px;
    font-size: var(--fs-xs);
    color: var(--muted);
  }
  .history-badge {
    padding: 1px 5px;
    border-radius: 999px;
    border: 1px solid var(--border);
    color: var(--muted);
  }
  .history-badge.mine {
    color: var(--fg);
    border-color: var(--fg);
  }
  .history-kind {
    color: inherit;
  }
  /* 文件时间线：最新一条保持默认字色，后面的历史压成灰色。 */
  .history-row.static.latest .history-meta {
    color: var(--fg);
  }
  .history-row.static:not(.latest) .history-meta,
  .history-row.static:not(.latest) .history-badge.mine {
    color: var(--muted);
  }
  .history-row.static:not(.latest) .history-badge {
    border-color: var(--border);
  }
  .history-row.static.latest .history-badge {
    color: var(--fg);
    border-color: var(--fg);
  }
</style>