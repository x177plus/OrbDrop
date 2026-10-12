import { invoke } from '@tauri-apps/api/core';
import type { ShareItem, AppSettings, PeerDevice, PeerShareItem, TreeEntry, PushInvite, PushBatch, BcTarget, BcStatus, DragOutSource, DragTicket, FsDrive, FsListing, LocalPick, PickerFavorite, ChatSnapshot, InboxEntry, RelayTraffic, UpdateState, KeepAliveState, OpenedPage, NotifyState } from './types';
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

export function updateSettings(s: Partial<AppSettings> & { port: number; downloadDir: string; downloadMode: string; deviceName: string; autoStart: boolean; bcExePath: string; autoBroadcastNew: boolean; groupRelay: boolean; allowRelay: boolean; relayMemory: boolean; transferWorkers: number; transferBlockMb: number }): Promise<AppSettings> {
  return invoke('update_settings', { req: s });
}

// 自更新（桌面端 + 安卓端共用同一套后端逻辑，见 `src-tauri/src/update/`）。
// 三条命令都只是"发起"：真正的进展靠 `update://state` 事件推回来（整份快照）。
export function updateState(): Promise<UpdateState> {
  return invoke('update_state');
}

/** `force = true`（用户手动点）忽略后端的静默检查缓存与镜像记忆。 */
export function updateCheck(force: boolean): Promise<UpdateState> {
  return invoke('update_check', { force });
}

/** 下载并安装：桌面端会覆盖当前 exe 并重启（进程随之结束），安卓端拉起系统安装器。 */
export function updateApply(): Promise<void> {
  return invoke('update_apply');
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

// 中转（F-106 / S-09）：本机替别人中转的**累计流量**与连接数 —— 设置里显示"替别人搬了多少"。
export function relayTraffic(): Promise<RelayTraffic> {
  return invoke('relay_traffic');
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

/**
 * 设备备注（F-101）：**只写本机配置**（`config.toml` 的 `deviceRemarks`），不发给对端、
 * 也不改对方的设备名。空串 = 清除。返回真正存下去的值（`null` = 已清除）——超长会被
 * 后端截断，所以界面以后端为准。
 */
export function setDeviceRemark(peerId: string, remark: string): Promise<string | null> {
  return invoke('set_device_remark', { peerId, remark });
}

/** 改设备在本机显示的名字（后端只在移动端构建里注册 `rename_peer`）。 */
export function renamePeer(peerId: string, name: string): Promise<void> {
  return invoke('rename_peer', { peerId, name });
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

/**
 * 一批本机路径的展示元数据（名字 / 是否目录 / 大小）。
 *
 * **只有拖放那条路用**：选择框里挑的项自带这三样（`FsEntry` → `LocalPick`），而 OS 拖放
 * 交给界面的只有路径 —— 会话里那条文件消息要显示"发的什么、多大"，得在这儿现问一次。
 */
export function pathPicks(paths: string[]): Promise<LocalPick[]> {
  return invoke('path_picks', { paths });
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

/**
 * 接收方点「同意接收」：起下载任务。
 *
 * `root` = 这次的落盘根目录（设置里「每次询问」时，界面先弹目录选择框再带上来）；
 * 不带就用设置里的下载目录。**已配对设备那条免同意自动收的路不问**，也就恒不带它。
 */
export function acceptPush(inviteId: string, root?: string): Promise<void> {
  return invoke('accept_push', { inviteId, root: root ?? null });
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

// 会话（聊天）：文本消息走长连接（后端 `chat.rs` / `WsEvent::ChatText` + `ChatRead`），
// 会话记录落在 `chat.json` 里。命令一律返回**整份会话状态** —— 界面不做增量演算。
/** 整份会话状态（面板打开时拉一次，之后由 `chat://changed` 事件推进）。 */
export function chatSnapshot(): Promise<ChatSnapshot> {
  return invoke('chat_snapshot');
}

/**
 * 发一条文本消息给一台或多台设备（多台 = 界面上的「群发」框；每台一份副本、同一个消息 id）。
 *
 * 除了整份快照还带回这条消息的 `id`：界面据此**逐台读状态**（谁送到了、谁没送到），
 * 不用去猜"哪条是刚发出去的"。
 */
export function chatSend(
  peerIds: string[],
  text: string,
  batchId?: string | null,
): Promise<{ id: string; batchId: string | null; snapshot: ChatSnapshot }> {
  return invoke('chat_send', { peerIds, text, batchId: batchId ?? null });
}

/**
 * 「这批设备」用哪份群发名单：点名了 `id` 就用它（并把成员更新成这次选的这几台），
 * 没点名就按成员集合复用一份、都没有再新建（名字留空，随后由 `chatBatchRename` 命名）。
 *
 * 两条路都走它：新建群发（不点名 → 复用/新建）与「编辑收件人」（点名 → 成员当场落定，
 * 不用等到下次发送）。
 */
export function chatBatchEnsure(members: string[], id?: string | null): Promise<{ id: string; snapshot: ChatSnapshot }> {
  return invoke('chat_batch_ensure', { members, id: id ?? null });
}

/** 改群发的名字（列表里右键「修改名字」）；名字留空 = 界面退回「群发 · N 台」。 */
export function chatBatchRename(id: string, name: string): Promise<ChatSnapshot> {
  return invoke('chat_batch_rename', { id, name });
}

/** 删掉一份群发名单（消息一条不动，它们本来就在各台设备的会话里）。 */
export function chatBatchDelete(id: string): Promise<ChatSnapshot> {
  return invoke('chat_batch_delete', { id });
}

/** 界面读过这些设备的会话：推进本地已读水位，并给对端回一条已读回执。 */
export function chatMarkRead(peerIds: string[]): Promise<ChatSnapshot> {
  return invoke('chat_mark_read', { peerIds });
}

/** 清空与一台设备的聊天记录（会话菜单里的「清空聊天记录」）。 */
export function chatClear(peerId: string): Promise<ChatSnapshot> {
  return invoke('chat_clear', { peerId });
}

// 共享群聊：一份"所有人的同一个"消息流（与群发相反 —— 群消息不镜像进各台的 1:1）。
// 与上面几条同一个路数：命令一律返回**整份会话状态**，界面拿来就换。
/**
 * 建一个群。`members` 是**除本机以外**的成员（名册里本机那一条由后端补）。
 * 返回新群 id 与整份状态（界面据此直接打开这条群聊）。
 */
export function groupCreate(members: string[], name: string): Promise<{ id: string; snapshot: ChatSnapshot }> {
  return invoke('group_create', { members, name });
}

/** 改群的名册与群名（名册走 LWW，`rev` 递增）。 */
export function groupEdit(id: string, members: string[], name: string): Promise<ChatSnapshot> {
  return invoke('group_edit', { id, members, name });
}

/** 往群里发一条文本；返回的 `id` 是**这条消息**的 id（不是群的）。 */
export function groupSend(id: string, text: string): Promise<{ id: string; snapshot: ChatSnapshot }> {
  return invoke('group_send', { id, text });
}

/** 本机读到哪儿了：推进群里的已读水位（未读清零）。 */
export function groupMarkRead(id: string): Promise<ChatSnapshot> {
  return invoke('group_mark_read', { id });
}

/**
 * 补拉一个群：把本机**已知缺**的那几条（`seq` 的洞 + `since_at` 之后的缺口）向成员要回来，
 * 一次最多 200 条、多轮（后端 `GroupSyncAsk` / `GroupSyncPost`）。返回的还是整份快照。
 *
 * 尽力而为：够不着的成员就是够不着（它不在线），要点回来的也可能只到一部分 —— 所以调用方
 * **失败静默**，缺口本身由 `ChatGroup.missing` 如实报给用户。
 */
export function groupSync(id: string): Promise<ChatSnapshot> {
  return invoke('group_sync', { id });
}

/** 接受一条群邀请 —— 从这一刻起它才算群（进会话列表，不再只是那张邀请卡）。 */
export function groupJoin(id: string): Promise<ChatSnapshot> {
  return invoke('group_join', { id });
}

/** 拒绝一条群邀请（本地记下"拒绝过"，同一份邀请不会再弹）。 */
export function groupDismiss(id: string): Promise<ChatSnapshot> {
  return invoke('group_dismiss', { id });
}

/** 退出一个群：名册里去掉本机并把"没有本机了"的名册广播给其他成员；**本机这份记录一并删掉**
 *  （后端 `GroupStore::leave` 会把本地的群、会话、读位都清干净；被对方移出才是"留在盘上"）。 */
export function groupLeave(id: string): Promise<ChatSnapshot> {
  return invoke('group_leave', { id });
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
/** 三条都返回落盘路径；**「每次询问」下用户在保存框里取消时返回 `null`** —— 不落盘，
 *  调用方当"什么都没发生"（别弹失败提示，也别打开不存在的路径）。 */
export function downloadFile(shareId: string, pathInShare: string): Promise<string | null> {
  return invoke('download_file', { shareId, pathInShare });
}

export function downloadFromPeer(peerId: string, shareId: string, pathInShare: string, fileName?: string): Promise<string | null> {
  return invoke('download_from_peer', { peerId, shareId, pathInShare, fileName: fileName ?? null });
}

export function downloadAndOpen(shareId: string, pathInShare: string): Promise<string | null> {
  return invoke('download_and_open', { shareId, pathInShare });
}

/** 用系统默认方式打开一个路径：目录就落在文件管理器里（收完推送那条通知的「打开目录」）。
 *  路径不在了会 reject —— 调用方据此把通知留住。 */
export function openPath(path: string): Promise<void> {
  return invoke('open_path', { path });
}

// 收件目录（移动端专属：手机上没有一个文件夹可翻，后端只在那边的构建里注册这两条）
/** 列收件目录：本机下载的直接项 + 每个「发送方目录」里的直接项（只两层，不递归）。 */
export function listInbox(): Promise<InboxEntry[]> {
  return invoke('list_inbox');
}

/** 删掉收件目录里的一项**文件**（后端会挡住下载目录之外的路径与目录本身）。 */
export function deleteInboxFile(path: string): Promise<void> {
  return invoke('delete_inbox_file', { path });
}

// 「让应用在后台活着」所需的系统那一层（F-87）：全是"读回状态 + 跳系统页"，只有安卓有实体
// （后端那两条命令只在移动端构建里注册，见 `src-tauri/src/system_settings.rs`）。

/** 电池优化白名单在不在 —— 这一层里**唯一能读到**的状态。 */
export function keepAliveState(): Promise<KeepAliveState> {
  return invoke('keepalive_state');
}

/** 弹系统的"允许后台运行"对话框（ROM 不认那条 action 时退到白名单列表页）。 */
export function requestIgnoreBatteryOptimizations(): Promise<void> {
  return invoke('request_ignore_battery_optimizations');
}

/** 跳到 ROM 的「自启动 / 后台活动」页；返回实际打开的是哪一页（华为那三个开关都在那一页上）。 */
export function openAutoStartSettings(): Promise<OpenedPage> {
  return invoke('open_autostart_settings');
}

/** 首次引导给他看过了没有（写在配置文件里，跨重启有效；重装会清掉、于是重新问一次）。 */
export function keepaliveGuided(): Promise<boolean> {
  return invoke('keepalive_guided');
}

/** 记一笔"引导已经给他看过"。 */
export function markKeepaliveGuided(): Promise<boolean> {
  return invoke('mark_keepalive_guided');
}

/** 通知这一层的真实状态：总开关 + 「消息通知」渠道的重要性（`-1` = 渠道还没建）。 */
export function notificationState(): Promise<NotifyState> {
  return invoke('notification_state');
}

/** 跳本应用的通知设置页（华为那一层「静默通知 / 横幅」就在这页上）。 */
export function openNotificationSettings(): Promise<void> {
  return invoke('open_notification_settings');
}

/** 跳「消息通知」那一条渠道自己的设置页（横幅、铃声、锁屏）。 */
export function openMessageChannelSettings(): Promise<void> {
  return invoke('open_message_channel_settings');
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
 * 窗口里「吃点击」的那几块（CSS px，窗口左上角为原点）。
 *
 * 窗口比看得见的那件东西大一圈：四周留的是 `--shadow` 的余量（`orbShape.RECT_SLACK` /
 * `App.PANEL_SLACK`）。那圈余量是**墨迹**不是实体，可透明像素在 Windows 上照样算命中 ——
 * 点它就是在点我们的窗口，压在下面的窗口一直收不到（点击、悬停、右键、焦点都被吃掉）。
 * 后端按这份区域 + 指针位置开关整窗穿透（见 `src-tauri/src/hit_region.rs`）。
 */
export function orbSetHitRegion(rects: ScreenRect[]): Promise<void> {
  return invoke('orb_set_hit_region', { rects });
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

/** 记录悬浮体本体中心（球心 / 卡片白底中心；物理像素）：下次启动复原到同一位置 */
export function orbSaveCenter(x: number, y: number): Promise<void> {
  return invoke('orb_save_center', { x, y });
}

// 对端浏览：**走后端命令**，不在 WebView 里直接 fetch 对端。
//
// 到对端那一跳是 TLS + 自签证书，WebView 不会认我们的证书（也不该让它认 —— 那要靠把证书
// 装进系统信任库，是明确不要的方案）。放后端还有两个顺带的好处：token 不再出现在前端，
// 证书指纹的校验与"连上了就把指纹钉住"也和其它调用共用同一份实现。
export async function peerShares(peerId: string): Promise<PeerShareItem[]> {
  const data = await invoke<{ shares: PeerShareItem[] }>('peer_shares', { peerId });
  return data.shares;
}

export async function peerTree(peerId: string, shareId: string, path: string): Promise<TreeEntry[]> {
  const data = await invoke<{ entries: TreeEntry[] }>('peer_tree', { peerId, shareId, path });
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