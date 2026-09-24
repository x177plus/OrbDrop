<script lang="ts">
  import Sheet from "../components/Sheet.svelte";
  import Icon from "../Icon.svelte";
  import * as store from "../store.svelte";
  import * as api from "$lib/api";
  import type { InboxEntry } from "$lib/types";

  let { entry, onClose }: { entry: InboxEntry; onClose: () => void } = $props();

  // 下面这两个格式化与收件列表同一口径。契约只把 `entry` 递给这张表，两个文件之间又不许
  // 新开共享模块，所以就地各写一份极小实现（不是第二套约定）。
  function fmtBytes(n: number): string {
    if (n >= 1024 * 1024 * 1024) return `${(n / 1024 / 1024 / 1024).toFixed(2)} GB`;
    if (n >= 1024 * 1024) return `${(n / 1024 / 1024).toFixed(1)} MB`;
    if (n >= 1024) return `${(n / 1024).toFixed(0)} KB`;
    return `${n} B`;
  }

  function shortTime(at: number): string {
    const d = new Date(at);
    const now = new Date();
    const hm = `${String(d.getHours()).padStart(2, "0")}:${String(d.getMinutes()).padStart(2, "0")}`;
    if (d.toDateString() === now.toDateString()) return `今天 ${hm}`;
    const yesterday = new Date(now);
    yesterday.setDate(now.getDate() - 1);
    if (d.toDateString() === yesterday.toDateString()) return `昨天 ${hm}`;
    return `${d.getMonth() + 1}月${d.getDate()}日`;
  }

  /** 表头那行：`发送方 · 大小 · 时间`（`from` 为空 = 本机自己下载的，省掉那一段）。 */
  const meta = $derived(
    [entry.from, entry.isDir ? "文件夹" : fmtBytes(entry.size), shortTime(entry.modified)]
      .filter((s) => s !== "")
      .join(" · "),
  );

  async function open(): Promise<void> {
    onClose();
    try {
      await api.openPath(entry.path);
      store.toast(`已用系统应用打开「${entry.name}」`, "file");
    } catch (e) {
      store.failed("打开", e);
    }
  }

  async function copyPath(): Promise<void> {
    onClose();
    try {
      // 原型拼的是 `settings.inboxDir + '/' + name`（那份设置是 mock）；真实后端把完整路径
      // 随着收件项一起给全了，不自己拼，免得目录名一改就指到不存在的地方。
      await navigator.clipboard.writeText(entry.path);
      store.toast(`已复制路径：${entry.path}`, "copy");
    } catch (e) {
      store.failed("复制路径", e);
    }
  }

  async function remove(): Promise<void> {
    // 后端是直接删盘上这一份（没有回收站），删错了找不回来；原型敢直接删是因为列表全是 mock。
    // 先问一句再动手 —— 移动端 webview 里 `window.confirm` 是原生对话框，拿得到用户的选择。
    if (!confirm(`删除收件目录中的「${entry.name}」？此操作不可撤销。`)) return;
    onClose();
    try {
      await store.deleteInboxEntry(entry.path);
      store.toast(`已删除本地副本「${entry.name}」`, "trash");
    } catch (e) {
      store.failed("删除", e);
    }
  }
</script>

<!-- 「分享」这一项本阶段不做：系统分享菜单要原生 Intent，后端还没有这条命令，放上去也是假按钮。 -->
<Sheet title={entry.name} desc={meta} label="文件操作" onClose={onClose}>
  <button class="opt" type="button" onclick={open}>
    <span class="oi"><Icon name="file" /></span>
    <span class="om"><span class="on">打开</span><span class="od">用系统应用打开这份文件。</span></span>
  </button>
  <button class="opt" type="button" onclick={copyPath}>
    <span class="oi"><Icon name="copy" /></span>
    <span class="om"><span class="on">复制路径</span><span class="od">复制收件目录中的完整路径。</span></span>
  </button>
  <button class="opt danger" type="button" onclick={remove}>
    <span class="oi"><Icon name="trash" /></span>
    <span class="om"><span class="on">删除本地副本</span><span class="od">仅删除本机收件目录中的这一份。</span></span>
  </button>
</Sheet>