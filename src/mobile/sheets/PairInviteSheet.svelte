<script lang="ts">
  /**
   * 别人发来的配对邀请（桌面端是右下角那条带「接收/拒绝」的通知）。
   *
   * 手机上那组通知气泡不存在，所以用一张底板接住；有效期与后端一致（`pair::INVITE_TTL` = 60s），
   * 到点由 `pair://invite_closed` 把这张表收掉（见 `events.ts`）。**身份未经校验**（`unproven`）
   * 的那一档要写在明面上：那台自称是本机已配对设备却没带 token —— 它可能是换了新地址的老搭档，
   * 也可能不是，决定权在这一屏。
   */
  import Sheet from "../components/Sheet.svelte";
  import Icon from "../Icon.svelte";
  import * as store from "../store.svelte";

  let { onClose }: { onClose: () => void } = $props();

  const invite = $derived(store.incomingPairInvite());

  /** 与后端 `pair::INVITE_TTL` 同一个数：这里只是把它显示出来。 */
  const TTL = 60;
  let now = $state(Date.now());
  $effect(() => {
    const timer = setInterval(() => (now = Date.now()), 1000);
    return () => clearInterval(timer);
  });
  // 剩余秒数是**本地观测**：从这张表被打开那一刻起算（后端那份是它自己的 60s，
  // 两者差一个网络往返，用户看到的差别只有一秒以内）。
  const arrivedAt = Date.now();
  const left = $derived(Math.max(0, TTL - Math.floor((now - arrivedAt) / 1000)));

  async function answer(accept: boolean): Promise<void> {
    await store.answerPairInvite(accept);
    onClose();
  }
</script>

<Sheet title="配对邀请" desc={invite ? `来自「${invite.peerName}」` : ""} label="配对邀请" onClose={onClose}>
  {#if invite}
    <div class="opt">
      <span class="oi"><Icon name="link" /></span>
      <span class="om">
        <span class="on">接受后两台设备互信</span>
        <span class="od">双方各存一枚 token，之后即可互相发消息、收文件。</span>
      </span>
    </div>
    {#if invite.unproven}
      <p class="field-hint" style="color:var(--warn)">
        这台设备自称是本机已配对的设备，但没能出示凭据（它可能只是换了地址，也可能不是）。接受前请确认对方身份。
      </p>
    {/if}
    {#if invite.sas}
      <div class="code-box">
        <span class="sas-code">{invite.sas}</span>
        <span class="code-hint">和对方屏幕上的这串一致再点「接收」；不一致请拒绝。</span>
      </div>
    {:else}
      <p class="field-hint" style="color:var(--warn)">
        对方版本过旧，无法核对身份；建议拒绝并改用配对码。
      </p>
    {/if}
    <p class="field-hint">剩余 {left} 秒，超时自动作废。</p>
  {/if}

  {#snippet foot()}
    <button class="btn btn-secondary" type="button" onclick={() => void answer(false)}>拒绝</button>
    <button class="btn btn-primary" type="button" onclick={() => void answer(true)}>接收</button>
  {/snippet}
</Sheet>