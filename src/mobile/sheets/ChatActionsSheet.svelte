<script lang="ts">
  import * as chat from "$lib/stores/chat.svelte";
  import * as devices from "$lib/stores/devices.svelte";
  import Icon from "../Icon.svelte";
  import Sheet from "../components/Sheet.svelte";
  import * as store from "../store.svelte";
  import { t } from "$lib/i18n.svelte";

  let { convoKey, onClose }: { convoKey: string; onClose: () => void } = $props();

  const thread = $derived(chat.activeThread());
  const title = $derived(thread?.title ?? t("会话"));
  const isThread = $derived(!!thread && thread.mode !== "direct");
  /**
   * 一对一会话对着哪台设备（多台会话是空串）：「修改备注」与「配对」两项长在它身上。
   *
   * `dev` 取设备表里那一条 —— 会话列表只列**还在设备表里**的设备（`chat.svelte` 的 `known`），
   * 所以正常都在；真被移除（解除配对后设备行也消失）时这两项什么都不摆：`remarkDevice` 与
   * `pair` 两张表都没有主语。
   */
  const peerId = $derived(thread?.mode === "direct" ? thread.participants[0] : "");
  const dev = $derived(devices.deviceById(peerId));

  /**
   * 标记为已读：走"打开这条会话"那条路，而不是直接调 `chat_mark_read` / `group_mark_read` ——
   * 两种模式各自的已读水位（一对一的按台、群聊的按群）都在会话 store 里，直接调 api 还得自己
   * 把返回的快照塞回 store（`replace` 是私有的），那两档判据就成了两份。
   */
  function markRead(): void {
    chat.openConversation(convoKey);
    onClose();
    store.toast(t("已标记为已读"), "check");
  }

  function editRecipients(): void {
    // 先收这张再开下一张：同一时刻只可能有一张底板（槽位在 store 里只有一个）。
    onClose();
    store.openSheet({ kind: "editRecipients", key: convoKey });
  }

  async function clearGroupHistory(): Promise<void> {
    const key = convoKey;
    const ok = await store.confirmAction({
      title: t("清空聊天记录"),
      desc: t("清空「{title}」的本地聊天记录？不会退群，之后的新消息仍会收到；其他成员不受影响。", { title }),
      confirmLabel: t("清空"),
      danger: true,
    });
    if (!ok) return;
    try {
      await chat.clearGroupHistory(key);
    } catch (e) {
      store.failed(t("清空聊天记录"), e);
      return;
    }
    onClose();
    store.toast(t("已清空本地聊天记录（未退群）"), "trash");
  }

  async function clearHistory(): Promise<void> {
    const peerId = thread?.participants[0];
    if (!peerId) return;
    const ok = await store.confirmAction({
      title: t("清空聊天记录"),
      desc: t("清空与「{title}」的本机聊天记录？不会影响对方，删掉后不能恢复。", { title }),
      confirmLabel: t("清空"),
      danger: true,
    });
    if (!ok) return;
    try {
      await chat.clearPeer(peerId);
    } catch (e) {
      store.failed(t("清空聊天记录"), e);
      return;
    }
    onClose();
    store.toast(t("已清空聊天记录"), "trash");
  }

  /**
   * 会话菜单最后那一项：**群聊 = 退出群聊 / 群发 = 删除群发**（与桌面端会话右键菜单同一项、
   * 同一套语义与提示；共享 store 里本来就是一条路 `chat.exitThread` → 群聊走 `group_leave`、
   * 群发走 `chatBatchDelete`）。
   *
   * 三件事是移动端这边必须自己做的：
   * - **先问一句**：退群没有回收站，"退错了再进"要对方重新邀请，所以按移动端一贯的做法
   *   （见 `FileActionsSheet`）用应用自己的确认框（`store.confirmAction`）；文案写明"其余
   *   成员不受影响、**本机这份记录会一起删掉**"（`GroupStore::leave` 的实情：本地 groups/
   *   convos/mySeq 都清掉）。
   * - **退回会话列表**：子屏的路由是 hash（`#/chat/<key>`），退出之后那条会话已经不在快照里
   *   （`activeThread()` 为空）—— 停在原处就是一屏没有标题、也发不出消息的空壳。
   * - 提示用 `exitThread` 回来的标题（那是离开前快照里的名字），不是这里 `$derived` 的那份。
   */
  async function exitThread(): Promise<void> {
    const target = thread;
    if (!target || target.mode === "direct") return;
    const key = convoKey;
    const isGroup = target.mode === "group";
    const ask = isGroup
      ? t("退出「{title}」？本机不再收发这个群的消息，本机这份聊天记录也会一起删掉；其他成员不受影响。", { title })
      : t("删除群发「{title}」？消息仍留在与各台设备的单独会话里，名单不再保留。", { title });
    const ok = await store.confirmAction({
      title: isGroup ? t("退出群聊") : t("删除群发"),
      desc: ask,
      confirmLabel: isGroup ? t("退出") : t("删除"),
      danger: true,
    });
    if (!ok) return;
    let gone: { title: string } | null = null;
    try {
      gone = await chat.exitThread(key);
    } catch (e) {
      store.failed(isGroup ? t("退出群聊") : t("删除群发"), e);
      return;
    }
    if (!gone) return;
    onClose();
    store.navigateBackToList();
    store.toast(isGroup ? t("已退出「{title}」", { title: gone.title }) : t("已删除「{title}」", { title: gone.title }), "trash");
  }
</script>

<Sheet title={title} desc={t("{n} 台设备", { n: thread?.participants.length ?? 0 })} label={t("会话操作")} {onClose}>
  <button class="opt" type="button" onclick={markRead}>
    <span class="oi"><Icon name="check" /></span>
    <span class="om">
      <span class="on">{t("标记为已读")}</span>
      <span class="od">{t("清空此会话的未读计数。")}</span>
    </span>
  </button>

  {#if isThread}
    <button class="opt" type="button" onclick={editRecipients}>
      <span class="oi"><Icon name="group" /></span>
      <span class="om">
        <span class="on">{t("编辑收件人")}</span>
        <span class="od">{t("调整群发或群聊的参与设备。")}</span>
      </span>
    </button>
  {/if}

  <!-- 一对一那条也摆着**设备那两项**（与设备操作底板、桌面端右键菜单同一份动作与文案）：
       会话正是"看着这台设备"的地方，要改对它的叫法或重新配对，不该先退回设备页长按那一行。
       分档与设备底板逐条同规 —— 备注与在线状态无关（纯本机数据，离线/未绑定的会话照样能改）；
       配对项只给「未绑定」与「凭据失效」两档（已配对的一对一会话没有配对可言）。
       配对表直接钉在这台设备上（带 `peerId`），不再让人从整份设备列表里重挑。 -->
  {#if dev}
    <button class="opt" type="button" onclick={() => store.openSheet({ kind: "remarkDevice", peerId: dev.id })}>
      <span class="oi"><Icon name="settings" /></span>
      <span class="om">
        <span class="on">{dev.remark ? t("修改备注 · {remark}", { remark: dev.remark }) : t("修改备注")}</span>
        <span class="od">{t("给这台设备起一个只存在本机的名字（不发给对端）：会话与标题只写备注本身，设备页那一行显示「备注(设备名)」。")}</span>
      </span>
    </button>

    {#if !dev.paired || devices.needsRepair(dev)}
      <button class="opt" type="button" onclick={() => store.openSheet({ kind: "pair", peerId: dev.id })}>
        <span class="oi"><Icon name="plus" /></span>
        <span class="om">
          <span class="on">{dev.paired ? t("重新配对") : t("配对")}</span>
          <span class="od">
            {dev.paired
              ? t("对端不认本机的凭据（它清过数据，或换了机器）：拨号必然被拒，重新配对换一枚新 token。")
              : t("到配对表里完成配对。未配对也能发消息（HTTP 兜底），但拨不动长连接、也读不到对方的共享项。")}
          </span>
        </span>
      </button>
    {/if}
  {/if}

  <!-- 清空聊天记录只给一对一：群发是"每台各存一份副本"，群里那份记录本机也只有一半的账
       （别人的消息、别的成员的副本都不归这里清），这一阶段不做。 -->
  {#if thread?.mode === "direct"}
    <button class="opt danger" type="button" onclick={() => void clearHistory()}>
      <span class="oi"><Icon name="trash" /></span>
      <span class="om">
        <span class="on">{t("清空聊天记录")}</span>
        <span class="od">{t("只清理本机记录，不影响对方。")}</span>
      </span>
    </button>
  {:else if thread?.mode === "group"}
    <button class="opt" type="button" onclick={() => void clearGroupHistory()}>
      <span class="oi"><Icon name="trash" /></span>
      <span class="om">
        <span class="on">{t("清空聊天记录")}</span>
        <span class="od">{t("只清本机记录，不退群；之后的新消息照常收。")}</span>
      </span>
    </button>
  {/if}
  {#if thread && thread.mode !== "direct"}
    <!-- 多台会话的出口（与桌面端会话菜单的最后一项对齐）：群聊是退名册 + 广播离开，
         群发是删掉这份名单 —— 两者的消息都一条不动。 -->
    <button class="opt danger" type="button" onclick={() => void exitThread()}>
      <span class="oi"><Icon name="trash" /></span>
      <span class="om">
        <span class="on">{thread.mode === "group" ? t("退出群聊") : t("删除群发")}</span>
        <span class="od">
          {thread.mode === "group"
            ? t("本机退出这个群，本机这份记录也随之删除；其他成员不受影响。")
            : t("只删这份名单，消息仍留在各台设备的单独会话里。")}
        </span>
      </span>
    </button>
  {/if}
</Sheet>