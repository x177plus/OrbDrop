<script lang="ts">
  import { tick } from 'svelte';
  import { t } from '$lib/i18n.svelte';

  /**
   * 桌面端的应用内确认框。
   *
   * 为什么不用 `@tauri-apps/plugin-dialog` 的 `ask`：那是系统原生弹窗（Windows 上还会带
   * 系统图标与按钮风格），和 OrbDrop 自己的圆角 / 字号 / 危险按钮不是一套。删除共享项文件
   * 这种"要用户明确点一下"的动作统一走这里，外观、键盘与遮罩行为都跟其它对话框一致。
   *
   * 与 `window.confirm` 同形是**调用方**的事（App.svelte 里包一层 Promise），这个组件只负责
   * 画：`danger` 决定确认键走 `.btn-danger`，Escape 与点遮罩都等于取消。
   */
  let {
    open,
    title,
    message,
    confirmLabel,
    cancelLabel,
    danger = false,
    onConfirm,
    onCancel,
  }: {
    open: boolean;
    title: string;
    message: string;
    confirmLabel?: string;
    cancelLabel?: string;
    /** 危险动作：确认键走红色，初始焦点落在「取消」上，避免误按回车真的删掉。 */
    danger?: boolean;
    onConfirm: () => void;
    onCancel: () => void;
  } = $props();

  let cancelEl = $state<HTMLButtonElement | null>(null);
  let confirmEl = $state<HTMLButtonElement | null>(null);
  /** 打开前的焦点：关掉之后还给触发它的那颗按钮。 */
  let previousFocus: HTMLElement | null = null;

  $effect(() => {
    if (open) {
      previousFocus = document.activeElement instanceof HTMLElement ? document.activeElement : null;
      void tick().then(() => {
        (danger ? cancelEl : confirmEl)?.focus();
      });
    } else if (previousFocus) {
      previousFocus.focus();
      previousFocus = null;
    }
  });

  $effect(() => {
    if (!open) return;
    // Escape 挂在 window 上：焦点被 Tab 带到对话框外时，仍然与原生确认框一样能取消。
    const onKey = (event: KeyboardEvent) => {
      if (event.key !== 'Escape') return;
      // 捕获阶段 + stopPropagation：确认框开着时，Esc 只取消它，不再顺带关掉后面的
      // 会话浮层 / 收件人框（那些监听在 document 冒泡阶段，先到的会以为自己该收）。
      event.preventDefault();
      event.stopPropagation();
      onCancel();
    };
    window.addEventListener('keydown', onKey, true);
    return () => window.removeEventListener('keydown', onKey, true);
  });
</script>

{#if open}
  <!-- svelte-ignore a11y_click_events_have_key_events a11y_no_static_element_interactions -->
  <div
    class="overlay open"
    role="presentation"
    onclick={(e) => {
      if (e.target === e.currentTarget) onCancel();
    }}
  >
    <div
      class="dialog confirm-dialog"
      role="dialog"
      aria-modal="true"
      aria-label={title}
      tabindex="-1"
    >
      <h3>{title}</h3>
      <p class="confirm-message">{message}</p>
      <div class="d-acts">
        <button class="btn btn-secondary" type="button" bind:this={cancelEl} onclick={onCancel}>
          {cancelLabel ?? t('取消')}
        </button>
        <button
          class="btn {danger ? 'btn-danger' : 'btn-primary'}"
          type="button"
          bind:this={confirmEl}
          onclick={onConfirm}
        >
          {confirmLabel ?? t('确定')}
        </button>
      </div>
    </div>
  </div>
{/if}

<style>
  .confirm-dialog {
    width: min(380px, calc(100% - 32px));
  }
  .confirm-message {
    margin: 0;
    font-size: var(--fs-sm);
    color: var(--muted);
    line-height: 1.6;
    overflow-wrap: anywhere;
  }
</style>