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
keyring = "3"                                           # 仅用于迁移老系统凭据库那一条
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
├── credential.rs               # 凭据文件封装（credentials.dat）
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
├── orb_window.rs              # 悬浮球窗口几何：显示器工作区查询 + 原子移动/缩放 + 鼠标键状态（等松手）
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
// Windows: %LOCALAPPDATA%/OrbDrop/config.toml
// macOS:   ~/Library/Application Support/OrbDrop/config.toml
// 全部落在"本机专属"目录：不参与域漫游配置文件的同步

#[derive(Debug, Serialize, Deserialize)]
pub struct Config {
    pub port: u16,
    pub download_dir: PathBuf,
    pub download_mode: DownloadMode,  // Direct | AskEachTime
    pub device_name: String,
    pub auto_start: bool,
}

// credential.rs — 凭据文件封装（数据目录下的 credentials.dat）
pub fn save_peers(peers: &HashMap<String, String>) -> Result<()>  // 覆盖写凭据文件（原子写）
pub fn load_peers() -> Result<HashMap<String, String>>            // 读凭据文件；文件不在时只读回落到老系统凭据库那条
pub fn remove_peer(id: &str) -> Result<()>                       // 解除配对
pub fn wipe() -> Result<bool>                                    // 一键清空（删文件 + 老条目）
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

> 2026-09-21：`add_share` 现在**按路径去重**（F-21）—— 已经登记过的路径原样返回那一项，
> 不新增也不改名；选择框那条路（`App.addShares`）靠"回来的 id 是不是本来就有的"提示
> 「已添加」还是「已在共享项里」。另外 `load_config_at` 读配置时把**历史遗留的同路径重复项
> 整条收掉**（保留最先登记的那一条）并写回文件 —— 列表与 `config.toml` 一起干净。

### Step 4：悬浮球 + 主界面（Svelte）

- 悬浮球点击展开/收缩动画
- 展开方向自适应：按悬浮体（球 / 卡片）**本体中心**与显示器工作区（`GetMonitorInfoW` 的 rcWork，已扣任务栏）的位置决定面板朝哪边区展开，
  并用一次 `SetWindowPos` 把展开窗口夹取在工作区内，保证面板完整可见；收起时窗口缩回「悬浮球在面板里的那一角」——
  展开态拖动过窗口时，悬浮球跟着面板走（按窗口位移平移原矩形），而不是跳回展开前那块屏幕坐标。
  参考点是**本体中心**（球的圆心 / 卡片白底 176×134 的中心），面板的**可视盒角**落在它上面（朝左下展开 → 盒右上角 = 中心）：
  落位公式里不出现悬浮体尺寸，所以同一个中心下两种形态展开出的面板逐像素相同（拿窗口或悬浮体某条边算都会带上尺寸，
  两形态差一个悬浮体尺寸）。改前拿 OS 窗口算的后果是 146px 横跳 + 面板开着改形态再收起时 4px 偏移；
  贴边收起那一档中心已在屏幕外，该轴不留边距（窗口贴住工作区边），面板因此贴着屏幕边
- 悬浮球贴边收起：收起落位（以及拖动悬浮球松手后）判一次吸附，球边缘距工作区边缘 ≤ `DOCK_THRESHOLD`（20px）即贴边：
  窗口贴住工作区边缘、把球大半个推出屏幕外，只露 `DOCK_EXPOSE`（当前 1/3 ≈ 21px，改这一个常量即可调 1/2 或更小）。
  「大半颗在屏幕外」靠窗口裁剪球实现，窗口本身始终整个留在工作区内（不必让 OS 窗口探出屏幕），动画也就只是一次 CSS 滑动；
  贴边点必须落在工作区边缘上 —— 窗口若停在离边缘还有几像素处，裁掉球的就成了窗口边（屏幕上一条看得见的切线）。
  指针移入露出的那一小条即滑回完整可见，移出再滑回去（进出各有 0.22s 过渡）。悬停热区 = **露出的那一小条**：
  呼出阈值就是屏上看得见的那点儿 —— 球收进去之后才会占的那一整颗方框**不算**热区（否则收起态在屏幕边上圈着
  一大块，指针从旁边路过就把球呼出来，压在下面的窗口也点不着）；球滑回完整位之后指针落在球上，那时才把球
  并进热区（`App.collapsedHitRects`，2026-09-24 改，原为「露出的部分 ∪ 完整球」的并集）。热区本身固定不动，
  只有球在里面滑 —— 热区若跟着球动，指针会被滑走的球甩下、球滑回又被压到，一进一出抖个不停；
  未贴边的轴热区就是球本身，行为与不贴边时一致。
  **拖着外部文件进来同样把悬浮体呼出来**（不是展开面板）：OS 的文件拖动走 OLE 自己的模态循环，WebView 只收到 Tauri 转来的
  `onDragDropEvent`、收不到 pointerenter/`:hover`，只认指针的显示逻辑在拖动下整个是哑的 —— 而贴边收起时屏上只有那一条竖条，
  拖过去没有任何预告，松手却真的登记成共享项。于是 `App` 的 `reveal` = `zoneHot || dragging`，拖入复用悬停那一档显示
  （滑回完整位 + `.orb-stage.drag` 那圈主色高亮）；不展开面板是因为展开要在拖动中途 `SetWindowPos`，落点目标（`IDropTarget`）
  会被搬走/改尺寸，而面板本身没有落点语义（窗口内任意处落下都是同一条 `addShares`）。2026-09-24 改。
- 拖动期间不吸附：OS 拖动的模态循环会吃掉 mouseup，页面看不到松手，只看 move 事件间隔就会把「拖到一半停一下」当成松手，
  球在手里被抢到贴边位置。所以落位判定先查 OS 键状态（`orb_mouse_down` = Win32 `GetAsyncKeyState(VK_LBUTTON)`），按住就不动窗口，
  松手那一刻的窗口位置才是落点，球从那儿滑出去
- 悬浮球位置记忆（F-74「窗口位置」）：把**几何落定之后**的悬浮体**本体中心**（物理像素）记进配置 `orbCenter`，
  下次启动在首帧之前由后端写回窗口（`place_orb_on_startup`：窗口左上角 = 中心 − 本体内缩，圆球 45/45、卡片 102/77），
  界面随后对它重跑一次吸附判定复原贴边形态
  （吸附是幂等的 —— 已贴边的矩形再判一次得到同样的窗口与球偏移），所以只存这一个点，不存贴边方向/球偏移/尺寸/形态内缩。
  老配置里的 `orbPos`（窗口左上角）在启动时按当前形态与 DPI 换算一次成本体中心并落盘（迁移只做一次，位置不跳）。
  **记录点**：折叠态拖动落位判定跑完（`settleDrag` 末尾）、展开态拖动面板落位判定跑完
  （`settleDrag` → `rememberBallUnderPanel`：按窗口位移把球算回折叠态矩形 + 判一次吸附，
  只记录**不动窗口** —— 窗口是面板，为了球贴边把面板拽走显然不对）、收起落位跑完（`settleCollapse`）。
  松手那一刻不记：那时只是自由落点，贴边还会再挪一次窗口；展开动画期间也不记（`opening`）。
  没有记录时（首启 / 配置丢了）默认落在**工作区右上、垂直线 1/3 处**，右边留 `DEFAULT_ORB_MARGIN` ——
  余量必须大于吸附阈值 `DOCK_THRESHOLD`（20 CSS px），否则首启那次落位判定会当场把球吸到边上收起大半颗。
  复原前做一次「与显示器矩形相交」校验：上次那块屏被拔掉/分辨率变小就保持居中，
  否则球落到屏幕外，而透明无边框窗口没有标题栏可以拖回来
- Win11 透明无边框窗口默认带一条系统绘制的 1px 边框（沿 OS 圆角露在面板圆角之外，每次改窗口尺寸重画 → 窗口最外缘闪烁的线）：
  启动与每次改窗口尺寸时用 `DWMWA_BORDER_COLOR = DWMWA_COLOR_NONE` + `DWMWA_WINDOW_CORNER_PREFERENCE = DWMWCP_DONOTROUND` 抹掉
- 本机标签（显示共享项列表 + 广播开关）
- 设备标签（P0 显示空状态"暂无已配对设备"）

### Step 5：设置面板

- 下载目录 + 下载模式 + 端口配置
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
        data_dir().join("logs"), "orbdrop.log"
    );
    //  详细日志：%LOCALAPPDATA%/OrbDrop/logs/orbdrop.YYYY-MM-DD.log
    //  ⚠ 尚未设 max_log_files：N-11 要求的"保留 7 天/100MB"还没落地
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

// 设备（持久化到配置 + token 到凭据文件）
struct PeerDevice {
    id: String,
    name: String,
    ip: String,
    port: u16,
    online: bool,
    paired: bool,
    // token 存在凭据文件里，不在配置文件里
}

// 设置（持久化到配置文件）
#[derive(Serialize, Deserialize)]
struct Settings {
    port: u16,                           // S-06 默认 3890
    download_dir: PathBuf,               // S-01 默认桌面（唯一的落盘位置：下载 + 推送接收）
    download_mode: DownloadMode,         // S-02 默认 Direct
    auto_start: bool,                    // S-04 默认 true
    device_name: String,                 // S-05 默认主机名；安卓没有主机名 → 取设备型号/厂商名
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
☐ 设置面板：下载目录/下载模式/端口
☐ 配置持久化：重启后共享项/设置/悬浮球位置恢复
☐ 端口被占用自动 +N 探测（日志记录实际端口）
```