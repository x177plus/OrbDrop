import { listen } from '@tauri-apps/api/event';
import { formatFileSize } from '$lib/types';
import { hasContent, revokeSegs, type ComposerSeg } from '$lib/composer';
import type { ChatDelivery, ChatFileMsg, ChatGroup, ChatGroupMsg, ChatSnapshot, ChatMessage, ChatPeerConvo, ConvoMode, LocalPick, PendingGroup, PushBatch, PushInvite, PushTargetState } from '$lib/types';
import * as api from '$lib/api';
import { getDevices, deviceById, deviceState, threadDotState, threadTrusted, peerShortName, type DeviceDotState } from './devices.svelte';

/**
 * 会话（聊天）：后端那份记录在前端的影子 + 会话界面自己的状态。
 *
 * **一份真相在后端**（`chat.dat` + `chat://changed` 事件，载荷是整份快照）：这一层只做
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
 * **文件消息**（`files`，设备 id → 那几条）：会话里那条"发了 / 收到了一个文件"的气泡。
 * 它与文本**同处落盘**（后端 `chat.dat` 的 `ChatFileMsg`，`chatFilesSet` 是写入口）——以前
 * 它只活在本机内存里，重启就丢：只发过文件、没说过话的会话整条从列表里消失（见 `convoRows`），
 * 重发一条文本把它拉回来时文件那几条也回不来了。
 *
 * 但**进度不在落盘里**（每 200ms 一跳，不值得每拍写一次盘）：这一层是它的**实时作者**，
 * 只在能定下来的时刻（刚发出 / 刚收到 / 接受或拒绝 / 收完或失败 / 整批走完）把该设备那份
 * 列表写回后端（`persistFiles`）；启动时反过来，从快照里种下（`seedFiles`）。界面上
 * 那些"传输中 62%"只在这份内存里 —— 进程断在中间时，后端重启扫尾会把那条落成终态。
 * 群里发文件也没有新的后端命令：那就是"拿群成员当收件人跑一次多目标推送"，本机这条记录
 * 挂上 `groupId` 就能归到群那条视图里（见 `noteOutgoingFiles`）。
 */

/** 会话浮层没数据时的占位（只读，别改）。 */
const EMPTY_SNAPSHOT: ChatSnapshot = { rev: 0, convos: [], batches: [], groups: [], pendingGroups: [], unread: 0 };

let snapshot = $state<ChatSnapshot>(EMPTY_SNAPSHOT);
/** 会话浮层展开着（底栏那颗「会话」按钮）。 */
let open = $state(false);
/** 详情里开着哪个会话（`null` = 停在会话列表）。键见 `deviceKey` / `refOf`。 */
let openKey = $state<string | null>(null);
/** 还没发出去的草稿：**按会话**存，切来切去不丢（分段表，见 `lib/composer.ts`）。 */
let draft = $state<Record<string, ComposerSeg[]>>({});
/** 收件人选择框（「新建会话」「编辑参与者」共用一份）。 */
let recipientReq = $state<RecipientReq | null>(null);
/** 文件消息：设备 id → 那几条（见文件头说明）。启动时由 `seedFiles` 从快照种下。 */
let files = $state<Record<string, ChatFileMsg[]>>({});
/**
 * 会话里**正在接收**的推送被用户按了暂停的那些邀请（`inviteId → true`）。
 *
 * **只在内存里、不落盘**：暂停是"这一轮进程里的这个任务"的状态，与「浏览」下载同一档 ——
 * 重启后接收任务按续传记录自己接着跑，界面不该还停在「已暂停」。收完 / 失败 / 取消时清掉。
 */
let receivePaused = $state<Record<string, boolean>>({});
/** 已经种过了：快照里的文件消息只在启动那一刻取一次，之后这份内存就是实时的真相。 */
let filesSeeded = false;
/**
 * 正在补拉的群 id（见 `syncGroup`）：同一条会话被连着开两次（连点两下、或刚建完群紧接着
 * 打开）时，补拉只飞一次 —— 一次补拉回来的是整份快照，重复飞几遍只是白花几个来回。
 */
let syncingGroup: string | null = null;

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

/** 这台设备还在不在**设备表**里（设备页那一份，见 `devices.visibleDevices`）。 */
function known(peerId: string): boolean {
  return getDevices().some((d) => d.id === peerId);
}

/**
 * 这台设备**还有没有聊天记录**（一条文本或一条文件消息都算）。
 *
 * 会话与设备行是两件事：设备行说的是"这台现在能不能连上"（离线**且未绑定**的不列，见
 * `visibleDevices`），而聊天记录是**用户数据** —— 一台聊过天的设备解除配对（关系没了、
 * 地址簿里也删了）、又离线之后，记录还躺在 `chat.dat` 里，会话列表那一行就没有跟着消失的
 * 道理。所以"列不列、能不能打开、未读算不算"都认这一条，见 [`directPeerIds`] / [`threadOf`]。
 */
function hasHistory(peerId: string): boolean {
  return (convoOf(peerId)?.messages.length ?? 0) > 0 || (files[peerId]?.length ?? 0) > 0;
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

/**
 * 通知点进来的落点：后端给的 `kind`（`dev` / `grp`）+ 它那份 id → 会话键
 * （`notify::Convo` 只带这两样，键怎么拼是界面自己的约定，后端不复制一份）。
 *
 * 认不出的 `kind` 返回 `null`：宁可不跳，也别跳到一条不存在的会话上。
 */
export function convoKeyOf(kind: string, id: string): string | null {
  if (kind === 'dev') return deviceKey(id);
  if (kind === 'grp') return groupKey(id);
  return null;
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
  return deviceNameOf(participants[0] ?? '');
}

/**
 * 群标题：有群名就用群名；**没群名时是成员名**（顺序按名册），不是成员 id。
 *
 * 回落必须走名册带来的名字（`ChatGroupMember.name`）：群成员**不需要配对**、也可能本机压根
 * 没发现过它，这种条目在设备表里查不到 —— `deviceNameOf` 会**原样退回设备 id**，标题于是
 * 变成「群聊 · 3f9c2a…、a11b7d…」。优先级：**设备表优先**（本机备注 F-101 就在这条路上）
 * → 名册里那个名字 → id（名册自己的名字也可能是 id：后端 `resolve_members` 拿不到名字时
 * 就是这么写的，那不是本机多出来的错）。
 */
function groupTitle(g: ChatGroup): string {
  if (g.name.trim()) return `群聊 · ${g.name.trim()}`;
  return g.members.map((m) => peerShortName(m.id, m.name.trim() || m.id)).join('、') || '群聊';
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
 * - 一对一：那台设备还在设备表里，**或**跟它的记录还在（解绑又离线之后，会话不跟着消失 ——
 *   见 `hasHistory`）；
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
    // 设备页那一行可能已经不在了（解绑又离线）：记录还在就照旧开着这条会话。
    if (!known(ref.id) && !hasHistory(ref.id)) return null;
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
  return { key: groupKey(g.id), mode: 'group', id: g.id, name: g.name, participants, title: groupTitle(g) };
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
 * 与 [`convoRows`] 同一份名单（`directPeerIds`），不是"还有几台设备"：**会话列表里列得出的**，
 * 未读就得算进来 —— 反向漏掉（列表里挂着未读、角标不认）会让那个数永远清不干净，反向多算
 * （算了列不出来的）会让角标清不掉。设备解绑又离线但**记录还在**的那几台照样列得出来，
 * 所以照样算（`unreadForPeer` 照着同一份记录读，不看它现在在不在网上）。
 *
 * 多台会话不产生未读（群发是单向的）。群聊的未读是**后端算好的**（本机之外、排在已读水位
 * 之后的条数）—— 这里只把还在快照里的那些群加一遍，不重算。
 */
export function unreadTotal(): number {
  const peers = directPeerIds().reduce((n, peerId) => n + unreadForPeer(peerId), 0);
  const groups = snapshot.groups.reduce((n, g) => n + g.unread, 0);
  return peers + groups;
}

/**
 * **别的会话**上等着看的未读（当前开着的那条不算）—— 会话详情头顶那颗角标记的就是它。
 *
 * 正停在 A 的会话里、B 来了话，用户要问的是"**别处**还等着几条"：把当前这条自己的未读算进去
 * 是错的，那是屏幕上已经摊开的东西。而且当前这条一进来就被标已读（`markOpenRead`，回来还要
 * 等一次 `chat_mark_read` 的往返），算进去的话数字要么恒为 0、要么在那一瞬间闪一下 —— 不是
 * 一条能读的数。减掉的那一项必须是**当前这条自己的**：一对一按台（`unreadForPeer`）、群聊按群
 * （`ChatGroup.unread`）、群发不产生未读（单向，没有要减的）。
 *
 * 顶掉的是"面板收起时看底栏角标、进了会话看不到"那一段：不显示的话，会话详情里**没有任何
 * 像素**提示别处来消息了，得退回去才知道。
 */
export function unreadElsewhere(): number {
  const t = activeThread();
  if (!t) return unreadTotal();
  if (t.mode === 'direct') return unreadTotal() - unreadForPeer(t.participants[0]);
  if (t.mode === 'group') return unreadTotal() - (groupOf(t.id)?.unread ?? 0);
  return unreadTotal();
}

/**
 * 会话侧"等你处理"的总数 = 未读 **+ 还没确认的群邀请**（底栏「会话」那颗角标、悬浮球左上角
 * 那颗、移动端「会话」页签都读它）。
 *
 * 为什么邀请要并进来：邀请卡只画在会话列表（桌面端）/「会话」页（移动端）里，而这两处都是
 * 用户主动打开的 —— 不计数的话，**面板收着（或停在别的页）时一条群邀请一个像素都不显示**
 * （后端那份 `unread` 只算一对一 + 已加入的群，邀请卡不在其中，这是刻意的：邀请不是消息）。
 */
export function convoBadgeTotal(): number {
  return unreadTotal() + snapshot.pendingGroups.length;
}

/**
 * 一台设备在会话侧的**短名**（F-101：有备注就是备注本身，没备注才是设备名）：消息里存的是收到那一刻的名字，改过名/起过备注的设备以本机设备表为准。
 *
 * 设备表里**一台都找不到**时（解绑又离线，见 `hasHistory`）还有记录里那个名字可退：
 * 对端自己发来的消息带着它当时的设备名（`ChatMessage.fromName`，后端落盘的那一份）——
 * 会话还在，名字就不该只剩一串设备 id。记录里也没名字（比如只收过一条文件消息）才退回 id。
 */
export function deviceNameOf(peerId: string): string {
  return peerShortName(peerId, nameFromHistory(peerId) ?? undefined);
}

/** 记录里认得的这台设备的**最后一个名字**（对端自己发的消息带着它当时的设备名）；记录里没有 = `null`。 */
function nameFromHistory(peerId: string): string | null {
  const msgs = convoOf(peerId)?.messages ?? [];
  for (let i = msgs.length - 1; i >= 0; i--) {
    const m = msgs[i];
    const name = m.fromMe ? '' : m.fromName.trim();
    if (name) return name;
  }
  return null;
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
  const body =
    last.kind === 'file' ? `[文件] ${last.name}` : last.image ? '[图片]' : last.text;
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
  const body = last.kind === 'file' ? `[文件] ${last.name}` : last.image ? '[图片]' : last.text;
  return (r ? `已送达 ${r.delivered}/${r.total} 台 · ` : '') + body;
}

/**
 * 会话列表里的一行：一对一 / 群发 / 群聊三种身份混排。
 *
 * **一对一那行要有过消息才画**：设备表里"刚配对、或刚跟对端互报了设备表（F-11a）学到、
 * 一句话都没说过"的设备不进会话列表 —— 互报一次能把对端视野里的设备全带进来，全列出来的话
 * 会话页就成了设备页的副本。要跟没说过话的设备开聊走「＋ 新建会话」（桌面端设备页那一行
 * 也还有「发消息」）。群发/群聊照旧列：它们本来就是用户自己建/加入的，还没说话也该看得见。
 *
 * **判据是"有没有记录"，不是"这台设备还在不在设备表里"**：有过消息的设备解除配对、又离线
 * 之后，它那一行照旧留着（名单见 `directPeerIds`）—— 记录还在盘上，用户要能翻它。这也是
 * 这一列表与设备页最大的不同：设备页是"这台现在能不能连上"，会话页是"跟谁说过话"。
 *
 * 动过的按最近一条倒序在前，没动过的排在后面（名单按各自的顺序），且不会每次刷新乱跳。
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
  /** 状态点：`stores/devices` 那五档（颜色 = 可达性、实心/空心 = 是否已验证）。 */
  dot: DeviceDotState;
  /** 这一条线程里是否所有设备都已验证（实心/空心的判据）。 */
  trusted: boolean;
  /** 一对一那行头像上的字（群发/群聊画图标，用不到）。 */
  initial: string;
};

/**
 * 一对一那几条会话要看的设备 id：**设备表里的全部**（没说过话的那几台靠 `lastOf` 挡在外面），
 * 加上**表里已经没有、但记录还在**的那些。
 *
 * 后半截就是这个列表存在的原因：跟一台设备聊过天之后，它（或本机）解除配对、又离线 ——
 * 设备页那一行按 F-13 不列了（离线且未绑定的没有可做的动作），会话却还在盘上。记录是
 * **用户数据**，不该跟着"这台现在能不能连上"走：会话列表那一行、能不能打开它、它的未读
 * 都靠这份名单（名字见 `deviceNameOf`）。
 *
 * **顺序**：设备表里的在前（表里那些行的相对次序照旧），记录里多出来的那些跟在后面 ——
 * 列表本身按"最近一条"倒序排（见 `convoRows` 末尾），这里的次序只影响同一时刻的并列。
 */
function directPeerIds(): string[] {
  const ids = getDevices().map((d) => d.id);
  const seen = new Set(ids);
  for (const convo of snapshot.convos) {
    if (seen.has(convo.peerId)) continue;
    seen.add(convo.peerId);
    ids.push(convo.peerId);
  }
  // 本机手上还没落盘的那份文件记录也算（收到一条推送邀请就有一条，见 `noteInboundInvite`）：
  // 它可能比快照先到，或后端那份已经被清掉而本机这条还在。
  for (const peerId of Object.keys(files)) {
    if (seen.has(peerId)) continue;
    seen.add(peerId);
    ids.push(peerId);
  }
  return ids;
}

export function convoRows(): ConvoRow[] {
  // 一对一：**有过消息才算一条会话**（见上面那段说明）—— 没说过话的设备不进这个列表。
  // 设备表里已经没有的那几台只要记录还在就照样列（见 `directPeerIds`）。
  const rows: ConvoRow[] = [];
  for (const peerId of directPeerIds()) {
    const last = lastOf(peerId);
    if (!last) continue;
    // 这一台查**两份表**（见 `deviceById`）：设备页列不出来的那几台（解绑又离线）在后端
    // 原样那份里，而这一行的名字与**状态点**都要用它们 —— 只查可见那份的话，
    // 找不到 = 恒 `offline`，与后端真实状态无关。
    const dev = deviceById(peerId);
    rows.push({
      key: deviceKey(peerId),
      mode: 'direct' as const,
      at: last.at,
      label: deviceNameOf(peerId),
      sub: summaryOf(peerId),
      time: timeOf(peerId),
      unread: unreadForPeer(peerId),
      dot: deviceState(dev).dot,
      trusted: deviceState(dev).trusted,
      initial: peerInitial(peerId),
    });
  }
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
      trusted: threadTrusted(t.participants),
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
      trusted: threadTrusted(t.participants),
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
 *
 * `declined` = 这个群里**明确拒绝加入**的那些成员（`ChatGroup.declined`，调用方从会话上取）：
 * 它们那几份是 `failed`，但说成「未送达」就把"明确拒绝"和"这一跳没成"混成一件事了 ——
 * 用户看到收件人胶囊上的红色，需要这行字给出同一个说法。
 */
export function groupReceiptText(item: ChatItem, declined: readonly string[] = []): string {
  if (!item.fromMe || item.kind !== 'text' || 'targets' in item) return '';
  const states = Object.entries(item.delivery);
  if (!states.length) return '';
  const parts = [`已读 ${item.readCount}/${states.length}`];
  const waiting = states.filter(([, s]) => s === 'queued').length;
  const sending = states.filter(([, s]) => s === 'sending').length;
  const failedIds = states.filter(([, s]) => s === 'failed').map(([peerId]) => peerId);
  const refused = failedIds.filter((peerId) => declined.includes(peerId)).length;
  const failed = failedIds.length - refused;
  // 从别的成员那里补到的那些：也是到了，但走的不是本机这一跳 —— 单说一句，别混进"已读"
  // （它只是拿到了，不等于读过；后端也是等它真读了才把 `readCount` 加上去）。
  const synced = states.filter(([, s]) => s === 'synced').length;
  if (waiting) parts.push(`${waiting} 台等待发送`);
  if (sending) parts.push(`${sending} 台发送中`);
  if (synced) parts.push(`${synced} 台已同步（经其他成员补到）`);
  if (refused) parts.push(`${refused} 台已拒绝加入`);
  if (failed) parts.push(`${failed} 台未送达`);
  return parts.join(' · ');
}

/**
 * 这个群里**明确拒绝加入**的那些成员 id（`ChatGroup.declined`；不是群 = 空）。
 *
 * 收件人胶囊与消息那行小字都用它：一处染红、一处改说法，两处取同一个来源。
 */
export function groupDeclinedIds(groupId: string): string[] {
  return groupOf(groupId)?.declined ?? [];
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

/** `HH:MM`（会话列表行用：列表只关心"最近一条是什么时候"，日期在记录里给）。 */
export function clock(at: number): string {
  const d = new Date(at);
  return `${String(d.getHours()).padStart(2, '0')}:${String(d.getMinutes()).padStart(2, '0')}`;
}

/**
 * 会话记录里一条消息的**日期 + 时刻**：`今天 14:30` / `昨天 14:30` / `10月3日 14:30`；
 * 跨年写全 `2025年10月3日 14:30`。
 *
 * 与 [`clock`]（只要 `HH:MM`，列表行用）分开：翻记录时"哪一天"和"几点"一样要看得见 ——
 * 只写 `HH:MM` 的话，昨天的消息和刚才的消息长得一模一样。相对口径与移动端收件箱
 * （`FilesTab.shortTime`）同一套。
 */
export function stamp(at: number): string {
  const d = new Date(at);
  const now = new Date();
  const hm = `${String(d.getHours()).padStart(2, '0')}:${String(d.getMinutes()).padStart(2, '0')}`;
  if (d.toDateString() === now.toDateString()) return `今天 ${hm}`;
  const yesterday = new Date(now);
  yesterday.setDate(now.getDate() - 1);
  if (d.toDateString() === yesterday.toDateString()) return `昨天 ${hm}`;
  const md = `${d.getMonth() + 1}月${d.getDate()}日`;
  return d.getFullYear() === now.getFullYear() ? `${md} ${hm}` : `${d.getFullYear()}年${md} ${hm}`;
}

/**
 * 这条推送此刻是不是用户按了暂停（没暂停过 / 已经收尾 = `false`）。
 *
 * 界面读它画右侧那圈（`已暂停` = 圆环转灰、中心是继续键），状态文案也读它
 * （见 [`fileStatusText`]）。
 */
export function invitePaused(inviteId: string): boolean {
  return inviteId !== '' && receivePaused[inviteId] === true;
}

/**
 * 这条邀请此刻还在不在接收中（`inviteState === 'accepted'`；找不到 = `false`）。
 *
 * 只给"点圆环发命令"那条路收尾用：命令往返途中 `push://done` 可能已经先到（消息落成
 * 终态），那时别再把"已暂停/继续"的提示补上去。
 */
export function inviteReceiving(inviteId: string): boolean {
  if (inviteId === '') return false;
  for (const list of Object.values(files)) {
    for (const f of list) {
      if (f.inviteId === inviteId) return f.inviteState === 'accepted';
    }
  }
  return false;
}

/**
 * 界面按下暂停 / 继续后当场翻这一档（后端那道闸由调用方单独发命令）。
 *
 * 先改界面再发命令是刻意的：圆环要在按下的那一帧就变（等 IPC 回来才变，手快的人会以为
 * 没点上）；命令返回 `false`（那条接收已经收尾）时调用方再把它翻回去。
 */
export function setInvitePaused(inviteId: string, paused: boolean): void {
  if (!inviteId) return;
  if (paused) {
    if (receivePaused[inviteId]) return;
    receivePaused = { ...receivePaused, [inviteId]: true };
    return;
  }
  if (!receivePaused[inviteId]) return;
  const next = { ...receivePaused };
  delete next[inviteId];
  receivePaused = next;
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

/** 这一台此刻还能不能喊停（`invited` / `sending`，且拿着能取消的那条邀请）。 */
export function outgoingWaitingInvites(f: ChatFileMsg, peerId = ''): string[] {
  if (!f.fromMe) return [];
  return f.perTargets
    .filter(
      (t) =>
        (peerId === '' || t.peerId === peerId) &&
        (t.state === 'invited' || t.state === 'sending') &&
        !!t.inviteId,
    )
    .map((t) => t.inviteId);
}

/**
 * 自己发出去的那条气泡右侧要不要给停止：一对一 = **这一台**还在等 / 在传；
 * 群发 / 群聊 = 这一批里**还有台**在等 / 在传（逐台复制件也在各自 1:1 里，按各自那台判）。
 */
export function outgoingStoppable(f: ChatFileMsg, peerId = ''): boolean {
  return outgoingWaitingInvites(f, peerId).length > 0;
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
      if (!target) return '';
      const label = pushStateLabel(target.state);
      // 进度环已经画了百分比，左边这行再给个数 —— 不看环也能读出走到哪了。
      return target.state === 'sending'
        ? `${label} ${Math.min(100, Math.max(0, Math.round(f.pct)))}%`
        : label;
    }
    if (f.sending) return `传输中 ${Math.min(100, Math.max(0, Math.round(f.pct)))}%`;
    if (f.targets === 0) return '';
    const targets = f.perTargets;
    if (!targets.length) {
      if (f.targets - f.failed <= 0) return '未送达';
      const done = `已发送至 ${f.targets - f.failed} 台`;
      return f.failed > 0 ? `${done} · ${f.failed} 台未送达` : done;
    }
    const count = (state: PushTargetState) => targets.filter((t) => t.state === state).length;
    const done = count('done');
    const cancelled = count('cancelled');
    const rejected = count('rejected');
    const failed = count('failed');
    if (cancelled === f.targets) return '已取消';
    if (done === 0 && failed === f.targets) return '未送达';
    if (done === 0 && rejected === f.targets) return '已拒绝';
    const parts: string[] = [];
    if (done) parts.push(`已发送至 ${done} 台`);
    if (cancelled) parts.push(`${cancelled} 台已取消`);
    if (rejected) parts.push(`${rejected} 台已拒绝`);
    if (failed) parts.push(`${failed} 台未送达`);
    return parts.join(' · ');
  }
  if (f.inviteState === 'received') return '已接收 · 已保存到下载目录';
  if (f.inviteState === 'rejected') return '已拒绝接收';
  if (f.inviteState === 'cancelled') return '已停止接收';
  if (f.inviteState === 'failed') return '接收失败';
  // 接收中：百分比写在状态文字里（右侧圆环同一份进度）；暂停也是一句话，别让环成唯一的信号。
  if (f.inviteState === 'accepted') {
    const pct = Math.min(100, Math.max(0, Math.round(f.pct)));
    return invitePaused(f.inviteId) ? `已暂停 · ${pct}%` : `接收中 ${pct}%`;
  }
  return '';
}

// ── 界面状态 ──────────────────────────────────────────────────────────────

export function isChatOpen(): boolean {
  return open;
}

/**
 * 底栏那颗「会话」的开合。**打开一律落在会话列表**上 —— 每次开都先把会话键（`openKey`）清掉。
 *
 * 不在这里清的话，收起面板之后那个键还留着（"收起"只是收起，不是"离开这条会话"的声明），
 * 下次点「会话」落到的就是上次那条会话的详情：用户点的是"会话"，看到的却是上一次的落点。
 * 移动端那边子屏由 hash 决定，退回列表走 `backToList`，冷启动也停在列表 —— 这里是同一套落点。
 *
 * **清在"开"这一侧、而不是"关"那一侧**：关的那一下浮层还在淡出（`.convo-pop` 0.18s），
 * 此刻换屏会在这段时间里把列表闪给用户看（详情 ↔ 列表是两块完全不同的屏）；开的时候才换就
 * 看不见 —— 那一下浮层正从 `scale(0.2)` / `opacity: 0` 长出来。
 *
 * 只有这一条路会清：会话自己是走 `openConversation` 开的（那里把 `open` 直接置真），
 * 设备行的「发消息」、通知点进来、群邀请的「加入」都照旧直接落在那条会话上。
 *
 * 代价只有一个：收起前那条会话不再自动续上（要回去就再点一次那一行）。草稿按会话留着
 * （`draft`），列表 ↔ 详情来回都不丢字。
 */
export function setChatOpen(next: boolean) {
  open = next;
  if (next) { flushPendingDraftDrop(openKey); openKey = null; }
  else flushPendingDraftDrop(openKey);
}

/**
 * 一个会话的草稿：**有序的分段表**（一段文字 / 一张图），不是一坨字符串。
 *
 * 需求要的是"图夹在前后两段文字中间"，还要按框里的顺序发出去 —— 顺序本身就是数据，
 * 见 `lib/composer.ts` 的文件头。图片那一段里放的是**本机暂存文件的路径**（粘贴那一刻就
 * 落盘了），框里的 `url` 只是预览。
 */
export function draftSegsOf(key: string): ComposerSeg[] {
  return draft[key] ?? [];
}

/** 只换草稿，不做任何 URL 回收（回收判据因调用点而异，见 `setDraftSegs` / `sendComposer`）。 */
function writeDraft(key: string, segs: ComposerSeg[]) {
  draft = { ...draft, [key]: segs };
}

/** 整份换掉这个会话的草稿（输入区每次 `input` 之后叫一次）。 */
export function setDraftSegs(key: string, segs: ComposerSeg[]) {
  const prev = draft[key];
  writeDraft(key, segs);
  if (prev) revokeDroppedSegs(prev, segs);
}

/**
 * 上一版里有、这一版里没有的图片段 = 刚从输入区删掉的那几张芯片。它们的预览 `blob:` URL
 * 再没人读，当场回收 —— 这一版正是照着"框里此刻有什么"重算出来的（`composer.serializeComposer`
 * 读的就是 DOM 里的芯片），没了的那些确已不在 DOM / 预览里，回收不会把正在显示的图弄裂。
 * 按 `url` 字符串比对（不是对象）：序列化每次都给图片段新建对象，但 URL 是同一份。
 */
function revokeDroppedSegs(prev: readonly ComposerSeg[], next: readonly ComposerSeg[]): void {
  const kept = new Set<string>();
  for (const s of next) if (s.kind === 'image') kept.add(s.url);
  for (const s of prev) {
    if (s.kind === 'image' && s.url.startsWith('blob:') && !kept.has(s.url)) URL.revokeObjectURL(s.url);
  }
}

/**
 * 因为"这条会话正开着"而没能在 `clearPeer` 里当场清掉的草稿（键 = 会话键）。正开着的时候，
 * 草稿里的 `blob:` 预览还挂在输入区的芯片上：当场丢掉会让输入区"有图却发不出去"
 * （`draftHasContent` 变 false），当场 revoke 又会把正在显示的图弄裂 —— 所以整件事推迟到
 * **离开这条会话**（`backToList` / 开下一条 / 收起面板）时再做：那一刻芯片已不在屏幕上。
 */
const pendingDraftDrop = new Map<string, ComposerSeg[]>();

/** 清掉一条会话的草稿（`clearPeer` 用）：正开着则推迟到离开时（见 `pendingDraftDrop`）。 */
function dropDraft(key: string) {
  const segs = draft[key];
  if (!segs) return;
  if (openKey === key) {
    // 记的是**当时那几段**，不是"这个键"：推迟这段时间里用户可能又往框里敲字、粘图，
    // 那些一个字都不该动（推迟是为了显示，不是"离开时把草稿清空"）。
    pendingDraftDrop.set(key, segs);
    return;
  }
  clearDraftNow(key);
}

/** 真正丢掉草稿并回收它的预览 URL（这一刻它的芯片确已不在任何输入区/预览里）。 */
function clearDraftNow(key: string) {
  const segs = draft[key];
  if (!segs) return;
  const next = { ...draft };
  delete next[key];
  draft = next;
  revokeSegs(segs);
}

/** 离开一条会话时把推迟的那些**段**清掉（见 `pendingDraftDrop`）。正常路径下这里是空操作。 */
function flushPendingDraftDrop(key: string | null) {
  if (!key) return;
  const dropped = pendingDraftDrop.get(key);
  if (!dropped) return;
  pendingDraftDrop.delete(key);
  const gone = new Set<string>();
  for (const s of dropped) {
    if (s.kind === 'image' && s.url.startsWith('blob:')) gone.add(s.url);
  }
  if (gone.size === 0) return;
  const cur = draft[key];
  if (!cur) return; // 草稿已经不在了（发出去 / 被换成别的）：没有可收的
  const removed = cur.filter((s) => s.kind === 'image' && gone.has(s.url));
  if (removed.length === 0) return; // 这几段早被别的路（删芯片 / 发送）收过了
  const kept = cur.filter((s) => !(s.kind === 'image' && gone.has(s.url)));
  if (kept.length === 0) {
    const next = { ...draft };
    delete next[key];
    draft = next;
  } else {
    writeDraft(key, kept);
  }
  revokeSegs(removed);
}

/** 这条草稿里有没有值得发的东西（两段空白文字不算）。 */
export function draftHasContent(key: string): boolean {
  return hasContent(draftSegsOf(key));
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

// ── 动作 ──────────────────────────────────────────────────────────────────

function replace(next: ChatSnapshot) {
  // 命令的返回（IPC 响应）与 `chat://changed`（`eval` 进来的脚本）是两条通道，谁先到没有
  // 保证：发消息那一刻的返回里这条还是 `sending`，而后台 HTTP 那一跳落定后推的快照已经是
  // `sent` —— 前者晚到就会把后者盖回去，消息于是一直停在「发送中」，直到重新拉一次整份
  // 状态（关掉再打开会话）。只往版本大的那份走。
  if (typeof next.rev === 'number' && next.rev <= snapshot.rev) return;
  snapshot = next;
  // 第一次拿到快照（启动时 `refresh` / 第一条 `chat://changed`）：把落盘的文件消息种进内存。
  // 之后这份内存就是实时的真相 —— 每次 `replace` 再种一遍会把正在传的进度抹回上一次落盘的样子。
  if (!filesSeeded) {
    filesSeeded = true;
    seedFiles();
  }
  markOpenRead();
}

/**
 * 把快照里落盘的文件消息种进内存（**只在启动那一次**，见 `replace`）。
 *
 * **按设备合并**：本机已经有的那几条（万一某条推送事件比首次快照先到 —— 两边都是异步的）
 * 不该被后端那份旧的盖掉，后端那份里本机还没有的也要补上 —— 两边谁都不是"全的"。
 * 后端的排在前面：它是更早的（启动之前）记录。
 */
function seedFiles() {
  const seeded: Record<string, ChatFileMsg[]> = { ...files };
  for (const convo of snapshot.convos) {
    const persisted = convo.files ?? [];
    const local = seeded[convo.peerId];
    if (!local) {
      seeded[convo.peerId] = persisted;
      continue;
    }
    const have = new Set(local.map((f) => f.id));
    seeded[convo.peerId] = [...persisted.filter((f) => !have.has(f.id)), ...local];
  }
  files = seeded;
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
  if (openKey !== key) flushPendingDraftDrop(openKey);
  openKey = key;
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
  flushPendingDraftDrop(openKey);
  openKey = null;
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
 * 清空一个群聊的本地记录，但**不退群**：名册与成员关系保留，之后的新消息照常收。
 * 其他成员不受影响；补拉按清空下界走，不会把历史拉回来。
 */
export async function clearGroupHistory(key: string): Promise<boolean> {
  const t = threadOf(key);
  if (!t || t.mode !== 'group') return false;
  replace(await api.groupClearHistory(t.id));
  return true;
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
 *
 * `at` = 点名这条消息在时间线上的位置（只有输入区"图文混排"的每一段会点名，见 `sendComposer`）。
 */
export async function sendText(
  key: string,
  text: string,
  at?: number,
): Promise<{ id: string; batchId: string | null } | null> {
  const t = threadOf(key);
  const body = text.trim();
  if (!t || !body) return null;
  if (t.mode === 'group') {
    const res = await api.groupSend(t.id, body, at);
    replace(res.snapshot);
    return { id: res.id, batchId: null };
  }
  const res = t.mode === 'direct'
    ? await api.chatSend([t.participants[0]], body, null, at)
    : await api.chatSend(t.participants, body, t.id, at);
  replace(res.snapshot);
  return { id: res.id, batchId: res.batchId };
}

/** `sendComposer` 的结果：界面据此报"发了几条 / 哪几条没发出去"。 */
export interface ComposerSendResult {
  /** 最后一条**文本**消息的 id（群发那条视图据此读逐台战果；没文本 = `null`）。 */
  textId: string | null;
  /** 那份群发名单的 id（一对一 / 群聊 = `null`）。 */
  batchId: string | null;
  texts: number;
  images: number;
  /** 没发出去的那几段（已放回草稿：失败不清空，用户不用重打一遍）。 */
  failed: number;
  /** 第一次失败的原因（后端原话；没有失败 = `null`）。 */
  error: string | null;
}

/**
 * 把输入区里那一整条**图文混排**发出去 —— 这段文字 / 这张图 / 后面那段文字，一条一条按
 * 框里的顺序落成消息。
 *
 * 四件事必须一起做对，缺哪件用户都能看出来：
 *
 * 1. **顺序**：整条消息的每一段由**这里**统一编号（`at`），从"现在"起一段加一毫秒。为什么
 *    不能让后端各取各的"现在"：一条里图片夹在两段文字中间时，那几次调用很可能落在同一
 *    毫秒里，而两端的时间线都是按 `at` 排的（同一毫秒就跟着遍历顺序飘）——用户看到的"图在
 *    中间"就散了。图片那一段的号跟着**消息本身**过去（`ChatImage.at`），对端拿它排位置。
 * 2. **一条条发**：串行 `await`，不做并发 —— 并发发出去的三段在时间线上就没有先后可言了。
 * 3. **失败不清空**：哪一段没发出去，那一段原样留在草稿里（发出去的段落不再留）；
 *    一张图失败不影响后面那段文字照发。
 * 4. **图片是消息内容，不是推送**：走 `chat_send_image`（对端直接就能看，没有"接收 / 同意"
 *    那一步，也不进"传输中"那本账）。唯一例外是**群聊**（那一半的收发还没落地，见
 *    docs/p1-implementation.md §33）：群里暂时仍旧按文件推送 —— 也就是说群里的图还要对端
 *    点一次「接收」。
 */
export async function sendComposer(key: string): Promise<ComposerSendResult | null> {
  const t = threadOf(key);
  if (!t) return null;
  const segs = draftSegsOf(key);
  if (!hasContent(segs)) return null;

  // 这一整条的时间戳：从"现在"起严格递增，且**排在已有消息之后**（同一毫秒里发第二条时，
  // 不能让它落到上一条前面去）。
  const lastAt = itemsOf(key).reduce((max, i) => Math.max(max, i.at), 0);
  let stamp = Math.max(Date.now(), lastAt + 1);
  const nextStamp = () => stamp++;

  // 这条会话该发给谁（与 App.svelte 的 `convoSendTargets` 同一套：名册里那些已经不在本机
  // 设备表里的成员寻不到址），以及这份记录挂在哪份群发名单上。
  const peers = getDevices().filter((d) => t.participants.includes(d.id)).map((d) => d.id);
  const chatBatchId = t.mode === 'broadcast' ? t.id : '';

  const failedSegs: ComposerSeg[] = [];
  const result: ComposerSendResult = {
    textId: null,
    batchId: null,
    texts: 0,
    images: 0,
    failed: 0,
    error: null,
  };
  for (const seg of segs) {
    // 图片那一段要按设备寻址（一对一 / 群发），一台都找不到就把这张留在草稿里等下一次。
    // **群聊不算这一档**：群里发图按名册寻址（成员不必在本机设备表里）。
    if (seg.kind === 'image' && t.mode !== 'group' && !peers.length) {
      result.failed += 1;
      result.error ??= '这条会话里没有能送达的设备';
      failedSegs.push(seg);
      continue;
    }
    try {
      if (seg.kind === 'text') {
        // 空白的文字段（比如两张图之间用户多敲了个空格）不该落成一条空消息。
        if (!seg.text.trim()) continue;
        const res = await sendText(key, seg.text, nextStamp());
        if (res) {
          result.textId = res.id;
          if (res.batchId) result.batchId = res.batchId;
          result.texts += 1;
        }
        continue;
      }
      // 图片那一段：这里是"路径 + 名字"两条信息，后端把字节读进来、落成会话内容再发出去。
      const at = nextStamp();
      if (t.mode === 'group') {
        // 群聊：与一对一同一套（图片走会话协议，对端直接就能看）。
        const res = await api.groupSendImage(t.id, seg.path, seg.name, at);
        replace(res.snapshot);
      } else {
        // 图片走**会话协议**（`chat_send_image`）：对端直接就能看，没有同意那一步。
        const res = await api.chatSendImage(peers, seg.path, seg.name, at, chatBatchId || null);
        replace(res.snapshot);
        // 界面上那句"群发战果"读的是**最后一条文本**的消息 id；这一条是图，就不动它。
        result.batchId ??= res.batchId;
      }
      result.images += 1;
    } catch (e) {
      result.failed += 1;
      result.error ??= e instanceof Error ? e.message : String(e);
      failedSegs.push(seg);
    }
  }

  // 发出去的那几段连同它们的预览 URL 一起丢掉；没发出去的**原样留着**（图还在草稿里，
  // 它的暂存文件也还在盘上，直接再点一次发送就能重发）。
  // 回收的判据用**开始时捕获的 `segs`**（不是此刻的 `draft[key]`）：发送是异步的，期间用户
  // 若又粘了一张图，那张不在 `segs` 里、也不该被这里回收 —— 所以这里只 `writeDraft`，回收交给
  // 上面那趟显式循环（按 `segs` 逐段判），不用 `setDraftSegs` 的"按当前草稿差值回收"。
  for (const seg of segs) {
    if (!failedSegs.includes(seg)) revokeSegs([seg]);
  }
  writeDraft(key, failedSegs);
  return result;
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
  // 这条会话名下的草稿也一起丢：它里面的图片段挂着预览用的 `blob:` URL，`draft` 是按会话累积的，
  // 不清就会跟着这条已经不存在的会话一直活着。正开着这条会话时改为推迟回收（见 `dropDraft`）。
  dropDraft(deviceKey(peerId));
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
 * 把这几台设备的文件消息记录写回后端（`chat_files_set`）。
 *
 * **只在能定下来的时刻叫**（刚发出 / 刚收到 / 接受或拒绝 / 收完或失败 / 整批走完）：
 * 传输中的进度每 200ms 一跳，那是内存里的事，每拍都写一次盘（`chat.dat` 整份重写）
 * 纯属白花 —— 而断了的那一刻，后端重启扫尾会把还在传的那条落成终态。
 *
 * 失败只记一行：这是"记录"的持久化，把它变成用户可见的错误没有出口（下一次能定下来的
 * 时刻会再写一遍）。命令不返回快照的更新（后端也不推 `chat://changed`）—— 界面手上这份
 * 就是它的作者。
 */
function persistFiles(peerIds: Iterable<string>) {
  for (const peerId of new Set(peerIds)) {
    void api.chatFilesSet(peerId, files[peerId] ?? []).catch((e) => console.error('保存文件记录失败:', e));
  }
}

/**
 * 本机选了文件发出去：**每台目标各一条气泡**（群发过去的那份在每台设备的会话里各算一条，
 * 与文本消息同一个道理），都挂在同一个 `batchId` 上。
 *
 * `chatBatchId` = 这次是从哪个群发里发的（空 = 不是群发）；`groupId` = 这次是发到哪个群里
 * （空 = 不是群里发的）—— 群那条视图按 `groupId` 筛出记录，两个字段互斥（群聊不镜像进 1:1，
 * 群里发文件就是"拿群成员当收件人跑一次多目标推送"）。调用方（App）从**当前开着的那条会话**
 * 读它们。
 *
 * `at` = 这条气泡在时间线上的位置。0（默认）= 当场取现在；输入区那条"图文混排"的路会点名给它
 * （见 `sendComposer`）—— 那时图片夹在前后两段文字中间，位置必须由界面统一排。
 */
export function noteOutgoingFiles(peerIds: string[], items: LocalPick[], push: PushBatch, chatBatchId: string, groupId = '', at = 0) {
  const pushBatchId = push.batchId;
  // 逐台状态当场就有了（`push_paths` 回来的那份快照）——不等 `push://batch` 再回一条：
  // 未绑定的对端停在"等待同意"时不会有下一条事件，那条记录下面就会一直空着。
  const perTargets = push.targets.map((t) => ({ peerId: t.peerId, inviteId: t.inviteId, state: t.state }));
  const live = peerIds.filter(known);
  if (!live.length) return;
  const base = at || Date.now();
  for (const peerId of live) {
    const added: ChatFileMsg[] = items.map((it, i) => ({
      id: `file:${pushBatchId}:${i}`,
      at: base + i,
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
      // 源文件本来就在这台机器上：点这条气泡开的就是它（零拷贝，与文件标签页双击一格同一落点）。
      path: it.path,
      count: 0,
    }));
    patchFiles(peerId, (list) => [...list, ...added]);
  }
  persistFiles(live);
}

/** 推送批次快照 → 那一批文件消息的进度与成败（右下角那张卡是唯一的数据来源）。 */
export function noteBatch(b: PushBatch) {
  let hit = false;
  const next: Record<string, ChatFileMsg[]> = {};
  const touched: string[] = [];
  for (const [peerId, list] of Object.entries(files)) {
    next[peerId] = list.map((f) => {
      if (f.batchId !== b.batchId) return f;
      hit = true;
      touched.push(peerId);
      const sending = b.waiting > 0;
      const pct = b.total > 0 ? Math.min(100, Math.round((b.received / b.total) * 100)) : 0;
      return {
        ...f,
        targets: b.targets.length,
        failed: b.failed,
        pct: sending ? pct : 100,
        sending,
        perTargets: b.targets.map((t) => ({ peerId: t.peerId, inviteId: t.inviteId, state: t.state })),
        // 结算后就把卡片 id 摘掉：卡片 3 秒后从界面上消失，气泡不能跟着变成空数据。
        batchId: sending ? f.batchId : '',
      };
    });
  }
  if (hit) files = next;
  // 只有**整批走完**这一拍值得落盘（进度那几拍不写，见 `persistFiles`）。
  if (hit && b.waiting === 0) persistFiles(touched);
}

/** 收到一条推送邀请：会话里落下那张「接收 / 拒绝」卡（落在与推送方的那条会话里）。 */
export function noteInboundInvite(inv: PushInvite) {
  const peerId = inv.sender.id;
  const msg: ChatFileMsg = {
    id: `invite:${inv.inviteId}`,
    // **发送方给的**位置（不是"收到的这一刻"）：一次发出去的一整条里，图片夹在前后两段文字
    // 中间时，只有它能让对端排出同一个顺序（见 `sendComposer` / `WsEvent::PushInvite::at`）。
    // 老对端不带（0）→ 退回收到的那一刻。
    at: inv.at || Date.now(),
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
    // 落在哪只有后端知道：收完那一刻由 `push://done` 的 `dest` 补上（见 `noteInviteDone`）。
    path: '',
    count: inv.count,
  };
  patchFiles(peerId, (list) => (list.some((f) => f.inviteId === inv.inviteId) ? list : [...list, msg]));
  persistFiles([peerId]);
}

/**
 * 那条邀请被收下了（本机点「接收」，或后端自动收下）。
 *
 * 落盘的只是"这台设备此刻的记录"：`accepted` 不是终态，进程断在这儿时重启扫尾会把它落成
 * 终态（接收失败）—— 这里写一次是为了让**已经走过的决定**（用户点过接收）不随崩溃消失。
 */
export function noteInviteAccepted(inviteId: string) {
  persistFiles(patchInvite(inviteId, (f) => (f.inviteState === 'pending' ? { ...f, inviteState: 'accepted' } : f)));
}

/** 接收进度（`push://progress`）。 */
export function noteInviteProgress(inviteId: string, received: number, total: number) {
  patchInvite(inviteId, (f) => {
    const bytes = total || f.bytes;
    // **百分比在这里算**（P1-13③）：桌面端的气泡是拿 `received/bytes` 现算的，而移动端的
    // 「传输」页读的是 `pct` —— 只有发送侧那条路写它，接收侧一直是 0，于是手机上收到的
    // 传输永远显示 0%、进度条不动。收侧与发侧共用这一份，两边就都对了。
    const pct = bytes > 0 ? Math.min(100, Math.max(0, Math.round((received / bytes) * 100))) : f.pct;
    return {
      ...f,
      received,
      bytes,
      pct,
      inviteState: f.inviteState === 'pending' ? 'accepted' : f.inviteState,
    };
  });
}

/**
 * 落盘那一层目录 + 这条消息 → 本机上这一项的绝对路径。
 *
 * **多选合并成的那一条只能开目录**：`name`（共同父目录名 /「N 项」）只是显示用的，落盘根下面
 * 躺着的是**每个源自己的名字**（后端 `manifest_for_paths` 就是这么排的），拿显示名去拼会指到
 * 一个不存在的地方。单项推送才拼得准 —— 清单里那个顶层路径就是它自己的名字。
 * 分隔符跟 `dest` 走：Windows 给的是反斜杠，安卓给的是斜杠。
 */
function landingPath(dest: string, f: ChatFileMsg): string {
  if (f.count > 1) return dest;
  const sep = dest.includes('\\') && !dest.includes('/') ? '\\' : '/';
  return dest.endsWith(sep) ? dest + f.name : dest + sep + f.name;
}

/**
 * 这条邀请是不是**别人推给本机**的（会话里找得到那张接收卡）。
 *
 * `push://done` 两侧都会发：接收侧要报"收到 / 已取消 / 失败"，发送侧的同一条事件由
 * `push://batch` 收尾。发送侧没有这张卡，据此把那几句 toast 挡掉（否则会报成「文件」）。
 */
export function hasIncomingInvite(inviteId: string): boolean {
  if (!inviteId) return false;
  for (const list of Object.values(files)) {
    for (const f of list) {
      if (!f.fromMe && f.inviteId === inviteId) return true;
    }
  }
  return false;
}

/**
 * 那条邀请的终局：已接收 / 已拒绝 / 接收失败。
 *
 * `dest` = 后端随 `push://done` 给的那一层**落盘目录**（`<落盘目录>/<发送方名>`；S-12 关掉那层
 * 时就是落盘根本身），只有"已接收"才有。它是会话里那条气泡「打开」的**唯一路径来源**：
 * 东西落在哪只有后端知道，界面不自己拼目录名（同名/改写与净化都在后端）。
 */
export function noteInviteDone(inviteId: string, outcome: 'received' | 'rejected' | 'failed' | 'cancelled', dest = '') {
  // 收尾了就不存在"暂停/继续"这回事：那条圆环随后会被终态气泡替换掉。
  setInvitePaused(inviteId, false);
  persistFiles(
    patchInvite(inviteId, (f) => ({
      ...f,
      inviteState: outcome,
      sending: false,
      path: outcome === 'received' && dest ? landingPath(dest, f) : f.path,
    })),
  );
}

/** 改这条邀请所在的那几条文件消息，返回**改到了哪几台设备**（调用方据此决定要不要落盘）。 */
function patchInvite(inviteId: string, fn: (f: ChatFileMsg) => ChatFileMsg): string[] {
  const hit: string[] = [];
  const next: Record<string, ChatFileMsg[]> = {};
  for (const [peerId, list] of Object.entries(files)) {
    next[peerId] = list.map((f) => {
      if (f.inviteId !== inviteId) return f;
      hit.push(peerId);
      return fn(f);
    });
  }
  if (hit.length) files = next;
  return hit;
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