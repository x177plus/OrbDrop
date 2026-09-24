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

use tauri::State;
use tracing::info;

use crate::config::PeerDevice;
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
#[tauri::command]
pub fn list_inbox(state: State<'_, Arc<AppState>>) -> Vec<InboxEntryDto> {
    let root = state.config.lock().download_dir.clone();
    let mut out: Vec<InboxEntryDto> = Vec::new();

    let Ok(dir) = std::fs::read_dir(&root) else {
        return out;
    };
    for item in dir.flatten() {
        let path = item.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        if meta.is_dir() {
            // 一级子目录 = 一台发送方（或用户自己放进去的文件夹）。列它里面的直接项。
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
#[tauri::command]
pub fn delete_inbox_file(path: String, state: State<'_, Arc<AppState>>) -> Result<(), String> {
    let root = state
        .config
        .lock()
        .download_dir
        .canonicalize()
        .map_err(|e| format!("下载目录不可用: {e}"))?;
    let target = PathBuf::from(&path)
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

/// 改一台设备在本机显示的名字（F-71）。
///
/// 直接复用地址簿的 upsert（`AppState::add_peer_book`）：它本来就是"只改传进来的非空字段、
/// 落盘、通知界面刷新"这套语义，正是重命名需要的 —— 不必再开一条写入路径。
#[tauri::command]
pub fn rename_peer(peer_id: String, name: String, state: State<'_, Arc<AppState>>) -> Result<(), String> {
    let name = name.trim().to_string();
    if name.is_empty() {
        return Err("设备名不能为空".into());
    }
    state.add_peer_book(PeerDevice {
        id: peer_id,
        name,
        ip: String::new(),
        port: 0,
        ..Default::default()
    });
    Ok(())
}