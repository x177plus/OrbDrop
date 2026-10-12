//! 收到对端消息时在**手机的通知栏**里出一条（F-108）。
//!
//! 为什么通知在**后端**发、而不是界面发：通知的意义就是"人没看着屏幕的时候也得知道"。
//! 应用切到后台时 `WryActivity.onPause` 会 `webView.onPause()`（JS 的定时器全停），界面本身
//! 也可能被系统回收；而后端一直在收消息（`ws::session::handle_inbound`）—— 那条路不依赖视图。
//! 所以这条通知从"消息落账"的那一步直接出，中间不经过界面。
//!
//! 落地在 Kotlin（`MessageNotifierPlugin`）：渠道、图标、点一下回到哪个会话都是安卓那一侧的事。
//! Rust 这边只有三样：一个插件句柄（`plugin()` 注册时放进来的）、一个 [`message()`]、
//! 一份"这条会话在通知里怎么标识"（[`Convo`]）。
//!
//! 桌面端没有实体（[`message()`] 是空操作）：桌面端本来就有一块看得见的面板，右下角那组
//! 通知气泡说的是别的事（推送与配对邀请），消息进去只会把面板变成第二个通知中心。

#[cfg(mobile)]
use std::sync::OnceLock;
#[cfg(mobile)]
use serde::{Deserialize, Serialize};
#[cfg(mobile)]
use tauri::plugin::{PluginHandle, TauriPlugin};
#[cfg(mobile)]
use tracing::warn;

/// 通知要落到哪条会话上：点一下通知，界面要**直接进这条会话**（而不是停在列表里）。
///
/// 只带"哪一种 + 后端那份 id"，不带前端那串会话键（`dev:` / `grp:`）：键是界面自己的约定，
/// 后端复制一份就成了两处得同时改的东西。界面拿这两个字段自己拼（`chat.convoKeyOf`）。
pub enum Convo {
    /// 一对一（id = 对方设备 id）。
    Direct(String),
    /// 群聊（id = 群 id）。
    Group(String),
}

#[cfg(mobile)]
impl Convo {
    /// 传给 Kotlin 的两个字段（`kind` 的取值与前端 `chat.convoKeyOf` 约定一致）。
    fn wire(&self) -> (&'static str, &str) {
        match self {
            Convo::Direct(id) => ("dev", id),
            Convo::Group(id) => ("grp", id),
        }
    }
}

/// 插件句柄。注册发生在 `run()` 里挂插件那一刻，而用它是在收到消息那一刻 —— 一个静态格
/// 比往 `AppState` 里绕一圈简单：那条路要等 `setup` 才建得起来，插件却注册在它之前。
#[cfg(mobile)]
static HANDLE: OnceLock<PluginHandle<tauri::Wry>> = OnceLock::new();

/// 界面（Activity / WebView）还在吗 —— 见 [`set_ui_alive`]。开局为真：进程就是从界面起来的。
#[cfg(mobile)]
static UI_ALIVE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(true);

/// 记下"界面已经没了"（安卓把 Activity 销毁时，`lib.rs` 的 `on_window_event` 调它）。
///
/// **为什么非要有这个标记**：Rust 叫 Kotlin 那条路（wry 的 `dispatch`）在"一个 Activity 都不剩"
/// 时是 `expect("no available activity")` —— 直接 panic。而本仓的 release 档写着
/// `panic = "abort"`，也就是**整个进程当场倒地**（连前台服务一起）。收消息这条路上不能有这种雷：
/// 消息照收、账照落，只是"界面都没了"的时候不再指望通知栏（那时界面本来也画不出东西）。
///
/// 反过来（标记还在、其实界面没了）只会在"销毁与这一条消息同时发生"的缝里出现，而那个瞬间
/// 进程正在退出（`WindowEvent::Destroyed` 之后运行循环就走 `Exit`），够不着。
#[cfg(mobile)]
pub fn set_ui_alive(alive: bool) {
    UI_ALIVE.store(alive, std::sync::atomic::Ordering::Relaxed);
}

/// 通知插件（只在安卓构建里挂，见 `lib.rs`）。
#[cfg(mobile)]
pub fn plugin() -> TauriPlugin<tauri::Wry> {
    tauri::plugin::Builder::new("orbdrop-notifier")
        .setup(|_app, api| {
            // 类名与 Kotlin 那边的 `@TauriPlugin class MessageNotifierPlugin` 对应。
            let handle = api.register_android_plugin("com.x177plus.orbdrop", "MessageNotifierPlugin")?;
            if HANDLE.set(handle).is_err() {
                warn!("通知插件重复注册：沿用第一份句柄");
            }
            Ok(())
        })
        .build()
}

/// 收到一条对端消息 → 手机通知栏里出一条。
///
/// `title` / `body` 由调用方给：通知栏是一行字的场合，写什么得看是哪一种消息
/// （一对一是"谁说了什么"，群聊是"哪个群、谁说了什么"）。
///
/// **只该在"确实新记下一条"时调用**（调用方拿落账那步的返回值判断）：重复投递是常态
/// （补发、重连重投、HTTP 到了但对端没来得及回话），同一条消息响第二遍比不响更烦人。
///
/// 失败只记一行日志：通知没出成不该影响消息本身（消息早就落账了）。
#[cfg(mobile)]
pub async fn message(convo: &Convo, title: &str, body: &str) {
    let Some(handle) = HANDLE.get() else {
        warn!("通知插件句柄不在：跳过这条通知");
        return;
    };
    // 界面已经没了（Activity 被销毁）：不能叫 Kotlin —— 那条路会 panic，而 release 档是
    // `panic = "abort"`（见 `set_ui_alive`）。消息早就落账了，这一条通知跳过即可。
    if !UI_ALIVE.load(std::sync::atomic::Ordering::Relaxed) {
        return;
    }
    let (kind, id) = convo.wire();
    let payload = serde_json::json!({
        "kind": kind,
        "id": id,
        "title": title,
        "body": body,
    });
    if let Err(e) = handle
        .run_mobile_plugin_async::<serde_json::Value>("notifyMessage", payload)
        .await
    {
        warn!(error = %e, "通知没出成");
    }
}

/// 桌面端：没有通知栏这回事，空操作。
#[cfg(not(mobile))]
pub async fn message(_convo: &Convo, _title: &str, _body: &str) {}

// ---------------------------------------------------------------------------
// 通知这一层的**自查与跳转**（F-108 / F-87 那一半：用户去系统设置里把横幅打开）
// ---------------------------------------------------------------------------

/// 通知这一层此刻的真实状态（Kotlin `MessageNotifierPlugin.notificationState`）。
///
/// **为什么要读**：横幅不是权限，而是"渠道重要程度 ≥ 高 **且** 这条通知真的会响/震"的派生表现
/// （AOSP `NotificationInterruptStateProviderImpl.shouldHeadsUpWhenAwake` 里那道
/// `entry.getImportance() < IMPORTANCE_HIGH → 不弹`）。而渠道的重要性**建完只能降不能升**，
/// 所以要区分"我们声明得不对"与"用户在别处关了它"，界面只能拿这个值说真话。
#[cfg(mobile)]
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NotifyState {
    /// 应用级通知总开关（Android 13+ 的 `POST_NOTIFICATIONS` / 系统设置里那条"允许通知"）。
    pub enabled: bool,
    /// 「消息通知」渠道此刻的重要程度（`NotificationManager.IMPORTANCE_*`，2=低 3=默认 4=高）；
    /// `-1` = 渠道还没建（重要性的取值域是 0–5，负数不会撞上）。
    pub message_channel_importance: i32,
}

#[cfg(mobile)]
#[tauri::command]
pub async fn notification_state() -> Result<NotifyState, String> {
    let Some(handle) = HANDLE.get() else {
        return Err("通知插件句柄不在（内部错误）".into());
    };
    handle
        .run_mobile_plugin_async::<NotifyState>("notificationState", serde_json::json!({}))
        .await
        .map_err(|e| e.to_string())
}

/// 跳到本应用的通知设置页（Kotlin `openNotificationSettings`）。
#[cfg(mobile)]
#[tauri::command]
pub async fn open_notification_settings() -> Result<(), String> {
    let Some(handle) = HANDLE.get() else {
        return Err("通知插件句柄不在（内部错误）".into());
    };
    handle
        .run_mobile_plugin_async::<serde_json::Value>("openNotificationSettings", serde_json::json!({}))
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

/// 跳到「消息通知」那一条渠道自己的设置页 —— 横幅开关就在那儿（Kotlin `openChannelSettings`）。
#[cfg(mobile)]
#[tauri::command]
pub async fn open_message_channel_settings() -> Result<(), String> {
    let Some(handle) = HANDLE.get() else {
        return Err("通知插件句柄不在（内部错误）".into());
    };
    handle
        .run_mobile_plugin_async::<serde_json::Value>("openChannelSettings", serde_json::json!({}))
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}