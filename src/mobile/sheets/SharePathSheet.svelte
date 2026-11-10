<script lang="ts">
  import { onMount } from "svelte";
  import Sheet from "../components/Sheet.svelte";
  import Icon from "../Icon.svelte";
  import FileTile from "../components/FileTile.svelte";
  import * as api from "$lib/api";
  import * as store from "../store.svelte";
  import * as shares from "$lib/stores/shares.svelte";
  import { formatFileSize } from "$lib/types";
  import type { AndroidShareListing, AndroidShareRoot } from "$lib/types";
  import { t } from "$lib/i18n.svelte";

  /**
   * 「本机」段的添加入口：在共享存储里选一个文件或文件夹。
   *
   * 与桌面端的 `FilePicker.svelte` 是同一件事，但安卓上不能复用桌面那套 `fs_browse`
   * （分区存储列不出共享存储）。这里走「所有文件访问」拿到的**真实路径**：原生列目录，
   * 选中的路径直接交给跨端 `add_share`；后端共享链路（/shares、/tree、/file、/dav）
   * 与桌面端完全同一套。
   *
   * 权限是**按需**申请的：第一下先看 `android_share_state`；没授权时这张表只显示说明与
   * 「去授权」，用户从系统页回来后由 `visibilitychange` 重读 —— 不假装知道授权结果。
   */
  let { onClose }: { onClose: () => void } = $props();

  const access = $derived(store.getShareAccess());
  let roots = $state<AndroidShareRoot[]>([]);
  let current = $state<string | null>(null);
  let listing = $state<AndroidShareListing | null>(null);
  let loading = $state(false);
  let busy = $state(false);
  let error = $state<string | null>(null);

  /** 读权限状态；授权后第一次进入时把根目录与第一层拉出来。 */
  async function refreshAccess(): Promise<void> {
    await store.refreshShareAccess();
    const a = store.getShareAccess();
    if (a?.supported && a.granted && (roots.length === 0 || !listing)) await start();
  }

  /** 拉共享存储的根，并打开第一条（通常是内部存储）。 */
  async function start(): Promise<void> {
    loading = true;
    error = null;
    try {
      roots = await api.androidShareRoots();
      if (!roots.length) {
        error = t("没有可用的共享存储");
        return;
      }
      let first = roots[0].path;
      if (current && roots.some((r) => current === r.path || current?.startsWith(r.path + "/"))) {
        first = current;
      }
      await open(first);
    } catch (e) {
      error = e instanceof Error ? e.message : String(e);
    } finally {
      loading = false;
    }
  }

  /** 打开一层；目录行点击进入，文件行点击直接登记。 */
  async function open(path: string): Promise<void> {
    loading = true;
    error = null;
    try {
      listing = await api.androidShareList(path);
      current = path;
    } catch (e) {
      listing = null;
      error = e instanceof Error ? e.message : String(e);
    } finally {
      loading = false;
    }
  }

  /**
   * 把一条真实路径登记成共享项。路径重复时后端按路径去重、返回已存在那一项 ——
   * 这里先查一遍本机列表，把「已添加」与「已经在共享项里」分开说，和桌面端同一句话。
   */
  async function add(path: string): Promise<void> {
    if (busy) return;
    busy = true;
    try {
      // 后端按路径去重、重复登记时返回**已经存在的那一项**（id 不变）。先记下已知 id，
      // 回来后按 id 判断是"新加"还是"本来就有" —— 只比字符串会漏掉大小写/结尾斜杠这类写法。
      const known = new Set(shares.getShares().map((s) => s.id));
      const item = await shares.addShare(path);
      const added = !known.has(item.id);
      store.toast(
        added ? t("已添加 {added} 个共享项", { added: 1 }) : t("这项已经在共享项里了"),
        added ? "check" : "share",
      );
      store.setSeg("local");
      onClose();
    } catch (e) {
      store.failed(t("添加共享项"), e);
    } finally {
      busy = false;
    }
  }

  /** 打开系统「所有文件访问」页。授权结果不在这条命令里，回前台时重读。 */
  async function grant(): Promise<void> {
    try {
      await store.requestShareAccess();
      store.toast(t("已打开系统设置：打开「所有文件访问」后返回"), "check");
    } catch (e) {
      store.failed(t("打开「所有文件访问」设置"), e);
    }
  }

  onMount(() => {
    void refreshAccess();
    // 从系统设置页回来时 WebView 重新可见：权限可能已经变了，重读一次。
    const onVisible = () => {
      if (document.visibilityState === "visible") void refreshAccess();
    };
    document.addEventListener("visibilitychange", onVisible);
    return () => document.removeEventListener("visibilitychange", onVisible);
  });
</script>

<Sheet
  title={t("选择共享项")}
  desc={t("只共享你选中的这一项；手机上的其他文件不会因为授权而被共享。")}
  onClose={onClose}
>
  {#if access === null}
    <div class="empty"><Icon name="folder" /><p>{t("正在读取…")}</p></div>
  {:else if !access.supported}
    <div class="card gcard">
      <div class="srow">
        <div class="srow-top"><span class="slabel">{t("需要「所有文件访问」权限")}</span></div>
        <p class="shelp">{t("直接共享手机目录需要 Android 11 及以上；这台系统版本不支持。")}</p>
      </div>
    </div>
  {:else if !access.granted}
    <div class="card gcard">
      <div class="srow">
        <div class="srow-top"><span class="slabel">{t("需要「所有文件访问」权限")}</span></div>
        <p class="shelp">
          {t("OrbDrop 只会共享你在这里明确选择的文件或文件夹；权限本身不会读取或上传任何内容。")}
        </p>
        <div class="perm-acts">
          <button class="btn btn-primary btn-sm" type="button" onclick={() => void grant()}>
            {t("去授权")}
          </button>
          <button class="btn btn-secondary btn-sm" type="button" onclick={() => void refreshAccess()}>
            {t("重新检查")}
          </button>
        </div>
      </div>
    </div>
  {:else}
    <p class="field-hint path-hint">{current ?? ""}</p>

    {#if roots.length > 1}
      <div class="chips">
        {#each roots as r (r.path)}
          <button
            class="chip"
            class:active={current === r.path || (current?.startsWith(r.path + "/") ?? false)}
            type="button"
            onclick={() => void open(r.path)}
          >
            <Icon name="folder" />{r.name}
          </button>
        {/each}
      </div>
    {/if}

    {#if error}
      <div class="empty">
        <Icon name="x" />
        <p>{t("读取目录失败")}</p>
        <p class="es">{error}</p>
      </div>
    {:else if loading}
      <div class="empty"><Icon name="folder" /><p>{t("正在读取…")}</p></div>
    {:else if listing}
      <div class="card list">
        {#if listing.parent}
          <button
            class="frow"
            type="button"
            onclick={() => {
              if (listing?.parent) void open(listing.parent);
            }}
          >
            <span class="ftile folder"><Icon name="chev" cls="up" /></span>
            <span class="fmain"><span class="fname2">{t("上一级")}</span></span>
          </button>
        {/if}
        {#each listing.entries as e (e.path)}
          <button
            class="frow"
            type="button"
            onclick={() => (e.isDir ? void open(e.path) : void add(e.path))}
          >
            <FileTile name={e.name} isDir={e.isDir} />
            <span class="fmain">
              <span class="fname2">{e.name}</span>
              <span class="fmeta">{e.isDir ? t("文件夹") : formatFileSize(e.size)}</span>
            </span>
            <span class="fgo" aria-hidden="true">
              <Icon name={e.isDir ? "chev" : "plus"} />
            </span>
          </button>
        {:else}
          <div class="empty"><Icon name="folder" /><p>{t("空文件夹")}</p></div>
        {/each}
      </div>
      {#if listing.truncated}
        <p class="field-hint">
          {t("只显示前 {n} 项，进入子目录继续查看。", { n: listing.entries.length })}
        </p>
      {/if}
    {/if}
  {/if}

  {#snippet foot()}
    <button class="btn btn-secondary" type="button" onclick={onClose}>{t("取消")}</button>
    {#if access?.supported && access.granted}
      <button
        class="btn btn-primary"
        type="button"
        disabled={!current || loading || busy}
        onclick={() => current && void add(current)}
      >
        {t("共享此文件夹")}
      </button>
    {/if}
  {/snippet}
</Sheet>