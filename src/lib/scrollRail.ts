/**
 * 横向轨道（标签栏 `.tabs` / 选择框「常用」栏 `.picker-rail`）够到后面那几项的鼠标手势。
 *
 * 这两条轨道都是 `overflow-x: auto` 且**不画滚动条**（`scrollbar-width: none`）：项目一多
 * 就横向溢出，而 Chromium 天生只给三条路 —— Shift+滚轮、触控板横滑、键盘聚焦时自动滚进视野。
 * 普通滚轮不转横向，鼠标拖动完全没反应，于是**后面的项直接点不到**（"常用加多了以后点不了"
 * 就是这么来的）。这里把鼠标那两条补上：拖动横滚 + 滚轮横滚。
 *
 * 挂上即用，不看参数：两条轨道的交互约定必须一致，有第二套行为就会有人踩错。
 */
export function scrollRail(node: HTMLElement) {
  /** 拖动判定阈值（px）：没越过它就算点击，里面按钮的 onclick 照常触发。 */
  const DRAG_THRESHOLD = 4;

  let drag: { pointerId: number; startX: number; startLeft: number; moved: boolean } | null = null;

  /** 没溢出就没有可滚的东西，一切手势都别插手。 */
  const overflowing = () => node.scrollWidth > node.clientWidth;

  function onPointerDown(e: PointerEvent) {
    // 只认主键：右键在这些轨道上是正经操作（右键目录加常用、右键芯片出自己的菜单），别抢。
    if (e.button !== 0 || !overflowing()) return;
    drag = { pointerId: e.pointerId, startX: e.clientX, startLeft: node.scrollLeft, moved: false };
  }

  function onPointerMove(e: PointerEvent) {
    if (!drag || e.pointerId !== drag.pointerId) return;
    const dx = e.clientX - drag.startX;
    if (!drag.moved) {
      if (Math.abs(dx) < DRAG_THRESHOLD) return;
      /* 越过阈值才抓指针。抓早了会把 pointerdown 之后的 mousedown/mouseup 一并重定向到容器，
         click 的目标于是变成容器，里面按钮的 onclick 再也收不到 —— 单击就废了。 */
      drag.moved = true;
      node.classList.add('dragging');
      node.setPointerCapture(drag.pointerId);
    }
    node.scrollLeft = drag.startLeft - dx;
    e.preventDefault();
  }

  function endDrag(e: PointerEvent) {
    if (!drag || e.pointerId !== drag.pointerId) return;
    if (node.hasPointerCapture(e.pointerId)) node.releasePointerCapture(e.pointerId);
    drag = null;
    node.classList.remove('dragging');
  }

  function onWheel(e: WheelEvent) {
    if (e.deltaX !== 0 || e.deltaY === 0) return;   // 触控板横滑：交给原生，别叠加
    if (!overflowing()) return;                     // 没溢出：不拦
    node.scrollLeft += e.deltaY;
    e.preventDefault();
  }

  node.addEventListener('pointerdown', onPointerDown);
  node.addEventListener('pointermove', onPointerMove);
  node.addEventListener('pointerup', endDrag);
  node.addEventListener('pointercancel', endDrag);
  node.addEventListener('wheel', onWheel, { passive: false });

  return {
    destroy() {
      node.removeEventListener('pointerdown', onPointerDown);
      node.removeEventListener('pointermove', onPointerMove);
      node.removeEventListener('pointerup', endDrag);
      node.removeEventListener('pointercancel', endDrag);
      node.removeEventListener('wheel', onWheel);
    },
  };
}