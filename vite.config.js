import { defineConfig } from "vite";
import { svelte } from "@sveltejs/vite-plugin-svelte";
import { fileURLToPath, URL } from "node:url";
import process from "node:process";
const host = process.env.TAURI_DEV_HOST;

export default defineConfig(({ mode }) => ({
  plugins: [svelte()],
  // CSS **不交给 Lightning CSS**：它会对同属性的重复声明做去重 —— 而我们这套兼容性正是靠
  // "重复声明"承载的（先 sRGB、后 oklch/color-mix），实测它会把回退整条删掉（自定义属性与普通
  // 属性都删），于是老引擎又把配色丢光。回退由 `scripts/gen-color-tokens.mjs` 生成、
  // 由 `scripts/check-compat.mjs` 门禁，构建期不再插手。
  // 桌面端同样按下面这份配置走（它本来就不需要降级）。
  // 平台产物分家：`--mode mobile` 出到 `dist-mobile/`（移动端那份，见
  // `src-tauri/tauri.android.conf.json` 覆盖的 `frontendDist`），默认仍是 `dist/`（桌面端）。
  // 两个目录各含它自己那套 UI —— `src/main.ts` 按 `import.meta.env.MODE` 静态二选一，
  // 另一个分支连同它的动态 import 会被摇掉。
  build: {
    outDir: mode === "mobile" ? "dist-mobile" : "dist",
    // JS 语法也钉到下限：默认目标是 Chrome 107+，不改的话新语法（Svelte 5 运行时里的 `??=`）会被
    // 原样留在产物里，而低于 Chrome 85 的引擎**解析期就 SyntaxError** → 整块脚本作废 → 纯白屏
    // （实测：三星 SM-G8750 + 系统 WebView 81，见 `docs/p1-implementation.md` §35）。
    // 81 是移动端的实际下限：降完语法之后，剩下门槛由 `static/engine-floor.js` 兜（replaceAll 打补丁、
    // 缺 flex gap/inset 才按需加载 `legacy-gap.css`/`legacy-misc.css`）—— 现代引擎一个字节都不多花。
    target: mode === "mobile" ? "chrome81" : undefined,
    // CSS 不压缩：压缩器（lightningcss 或 esbuild）会去重同属性的重复声明，而回退正是靠它们承载的。
    // 样式表要随包走、由本地读，几十 KB 的体积无关紧要；换来的是回退声明 1:1 原样出货。
    cssMinify: mode === "mobile" ? false : undefined,
  },
  // index.html 里的 `/favicon.png` 指向 static/ —— 不声明的话这里面谁也不会被服务/拷进 dist
  publicDir: "static",
  resolve: {
    alias: {
      $lib: fileURLToPath(new URL("./src/lib", import.meta.url)),
    },
  },
  clearScreen: false,
  server: {
    port: 1420,
    strictPort: true,
    host: host || "127.0.0.1",
    hmr: host
      ? { protocol: "ws", host, port: 1421 }
      : undefined,
    watch: { ignored: ["**/src-tauri/**"] },
  },
}));