/**
 * 移动端的视图层状态：**只放"界面自己的"状态与跨 tab 复用的动作**，业务数据一律来自共享层
 * （`$lib/stores/*` 与 `$lib/api`）。
 *
 * 分工是刻意的：
 * - 会话、设备、设置这些"真相"在共享 store 里，移动端**不重算**（桌面端与移动端必须说同一句话）；
 * - 这个文件只补两件共享层还没有的事：移动端自己的导航/分段状态，以及收件目录、对端浏览
 *   这类"移动端才有的视图"，后者的数据来自后端命令。
 */
import * as api from "$lib/api";
import type { ChatFileMsg, InboxEntry, InstalledApp, LocalPick, PeerShareItem, PickedFile, TreeEntry } from "$lib/types";
import * as chat from "$lib/stores/chat.svelte";
import * as devices from "$lib/stores/devices.svelte";
import { t } from "$lib/i18n.svelte";
import type { IconName } from "./icons";

// ── 导航 ────────────────────────────────────────────────────────────────────

export type TabId = "convos" | "files" | "devices" | "settings";

/** 四个 tab 的键：与 `#/<id>` 路由段、底栏顺序一一对应。 */
const TABS: TabId[] = ["convos", "files", "devices", "settings"];

let tab = $state<TabId>("convos");

export function currentTab(): TabId {
  return tab;
}

export function setTab(next: string): void {
  tab = (TABS as string[]).includes(next) ? (next as TabId) : "convos";
}

/** 文件页的三段：收件 / 传输 / 浏览。 */
export type FilesSeg = "inbox" | "transfer" | "browse";

let filesSeg = $state<FilesSeg>("inbox");

export function currentSeg(): FilesSeg {
  return filesSeg;
}

export function setSeg(next: FilesSeg): void {
  filesSeg = next;
}

/** 会话子屏开着时返回它的键（键的合法性由共享 store 判定，见 `chat.activeThread`）。 */
export function openChatKey(): string | null {
  return chat.activeThread()?.key ?? null;
}

/**
 * 路由：用 hash 是因为它同时白拿了"系统返回键 = 退一层"这条链（webview 的历史栈），
 * 不需要自己维护一份栈。
 *
 * 外壳与各 tab 都从这里走 —— tab 里直接写 `location.hash` 会让"哪一层能退"散在四处。
 */
export function navigateTab(id: TabId): void {
  location.hash = `#/${id}`;
}

export function navigateChat(key: string): void {
  location.hash = `#/chat/${encodeURIComponent(key)}`;
}

/** 从会话子屏退回会话列表（安卓返回键与子屏左上角那颗都用它）。 */
export function navigateBackToList(): void {
  location.hash = `#/${tab}`;
}

/**
 * 打开与一台设备的会话（设备行的「发消息」、设备操作底板那一项、新建会话底板都走这一条）。
 *
 * **两件事必须一起做**：打开会话（共享 store 的 `openDeviceChat`，它把这条键摆到
 * `activeThread` 上）与拨 hash（会话子屏只在 `#/chat/<键>` 上挂，见 `navigateChat`）。
 * 只做前者的话路由还停在原地 —— 界面看着像"点了没反应"。
 */
export function openChat(peerId: string): void {
  chat.openDeviceChat(peerId);
  navigateChat(chat.deviceKey(peerId));
}

// ── 提示条 ──────────────────────────────────────────────────────────────────

export interface Toast {
  id: number;
  text: string;
  icon?: IconName;
  /** 正在淡出（`.leaving`）：先挂上去，再移除。 */
  leaving: boolean;
}

let toasts = $state<Toast[]>([]);
let toastSeq = 0;

export function getToasts(): Toast[] {
  return toasts;
}

/** 一条提示：先显示，再淡出。同一时刻最多留 3 条（更早的直接让位）。 */
export function toast(text: string, icon?: IconName): void {
  const id = ++toastSeq;
  toasts = [...toasts, { id, text, icon, leaving: false }].slice(-3);
  setTimeout(() => {
    toasts = toasts.map((t) => (t.id === id ? { ...t, leaving: true } : t));
    setTimeout(() => {
      toasts = toasts.filter((t) => t.id !== id);
    }, 260);
  }, 2600);
}

/** 动作失败时统一出口：提示原文（后端已经写好了给人看的话），不吞、不改写。 */
export function failed(what: string, e: unknown): void {
  toast(
    t("{what}失败：{error}", {
      what,
      error: e instanceof Error ? e.message : String(e),
    }),
  );
}

// ── 收件目录（移动端才有的视图）──────────────────────────────────────────────

let inbox = $state<InboxEntry[]>([]);
let inboxLoading = $state(false);
let inboxError = $state<string | null>(null);

export function getInbox(): InboxEntry[] {
  return inbox;
}

export function inboxState(): { loading: boolean; error: string | null } {
  return { loading: inboxLoading, error: inboxError };
}

export async function loadInbox(): Promise<void> {
  inboxLoading = true;
  inboxError = null;
  try {
    inbox = await api.listInbox();
  } catch (e) {
    inboxError = e instanceof Error ? e.message : String(e);
  } finally {
    inboxLoading = false;
  }
}

export async function deleteInboxEntry(path: string): Promise<void> {
  await api.deleteInboxFile(path);
  // 收件里删掉的这份副本，浏览页可能还记着「已下载」：把指向它的那些状态一并收掉，
  // 否则那一行还打着勾，点一下只会得到一句"文件不存在"。
  forgetBrowseDownloadsAt(path);
  await loadInbox();
}

// ── 待接收 / 传输中（从会话的文件消息里筛）────────────────────────────────────

/** 一条待接收的推送：会话里那条 `pending` 的文件消息 + 它在哪条会话里。 */
export interface PendingInvite {
  key: string;
  peerId: string;
  msg: ChatFileMsg;
}

/**
 * 还没处理的推送邀请。
 *
 * 数据来自会话 store —— 推送邀请在会话里就是一条文件消息（`inviteState === 'pending'`），
 * 移动端不再单独记一份（桌面端那张气泡卡也是这么来的）。
 */
export function pendingInvites(): PendingInvite[] {
  const out: PendingInvite[] = [];
  for (const row of chat.convoRows()) {
    if (row.mode !== "direct") continue;
    for (const item of chat.itemsOf(row.key)) {
      if (item.kind === "file" && item.inviteState === "pending") {
        out.push({ key: row.key, peerId: item.peerId, msg: item });
      }
    }
  }
  return out;
}

/** 正在动的传输（发出去的还在传、或收进来的还在写盘）。 */
export interface TransferRow {
  id: string;
  name: string;
  isDir: boolean;
  /** `out` = 本机发出去的，`in` = 本机在接收。 */
  dir: "in" | "out";
  /**
   * 这一行"跟谁有关"：**发出去的**是收件方（一台就写名字、多台写「N 台设备」—— 一条文件消息
   * 的 `peerId` 是空的，那是"每台各存一份"留下的，不是"没有收件人"）；**收进来的**是发送方。
   */
  who: string;
  /** 0–100。 */
  pct: number;
  /** 已收/已发字节与总量（接收侧才准）。 */
  received: number;
  total: number;
  /** 展示用的状态串（`stores/chat` 已经写好了文案，这里不另写一套）。 */
  status: string;
}

export function activeTransfers(): TransferRow[] {
  const rows: TransferRow[] = [];
  for (const row of chat.convoRows()) {
    for (const item of chat.itemsOf(row.key)) {
      if (item.kind !== "file") continue;
      const sending = item.sending;
      const receiving = item.inviteState === "accepted";
      if (!sending && !receiving) continue;
      // 发出去那条的收件方要看**这次推送的目标**（`perTargets`）：文件消息是按对端存的，
      // 单条记录上的 `peerId` 因此是空串（那是"每台各存一份"留下的空位，不是"没有收件人"）。
      const targets = item.perTargets ?? [];
      rows.push({
        id: item.id,
        name: item.name,
        isDir: item.isDir,
        dir: sending ? "out" : "in",
        who:
          sending && targets.length > 1
            ? t("{n} 台设备", { n: targets.length })
            : chat.deviceNameOf((sending ? targets[0]?.peerId : item.peerId) ?? ""),
        pct: item.pct,
        received: item.received,
        total: item.bytes,
        status: chat.fileStatusText(item, item.peerId),
      });
    }
  }
  return rows;
}

/** 接收 / 拒绝一条推送邀请。两件事都要落到后端，再让会话 store 更新那条消息的状态。 */
export async function answerInvite(msg: ChatFileMsg, accept: boolean): Promise<void> {
  try {
    if (accept) await api.acceptPush(msg.inviteId);
    else await api.rejectPush(msg.inviteId);
  } catch (e) {
    failed(t(accept ? "接收" : "拒绝"), e);
  }
}

/**
 * 点会话里那条**正在接收**的文件气泡右侧的圆环：进行中 → 暂停，已暂停 → 继续。
 *
 * 后端那道闸与「浏览」下载共用一套（`DownloadControl`）：只让接收循环在下一块之前停住，
 * 已收字节与 `.orbdrop-part` 都留着，继续就是同一条任务接着拉。暂停状态只在内存里
 * （`chat.invitePaused`），与浏览下载同一档 —— 重启后接收自己按续传记录接着跑。
 *
 * 先翻界面再发命令：圆环要在按下的那一帧就变；后端回 `false`（这条接收这一轮已经收尾，
 * 或者正处在两轮重试之间）时**安静地翻回去** —— 终局/重试各有自己的事件，这里再报一句
 * "已不在接收中"只会和"接收中"那张卡自相矛盾。
 */
export async function toggleReceivePause(msg: ChatFileMsg): Promise<void> {
  if (!msg.inviteId || msg.inviteState !== "accepted") return;
  const before = chat.invitePaused(msg.inviteId);
  const want = !before;
  chat.setInvitePaused(msg.inviteId, want);
  try {
    const ok = await api.setPushReceivePaused(msg.inviteId, want);
    if (!ok) {
      // 闸门已经没了（这一轮收尾，或正处在两轮重试之间）：**一律退回"未暂停"**。
      // 退回 `before` 的话，"暂停中点继续却碰上重试空档"会把界面永久留在「已暂停」，
      // 而下一轮重试其实会自己接着收。
      chat.setInvitePaused(msg.inviteId, false);
      return;
    }
    // 命令往返途中这条可能已经收完（`push://done` 先到）：那时不再补一句"已暂停"。
    if (chat.inviteReceiving(msg.inviteId)) {
      toast(t(want ? "已暂停接收" : "已继续接收"), want ? "stop" : "download");
    }
  } catch (e) {
    chat.setInvitePaused(msg.inviteId, before);
    failed(t(want ? "暂停接收" : "继续接收"), e);
  }
}

/**
 * 点自己发出去那条文件气泡右侧的停止圆环：取消这条推送。
 *
 * 与桌面端同一个判据：一对一（含群发副本躺在 1:1 里那一档）只撤**这一台**；群发 / 群聊
 * 那条气泡代表整批，走 `cancel_push_batch`（后端逐台撤、只动还在等 / 在传的）。界面不用
 * 自己改状态：`push://batch` 随后会把这条落成「已取消」/ 整批里少一台。
 */
export async function stopOutgoingFile(msg: ChatFileMsg): Promise<void> {
  if (!msg.fromMe) return;
  const thread = chat.activeThread();
  const peerId = thread?.mode === "direct" ? thread.participants[0] : "";
  const invites = chat.outgoingWaitingInvites(msg, peerId);
  if (invites.length === 0) return;
  try {
    if (!peerId && msg.batchId) {
      await api.cancelPushBatch(msg.batchId);
      return;
    }
    for (const inviteId of invites) await api.cancelPush(inviteId);
  } catch (e) {
    failed(t("停止发送"), e);
  }
}

// ── 会话里的那颗「＋」：选图片 / 选文件 / 选应用（F-118、F-119）────────────────────

/**
 * 刚选好、等着落进输入区的那几张图（`null` = 没有）。
 *
 * **为什么要过 store 这一道**：选图片是由底板（`AttachSheet`）发起的，而"把图插进输入框"
 * 只有会话子屏做得到（`contenteditable` 那个元素在它手上，见 `ChatScreen.svelte`）。底板因此
 * 只把结果放这儿，由会话子屏取走 —— 比让 store 攥着一个 DOM 回调干净，也比让底板自己去
 * `querySelector` 找那个输入框牢靠。
 *
 * **带着 `key` 一起存**：等结果回来时用户可能已经退到别的会话去了（选图那一步在系统选择器
 * 里，可能停留很久）。那几张图只属于当时那条会话 —— 键对不上就谁也不插（子屏取的时候核对）。
 */
let pendingImages = $state<{ key: string; files: PickedFile[] } | null>(null);

/** 会话子屏取走属于自己那条会话的待插图片（取走即清空；不是自己那条就原封不动）。 */
export function takePendingImages(key: string): PickedFile[] | null {
  const held = pendingImages;
  if (!held || held.key !== key) return null;
  pendingImages = null;
  return held.files;
}

/**
 * 那颗「＋」选了之后要走的路：图片进输入区当草稿，文件当场推送。
 *
 * **两条路的语义不同，所以是两项而不是一个框**：图片先进输入区，发送时在 OrbDrop 里是
 * 图片消息、在 LocalSend 里是一条图片消息（线上走文件传输）；文件是**推送**（对端收到一张
 * 「接收 / 拒绝」卡）。混在一个框里让用户猜"我选的这张图会被当成哪一种"，不如让他自己挑。
 *
 * 目标会话就是**此刻开着的那条**（与桌面端 `chatSendFiles` 同一条判据）——「＋」只长在会话
 * 子屏上，没有会话时这个动作根本不会被调到。
 */
export async function pickAttach(kind: "image" | "file"): Promise<void> {
  const thread = chat.activeThread();
  if (!thread) return;
  let files: PickedFile[];
  try {
    files = await api.pickFiles(kind);
  } catch (e) {
    failed(t("选择文件"), e);
    return;
  }
  // 取消（空表）什么都不做：用户反悔是正常操作，不该看到任何提示。
  if (files.length === 0) return;
  if (kind === "image") {
    // 两条协议都先把图放进输入区（和桌面端粘贴同一条路）；点发送才真正发出去。
    pendingImages = { key: thread.key, files };
    return;
  }
  await sendFilesToConvo(
    thread,
    files.map((f) => ({ path: f.path, name: f.name, size: f.size })),
  );
}

/**
 * 要推送的一项：本机路径 + 会话里那条气泡显示的名字 + 字节数。
 *
 * `rename` = 这个名字**只是给对端看的**（本机那份文件的真名不是它）：只有「应用」那一项会
 * 置它 —— 源是 `/data/app/…/base.apk`，落盘名该是
 * `应用名_versionName(versionCode)_包名.apk`（见 `transfer::manifest_for_named_paths`）。
 */
interface PushItem {
  path: string;
  name: string;
  size: number;
  rename?: boolean;
}

/**
 * 把这几份东西推送到这条会话的每一台设备（一对一就是那一台）。
 *
 * 与桌面端 `App.svelte` 的 `chatSendFiles` → `pushPicked` 是同一套：收件人取自这条会话的
 * 参与者，群发那条会话把这批文件记进自己的记录（`chatBatchId`），群聊同样是"拿群成员当收件人
 * 跑一次多目标推送"、本机这条记录挂 `groupId`（群里发文件没有单独的后端命令）。
 *
 * 只发给**此刻在线**的那几台（与桌面端同一条判据）：推送不会排队，一台都够不着时当场说清楚，
 * 而不是让用户对着一张永远不动的进度条等。
 */
async function sendFilesToConvo(thread: chat.ConvoThread, files: PushItem[]): Promise<void> {
  const peers = devices.devicesById(thread.participants);
  const live = peers.filter((d) => d.online);
  if (live.length === 0) {
    toast(
      peers.length === 1
        ? t("「{name}」不在网上，推送未发送", { name: devices.deviceShortName(peers[0]) })
        : t("所选设备都不在网上，推送未发送"),
    );
    return;
  }
  const items: LocalPick[] = files.map((f) => ({ path: f.path, name: f.name, isDir: false, size: f.size }));
  // 覆盖名逐位对齐：不改名的那些给空串（后端按空串 = 不覆盖处理）。整表全空时干脆不传
  // —— 老那条路（选择器选的文件）与以前一个字节都不差。
  const names = files.map((f) => (f.rename ? f.name : ""));
  const keepName = names.some((n) => n !== "");
  try {
    const batch = await api.pushPaths(
      files.map((f) => f.path),
      live.map((d) => d.id),
      undefined,
      keepName ? names : undefined,
    );
    // 会话里那条文件气泡当场就有了（进度与成败随后由 `push://batch` 推着走，见 `events.ts`）。
    chat.noteOutgoingFiles(
      live.map((d) => d.id),
      items,
      batch,
      thread.mode === "broadcast" ? thread.id : "",
      thread.mode === "group" ? thread.id : "",
    );
    toast(
      live.length === 1
        ? t("正在推送到「{name}」", { name: devices.deviceShortName(live[0]) })
        : t("正在推送到 {n} 台设备", { n: live.length }),
      "file",
    );
  } catch (e) {
    failed(t("推送"), e);
  }
}

// ── 会话里那颗「＋」的第三项：发送应用（F-119）──────────────────────────────

/**
 * 设备上已安装的应用（`null` = 还没读过）。
 *
 * **留着上一份**：这张表是原生现查的（一次包管理器走查），而底板是"点开就重现"的东西 ——
 * 每次都只画一个转圈会让这张表显得很慢。留着旧的一份先画出来，新的回来再换（`loadApps`
 * 每次都重查：装了/卸了应用之后表得跟着变）。
 */
let apps = $state<InstalledApp[] | null>(null);
let appsLoading = $state(false);
let appsError = $state<string | null>(null);

export function installedApps(): InstalledApp[] | null {
  return apps;
}

export function appsState(): { loading: boolean; error: string | null } {
  return { loading: appsLoading, error: appsError };
}

/** 重新读一遍设备上的应用（进那张底板时调一次；失败把话说出来，不留一个空表）。 */
export async function loadApps(): Promise<void> {
  appsLoading = true;
  appsError = null;
  try {
    apps = await api.listInstalledApps();
  } catch (e) {
    appsError = e instanceof Error ? e.message : String(e);
  } finally {
    appsLoading = false;
  }
}

/**
 * 发送一个应用：把**它自己的 `base.apk`** 推给这条会话的每一台，落盘名用后端拼好的那个
 * （`应用名_versionName(versionCode)_包名.apk`）。
 *
 * **不拷文件**：本机从 `/data/app/…/base.apk` 直接读，名字只写在清单里（见 `PushItem.rename`）。
 */
export async function sendApp(app: InstalledApp): Promise<void> {
  const thread = chat.activeThread();
  if (!thread) return;
  await sendFilesToConvo(thread, [{ path: app.path, name: app.fileName, size: app.size, rename: true }]);
}

// ── 浏览对端共享项 ──────────────────────────────────────────────────────────

let browsePeer = $state<string | null>(null);
let browseShares = $state<PeerShareItem[]>([]);
let browseLoading = $state(false);
let browseError = $state<string | null>(null);
/** 已展开的共享项 → 那一层的条目。键是共享项 id（对端共享项只有一层展开，不做无限深钻）。 */
let browseChildren = $state<Record<string, TreeEntry[]>>({});

export function getBrowsePeer(): string | null {
  return browsePeer;
}

export function browseSharesOf(): PeerShareItem[] {
  return browseShares;
}

export function browseState(): { loading: boolean; error: string | null } {
  return { loading: browseLoading, error: browseError };
}

export function browseChildrenOf(shareId: string): TreeEntry[] | undefined {
  return browseChildren[shareId];
}

/** 换一台设备：清掉上一个的展开状态，重新拉共享项。 */
export async function selectBrowsePeer(peerId: string): Promise<void> {
  if (browsePeer !== peerId) browseChildren = {};
  browsePeer = peerId;
  await loadBrowseShares();
}

/**
 * 收掉整个浏览状态：正在浏览的那台不在可浏览集合里了（掉线 / 断开长连接）时由界面调用。
 * 留着选中会让底下列着已经读不到的旧共享项 —— 设备都没了，那份数据不该再挂在那儿。
 */
export function clearBrowsePeer(): void {
  browsePeer = null;
  browseShares = [];
  browseChildren = {};
  browseError = null;
  browseLoading = false;
}

export async function loadBrowseShares(): Promise<void> {
  const peerId = browsePeer;
  if (!peerId) return;
  browseLoading = true;
  browseError = null;
  try {
    const items = await api.peerShares(peerId);
    // 期间换了设备（或被 `clearBrowsePeer` 收掉）：这一份属于上一个 peer，作废。
    if (browsePeer !== peerId) return;
    browseShares = items;
  } catch (e) {
    if (browsePeer !== peerId) return;
    browseShares = [];
    browseError = e instanceof Error ? e.message : String(e);
  } finally {
    if (browsePeer === peerId) browseLoading = false;
  }
}

/** 展开/收起一个共享项；展开过就留着，收起再展开不重拉。 */
export async function toggleShare(shareId: string): Promise<void> {
  const peerId = browsePeer;
  if (!peerId) return;
  if (browseChildren[shareId]) {
    const next = { ...browseChildren };
    delete next[shareId];
    browseChildren = next;
    return;
  }
  try {
    // `api.peerTree` 已经把后端的 `{ entries }` 拆开了，返回值就是那一层本身。
    const entries = await api.peerTree(peerId, shareId, "");
    browseChildren = { ...browseChildren, [shareId]: entries };
  } catch (e) {
    failed(t("列出共享项"), e);
  }
}

// ── 浏览段的下载状态 ────────────────────────────────────────────────────────
//
// 后端 `download_from_peer` 阻塞到落盘才回话，中途的字节数由 `browse://download` 事件送来。
// 这里按 `peerId + shareId + path` 记住每一行（`path` 为空 = 共享项根就是一个文件）：
// 「浏览」那行右侧的圆环按它画进度、点它暂停/继续；「传输」页签数正在下几项；
// 下载完成后点这一行直接打开。

export interface BrowseDownload {
  /** `peerId\0shareId\0path`。 */
  key: string;
  peerId: string;
  shareId: string;
  /** 共享项内相对路径；共享项根是文件时为空串。 */
  path: string;
  name: string;
  /** 对端报的大小；0 = 不知道（老版本对端 / 共享项根文件），进度只报已收字节。 */
  total: number;
  received: number;
  status: "running" | "paused" | "done" | "failed";
  /** 落盘路径（`done` 才有）。 */
  dest: string | null;
  error: string | null;
}

let browseDownloads = $state<Record<string, BrowseDownload>>({});

function browseKey(peerId: string, shareId: string, path: string): string {
  return `${peerId}\u0000${shareId}\u0000${path}`;
}

/** 当前浏览的这台设备上，某个共享项（根或它的直接子项）的下载状态。 */
export function browseDownloadOf(shareId: string, path: string): BrowseDownload | undefined {
  const peerId = browsePeer;
  if (!peerId) return undefined;
  return browseDownloads[browseKey(peerId, shareId, path)];
}

/** 还在传输的行（进行中 + 已暂停；「传输」页签的角标与「进行中」表都读它）。 */
export function activeBrowseDownloads(): BrowseDownload[] {
  return Object.values(browseDownloads).filter(
    (d) => d.status === "running" || d.status === "paused",
  );
}

/** 收件里删掉了一条副本：把浏览页那些指向同一落盘路径的「已下载」状态一并作废。 */
function forgetBrowseDownloadsAt(dest: string): void {
  const next: Record<string, BrowseDownload> = {};
  let changed = false;
  for (const [key, d] of Object.entries(browseDownloads)) {
    if (d.dest === dest) {
      changed = true;
      continue;
    }
    next[key] = d;
  }
  if (changed) browseDownloads = next;
}

function putBrowseDownload(key: string, next: BrowseDownload): void {
  browseDownloads = { ...browseDownloads, [key]: next };
}

/** 落盘成功：只收一次尾（事件与命令返回谁先到都行），顺带把收件页签的数补上。 */
function finishBrowseDownload(key: string, received: number, dest: string): void {
  const cur = browseDownloads[key];
  if (!cur || cur.status === "done") return;
  putBrowseDownload(key, { ...cur, received, status: "done", dest, error: null });
  toast(t("已下载"), "download");
  // 收件页签那颗数就是收件目录的长度（`list_inbox`）：不重拉，下载完它不会 +1。
  void loadInbox();
}

/** 后端 `browse://download` 事件：进度 / 落盘 / 失败。 */
export function noteBrowseDownload(ev: {
  peerId: string;
  shareId: string;
  path: string;
  name: string;
  status: "progress" | "done" | "error";
  received: number;
  dest?: string | null;
  error?: string | null;
}): void {
  const key = browseKey(ev.peerId, ev.shareId, ev.path);
  const cur = browseDownloads[key];
  if (ev.status === "progress") {
    // 终局之后的迟到进度（事件与命令返回有先后）不再把已完成翻回「下载中」；暂停期间
    // 已经发出的一块也照收字节数，但状态保持「已暂停」，等界面点继续再变回 running。
    if (cur?.status === "done" || cur?.status === "failed") return;
    putBrowseDownload(key, {
      key,
      peerId: ev.peerId,
      shareId: ev.shareId,
      path: ev.path,
      name: ev.name || cur?.name || t("文件"),
      total: cur?.total ?? 0,
      received: Math.max(0, ev.received),
      status: cur?.status === "paused" ? "paused" : "running",
      dest: null,
      error: null,
    });
    return;
  }
  if (ev.status === "done") {
    if (ev.dest && cur) finishBrowseDownload(key, ev.received, ev.dest);
    return;
  }
  // 失败：圆环退回下载键，点它可以重试（`activateBrowseEntry` 把状态改回 running）。
  if (cur) putBrowseDownload(key, { ...cur, status: "failed", error: ev.error ?? t("下载失败") });
}

/**
 * 暂停 / 继续一次浏览下载（后端那道闸只停流、不丢临时文件与 offset）。
 *
 * 返回 `false`（命令已经结束）时不改状态：完成 / 失败由终局事件自己收尾，
 * 这里只补一句提示 —— 否则界面会停在「已暂停」，其实文件早就下完了。
 */
async function setBrowsePaused(
  shareId: string,
  path: string,
  paused: boolean,
): Promise<void> {
  const peerId = browsePeer;
  if (!peerId) return;
  const key = browseKey(peerId, shareId, path);
  const cur = browseDownloads[key];
  if (!cur || (cur.status !== "running" && cur.status !== "paused")) return;
  const before = cur.status;
  const want = paused ? "paused" : "running";
  // 先改界面：圆环要在按下的那一帧就变（等 IPC 回来再变，手快的人会以为没点上）。
  // 后端没找到这条命令（已经完成/失败）时再退回去，终局事件本来也会随后收尾。
  putBrowseDownload(key, { ...cur, status: want });
  try {
    const ok = await api.setBrowseDownloadPaused(peerId, shareId, path, paused);
    if (!ok) {
      const now = browseDownloads[key];
      if (now && now.status === want) putBrowseDownload(key, { ...now, status: before });
      // 终局事件如果已经先到（下完了 / 失败了），那句"已不在下载中"就是多余的。
      if (!now || (now.status !== "done" && now.status !== "failed")) {
        toast(t("这项已不在下载中"));
      }
      return;
    }
    const now = browseDownloads[key];
    if (now && now.status === want) {
      toast(t(paused ? "已暂停下载" : "已继续下载"), paused ? "stop" : "download");
    }
  } catch (e) {
    const now = browseDownloads[key];
    if (now && now.status === want) putBrowseDownload(key, { ...now, status: before });
    failed(t(paused ? "暂停下载" : "继续下载"), e);
  }
}

/**
 * 点浏览里文件行右侧那一圈进度环：进行中 → 暂停，已暂停 → 继续，其余 → 开始下载
 * （已完成的环是勾，不响应这个动作；行本身仍然负责打开）。
 */
export async function toggleBrowseDownload(
  shareId: string,
  path: string,
  name: string,
  size?: number | null,
): Promise<void> {
  const dl = browseDownloadOf(shareId, path);
  if (dl?.status === "running") {
    await setBrowsePaused(shareId, path, true);
    return;
  }
  if (dl?.status === "paused") {
    await setBrowsePaused(shareId, path, false);
    return;
  }
  await activateBrowseEntry(shareId, path, name, size);
}

/**
 * 点浏览里的一项：没下载过就开下，暂停中就从暂停处继续，正在下就只说一声，已经下好就
 * 直接用系统应用打开。
 *
 * 后端自己决定落盘位置与命名；「每次询问」里用户在保存框点取消时返回 `null` —— 那什么都
 * 没发生，不能报「已开始下载」，也不能把人送到传输页。
 */
export async function activateBrowseEntry(
  shareId: string,
  path: string,
  name: string,
  size?: number | null,
): Promise<void> {
  const peerId = browsePeer;
  if (!peerId) return;
  const key = browseKey(peerId, shareId, path);
  const cur = browseDownloads[key];
  if (cur?.status === "running") {
    toast(t("正在下载"));
    return;
  }
  if (cur?.status === "paused") {
    await setBrowsePaused(shareId, path, false);
    return;
  }
  if (cur?.status === "done" && cur.dest) {
    try {
      await api.openPath(cur.dest);
      toast(t("已打开文件"), "file");
    } catch (e) {
      // 文件被删 / 被移走：这一行退回「可下载」，否则它永远指着不存在的路径。
      const next = { ...browseDownloads };
      delete next[key];
      browseDownloads = next;
      failed(t("打开"), e);
    }
    return;
  }
  // 先落一行 running：命令返回前那一行就要显示 0%，失败重试也走这里。
  putBrowseDownload(key, {
    key,
    peerId,
    shareId,
    path,
    name,
    total: size ?? 0,
    received: 0,
    status: "running",
    dest: null,
    error: null,
  });
  try {
    const saved = await api.downloadFromPeer(peerId, shareId, path, name);
    if (!saved) {
      // 保存框里取消：什么都没发生，把那一行收掉（不 toast、不切页）。
      const next = { ...browseDownloads };
      delete next[key];
      browseDownloads = next;
      return;
    }
    // 正常情况 `done` 事件已经先到；这一条是兜底（事件通道丢了也能收尾）。
    finishBrowseDownload(key, browseDownloads[key]?.received ?? 0, saved);
  } catch (e) {
    const now = browseDownloads[key];
    if (now && now.status !== "failed") {
      putBrowseDownload(key, {
        ...now,
        status: "failed",
        error: e instanceof Error ? e.message : String(e),
      });
    }
    failed(t("下载"), e);
  }
}

// ── 设备动作 ────────────────────────────────────────────────────────────────

/** 连接/断开、解除配对、手动添加：全部转发到后端，再由设备 store 的事件刷新列表。 */
export async function toggleConnection(peerId: string, connected: boolean): Promise<void> {
  const peer = devices.deviceById(peerId);
  const name = peer ? devices.deviceShortName(peer) : t("设备");
  // 这一台此刻"看得见却直连不通"时，那句解释在这里说（连接是一个动作，不是一个常驻状态）。
  const tip = connected ? null : devices.unicastBlockedTip(peer);
  try {
    if (connected) {
      await api.disconnectPeer(peerId);
      if (tip) toast(tip);
      return;
    }
    // 「连接」= 先向对端发一条**请求**（F-16 的同意制），不是直接拨号。
    const outcome = await api.connectPeer(peerId);
    if (outcome.kind === "asked") {
      // 真 requestId 由 `link://request_sent` 补上（后端不从这个返回里给 id，见 `connect_peer`）；
      // 这里只在事件还没到时先占一档，好让"在等谁同意"当场可见、可取消。
      if (!pendingLinkRequest()) setPendingLinkRequest("", peer ? devices.deviceShortName(peer) : t("设备"));
      toast(t("已把连接请求发给「{name}」：等它在自己的通知里点「接受」", { name }));
      return;
    }
    // 对端不在网上 / 链路本就在：后端直接拨号，与旧行为同一句（那儿还有"看得见却连不上"的解释）。
    toast(tip ?? t("正在与「{name}」建立长连接", { name }));
  } catch (e) {
    failed(t(connected ? "断开" : "连接"), e);
  }
}

/**
 * 解除配对：会清掉两侧保存的 token（对端下次推送/拨号就认不出本机了），误触代价高，
 * 后端也没有"撤销" —— 所以先问一句。**这一句只写在这里**：设备行与设备操作底板都调它，
 * 两处各写一遍的话文案与判据迟早分叉。
 *
 * 返回"真的解除了"：在问一句那里被用户取消时，调用方（设备的操作底板）不该跟着关表。
 */
export async function unpair(peerId: string, name: string): Promise<boolean> {
  const ok = await confirmAction({
    title: t("解除配对"),
    desc: t("解除与「{name}」的配对？对端将不再认得本机，需要重新配对。", { name }),
    confirmLabel: t("解除配对"),
    danger: true,
  });
  if (!ok) return false;
  try {
    await api.removePeer(peerId);
    toast(t("已解除与「{name}」的配对", { name }));
    return true;
  } catch (e) {
    failed(t("解除配对"), e);
    return false;
  }
}

/**
 * 写一台设备的备注（F-101）：`''` = 清除，返回后端真正存下去的值（`null` = 已清除）。
 *
 * 与桌面端**同一条路**（后端同一命令 `set_device_remark`、同一个落盘位置 `config.toml` 的
 * `deviceRemarks`）：备注只写本机，不发给对端、也不改对方自己的设备名。提示里报的是这台设备
 * **自己的设备名**（对端自报的 `name`，不含备注）与真正存下去的值 —— 用户得看得出改的是哪台。
 *
 * 失败不在这里吞：由调用方（备注底板）报错并把底板留着，用户改完能再试。
 */
export async function saveRemark(
  peerId: string,
  rawName: string,
  remark: string,
): Promise<string | null> {
  const stored = await devices.saveDeviceRemark(peerId, remark);
  toast(
    stored
      ? t("已把「{name}」的备注设为「{remark}」", { name: rawName, remark: stored })
      : t("已清除「{name}」的备注", { name: rawName }),
  );
  return stored;
}

export async function addManualPeer(host: string, port: number): Promise<void> {
  try {
    await api.addPeer(host, port);
    toast(t("已添加 {host}:{port}，等待配对", { host, port }));
  } catch (e) {
    failed(t("添加设备"), e);
  }
}

// ── 设置表单 ────────────────────────────────────────────────────────────────

/**
 * 设置页的编辑态：只放"用户正在改"的那几个字段，保存时一次性交给 `update_settings`。
 * 其余字段（下载目录、在线状态等）直接读 `get_settings` 的结果，不进这里。
 */
export interface SettingsForm {
  deviceName: string;
  port: string;
}

let settingsForm = $state<SettingsForm | null>(null);

export function form(): SettingsForm | null {
  return settingsForm;
}

/** 从后端设置里灌一次初值（每次进设置页都可以重灌）。 */
export function resetForm(s: { deviceName: string; port: number }): void {
  settingsForm = { deviceName: s.deviceName, port: String(s.port) };
}

export function editForm(patch: Partial<SettingsForm>): void {
  if (settingsForm) settingsForm = { ...settingsForm, ...patch };
}

// ── 弹出的底板（sheet）──────────────────────────────────────────────────────
//
// 同一时刻只可能有一张：它们是模态的，而且都盖在整个 app 上（`.sheet-layer` 是 absolute
// inset:0）。所以槽位放在这里由外壳统一渲染，各 tab 只说"要开哪一张" —— 免得每个 tab 自己
// 造一层定位祖先，最后叠出两张同时打开的底板。

/**
 * 应用内确认框的内容。
 *
 * **为什么不用 `window.confirm`**：Android WebView 会把它交给 Tauri 生成的
 * `RustWebChromeClient`，弹的是系统 `AlertDialog`（Material 外观、按钮写死英文 OK/Cancel），
 * 与移动端这套 `.sheet` 设计完全不是一回事。确认框因此也走 `Sheet` —— 同一套圆角、字号、
 * 按钮与遮罩，用户看不出这是"另一个系统"。
 */
export type ConfirmPrompt = {
  /** 动作名（「删除本地副本」「解除配对」）。 */
  title: string;
  /** 一句把后果说清楚的话；换行交给 CSS。 */
  desc: string;
  /** 确认按钮文案（「删除」「清空」「解除配对」）。 */
  confirmLabel: string;
  /** 取消按钮文案，缺省「取消」。 */
  cancelLabel?: string;
  /** true = 红色危险按钮；缺省普通主按钮。 */
  danger?: boolean;
};

export type Sheet =
  /** `peerId` = 从某台设备的行/底板点进来：配对只针对这一台，不列全部候选。 */
  | { kind: "pair"; peerId?: string }
  | { kind: "pairInvite" }
  | { kind: "linkRequest" }
  | { kind: "addDevice" }
  | { kind: "deviceActions"; peerId: string }
  | { kind: "remarkDevice"; peerId: string }
  | { kind: "fileActions"; entry: InboxEntry }
  | { kind: "chatActions"; key: string }
  /** 长按一条消息：移动端的"消息右键菜单"（桌面端见 `ContextMenu.svelte` 的 `kind === 'chat'`）。 */
  | { kind: "messageActions"; copyText: string; desc: string }
  | { kind: "attach" }
  | { kind: "appPicker" }
  | { kind: "convo" }
  | { kind: "editRecipients"; key: string }
  | { kind: "keepAlive" }
  | { kind: "update" }
  /** 只由 `confirmAction` 创建：直接 `openSheet` 一个 confirm 不会有人结算 Promise。 */
  | { kind: "confirm"; prompt: ConfirmPrompt };

let sheet = $state<Sheet | null>(null);

/**
 * 当前确认框的 Promise 结算函数。**不放 `$state`**：它只是回调，不是要渲染的数据。
 * 打开确认框时替换掉当前这张 sheet（来源的操作表随之下架），取消/确定后再回到页面 ——
 * 与"操作表里点危险项 → 弹确认"的系统惯例一致。
 */
let confirmResolver: ((ok: boolean) => void) | null = null;

export function openSheet(next: Sheet): void {
  // 确认框被别的底板顶掉时按"取消"结算，别让调用方的 await 永远悬着。
  if (sheet?.kind === "confirm") {
    const resolve = confirmResolver;
    confirmResolver = null;
    resolve?.(false);
  }
  sheet = next;
}

export function closeSheet(): void {
  if (sheet?.kind === "confirm") {
    resolveConfirm(false);
    return;
  }
  sheet = null;
}

export function currentSheet(): Sheet | null {
  return sheet;
}

/**
 * 应用内确认框：打开一张与其它底板同款的 Sheet，返回用户的选择。
 *
 * 调用方通常是从某张操作表里点出来的（删除收件、退出群聊、解除配对…），所以这里直接替换
 * 当前 sheet —— 取消后回到页面，而不是回到那张已经点过的操作表。
 */
export function confirmAction(prompt: ConfirmPrompt): Promise<boolean> {
  // 同时只允许一个：旧的按"取消"结算，免得上一个调用方永远等下去。
  if (confirmResolver) {
    const previous = confirmResolver;
    confirmResolver = null;
    previous(false);
  }
  // 不用 `Promise.withResolvers`：它是 Chrome 119 才有的，而移动端下限是 Chrome 81
  //（`scripts/check-compat.mjs` 的 FLOOR）。这里只能走 executor 形式。
  return new Promise((resolve) => {
    confirmResolver = resolve;
    sheet = { kind: "confirm", prompt };
  });
}

/** 结算当前确认框（true = 确定；false = 取消、返回键、遮罩或被别的底板顶掉）。 */
export function resolveConfirm(ok: boolean): void {
  const resolve = confirmResolver;
  confirmResolver = null;
  if (sheet?.kind === "confirm") sheet = null;
  resolve?.(ok);
}

/**
 * 首次运行引导（F-87）：装完那一刻，国产 ROM 的「应用启动管理」默认会在后台把进程冻掉 ——
 * 用户看到的是"别人推不过来"，而通知栏那条常驻通知还挂着（从外面看不出毛病）。所以第一次进
 * 应用就把这件事说清楚，并把话说到"去哪一页、开哪几个开关"。
 *
 * 只问一次：`Config::keepalive_guided` 落盘、跨重启有效。**已经在白名单里的机器也记一笔** ——
 * 它没什么可引导的，不该每次开机都被问一遍。读不到（例如命令没注册）就安静地什么都不做：
 * 这是"多说一句"，不是功能，不该在这里弹错误。
 */
let keepAliveAsked = false;

export async function offerKeepAliveGuide(): Promise<void> {
  if (keepAliveAsked) return;
  keepAliveAsked = true;
  try {
    if (await api.keepaliveGuided()) return;
    await api.markKeepaliveGuided();
    const state = await api.keepAliveState();
    if (state.batteryOptimizationIgnored) return;
    openSheet({ kind: "keepAlive" });
  } catch {
    return;
  }
}

// ── 配对邀请（两端）──────────────────────────────────────────────────────────
//
// **别人发来的**（`pair://invite`）：桌面端挂在右下角通知里等用户决定；移动端没有那组气泡，
// 用一张底板接住 —— 配对邀请只有 60s（`pair::INVITE_TTL`），比推送邀请更需要当场处理。
//
// **本机发出去的**（`pair_invite` 的返回 = 邀请 id）：桌面端是通知组里一张「等待对方确认配对」
// 的卡（带「取消」）；移动端在配对表里就地变成这一档，取消走 `cancel_pair_invite`（撤掉
// 对端界面上那条通知）。结果由 `pair://completed` / `pair://invite_failed` 回来。

export type IncomingPairInvite = {
  inviteId: string;
  peerId: string;
  peerName: string;
  /** 自称是本机已配对的设备却没带凭据：卡片上要把这件事说明白。 */
  unproven: boolean;
  /** 8 位 SAS：两端应显示同一串；空 = 对端版本过旧，无法核对。 */
  sas?: string;
};

let pairInvite = $state<IncomingPairInvite | null>(null);
let pairWait = $state<{ inviteId: string; peerName: string; sas: string } | null>(null);

export function incomingPairInvite(): IncomingPairInvite | null {
  return pairInvite;
}

/** 收到一条配对邀请：挂上并当场开那张表（`pair://invite`）。 */
export function notePairInvite(invite: IncomingPairInvite): void {
  pairInvite = invite;
  openSheet({ kind: "pairInvite" });
}

/** 邀请作废了（超时 / 对方撤回）：把表和状态一起收掉。 */
export function dropPairInvite(inviteId: string): void {
  if (pairInvite?.inviteId !== inviteId) return;
  pairInvite = null;
  if (sheet?.kind === "pairInvite") closeSheet();
}

/** 用户在表上点了「接收 / 拒绝」。 */
export async function answerPairInvite(accept: boolean): Promise<void> {
  const invite = pairInvite;
  if (!invite) return;
  pairInvite = null;
  try {
    if (accept) {
      const peerName = await api.acceptPairInvite(invite.inviteId);
      toast(t("已与「{name}」完成配对", { name: peerName }));
    } else {
      await api.rejectPairInvite(invite.inviteId);
      toast(t("已拒绝「{name}」的配对邀请", { name: invite.peerName }), "x");
    }
  } catch (e) {
    failed(t(accept ? "接受配对邀请" : "拒绝配对邀请"), e);
  }
}

/** 本机发出去、还在等对方确认的邀请（空 = 没有）。 */
export function pendingPairInvite(): { inviteId: string; peerName: string; sas: string } | null {
  return pairWait;
}

export function setPendingPairInvite(inviteId: string, peerName: string, sas: string): void {
  pairWait = { inviteId, peerName, sas };
}

export function clearPendingPairInvite(inviteId?: string): void {
  if (!pairWait) return;
  if (inviteId && pairWait.inviteId !== inviteId) return;
  pairWait = null;
}

/** 用户点了「取消邀请」：撤回对端界面上那条通知，然后收掉这一档。 */
export async function cancelPendingPairInvite(): Promise<void> {
  const pending = pairWait;
  if (!pending) return;
  pairWait = null;
  try {
    await api.cancelPairInvite(pending.inviteId);
    toast(t("已取消对「{name}」的配对邀请", { name: pending.peerName }), "x");
  } catch (e) {
    failed(t("取消配对邀请"), e);
  }
}

// ── 连接请求（两端）──────────────────────────────────────────────────────────
//
// F-16 的同意制：点「连接」不直接拨号，先给对方发一条**请求**，对方在自己的通知里点「接受」
// 才建链。形状与「配对邀请（两端）」那一段一致 —— 同样是"别人发来的挂一张底板接住、本机
// 发出去的停在等待那一档、结果/作废由事件回来"，所以放在一起读。
//
// **别人发来的**（`link://request`）：只有 60s 有效期，比推送邀请更等不了，当场开底板。
// **本机发出去的**（`link://request_sent`）：请求 id 由事件带来（`connect_peer` 的返回不带
// id，后端明说"界面不该同时从两个地方拼同一件事"），设备页上因此显示「在等谁同意」并能取消。

export type IncomingLinkRequest = {
  requestId: string;
  peerId: string;
  peerName: string;
};

let linkRequest = $state<IncomingLinkRequest | null>(null);
let linkWait = $state<{ requestId: string; peerName: string } | null>(null);

export function incomingLinkRequest(): IncomingLinkRequest | null {
  return linkRequest;
}

/** 收到一条连接请求：挂上并当场开那张表（`link://request`）。 */
export function noteLinkRequest(req: IncomingLinkRequest): void {
  linkRequest = req;
  openSheet({ kind: "linkRequest" });
}

/** 请求作废了（超时 / 对方撤回）：把表和状态一起收掉。 */
export function dropLinkRequest(requestId: string): void {
  if (linkRequest?.requestId !== requestId) return;
  linkRequest = null;
  if (sheet?.kind === "linkRequest") closeSheet();
}

/** 用户在表上点了「接受 / 拒绝」。接受走 `accept_link_request`，返回对方设备名。 */
export async function answerLinkRequest(accept: boolean): Promise<void> {
  const req = linkRequest;
  if (!req) return;
  linkRequest = null;
  try {
    if (accept) {
      const peerName = await api.acceptLinkRequest(req.requestId);
      toast(t("已同意「{name}」建立长连接", { name: peerName }));
    } else {
      await api.rejectLinkRequest(req.requestId);
      toast(t("已拒绝「{name}」的连接请求", { name: req.peerName }), "x");
    }
  } catch (e) {
    failed(t(accept ? "接受连接请求" : "拒绝连接请求"), e);
  }
}

/** 本机发出去、还在等对方同意的请求（空 = 没有）。 */
export function pendingLinkRequest(): { requestId: string; peerName: string } | null {
  return linkWait;
}

export function setPendingLinkRequest(requestId: string, peerName: string): void {
  linkWait = { requestId, peerName };
}

export function clearPendingLinkRequest(requestId?: string): void {
  if (!linkWait) return;
  if (requestId && linkWait.requestId !== requestId) return;
  linkWait = null;
}

/** 用户点了「取消」：撤回对方界面上那条请求，然后收掉这一档。 */
export async function cancelPendingLinkRequest(): Promise<void> {
  const pending = linkWait;
  if (!pending) return;
  linkWait = null;
  try {
    // 真 id 来自 `link://request_sent`；万一还没到（极短的一瞬），就只收掉本机这一档，
    // 不拿空 id 去撞后端。
    if (pending.requestId) await api.cancelLinkRequest(pending.requestId);
    toast(t("已取消对「{name}」的连接请求", { name: pending.peerName }), "x");
  } catch (e) {
    failed(t("取消连接请求"), e);
  }
}

// ── 本机地址（设备页那行「本机 · ip:port」）────────────────────────────────────
//
// 端口要的是**实际绑定**的那个（配置端口被占时会顺延），所以走 `get_actual_port`；
// 地址取首选内网地址（多网卡时后端已经挑好了，见 F-09）。

let localAddr = $state<{ ip: string; port: number } | null>(null);

export function getLocalAddr(): { ip: string; port: number } | null {
  return localAddr;
}

export async function loadLocalAddr(): Promise<void> {
  try {
    const [port, addrs] = await Promise.all([api.getActualPort(), api.localAddresses()]);
    localAddr = { ip: addrs.primary ?? "", port };
  } catch (e) {
    // 取不到就不显示地址那一截（离线/无网卡时本就该只显示端口），不是错误。
    localAddr = null;
    void e;
  }
}
