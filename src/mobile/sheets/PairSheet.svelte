<script lang="ts">
  import Sheet from "../components/Sheet.svelte";
  import Icon from "../Icon.svelte";
  import StateChip from "../components/StateChip.svelte";
  import * as store from "../store.svelte";
  import * as api from "$lib/api";
  import * as devices from "$lib/stores/devices.svelte";

  let { onClose, peerId }: { onClose: () => void; peerId?: string } = $props();

  /**
   * 候选目标：未绑定的，外加"需重新配对"的（token 被对端清过 / 换了机器 —— 那台的长连接
   * 会一直被拒，只能重走配对换一枚新 token，见 `needsRepair`）。
   *
   * 原型的配对是"凭空编一个 6 位码"的假流程，后端的 `pair_start` 却**必须给 peerId**：
   * 码本身不跟设备绑定（`PairStore` 里写得很清楚：持码即凭证，认的是谁把它念出来），
   * 但命令要有一台目标设备，所以这里先选目标再生成码，而不是照抄原型那把假码。
   */
  const allCandidates = $derived(
    devices.visibleDevices(devices.getDevices()).filter((d) => !d.paired || devices.needsRepair(d)),
  );

  /**
   * 从某台设备的行 / 底板点进来时，目标**已经选定**（`peerId`）：表里只画这一台，不再把
   * 整份设备列表摊开让人重新挑 —— 用户点的就是它。那台若在表开着的时候被别处配好，
   * `fixed` 为空、这张表落到「不需要配对」那一档，而不是悄悄选到别的设备上去。
   */
  const fixed = $derived(peerId ? allCandidates.find((d) => d.id === peerId) ?? null : null);
  const candidates = $derived(peerId ? (fixed ? [fixed] : []) : allCandidates);

  let picked = $state<string | null>(null);
  /** 选中项跟着候选收敛：那台配对成功后就从候选里消失，自动落到下一台，别留一个悬空的 id。 */
  const target = $derived(
    peerId ? fixed : (candidates.find((d) => d.id === picked) ?? candidates[0] ?? null),
  );

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
    // 发起配对 = 发起与这一台的连接：它此刻"看得见却直连不通"时（F-106），那句话在这里说
    // ——码由本机持有、对方拨进来，这张网不让两个客户端互相说话时这一趟就是白等。
    const tip = devices.unicastBlockedTip(target);
    if (tip) store.toast(tip);
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
      // 这一跳就是拨到对端的一次 POST：失败时那台"看得见却直连不通"要说清楚，
      // 否则用户只会看到一句"配对失败"然后反复试。
      const tip = devices.unicastBlockedTip(target);
      if (tip) store.toast(tip);
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
      //
      // **表不关**（P1-13⑤）：发出去的邀请只有 60s，而撤回的唯一入口就是这一张表 ——
      // 收起它等于把"取消"这颗按钮藏起来（对端界面上那条通知只能等它超时）。
      const start = await api.pairInvite(peer.id);
      store.setPendingPairInvite(start.inviteId, devices.deviceShortName(peer), start.sas);
      store.toast(`已向「${devices.deviceShortName(peer)}」发起配对邀请`);
    } catch (e) {
      store.failed("发起配对邀请", e);
      // 邀请是一次到对端的 POST：投不出去、而那台"看得见却直连不通"时，说清这条路为什么不通。
      const tip = devices.unicastBlockedTip(peer);
      if (tip) store.toast(tip);
    } finally {
      busy = false;
    }
  }

  /** 本机发出去、还在等对方确认的那一条（`pair://completed`/`invite_failed` 回来时会清掉）。 */
  const waiting = $derived(store.pendingPairInvite());

  async function cancelInvite(): Promise<void> {
    if (busy) return;
    busy = true;
    try {
      await store.cancelPendingPairInvite();
      onClose();
    } finally {
      busy = false;
    }
  }
</script>

<Sheet title="配对设备" desc="两台设备交换 6 位码即完成配对，token 保存在系统凭据库。" onClose={onClose}>
  {#if !target}
    <div class="empty">
      <Icon name="devices" />
      <p>{peerId ? "这台设备当前不需要配对" : "没有待配对的设备"}</p>
      <p class="es">{peerId
        ? "它已经配对完成；若对端不认本机凭据，设备列表会把它标成「需重新配对」。"
        : "确保两台设备在同一局域网，或手动添加 IP:端口。"}</p>
    </div>
  {:else}
    <!-- 从设备行点进来时目标已经定了（`peerId`）：只画这一台，不出整份候选表。
         没给 `peerId`（「配对设备」那颗全局按钮）才列所有候选。 -->
    {#if fixed}
      <div class="sec-label" style="padding-bottom:0">
        <span>目标设备</span><span class="num">1 台</span>
      </div>
      <div class="stack">
        <div class="pick is-checked">
          <span class="pav">{devices.deviceDisplayName(fixed).charAt(0)}</span>
          <span class="fmain">
            <span class="fname2" title={devices.deviceDisplayName(fixed)}>{devices.deviceDisplayName(fixed)}</span>
            <span class="fmeta num">{fixed.ip}:{fixed.port}</span>
          </span>
          <StateChip state={devices.deviceState(fixed)} />
        </div>
      </div>
    {:else}
      <!-- 目标设备这一份就是**设备列表**（候选取自 `visibleDevices`）：名字与头像字都走
           **显示名**（F-101：有备注就是「备注(设备名)」），与设备页那一行同规。 -->
      <div class="sec-label" style="padding-bottom:0">
        <span>目标设备</span><span class="num">{candidates.length} 台</span>
      </div>
      <div class="stack">
        {#each candidates as d (d.id)}
          <label class="pick" class:is-checked={target.id === d.id}>
            <input type="radio" name="pairTarget" checked={target.id === d.id} onchange={() => (picked = d.id)} />
            <span class="pav">{devices.deviceDisplayName(d).charAt(0)}</span>
            <span class="fmain">
              <span class="fname2" title={devices.deviceDisplayName(d)}>{devices.deviceDisplayName(d)}</span>
              <span class="fmeta num">{d.ip}:{d.port}</span>
            </span>
            <StateChip state={devices.deviceState(d)} />
          </label>
        {/each}
      </div>
    {/if}

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

    {#if waiting}
      <div class="opt">
        <span class="oi"><Icon name="bell" /></span>
        <span class="om">
          <span class="on">等待「{waiting.peerName}」确认</span>
          <span class="od">先核对两边的码一致，再由对方点「接收」；60 秒没人处理自动作废。</span>
        </span>
      </div>
      {#if waiting.sas}
        <div class="code-box">
          <span class="sas-code">{waiting.sas}</span>
          <span class="code-hint">对方屏幕应显示同一串；不一致就取消这次邀请。</span>
        </div>
      {/if}
      <button class="btn btn-secondary btn-block" type="button" onclick={() => void cancelInvite()}>
        <Icon name="x" />取消邀请
      </button>
    {:else}
      <button class="btn btn-secondary btn-block" type="button" onclick={() => void invite()}>
        <Icon name="bell" />发起配对邀请
      </button>
    {/if}
  {/if}

  {#snippet foot()}
    <button class="btn btn-secondary" type="button" onclick={onClose}>关闭</button>
    {#if target}
      <button class="btn btn-primary" type="button" onclick={() => void confirm()}>确认配对</button>
    {/if}
  {/snippet}
</Sheet>