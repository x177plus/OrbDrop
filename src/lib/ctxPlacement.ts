/**
 * 右键菜单：贴着指针出现，并且**永远留在容器内**。
 *
 * 面板里的菜单（共享项 / 设备 / 对端）与文件选择框里的菜单（目录行 / 「常用」芯片）共用这一份：
 * 空间不够时各写一套"翻边 + 夹取"必然会分叉 —— 一处翻了、另一处顶出边界，用户就得在两个地方
 * 各报一次同样的 bug。
 *
 * 规则（与系统右键菜单一致）：
 *   右下放得下 → 放在指针右下；
 *   右边放不下 → **翻到指针左侧**（菜单右边缘贴着指针，光标仍落在菜单角上）；
 *   下边放不下 → 同理翻到上边；
 *   翻过去还越界（菜单比容器还大之类）→ 夹进容器，四周留 `MARGIN`。
 */
type Bounds = { width: number; height: number };

type Args = {
  /** 期望位置：指针在**定位参照系**里的坐标（元素 `fixed` = 视口；`absolute` = 容器内）。 */
  x: number;
  y: number;
  /** 允许占用的区域（与 x/y 同一参照系）。 */
  bounds: Bounds;
};

export function ctxPlacement(node: HTMLElement, initial: Args) {
  const MARGIN = 8;

  let args = initial;

  const clamp = (v: number, lo: number, hi: number) => Math.max(lo, Math.min(v, Math.max(lo, hi)));

  function place() {
    // 量**真实**尺寸再定：菜单的项目数差得很远（设备菜单十几条、芯片菜单两条），
    // 按"约 200×250"估一个必然有一半情况不准 —— 那正是菜单顶出边界的直接原因。
    const w = node.offsetWidth;
    const h = node.offsetHeight;

    const left = args.x + w + MARGIN > args.bounds.width ? args.x - w : args.x;
    const top = args.y + h + MARGIN > args.bounds.height ? args.y - h : args.y;

    node.style.left = `${clamp(left, MARGIN, args.bounds.width - w - MARGIN)}px`;
    node.style.top = `${clamp(top, MARGIN, args.bounds.height - h - MARGIN)}px`;
  }

  place();

  return {
    update(next: Args) {
      args = next;
      place();
    },
  };
}