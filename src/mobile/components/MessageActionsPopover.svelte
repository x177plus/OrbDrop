<script lang="ts">
  import Icon from "../Icon.svelte";
  import * as store from "../store.svelte";
  import * as api from "$lib/api";
  import { t } from "$lib/i18n.svelte";

  // 长按一条消息后贴在气泡旁边的「复制」小气泡 —— 移动端的"消息右键菜单"
  // （桌面端见 `ContextMenu.svelte` 的 `kind === 'chat'`）。当前只有一项：复制整条。
  //
  // **为什么不是底板**：长按只为一个动作，而底板要铺满整屏的遮罩、再从底部推一张表上来 ——
  // 动作被推到离手指按着的那个气泡最远的地方，会话还整个压暗了。这里只弹一颗贴着气泡的
  // 小气泡，点别处即收，与系统"长按文字弹复制工具栏"是同一套手感。
  // 以后要加"转发 / 引用"之类，就往这颗小气泡里加一项（它自己会长宽），不要退回底板。
  let {
    copyText,
    layout,
    above,
    /** 尾巴靠哪边：`me` 的气泡靠右，尾巴也跟着靠右（指着气泡靠近屏幕边缘的那一角）。 */
    side,
    onClose,
  }: {
    copyText: string;
    /** 视口坐标下这颗小气泡的落点：只写其中两条边，见 `ChatScreen.openMessageActions`。 */
    layout: { left?: number; right?: number; top?: number; bottom?: number };
    /** 挂在气泡上方（尾巴朝下）还是下方（尾巴朝上）。 */
    above: boolean;
    side: "l" | "r";
    onClose: () => void;
  } = $props();

  const pos = $derived(
    [
      layout.left !== undefined ? `left:${layout.left}px` : "",
      layout.right !== undefined ? `right:${layout.right}px` : "",
      layout.top !== undefined ? `top:${layout.top}px` : "",
      layout.bottom !== undefined ? `bottom:${layout.bottom}px` : "",
    ]
      .filter(Boolean)
      .join(";"),
  );

  /**
   * 一打开就把焦点放到这颗小气泡上（`tabindex="-1"` 的容器，见下面模板）：读屏这时会念出
   * "消息操作"，外接键盘也接得上（Esc 收起，见 `ChatScreen`）。**不把焦点直接放到按钮上**：
   * 触摸长按之后由脚本挪过去的焦点会被算作 `:focus-visible`，那颗按钮就顶着一圈蓝色描边 ——
   * 按钮上的描边留给真正的键盘 Tab。`preventScroll` 是必须的 —— 聚焦非输入控件也可能把
   * WebView 滚一下，浮层就跳离它指的那颗气泡了。
   */
  let popEl = $state<HTMLDivElement | null>(null);
  $effect(() => {
    popEl?.focus({ preventScroll: true });
  });

  async function copyAll(): Promise<void> {
    // 先把要写的值取出来（收起后 props 就读不到了），并且**趁小气泡还开着**调复制：
    // 写剪贴板走原生那条命令（`api.copyText`）—— Android WebView 的
    // `navigator.clipboard.writeText` 永远抛 `NotAllowedError: Write permission denied.`，
    // 原生的 `ClipboardManager` 既不要求焦点、也不要任何安卓权限（见 `src-tauri/src/clipboard.rs`）。
    // 写失败也照样收起。
    const text = copyText;
    try {
      await api.copyText(text);
      onClose();
      store.toast(t("已复制整条消息"), "copy");
    } catch (e) {
      onClose();
      // 不说"请手动选中文字"：手机端为了这条长按手势关掉了系统选字（见 `mobile.css`），
      // 那句提示在这里做不到。把后端/原生的原话给出去。
      store.toast(t("复制失败：{error}", { error: e instanceof Error ? e.message : String(e) }), "x");
    }
  }
</script>

<!-- 点别处 = 收起（与底板遮罩同一个手势），但这一层**不画底色**：会话仍然看得见，
     动作就贴着那颗气泡 —— 长按只为一个动作，把整屏压暗比动作本身重得多。 -->
<div class="msgpop-layer">
  <div
    class="msgpop-scrim"
    role="presentation"
    onclick={onClose}
    oncontextmenu={(e) => e.preventDefault()}
  ></div>
  <div
    class="msgpop"
    class:above
    class:below={!above}
    class:tail-l={side === "l"}
    class:tail-r={side === "r"}
    style={pos}
    role="toolbar"
    aria-label={t("消息操作")}
    tabindex="-1"
    bind:this={popEl}
  >
    <button
      class="msgpop-btn"
      type="button"
      aria-label={t("复制整条消息")}
      onclick={() => void copyAll()}
    >
      <Icon name="copy" />
      <span>{t("复制")}</span>
    </button>
  </div>
</div>