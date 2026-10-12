<script lang="ts">
  import Sheet from "../components/Sheet.svelte";
  import { formatFileSize, updateStatusText, updateTitleText } from "$lib/types";
  import * as upd from "$lib/stores/update.svelte";
  import * as store from "../store.svelte";

  // 「发现新版本」：启动时的静默检查（后端 `check(force = false)`，起来 8 秒后跑）在手机上
  // 落到这一张板 —— 移动端没有桌面那组右下角气泡，而"有新版本"是要用户当场决定的事。
  //
  // 与桌面上那条通知气泡**同一份状态、同一份文案**（`updateTitleText` / `updateStatusText`）：
  // 点过「下载并安装」之后这一张板就从"提示"变成"进度"（阶段由后端推，见 `update://state`）。
  // 下载中关掉这张板**不会停下载** —— 只是本次运行不再自动弹（`dismissUpdatePrompt`），
  // 进度在设置页「版本与更新」那一格照旧看得见（同 F-110 §8）。
  let { onClose }: { onClose: () => void } = $props();

  const update = $derived(upd.getUpdateState());
  const busy = $derived(
    update != null && ["downloading", "verifying", "applying"].includes(update.phase),
  );
  const pct = $derived(
    update?.size ? Math.min(100, Math.round((update.received / update.size) * 100)) : 0,
  );

  /** 「稍后」：收起这张板，本次运行内不再自动弹（下次启动会重新检查、重新弹）。 */
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
      store.failed("更新", e);
    }
  }
</script>

<Sheet
  title={update ? updateTitleText(update) : "发现新版本"}
  desc={update?.version ? `v${update.version}${update.size ? ` · ${formatFileSize(update.size)}` : ""}` : ""}
  label="版本更新"
  onClose={later}
>
  {#if update}
    {#if update.phase === "available"}
      <p class="shelp">{update.notes ?? "这一版没有写更新说明。"}</p>
      <p class="shelp">下载完会打开系统安装器，需要你确认一次安装（非系统应用不能静默安装）。</p>
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
      <button class="btn btn-secondary" type="button" onclick={later}>稍后</button>
      <button class="btn btn-primary" type="button" disabled={upd.isApplying()} onclick={() => void install()}>
        {upd.isApplying() ? "准备中…" : "下载并安装"}
      </button>
    {:else if update?.phase === "error"}
      <button class="btn btn-secondary" type="button" onclick={later}>关闭</button>
      <button class="btn btn-primary" type="button" disabled={upd.isApplying()} onclick={() => void install()}>
        重试
      </button>
    {/if}
  {/snippet}
</Sheet>