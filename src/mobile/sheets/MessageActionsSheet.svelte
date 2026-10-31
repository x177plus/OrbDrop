<script lang="ts">
  import Sheet from "../components/Sheet.svelte";
  import Icon from "../Icon.svelte";
  import * as store from "../store.svelte";
  import { t } from "$lib/i18n.svelte";

  // 长按一条消息后弹出的操作表 —— 移动端的"消息右键菜单"（桌面端见 `ContextMenu.svelte`
  // 的 `kind === 'chat'`）。当前只有一项：复制整条。以后要加"转发 / 引用"之类，就长在这张表里，
  // 不要另起一套手势。
  let {
    copyText,
    desc,
    onClose,
  }: { copyText: string; desc: string; onClose: () => void } = $props();

  async function copyAll(): Promise<void> {
    // 先把要写的值取出来（关表后 props 就读不到了），并且**趁表还开着**调剪贴板：
    // Android WebView 的 `navigator.clipboard` 要求文档有焦点，先 `onClose` 再写会落成
    // `Document is not focused`（「复制路径」那条已知问题同源）。写失败也照样关表。
    const text = copyText;
    try {
      await navigator.clipboard.writeText(text);
      onClose();
      store.toast(t("已复制整条消息"), "copy");
    } catch (e) {
      onClose();
      // 不说"请手动选中文字"：手机端为了这条长按手势关掉了系统选字（见 `mobile.css`），
      // 那句提示在这里做不到。把后端/WebView 的原话给出去。
      store.toast(t("复制失败：{error}", { error: e instanceof Error ? e.message : String(e) }), "x");
    }
  }
</script>

<Sheet title={t("消息内容")} {desc} label={t("消息操作")} {onClose}>
  <button class="opt" type="button" onclick={() => void copyAll()}>
    <span class="oi"><Icon name="copy" /></span>
    <span class="om">
      <span class="on">{t("复制整条消息")}</span>
    </span>
  </button>
</Sheet>