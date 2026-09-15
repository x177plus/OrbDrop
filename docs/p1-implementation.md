# OrbDrop P1 实现指南

> P1 目标：局域网完整双路径传输链路——发现、配对、推送、断点续传、变更通知、开机自启。P0 单机版在此基础上连成真正的双机链路。

## 1. P1 范围

| 模块 | 内容 | 涉及需求 |
|---|---|---|
| 发现 | mDNS 注册/扫描 + 手动添加 IP:端口 | F-10, F-11 |
| 配对 | 6 位码生成/验证/超时 + token 交换 + 凭据持久化 | F-12, F-15 |
| 设备列表 | 在线状态、共享项摘要、中断连接、解除配对 | F-13, F-14, F-25 |
| 推送 | 邀请/同意/拒绝/流式落盘/进度/取消 | F-50~F-54 |
| 断点续传 | Range 续传 + 本地偏移记录 | F-60 |
| 变更通知 | 目录文件系统监听 → WS 推送到对端 | F-33 |
| 信令 | WebSocket 在线心跳 + 推送邀请 + 变更事件 | WS /events |
| 掉线检测 | WS ping/pong + 超时标记离线 + 自动重连 | N-05 |
| 开机自启 | 注册表 / LaunchAgent | F-06, S-04 |
| 设备状态 | 未绑定/已配对/长连接三态管理与流转 | F-13, F-16 |

**设备状态模型**（贯穿 P1 所有模块）：

```
未绑定 ──配对──▶ 已配对 ──自动建链（无需点击）──▶ 长连接
 │                │                                │
 │                ├──点击"断开"（暂停自动建链）─────┘
 │                │    └──点击"连接"───────────────▶ 长连接
 │                └──解除配对────────────────▶ 移除
 └──可推送（HTTP 直达，接收需同意）
```

- **未绑定**：mDNS 发现/手动添加，无 token。可**推送**（HTTP 邀请直达，无需 WS/配对）
- **已配对**：token 交换完成。可推送；长连接由后台自动拉起，界面显示"连接中"
- **长连接**：已配对 + WS 实时连接（**配对即自动建立**；只有用户点过"断开"才需要手动"连接"恢复）。拖入文件自动广播给所有长连接设备

> 推送**不依赖**配对或长连接——邀请是普通 HTTP 请求，对端在线即可收到；接收方同意后才开始传输，同意制兜底安全。

## 2. 新增 Rust 依赖

```toml
# P0 已有 + 以下新增
uuid = { version = "1", features = ["v4"] }        # 共享项/推送 job ID
rand = "0.8"                                        # 6 位配对码生成
```

P0 依赖已覆盖：`axum`（含 WS）、`mdns-sd`、`keyring`、`notify`、`tokio`——P1 不需要额外大件。

## 3. 新增 Rust 模块

```
src-tauri/src/
├── ...
├─┬ server/
│ ├── ...
│ ├── routes_push.rs          # POST /push/invite, /respond, /confirm, GET /download
│ └── routes_pair.rs          # POST /pair  (pair 端点，WS 事件后续走信令)
├─┬ discover/
│ ├── mod.rs                  # mDNS 注册 _orbdrop._tcp + 后台扫描服务
│ └── peer.rs                 # PeerDevice 模型 + 在线状态管理
├─┬ transfer/
│ ├── mod.rs                  # 推送 job 生命期管理
│ ├── push_job.rs             # 单个推送 job（状态机：invited→accepted→downloading→done/failed）
│ └── resume.rs               # 断点偏移记录（.orbdrop_resume.json）
├─┬ ws/
│ ├── mod.rs                  # WS 连接池（每设备一条长连接）
│ ├── session.rs              # 单条 WS session（读+写循环）
│ ├── heartbeat.rs            # 30s ping + 3 次超时离线
│ └── events.rs               # 事件类型定义 + 序列化
├── pair.rs                   # 配对码生成/缓存/验证（2 分钟过期）
├── task.rs                   # 后台任务启动收口（无 Tokio 运行时则记日志跳过，绝不 panic）
└── autostart.rs              # 开机自启注册/取消
```

## 4. WebSocket 信令协议（WS /events）

### 服务端推送事件（Server → Client）

```json5
// 有新设备上线（mDNS 发现）
{ "type": "peer_online", "peer": { "id": "uuid", "name": "PC-B", "ip": "192.168.1.6", "port": 3890 } }

// 有设备下线（WS 断开 3 次心跳）
{ "type": "peer_offline", "peer_id": "uuid" }

// 收到推送邀请
{
  "type": "push_invite",
  "invite_id": "uuid",
  "sender": { "id": "uuid", "name": "PC-A" },
  "name": "项目文档",            // 文件/目录名
  "size": 12345678,             // 总大小，目录为 0
  "is_dir": false
}

// 推送进度更新（发送方收到 → 展示进度条）
{
  "type": "push_progress",
  "invite_id": "uuid",
  "received": 6543210,
  "total": 12345678
}

// 推送完成确认（发送方收到 → 标记完成）
{ "type": "push_done", "invite_id": "uuid", "status": "completed" }
{ "type": "push_done", "invite_id": "uuid", "status": "failed", "reason": "connection lost" }

// 共享方目录发生变更
{
  "type": "share_changed",
  "share_id": "uuid",
  "path": "subdir/file.txt",     // 相对于共享项根的路径
  "kind": "created"               // created | modified | deleted
}
```

### 客户端发送（Client → Server）

```json5
// 响应推送邀请
{ "type": "push_response", "invite_id": "uuid", "accepted": true }
{ "type": "push_response", "invite_id": "uuid", "accepted": false }
```

## 5. 发现流程

### mDNS 注册（启动时，axum 就绪后）

```rust
// discover/mod.rs
pub async fn register_mdns(port: u16, name: &str) -> Result<MdnsResponder> {
    let mdns = Mdns::new()?;
    let hostname = hostname();
    let service_info = ServiceInfo::new(
        "_orbdrop._tcp.local",
        name,              // 设备显示名
        &format!("{}.local.", hostname),
        "",                // host target
        port,
        &["name=".to_string() + name,
          "ver=1"],
    )?;
    mdns.register(service_info)?;
    // 注册后的 service_info 会自动广播
    Ok(mdns)
}
```

### mDNS 扫描（后台任务）

```rust
// 监听 _orbdrop._tcp 事件
pub async fn scan_mdns(tx: mpsc::Sender<PeerEvent>) -> Result<()> {
    let mdns = Mdns::new()?;
    let receiver = mdns.browse("_orbdrop._tcp.local")?;
    loop {
        if let Ok(event) = receiver.recv_async().await {
            match event {
                MdnsEvent::ServiceFound(info, ..) => {
                    let peer = PeerDevice::from(info);
                    // 检查是否已配对（keyring 中有向设备 token）
                    peer.is_paired = credential::has_peer(&peer.id);
                    tx.send(PeerEvent::Online(peer)).await;
                }
                MdnsEvent::ServiceRemoved(info, ..) => {
                    tx.send(PeerEvent::Offline(info.get_hostname())).await;
                }
            }
        }
    }
}
```

### 手动添加

```rust
// 跨网段兜底：用户输入 IP:port
// 不经过 mDNS，直接检查 keyring 中是否有该设备的 token
// 有 → 建立 WS 连接；无 → 触发配对流程
```

### 在线状态判定（F-13）

mDNS 只在对方主动告别或记录过期（约 2 分钟）时报告离开，且同名实例重启后浏览器未必重新解析，
所以在线状态由三条信号共同决定：

| 信号 | 触发 | 语义 |
|---|---|---|
| mDNS `ServiceResolved` | 对方注册/更新 | 立即标在线（并补全 ip/port） |
| WS 长连接建立/断开 | 已配对设备 | 权威信号：连上即在线，断开即离线 |
| `/pair/info` 探测（每 5s，1.5s 超时） | 上面两条之外的兜底 | 可达即在线，不可达即离线；已有长连接则不探测 |

前端只渲染 `online` 设备，因此在对方退出后数秒内该行从列表消失；地址簿与凭据保留，
对方再上线会自动重新出现（不产生重复行）。

**设备 id 必须稳定**：`deviceId` 是地址簿与凭据的键。配置里所有其他字段都带 serde 默认值，
缺字段（旧版本配置/手工裁剪）只补默认值，绝不整份重置 —— 否则每次启动都会换一个 id，
对端就会为"同一台设备"多出一行，并让旧行上的配对码失效。

## 6. 配对流程（F-12）

### 状态机

```
        A 端（发起，显示码）                 B 端（输入码）
    ┌────────────────────┐            ┌────────────────────┐
    │ 点击设备行「配对」    │            │ 点击「配对」打开弹窗 │
    │ pair_start          │            │ 在「输入对方设备上显示 │
    │ 本机生成 6 位码      │            │ 的配对码」里填码      │
    │ 存内存 2 分钟        │            └─────────┬──────────┘
    │ 弹窗显示码 "482916"  │                      │
    └─────────┬──────────┘                      │
              │ 用户读一眼 A 屏幕上的码             │
              │        POST /pair                 │
              │  { peerId, peerName, code }        │
              │◄───────────────────────────────────┘
              │  验证码一致？(单次有效，2 分钟过期)
              │  生成 token → 存 keyring（键 = B.id）
              │  标记 B 已配对（保留已知 IP:port）
              │  A.id < B.id → 拉起 WS 长连接
              │  返回 { token, peerId, peerName }
              │ ─────────────────────────────────►  B 存同一枚 token
              │                                    标记 A 已配对
              │                                    B.id < A.id → 拉起长连接
              │                                    弹窗自动关闭 + 提示配对成功
              │  A 收到 pair://completed → 弹窗关闭 + 提示
```

任一端都能发起：谁点了「配对」谁显示码，另一端在同一个弹窗里输入即可。
未绑定设备可用弹窗里的「添加设备」（IP / IP:端口，探测对端 `GET /pair/info`）先入库（F-11）。

### 代码结构

```rust
// pair.rs —— 待验证码，键 = 对端 device id（每个对端只保留最近一枚码）
struct PairStore { pending: Mutex<Vec<(String, PendingPair)>> }
struct PendingPair { code: String, created_at: Instant }
const CODE_TTL: Duration = Duration::from_secs(120);   // create_code / verify_code

// POST /pair 请求体（camelCase）
struct PairRequest {
    peer_id: Option<String>,   // 输入码一方自己的 device id
    peer_name: Option<String>,
    code: String,              // 发起方屏幕上显示的那 6 位码
}

// POST /pair 响应（camelCase）
struct PairResponse {
    token: String,
    peer_id: String,           // 验证方自己的 id（caller 以它登记对端）
    peer_name: String,
}
```

### 配对码生成

```rust
fn generate_code() -> String {
    let code: u32 = rand::thread_rng().gen_range(100_000..1_000_000);
    code.to_string()
}
// 6 位纯数字，100 万组合，2 分钟有效、单次使用
```

两条刻意的设计（都来自实际踩坑）：

- **码不绑定 peer id**：凭据就是屏幕上那串数字。绑定 + 对端 id 漂移（配置被重置、地址簿残留
  旧行、点错重复行）会直接报「配对码错误或已过期」，排查成本极高。
- **倒计时结束不自动换码**：前端只显示剩余秒数，过期后由用户点「刷新配对码」。
  曾按"过期自动刷新"每 2 分钟换码，对方照着屏幕输入时被静默换掉 → 报「配对码错误或已过期」。

### token 交换

配对成功时（token 由**验证方**生成，双方保存同一枚）：
- A 生成 token（32 字节随机，hex 编码）
- `credential::set_peer(&B.id, &token)` → A 的系统凭据库
- 响应返回 token → B 收到 → `credential::set_peer(&A.id, &token)` → B 的系统凭据库
- 两端各自 `mark_paired`；长连接由 device id 较小的一方发起（`ws::spawn_outbound_link` 内判定，重复调用幂等）
- 后续所有 HTTP 请求 `Authorization: Bearer <token>` / WebDAV `Basic base64("x:<token>")`
- 凭据落库依赖 keyring 的 store feature：Windows `windows-native`、macOS `apple-native`。
  缺失时 keyring 会退回进程内 mock（写成功、读为空），配对后拿不到 token。

## 7. 推送流程（F-50~F-54）

### 通知通道

| 目标设备状态 | 邀请方式 | 认证 |
|---|---|---|
| **长连接** | WS 推送 `push_invite` | Bearer token |
| **已配对（未长连接）** | HTTP POST `/push/invite` 直达 | Bearer token |
| **未绑定** | HTTP POST `/push/invite` 直达 | **无需认证**——同意制兜底 |

> 未绑定设备推送：邀请本身不认证（任何人可发邀请，你同意才传输）；接收方同意后，发送方为本次 job 生成**短期一次性 download_token**（内存缓存，5 分钟有效），随同意响应返回接收方，用于 `GET /push/download` 认证。

### 状态机

```
A（发送方）                              B（接收方）
┌──────────┐                            ┌──────────┐
│ 选文件/设备 │                           │          │
│ 发邀请：    │                           │   受邀    │
│  • 长连接: WS push_invite             │          │
│  • 其他:  POST /push/invite ─ HTTP ─►│ 弹窗通知   │
│  状态: invited                        │          │
│                                      │ 点击同意   │
│  (WS 收到 push_response 或            │ POST /push/respond ─ HTTP ─►
│   HTTP 收到同意响应)                  │  状态: accepted
│  状态: accepted                       │          │
│  未绑定 → 生成短期 download_token     │ (响应带回 token)
│                                      │          │
│                          GET /push/download?id=&offset=  ←─ HTTP Range │
│                          (B 主动拉取，带 Bearer/一次性 token)  │
│                                      │ 流式写入收件目录 │
│               POST /push/confirm ◄────│───────────│
│  状态: completed                      │           │
└──────────────────────────────────────┘           │
```

### 推送 job 模型

```rust
// transfer/push_job.rs
enum PushStatus { Invited, Accepted, Downloading { received: u64, total: u64 }, Completed, Failed(String) }

struct PushJob {
    id: String,
    from_peer_id: String,
    from_peer_name: String,
    path: String,              // 源路径（相对于共享项根）
    local_path: PathBuf,       // 接收方收件目录下的实际路径
    is_dir: bool,
    status: PushStatus,
}
```

### 发送方侧（A）

```rust
// 1. A 点击推送 → 选择目标设备（含未绑定）→ 发起邀请
// 根据目标设备状态分发邀请通道：
//   - 长连接：WS 发 push_invite 事件
//   - 已配对（未长连接）：HTTP POST /push/invite（带 Bearer token）
//   - 未绑定：HTTP POST /push/invite（无认证，body 含 sender device info）

// 2. A 收到 push_response（WS 事件 / HTTP 响应）
//   if accepted →
//     if 接收方未绑定 → 生成短期 download_token（32B hex，内存缓存 5 分钟）
//     标记 PushJob 状态为 accepted，等待接收方来 GET /push/download
//   if rejected → 更新状态为 failed，展示给用户
```

### 接收方侧（B）

```rust
// 1. B 收到 push_invite（WS 事件 / HTTP POST 请求）
//    → 弹窗（同意/拒绝），60 秒超时无操作 → 自动拒绝
//    同意 → POST /push/respond { invite_id, accepted: true }
//    → 响应带回认证 token：
//       - 长连接/已配对：既有 Bearer token
//       - 未绑定：一次性 download_token

// 2. B 拉起下载：
//   let offset = resume::load_offset(invite_id);  // 断点续传
//   for each file:
//     loop GET /push/download?id=&offset=   // 带 Bearer 或一次性 token
//         写入收件目录/{share_name}/{path}
//         offset += received

// 3. 完成 → POST /push/confirm { invite_id, status: "completed" }
```

### 推送通知弹窗交互

```rust
// 收到 push_invite 时：
//   - 托盘气泡提示："PC-A 给你发了一个文件「项目文档」(12.3MB)"
//   - 点击气泡展开主界面 → 弹确认对话框：
//     ┌─────────────────────────────────┐
//     │ 收到推送                        │
//     │ PC-A 发来：项目文档 (12.3MB)     │
//     │ 保存到：收件目录                  │
//     │                                 │
//     │    [拒绝]        [接收]          │
//     └─────────────────────────────────┘
//   - 超时 60 秒无操作 → 自动拒绝
```

## 8. 断点续传（F-60）

```rust
// transfer/resume.rs
// 下载偏移记录文件：%APPDATA%/OrbDrop/resume.json 或 ~/Library/Application Support/OrbDrop/resume.json

struct ResumeRecord {
    invite_id: String,
    file_path: String,     // 相对于收件目录的路径
    offset: u64,           // 已安全写入的字节数
    total: u64,
    hash_so_far: String,   // 已传输部分的哈希（不常用，可用于校验）
    updated_at: DateTime<Utc>,
}

// 下载流程：
// 1. 开始前：resume::load(invite_id) → 有记录则 offset = 上次位置
// 2. GET /push/download?id=&offset= (服务器从 offset 开始响应)
// 3. 流式写入文件（追加模式）
// 4. 定期保存 offset（每 ~1MB 或每 5 秒）
// 5. 完成后：resume::clear(invite_id)
// 6. 中断/取消后：resume 记录保留，下次可续传
```

传输方向是**接收方主动拉取**（B 侧下载线程循环 `GET /push/download`），所以续传逻辑完全在接收方，发送方只需响应 Range 请求。

## 9. 目录变更通知（F-33）

```rust
// share/watcher.rs — P0 已建立，P1 接入 WS

// 在 notify crate 的回调中，当检测到共享项路径变化时：
fn on_change(event: notify::Event, share_id: &str) {
    match event.kind {
        EventKind::Create(_) => kind = "created",
        EventKind::Modify(_) => kind = "modified",
        EventKind::Remove(_) => kind = "deleted",
    }
    // 通过 WS 广播给所有开启该共享项广播的设备
    for peer in state.get_visible_peers(share_id) {
        ws::send(peer, &WsEvent::ShareChanged { share_id, path, kind });
    }
}
```

变更通知发送后，对端设备标签中的树自动刷新对应节点。

## 10. WS 心跳与掉线检测（N-05）

```rust
// ws/heartbeat.rs
// 每个 WS session 独立心跳

const PING_INTERVAL: Duration = Duration::from_secs(30);
const MAX_MISSED_PINGS: u32 = 3;

// 服务端每 30s 发 ping，客户端回复 pong
// 连续 3 次 ping 无 pong → 判定离线
//   - 标记 peer offline（WS 事件推送其他设备）
//   - 释放 session 资源
//   - 后台启动重连计时器（指数退避：5s, 10s, 20s, ... max 60s）

// 重连后：
//   - 重新建立 WS
//   - 中断的推送 job 尝试续传（如果接收方还有 resume 记录）
//   - 设备标签恢复在线状态
```

## 11. 开机自启（F-06 / S-04）

```rust
// autostart.rs
pub fn enable_autostart() -> Result<()> {
    let exe_path = std::env::current_exe()?;
    let exe_str = exe_path.to_string_lossy();

    #[cfg(target_os = "windows")]
    {
        // HKCU\Software\Microsoft\Windows\CurrentVersion\Run
        // 值: OrbDrop = "exe_path"
        let hkcu = winreg::RegKey::predef(winreg::enums::HKEY_CURRENT_USER);
        let run = hkcu.open_subkey_with_flags(
            r"Software\Microsoft\Windows\CurrentVersion\Run",
            winreg::enums::KEY_SET_VALUE,
        )?;
        run.set_value("OrbDrop", &exe_str)?;
    }

    #[cfg(target_os = "macos")]
    {
        // ~/Library/LaunchAgents/com.orbdrop.plist
        let plist_content = format!(r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.orbdrop</string>
    <key>ProgramArguments</key>
    <array><string>{exe_str}</string></array>
    <key>RunAtLoad</key>
    <true/>
</dict>
</plist>"#);
        let plist_path = dirs::home_dir()
            .unwrap()
            .join("Library/LaunchAgents/com.orbdrop.plist");
        std::fs::write(&plist_path, plist_content)?;
    }
    Ok(())
}

pub fn disable_autostart() -> Result<()> { /* 清除注册表/删除 plist */ }
```

## 12. Timer / Background Tasks

| 任务 | 间隔 | 行为 |
|---|---|---|
| mDNS 扫描 | 持续监听 | `mdns.browse("_orbdrop._tcp")` 事件驱动 |
| WS ping | 30s | 每 session 独立定时器 |
| 离线判定 | 90s (3×30s) | 3 次无 pong → 标记 offline |
| 重连尝试 | 指数退避 5-60s | WS 断开后自动重连 |
| 配对码过期清理 | 2 分钟 | 清除 PENDING 中过期的条目 |
| 推送 job 超时 | 5 分钟 | invite 后 5 分钟无 respond → 自动取消 |
| 断点 offset 定期保存 | 每 1MB 或 5s | 写入 `resume.json` |

## 13. 构建顺序

### Step 1：mDNS 发现

```
discover/mod.rs + peer.rs
1. 注册 _orbdrop._tcp 服务（启动时）
2. 启动 mDNS 扫描后台任务
3. 设备上线/下线事件 → 更新 state 中的设备列表
4. 设备列表通过 Tauri 事件同步到前端
```

可行验证：启动两个实例，设备标签中互相出现对方。

### Step 2：WebSocket 信令（长连接）

```
ws/session.rs + heartbeat.rs + events.rs
1. 升级 WS /events 路由（握手查询参数 ?token=xxx 携带设备身份）
2. 连接池：每设备一条长连接；设备状态以真实 WS 会话为准（`WsManager::has_connection`）
3. 长连接**自动建立**（F-16），无需用户点击：
   - 配对完成 → `ws::ensure_link` 由 id 较小的一方拨号（无地址就先等 mDNS/探测补上）
   - 启动时对全部已配对设备拉起；mDNS 发现/5s 探活命中已配对设备时重新拉起
   - 「断开」= 断链 + 本机暂停自动建链（`WsManager::pause`）：**连接由单任务持有**，
     丢掉事件发送端即关掉 socket，对端立刻看到掉线；此后对端的自动重连被
     `events_ws_handler` 拒（403），否则点"断开"一秒后就被对端撤销
   - 任一台点「连接」= `ws::ensure_link_now`：带 `manual=1` 拨号，无视 id 规则
     （被拨号的那一侧、被拒的那一侧都因此能连回来），对端据此放行它自己的暂停
   - 配对请求同时带回来源 IP + 对端自报端口（`peerPort`），因此 id 较小的一方
     即使从未见过对端也能立刻拨号
4. 心跳 ping/pong 循环（30s）
5. 会话登记（`WsManager`）：`peer_id → 会话列表`，每个会话带 generation，
   **只有自己那份能被自己注销**；「有链接」= 列表非空、发送取最新会话。
   这样即使两端同时拨号（双方同时点「连接」）也可以共存，而不会出现
   "新会话挤掉旧会话 → 旧会话关闭 → 对方拨号循环立刻重拨"的 10Hz 连-断狂闪；
   拨号循环在**已有链接时只等待不重拨**，避免堆积无用 socket。
6. 设备状态变化（未绑定/已配对/长连接）通过 Tauri 事件同步到前端：
   `list_devices` 返回 `connected`（WS 会话在否），会话建立/断开两侧都 emit
   `devices://changed`
```

> 长连接**是自动建立的**——配对只完成 token 交换，随后由确定性规则（id 较小的一方拨号）自行建链；界面上"长连接"= WS 会话在，不是"HTTP 可达"。断裂后按指数退避自动重连；本机点「断开」才停止自动建链，任一台点「连接」即可恢复。
>
> 后台任务的启动一律走 `task::spawn`（没有 Tokio 运行时则记录并跳过）：Tauri 的**同步**命令跑在主线程，直接 `tokio::spawn` 会 panic 掉整个进程（点按钮"闪退"）。
>
> 文件日志的 `WorkerGuard` 存在静态里：它被提前 drop 就等于提前结束刷盘职责，日志文件会一直是 0 字节（出问题时没有任何线索）。
>
> 两条硬规矩：`/pair/info` 只报**实际监听端口**（端口被占会顺延到 +1..+10，报配置值等于让对端去连别的进程）；**单实例**用 `tauri-plugin-single-instance`（OS 级锁，进程退出即释放）—— 第二份进程把已在跑的实例调到前台并 emit `orb://show` 让面板展开。**绝不能**让第二份进程自己静默 `exit`：那在用户侧就是"启动一下就闪退"，而且日志都来不及落盘。

可行验证：`tests/p1_integration.rs` —— 配对后无需点击即建链、被拨号方断开后双方掉线、断开方/对端点"连接"都能恢复、对端自动重连被拒、重连后链路稳定（含两端同时点「连接」）、`/pair/info` 报实际端口、没有运行时点按钮不 panic。

### Step 3：配对

```
pair.rs + routes_pair.rs
1. pair_start 本机生成码 + 前端显示（2 分钟过期，过期自动换码）
2. B 端输入该码 → POST /pair 验证 + 生成 token + 双方存 keyring
3. 配对成功后 WS 刷新设备列表 + 由 id 较小的一方拉起长连接
4. App 内配对 UI（弹窗同时提供：显示本机码 / 输入对方码 / 手动添加设备）
```

可行验证：A 端点配对显示 6 位码，B 端输入码 → 配成；反向由 A 输入 B 的码同样配成。

### Step 4：推送

```
routes_push.rs + transfer/push_job.rs
1. POST /push/invite（发送方）→ WS push_invite（接收方）
2. 接收方弹窗 → POST /push/respond
3. 接收方 GET /push/download 流式拉取 + 落盘
4. POST /push/confirm
5. 通知发送方推送完成/失败
6. 前端推送进度条
```

可行验证：A 推送文件 → B 收到通知 → 同意 → 文件落到 B 收件目录。

### Step 5：断点续传

```
transfer/resume.rs
1. 下载中定期保存 offset
2. 中断后重启下载 → 检查 resume 记录 → 续传
3. 完成后清除记录
```

### Step 6：目录变更通知

```
share/watcher.rs（P0 扩展）
1. notify crate 监听共享项目录
2. 变化事件 → 整理成相对路径 → WS 广播给可见设备
3. 对端收到后自动刷新树节点
```

### Step 7：开机自启

```
autostart.rs
1. Tauri 命令 enable_autostart / disable_autostart
2. 设置面板 S-04 开关
```

## 14. 校验清单

```
☐ mDNS：启动后设备互相发现，设备标签出现对端（未绑定状态）
☐ 手动添加：输入 IP:port 添加跨网段设备
☐ 配对：A 显示 6 位码（2 分钟倒计时）→ B 输入 → 双向确认 → token 交换 → 设备变"已配对"
☐ 配对超时：2 分钟未配对 → 码自动失效
☐ 长连接：配对完成即自动建立 WS → 设备变"长连接"（无需点击）；「断开」→ 双方都回到"已配对"且对端自动重连被拒；任一端点「连接」→ 恢复
☐ 后台任务不 panic：同步命令（点「连接」/「同意接收推送」）触发的 spawn 在无 Tokio 运行时下只记日志
☐ 解除配对：App 内移除设备 → 双方 keyring 清理
☐ 推送邀请（长连接设备）：A 推送 → B 通过 WS 收到通知 + 弹窗
☐ 推送邀请（未绑定设备）：A 推送 → B 通过 HTTP 收到通知 + 弹窗，无需预先配对
☐ 推送同意：B 点同意 → 文件流式落盘到收件目录（未绑定走一次性 download_token）
☐ 推送拒绝：B 点拒绝 → A 收到失败提示
☐ 推送离线：B 离线 → A 收到"对方离线"提示，不排队
☐ 推送进度条：A 端实时看到传输进度
☐ 断点续传：中断后重新同意 → 从断处继续
☐ 目录变更：A 修改共享目录文件 → B 的树自动刷新
☐ WS 心跳：30s ping/pong；90s 无响应 → 标记离线
☐ 掉线重连：长连接断开后自动尝试重连（指数退避），设备状态保持"长连接"
☐ 开机自启：开关生效，重启后 App 自动启动