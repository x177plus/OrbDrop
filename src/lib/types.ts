export interface ShareItem {
  id: string;
  name: string;
  path: string;
  createdAt: string;
  broadcastEnabled: boolean;
  /** 真元数据（后端 `ShareItemDto.isDir`），不再按路径猜。 */
  isDir: boolean;
  /** 文件大小；目录是 null。 */
  size: number | null;
  /** 目录的**直接**子项数；文件是 null。 */
  itemCount: number | null;
}

/** 对端设备标签里看到的共享项：只有展示所需的字段（没有对端本机绝对路径）。 */
export interface PeerShareItem {
  id: string;
  name: string;
  createdAt: string;
  /** 目录才能展开。 */
  isDir: boolean;
  /** 老版本对端不带这两个字段（那时是 `undefined`）。 */
  size?: number | null;
  itemCount?: number | null;
}

/** 没连着的时候"为什么"（后端 `LinkState`）。`connected` 为真时忽略它。 */
export type LinkState =
  | { kind: 'connected' }
  | { kind: 'retrying' }
  /** 对端此刻不在网上：等它出现（不再空发 TCP）。 */
  | { kind: 'waiting' }
  | { kind: 'stopped'; reason: 'peerDisconnected' | 'tokenRejected' | 'identityChanged' | 'localDisconnected' };

export interface PeerDevice {
  id: string;
  name: string;
  ip: string;
  port: number;
  online: boolean;
  paired: boolean;
  /** WS long connection is live (paired only) — this is what "长连接" means. */
  connected: boolean;
  /** 自动重连的现状；缺省 = 没有拨号意图（已配对但没人点过「连接」）。 */
  linkState?: LinkState | null;
}

export interface AppSettings {
  port: number;
  /** 唯一的落盘目录：手动下载（受 downloadMode 影响）与推送接收都写这里。 */
  downloadDir: string;
  downloadMode: 'Direct' | 'AskEachTime';
  deviceName: string;
  autoStart: boolean;
  /** BeyondCompare 可执行文件路径；'' = 自动探测。 */
  bcExePath: string;
  /**
   * 本机「在线 / 离线」（面板右上角状态框）。`false` = 隐身：不广播 mDNS、断开全部
   * 长连接、拒收一切入站请求。落盘、跨重启有效。
   */
  online: boolean;
  /**
   * 折叠态悬浮形态：`circle` = 62px 圆球（`Orb.svelte`）；`rect` = 圆角矩形悬浮框
   * （`RectOrb.svelte`，`design/orbdrop-rect-orb.html`）。落盘、跨重启有效。
   */
  orbShape: OrbShape;
  /**
   * 新添加的目录/文件是否**默认开播**（设置里的「添加后自动共享」，默认 `false`）。
   *
   * 只作用于之后新登记的共享项：后端 `add_share`（`add_share_impl`）建条目时把这个值抄进
   * `broadcastEnabled` 的初值，已经在列表里的开关一个都不动。
   */
  autoBroadcastNew: boolean;
}

/** 折叠态悬浮形态：圆球 / 圆角矩形悬浮框（后端 `config::OrbShape` 的同一套小写字面量）。 */
export type OrbShape = 'circle' | 'rect';

/**
 * BeyondCompare 比较的一端：本机共享项 / 长连接设备的共享项。
 * `path` 是共享项内相对路径（'' = 共享项根）。
 * `label` 是给人看的名字，由前端填（对端共享项的名字只有前端知道）；后端优先采用它。
 */
export type BcTarget =
  | { kind: 'share'; shareId: string; path: string; label?: string }
  | { kind: 'peer'; peerId: string; shareId: string; path: string; label?: string };

/** 比较位上的项。 */
export interface BcSide {
  target: BcTarget;
  label: string;
}

/** 后端返回的 BeyondCompare 状态：左/右比较位为空时对应字段为 null。 */
export interface BcStatus {
  exePath: string | null;
  left: BcSide | null;
  right: BcSide | null;
}

export interface TreeEntry {
  name: string;
  isDir: boolean;
  size: number;
  modified: string;
}

export interface TreeResponse {
  entries: TreeEntry[];
}

/** 选择框顶部「常用」的一条（配置里的 `pickerFavorites`）：用户右键目录攒的快捷入口。 */
export interface PickerFavorite {
  name: string;
  path: string;
}

/** 「设备与驱动器」里的一项（后端 `FsDrive`）。 */
export interface FsDrive {
  name: string;
  path: string;
  /** 可移动磁盘（U 盘、移动硬盘）。 */
  removable: boolean;
  /** 总容量 / 可用空间（字节）。取不到时为 0，界面据此不画占用条。 */
  total: number;
  free: number;
}

export interface FsEntry {
  name: string;
  path: string;
  isDir: boolean;
  /** 文件夹固定 0：列一层不读它的子项。 */
  size: number;
}

/** 面包屑的一段：`path` 是点它要跳到的绝对路径。 */
export interface FsCrumb {
  name: string;
  path: string;
}

/** 一层目录的内容（后端 `fs_list`）。遍历一律走 `crumbs` + `entries` 的 `path`，前端不拼路径。 */
export interface FsListing {
  path: string;
  crumbs: FsCrumb[];
  entries: FsEntry[];
}

/** 列举失败（后端 `FsError`）：`kind` 选状态页，`crumbs` 保证失败时面包屑也是对的。 */
export interface FsError {
  kind: 'denied' | 'missing' | 'notDir' | 'other';
  message: string;
  crumbs: FsCrumb[];
}

/** 选择框里选中并交给调用方的一项（推送直接拿 `path` 拼一批路径）。 */
export interface LocalPick {
  name: string;
  path: string;
  isDir: boolean;
  size: number;
}

/**
 * 拖拽下载的源（F-46）：本机共享项（省略 `peerId`）或对端共享项。
 * `path` 是共享项内相对路径（'' = 共享项根），`name` 是拖出去那一项的名字 ——
 * 对端共享项根的名字本机不知道（只有 id），只能由界面带下来。
 */
export interface DragOutSource {
  peerId?: string;
  shareId: string;
  path: string;
  name: string;
  isDir: boolean;
}

/** 拖拽票据：`ready` = false 表示后端还在准备对端项（走目录，必要时先下到暂存目录）。 */
export interface DragTicket {
  ticket: string;
  name: string;
  ready: boolean;
}

export interface PushInvite {
  inviteId: string;
  sender: { id: string; name: string };
  name: string;
  size: number;
  isDir: boolean;
  /** 发送方这次选了几项（>1 = 多选合并成的一条推送）。 */
  count: number;
  /**
   * 后端已经**自动收下**了这条推送（发送方是本机已绑定设备：token 就是那份信任，
   * 不再问用户一次）。界面据此直接显示"正在接收"，不放「接收/拒绝」。
   */
  auto?: boolean;
}

/** 一台目标设备在一批推送里的状态（后端 `TargetState`）。 */
export type PushTargetState = 'invited' | 'sending' | 'done' | 'cancelled' | 'rejected' | 'failed';

/** 一批推送里的一台目标设备（卡片上的一行，后端 `BatchTargetView`）。 */
export interface PushTarget {
  peerId: string;
  /** 设备名。 */
  target: string;
  inviteId: string;
  /** 已配对 = 对方不问同意直接收：行文案别说「等待对方同意」。 */
  paired: boolean;
  state: PushTargetState;
  received: number;
  error: string | null;
}

/**
 * 一次推送操作（= 界面上一张卡片，后端 `BatchView`）。单台推送也是一批（只有一条目标），
 * 所以"等待对方同意 → 传输中 → 已发送"和"3 台里成了几台"是同一条数据。
 */
export interface PushBatch {
  batchId: string;
  /** 清单的显示名（"推的是什么"），不参与落盘路径。 */
  name: string;
  /** 这次选了几项（>1 = 多选合并成的一条推送）。 */
  count: number;
  /** 清单里被跳过的项数（链接 / 读不到的项）。>0 时卡片要说出来，别再让用户以为"就这些"。 */
  skipped: number;
  /** 成功 / 等待（含传输中）/ 失败（含被拒、被取消）—— 卡片上「成功 x · 等待 y」那行就是它。 */
  done: number;
  waiting: number;
  failed: number;
  /** 聚合进度：分母只算还活着的台（失败/取消的整条不计），所以有台失败也能走到 100%。 */
  received: number;
  total: number;
  targets: PushTarget[];
}

/**
 * 会话（聊天）里一条消息在一台设备上的状态（后端 `chat::Delivery`）。
 *
 * `sent` = 已写进那条长连接；`read` = 对端回执说它读过了；`failed` = 发出去的时候对端没有
 * 长连接（**不排队重发**：会话里的话过期补发没有意义）；`received` = 收到的消息 ——
 * 本机视角没有"送达"可言。
 */
export type ChatDelivery = 'sent' | 'read' | 'failed' | 'received';

/**
 * 会话里的一条文本消息。
 *
 * 群发时**每台设备各存一份副本、共用同一个 `id`**（各自落在与该设备的 1:1 会话里）：
 * "送到了几台"就是对着 `targets` 逐台看副本的 `delivery`。
 */
export interface ChatMessage {
  id: string;
  /** 本机发出的 = true（气泡靠右）；对端发来的 = false（靠左，`fromName` 说明是谁说的）。 */
  fromMe: boolean;
  /** 对端消息的发送方名字；本机发出的为空串。 */
  fromName: string;
  text: string;
  /** 发送方的墙上时间（epoch 毫秒）：排序与已读水位都按它。 */
  at: number;
  delivery: ChatDelivery;
  targets: string[];
}

/** 一台设备的会话：会话就是**与这台设备**的那一条（没有群实体，群发只是每台各存一份）。 */
export interface ChatPeerConvo {
  peerId: string;
  messages: ChatMessage[];
  /** 本机已读水位（epoch 毫秒）：`at <= readAt` 的对端消息算读过。 */
  readAt: number;
  unread: number;
}

/** 整份会话状态：`chat_snapshot` 命令的返回，也是 `chat://changed` 事件的载荷。 */
export interface ChatSnapshot {
  convos: ChatPeerConvo[];
  /** 全部未读（底栏那颗角标 / 悬浮球左上角那颗）。 */
  unread: number;
}

/**
 * 悬浮球起拖那一刻的按下点（CSS px，窗口坐标 = 折叠态窗口的视口坐标）。
 *
 * `App.beginOrbDrag` 把它交给后端：后端按它把窗口的抓取锚点校正回按下点，再当场起拖。
 */
export interface OrbDragInfo {
  /** 按下时的指针位置 */
  pressX: number;
  pressY: number;
}

/** 圆角矩形悬浮框里的一格：一台设备的共享项（或钻入子目录后的一个子项）在卡上的样子。 */
export interface RectSlot {
  /** 共享项 id：拖出（`DragOutSource.shareId`）与打开（`openPath` / `download_from_peer`）都用它定位。 */
  id: string;
  /** 共享项内的相对路径（`''` = 共享项根）：拖出与打开都按「共享项 + 相对路径」定位。 */
  path: string;
  name: string;
  isDir: boolean;
  /** 第二行的等宽小字（卡上只放得下很短的一段）。 */
  meta: string;
}

/**
 * 列表头上那枚「↰ 上一级」（只有钻进了子目录才在）：双击 = 回上一层，名字是**父目录**
 * 的名字（回根那一层时是共享项自己的名字）。
 */
export interface RectUp {
  name: string;
  /** 第二行小字，固定说清这一格是干什么的。 */
  meta: string;
}

/**
 * 圆角矩形悬浮框的一页：**第一页永远是本机**，其后每台长连接设备一页。
 *
 * 设备页只有长连接设备才有：对端的共享项**只有在本机与它保持长连接时才对本机可见**
 * （F-25 的广播范围），离线/未配对的设备根本没有可列的东西。本机页则一直在，
 * 内容就是「本机」标签页那份共享项清单。
 */
export interface RectPage {
  /** 稳定 key：本机页 = `'local'`，设备页 = peerId。 */
  id: string;
  /** 这一页是谁：本机的共享项 / 一台设备对本机可见的共享项。 */
  kind: 'local' | 'peer';
  /** 头部第一行。本机页固定是「本机」（不写设置里那个设备名，见 `App.rectPages` 的说明）。 */
  name: string;
  /** `ip:port`：卡片头部第二行的左半（本机页是自己那个地址；取不到时为空）。 */
  ip: string;
  /** 这一页还在拉取中（设备页首屏那一瞬）。 */
  loading: boolean;
  /** 拉取失败的原因（未配对 / 对端不可达）：卡片里说清楚，而不是显示成一个空列表。 */
  error: string | null;
  /**
   * 钻入子目录后**现在在哪**：`label` 是给 176px 宽的卡片量过的显示串（长了从头上砍，
   * 尾巴才是"我在哪"），`full` 给悬停 `title`。`null` = 这一页在看共享项清单，
   * 头部第二行照旧说 `ip:port`。
   */
  dir: { label: string; full: string } | null;
  /** 「↰ 上一级」那一格（`null` = 没钻入）。**不算进 `items`**：项数说的是"这里有几项"。 */
  up: RectUp | null;
  items: RectSlot[];
}

/** 一级标签栏的标签：本机 / 设备（关系管理） / 每台长连接设备一个浏览标签（F-24、F-30）。 */
export type TabKey =
  | { kind: 'local' }
  | { kind: 'devices' }
  | { kind: 'peer'; peerId: string };

export function tabKeyEquals(a: TabKey, b: TabKey): boolean {
  if (a.kind !== b.kind) return false;
  return a.kind === 'peer' && b.kind === 'peer' ? a.peerId === b.peerId : true;
}

export function tabKeyId(tab: TabKey): string {
  return tab.kind === 'peer' ? `peer:${tab.peerId}` : tab.kind;
}

/**
 * 共享项第二行（卡片格、「本机」/设备标签页）那一段小字：目录给「N 项」，文件给大小。
 *
 * 两个数都由后端问一次文件系统得来（本机 `ShareItemDto`、对端 `/shares`），拿不到时是
 * `null` —— 那时只说「文件夹 / 文件」：**宁可不说，也不报一个没根据的数字**（上一版这里显示
 * 的是共享项的登记日期，连对端看到的都是"别人哪天加进来的"）。
 *
 * 本机共享项与对端共享项共用这一份：同一批共享项不该在两处长得不一样。
 */
export function shareMeta(
  isDir: boolean,
  itemCount: number | null | undefined,
  size: number | null | undefined,
): string {
  if (isDir) return typeof itemCount === 'number' ? `${itemCount} 项` : '文件夹';
  return typeof size === 'number' ? formatFileSize(size) : '文件';
}

export function formatFileSize(bytes: number): string {
  if (bytes <= 0) return '0 B';
  const units = ['B', 'KB', 'MB', 'GB', 'TB'];
  const i = Math.min(units.length - 1, Math.floor(Math.log(bytes) / Math.log(1024)));
  return (bytes / Math.pow(1024, i)).toFixed(i > 0 ? 1 : 0) + ' ' + units[i];
}

export function formatTime(iso: string): string {
  try {
    return new Date(iso).toLocaleString();
  } catch { return iso; }
}