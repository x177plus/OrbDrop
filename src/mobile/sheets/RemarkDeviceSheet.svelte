<script lang="ts">
  import Sheet from "../components/Sheet.svelte";
  import * as store from "../store.svelte";
  import * as devices from "$lib/stores/devices.svelte";
  import { t } from "$lib/i18n.svelte";

  // 移动端的「设备备注」底板 —— 与桌面端那一张（`App.svelte` 的 `remarkState` 对话框）同一件事、
  // 同一套规则：只写本机 `config.toml` 的 `deviceRemarks`，不发给对端、也不改对方的设备名。
  // 两边的字段与文案要对着核（改一处就核另一处）。
  let { peerId, onClose }: { peerId: string; onClose: () => void } = $props();

  const d = $derived(devices.deviceById(peerId));

  // 初值只取一次：输入框里是"用户正在改的东西"，后端事件随后再刷新设备表也不该把它冲掉。
  const open = devices.deviceById(peerId);
  /** 对端**自己的设备名**（不含备注）：提示里说的是这台机器，不是它的显示名。 */
  const rawName = open?.name ?? "";
  /** 打开那一刻后端里存着的备注（'' = 没备注）：用它决定画不画「清除备注」。 */
  const saved = open?.remark ?? "";
  let value = $state(saved);
  let busy = $state(false);

  // 设备没了（对端注销 / 别处解除了配对）这张表就没有主语：收起。
  $effect(() => {
    if (!d) onClose();
  });

  /** 保存（空串即清除）：`store.saveRemark` 报"这台设备的备注改成了什么"，失败留在原地。 */
  async function save(): Promise<void> {
    if (busy || !d) return;
    busy = true;
    try {
      await store.saveRemark(d.id, rawName, value);
    } catch (e) {
      store.toast(t("保存备注失败：{error}", { error: e instanceof Error ? e.message : String(e) }));
      return;
    } finally {
      busy = false;
    }
    onClose();
  }

  /** 「清除备注」：先把输入框置空再走保存 —— 与"手动删光再点保存"落到同一段代码上，不另开一条路。 */
  async function clear(): Promise<void> {
    value = "";
    await save();
  }
</script>

<Sheet title={t("设备备注")} desc={t("备注只保存在本机，不会发给对端，也不改对方自己的设备名。")} onClose={onClose}>
  <p class="field-hint">{t("当前设备名：{name}", { name: rawName })}</p>
  <div class="field">
    <label for="remarkInput">{t("备注")}</label>
    <input class="input" id="remarkInput" maxlength="24" placeholder={t("例如：客厅台式机")} bind:value={value} />
    <p class="field-hint">
      {t("设好之后设备页那一行显示成「备注(设备名)」，会话与标题只写备注本身；留空保存即清除。")}
    </p>
  </div>

  {#snippet foot()}
    {#if saved}
      <button class="btn btn-secondary" type="button" onclick={() => void clear()} disabled={busy}>{t("清除备注")}</button>
    {/if}
    <button class="btn btn-secondary" type="button" onclick={onClose}>{t("取消")}</button>
    <button class="btn btn-primary" type="button" onclick={() => void save()} disabled={busy}>{t("保存")}</button>
  {/snippet}
</Sheet>