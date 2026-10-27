#!/usr/bin/env node
// 界面文案的完整性门禁。
//
// 消息键是**简体中文原文**（见 `src/lib/i18n.svelte.ts` 的文件头）。这个脚本做四件事：
//   1. 把所有英文词典片段合并，报出**同一个 key 在两份片段里译得不一样**；
//   2. 扫 `src/**` 里的 `t('…')`，报出**词典里没有的 key**；
//   3. 校验占位符：msgid 里的 `{name}` 必须在每一条译文里原样出现；
//   4. 兜底扫描：模板文本 / 属性里还留着中文、且没有包进 `t()` 的，列出来（警告）。
//
// 退出码：1 = 有缺译 / 占位符对不上 / 词典冲突；0 = 通过（残留中文只警告）。

import { readFileSync, readdirSync, statSync } from 'node:fs';
import { join, relative, sep } from 'node:path';

const ROOT = process.cwd();
const SRC = join(ROOT, 'src');
const LOCALES = join(SRC, 'lib', 'i18n', 'locales', 'en');

const walk = (dir) => {
  const out = [];
  for (const name of readdirSync(dir)) {
    const path = join(dir, name);
    if (statSync(path).isDirectory()) out.push(...walk(path));
    else out.push(path);
  }
  return out;
};

const readJson = (path) => JSON.parse(readFileSync(path, 'utf8'));

const fragments = new Map();
for (const file of readdirSync(LOCALES).filter((f) => f.endsWith('.json'))) {
  fragments.set(file, readJson(join(LOCALES, file)));
}

/** key → { value, file }；同一个 key 在多份片段里出现且值不同 = 冲突。 */
const catalog = new Map();
const conflicts = [];
for (const [file, entries] of fragments) {
  for (const [key, value] of Object.entries(entries)) {
    const prev = catalog.get(key);
    if (prev && JSON.stringify(prev.value) !== JSON.stringify(value)) {
      conflicts.push(`${key}\n    ${prev.file}: ${JSON.stringify(prev.value)}\n    ${file}: ${JSON.stringify(value)}`);
      continue;
    }
    if (!prev) catalog.set(key, { value, file });
  }
}

const PLURAL_KEYS = ['zero', 'one', 'two', 'few', 'many', 'other'];
const isPlural = (value) =>
  value && typeof value === 'object' && !Array.isArray(value) &&
  Object.keys(value).length > 0 && Object.keys(value).every((k) => PLURAL_KEYS.includes(k));

const placeholders = (text) => new Set([...text.matchAll(/\{(\w+)\}/g)].map((m) => m[1]));
const samePlaceholders = (a, b) => {
  const x = placeholders(a);
  const y = placeholders(b);
  return x.size === y.size && [...x].every((k) => y.has(k));
};

/** 去掉注释，但保留字符串与模板（行号基本可用）。 */
function stripComments(source) {
  let out = '';
  let i = 0;
  let quote = null;
  while (i < source.length) {
    const c = source[i];
    const n = source[i + 1];
    if (quote) {
      out += c;
      if (c === '\\') {
        out += n ?? '';
        i += 2;
        continue;
      }
      if (c === quote) quote = null;
      i += 1;
      continue;
    }
    if (c === '"' || c === "'" || c === '`') {
      quote = c;
      out += c;
      i += 1;
      continue;
    }
    if (c === '/' && n === '/') {
      while (i < source.length && source[i] !== '\n') i += 1;
      continue;
    }
    if (c === '/' && n === '*') {
      const end = source.indexOf('*/', i + 2);
      i = end === -1 ? source.length : end + 2;
      continue;
    }
    if (c === '<' && source.startsWith('<!--', i)) {
      const end = source.indexOf('-->', i + 4);
      i = end === -1 ? source.length : end + 3;
      continue;
    }
    out += c;
    i += 1;
  }
  return out;
}

const files = walk(SRC).filter((p) => p.endsWith('.svelte') || p.endsWith('.ts'));
const used = new Map(); // key → 第一处 file:line

/** 把 JS 字符串字面量里的常见转义还原（msgid 里可能有 `\\n` 等）。 */
const unescapeJs = (value) =>
  value.replace(/\\(u\{[0-9a-fA-F]+\}|u[0-9a-fA-F]{4}|x[0-9a-fA-F]{2}|n|r|t|b|f|v|0|['"`\\])/g, (whole, esc) => {
    if (esc[0] === 'u') return String.fromCodePoint(parseInt(esc.replace(/[u{}]/g, ''), 16));
    if (esc[0] === 'x') return String.fromCharCode(parseInt(esc.slice(1), 16));
    return { n: '\n', r: '\r', t: '\t', b: '\b', f: '\f', v: '\v', '0': '\0' }[esc] ?? esc;
  });

const T_CALL = /\bt\(\s*(['"])((?:\\.|(?!\1).)*)\1/g;
for (const file of files) {
  const source = stripComments(readFileSync(file, 'utf8'));
  for (const match of source.matchAll(T_CALL)) {
    const key = unescapeJs(match[2]);
    if (!used.has(key)) {
      const line = source.slice(0, match.index).split('\n').length;
      used.set(key, `${relative(ROOT, file)}:${line}`);
    }
  }
}

const missing = [];
const badPlaceholders = [];
const badPlural = [];
for (const [key, where] of used) {
  const entry = catalog.get(key);
  if (!entry) {
    // 纯英文/品牌名不需要词典（源语言原样显示，英语也原样显示）。
    if (/[\u4e00-\u9fff]/.test(key)) missing.push(`${where}  ${key}`);
    continue;
  }
  const want = placeholders(key);
  if (typeof entry.value === 'string') {
    if (!samePlaceholders(key, entry.value)) {
      badPlaceholders.push(`${where}  ${key}\n    en: ${entry.value}`);
    }
  } else if (isPlural(entry.value)) {
    for (const [form, text] of Object.entries(entry.value)) {
      if (!samePlaceholders(key, text)) {
        badPlaceholders.push(`${where}  ${key} [${form}]\n    en: ${text}`);
      }
    }
  } else {
    badPlural.push(`${where}  ${key}  → ${JSON.stringify(entry.value)}`);
  }
}

// 词典里存在、但源码里没有 `t()` 使用的条目（不算错：可能是公共词条）。
const unused = [...catalog.keys()].filter((key) => !used.has(key) && /[\u4e00-\u9fff]/.test(key));

// 兜底：还没包进 t() 的中文（模板文本 / 属性 / 字符串）。只报警告。
const leftovers = [];
for (const file of files) {
  const source = stripComments(readFileSync(file, 'utf8'));
  const withoutCalls = source.replace(T_CALL, (m) => m.replace(/[\u4e00-\u9fff]/g, ' '));
  const lines = withoutCalls.split('\n');
  for (let i = 0; i < lines.length; i += 1) {
    const line = lines[i];
    if (!/[\u4e00-\u9fff]/.test(line)) continue;
    if (/console\.|debug!|info!|warn!|error!|tracing::|\.log\(/.test(line)) continue;
    leftovers.push(`${relative(ROOT, file)}:${i + 1}  ${line.trim().slice(0, 160)}`);
  }
}

if (conflicts.length) {
  console.error(`\n✖ 词典冲突（同一个 key 译成了不同的值）：\n${conflicts.join('\n')}`);
}
if (missing.length) {
  console.error(`\n✖ 缺译 ${missing.length} 条：\n${missing.join('\n')}`);
}
if (badPlaceholders.length) {
  console.error(`\n✖ 占位符不一致 ${badPlaceholders.length} 条：\n${badPlaceholders.join('\n')}`);
}
if (badPlural.length) {
  console.error(`\n✖ 复数词条形状不对 ${badPlural.length} 条：\n${badPlural.join('\n')}`);
}
if (leftovers.length) {
  console.warn(`\n⚠ 仍有 ${leftovers.length} 行中文没有包进 t()（日志/注释请忽略）：\n${leftovers.slice(0, 80).join('\n')}${leftovers.length > 80 ? '\n…' : ''}`);
}
if (unused.length) {
  console.warn(`\n⚠ 词典里有 ${unused.length} 条没被用到（公共词条可忽略）。`);
}

const failed = conflicts.length + missing.length + badPlaceholders.length + badPlural.length;
console.log(
  `\ni18n: ${used.size} 个调用点，${catalog.size} 条英文词条，${failed ? `${failed} 处错误` : '通过'}。`,
);
process.exit(failed ? 1 : 0);