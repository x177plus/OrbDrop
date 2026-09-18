import { defineConfig } from "vite";
import { svelte } from "@sveltejs/vite-plugin-svelte";
import { fileURLToPath, URL } from "node:url";
import process from "node:process";
const host = process.env.TAURI_DEV_HOST;

export default defineConfig(() => ({
  plugins: [svelte()],
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