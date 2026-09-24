<script lang="ts">
  import Sheet from "../components/Sheet.svelte";
  import * as store from "../store.svelte";
  import * as devices from "$lib/stores/devices.svelte";

  let { peerId, onClose }: { peerId: string; onClose: () => void } = $props();

  const d = $derived(devices.getDevices().find((x) => x.id === peerId));

  // 初值只取一次：输入框里是"用户正在改的东西"，后端事件随后再改设备名也不该把它冲掉。
  let name = $state(devices.getDevices().find((x) => x.id === peerId)?.name ?? "");
  let busy = $state(false);

  // 设备没了（对端注销 / 别处解除了配对）这张表就没有主语：收起。
  $effect(() => {
    if (!d) onClose();
  });

  async function save(): Promise<void> {
    if (busy || !d) return;
    const v = name.trim();
    if (!v) {
      store.toast("名称不能为空");
      return;
    }
    busy = true;
    try {
      await store.renamePeer(d.id, v);
    } finally {
      busy = false;
    }
    onClose();
  }
</script>

<Sheet title="重命名设备" desc="仅修改本机显示的名称。" onClose={onClose}>
  <div class="field">
    <label for="renameInput">设备名称</label>
    <input class="input" id="renameInput" maxlength="20" bind:value={name} />
  </div>

  {#snippet foot()}
    <button class="btn btn-secondary" type="button" onclick={onClose}>取消</button>
    <button class="btn btn-primary" type="button" onclick={() => void save()}>保存</button>
  {/snippet}
</Sheet>