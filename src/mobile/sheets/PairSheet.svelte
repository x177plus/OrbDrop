<script lang="ts">
  import Sheet from "../components/Sheet.svelte";
  import Icon from "../Icon.svelte";
  import StateChip from "../components/StateChip.svelte";
  import * as store from "../store.svelte";
  import * as api from "$lib/api";
  import * as devices from "$lib/stores/devices.svelte";

  let { onClose }: { onClose: () => void } = $props();

  /**
   * 候选目标：未绑定的，外加"需重新配对"的（token 被对端清过 / 换了机器 —— 那台的长连接
   * 会一直被拒，只能重走配对换一枚新 token，见 `needsRepair`）。
   *
   * 原型的配对是"凭空编一个 6 位码"的假流程，后端的 `pair_start` 却**必须给 peerId**：
   * 码本身不跟设备绑定（`PairStore` 里写得很清楚：持码即凭证，认的是谁把它念出来），
   * 但命令要有一台目标设备，所以这里先选目标再生成码，而不是照抄原型那把假码。
   */
  const candidates = $derived(
    devices.visibleDevices(devices.getDevices()).filter((d) => !d.paired || devices.needsRepair(d)),
  );

  let picked = $state<string | null>(null);
  /** 选中项跟着候选收敛：那台配对成功后就从候选里消失，自动落到下一台，别留一个悬空的 id。 */
  const target = $derived(candidates.find((d) => d.id === picked) ?? candidates[0] ?? null);

  let code = $state<string | null>(null);
  let codeAt = $state(0);
  let input = $state("");
  let busy = $state(false);

  /** 后端那串码的有效期就是 120s（`pair::CODE_TTL`），这里只是把这个事实显示出来。 */
  const CODE_TTL = 120;
  let now = $state(Date.now());
  // 剩余时间由时间戳算，而不是每秒减一 —— 定时器被节流时不会算歪；没有码就不跑这个定时器。
  $effect(() => {
    if (!code) return;
    const timer = setInterval(() => (now = Date.now()), 1000);
    return () => clearInterval(timer);
  });
  const left = $derived(code ? Math.max(0, CODE_TTL - Math.floor((now - codeAt) / 1000)) : 0);

  async function start(): Promise<void> {
    if (busy || !target) return;
    busy = true;
    try {
      // 码由**本机**生成、显示在本机屏幕上，由对方在它那边输入：`pair_confirm` 是对方那侧的事。
      code = await api.pairStart(target.id);
      codeAt = Date.now();
    } catch (e) {
      store.failed("生成配对码", e);
    } finally {
      busy = false;
    }
  }

  async function confirm(): Promise<void> {
    if (busy || !target) return;
    const v = input.replace(/\D/g, "");
    if (v.length !== 6) {
      store.toast("请输入 6 位数字配对码");
      return;
    }
    busy = true;
    try {
      // 返回的是对端自报的名字：用它做提示，而不是本机记着的那条（对端可能刚改过名）。
      const peerName = await api.pairConfirm(target.id, v);
      store.toast(`已与「${peerName}」完成配对`);
      onClose();
    } catch (e) {
      store.failed("配对", e);
    } finally {
      busy = false;
    }
  }

  async function invite(): Promise<void> {
    if (busy || !target) return;
    // 邀请投出去之后候选表可能就变了（那台刚好被别处配对成功），提示里要写的是**发起时**选中的那台。
    const peer = target;
    busy = true;
    try {
      // `pair_invite` 返回时就代表邀请已经投到对端（它自己起了后台任务等回执），
      // 对方点不点「接收」走 `pair://completed` / `pair://invite_failed` 事件回来 ——
      // 所以这里只报"已发出"，别在这里等结果、更别报"已配对"。
      await api.pairInvite(peer.id);
      store.toast(`已向「${peer.name}」发起配对邀请`);
      onClose();
    } catch (e) {
      store.failed("发起配对邀请", e);
    } finally {
      busy = false;
    }
  }
</script>

<Sheet title="配对设备" desc="两台设备交换 6 位码即完成配对，token 保存在系统凭据库。" onClose={onClose}>
  {#if !target}
    <div class="empty">
      <Icon name="devices" />
      <p>没有待配对的设备</p>
      <p class="es">确保两台设备在同一局域网，或手动添加 IP:端口。</p>
    </div>
  {:else}
    <!-- 原型那两颗「选设备」的表用的是 `.sec-label` + `.stack` + `.pick`，这里照抄结构。 -->
    <div class="sec-label" style="padding-bottom:0">
      <span>目标设备</span><span class="num">{candidates.length} 台</span>
    </div>
    <div class="stack">
      {#each candidates as d (d.id)}
        <label class="pick" class:is-checked={target.id === d.id}>
          <input type="radio" name="pairTarget" checked={target.id === d.id} onchange={() => (picked = d.id)} />
          <span class="pav">{d.name.charAt(0)}</span>
          <span class="fmain">
            <span class="fname2">{d.name}</span>
            <span class="fmeta num">{d.ip}:{d.port}</span>
          </span>
          <StateChip state={devices.deviceState(d)} />
        </label>
      {/each}
    </div>

    <!-- 换目标设备不清掉已生成的码：后端那串码不跟设备绑定（`PairStore`），清了反而是骗人。 -->
    <button class="btn btn-primary btn-block" type="button" onclick={() => void start()}>
      <Icon name="link" />生成配对码
    </button>

    {#if code}
      <div class="code-box">
        <span class="code">{code}</span>
        <span class="code-hint">
          {left > 0
            ? `在对方设备上输入此码 · 剩余 ${Math.floor(left / 60)}:${String(left % 60).padStart(2, "0")}`
            : "配对码已过期，点「生成配对码」重新生成"}
        </span>
      </div>
    {/if}

    <div class="field">
      <label for="pairInput">或输入对方配对码</label>
      <input class="input num" id="pairInput" inputmode="numeric" maxlength="6" placeholder="6 位数字" bind:value={input} />
      <p class="field-hint">对方在「设备 → 配对设备」中显示。</p>
    </div>

    <button class="btn btn-secondary btn-block" type="button" onclick={() => void invite()}>
      <Icon name="bell" />发起配对邀请
    </button>
  {/if}

  {#snippet foot()}
    <button class="btn btn-secondary" type="button" onclick={onClose}>关闭</button>
    {#if target}
      <button class="btn btn-primary" type="button" onclick={() => void confirm()}>确认配对</button>
    {/if}
  {/snippet}
</Sheet>