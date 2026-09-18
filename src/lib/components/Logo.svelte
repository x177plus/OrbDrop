<script lang="ts">
  // 全项目唯一的品牌标记：一个圆盘，中间挖出 D（OrbDrop 的球 + Drop 的首字母）。
  // 悬浮球（30）、面板/设置的左上角（20）都用这里；原生图标（托盘、打包图标）是同一形状
  // 离屏栅格化的结果，改这个 `d` 时原生图标要一起重出（`npx tauri icon <1024px PNG>`）。
  // 颜色取 `--accent`（与项目主色同源），因此跟随主题，不写死色值。
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