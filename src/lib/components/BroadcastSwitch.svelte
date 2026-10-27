<script lang="ts">
  import { t } from '$lib/i18n.svelte';

  let {
    shareId,
    enabled,
    onChange,
  }: {
    shareId: string;
    enabled: boolean;
    onChange: (shareId: string, enabled: boolean) => void;
  } = $props();

  function toggle(e: Event) {
    e.stopPropagation();
    onChange(shareId, !enabled);
  }
</script>

<label class="switch" title={enabled ? t('广播中') : t('未广播')}>
  <input type="checkbox" checked={enabled} onchange={toggle} />
  <span class="track"></span>
</label>