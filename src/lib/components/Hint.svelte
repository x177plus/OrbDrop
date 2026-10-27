<script lang="ts">
  import type { Snippet } from 'svelte';
  import { hideHint, showHint } from '$lib/stores/hint.svelte';
  import { t } from '$lib/i18n.svelte';

  /**
   * 说明正文：直接写成子内容，能带 `<strong>` / `<span class="hl">` 这类强调。
   *
   * 点「?」**不往上传**：它常挨着某个控件（设置里的字段标题、会话创建框里的模式卡片都在
   * `<label>` 里）—— 不拦一下，点说明会顺带把那个单选/开关给切了。
   */
  let { children }: { children: Snippet } = $props();

  let trigger = $state<HTMLElement | null>(null);

  /** 悬停与键盘聚焦都算"要看说明"：落位与渲染交给 `HintLayer`。 */
  function show() {
    if (trigger) showHint(children, trigger);
  }

  // 触发点随面板/对话框一起消失时（切标签、关设置），说明必须跟着走 ——
  // 否则那颗「?」没了，气泡还挂在原地。
  $effect(() => () => hideHint());
</script>

<!-- svelte-ignore a11y_click_events_have_key_events a11y_no_static_element_interactions -->
<span
  class="hint"
  bind:this={trigger}
  role="button"
  tabindex="0"
  aria-label={t('说明')}
  aria-describedby="hint-tip"
  onmouseenter={show}
  onmouseleave={hideHint}
  onfocus={show}
  onblur={hideHint}
  onclick={(e) => e.stopPropagation()}
>?</span>