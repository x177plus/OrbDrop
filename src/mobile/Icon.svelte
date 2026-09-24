<script lang="ts">
  import { ICONS, type IconName } from "./icons";

  let { name, cls = "" }: { name: IconName; cls?: string } = $props();
  const def = $derived(ICONS[name]);
</script>

<!-- 图元按原型的属性原样渲染：有几处实心点自带 fill/stroke 覆盖，不能被外层样式吃掉。 -->
<svg class="ic {cls}" viewBox={def.viewBox} aria-hidden="true">
  {#each def.prims as p}
    {#if p.tag === "path"}
      <path {...p.attrs} />
    {:else if p.tag === "circle"}
      <circle {...p.attrs} />
    {:else}
      <rect {...p.attrs} />
    {/if}
  {/each}
</svg>