<script lang="ts">
  /**
   * 别人发来的一条长连接请求（F-16 的同意制；桌面端是右下角那条带「接受/拒绝」的通知）。
   *
   * 手机上那组通知气泡不存在，所以用一张底板接住；有效期与后端一致（`link::REQUEST_TTL` = 60s），
   * 到点由 `link://request_closed` 把这张表收掉（见 `events.ts`）。与配对邀请同一取舍：
   * 60 秒就要过期，等不起，收到就当场开在脸上。
   */
  import Sheet from "../components/Sheet.svelte";
  import Icon from "../Icon.svelte";
  import * as store from "../store.svelte";
  import { t } from "$lib/i18n.svelte";

  let { onClose }: { onClose: () => void } = $props();

  const req = $derived(store.incomingLinkRequest());

  /** 与后端 `link::REQUEST_TTL` 同一个数：这里只是把它显示出来。 */
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
    await store.answerLinkRequest(accept);
    onClose();
  }
</script>

<Sheet
  title={t("连接请求")}
  desc={req ? t("来自「{name}」", { name: req.peerName }) : ""}
  label={t("连接请求")}
  onClose={onClose}
>
  {#if req}
    <div class="opt">
      <span class="oi"><Icon name="link" /></span>
      <span class="om">
        <span class="on">{t("接受后对方会与本机建立长连接")}</span>
        <span class="od">{t("实时通道：共享项变更广播、消息即时送达。")}</span>
      </span>
    </div>
    <p class="field-hint">{t("60 秒内未处理自动作废（剩余 {n} 秒）。", { n: left })}</p>
  {/if}

  {#snippet foot()}
    <button class="btn btn-secondary" type="button" onclick={() => void answer(false)}>{t("拒绝")}</button>
    <button class="btn btn-primary" type="button" onclick={() => void answer(true)}>{t("接受")}</button>
  {/snippet}
</Sheet>