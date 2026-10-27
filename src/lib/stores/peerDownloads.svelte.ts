/**
 * 桌面对端共享项「手动下载」的进度（右键「下载到本地」与双击打开）。
 *
 * 后端 `download_from_peer` 阻塞到落盘才回话，中途的字节数由 `browse://download` 事件送来
 * （与安卓「浏览」里那圈进度是同一份事件）。这里只做桌面端需要的那一小块：
 * 按 `peerId\0shareId\0path` 记一行，行里的背景底色按 `received / total` 从左往右推进。
 *
 * 暂停走的是后端那道闸（`set_browse_download_paused`）：命令停在下一块之前，已落盘的字节
 * 与 `.orbdrop-part` 都不动；继续就是同一条命令接着拉。界面在暂停期间保留同一个百分比，
 * 只把底色换成另一档（见 main.css 的 `.dl-paused`）。
 *
 * `total` **不在事件里**（后端只报已收字节），所以调用方在发起下载前用
 * `preparePeerDownload` 把列表里已经知道的大小登记进来；没有大小就不画进度——
 * 宁可不动，也不编一个百分比。
 */
import * as api from '$lib/api';
import { t } from '$lib/i18n.svelte';

export interface PeerDownload {
  key: string;
  peerId: string;
  shareId: string;
  /** 共享项内相对路径；共享项根是文件时为空串。 */
  path: string;
  name: string;
  /** 对端报的大小；0 = 不知道（这时不画进度）。 */
  total: number;
  received: number;
  status: 'running' | 'paused' | 'done' | 'failed';
  dest: string | null;
  error: string | null;
}

export interface PeerDownloadEvent {
  peerId: string;
  shareId: string;
  path: string;
  name: string;
  status: 'progress' | 'done' | 'error';
  received: number;
  dest?: string | null;
  error?: string | null;
}

let downloads = $state<Record<string, PeerDownload>>({});

/** 事件里没有 total，发起方登记在这里（非响应式：它只参与下一次事件的计算）。 */
const totals: Record<string, number> = {};

function keyOf(peerId: string, shareId: string, path: string): string {
  return `${peerId}\u0000${shareId}\u0000${path}`;
}

/** 当前这一项（对端文件）的下载状态；本机项 / 没有对端时返回 `undefined`。 */
export function peerDownloadOf(
  peerId: string | undefined,
  shareId: string,
  path: string,
): PeerDownload | undefined {
  if (!peerId) return undefined;
  return downloads[keyOf(peerId, shareId, path)];
}

/** 0–100；不知道总大小时返回 0（调用方据此不画进度）。暂停时百分比原地不动。 */
export function peerDownloadPct(dl: PeerDownload | undefined): number {
  if (!dl || (dl.status !== 'running' && dl.status !== 'paused') || dl.total <= 0) return 0;
  return Math.min(100, Math.max(0, Math.round((dl.received / dl.total) * 100)));
}

/** 这一行此刻要不要画背景进度（传输中或已暂停、且知道大小时才画）。 */
export function peerDownloadActive(dl: PeerDownload | undefined): boolean {
  return !!dl && (dl.status === 'running' || dl.status === 'paused') && dl.total > 0;
}

function put(dl: PeerDownload): void {
  downloads = { ...downloads, [dl.key]: dl };
}

function drop(key: string): void {
  if (!(key in downloads)) return;
  const next = { ...downloads };
  delete next[key];
  downloads = next;
}

/**
 * 发起下载前登记这一项的大小，并把上一次已经结算的状态清掉（重新下载同一项时，
 * 迟到的进度不能被旧的 `done` 挡住）。
 *
 * 已经在传的那条**不动**：后端对同一项只允许一条命令，界面这时候重复点只该看到"正在下载"。
 */
export function preparePeerDownload(
  peerId: string,
  shareId: string,
  path: string,
  size: number,
): void {
  const key = keyOf(peerId, shareId, path);
  if (size > 0) totals[key] = size;
  else if (downloads[key]?.status !== 'running' && downloads[key]?.status !== 'paused') delete totals[key];
  const cur = downloads[key];
  if (cur && cur.status !== 'running' && cur.status !== 'paused') drop(key);
}

/** 保存框里取消 / 没开始：把还没动过的那一行和登记的大小收掉（不弹失败）。 */
export function cancelPeerDownload(peerId: string, shareId: string, path: string): void {
  const key = keyOf(peerId, shareId, path);
  delete totals[key];
  const cur = downloads[key];
  if (cur?.status === 'running' && cur.received === 0) drop(key);
}

/** 命令成功返回的兜底收尾：`done` 事件还没到（或丢了）时别让那一行停在半路。 */
export function finishPeerDownload(peerId: string, shareId: string, path: string): void {
  const key = keyOf(peerId, shareId, path);
  const cur = downloads[key];
  if (!cur || (cur.status !== 'running' && cur.status !== 'paused')) return;
  put({ ...cur, received: cur.received || cur.total, status: 'done', error: null });
}

/** 双击那一行暂停 / 继续的结果：`settled` = 命令已经收尾（不用再报"已不在下载中"）。 */
export type PeerPauseResult = 'paused' | 'resumed' | 'settled' | 'missing' | 'error';

/**
 * 双击下载中的那一行：暂停；再双击：从当前 offset 继续。
 *
 * 先改界面（手按下就要看到颜色变化），后端没找到那条命令（已经下完/失败）再退回去。
 * 暂停期间照收已经在路上的进度事件，但状态保持 `paused`（见 `notePeerDownload`），
 * 于是底色宽度停在原地，只换颜色。
 */
export async function togglePeerDownloadPause(
  peerId: string,
  shareId: string,
  path: string,
): Promise<PeerPauseResult> {
  const key = keyOf(peerId, shareId, path);
  const cur = downloads[key];
  if (!cur || (cur.status !== 'running' && cur.status !== 'paused')) return 'missing';
  const want = cur.status === 'running' ? 'paused' : 'running';
  const before = cur.status;
  put({ ...cur, status: want });
  try {
    const ok = await api.setBrowseDownloadPaused(peerId, shareId, path, want === 'paused');
    if (!ok) {
      const now = downloads[key];
      if (now?.status === 'done' || now?.status === 'failed') return 'settled';
      if (now && now.status === want) put({ ...now, status: before });
      return 'missing';
    }
    return want === 'paused' ? 'paused' : 'resumed';
  } catch {
    const now = downloads[key];
    if (now && now.status === want) put({ ...now, status: before });
    return 'error';
  }
}

/** 后端 `browse://download` 事件：进度 / 落盘 / 失败。 */
export function notePeerDownload(ev: PeerDownloadEvent): void {
  const key = keyOf(ev.peerId, ev.shareId, ev.path);
  const cur = downloads[key];
  if (ev.status === 'progress') {
    // 终局之后迟到的进度不再把已完成翻回「下载中」；暂停期间**整拍丢掉**——用户看到
    // 的宽度就停在按下的那一刻（后端可能在下一块边界前又多写了一点，继续后的第一拍
    // 会把它补上）。
    if (cur?.status === 'done' || cur?.status === 'failed' || cur?.status === 'paused') return;
    put({
      key,
      peerId: ev.peerId,
      shareId: ev.shareId,
      path: ev.path,
      name: ev.name || cur?.name || t('单个文件'),
      total: totals[key] ?? cur?.total ?? 0,
      received: Math.max(0, ev.received),
      status: 'running',
      dest: null,
      error: null,
    });
    return;
  }
  const base: PeerDownload = cur ?? {
    key,
    peerId: ev.peerId,
    shareId: ev.shareId,
    path: ev.path,
    name: ev.name || t('单个文件'),
    total: totals[key] ?? 0,
    received: 0,
    status: 'running',
    dest: null,
    error: null,
  };
  if (ev.status === 'done') {
    // 事件与命令返回谁先到都只收一次尾；没有进度事件（空文件 / 太快）也照样落成一档。
    put({
      ...base,
      received: Math.max(base.received, ev.received),
      total: base.total || ev.received,
      status: 'done',
      dest: ev.dest ?? null,
      error: null,
    });
    return;
  }
  if (cur) put({ ...cur, status: 'failed', error: ev.error ?? t('下载失败') });
}