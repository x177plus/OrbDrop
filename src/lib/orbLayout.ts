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