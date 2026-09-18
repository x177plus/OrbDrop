import type { Snippet } from 'svelte';

/**
 * 「?」悬停说明的唯一状态：要显示什么 + 触发点是哪颗「?」。
 *
 * 说明气泡**不画在触发点旁边**，而由 `HintLayer` 统一画在面板最外层：设置面板的
 * `.sheet-body`、标签页的 `.panel-body` 都是 `overflow: auto` 的滚动容器，面板本身
 * 还是 `overflow: hidden`（`main.css`），画在触发点旁边的气泡会被这些容器按边界裁掉。
 * 所以触发点只登记状态，落位与避让窗口边缘由 `HintLayer` 负责 ——
 * 它也顺带负责"触发点滚到哪儿，气泡跟到哪儿"。
 */
export type HintState = {
  content: Snippet;
  /** 触发点本身：气泡要按它此刻的视口矩形定位（滚动后是新的矩形）。 */
  trigger: HTMLElement;
};

let current = $state<HintState | null>(null);

export function getHint(): HintState | null {
  return current;
}

/** 指针悬停 / 键盘聚焦到触发点时调用。 */
export function showHint(content: Snippet, trigger: HTMLElement) {
  current = { content, trigger };
}

export function hideHint() {
  current = null;
}