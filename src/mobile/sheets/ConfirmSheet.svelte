<script lang="ts">
  import Sheet from "../components/Sheet.svelte";
  import * as store from "../store.svelte";
  import type { ConfirmPrompt } from "../store.svelte";

  /**
   * 应用内确认框：与其它底板同一套 `.sheet` 外壳、同一套按钮与遮罩。
   *
   * 它占的是 store 里那个唯一的 sheet 槽位（打开时替换来源那张操作表），所以取消/返回键/
   * 点遮罩都会走 `store.closeSheet()` → `resolveConfirm(false)`；确定走
   * `resolveConfirm(true)`。调用方拿到的是一个 Promise<boolean>，与 `window.confirm` 同形。
   */
  let { prompt, onClose }: { prompt: ConfirmPrompt; onClose: () => void } = $props();
</script>

<Sheet title={prompt.title} desc={prompt.desc} label={prompt.title} {onClose}>
  {#snippet foot()}
    <button class="btn btn-secondary" type="button" onclick={onClose}>
      {prompt.cancelLabel ?? "取消"}
    </button>
    <button
      class="btn {prompt.danger ? "btn-danger" : "btn-primary"}"
      type="button"
      onclick={() => store.resolveConfirm(true)}
    >
      {prompt.confirmLabel}
    </button>
  {/snippet}
</Sheet>