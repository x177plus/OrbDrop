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
    /** 左上角那颗待处理数（面板里那组通知与在途推送，**加上会话未读**）。 */
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
  function startWindowDrag(pointerId: number) {
    started = true;
    /* 交给 OS 之前**必须放掉指针捕获**。`start_dragging` 之后页面再也收不到 pointerup
       （见文件头那段），捕获就没法在 `handlePointerUp` 里释放了 —— 而**留着的捕获 = 留着的
       hover**：Chromium 把捕获目标当作"指针正悬停在它上面"，于是 `.orb-zone:hover .orb` /
       `.orb:hover` 不再随指针离开而消失（贴边收起的球会一直停在滑出位）。捕获的用处只在
       阈值判定之前（那时任务已经完成），移交之后一点用都没有。 */
    if (orbEl?.hasPointerCapture(pointerId)) orbEl.releasePointerCapture(pointerId);
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
    startWindowDrag(e.pointerId);
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
  <!-- 角标臂：两颗角标分别挂在一条「臂」上（臂 `inset: 0` 铺满球的 padding box，转心就是球心）。
       为什么要这一层：贴边收起后球有大半颗在屏幕外，角标得绕球心转到**露在屏幕内的那段弧**上
       （转到哪由 App 的 `spinForDock` 按贴边方向算好，经 `--spin-l` / `--spin-r` 传给 CSS）。
       有臂才有「绕球心的自然转心」—— 否则每颗角标都得写一个手算的 `transform-origin`
       （角标中心到球心的偏移），球径/角标尺寸一改就全错。臂铺满球的 padding box，与角标原来
       相对的那个盒子是同一个，所以角标的 left/top 坐标一个像素都不用改。
       臂里那颗角标自己反向转同样的角度（转心是它自己的圆心，角度从所在臂继承）——
       角标是正圆，反向转只影响里面的数字，于是数字全程正立，转的只有位置。
       `notif` = 球的左上角那颗（黄，通知数）；`count` = 右上角那颗（绿，长连接设备数）。 -->
  <span class="orb-arm notif">
    <!-- 左上角 = 待处理数：面板里那组通知与在途推送，**再加上会话未读**（未读消息也是"有事
         等你"，面板收着的时候这颗球是唯一还看得见的提示）。与右上角的「长连接设备数」对称；
         黄色是"要你看一眼"，灰绿是"当前有多少条链路"。 -->
    <span class="orb-badge notif num" hidden={notifCount === 0}>{notifCount}</span>
  </span>
  <span class="orb-arm count">
    <span class="orb-badge num" hidden={badgeCount === 0}>{badgeCount}</span>
  </span>
</button>