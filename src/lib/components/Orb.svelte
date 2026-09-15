<script lang="ts">
  import { getCurrentWebviewWindow } from '@tauri-apps/api/webviewWindow';

  let {
    badgeCount = 0,
    expanded = false,
    onToggle,
  }: {
    badgeCount?: number;
    expanded?: boolean;
    onToggle: () => void;
  } = $props();

  // 这里不能用 data-tauri-drag-region。Tauri 的拖拽脚本在 document 上监听 mousedown：
  //   detail===1 → preventDefault + stopImmediatePropagation + start_dragging
  // start_dragging 会让原生拖拽循环抢占鼠标，本次按下的 mouseup/click/pointerup 全部丢失，
  // 单击永远无效；只有双击的第二下（detail===2）走 internal_toggle_maximize 才放行。
  // 所以改为手动拖拽：位移超过阈值才拖窗口，否则松手即视为单击。
  const DRAG_THRESHOLD_PX = 4;
  type OrbPointerEvent = PointerEvent & { currentTarget: HTMLButtonElement };
  let downX = 0;
  let downY = 0;
  let dragging = false;

  function handlePointerDown(e: OrbPointerEvent) {
    if (e.button !== 0) return;
    downX = e.clientX;
    downY = e.clientY;
    dragging = false;
    e.currentTarget.setPointerCapture(e.pointerId);
  }

  function handlePointerMove(e: OrbPointerEvent) {
    if (dragging || e.buttons !== 1) return;
    const dx = e.clientX - downX;
    const dy = e.clientY - downY;
    if (dx * dx + dy * dy <= DRAG_THRESHOLD_PX * DRAG_THRESHOLD_PX) return;
    dragging = true;
    void getCurrentWebviewWindow().startDragging();
  }

  function handlePointerUp(e: OrbPointerEvent) {
    if (e.button !== 0) return;
    if (e.currentTarget.hasPointerCapture(e.pointerId)) {
      e.currentTarget.releasePointerCapture(e.pointerId);
    }
    if (!dragging) onToggle();
  }

  function handleKeyDown(e: KeyboardEvent) {
    if (e.repeat) return;
    if (e.key === 'Enter' || e.key === ' ') {
      e.preventDefault();
      onToggle();
    }
  }
</script>

<button
  class="orb"
  aria-label={expanded ? '收起 OrbDrop 面板' : '展开 OrbDrop 面板'}
  aria-expanded={expanded}
  onpointerdown={handlePointerDown}
  onpointermove={handlePointerMove}
  onpointerup={handlePointerUp}
  onpointercancel={() => { dragging = true; }}
  onkeydown={handleKeyDown}
>
  <span class="mark">
    <span class="pulse"></span>
    <svg viewBox="0 0 30 30" fill="none" aria-hidden="true">
      <circle cx="15" cy="15" r="13" stroke="var(--accent)" stroke-opacity="0.28" stroke-width="1.4"/>
      <circle cx="15" cy="15" r="5" fill="var(--accent)"/>
      <path d="M15 9.5 10.5 14M15 9.5l4.5 4.5" stroke="var(--surface)" stroke-width="1.6" stroke-linecap="round" stroke-linejoin="round"/>
    </svg>
  </span>
  <span class="orb-badge num" hidden={badgeCount === 0}>{badgeCount}</span>
</button>