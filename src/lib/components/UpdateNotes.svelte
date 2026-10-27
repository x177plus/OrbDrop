<script lang="ts">
  import { t } from '$lib/i18n.svelte';

  /**
   * 更新日志：**默认折叠**，只露两行；点「查看完整更新日志」才展开全部。
   *
   * 更新说明常常是整段变更（十几行），原样塞进通知气泡/底板会把那一格撑得比面板还高，
   * 用户真正要先看的是"哪个版本、多大、要不要装"。所以折叠是默认态，展开是明确动作。
   *
   * 文案极短（没有换行、也不到两行）时不放按钮：给一句话配一个"展开"只会多一次点击。
   */
  let { notes, empty }: {
    notes: string | null | undefined;
    /** 没有更新说明时显示的那句话。 */
    empty?: string;
  } = $props();

  const emptyText = $derived(empty ?? t("这一版没有写更新说明。"));
  let open = $state(false);
  const text = $derived((notes ?? "").trim());
  /** 预判"两行放不下"：有换行，或长度明显超过两行（气泡宽 300px / 12px 字号约 40 字一行）。 */
  const collapsible = $derived(text.includes("\n") || text.length > 120);
</script>

<div class="unotes">
  {#if text}
    {#if !open && collapsible}
      <p class="unotes-text clamp">{text}</p>
      <button
        class="unotes-toggle"
        type="button"
        aria-expanded="false"
        onclick={() => (open = true)}
      >
        {t('查看完整更新日志')}
      </button>
    {:else}
      <p class="unotes-text">{text}</p>
      {#if collapsible}
        <button
          class="unotes-toggle"
          type="button"
          aria-expanded="true"
          onclick={() => (open = false)}
        >
          {t('收起')}
        </button>
      {/if}
    {/if}
  {:else}
    <p class="unotes-text">{emptyText}</p>
  {/if}
</div>

<style>
  .unotes {
    margin-top: 6px;
    min-width: 0;
  }
  .unotes-text {
    margin: 0;
    font-size: var(--fs-xs);
    color: var(--muted);
    line-height: 1.55;
    white-space: pre-wrap;
    overflow-wrap: anywhere;
  }
  /* 两行以上的预览：`white-space: normal` 才按视觉行数截（换成 pre-wrap 时换行符也算行）。 */
  .unotes-text.clamp {
    display: -webkit-box;
    -webkit-box-orient: vertical;
    line-clamp: 2;
    -webkit-line-clamp: 2;
    overflow: hidden;
    white-space: normal;
  }
  .unotes-toggle {
    display: inline-block;
    margin-top: 4px;
    padding: 0;
    border: 0;
    background: none;
    color: var(--accent);
    font: inherit;
    font-size: var(--fs-xs);
    cursor: pointer;
  }
  .unotes-toggle:hover {
    text-decoration: underline;
  }
</style>