<script lang="ts">
  // 全项目唯一的品牌标记：一个圆盘，中间挖出 D（OrbDrop 的球 + Drop 的首字母）。
  // 悬浮球（30）、面板/设置的左上角（20）都用这里；原生图标（托盘、打包图标）是同一形状
  // 离屏栅格化的结果，改这个 `d` 时原生图标要一起重出：`npx tauri icon src-tauri/icons/app-icon.json`
  // （清单；**安卓那枚的 D 另写在 `src-tauri/icons/android-fg.svg` 里，是同一段路径同一变换，
  // 改这里时那份也要跟着改**）。
  // **安卓与 PC 共用同一个源图**（`src-tauri/icons/icon.png`，512 够所有密度）：同一条命令
  // 会一并写出 `gen/android/.../res/mipmap-*`（含自适应图标的 `mipmap-anydpi-v26/`），所以两端
  // 不会跑偏 —— 重出之后 `src-tauri/icons/` 与 `gen/android` 下那几份要一起提交，否则只有一端是新的。
  // 颜色取 `--accent`（与项目主色同源），因此跟随主题，不写死色值。
  //
  // **安卓自适应图标的底色必须是蓝的、且 D 要由前景自己画**（清单里的 `bg_color` 与 `android_fg`
  // 是一件事的两半）：iOS 之外的图标外形由启动器遮罩（圆角矩形/圆形/圆方）决定，可任何把图标画到
  // 遮罩之外的场合（从应用返回桌面那一下的过渡动画就是）会把整层露出来。原来背景层是白 `#fff`、
  // 而 D 是前景圆盘上挖的**洞**（洞由背景上色）——于是露出来的就是一圈白边/白角。
  // 现在背景 = `--accent` 那个蓝（`#1779e1`），D 由 `android-fg.svg` 画成白色：启动器稳态下一模一样
  // （圆盘本就和底色同色，形状由遮罩给），露出来的部分永远是蓝的。`bg_color` 同时是 iOS 图标的底色，
  // 但本项目没有 iOS 目标。
  let { size = 20 }: { size?: number } = $props();
</script>

<svg viewBox="0 0 32 32" width={size} height={size} fill="none" aria-hidden="true">
  <path
    fill-rule="evenodd"
    clip-rule="evenodd"
    d="M16 2.6A13.4 13.4 0 1 0 16 29.4A13.4 13.4 0 1 0 16 2.6ZM13 9.4H16.4A6.6 6.6 0 0 1 16.4 22.6H13Z"
    fill="var(--accent)"
  />
</svg>