<script lang="ts">
  import Logo from '$lib/components/Logo.svelte';
  import type { OrbDragInfo } from '$lib/types';

  let {
    badgeCount = 0,
    notifCount = 0,
    expanded = false,
    onToggle,
    onDragStart,
  }: {
    badgeCount?: number;
    notifCount?: number;
    expanded?: boolean;
    onToggle: () => void;
    /** 起拖：校正锚点并把窗口拖动交给 OS（`App.beginOrbDrag`） */
    onDragStart: (info: OrbDragInfo) => void;
  } = $props();

  // 这里不能用 data-tauri-drag-region。Tauri 的拖拽脚本在 document 上监听 mousedown：
  //   detail===1 → preventDefault + stopImmediatePropagation + start_dragging
  // start_dragging 会让原生拖拽循环抢占鼠标，本次按下的 mouseup/click/pointerup 全部丢失，
  // 单击永远无效；只有双击的第二下（detail===2）走 internal_toggle_maximize 才放行。
  // 所以改为手动拖拽：位移超过阈值才拖窗口，否则松手即视为单击。起拖本身交给 App 走一趟
  // 后端（`orb_drag_start`：那里要先按按下点校正锚点，再当场把拖动交给 OS）。
  const DRAG_THRESHOLD_PX = 4;
  type OrbPointerEvent = PointerEvent & { currentTarget: HTMLButtonElement };
  let downX = 0;
  let downY = 0;
  let pressed = false;   // 主键还按着（只用于拖动开始前；起拖之后页面收不到 pointerup）
  let moved = false;     // 越过阈值：这一按是拖动，不是单击
  let started = false;   // 已经起拖
  const SLIDE_PROPS = ['left', 'top'];
  let orbEl = $state<HTMLButtonElement | null>(null);

  /**
   * 球正在窗口里滑（贴边收起半颗 ↔ 悬停滑回完整，见 main.css 里 `.orb` 的 left/top 过渡）？
   *
   * 这段时间里绝不能交给 OS 拖：OS 是**起拖那一刻**把窗口钉在指针上的，而球此时还在往内滑、
   * 之后还会继续滑 —— 窗口跟着指针走、球在窗口里滑，两段位移叠起来就是「球从鼠标底下漂走」。
   * 等这 0.22s 走完再拖，球已经停在悬停槽，此后拖多久都纹丝不动（球也不再需要被「摆回槽里」，
   * 所以锚点只用算指针那一段）。滑动结束后的下一次移动会把这次拖补上。
   *
   * 直接问「现在有没有在跑的 left/top 过渡」，而不是用 transitionstart/end 记账：账本会被
   * 打断过渡的样式变化（切窗口时的 `transition: none`）留在「正在滑」里出不来 —— 那样球就
   * 再也拖不动了。
   */
  function isSliding(): boolean {
    if (!orbEl) return false;
    return orbEl.getAnimations().some((a) => {
      const t = a as CSSTransition;
      return typeof t.transitionProperty === 'string'
        && SLIDE_PROPS.includes(t.transitionProperty)
        && a.playState !== 'finished';
    });
  }

  /** 起拖：锚点校正与「交给 OS」都在后端一次做完（见 `App.beginOrbDrag`） */
  function startWindowDrag() {
    started = true;
    onDragStart({ pressX: downX, pressY: downY });
  }

  function handlePointerDown(e: OrbPointerEvent) {
    if (e.button !== 0) return;
    downX = e.clientX;
    downY = e.clientY;
    pressed = true;
    moved = false;
    started = false;
    e.currentTarget.setPointerCapture(e.pointerId);
  }

  function handlePointerMove(e: OrbPointerEvent) {
    if (!pressed || started || e.buttons !== 1) return;
    const dx = e.clientX - downX;
    const dy = e.clientY - downY;
    if (!moved) {
      if (dx * dx + dy * dy <= DRAG_THRESHOLD_PX * DRAG_THRESHOLD_PX) return;
      moved = true;   // 到此为止这一按已经算拖动，松手不再当单击
    }
    if (isSliding()) return;   // 球还在滑：这一帧先不起拖，滑动结束后的下一次移动补上
    startWindowDrag();
  }

  function handlePointerUp(e: OrbPointerEvent) {
    if (e.button !== 0) return;
    pressed = false;
    if (e.currentTarget.hasPointerCapture(e.pointerId)) {
      e.currentTarget.releasePointerCapture(e.pointerId);
    }
    if (!moved) onToggle();
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
  bind:this={orbEl}
  aria-label={expanded ? '收起 OrbDrop 面板' : '展开 OrbDrop 面板'}
  aria-expanded={expanded}
  onpointerdown={handlePointerDown}
  onpointermove={handlePointerMove}
  onpointerup={handlePointerUp}
  onpointercancel={() => { pressed = false; moved = true; }}
  onkeydown={handleKeyDown}
>
  <span class="mark">
    <span class="pulse"></span>
    <Logo size={30} />
  </span>
  <span class="orb-badge num" hidden={badgeCount === 0}>{badgeCount}</span>
  <!-- 左上角 = 待处理的通知数（推送邀请等），与右上角的「长连接设备数」对称；
       黄色是"要你看一眼"，灰绿是"当前有多少条链路"。 -->
  <span class="orb-badge notif num" hidden={notifCount === 0}>{notifCount}</span>
</button>