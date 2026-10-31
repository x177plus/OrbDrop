<script lang="ts">
  import * as api from "$lib/api";
  import * as chat from "$lib/stores/chat.svelte";
  import { deviceState, deviceShortName, getDevices } from "$lib/stores/devices.svelte";
  import Icon from "../Icon.svelte";
  import Sheet from "../components/Sheet.svelte";
  import StateChip from "../components/StateChip.svelte";
  import * as store from "../store.svelte";
  import { t } from "$lib/i18n.svelte";

  let { convoKey, onClose }: { convoKey: string; onClose: () => void } = $props();

  // 模式从键的前缀判（`bc:` / `grp:`，键的形状由会话 store 定）；名单 id 优先取会话视图里那份，
  // 视图不在时退回键上那半截 —— 两者本来就是同一个 id 的两种写法。
  const mode = $derived(convoKey.startsWith("grp:") ? "group" : "broadcast");
  const thread = $derived.by(() => {
    const th = chat.activeThread();
    return th && th.key === convoKey ? th : null;
  });
  const listId = $derived(thread?.id ?? convoKey.slice(convoKey.indexOf(":") + 1));
  /**
   * 群名要原样带回：`group_edit` 会整份替掉名册与名字。
   *
   * 直接读群记录（`groupOf`）而不是会话视图：视图只在**这条会话正开着**时才有值，而键上那半截
   * 就是群 id —— 从这个 id 读名字，与当前开着哪条会话无关。
   */
  const groupName = $derived(mode === "group" ? (chat.groupOf(listId)?.name ?? thread?.name ?? "") : "");

  // **不按配对过滤**（与新建会话那张表、桌面端的收件人框同一条规则）：群聊名册里只存
  // `{id, 名字}`，成员本来就不需要配对（见 `group.rs` 的 D6=b）；群发也只是逐台各发一条
  // 一对一的普通消息，未配对的对端同样收得下（长连接优先、HTTP 兜底）。
  // 只列已配对的会让名册里那台未配对的成员在表上**看不见**——标记还留在 `picked` 里，
  // 用户既看不到也改不掉它。
  // LocalSend 设备不进群发/群聊名册（它只有一对一的文件会话）。
  const candidates = $derived(getDevices().filter((d) => d.transport !== 'localsend'));

  // 勾选是**开表那一刻**拍下来的快照，不是跟着视图走的派生值：会话里随时可能来新消息，
  // 视图一重算就把用户刚改的勾选冲掉。保存时再回头看视图要最新的 id 与名字。
  // 只剔除 LocalSend（不在设备表里的旧成员照旧保留，否则一保存就把名册里那台删了）。
  let picked = $state<string[]>(
    initialPicked().filter((id) => getDevices().find((d) => d.id === id)?.transport !== 'localsend'),
  );

  function initialPicked(): string[] {
    const th = chat.activeThread();
    if (th && th.key === convoKey) return [...th.participants];
    // 会话视图不在（键失效、或这条会话刚好被切走）时至少把群的名册读回来：群记录是独立的，
    // 不依赖哪条会话正开着。群发没有这样的旁路（名单只出现在会话视图里）—— 那种情况选择留空，
    // 保存会被"至少两台"挡下，不会把名单写坏。
    if (convoKey.startsWith("grp:")) return (chat.groupOf(convoKey.slice(4))?.members ?? []).map((m) => m.id);
    return [];
  }

  function toggle(id: string, on: boolean): void {
    picked = on ? [...picked, id] : picked.filter((x) => x !== id);
  }

  async function save(): Promise<void> {
    const ids = picked;
    if (ids.length < 2) {
      store.toast(t("至少保留两台设备"));
      return;
    }
    try {
      if (mode === "group") await api.groupEdit(listId, ids, groupName);
      else await api.chatBatchEnsure(ids, listId);
      await chat.refresh();
      onClose();
      store.toast(t("已更新参与设备"), "check");
    } catch (e) {
      store.toast(t("更新参与设备失败：{error}", { error: e instanceof Error ? e.message : String(e) }));
    }
  }
</script>

<Sheet
  title={mode === "group" ? t("编辑群聊") : t("编辑群发")}
  desc={t("至少保留两台设备。")}
  label={t("编辑收件人")}
  onClose={onClose}
>
  <!-- 原型在底板里给这行去掉了底边距（`.sheet-body` 自己用 gap 排），照抄它那份 inline。 -->
  <div class="sec-label" style="padding-bottom:0">
    <span>{t("参与设备")}</span><span class="num">{t("已选 {n} 台", { n: picked.length })}</span>
  </div>
  {#if candidates.length === 0}
    <div class="card list">
      <div class="empty">
        <Icon name="devices" />
        <p>{t("还没有设备")}</p>
        <p class="es">{t("名册里的设备都不在本机设备表里了。")}</p>
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
    <button class="btn btn-primary" type="button" onclick={() => void save()}>{t("保存")}</button>
  {/snippet}
</Sheet>