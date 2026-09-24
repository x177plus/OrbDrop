/**
 * 悬浮球展开布局：纯几何，全部用物理像素。
 *
 * 参考点只有一个：折叠态**悬浮体本体的中心**（球是圆心、卡片是白底中心，见 `orbShape.ts` 的
 * `body`）。展开时面板的**可视盒角**落在这个中心上（朝左下展开 → 盒的右上角 = 中心），
 * 面板从中心往空间足够的一侧长出；任何情况下展开窗口都在显示器工作区内夹取，保证框完整可见、
 * 不被任务栏/屏幕边缘裁掉。收起时按窗口位移把这个中心平移回去，再由调用方按**当前形态**
 * 换算成折叠态窗口左上角。
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
   * 参考点（悬浮体**本体中心**）在展开窗口内的偏移（物理像素；转 CSS 需除以 scaleFactor）。
   * 用绝对偏移而不是「贴角 + inset」：绝对 left/top 不受 CSS 视口尺寸影响，
   * 而贴 right/bottom 会依赖视口宽度/高度 —— WebView2 的视口尺寸比 OS 窗口
   * 变化滞后 1~2 帧，贴角方案会因此把悬浮体画偏。
   */
  anchorOffset: { x: number; y: number };
};

/**
 * 展开布局：**以悬浮体本体（球 / 卡片）的中心为唯一参考点**，面板的**可视盒角**落在这一点上。
 *
 * 规则：面板的盒（`box`）从中心往选定方向长，也就是「朝左下展开 → 盒的右上角 = 中心」，
 * 其余方向同理。于是悬浮体永远被面板的角**切掉一半**（球心/卡片中心正好压在角的顶点上），
 * 两形态四方向完全一致。
 *
 * 为什么参考点是中心而不是左上角、也不是悬浮体的某条边：
 *   · 落位公式里**不出现悬浮体尺寸** → 同一个中心，球（62）与卡片（176×134）展开出的面板
 *     **逐像素相同**（拿边算的话，位置里会带上尺寸，两形态差一个悬浮体尺寸：朝左 144px）。
 *   · 又因为"点在悬浮体内部半个身位处"，面板天然压住悬浮体那一块 —— 不需要额外规则。
 *
 * 盒外的 `slack`（面板四周的阴影余量）由调用方给：窗口 = 盒 + 四周一圈 slack，所以窗口比盒大
 * `slack` 两边、落位时也要把这一圈算进去，否则压在角上的会是窗口边（露出整条阴影余量的缝）。
 *
 * 装不下时（工作区比面板小）先收盒，再夹进工作区；贴边那一档把该轴 `margin` 给 0 —— 此时
 * 中心多半已经在屏幕外（贴边收起把悬浮体推出了大半），夹取会把窗口顶在工作区边上，面板就
 * 贴着屏幕边（正是贴边档要的效果）。
 */
export function planExpansion(input: {
  /** 折叠态悬浮体本体的中心（屏幕物理像素） */
  center: { x: number; y: number };
  /** 悬浮体所在显示器的工作区（不含任务栏/Dock） */
  work: ScreenRect;
  /** 面板可视盒尺寸（不含四周阴影余量） */
  box: { w: number; h: number };
  /** 面板四周留给阴影/投影的余量（窗口比盒大出的那一圈） */
  slack: number;
  /** 各轴与工作区边缘的最小间距（贴边那一档该轴给 0：窗口贴住工作区边） */
  margin: { x: number; y: number };
}): ExpansionPlan {
  const { center, work, box, slack, margin } = input;

  // 工作区放不下目标尺寸时先收盒（面板随之收缩），「完整可见」优先于标称尺寸
  const bw = Math.min(box.w, Math.max(1, work.w - (margin.x + slack) * 2));
  const bh = Math.min(box.h, Math.max(1, work.h - (margin.y + slack) * 2));
  const w = Math.round(bw + slack * 2);
  const h = Math.round(bh + slack * 2);

  // 参考点到工作区各边的余量：盒（连同它那一侧要占的 slack 与边距）要整个装进其中一边
  const needX = bw + slack + margin.x;
  const needY = bh + slack + margin.y;
  const spaceRight = work.x + work.w - center.x;
  const spaceLeft = center.x - work.x;
  const spaceDown = work.y + work.h - center.y;
  const spaceUp = center.y - work.y;

  // 哪边装得下就朝哪边；两边都不够时朝空间更大的一边
  const openRight = spaceRight >= needX ? true : spaceLeft >= needX ? false : spaceRight >= spaceLeft;
  const openDown = spaceDown >= needY ? true : spaceUp >= needY ? false : spaceDown >= spaceUp;

  // 盒角落在中心上：朝右/下 → 盒的近边 = 中心；朝左/上 → 盒的远边 = 中心
  const atX = openRight ? center.x - slack : center.x - bw - slack;
  const atY = openDown ? center.y - slack : center.y - bh - slack;
  const x = Math.min(
    Math.max(atX, work.x + margin.x),
    work.x + work.w - w - margin.x,
  );
  const y = Math.min(
    Math.max(atY, work.y + margin.y),
    work.y + work.h - h - margin.y,
  );
  const win: ScreenRect = { x: Math.round(x), y: Math.round(y), w, h };

  return {
    window: win,
    anchorOffset: { x: center.x - win.x, y: center.y - win.y },
  };
}

/** 收起布局：把参考点（悬浮体本体中心）按窗口的位移整体平移。
 *
 * 展开态下用户拖动窗口（标题栏拖动）时，悬浮体相对窗口的偏移不变，所以收起后的参考点 =
 * 展开时的参考点 + 窗口位移 —— 悬浮体仍落在面板里它原来所在的那一角，而不是跳回
 * 展开前那块屏幕坐标。落位由调用方接着算：按**当前形态**把参考点换算成折叠态窗口左上角
 * （`App.svelte` 的 `windowFromCenter`），再交给 `planDock`（圆球）/ `planRectDock`（悬浮框）判吸附
 * —— 面板开着时改过形态，也只会挪窗口，悬浮体一动不动。
 */
export function planCollapse(input: {
  /** 展开时记下的悬浮体本体中心（屏幕物理像素） */
  anchor: { x: number; y: number };
  /** 展开时记下的展开窗口位置（物理像素） */
  expanded: { x: number; y: number };
  /** 展开窗口当前位置（拖动后与 expanded 不同；物理像素） */
  current: { x: number; y: number };
}): { x: number; y: number } {
  const { anchor, expanded, current } = input;
  return { x: anchor.x + current.x - expanded.x, y: anchor.y + current.y - expanded.y };
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
   * **收起态（未悬停）的悬停热区**（窗口内偏移 + 尺寸，物理像素）= 屏上**看得见的那一条**：
   * 圆球是露出那截弧的方框、悬浮框是那条竖条。呼出阈值就是它 —— 悬浮体滑出去之后**才会**占的
   * 那一块不吃指针（否则收起态在屏幕边上圈着一大块，指针从旁边路过就把它呼出来，压在下面的
   * 窗口也点不着）。
   *
   * 呼出**之后**的热区是它 ∪ 完整悬浮体，由 `App.collapsedHitRects` 现拼：那时悬浮体确实已经
   * 滑到完整位上，指针就落在它上面；区域不含那一块的话，指针一离开那一条，窗口就当场穿透、
   * hover 丢掉，悬浮体又收回去 —— 一进一出抖个不停。
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
 *
 * **呼出阈值 = 露在屏内那一小条**（`zone`）：球收进去之后要占的那块**不算**热区 ——
 * 它占了，收起态就等于在屏幕边上圈了一整颗球的方框，指针从旁边路过就把球呼出来，压在下面的
 * 窗口也点不着（见 `DockPlan.zone`）。球滑回完整位之后指针落在球上，那时才把球并进热区
 * （`App.collapsedHitRects`）—— 热区本身**固定不动**：跟着球走的话，指针会被滑走的球甩下、
 * 球随即滑回又被指针压到，一进一出抖个不停。
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
  /** 贴边收起后**仍露在屏幕内**的球宽度（= `orbSize - overhang`）：呼出阈值就只算这一截。 */
  const expose = orbSize - overhang;

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
      // 热区 = 屏上**看得见的那一条**：贴边侧从窗口边缘起算，长度就是露在屏内的那截 `expose`；
      // 未贴边（d === 0）时整颗球都在屏内，热区就是球本身
      zone: d === 0 ? orbInset : d === -1 ? 0 : size - expose,
      zoneSize: d === 0 ? orbSize : expose,
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

/**
 * 圆角矩形悬浮框的吸附布局：**只贴左右两条边** —— 窗口贴住工作区左/右边，悬浮框横向平移
 * 出窗口，露在屏幕内的正好是一条**竖条**（宽 = `peek`，里面竖排着图标 / 设备名 / ip:port）。
 *
 * 与圆球同一套语义：窗口永远整个留在工作区内（"大半张卡在屏幕外"靠把悬浮框滑到窗口外、
 * 被窗口裁掉实现），贴边点必须落在工作区边缘上；悬停时悬浮框滑回完整位（`--orb-inset`）。
 * 只有**观感**不同：卡片不"滑回来"，而是内容交叉淡入淡出（见 `main.css` 的 `.rq-dock`）。
 *
 * 竖条钉在悬浮框**贴边那一端**（贴右钉在左端、贴左钉在右端，位置由 `RECT_DOCK_INSET` 定），
 * 于是露出的那 `peek` 宽正是卡片贴边侧的那一竖条 —— 竖条与它占同一块屏幕区域。
 *
 * **呼出阈值 = 那条竖条**（`zone`）：卡片滑回来之后要占的那一大片**不算**热区 —— 算进来的话，
 * 收起态就是在屏幕边上圈了一张卡片那么大的方框（222×134），指针从旁边路过就把卡片呼出来，
 * 压在下面的窗口也点不着（见 `DockPlan.zone`）。卡片滑回完整位之后指针落在卡片上，那时才把
 * 卡片并进热区（`App.collapsedHitRects`）。
 *
 * 竖向不贴（与圆球不同）：卡片是横幅，贴上边露的是列表尾部、贴左边露的是分页点列 ——
 * 收起态读不出东西的贴边位不如不要；上下两条边按 `margin` 夹进工作区就算了。
 *
 * 判吸附按**悬浮框**（卡片 + 右侧点列，也就是窗口围绕的那个盒子）的左右缘算，与圆球按
 * 球缘算是同一条规则 —— 圆球那一档球就是悬浮框。**不能按卡片算**：贴右边时窗口贴住工作区
 * 右缘，卡片（在窗口内只占左边 14 + 176）离屏边还有 30 + 16 = 46px，照卡片复判一次就会掉回
 * 未贴边，而复原只存左上角、靠"同一个矩形再判一次得到同一结果"（`planDock` 里那条）——
 * 形态会在重启后丢掉。按悬浮框算则贴边后余量正好是 `RECT_SLACK.left` / `.right`（14 / 16，
 * 都小于 `threshold`），复判幂等。
 */
export function planRectDock(input: {
  /** 自由落位的折叠态窗口矩形（物理像素） */
  window: ScreenRect;
  /** 悬浮框所在显示器的工作区（物理像素） */
  work: ScreenRect;
  /** 露出的竖条宽度 = 吸附后仍留在屏内的宽度（物理像素） */
  peek: number;
  /** 悬浮框（卡片 + 点列）在窗口内的矩形（物理像素） */
  widget: ScreenRect;
  /** 卡片本身（不含右侧点列）的宽度（物理像素）：卡片贴边收起滑到哪个位置按它算 */
  cardW: number;
  /** 吸附阈值：悬浮框边缘距工作区边缘 ≤ 该值即贴边（物理像素） */
  threshold: number;
  /** 未贴边时窗口与工作区边缘的最小间距（物理像素） */
  margin: number;
}): DockPlan {
  const { window: win, work, peek, widget, cardW, threshold, margin } = input;
  // 工作区比窗口还小（极小屏 / 极端 DPI）时 max < min：宁可让窗口朝外溢出一截，
  // 也别把它推到工作区另一侧去（那样卡片会整张跑到屏幕外，拖都拖不回来）。
  const clamp = (v: number, min: number, max: number) =>
    Math.round(max < min ? min : Math.min(Math.max(v, min), max));

  const left = win.x + widget.x;
  const near = left - work.x <= threshold;                          // 悬浮框左缘离工作区左边够近了
  const far = work.x + work.w - (left + widget.w) <= threshold;     // 悬浮框右缘离工作区右边够近了
  const dock: DockAxis = near ? -1 : far ? 1 : 0;

  return {
    window: {
      x: dock === -1 ? work.x
        : dock === 1 ? work.x + work.w - win.w
        : clamp(win.x, work.x + margin, work.x + work.w - win.w - margin),
      y: clamp(win.y, work.y + margin, work.y + work.h - win.h - margin),
      w: win.w,
      h: win.h,
    },
    dock: { x: dock, y: 0 },
    // 贴边：竖条钉到贴边那一端（贴右 = 窗口最右 `peek`，贴左 = 最左 `peek`）；未贴边：悬浮框原位
    orb: {
      x: dock === -1 ? -(cardW - peek) : dock === 1 ? win.w - peek : widget.x,
      y: widget.y,
    },
    // 热区 = 屏上看得见的那条**竖条**（卡片滑出窗口后只剩它）：竖条钉在卡片贴边的那一端，
    // 所以贴左在窗口最左 `peek`、贴右在窗口最右 `peek`；未贴边就是悬浮框本身
    zone: {
      x: dock === -1 ? 0 : dock === 1 ? win.w - peek : widget.x,
      y: widget.y,
      w: dock === 0 ? widget.w : peek,
      h: widget.h,
    },
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
