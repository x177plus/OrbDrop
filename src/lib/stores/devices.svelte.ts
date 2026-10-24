import { listen } from '@tauri-apps/api/event';
import type { PeerDevice } from '$lib/types';
import * as api from '$lib/api';

let devices = $state<PeerDevice[]>([]);
/**
 * 后端原样报上来的那一份（上面那份是 [`visibleDevices`] 过滤 + 排序之后的）。
 *
 * 留着它是给**会话侧取名字**用的：一台聊过天的设备解除配对又离线之后会掉出上面那份
 * （F-13：离线且未绑定的不列），但它的会话还留着（记录是用户数据，见 `stores/chat` 的
 * `convoRows`）—— 那一行的名字得从这儿找（`peerShortName` 会在这两份里各找一遍）。设备表
 * 本身很小（一屏设备数），多留一份引用不值一提。
 */
let allDevices = $state<PeerDevice[]>([]);
/**
 * 本机的设备备注（F-101）：`设备 id → 备注`，从后端整份读来（`list_device_remarks`）。
 *
 * 为什么与上面两份设备表**各存一份**：备注**不跟着设备行走**。没有那一行的设备（未配对、
 * 也不在地址簿：那一行只活在内存里，重启即忘 / 这一趟还没被观察到）在会话列表里照样要显示
 * 成用户起的那个备注 —— 而 `list_devices` 里根本没有它的行，于是"备注优先"会整条失效、
 * 退回"对端自报的设备名"（真机反馈：离线时会话里显示的是设备名而不是备注）。
 * 备注是用户数据（`config.toml` 的 `deviceRemarks`），与"此刻有没有那一行"无关。
 */
let remarks = $state<Record<string, string>>({});
let loading = $state(false);

export function getDevices() { return devices; }
export function isDevicesLoading() { return loading; }

/**
 * 交给界面的设备行：在线设备全列（长连接 / 已配对 / 未绑定），**已配对但离线的也列**
 * —— 对方不在线时才是真正需要「解除配对」的场合，而解除配对是本机动作
 * （`unpair_peer` 只是"尽力通知对端"，对端离线照样清干净本机 token）。
 *
 * 离线且未绑定的不列：它没有任何可做的动作，只会堆积曾经扫到的设备。
 * **例外是"看得见但连不上"的**（`seen`，F-106）：它照样列出来 —— 用户的感受是"刚才还看到它、
 * 现在没了"，稳定地摆一行比闪一下就没好。这一行**不带任何特殊状态**：关系没建立就是「未绑定」，
 * 状态点照旧按 `offline` 画；"这张网把单播隔开了"只在**发起连接**时弹一句 tip（见 `unicastBlocked`）。
 *
 * 排序（用户要求）：**在线在前、离线在后**；在线组内 长连接 → 已配对 → 未绑定
 * （离线组内同理，只是没有长连接可言）。同级按名字排 —— 按**设备列表上那个显示名**
 * （设了备注就是「备注(设备名)」）：用户看到的是显示名，排序就该跟眼睛一致，否则备注会把行序弄乱。
 */
export function visibleDevices(all: PeerDevice[]): PeerDevice[] {
  const group = (d: PeerDevice) => (d.online ? 0 : 1);
  const rank = (d: PeerDevice) => (d.connected ? 0 : d.paired ? 1 : 2);
  return all
    .filter((d) => d.online || d.paired || d.seen)
    .sort(
      (a, b) =>
        group(a) - group(b) ||
        rank(a) - rank(b) ||
        deviceDisplayName(a).localeCompare(deviceDisplayName(b)),
    );
}

export async function loadDevices() {
  loading = true;
  try {
    // 两份一起拉（备注那一份很小）：事件（`devices://changed`）来时两个都跟着刷新，
    // 所以本地镜像不会与后端分叉。
    const [all, saved] = await Promise.all([api.listDevices(), api.deviceRemarks()]);
    allDevices = all;
    devices = visibleDevices(all);
    remarks = saved ?? {};
  } catch (e) {
    console.error('Failed to load devices:', e);
  } finally {
    loading = false;
  }
}

/**
 * 一台设备的**显示名**（F-101）：设了备注就是「备注(设备名)」，否则就是设备名本身。
 *
 * **只给局域网设备列表那一处用**：设备行（名字 + 头像字）、它的右键菜单、搜索匹配与排序 ——
 * 那里是"挑哪一台机器"的地方，把设备名一起写着才认得出。别处一律 `deviceShortName`
 * （有备注就只写备注）：会话列表/标题、标签栏、设备页头、收件人选择框、推送卡、toast 与通知。
 * 名字长了照样能读全：这些地方都挂了 `title`（详见 `deviceShortName` 那段）。
 */
export function deviceDisplayName(d: PeerDevice): string {
  const remark = d.remark?.trim();
  return remark ? `${remark}(${d.name})` : d.name;
}

/**
 * 一台设备的**短名**（F-101）：有备注就是备注本身（**不挂设备名**），没备注就是设备名。
 *
 * 这是本机界面上的**默认**名字。备注只存在本机（`config.toml` 的 `deviceRemarks`），
 * 对端看到的仍是它自己的名字 —— 拼装只在这一处，谁也别自己拼一遍。
 *
 * 空字符串的备注按"没备注"处理：后端不会存空串（见 `Config::set_device_remark`），
 * 但手改过的 `config.toml` 里可能有。名字被截断时全名在 `title` 里（悬停看全）。
 */
export function deviceShortName(d: PeerDevice): string {
  const remark = d.remark?.trim();
  return remark || d.name;
}

/**
 * 按 id 取设备行：**先查可见的那份（设备页用的），再查后端原样那份**（`allDevices`）。
 *
 * 为什么必须查两份：`visibleDevices` 把"离线 + 未绑定 + 没被 mDNS 看见"的行整行挡住了
 * （它没有任何可做的动作），可**跟它的会话还在**（记录是用户数据）—— 那些行只有
 * `allDevices` 里才有。名字那条路早就是这么取的（见 `peerShortName`），状态那条路漏了：
 * 会话列表那一行于是恒画灰点（`deviceDotState(undefined)` = `offline`），与后端真实状态无关。
 *
 * **凡是要"这台现在怎么样"的地方（点、胶囊、动作可用性）都走这里**；设备页自己那份列表
 * （`DeviceTab`/`DevicesTab`）照旧只认可见的行 —— 它画的就是"设备页里列得出来的那几台"。
 */
export function deviceById(peerId: string): PeerDevice | undefined {
  return devices.find((d) => d.id === peerId) ?? allDevices.find((d) => d.id === peerId);
}

/**
 * 一批 id → 设备行（每个都走 [`deviceById`]）：**取不到的丢掉**（设备表里真没有这一台 ——
 * 记录里只有名字的那种）。
 *
 * 与 `getDevices().filter(...)` 的区别只在第二份表：会话那一侧（聊过天但设备页列不出来的
 * 那几台）要用它算"有几台在线 / 送得到"。
 */
export function devicesById(peerIds: string[]): PeerDevice[] {
  const rows: PeerDevice[] = [];
  for (const id of peerIds) {
    const d = deviceById(id);
    if (d) rows.push(d);
  }
  return rows;
}

/**
 * 一台设备的**备注**（F-101）：先看设备行那份，再看整份备注表（`list_device_remarks`）。
 *
 * 两份都要看：行里那份是"此刻这一行带的备注"，而**没有那一行的设备**（未配对、也不在地址簿，
 * 重启即忘）在会话列表里照样要显示成用户起的备注 —— 那份备注只存在本机的 `deviceRemarks` 里。
 * 空串按"没备注"处理（后端不存空串，手改过的 `config.toml` 里可能有）。
 */
export function deviceRemarkOf(peerId: string): string | null {
  const row = deviceById(peerId)?.remark?.trim();
  if (row) return row;
  return remarks[peerId]?.trim() || null;
}

/**
 * 按设备 id 取**短名**（拿不到这台设备时用 `fallback`，没给就退回 id 本身）。
 *
 * **备注优先**（F-101 的设计）：有备注就是备注本身，**不管此刻有没有那一行** —— 它是用户起的
 * 名字，比设备名更该被看见（行不在时靠 `list_device_remarks` 那份，见 `deviceRemarkOf`）。
 *
 * `fallback` 是给"名字来自别处"的场合用的：会话消息、推送卡与通知里的名字是**收到那一刻**
 * 存下来的字符串，设备表里可能已经没有这一台（移除过、或还没学到它），那时至少要有名字可显示。
 *
 * **设备页那份列表里没有的这台，再去后端原样那份里找一遍**（见 `deviceById`）：解绑又离线的
 * 设备会被 `visibleDevices` 挡在设备页之外，可跟它的会话还在（记录是用户数据，见 `stores/chat`
 * 的 `convoRows`）—— 会话列表与标题要拿它的名字，而不是一串设备 id。
 */
export function peerShortName(peerId: string, fallback?: string): string {
  // 备注优先（F-101 的设计）：**不管此刻有没有那一行** —— 它是用户起的名字，比设备名更该被看见。
  const remark = deviceRemarkOf(peerId);
  if (remark) return remark;
  const d = deviceById(peerId);
  const known = d ? deviceShortName(d).trim() : '';
  // **占位名 = "还没有名字"**：设备行是拿设备 id（或空串）顶着的 —— `AppState::new` 给
  // "只有凭据、地址簿里没有"的行起名就是 id，中转核实过的行（`observe_verified_addr`）也是。
  // 判据与后端 `name_is_placeholder` 一字不差：空串、或等于设备 id。
  //
  // 为什么必须这么判：名字有**两条来源**（设备行那份 = 观察值，会话记录里那份 = 对端自己
  // 发来的消息带着的 `fromName`，落盘、跨重启还在），而设备行**存在但不含信息**时旧写法
  // 会把它当成"已经知道了"，于是启动那一刻（还没有任何新的观察）永远先显示一串 deviceid，
  // 要等探活回话/它自己开口才变成名字 —— 而这句话本机早就有了。
  if (known && known !== peerId) return known;
  return fallback || known || peerId;
}

/**
 * 写一台设备的备注（F-101）：`''` = 清除。返回后端真正存下去的值（`null` = 已清除）。
 *
 * 本地**当场**改一份（右键点完立刻能看到新显示名），后端随后广播的 `devices://changed`
 * 会再拉一次完整列表（两边同值，谁先到都不影响）。
 */
export async function saveDeviceRemark(peerId: string, remark: string): Promise<string | null> {
  const stored = (await api.setDeviceRemark(peerId, remark)) ?? null;
  // 两份设备表都改（只改可见那份的话，掉出可见集的那台在会话里一直显示旧名字），
  // 备注表也跟着改 —— 它与"有没有那一行"无关，是会话侧那一路的来源。
  const patch = (list: PeerDevice[]) => list.map((d) => (d.id === peerId ? { ...d, remark: stored } : d));
  devices = patch(devices);
  allDevices = patch(allDevices);
  const next = { ...remarks };
  if (stored) next[peerId] = stored;
  else delete next[peerId];
  remarks = next;
  return stored;
}

// ── 设备状态：一处判据，处处共用 ────────────────────────────────────────────
//
// 会话浮层（列表行 / 标题旁 / 收件人栏）、设备页（胶囊 + 点）、收件人选择框都画同一件事：
// "这台现在怎么样"。以前每处各算一套，聊天上线后立刻分叉 —— 会话说"灰点也能送到"、
// 设备页说"灰点=已配对"、离线也是灰点。现在只有这里的两个函数算，界面只负责画。

/**
 * 「本机在线 / 离线」（隐身）：面板头那颗胶囊切的就是它（落盘在 `config` 里，App 只做镜像）。
 *
 * 状态点要看它：隐身时后端 `ensure_online` 直接拒发（不广播 mDNS、断全部长连接、拒收入站），
 * 这时候还画绿点就是骗人 —— 一律按"送不到"（`offline`）画，面板头那颗胶囊会说明原因。
 */
let localOnline = $state(true);

export function isLocalOnline(): boolean {
  return localOnline;
}

/** 本机在线状态变了（启动读到落盘值 / 面板头切换 / 后端事件）——调用方把当前值报过来。 */
export function noteLocalOnline(on: boolean) {
  localOnline = on;
}

/**
 * 设备状态点的五档：**颜色说"现在送得到吗"，形状说"有没有实时链路"**。
 *
 * 只有 `live` 那一档填实心（并带一圈浅绿光环），其余都是空心环 —— 拆分这两条轴是聊天上线
 * 之后才必要的：消息走"长连接优先 + HTTP 兜底"，**能不能送到并不取决于长连接**，而以前
 * 灰点同时意味着"在线、能送到"和"离线、送不到"。
 *
 * - `live`：在线 + 长连接（消息即时到；共享项变更的自动广播也只发给长连接设备）；
 * - `online`：在线、没有长连接 —— 已配对的那个方向直接收，未绑定的要对方点一次同意。
 *   **在线 ≠ 此刻一定拨得动**：判据是"探活成功（本机够得着）**或**刚听到过它"（对端能拨进来、
 *   本机拨不回去 —— AP 隔开单播、跨段、只有一条借来的中转链时就是这样，见后端 `AppState::heard`）。
 *   本机这一侧没有路时消息会排队（`chat::deliver` 只认 `dialable()`），但"不在网上"这句是错的：
 *   它刚刚才跟本机说过话；
 * - `offline`：不在网上（**此刻送不到**：消息会排队等它上线，推送则就地失败 —— 见 F-95/F-53）；
 * - `retrying`：本机在等（拨号重连中 / 等对端上线）——"还没连上，但也没放弃"；
 * - `repair`：关系失效（对端不认本机 token / 证书换过），出路只有重新配对。
 */
export type DeviceDotState = 'live' | 'online' | 'offline' | 'retrying' | 'repair';

/**
 * 这台设备的关系是否已经失效（对端清过数据 = 不认本机的 token；或换了机器 = 证书指纹对不上）。
 *
 * 这一档**不能给「连接」**：拨号必然 401，点多少次都一样，唯一出路是重新配对。设备行、
 * 右键菜单、状态点共用这一个判据。
 */
export function needsRepair(d: PeerDevice): boolean {
  const s = d.linkState;
  return !!s && s.kind === 'stopped' && (s.reason === 'tokenRejected' || s.reason === 'identityChanged');
}

/**
 * 「看得见但连不上」的**唯一判据**（F-106）：mDNS 记录还在（对端在喊），可本机**手里没有
 * 一条拨得动的路** —— 有些 AP 把同一网段里客户端之间的单播隔开了（组播还过得去，于是
 * "设备看得见、连不上"）。
 *
 * **它只管一件事：发起连接那一刻说不说那句话**（`unicastBlockedTip`）。设备行本身不给这一档
 * —— 关系没建立就是「未绑定」，点照旧按 `offline` 画；理由等到用户真要拨号时才说，那时它才是
 * "这一次为什么连不上"的解释，而不是列表上一条要用户自己维护的标签。
 *
 * 判据两半，缺一不可：
 *
 * - **`seen && !relayVia && !online`**：mDNS 看得见它、探活却不成 —— 这正是这句 tip 写的
 *   那一档（AP 把单播隔开）。**这一半不能删**：mDNS 发现出来的行**一定带着记录里的地址**
 *   （`lib.rs` 的 `PeerEvent::Online`），所以"有没有地址"在这条路上一点区分度都没有。
 * - **`seen && !relayVia && 手里没有路`**：`online` 现在还能由"刚听到过它"撑起来（后端
 *   `AppState::heard`：对端能拨进来、本机拨不回去），那时 `!online` 不再蕴含"拨不动" ——
 *   一台"刚发来消息、但本机没有它的地址"的设备点「连接」会一句话都没有。补的这一半专治它。
 *
 * `relayVia` 不算：那一行此刻拨的是**借来的中转端点**，不是直连这一跳 —— "单播被隔开"那句
 * 就不适用。中转本身不上界面（用户要求），但这条判据照旧要看它。
 */
export function unicastBlocked(d: PeerDevice | undefined): boolean {
  // "此刻拨得动吗"：地址在手（或手里有一条借来的端点）。
  const hasRoute = !!d && ((d.ip !== '' && d.port !== 0) || !!d.relayVia);
  return !!d && d.seen && !d.relayVia && (!d.online || !hasRoute);
}

/**
 * 「发起连接」（配对 / 连接 / 立即连接）却过不去时那一句 tip；判据不成立时 `null` ——
 * 调用方照旧说它自己的话，别把这句话塞进每一条提示。
 */
export function unicastBlockedTip(d: PeerDevice | undefined): string | null {
  if (!d || !unicastBlocked(d)) return null;
  return `「${deviceShortName(d)}」直连不通：它在同一网段（能收到它的 mDNS 记录），但这张 Wi-Fi 可能把客户端之间的单播隔开了。要立刻连上它，得有一台两端都够得着的设备打开设置里的「允许为其它设备中转」。`;
}

/**
 * 一台设备的状态点档位。
 *
 * 顺序就是优先级：本机隐身 > 离线 > 长连接 > 需配对 > 重连中 > 在线。
 * 离线排在链路状态之前：对方都不在网上时，"重连中 / 需配对"说明不了任何事
 * （这也是设备行原来的判断顺序，别调换 —— 否则离线设备会挂着一条已经没意义的链路状态）。
 */
export function deviceDotState(d: PeerDevice | undefined): DeviceDotState {
  if (!localOnline || !d || !d.online) return 'offline';
  if (d.connected) return 'live';
  if (needsRepair(d)) return 'repair';
  if (d.linkState?.kind === 'retrying') return 'retrying';
  return 'online';
}

/** 档位的"好"次序：多台会话取最好的一档时用（`online` 比 `retrying` 好，`repair` 比 `offline` 好）。 */
const DOT_RANK: Record<DeviceDotState, number> = { live: 4, online: 3, retrying: 2, repair: 1, offline: 0 };

/**
 * 多台会话（群发 / 群聊）的状态点：取参与者里**最好**的那一档 —— 群发只要有一台收得到就有
 * 意义，所以"能送到"压过"送不到"；逐台的差异到设备页看（那里一行一台，还带悬停说明）。
 */
export function threadDotState(peerIds: string[]): DeviceDotState {
  let best: DeviceDotState = 'offline';
  for (const id of peerIds) {
    // 查两份表：可见那份里没有的行（离线 + 未绑定 + 没被 mDNS 看见）在后端原样那份里，
    // 而会话列表/群成员那一行照样要用它算状态 —— 只查 `devices` 的话，找不到 = 恒 `offline`，
    // 与后端真实状态无关（见 `deviceById`）。
    const state = deviceDotState(deviceById(id));
    if (DOT_RANK[state] > DOT_RANK[best]) best = state;
  }
  return best;
}

/**
 * 多台会话的信任档：**全部成员都验证过**才算 true。
 *
 * 群聊/群发的线程级状态点用它决定实心/空心；只要有一台未配对或指纹为空，就是空心
 * —— 那正是群表里要提醒的事。
 */
export function threadTrusted(peerIds: string[]): boolean {
  return peerIds.length > 0 && peerIds.every((id) => deviceState(deviceById(id)).trusted);
}

/** 一行设备的状态：点（`dot`）、胶囊档位（`chip`，`unbound` 自己一档）、那句话（`label`）
 *  与信任档（`trusted`：已配对且指纹已验证；状态点实心/空心就看它）。 */
export type DeviceState = {
  dot: DeviceDotState;
  chip: DeviceDotState | 'unbound' | 'offline';
  label: string;
  trusted: boolean;
};

/**
 * 设备行 / 收件人行 / 会话标题那一整套状态（桌面端=`DeviceTab` / `RecipientDialog` /
 * `ConvoPanel`，移动端=设备页 / 收件人表 / 配对表 / 会话头）。
 *
 * 状态点两维：
 * - **颜色 = 链路**：绿=长连接、蓝=在线、灰=离线、琥珀=重连中、红=需配对；
 * - **填充 = 信任**：实心=已配对且指纹已验证，空心=未配对/未验证。`repair` 永远空心。
 *
 * 长连接另有浅绿光晕（`main.css`），作为颜色之外的第二条线索；未配对是局域网设备的常态，
 * 胶囊用中性灰，不跟"要你处理"的暖色抢注意力。
 * 胶囊文案说关系与链路，与点不重复（见 `visibleDevices`）。
 */
export function deviceState(d: PeerDevice | undefined): DeviceState {
  const dot = deviceDotState(d);
  if (!d || !d.paired) {
    // 关系还没建立：点照旧按链路画（蓝环/灰环），但永远是空心。
    return { dot, chip: 'unbound', label: '未配对', trusted: false };
  }
  // 长连接本身已经证明过指纹，是最强的一档。
  if (d.connected) return { dot: 'live', chip: 'live', label: '长连接', trusted: true };
  // 已配对但指纹为空（老记录），或链路明确失效：关系需要重建，画空心红。
  // 提前把这一档露出来，别等下一次拨号失败才告诉用户。
  if (!d.verified || dot === 'repair') {
    return { dot: 'repair', chip: 'repair', label: '需配对', trusted: false };
  }
  if (dot === 'offline') return { dot, chip: 'offline', label: '离线', trusted: true };
  if (dot === 'retrying') return { dot, chip: 'retrying', label: '重连中', trusted: true };
  return { dot, chip: 'online', label: '已配对', trusted: true };
}

export function addDevice(d: PeerDevice) {
  devices = [...devices, d];
}

// Watch for live peer state changes from the Rust backend.
const initListeners: () => Promise<void> = (() => {
  // Return a cleanup function (called once on module init).
  const unlisteners: Promise<() => void>[] = [];

  unlisteners.push(
    listen<{ peerId: string }>('devices://changed', () => {
      loadDevices().catch(console.error);
    })
  );
  unlisteners.push(
    listen<{ peerId: string; name: string }>('peer://online', () => {
      loadDevices().catch(console.error);
    })
  );
  unlisteners.push(
    listen<{ peerId: string }>('peer://offline', () => {
      loadDevices().catch(console.error);
    })
  );

  return async () => {
    for (const u of unlisteners) {
      (await u)();
    }
  };
})();

export function cleanup() {
  void initListeners();
}

// Export event channel for use by other stores / components.
export const eventChannels = {
  listen: listen,
};