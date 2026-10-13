/*
 * 引擎下限层（**ES5**，刻意不参与任何构建：这里不许出现 `let/const`、箭头函数、模板串 —— 它要能在
 * 比产物下限还老的引擎上跑起来）。
 *
 * 三件事，按同一张原则：**能力齐全的引擎一个字节都不多花，缺能力才按需补**。
 *
 * 1. `String.prototype.replaceAll`（Chrome 85 起）**当场兜底**：Svelte 5 的 template 助手与 Vite 的
 *    预载助手都在用它，而 esbuild 只降语法、不补内建 —— 不装这一步，85 以下的引擎上入口一跑就
 *    TypeError。必须排在入口 module 之前（本文件在 `<head>` 里、是非 module 脚本，天然先执行）。
 *
 * 2. 老引擎的**补丁表按需注入**：`legacy-gap.css`（flex `gap` 缺失时 —— Chrome 84 起才有 flex 上下
 *    文的 gap，缺了它是**静默失效**，子元素全贴在一起）与 `legacy-misc.css`（`inset: 0` 简写缺失时 ——
 *    Chrome 87 起，缺了它那 5 处铺满层会失去定位）。现代引擎这两条探测都过，**不会加载这两张表**
 *    （加载了反而会与现代的 gap 叠加成双倍间距）。
 *
 * 3. 真的跑不动的引擎（连 ES2020 语法都没有，Chrome 80 以下）**不再给白屏**：画一个浮层把话说明白，
 *    并给一个"到应用商店更新系统 WebView"的入口（那个 `market://` 是**导航**，由后端导航钩子接住
 *    换成系统 Intent —— 需要它的那一刻前端恰好是死的，页面里那套 IPC 根本不存在；见
 *    `src-tauri/src/mobile.rs::store_plugin`）。
 *
 * 为什么探测而不是判版本号：探的是产物**真的**用到的能力。`??=` 这种语法在构建期已被降级
 * （`vite.config.js` 的 `target: "chrome81"`），所以这里只探"降完还剩下的门槛"（ES2020 语法、
 * flex gap、inset、replaceAll）；版本号会随构建配置漂移，能力不会。
 *
 * 实测与取舍见 `docs/p1-implementation.md` §35/§36。
 */
(function () {
  /* ── 1. replaceAll 兜底 ─────────────────────────────────────────────────── */

  if (typeof String.prototype.replaceAll !== "function") {
    var replaceAll = function (search, replacement) {
      var self = String(this);
      if (search instanceof RegExp) {
        // 与规范一致：非全局正则直接抛（split/join 那条路会静默地只替第一处）
        if (!search.global) {
          throw new TypeError("replaceAll must be called with a global RegExp");
        }
        return self.replace(search, replacement);
      }
      // 字符串形式按字面替换：`$&` 之类替换模式在这一条下不生效 —— 用到它的两处（Svelte 的 template
      // 助手、Vite 的预载助手）传的都是纯占位符/正则，够用。
      return self.split(String(search)).join(String(replacement));
    };
    if (Object.defineProperty) {
      Object.defineProperty(String.prototype, "replaceAll", {
        value: replaceAll,
        writable: true,
        configurable: true,
      });
    } else {
      String.prototype.replaceAll = replaceAll;
    }
  }

  /* ── 2. 能力探测 ───────────────────────────────────────────────────────── */

  /** 内核自报的名字与版本（认不出来就说认不出来，别编）。 */
  function engineName() {
    var ua = navigator.userAgent || "";
    var m = /(Chrome|Chromium|CriOS)\/([0-9.]+)/.exec(ua);
    if (m) return m[1] + " " + m[2];
    m = /Version\/([0-9.]+)[^)]*Safari/.exec(ua);
    if (m) return "Safari " + m[1];
    return "未知内核";
  }

  /**
   * flex 容器里的 `gap` 到底生效没有：塞两个 10px 的块 + 10px 的 gap，量第二块落在哪。
   * 有 gap → 第二块被推到 20px 之后；没有 gap → 它就紧挨着第一块（10px）。
   *
   * **阈值必须卡在 10 与 20 之间**（取 15）：`offsetLeft` 是"位置"，不是"间距" —— 用
   * `b.offsetLeft - a.offsetLeft >= 10` 这种写法在没有 gap 时同样成立（差值正好 10），
   * 那就是"逢老引擎必判支持"，兜底表永远不注入（真机上第一次就是这么栽的：81 上量出 10，
   * 判定"支持"，空态那块因此少了 18px 间距）。
   */
  function flexGapWorks() {
    var host = document.body || document.documentElement;
    if (!host) return true;
    var probe = document.createElement("div");
    probe.setAttribute("style", "position:absolute;left:-9999px;top:0;display:flex;gap:10px");
    var a = document.createElement("i");
    var b = document.createElement("i");
    a.setAttribute("style", "display:block;width:10px;height:1px");
    b.setAttribute("style", "display:block;width:10px;height:1px");
    probe.appendChild(a);
    probe.appendChild(b);
    host.appendChild(probe);
    var distance = b.offsetLeft - a.offsetLeft;
    host.removeChild(probe);
    return distance >= 15;
  }

  /** `inset` 简写认不认（Chrome 87 起）。`CSS.supports` 本身都没有的引擎，一并算作不认。 */
  function insetWorks() {
    return typeof CSS !== "undefined" && !!CSS.supports && CSS.supports("inset: 0");
  }

  /** 产物降级后还需要的语法门槛：可选链与空值合并（Chrome 80 起，ES2020）。 */
  function syntaxTooOld() {
    try {
      new Function("var a = {}; var b = a && a.x; b = a?.x; b = a.x ?? 1;");
      return false;
    } catch (e) {
      // 只在 SyntaxError 时算数：CSP 挡 eval 抛的是 EvalError，那种情况不判死刑（宁可不拦）。
      return !!(e && e.name === "SyntaxError");
    }
  }

  function injectSheet(href) {
    if (!document.head || document.getElementById("legacy-style-" + href)) return;
    var link = document.createElement("link");
    link.id = "legacy-style-" + href;
    link.rel = "stylesheet";
    link.href = href;
    // 排在 `<head>` 里、且在首屏之前：加进来就是渲染阻塞的，页面不会先闪一版"没间距"再补上。
    document.head.appendChild(link);
  }

  /** 间距兜底表要靠 `html.lgap` 前缀把选择器基数抬上去（压过 `.empty p { margin: 0 }` 这类后代规则）。 */
  function markLegacyGap() {
    var root = document.documentElement;
    if (!root) return;
    var cls = " " + (root.className || "") + " ";
    if (cls.indexOf(" lgap ") < 0) root.className = (root.className ? root.className + " " : "") + "lgap";
  }

  /** 能补的当场补：缺哪样才加载哪张表（补丁表自带的 `html.lgap` 前缀是第二道保险）。 */
  function applyLegacy() {
    if (!flexGapWorks()) {
      markLegacyGap();
      injectSheet("/legacy-gap.css");
    }
    if (!insetWorks()) injectSheet("/legacy-misc.css");
  }

  /* ── 3. 跑起来：能补的当场补，补不了的才拦 ─────────────────────────────── */

  // 两张补丁表当场注入：探测只需要元素与 CSSOM，不需要等 body（脚本在 `<head>` 里就能做）。
  if (document.documentElement) {
    applyLegacy();
  } else {
    document.addEventListener("DOMContentLoaded", applyLegacy);
  }

  /** 兜底页：只有"连语法都不够"的引擎才看得到它。 */
  function notice() {
    if (!document.body || document.getElementById("engine-floor")) return;
    var android = (navigator.userAgent || "").indexOf("Android") >= 0;
    var box = document.createElement("div");
    box.id = "engine-floor";
    // 全部内联：这一层必须与产物里那份样式表（及其下限）无关。
    box.setAttribute(
      "style",
      "position:fixed;top:0;right:0;bottom:0;left:0;z-index:2147483647;" +
        "background:#fbfcfd;color:#0e1217;padding:28px;box-sizing:border-box;" +
        "font:16px/1.7 -apple-system,BlinkMacSystemFont,'Noto Sans SC','Microsoft YaHei',sans-serif;" +
        "display:flex;flex-direction:column;justify-content:center;overflow:auto"
    );
    var link = android
      ? '<a href="market://details?id=com.google.android.webview" style="display:block;margin:22px 0 0;' +
        "padding:14px 18px;border-radius:12px;background:#2779e1;color:#fff;font-size:17px;" +
        'font-weight:600;text-align:center;text-decoration:none">到应用商店更新系统 WebView</a>'
      : "";
    var hint = android
      ? "没反应的话：在「应用商店 / 华为应用市场 / Play 商店」里搜 “Android System WebView” 更新，" +
        "再回来重开 OrbDrop。"
      : "请更新本机的 WebView / 浏览器内核，或升级 OrbDrop 之后再打开。";
    box.innerHTML =
      '<div style="font-size:21px;font-weight:700">界面没能启动</div>' +
      '<div style="margin-top:10px;color:#5a6068">这台设备的内核是 <b>' +
      engineName() +
      "</b>，连 OrbDrop 界面需要的 ES2020 语法都没有 —— 入口脚本解析不过去，所以一个字也画不出来。</div>" +
      link +
      '<button id="engine-floor-retry" style="margin-top:14px;padding:13px 18px;border:1px solid #d7dade;' +
      'border-radius:12px;background:#fff;color:#0e1217;font-size:17px">重试</button>' +
      '<div style="margin-top:16px;color:#8a9099;font-size:13px">' +
      hint +
      "</div>";
    document.body.appendChild(box);
    var retry = document.getElementById("engine-floor-retry");
    if (retry) {
      retry.onclick = function () {
        location.reload();
      };
    }
  }

  if (syntaxTooOld()) {
    if (document.readyState === "loading") document.addEventListener("DOMContentLoaded", notice);
    else notice();
  }

})();