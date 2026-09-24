import { mount } from "svelte";

// 平台入口：移动端产物（`npm run build:mobile` / `npm run dev:mobile`）挂移动端根组件，
// 桌面端产物挂 `App.svelte`。
//
// `import.meta.env.MODE` 在构建时被替换成字面量，所以三元里那一支连同它的动态 import
// 会被完全摇掉 —— 两个产物各自只含自己那套 UI，桌面端代码不会进移动端包（反之亦然）。
// 用 `.then` 而不是顶层 await：入口脚本保持对构建目标的旧语法兼容，不引入新的语法门槛。
const entry = import.meta.env.MODE === "mobile"
  ? import("./mobile/MobileApp.svelte")
  : import("./App.svelte");

entry.then(({ default: Root }) => {
  mount(Root, { target: document.getElementById("app")! });
});