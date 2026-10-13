#!/usr/bin/env node
/**
 * 生成老引擎的间距兜底表：`static/legacy-gap.css`。
 *
 * 背景：flex 布局里的 `gap` 是 Chrome 84 起才支持的，低于它的引擎**静默忽略**它 —— 不报错，`@supports`
 * 也探不出来（`@supports (gap: …)` 在"只有 grid gap"的老引擎上同样是真），结果是子元素全贴在一起。
 * 移动端产物的下限因此被顶到 84 以上（`scripts/check-compat.mjs` 的 FLOOR）。
 *
 * 把下限压到 81 的做法：**现代引擎一个字节都不多加载**，只让缺 gap 的引擎按需吃这张表 ——
 * 加载时机在 `static/engine-floor.js`（ES5、排在入口 module 之前）：它量得出 gap 到底生不生效，
 * 不生效就把这张表 `<link>` 进去。所以这里的每条规则都**不许**出现在产物引用的样式表里，否则会
 * 与现代引擎的 gap 叠加成双倍间距。
 *
 * 手法：`父 > * + *` 给相邻兄弟之间补一份 margin —— 老引擎没有 gap 语义，margin 是唯一等价物。
 * 值原样沿用 `mobile.css` 里那条 `gap`（都是单值 `calc(N * var(--u))`），字号/密度跟现代端一致。
 *
 * 用法：`npm run legacy:gen`（写文件）/ `npm run legacy:check`（只校验是否同步，退出码非零 = 漂移）
 */
import fs from "node:fs";
import path from "node:path";
import process from "node:process";

const ROOT = path.resolve(path.dirname(new URL(import.meta.url).pathname.replace(/^\/([A-Za-z]:)/, "$1")), "..");
const CSS_PATH = path.join(ROOT, "src/mobile/mobile.css");
const OUT_PATH = path.join(ROOT, "static/legacy-gap.css");
const GENERATED_BY = "scripts/gen-legacy-gap.mjs";

/** 剥注释但**保留换行**（行号与逐行判断才不跑偏）。 */
const stripComments = (t) => t.replace(/\/\*[\s\S]*?\*\//g, (m) => m.replace(/[^\n]/g, " "));

const declsOf = (body) =>
  body
    .split(";")
    .map((s) => s.trim())
    .filter(Boolean)
    .map((s) => {
      const m = /^([a-z-]+)\s*:\s*(.+)$/.exec(s);
      return m ? { prop: m[1], value: m[2] } : null;
    })
    .filter(Boolean);

const val = (decls, prop) => decls.find((d) => d.prop === prop)?.value;

/** 按**括号深度 0** 的空白切分（`calc(12 * var(--u))` 里的空格不算分隔符）。 */
const splitTopLevel = (v) => {
  const out = [];
  let depth = 0;
  let cur = "";
  for (const ch of v) {
    if (ch === "(") depth += 1;
    if (ch === ")") depth -= 1;
    if (/\s/.test(ch) && depth === 0) {
      if (cur) out.push(cur);
      cur = "";
      continue;
    }
    cur += ch;
  }
  if (cur) out.push(cur);
  return out;
};

/** 从 `mobile.css` 正文里抽出所有"flex 容器 + gap"的规则。 */
export function collect(source) {
  const text = stripComments(source);
  const rules = [];
  const notes = [];
  const keep = []; // 应用里"故意的外边距"，生成时原样再声明一遍（见下面 render）
  const blocks = [...text.matchAll(/([^{}@]+?)\s*\{([^{}]*)\}/g)];
  const allDecls = blocks.map((m) => ({
    selector: m[1].trim(),
    decls: declsOf(m[2]),
    line: text.slice(0, m.index).split("\n").length,
  }));

  for (const { selector, decls, line } of allDecls) {
    const display = val(decls, "display");
    if (!display || !/(inline-)?flex$/.test(display.trim())) continue;
    const gap = val(decls, "gap");
    if (!gap) continue;
    const direction = (val(decls, "flex-direction") || "row").trim();
    const column = direction.startsWith("column");
    if (splitTopLevel(gap).length > 1) {
      notes.push(`第 ${line} 行 \`${selector}\`：\`gap\` 是两个值（行/列不同），生成器按单值处理 —— 手工核对`);
    }
    const justify = (val(decls, "justify-content") || "").trim();
    const wrap = (val(decls, "flex-wrap") || "").trim();
    // `space-between/around/evenly` 自带分离：再补 margin 只会把间距推歪 → 整条跳过。
    if (justify.startsWith("space-")) {
      notes.push(`第 ${line} 行 \`${selector}\`：\`justify-content: ${justify}\` 已自带分离，**跳过**（不补 margin）`);
      continue;
    }
    if (wrap && wrap !== "nowrap") {
      notes.push(`第 ${line} 行 \`${selector}\`：换行容器，走"容器负外边距 + 子项外边距"（行/列间距都有）`);
    }
    // 该容器下已有 margin 的规则（**直接子元素与后代都要看**：后代的 `.empty p { margin: 0 }` 这类
    // 选择器基数是 (0,1,1)，比 `父 > * + *` 的 (0,1,0) 更高，会把兜底的 margin 顶掉 ——
    // 所以下面生成时统一加上 `html.lgap` 前缀把基数抬上去）。
    const classes = selector.split(/[.\s>+~,]+/).filter((c) => c && !c.startsWith("[") && !c.startsWith(":"));
    const children = allDecls.filter((b) => {
      const sels = b.selector.split(",").map((s) => s.trim());
      return sels.some((s) => s.startsWith(`${selector} >`) || s.startsWith(`${selector} `) || classes.some((c) => s.startsWith(`.${c}`)));
    });
    for (const child of children) {
      if (child.selector === selector) continue;
      const m = child.decls.find((d) => d.prop === "margin" || d.prop.startsWith("margin-"));
      if (m && (m.prop === "margin" || m.prop === `margin-${column ? "top" : "left"}`)) {
        notes.push(
          `第 ${line} 行 \`${selector}\` 下的 \`${child.selector}\` 自带 \`${m.prop}: ${m.value}\`` +
            (m.value === "0" ? "（重置，兜底表的间距照常生效）" : "（设计值，下面会原样再声明一遍保住它）"),
        );
        if (m.value !== "0") keep.push({ selector: child.selector, prop: m.prop, value: m.value });
      }
    }
    rules.push({ selector, gap, column, line, wrap: Boolean(wrap && wrap !== "nowrap") });
  }
  return { rules, notes, keep };
}

export function render({ rules, notes, keep }) {
  const lines = [
    "/*",
    " * 老引擎的间距兜底表 —— **由 `" + GENERATED_BY + "` 从 `src/mobile/mobile.css` 生成，勿手改**。",
    " * 重新生成：`npm run legacy:gen`；是否同步：`npm run legacy:check`（也挂在 `check:compat` 里）。",
    " *",
    " * 谁会用：只有**缺 flex gap**（Chrome 84 以下）的引擎 —— `static/engine-floor.js` 量到 gap 不生效",
    " * 就把这张表按需 `<link>` 进去。现代引擎根本不加载它（加载了会与现代的 gap 叠加成双倍间距）。",
    " *",
    " * 手法：`父 > * + *` 给相邻兄弟补一份 margin，值原样沿用 `mobile.css` 里那条 `gap`。",
    " *   · `justify-content: space-*` 的容器**不进这张表**：它本来就靠 space 把子项推开，再补 margin 只会推歪。",
    " *   · 换行容器（`flex-wrap`）走「容器负外边距 + 子项外边距」：只补 margin-left 的话，换行后的行间距会丢。",
    " *   · 每条都带 `html.lgap` 前缀：`父 > * + *` 的基数是 (0,1,0)，压不过应用里 `.empty p { margin: 0 }`",
    " *     这类 (0,1,1) 的后代规则（真机上就是这么丢掉空态那 23px 间距的）。这个类由",
    " *     `engine-floor.js` 在注入本表时挂到 `<html>` 上 —— 没挂上时本表一条都不生效，等于双保险。",
    " */",
    "",
  ];
  for (const r of rules) {
    if (r.wrap) {
      lines.push(`html.lgap ${r.selector} { margin: calc(-1 * (${r.gap})) 0 0 calc(-1 * (${r.gap})); }`);
      lines.push(`html.lgap ${r.selector} > * { margin: ${r.gap} 0 0 ${r.gap}; }`);
    } else {
      lines.push(`html.lgap ${r.selector} > * + * { margin-${r.column ? "top" : "left"}: ${r.gap}; }`);
    }
  }
  if (keep.length) {
    lines.push(
      "",
      "/*",
      " * 应用里**故意写的**外边距：兜底表基数更高（(0,2,1)），会把 `mobile.css` 里 `.fbub .fstop { margin-left: … }`",
      " * 这类设计值盖掉 —— 原样再声明一遍（基数 (0,3,1)）把它保回来。`margin: 0` 那种不算（那是重置 <p>/<input>",
      " * 的默认外边距，得让兜底表的间距生效）。",
      " */",
    );
    for (const k of keep) lines.push(`html.lgap ${k.selector} { ${k.prop}: ${k.value}; }`);
    lines.push("");
  }
  if (notes.length) {
    lines.push("", "/*", " * 下面这些要人工核对（生成器只能照搬，判断得人来做）：");
    for (const n of notes) lines.push(` *   · ${n}`);
    lines.push(" */", "");
  }
  return lines.join("\n");
}

const source = fs.readFileSync(CSS_PATH, "utf8");
const collected = collect(source);
const next = render(collected);

if (process.argv[1] && import.meta.url.endsWith(path.basename(process.argv[1]))) {
  if (process.argv.includes("--check")) {
    const current = fs.existsSync(OUT_PATH) ? fs.readFileSync(OUT_PATH, "utf8") : "";
    if (current !== next) {
      console.error(`✗ ${path.relative(ROOT, OUT_PATH)} 与 mobile.css 不同步：请运行 \`npm run legacy:gen\``);
      process.exit(1);
    }
    console.log(`✓ 间距兜底表已同步（${collected.rules.length} 条 flex 容器，${collected.notes.length} 条待人工核对）`);
    process.exit(0);
  }
  fs.mkdirSync(path.dirname(OUT_PATH), { recursive: true });
  fs.writeFileSync(OUT_PATH, next);
  console.log(`已写入 ${path.relative(ROOT, OUT_PATH)}：${collected.rules.length} 条（column ${collected.rules.filter((r) => r.column).length} 条）`);
  for (const n of collected.notes) console.log(`  · ${n}`);
}