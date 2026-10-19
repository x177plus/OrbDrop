//! 移动端才有的后端命令。
//!
//! 这三条**只在移动端登记**（`lib.rs` 的 handler 列表里带 `#[cfg(mobile)]`）：桌面端的收件
//! 结果直接交给系统文件管理器看，而手机上没有一个"文件夹"可浏览 —— 收到的东西必须在应用里
//! 列出来，否则用户没有任何入口能碰到它。
//!
//! 落盘布局是共用的事实（`transfer/push_job.rs`）：推送收到的写在
//! `<下载目录>/<发送方名>/<清单里的相对路径>`，本机手动下载的写在 `<下载目录>/<文件名>`。
//! 所以"这条是谁给我的"就是**第一层子目录名**，不再另记一份元数据。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::UNIX_EPOCH;

#[cfg(mobile)]
use std::sync::OnceLock;
use tauri::State;
#[cfg(mobile)]
use tauri::plugin::{PluginHandle, TauriPlugin};
use tracing::info;
#[cfg(mobile)]
use tracing::warn;

use crate::state::AppState;

/// 列表最多列这么多条：手机屏幕上滚不完的东西没有价值，"最近 200 个"覆盖了真实用法。
const INBOX_LIMIT: usize = 200;

/// 收件目录里的一项。
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InboxEntryDto {
    name: String,
    path: String,
    is_dir: bool,
    size: u64,
    /// epoch 毫秒（前端按它排序与显示）。
    modified: u64,
    /// 发送方设备名（来自第一层子目录名）；本机自己下载下来的为空串。
    from: String,
}

fn modified_ms(meta: &std::fs::Metadata) -> u64 {
    meta.modified()
        .ok()
        .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn entry_of(path: &Path, from: &str) -> Option<InboxEntryDto> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    let is_dir = meta.is_dir();
    Some(InboxEntryDto {
        name: path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default(),
        path: path.to_string_lossy().to_string(),
        is_dir,
        // 目录的大小没有意义（也不该为它递归求和）：置 0，前端按 isDir 显示"文件夹"。
        size: if is_dir { 0 } else { meta.len() },
        modified: modified_ms(&meta),
        from: from.to_string(),
    })
}

/// 收件目录的内容：本机下载的直接项 + 每个"发送方目录"里的直接项。
///
/// **只列两层**，不递归：推送过来的目录在清单里是一条，它里面的东西属于那一条 ——
/// 铺开成几百行既看不出层级，也点不动（打开那条目录才是正路）。
///
/// **必须 async**（P1-13⑥）：这条命令要读下载目录、每个发送方目录各一次 `read_dir`、每一项
/// 一次 `symlink_metadata` —— 手机上那是应用私有的外部存储，几百上千项时够卡住主线程好几帧。
/// 项目自己的规矩写在 `fs_browse.rs:216`：**同步命令跑在主线程上**，阻塞 IO 一律
/// `spawn_blocking` 挪走。
#[tauri::command]
pub async fn list_inbox(state: State<'_, Arc<AppState>>) -> Result<Vec<InboxEntryDto>, String> {
    let root = state.config.lock().download_dir.clone();
    tauri::async_runtime::spawn_blocking(move || list_inbox_at(&root))
        .await
        .map_err(|e| format!("列收件目录失败：{e}"))
}

/// 真正的读取逻辑（同步、在阻塞线程池上跑）。
fn list_inbox_at(root: &Path) -> Vec<InboxEntryDto> {
    let mut out: Vec<InboxEntryDto> = Vec::new();

    let Ok(dir) = std::fs::read_dir(root) else {
        return out;
    };
    for item in dir.flatten() {
        let path = item.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.is_dir() {
            // 一级子目录 = 一台发送方（或用户自己放进去的文件夹）。列它里面的直接项。
            // S-12 关掉那层时推送直接落在根里 —— 那时顶部这一层就是它们本身（显示上没有
            // "发送方"，`from` 空），这里的画法一个字都不用改。
            let sender = path
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_default();
            if let Ok(inner) = std::fs::read_dir(&path) {
                for sub in inner.flatten() {
                    if let Some(e) = entry_of(&sub.path(), &sender) {
                        out.push(e);
                    }
                }
            }
        } else if let Some(e) = entry_of(&path, "") {
            out.push(e);
        }
    }

    out.sort_by(|a, b| b.modified.cmp(&a.modified));
    out.truncate(INBOX_LIMIT);
    out
}

/// 删掉收件目录里的一项文件（会话与文件页那条「删除本地副本」）。
///
/// 两道闸：路径必须落在下载目录之内（前端传来的字符串是不可信输入，`..` 与绝对路径都要挡住），
/// 且只删文件 —— 目录是"一台设备给我的一批东西"或用户自己放进来的文件夹，误删整棵树的代价
/// 远大于收益。
/// **同样必须 async**（P1-13⑥）：`canonicalize` 与 `remove_file` 都是阻塞系统调用，
/// 在掉线的外置卡/慢存储上会卡成"点了没反应"。判据与删除动作一起挪到阻塞线程池上跑。
#[tauri::command]
pub async fn delete_inbox_file(path: String, state: State<'_, Arc<AppState>>) -> Result<(), String> {
    let root = state.config.lock().download_dir.clone();
    tauri::async_runtime::spawn_blocking(move || delete_inbox_file_at(&root, &path))
        .await
        .map_err(|e| format!("删除失败：{e}"))?
}

fn delete_inbox_file_at(root: &Path, path: &str) -> Result<(), String> {
    let root = root
        .canonicalize()
        .map_err(|e| format!("下载目录不可用: {e}"))?;
    let target = PathBuf::from(path)
        .canonicalize()
        .map_err(|e| format!("文件不存在: {e}"))?;
    if !target.starts_with(&root) {
        return Err("只能删除收件目录里的文件".into());
    }
    let meta = std::fs::symlink_metadata(&target).map_err(|e| e.to_string())?;
    if meta.is_dir() {
        return Err("这是一个文件夹，请逐个删除里面的文件".into());
    }
    std::fs::remove_file(&target).map_err(|e| format!("删除失败: {e}"))?;
    info!(path = %target.display(), "已删除收件目录里的一项");
    Ok(())
}

/// 修正一个只可能是"平台兜底"留下的下载目录。
///
/// 早先的 Android 构建把 Windows 那句兜底（`C:\\Users\\Default\\Desktop`）写了进去：那个路径
/// 不存在，于是"接收推送"会在那儿建目录并当场失败，而设置页里显示的还是一个用户既看不懂、
/// 也改不掉的怪路径。**只在移动端**做这件事，且只认"不是绝对 POSIX 路径"这一种：
/// 安卓上合法路径一定以 `/` 开头，用户自己挑的目录绝不会长成 Windows 那个样子。
/// 不拿"目录此刻不存在"当判据 —— 外置卡被拔掉的那一瞬间也不该被改写。
#[cfg(mobile)]
pub fn repair_download_dir(state: &AppState) {
    let mut config = state.config.lock();
    if config.download_dir.to_string_lossy().starts_with('/') {
        return;
    }
    config.download_dir = crate::config::default_download_dir();
    if let Err(e) = crate::config::save_config_at(&state.config_path, &config) {
        tracing::warn!("修正下载目录后落盘失败: {e}");
        return;
    }
    tracing::info!(dir = %config.download_dir.display(), "已把下载目录修正为应用私有收件目录");
}

/// 把"我们自己自动取名"留下的设备名换成现在取得到的好名字。
///
/// 两类历史值要换：
///
/// 1. **平台兜底串**（`OrbDrop-Mac`）：早先的 Android 构建没有取型号那一支，一路退到非
///    Windows 那句兜底 —— 那个名字在手机上没有任何意义（机器是安卓的），同一局域网里两台
///    手机还会重名；
/// 2. **型号本身**（`V2502A` / `24129PN74C`）：紧接着那版拿 `ro.product.model` 当默认名，
///    而国行 ROM 的 `model` 是型号代码，名字认不出是哪台机器 —— 现在能取到厂商的市场名
///    （`vivo X300` / `Xiaomi 15 Pro`）或至少"品牌 + 型号"，该换成那个。
///
/// 判据在 `config::should_replace_auto_name` 里：**逐字节等于我们自己会写下的那几个值**才动，
/// 用户自己起过的名字一个字都不碰。
#[cfg(mobile)]
pub fn repair_device_name(state: &AppState) {
    // 一条属性都读不到时 `default_device_name` 会自己退回 "OrbDrop-Android"（仍是"平台兜底"，
    // 但至少不再冒充别的平台）。
    let fixed = crate::config::default_device_name();
    let mut config = state.config.lock();
    if !crate::config::should_replace_auto_name(&config.device_name, &fixed) {
        return;
    }
    let old = std::mem::replace(&mut config.device_name, fixed.clone());
    if let Err(e) = crate::config::save_config_at(&state.config_path, &config) {
        tracing::warn!("修正设备名后落盘失败: {e}");
        return;
    }
    tracing::info!(%old, %fixed, "已把自动取的设备名换成设备型号/厂商名");
}

// ── 「用系统应用打开一个文件」（`open_path`）─────────────────────────────────────

/// 文件打开插件的句柄。与 `notify` 那边同款：注册在插件 `setup` 那一刻，用在使用那一刻。
#[cfg(mobile)]
static OPEN_HANDLE: OnceLock<PluginHandle<tauri::Wry>> = OnceLock::new();

/// 「打开文件」插件（只在安卓构建里挂，见 `lib.rs`）。
#[cfg(mobile)]
pub fn open_plugin() -> TauriPlugin<tauri::Wry> {
    tauri::plugin::Builder::new("orbdrop-file-open")
        .setup(|_app, api| {
            // 类名与 Kotlin 那边的 `@TauriPlugin class FileOpenPlugin` 对应。
            let handle = api.register_android_plugin("com.x177plus.orbdrop", "FileOpenPlugin")?;
            if OPEN_HANDLE.set(handle).is_err() {
                warn!("文件打开插件重复注册：沿用第一份句柄");
            }
            Ok(())
        })
        .build()
}

/// 用系统应用打开一个路径（安卓侧）。
///
/// **为什么不用 `tauri_plugin_opener` 的 `open_path`**（桌面端那条路还在用它）：它移动端那一条
/// 把路径当**裸字符串**发给 Kotlin（`run_mobile_plugin("open", path.into())`），而 Kotlin 侧
/// `OpenArgs` 要的是 `{url, with}` 那个对象 —— Jackson 当场反序列化失败，也就是安卓上"打开"
/// 任何文件都只会拿到一个报错；就算参数形状对上，它递给 `ACTION_VIEW` 的也是没有 scheme、不带
/// MIME 的东西，系统解析不出 activity。细节与实测见 `FileOpenPlugin.kt` 与
/// `docs/p1-implementation.md` §20。
///
/// 自己这条走 FileProvider 的 `content://`（根声明在 `res/xml/file_paths.xml`）+ 按扩展名给的
/// MIME：`.apk` 因此落到系统安装器上 —— 也就是"收到 APK 点一下直接装"（F-109）。
#[cfg(mobile)]
pub async fn open_file(path: String) -> Result<(), String> {
    let Some(handle) = OPEN_HANDLE.get() else {
        return Err("文件打开插件没注册上（内部错误）".into());
    };
    handle
        .run_mobile_plugin_async::<serde_json::Value>("openFile", serde_json::json!({ "path": path }))
        .await
        .map(|_| ())
        .map_err(|e| e.to_string())
}

// ── 「把输入法剪贴板里的那张图拷成暂存文件」（`resolve_image_uri`）─────────────────

/// 粘贴图片插件的句柄（与文件打开那条路同一个形状：`setup` 那一刻注册，用的时候取）。
#[cfg(mobile)]
static PASTE_HANDLE: OnceLock<PluginHandle<tauri::Wry>> = OnceLock::new();

/// 「粘贴图片」插件（只在安卓构建里挂，见 `lib.rs`）。
#[cfg(mobile)]
pub fn paste_plugin() -> TauriPlugin<tauri::Wry> {
    tauri::plugin::Builder::new("orbdrop-paste-image")
        .setup(|_app, api| {
            // 类名与 Kotlin 那边的 `@TauriPlugin class PasteImagePlugin` 对应。
            let handle = api.register_android_plugin("com.x177plus.orbdrop", "PasteImagePlugin")?;
            if PASTE_HANDLE.set(handle).is_err() {
                warn!("粘贴图片插件重复注册：沿用第一份句柄");
            }
            Ok(())
        })
        .build()
}

/// 安卓：把一个**别的应用给的图片 URI**（输入法剪贴板往输入框里插图时给的就是 `content://`）
/// 拷成一份暂存文件，返回它的路径。
///
/// 为什么非要过原生这一跳：`content://` 那串 URL 在 WebView 的 JS 里**读不出来**（它属于
/// ContentResolver，不是网页能取的地址），而会话里"发图片"走的是推送 —— 推送只认本机真实
/// 路径。拷到应用自己的缓存目录之后，那条路与桌面端粘贴的字节完全一致（同一个
/// `paste::paste_dir`... 不，桌面端落系统临时目录、这里落缓存目录，但**对推送而言都只是
/// 一个本机路径**，见 `transfer::manifest_for_paths`）。
#[cfg(mobile)]
pub async fn resolve_image_uri(uri: String) -> Result<PickedImage, String> {
    let Some(handle) = PASTE_HANDLE.get() else {
        return Err("粘贴图片插件没注册上（内部错误）".into());
    };
    let value = handle
        .run_mobile_plugin_async::<serde_json::Value>("resolveImage", serde_json::json!({ "uri": uri }))
        .await
        .map_err(|e| e.to_string())?;
    // Kotlin 那边回的是 `{path, size}`（`Plugin.resolve` 收 `JSObject?`，塞不了裸字符串）。
    let path = value["path"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| "原生那一跳没给出暂存路径".to_string())?;
    Ok(PickedImage { path, size: value["size"].as_u64().unwrap_or(0) })
}

/// 原生那一跳拷回来的东西：暂存路径 + 字节数（会话里那条气泡要显示"多大"，而这一层是唯一
/// 知道的地方 —— 少了它那条气泡会写「0 B」）。
#[cfg(mobile)]
#[derive(serde::Serialize)]
pub struct PickedImage {
    pub path: String,
    pub size: u64,
}

// ── 「会话里那颗＋：从系统选择器里挑图片 / 文件」（`pick_files`）──────────────────

/// 系统选择器插件的句柄（与文件打开、粘贴图片同款：插件 `setup` 那一刻注册，用的时候取）。
#[cfg(mobile)]
static PICK_HANDLE: OnceLock<PluginHandle<tauri::Wry>> = OnceLock::new();

/// 「选图片 / 选文件」插件（只在安卓构建里挂，见 `lib.rs`）。
#[cfg(mobile)]
pub fn pick_plugin() -> TauriPlugin<tauri::Wry> {
    tauri::plugin::Builder::new("orbdrop-pick-file")
        .setup(|_app, api| {
            // 类名与 Kotlin 那边的 `@TauriPlugin class PickFilePlugin` 对应。
            let handle = api.register_android_plugin("com.x177plus.orbdrop", "PickFilePlugin")?;
            if PICK_HANDLE.set(handle).is_err() {
                warn!("选文件插件重复注册：沿用第一份句柄");
            }
            Ok(())
        })
        .build()
}

/// 原生选择器交回来的一项。
///
/// `name` 是系统给的**真实名字**（`OpenableColumns.DISPLAY_NAME`）：会话里那条文件气泡、对端
/// 落盘的那一份都写它，用户才认得出"我发的是哪一张"；`size` 以拷好的那份文件为准（有些
/// provider 不报大小）；`mime` 给界面判"这是不是图片"（推送与图片消息是两条路）。
#[cfg(mobile)]
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct PickedFile {
    pub path: String,
    pub name: String,
    pub size: u64,
    pub mime: String,
}

/// 安卓：开一次系统选择器（`kind` = `"image"` 或 `"file"`），把选中的几项拷成应用缓存里的
/// 暂存文件，返回它们的路径 / 名字 / 大小。
///
/// **为什么必须有这一跳**：桌面端那颗「＋」开的是自绘的本机文件浏览器（`fs_browse` 那几条命令
/// 全被 `#[cfg(desktop)]` 门控），安卓上没有对等物 —— 分区存储不给"按路径列共享存储"，非媒体
/// 文件连读都读不到，只能走 SAF 的 URI 读授权。而发文件（推送）与发图片（会话消息）只认本机
/// **真实路径**，所以 URI 必须在原生这一侧换成文件，界面拿到的才是一条能直接进
/// `push_paths` / `chat_send_image` 的路径（与粘贴图片那条路同一个形状，见 `resolve_image_uri`）。
///
/// **取消不是错误**：用户在系统选择器里按返回键，Kotlin 回一份空表 —— 这里原样返回空 `Vec`，
/// 界面据此什么都不做（写成一个错误的话，用户每次反悔都会看到一条"失败"提示）。
#[cfg(mobile)]
pub async fn pick_files(kind: String) -> Result<Vec<PickedFile>, String> {
    let Some(handle) = PICK_HANDLE.get() else {
        return Err("选文件插件没注册上（内部错误）".into());
    };
    let value = handle
        .run_mobile_plugin_async::<serde_json::Value>("pick", serde_json::json!({ "kind": kind }))
        .await
        .map_err(|e| e.to_string())?;
    let Some(items) = value["files"].as_array() else {
        return Ok(Vec::new());
    };
    Ok(items
        .iter()
        .filter_map(|it| {
            let path = it["path"].as_str()?.to_string();
            Some(PickedFile {
                name: it["name"].as_str().unwrap_or_default().to_string(),
                size: it["size"].as_u64().unwrap_or(0),
                mime: it["mime"].as_str().unwrap_or_default().to_string(),
                path,
            })
        })
        .collect())
}

// ── 「发送应用」：列出设备上已安装的应用（`list_installed_apps`）──────────────────

/// 应用列表插件的句柄（与文件打开、粘贴图片同款：插件 `setup` 那一刻注册，用的时候取）。
#[cfg(mobile)]
static APPS_HANDLE: OnceLock<PluginHandle<tauri::Wry>> = OnceLock::new();

/// 「已安装应用」插件（只在安卓构建里挂，见 `lib.rs`）。
#[cfg(mobile)]
pub fn apps_plugin() -> TauriPlugin<tauri::Wry> {
    tauri::plugin::Builder::new("orbdrop-installed-apps")
        .setup(|_app, api| {
            // 类名与 Kotlin 那边的 `@TauriPlugin class InstalledAppsPlugin` 对应。
            let handle = api.register_android_plugin("com.x177plus.orbdrop", "InstalledAppsPlugin")?;
            if APPS_HANDLE.set(handle).is_err() {
                warn!("应用列表插件重复注册：沿用第一份句柄");
            }
            Ok(())
        })
        .build()
}

/// 设备上一个已安装的应用（会话里「＋」→「应用」那张表的一行）。
///
/// `path` 是**它自己的 `base.apk`**（`/data/app/…/base.apk`，不需要 root 就读得到，见
/// `InstalledAppsPlugin.kt`）。我们**不拷**它：推送那条路只认"本机路径"，而清单里那份
/// "对端看到的名字"是另一件事 —— 发送时把 `file_name` 当覆盖名交给 `push_paths`
/// （见 `transfer::manifest_for_named_paths`），源路径一个字节都不动。
#[cfg(mobile)]
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct InstalledApp {
    /// 应用自己声明的显示名（可以带 `:`、可以很长、可以是别的语言）。
    pub label: String,
    pub package_name: String,
    /// 可能为空：有的应用就是不写版本名。
    pub version_name: String,
    pub version_code: i64,
    /// `base.apk` 的绝对路径。
    pub path: String,
    pub size: u64,
    /// 落盘名（后端拼好并**净化过**）：`应用名_versionName(versionCode)_包名.apk`。
    ///
    /// 在**这里**拼而不是在 Kotlin 里拼：净化判据（哪些字符会让对端整次接收判失败）住在这边
    /// （`transfer::resume::sanitize_file_name`），两处各写一套迟早分叉。界面拿到它只负责显示
    /// 与原样交回，不再自己拼一次。
    pub file_name: String,
    /// 拆包安装（AAB）：单发 `base.apk` 对端多半装不上（`INSTALL_FAILED_MISSING_SPLIT`），
    /// 界面据此在那一行标出来 —— 不标就是"发过去对方点开报错，用户以为功能坏了"。
    pub has_splits: bool,
}

/// 安卓：列出这台设备上**有桌面图标**的应用（会话里「＋」→「应用」）。
///
/// 原生命令那一趟是只读的（`queryIntentActivities` + 每包一次 `getPackageInfo` + 一次 `stat`，
/// 见 `InstalledAppsPlugin.kt`）；这一层只做两件事：**拼落盘名**（净化在这里，与接收侧的闸门
/// 同一份判据）与把 Kotlin 的 `JSObject` 翻成 Rust 结构。
#[cfg(mobile)]
pub async fn list_installed_apps() -> Result<Vec<InstalledApp>, String> {
    let Some(handle) = APPS_HANDLE.get() else {
        return Err("应用列表插件没注册上（内部错误）".into());
    };
    let value = handle
        .run_mobile_plugin_async::<serde_json::Value>("list", serde_json::json!({}))
        .await
        .map_err(|e| e.to_string())?;
    let Some(items) = value["apps"].as_array() else {
        return Ok(Vec::new());
    };
    Ok(items
        .iter()
        .filter_map(|it| {
            let path = it["path"].as_str()?.to_string();
            let label = it["label"].as_str().unwrap_or_default().to_string();
            let version_name = it["versionName"].as_str().unwrap_or_default().to_string();
            let version_code = it["versionCode"].as_i64().unwrap_or(0);
            let package_name = it["packageName"].as_str().unwrap_or_default().to_string();
            Some(InstalledApp {
                file_name: crate::transfer::resume::apk_file_name(
                    &label,
                    &version_name,
                    version_code,
                    &package_name,
                ),
                label,
                package_name,
                version_name,
                version_code,
                size: it["size"].as_u64().unwrap_or(0),
                has_splits: it["hasSplits"].as_bool().unwrap_or(false),
                path,
            })
        })
        .collect())
}

/// 安卓：取一个应用的图标（`data:image/png;base64,…`，F-119）。
///
/// **按行按需取**，不是列表那一趟一起给：一百多个图标一起画、一起塞进一次返回值，会把"打开
/// 这张表"变成一次几百毫秒的等待，而图标是"看得见才需要"的东西（界面上那一行先显示首字母，
/// 图标随后补上，取过的在两端都缓存 —— 见 `InstalledAppsPlugin.kt` 与 `src/mobile/appIcons.ts`）。
///
/// 失败如实返回错误（正在卸载、图标坏了）：界面那一行退回首字母 —— 一张表不该因为某一行没有
/// 图标就空一块。
#[cfg(mobile)]
pub async fn app_icon(package_name: String) -> Result<String, String> {
    let Some(handle) = APPS_HANDLE.get() else {
        return Err("应用列表插件没注册上（内部错误）".into());
    };
    let value = handle
        .run_mobile_plugin_async::<serde_json::Value>(
            "icon",
            serde_json::json!({ "packageName": package_name }),
        )
        .await
        .map_err(|e| e.to_string())?;
    value["icon"]
        .as_str()
        .map(|s| s.to_string())
        .ok_or_else(|| "原生那一跳没给出图标".to_string())
}

// ── 「引擎太旧时，一键去应用商店更新 WebView」────────────────────────────────────

/// 商店打开插件的句柄（与文件打开、粘贴图片同款：插件 `setup` 那一刻注册，用的时候取）。
#[cfg(mobile)]
static STORE_HANDLE: OnceLock<PluginHandle<tauri::Wry>> = OnceLock::new();

/// 「打开应用商店」插件。
///
/// 与前面两个插件不同，它的入口是**导航钩子**而不是一条命令 —— 需要它的那一刻，前端恰好是死的：
/// 引擎低于下限时入口 module 连解析都过不去（产物里带 `??=`，Chrome 85 起），界面里那套 IPC 根本
/// 不存在。页面上唯一还能点的东西是 `static/engine-floor.js`（ES5）画的浮层，它只能发出一次
/// **导航**（`market://details?id=com.google.android.webview`）。wry 的 Android `WebViewClient`
/// 把每一次导航都交给 Rust（`shouldOverride` → 插件的 `on_navigation`），于是这里把那次导航截住，
/// 换成系统 Intent（Kotlin 侧的 `StoreOpenPlugin.openStore`）。
///
/// **拦下而不是放行**：`market://` 这个 scheme WebView 自己加载不了（unknown scheme），放行只会得到一个
/// 空白页加一行错误。`https://play.google.com/...` 一并接住：浮层上的链接是 `market://`，但用户也可能
/// 直接点到 Play 的网页版，而那个地址在 WebView 里只会换掉界面（这个界面没有后退按钮可退）。
#[cfg(mobile)]
pub fn store_plugin() -> TauriPlugin<tauri::Wry> {
    tauri::plugin::Builder::new("orbdrop-store-open")
        .on_navigation(|_webview, url| {
            if !is_store_url(url) {
                return true; // 别的导航照旧，我们只认这一种
            }
            let Some(handle) = STORE_HANDLE.get().cloned() else {
                warn!("商店插件没注册上：导航被拦下了，但没人接手");
                return false;
            };
            let target = url.as_str().to_string();
            // 同步那条 `run_mobile_plugin` 会阻塞调用线程等 Kotlin 回话，而这里正是 WebView 的主线程
            // 回调（Kotlin 那边发 Intent 也要主线程）—— 只能用异步那条，否则当场死锁。
            tauri::async_runtime::spawn(async move {
                if let Err(e) = handle
                    .run_mobile_plugin_async::<serde_json::Value>(
                        "openStore",
                        serde_json::json!({ "url": target }),
                    )
                    .await
                {
                    warn!("打开应用商店失败：{e}");
                }
            });
            false
        })
        .setup(|_app, api| {
            // 类名与 Kotlin 那边的 `@TauriPlugin class StoreOpenPlugin` 对应。
            let handle = api.register_android_plugin("com.x177plus.orbdrop", "StoreOpenPlugin")?;
            if STORE_HANDLE.set(handle).is_err() {
                warn!("商店插件重复注册：沿用第一份句柄");
            }
            Ok(())
        })
        .build()
}

/// 这条导航该不该交给系统去开：应用商店的 scheme，以及 Play 的网页版。
#[cfg(mobile)]
fn is_store_url(url: &tauri::Url) -> bool {
    url.scheme() == "market" || (url.scheme() == "https" && url.host_str() == Some("play.google.com"))
}