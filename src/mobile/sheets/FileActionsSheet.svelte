<script lang="ts">
  import Sheet from "../components/Sheet.svelte";
  import Icon from "../Icon.svelte";
  import * as store from "../store.svelte";
  import * as api from "$lib/api";
  import type { InboxEntry } from "$lib/types";
  import { t } from "$lib/i18n.svelte";

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
    if (d.toDateString() === now.toDateString()) return t("今天 {time}", { time: hm });
    const yesterday = new Date(now);
    yesterday.setDate(now.getDate() - 1);
    if (d.toDateString() === yesterday.toDateString()) return t("昨天 {time}", { time: hm });
    return t("{month}月{day}日", { month: d.getMonth() + 1, day: d.getDate() });
  }

  /** 表头那行：`发送方 · 大小 · 时间`（`from` 为空 = 本机自己下载的，省掉那一段）。 */
  const meta = $derived(
    [entry.from, entry.isDir ? t("文件夹") : fmtBytes(entry.size), shortTime(entry.modified)]
      .filter((s) => s !== "")
      .join(" · "),
  );

  /**
   * 这份是不是 APK。是的话第一项从「打开」改成「安装」——安卓上 `.apk` 交给系统安装器才是
   * 唯一"点一下就把东西装上"的路：后端按扩展名给 `application/vnd.android.package-archive`，
   * 系统安装器认领那个 MIME（见 `docs/p1-implementation.md` §20）。
   */
  const isApk = $derived(entry.name.toLowerCase().endsWith(".apk"));

  /**
   * 三项动作都**先把要用的值取出来、再关表**。
   *
   * 为什么不能先关表：`onClose()` 一下架这张表，这个组件的 props 当场就读不到了 —— 实测
   * `打开`/`复制路径`/`删除` 任何一项都只会得到一句
   * `…失败：Cannot read properties of null (reading 'entry')`（`onClose()` 之后那句
   * `entry.path` 抛的）。以前没人发现，是因为后端的 `open_path` 在安卓上本来就坏着
   * （见 `docs/p1-implementation.md` §20），这条前端 bug 只是把它遮住了。
   */
  async function open(): Promise<void> {
    const { path } = entry;
    const apk = isApk;
    onClose();
    try {
      await api.openPath(path);
      store.toast(apk ? t("已交给系统安装器安装") : t("已用系统应用打开"), apk ? "download" : "file");
    } catch (e) {
      store.toast(
        apk
          ? t("安装失败：{error}", { error: e instanceof Error ? e.message : String(e) })
          : t("打开失败：{error}", { error: e instanceof Error ? e.message : String(e) }),
      );
    }
  }

  async function copyPath(): Promise<void> {
    const path = entry.path;
    onClose();
    try {
      // 原型拼的是 `settings.inboxDir + '/' + name`（那份设置是 mock）；真实后端把完整路径
      // 随着收件项一起给全了，不自己拼，免得目录名一改就指到不存在的地方。
      await navigator.clipboard.writeText(path);
      store.toast(t("已复制路径"), "copy");
    } catch (e) {
      store.toast(t("复制路径失败：{error}", { error: e instanceof Error ? e.message : String(e) }));
    }
  }

  async function remove(): Promise<void> {
    // 后端是直接删盘上这一份（没有回收站），删错了找不回来；原型敢直接删是因为列表全是 mock。
    // 确认框走应用自己的 Sheet（`store.confirmAction`），不再用 WebView 的系统 `confirm`。
    const { name, path } = entry;
    const ok = await store.confirmAction({
      title: t("删除本地副本"),
      desc: t("「{name}」将从收件目录中移除。此操作不可撤销。", { name }),
      confirmLabel: t("删除"),
      danger: true,
    });
    if (!ok) return;
    onClose();
    try {
      await store.deleteInboxEntry(path);
      store.toast(t("已删除本地副本"), "trash");
    } catch (e) {
      store.toast(t("删除失败：{error}", { error: e instanceof Error ? e.message : String(e) }));
    }
  }
</script>

<!-- 「分享」这一项本阶段不做：系统分享菜单要原生 Intent，后端还没有这条命令，放上去也是假按钮。 -->
<Sheet title={entry.name} desc={meta} label={t("文件操作")} onClose={onClose}>
  <button class="opt" type="button" onclick={open}>
    <span class="oi"><Icon name={isApk ? "download" : "file"} /></span>
    <span class="om">
      <span class="on">{isApk ? t("安装") : t("打开")}</span>
      <span class="od">
        {isApk ? t("交给系统安装器安装这个 APK（首次需先允许 OrbDrop 安装未知应用）。") : t("用系统应用打开这份文件。")}
      </span>
    </span>
  </button>
  <button class="opt" type="button" onclick={copyPath}>
    <span class="oi"><Icon name="copy" /></span>
    <span class="om"><span class="on">{t("复制路径")}</span><span class="od">{t("复制收件目录中的完整路径。")}</span></span>
  </button>
  <button class="opt danger" type="button" onclick={remove}>
    <span class="oi"><Icon name="trash" /></span>
    <span class="om"><span class="on">{t("删除本地副本")}</span><span class="od">{t("仅删除本机收件目录中的这一份。")}</span></span>
  </button>
</Sheet>