/*
 * 引擎下限兜底（**ES5**，刻意不参与任何构建：产物里不许出现 `let/const`、箭头函数、模板串）。
 *
 * 为什么需要它：移动端产物的下限是 Chrome 90（`scripts/check-compat.mjs` 里的 FLOOR）。低于下限的
 * 引擎连入口 module 都过不去 —— 产物里带 `??=`（Chrome 85 起的语法），解析期就 SyntaxError，整块
 * 脚本作废：不 mount，也不注入样式（移动端那份样式表是由动态 import 的 chunk 带进去的），页面只剩
 * 一层窗口底色。用户看到的是**纯白屏**，一个字都没有 —— 实测：SM-G8750 + WebView 81.0.4044.138，
 * 见 `docs/p1-implementation.md` §22。
 *
 * 所以这一段必须满足两条：**ES5**（要在那种引擎上跑得起来）、**排在 module 之前**（非 module 的
 * 脚本先执行，module 默认 defer）。它只做一件事：探到能力缺口就把话说明白，并给一个"去商店更新
 * 系统 WebView"的入口（那个 `market://` 链接由 `mobile::store_plugin` 的导航钩子接住，
 * 见 `src-tauri/src/mobile.rs`；这里不能走 IPC —— 需要它的时候前端恰好是死的）。
 *
 * 探的每一项都是产物**真的**用到的能力，不是版本号：
 *   · `??=` 语法（Chrome 85）—— Svelte 5 运行时的 `(a.effects ??= []).push(i)`；
 *   · `String.prototype.replaceAll`（Chrome 85）—— Svelte 的 template 助手与 Vite 的预载助手在用；
 *   · flex 布局的 `gap`（Chrome 84）—— `mobile.css` 里 46 处；
 *   · `inset` 简写（Chrome 87）—— `mobile.css` 里 5 处（弹层与遮罩靠它铺满）。
 * 探测手法本身也不能用新语法：`new Function` 只在抛 **SyntaxError** 时算数 —— CSP 禁掉 eval 时抛的是
 * EvalError，那种情况下这一项跳过，不误报；`gap` 只能量（`@supports (gap:…)` 在只有 grid gap 的老
 * 引擎上同样是真，量不出来）。
 */
(function () {
  // 与 `scripts/check-compat.mjs` 的 FLOOR 同一件事，改一个记得改另一个。
  var FLOOR = "Chrome 90";
  var WEBVIEW_PKG = "com.google.android.webview";

  function esc(s) {
    return String(s)
      .replace(/&/g, "&amp;")
      .replace(/</g, "&lt;")
      .replace(/>/g, "&gt;")
      .replace(/"/g, "&quot;");
  }

  /** 内核自报的名字与版本（认不出来就说认不出来，别编）。 */
  function engineName() {
    var ua = navigator.userAgent || "";
    var m = /(Chrome|Chromium|CriOS)\/([0-9.]+)/.exec(ua);
    if (m) return m[1] + " " + m[2];
    m = /Version\/([0-9.]+)[^)]*Safari/.exec(ua);
    if (m) return "Safari " + m[1];
    return "未知内核";
  }

  /** flex 容器里的 `gap` 到底生效没有：塞两个 10px 的块，量第二块有没有再挪 10px。 */
  function flexGapWorks() {
    if (!document.body) return true;
    var probe = document.createElement("div");
    probe.setAttribute("style", "position:absolute;left:-9999px;top:0;display:flex;gap:10px");
    var a = document.createElement("i");
    var b = document.createElement("i");
    a.setAttribute("style", "display:block;width:10px;height:1px");
    b.setAttribute("style", "display:block;width:10px;height:1px");
    probe.appendChild(a);
    probe.appendChild(b);
    document.body.appendChild(probe);
    var ok = b.offsetLeft - a.offsetLeft >= 10;
    document.body.removeChild(probe);
    return ok;
  }

  /** 产物用到的能力里缺了哪些（空数组 = 这台引擎跑得动）。 */
  function missing() {
    var out = [];
    var canEval = true;
    try {
      // 只在 SyntaxError 时算数：CSP 挡住 eval 抛的是 EvalError，不能算成"引擎太旧"。
      new Function("var a; a ??= 1;");
    } catch (e) {
      if (e && e.name === "SyntaxError") out.push("JavaScript 语法 <code>??=</code>（Chrome 85 起）");
      else canEval = false;
    }
    if (typeof String.prototype.replaceAll !== "function") {
      out.push("<code>String.prototype.replaceAll</code>（Chrome 85 起）");
    }
    if (typeof CSS !== "undefined" && CSS.supports && !CSS.supports("inset: 0")) {
      out.push("<code>inset</code> 简写（Chrome 87 起）");
    }
    if (!flexGapWorks()) out.push("flex 布局的 <code>gap</code>（Chrome 84 起）");
    // 一项都没探到却又动过 eval，说明这一层的判断力被 CSP 抽掉了 —— 宁可不拦，也别拿猜测当结论。
    if (!out.length && !canEval) return [];
    return out;
  }

  function overlay(list) {
    var android = (navigator.userAgent || "").indexOf("Android") >= 0;
    var box = document.createElement("div");
    box.id = "engine-floor";
    // 全部内联：这一层必须与产物里的样式表（及其下限）完全无关。
    box.setAttribute(
      "style",
      "position:fixed;top:0;right:0;bottom:0;left:0;z-index:2147483647;" +
        "background:#fbfcfd;color:#0e1217;padding:28px;box-sizing:border-box;" +
        "font:16px/1.7 -apple-system,BlinkMacSystemFont,'Noto Sans SC','Microsoft YaHei',sans-serif;" +
        "display:flex;flex-direction:column;justify-content:center;overflow:auto"
    );
    var items = "";
    for (var i = 0; i < list.length; i++) items += "· " + list[i] + "<br>";
    var link = android
      ? '<a href="market://details?id=' +
        WEBVIEW_PKG +
        '" style="display:block;margin:22px 0 0;padding:14px 18px;border-radius:12px;' +
        "background:#2779e1;color:#fff;font-size:17px;font-weight:600;text-align:center;" +
        'text-decoration:none">到应用商店更新系统 WebView</a>'
      : "";
    var hint = android
      ? "没反应的话：在「应用商店 / 华为应用市场 / Play 商店」里搜 “Android System WebView” 更新，" +
        "再回来重开 OrbDrop。"
      : "请更新本机的 WebView / 浏览器内核，或升级 OrbDrop 之后再打开。";
    box.innerHTML =
      '<div style="font-size:21px;font-weight:700">界面没能启动</div>' +
      '<div style="margin-top:10px;color:#5a6068">这台设备的 WebView 内核是 <b>' +
      esc(engineName()) +
      "</b>，低于 OrbDrop 移动端界面要求的 " +
      esc(FLOOR) +
      " —— 入口脚本连解析都过不去，所以界面上一个字也画不出来。</div>" +
      '<div style="margin-top:16px;color:#5a6068">缺少的能力：<br>' +
      items +
      "</div>" +
      link +
      '<button id="engine-floor-retry" style="margin-top:14px;padding:13px 18px;border:1px solid #d7dade;' +
      'border-radius:12px;background:#fff;color:#0e1217;font-size:17px">重试</button>' +
      '<div style="margin-top:16px;color:#8a9099;font-size:13px">' +
      esc(hint) +
      "</div>";
    document.body.appendChild(box);
    var retry = document.getElementById("engine-floor-retry");
    if (retry) {
      retry.onclick = function () {
        location.reload();
      };
    }
  }

  function run() {
    if (document.getElementById("engine-floor")) return;
    var list = missing();
    if (list.length) overlay(list);
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", run);
  } else {
    run();
  }
})();