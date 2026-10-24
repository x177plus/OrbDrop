<script lang="ts">
  import type { Snippet } from "svelte";
  import Icon from "../Icon.svelte";

  // 底板（sheet）的公共外壳：底板 + 遮罩 + 抓手 + 头部 + 可滚动的 body + 可选底部按钮区。
  // 各 sheet 只填内容 —— 免得每张表都自己写一遍 `.sheet-layer` 的定位与关闭手势。
  let { title, desc = "", label, onClose, children, foot }: {
    title: string;
    desc?: string;
    /** 无障碍名字；缺省用标题。 */
    label?: string;
    onClose: () => void;
    /** 确认框只有标题、说明与底部按钮，没有 body 内容。 */
    children?: Snippet;
    foot?: Snippet;
  } = $props();
</script>

<!-- 遮罩本身就是关闭手势（原型同款）：点它 = 收起，去按钮上找"取消"不是唯一出路。 -->
<div class="scrim open" role="presentation" onclick={onClose}></div>

<div class="sheet-layer open">
  <div class="sheet" role="dialog" aria-modal="true" aria-label={label ?? title}>
    <div class="sheet-grip"></div>
    <div class="sheet-head">
      <div class="sht">
        <h3 class="sheet-title">{title}</h3>
        {#if desc}<p class="sheet-desc">{desc}</p>{/if}
      </div>
      <button class="icon-btn" type="button" aria-label="关闭" onclick={onClose}><Icon name="x" /></button>
    </div>
    {#if children || foot}
      <div class="sheet-body">
        {#if children}{@render children()}{/if}
        {#if foot}<div class="sheet-foot">{@render foot()}</div>{/if}
      </div>
    {/if}
  </div>
</div>