import { prepareDragOut, startDragOut } from './api';
import type { DragOutSource } from './types';

/**
 * 拖拽下载（F-46）：把共享项里的文件/目录拖到资源管理器（Finder）里，落在拖到的那个目录。
 *
 * 用不了网页的 HTML5 拖拽（`draggable` / `dragstart`）：网页拖拽出不了 webview，跨进程落到
 * 资源管理器必须是 OS 的原生拖放，而原生拖放得由 Rust 侧发起（见 `src-tauri/src/drag.rs`）。
 * 这里只负责**认手势**：主键按下、移动过阈值 → `prepare`（对端项同时开始在后台下到暂存
 * 目录）→ `start` 起拖（阻塞到用户松手）。
 */

/** 移动多少像素算"要拖出去"。低于它还是普通的按下/点击，不该惊动后端。 */
const DRAG_THRESHOLD_PX = 6;

export interface DragOutParams {
  source: DragOutSource;
  /** 拖拽过程中的提示（"正在准备…"、失败原因），由界面转成浮层提示。 */
  onMessage?: (msg: string) => void;
}

export function dragOut(node: HTMLElement, params: DragOutParams) {
  let current = params;
  let down: { x: number; y: number } | null = null;
  let fired = false;

  function onPointerDown(e: PointerEvent) {
    if (e.button !== 0) return;
    // 行内的按钮/开关有自己的点击语义（展开箭头、广播开关），别让它们变成拖拽把手。
    if ((e.target as HTMLElement | null)?.closest('button, input, label, a')) return;
    /* **必须抓指针捕获**：拖出的判定（越过 6px 阈值）靠这一格自己的 pointermove 事件，而
       事件只送给"指针底下那个元素"。甩得快的时候第一个 move 就已经落在这一格之外了 ——
       那一趟事件谁都没收到，这一次拖出**整趟不成立**（用户看到的正是"拖一下没反应，
       再慢慢拖一次就行"）。捕获之后，不管指针甩到哪儿，move 都还是先送到起手这一格。 */
    node.setPointerCapture(e.pointerId);
    down = { x: e.clientX, y: e.clientY };
    fired = false;
  }

  function onPointerMove(e: PointerEvent) {
    if (!down || fired || e.buttons !== 1) return;
    const dx = e.clientX - down.x;
    const dy = e.clientY - down.y;
    if (dx * dx + dy * dy < DRAG_THRESHOLD_PX * DRAG_THRESHOLD_PX) return;
    fired = true;
    // 交给 OS 之前放掉捕获（与 `RectOrb`/`Orb` 起拖窗口同一条理由）：`start_drag_out` 之后
    // 页面再也收不到 pointerup，捕获就没法释放了 —— 而**留着的捕获 = 留着的 hover**，
    // 指针移开这一格也还亮着。
    if (node.hasPointerCapture(e.pointerId)) node.releasePointerCapture(e.pointerId);
    void fire(current);
  }

  function onPointerUp(e: PointerEvent) {
    down = null;
    if (node.hasPointerCapture(e.pointerId)) node.releasePointerCapture(e.pointerId);
  }

  async function fire(p: DragOutParams) {
    const src = p.source;
    try {
      const ticket = await prepareDragOut(src);
      if (!ticket.ready) {
        // 对端项：两种实现都在这里等（镜像只需问一次元数据就建好占位符；旧路可能要先把内容
        // 下到暂存目录）。无论哪种，起拖都跟着**按住的主键**走 —— 所以这句必须提醒"按住不放"。
        p.onMessage?.(`正在准备「${ticket.name}」…（请按住不放）`);
      }
      await startDragOut(ticket.ticket);
    } catch (err) {
      p.onMessage?.(err instanceof Error ? err.message : String(err));
    }
  }

  node.addEventListener('pointerdown', onPointerDown);
  node.addEventListener('pointermove', onPointerMove);
  node.addEventListener('pointerup', onPointerUp);
  node.addEventListener('pointercancel', onPointerUp);

  return {
    update(next: DragOutParams) {
      current = next;
    },
    destroy() {
      node.removeEventListener('pointerdown', onPointerDown);
      node.removeEventListener('pointermove', onPointerMove);
      node.removeEventListener('pointerup', onPointerUp);
      node.removeEventListener('pointercancel', onPointerUp);
    },
  };
}