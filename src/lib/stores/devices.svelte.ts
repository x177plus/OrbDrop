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
    const all = await api.listDevices();
    allDevices = all;
    devices = visibleDevices(all);
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
 * 按设备 id 取**短名**（拿不到这台设备时用 `fallback`，没给就退回 id 本身）。
 *
 * `fallback` 是给"名字来自别处"的场合用的：会话消息、推送卡与通知里的名字是**收到那一刻**
 * 存下来的字符串，设备表里可能已经没有这一台（移除过、或还没学到它），那时至少要有名字可显示。
 *
 * **设备页那份列表里没有的这台，再去后端原样那份里找一遍**（`allDevices`）：解绑又离线的设备
 * 会被 `visibleDevices` 挡在设备页之外，可跟它的会话还在（记录是用户数据，见 `stores/chat`
 * 的 `convoRows`）—— 会话列表与标题要拿它的名字，而不是一串设备 id。
 */
export function peerShortName(peerId: string, fallback?: string): string {
  const d = devices.find((x) => x.id === peerId) ?? allDevices.find((x) => x.id === peerId);
  return d ? deviceShortName(d) : fallback ?? peerId;
}

/**
 * 写一台设备的备注（F-101）：`''` = 清除。返回后端真正存下去的值（`null` = 已清除）。
 *
 * 本地**当场**改一份（右键点完立刻能看到新显示名），后端随后广播的 `devices://changed`
 * 会再拉一次完整列表（两边同值，谁先到都不影响）。
 */
export async function saveDeviceRemark(peerId: string, remark: string): Promise<string | null> {
  const stored = (await api.setDeviceRemark(peerId, remark)) ?? null;
  devices = devices.map((d) => (d.id === peerId ? { ...d, remark: stored } : d));
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
 * - `online`：在线、没有长连接 —— **能送到**（走 HTTP 兜底；已配对的对端直接收，未绑定的
 *   要对方点一次同意）；
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
 * 「看得见但连不上」的**唯一判据**（F-106）：mDNS 记录还在（对端在喊），探活却不成 ——
 * 有些 AP 把同一网段里客户端之间的单播隔开了（组播还过得去，于是"设备看得见、连不上"）。
 *
 * **它只管一件事：发起连接那一刻说不说那句话**（`unicastBlockedTip`）。设备行本身不给这一档
 * —— 关系没建立就是「未绑定」，点照旧按 `offline` 画；理由等到用户真要拨号时才说，那时它才是
 * "这一次为什么连不上"的解释，而不是列表上一条要用户自己维护的标签。
 *
 * 经中转（`relayVia`）不算：那条路已经绕开直连这一跳，此刻拨的是中转端点。
 */
export function unicastBlocked(d: PeerDevice | undefined): boolean {
  return !!d && d.seen && !d.online && !d.relayVia;
}

/**
 * 「发起连接」（配对 / 连接 / 立即连接）却过不去时那一句 tip；判据不成立时 `null` ——
 * 调用方照旧说它自己的话，别把这句话塞进每一条提示。
 */
export function unicastBlockedTip(d: PeerDevice | undefined): string | null {
  if (!d || !unicastBlocked(d)) return null;
  return `「${deviceShortName(d)}」直连不通：它在同一网段（能收到它的 mDNS 记录），但这张 Wi-Fi 可能把客户端之间的单播隔开了。要立刻连上它，得有一台两端都够得着的设备打开设置里的「允许为其它设备中转」——由它接管之后这一行会写「经 X 中转」。`;
}

/**
 * F-107：这台设备**所在的网段**有没有中转记忆 —— 有就返回一句悬停说明，没有就 `null`。
 *
 * 与 `relayVia`（此刻经 X 中转）是两件事：那是"现在这条路"，这是"这一段直连不通时该走哪一段"
 * 的**持久策略**（后端 `config.toml` 的 `relayRoutes`）。所以它跟 `unicastBlocked` 同一档：
 * 只在悬停说明里说话，**不是**设备行上的状态档，也不参与排序。
 */
export function transitHint(d: PeerDevice | undefined): string | null {
  const t = d?.transit;
  if (!t) return null;
  return `这一段有中转记忆：直连不通时走 ${t.viaCidr} 那一跳（池里 ${t.poolSize} 台，掉线会自动换一台）；直连一通就自动回到直连。`;
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
    const state = deviceDotState(devices.find((d) => d.id === id));
    if (DOT_RANK[state] > DOT_RANK[best]) best = state;
  }
  return best;
}

/** 一行设备的状态：点（`dot`）、胶囊档位（`chip`，`unbound` 自己一档）与那句话（`label`）。 */
export type DeviceState = { dot: DeviceDotState; chip: DeviceDotState | 'unbound' | 'offline'; label: string };

/**
 * 设备行 / 收件人行那一整套状态（桌面端=`DeviceTab` / `RecipientDialog`，
 * 移动端=设备页 / 收件人表 / 配对表，都画同一枚胶囊）。
 *
 * **会话标题旁那枚状态点不在这里**：那里只画点（`deviceDotState`），不挂胶囊 ——
 * 标题旁再挂一枚就是并排两颗点（见 `ConvoPanel.svelte` 标题那一段的注释）。
 *
 * 胶囊的配色跟**点**走（绿=长连接、蓝=已配对在线、在等=琥珀、要重新配对=红、送不到=灰虚框），
 * 文案说关系与链路 —— 一行里"点"和"胶囊"于是不会互相打架。**绿只给长连接**：自动广播的对象
 * 就是它，而"实心绿 vs 绿环"在 6px 的点上第一眼分不出来（要能扫一眼看出哪几台收得到广播）。
 * **例外是「未绑定」**：那台设备确实能收到东西（点照常给环），但关系还没建立，胶囊保持中性灰，
 * 别让它跟"要你处理"的琥珀抢注意力（见 `visibleDevices` 那条注释：未绑定是局域网设备的常态）。
 */
export function deviceState(d: PeerDevice | undefined): DeviceState {
  const dot = deviceDotState(d);
  if (!d || !d.paired) return { dot, chip: 'unbound', label: '未绑定' };
  if (dot === 'offline') return { dot, chip: 'offline', label: '离线' };
  if (dot === 'live') return { dot, chip: 'live', label: '长连接' };
  if (dot === 'repair') return { dot, chip: 'repair', label: '需配对' };
  if (dot === 'retrying') return { dot, chip: 'retrying', label: '重连中' };
  return { dot, chip: 'online', label: '已配对' };
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