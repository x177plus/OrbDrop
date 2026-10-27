<script lang="ts">
  import Sheet from "../components/Sheet.svelte";
  import Icon from "../Icon.svelte";
  import * as store from "../store.svelte";
  import * as chat from "$lib/stores/chat.svelte";
  import { t } from "$lib/i18n.svelte";

  /**
   * 会话里那颗「＋」的底板（F-118 / F-119）：**选图片**、**选文件**、还是**选应用**。
   *
   * 为什么不直接开一个框把几样都选了：这几条路的语义不同 —— 图片是**消息内容**（字节随会话
   * 消息过去，对端直接看得见），文件是**推送**（对端收到一张「接收 / 拒绝」卡），应用也是推送
   * 但落盘名由我们定（`应用名_versionName(versionCode)_包名.apk`）。同一个框里混着选，用户
   * 没法知道"我选的这张图会被当成哪一种"，自己挑一下最清楚。
   *
   * 桌面端那颗「＋」是一步到底（开自绘选择框、选出即推送），移动端多这一下是因为手机上只能
   * 借系统的两个选择器（照片选择器与文件选择器）—— 那正是"像桌面那样自绘一个浏览器"在安卓上
   * 走不通的代价，理由见 `PickFilePlugin.kt` 与 `mobile::pick_files`。
   */
  let { onClose }: { onClose: () => void } = $props();

  const thread = $derived(chat.activeThread());

  /**
   * 三项动作都**先关表再干活**：系统选择器要盖在整个应用上（它是另一个 Activity），底板留着
   * 只会先闪一下再被盖住。取值也在关表之前取完 —— `onClose()` 之后 props 就读不到了
   * （文件操作底板那一节记着同一个坑）。
   *
   * 「应用」那一项不借系统选择器：它要的是一张应用表，而我们自己就查得到（没有 URI、没有授权
   * 问题）—— 所以关掉这张表之后开的是**另一张底板**（`appPicker`）。
   */
  function pick(kind: "image" | "file"): void {
    onClose();
    void store.pickAttach(kind);
  }

  function pickApp(): void {
    onClose();
    store.openSheet({ kind: "appPicker" });
  }
</script>

<Sheet
  title={t("发送")}
  desc={thread ? t("选择要发送到「{title}」的内容。", { title: thread.title }) : ""}
  label={t("发送文件、图片或应用")}
  onClose={onClose}
>
  <button class="opt" type="button" onclick={() => pick("image")}>
    <span class="oi"><Icon name="image" /></span>
    <span class="om">
      <span class="on">{t("图片")}</span>
      <span class="od">{t("从相册里选（老系统上退回顾照片的\"文件\"应用），选好的图落在输入区里 —— 还能配上文字再发。对端直接就能看到图。")}</span>
    </span>
  </button>
  <button class="opt" type="button" onclick={() => pick("file")}>
    <span class="oi"><Icon name="file" /></span>
    <span class="om">
      <span class="on">{t("文件")}</span>
      <span class="od">{t("从系统的文件选择器里选，选好即推送：对端收到一张「接收 / 拒绝」卡，进度与成败显示在会话里那条文件气泡上。")}</span>
    </span>
  </button>
  <!-- 「应用」（F-119）：发的是它自己的安装包（`base.apk`），落盘名
       `应用名_versionName(versionCode)_包名.apk` —— 那台设备上装的是什么版本，对方一眼看得出。
       流的是**推送**那一条（同「文件」），所以没说"对端直接就能看"。 -->
  <button class="opt" type="button" onclick={pickApp}>
    <span class="oi"><Icon name="zip" /></span>
    <span class="om">
      <span class="on">{t("应用")}</span>
      <span class="od">{t("从这台设备已安装的应用里挑一个，把它的安装包原样发过去（落盘名叫「应用名_版本名(版本号)_包名.apk」）。同「文件」：对方收到一张「接收 / 拒绝」卡。")}</span>
    </span>
  </button>
</Sheet>