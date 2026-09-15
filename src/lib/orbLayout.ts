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

  /** 单轴吸附：贴边方向 d、窗口该轴坐标 win、球在该轴的偏移 orb、悬停热区该轴的起点与长度 */
  const axis = (start: number, size: number, limit: number, total: number) => {
    const free = start + orbInset;                       // 自由落位下球在该轴上的起点
    const d: DockAxis = free - limit <= threshold ? -1
      : limit + total - (free + orbSize) <= threshold ? 1
      : 0;
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
  const ay = axis(ball.y, ball.h, work.y, work.h);

  return {
    window: { x: ax.win, y: ay.win, w: ball.w, h: ball.h },
    orb: { x: ax.orb, y: ay.orb },
    zone: { x: ax.zone, y: ay.zone, w: ax.zoneSize, h: ay.zoneSize },
  };
}