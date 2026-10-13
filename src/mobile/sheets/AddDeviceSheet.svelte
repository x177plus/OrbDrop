<script lang="ts">
  import Sheet from "../components/Sheet.svelte";
  import * as api from "$lib/api";
  import * as store from "../store.svelte";

  let { onClose }: { onClose: () => void } = $props();

  let input = $state("");
  let busy = $state(false);

  async function add(): Promise<void> {
    if (busy) return;
    const v = input.trim();
    // 只认原型那种写法："IPv4 / 主机名[:端口]"。IPv6 要方括号（`[::1]:3890`），这一版不做。
    // **端口可以不给**：不给就交给后端从 3890 起探 10 个（S-06 的端口段，对方端口被占时
    // 会顺延到 3891-3899）。
    const m = /^([\w.\-]+)(?::(\d{1,5}))?$/.exec(v);
    if (!m) {
      store.toast("请输入 IP（或 IP:端口），例如 192.168.1.20 或 192.168.1.20:3890");
      return;
    }
    const port = m[2] ? Number(m[2]) : 0;
    if (port && (port < 1 || port > 65535)) {
      store.toast("端口要在 1–65535 之间");
      return;
    }
    busy = true;
    try {
      // `add_peer(ip, port)` 是两个参数，地址得自己切。这里不走 `store.addManualPeer`：
      // 那条包装把成败吞在内部，失败时这张表也会被关掉，用户刚敲进去的地址就没了。
      await api.addPeer(m[1], port);
    } catch (e) {
      store.failed("添加设备", e);
      return;
    } finally {
      busy = false;
    }
    store.toast(`已添加 ${m[1]}${port ? `:${port}` : ''}，等待配对`);
    onClose();
  }
</script>

<Sheet title="手动添加设备" desc="mDNS 不能跨网段，跨网段时输入对方 IP（端口留空自动扫 3890-3899）。" onClose={onClose}>
  <div class="field">
    <label for="ipInput">设备地址</label>
    <input class="input num" id="ipInput" placeholder="192.168.1.20" bind:value={input} />
  </div>

  {#snippet foot()}
    <button class="btn btn-secondary" type="button" onclick={onClose}>取消</button>
    <button class="btn btn-primary" type="button" onclick={() => void add()}>添加</button>
  {/snippet}
</Sheet>