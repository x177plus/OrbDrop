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

export interface PeerDevice {
  id: string;
  name: string;
  ip: string;
  port: number;
  online: boolean;
  paired: boolean;
  /** WS long connection is live (paired only) — this is what "长连接" means. */
  connected: boolean;
}

export interface AppSettings {
  port: number;
  downloadDir: string;
  downloadMode: 'Direct' | 'AskEachTime';
  inboxDir: string;
  deviceName: string;
  autoStart: boolean;
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

export interface PushInvite {
  inviteId: string;
  sender: { id: string; name: string };
  name: string;
  size: number;
  isDir: boolean;
}

export interface PushProgress {
  inviteId: string;
  received: number;
  total: number;
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