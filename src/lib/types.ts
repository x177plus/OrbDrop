export interface ShareItem {
  id: string;
  name: string;
  path: string;
  createdAt: string;
  broadcastEnabled: boolean;
}

/** 对端设备标签里看到的共享项：只有展示所需的字段（没有对端本机绝对路径）。 */
export interface PeerShareItem {
  id: string;
  name: string;
  createdAt: string;
  /** 目录才能展开。 */
  isDir: boolean;
}

/** 没连着的时候"为什么"（后端 `LinkState`）。`connected` 为真时忽略它。 */
export type LinkState =
  | { kind: 'connected' }
  | { kind: 'retrying' }
  /** 对端此刻不在网上：等它出现（不再空发 TCP）。 */
  | { kind: 'waiting' }
  | { kind: 'stopped'; reason: 'peerDisconnected' | 'tokenRejected' | 'localDisconnected' };

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
}

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

/** 拖拽票据：`ready` = false 表示后端还在把对端内容下到暂存目录。 */
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

export function formatFileSize(bytes: number): string {
  if (bytes === 0) return '0 B';
  const units = ['B', 'KB', 'MB', 'GB', 'TB'];
  const i = Math.floor(Math.log(bytes) / Math.log(1024));
  return (bytes / Math.pow(1024, i)).toFixed(i > 0 ? 1 : 0) + ' ' + units[i];
}

export function formatTime(iso: string): string {
  try {
    return new Date(iso).toLocaleString();
  } catch { return iso; }
}