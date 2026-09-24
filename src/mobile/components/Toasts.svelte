<script lang="ts">
  import Icon from "../Icon.svelte";
  import { getToasts } from "../store.svelte";

  // 底栏上方那一叠提示。离开前先挂 `.leaving` 淡出（原型同款）—— 这里只负责画，
  // 生命周期（何时加 `.leaving`、何时移除）在 store 里，免得两处各记一份计时。
  const items = $derived(getToasts());
</script>

<div class="toasts" role="status" aria-live="polite">
  {#each items as t (t.id)}
    <div class="toast" class:leaving={t.leaving}>
      {#if t.icon}<Icon name={t.icon} />{/if}
      <span>{t.text}</span>
    </div>
  {/each}
</div>