/**
 * 移动端要接的后端事件。
 *
 * **为什么需要这个文件**：`push://*` 这一族事件原先只在桌面端登记（`src/App.svelte` 的
 * `unPushInvite` / `unPushBatch` / `unPushResponse` / `unPushProgress` / `unPushDone`），
 * 移动端的模块图里一条都没有。缺席的是**界面这一半**：后端那半边一直是全的（邀请落账、
 * 进度、终局都在 `chat` / `push_job` 里跑，已配对设备还会 `auto` 直接开收，
 * 见 `server/routes_push.rs`），而把事件翻译进共享会话 store（`$lib/stores/chat.svelte` 的
 * `noteInboundInvite` 等）这一步没人做。表现就是**PC 在会话里发来的文件，安卓的会话里
 * 什么都不出现**，而且「文件」页的"待接收 / 传输中"永远空着 —— 那两处都是从会话里的
 * 文件消息筛出来的（`src/mobile/store.svelte.ts` 的 `pendingInvites` / `activeTransfers`）。
 *
 * 只接移动端真正有出口的那些：
 * - `push://invite` / `push://progress` / `push://done` —— 会话里那张「接收 / 拒绝」卡
 *   （`ChatScreen` 画好了卡、`store.answerInvite` 也落到了 `accept_push`/`reject_push`）、
 *   它的进度与终局，外加一条 toast：移动端没有桌面端右下角那叠通知气泡，"收到东西了"
 *   这件事只有 toast 与「文件」页两个出口。
 * - `pair://completed` / `pair://invite_failed` —— 本机**发出去**的配对邀请的结果
 *   （`PairSheet` 只负责报"已发出"，结果由这两条回来）。
 *
 * **不接的**（各有明确理由，别顺手加上）：
 * - `push://batch` / `push://response`：发送侧的事件，而移动端目前**没有发文件的入口**
 *   （`push_paths` 只在桌面端被调用）。将来手机上能发文件时，在这里接
 *   `push://batch` → `chat.noteBatch`，会话气泡里那个百分比才会动。
 * - `pair://invite` / `pair://invite_closed`：**别人发来的**配对邀请，移动端还没有
 *   "接收 / 拒绝"那套界面（桌面端是右下角一条带按钮的通知）—— 不登记空监听器。
 * - `orb://show`：桌面端"第二份进程把我调出来"，移动端没有这个概念。
 * - `devices://changed`、`peer://*`、`shares://changed`、`chat://changed` 已经在共享 store
 *   （`$lib/stores/devices.svelte` / `peers.svelte.ts` / `chat.svelte.ts`）里登记过了。
 *
 * 登记时机与共享 store 同一套：模块级 IIFE，`MobileApp.svelte` 引一次这个模块即可。
 */
import { listen } from "@tauri-apps/api/event";
import * as chat from "$lib/stores/chat.svelte";
import type { PushInvite } from "$lib/types";
import * as store from "./store.svelte";

/** 事件载荷里只有 `inviteId`，而 toast 要说"收到了哪个文件"，所以回会话里认一下。 */
function nameOfInvite(inviteId: string): string {
  if (!inviteId) return "文件";
  for (const row of chat.convoRows()) {
    for (const item of chat.itemsOf(row.key)) {
      if (item.kind === "file" && item.inviteId === inviteId) return item.name;
    }
  }
  return "文件";
}

export const ready: Promise<void> = (async () => {
  // 别人推来的文件：会话里当场落一条文件消息。已配对设备后端已经 `auto` 收下了，
  // 那张卡一出现就是"接收中"；未绑定的那条要等人在卡上点「接收」。
  await listen<PushInvite>("push://invite", (ev) => {
    const inv = ev.payload;
    chat.noteInboundInvite(inv);
    store.toast(
      inv.auto ? `正在接收「${inv.name}」` : `「${inv.sender.name}」发来「${inv.name}」`,
      "download",
    );
  });

  // 接收进度：会话里那张卡就地变成"接收中 xx%"（`noteInviteProgress` 顺带把
  // `pending` 推进到 `accepted` —— 人点过「接收」之后第一条进度就是它）。
  await listen<{ inviteId: string; received: number; total: number }>("push://progress", (ev) => {
    const { inviteId, received, total } = ev.payload;
    if (total <= 0) return;
    chat.noteInviteProgress(inviteId, received, total);
  });

  // 一条推送的终局：已接收 / 拒绝 / 取消 / 失败。
  await listen<{ inviteId: string; status: string; reason?: string | null }>("push://done", (ev) => {
    const { inviteId, status, reason } = ev.payload;
    const name = nameOfInvite(inviteId);
    chat.noteInviteDone(inviteId, status === "completed" ? "received" : "failed");
    if (status === "completed") {
      store.toast(`已收到「${name}」`, "download");
      // 收件目录里刚多了一项，而「文件」页那份列表只在挂载/手动刷新/删除时才拉 ——
      // 不在这儿补一次，用户会看到会话里写着"已接收"、文件页却还是"还没有收到的文件"。
      void store.loadInbox();
    } else if (status !== "cancelled") {
      // 取消是自己点的，不必再报一次；失败要给原因（磁盘满了、对端走了都在这里）。
      store.failed(`接收「${name}」`, reason ?? "接收失败");
    }
  });

  // 本机发出的配对邀请有结果了。**`inviteId` 非空才是这条路**：本机自己输码那条
  // （`pair_confirm`）当场就配好了、`PairSheet` 自己也报过一句，别报两遍
  // （后端只在邀请那条路带上 `inviteId`，见 `pair.rs::commit_pairing` 的调用点）。
  await listen<{ peerId: string; peerName?: string | null; inviteId?: string | null }>(
    "pair://completed",
    (ev) => {
      if (!ev.payload.inviteId) return;
      store.toast(`已与「${ev.payload.peerName ?? "设备"}」配对成功`);
    },
  );
  await listen<{ inviteId: string; reason?: string | null }>("pair://invite_failed", (ev) => {
    store.failed("配对邀请", ev.payload.reason ?? "对方未接受");
  });
})();