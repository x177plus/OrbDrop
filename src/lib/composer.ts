/**
 * 会话输入区的**分段模型**：一段文字 / 一张图，按顺序排成一条。
 *
 * 为什么不是"一个字符串 + 一份附件表"：需求要的是**图能夹在文字中间**（图前一段、图后
 * 一段），发送顺序也按它在框里出现的顺序 —— 顺序本身就是数据，只有一条有序的分段表能表达它。
 * 一条分段表 → 依次落成若干条消息（文字一条、图一条），见 `stores/chat.svelte.ts` 的
 * `sendComposer`。
 *
 * 两端（桌面 `ConvoPanel.svelte` / 移动 `ChatScreen.svelte`）共用这一份：输入区是一个
 * `contenteditable`，DOM 是**编辑时的真相**（这个模块负责 DOM ↔ 分段的双向翻译），
 * 而 store 里那份分段表是**切会话不丢**的那一份。
 *
 * 图片在框里显示的是 `blob:` 预览 URL；真要发出去的那一份是 `path`（本机暂存文件）——
 * 推送只认路径（见 `src-tauri/src/paste.rs`），所以**粘贴那一刻就落盘**，框里只留一个句柄。
 */

import { readImageDataUrl, savePastedImage, resolveImageUri } from './api';

/** 输入区里的一段。`image` 的 `url` 只是框里的预览（blob: 或 data:），发出去用的是 `path`。 */
export type ComposerSeg =
  | { kind: 'text'; text: string }
  | { kind: 'image'; path: string; url: string; name: string; size: number };

/** 图片字节（钉成 `ArrayBuffer` 而不是默认的 `ArrayBufferLike`：`Blob` 只收前者）。 */
type Bytes = Uint8Array<ArrayBuffer>;

/** 会话里能当图片预览的扩展名（与后端 `paste::mime_of` 同一份名单）。 */
const IMAGE_EXTS: Record<string, true> = {
  png: true,
  jpg: true,
  jpeg: true,
  gif: true,
  webp: true,
  bmp: true,
  avif: true,
};

/** MIME → 扩展名。认不出来 = `null`（那种要先用画布转成 PNG，见 `bytesOfBlob`）。 */
function extOfMime(mime: string): string | null {
  switch (mime.toLowerCase().split(';')[0].trim()) {
    case 'image/png': return 'png';
    case 'image/jpeg': return 'jpg';
    case 'image/gif': return 'gif';
    case 'image/webp': return 'webp';
    case 'image/bmp': return 'bmp';
    case 'image/avif': return 'avif';
    default: return null;
  }
}

/** 这条文件名看着像图片吗（会话里决定画不画缩略图的唯一判据）。 */
export function isImageName(name: string): boolean {
  const dot = name.lastIndexOf('.');
  return dot >= 0 && IMAGE_EXTS[name.slice(dot + 1).toLowerCase()] === true;
}

/** 框里有没有值得发的东西（两段空白文字不算）。 */
export function hasContent(segs: readonly ComposerSeg[]): boolean {
  return segs.some((s) => (s.kind === 'image' ? true : s.text.trim() !== ''));
}

/** 丢掉这几段里图片的预览 URL（发出去 / 清空草稿 / 从框里删掉时叫）。 */
export function revokeSegs(segs: readonly ComposerSeg[]): void {
  for (const s of segs) {
    if (s.kind === 'image' && s.url.startsWith('blob:')) URL.revokeObjectURL(s.url);
  }
}

// ── DOM ↔ 分段 ────────────────────────────────────────────────────────────

function chipEl(seg: ComposerSeg & { kind: 'image' }): HTMLImageElement {
  const img = document.createElement('img');
  img.className = 'cx-img';
  img.src = seg.url;
  img.alt = '';
  // 这一条不能省：`contenteditable="false"` 让整张图成为一个**原子**（光标跳得过去、
  // 退格一次删掉一整张），不然它会被当成普通节点逐字符编辑。
  img.contentEditable = 'false';
  img.draggable = false;
  img.dataset.path = seg.path;
  img.dataset.name = seg.name;
  img.dataset.size = String(seg.size);
  return img;
}

/** 输入区的 DOM → 分段表（每次 `input` 之后叫一次）。 */
export function serializeComposer(el: HTMLElement): ComposerSeg[] {
  const segs: ComposerSeg[] = [];
  let buf = '';
  const flush = () => {
    if (buf !== '') segs.push({ kind: 'text', text: buf });
    buf = '';
  };
  const walk = (node: Node) => {
    for (const child of Array.from(node.childNodes)) {
      if (child.nodeType === Node.TEXT_NODE) {
        buf += child.textContent ?? '';
        continue;
      }
      if (child.nodeType !== Node.ELEMENT_NODE) continue;
      const e = child as HTMLElement;
      if (e.tagName === 'IMG') {
        const path = e.dataset.path ?? '';
        // 没有 `data-path` 的图是**还没认领**的（浏览器自己塞进来的，见 `adoptDomImages`）——
        // 这里不丢它（丢了用户刚粘的图就没了），先当作图片段落在原地，等认领那一趟补上路径。
        flush();
        segs.push({
          kind: 'image',
          path,
          url: (e as HTMLImageElement).src,
          name: e.dataset.name ?? '图片',
          size: Number(e.dataset.size ?? 0),
        });
        continue;
      }
      if (e.tagName === 'BR') {
        buf += '\n';
        continue;
      }
      // 其余元素（Chromium 偶尔把一次粘贴包进 div/span）：块级的前后补换行，内容按文字收。
      const block = e.tagName === 'DIV' || e.tagName === 'P';
      if (block && buf && !buf.endsWith('\n')) buf += '\n';
      walk(e);
      if (block && !buf.endsWith('\n')) buf += '\n';
    }
  };
  walk(el);
  flush();
  return segs;
}

/** 分段表 → 输入区的 DOM（切会话时重建；平时**不动** DOM，不然光标会被拽走）。 */
export function applyComposer(el: HTMLElement, segs: readonly ComposerSeg[]): void {
  el.textContent = '';
  for (const s of segs) {
    el.appendChild(s.kind === 'text' ? document.createTextNode(s.text) : chipEl(s));
  }
  setCaretEnd(el);
}

/** 光标放到框末（打开会话、插入图片之后都要它）。 */
export function setCaretEnd(el: HTMLElement): void {
  const sel = window.getSelection();
  if (!sel) return;
  const range = document.createRange();
  range.selectNodeContents(el);
  range.collapse(false);
  sel.removeAllRanges();
  sel.addRange(range);
}

/**
 * 把一张图插到**光标处**（不是框末）—— "在图片前后添加文字"这件事全靠它：
 * 光标在中间，图就落在中间；前后各有一段文字时，那两段就是两条消息。
 */
function insertChip(el: HTMLElement, seg: ComposerSeg & { kind: 'image' }): void {
  const chip = chipEl(seg);
  const sel = window.getSelection();
  const inside = sel && sel.rangeCount > 0 && el.contains(sel.getRangeAt(0).startContainer);
  if (!inside) {
    el.appendChild(chip);
    setCaretEnd(el);
    return;
  }
  const range = sel!.getRangeAt(0);
  range.deleteContents();
  range.insertNode(chip);
  // 光标停到图后面：用户接着打的字就落在图的**后面**那一半，顺序自然是对的。
  const after = document.createRange();
  after.setStartAfter(chip);
  after.collapse(true);
  sel!.removeAllRanges();
  sel!.addRange(after);
}

// ── 字节那一层 ────────────────────────────────────────────────────────────

/** 字节 → base64（分块，别拿 `String.fromCharCode(...bytes)` 一次性摊——大图会把栈撑爆）。 */
function bytesToBase64(bytes: Bytes): string {
  let out = '';
  const CHUNK = 0x8000;
  for (let i = 0; i < bytes.length; i += CHUNK) {
    out += String.fromCharCode(...bytes.subarray(i, i + CHUNK));
  }
  return btoa(out);
}

/** `data:image/png;base64,…` → 字节 + 扩展名（浏览器塞进 DOM 的图常常就是这个形状）。 */
function bytesOfDataUrl(src: string): { bytes: Bytes; ext: string; mime: string } | null {
  const comma = src.indexOf(',');
  if (!src.startsWith('data:') || comma < 0) return null;
  const head = src.slice(5, comma);
  if (!head.includes('base64')) return null;
  const mime = head.split(';')[0].trim();
  const ext = extOfMime(mime);
  try {
    const bin = atob(src.slice(comma + 1));
    const bytes = new Uint8Array(bin.length);
    for (let i = 0; i < bin.length; i++) bytes[i] = bin.charCodeAt(i);
    return { bytes, ext: ext ?? 'png', mime };
  } catch {
    return null;
  }
}

/**
 * 认不出来的格式（比如剪贴板给的是 TIFF）用画布转成 PNG —— 浏览器能画出来就说明它有解码器，
 * 转一手比"跟用户说这个格式不支持"有用得多。
 */
async function toPng(blob: Blob): Promise<{ bytes: Bytes; ext: string; mime: string } | null> {
  try {
    const bmp = await createImageBitmap(blob);
    const cv = document.createElement('canvas');
    cv.width = bmp.width;
    cv.height = bmp.height;
    const ctx = cv.getContext('2d');
    if (!ctx) return null;
    ctx.drawImage(bmp, 0, 0);
    bmp.close?.();
    const out = await new Promise<Blob | null>((resolve) => cv.toBlob(resolve, 'image/png'));
    if (!out) return null;
    return { bytes: new Uint8Array(await out.arrayBuffer()), ext: 'png', mime: 'image/png' };
  } catch {
    return null;
  }
}

/** 一个图片 Blob → 落盘要的三样（字节 / 扩展名 / 类型）。转不出来返回 `null`。 */
async function bytesOfBlob(blob: Blob): Promise<{ bytes: Bytes; ext: string; mime: string } | null> {
  const ext = extOfMime(blob.type);
  if (ext && blob.size > 0) {
    return { bytes: new Uint8Array(await blob.arrayBuffer()), ext, mime: blob.type };
  }
  return toPng(blob);
}

/**
 * 一个 Blob → 一张**已在框里、已落盘**的图片段。
 *
 * 预览 URL 用 `blob:`（不是原样的 data URL）：data URL 会把整份字节挂在输入区里，大图在保存
 * 与重画时都要多拷一遍。
 */
async function stageBlob(blob: Blob): Promise<ComposerSeg & { kind: 'image' }> {
  const got = await bytesOfBlob(blob);
  if (!got) throw new Error('这个图片格式读不出来');
  const base64 = bytesToBase64(got.bytes);
  const path = await savePastedImage(base64, got.ext);
  const url = URL.createObjectURL(new Blob([got.bytes], { type: got.mime }));
  return { kind: 'image', path, url, name: fileNameOf(path), size: got.bytes.length };
}

/** 暂存文件的文件名就是这条消息的名字（"图片-20261013-153012.png"）。 */
function fileNameOf(path: string): string {
  const parts = path.split(/[\\/]/);
  return parts[parts.length - 1] || '图片';
}

// ── 粘贴 ──────────────────────────────────────────────────────────────────

/** 剪贴板里那几张图（没图 = 空表，交给浏览器的默认粘贴去处理文字）。 */
export function imageFiles(dt: DataTransfer | null): File[] {
  if (!dt) return [];
  const out: File[] = [];
  for (const item of Array.from(dt.items)) {
    if (item.kind !== 'file' || !item.type.startsWith('image/')) continue;
    const f = item.getAsFile();
    if (f) out.push(f);
  }
  return out;
}

/**
 * 把这几张图落盘并插到光标处（`paste` 事件那条路）。
 *
 * 一张失败不影响其余几张：坏的那张报一句原话（`onError`），别的照常进框。
 */
export async function insertImages(el: HTMLElement, files: readonly File[], onError: (msg: string) => void): Promise<boolean> {
  let ok = false;
  for (const f of files) {
    try {
      insertChip(el, await stageBlob(f));
      ok = true;
    } catch (e) {
      onError(e instanceof Error ? e.message : String(e));
    }
  }
  return ok;
}

/**
 * 把**系统选择器选中的那几张图**插到光标处（安卓那颗「＋」→「图片」那条路，F-118）。
 *
 * 与粘贴那条路的区别只在"字节从哪来"：粘贴给我们的是 `File` / `content://`，这一条给的是
 * **原生已经拷好的本机路径**（见 `pickFiles`），所以不必再落盘一次 —— 只把预览读出来
 * （`readImageDataUrl`，与草稿重建用的是同一个读法），做出一段与粘贴完全同形的图片段。
 * 之后的事一条都不用改：发送顺序、对端的图、草稿里那张缩略图都走同一份模型。
 *
 * 读不出预览的那一张（超过预览上限的大图、扩展名不在名单里的）报一句原话并跳过 ——
 * 与粘贴那条路的失败口径一致。
 */
export async function insertImagePicks(
  el: HTMLElement,
  picks: readonly { path: string; name: string; size: number }[],
  onError: (msg: string) => void,
): Promise<boolean> {
  let ok = false;
  for (const p of picks) {
    try {
      const url = await readImageDataUrl(p.path);
      insertChip(el, { kind: 'image', path: p.path, url, name: p.name, size: p.size });
      ok = true;
    } catch (e) {
      onError(e instanceof Error ? e.message : String(e));
    }
  }
  return ok;
}

/**
 * 认领浏览器**自己塞进输入区**的图（`input` 事件那条路）。
 *
 * 为什么两套路：桌面端 WebView 的 `paste` 事件一般会连字节一起给我们（上面那条路），但
 * **输入法的剪贴板**（安卓那半就是靠它）不走 `paste`，它是直接往 `contenteditable` 里插一个
 * `<img>`；插进来的 `src` 可能是 `data:`、`blob:`，也可能是一个 `content://`（那时得请
 * 原生那一跳把它拷成文件）。认领 = 把这个 `<img>` 换成我们自己那张有路径的图（`data-path`）。
 *
 * 返回"有没有认领到东西"（调用方据此决定要不要再序列化一次）。
 */
export async function adoptDomImages(el: HTMLElement, onError: (msg: string) => void): Promise<boolean> {
  const pending = Array.from(el.querySelectorAll('img:not([data-path])')) as HTMLImageElement[];
  let ok = false;
  for (const img of pending) {
    const src = img.getAttribute('src') ?? '';
    try {
      const seg = await stageSrc(src);
      const chip = chipEl(seg);
      img.replaceWith(chip);
      ok = true;
    } catch (e) {
      onError(e instanceof Error ? e.message : String(e));
      img.remove();
    } finally {
      // 认领这一步会给新 chip **另 mint 一条** `blob:` URL（见 `stageSrc`）；原来那个 `<img>`
      // （无论成功被换掉、还是失败被摘掉）此刻已经不在 DOM 里 —— 它那条 `blob:` URL 再没人读，
      // 不 revoke 就是一份永不释放的浏览器内存。放在 `finally`、且在 `replaceWith`/`remove`
      // **之后**才 revoke：这一刻原图确已不在任何 chip/预览里，不会把正在显示的图弄裂。
      // `data:` / `content://` 不是对象 URL，revoke 无意义也不该收。
      if (src.startsWith('blob:')) URL.revokeObjectURL(src);
    }
  }
  return ok;
}

/** 一个 `<img src>` → 已在框里、已落盘的图片段。 */
async function stageSrc(src: string): Promise<ComposerSeg & { kind: 'image' }> {
  const data = bytesOfDataUrl(src);
  if (data) {
    const path = await savePastedImage(bytesToBase64(data.bytes), data.ext);
    const url = URL.createObjectURL(new Blob([data.bytes], { type: data.mime || 'image/png' }));
    return { kind: 'image', path, url, name: fileNameOf(path), size: data.bytes.length };
  }
  if (src.startsWith('blob:')) return stageBlob(await (await fetch(src)).blob());
  if (src.startsWith('http:') || src.startsWith('https:')) return stageBlob(await (await fetch(src)).blob());
  // 剩下的交给原生那一跳（安卓的 `content://` 就在这里）：它把那份 URI 拷成暂存文件，
  // 连字节数一起给回来 —— 路径已经在手上，预览的 URL 再向它要一次 data URL 即可。
  const picked = await resolveImageUri(src);
  const url = await readImageDataUrl(picked.path);
  return { kind: 'image', path: picked.path, url, name: fileNameOf(picked.path), size: picked.size };
}