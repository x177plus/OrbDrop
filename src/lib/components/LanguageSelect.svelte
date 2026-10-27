<script lang="ts">
  /**
   * 界面语言下拉框（桌面端与移动端共用）。
   *
   * 为什么不用原生 `<select>`：原生控件的**选项弹层**由系统画，样式跟应用的圆角/配色对不上，
   * 且部分平台会给文字再塞一圈内部缩进。这里用按钮 + 自绘 listbox：收起时是与 `.input`
   * 同高同圆角的矩形，展开时是应用自己的圆角菜单。样式分别在 `styles/main.css` 与
   * `mobile/mobile.css` 的 `.lang-*` 段。
   */
  import { tick } from 'svelte';
  import { SUPPORTED_LOCALES, type Locale } from '$lib/i18n.svelte';

  let {
    id,
    value,
    ariaLabel,
    onChange,
  }: {
    /** 给外层 `<label for>` 用；移动端没有 label 元素时可省略。 */
    id?: string;
    value: Locale;
    ariaLabel: string;
    onChange: (locale: Locale) => void;
  } = $props();

  let open = $state(false);
  let rootEl = $state<HTMLDivElement>();
  let triggerEl = $state<HTMLButtonElement>();
  let menuEl = $state<HTMLDivElement>();

  const selected = $derived(SUPPORTED_LOCALES.find((loc) => loc.id === value) ?? SUPPORTED_LOCALES[0]);

  function choose(locale: Locale): void {
    open = false;
    onChange(locale);
    void tick().then(() => triggerEl?.focus());
  }

  function onTriggerKeydown(event: KeyboardEvent): void {
    if (event.key !== 'ArrowDown' && event.key !== 'ArrowUp') return;
    event.preventDefault();
    open = true;
    void tick().then(() => {
      const options = menuEl?.querySelectorAll<HTMLButtonElement>('.lang-option');
      if (!options?.length) return;
      (event.key === 'ArrowDown' ? options[0] : options[options.length - 1]).focus();
    });
  }

  $effect(() => {
    if (!open || typeof window === 'undefined') return;
    const onPointerDown = (event: PointerEvent) => {
      if (rootEl && !rootEl.contains(event.target as Node)) open = false;
    };
    const onKeydown = (event: KeyboardEvent) => {
      if (event.key !== 'Escape') return;
      open = false;
      triggerEl?.focus();
    };
    window.addEventListener('pointerdown', onPointerDown);
    window.addEventListener('keydown', onKeydown);
    return () => {
      window.removeEventListener('pointerdown', onPointerDown);
      window.removeEventListener('keydown', onKeydown);
    };
  });
</script>

<div class="lang-select" class:open bind:this={rootEl}>
  <button
    {id}
    bind:this={triggerEl}
    class="lang-trigger"
    type="button"
    aria-haspopup="listbox"
    aria-expanded={open}
    aria-label="{ariaLabel}: {selected.label}"
    onclick={() => (open = !open)}
    onkeydown={onTriggerKeydown}
  >
    <span>{selected.label}</span>
    <svg class="lang-caret" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="m6 9 6 6 6-6"/></svg>
  </button>
  {#if open}
    <div class="lang-menu" bind:this={menuEl} role="listbox" aria-label={ariaLabel}>
      {#each SUPPORTED_LOCALES as loc (loc.id)}
        <button
          class="lang-option"
          class:active={loc.id === value}
          type="button"
          role="option"
          aria-selected={loc.id === value}
          onclick={() => choose(loc.id)}
        >
          <span>{loc.label}</span>
          {#if loc.id === value}
            <svg class="lang-check" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round" aria-hidden="true"><path d="m5 12 4.5 4.5L19 7"/></svg>
          {/if}
        </button>
      {/each}
    </div>
  {/if}
</div>