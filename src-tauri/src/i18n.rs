//! 后端自己的少量界面文案（托盘菜单、安卓通知栏）。
//!
//! 界面正文（面板、移动端、提示条）全部在前端，由 `src/lib/i18n.svelte.ts` 管；这里只兜住
//! **前端管不到的两处**：
//!
//! - 桌面端托盘菜单与 tooltip（原生菜单，由 Rust 建）；
//! - 安卓通知栏（消息落账那一刻由 `notify::message` 直接出，界面可能已经被系统回收）。
//!
//! 语言偏好落在 `Config::language`：`"auto"` = 跟随系统，`"en"` / `"zh-CN"` = 显式选择。
//! `auto` 用 `sys-locale` 读系统语言；读不到或不支持时**一律英语** —— 与前端同一条规则。
//! 前端切换语言时会调 `set_language`，这里换掉全局值并让托盘就地改字（不重建图标）。

use std::sync::atomic::{AtomicU8, Ordering};

/// 实际生效的语言（只有前端支持的那两种）。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    En = 0,
    ZhCn = 1,
}

impl Lang {
    pub fn id(self) -> &'static str {
        match self {
            Lang::En => "en",
            Lang::ZhCn => "zh-CN",
        }
    }

    fn from_byte(byte: u8) -> Self {
        if byte == Lang::ZhCn as u8 {
            Lang::ZhCn
        } else {
            Lang::En
        }
    }
}

/// 当前生效语言。启动时 `init`，设置页保存/切换时 `set_preference`。
static CURRENT: AtomicU8 = AtomicU8::new(Lang::En as u8);

/// BCP 47 标签 → 支持的语言。`zh` / `zh-Hans*` / `zh-CN` / `zh-SG` 归简体；
/// 繁体不假装是简体（按规则回退英语）。
pub fn locale_from_tag(tag: &str) -> Option<Lang> {
    let raw = tag.trim().replace('_', "-").to_ascii_lowercase();
    if raw.is_empty() {
        return None;
    }
    let is = |prefix: &str| raw == prefix || raw.starts_with(&format!("{prefix}-"));
    if is("en") {
        return Some(Lang::En);
    }
    if is("zh-hant") || is("zh-tw") || is("zh-hk") || is("zh-mo") {
        return None;
    }
    if is("zh") {
        return Some(Lang::ZhCn);
    }
    None
}

/// 系统语言 → 支持的语言；不支持 / 读不到 = 英语。
pub fn system_lang() -> Lang {
    sys_locale::get_locale()
        .as_deref()
        .and_then(locale_from_tag)
        .unwrap_or(Lang::En)
}

/// 把配置值收成规范字面量：**具体语言**，不再保留 `auto`。
///
/// `auto` / 空串 = 旧配置或首次启动：解析成当前系统语言并写死（系统语言不支持时为 `en`）。
/// 其他不认识的显式值按**英语**处理（不是跟随系统）。
pub fn canonical_preference(preference: &str) -> String {
    let pref = preference.trim();
    if pref.is_empty() || pref.eq_ignore_ascii_case("auto") {
        return system_lang().id().to_string();
    }
    match locale_from_tag(pref) {
        Some(Lang::ZhCn) => "zh-CN".to_string(),
        _ => "en".to_string(),
    }
}

/// 解析偏好：旧 `auto` / 空串读系统，显式值直接用；无法识别 = 英语。
pub fn resolve(preference: &str) -> Lang {
    let pref = preference.trim();
    if pref.is_empty() || pref.eq_ignore_ascii_case("auto") {
        system_lang()
    } else {
        locale_from_tag(pref).unwrap_or(Lang::En)
    }
}

/// 启动时在 `boot_state` 里调一次。
pub fn init(preference: &str) {
    CURRENT.store(resolve(preference) as u8, Ordering::Relaxed);
}

/// 设置页切换语言：换全局值并返回规范后的偏好（调用方落盘用）。
pub fn set_preference(preference: &str) -> String {
    let canonical = canonical_preference(preference);
    CURRENT.store(resolve(&canonical) as u8, Ordering::Relaxed);
    canonical
}

pub fn current() -> Lang {
    Lang::from_byte(CURRENT.load(Ordering::Relaxed))
}

/// 后端固定文案。key 用英文短名（与前端 msgid 那套无关）；查不到时返回 key 本身，
/// 方便测试和日志一眼看出漏了哪条。
pub fn translate(lang: Lang, key: &'static str) -> &'static str {
    match (lang, key) {
        (Lang::ZhCn, "tray.status") => "状态",
        (_, "tray.status") => "Status",
        (Lang::ZhCn, "tray.online") => "在线",
        (_, "tray.online") => "Online",
        (Lang::ZhCn, "tray.offline") => "离线",
        (_, "tray.offline") => "Offline",
        (Lang::ZhCn, "tray.quit") => "退出",
        (_, "tray.quit") => "Quit",
        (Lang::ZhCn, "tray.tooltip.online") => "OrbDrop · 在线",
        (_, "tray.tooltip.online") => "OrbDrop · Online",
        (Lang::ZhCn, "tray.tooltip.offline") => "OrbDrop · 离线（对局域网隐身）",
        (_, "tray.tooltip.offline") => "OrbDrop · Offline (invisible on LAN)",
        (Lang::ZhCn, "notify.image") => "[图片]",
        (_, "notify.image") => "[Image]",
        (Lang::ZhCn, "notify.group") => "群聊",
        (_, "notify.group") => "Group chat",
        _ => key,
    }
}

pub fn t(key: &'static str) -> &'static str {
    translate(current(), key)
}

/// 群消息通知正文：`发言人 + 内容`。中文用全角冒号，英文用 `": "`。
pub fn group_notification_body(lang: Lang, who: &str, body: &str) -> String {
    match lang {
        Lang::ZhCn => format!("{who}：{body}"),
        Lang::En => format!("{who}: {body}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locale_aliases_cover_simplified_and_fall_back_to_english() {
        assert_eq!(locale_from_tag("zh-Hans-CN"), Some(Lang::ZhCn));
        assert_eq!(locale_from_tag("zh-CN"), Some(Lang::ZhCn));
        assert_eq!(locale_from_tag("zh"), Some(Lang::ZhCn));
        assert_eq!(locale_from_tag("en-US"), Some(Lang::En));
        assert_eq!(locale_from_tag("zh-TW"), None);
        assert_eq!(locale_from_tag("fr-FR"), None);
        assert_eq!(locale_from_tag(""), None);
    }

    #[test]
    fn unknown_explicit_preference_resolves_to_english() {
        assert_eq!(resolve("fr"), Lang::En);
        assert_eq!(canonical_preference("fr"), "en");
        assert_eq!(canonical_preference("zh-Hans"), "zh-CN");
        assert_eq!(canonical_preference("zh-Hans-CN"), "zh-CN");
        assert_eq!(canonical_preference("en-US"), "en");
        // 旧 `auto` 与空值都解析成具体系统语言，不再写回 `auto`。
        assert_eq!(canonical_preference("auto"), system_lang().id());
        assert_eq!(canonical_preference(""), system_lang().id());
        assert_eq!(resolve("auto"), system_lang());
        assert_eq!(resolve(""), system_lang());
    }

    #[test]
    fn tray_and_notification_strings_are_translated() {
        assert_eq!(translate(Lang::En, "tray.status"), "Status");
        assert_eq!(translate(Lang::ZhCn, "tray.status"), "状态");
        assert_eq!(
            translate(Lang::En, "tray.tooltip.offline"),
            "OrbDrop · Offline (invisible on LAN)"
        );
        assert_eq!(
            group_notification_body(Lang::En, "Alice", "hi"),
            "Alice: hi"
        );
        assert_eq!(
            group_notification_body(Lang::ZhCn, "Alice", "hi"),
            "Alice：hi"
        );
    }
}
