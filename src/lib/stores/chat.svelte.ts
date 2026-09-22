import { listen } from '@tauri-apps/api/event';
import { formatFileSize } from '$lib/types';
import type { ChatSnapshot, ChatMessage, ChatPeerConvo, LocalPick, PeerDevice, PushBatch, PushInvite } from '$lib/types';
import * as api from '$lib/api';
import { getDevices } from './devices.svelte';

/**
 * 会话（聊天）：后端那份记录在前端的影子 + 会话界面自己的状态。
 *
 * **一份真相在后端**（`chat.json` + `chat://changed` 事件，载荷是整份快照）：这一层只做
 * "拿来就换"（`replace`），不做增量演算 —— 独立演算的第二份状态迟早跟后端对不上，而会话
 * 是人打字的速度，整份换的开销可以忽略（与推送卡片 `push://batch` 同一个取舍）。
 *
 * **会话 = 与一台设备的那一条**（`peerId` 就是身份）。没有群实体：**群发是一次投递动作**
 * —— 给选中的每台各存一份（同一个消息 id），于是"与 B""与 C"两条会话里各出现同一条消息，
 * 每台看到的就是一条普通消息（回复各自回到与发送方的 1:1）。这是本项目网络拓扑
 * （两两配对、没有组播）下唯一诚实的语义，见 `src-tauri/src/chat.rs` 的模块说明。
 *
 * **界面状态**（浮层开没开、详情里开着哪台设备、草稿、群发框、收件人选择框）也放这里：
 * 底栏那颗入口在 App、会话浮层在自己的组件、设备行右键的「发消息」在第三个地方 ——
 * 三处都要能开到同一个会话，状态必须有一个共同的家。
 *
 * **文件消息只活在本机内存里**（`files`）：文件本身有它自己的持久物（落盘的那份文件、
 * 右下角那张推送卡），会话里那条是**这次传输的状态行** —— 后端只存文本。
 */

/** 会话浮层没数据时的占位（只读，别改）。 */
const EMPTY_SNAPSHOT: ChatSnapshot = { convos: [], unread: 0 };

let snapshot = $state<ChatSnapshot>(EMPTY_SNAPSHOT);
/** 会话浮层展开着（底栏那颗「会话」按钮）。 */
let open = $state(false);
/** 详情里开着哪台设备（`null` = 停在会话列表）。 */
let openPeer = $state<string | null>(null);
/** 还没发出去的草稿：按设备存，切来切去不丢字（原型 `chat.draft`）。 */
let draft = $state<Record<string, string>>({});
/** 收件人选择框（「群发」「添加」共用一份）。 */
let recipientReq = $state<{ seed: string[]; onConfirm: (ids: string[]) => void } | null>(null);
/** 群发框：一次投递的**临时载体**（收件人 + 草稿）。发出去就没了 —— 它自己不是会话、
 *  不留历史（历史落在各台设备的会话里），关掉或发完即消失。 */
let batch = $state<{ recipients: string[]; draft: string } | null>(null);
/** 文件消息：设备 id → 那一条（见文件头说明）。 */
let files = $state<Record<string, ChatFileMsg[]>>({});

/**
 * 会话里那条文件消息（**只在本机内存里**）。
 *
 * 发送侧：`batchId` 指向右下角那张推送卡（进度、成败都从它抄过来）；接收侧：`inviteState`
 * 是那条邀请走到哪一步了，`received`/`bytes` 是接收进度。
 */
export interface ChatFileMsg {
  id: string;
  at: number;
  fromMe: boolean;
  name: string;
  /** 文件夹还是文件（气泡/卡片上那颗图标）。 */
  isDir: boolean;
  /** 收到的那条是谁推过来的（`fromMe` 时为空）。 */
  peerId: string;
  /** 显示用的大小串。 */
  size: string;
  /** 发送侧：这批推到几台、几台没成、进度百分比、还在不在传。 */
  targets: number;
  failed: number;
  pct: number;
  sending: boolean;
  /** 发送侧：`push://batch` 那张卡的 id（结算后清掉 —— 卡片 3 秒就没了）。 */
  batchId: string;
  /** 接收侧：那条推送邀请。 */
  inviteId: string;
  inviteState: 'pending' | 'accepted' | 'received' | 'rejected' | 'failed' | '';
  /** 接收侧：已收字节 / 总字节（进度条）。 */
  received: number;
  bytes: number;
}

/** 时间线上的一项：后端存的文本消息，或本机内存里那条文件消息。 */
export type ChatItem =
  | ({ kind: 'text' } & ChatMessage)
  | ({ kind: 'file' } & ChatFileMsg);

function convoOf(peerId: string): ChatPeerConvo | undefined {
  return snapshot.convos.find((c) => c.peerId === peerId);
}

/** 这台设备还在不在设备表里（设备被移除后，它的会话也就不列了）。 */
function known(peerId: string): boolean {
  return getDevices().some((d) => d.id === peerId);
}

// ── 读 ────────────────────────────────────────────────────────────────────

/** 一台设备的未读数（会话列表、底栏角标、页签上那颗点都用它）。 */
export function unreadForPeer(peerId: string): number {
  return convoOf(peerId)?.unread ?? 0;
}

/**
 * 全部未读（底栏那颗角标 / 悬浮球左上角那颗）。
 *
 * **只算还在设备表里的会话**，不直接读后端那份总数：设备被解除配对又离线之后，那个会话
 * 在界面上已经列不出来（列不出来 = 打不开 = 标不了已读），它的未读要还计入，角标就成了
 * 一个永远清不掉的数。
 */
export function unreadTotal(): number {
  return getDevices().reduce((n, d) => n + unreadForPeer(d.id), 0);
}

/** 一台设备的显示名（消息里存的是收到那一刻的名字，改过名的设备以设备表为准）。 */
export function deviceNameOf(peerId: string): string {
  return getDevices().find((d) => d.id === peerId)?.name ?? peerId;
}

/** 头像上的那个字（设备名的首字符；与设备页 `.d-avatar` 同一套算法）。 */
export function peerInitial(peerId: string): string {
  const name = deviceNameOf(peerId);
  return (name.trim().charAt(0) || '?').toUpperCase();
}

/** 状态点：有长连接 = 绿；这台不在网上 = 琥珀（发不出去，要你看一眼）；其余中性。 */
export function peerDotState(peerId: string): 'ok' | 'warn' | '' {
  const dev = getDevices().find((d) => d.id === peerId);
  if (dev?.connected) return 'ok';
  return dev && !dev.online ? 'warn' : '';
}

/** 一个会话的时间线：这台设备那条记录 + 本机那条文件消息，按时间穿插。 */
export function itemsOf(peerId: string): ChatItem[] {
  const texts: ChatItem[] = (convoOf(peerId)?.messages ?? []).map((m) => ({
    kind: 'text' as const,
    ...m,
  }));
  const local: ChatItem[] = (files[peerId] ?? []).map((f) => ({ kind: 'file' as const, ...f }));
  return [...texts, ...local].sort((a, b) => a.at - b.at);
}

/** 会话列表里那行"最近一条"（没有 = `null`）。 */
function lastOf(peerId: string): ChatItem | null {
  const items = itemsOf(peerId);
  return items.length ? items[items.length - 1] : null;
}

/** 最近一条的摘要（原型 `convoSummarize`）：自己发的加「你：」，文件写 `[文件] 名字`。 */
export function summaryOf(peerId: string): string {
  const last = lastOf(peerId);
  if (!last) return '还没有消息';
  const body = last.kind === 'file' ? `[文件] ${last.name}` : last.text;
  return (last.fromMe ? '你：' : '') + body;
}

/** 最近一条的时刻（会话列表右端那颗时间）。 */
export function timeOf(peerId: string): string {
  const last = lastOf(peerId);
  return last ? clock(last.at) : '';
}

/**
 * 会话列表的顺序：**有消息的按最近一条倒序在前，没消息的按设备表顺序排后**
 * （原型：动过的排前面，其余设备补在后面），且不会每次刷新乱跳。
 */
export function convoPeers(): string[] {
  const ids = getDevices().map((d) => d.id);
  const touched = ids.filter((id) => itemsOf(id).length > 0);
  const untouched = ids.filter((id) => itemsOf(id).length === 0);
  touched.sort((a, b) => (lastOf(b)?.at ?? 0) - (lastOf(a)?.at ?? 0));
  return [...touched, ...untouched];
}

/** `HH:MM`（会话里的时刻一律这个形状，原型也是）。 */
export function clock(at: number): string {
  const d = new Date(at);
  return `${String(d.getHours()).padStart(2, '0')}:${String(d.getMinutes()).padStart(2, '0')}`;
}

/** 一条消息的送达状态文案（原型 `chat-meta` 那一行的后半段）。 */
export function deliveryText(item: ChatItem): string {
  if (item.kind !== 'text' || !item.fromMe) return '';
  const deliveries = deliveryOf(item);
  if (deliveries.includes('failed')) return `${deliveries.filter((d) => d === 'failed').length} 台未送达`;
  if (deliveries.length > 1) return deliveries.every((d) => d === 'read') ? '已读' : '已送达';
  return deliveries[0] === 'read' ? '已读' : '已送达';
}

/**
 * 本机发出的一条消息在各台目标上的状态（群发时同一个 id 的几份副本各带各的 `delivery`）。
 *
 * 目标已经不在设备表里（被移除）的那几份就不算了；一份都找不到时按"已送达"兜底
 * （消息确实是本机发出的，只是副本被清掉了）。
 */
function deliveryOf(item: ChatItem & { kind: 'text' }): string[] {
  const out: string[] = [];
  for (const peerId of item.targets) {
    if (!known(peerId)) continue;
    const copy = convoOf(peerId)?.messages.find((m) => m.id === item.id);
    if (copy) out.push(copy.delivery);
  }
  return out.length ? out : ['sent'];
}

/** 设备名串：`「A」「B」`（超过两台按「A」「B」等 N 台收尾，与推送那边同一套）。 */
export function namesLabel(peerIds: string[]): string {
  const names = peerIds.map((id) => `「${deviceNameOf(id)}」`);
  if (names.length <= 2) return names.join('');
  return `${names[0]}${names[1]}等 ${names.length} 台设备`;
}

/** 文件消息那一行的状态文案（原型 `fileStatusText`）。 */
export function fileStatusText(f: ChatFileMsg): string {
  if (f.fromMe) {
    if (f.sending) return `传输中 ${f.pct}%`;
    if (f.targets === 0) return '';
    if (f.targets - f.failed <= 0) return '未送达';
    const done = `已发送至 ${f.targets - f.failed} 台`;
    return f.failed > 0 ? `${done} · ${f.failed} 台未送达` : done;
  }
  if (f.inviteState === 'received') return '已接收 · 已保存到下载目录';
  if (f.inviteState === 'rejected') return '已拒绝接收';
  if (f.inviteState === 'failed') return '接收失败';
  if (f.inviteState === 'accepted') return '接收中';
  return '';
}

// ── 界面状态 ──────────────────────────────────────────────────────────────

export function isChatOpen(): boolean {
  return open;
}

export function setChatOpen(v: boolean) {
  open = v;
}

export function getOpenPeer(): string | null {
  return openPeer;
}

export function draftOf(peerId: string): string {
  return draft[peerId] ?? '';
}

export function setDraftOf(peerId: string, text: string) {
  draft = { ...draft, [peerId]: text };
}

export function getRecipientReq(): { seed: string[]; onConfirm: (ids: string[]) => void } | null {
  return recipientReq;
}

/** 开收件人选择框（「群发」传空种子，群发框里的「添加」传现有收件人）。 */
export function askRecipients(seed: string[], onConfirm: (ids: string[]) => void) {
  recipientReq = { seed, onConfirm };
}

export function closeRecipients() {
  recipientReq = null;
}

export function getBatch(): { recipients: string[]; draft: string } | null {
  return batch;
}

/**
 * 开群发框（设备页那颗「群发」按钮 / 会话列表头的「＋ 群发」）。
 *
 * 它是**临时载体**：收件人在这里选、话在这里打，发出去之后记录落在各台设备的会话里，
 * 框自己关掉、不留一条"群会话"（用户定下的语义：群发就是群发，不是群）。
 */
export function openBatch(recipients: string[]) {
  batch = { recipients: recipients.filter(known), draft: '' };
  openPeer = null;
  open = true;
}

export function closeBatch() {
  batch = null;
}

export function setBatchRecipients(ids: string[]) {
  if (!batch) return;
  batch = { ...batch, recipients: [...new Set(ids)].filter(known) };
}

export function setBatchDraft(text: string) {
  if (batch) batch = { ...batch, draft: text };
}

// ── 动作 ──────────────────────────────────────────────────────────────────

function replace(next: ChatSnapshot) {
  snapshot = next;
  markOpenRead();
}

/**
 * 打开着的那个会话里来了新消息 → 立刻标已读。
 *
 * 只在**详情真开着**（浮层展开 + 打开的是这台设备）时做：面板收起、或停在列表上时，
 * 那几条就该老老实实留在未读里（底栏角标是给那个时候看的）。
 */
function markOpenRead() {
  if (!open || !openPeer) return;
  if (unreadForPeer(openPeer) > 0) void api.chatMarkRead([openPeer]).then(replace).catch(() => {});
}

/** 打开与一台设备的会话（群发框顺手收掉：它们是同一块浮层里的两个视图）。 */
export function openConversation(peerId: string) {
  if (!known(peerId)) return;
  openPeer = peerId;
  batch = null;
  open = true;
  markOpenRead();
}

/** 回到会话列表（草稿留着）。 */
export function backToList() {
  openPeer = null;
  batch = null;
}

/** 发一条文本消息给这台设备。 */
export async function sendText(peerId: string, text: string): Promise<boolean> {
  const body = text.trim();
  if (!body || !known(peerId)) return false;
  const { snapshot: next } = await api.chatSend([peerId], body);
  replace(next);
  return true;
}

/**
 * 群发框里的那条发出去：给每台收件人各存一份（同一个消息 id），发完框自己关掉。
 *
 * 返回逐台的成败（id 从后端拿，不靠"猜哪条是刚发的"），调用方据此给一句话；
 * 框里的历史**不存在** —— 记录就在各台设备的会话里。
 */
export async function sendBatch(): Promise<{ sent: string[]; failed: string[] } | null> {
  const cur = batch;
  if (!cur) return null;
  const text = cur.draft.trim();
  const peers = cur.recipients.filter(known);
  if (!text || !peers.length) return null;
  const { id, snapshot: next } = await api.chatSend(peers, text);
  replace(next);
  const sent: string[] = [];
  const failed: string[] = [];
  for (const peerId of peers) {
    const copy = next.convos.find((c) => c.peerId === peerId)?.messages.find((m) => m.id === id);
    (copy?.delivery === 'failed' ? failed : sent).push(peerId);
  }
  batch = null;
  return { sent, failed };
}

/** 清空与一台设备的聊天记录。 */
export async function clearPeer(peerId: string): Promise<void> {
  replace(await api.chatClear(peerId));
  // 后端那份清掉了，本机这条文件消息也一起走 —— 只清一半的话，会话列表的"最近一条"
  // 还会挂着一条已经不存在的记录。
  dropLocalFiles(peerId);
}

/** 丢掉一台设备的本机文件消息（清空聊天记录时用）。 */
function dropLocalFiles(peerId: string) {
  if (!(peerId in files)) return;
  const { [peerId]: _drop, ...rest } = files;
  files = rest;
}

/** 面板打开时拉一次（浮层每次展开都拉：收起期间可能收到过消息）。 */
export async function refresh(): Promise<void> {
  try {
    replace(await api.chatSnapshot());
  } catch (e) {
    console.error('读取会话失败:', e);
  }
}

// ── 文件消息（发送侧 / 接收侧）────────────────────────────────────────────

function patchFiles(peerId: string, fn: (list: ChatFileMsg[]) => ChatFileMsg[]) {
  files = { ...files, [peerId]: fn(files[peerId] ?? []) };
}

/**
 * 本机选了文件发出去：**每台目标各一条气泡**（群发过去的那份在每台设备的会话里各算一条，
 * 与文本消息同一个道理），都挂在同一个 `batchId` 上。
 */
export function noteOutgoingFiles(peerIds: string[], items: LocalPick[], batchId: string) {
  const at = Date.now();
  for (const peerId of peerIds) {
    if (!known(peerId)) continue;
    const added: ChatFileMsg[] = items.map((it, i) => ({
      id: `file:${batchId}:${i}`,
      at: at + i,
      fromMe: true,
      name: it.name,
      isDir: it.isDir,
      peerId: '',
      size: it.isDir ? '文件夹' : formatFileSize(it.size),
      targets: peerIds.length,
      failed: 0,
      pct: 0,
      sending: true,
      batchId,
      inviteId: '',
      inviteState: '',
      received: 0,
      bytes: 0,
    }));
    patchFiles(peerId, (list) => [...list, ...added]);
  }
}

/** 推送批次快照 → 那一批文件消息的进度与成败（右下角那张卡是唯一的数据来源）。 */
export function noteBatch(b: PushBatch) {
  let hit = false;
  const next: Record<string, ChatFileMsg[]> = {};
  for (const [peerId, list] of Object.entries(files)) {
    next[peerId] = list.map((f) => {
      if (f.batchId !== b.batchId) return f;
      hit = true;
      const sending = b.waiting > 0;
      const pct = b.total > 0 ? Math.min(100, Math.round((b.received / b.total) * 100)) : 0;
      return {
        ...f,
        targets: b.targets.length,
        failed: b.failed,
        pct: sending ? pct : 100,
        sending,
        // 结算后就把卡片 id 摘掉：卡片 3 秒后从界面上消失，气泡不能跟着变成空数据。
        batchId: sending ? f.batchId : '',
      };
    });
  }
  if (hit) files = next;
}

/** 收到一条推送邀请：会话里落下那张「接收 / 拒绝」卡（落在与推送方的那条会话里）。 */
export function noteInboundInvite(inv: PushInvite) {
  const peerId = inv.sender.id;
  const msg: ChatFileMsg = {
    id: `invite:${inv.inviteId}`,
    at: Date.now(),
    fromMe: false,
    name: inv.name,
    isDir: inv.isDir,
    peerId,
    size: inv.size > 0 ? formatFileSize(inv.size) : '文件夹',
    targets: 0,
    failed: 0,
    pct: 0,
    sending: false,
    batchId: '',
    inviteId: inv.inviteId,
    // 已配对设备的推送后端直接收下了（`auto`）：卡片直接是"接收中"。
    inviteState: inv.auto ? 'accepted' : 'pending',
    received: 0,
    bytes: inv.size,
  };
  patchFiles(peerId, (list) => (list.some((f) => f.inviteId === inv.inviteId) ? list : [...list, msg]));
}

/** 那条邀请被收下了（本机点「接收」，或后端自动收下）。 */
export function noteInviteAccepted(inviteId: string) {
  patchInvite(inviteId, (f) => (f.inviteState === 'pending' ? { ...f, inviteState: 'accepted' } : f));
}

/** 接收进度（`push://progress`）。 */
export function noteInviteProgress(inviteId: string, received: number, total: number) {
  patchInvite(inviteId, (f) => ({
    ...f,
    received,
    bytes: total || f.bytes,
    inviteState: f.inviteState === 'pending' ? 'accepted' : f.inviteState,
  }));
}

/** 那条邀请的终局：已接收 / 已拒绝 / 接收失败。 */
export function noteInviteDone(inviteId: string, outcome: 'received' | 'rejected' | 'failed') {
  patchInvite(inviteId, (f) => ({ ...f, inviteState: outcome, sending: false }));
}

function patchInvite(inviteId: string, fn: (f: ChatFileMsg) => ChatFileMsg) {
  let hit = false;
  const next: Record<string, ChatFileMsg[]> = {};
  for (const [peerId, list] of Object.entries(files)) {
    next[peerId] = list.map((f) => {
      if (f.inviteId !== inviteId) return f;
      hit = true;
      return fn(f);
    });
  }
  if (hit) files = next;
}

// 后端每改一次会话就推一份整快照过来（与推送卡片同一条路数）。
const initListeners: Promise<void> = (async () => {
  await listen<ChatSnapshot>('chat://changed', (ev) => {
    replace(ev.payload);
  });
  // 启动先拉一次：上次关掉应用之后收到的消息，未读角标得在**还没打开浮层时**就是对的
  // （浮层展开时那次 `refresh` 只补得了"收起期间"的那一段）。
  await refresh();
})();

export function cleanup() {
  void initListeners;
}