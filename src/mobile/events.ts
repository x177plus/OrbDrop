/**
 * 移动端要接的后端事件。
 *
 * **为什么需要这个文件**：`push://*` 这一族事件原先只在桌面端登记（`src/App.svelte` 的
 * `unPushInvite` / `unPushBatch` / `unPushResponse` / `unPushProgress` / `unPushDone`），
 * 移动端的模块图里一条都没有。缺席的是**界面这一半**：后端那半边一直是全的（邀请落账、
 * 进度、终局都在 `chat` / `push_job` 里跑，已配对且开了「自动接收」的设备还会 `auto`
 * 直接开收，见 `server/routes_push.rs`），而把事件翻译进共享会话 store（`$lib/stores/chat.svelte` 的
 * `noteInboundInvite` 等）这一步没人做。表现就是**PC 在会话里发来的文件，安卓的会话里
 * 什么都不出现**，而且「文件」页的"待接收 / 传输中"永远空着 —— 那两处都是从会话里的
 * 文件消息筛出来的（`src/mobile/store.svelte.ts` 的 `pendingInvites` / `activeTransfers`）。
 *
 * 只接移动端真正有出口的那些：
 * - `push://invite` / `push://progress` / `push://done` —— 会话里那张「接收 / 拒绝」卡
 *   （`ChatScreen` 画好了卡、`store.answerInvite` 也落到了 `accept_push`/`reject_push`）、
 *   它的进度与终局，外加一条 toast：移动端没有桌面端右下角那叠通知气泡，"收到东西了"
 *   这件事只有 toast 与「文件」页两个出口。
 * - `push://batch` —— **发送侧**那张快照（输入框里粘一张图发出去就是一次推送）：
 *   会话里那条文件气泡的百分比与成败都抄它。
 * - `browse://download` —— 浏览对端共享项时的一次下载：`progress` 推行右那圈进度环，
 *   `done` / `error` 更新终局；「传输」页签正在下载的数量读同一份 store。
 * - `pair://completed` / `pair://invite_failed` —— 本机**发出去**的配对邀请的结果
 *   （`PairSheet` 只负责报"已发出"，结果由这两条回来）。
 * - `link://request` / `link://request_sent` / `link://request_closed` / `link://request_result`
 *   —— 长连接请求（F-16 的同意制），两端都接：别人发来的请求开一张底板当场接受/拒绝；
 *   本机发出的请求在设备页上停在"等对方同意"那一档、可取消；结果三档（同意/拒绝/退回直拨）
 *   都有 toast。
 * - `local://addresses` —— 本机网卡地址变了（手机上换 Wi-Fi / 开热点比桌面上常见得多）：
 *   「设备」页那行「本机 · ip:port」重拉一次。
 *
 * **不接的**（各有明确理由，别顺手加上）：
 * - `push://response`：发送侧那一条"某台单独答复"的通知。移动端的发送侧出入口是会话里那条
 *   文件气泡 + 「文件」页的"传输中"，两者读的都是 `push://batch` 那张快照（已经接了），
 *   桌面端右下角那叠推送卡片与它的逐台通知是桌面端自己那套 UI 的事。
 *   （`push://batch` 以前也在这一档里 —— "移动端没有发文件的入口，`push_paths` 只在桌面端被
 *   调用"；现在输入框里粘一张图就能发，那一半已经接上了。）
 * - `pair://invite` / `pair://invite_closed` / `pair://completed` / `pair://invite_failed`：
 *   两端都接（P1-13⑤）—— 别人发来的邀请开一张「接收 / 拒绝」底板（桌面端是右下角那条通知），
 *   本机发出去的邀请在配对表里停在"等待对方确认"那一档、可取消。
 * - `orb://show`：桌面端"第二份进程把我调出来"，移动端没有这个概念。
 * - `devices://changed`、`peer://*`、`shares://changed`、`chat://changed` 已经在共享 store
 *   （`$lib/stores/devices.svelte` / `peers.svelte.ts` / `chat.svelte.ts`）里登记过了。
 *
 * 登记时机与共享 store 同一套：模块级 IIFE，`MobileApp.svelte` 引一次这个模块即可。
 */
import { listen } from "@tauri-apps/api/event";
import * as chat from "$lib/stores/chat.svelte";
import { peerShortName } from "$lib/stores/devices.svelte";
import { undeliverableText, type PushBatch, type PushInvite, type Undeliverable } from "$lib/types";
import { t } from "$lib/i18n.svelte";
import * as store from "./store.svelte";

export const ready: Promise<void> = (async () => {
  // 别人推来的文件：会话里当场落一条文件消息。`auto`（已配对 + 开着自动接收）后端
  // 已经收下了，那张卡一出现就是"接收中"；其余情况要等人在卡上点「接收」。
  await listen<PushInvite>("push://invite", (ev) => {
    const inv = ev.payload;
    chat.noteInboundInvite(inv);
    store.toast(
      inv.auto
        ? t("正在接收文件")
        : t("「{name}」发来文件", { name: peerShortName(inv.sender.id, inv.sender.name) }),
      "download",
    );
  });

  // 接收进度：会话里那条气泡的百分比与右侧圆环就地推进（状态文字也带同一个数）；
  // `noteInviteProgress` 顺带把 `pending` 推进到 `accepted` —— 人点过「接收」后第一条进度就是它。
  await listen<{ inviteId: string; received: number; total: number }>("push://progress", (ev) => {
    const { inviteId, received, total } = ev.payload;
    if (total <= 0) return;
    chat.noteInviteProgress(inviteId, received, total);
  });

  // **发送侧**那张快照：输入框里粘一张图发出去，走的就是推送这条路
  // （`stores/chat.svelte.ts::sendComposer` → `push_paths`）。会话里那条文件气泡的百分比与
  // 成败都是这张快照抄来的（唯一的进度来源），没有它那条气泡会一直停在"传输中 0%"。
  //
  // 这是移动端**唯一**的发送侧事件：别的（`push://response` 那几条单独通知、右下角那叠
  // 推送卡片）是桌面端自己那套 UI 的事，移动端出入口是「文件」页的"传输中"，读的是同一份
  // 文件消息（见 `store.activeTransfers`）。
  await listen<PushBatch>("push://batch", (ev) => {
    chat.noteBatch(ev.payload);
  });

  // 一条推送的终局：已接收 / 已拒绝或取消 / 失败。**三档要如实对应**（P1-13②）：
  // store 里本来就有 `rejected` 这个状态（气泡写「已拒绝接收」），旧写法把所有非 completed
  // 都塞进 `failed` —— 用户自己点「拒绝」、或任一端取消，界面上都显示成「接收失败」，
  // 看着像出错了，其实是有人主动做的决定。
  await listen<{ inviteId: string; status: string; reason?: string | null; dest?: string }>("push://done", (ev) => {
    const { inviteId, status, reason, dest } = ev.payload;
    // 发送侧也会收到同一条 `push://done`（取消 / 完成）：那几条由 `push://batch` 推着气泡走，
    // 接收侧才有"收到 / 已取消"这句话。没有那张接收卡就别报 —— 否则取消自己的发送会跳出一句
    // 「文件」已取消。
    const incoming = chat.hasIncomingInvite(inviteId);
    const outcome =
      status === "completed" ? "received" : status === "cancelled" ? "cancelled" : "failed";
    // `dest`（落盘那一层目录）一并给会话 store：会话里那条气泡上的「打开」按它去开
    // （见 `ChatFileMsg.path`）—— 东西落在哪只有后端知道。
    chat.noteInviteDone(inviteId, outcome, dest);
    if (status === "completed") {
      if (!incoming) return;
      store.toast(t("已收到文件"), "download");
      // 收件目录里刚多了一项，而「文件」页那份列表只在挂载/手动刷新/删除时才拉 ——
      // 不在这儿补一次，用户会看到会话里写着"已接收"、文件页却还是"还没有收到的文件"。
      void store.loadInbox();
    } else if (status === "cancelled") {
      // 取消/拒绝是自己点的或对方点的，不必再报一次错。
      if (!incoming) return;
      store.toast(t("传输已取消"), "x");
    } else {
      if (!incoming) return;
      // 失败要给原因（磁盘满了、对端走了都在这里）。
      store.failed(t("接收文件"), reason ?? t("原因不明"));
    }
  });

  // 浏览对端共享项时的一次下载：中途字节数与终局都走这一条。命令 `download_from_peer`
  // 本身仍阻塞到落盘，界面拿它的返回值兜底收尾；进度只能从事件来（见 `store.noteBrowseDownload`）。
  await listen<{
    peerId: string;
    shareId: string;
    path: string;
    name: string;
    status: "progress" | "done" | "error";
    received: number;
    dest?: string | null;
    error?: string | null;
  }>("browse://download", (ev) => {
    store.noteBrowseDownload(ev.payload);
  });

  // 本机发出的配对邀请有结果了。**`inviteId` 非空才是这条路**：本机自己输码那条
  // （`pair_confirm`）当场就配好了、`PairSheet` 自己也报过一句，别报两遍
  // （后端只在邀请那条路带上 `inviteId`，见 `pair.rs::commit_pairing` 的调用点）。
  await listen<{ peerId: string; peerName?: string | null; inviteId?: string | null }>(
    "pair://completed",
    (ev) => {
      // 等对方确认的那一档到此结束（表里那颗「取消邀请」跟着收掉）。
      store.clearPendingPairInvite(ev.payload.inviteId ?? undefined);
      if (!ev.payload.inviteId) return;
      store.toast(
        t("已与「{name}」配对成功", {
          name: peerShortName(ev.payload.peerId, ev.payload.peerName ?? t("设备")),
        }),
      );
    },
  );
  await listen<{ inviteId: string; reason?: string | null }>("pair://invite_failed", (ev) => {
    // 对方拒绝 / 超时 / 已撤回 / 联系不上：等待那一档收掉，并把原因说出来。
    store.clearPendingPairInvite(ev.payload.inviteId);
    store.failed(t("配对邀请"), ev.payload.reason ?? t("对方未接受"));
  });

  // **别人发来的**配对邀请（P1-13⑤）：桌面端挂一条带「接收/拒绝」的通知，移动端用一张底板
  // 接住（`store.notePairInvite` 会把它打开）。旧版本这里干脆没登记，于是"PC 点发起邀请、
  // 手机上什么都不会发生"，60 秒后那条邀请就作废了。
  await listen<{ inviteId: string; peerId: string; peerName: string; unproven?: boolean; sas?: string | null }>(
    "pair://invite",
    (ev) => {
      const p = ev.payload;
      store.notePairInvite({
        inviteId: p.inviteId,
        peerId: p.peerId,
        peerName: peerShortName(p.peerId, p.peerName),
        unproven: p.unproven === true,
        sas: p.sas ?? undefined,
      });
    },
  );
  await listen<{ inviteId: string; reason?: string | null }>("pair://invite_closed", (ev) => {
    const { inviteId, reason } = ev.payload;
    const wasOpen = store.incomingPairInvite()?.inviteId === inviteId;
    store.dropPairInvite(inviteId);
    if (wasOpen) {
      store.toast(
        t("配对邀请已作废：{reason}", { reason: reason ?? t("超时未处理") }),
        "x",
      );
    }
  });

  // **别人发来的**长连接请求（F-16 的同意制）：点「连接」不再直接拨号，先到对方这里问一声。
  // 桌面端是右下角一条带「接受/拒绝」的通知；移动端用一张底板接住（`noteLinkRequest` 会打开
  // 它）。和配对邀请同一取舍：有效期只有 60s（后端 `link::REQUEST_TTL`），等不起 —— 除了
  // toast，还要**当场**把那张表开到脸上。
  await listen<{ requestId: string; peerId: string; peerName: string }>("link://request", (ev) => {
    const p = ev.payload;
    store.noteLinkRequest({ requestId: p.requestId, peerId: p.peerId, peerName: peerShortName(p.peerId, p.peerName) });
    store.toast(
      t("「{name}」请求与本机建立长连接", { name: peerShortName(p.peerId, p.peerName) }),
      "link",
    );
  });

  // 本机刚把一条请求发出去（`connect_peer` 返回 `asked` 时后端发的）：挂上"在等谁同意"
  // 那一档（设备页上可见、可取消）。**真 requestId 只从这条来** —— `connect_peer` 的返回
  // 不带 id（后端明说"界面不该同时从两个地方拼同一件事"）。
  await listen<{ requestId: string; peerId: string; peerName: string }>(
    "link://request_sent",
    (ev) => {
      store.setPendingLinkRequest(ev.payload.requestId, peerShortName(ev.payload.peerId, ev.payload.peerName));
    },
  );

  // 一条请求作废了（超时 / 对端撤回）：收掉底板与本机那一档等待态。发散处照
  // `pair://invite_closed` 的写法：**只在底板正开着时才 toast**（没开表说明用户压根没看见，
  // 再报一句"作废了"是噪音）。
  await listen<{ requestId: string; reason?: string | null }>("link://request_closed", (ev) => {
    const { requestId, reason } = ev.payload;
    const wasOpen = store.incomingLinkRequest()?.requestId === requestId;
    store.dropLinkRequest(requestId);
    store.clearPendingLinkRequest(requestId);
    if (wasOpen) {
      store.toast(
        t("连接请求已作废：{reason}", { reason: reason ?? t("超时未处理") }),
        "x",
      );
    }
  });

  // 本机发出的请求有结果了。三档要如实对应：同意 → 正在建链；拒绝（或对方没处理）→ 把
  // 原因说出来并收等待态；退回直接拨号 → 与旧行为同一句"正在建立长连接"。
  await listen<{
    requestId: string;
    peerId: string;
    outcome: "granted" | "denied" | "fallback";
    reason?: string | null;
  }>("link://request_result", (ev) => {
    const { requestId, outcome, reason } = ev.payload;
    const name = store.pendingLinkRequest()?.peerName ?? t("设备");
    store.clearPendingLinkRequest(requestId);
    if (outcome === "granted") {
      store.toast(t("「{name}」已同意：正在建立长连接", { name }));
    } else if (outcome === "denied") {
      store.toast(
        t("「{name}」没有接受连接请求：{reason}", { name, reason: reason ?? t("对方未处理") }),
        "x",
      );
    } else {
      store.toast(t("正在与「{name}」建立长连接", { name }));
    }
  });

  // 一条消息直连不通、也没有可用的中转（F-106）：**别静默** —— 消息照既有语义排队等它上线，
  // 这里只把"为什么没反应"说出来（桌面端是会话头上一行可关的小字，移动端没有那一层，
  // 走全局提示条。文案两端共用 `undeliverableText`，别各写一份）。
  await listen<Undeliverable>("chat://undeliverable", (ev) => {
    store.toast(undeliverableText(ev.payload), "bell");
  });

  // 本机网卡地址变了（换 Wi-Fi、开热点、VPN 起来）：「设备」页那行「本机 · ip:port」重拉
  // 一次。手机上换网比桌面上常见得多，而这一行是手输 IP 那条路（跨网段手动添加）要念给
  // 对面听的东西 —— 停在旧地址上就是"照它输、连不上"。后端只在**变化时**发这条。
  await listen("local://addresses", () => {
    void store.loadLocalAddr();
  });
})();