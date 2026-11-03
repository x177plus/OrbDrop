<script lang="ts">
  import Icon from "../Icon.svelte";
  import { busyText, getToasts } from "../store.svelte";

  // 底栏上方那一叠提示。离开前先挂 `.leaving` 淡出（原型同款）—— 这里只负责画，
  // 生命周期（何时加 `.leaving`、何时移除）在 store 里，免得两处各记一份计时。
  const items = $derived(getToasts());
  // 选目录时"正在准备"的那一条：它**不自己消失**（收尾由 `pickAttach` 摘掉），所以不放进
  // `items` 那套 2.6 秒的计时里。
  const busy = $derived(busyText());
</script>

<div class="toasts" role="status" aria-live="polite">
  {#if busy}
    <div class="toast">
      <Icon name="folder" />
      <span class="toast-msg">{busy}</span>
    </div>
  {/if}
  {#each items as t (t.id)}
    <div class="toast" class:leaving={t.leaving} title={t.text}>
      {#if t.icon}<Icon name={t.icon} />{/if}
      <span class="toast-msg">{t.text}</span>
    </div>
  {/each}
</div>