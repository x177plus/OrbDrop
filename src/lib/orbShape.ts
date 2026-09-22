import type { OrbShape } from '$lib/types';
import type { ScreenRect } from '$lib/orbLayout';

/**
 * 折叠态悬浮形态的几何：圆球（`Orb.svelte`）与圆角矩形悬浮框（`RectOrb.svelte`）。
 *
 * 两者都是「窗口 = 悬浮体 + 四周余量」：窗口是那个置顶的透明 os 窗口，悬浮体是里面真正
 * 画出来的东西。位置由 `App.svelte` 换算成 CSS 变量（`--orb-x/y`、`--zone-*`），
 * 这里只回答「窗口多大、悬浮体在窗口里的哪一块」。全部 CSS px。
 *
 * ── 圆球 ──────────────────────────────────────────────
 * 窗口边长 90（= tauri.conf 里 main 窗口的 width/height），球 62 居中，四周各 14。
 * 这个 14 是**阴影余量**：`--shadow` 的可见半径左右 12（main.css 有实测值），
 * 落到窗口边上会被裁出一条切线，所以余量必须 ≥ 12。
 *
 * ── 圆角矩形悬浮框 ────────────────────────────────────
 * 尺寸/间距逐值取自 `design/orbdrop-rect-orb.html`，**收起态是定尺**：
 *   卡片 176×134（`--rq-w` / `--rq-card-h`），右侧分页点列 20 宽、与卡片间距 10。
 *   点列那 30px（间距 + 列宽）**永远占着** —— 哪怕只有本机一页、点列一颗都不画。
 *   于是窗口宽度不随设备接入/断开变，也不随 IP 长短变（卡片头那行超宽时由 `.rq-ip`
 *   自己省略）：一件常驻桌面的东西不该在设备上线下线时自己胀缩。
 * 四周余量：卡片自己的阴影（左右 12 / 上 7 / 下 17）+ 两枚角标**探出卡片 5px**
 * （原型 `.rq-shellbadge` 的 `top/left/right: -5px`）—— 左 14 / 上 10 是照这两项取的；
 * 右侧与底部见 `RECT_SLACK`。
 *
 * 与后端的一致性：`src-tauri/src/lib.rs` 的 `collapsed_logical` 用同一组数字算首启落位与
 * 首帧尺寸（圆角矩形那一档写死 236×162，就是这里的定尺）；`config.rs` 的
 * `collapsed_center_inset` 用 `body` 算"本体中心 → 窗口左上角"的内缩
 * （圆球 45/45、卡片 102/77），落盘位置（`orbCenter`）与 `body` 的中心也靠它对齐。
 */

/** 圆球直径（CSS px）。 */
export const ORB_SIZE = 62;

/** 圆球折叠态窗口边长（= tauri.conf 里 main 窗口的 width/height）。 */
export const CIRCLE_COLLAPSED = 90;

/** 卡片内容宽（原型 `--rq-w`）：两列各容纳 图标 + 约 5 个字，超出右端渐隐。定尺，不随 IP 撑开。 */
export const RECT_CARD_W = 176;

/** 卡片高（原型 `--rq-card-h`）= 上下 padding 8×2 + 头 29 + 间距 5 + 列表可视 84。 */
export const RECT_CARD_H = 134;

/** 卡片与右侧分页点列的间距（原型 `.rq-wrap` 的 gap）。 */
export const RECT_DOTS_GAP = 10;

/** 右侧分页点列宽度（原型 `.rq-dots`）。 */
export const RECT_DOTS_W = 20;

/**
 * 贴边收起后**仍露在屏幕内**的宽度：**定尺 47** —— 就是卡片靠边那一条（卡片本体被屏幕边裁掉，
 * 只剩它）。两列：**图标 + 设备名同一列**、`ip:port` 一列，都竖排（一列只占一个行高，
 * 与内容长短无关 → 写死）：
 *   1 描边 + 6 内边距 + 16（设备名列，图标也在这一列里）+ 4 + 13（ip 列）+ 6 + 1 = 47。
 * 与 `main.css` 的 `--rq-dock-*` 那组 token 必须同值，改一处要改两处。
 *
 * 与之相对，**每一列的高度**才随内容变（16 字符的 `ip:port` 竖排约 94），由 CSS 封顶、
 * 超了省略号收尾（见 `main.css` 的 `.rq-dock-name` / `.rq-dock-ip`）。
 */
export const RECT_DOCK_W = 47;

/** 窗口四周的余量：卡片阴影 + 角标探出的那一截，一个像素都不能少（见文件头的说明）。
 *  底部 18：轨道（高 = 卡片 1/2、垂直居中）比卡片矮，它下缘比卡片下沿高约 33px，那道 15px
 *  的下阴影落在卡片下沿**以上**，占主导的仍是卡片自己那笔 17px。
 *  右侧 16 = **有点列**那一档、也是唯一一档：轨道自己也投 `--shadow`，实测它右缘的可见半径
 *  是 13~14px（比卡片还多，因为它窄），而这 30px 的"间距 + 点列"区已经吃不下它。 */
export const RECT_SLACK = { left: 14, top: 10, right: 16, bottom: 18 };

/** 悬浮体在折叠态窗口内占的那一块（CSS px）。 */
export type CollapsedLayout = {
  /** 折叠态窗口尺寸 */
  size: { w: number; h: number };
  /**
   * **真正画出来的那件东西**在窗口内的矩形：球的圆盘 / 卡片那张白底（不含右侧分页点列）。
   *
   * 位置的参考点是它的**中心**（`App.svelte` 的 `orbAnchor`、`orbLayout.planExpansion`）：面板的
   * 可视盒角就落在这个中心上。两种形态用它各自的中心 —— 球是圆心、卡片是白底中心，所以它
   * 和 `widget` 在圆角矩形那一档不同（后者含右侧「间距 10 + 点列 20」，中心会右偏 15px）。
   */
  body: ScreenRect;
  /**
   * 悬浮球 / 悬浮框（含右侧分页点）在窗口内的矩形：`--orb-x/y`、悬停热区、贴边判定都由它来。
   *
   * 折叠态位置记的是这个矩形的左上角（`windowFromCenter` 反推出来的窗口 + 内缩）：两种形态的
   * 内缩不同（球 14/14、卡片 14/10），所以切形态只挪窗口，悬浮体一动不动。
   */
  widget: ScreenRect;
};

/** 圆球：窗口 90×90，球 62 居中（本体就是整颗球，与 `widget` 相同）。 */
export function circleLayout(): CollapsedLayout {
  const inset = (CIRCLE_COLLAPSED - ORB_SIZE) / 2;
  const ball: ScreenRect = { x: inset, y: inset, w: ORB_SIZE, h: ORB_SIZE };
  return {
    size: { w: CIRCLE_COLLAPSED, h: CIRCLE_COLLAPSED },
    body: ball,
    widget: ball,
  };
}

/**
 * 圆角矩形悬浮框：窗口 = 卡片 + 点列那 30px + 四周余量 = **236×162**。
 * 定尺，没有参数：点列画不画、卡片里 IP 多长都不改窗口尺寸（见文件头）。
 *
 * 本体只算**卡片那张白底**（`body` = 176×134）：中心压在面板角上的是它，不是含点列那一版。
 */
export function rectLayout(): CollapsedLayout {
  const trail = RECT_DOTS_GAP + RECT_DOTS_W;
  return {
    size: {
      w: RECT_SLACK.left + RECT_CARD_W + trail + RECT_SLACK.right,
      h: RECT_SLACK.top + RECT_CARD_H + RECT_SLACK.bottom,
    },
    body: {
      x: RECT_SLACK.left,
      y: RECT_SLACK.top,
      w: RECT_CARD_W,
      h: RECT_CARD_H,
    },
    widget: {
      x: RECT_SLACK.left,
      y: RECT_SLACK.top,
      w: RECT_CARD_W + trail,
      h: RECT_CARD_H,
    },
  };
}

export function collapsedLayout(shape: OrbShape): CollapsedLayout {
  return shape === 'rect' ? rectLayout() : circleLayout();
}