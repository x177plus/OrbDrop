<script lang="ts">
  import * as api from "$lib/api";
  import * as chat from "$lib/stores/chat.svelte";
  import { deviceState, deviceShortName, getDevices } from "$lib/stores/devices.svelte";
  import Icon from "../Icon.svelte";
  import Sheet from "../components/Sheet.svelte";
  import StateChip from "../components/StateChip.svelte";
  import * as store from "../store.svelte";
  import { t } from "$lib/i18n.svelte";

  let { onClose }: { onClose: () => void } = $props();

  // **不按配对过滤**（桌面端的收件人框同样列全部设备）：未配对的对端也收得下消息 —— 一对一走
  // "长连接优先、HTTP 兜底"（对方名字是它自报的），群聊的名册里本来就只存 `{id, 名字}`。
  // 以前这里只列已配对的，于是"设备页没有发消息入口 + 新建会话里看不见这台"叠在一起，
  // 未配对的设备在移动端一条会话也开不出来。
  // LocalSend 设备不进收件人候选：它只有一对一的文件会话，不能进群发/群聊。
  const candidates = $derived(getDevices().filter((d) => d.transport !== 'localsend'));

  let picked = $state<string[]>([]);
  // 原型两张卡初始都不勾、但语义回落到"群聊"，于是按钮写「进入群聊」而高亮的却是空 —— 这里预选群聊，
  // 让卡片、按钮、预览三处说同一件事。
  let mode = $state<"broadcast" | "group">("group");
  let name = $state("");

  const many = $derived(picked.length > 1);
  /** 标题预览里那串设备名（原型 `ids.map(devName).join('、')`）——**短名**（F-101：有备注就是备注本身）。 */
  const pickedNames = $derived(candidates.filter((d) => picked.includes(d.id)).map((d) => deviceShortName(d)));
  const preview = $derived(
    mode === "broadcast"
      ? t("群发 · {name}", { name: name.trim() || t("未命名群发") })
      : t("群聊 · {name}", { name: name.trim() || pickedNames.join(t("、")) || t("未选择设备") }),
  );
  const actionLabel = $derived(many ? (mode === "broadcast" ? t("进入群发") : t("进入群聊")) : t("进入会话"));

  function toggle(id: string, on: boolean): void {
    picked = on ? [...picked, id] : picked.filter((x) => x !== id);
  }

  /**
   * 建会话：一对一是后端已有的那条记录；多台按模式各走一条真命令（原型只在内存里造了个 thread）。
   * 名字留空时不写 —— 群聊由后端按成员名兜底，群发保留"未命名群发"那份默认。
   */
  async function enter(): Promise<void> {
    const ids = picked;
    if (!ids.length) return;
    const nm = name.trim();
    try {
      if (ids.length === 1) {
        const id = ids[0];
        onClose();
        store.openChat(id);
        return;
      }
      if (mode === "broadcast") {
        const { id } = await api.chatBatchEnsure(ids);
        if (nm) await api.chatBatchRename(id, nm);
        await chat.refresh();
        onClose();
        store.navigateChat(`bc:${id}`);
      } else {
        const { id } = await api.groupCreate(ids, nm);
        await chat.refresh();
        onClose();
        store.navigateChat(`grp:${id}`);
      }
    } catch (e) {
      // 失败就把底板留着：名单还在，用户可以改完再试。
      store.failed(t("新建会话"), e);
    }
  }
</script>

<Sheet title={t("新建会话")} desc={t("选一台设备建立一对一；选两台以上可用群发或群聊。")} onClose={onClose}>
  <div class="stack">
    <label class="mode" data-mode="broadcast" class:is-checked={mode === "broadcast"}>
      <input type="radio" name="convoMode" value="broadcast" bind:group={mode} />
      <span class="mi"><Icon name="bc" /></span>
      <span class="mm">
        <span class="mn">{t("群发")}</span>
        <span class="md">{t("单向：每条消息分别作为单独的一对一消息发给每台设备。")}</span>
      </span>
    </label>
    <label class="mode" data-mode="group" class:is-checked={mode === "group"}>
      <input type="radio" name="convoMode" value="group" bind:group={mode} />
      <span class="mi"><Icon name="group" /></span>
      <span class="mm">
        <span class="mn">{t("群聊")}</span>
        <span class="md">{t("共享：所有参与设备都能看到彼此的消息。")}</span>
      </span>
    </label>
  </div>

  <!-- 名称只在多选时出现（原型 `nameField.hidden`）；用 hidden 而不是 {#if}，缩回一台再选回来时不用重打。 -->
  <div class="field" hidden={!many}>
    <label for="convoName">{t("会话名称")}</label>
    <input class="input" id="convoName" maxlength="16" placeholder={t("例如：发布通知")} bind:value={name} />
    <p class="field-hint">{t("显示为")} <b>{preview}</b></p>
  </div>

  <!-- 原型在底板里给这行去掉了底边距（`.sheet-body` 自己用 gap 排），照抄它那份 inline。 -->
  <div class="sec-label" style="padding-bottom:0">
    <span>{t("选择设备")}</span><span class="num">{t("已选 {n} 台", { n: picked.length })}</span>
  </div>
  {#if candidates.length === 0}
    <div class="card list">
      <div class="empty">
        <Icon name="devices" />
        <p>{t("还没有设备")}</p>
        <p class="es">{t("等局域网里的设备出现，或到「设备」页手动添加。")}</p>
      </div>
    </div>
  {:else}
    <div class="stack">
      {#each candidates as d (d.id)}
        <label class="pick" class:is-checked={picked.includes(d.id)}>
          <input
            type="checkbox"
            checked={picked.includes(d.id)}
            onchange={(e: Event) => toggle(d.id, (e.currentTarget as HTMLInputElement).checked)}
          />
          <span class="pav">{chat.peerInitial(d.id)}</span>
          <span class="fmain">
            <span class="fname2" title={deviceShortName(d)}>{deviceShortName(d)}</span>
            <span class="fmeta num">{d.ip}</span>
          </span>
          <StateChip state={deviceState(d)} />
        </label>
      {/each}
    </div>
  {/if}

  {#snippet foot()}
    <button class="btn btn-secondary" type="button" onclick={onClose}>{t("取消")}</button>
    <button class="btn btn-primary" type="button" disabled={picked.length === 0} onclick={() => void enter()}>
      {actionLabel}
    </button>
  {/snippet}
</Sheet>