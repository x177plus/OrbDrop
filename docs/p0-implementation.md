# OrbDrop P0 实现起点

> 基于需求文档 v0.5。P0 目标：桌面端端到端 MVP——悬浮球 + 拖入登记 + 引用树浏览 + 下载 + 状态持久化（单机版，无网络互连）。

## 1. 项目脚手架

```bash
npm create tauri-app@latest orbdrop -- --template svelte-ts
cd orbdrop
```

Tauri v2 默认结构：

```
orbdrop/
├── src/                  # Svelte 5 前端
├── src-tauri/            # Rust 后端
│   ├── src/
│   │   ├── lib.rs        # Tauri 插件/命令注册
│   │   ├── main.rs       # 入口
│   └── Cargo.toml
├── package.json
└── tauri.conf.json       # 窗口配置（置顶/无边框/尺寸）
```

## 2. Rust 依赖（src-tauri/Cargo.toml）

```toml
[dependencies]
tauri = { version = "2", features = ["tray-icon"] }   # 托盘/菜单栏
tauri-plugin-shell = "2"                                # 启动外部进程（BC）
serde = { version = "1", features = ["derive"] }
serde_json = "1"
toml = "0.8"                                            # 配置文件
keyring = "3"                                           # 系统凭据库
tokio = { version = "1", features = ["full"] }
axum = { version = "0.8", features = ["ws"] }           # HTTP + WS + WebDAV
tower-http = { version = "0.6", features = ["cors"] }   # 中间件
mdns-sd = "0.13"                                        # mDNS
sha2 = "0.10"                                           # 哈希校验
notify = "7"                                            # 文件变更监听（F-33）
quick-xml = "0.37"                                      # WebDAV PROPFIND XML
chrono = { version = "0.4", features = ["serde"] }      # 时间
tracing = "0.1"                                         # 日志
tracing-subscriber = { version = "0.3", features = ["env-filter", "json"] }
```

## 3. Rust 模块布局

```
src-tauri/src/
├── main.rs                    # 入口，Tauri builder
├── lib.rs                     # Tauri 插件/命令导出
├── config.rs                  # 配置读写（TOML）
├── credential.rs               # 系统凭据库封装（keyring）
├── server/
│   ├── mod.rs                 # axum 服务启动 + 端口探测
│   ├── routes_tree.rs         # GET /tree
│   ├── routes_file.rs         # GET/PUT/DELETE /file
│   ├── routes_dav.rs          # PROPFIND / OPTIONS（WebDAV）
│   ├── routes_shares.rs       # GET /shares, POST /share/permissions
│   ├── ws.rs                  # WS /events
│   └── auth.rs                # Bearer + Basic 统一认证中间件
├── discover/
│   ├── mod.rs                 # mDNS 服务注册 + 扫描
│   └── peer.rs                # 设备信息模型
├── share/
│   ├── mod.rs                 # 共享项生命期管理
│   ├── item.rs                # 共享项数据结构
│   └── watcher.rs             # 目录变更检测（notify → WS）
├── transfer/
│   ├── mod.rs                 # 传输管理
│   └── range.rs               # Range 处理
├── state.rs                   # App 全局状态（共享项/设备列表/设置）
├── orb_window.rs              # 悬浮球窗口几何：显示器工作区查询 + 原子移动/缩放
└── log.rs                     # 日志初始化
```

## 4. 前端 Svelte 5 组件

```
src/
├── App.svelte                 # 根：悬浮球 + 主界面切换
├── lib/
│   ├── components/
│   │   ├── Orb.svelte         # 悬浮球（点击展开/收缩）
│   │   ├── TabBar.svelte      # 一级设备标签栏
│   │   ├── TabContent.svelte  # 标签内容区（目录树/文件列表）
│   │   ├── TreeNode.svelte    # 树节点（惰性展开）
│   │   ├── ContextMenu.svelte # 右键菜单
│   │   ├── BroadcastSwitch.svelte  # 广播开关
│   │   ├── DevicePicker.svelte     # 设备选择面板
│   │   └── SettingsPanel.svelte    # 设置面板
│   ├── stores/
│   │   ├── shares.ts          # 本机共享项状态
│   │   ├── devices.ts         # 设备列表状态
│   │   └── settings.ts        # 设置状态
│   └── types.ts               # 类型定义
├── styles/
│   └── main.css               # 全局样式（无边框/圆角/置顶）
└── main.ts                    # Tauri 应用入口
```

## 5. Tauri 配置关键项（tauri.conf.json）

```json
{
  "app": {
    "windows": [
      {
        "label": "main",
        "decorations": false,
        "alwaysOnTop": true,
        "transparent": false,
        "width": 60,
        "height": 60,
        "minWidth": 40,
        "minHeight": 40,
        "center": true,
        "resizable": false,
        "dragDropEnabled": true
      }
    ],
    "trayIcon": {
      "iconPath": "icons/tray.png",
      "tooltip": "OrbDrop"
    }
  },
  "bundle": {
    "windows": {
      "wix": {
        "firewall": {
          "enabled": true,
          "program": "{app}",
          "direction": "inbound",
          "profile": "private"
        }
      }
    }
  }
}
```

## 6. 构建顺序（按依赖排）

### Step 1：配置文件 + 状态持久化（config.rs）

P0 第一个落地模块，所有后续依赖它。

```rust
// config.rs — 配置文件路径 + 读写
// Windows: %APPDATA%/OrbDrop/config.toml
// macOS:   ~/Library/Application Support/OrbDrop/config.toml

#[derive(Debug, Serialize, Deserialize)]
pub struct Config {
    pub port: u16,
    pub download_dir: PathBuf,
    pub download_mode: DownloadMode,  // Direct | AskEachTime
    pub inbox_dir: PathBuf,
    pub device_name: String,
    pub auto_start: bool,
}

// credential.rs — 系统凭据库封装
pub fn save_peers(peers: &HashMap<String, String>) -> Result<()>  // 写入 keyring
pub fn load_peers() -> Result<HashMap<String, String>>            // 从 keyring 读取
pub fn remove_peer(id: &str) -> Result<()>                       // 解除配对
```

### Step 2：axum HTTP 服务（server/mod.rs）

```rust
// 端口探测逻辑
pub async fn start_server(config: &Config) -> Result<u16> {
    let base_port = config.port;
    for port in base_port..=base_port + 10 {
        match bind(port).await {
            Ok(listener) => {
                // mDNS 注册实际端口
                mdns::register(port).await;
                return Ok(port);
            }
            Err(_) if port < base_port + 10 => continue,
            Err(_) => bail!("端口 {base_port}~{} 均被占用", base_port + 9),
        }
    }
}
```

**P0 只需要：**
- `GET /tree?share=&path=` → 基于共享项根路径惰性展开一层
- `GET /file?share=&path=&offset=&len=` → 带 Range 的文件读取
- 这两个路由**不需要设备互连**，P0 是对本机路径服务

### Step 3：拖入 + 共享项（Svelte + Rust）

**Tauri 侧**（Rust）：

```rust
// lib.rs
#[tauri::command]
fn add_share(path: String) -> Result<ShareItem, String> {
    let item = ShareItem {
        id: Uuid::new_v4(),
        name: Path::new(&path).file_name().unwrap().to_string_lossy().to_string(),
        path: path.clone(),
        broadcast_enabled: false,
        broadcast_targets: vec![],
    };
    // 持久化到 config
    state.add_share(item.clone());
    Ok(item)
}

#[tauri::command]
fn list_shares() -> Result<Vec<ShareItem>, String> {
    Ok(state.list_shares())
}

#[tauri::command]
fn toggle_broadcast(share_id: String, targets: Vec<String>) -> Result<(), String> {
    // 开启广播 → 更新可见性列表
}
```

**Svelte 侧**：监听 `tauri:drop` 事件触发 `add_share` 命令。

### Step 4：悬浮球 + 主界面（Svelte）

- 悬浮球点击展开/收缩动画
- 展开方向自适应：按悬浮球与显示器工作区（`GetMonitorInfoW` 的 rcWork，已扣任务栏）的位置决定面板朝哪边区展开，
  并用一次 `SetWindowPos` 把展开窗口夹取在工作区内，保证面板完整可见；收起时窗口缩回悬浮球原矩形，悬浮球不跳位
- Win11 透明无边框窗口默认带一条系统绘制的 1px 边框（沿 OS 圆角露在面板圆角之外，每次改窗口尺寸重画 → 窗口最外缘闪烁的线）：
  启动与每次改窗口尺寸时用 `DWMWA_BORDER_COLOR = DWMWA_COLOR_NONE` + `DWMWA_WINDOW_CORNER_PREFERENCE = DWMWCP_DONOTROUND` 抹掉
- 本机标签（显示共享项列表 + 广播开关）
- 设备标签（P0 显示空状态"暂无已配对设备"）

### Step 5：设置面板

- 下载目录 + 下载模式 + 收件目录 + 端口配置
- 对应 S-01 ~ S-06

### Step 6：远程树浏览 + 下载

P0 的远程树是通过 `GET /tree` 和 `GET /file` 实现的——但对端是自己的本机，用于验证 UI 和协议是否正常。

- 树组件：惰性加载，点击展开调用 `GET /tree`
- 右键菜单：下载到本地（调用 `GET /file` Range，写本地文件）
- 下载遵循 S-02 模式设置

### Step 7：日志（log.rs）

```rust
// 初始化 tracing，写文件 + 轮转
fn init_log() {
    let file_appender = tracing_appender::rolling::daily(
        config_dir().join("logs"), "orbdrop.log"
    );
    //  详细日志：%APPDATA%/OrbDrop/logs/orbdrop.YYYY-MM-DD.log
}
```

## 7. 关键数据模型

```rust
// 共享项（持久化到配置文件）
struct ShareItem {
    id: String,
    name: String,
    path: PathBuf,              // 本地文件系统路径
    created_at: DateTime<Utc>,
    broadcast_enabled: bool,    // 广播开关
    broadcast_targets: Vec<String>, // 目标设备 ID 列表
}

// 设备（持久化到配置 + token 到凭据库）
struct PeerDevice {
    id: String,
    name: String,
    ip: String,
    port: u16,
    online: bool,
    paired: bool,
    // token 存在 keyring，不在配置文件里
}

// 设置（持久化到配置文件）
#[derive(Serialize, Deserialize)]
struct Settings {
    port: u16,                           // S-06 默认 3890
    download_dir: PathBuf,               // S-01 默认桌面
    download_mode: DownloadMode,         // S-02 默认 Direct
    inbox_dir: PathBuf,                  // S-03
    auto_start: bool,                    // S-04 默认 true
    device_name: String,                 // S-05 默认 hostname
}
```

## 8. 校验清单（P0 完成标志）

```
☐ 悬浮球启动，始终置顶，可拖动
☐ 拖入目录/文件 → 本机标签中出现共享项
☐ 本机标签中共享项可展开为目录树（惰性）
☐ 右键 → 下载到本地（到 S-01 指定目录）
☐ 右键 → 下载到本地并用 Explorer/Finder 打开
☐ 广播开关 UI：开关 + 设备选择面板（P0 显示空列表）
☐ 设置面板：下载目录/收件目录/下载模式/端口
☐ 配置持久化：重启后共享项/设置恢复
☐ 端口被占用自动 +N 探测（日志记录实际端口）
```