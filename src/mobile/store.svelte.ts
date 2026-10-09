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
import type { InboxEntry, PeerShareItem, TreeEntry } from "$lib/types";
import * as chat from "$lib/stores/chat.svelte";
import type { ChatFileMsg } from "$lib/stores/chat.svelte";
import * as devices from "$lib/stores/devices.svelte";
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
  toast(`${what}失败：${e instanceof Error ? e.message : String(e)}`);
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
  peerId: string;
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
      rows.push({
        id: item.id,
        name: item.name,
        isDir: item.isDir,
        dir: sending ? "out" : "in",
        peerId: item.peerId,
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
    failed(accept ? "接收" : "拒绝", e);
  }
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

export async function loadBrowseShares(): Promise<void> {
  const peerId = browsePeer;
  if (!peerId) return;
  browseLoading = true;
  browseError = null;
  try {
    browseShares = await api.peerShares(peerId);
  } catch (e) {
    browseShares = [];
    browseError = e instanceof Error ? e.message : String(e);
  } finally {
    browseLoading = false;
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
    failed("列出共享项", e);
  }
}

/**
 * 下载对端的一项到本机收件目录（后端自己决定落盘位置与命名）。
 *
 * 后端在「每次询问」下会弹保存框；用户在框里取消时返回 `null` —— 那什么都没发生，
 * 不能报"已开始下载"、更不能把人送到传输页。
 */
export async function downloadEntry(shareId: string, path: string, name: string): Promise<void> {
  const peerId = browsePeer;
  if (!peerId) return;
  try {
    const saved = await api.downloadFromPeer(peerId, shareId, path);
    if (!saved) return;
    toast(`已开始下载「${name}」`, "download");
    filesSeg = "transfer";
  } catch (e) {
    failed("下载", e);
  }
}

// ── 设备动作 ────────────────────────────────────────────────────────────────

/** 连接/断开、解除配对、手动添加：全部转发到后端，再由设备 store 的事件刷新列表。 */
export async function toggleConnection(peerId: string, connected: boolean): Promise<void> {
  // 这一台此刻"看得见却直连不通"时，那句解释在这里说（连接是一个动作，不是一个常驻状态）。
  const tip = connected ? null : devices.unicastBlockedTip(devices.getDevices().find((d) => d.id === peerId));
  try {
    if (connected) await api.disconnectPeer(peerId);
    else await api.connectPeer(peerId);
    if (tip) toast(tip);
  } catch (e) {
    failed(connected ? "断开" : "连接", e);
  }
}

export async function unpair(peerId: string, name: string): Promise<void> {
  try {
    await api.removePeer(peerId);
    toast(`已解除与「${name}」的配对`);
  } catch (e) {
    failed("解除配对", e);
  }
}

export async function renamePeer(peerId: string, name: string): Promise<void> {
  try {
    await api.renamePeer(peerId, name);
    toast(`已重命名为「${name}」`);
  } catch (e) {
    failed("重命名", e);
  }
}

export async function addManualPeer(host: string, port: number): Promise<void> {
  try {
    await api.addPeer(host, port);
    toast(`已添加 ${host}:${port}，等待配对`);
  } catch (e) {
    failed("添加设备", e);
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

export type Sheet =
  | { kind: "pair" }
  | { kind: "pairInvite" }
  | { kind: "addDevice" }
  | { kind: "deviceActions"; peerId: string }
  | { kind: "renameDevice"; peerId: string }
  | { kind: "fileActions"; entry: InboxEntry }
  | { kind: "chatActions"; key: string }
  | { kind: "convo" }
  | { kind: "editRecipients"; key: string };

let sheet = $state<Sheet | null>(null);

export function openSheet(next: Sheet): void {
  sheet = next;
}

export function closeSheet(): void {
  sheet = null;
}

export function currentSheet(): Sheet | null {
  return sheet;
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
};

let pairInvite = $state<IncomingPairInvite | null>(null);
let pairWait = $state<{ inviteId: string; peerName: string } | null>(null);

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
      toast(`已与「${peerName}」完成配对`);
    } else {
      await api.rejectPairInvite(invite.inviteId);
      toast(`已拒绝「${invite.peerName}」的配对邀请`, "x");
    }
  } catch (e) {
    failed(accept ? "接受配对邀请" : "拒绝配对邀请", e);
  }
}

/** 本机发出去、还在等对方确认的邀请（空 = 没有）。 */
export function pendingPairInvite(): { inviteId: string; peerName: string } | null {
  return pairWait;
}

export function setPendingPairInvite(inviteId: string, peerName: string): void {
  pairWait = { inviteId, peerName };
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
    toast(`已取消对「${pending.peerName}」的配对邀请`, "x");
  } catch (e) {
    failed("取消配对邀请", e);
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
