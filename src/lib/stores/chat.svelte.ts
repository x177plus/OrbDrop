import { listen } from '@tauri-apps/api/event';
import { formatFileSize } from '$lib/types';
import type { ChatDelivery, ChatGroup, ChatGroupMsg, ChatSnapshot, ChatMessage, ChatPeerConvo, ConvoMode, LocalPick, PendingGroup, PushBatch, PushInvite, PushTargetState } from '$lib/types';
import * as api from '$lib/api';
import { getDevices, deviceDotState, threadDotState, type DeviceDotState } from './devices.svelte';

/**
 * 会话（聊天）：后端那份记录在前端的影子 + 会话界面自己的状态。
 *
 * **一份真相在后端**（`chat.json` + `chat://changed` 事件，载荷是整份快照）：这一层只做
 * "拿来就换"（`replace`），不做增量演算 —— 独立演算的第二份状态迟早跟后端对不上，而会话
 * 是人打字的速度，整份换的开销可以忽略（与推送卡片 `push://batch` 同一个取舍）。
 *
 * **三种会话（原型的三张身份）**，用一个**会话键**统一寻址：
 * - `dev:<设备 id>`：一对一。记录 = 后端那条 `ChatPeerConvo`。
 * - `bc:<名单 id>`：**单向群发**。记录 = 后端那份**名单**（`ChatBatch`）+ 各台会话里带着
 *   它 `batchId` 的副本；群发那条视图只显示"我发出去的"，逐台战果由副本的 `delivery` 拼出来。
 *   对端收到的永远是一条普通消息（回复各自回到与发送方的 1:1）—— 见 `src-tauri/src/chat.rs`。
 * - `grp:<群 id>`：**共享群聊**。记录 = 后端那个群（`ChatGroup`：名册、消息、已读水位都在它
 *   身上）。群消息**不镜像进各台的 1:1**（与群发相反），界面上只出现在这一条视图里；名册的
 *   视角**不含本机**（后端已滤掉），所以这一层根本不需要知道本机设备 id。收到邀请还没点
 *   「加入」的群不算群 —— 它只在 `snapshot.pendingGroups` 里，界面渲染成会话列表那张邀请卡。
 *
 * **界面状态**（浮层开没开、详情里开着哪个会话、草稿、收件人选择框、接收方视角预览）也放
 * 这里：底栏那颗入口在 App、会话浮层在自己的组件、设备行右键的「发消息」在第三个地方 ——
 * 三处都要能开到同一个会话，状态必须有一个共同的家。
 *
 * **文件消息只活在本机内存里**（`files`）：文件本身有它自己的持久物（落盘的那份文件、
 * 右下角那张推送卡），会话里那条是**这次传输的状态行** —— 后端只存文本。群里发文件也没有
 * 新的后端命令：那就是"拿群成员当收件人跑一次多目标推送"，本机这条记录挂上 `groupId` 就能
 * 归到群那条视图里（见 `noteOutgoingFiles`）。
 */

/** 会话浮层没数据时的占位（只读，别改）。 */
const EMPTY_SNAPSHOT: ChatSnapshot = { convos: [], batches: [], groups: [], pendingGroups: [], unread: 0 };

let snapshot = $state<ChatSnapshot>(EMPTY_SNAPSHOT);
/** 会话浮层展开着（底栏那颗「会话」按钮）。 */
let open = $state(false);
/** 详情里开着哪个会话（`null` = 停在会话列表）。键见 `deviceKey` / `refOf`。 */
let openKey = $state<string | null>(null);
/** 还没发出去的草稿：**按会话**存，切来切去不丢字（原型 `chat.draft[key]`）。 */
let draft = $state<Record<string, string>>({});
/** 收件人选择框（「新建会话」「编辑参与者」共用一份）。 */
let recipientReq = $state<RecipientReq | null>(null);
/** 接收方视角预览（群发那条记录上的「查看收件效果」）。 */
let peek = $state<{ key: string; itemId: string; index: number } | null>(null);
/** 文件消息：设备 id → 那几条（见文件头说明）。 */
let files = $state<Record<string, ChatFileMsg[]>>({});
/**
 * 正在补拉的群 id（见 `syncGroup`）：同一条会话被连着开两次（连点两下、或刚建完群紧接着
 * 打开）时，补拉只飞一次 —— 一次补拉回来的是整份快照，重复飞几遍只是白花几个来回。
 */
let syncingGroup: string | null = null;

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
  /** 发送侧：这条文件消息属于哪份**群发名单**（空 = 不是群发那条发的）；群发那条视图按它筛。 */
  chatBatchId: string;
  /** 发送侧：这条文件消息属于哪个**群**（空 = 不是发到群里的）；群那条视图按它筛。 */
  groupId: string;
  /** 发送侧：逐台的推送状态（等待同意 / 传输中 / 已送达 / 未送达…），群发视图按它逐台写。 */
  perTargets: { peerId: string; state: PushTargetState }[];
  /** 接收侧：那条推送邀请。 */
  inviteId: string;
  inviteState: 'pending' | 'accepted' | 'received' | 'rejected' | 'failed' | '';
  /** 接收侧：已收字节 / 总字节（进度条）。 */
  received: number;
  bytes: number;
}

/**
 * 时间线上的一项：后端存的文本消息、群里的一条消息，或本机内存里那条文件消息。
 *
 * 群消息与 1:1 的一条文本在界面上长得一样（都走"文本气泡"这一档），但数据形状不同：
 * 它带的是名册版的 `delivery`（成员 id → 状态）与 `readCount`，没有 `targets`
 * —— 所以两种文本各是一个成员，取共同字段用 `kind === 'text'`，要 `targets` 的地方
 * 得再收一道（见 `perDeviceOf` / `deliveryText`）。
 */
export type ChatItem =
  | ({ kind: 'text' } & ChatMessage)
  | ({ kind: 'text' } & ChatGroupMsg)
  | ({ kind: 'file' } & ChatFileMsg);

function convoOf(peerId: string): ChatPeerConvo | undefined {
  return snapshot.convos.find((c) => c.peerId === peerId);
}

/** 这台设备还在不在设备表里（设备被移除后，它的会话也就不列了）。 */
function known(peerId: string): boolean {
  return getDevices().some((d) => d.id === peerId);
}

// ── 会话键 ────────────────────────────────────────────────────────────────

/** 一对一那条会话的键。 */
export function deviceKey(peerId: string): string {
  return `dev:${peerId}`;
}

function batchKey(id: string): string {
  return `bc:${id}`;
}

function groupKey(id: string): string {
  return `grp:${id}`;
}

/** 键 → `{ 模式, id }`（认不出来的键 = `null`，界面上什么都不画）。 */
function refOf(key: string | null): { mode: ConvoMode; id: string } | null {
  if (!key) return null;
  if (key.startsWith('dev:')) return { mode: 'direct', id: key.slice(4) };
  if (key.startsWith('bc:')) return { mode: 'broadcast', id: key.slice(3) };
  if (key.startsWith('grp:')) return { mode: 'group', id: key.slice(4) };
  return null;
}

/** 详情里开着的那条会话（键对应的那份完整视图），键失效时是 `null`。 */
export interface ConvoThread {
  key: string;
  mode: ConvoMode;
  /** 名单 id（一对一为空串）。 */
  id: string;
  /** 群发/群聊的名字（一对一为空串）。 */
  name: string;
  /** 参与者（一对一 = 那一台；顺序按名单）。 */
  participants: string[];
  /** 标题：`群发 · 名字` / `群聊 · 名字` / 设备名。 */
  title: string;
}

function titleOf(mode: ConvoMode, name: string, participants: string[]): string {
  if (mode === 'broadcast') return `群发 · ${name.trim() || '未命名群发'}`;
  if (mode === 'group') return name.trim() ? `群聊 · ${name.trim()}` : participants.map(deviceNameOf).join('、') || '群聊';
  return deviceNameOf(participants[0] ?? '');
}

/** 本机已知的那个群（找不到 = `undefined`：退出了、或这条键本来就过期了）。 */
export function groupOf(id: string): ChatGroup | undefined {
  return snapshot.groups.find((g) => g.id === id);
}

/** 收到但还没确认的群邀请（会话列表那张卡就画它；用户没点「加入」就不算群）。 */
export function pendingGroups(): PendingGroup[] {
  return snapshot.pendingGroups;
}

/**
 * 键 → 会话视图。三种模式各自的"还算不算数"：
 *
 * - 一对一：那台设备还在设备表里；
 * - 群发：名单还在，且**有两台以上**成员还在设备表里（少于两台就不再是"多台记录"了）；
 * - 群聊：后端那份群还在（退出 / 被解散后它就从快照里消失了）。**名册就是唯一依据**：
 *   成员列出来就是成员，不看它在不在设备表里（群是后端的一份实体，不随本机的设备表缩水）。
 *
 * 一对一与群发的消息/名单都是后端的（`snapshot`），群聊的也是（`snapshot.groups`）——
 * 这个函数是三者唯一的汇合处，界面别自己去 `snapshot.batches` / `snapshot.groups` 里翻。
 */
function threadOf(key: string | null): ConvoThread | null {
  const ref = refOf(key);
  if (!ref) return null;
  if (ref.mode === 'direct') {
    if (!known(ref.id)) return null;
    return { key: deviceKey(ref.id), mode: 'direct', id: '', name: '', participants: [ref.id], title: deviceNameOf(ref.id) };
  }
  if (ref.mode === 'broadcast') {
    const b = snapshot.batches.find((x) => x.id === ref.id);
    if (!b) return null;
    const participants = b.members.filter(known);
    if (participants.length < 2) return null;
    return { key: batchKey(b.id), mode: 'broadcast', id: b.id, name: b.name, participants, title: titleOf('broadcast', b.name, participants) };
  }
  const g = groupOf(ref.id);
  if (!g) return null;
  const participants = g.members.map((m) => m.id);
  return { key: groupKey(g.id), mode: 'group', id: g.id, name: g.name, participants, title: titleOf('group', g.name, participants) };
}

/** 详情里开着的那条会话（底下那份键失效时 `null`：界面回到列表）。 */
export function activeThread(): ConvoThread | null {
  return threadOf(openKey);
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
 * 一个永远清不掉的数。多台会话不产生未读（群发是单向的）。群聊的未读是**后端算好的**
 * （本机之外、排在已读水位之后的条数）—— 这里只把还在快照里的那些群加一遍，不重算。
 */
export function unreadTotal(): number {
  const peers = getDevices().reduce((n, d) => n + unreadForPeer(d.id), 0);
  const groups = snapshot.groups.reduce((n, g) => n + g.unread, 0);
  return peers + groups;
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

/**
 * 一条群发记录里的那些副本（**扫所有会话**，按消息 id 去重）。
 *
 * 不按"当前成员"去找：成员是可以被加减的，而老消息的副本躺在**它当时那些目标**的会话里
 * （甚至可能是一台已经被移出名单的设备）——按成员找会凭空少几条。
 */
function threadTexts(batchId: string): ChatItem[] {
  const byId = new Map<string, ChatMessage>();
  for (const convo of snapshot.convos) {
    for (const m of convo.messages) {
      if (m.fromMe && m.batchId === batchId && !byId.has(m.id)) byId.set(m.id, m);
    }
  }
  return [...byId.values()].map((m) => ({ kind: 'text' as const, ...m }));
}

/**
 * 群发那条视图里的记录：文本取副本、文件取本机那条 —— 都只留**我发出去的**。
 *
 * 文件消息（只在本机内存里）按 id 去重：群发给两台时，同一条文件记录在两台各自的列表里
 * 各有一份（id 相同）—— 不去重就是同一个 key 出现两次，Svelte 直接抛 `each_key_duplicate`，
 * 整块视图画不出来（表现就是"群发里发了文件，记录什么都没有"）。
 */
function threadItems(batchId: string): ChatItem[] {
  const texts = threadTexts(batchId);
  const local = new Map<string, ChatItem>();
  for (const f of Object.values(files).flat()) {
    if (!f.fromMe || f.chatBatchId !== batchId || local.has(f.id)) continue;
    local.set(f.id, { kind: 'file' as const, ...f });
  }
  return [...texts, ...local.values()].sort((a, b) => a.at - b.at);
}

/**
 * 时间线排序：按 `at`；**同一毫秒**里用群消息的 `seq`（同一个发送方说话的先后）稳定
 * tiebreak —— 落盘顺序靠它才排得对，不然同一毫秒的几条会跟着遍历顺序飘。
 */
function byTime(a: ChatItem, b: ChatItem): number {
  if (a.at !== b.at) return a.at - b.at;
  const sa = a.kind === 'text' && 'seq' in a ? a.seq : 0;
  const sb = b.kind === 'text' && 'seq' in b ? b.seq : 0;
  return sa - sb;
}

/** 一个会话的时间线：一对一 = 那台的记录 + 本机文件消息；群发 = 我发出去的那些；群聊 = 群消息 + 发到群里的文件。 */
export function itemsOf(key: string): ChatItem[] {
  const ref = refOf(key);
  if (!ref) return [];
  if (ref.mode === 'direct') {
    const texts: ChatItem[] = (convoOf(ref.id)?.messages ?? []).map((m) => ({ kind: 'text' as const, ...m }));
    const local: ChatItem[] = (files[ref.id] ?? []).map((f) => ({ kind: 'file' as const, ...f }));
    return [...texts, ...local].sort((a, b) => a.at - b.at);
  }
  if (ref.mode === 'broadcast') return threadItems(ref.id);
  const g = groupOf(ref.id);
  if (!g) return [];
  const texts: ChatItem[] = g.messages.map((m) => ({ kind: 'text' as const, ...m }));
  // 本机发到群里的文件：文件消息按**设备**存（本机内存），一次多目标推送会在每台成员那儿
  // 各落一条 id 相同的记录 —— 按 id 去重，不然同一个 key 出现两次，Svelte 直接抛
  // `each_key_duplicate`，整块视图画不出来。
  const seen = new Set<string>();
  const local: ChatItem[] = [];
  for (const f of Object.values(files).flat()) {
    if (f.groupId !== ref.id || seen.has(f.id)) continue;
    seen.add(f.id);
    local.push({ kind: 'file' as const, ...f });
  }
  return [...texts, ...local].sort(byTime);
}

/** 会话里某一条（刚发出去的那条要拿它读逐台战果）。 */
export function itemById(key: string, id: string): ChatItem | null {
  return itemsOf(key).find((i) => i.id === id) ?? null;
}

/** 一对一那行"最近一条"（没有 = `null`）。 */
function lastOf(peerId: string): ChatItem | null {
  const items = itemsOf(deviceKey(peerId));
  return items.length ? items[items.length - 1] : null;
}

/** 最近一条的摘要（原型 `convoSummarize`）：自己发的加「你：」，文件写 `[文件] 名字`。 */
function summaryOf(peerId: string): string {
  const last = lastOf(peerId);
  if (!last) return '还没有消息';
  const body = last.kind === 'file' ? `[文件] ${last.name}` : last.text;
  return (last.fromMe ? '你：' : '') + body;
}

/** 最近一条的时刻（会话列表右端那颗时间）。 */
function timeOf(peerId: string): string {
  const last = lastOf(peerId);
  return last ? clock(last.at) : '';
}

/** 群发那行的预览：先说送到了几台，再说内容（原型 `convoSummarize` 的群发那一档）。 */
function threadSummary(items: ChatItem[]): string {
  const last = items.length ? items[items.length - 1] : null;
  if (!last) return '还没有消息';
  const r = receiptOf(last);
  const body = last.kind === 'file' ? `[文件] ${last.name}` : last.text;
  return (r ? `已送达 ${r.delivered}/${r.total} 台 · ` : '') + body;
}

/**
 * 会话列表里的一行：一对一 / 群发 / 群聊三种身份混排。
 *
 * 动过的按最近一条倒序在前，没动过的排在后面（设备按设备表顺序、名单按各自的顺序），
 * 且不会每次刷新乱跳。
 */
export type ConvoRow = {
  /** 会话键（点它开哪条会话）。 */
  key: string;
  mode: ConvoMode;
  /** 最近一条的时刻（0 = 还没消息）。 */
  at: number;
  label: string;
  sub: string;
  time: string;
  unread: number;
  /** 状态点。 */
  /** 状态点：`stores/devices` 那五档（颜色 = 可达性、形状 = 链路）。 */
  dot: DeviceDotState;
  /** 一对一那行头像上的字（群发/群聊画图标，用不到）。 */
  initial: string;
};

export function convoRows(): ConvoRow[] {
  const rows: ConvoRow[] = getDevices().map((d) => {
    const last = lastOf(d.id);
    return {
      key: deviceKey(d.id),
      mode: 'direct' as const,
      at: last?.at ?? 0,
      label: deviceNameOf(d.id),
      sub: summaryOf(d.id),
      time: timeOf(d.id),
      unread: unreadForPeer(d.id),
      dot: deviceDotState(d),
      initial: peerInitial(d.id),
    };
  });
  for (const b of snapshot.batches) {
    const t = threadOf(batchKey(b.id));
    if (!t) continue;
    const items = threadItems(b.id);
    const last = items.length ? items[items.length - 1] : null;
    rows.push({
      key: t.key,
      mode: 'broadcast',
      at: last?.at ?? 0,
      label: t.title,
      sub: threadSummary(items),
      time: last ? clock(last.at) : '',
      unread: 0, // 群发那条只放"我发出去的"：未读在各自的 1:1 里
      dot: threadDotState(t.participants),
      initial: '',
    });
  }
  for (const g of snapshot.groups) {
    const key = groupKey(g.id);
    const t = threadOf(key);
    if (!t) continue;
    // 预览取这条视图的**时间线**（群消息 + 本机发到群里的文件），不是光看 `g.messages`：
    // 群里刚传完一个文件时，列表里的"最近一条"就是那条文件。写法与一对一同款。
    const items = itemsOf(key);
    const last = items.length ? items[items.length - 1] : null;
    const body = !last ? '还没有消息' : last.kind === 'file' ? `[文件] ${last.name}` : last.text;
    rows.push({
      key,
      mode: 'group',
      at: last?.at ?? 0,
      label: t.title,
      sub: (last?.fromMe ? '你：' : '') + body,
      time: last ? clock(last.at) : '',
      unread: g.unread,
      dot: threadDotState(t.participants),
      initial: '',
    });
  }
  const touched = rows.filter((r) => r.at > 0).sort((a, b) => b.at - a.at);
  const untouched = rows.filter((r) => r.at === 0);
  return [...touched, ...untouched];
}

// ── 逐台状态（群发那条视图与"接收方视角"预览共用）──────────────────────

/** 一台目标上的状态：`ok` = 到了、`pending` = 还在路上、`waiting` = 排队等对端上线、`off` = 没成。 */
export type PerDeviceState = 'ok' | 'pending' | 'waiting' | 'off';

export interface PerDevice {
  peerId: string;
  state: PerDeviceState;
  /** 状态那几个字（`已读` / `已送达` / `发送中` / `等待发送` / `未送达`…）。 */
  label: string;
}

function stateOfDelivery(d: ChatDelivery): { state: PerDeviceState; label: string } {
  if (d === 'read') return { state: 'ok', label: '已读' };
  if (d === 'sent') return { state: 'ok', label: '已送达' };
  // 对方从**别的成员**那里补到了这条（补拉 / 跨段转发）：内容与直投同一份，路径不同 ——
  // 归"到位"那一档（与 `sent`/`read` 同档），但文案要说清它是**绕路到的**，别冒充直投。
  if (d === 'synced') return { state: 'ok', label: '已同步' };
  if (d === 'sending') return { state: 'pending', label: '发送中' };
  // 排队等对端上线：**不是"没成"**（它一上线就会发出去），所以不能落进 `off` 那一档
  // （那档在界面上读作"未送达"）。
  if (d === 'queued') return { state: 'waiting', label: '等待发送' };
  return { state: 'off', label: d === 'failed' ? '未送达' : '已收到' };
}

function stateOfPush(s: PushTargetState): { state: PerDeviceState; label: string } {
  if (s === 'done') return { state: 'ok', label: '已送达' };
  if (s === 'invited' || s === 'sending') return { state: 'pending', label: pushStateLabel(s) };
  return { state: 'off', label: pushStateLabel(s) };
}

/**
 * 一条消息**逐台**的状态（`{ 设备, 状态 }`），群发那条视图就按它写"谁已读、谁没送到"。
 *
 * 按消息自己的 `targets` 走（不是名单的当前成员）：这条当时发给了谁，就只说谁。
 * 已经不在设备表里的那几台不列（界面上它们哪儿都不出现），一台都不剩时返回空。
 */
export function perDeviceOf(item: ChatItem): PerDevice[] {
  const out: PerDevice[] = [];
  if (item.kind === 'text') {
    // 群消息没有"逐台副本"可读：它的收件人写在 `delivery` 上（见 `groupReceiptText`）。
    if (!('targets' in item)) return out;
    for (const peerId of item.targets) {
      if (!known(peerId)) continue;
      const copy = convoOf(peerId)?.messages.find((m) => m.id === item.id);
      if (copy) out.push({ peerId, ...stateOfDelivery(copy.delivery) });
    }
    return out;
  }
  for (const t of item.perTargets) {
    if (!known(t.peerId)) continue;
    out.push({ peerId: t.peerId, ...stateOfPush(t.state) });
  }
  return out;
}

/**
 * 本机发出的**群消息**气泡下那句逐台状态（群那条视图用）。
 *
 * 群里没有"各台各一份副本"那套账（群是一条共享消息流），所以不摆逐台回执行，一行小字就够：
 * 读过这条的有几台（`readCount`，后端算的）配这条当时发给的台数（`delivery` 的键数），
 * 再补上还在路上的那几档 —— 没人读过时**不能只写「已读 0/4」**，得说清剩下 4 台在等什么。
 */
export function groupReceiptText(item: ChatItem): string {
  if (!item.fromMe || item.kind !== 'text' || 'targets' in item) return '';
  const states = Object.values(item.delivery);
  if (!states.length) return '';
  const parts = [`已读 ${item.readCount}/${states.length}`];
  const waiting = states.filter((s) => s === 'queued').length;
  const sending = states.filter((s) => s === 'sending').length;
  const failed = states.filter((s) => s === 'failed').length;
  // 从别的成员那里补到的那些：也是到了，但走的不是本机这一跳 —— 单说一句，别混进"已读"
  // （它只是拿到了，不等于读过；后端也是等它真读了才把 `readCount` 加上去）。
  const synced = states.filter((s) => s === 'synced').length;
  if (waiting) parts.push(`${waiting} 台等待发送`);
  if (sending) parts.push(`${sending} 台发送中`);
  if (synced) parts.push(`${synced} 台已同步（经其他成员补到）`);
  if (failed) parts.push(`${failed} 台未送达`);
  return parts.join(' · ');
}

/**
 * 发言人的语义色（0|1|2）：**按设备 id 哈希**，不按名字 —— 名字是可改的，改一次名颜色就跳，
 * 同一个人的两句话会认成两个人。组件用它取 `.sp-0` / `.sp-1` / `.sp-2`。
 */
export function speakerIndex(peerId: string): 0 | 1 | 2 {
  let h = 0;
  for (let i = 0; i < peerId.length; i++) h = (h * 31 + peerId.charCodeAt(i)) % 997;
  return (h % 3) as 0 | 1 | 2;
}

export interface Receipt {
  /** 已经到位的台数（已送达 / 已读）。 */
  delivered: number;
  /** 当时发给了几台（还在设备表里的那些）。 */
  total: number;
  /** 没送到的台数（对端明确拒绝）。 */
  offline: number;
  /** 排队等对端上线的台数（消息没丢，它一上线就发出去）。 */
  waiting: number;
}

/** 一条群发记录此刻的逐台战果（回执行那行小字、发出去那句话都用它）。 */
export function receiptOf(item: ChatItem): Receipt | null {
  const per = perDeviceOf(item);
  if (!per.length) return null;
  return {
    delivered: per.filter((p) => p.state === 'ok').length,
    total: per.length,
    offline: per.filter((p) => p.state === 'off').length,
    waiting: per.filter((p) => p.state === 'waiting').length,
  };
}

/** 推送那边的一台状态 → 逐台那行小字（文件没有"已读"，用的是推送自己那套状态）。 */
function pushStateLabel(state: PushTargetState): string {
  if (state === 'done') return '已送达';
  if (state === 'failed') return '未送达';
  if (state === 'rejected') return '已拒绝';
  if (state === 'cancelled') return '已取消';
  if (state === 'sending') return '传输中';
  return '等待同意';
}

/** `HH:MM`（会话里的时刻一律这个形状，原型也是）。 */
export function clock(at: number): string {
  const d = new Date(at);
  return `${String(d.getHours()).padStart(2, '0')}:${String(d.getMinutes()).padStart(2, '0')}`;
}

/**
 * 这条消息在**某台设备**上的状态文案（一对一那条视图就写它，原型 `chat-meta` 的后半段）。
 *
 * 一对一那条会话里也可能躺着群发过去的副本（群发是"每台各存一份"），但那一屏说的是"与这台
 * 设备的往来"——只写**这台自己的**状态；"几台送到了 / 几台没送到"是群发那条视图的事
 * （那里有逐台回执，见 `perDeviceOf`）。
 *
 * 副本被清掉（记录清空）或目标不在设备表里时返回空串：宁可不写，也不猜一个状态。
 */
export function deliveryText(peerId: string, item: ChatItem): string {
  // 群消息不走这条路：它的状态在自己的 `delivery` 上（见 `groupReceiptText`），
  // 与"某台设备上的副本"是两码事。
  if (!item.fromMe || item.kind !== 'text' || !('targets' in item)) return '';
  const copy = convoOf(peerId)?.messages.find((m) => m.id === item.id);
  return copy ? stateOfDelivery(copy.delivery).label : '';
}

/**
 * 文件消息那一行的状态文案（原型 `fileStatusText`）。
 *
 * `peerId` 非空 = 这条记录正躺在**一对一**那条会话里：只写**这台设备自己的**状态
 * （等待同意 / 传输中 / 已送达 / 未送达）—— 群发是"每台各存一份"，同一份清单在每台的会话里
 * 都有一条，一对一那一屏不该出现"发到了几台"这种总账。为空 = 群发那条视图（那里逐台状态
 * 另有一排回执，气泡里这句是整批的进度与成败）。
 */
export function fileStatusText(f: ChatFileMsg, peerId = ''): string {
  if (f.fromMe) {
    if (peerId) {
      const target = f.perTargets.find((t) => t.peerId === peerId);
      return target ? pushStateLabel(target.state) : '';
    }
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

export function draftOf(key: string): string {
  return draft[key] ?? '';
}

export function setDraftOf(key: string, text: string) {
  draft = { ...draft, [key]: text };
}

/** 收件人选择框的那份请求：种子、锁定的模式、预填名字、确认回调。 */
export interface RecipientReq {
  /** 已经选中的设备（设备表里没有的那些不算数）。 */
  seed: string[];
  /** 编辑已有会话时锁定的模式（`null` = 让人选）；锁定时模式那一组只读。 */
  lock: ConvoMode | null;
  /** 名字输入框的初值。 */
  name: string;
  onConfirm: (ids: string[], mode: ConvoMode, name: string) => void;
}

export function getRecipientReq(): RecipientReq | null {
  return recipientReq;
}

export function closeRecipients() {
  recipientReq = null;
}

function askRecipients(req: { seed?: string[]; lock?: ConvoMode | null; name?: string }, onConfirm: RecipientReq['onConfirm']) {
  recipientReq = {
    seed: (req.seed ?? []).filter(known),
    lock: req.lock ?? null,
    name: req.name ?? '',
    onConfirm,
  };
}

/** 新建会话（会话列表头的「＋ 新建会话」/ 设备页那颗）：一台 = 一对一，多台 = 按选的模式建。 */
export function askNewConvo() {
  askRecipients({}, (ids, mode, name) => void createThread(ids, mode, name));
}

/**
 * 收件人那一栏的「添加」（⋮ 菜单与模式条各有一颗走同一条路）。
 *
 * 一对一是"**再加几台**"：选完两台以上才成为一个多台会话（模式与名字当场选）；多台会话是
 * "编辑这份名单"（模式锁定 —— 换了模式，历史就成了两半）。
 */
export function askParticipants(key: string) {
  const t = threadOf(key);
  if (!t) return;
  if (t.mode === 'direct') {
    askRecipients({ seed: t.participants }, (ids, mode, name) => void createThread(ids, mode, name));
    return;
  }
  askRecipients({ seed: t.participants, lock: t.mode, name: t.name }, (ids, _mode, name) => void editThread(key, ids, name));
}

// ── 接收方视角预览（群发那条记录上的「查看收件效果」）──────────────────────

export function getPeek(): { key: string; itemId: string; index: number } | null {
  return peek;
}

export function openPeek(key: string, itemId: string) {
  peek = { key, itemId, index: 0 };
}

export function closePeek() {
  peek = null;
}

/** 预览框里那一屏的数据：这一条对某台设备来说长什么样（对方那边的样子）。 */
export interface PeekView {
  index: number;
  total: number;
  deviceName: string;
  initial: string;
  body: { kind: 'text'; text: string } | { kind: 'file'; name: string; size: string };
  time: string;
  state: PerDeviceState;
  stateLabel: string;
}

export function peekView(): PeekView | null {
  const cur = peek;
  if (!cur) return null;
  const item = itemById(cur.key, cur.itemId);
  if (!item || !item.fromMe) return null;
  const per = perDeviceOf(item);
  if (!per.length) return null;
  const index = cur.index % per.length;
  const target = per[index];
  return {
    index,
    total: per.length,
    deviceName: deviceNameOf(target.peerId),
    initial: peerInitial(target.peerId),
    body: item.kind === 'text'
      ? { kind: 'text', text: item.text }
      : { kind: 'file', name: item.name, size: item.size },
    time: clock(item.at),
    state: target.state,
    stateLabel: target.label,
  };
}

/** 换下一台（一台时什么都不做 —— 界面上那颗按钮也会是禁用的）。 */
export function peekNext() {
  const view = peekView();
  if (!peek || !view || view.total < 2) return;
  peek = { ...peek, index: (peek.index + 1) % view.total };
}

// ── 动作 ──────────────────────────────────────────────────────────────────

function replace(next: ChatSnapshot) {
  snapshot = next;
  markOpenRead();
}

/**
 * 打开着的那个会话里来了新消息 → 立刻标已读。
 *
 * 只在**详情真开着**（浮层展开 + 打开的是这台设备/这个群）时做：面板收起、或停在列表上时，
 * 那几条就该老老实实留在未读里（底栏角标是给那个时候看的）。多台会话没有入站的未读（群发是
 * 单向的）。群聊那次是**群自己的水位**（`group_mark_read`），与各台的 1:1 互不相干。
 */
function markOpenRead() {
  if (!open) return;
  const ref = refOf(openKey);
  if (!ref) return;
  if (ref.mode === 'direct') {
    if (unreadForPeer(ref.id) > 0) void api.chatMarkRead([ref.id]).then(replace).catch(() => {});
    return;
  }
  if (ref.mode === 'group' && (groupOf(ref.id)?.unread ?? 0) > 0) {
    void api.groupMarkRead(ref.id).then(replace).catch(() => {});
  }
}

/**
 * 打开一个群时补拉一次（`group_sync`）：名册落后 / 掉线期间缺的那几条，开这条会话就去要。
 *
 * 失败**什么都不说**：补拉本来就是尽力而为（够不着的成员就是够不着），一次没拉到不代表下次
 * 拉不到；缺口本身由消息流头顶那行如实报（`ChatGroup.missing`），不需要再给一句会误导的提示。
 * 同一个群同一时刻只飞一次。
 */
function syncGroup(id: string) {
  if (syncingGroup === id) return;
  syncingGroup = id;
  void api
    .groupSync(id)
    .then(replace)
    .catch(() => {})
    .finally(() => {
      if (syncingGroup === id) syncingGroup = null;
    });
}

/** 打开一个会话（键见 `deviceKey`；已完成/失效的名单在这里被挡掉）。 */
export function openConversation(key: string) {
  if (!threadOf(key)) return;
  openKey = key;
  peek = null;
  open = true;
  // 打开群会话时补拉一次：开它的人就是马上要看的人，缺的那几条此刻去要最值（`group_sync`）。
  const ref = refOf(key);
  if (ref?.mode === 'group') syncGroup(ref.id);
  markOpenRead();
}

/** 打开与一台设备的会话（设备行上的「发消息」、右键菜单、文件标签页都走它）。 */
export function openDeviceChat(peerId: string) {
  openConversation(deviceKey(peerId));
}

/** 回到会话列表（草稿留着）。 */
export function backToList() {
  openKey = null;
  peek = null;
}

/**
 * 新建一个多台会话（收件人选择框的确认回调）。
 *
 * 只选了一台 = 一对一会话（不产生"只有一台的群发"）；群发走后端那份名单（同一批人重复建
 * 复用同一份），群聊走后端那个群（`group_create`，名册里本机那一条由后端补）—— 两者都
 * 落成一份后端记录，所以刷新 / 重启之后还在。
 */
async function createThread(ids: string[], mode: ConvoMode, name: string): Promise<void> {
  const peers = ids.filter(known);
  if (!peers.length) return;
  if (peers.length === 1) {
    openDeviceChat(peers[0]);
    return;
  }
  if (mode === 'group') {
    const created = await api.groupCreate(peers, name.trim());
    replace(created.snapshot);
    openConversation(groupKey(created.id));
    return;
  }
  const created = await api.chatBatchEnsure(peers);
  replace(created.snapshot);
  if (name.trim()) replace(await api.chatBatchRename(created.id, name));
  openConversation(batchKey(created.id));
}

/**
 * 编辑多台会话的参与者与名字（模式在编辑里不换：元数据不齐的"换模式"会把历史变成两半）。
 *
 * 群聊那条路与另两种有一处根本差别：**名册是后端的真相**，不是"本机设备表里还在的那几台"。
 * 选择框只列得出设备表里的设备，所以名册里那些列不出来的成员在这里必须原样留着 —— 否则改一次
 * 名字就顺手把它们从群里删了（用户从没做过这个决定）。要专门去掉某一份，走收件人 chips 上
 * 那颗 ✕：它按名册整份交出去，不受"看不看得见"影响。
 */
async function editThread(key: string, ids: string[], name: string): Promise<void> {
  const t = threadOf(key);
  if (!t || t.mode === 'direct') return;
  if (t.mode === 'group') {
    const invisible = (groupOf(t.id)?.members ?? []).map((m) => m.id).filter((id) => !known(id));
    const roster = [...new Set([...ids.filter(known), ...invisible])];
    if (!roster.length) return;
    replace(await api.groupEdit(t.id, roster, name.trim()));
    return;
  }
  const peers = ids.filter(known);
  if (peers.length < 2) return;
  const updated = await api.chatBatchEnsure(peers, t.id);
  replace(updated.snapshot);
  replace(await api.chatBatchRename(t.id, name));
}

/**
 * 退出 / 删掉一个多台会话（⋮ 菜单）。
 *
 * 群发删的是**这份名单**：消息一条不动（它们本来就在各台设备的会话里，两边都在）；
 * 群聊走 `group_leave`（本机从名册里去掉，后端跟着把本机的已读水位与这个群的关系都收掉），
 * 拉回来的那份快照里就没有它了 —— 列表里那一行跟着消失。返回标题给调用方拼提示
 * （`null` = 这不是一个多台会话）。
 */
export async function exitThread(key: string): Promise<{ title: string; mode: ConvoMode } | null> {
  const t = threadOf(key);
  if (!t || t.mode === 'direct') return null;
  if (t.mode === 'group') replace(await api.groupLeave(t.id));
  else replace(await api.chatBatchDelete(t.id));
  if (openKey === key) backToList();
  return { title: t.title, mode: t.mode };
}

/**
 * 把一份群发（`bc:`）**升级成群聊**：拿它那份名单建一个群（`group_create`），换上新快照并
 * 打开这条群聊。返回新群 id（`null` = 这不是一份群发，或那份名单已经不在快照里）。
 *
 * **历史不迁**：群发出去的每条消息早就各自躺在各台的 1:1 会话里，对端也从没见过它属于一个
 * "组"—— 升级只是把同一批人从"各发各的"换成"共享一条消息流"，旧消息仍留在原来的地方。
 * 这句要如实告诉用户（菜单项的 `title` 里就是这么写的），别让人以为换个模式能补出一段历史。
 *
 * 名单原样交出去：`ChatBatch.members` 里那些已经不在本机设备表里的成员一个不删 ——
 * "升级"不是"顺手清理名单"，要加要减走编辑那份名单。
 */
export async function upgradeBatch(key: string): Promise<string | null> {
  const ref = refOf(key);
  if (!ref || ref.mode !== 'broadcast') return null;
  const batch = snapshot.batches.find((b) => b.id === ref.id);
  if (!batch) return null;
  const created = await api.groupCreate(batch.members, batch.name);
  replace(created.snapshot);
  openConversation(groupKey(created.id));
  return created.id;
}

/**
 * 收件人栏上那颗 ✕：把一台从名单里去掉（**不是踢人**，对方不会收到任何通知）。
 *
 * 去掉之后只剩一台（或一台不剩）时，这份"多台记录"就没有存在的道理了 —— 群发删名单
 * （消息本来就在那台的 1:1 里），群聊退出这个群（本机从名册里走），然后回到那一台的会话。
 */
export async function removeParticipant(
  key: string,
  peerId: string,
): Promise<{ name: string; dissolved: boolean; remaining: string | null } | null> {
  const t = threadOf(key);
  if (!t || t.mode === 'direct') return null;
  const name = deviceNameOf(peerId);
  const rest = t.participants.filter((id) => id !== peerId);
  if (rest.length >= 2) {
    // 群的名册就是真相：这次要的就是"把这一台从名册里去掉"，整份 `rest` 直接交出去
    // （其余成员哪怕不在本机设备表里也一个不动）。走 `editThread` 的话，那些"选择框里列不出来"
    // 的成员会被原样补回去 —— 刚点掉的这一台只要也是这种情况，就又回去了。
    if (t.mode === 'group') replace(await api.groupEdit(t.id, rest, t.name));
    else await editThread(key, rest, t.name);
    return { name, dissolved: false, remaining: null };
  }
  await exitThread(key);
  if (rest.length === 1) openDeviceChat(rest[0]);
  return { name, dissolved: true, remaining: rest[0] ?? null };
}

/**
 * 发一条文本消息：一对一 = 给那一台；群发 = 给名单上的每台（各存一份副本、同一个消息 id）；
 * 群聊 = 群里那一条消息流（`group_send`，**不镜像进各台的 1:1**，与群发相反）。
 *
 * 返回这条的 id 与它挂在哪份名单上（界面据此读逐台战果；`null` = 没发）。群消息没有"名单"
 * 那一层，`batchId` 一律是 `null`。
 */
export async function sendText(key: string, text: string): Promise<{ id: string; batchId: string | null } | null> {
  const t = threadOf(key);
  const body = text.trim();
  if (!t || !body) return null;
  if (t.mode === 'group') {
    const res = await api.groupSend(t.id, body);
    replace(res.snapshot);
    return { id: res.id, batchId: null };
  }
  const res = t.mode === 'direct'
    ? await api.chatSend([t.participants[0]], body)
    : await api.chatSend(t.participants, body, t.id);
  replace(res.snapshot);
  return { id: res.id, batchId: res.batchId };
}

/**
 * 接受一条群邀请 —— 从这一刻起它才算群（进会话列表），并直接打开它。
 *
 * **不加入的群不进会话列表**：邀请只从 `pendingGroups` 进会话列表那张卡，点「加入」才
 * 落到 `groups` 里（后端 `group_join`）。
 */
export async function joinGroup(id: string): Promise<void> {
  replace(await api.groupJoin(id));
  openConversation(groupKey(id));
}

/** 拒绝一条群邀请：邀请卡跟着快照一起消失（本地记下"拒绝过"，同一份邀请不会再弹）。 */
export async function dismissGroup(id: string): Promise<void> {
  replace(await api.groupDismiss(id));
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
 *
 * `chatBatchId` = 这次是从哪个群发里发的（空 = 不是群发）；`groupId` = 这次是发到哪个群里
 * （空 = 不是群里发的）—— 群那条视图按 `groupId` 筛出记录，两个字段互斥（群聊不镜像进 1:1，
 * 群里发文件就是"拿群成员当收件人跑一次多目标推送"）。调用方（App）从**当前开着的那条会话**
 * 读它们。
 */
export function noteOutgoingFiles(peerIds: string[], items: LocalPick[], push: PushBatch, chatBatchId: string, groupId = '') {
  const pushBatchId = push.batchId;
  // 逐台状态当场就有了（`push_paths` 回来的那份快照）——不等 `push://batch` 再回一条：
  // 未绑定的对端停在"等待同意"时不会有下一条事件，那条记录下面就会一直空着。
  const perTargets = push.targets.map((t) => ({ peerId: t.peerId, state: t.state }));
  const live = peerIds.filter(known);
  if (!live.length) return;
  const at = Date.now();
  for (const peerId of live) {
    const added: ChatFileMsg[] = items.map((it, i) => ({
      id: `file:${pushBatchId}:${i}`,
      at: at + i,
      fromMe: true,
      name: it.name,
      isDir: it.isDir,
      peerId: '',
      size: it.isDir ? '文件夹' : formatFileSize(it.size),
      targets: live.length,
      failed: 0,
      pct: 0,
      sending: true,
      batchId: pushBatchId,
      chatBatchId,
      groupId,
      perTargets,
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
        perTargets: b.targets.map((t) => ({ peerId: t.peerId, state: t.state })),
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
    chatBatchId: '',
    groupId: '',
    perTargets: [],
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