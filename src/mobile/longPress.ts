/**
 * 长按手势（移动端的"右键"）。
 *
 * 与设备行（`tabs/DevicesTab.svelte`）同一套参数：按住 500ms 触发，指针挪动超过 8px
 * 视为在滑列表并撤销。WebView 自己的长按（选字 / 系统菜单）由 CSS 的
 * `user-select: none` 与 `-webkit-touch-callout: none` 挡掉（见 `mobile.css` 的
 * `.lrow` / `.ltree .node`）。
 *
 * 用法：
 * - `pointerdown` → `start`
 * - `pointermove` → `move`
 * - `pointerup` / `pointercancel` / `pointerleave` → `end`
 * - 随后的 `click` 先问 `consumed()`：长按已经开过菜单，松手时浏览器补的那一次 click
 *   不该再算短按（否则菜单刚开就又展开一次目录）。
 */
export interface LongPress {
  start(e: PointerEvent): void;
  move(e: PointerEvent): void;
  end(): void;
  consumed(): boolean;
}

export function createLongPress(onLongPress: () => void, ms = 500, slop = 8): LongPress {
  let timer: ReturnType<typeof setTimeout> | null = null;
  let from = { x: 0, y: 0 };
  let firedAt = 0;

  function clear(): void {
    if (timer) {
      clearTimeout(timer);
      timer = null;
    }
  }

  return {
    start(e) {
      // 只认主键 / 触摸；鼠标右键、中键不参与。
      if (e.button !== 0) return;
      from = { x: e.clientX, y: e.clientY };
      clear();
      timer = setTimeout(() => {
        timer = null;
        firedAt = Date.now();
        onLongPress();
      }, ms);
    },
    move(e) {
      if (!timer) return;
      if (Math.abs(e.clientX - from.x) > slop || Math.abs(e.clientY - from.y) > slop) clear();
    },
    end() {
      clear();
    },
    consumed() {
      return Date.now() - firedAt < 500;
    },
  };
}