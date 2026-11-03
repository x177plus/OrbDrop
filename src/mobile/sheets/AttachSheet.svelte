<script lang="ts">
  import Sheet from "../components/Sheet.svelte";
  import Icon from "../Icon.svelte";
  import * as store from "../store.svelte";
  import * as chat from "$lib/stores/chat.svelte";
  import { t } from "$lib/i18n.svelte";

  /**
   * 会话里那颗「＋」的底板（F-118 / F-119 / F-120）：**选图片**、**选文件**、**选文件夹**，
   * 还是**选应用**。
   *
   * 为什么不直接开一个框把几样都选了：这几条路的语义不同 —— 图片是**消息内容**（字节随会话
   * 消息过去，对端直接看得见），文件与文件夹是**推送**（对端收到一张「接收 / 拒绝」卡；文件夹
   * 只是清单里多了一整棵相对路径），应用也是推送但落盘名由我们定
   * （`应用名_versionName(versionCode)_包名.apk`）。同一个框里混着选，用户没法知道"我选的
   * 这张图会被当成哪一种"，自己挑一下最清楚。
   *
   * 桌面端那颗「＋」是一步到底（开自绘选择框、选出即推送），移动端多这一下是因为手机上只能
   * 借系统的三个选择器（照片选择器、文件选择器与目录选择器）—— 那正是"像桌面那样自绘一个
   * 浏览器"在安卓上走不通的代价，理由见 `PickFilePlugin.kt` 与 `mobile::pick_files`。
   */
  let { onClose }: { onClose: () => void } = $props();

  const thread = $derived(chat.activeThread());

  /**
   * 四项动作都**先关表再干活**：系统选择器要盖在整个应用上（它是另一个 Activity），底板留着
   * 只会先闪一下再被盖住。取值也在关表之前取完 —— `onClose()` 之后 props 就读不到了
   * （文件操作底板那一节记着同一个坑）。
   *
   * 「应用」那一项不借系统选择器：它要的是一张应用表，而我们自己就查得到（没有 URI、没有授权
   * 问题）—— 所以关掉这张表之后开的是**另一张底板**（`appPicker`）。
   */
  function pick(kind: "image" | "file" | "folder"): void {
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
  label={t("发送文件、文件夹、图片或应用")}
  onClose={onClose}
>
  <!-- 四项只留**图标 + 名字**：选项名已经说清了发什么，长说明只把底板撑到大半屏。
       横向四列是这一档的标准形状（图片 / 文件 / 文件夹 / 应用一眼扫完），比竖排四行短一半。 -->
  <div class="attach-grid">
    <button class="attach-item" type="button" onclick={() => pick("image")}>
      <span class="ftile image"><Icon name="image" /></span>
      <span class="attach-name">{t("图片")}</span>
    </button>
    <button class="attach-item" type="button" onclick={() => pick("file")}>
      <span class="ftile"><Icon name="file" /></span>
      <span class="attach-name">{t("文件")}</span>
    </button>
    <!-- 「文件夹」（F-120）：系统只给一次选一棵（目录选择器不支持多选），回来的是整棵树；
         原生先把树拷进缓存再推送，所以大目录回来前会有一条「正在准备文件夹…」的提示。 -->
    <button class="attach-item" type="button" onclick={() => pick("folder")}>
      <span class="ftile folder"><Icon name="folder" /></span>
      <span class="attach-name">{t("文件夹")}</span>
    </button>
    <!-- 「应用」（F-119）：发的是它自己的安装包（`base.apk`），落盘名
         `应用名_versionName(versionCode)_包名.apk`。 -->
    <button class="attach-item" type="button" onclick={pickApp}>
      <span class="ftile zip"><Icon name="zip" /></span>
      <span class="attach-name">{t("应用")}</span>
    </button>
  </div>
</Sheet>

<style>
  /* 四等分一行。间距与圆角都用 `--u`（与其余底板同一套缩放）；`minmax(0, 1fr)` 是给
     长英文名兜底 —— 普通写 `1fr` 时 min-content 会把列顶宽，360 宽上 "Folders" 会溢出一截。 */
  .attach-grid {
    display: grid;
    grid-template-columns: repeat(4, minmax(0, 1fr));
    gap: calc(8 * var(--u));
  }
  /* 不带外框：图标块自己就是视觉锚点，整格仍是 44u 以上的点击区（padding 撑起来），
     只留按压/悬停的浅底 —— 加边框反而像"四张表单"。 */
  .attach-item {
    display: flex;
    flex-direction: column;
    align-items: center;
    gap: calc(9 * var(--u));
    padding: calc(10 * var(--u)) calc(2 * var(--u));
    border: 0;
    border-radius: calc(14 * var(--u));
    background: none;
    color: inherit;
    text-align: center;
  }
  .attach-item:hover,
  .attach-item:active { background: var(--fg-soft); }
  .attach-name {
    font-size: var(--fs-sm);
    font-weight: 600;
    line-height: 1.2;
    white-space: nowrap;
  }
</style>