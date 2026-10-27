<script lang="ts">
  import { loadAppIcon, peekAppIcon } from "../appIcons";
  import { t } from "$lib/i18n.svelte";

  /**
   * 应用表里那一行的图标（F-119）：**取到之前显示首字母**。
   *
   * 为什么要有这一层组件：图标是异步来的（`loadAppIcon` 走一趟 IPC），而"取不到"是**正常**
   * 结果之一（正在卸载、图标坏了、原生报错）—— 那种时候这一行必须还有东西可看（首字母），
   * 一张表不该因为某一行没有图标就空一块。
   *
   * 缓存里已经有那一张时**当帧就画出来**（`url` 是 `$derived`，首帧就求值）：搜索框每敲一个字
   * 都会重建整张表的行，那种时候先闪一下首字母再换成图标，看着就是"整张表在抖"。
   */
  let { pkg, label }: { pkg: string; label: string } = $props();

  /** 这一趟取回来的那一张（带包名：挂载之后换了包名也不会拿旧图顶上）。 */
  let fetched = $state<{ pkg: string; url: string } | null>(null);
  const url = $derived(peekAppIcon(pkg) ?? (fetched?.pkg === pkg ? fetched.url : undefined));

  $effect(() => {
    const key = pkg;
    if (peekAppIcon(key)) return; // 缓存里有：不用再走一趟
    let alive = true;
    void loadAppIcon(key)
      .then((got) => {
        if (alive) fetched = { pkg: key, url: got };
      })
      .catch(() => {
        // 取不到就一直是首字母（那一行照样能点、能发）。
      });
    return () => {
      alive = false;
    };
  });

  /** 标签是别的应用给的字符串，空标签（只有包名的裸包）退成「应」。 */
  const initial = $derived(label.trim().charAt(0) || pkg.charAt(0) || t("应"));
</script>

{#if url}
  <img class="aav aav-img" src={url} alt="" />
{:else}
  <span class="aav">{initial}</span>
{/if}