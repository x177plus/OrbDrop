use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use chrono::{DateTime, Utc};

// ---------------------------------------------------------------------------
// Data models
// ---------------------------------------------------------------------------

/// 下载模式（S-02）。
///
/// 字面量跟**前端同一套**：`types.ts` 的 `'Direct' | 'AskEachTime'` 联合类型、
/// `update_settings` 请求里的同名字段都是 PascalCase。这里再用 `camelCase` 序列化就会
/// 变成 `"direct"`，前端单选框一个都对不上（面板显示成"两个都没选"），保存时又把
/// `"direct"` 发回来 —— 后端只认 `"Direct"`/`"AskEachTime"`，于是**整个设置面板存不下去**。
/// alias 保留旧配置里的 `"direct"` / `"askEachTime"`，已落盘的 config.toml 不会解析失败
/// （解析失败 = 改名成 `.toml.bad` 再写默认值，用户的共享项和配对表就没了）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DownloadMode {
    #[serde(rename = "Direct", alias = "direct")]
    Direct,
    #[serde(rename = "AskEachTime", alias = "askEachTime")]
    AskEachTime,
}

impl Default for DownloadMode {
    fn default() -> Self {
        Self::Direct
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ShareItem {
    pub id: String,
    pub name: String,
    pub path: PathBuf,
    pub created_at: DateTime<Utc>,
    /// 开播开关（F-22）。可见范围不在这里存：广播对象 = **当前**处于长连接的设备，
    /// 由 `Caller::sees_share` 实时判定（F-25）。
    pub broadcast_enabled: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerDevice {
    pub id: String,
    pub name: String,
    pub ip: String,
    pub port: u16,
    pub online: bool,
    pub paired: bool,
    /// 关机时这台设备处于长连接（F-16）：重启后自动把链路拨回来。
    ///
    /// 在**链路建立那一刻**写、用户点「断开」时清零，所以进程被强杀也不丢。
    /// 这是**地址簿**（`Config::peers`）里的状态：`state.peers` 是发现结果的合并视图，
    /// `upsert_peer` / `mark_paired` 都不合并这个字段 —— 别让 mDNS 的一次广播把用户
    /// 的长连接意图抹掉。
    #[serde(default)]
    pub auto_connect: bool,
}

/// BeyondCompare 的"比较位"目标（F-41）：本机共享项，或长连接设备上的共享项。
///
/// 只存定位需要的东西；`label` 是前端给的可读名字——对端共享项的名字本机不知道
/// （只有 id），所以菜单/toast 里要显示的名字只能由界面带下来，缺失时后端再凑一个。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", tag = "kind")]
pub enum BcTarget {
    Share {
        #[serde(rename = "shareId")]
        share_id: String,
        #[serde(default, rename = "path")]
        path: String,
        #[serde(default, rename = "label", skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
    Peer {
        #[serde(rename = "peerId")]
        peer_id: String,
        #[serde(rename = "shareId")]
        share_id: String,
        #[serde(default, rename = "path")]
        path: String,
        #[serde(default, rename = "label", skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
}

/// 文件选择框顶部「常用」的一条（F-50）：用户右键目录攒出来的快捷入口。
///
/// 存**名字 + 绝对路径**：名字是加进来那一刻的目录名，只当标签用（目录此后改名不回头改它）；
/// 路径才是身份 —— 去重、跳转全按它。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PickerFavorite {
    pub name: String,
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AppSettings {
    pub port: u16,
    pub download_dir: PathBuf,
    pub download_mode: DownloadMode,
    pub device_name: String,
    pub auto_start: bool,
    /// BeyondCompare 可执行文件路径；空 = 自动探测（注册表 / 常见安装路径）。
    pub bc_exe_path: String,
}

impl Default for AppSettings {
    fn default() -> Self {
        Self {
            port: 3890,
            download_dir: default_download_dir(),
            download_mode: DownloadMode::Direct,
            device_name: default_device_name(),
            auto_start: true,
            bc_exe_path: String::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Persistent config (TOML)
// ---------------------------------------------------------------------------

/// 悬浮球上一次的落位：折叠态窗口左上角（物理像素，虚拟屏坐标）。
///
/// 只记窗口左上角，**不记贴边方向与球偏移**：吸附是幂等的 —— 把已贴边的矩形再判一次
/// 得到同样的窗口与球偏移，所以复原时（`orbLayout.planDock`）能把贴边形态一起算回来；
/// 尺寸也不记，跟着当前 DPI 走。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrbPos {
    pub x: i32,
    pub y: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Config {
    #[serde(default = "default_port")]
    pub port: u16,
    /// **唯一**的落盘位置：手动「下载到本地」（S-01，受 `download_mode` 影响）和对端
    /// 「推送」接收（F-52）都写这里。推送一律落在 `<发送方名>/` 下，路径由发送方给的
    /// 清单决定（选中项各自的名字，见 `transfer::manifest_for_paths`）—— 没有"推送名
    /// 子目录"那一层。不再单设「收件目录」——两个预设位置对用户是同一种操作。
    #[serde(default = "default_download_dir")]
    pub download_dir: PathBuf,
    /// 下载模式（S-02）：只作用于手动下载；推送一直是弹窗同意后就落盘。
    #[serde(default)]
    pub download_mode: DownloadMode,
    #[serde(default = "default_device_name")]
    pub device_name: String,
    #[serde(default = "default_auto_start")]
    pub auto_start: bool,
    #[serde(default)]
    pub shares: Vec<ShareItem>,
    /// BeyondCompare 可执行文件（留空 = 自动探测）。便携版 BC 不在注册表里，只能靠这项。
    #[serde(default)]
    pub bc_exe_path: Option<PathBuf>,
    /// 「添加到左侧/右侧比较位」的两个槽位（跨重启保留，BC 自己的右键扩展也是这么做的）。
    #[serde(default)]
    pub bc_left: Option<BcTarget>,
    #[serde(default)]
    pub bc_right: Option<BcTarget>,
    /// Stable device identity advertised via mDNS / used as peer id.
    ///
    /// Every other field is optional on load: a config written by an older
    /// build (or hand-edited) must not reset our identity, or peers accumulate a
    /// fresh row for the "same" device on each restart.
    #[serde(default = "new_device_id")]
    pub device_id: String,
    /// Known devices: manual adds + paired peers (address book, persisted).
    #[serde(default)]
    pub peers: Vec<PeerDevice>,
    /// 悬浮球落位（F-74「窗口位置」）：`None` = 还没记过（首启 / 旧配置），启动时按居中显示。
    #[serde(default)]
    pub orb_pos: Option<OrbPos>,
    /// 文件选择框的「常用」列表（F-50）：**默认空**，全靠用户右键目录攒。
    ///
    /// 放配置文件里而不是 webview 的 localStorage：它跟共享项一样是**用户数据** ——
    /// 该和其余状态躺在同一个数据目录（备份/迁移/清理都是一个动作），也不该因为
    /// 清一次站点数据就凭空消失。
    #[serde(default)]
    pub picker_favorites: Vec<PickerFavorite>,
}

fn default_port() -> u16 {
    3890
}

fn default_auto_start() -> bool {
    true
}

fn new_device_id() -> String {
    Uuid::new_v4().to_string()
}

impl Config {
    pub fn settings(&self) -> AppSettings {
        AppSettings {
            port: self.port,
            download_dir: self.download_dir.clone(),
            download_mode: self.download_mode,
            device_name: self.device_name.clone(),
            auto_start: self.auto_start,
            bc_exe_path: self
                .bc_exe_path
                .as_ref()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default(),
        }
    }

    /// 写入选择框「常用」：**整份替换**（界面给的就是它此刻显示的那份）。
    ///
    /// 顺手规整：丢掉空条目、按 `path` 去重（同一条加两次只留一条；不同路径同名是两条）。
    /// 返回**真正存下去的那份**，让界面以后端为准 —— 否则界面上的列表和配置里的列表会慢慢
    /// 分叉（比如重复项在界面上看得见、重启后却少一条）。
    pub fn set_picker_favorites(&mut self, items: Vec<PickerFavorite>) -> Vec<PickerFavorite> {
        let mut kept: Vec<PickerFavorite> = Vec::new();
        for item in items {
            let name = item.name.trim();
            let path = item.path.trim();
            if path.is_empty() {
                continue;
            }
            if kept.iter().any(|k| k.path == path) {
                continue;
            }
            kept.push(PickerFavorite {
                name: if name.is_empty() { path.to_string() } else { name.to_string() },
                path: path.to_string(),
            });
        }
        self.picker_favorites = kept.clone();
        kept
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            port: 3890,
            download_dir: default_download_dir(),
            download_mode: DownloadMode::Direct,
            device_name: default_device_name(),
            auto_start: true,
            shares: Vec::new(),
            bc_exe_path: None,
            bc_left: None,
            bc_right: None,
            device_id: new_device_id(),
            peers: Vec::new(),
            orb_pos: None,
            picker_favorites: Vec::new(),
        }
    }
}

// ---------------------------------------------------------------------------
// Path helpers
// ---------------------------------------------------------------------------

/// OrbDrop 数据根目录：Windows `%LOCALAPPDATA%\OrbDrop`，macOS `~/Library/Application Support/OrbDrop`。
///
/// 一律落在**本机专属**目录（`dirs::data_local_dir`）：设置、日志、传输断点都不参与域
/// 漫游用户配置文件的登录/注销同步，也不进文件夹重定向。落在 Roaming 会让"几百 MB 日志
/// 拖慢登录"和"设置被多台机器互相覆盖"同时发生；而且设备 id 本来就该是每台机器独立的。
///
/// 拿不到 known folder 时退到 `%LOCALAPPDATA%`、再退到临时目录：能跑，只是不持久
/// （同 `log.rs::crash_log_path` 的兜底策略）。
pub fn data_dir() -> PathBuf {
    // 整包搬走数据目录（便携部署 / 测试里指向临时目录，别碰真实数据）。
    if let Some(dir) = std::env::var_os("ORBDROP_DATA_DIR") {
        return PathBuf::from(dir);
    }
    dirs::data_local_dir()
        .or_else(|| std::env::var_os("LOCALAPPDATA").map(PathBuf::from))
        .unwrap_or_else(std::env::temp_dir)
        .join("OrbDrop")
}

/// 原子写：先写同目录的临时文件，再 `rename` 覆盖目标。
///
/// 写到一半被打断（崩溃、断电、被杀）**不会**留下半截目标文件 —— 凭据文件和 BC 的
/// profile 文件都靠它：半截凭据文件 = 读不出来 = 用户眼里"配对全没了"。
pub fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let name = path
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_else(|| "orbdrop".to_string());
    let tmp = path.with_file_name(format!("{name}.orbdrop-tmp"));
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)
}

pub fn config_file_path() -> PathBuf {
    data_dir().join("config.toml")
}

fn default_download_dir() -> PathBuf {
    dirs::desktop_dir().unwrap_or_else(|| PathBuf::from(r"C:\Users\Default\Desktop"))
}

fn default_device_name() -> String {
    #[cfg(target_os = "windows")]
    {
        std::env::var("COMPUTERNAME").unwrap_or_else(|_| "OrbDrop-PC".into())
    }
    #[cfg(not(target_os = "windows"))]
    {
        std::env::var("HOSTNAME")
            .or_else(|_| std::env::var("HOST"))
            .unwrap_or_else(|_| "OrbDrop-Mac".into())
    }
}

// ---------------------------------------------------------------------------
// Config persistence
// ---------------------------------------------------------------------------

pub fn load_config() -> Config {
    let path = config_file_path();
    if !path.exists() {
        // First run: keep the generated identity. Peers key their address book
        // and pairing codes off our device id, so it must not be re-minted on
        // every start (that showed up as a duplicate device row per restart).
        let config = Config::default();
        if let Err(e) = save_config_at(&path, &config) {
            eprintln!("Failed to persist initial config: {e}");
        }
        return config;
    }
    match std::fs::read_to_string(&path) {
        Ok(content) => toml::from_str(&content).unwrap_or_else(|e| {
            eprintln!("Failed to parse config, using defaults: {e}");
            // Keep our identity across a corrupt file instead of minting a new
            // device id on each launch — but never silently drop the old file.
            let backup = path.with_extension("toml.bad");
            if let Err(e) = std::fs::rename(&path, &backup) {
                eprintln!("Failed to back up config to {backup:?}: {e}");
            }
            let config = Config::default();
            if let Err(e) = save_config_at(&path, &config) {
                eprintln!("Failed to rewrite config: {e}");
            }
            config
        }),
        Err(e) => {
            eprintln!("Failed to read config, using defaults: {e}");
            Config::default()
        }
    }
}

/// Write a config to an arbitrary path. Used by production code and tests.
pub fn save_config_at(path: &Path, config: &Config) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).context("Failed to create config directory")?;
    }
    let content = toml::to_string_pretty(config).context("Failed to serialize config")?;
    std::fs::write(path, content).context("Failed to write config file")?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Unique file naming (S-01)
// ---------------------------------------------------------------------------

pub fn unique_path(dir: &PathBuf, name: &str) -> PathBuf {
    let target = dir.join(name);
    if !target.exists() {
        return target;
    }
    let stem = match target.extension() {
        Some(ext) => {
            let s = name.strip_suffix(&format!(".{}", ext.to_string_lossy())).unwrap_or(name);
            s.to_string()
        }
        None => name.to_string(),
    };
    let ext = target.extension()
        .map(|e| format!(".{}", e.to_string_lossy()))
        .unwrap_or_default();

    for i in 1..9999 {
        let candidate = dir.join(format!("{} ({}){}", stem, i, ext));
        if !candidate.exists() {
            return candidate;
        }
    }
    dir.join(format!("{} ({}){}", stem, 9999, ext))
}

// ---------------------------------------------------------------------------
// Helpers for Tauri-side share management
// ---------------------------------------------------------------------------

impl ShareItem {
    pub fn new(path: PathBuf) -> Self {
        let name = path
            .file_name()
            .map(|s| s.to_string_lossy().to_string())
            .unwrap_or_else(|| path.to_string_lossy().to_string());
        let id = Uuid::new_v4().to_string();
        Self {
            id,
            name,
            path,
            created_at: Utc::now(),
            broadcast_enabled: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Round-trip: save a config with a share, load it back, and check equality.
    /// This is the persistence contract behind "重启后共享项/设置恢复".
    #[test]
    fn config_round_trip_preserves_shares_and_settings() {
        let dir = std::env::temp_dir().join(format!("orbdrop_cfg_test_{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();

        let config = Config {
            port: 3895,
            download_dir: PathBuf::from("C:/test/downloads"),
            download_mode: DownloadMode::AskEachTime,
            device_name: "TestPC".into(),
            auto_start: false,
            device_id: "device-test-1".into(),
            peers: vec![],
            bc_exe_path: Some(PathBuf::from("D:/Tools/BCompare/BCompare.exe")),
            bc_left: Some(BcTarget::Share {
                share_id: "share-1".into(),
                path: "src".into(),
                label: None,
            }),
            bc_right: None,
            orb_pos: Some(OrbPos { x: -1280, y: 40 }),
            picker_favorites: vec![
                PickerFavorite { name: "桌面".into(), path: "C:/Users/t/Desktop".into() },
                PickerFavorite { name: "项目".into(), path: "D:/项目".into() },
            ],
            shares: vec![ShareItem {
                id: "share-1".into(),
                name: "Projects".into(),
                path: PathBuf::from("C:/Projects"),
                created_at: Utc::now(),
                broadcast_enabled: true,
            }],
        };

        // Isolate config file writes to the temp dir: write the TOML exactly as
        // save_config would, then parse it back exactly as load_config does.
        let tmp_file = dir.join("config.toml");

        std::fs::write(&tmp_file, toml::to_string_pretty(&config).unwrap()).unwrap();
        let loaded: Config = toml::from_str(&std::fs::read_to_string(&tmp_file).unwrap().as_str()).unwrap();

        assert_eq!(loaded.port, 3895);
        assert_eq!(loaded.download_dir, PathBuf::from("C:/test/downloads"));
        assert_eq!(loaded.download_mode, DownloadMode::AskEachTime);
        assert_eq!(loaded.device_name, "TestPC");
        assert_eq!(loaded.auto_start, false);
        assert_eq!(loaded.shares.len(), 1);
        assert_eq!(loaded.shares[0].id, "share-1");
        assert_eq!(loaded.shares[0].name, "Projects");
        assert_eq!(loaded.shares[0].path, PathBuf::from("C:/Projects"));
        assert_eq!(loaded.shares[0].broadcast_enabled, true);
        assert_eq!(
            loaded.picker_favorites,
            vec![
                PickerFavorite { name: "桌面".into(), path: "C:/Users/t/Desktop".into() },
                PickerFavorite { name: "项目".into(), path: "D:/项目".into() },
            ],
            "选择框「常用」要跨重启保留（用户攒的那几条）"
        );
        assert_eq!(
            loaded.bc_exe_path,
            Some(PathBuf::from("D:/Tools/BCompare/BCompare.exe")),
            "BC 路径要跨重启保留（便携版 BC 只能靠它）"
        );
        assert_eq!(
            loaded.bc_left,
            Some(BcTarget::Share {
                share_id: "share-1".into(),
                path: "src".into(),
                label: None
            }),
            "比较位要跨重启保留"
        );
        assert_eq!(loaded.bc_right, None);
        assert_eq!(
            loaded.orb_pos,
            Some(OrbPos { x: -1280, y: 40 }),
            "悬浮球位置要跨重启保留（F-74）：负坐标是左边那块显示器"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 前后端字面量契约：`AppSettings.downloadMode` 与 `UpdateSettingsRequest.downloadMode`
    /// 都是 `Direct` / `AskEachTime`（前端 types.ts 的联合类型）。
    ///
    /// 这条曾经挂过：enum 上挂 `rename_all = "camelCase"` → 序列化成 `"direct"`，
    /// 前端的单选框一个都对不上（面板显示"没选"），保存时又把 `"direct"` 发回来被后端拒掉。
    /// 所以这里钉死线上字面量，同时确认旧配置（`"direct"` / `"askEachTime"`）照样读得回来。
    #[test]
    fn download_mode_wire_spelling_matches_frontend() {
        assert_eq!(
            serde_json::to_string(&DownloadMode::Direct).unwrap(),
            "\"Direct\""
        );
        assert_eq!(
            serde_json::to_string(&DownloadMode::AskEachTime).unwrap(),
            "\"AskEachTime\""
        );

        let legacy: Config = toml::from_str("downloadMode = \"direct\"\n").unwrap();
        assert_eq!(legacy.download_mode, DownloadMode::Direct);
        let legacy: Config = toml::from_str("downloadMode = \"askEachTime\"\n").unwrap();
        assert_eq!(legacy.download_mode, DownloadMode::AskEachTime);

        let current: Config = toml::from_str("downloadMode = \"AskEachTime\"\n").unwrap();
        assert_eq!(current.download_mode, DownloadMode::AskEachTime);
    }

    /// 旧配置里的 `inboxDir`（收件目录已并入 `downloadDir`）不能把配置文件判成损坏：
    /// 未知字段照旧忽略，共享项和配对表保住（解析失败会把文件改名成 `.toml.bad` 再写默认值）。
    #[test]
    fn config_with_legacy_inbox_dir_keeps_loading() {
        let config: Config = toml::from_str(
            "downloadDir = 'C:/dl'\ninboxDir = 'C:/old-inbox'\ndeviceId = \"device-1\"\n",
        )
        .unwrap();
        assert_eq!(config.download_dir, PathBuf::from("C:/dl"));
        assert_eq!(config.device_id, "device-1");
    }

    /// 旧配置文件里没有 `orbPos`：照样解析成 `None`，不能判成损坏。
    ///
    /// （`load_config` 遇到损坏文件会把它改名成 `.toml.bad` 再写回默认值 —— 加一个字段
    /// 就让用户的共享项/配对表消失，代价太大。）
    #[test]
    fn config_without_orb_pos_keeps_loading() {
        let config: Config = toml::from_str("port = 3895\ndeviceId = \"device-1\"\n").unwrap();
        assert_eq!(config.orb_pos, None);
        assert_eq!(config.port, 3895);
        assert_eq!(config.device_id, "device-1");
    }

    /// 选择框「常用」默认是空的：老配置（没有这一节）解析出来就是空表，界面不会凭空冒出
    /// 几个"默认常用目录"来。
    #[test]
    fn picker_favorites_default_to_empty() {
        let config: Config = toml::from_str("deviceId = \"device-1\"\n").unwrap();
        assert!(config.picker_favorites.is_empty());
    }

    /// 写回「常用」时规整：空路径丢掉、同路径只留一条、空名字用路径顶上。
    #[test]
    fn set_picker_favorites_dedupes_and_drops_blanks() {
        let mut config = Config::default();
        let stored = config.set_picker_favorites(vec![
            PickerFavorite { name: "桌面".into(), path: "C:/Users/t/Desktop".into() },
            PickerFavorite { name: "桌面（重复）".into(), path: "C:/Users/t/Desktop".into() },
            PickerFavorite { name: "空的".into(), path: "   ".into() },
            PickerFavorite { name: "  ".into(), path: "D:/项目".into() },
        ]);

        assert_eq!(
            stored,
            vec![
                PickerFavorite { name: "桌面".into(), path: "C:/Users/t/Desktop".into() },
                PickerFavorite { name: "D:/项目".into(), path: "D:/项目".into() },
            ]
        );
        // 存下去的就是返回的那份：界面以后端为准，两边不会分叉
        assert_eq!(config.picker_favorites, stored);
    }

    /// S-01: same-name downloads get "xxx (1).ext" style renames.
    #[test]
    fn unique_path_appends_counter() {
        let dir = std::env::temp_dir().join(format!("orbdrop_uniq_test_{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();

        // Nothing exists → base name.
        let p1 = unique_path(&dir, "report.txt");
        assert_eq!(p1.file_name().unwrap(), "report.txt");

        std::fs::write(&p1, "x").unwrap();

        // Exists → "report (1).txt"
        let p2 = unique_path(&dir, "report.txt");
        assert_eq!(p2.file_name().unwrap(), "report (1).txt");
        std::fs::write(&p2, "x").unwrap();

        // Both exist → "report (2).txt"
        let p3 = unique_path(&dir, "report.txt");
        assert_eq!(p3.file_name().unwrap(), "report (2).txt");

        // No-extension files still get suffixes.
        let p4 = unique_path(&dir, "README");
        assert_eq!(p4.file_name().unwrap(), "README");
        std::fs::write(&p4, "x").unwrap();
        let p5 = unique_path(&dir, "README");
        assert_eq!(p5.file_name().unwrap(), "README (1)");

        std::fs::remove_dir_all(&dir).unwrap();
    }
}