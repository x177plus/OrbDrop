#!/usr/bin/env node
/**
 * 把 `src/mobile/mobile.css` 里的现代颜色写法**降级到 Chrome 90 也能读**，同时保住新引擎的高保真。
 *
 * 背景：`oklch()` / `color-mix()` 是 Chrome 111（2023-03）起才有的。老 WebView（MuMu 里是 110）
 * 上它们**不报错**，只是那条声明失效 —— 界面变成白底黑字没配色，开发机（新引擎）完全看不出来。
 *
 * 三种处理方式，**因为 CSS 在三处的失效时机不同**（都是实测出来的，不是照规范推的）：
 *
 * 1. **自定义属性**（`--bg` / `--accent-soft` …）：自定义属性不做值校验，后一条会无条件覆盖前一条，
 *    然后在 `var()` 使用处整条失效。所以主 `:root` 只放 sRGB，现代值收进 `@supports` 覆盖块：
 *
 *        :root { --bg: rgb(251, 252, 253); }
 *        @supports (…) { :root { --bg: oklch(99% 0.002 240); } }
 *
 * 2. **纯 `oklch()` 的普通属性**（如 `box-shadow: … oklch(…)`）：`oklch()` 在老引擎上**解析期**就非法，
 *    那条声明被丢掉、前一条生效 —— 所以「同属性两条声明」有效：
 *
 *        box-shadow: 0 12px 30px -22px rgba(14, 18, 23, .5);
 *        box-shadow: 0 12px 30px -22px oklch(18% 0.012 250 / .5);
 *
 * 3. **含 `color-mix()` 的普通属性**：老引擎**能解析**它（CSSOM 里留得住），失败发生在**计算期**。
 *    按规范那是 "invalid at computed-value time" → 结果是 `unset`，**不会回退到前一条声明**
 *    （实测 `.toast` / `.scrim` 的成对回退因此仍算作透明）。所以这类也必须进 `@supports`：
 *
 *        .toast { background: rgba(14, 18, 23, .93); }
 *        @supports (…) { .toast { background: color-mix(in oklab, var(--fg) 93%, transparent); } }
 *
 * 另外：本文件里**所有 `color-mix()` 都写 `in oklab`（直角插值），不写 `in oklch`** —— 极坐标色相
 * 插值在 Chromium 111–114 上会把 `transparent` 的色相当 0 参与绕行，把蓝色底纹染成粉色（真机
 * Huawei WebView 114.0.5.302 实测）。等价性与理由见 `SUPPORTS_COND` 上方那段注释。
 *
 * 配色仍只有**一处来源**（原型那套 oklch/color-mix 写法），sRGB 值是它的机械派生。
 * 计算自己实现（OKLab ↔ sRGB + `color-mix` 的预乘插值），不引依赖；生成值已用 Chromium 的 canvas
 * 渲染逐条对照过（27 条里 26 条逐通道一致，唯一偏差是 canvas 在低不透明度下预乘量化的读数误差）。
 *
 * 用法：`npm run tokens:gen` 写入 / `npm run tokens:check` 校验（不同步则退出码 1）。
 */
import fs from "node:fs";
import path from "node:path";
import process from "node:process";

const CSS_PATH = path.resolve("src/mobile/mobile.css");
const CHECK_ONLY = process.argv.includes("--check");

const MODERN = /color-mix\(|oklch\(|oklab\(|lab\(|lch\(|\bcolor\(/;
const isLegacySafe = (value) => !MODERN.test(value);

const BEGIN_MARK = "/* ↓↓↓ 生成区：高保真颜色（Chrome 111+），老引擎整块跳过 —— 见 scripts/gen-color-tokens.mjs ↓↓↓ */";
const END_MARK = "/* ↑↑↑ 生成区结束 ↑↑↑ */";
/**
 * 生成区里**只用直角插值**（`in oklab`），不用 `in oklch`。
 *
 * `in oklch` 是极坐标色相插值：Chromium 111–114 会把 `transparent`（`rgb(0 0 0 / 0)`，转到 oklch 后
 * 色相是 0）的色相当成"有值"参与绕行 —— 真机 Huawei WebView 114.0.5.302 实测：`--accent-soft`
 * （`var(--accent) 12% + transparent`）叠在白底上得 rgb(243,221,237)（粉），正确值 rgb(227,239,251)（浅蓝）。
 * 该引擎的 `oklch()` 本身是准的（同一个包里的 `--accent` 实测仍是蓝 rgb(23,120,225)），坏的只是极坐标混合。
 *
 * `in oklab` 与 `in oklch` 的差别只在**两个带色相的颜色**互混时出现：本仓那几处（`--hairline`、
 * `.pend`、`.opt.danger`）用极坐标会沿色相绕行（实测 `.pend` 绕到 302°、`.opt.danger` 绕到 288°，
 * 一个发紫一个发紫蓝），直角插值给出的才是设计意图（也正好等于本脚本派生的 sRGB 回退值）。
 * 与 `transparent` / `black` 混时两者结果一致，所以这条规则对所有既有取值是**等价**的。
 */
const SUPPORTS_COND = "@supports (color: oklch(0% 0 0)) and (background: color-mix(in oklab, red, blue))";

// ── OKLab ↔ linear sRGB（矩阵取自 Björn Ottosson 的 OKLab 定义，与 CSS Color 4 一致）──
function oklabToLinearSrgb(L, a, b) {
  const l = (L + 0.3963377774 * a + 0.2158037573 * b) ** 3;
  const m = (L - 0.1055613458 * a - 0.0638541728 * b) ** 3;
  const s = (L - 0.0894841775 * a - 1.291485548 * b) ** 3;
  return [
    4.0767416621 * l - 3.3077115913 * m + 0.2309699292 * s,
    -1.2684380046 * l + 2.6097574011 * m - 0.3413193965 * s,
    -0.0041960863 * l - 0.7034186147 * m + 1.707614701 * s,
  ];
}
function linearSrgbToOklab(r, g, b) {
  const l = Math.cbrt(0.4122214708 * r + 0.5363325363 * g + 0.0514459929 * b);
  const m = Math.cbrt(0.2119034982 * r + 0.6806995451 * g + 0.1073969566 * b);
  const s = Math.cbrt(0.0883024619 * r + 0.2817188376 * g + 0.6299787005 * b);
  return [
    0.2104542553 * l + 0.793617785 * m - 0.0040720468 * s,
    1.9779984951 * l - 2.428592205 * m + 0.4505937099 * s,
    0.0259040371 * l + 0.7827717662 * m - 0.808675766 * s,
  ];
}
const srgbToLinear = (c) => (c <= 0.04045 ? c / 12.92 : ((c + 0.055) / 1.055) ** 2.4);
const linearToSrgb = (c) => (c <= 0.0031308 ? 12.92 * c : 1.055 * c ** (1 / 2.4) - 0.055);
const clamp01 = (x) => Math.min(1, Math.max(0, x));

function parseOklch(text) {
  const m = /^oklch\(\s*([\d.]+)%\s+([\d.]+)\s+([\d.]+)\s*(?:\/\s*([\d.]+)(%?)\s*)?\)$/.exec(text.trim());
  if (!m) return null;
  const [, L, C, H, A, unit] = m;
  const h = (Number(H) * Math.PI) / 180;
  return {
    ok: [Number(L) / 100, Number(C) * Math.cos(h), Number(C) * Math.sin(h)],
    alpha: A === undefined ? 1 : unit === "%" ? Number(A) / 100 : Number(A),
  };
}

function parseColor(text) {
  const t = text.trim().toLowerCase();
  if (t === "transparent") return { ok: [0, 0, 0], alpha: 0 };
  if (t === "black") return { ok: [0, 0, 0], alpha: 1 };
  if (t === "white") return { ok: [1, 1, 1], alpha: 1 };
  const ok = parseOklch(t);
  if (ok) return ok;
  const hex = /^#([0-9a-f]{6})([0-9a-f]{2})?$/.exec(t);
  if (hex) {
    const linear = [0, 2, 4].map((i) => srgbToLinear(parseInt(hex[1].slice(i, i + 2), 16) / 255));
    return { ok: linearSrgbToOklab(...linear), alpha: hex[2] ? parseInt(hex[2], 16) / 255 : 1 };
  }
  const rgba = /^rgba?\(\s*([\d.]+)[,\s]\s*([\d.]+)[,\s]\s*([\d.]+)\s*(?:[,/]\s*([\d.]+)\s*)?\)$/.exec(t);
  if (rgba) {
    const linear = [1, 2, 3].map((i) => srgbToLinear(Number(rgba[i]) / 255));
    return { ok: linearSrgbToOklab(...linear), alpha: rgba[4] === undefined ? 1 : Number(rgba[4]) };
  }
  return null;
}

/** `color-mix(in oklab, …)` 的插值：在 oklab 的**直角分量**上按预乘 alpha 混合。
    与 `transparent` 混时结果与色彩空间无关（只有 alpha 起作用）；与 `black` 混时与 `in oklch` 等价；
    与带色相的哑色（`var(--surface)` 这种显式 h=0 的颜色）混时，`in oklch` 会沿色相绕行，直角插值不会。 */
function mixOklab(a, b, aWeightPercent) {
  const wa = aWeightPercent / 100;
  const wb = 1 - wa;
  const pa = a.ok.map((v) => v * a.alpha);
  const pb = b.ok.map((v) => v * b.alpha);
  const alpha = a.alpha * wa + b.alpha * wb;
  const ok = pa.map((v, i) => (alpha === 0 ? 0 : (v * wa + pb[i] * wb) / alpha));
  return { ok, alpha };
}

function toSrgb(color) {
  const clipped = oklabToLinearSrgb(...color.ok).map(clamp01);
  const bytes = clipped.map((v) => Math.round(clamp01(linearToSrgb(v)) * 255));
  const delta = Math.hypot(...linearSrgbToOklab(...clipped).map((v, i) => v - color.ok[i]));
  return {
    text:
      color.alpha >= 1
        ? `rgb(${bytes[0]}, ${bytes[1]}, ${bytes[2]})`
        : `rgba(${bytes[0]}, ${bytes[1]}, ${bytes[2]}, ${Number(color.alpha.toFixed(3))})`,
    delta,
  };
}

// ── 源码读取：主 :root 的 sRGB + 生成区里的现代值（后者是重算依据，优先）──────────
const source = fs.readFileSync(CSS_PATH, "utf8");

const readTokenMap = (text) => {
  const out = {};
  for (const line of text.split("\n")) {
    const m = /^\s*(--[\w-]+)\s*:\s*(.+?);\s*$/.exec(line);
    if (m) out[m[1]] = m[2].trim();
  }
  return out;
};

const mainRoot = /:root\s*\{([\s\S]*?)\n\}/.exec(source);
if (!mainRoot) throw new Error("找不到主令牌块 :root { … }");
const rootTokens = readTokenMap(mainRoot[1]);

const generatedRegion = (() => {
  const a = source.indexOf(BEGIN_MARK);
  const b = source.indexOf(END_MARK);
  return a >= 0 && b > a ? source.slice(a, b) : "";
})();
const hiFiRootMatch = /:root\s*\{([\s\S]*?)\n\s*\}/.exec(generatedRegion);
const modernTokens = hiFiRootMatch ? readTokenMap(hiFiRootMatch[1]) : {};
/** var() 解析用：现代值优先，退回主 root（没有现代写法的令牌本来就不参与）。 */
const tokens = { ...rootTokens, ...modernTokens };

/** 生成区里已有的规则级覆盖：选择器 → [{prop, value}]（用于幂等重建）。 */
const prevRuleOverrides = new Map();
{
  const withoutRoot = generatedRegion.replace(/:root\s*\{[\s\S]*?\n\s*\}/, "");
  for (const rule of withoutRoot.matchAll(/([^{}]+?)\s*\{([^{}]*)\}/g)) {
    const sel = rule[1].trim().replace(/^@supports[^{]*/, "").trim();
    if (!sel || sel.startsWith("@")) continue;
    const decls = rule[2]
      .split(";")
      .map((s) => s.trim())
      .filter(Boolean)
      .map((s) => {
        const m = /^((?:--)?[a-z][\w-]*)\s*:\s*(.*)$/.exec(s);
        return m ? { prop: m[1], value: m[2] } : null;
      })
      .filter(Boolean);
    if (decls.length) prevRuleOverrides.set(sel, decls);
  }
}

// ── 值解析 ───────────────────────────────────────────────────────────────────
function sliceCall(text, start) {
  let depth = 0;
  for (let i = start + "color-mix".length; i < text.length; i += 1) {
    if (text[i] === "(") depth += 1;
    else if (text[i] === ")") {
      depth -= 1;
      if (depth === 0) return { inner: text.slice(start + "color-mix(".length, i), end: i };
    }
  }
  throw new Error(`括号不配对：${text}`);
}

function resolveColor(text) {
  const t = text.replace(/\s*[\d.]+%\s*$/, "").trim();
  if (t.startsWith("color-mix(")) {
    const { inner } = sliceCall(t, 0);
    const parts = inner.split(",").map((x) => x.trim());
    if (!/^in\s+oklab$/i.test(parts.shift() ?? "")) throw new Error(`只支持 in oklab（直角插值）：${text}`);
    const [aText, bText] = parts;
    const weightOf = (x) => {
      const m = /([\d.]+)%$/.exec(x ?? "");
      return m ? Number(m[1]) : null;
    };
    const wa = weightOf(aText) ?? (weightOf(bText) === null ? 50 : 100 - weightOf(bText));
    return mixOklab(resolveColor(aText), resolveColor(bText), wa);
  }
  const varMatch = /^var\(\s*(--[\w-]+)\s*\)$/.exec(t);
  if (varMatch) {
    if (!(varMatch[1] in tokens)) throw new Error(`未知令牌 ${varMatch[1]}`);
    return resolveColor(tokens[varMatch[1]]);
  }
  const c = parseColor(t);
  if (!c) throw new Error(`无法解析的颜色：${text}`);
  return c;
}

/** 把一条值里的 `color-mix()` / `oklch()` 全换成 sRGB 文本。 */
function downlevelValue(value) {
  let out = value;
  let delta = 0;
  for (let guard = 0; guard < 64; guard += 1) {
    const start = out.indexOf("color-mix(");
    if (start < 0) break;
    const { inner, end } = sliceCall(out, start);
    const parts = inner.split(",").map((x) => x.trim());
    if (!/^in\s+oklab$/i.test(parts.shift() ?? "")) throw new Error(`只支持 in oklab（直角插值）：${value}`);
    const [aText, bText] = parts;
    const weightOf = (x) => {
      const m = /([\d.]+)%$/.exec(x ?? "");
      return m ? Number(m[1]) : null;
    };
    const wa = weightOf(aText) ?? (weightOf(bText) === null ? 50 : 100 - weightOf(bText));
    const mixed = toSrgb(mixOklab(resolveColor(aText), resolveColor(bText), wa));
    delta = Math.max(delta, mixed.delta);
    out = out.slice(0, start) + mixed.text + out.slice(end + 1);
  }
  for (let guard = 0; guard < 64; guard += 1) {
    const m = /oklch\([^()]*\)/.exec(out);
    if (!m) break;
    const parsed = parseOklch(m[0]);
    if (!parsed) break;
    const srgb = toSrgb(parsed);
    delta = Math.max(delta, srgb.delta);
    out = out.slice(0, m.index) + srgb.text + out.slice(m.index + m[0].length);
  }
  return { text: out, delta };
}

// ── ① 令牌：主 root 只留 sRGB，现代值进生成区 ────────────────────────────────
const tokenLines = [];
const hiFiTokens = [];
for (const raw of mainRoot[1].split("\n")) {
  const m = /^(\s*)(--[\w-]+)\s*:\s*(.+?);(\s*)$/.exec(raw);
  if (!m) {
    tokenLines.push(raw);
    continue;
  }
  const [, indent, name, value] = m;
  const modern = modernTokens[name] ?? value;
  if (!MODERN.test(modern)) {
    tokenLines.push(raw);
    continue;
  }
  tokenLines.push(`${indent}${name}: ${downlevelValue(modern).text};`);
  hiFiTokens.push(`${indent}${name}: ${modern};`);
}
const newTokenBlock = tokenLines.join("\n");

// ── ② 规则：纯 oklch 用成对声明；color-mix 挪进生成区 ────────────────────────
const afterTokens = mainRoot.index + mainRoot[0].length;
const beginIdx = source.indexOf(BEGIN_MARK, afterTokens);
const bodyEnd = beginIdx < 0 ? source.length : Math.max(afterTokens, source.lastIndexOf("\n", beginIdx));
const bodyLines = source.slice(afterTokens, bodyEnd).split("\n");

const out = [];
const paired = [];
const skipped = [];
const extraCustomProps = [];
const collectedOverrides = new Map(); // 选择器 → [{prop, value}]
const bodySelectors = [];
let atRuleDepth = 0;
/** 跨行选择器列表的前半截（`.a,\n.b { … }`）：单独一行的选择器不以 `{` 结尾，得先攒着。 */
let selectorLead = "";

const recordOverride = (selector, prop, value) => {
  if (!collectedOverrides.has(selector)) {
    collectedOverrides.set(selector, []);
    bodySelectors.push(selector);
  }
  const list = collectedOverrides.get(selector);
  const at = list.findIndex((x) => x.prop === prop);
  if (at >= 0) list[at] = { prop, value };
  else list.push({ prop, value });
};

for (let i = 0; i < bodyLines.length; i += 1) {
  const line = bodyLines[i];

  if (/^\s*@/.test(line)) atRuleDepth += 1;
  if (/^\s*\}/.test(line)) atRuleDepth = Math.max(0, atRuleDepth - 1);

  // 单行规则
  const single = /^(\s*)([^{}@]+?)\s*\{\s*([^{}]+?)\s*\}\s*$/.exec(line);
  if (single) {
    // 选择器可能跨行（`.a,\n.b { … }`）：把前面以逗号结尾的行接回来，否则覆盖只落到最后一个选择器上
    const selector = (selectorLead + single[2]).trim().replace(/\s+/g, " ");
    selectorLead = "";
    if (!bodySelectors.includes(selector)) bodySelectors.push(selector);
    if (!MODERN.test(single[3])) {
      out.push(line);
      continue;
    }
    const parts = single[3].split(";").map((x) => x.trim()).filter(Boolean);
    const rebuilt = [];
    for (const part of parts) {
      const m = /^((?:--)?[a-z][\w-]*)\s*:\s*(.*)$/.exec(part);
      if (!m) {
        rebuilt.push(part);
        continue;
      }
      const [, prop, value] = m;
      if (!MODERN.test(value)) {
        rebuilt.push(part);
        continue;
      }
      const prev = rebuilt.length ? /^((?:--)?[a-z][\w-]*)\s*:\s*(.*)$/.exec(rebuilt[rebuilt.length - 1]) : null;
      const prevIsSibling = Boolean(prev && prev[1] === prop && isLegacySafe(prev[2]));
      if (/currentColor/.test(value)) {
        // currentColor 算不出来（取决于继承的文字色）：基础表里必须有**手写**的中性回退。
        // 现代写法一样要挪进生成区 —— 否则老引擎在计算期整条失效，成对声明救不了它。
        if (prevIsSibling) {
          // 手写的中性回退留在基础表，现代写法只进生成区（**不要再 push 回基础表**）
          if (atRuleDepth > 0) throw new Error(`at-rule 内的 color-mix 需要手工处理：${selector} { ${prop} }`);
          recordOverride(selector, prop, value);
        } else {
          skipped.push({ prop, value, selector });
          rebuilt.push(part);
        }
        continue;
      }
      if (prevIsSibling) rebuilt.pop();
      const { text } = downlevelValue(value);
      if (value.includes("color-mix(")) {
        if (atRuleDepth > 0) throw new Error(`at-rule 内的 color-mix 需要手工处理（选择器不能搬进生成区）：${selector} { ${prop} }`);
        rebuilt.push(`${prop}: ${text}`);
        recordOverride(selector, prop, value);
      } else {
        rebuilt.push(`${prop}: ${text}`);
        rebuilt.push(part);
        paired.push({ selector, prop });
      }
    }
    const dedup = rebuilt.filter((p, idx) => idx === 0 || p.trim() !== rebuilt[idx - 1].trim());
    out.push(`${single[1]}${selector} { ${dedup.join("; ")}${dedup.length ? ";" : ""} }`);
    continue;
  }

  // 多行规则的声明行
  const ruleStart = /^(\s*)([^{}@]+?)\s*\{\s*$/.exec(line);
  if (ruleStart) {
    // 选择器可能跨行：把前面以逗号结尾的几行接回来，否则覆盖只会落到列表里的最后一个选择器上
    const selector = (selectorLead + ruleStart[2]).trim().replace(/\s+/g, " ");
    selectorLead = "";
    if (!bodySelectors.includes(selector)) bodySelectors.push(selector);
    out.push(line);
    continue;
  }

  const decl = /^(\s*)(--[\w-]+|[a-z-]+)\s*:\s*(.+?);(\s*)$/.exec(line);
  if (!decl) {
    // 不是声明也不是规则头：可能是跨行选择器的前一行（以逗号结尾）
    selectorLead = line.trim().endsWith(",") ? selectorLead + line.trim() + " " : "";
    out.push(line);
    continue;
  }
  const [, indent, prop, value] = decl;
  const selector = bodySelectors.length ? bodySelectors[bodySelectors.length - 1] : "";
  const prevLine = out.length ? out[out.length - 1] : "";
  const prevDecl = /^(\s*)(--[\w-]+|[a-z-]+)\s*:\s*(.+?);(\s*)$/.exec(prevLine);
  const prevIsFallback = Boolean(prevDecl && prevDecl[2] === prop && isLegacySafe(prevDecl[3]));

  if (!MODERN.test(value)) {
    out.push(line);
    continue;
  }
  if (/currentColor/.test(value)) {
    if (prevIsFallback) {
      // 保留手写的中性回退，把现代写法挪进生成区
      if (atRuleDepth > 0) throw new Error(`at-rule 内的 color-mix 需要手工处理：${selector} { ${prop} }`);
      recordOverride(selector, prop, value);
      out.pop();
      out.push(prevLine);
    } else {
      skipped.push({ prop, value, selector });
      out.push(line);
    }
    continue;
  }
  if (prevIsFallback) out.pop();
  const { text } = downlevelValue(value);

  if (value.includes("color-mix(")) {
    // 计算期才失效 → 必须进 @supports（成对声明不管用）
    if (atRuleDepth > 0) throw new Error(`at-rule 内的 color-mix 需要手工处理：${selector} { ${prop} }`);
    if (prop.startsWith("--")) extraCustomProps.push(`${indent}${prop}: ${value};`);
    else recordOverride(selector, prop, value);
    out.push(`${indent}${prop}: ${text};`);
    continue;
  }

  if (prop.startsWith("--")) {
    extraCustomProps.push(`${indent}${prop}: ${value};`);
    out.push(`${indent}${prop}: ${text};`);
  } else {
    out.push(`${indent}${prop}: ${text};`);
    out.push(line);
    paired.push({ selector, prop });
  }
}

// 生成区里的规则级覆盖 = 本轮收集到的 + 上一轮已有（且该选择器仍在表里）—— 保证幂等
const ruleOverrides = [];
for (const selector of bodySelectors) {
  const list = collectedOverrides.get(selector) ?? prevRuleOverrides.get(selector);
  if (!list || !list.length) continue;
  ruleOverrides.push(`  ${selector} { ${list.map((x) => `${x.prop}: ${x.value};`).join(" ")} }`);
}

const hiFiBlock = [
  "",
  BEGIN_MARK,
  "/* 规则级的 color-mix 为什么也在这里：老引擎**能解析**它（CSSOM 里留得住），失败发生在**计算期**，",
  "   按规范属于 invalid at computed-value time → 结果是 unset，**不会回退到前一条声明**",
  "   （实测 .toast / .scrim 的成对回退因此仍是透明）。放进 @supports 才会被整块跳过。",
  "   纯 oklch() 不同：它在解析期就非法、那条声明被丢掉、前一条生效，所以那些用成对声明就够。",
  "   另一条硬规矩：这块里的 color-mix() **一律 `in oklab`（直角插值）**，不用 `in oklch` ——",
  "   Chromium 111–114 的极坐标色相插值会把 transparent 的色相当 0 绕行，蓝色底纹会被染成粉色",
  "   （真机 Huawei WebView 114.0.5.302 实测）；与 transparent / black 混时两者等价，见 gen 脚本注释。 */",
  SUPPORTS_COND + " {",
  "  :root {",
  ...[...hiFiTokens, ...extraCustomProps].join("\n").split("\n"),
  "  }",
  ...ruleOverrides,
  "}",
  END_MARK,
  "",
].join("\n");

const next =
  source.slice(0, mainRoot.index) +
  `:root {${newTokenBlock}\n}` +
  out.join("\n").replace(/\s*$/, "") +
  "\n" +
  hiFiBlock;

if (CHECK_ONLY) {
  if (next !== source) {
    console.error("✗ 兼容性回退未同步：请运行 `npm run tokens:gen` 并提交结果");
    process.exit(1);
  }
  console.log(`✓ 已同步（令牌 ${hiFiTokens.length + extraCustomProps.length} 条 + 规则覆盖 ${ruleOverrides.length} 条进 @supports；成对回退 ${paired.length} 条）`);
  process.exit(0);
}

fs.writeFileSync(CSS_PATH, next);
console.log(`令牌 ${hiFiTokens.length + extraCustomProps.length} 条 + 规则覆盖 ${ruleOverrides.length} 条收进 @supports；成对回退 ${paired.length} 条`);
for (const s of skipped) console.log(`  · currentColor 跳过：${s.selector} { ${s.prop} }（需手写中性回退）`);