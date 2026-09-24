use std::collections::HashMap;
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

/// 折叠态悬浮形态：圆球（现在的 62px 球）或圆角矩形悬浮框（设备分页卡片）。
///
/// 字面量跟**前端同一套**：`types.ts` 的 `'circle' | 'rect'` 联合类型、
/// `UpdateSettingsRequest` 的同名字段都是小写。`rename_all = "camelCase"` 落在枚举上
/// 改的是**变体名**（`Circle` → `"circle"`、`Rect` → `"rect"`），正好对上。
///
/// `serde(default)` 对**旧配置**是必须的：没有这一项时解析失败会让 `load_config` 把
/// 配置文件改名成 `.toml.bad` 再写默认值 —— 用户升级一次就丢掉全部共享项与配对表。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum OrbShape {
    Circle,
    Rect,
}

impl Default for OrbShape {
    fn default() -> Self {
        Self::Circle
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

/// 注：`u16`/`String`/`bool` 各有 Default，`PeerDevice` 因此可以 `..Default::default()`
/// 补齐 —— 只在"这一行只是观察值、意图字段不由构造方决定"的地方用（如对端换 id 时
/// 迁行的输入，见 `AppState::rekey_peer`）。
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
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
    /// 「断开」的另一半（F-16）：**拒绝对端的自动重拨**（`server::events_ws_handler`
    /// 对不带 `manual=1` 的升级回 403）。
    ///
    /// 必须落盘，否则「断开」只在**本进程**里算数：本机是安静了，可对端还记着这次长
    /// 连接（`auto_connect` 在它那侧照旧），它一看见本机回到网上就拨过来 —— 而本机的
    /// 暂停标记已随进程消失，于是照收那条链接，双方界面回到「长连接」，用户按的那一下
    /// 等于没按。更糟的是被拨号的一端还会顺势把 `auto_connect` 写回来，从此每次重启都
    /// 自己连（用户这边看到的就是"主动断开、退出重开又连上了"）。
    ///
    /// 与 `auto_connect` 是同一个意图的两面（本机拨不拨 / 接不接对端的拨），
    /// 唯一写点是 `AppState::set_link_intent`：分开写早晚会出现"不拨但照收"。
    #[serde(default)]
    pub paused: bool,
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
    /// 本机「在线 / 离线」状态（面板右上角那个状态框）。见 `Config::online`。
    pub online: bool,
    /// 折叠态悬浮形态（设置里的「悬浮形态」）。见 `OrbShape`。
    pub orb_shape: OrbShape,
    /// 新添加的目录/文件是否默认开播（设置里的「添加后自动共享」，默认关）。见 `Config::auto_broadcast_new`。
    pub auto_broadcast_new: bool,
    /// 是否允许本机为群聊**中转**消息（跨网段那一档，默认关）。见 `Config::group_relay`。
    pub group_relay: bool,
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
            online: true,
            orb_shape: OrbShape::default(),
            auto_broadcast_new: false,
            group_relay: false,
        }
    }
}

// ---------------------------------------------------------------------------
// Persistent config (TOML)
// ---------------------------------------------------------------------------

/// 悬浮球上一次的落位：悬浮体**本体中心**（球心 / 卡片白底中心；物理像素，虚拟屏坐标）。
///
/// 只记这一个点，**不记贴边方向与球偏移**：吸附是幂等的 —— 把已贴边的矩形再判一次
/// 得到同样的窗口与球偏移，所以复原时（`orbLayout.planDock`）能把贴边形态一起算回来；
/// 尺寸与本体在窗口里的内缩也不记，跟随当前形态与 DPI（窗口左上角 = 中心 − 内缩）。
/// 内缩见 `lib.rs` 的 `collapsed_center_inset`（与前端 `orbShape.ts` 的 `body` 逐值一致）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OrbPos {
    pub x: i32,
    pub y: i32,
}

/// 悬浮体**本体中心**在折叠态窗口内的偏移（**逻辑像素**）：窗口左上角 = 中心 − 这个值。
///
/// 与前端 `src/lib/orbShape.ts` 的 `CollapsedLayout.body` **逐值一致**（改一处要改两处）：
///   圆球：本体 = 62 的球，居中于 90×90 窗口 → (90−62)/2 + 31 = 45、45
///   圆角矩形：本体 = 卡片那张白底 176×134，落在窗口的 (14, 10) → 14 + 88 = 102、10 + 67 = 77
///             （**不含**右侧「间距 10 + 点列 20」那一条）
pub fn collapsed_center_inset(shape: OrbShape) -> (f64, f64) {
    match shape {
        OrbShape::Rect => (102.0, 77.0),
        OrbShape::Circle => (45.0, 45.0),
    }
}

/// 老配置的 `orbPos`（折叠态窗口左上角）→ 本体中心（`orbCenter`）。
///
/// 换算只用到**当前形态**的本体内缩乘当前 DPI —— 老值是拿"那种形态的窗口左上角"记的，
/// 而形态本来就在同一份配置里，所以这一步是确定的、不丢位置。调用点只有启动落位那一处
/// （那里才拿得到 DPI，见 `lib.rs` 的 `place_orb_on_startup`）。
pub fn orb_center_from_legacy(pos: OrbPos, shape: OrbShape, scale_factor: f64) -> OrbPos {
    let (ix, iy) = collapsed_center_inset(shape);
    OrbPos {
        x: pos.x + (ix * scale_factor).round() as i32,
        y: pos.y + (iy * scale_factor).round() as i32,
    }
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
    /// 悬浮体本体中心（F-74「窗口位置」）：`None` = 还没记过（首启 / 旧配置），启动时按默认落位。
    #[serde(default)]
    pub orb_center: Option<OrbPos>,
    /// **已废弃**，只为读老配置：早先存的是"折叠态窗口左上角"。读取时由 `orb_center_from_legacy`
    /// 换算成本体中心并落盘；不再写出（`skip_serializing`）。
    #[serde(default, skip_serializing)]
    pub orb_pos: Option<OrbPos>,
    /// 折叠态悬浮形态（设置里的「悬浮形态」）：圆球 / 圆角矩形悬浮框。见 `OrbShape`。
    #[serde(default)]
    pub orb_shape: OrbShape,
    /// 文件选择框的「常用」列表（F-50）：**默认空**，全靠用户右键目录攒。
    ///
    /// 放配置文件里而不是 webview 的 localStorage：它跟共享项一样是**用户数据** ——
    /// 该和其余状态躺在同一个数据目录（备份/迁移/清理都是一个动作），也不该因为
    /// 清一次站点数据就凭空消失。
    #[serde(default)]
    pub picker_favorites: Vec<PickerFavorite>,
    /// 本机「在线 / 离线」（面板右上角状态框）：**离线 = 隐身**。
    ///
    /// 离线时本机不广播 mDNS、断开全部长连接、拒收一切入站请求（含 WS 升级与探活
    /// `/pair/info`）—— 对端因此把本机看成"不在网上"。配对关系、token、共享项全都不动：
    /// 切回「在线」就重新广播，并按地址簿里的长连接记忆把链路接回来（F-16）。
    ///
    /// 这是**用户意图**，所以落在配置文件里跨重启生效（和「断开」的记忆同一个道理）。
    #[serde(default = "default_online")]
    pub online: bool,
    /// 新添加的目录/文件**默认是否开播**（设置里的「添加后自动共享」，**默认关**）。
    ///
    /// 只作用于**之后新登记**的共享项：`lib.rs::add_share_impl` 建条目时把这个值抄进
    /// `ShareItem::broadcast_enabled`，已在列表里的项一个都不动 —— 用户手动关掉的开关
    /// 不会因为这里开着而被翻回来（早先登记的项永不受影响）。
    ///
    /// 落在配置文件里跨重启有效：它是**用户意图**（"以后加的东西要不要默认给长连接设备看"），
    /// 和「在线 / 离线」「开机自启」同一个道理。
    #[serde(default)]
    pub auto_broadcast_new: bool,
    /// 是否允许本机**为群聊中转消息**（设置里的「允许为群聊中转消息」，默认关）。
    ///
    /// 只在**群消息**上生效，而且是"本机也在那个群里"时才转（见 `group::relay_to`）：
    /// 跨网段时两端之间没有路径，缺的那一段只能由链路上的成员帮着搬。代价是本机替别人
    /// 占用上行带宽，且这条与本项目"只走单播、不做多跳"的取舍相冲 —— 所以它必须由用户
    /// 显式打开，默认关（需求文档里那两条非目标也随之改写）。
    #[serde(default)]
    pub group_relay: bool,
    /// 设备备注（F-101）：`设备 id → 备注`，**只存在本机**。
    ///
    /// 键是**对端自己的设备 id**（`PeerDevice::id`，与配对凭据、共享项广播寻址同一套），
    /// 不是 IP —— 换网段、DHCP 换地址之后备注要跟着那台设备，不能跟着地址走。
    ///
    /// 单独一张表、而不是塞进 `peers` 条目：备注是**本机的看法**，不是那台设备的事实 ——
    /// 解除配对（条目重建成"未绑定"）、重新配对（换 token）都不该把它抹掉；也不会随设备名
    /// 广播出去（对端看到的始终是它自己的名字，见 `lib.rs::list_devices`）。
    #[serde(default)]
    pub device_remarks: HashMap<String, String>,
}

fn default_port() -> u16 {
    3890
}

fn default_online() -> bool {
    true
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
            online: self.online,
            orb_shape: self.orb_shape,
            auto_broadcast_new: self.auto_broadcast_new,
            group_relay: self.group_relay,
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

    /// 写一台设备的备注（F-101）：**空串 / 纯空白 = 清除这一条**（不留空字符串，免得配置里
    /// 堆一地 `""`）；返回真正存下去的值（`None` = 已清除），让界面以后端为准。
    ///
    /// 超过 `REMARK_MAX_CHARS` 个字就截断：这是给人看的短标签（它会被拼进设备名里显示），
    /// 不是自由文本 —— 一屏 20 台设备的名字里塞进一句 200 字的话，界面先垮。
    pub fn set_device_remark(&mut self, peer_id: &str, remark: &str) -> Option<String> {
        let remark = remark.trim();
        if remark.is_empty() {
            self.device_remarks.remove(peer_id);
            return None;
        }
        let remark: String = remark.chars().take(REMARK_MAX_CHARS).collect();
        self.device_remarks.insert(peer_id.to_string(), remark.clone());
        Some(remark)
    }
}

/// 备注最长多少个字（按**字符**数，不是字节：中文一个字算一个）。
pub const REMARK_MAX_CHARS: usize = 24;

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
            orb_center: None,
            orb_pos: None,
            orb_shape: OrbShape::default(),
            picker_favorites: Vec::new(),
            online: true,
            auto_broadcast_new: false,
            group_relay: false,
            device_remarks: HashMap::new(),
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

pub(crate) fn default_download_dir() -> PathBuf {
    // Android 上没有"桌面"这个概念：`dirs::desktop_dir()` 返回 None，而下面 Windows 那句
    // 兜底会写出一个真机上根本不存在的路径 —— 接收推送时就会在那儿建目录并当场失败。
    // 移动端一律落在应用私有的收件目录里；用户可见的外部目录（SAF）是 F-86 的后续需求。
    #[cfg(mobile)]
    {
        return data_dir().join("received");
    }
    #[cfg(not(mobile))]
    dirs::desktop_dir().unwrap_or_else(|| PathBuf::from(r"C:\Users\Default\Desktop"))
}

pub(crate) fn default_device_name() -> String {
    #[cfg(target_os = "android")]
    {
        // 手机上既没有 `COMPUTERNAME` 也没有 `HOSTNAME`：早先那条非 Windows 兜底会让
        // **每台安卓设备都叫 "OrbDrop-Mac"**（同一个局域网里两台手机还会重名）。默认名
        // 从系统属性里挑一个"给人看的名字"（见 `pick_android_device_name`），一条都取不到
        // 才退平台兜底。
        return pick_android_device_name(android_property)
            .unwrap_or_else(|| "OrbDrop-Android".into());
    }
    #[cfg(target_os = "windows")]
    {
        std::env::var("COMPUTERNAME").unwrap_or_else(|_| "OrbDrop-PC".into())
    }
    #[cfg(not(any(target_os = "android", target_os = "windows")))]
    {
        std::env::var("HOSTNAME")
            .or_else(|_| std::env::var("HOST"))
            .unwrap_or_else(|_| "OrbDrop-Mac".into())
    }
}

/// 厂商放出来的"市场名"属性，按可信度从高到低排。
///
/// 为什么非要这些键：`ro.product.model` 在国行 ROM 上多是**型号代码** —— 小米 `24129PN74C`、
/// vivo `V2502A`、三星 `SM-S928B`，一屏设备名里谁也认不出这是哪台机器；用户要的是
/// `Xiaomi 15 Pro` / `vivo X300` 这种一眼认得出的名字，而它只在厂商自己放的那条属性里：
///
/// - 小米 / 红米（HyperOS、MIUI）：`ro.product.marketname`，分区的变体（`odm` / `vendor` /
///   `system` / `product` / `bootimage`）各有机器在用；
/// - OPPO / OnePlus / realme（ColorOS）：`ro.vendor.oplus.market.name`；
/// - vivo / iQOO：`ro.vivo.market.name`。
///
/// 这些键**厂商私有、没有文档保证**，某个 ROM 上不存在很正常 —— 属性不存在时
/// `__system_property_get` 返回 0（一次进程内的属性表查表，没有代价），所以整串探一遍划算。
/// 三星、华为这类不放市场名的机器就落回"品牌 + 型号"（见 `pick_android_device_name`）。
const ANDROID_MARKET_NAME_KEYS: [&str; 8] = [
    "ro.product.marketname",
    "ro.product.odm.marketname",
    "ro.product.vendor.marketname",
    "ro.product.system.marketname",
    "ro.product.product.marketname",
    "ro.product.bootimage.marketname",
    "ro.vendor.oplus.market.name",
    "ro.vivo.market.name",
];

/// 从一串系统属性里挑一个设备名；`None` = 一条可用属性都没有（调用方去用平台兜底）。
///
/// 顺序：**厂商市场名 → 品牌 + 型号 → 型号 → 品牌 → 厂商**。第二档是给"只有型号代码"的机器
/// 兜的：`V2502A` 认不出是谁，`vivo V2502A` 至少认得出牌子。型号本身已经像个人话
/// （`Pixel 7`、`Xiaomi 15`）或已经以品牌开头时不再叠品牌 —— 否则会长成 `Xiaomi Xiaomi 15`。
///
/// **刻意不 `#[cfg(target_os = "android")]`**：它只跟"属性表"打交道（拿的是闭包），
/// 桌面测试塞一份假表就能把挑选规则钉死 —— 真机上的属性值在 CI 里造不出来。
#[cfg_attr(not(target_os = "android"), allow(dead_code))]
fn pick_android_device_name(get: impl Fn(&str) -> Option<String>) -> Option<String> {
    if let Some(market) = ANDROID_MARKET_NAME_KEYS.iter().find_map(|key| usable_property(get(key))) {
        return Some(market);
    }
    let brand = usable_property(get("ro.product.brand"));
    if let Some(model) = usable_property(get("ro.product.model")) {
        // 只有"看起来是串代码"的型号才要品牌陪着（没有空格＝不是一句人话）。
        let prefix = brand.filter(|b| !model.contains(' ') && !starts_with_ignore_case(&model, b));
        return Some(match prefix {
            Some(b) => format!("{b} {model}"),
            None => model,
        });
    }
    brand.or_else(|| usable_property(get("ro.product.manufacturer")))
}

/// 属性值算不算一个能用的名字：空串、纯空白、以及 ROM 上常见的占位值 `unknown` 都不算。
fn usable_property(value: Option<String>) -> Option<String> {
    let value = value?.trim().to_string();
    (!value.is_empty() && !value.eq_ignore_ascii_case("unknown")).then_some(value)
}

fn starts_with_ignore_case(text: &str, prefix: &str) -> bool {
    text.to_lowercase().starts_with(&prefix.to_lowercase())
}

/// 配置里存着的这个设备名，该不该被 `fixed`（现在能取到的好名字）换掉。
///
/// 只认**我们自己自动取名时落下的值**：平台兜底串（`OrbDrop-Mac`），或这台机器自己的
/// 型号 / 品牌。判据是**逐字节相等** —— 用户手打的名字恰好等于其中之一的概率极低，而真撞上了
/// 也只是"名字变回这台设备真正的型号"，方向是对的；反之（把用户起的名改掉）才是错的，
/// 所以宁可漏改也不误改。
#[cfg_attr(not(mobile), allow(dead_code))]
pub(crate) fn should_replace_auto_name(current: &str, fixed: &str) -> bool {
    current != fixed && auto_picked_names().iter().any(|n| n == current)
}

/// 早先的自动命名可能落下的值。安卓上还要算上型号与品牌本身（那时默认名就是 `ro.product.model`，
/// 国行机器于是存着 `V2502A` 这种谁也认不出的串）。
#[cfg_attr(not(mobile), allow(dead_code))]
fn auto_picked_names() -> Vec<String> {
    #[allow(unused_mut)]
    let mut names = vec!["OrbDrop-Mac".to_string()];
    #[cfg(target_os = "android")]
    {
        names.extend(
            ["ro.product.model", "ro.product.brand"]
                .iter()
                .filter_map(|k| android_property(k)),
        );
    }
    names
}

/// 读一条 Android 系统属性（`ro.product.model` 这类）。
///
/// 走 **bionic 的 `__system_property_get`**，不走 JNI：属性服务对普通应用可读、不需要
/// 任何权限，也不需要在启动早期把 JavaVM 拿到手（`default_device_name` 是在配置默认值
/// 那一刻调的，那时 JNI 环境未必就绪）。返回值的长度就是写入的字节数，0 = 这条属性
/// 不存在或读不到。
#[cfg(target_os = "android")]
fn android_property(name: &str) -> Option<String> {
    use std::ffi::{CStr, CString};
    use std::os::raw::{c_char, c_int};

    extern "C" {
        fn __system_property_get(name: *const c_char, value: *mut c_char) -> c_int;
    }

    // bionic 的 `PROP_VALUE_MAX` 是 92（值最长 91 字节 + NUL）。缓冲区给宽一点：
    // 这个函数只往里写"值 + NUL"，不比 `PROP_VALUE_MAX` 多写一个字节。
    const BUF: usize = 128;
    let name = CString::new(name).ok()?;
    let mut buf = [0 as c_char; BUF];
    let len = unsafe { __system_property_get(name.as_ptr(), buf.as_mut_ptr()) };
    if len <= 0 {
        return None;
    }
    let value = unsafe { CStr::from_ptr(buf.as_ptr()) }.to_str().ok()?.trim().to_string();
    (!value.is_empty()).then_some(value)
}

// ---------------------------------------------------------------------------
// Config persistence
// ---------------------------------------------------------------------------

pub fn load_config() -> Config {
    load_config_at(&config_file_path())
}

/// 读一份配置（路径可给，便于测试）。文件不存在时落一份默认配置；损坏时改名成 `.toml.bad`
/// 再落一份默认 —— 两条都保住设备身份，也绝不静默丢掉旧文件。
///
/// 读进来后按路径**收一次重复的共享项**（早先版本允许重复登记），收掉了就当场写回：
/// 用户看到的列表与 `config.toml` 里躺着的是一致的，不会"每次启动都清一遍"。
pub fn load_config_at(path: &Path) -> Config {
    if !path.exists() {
        // First run: keep the generated identity. Peers key their address book
        // and pairing codes off our device id, so it must not be re-minted on
        // every start (that showed up as a duplicate device row per restart).
        let config = Config::default();
        if let Err(e) = save_config_at(path, &config) {
            eprintln!("Failed to persist initial config: {e}");
        }
        return config;
    }
    let mut config = match std::fs::read_to_string(path) {
        Ok(content) => toml::from_str(&content).unwrap_or_else(|e| {
            eprintln!("Failed to parse config, using defaults: {e}");
            // Keep our identity across a corrupt file instead of minting a new
            // device id on each launch — but never silently drop the old file.
            let backup = path.with_extension("toml.bad");
            if let Err(e) = std::fs::rename(path, &backup) {
                eprintln!("Failed to back up config to {backup:?}: {e}");
            }
            let config = Config::default();
            if let Err(e) = save_config_at(path, &config) {
                eprintln!("Failed to rewrite config: {e}");
            }
            config
        }),
        Err(e) => {
            eprintln!("Failed to read config, using defaults: {e}");
            Config::default()
        }
    };
    if dedupe_shares(&mut config.shares) {
        if let Err(e) = save_config_at(path, &config) {
            eprintln!("Failed to persist config without duplicate shares: {e}");
        }
    }
    config
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
// 共享项去重（F-21）
// ---------------------------------------------------------------------------

/// 共享项里**已经登记过**这条路径的那一项（重复登记同一个目录/文件没有意义）。
///
/// 判据是**路径本身**，不是名字：改过名不影响、同一个目录换个写法（结尾多个 `\`、大小写不同、
/// 中间夹个 `.`、符号链接）都算同一条。能 `canonicalize` 就按它的结果比 —— Windows 上顺带解决
/// 大小写、分隔符、8.3 短名；路径取不到（刚被删、权限不够）时退回「去掉尾分隔符 + 小写」的
/// 字符串比较：宁可宽松，也不在一个"要不要插一条"的判断上抛错。
pub fn find_share_by_path<'a>(shares: &'a [ShareItem], path: &Path) -> Option<&'a ShareItem> {
    let want = path.canonicalize().ok();
    shares.iter().find(|s| match (&want, s.path.canonicalize()) {
        (Some(a), Ok(b)) => *a == b,
        _ => normalize_path(&s.path) == normalize_path(path),
    })
}

/// 上面那条兜底比较用的形式：`/` 统一成 `\`、去掉尾分隔符；大小写**只在大小写不敏感的文件系统上**
/// 折叠（Windows / macOS）—— 在 Linux 上 `/tmp/A` 与 `/tmp/a` 是两个目录，折了会把用户的共享项
/// 误合成一条（真实存在的路径根本走不到这一步：那时按 `canonicalize` 的结果比）。
fn normalize_path(p: &Path) -> String {
    let s = p.to_string_lossy().replace('/', "\\").trim_end_matches('\\').to_string();
    if cfg!(any(windows, target_os = "macos")) {
        s.to_lowercase()
    } else {
        s
    }
}

/// 登记一条共享项（F-21）：**同一条路径只留一条** —— 已经在列表里就原样返回库里那一条
/// （id 不变、名字改过也不动、广播开关不重置），否则把它插进去。返回 `(是否新增, 库里那一条)`。
///
/// 查重与插入在同一个函数里完成：并发来的两笔（一批登记走 `Promise.all`）不会在"查一次、
/// 再插一次"之间互相漏过去。调用方负责把这一步放进同一次持锁里（见 `lib.rs::add_share`）。
pub fn register_share(shares: &mut Vec<ShareItem>, item: ShareItem) -> (bool, ShareItem) {
    match find_share_by_path(shares, &item.path) {
        Some(existing) => (false, existing.clone()),
        None => {
            shares.push(item.clone());
            (true, item)
        }
    }
}

/// 把共享项里**同一路径的重复**整条丢掉（读配置时收一次）：早先版本允许重复登记，重复那些是
/// 历史遗留，各带各的 id/名字/广播开关，留着只会在列表里多出几行一样的。
///
/// **保留最先登记的那一条**：名字可能被用户改过、广播开关是用户设的，都挂在最早那条身上。
/// 返回是否真的丢掉了东西（调用方据此决定要不要立刻写回文件）。
fn dedupe_shares(shares: &mut Vec<ShareItem>) -> bool {
    let before = shares.len();
    let mut kept: Vec<ShareItem> = Vec::with_capacity(before);
    for item in shares.drain(..) {
        if find_share_by_path(&kept, &item.path).is_none() {
            kept.push(item);
        }
    }
    *shares = kept;
    shares.len() != before
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

    /// 共享项按路径去重（F-21）：同一条路径换个写法（结尾 `\`、中间夹 `.`、Windows 上的大小写）
    /// 仍算**同一条** —— 否则拖回卡片、选择框里重复挑就会在列表里堆出第二份。
    /// 而父目录与子目录是两条不同的共享项，不能误判成重复。
    #[test]
    fn find_share_by_path_dedupes_by_path_not_spelling() {
        let dir = std::env::temp_dir().join(format!("orbdrop_dedupe_{}", Uuid::new_v4()));
        let inner = dir.join("inner");
        std::fs::create_dir_all(&inner).unwrap();
        let shares = vec![ShareItem {
            id: "s1".into(),
            name: "inner".into(),
            path: inner.clone(),
            created_at: Utc::now(),
            broadcast_enabled: false,
        }];

        assert!(find_share_by_path(&shares, &inner).is_some(), "同一条路径");
        assert!(find_share_by_path(&shares, &&inner).is_some(), "多写一个 & 也还是它");
        assert!(
            find_share_by_path(&shares, &inner.join(".")).is_some(),
            "路径中间带 `.`：canonicalize 之后是同一条"
        );
        assert!(
            find_share_by_path(&shares, &inner.join("child").join("..")).is_some(),
            "绕一圈回来还是它"
        );
        assert!(find_share_by_path(&shares, &dir).is_none(), "父目录是另一条共享项");
        assert!(find_share_by_path(&shares, &dir.join("other")).is_none(), "不存在的别的路径");
        #[cfg(windows)]
        assert!(
            find_share_by_path(&shares, &dir.join("INNER")).is_some(),
            "Windows 上大小写不同也是同一条（用户从资源管理器复制来的路径大小写常不一样）"
        );

        // 路径已经不在磁盘上（刚被删、权限不够）：退回字符串比较，同样不能凭空造第二条。
        let gone = dir.join("gone");
        let stale = vec![ShareItem {
            id: "s2".into(),
            name: "gone".into(),
            path: gone.clone(),
            created_at: Utc::now(),
            broadcast_enabled: false,
        }];
        assert!(find_share_by_path(&stale, &gone).is_some(), "取不到 canonicalize 也要认出来");
        assert!(find_share_by_path(&stale, &dir.join("other")).is_none());
        assert!(
            find_share_by_path(&stale, &PathBuf::from(format!("{}\\", gone.display()))).is_some(),
            "结尾多个分隔符是同一条"
        );

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 老配置里躺着同一条路径的两份（早先版本允许重复登记）：**读进来只留最先那一条**，
/// 名字（用户可能改过）与广播开关都保持原样，并且**当场写回** —— `config.toml` 里也不会
/// 再留着那几条重复。
    #[test]
    fn load_config_drops_duplicate_share_paths() {
        let dir = std::env::temp_dir().join(format!("orbdrop_dupshares_{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        std::fs::write(
            &path,
            "deviceId = \"device-1\"\n\
             \n\
             [[shares]]\n\
             id = \"first\"\n\
             name = \"projects\"\n\
             path = 'C:/Projects'\n\
             broadcastEnabled = true\n\
             createdAt = \"2026-09-01T00:00:00Z\"\n\
             \n\
             [[shares]]\n\
             id = \"dup-trailing-sep\"\n\
             name = \"projects（又拖了一次）\"\n\
             path = 'C:/Projects/'\n\
             broadcastEnabled = false\n\
             createdAt = \"2026-09-21T00:00:00Z\"\n\
             \n\
             [[shares]]\n\
             id = \"other\"\n\
             name = \"其他\"\n\
             path = 'C:/Other'\n\
             broadcastEnabled = false\n\
             createdAt = \"2026-09-21T00:00:00Z\"\n",
        )
        .unwrap();

        let config = load_config_at(&path);
        assert_eq!(config.shares.len(), 2, "重复那条丢了，别的照旧");
        assert_eq!(config.shares[0].id, "first", "留下的是最先登记的那一条");
        assert_eq!(config.shares[0].name, "projects", "名字不被后来那条顶掉");
        assert!(config.shares[0].broadcast_enabled, "广播开关保持原样");
        assert_eq!(config.shares[1].id, "other", "不同的路径一条都不动");
        assert_eq!(config.device_id, "device-1", "配置的其余部分不受影响");

        // 写回了：再读一次文件本身，里面也不该有那两条重复
        let on_disk = std::fs::read_to_string(&path).unwrap();
        assert!(!on_disk.contains("dup-trailing-sep"), "重复项已经从文件里消失");
        assert!(on_disk.contains("id = \"first\"") && on_disk.contains("id = \"other\""));
        let reread: Config = toml::from_str(&on_disk).unwrap();
        assert_eq!(reread.shares.len(), 2);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 没有重复时不写回：`load_config_at` 不会每启动一次就重写一遍用户的配置文件。
    #[test]
    fn load_config_leaves_clean_shares_untouched() {
        let dir = std::env::temp_dir().join(format!("orbdrop_cleanshares_{}", Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("config.toml");
        let original = "deviceId = \"device-1\"\n\n[[shares]]\nid = \"only\"\nname = \"one\"\npath = 'C:/One'\nbroadcastEnabled = false\ncreatedAt = \"2026-09-01T00:00:00Z\"\n";
        std::fs::write(&path, original).unwrap();
        let before = std::fs::metadata(&path).unwrap().modified().unwrap();

        let config = load_config_at(&path);
        assert_eq!(config.shares.len(), 1);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original, "文件一个字节都没动");
        assert_eq!(std::fs::metadata(&path).unwrap().modified().unwrap(), before);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    /// 登记按路径去重（F-21）：第二次登记同一条路径**不动库里那一条** —— id、名字（可能被改过）、
    /// 广播开关都保持原样。用户看到的是"没有多出一项"，而不是"多出一项、还把旧的顶掉"。
    #[test]
    fn register_share_keeps_the_first_entry_and_reports_not_created() {
        let dir = std::env::temp_dir().join(format!("orbdrop_reg_{}", Uuid::new_v4()));
        let path = dir.join("p");
        std::fs::create_dir_all(&path).unwrap();

        let mut shares = Vec::new();
        let first = ShareItem {
            id: "keep".into(),
            name: "旧名字".into(),
            path: path.clone(),
            created_at: Utc::now(),
            broadcast_enabled: true,
        };
        let (created, stored) = register_share(&mut shares, first.clone());
        assert!(created, "第一次登记是新增");
        assert_eq!(stored.id, "keep");

        let again = ShareItem {
            id: "new".into(),
            name: "新名字".into(),
            path: path.clone(),
            created_at: Utc::now(),
            broadcast_enabled: false,
        };
        let (created, stored) = register_share(&mut shares, again);
        assert!(!created, "同一条路径不该再插一条");
        assert_eq!(shares.len(), 1, "库里只有一条");
        assert_eq!(stored.id, "keep", "回来的还是原来那一条");
        assert_eq!(shares[0].name, "旧名字", "登记时改过的名字不动");
        assert!(shares[0].broadcast_enabled, "广播开关不被重置");

        // 同一个目录换个写法（结尾多一个分隔符）同样不新增
        let (created, _) = register_share(
            &mut shares,
            ShareItem {
                id: "x".into(),
                name: "x".into(),
                path: PathBuf::from(format!("{}\\", path.display())),
                created_at: Utc::now(),
                broadcast_enabled: false,
            },
        );
        assert!(!created);
        assert_eq!(shares.len(), 1);

        std::fs::remove_dir_all(&dir).unwrap();
    }

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
            orb_center: Some(OrbPos { x: -1280, y: 40 }),
            orb_pos: None,
            orb_shape: OrbShape::Rect,
            online: false,
            auto_broadcast_new: true,
            group_relay: true,
            picker_favorites: vec![
                PickerFavorite { name: "桌面".into(), path: "C:/Users/t/Desktop".into() },
                PickerFavorite { name: "项目".into(), path: "D:/项目".into() },
            ],
            device_remarks: HashMap::from([("peer-1".to_string(), "客厅台式机".to_string())]),
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
        assert_eq!(loaded.online, false, "「离线」要跨重启保留（用户选了就一直离线）");
        assert_eq!(
            loaded.orb_center,
            Some(OrbPos { x: -1280, y: 40 }),
            "悬浮球位置要跨重启保留（F-74）：负坐标是左边那块显示器"
        );
        assert!(
            !std::fs::read_to_string(&tmp_file).unwrap().contains("orbPos"),
            "老字段（窗口左上角）不再写出：位置只有 `orbCenter` 这一个真相"
        );
        assert_eq!(
            loaded.orb_shape,
            OrbShape::Rect,
            "悬浮形态要跨重启保留（用户在设置里选了哪一档，下次还得是那一档）"
        );
        assert_eq!(
            loaded.auto_broadcast_new, true,
            "「添加后自动共享」要跨重启保留（开着就一直开着）"
        );
        assert_eq!(
            loaded.device_remarks.get("peer-1").map(String::as_str),
            Some("客厅台式机"),
            "设备备注要跨重启保留（F-101：只在本机，重启后还在）"
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

    /// 旧配置文件里没有 `orbCenter`：照样解析成 `None`，不能判成损坏（旧字段 `orbPos` 最多是 `None`）。
    ///
    /// （`load_config` 遇到损坏文件会把它改名成 `.toml.bad` 再写回默认值 —— 加一个字段
    /// 就让用户的共享项/配对表消失，代价太大。）
    #[test]
    fn config_without_orb_center_keeps_loading() {
        let config: Config = toml::from_str("port = 3895\ndeviceId = \"device-1\"\n").unwrap();
        assert_eq!(config.orb_center, None);
        assert_eq!(config.orb_pos, None);
        assert_eq!(config.port, 3895);
        assert_eq!(config.device_id, "device-1");
    }

    /// 老配置里的 `orbPos`（窗口左上角）要能读出来，并换算出**同一个位置**的本体中心。
    ///
    /// 换算公式 = 旧值 + 本体内缩 × DPI：圆球 45/45、卡片 102/77（见 `collapsed_center_inset`）。
    /// 不这么换的话，升级后悬浮球会跳（球 45px、卡片 102/77px）—— 用户的"窗口位置记忆"就白存了。
    #[test]
    fn legacy_orb_pos_migrates_to_body_center() {
        let legacy: Config =
            toml::from_str("deviceId = \"device-1\"\norbShape = \"rect\"\n[orbPos]\nx = -1280\ny = 40\n")
                .unwrap();
        assert_eq!(legacy.orb_center, None);
        assert_eq!(legacy.orb_pos, Some(OrbPos { x: -1280, y: 40 }));
        assert_eq!(
            orb_center_from_legacy(legacy.orb_pos.unwrap(), OrbShape::Rect, 1.0),
            OrbPos { x: -1178, y: 117 },
            "卡片本体中心 = 窗口左上角 + (102, 77)"
        );
        assert_eq!(
            orb_center_from_legacy(OrbPos { x: 100, y: 200 }, OrbShape::Circle, 1.25),
            OrbPos { x: 100 + 56, y: 200 + 56 },
            "圆球本体中心 = 窗口左上角 + 45×DPI（125% 下是 56）"
        );

        // 老字段不再写出：迁移之后配置里只剩 `orbCenter`。
        let migrated = Config { orb_center: Some(OrbPos { x: -1178, y: 117 }), orb_pos: None, ..legacy };
        let text = toml::to_string_pretty(&migrated).unwrap();
        assert!(text.contains("orbCenter"), "{text}");
        assert!(!text.contains("orbPos"), "{text}");
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

    /// 「离线」跨重启有效：写进配置文件；**旧配置**（没有这一项）读出来是在线。
    ///
    /// 漏掉 `#[serde(default)]` 的话，旧配置会解析失败 → `load_config` 把它改名成
    /// `.toml.bad` 再写默认值，用户升级一次就丢掉全部共享项与配对表。
    #[test]
    fn online_round_trips_and_defaults_to_true() {
        let old: Config = toml::from_str("deviceId = \"device-1\"\n").unwrap();
        assert!(old.online, "老配置没有 online：默认在线，不能默认把用户静默拉黑");
        assert!(old.settings().online, "AppSettings（get_settings 的返回）要带上它");

        let offline: Config = toml::from_str("deviceId = \"device-1\"\nonline = false\n").unwrap();
        assert!(!offline.online);
    }

    /// 悬浮形态跨重启有效；**旧配置**（没有这一项）读出来是圆球。
    ///
    /// 漏掉 `#[serde(default)]` 的后果与 `online` 那条一样重：解析失败 → 配置文件被改名成
    /// `.toml.bad` → 用户的共享项与配对表全丢。字面量也要钉死（前端的联合类型是
    /// `'circle' | 'rect'`，落盘与 IPC 上必须是同一套小写）。
    #[test]
    fn orb_shape_round_trips_and_defaults_to_circle() {
        let old: Config = toml::from_str("deviceId = \"device-1\"\n").unwrap();
        assert_eq!(old.orb_shape, OrbShape::Circle, "老配置没有 orbShape：默认圆球");
        assert_eq!(old.settings().orb_shape, OrbShape::Circle, "AppSettings 要带上它");

        let rect: Config = toml::from_str("deviceId = \"device-1\"\norbShape = \"rect\"\n").unwrap();
        assert_eq!(rect.orb_shape, OrbShape::Rect);

        assert_eq!(serde_json::to_string(&OrbShape::Circle).unwrap(), "\"circle\"");
        assert_eq!(serde_json::to_string(&OrbShape::Rect).unwrap(), "\"rect\"");
    }

    /// 「添加后自动共享」跨重启有效；**旧配置**（没有这一项）读出来是**关**。
    ///
    /// 漏掉 `#[serde(default)]` 的后果和 `online` 那条一样重：解析失败 → 配置文件被改名成
    /// `.toml.bad` → 用户的共享项与配对表全丢。缺省值本身是**关**（`bool` 的默认，见
    /// `Config::auto_broadcast_new` 的说明）。
    #[test]
    fn auto_broadcast_new_defaults_to_false_and_round_trips() {
        let old: Config = toml::from_str("deviceId = \"device-1\"\n").unwrap();
        assert!(!old.auto_broadcast_new, "老配置没有这一项：默认关");
        assert!(
            !old.settings().auto_broadcast_new,
            "AppSettings（get_settings 的返回）要带上它"
        );

        let on: Config =
            toml::from_str("deviceId = \"device-1\"\nautoBroadcastNew = true\n").unwrap();
        assert!(on.auto_broadcast_new);
        assert!(on.settings().auto_broadcast_new);
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

    /// 设备备注（F-101）：默认空、跨重启保留、空串即清除、超长截断。
    ///
    /// 前两条是**用户数据**的底线：老配置（没有这一节）解析出来必须是空表（不是解析失败 ——
    /// 那会把共享项与配对表一起丢掉）；写下去的备注重启后要还在。
    #[test]
    fn device_remarks_round_trip_and_clear() {
        let old: Config = toml::from_str("deviceId = \"device-1\"\n").unwrap();
        assert!(old.device_remarks.is_empty(), "老配置没有这一节：没有备注");

        let mut config = Config::default();
        assert_eq!(config.set_device_remark("peer-a", "  客厅台式机 "), Some("客厅台式机".into()));
        assert_eq!(config.set_device_remark("peer-a", ""), None, "空串 = 清除");
        assert!(!config.device_remarks.contains_key("peer-a"));

        config.set_device_remark("peer-a", "客厅台式机");
        config.set_device_remark("peer-b", "会议室");
        let text = toml::to_string_pretty(&config).unwrap();
        let loaded: Config = toml::from_str(&text).unwrap();
        assert_eq!(loaded.device_remarks.get("peer-a").map(String::as_str), Some("客厅台式机"));
        assert_eq!(loaded.device_remarks.get("peer-b").map(String::as_str), Some("会议室"));

        // 超长的按字符截断（中文一个字算一个，不能按字节切 —— 那会切出半个字）
        let long = "备".repeat(REMARK_MAX_CHARS + 5);
        let stored = config.set_device_remark("peer-c", &long).unwrap();
        assert_eq!(stored.chars().count(), REMARK_MAX_CHARS);
        assert_eq!(config.device_remarks.get("peer-c").unwrap(), &stored);
    }

    /// 安卓默认设备名（S-05）的挑选规则：**厂商市场名优先**，其次是"品牌 + 型号"。
    ///
    /// 这是"这台设备到底是谁"唯一的来源：`ro.product.model` 在国行 ROM 上是型号代码
    /// （`24129PN74C` / `V2502A`），摆到设备列表里谁也认不出。假属性表代替真机（CI 里没有
    /// 安卓机器），只钉规则本身。
    #[test]
    fn android_device_name_prefers_vendor_market_name() {
        // 小米：市场名在，型号与品牌都不再参与
        let xiaomi = fake_props(&[
            ("ro.product.marketname", "Xiaomi 15 Pro"),
            ("ro.product.model", "24129PN74C"),
            ("ro.product.brand", "Xiaomi"),
        ]);
        assert_eq!(pick_android_device_name(xiaomi), Some("Xiaomi 15 Pro".into()));

        // 市场名只落在分区变体里 / OPPO 系 / vivo 系各探一条：属性不存在时返回 None，
        // 所以整串探一遍不亏（`ro.product.marketname` 缺席时下面的键才是答案）。
        assert_eq!(
            pick_android_device_name(fake_props(&[
                ("ro.product.vendor.marketname", "Redmi Note 13 Pro+ 5G"),
                ("ro.product.model", "2312DRAABC"),
            ])),
            Some("Redmi Note 13 Pro+ 5G".into())
        );
        assert_eq!(
            pick_android_device_name(fake_props(&[
                ("ro.vendor.oplus.market.name", "OnePlus 12"),
                ("ro.product.model", "PJD110"),
            ])),
            Some("OnePlus 12".into())
        );
        assert_eq!(
            pick_android_device_name(fake_props(&[
                ("ro.vivo.market.name", "vivo X300"),
                ("ro.product.model", "V2502A"),
            ])),
            Some("vivo X300".into())
        );
    }

    /// 没有市场名的机器（三星、华为这类）落回"品牌 + 型号"：型号是代码时加品牌当路标，
    /// 型号本身就是人话、或已经以品牌开头时不再叠加 —— 名字不该长成 `Xiaomi Xiaomi 15`。
    #[test]
    fn android_device_name_falls_back_to_brand_plus_model() {
        assert_eq!(
            pick_android_device_name(fake_props(&[
                ("ro.product.brand", "samsung"),
                ("ro.product.model", "SM-S928B"),
            ])),
            Some("samsung SM-S928B".into())
        );
        assert_eq!(
            pick_android_device_name(fake_props(&[
                ("ro.product.brand", "google"),
                ("ro.product.model", "Pixel 7"),
            ])),
            Some("Pixel 7".into()),
            "型号本身就是名字（有空格），不再叠品牌"
        );
        assert_eq!(
            pick_android_device_name(fake_props(&[
                ("ro.product.brand", "Xiaomi"),
                ("ro.product.model", "Xiaomi 15"),
            ])),
            Some("Xiaomi 15".into()),
            "型号已经以品牌开头，不重复叠"
        );

        // 一条属性都没有、或只有占位值：调用方拿 `None` 去用平台兜底
        assert_eq!(pick_android_device_name(fake_props(&[])), None);
        assert_eq!(
            pick_android_device_name(fake_props(&[
                ("ro.product.model", "unknown"),
                ("ro.product.brand", "  "),
            ])),
            None,
            "ROM 上常见的占位值不算名字"
        );
    }

    /// `repair_device_name` 的判据：只换"我们自己自动取名写下的值"，用户起过的名字一个字不动。
    #[test]
    fn only_auto_picked_names_get_replaced() {
        assert!(should_replace_auto_name("OrbDrop-Mac", "Pixel 7"), "平台兜底串要换");
        assert!(
            !should_replace_auto_name("OrbDrop-Mac", "OrbDrop-Mac"),
            "换不出更好的就别写盘（否则每次启动都白落一次盘）"
        );
        assert!(
            !should_replace_auto_name("王磊的手机", "Pixel 7"),
            "用户自己起的名字不动 —— 哪怕它更好看"
        );
    }

    /// 一张假的"系统属性表"，冒充 `__system_property_get`。
    fn fake_props(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let map: HashMap<String, String> =
            pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect();
        move |key: &str| map.get(key).cloned()
    }
}