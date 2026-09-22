import { invoke } from '@tauri-apps/api/core';
import type { ShareItem, AppSettings, PeerDevice, PeerShareItem, TreeEntry, PushInvite, PushBatch, BcTarget, BcStatus, DragOutSource, DragTicket, FsDrive, FsListing, PickerFavorite } from './types';
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

export function updateSettings(s: Partial<AppSettings> & { port: number; downloadDir: string; downloadMode: string; deviceName: string; autoStart: boolean; bcExePath: string; autoBroadcastNew: boolean }): Promise<AppSettings> {
  return invoke('update_settings', { req: s });
}

// Server info
export function getActualPort(): Promise<number> {
  return invoke('get_actual_port');
}

/**
 * 本机内网 IPv4 地址（左下角「IP:端口」用）。`primary` 是界面显示的那一个，
 * `all` 含其余几块网卡的地址 —— 多网卡时悬停提示要把它列全。
 */
export function localAddresses(): Promise<{ primary: string | null; all: string[] }> {
  return invoke('local_addresses');
}

export function listDevices(): Promise<PeerDevice[]> {
  return invoke('list_devices');
}

export function startHttpServer(): Promise<number> {
  return invoke('start_http_server');
}

/**
 * 本机「在线 / 离线」（面板右上角状态框）：`false` = 隐身。
 *
 * 离线 = 不广播 mDNS + 断开全部长连接 + 拒收一切入站请求（对端的探活因此判本机离线）；
 * 配对关系、token、共享项、地址簿里的长连接记忆都保留，切回在线即恢复广播并接回链路。
 * 跨重启有效（写在配置里）。返回落地后的值。
 */
export function setOnline(online: boolean): Promise<boolean> {
  return invoke('set_online', { online });
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

/**
 * 发起配对邀请（F-12 免配对码）：对端在它自己的通知里点「接收」即可配对，谁都不用输码。
 * 返回邀请 id；对方接受后走 `pair://completed`，拒绝/超时/撤回走 `pair://invite_failed`。
 */
export function pairInvite(peerId: string): Promise<string> {
  return invoke('pair_invite', { peerId });
}

/** 撤回一条还没被处理的配对邀请（对端界面上那条通知跟着撤掉）。 */
export function cancelPairInvite(inviteId: string): Promise<void> {
  return invoke('cancel_pair_invite', { inviteId });
}

/** 接收方：接受一条配对邀请（通知里的「接收」），返回对方设备名。 */
export function acceptPairInvite(inviteId: string): Promise<string> {
  return invoke('accept_pair_invite', { inviteId });
}

/** 接收方：拒绝一条配对邀请。 */
export function rejectPairInvite(inviteId: string): Promise<void> {
  return invoke('reject_pair_invite', { inviteId });
}

// 本机文件浏览（推送选择框 `FilePicker.svelte` 用）：
// 自绘选择框的存在理由见 `src-tauri/src/fs_browse.rs` —— 系统原生选择框不能把
// "选文件"和"选目录"合进同一个对话框，而推送要的正是两者混选。
/** 选择框「常用」：随配置持久化（`config.toml`），重启后还在。默认是空的。 */
export function listPickerFavorites(): Promise<PickerFavorite[]> {
  return invoke('list_picker_favorites');
}

/** 整份写回；返回后端规整过的那份（去重后），界面以后端为准。 */
export function setPickerFavorites(items: PickerFavorite[]): Promise<PickerFavorite[]> {
  return invoke('set_picker_favorites', { items });
}

/** 设备与驱动器（「此电脑」下面那一组）。「常用」默认是空的，全靠右键加。 */
export function fsDrives(): Promise<FsDrive[]> {
  return invoke('fs_drives');
}

/** 列一层目录。失败时 reject 的是整个 `FsError`（按 `kind` 选状态页）。 */
export function fsList(path: string): Promise<FsListing> {
  return invoke('fs_list', { path });
}

// Push (F-50 ~ F-54)
/**
 * 推送本机一批文件/目录给**一台或多台**设备；返回整批快照。
 * 一次操作只开一次文件选择框：多选的文件合并成**每台一条**推送（对端只弹一张卡、只同意一次），
 * 各台独立推进 —— 谁成谁败、还差几台，看 `push://batch` 事件推进的那张卡片。
 */
export function pushPaths(paths: string[], peerIds: string[]): Promise<PushBatch> {
  return invoke('push_paths', { paths, peerIds });
}

/** 发送方取消一整批推送（各台各撤一条）。 */
export function cancelPushBatch(batchId: string): Promise<void> {
  return invoke('cancel_push_batch', { batchId });
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

/** 用系统默认方式打开一个路径：目录就落在文件管理器里（收完推送那条通知的「打开目录」）。
 *  路径不在了会 reject —— 调用方据此把通知留住。 */
export function openPath(path: string): Promise<void> {
  return invoke('open_path', { path });
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

/**
 * 开始拖动悬浮球窗口：先按按下点校正抓取锚点，再当场把拖动交给 OS。
 *
 * `pressX/pressY` 是按下点在窗口里的位置（CSS px）。校正与起拖必须在同一瞬间完成 ——
 * OS 是在起拖那一刻把窗口钉在指针上的，中间多一次往返，指针就又走了一段（见
 * `App.beginOrbDrag`）。
 */
export function orbDragStart(pressX: number, pressY: number): Promise<void> {
  return invoke('orb_drag_start', { pressX, pressY });
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