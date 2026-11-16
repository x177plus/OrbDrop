<script lang="ts">
  import Sheet from "../components/Sheet.svelte";
  import Icon from "../Icon.svelte";
  import * as api from "$lib/api";
  import * as store from "../store.svelte";
  import * as shares from "$lib/stores/shares.svelte";
  import { formatFileSize } from "$lib/types";
  import { shareAbsPath, openLocalShareEntry } from "../shareTree";
  import { t } from "$lib/i18n.svelte";

  // 长按本机共享目录里的一项（不是共享项根）：文件给「打开」，两项都给「复制下载链接」
  // 与「复制路径」，最后是删除这一份。共享项根走 `ShareActionsSheet`（重命名 / 移除共享项）。
  let {
    shareId,
    path,
    name,
    isDir,
    size,
    onClose,
  }: {
    shareId: string;
    /** 共享项内相对路径（不会是空串：空串就是共享项根，走另一张表）。 */
    path: string;
    name: string;
    isDir: boolean;
    size: number;
    onClose: () => void;
  } = $props();

  const share = $derived(shares.getShares().find((s) => s.id === shareId));
  const meta = $derived(isDir ? t('文件夹') : formatFileSize(size));

  // 确认框会把这张表**换掉**（同一时刻只有一张底板），组件随即卸载；而父级传下来的
  // `path` / `name` 等是本模板里对 `sheet` 的动态读取 —— 确认之后再回头读它们，
  // `sheet` 已经是 null，getter 会抛 "Cannot read properties of null"。
  // 所以每个动作一开始就把跨 await 要用的值抄成局部快照。
  function snapshot() {
    return { rel: path, label: name, dir: isDir, close: onClose };
  }

  async function open(): Promise<void> {
    const s = share;
    if (!s) return;
    const { rel, close } = snapshot();
    try {
      await openLocalShareEntry(s, rel);
      close();
    } catch (e) {
      store.failed(t('打开'), e);
    }
  }

  async function copyLink(): Promise<void> {
    const s = share;
    if (!s) return;
    const { rel, label, dir, close } = snapshot();
    if (!s.broadcastEnabled) {
      store.toast(t('先打开广播开关，其他设备才能下载'));
      return;
    }
    try {
      const url = await api.shareDownloadUrl(s.id, rel, label, dir);
      await api.copyText(url);
      store.toast(
        dir ? t('已复制下载链接（目录将打包为 ZIP）') : t('已复制下载链接'),
        'link',
      );
      close();
    } catch (e) {
      store.failed(t('复制下载链接'), e);
    }
  }

  async function copyPath(): Promise<void> {
    const s = share;
    if (!s) return;
    const { rel, close } = snapshot();
    try {
      await api.copyText(shareAbsPath(s, rel));
      store.toast(t('已复制路径'), 'copy');
      close();
    } catch (e) {
      store.failed(t('复制路径'), e);
    }
  }

  async function remove(): Promise<void> {
    const s = share;
    if (!s) return;
    const { rel, label, dir, close } = snapshot();
    const ok = await store.confirmAction({
      title: dir ? t('删除本机文件夹') : t('删除本机文件'),
      desc: dir
        ? t('将从本机磁盘上删除文件夹「{name}」及其全部内容，无法恢复。', { name: label })
        : t('将从本机磁盘上删除「{name}」，无法恢复。', { name: label }),
      confirmLabel: t('删除'),
      danger: true,
    });
    if (!ok) return;
    try {
      await api.deleteShareEntry(s.id, rel);
      store.toast(t('已删除「{name}」', { name: label }), 'trash');
      // 本机树就地刷新（与桌面端删除后派 `od:entry-deleted` 同一个思路）：根层由
      // `FilesTab` 重拉，已展开的深层节点各自按 `refreshSignal` 重拉自己那一层。
      document.dispatchEvent(new CustomEvent('od:local-tree-changed', { detail: { shareId: s.id } }));
      close();
    } catch (e) {
      store.failed(t('删除'), e);
    }
  }
</script>

<Sheet title={name} desc={meta} label={t('共享内容操作')} onClose={onClose}>
  {#if !share}
    <div class="empty"><Icon name="folder" /><p>{t('共享项已不存在')}</p></div>
  {:else}
    {#if !isDir}
      <button class="opt" type="button" onclick={() => void open()}>
        <span class="oi"><Icon name="file" /></span>
        <span class="om">
          <span class="on">{t('打开')}</span>
          <span class="od">{t('用系统应用打开这份文件。')}</span>
        </span>
      </button>
    {/if}

    <button class="opt" type="button" onclick={() => void copyLink()}>
      <span class="oi"><Icon name="link" /></span>
      <span class="om">
        <span class="on">{t('复制下载链接')}</span>
        <span class="od">{t('同一局域网里的设备用浏览器打开即可下载，不需要配对。')}</span>
      </span>
    </button>

    <button class="opt" type="button" onclick={() => void copyPath()}>
      <span class="oi"><Icon name="copy" /></span>
      <span class="om">
        <span class="on">{t('复制路径')}</span>
        <span class="od">{t('复制手机上的完整路径。')}</span>
      </span>
    </button>

    <button class="opt danger" type="button" onclick={() => void remove()}>
      <span class="oi"><Icon name="trash" /></span>
      <span class="om">
        <span class="on">{isDir ? t('删除本机文件夹') : t('删除本机文件')}</span>
        <span class="od">{t('从手机磁盘上删除这一项，无法恢复。')}</span>
      </span>
    </button>
  {/if}

  {#snippet foot()}
    <button class="btn btn-secondary" type="button" onclick={onClose}>{t('取消')}</button>
  {/snippet}
</Sheet>