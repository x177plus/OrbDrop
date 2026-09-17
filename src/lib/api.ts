import { invoke } from '@tauri-apps/api/core';
import type { ShareItem, AppSettings, PeerDevice, PeerShareItem, TreeEntry, PushInvite, BcTarget, BcStatus, DragOutSource, DragTicket } from './types';
import type { ScreenMetrics, ScreenRect } from './orbLayout';

// Share management
export function addShare(path: string): Promise<ShareItem> {
  return invoke('add_share', { path });
}

export function listShares(): Promise<ShareItem[]> {
  return invoke('list_shares');
}

export function removeShare(shareId: string): Promise<void> {
  return invoke('remove_share', { shareId });
}

export function renameShare(shareId: string, name: string): Promise<void> {
  return invoke('rename_share', { shareId, name });
}

export function toggleBroadcast(shareId: string, enabled: boolean): Promise<void> {
  return invoke('toggle_broadcast', {
    req: { shareId, enabled },
  });
}

// Settings
export function getSettings(): Promise<AppSettings> {
  return invoke('get_settings');
}

export function updateSettings(s: Partial<AppSettings> & { port: number; downloadDir: string; downloadMode: string; deviceName: string; autoStart: boolean; bcExePath: string }): Promise<AppSettings> {
  return invoke('update_settings', { req: s });
}

// Server info
export function getActualPort(): Promise<number> {
  return invoke('get_actual_port');
}

export function listDevices(): Promise<PeerDevice[]> {
  return invoke('list_devices');
}

export function startHttpServer(): Promise<number> {
  return invoke('start_http_server');
}

// Peer management
export function addPeer(ip: string, port: number): Promise<PeerDevice> {
  return invoke('add_peer', { ip, port });
}

export function removePeer(peerId: string): Promise<void> {
  return invoke('remove_peer', { peerId });
}

export function getPeerToken(peerId: string): Promise<string | null> {
  return invoke('get_peer_token', { peerId });
}

/** 一键清空本机全部配对凭据（含 BC 里写下的 WebDAV profile），返回清掉的设备数。 */
export function wipeCredentials(): Promise<number> {
  return invoke('wipe_credentials');
}

// Long connection (F-16). Only the user opens it — pairing just establishes the
// 「已配对」 relationship — these two are the manual 断开 / 连接 buttons.
export function connectPeer(peerId: string): Promise<void> {
  return invoke('connect_peer', { peerId });
}

export function disconnectPeer(peerId: string): Promise<void> {
  return invoke('disconnect_peer', { peerId });
}

// Pairing (F-12)
export function pairStart(peerId: string): Promise<string> {
  return invoke('pair_start', { peerId });
}

export function pairConfirm(peerId: string, code: string): Promise<string> {
  return invoke('pair_confirm', { peerId, code });
}

// Push (F-50 ~ F-54)
export function pushInvite(shareId: string, path: string, peerId: string): Promise<string> {
  return invoke('push_invite', { shareId, path, peerId });
}

export function acceptPush(inviteId: string): Promise<void> {
  return invoke('accept_push', { inviteId });
}

export function rejectPush(inviteId: string): Promise<void> {
  return invoke('reject_push', { inviteId });
}

export function cancelPush(inviteId: string): Promise<void> {
  return invoke('cancel_push', { inviteId });
}

export function cancelReceive(inviteId: string): Promise<void> {
  return invoke('cancel_receive', { inviteId });
}

export function pushJobStatus(inviteId: string): Promise<string | null> {
  return invoke('push_job_status', { inviteId });
}

// BeyondCompare 比较（本机 / 长连接设备的共享项）。四个命令都返回最新 BcStatus，
// 只有 bc_compare 返回 null（启动失败时 reject，错误为字符串）。
export function bcStatus(): Promise<BcStatus> {
  return invoke('bc_status');
}

export function bcSetSide(side: 'left' | 'right', target: BcTarget): Promise<BcStatus> {
  return invoke('bc_set_side', { side, target });
}

export function bcClearSide(side: 'left' | 'right'): Promise<BcStatus> {
  return invoke('bc_clear_side', { side });
}

export function bcCompare(a: BcTarget, b: BcTarget): Promise<null> {
  return invoke('bc_compare', { a, b });
}

// Download
export function downloadFile(shareId: string, pathInShare: string): Promise<string> {
  return invoke('download_file', { shareId, pathInShare });
}

export function downloadFromPeer(peerId: string, shareId: string, pathInShare: string, fileName?: string): Promise<string> {
  return invoke('download_from_peer', { peerId, shareId, pathInShare, fileName: fileName ?? null });
}

export function downloadAndOpen(shareId: string, pathInShare: string): Promise<string> {
  return invoke('download_and_open', { shareId, pathInShare });
}

// 拖拽下载（F-46）：把树里的任意文件/目录拖到资源管理器里，落在拖到的那个目录。
// 分两步是因为对端项要先下到暂存目录：prepare 登记并开始下，start 等备好后起拖。
export function prepareDragOut(src: DragOutSource): Promise<DragTicket> {
  return invoke('prepare_drag_out', {
    req: {
      peerId: src.peerId ?? null,
      shareId: src.shareId,
      path: src.path,
      name: src.name,
      isDir: src.isDir,
    },
  });
}

export function startDragOut(ticket: string): Promise<void> {
  return invoke('start_drag_out', { ticket });
}

// 悬浮球窗口几何（展开方向 / 夹取）
export function orbScreenMetrics(): Promise<ScreenMetrics> {
  return invoke('orb_screen_metrics');
}

export function orbSetRect(rect: ScreenRect): Promise<void> {
  return invoke('orb_set_rect', { x: rect.x, y: rect.y, w: rect.w, h: rect.h });
}

/** 鼠标主键是否仍按着：拖动悬浮球期间页面收不到 mouseup，贴边落位靠它等松手 */
export function orbMouseDown(): Promise<boolean> {
  return invoke('orb_mouse_down');
}

/** 记录悬浮球落位（折叠态窗口左上角，物理像素）：下次启动复原到同一位置 */
export function orbSavePos(x: number, y: number): Promise<void> {
  return invoke('orb_save_pos', { x, y });
}

// HTTP-based remote browsing (P1: against a peer's server with Bearer token)
export async function fetchPeerShares(baseUrl: string, token: string): Promise<PeerShareItem[]> {
  const resp = await fetch(`${baseUrl}/shares`, {
    headers: { Authorization: `Bearer ${token}` },
  });
  if (!resp.ok) throw new Error(`HTTP ${resp.status}`);
  const data = await resp.json();
  return data.shares;
}

export async function fetchPeerTree(baseUrl: string, token: string, shareId: string, path: string): Promise<TreeEntry[]> {
  const url = `${baseUrl}/tree?share=${encodeURIComponent(shareId)}&path=${encodeURIComponent(path)}`;
  const resp = await fetch(url, {
    headers: { Authorization: `Bearer ${token}` },
  });
  if (!resp.ok) throw new Error(`HTTP ${resp.status}: ${await resp.text()}`);
  const data = await resp.json();
  return data.entries;
}

// Local HTTP tree (P0 self-validation path)
export async function fetchTree(port: number, shareId: string, path: string): Promise<TreeEntry[]> {
  const url = `http://127.0.0.1:${port}/tree?share=${encodeURIComponent(shareId)}&path=${encodeURIComponent(path)}`;
  const resp = await fetch(url);
  if (!resp.ok) throw new Error(`HTTP ${resp.status}: ${await resp.text()}`);
  const data = await resp.json();
  return data.entries;
}