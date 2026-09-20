/**
 * 悬浮球展开布局：纯几何，全部用物理像素。
 *
 * 折叠态窗口就是悬浮球本身，展开态窗口是「面板 + 四周阴影余量」。展开时悬浮球在
 * 屏幕上的位置保持不变，面板朝空间足够的一侧长出；任何情况下展开窗口都在显示器
 * 工作区内夹取，保证框完整可见、不被任务栏/屏幕边缘裁掉。
 */

export type ScreenRect = { x: number; y: number; w: number; h: number };

/** 悬浮球所在显示器的几何信息（物理像素；scaleFactor 为设备像素比） */
export type ScreenMetrics = {
  /** 工作区：已排除任务栏 / Dock */
  workArea: ScreenRect;
  monitor: ScreenRect;
  scaleFactor: number;
};

export type ExpansionPlan = {
  /** 展开后的窗口矩形（物理像素） */
  window: ScreenRect;
  /**
   * 悬浮球在展开窗口内的偏移（物理像素；转 CSS 需除以 scaleFactor）。
   * 用绝对偏移而不是「贴角 + inset」：绝对 left/top 不受 CSS 视口尺寸影响，
   * 而贴 right/bottom 会依赖视口宽度/高度 —— WebView2 的视口尺寸比 OS 窗口
   * 变化滞后 1~2 帧，贴角方案会因此把悬浮球画偏。
   */
  orbOffset: { x: number; y: number };
};

export function planExpansion(input: {
  /** 悬浮球窗口当前矩形 */
  ball: ScreenRect;
  /** 悬浮球所在显示器的工作区（不含任务栏/Dock） */
  work: ScreenRect;
  /** 展开窗口目标尺寸 */
  expanded: { w: number; h: number };
  /** 折叠态悬浮球在窗口内的内缩 = (窗口边长 - 球直径) / 2 */
  orbInset: number;
  /** 展开窗口与工作区边缘的最小间距 */
  margin: number;
}): ExpansionPlan {
  const { ball, work, expanded, orbInset, margin } = input;

  // 工作区放不下目标尺寸时先收窗口（面板随之收缩），「完整可见」优先于标称尺寸
  const w = Math.min(expanded.w, Math.max(1, work.w - margin * 2));
  const h = Math.min(expanded.h, Math.max(1, work.h - margin * 2));

  const spaceRight = work.x + work.w - (ball.x + ball.w);
  const spaceLeft = ball.x - work.x;
  const spaceDown = work.y + work.h - (ball.y + ball.h);
  const spaceUp = ball.y - work.y;

  // 哪边装得下就朝哪边；两边都不够时朝空间更大的一边
  const openRight = spaceRight >= w ? true : spaceLeft >= w ? false : spaceRight >= spaceLeft;
  const openDown = spaceDown >= h ? true : spaceUp >= h ? false : spaceDown >= spaceUp;

  const x = Math.min(
    Math.max(openRight ? ball.x : ball.x + ball.w - w, work.x + margin),
    work.x + work.w - w - margin,
  );
  const y = Math.min(
    Math.max(openDown ? ball.y : ball.y + ball.h - h, work.y + margin),
    work.y + work.h - h - margin,
  );
  const win: ScreenRect = { x: Math.round(x), y: Math.round(y), w: Math.round(w), h: Math.round(h) };

  return {
    window: win,
    orbOffset: { x: ball.x - win.x + orbInset, y: ball.y - win.y + orbInset },
  };
}

/**
 * 收起布局：把悬浮球原矩形按窗口的位移整体平移。
 *
 * 展开态下用户拖动窗口（标题栏拖动）时，悬浮球相对窗口的偏移不变，所以收起后的
 * 窗口 = 悬浮球原位置 + 窗口位移 —— 球仍贴在面板里它原来所在的那一角，而不是跳回
 * 展开前那块屏幕坐标。落位后再交给 `planDock` 判吸附。
 */
export function planCollapse(input: {
  /** 展开时记下的悬浮球窗口矩形（物理像素） */
  ball: ScreenRect;
  /** 展开时记下的展开窗口位置（物理像素） */
  expanded: { x: number; y: number };
  /** 展开窗口当前位置（拖动后与 expanded 不同；物理像素） */
  current: { x: number; y: number };
}): ScreenRect {
  const { ball, expanded, current } = input;
  return {
    x: ball.x + current.x - expanded.x,
    y: ball.y + current.y - expanded.y,
    w: ball.w,
    h: ball.h,
  };
}

/** 贴边方向：-1 = 贴左/上，1 = 贴右/下，0 = 不贴边 */
export type DockAxis = -1 | 0 | 1;

export type DockPlan = {
  /** 落位后的折叠态窗口矩形（物理像素；永远是工作区的子矩形） */
  window: ScreenRect;
  /** 各轴的贴边方向：角标要按它转到「露在屏幕内」的那段球缘上（见 `Orb.svelte` 的注释） */
  dock: { x: DockAxis; y: DockAxis };
  /** 收起（未悬停）时悬浮球在窗口内的偏移：贴边侧 = `overhang` 探出屏幕外（物理像素） */
  orb: { x: number; y: number };
  /**
   * 悬停热区（窗口内偏移 + 尺寸，物理像素）：「收起露出的半颗」与「悬停后的完整球」
   * 的并集。热区固定不动，只有球在里面滑 —— 球若跟着动，指针会滑出它，随即又滑回来，
   * 一进一出抖个不停。
   */
  zone: ScreenRect;
};

/**
 * 吸附布局：悬浮球边缘距工作区边缘 ≤ `threshold` 就贴过去、把球大半个推出屏幕外，悬停时再滑回完整可见。
 *
 * 折叠态窗口 = 悬浮球 + 四周内缩，窗口永远整个留在工作区内 —— 「大半颗在屏幕外」不是靠
 * 把窗口挪出屏幕，而是把球滑到窗口外：球被窗口裁掉，露出的正好是 `orbSize - overhang`。
 * 这样做既不用让窗口探出工作区（避免 OS 把窗口挪回来/多显示器边界怪象），动画也只是一次
 * CSS 滑动。贴边点必须落在工作区边缘上：窗口若停在离边缘还有几像素的地方，裁掉球的就是
 * 窗口边（屏幕上一条看得见的切线），而不是屏幕边。
 *
 * 未贴边的轴保持自由落位（此时距两侧边缘都 > threshold，窗口自然完整落在工作区内）。
 *
 * **横竖不会同时贴**：横向一旦贴边，纵向就保持自由落位 —— 四个角跟左右两边一样，球只往
 * 左右让出屏幕（露出来的是球的一侧，那段弧约 142°，两颗角标都放得下），而不是往角上缩成
 * 只露一个角（那样只剩约 52°，连一颗 22px 的角标都得精打细算）。于是 `dock` 只可能是
 * `(0,0)` / `(±1,0)` / `(0,±1)`，`spinForDock` 也只需要管这几种。
 */
export function planDock(input: {
  /** 自由落位的折叠态窗口矩形（物理像素） */
  ball: ScreenRect;
  /** 悬浮球所在显示器的工作区（物理像素） */
  work: ScreenRect;
  /** 悬浮球直径（物理像素） */
  orbSize: number;
  /** 悬浮球在折叠态窗口内的内缩 = (窗口边长 - 直径) / 2（物理像素） */
  orbInset: number;
  /** 贴边收起时球越出窗口（= 屏幕）外的长度（物理像素）：露在屏幕内的 = orbSize - overhang */
  overhang: number;
  /** 吸附阈值：球边缘距工作区边缘 ≤ 该值即贴边（物理像素） */
  threshold: number;
}): DockPlan {
  const { ball, work, orbSize, orbInset, overhang, threshold } = input;

  /** 单轴吸附：贴边方向 d、窗口该轴坐标 win、球在该轴的偏移 orb、悬停热区该轴的起点与长度。
   *  `allow = false`：这一轴不判吸附、保持自由落位（横向已经贴边时的纵向，见上面的说明）。 */
  const axis = (start: number, size: number, limit: number, total: number, allow = true) => {
    const free = start + orbInset;                       // 自由落位下球在该轴上的起点
    const near = free - limit <= threshold;              // 球左/上缘离工作区左/上边够近了
    const far = limit + total - (free + orbSize) <= threshold;   // 球右/下缘离工作区右/下边够近了
    const d: DockAxis = !allow ? 0 : near ? -1 : far ? 1 : 0;
    return {
      d,
      win: d === -1 ? limit : d === 1 ? limit + total - size : start,
      // 负向（左/上）：球左缘探出 overhang；正向（右/下）：球右缘探出 overhang
      orb: d === -1 ? -overhang : d === 1 ? size - orbSize + overhang : orbInset,
      // 热区 = 「露出的部分」∪「完整球」：贴边侧从窗口边缘起算，另一侧到完整球的外缘
      zone: d === -1 ? 0 : orbInset,
      zoneSize: d === 0 ? orbSize : orbSize + orbInset,
    };
  };

  const ax = axis(ball.x, ball.w, work.x, work.w);
  const ay = axis(ball.y, ball.h, work.y, work.h, ax.d === 0);   // 横向贴了，纵向就不再贴

  return {
    window: { x: ax.win, y: ay.win, w: ball.w, h: ball.h },
    dock: { x: ax.d, y: ay.d },
    orb: { x: ax.orb, y: ay.orb },
    zone: { x: ax.zone, y: ay.zone, w: ax.zoneSize, h: ay.zoneSize },
  };
}
/** 贴边收起时两颗角标各自绕球心的转角（度，正值 = 顺时针）：左 = 黄（通知数）、右 = 绿（长连接设备数） */
export type BadgeSpin = { left: number; right: number };

/** 角标原位：球的左上角（10:30）与右上角（1:30），即屏幕角度（0 = 3:00、90 = 6:00）225° 与 315° */
const HOME_LEFT = 225;
const HOME_RIGHT = 315;

/**
 * 贴边收起时两颗角标转到哪：**把它们转到「露在屏幕内那段弧」上**。
 *
 * 球贴边后有大半颗在屏幕外，弧外的角标整颗看不见。横向贴边（左/右）露出来的是球的**一侧**，
 * 那段弧以 9:00 / 3:00 为中心、半宽约 71°（`DOCK_EXPOSE = 1/3` → 露出 21px，`acos(10/31) ≈ 71°`），
 * 装得下两颗角标（角标 22px、圆心在 32.5px 处，各占约 ±20°）；纵向贴边（上/下）露出的是球的
 * 上/下半。于是只需要管三种：横向贴、纵向贴、不贴（角上不会再出现两轴同时贴，见 `planDock`）。
 *
 * 转角 = 弧心方向 − 角标原位方向：
 *   只贴右（弧心 180°）→ 两条臂都 -90（黄 225°→135° = 7:30、绿 315°→225° = 10:30，两颗都在弧内）
 *   只贴左（弧心 0°）→ 都 +90（黄→315° = 1:30、绿→45° = 4:30，两颗都在弧内）
 *   只贴上（弧心 90°）→ 黄 -90（→135° = 7:30）、绿 +90（→45° = 4:30）：**两条臂各自荡开**，
 *     各留各的一侧（不是整对 180° —— 那样两颗会掉个个儿，谁是谁都看不出来了）
 *   只贴下（弧心 270°）→ 都 0（角标原位就在球的上半，本来就露着）
 *
 * 横向贴边时「整对一起转」：角标原位在弧心 ±45° 上，让这对的**中线**对上弧心即可。
 * 这些角度都是拿真机几何量出来的：`planDock` 给出球偏移，角标 22px 摆在球缘 32.5px 处，
 * 逐个组合检查「整颗是否都落在露出的那段弧里」。改 `DOCK_EXPOSE` / 球径 / 角标尺寸都要重算。
 */
export function spinForDock(dock: { x: DockAxis; y: DockAxis }): BadgeSpin {
  const arc = arcCenterFor(dock);
  if (arc === null) return { left: 0, right: 0 };              // 未贴边：角标原位就在完整可见的球上
  if (dock.x === 0 && dock.y === -1) {
    // 只贴上：两条臂各自荡开 —— 黄留在左侧（弧心 +45°）、绿留在右侧（弧心 −45°）
    return { left: normalize(arc + 45 - HOME_LEFT), right: normalize(arc - 45 - HOME_RIGHT) };
  }
  // 横向贴（左 / 右）或只贴下：整对扫过去 —— 两颗角标对称落在弧上，让「中线」对准弧心
  const both = normalize(arc - PAIR_MID);
  return { left: both, right: both };
}

/** 一对角标的中线方向：角标原位是 10:30 / 1:30，中线指 12:00 */
const PAIR_MID = 270;

/** 露出那段弧的中心方向（屏幕角度）；未贴边 = null。横竖不会同时贴，所以这里只有三种 */
function arcCenterFor(dock: { x: DockAxis; y: DockAxis }): number | null {
  if (dock.x === 1) return 180;    // 球被推到右边：露出的在左（9:00）
  if (dock.x === -1) return 0;     // 球被推到左边：露出的在右（3:00）
  if (dock.y === -1) return 90;    // 球被推上去：露出的是球的下半（6:00）
  if (dock.y === 1) return 270;    // 球被推下去：露出的是球的上半（12:00）
  return null;
}

/** 归一到 (-180, 180]，免得 405° 这种数进 CSS（转同样的角度，但读起来要能对上） */
function normalize(deg: number): number {
  const d = ((deg + 180) % 360 + 360) % 360 - 180;
  return d === -180 ? 180 : d;
}
