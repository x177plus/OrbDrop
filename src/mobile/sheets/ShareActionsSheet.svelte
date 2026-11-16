<script lang="ts">
  import Sheet from "../components/Sheet.svelte";
  import Icon from "../Icon.svelte";
  import * as api from "$lib/api";
  import * as store from "../store.svelte";
  import * as shares from "$lib/stores/shares.svelte";
  import { openLocalShareEntry } from "../shareTree";
  import { t } from "$lib/i18n.svelte";

  /**
   * 本机共享项的操作表：复制下载链接 / 重命名 / 移除。
   *
   * 读写删本身不在这里 —— 那是已连接设备通过 `/dav` 做的事（桌面端 BC 那条路）；
   * 手机这一侧只管理"共享项"这个登记本身，和桌面端 `LocalTab` 的右键菜单同一套语义。
   */
  let { shareId, onClose }: { shareId: string; onClose: () => void } = $props();

  const share = $derived(shares.getShares().find((s) => s.id === shareId));

  /** 共享项本身就是一个文件（例如 APK）：与行上短按同一个落点，交给系统应用打开。 */
  async function open(): Promise<void> {
    const s = share;
    if (!s || s.isDir) return;
    try {
      await openLocalShareEntry(s, "");
      onClose();
    } catch (e) {
      store.failed(t("打开"), e);
    }
  }

  async function copyLink(): Promise<void> {
    const s = share;
    if (!s) return;
    if (!s.broadcastEnabled) {
      store.toast(t("先打开广播开关，其他设备才能下载"));
      return;
    }
    try {
      const url = await api.shareDownloadUrl(s.id, "", s.name, s.isDir);
      await api.copyText(url);
      store.toast(
        s.isDir ? t("已复制下载链接（目录将打包为 ZIP）") : t("已复制下载链接"),
        "link",
      );
    } catch (e) {
      store.failed(t("复制下载链接"), e);
    }
  }

  function rename(): void {
    if (!share) return;
    store.openSheet({ kind: "renameShare", shareId: share.id });
  }

  async function remove(): Promise<void> {
    const s = share;
    if (!s) return;
    const ok = await store.confirmAction({
      title: t("移除共享项"),
      desc: t("「{name}」将从共享项中移除；手机上的文件不会被删除。", { name: s.name }),
      confirmLabel: t("移除共享项"),
      danger: true,
    });
    if (!ok) return;
    try {
      await shares.removeShare(s.id);
      store.toast(t("已移除共享项"), "trash");
      onClose();
    } catch (e) {
      store.failed(t("移除共享项"), e);
    }
  }
</script>

<Sheet
  title={share?.name ?? t("共享项操作")}
  desc={share?.path ?? ""}
  label={t("共享项操作")}
  onClose={onClose}
>
  {#if !share}
    <div class="empty"><Icon name="folder" /><p>{t("共享项已不存在")}</p></div>
  {:else}
    {#if !share.isDir}
      <button class="opt" type="button" onclick={() => void open()}>
        <span class="oi"><Icon name="file" /></span>
        <span class="om">
          <span class="on">{t("打开")}</span>
          <span class="od">{t("用系统应用打开这份文件。")}</span>
        </span>
      </button>
    {/if}

    <button class="opt" type="button" onclick={() => void copyLink()}>
      <span class="oi"><Icon name="link" /></span>
      <span class="om">
        <span class="on">{t("复制下载链接")}</span>
        <span class="od">{t("同一局域网里的设备用浏览器打开即可下载，不需要配对。")}</span>
      </span>
    </button>

    <button class="opt" type="button" onclick={rename}>
      <span class="oi"><Icon name="edit" /></span>
      <span class="om">
        <span class="on">{t("重命名")}</span>
        <span class="od">{t("只改共享项显示名，不改手机上的文件名。")}</span>
      </span>
    </button>

    <button class="opt danger" type="button" onclick={() => void remove()}>
      <span class="oi"><Icon name="trash" /></span>
      <span class="om">
        <span class="on">{t("移除共享项")}</span>
        <span class="od">{t("只取消共享，不会删除手机上的文件。")}</span>
      </span>
    </button>
  {/if}

  {#snippet foot()}
    <button class="btn btn-secondary" type="button" onclick={onClose}>{t("取消")}</button>
  {/snippet}
</Sheet>