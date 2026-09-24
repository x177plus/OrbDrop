<script lang="ts">
  import Sheet from "../components/Sheet.svelte";
  import Icon from "../Icon.svelte";
  import * as store from "../store.svelte";
  import * as devices from "$lib/stores/devices.svelte";

  let { peerId, onClose }: { peerId: string; onClose: () => void } = $props();

  const d = $derived(devices.getDevices().find((x) => x.id === peerId));

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

  /**
   * 解除配对会清掉两侧保存的 token（对端下次推送/拨号就认不出本机了），误触代价高，
   * 所以先问一句 —— 原型没有这一步，后端也没有"撤销"。
   */
  async function unpair(): Promise<void> {
    if (!d) return;
    if (!confirm(`解除与「${d.name}」的配对？对端将不再认得本机，需要重新配对。`)) return;
    await store.unpair(d.id, d.name);
    onClose();
  }
</script>

<Sheet title={d?.name ?? ""} desc={d ? `${d.ip}:${d.port}` : ""} label="设备操作" onClose={onClose}>
  {#if d}
    <button class="opt" type="button" onclick={() => store.openSheet({ kind: "renameDevice", peerId: d.id })}>
      <span class="oi"><Icon name="settings" /></span>
      <span class="om">
        <span class="on">重命名</span>
        <span class="od">修改设备在本机显示的名称。</span>
      </span>
    </button>

    <button class="opt" type="button" onclick={() => void toggleLink()}>
      <span class="oi"><Icon name="link" /></span>
      <span class="om">
        <span class="on">{d.connected ? "断开连接" : "建立长连接"}</span>
        <span class="od">{d.connected ? "断开后仍保持配对，可随时重连。" : "连接后即可发消息与收文件。"}</span>
      </span>
    </button>

    <button class="opt danger" type="button" onclick={() => void unpair()}>
      <span class="oi"><Icon name="trash" /></span>
      <span class="om">
        <span class="on">解除配对</span>
        <span class="od">移除设备并清理对应系统凭据。</span>
      </span>
    </button>
  {/if}
</Sheet>