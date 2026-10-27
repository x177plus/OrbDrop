<script lang="ts">
  import Sheet from "../components/Sheet.svelte";
  import { formatFileSize, updateStatusText, updateTitleText } from "$lib/types";
  import * as upd from "$lib/stores/update.svelte";
  import UpdateNotes from "$lib/components/UpdateNotes.svelte";
  import * as store from "../store.svelte";
  import { t } from "$lib/i18n.svelte";

  // 「发现新版本」：后台静默检查（启动 8 秒后 / 运行期每 6 小时 / 回前台，后端 `check(force = false)`）
  // 在手机上落到这一张板 —— 移动端没有桌面那组右下角气泡，而"有新版本"是要用户当场决定的事。
  //
  // 与桌面上那条通知气泡**同一份状态、同一份文案**（`updateTitleText` / `updateStatusText`）：
  // 点过「下载并安装」之后这一张板就从"提示"变成"进度"（阶段由后端推，见 `update://state`）。
  // 下载中关掉这张板**不会停下载** —— 只是这一版不再自动弹（`dismissUpdatePrompt`，
  // 按版本落盘），进度在设置页「版本与更新」那一格照旧看得见（同 F-110 §8）。
  let { onClose }: { onClose: () => void } = $props();

  const update = $derived(upd.getUpdateState());
  const busy = $derived(
    update != null && ["downloading", "verifying", "applying"].includes(update.phase),
  );
  const pct = $derived(
    update?.size ? Math.min(100, Math.round((update.received / update.size) * 100)) : 0,
  );

  /** 「稍后」：收起这张板，**这一版**不再自动弹（角标仍有；更新的一版照弹）。 */
  function later(): void {
    upd.dismissUpdatePrompt();
    onClose();
  }

  /** 「下载并安装」：下载 → 校验 → 交给系统安装器（用户确认一次），安装会杀掉本进程。 */
  async function install(): Promise<void> {
    try {
      await upd.installUpdate();
    } catch (e) {
      // 失败要说出来：后端那条报错讲的是真因（镜像都不通 / 校验不过 / 没开"安装未知应用"）。
      store.toast(t("更新失败：{error}", { error: e instanceof Error ? e.message : String(e) }));
    }
  }
</script>

<Sheet
  title={update ? updateTitleText(update) : t("发现新版本")}
  desc={update?.version ? `v${update.version}${update.size ? ` · ${formatFileSize(update.size)}` : ""}` : ""}
  label={t("版本更新")}
  onClose={later}
>
  {#if update}
    {#if update.phase === "available"}
      <UpdateNotes notes={update.notes} />
      <p class="shelp">{t("下载完会打开系统安装器，需要你确认一次安装（非系统应用不能静默安装）。")}</p>
    {:else if busy}
      <div style="height:calc(6 * var(--u)); border-radius:calc(3 * var(--u)); background:var(--fg-soft); overflow:hidden;">
        <div style="height:100%; width:{pct}%; background:var(--accent); transition:width .15s linear;"></div>
      </div>
      <p class="shelp">{updateStatusText(update)}</p>
    {:else}
      <p class="shelp" style="color:var(--danger);">{updateStatusText(update)}</p>
    {/if}
  {/if}

  {#snippet foot()}
    {#if update?.phase === "available"}
      <button class="btn btn-secondary" type="button" onclick={later}>{t("稍后")}</button>
      <button class="btn btn-primary" type="button" disabled={upd.isApplying()} onclick={() => void install()}>
        {upd.isApplying() ? t("准备中…") : t("下载并安装")}
      </button>
    {:else if update?.phase === "error"}
      <button class="btn btn-secondary" type="button" onclick={later}>{t("关闭")}</button>
      <button class="btn btn-primary" type="button" disabled={upd.isApplying()} onclick={() => void install()}>
        {t("重试")}
      </button>
    {/if}
  {/snippet}
</Sheet>