<script lang="ts">
  import Sheet from "../components/Sheet.svelte";
  import Icon from "../Icon.svelte";
  import * as store from "../store.svelte";
  import * as devices from "$lib/stores/devices.svelte";
  import * as api from "$lib/api";
  import { t } from "$lib/i18n.svelte";

  let { peerId, onClose }: { peerId: string; onClose: () => void } = $props();

  const d = $derived(devices.deviceById(peerId));

  // 设备在表开着的时候被删掉（对端注销、别处解除了配对）这张表就没有主语了：收起，
  // 而不是留一张指着空气的操作表（原型是 `if (!d) return;` —— 它连表都不开）。
  $effect(() => {
    if (!d) onClose();
  });

  async function toggleLink(): Promise<void> {
    if (!d) return;
    const connected = d.connected;
    await store.toggleConnection(d.id, connected);
    onClose();
  }

  /** 解除配对：问那一句在 `store.unpair` 里（设备行那颗按钮共用同一个判据），这里只负责收表。 */
  async function unpair(): Promise<void> {
    if (!d) return;
    if (await store.unpair(d.id, devices.deviceShortName(d))) onClose();
  }

  /** LocalSend 行只是本机发现结果：移除后它在网上会重新出现，所以文案不说"忘记/解除配对"。 */
  async function removeLocalSend(): Promise<void> {
    if (!d) return;
    const name = devices.deviceShortName(d);
    if (!(await store.confirmAction({
      title: t("从列表移除"),
      desc: t("从本机列表移除「{name}」？它还在网上时会重新出现。", { name }),
      confirmLabel: t("移除"),
      danger: true,
    }))) return;
    try {
      await api.removePeer(d.id);
      store.toast(t("已从列表移除「{name}」", { name }), "check");
      onClose();
    } catch (e) {
      store.failed(t("移除失败"), e);
    }
  }
</script>

<!-- 这一张表就是移动端的**设备右键菜单**（桌面端那份见 `ContextMenu.svelte` 的 `kind === 'device'`）：
     同一套动作、同一套档位规则，改一处就要对着另一处核一遍 —— 两边分叉过的事已经发生过一次
     （这里曾经给未配对 / 离线的行也摆着「建立长连接」，点了只会被拒；桌面端那两档压根不给）。 -->
<Sheet title={d ? devices.deviceShortName(d) : ""} desc={d ? `${d.ip}:${d.port}` : ""} label={t("设备操作")} onClose={onClose}>
  {#if d}
    <!-- 备注（F-101）与桌面端右键菜单那一项（`ContextMenu.svelte`「修改备注」）同规：
         **与在线状态无关**（纯本机数据，离线/未绑定的行照样能改），当前备注就摆在标题里。 -->
    <button class="opt" type="button" onclick={() => store.openSheet({ kind: "remarkDevice", peerId: d.id })}>
      <span class="oi"><Icon name="settings" /></span>
      <span class="om">
        <span class="on">{d.remark ? t("修改备注 · {remark}", { remark: d.remark }) : t("修改备注")}</span>
        <span class="od">{t("给这台设备起一个只存在本机的名字（不发给对端）：设好后这一行显示「备注(设备名)」，会话与标题只写备注本身。")}</span>
      </span>
    </button>

    {#if d.transport !== "localsend"}
    <!-- 长连接只给"在线 + 已配对 + 凭据还能用"这一档：剩下三档各拨不动 ——
         未配对（拨号要求 paired）、对端不在网上（只会开一个空转的拨号循环）、
         token 已被对端作废（必 401，出路是重新配对）。 -->
    {#if !d.paired}
      <button class="opt" type="button" onclick={() => store.openSheet({ kind: "pair", peerId: d.id })}>
        <span class="oi"><Icon name="plus" /></span>
        <span class="om">
          <span class="on">{t("配对")}</span>
          <span class="od">{t("到配对表里选这台设备完成配对。未配对也能发消息（HTTP 兜底），但拨不动长连接、也读不到对方的共享项。")}</span>
        </span>
      </button>
    {:else if d.online}
      {#if d.connected}
        <button class="opt" type="button" onclick={() => void toggleLink()}>
          <span class="oi"><Icon name="link" /></span>
          <span class="om">
            <span class="on">{t("断开连接")}</span>
            <span class="od">{t("断链并暂停自动重拨（不是解除配对）：本机不再自动接回，随时可再连。")}</span>
          </span>
        </button>
      {:else if devices.needsRepair(d)}
        <button class="opt" type="button" onclick={() => store.openSheet({ kind: "pair", peerId: d.id })}>
          <span class="oi"><Icon name="plus" /></span>
          <span class="om">
            <span class="on">{t("重新配对")}</span>
            <span class="od">{t("对端不认本机的凭据（它清过数据，或换了机器）：拨号必然被拒，重新配对换一枚新 token。")}</span>
          </span>
        </button>
      {:else}
        <button class="opt" type="button" onclick={() => void toggleLink()}>
          <span class="oi"><Icon name="link" /></span>
          <span class="om">
            <span class="on">{t("建立长连接")}</span>
            <span class="od">{t("配对只确立关系；连上之后本机与对端之间才是实时通道（对端拖入的文件自动广播过来）。")}</span>
          </span>
        </button>
      {/if}
    {/if}

    {/if}

    <!-- 发消息/发文件：LocalSend 只有 direct 文件会话；OrbDrop 才给文本会话。 -->
    {#if d.transport === "localsend"}
      <button class="opt" type="button" onclick={() => { store.openChat(d.id); onClose(); }}>
        <span class="oi"><Icon name="file" /></span>
        <span class="om">
          <span class="on">{t("发消息")}</span>
          <span class="od">{t("打开与「{name}」的 LocalSend 会话：可以发文字；图片/文件按 LocalSend 文件传输，每批需要对方确认。", { name: devices.deviceShortName(d) })}</span>
        </span>
      </button>
      <button class="opt danger" type="button" onclick={() => void removeLocalSend()}>
        <span class="oi"><Icon name="trash" /></span>
        <span class="om">
          <span class="on">{t("从列表移除")}</span>
          <span class="od">{t("只清本机这一行；它还在网上时会重新出现。")}</span>
        </span>
      </button>
    {:else if d.online || !d.paired}
      <button class="opt" type="button" onclick={() => { store.openChat(d.id); onClose(); }}>
        <span class="oi"><Icon name="send" /></span>
        <span class="om">
          <span class="on">{t("发消息")}</span>
          <span class="od">{t("打开与「{name}」的会话：长连接优先、HTTP 兜底，对端不在网上就排着队等它上线。", { name: devices.deviceShortName(d) })}</span>
        </span>
      </button>
    {/if}

    <!-- 解除配对只给已配对的 OrbDrop 设备（LocalSend 没有关系可解）。 -->
    {#if d.paired && d.transport !== "localsend"}
      <button class="opt danger" type="button" onclick={() => void unpair()}>
        <span class="oi"><Icon name="trash" /></span>
        <span class="om">
          <span class="on">{t("解除配对")}</span>
          <span class="od">{t("移除设备并清理对应系统凭据。")}</span>
        </span>
      </button>
    {/if}
  {/if}
</Sheet>