#!/usr/bin/env node
/**
 * 兼容性门禁：保证移动端产物在 **Chrome 90+** 的 WebView 上不会"静默失色"。
 *
 * 为什么必须有它：`oklch()` / `color-mix()`（Chrome 111 起）与 `:has()`（105 起）在不支持的引擎上
 * **不报错** —— 只是那条声明失效（背景透明、文字变默认黑、强调色全没）。开发机（新引擎）看不出来。
 *
 * 判据（对应 `scripts/gen-color-tokens.mjs` 的三套机制，都是实测出来的失效时机）：
 *   1. **基础表里不允许出现 `color-mix()` 声明**：老引擎能解析它、失败在计算期 → 结果是 unset，
 *      成对声明救不了（实测 `.toast`/`.scrim` 因此仍透明）。它只能待在 `@supports` 生成区里。
 *   2. 基础表里的**纯 `oklch()` 声明**必须有紧邻其前的"老引擎读得懂"的同属性声明
 *      （解析期就非法 → 那条被丢掉 → 前一条生效 ✓）。
 *   3. **生成区里的每条 `color-mix()` 声明，基础表里都要有同属性、老引擎读得懂的值**，
 *      否则老引擎那条属性会整个丢掉。
 *   4. 含 `:has()` 的选择器必须并联 `.is-checked`（组件按已有状态挂类）。
 *   5. 产物层：`dist-mobile/assets/*.css` 同 1–4；`*.js` 不得用超出下限的内建方法。
 *
 * 用法：`npm run check:compat`（退出码非零 = 有回归）
 */
import fs from "node:fs";
import path from "node:path";
import process from "node:process";

const FLOOR = "Chrome 90";
const MODERN = /color-mix\(|oklch\(|oklab\(|lab\(|lch\(|\bcolor\(/;
const isLegacySafe = (v) => !MODERN.test(v);
const BEGIN = "生成区：高保真颜色";
const END = "生成区结束";

const problems = [];
const notes = [];

/** 剥注释但**保留换行**（行号与行级检查才不跑偏；注释里出现 `:has()` 之类不该算数）。 */
const stripComments = (t) => t.replace(/\/\*[\s\S]*?\*\//g, (m) => m.replace(/[^\n]/g, " "));
const splitDecls = (body) =>
  body
    .split(";")
    .map((s) => s.trim())
    .filter(Boolean)
    .map((s) => {
      const m = /^((?:--)?[a-z][\w-]*)\s*:\s*(.*)$/.exec(s);
      return m ? { prop: m[1], value: m[2] } : null;
    })
    .filter(Boolean);

/** 把规则拆成 { selector, decls }（够用就行：我们这份表没有嵌套规则）。 */
/** 选择器归一化：去多余空白、逗号后统一一个空格 —— 两边写法不同时不该误报。 */
const normSel = (s) => s.split(",").map((x) => x.trim().replace(/\s+/g, " ")).join(", ");

function rules(cssText) {
  const out = [];
  for (const m of cssText.matchAll(/([^{}@]+?)\s*\{([^{}]*)\}/g)) {
    out.push({ selector: normSel(m[1]), decls: splitDecls(m[2]) });
  }
  return out;
}

function regionSplit(cssText) {
  const a = cssText.indexOf(BEGIN);
  const b = cssText.indexOf(END);
  if (a < 0 || b < 0 || b < a) return { base: cssText, region: "" };
  const lineStart = cssText.lastIndexOf("\n", a);
  const lineEnd = cssText.indexOf("\n", b + END.length);
  return {
    base: cssText.slice(0, lineStart < 0 ? a : lineStart) + cssText.slice(lineEnd < 0 ? cssText.length : lineEnd + 1),
    region: cssText.slice(lineStart < 0 ? a : lineStart, lineEnd < 0 ? cssText.length : lineEnd + 1),
  };
}

function checkSheet(label, raw) {
  // 先按**原文**切生成区，再各自剥注释 —— 标记本身是注释，剥完就找不到了（第一版就栽在这）。
  const { base: baseRaw, region: regionRaw } = regionSplit(raw);
  const base = stripComments(baseRaw);
  const region = stripComments(regionRaw);
  const baseRules = rules(base);
  const regionRules = rules(region);

  // 1 + 2：基础表
  const baseSelectors = new Map(); // 单个选择器 → 该规则里出现过的属性（列表选择器拆开建索引）
  for (const r of baseRules) {
    for (const sel of r.selector.split(",").map((x) => x.trim())) {
      const set = baseSelectors.get(sel) ?? new Set();
      for (const d of r.decls) set.add(d.prop);
      baseSelectors.set(sel, set);
    }
    r.decls.forEach((d, idx) => {
      if (d.value.includes("color-mix(")) {
        problems.push(`${label} 基础表里出现 \`color-mix()\`（应移进 @supports 生成区）：${r.selector} { ${d.prop} }`);
      }
      if (!MODERN.test(d.value)) return;
      const prev = r.decls.slice(0, idx).filter((x) => x.prop === d.prop).pop();
      if (!prev || !isLegacySafe(prev.value)) {
        problems.push(`${label} \`${r.selector} { ${d.prop} }\` 的现代写法缺少紧邻其前、老引擎读得懂的值\n      ${d.value.slice(0, 80)}`);
      }
    });
  }
  // 跨行规则的声明（本表的普通属性一行一条；上面按规则切已覆盖，这里补一遍纯文本兜底）
  for (const line of base.split("\n")) {
    if (/\bcolor-mix\(/.test(line) && /^[a-z-]+\s*:/.test(line.trim())) {
      problems.push(`${label} 基础表里有 \`color-mix()\` 声明：${line.trim().slice(0, 80)}`);
    }
  }

  // 3：生成区完整性
  for (const r of regionRules) {
    const props = baseSelectors.get(r.selector);
    for (const d of r.decls) {
      if (!d.value.includes("color-mix(")) continue;
      // 选择器可能是列表（`.a, .b`）：逐段比对，命中任意一段即算配对
      const selList = r.selector.split(",").map((x) => x.trim());
      const matched = selList.some((sel) => baseSelectors.get(sel)?.has(d.prop));
      if (!matched) {
        problems.push(`${label} 生成区里的 \`${r.selector} { ${d.prop} }\` 在基础表里没有对应的老引擎值（那条属性在老引擎上会整个丢掉）`);
      }
    }
  }

  // 4：:has() 并联 .is-checked
  const rawLines = stripComments(raw).split("\n");
  for (let i = 0; i < rawLines.length; i += 1) {
    if (!rawLines[i].includes(":has(")) continue;
    if (!rawLines[i].includes("is-checked")) {
      problems.push(`${label}:${i + 1} 含 \`:has()\`（${FLOOR} 不支持）却无 \`.is-checked\` 并行选择器\n      ${rawLines[i].trim().slice(0, 90)}`);
    }
  }
}

const CSS_PATH = path.resolve("src/mobile/mobile.css");
checkSheet("src/mobile/mobile.css", fs.readFileSync(CSS_PATH, "utf8"));

const distDir = path.resolve("dist-mobile/assets");
if (fs.existsSync(distDir)) {
  for (const file of fs.readdirSync(distDir)) {
    const full = path.join(distDir, file);
    const text = fs.readFileSync(full, "utf8");
    if (file.endsWith(".css")) checkSheet(`dist-mobile/assets/${file}`, text);
    if (file.endsWith(".js")) {
      const denied = [
        ["structuredClone", 98],
        ["Object.hasOwn", 93],
        [".at(", 92],
        ["crypto.randomUUID", 92],
        ["findLast", 97],
        ["Promise.withResolvers", 119],
      ];
      for (const [needle, since] of denied) {
        if (text.includes(needle)) problems.push(`dist-mobile/assets/${file} 用到 \`${needle}\`（Chrome ${since} 起），超出下限 ${FLOOR}`);
      }
    }
  }
} else {
  notes.push("dist-mobile 不存在，跳过产物层检查（只查了源码）");
}

for (const n of notes) console.log(`· ${n}`);
if (problems.length) {
  console.error(`\n✗ 兼容性门禁未通过（下限 ${FLOOR}），${problems.length} 个问题：`);
  for (const p of problems.slice(0, 14)) console.error(`  - ${p}`);
  if (problems.length > 14) console.error(`  … 另有 ${problems.length - 14} 条`);
  process.exit(1);
}
console.log(`✓ 兼容性门禁通过（下限 ${FLOOR}）`);