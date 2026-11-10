<script lang="ts">
  import Sheet from "../components/Sheet.svelte";
  import * as store from "../store.svelte";
  import * as shares from "$lib/stores/shares.svelte";
  import { t } from "$lib/i18n.svelte";

  // 与桌面端 `rename_share` 同一件事：只改 `config.toml` 里的显示名，不碰磁盘上的文件名。
  let { shareId, onClose }: { shareId: string; onClose: () => void } = $props();

  const share = $derived(shares.getShares().find((s) => s.id === shareId));
  let value = $state("");
  let seeded = false;
  let busy = $state(false);

  // 初值只取一次：输入框是"用户正在改的东西"，后端随后刷新共享项列表也不该把它冲掉。
  // 不在 `$state(...)` 里直接读 `share`（那是"只捕获初始值"的写法，Svelte 会告警）。
  $effect(() => {
    if (seeded) return;
    const s = share;
    if (!s) return;
    value = s.name;
    seeded = true;
  });

  async function save(): Promise<void> {
    const s = share;
    if (!s || busy) return;
    const name = value.trim();
    if (!name) {
      store.toast(t("名称不能为空"));
      return;
    }
    if (name === s.name) {
      onClose();
      return;
    }
    busy = true;
    try {
      await shares.renameShare(s.id, name);
      store.toast(t("已重命名共享项"), "check");
      onClose();
    } catch (e) {
      store.failed(t("重命名共享项"), e);
    } finally {
      busy = false;
    }
  }
</script>

<Sheet
  title={t("重命名共享项")}
  desc={t("只改显示名，不会重命名手机上的文件或文件夹。")}
  onClose={onClose}
>
  {#if !share}
    <div class="empty"><p>{t("共享项已不存在")}</p></div>
  {:else}
    <div class="field">
      <label for="shareNameInput">{t("名称")}</label>
      <input
        class="input"
        id="shareNameInput"
        maxlength="80"
        bind:value={value}
        onkeydown={(e) => {
          if (e.key === "Enter") void save();
        }}
      />
    </div>
  {/if}

  {#snippet foot()}
    <button class="btn btn-secondary" type="button" onclick={onClose}>{t("取消")}</button>
    <button class="btn btn-primary" type="button" disabled={busy || !share} onclick={() => void save()}>
      {t("保存")}
    </button>
  {/snippet}
</Sheet>