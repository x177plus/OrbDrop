/**
 * 界面语言（桌面端与移动端共用）。
 *
 * ## 消息键就是**简体中文原文**
 *
 * 这个仓库的界面文案原本就是中文、直接写在模板里。为了不把几百条文案先改名成一套
 * 一次性 key、再各写两份译文，这里采用 gettext 那套做法：**中文原文就是 msgid**。
 *
 * - 源语言（`zh-CN`）不需要译文：查不到就直接用 msgid 本身，占位符照旧替换。
 * - `en` 的译文按 msgid 存在 `i18n/locales/en/*.json` 里。
 * - 系统语言不在支持列表里 → 一律英语（`en`）。
 *
 * 代价与约定：
 * - 同一条中文在不同上下文要译成不同英文时，改中文原文区分（加一个词），不要造第二套 key。
 * - 动态内容一律写成**带占位符的完整句子**：`t('已清空 {n} 台设备的凭据', { n })`，
 *   不要用字符串拼句（英文语序与中文不同）。
 * - 复数：译文写成 `{ "one": "...", "other": "..." }`，调用方把数量放进 `count` 或 `n`。
 *
 * ## 响应式
 *
 * `locale` 是 `$state`，`t()` 在读取它 —— 模板里调用 `t(...)` 会建立依赖，切换语言后
 * 整棵用到它的界面自动重绘。`.ts` 文件里也能导入（只是不能写 `$state`）。
 *
 * ## 默认语言
 *
 * **没有「跟随系统」这一档**：首次启动直接取系统语言（支持 `en*` / `zh-CN` / `zh-Hans*`），
 * 不支持时用英语；解析出来的**具体语言**就是设置里的当前值，也写进后端配置与 localStorage。
 * 之后系统语言再变也不会悄悄改掉用户的选择 —— 想换语言在设置里手动选。
 * 旧版本留下的 `auto` 仍在读取时解析成系统语言（配置与 localStorage 都是），随后写成具体值。
 */

import common from './i18n/locales/en/common.json';
import app from './i18n/locales/en/app.json';
import desktopSettings from './i18n/locales/en/desktop-settings.json';
import desktopConvo from './i18n/locales/en/desktop-convo.json';
import desktopComponents from './i18n/locales/en/desktop-components.json';
import stores from './i18n/locales/en/stores.json';
import mobileCore from './i18n/locales/en/mobile-core.json';
import mobileTabs from './i18n/locales/en/mobile-tabs.json';
import mobileSheetsA from './i18n/locales/en/mobile-sheets-a.json';
import mobileSheetsB from './i18n/locales/en/mobile-sheets-b.json';

export type Locale = 'en' | 'zh-CN';

export type TParams = Record<string, string | number | undefined>;

type PluralForms = Partial<Record<Intl.LDMLPluralRule, string>> & { other: string };
type Translation = string | PluralForms;

const CATALOGS: Record<Locale, Record<string, Translation> | null> = {
  en: Object.assign(
    {},
    common,
    app,
    desktopSettings,
    desktopConvo,
    desktopComponents,
    stores,
    mobileCore,
    mobileTabs,
    mobileSheetsA,
    mobileSheetsB,
  ) as Record<string, Translation>,
  // 源语言：msgid 就是中文，不需要再抄一份。
  'zh-CN': null,
};

/** 支持的语言。顺序 = 设置里的排列顺序；`label` 用该语言自己的写法。 */
export const SUPPORTED_LOCALES: ReadonlyArray<{ id: Locale; label: string }> = [
  { id: 'en', label: 'English' },
  { id: 'zh-CN', label: '简体中文' },
];

const STORAGE_KEY = 'orbdrop.language';
const PLURAL_KEYS: Record<string, true> = {
  zero: true,
  one: true,
  two: true,
  few: true,
  many: true,
  other: true,
};

function storageGet(): string | null {
  try {
    return typeof localStorage !== 'undefined' ? localStorage.getItem(STORAGE_KEY) : null;
  } catch {
    return null;
  }
}

function storageSet(value: string): void {
  try {
    if (typeof localStorage !== 'undefined') localStorage.setItem(STORAGE_KEY, value);
  } catch {
    /* 隐私模式 / 存储被禁用：不影响本次会话。 */
  }
}

/**
 * 把 BCP 47 语言标签收进支持列表。返回 `null` = 不支持。
 *
 * `zh` / `zh-Hans*` / `zh-CN` / `zh-SG` 归到简体中文；繁体（`zh-Hant*`、`zh-TW`、
 * `zh-HK`、`zh-MO`）**不**假装是简体 —— 按规则回退到英语。
 */
export function normalizeLocale(tag: string | null | undefined): Locale | null {
  if (!tag) return null;
  const raw = tag.trim().replace(/_/g, '-').toLowerCase();
  if (!raw) return null;
  if (raw === 'zh-hant' || raw.startsWith('zh-hant-')) return null;
  if (raw === 'zh-tw' || raw.startsWith('zh-tw-')) return null;
  if (raw === 'zh-hk' || raw.startsWith('zh-hk-')) return null;
  if (raw === 'zh-mo' || raw.startsWith('zh-mo-')) return null;
  if (raw === 'en' || raw.startsWith('en-')) return 'en';
  if (raw === 'zh' || raw.startsWith('zh-')) {
    // 剩下的是 zh-Hans* / zh-CN / zh-SG 一类；`zh-*` 的未知子标签按简体处理。
    return 'zh-CN';
  }
  return null;
}

/** 系统语言；都不支持时英语。只看主语言（与后端 `sys-locale` 同一口径）。 */
export function systemLocale(): Locale {
  if (typeof navigator === 'undefined') return 'en';
  const primary = navigator.language || (Array.isArray(navigator.languages) ? navigator.languages[0] : '');
  return normalizeLocale(primary) ?? 'en';
}

/**
 * 把配置 / localStorage 里的值收成设置里真正选中的语言。
 *
 * - 空值或旧版的 `auto` = 首次启动（或旧数据迁移）：取系统语言。
 * - 支持的语言原样保留；其他明确写下的语言（含繁体）按规则回退英语。
 */
export function normalizePreference(pref: string | null | undefined): Locale {
  if (!pref || pref.trim().toLowerCase() === 'auto') return systemLocale();
  return normalizeLocale(pref) ?? 'en';
}

function applyDocumentLang(value: Locale): void {
  try {
    if (typeof document !== 'undefined') document.documentElement.lang = value;
  } catch {
    /* 非 DOM 环境（测试 / 构建期）没有 document。 */
  }
}

const initialLocale = normalizePreference(storageGet());
let locale = $state<Locale>(initialLocale);
// 首次启动 / 旧版 `auto`：把解析出来的具体语言记下来，不再保留「跟随系统」。
storageSet(initialLocale);
applyDocumentLang(initialLocale);

/** 当前生效的语言（也就是设置里选中的那一项）。 */
export function getLocale(): Locale {
  return locale;
}

/**
 * 应用一个语言：立即重绘界面、写入 localStorage、同步 `<html lang>`。
 *
 * 后端配置由调用方另写（桌面端/移动端设置页调用 `set_language`）：这一层不依赖
 * Tauri API，测试与浏览器预览也能单独跑。
 */
export function setLanguagePreference(pref: Locale | string): void {
  const next = normalizePreference(pref);
  locale = next;
  storageSet(next);
  applyDocumentLang(next);
}

/** 拿到后端配置后的对齐：配置优先于 localStorage，但不重复写盘。 */
export function adoptLanguagePreference(pref: string | null | undefined): void {
  const next = normalizePreference(pref);
  if (next === locale) return;
  locale = next;
  storageSet(next);
  applyDocumentLang(next);
}

function pluralText(forms: PluralForms, count: number | undefined): string {
  if (count === undefined || !Number.isFinite(count)) return forms.other;
  let rule: Intl.LDMLPluralRule = 'other';
  try {
    rule = new Intl.PluralRules(locale).select(count);
  } catch {
    rule = count === 1 ? 'one' : 'other';
  }
  // `Intl.PluralRules` 对 0 给 "other"；需要单独写 0 的语言（英语）允许 `zero` 覆盖。
  if (count === 0 && forms.zero) return forms.zero;
  return forms[rule] ?? forms.other;
}

/**
 * 取一条文案。
 *
 * @param msgid 简体中文原文（源语言即 msgid）
 * @param params 占位符表；数量放 `count`（或 `n`）以便按语言选复数形式
 */
export function t(msgid: string, params?: TParams): string {
  const table = CATALOGS[locale];
  const entry = table?.[msgid];
  let text: string;
  if (entry === undefined) {
    text = msgid;
  } else if (typeof entry === 'string') {
    text = entry;
  } else {
    const raw = params?.count ?? params?.n;
    const count = typeof raw === 'number' ? raw : raw !== undefined ? Number(raw) : undefined;
    text = pluralText(entry, count);
  }
  if (!params) return text;
  return text.replace(/\{(\w+)\}/g, (whole, name: string) => {
    const value = params[name];
    return value === undefined ? whole : String(value);
  });
}

/** 供校验脚本与测试读：所有已登记的英文 msgid。 */
export function englishMessageIds(): string[] {
  return Object.keys(CATALOGS.en ?? {});
}

/** 一条译文是否是复数形式表（校验脚本用）。 */
export function isPluralForms(value: unknown): value is PluralForms {
  if (!value || typeof value !== 'object') return false;
  const keys = Object.keys(value);
  return keys.length > 0 && keys.every((k) => PLURAL_KEYS[k]) && 'other' in value;
}