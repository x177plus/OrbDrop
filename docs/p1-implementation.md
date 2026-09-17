# OrbDrop P1 实现指南

> P1 目标：局域网完整双路径传输链路——发现、配对、推送、断点续传、变更通知、开机自启。P0 单机版在此基础上连成真正的双机链路。

## 1. P1 范围

| 模块 | 内容 | 涉及需求 |
|---|---|---|
| 发现 | mDNS 注册/扫描 + 手动添加 IP:端口 | F-10, F-11 |
| 配对 | 6 位码生成/验证/超时 + token 交换 + 凭据持久化 | F-12, F-15 |
| 设备列表 | 在线/离线状态、共享项摘要、中断连接、解除配对（离线行也能解除）| F-13, F-14, F-25 |
| 推送 | 邀请/同意/拒绝/流式落盘/进度/取消 | F-50~F-54 |
| 断点续传 | Range 续传 + 本地偏移记录 | F-60 |
| 变更通知 | 目录文件系统监听 → WS 推送到对端 | F-33 |
| 信令 | WebSocket 在线心跳 + 推送邀请 + 变更事件 | WS /events |
| 掉线检测 | WS ping/pong + 超时标记离线 + 链路重连 | N-05 |
| 开机自启 | 注册表 / LaunchAgent | F-06, S-04 |
| 设备状态 | 未绑定/已配对/长连接三态管理与流转 | F-13, F-16 |

**设备状态模型**（贯穿 P1 所有模块）：

```
未绑定 ──配对──▶ 已配对 ──点击"连接"／启动恢复──▶ 长连接
 │                │                                   │
 │                │──点击"断开"（断链 + 本机暂停）─────┤
 │                │    └──任一端点"连接"──────────────▶ 长连接
 │                └──解除配对──────────▶ 未绑定（仍在线则留在列表，离线则不再列出）
 └──可推送（HTTP 直达，接收需同意）
```

- **未绑定**：mDNS 发现/手动添加，无 token。可**推送**（HTTP 邀请直达，无需 WS/配对）
- **已配对**：token 交换完成。可推送；长连接**不自动建立** —— 设备行给「连接」按钮，由用户点
  （唯一例外：上次关机时它是长连接 → 启动恢复自动拨回来，见 F-16 与 §13 Step 2）
- **长连接**：已配对 + WS 实时连接（**用户点「连接」或启动恢复才建立**；mDNS 发现/探活都不拨号）。拖入文件自动广播给所有长连接设备

> 推送**不依赖**配对或长连接——邀请是普通 HTTP 请求，对端在线即可收到；接收方同意后才开始传输，同意制兜底安全。

## 2. 新增 Rust 依赖

```toml
# P0 已有 + 以下新增
uuid = { version = "1", features = ["v4"] }        # 共享项/推送 job ID
rand = "0.8"                                        # 6 位配对码生成
```

P0 依赖已覆盖：`axum`（含 WS）、`mdns-sd`、`keyring`（仅迁移老凭据）、`notify`、`tokio`——P1 不需要额外大件。

## 3. 新增 Rust 模块

```
src-tauri/src/
├── ...
├─┬ server/
│ ├── ...
│ ├── routes_push.rs          # POST /push/invite, /respond, /confirm, GET /download
│ └── routes_pair.rs          # POST /pair（配对）、POST /unpair（解除配对的另一半，Bearer/回环）
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
                    // 检查是否已配对（凭据文件里有向设备 token）
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
// 不经过 mDNS，直接检查凭据文件里是否有该设备的 token
// 有 → 建立 WS 连接；无 → 触发配对流程
```

### 在线状态判定（F-13）

mDNS 只在对方主动告别或记录过期（约 2 分钟）时报告离开，且同名实例重启后浏览器未必重新解析，
所以在线状态由三条信号共同决定：

| 信号 | 触发 | 语义 |
|---|---|---|
| mDNS `ServiceResolved` | 对方注册/更新 | 立即标在线（并补全 ip/port），并收拾同一地址上的旧身份（见下） |
| WS 长连接建立/断开 | 已配对设备 | 权威信号：连上即在线，断开即离线 |
| `/pair/info` 探测（每 5s，1.5s 超时） | 上面两条之外的兜底 | **回话里的 id 是这台设备**才算在线；地址上换了别的 id 则按"旧身份"处置 |

前端只渲染 `online` 设备，因此在对方退出后数秒内该行从列表消失；地址簿与凭据保留，
对方再上线会自动重新出现（不产生重复行）。

**设备 id 必须稳定**：`deviceId` 是地址簿与凭据的键。配置里所有其他字段都带 serde 默认值，
缺字段（旧版本配置/手工裁剪）只补默认值，绝不整份重置 —— 否则每次启动都会换一个 id，
对端就会为"同一台设备"多出一行，并让旧行上的配对码失效。

**设备 id 变了怎么办（去重的正题）**：id 是**唯一**的去重键 —— 不看名字、不看 ip（同名不同机、
同机多实例都真实存在）。用户清一次数据目录（`config.toml` 没了）就会换一个 id，而对端
手里那行还指着同一个 ip:port。两条规则收口：

- 探活与 mDNS 发现都必须核对**身份**：`/pair/info` 回的 id 不等于这一行记的 id，就说明
  这个地址上已经不是它了。只按"有人答话"判在线的话，旧行会永远在线 —— 列表里于是
  出现"两个同名设备，一个已绑定、一个未绑定"，且旧行永不消失。
- 核对之后按 `/pair/info` 的另一半回话（`pairedAs`：对端把本机带过去的 token 认作谁）分两路：
  - 认得出（= `pairedAs` 是本机 id）→ 对端只清了**配置**（device id 换了）而配对凭据还在：
    这一行**迁到新 id**，凭据的键一起换（`credential::rekey_peer`，token 值不变），配对状态与
    "长连接"记忆都跟着走，列表里只剩一行。
    （凭据文件与 `config.toml` 同在数据目录：只删/丢掉配置文件时凭据还在，正是这条路；
    整个数据目录一起清掉则是彻底重置，两端都得重新配对。）
  - 认不出 → 关系真断了：旧行只作废地址、标离线（**不动**凭据与配对记录，对端拿原 id
    回来还能用），列表里剩下的是 mDNS 发现的新身份。

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
              │  生成 token → 存凭据文件（键 = B.id）
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
- `credential::set_peer(&B.id, &token)` → A 的凭据文件
- 响应返回 token → B 收到 → `credential::set_peer(&A.id, &token)` → B 的凭据文件
- 两端各自 `mark_paired`；长连接**不在这里建** —— 用户点「连接」时才拨号（`ws::ensure_link_now`），
  或是下次启动时按上次的记忆恢复（`ws::restore_links_on_startup`）
- 后续所有 HTTP 请求 `Authorization: Bearer <token>` / WebDAV `Basic base64("OrbDrop:<token>")`（用户名不参与校验，只作发起方标记；服务端取第一个冒号之后的部分当 token）

### 凭据文件（`%LOCALAPPDATA%\OrbDrop\credentials.dat`）

全部已配对设备的 token 是**一份映射、一个文件**（P0 起它曾放在系统凭据库的单条凭据里；
那次搬家是为了绕开 Windows 单条凭据 **2560 字节上限**——按真实尺寸约 11 台设备就写不进去、
配对会直接 500——顺便让"一键清空"变成一个删除动作，而不是平台相关的枚举代码）。

```text
<一整块 base64：每 76 字符换行，没有空行、没有版本头、没有署名>
```

内层明文（**混淆前**的那份内容，文件里看不到）才是真正的结构：

```text
ODBF 2                                   ← 魔数 + 十进制版本（版本按文本读，不按宽度）
{"<device_id>":"<token>",…}              ← 裸 JSON
# 177plus                               ← 尾署名，`#` 开头的行都是注释
```

- **这是遮挡，不是加密**：文件 = 内层明文按固定密钥（`credential::SCRAMBLE_KEY`，就写在源码里）
  逐字节异或、再 base64、每 76 字符换行。于是打开文件看不到版本头、看不到署名，也不是
  "标准 base64 一解就是 JSON"（解出来是异或后的字节）。**但仅此而已**：密钥在源码里、算法是
  异或，任何人拿到文件都能解回明文 —— 别把它当保护，也别在它上面继续叠"更复杂的"假加密
  （零保护，却会让后来的人以为它有保护）。真正要加密只能引入密钥保管
  （Windows DPAPI / macOS Keychain），本版不引。
- **老格式（v1）只读兼容**：`ODBF <版本>` 头 + base64 正文 + `# 177plus` 署名 = `FORMAT_VERSION 1`。
  启动时（`migrate_legacy_entry` 之后）调一次 `credential::upgrade_vault_format()` 就地改写成 v2；
  改不动只 warn（读兼容一直在，下次启动重来）。**写一律 v2** —— 于是升级是单向的：用回旧版本会
  读不到（旧版只认 v1 形状，会报"缺少格式头"）。版本闸门两种格式共用，比 `FORMAT_VERSION` 新的
  格式明确拒绝（"请升级 OrbDrop"），不瞎猜。
- **老数据自动迁移**：启动时（`run()` 里、`AppState::new` 之前）调一次
  `credential::migrate_legacy_entry()`，把系统凭据库里那条读出来写进文件，**写成功之后**才删那条
  （先删后写、中间崩一次就永久丢配对）。迁移是**单向**的：用回旧版本会读不到（旧版只认凭据库）。
- **读路径不改任何东西**：文件不在时只**只读**地回落到老条目。曾经把迁移挂在"读一次凭据"上，
  结果跑一次 `cargo test` 就把开发机真实的配对搬走并删掉了老条目 —— 读一个字节都不该改用户的东西。
- 单测（lib 内）一律走临时 vault 文件；集成测试靠 `ORBDROP_DATA_DIR` 指到临时目录
  （`config::data_dir` 的唯一出口，顺带给便携部署用）。

**凭据模块的两条硬规矩**（都是踩过才写下的）：

1. **读也要和写串行**。整份映射是**一个**文件，所有写都是"读-改-写"；不加锁地读会在别人
   改写的当口读到半截（原子写让"半截"变成"读到旧版本"，但读-改-写丢更新的问题还在）——
   表现为"刚配对成功的 token 查不到"（`has_peer` 突然说没有这台设备）。
   `credential::load_peers` 与写路径共用 `STORE_LOCK`（写路径内部用不取锁的
   `read_map_unlocked`，避免 parking_lot 互斥锁不可重入）。
2. **"读不到" ≠ "没有"**。凭据存储的瞬时故障（文件被占、同机多实例并发）不是语义事实：
   - 拨号循环里读失败只算"这次尝试失败"（退避重试），**不能**当成"没 token"退出 ——
     那会让重启恢复永久停在这个设备上，且不报错；
   - 探活/身份核对里读失败**什么都不做**（跳过这一轮）：带着错的前提去问，会得到
     一个"它不认我们"的假结论，进而把一条有效的行当垃圾处置（作废地址）。
   同理，凭据文件解析失败（被截断、被别的工具改过）**必须报错**，不能当成空映射 ——
   那会让下一次写回把别人的 token 抹掉。

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
│                                      │ 流式写入下载目录 │
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
    local_path: PathBuf,       // 接收方下载目录下的实际路径
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
//         写入下载目录/{share_name}/{path}
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
//     │ 保存到：下载目录                  │
//     │                                 │
//     │    [拒绝]        [接收]          │
//     └─────────────────────────────────┘
//   - 超时 60 秒无操作 → 自动拒绝
```

## 8. 断点续传（F-60）

```rust
// transfer/resume.rs
// 下载偏移记录文件：%LOCALAPPDATA%/OrbDrop/resume.json 或 ~/Library/Application Support/OrbDrop/resume.json

struct ResumeRecord {
    invite_id: String,
    file_path: String,     // 相对于下载目录的路径
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

**拨号重试策略（`ws::spawn_outbound_link`，实测踩过 10060 才定的）**：

| 触发拨号（证据） | 节奏 | 何时停 |
|---|---|---|
| 用户点「连接」（`ensure_link_now`） | 不限次，5→60s 封顶退避（带 ±20% 抖动） | 用户点「断开」 |
| 启动恢复（`restore_links_on_startup`）/ 身份迁移接回（`restore_link`） | 先连试 **5 次**（5/10/20/40/60s），之后**待命** | 见右侧两行 |
| **对端重新出现在网上**（探活离线→在线、mDNS 重新发现 → `redial_if_remembered`） | 同上预算，秒级触发（不用等 15s 轮询） | — |
| 链路刚断（几秒前还连着） | 失败计数清零，重新享受完整预算 | — |
| 探活判离线且预算用尽 | **待命**（`LinkState::Waiting`）：每 15s 看一次在线状态，不发 TCP | 对端一上线立即恢复 |

**停止条件只有明确拒绝**（`stop_reason_for`）：403 = 对端点过「断开」→ `Stopped{PeerDisconnected}`；401 = 它不认我们的 token（清过数据/已解除配对）→ `Stopped{TokenRejected}`；本机点「断开」→ `Stopped{LocalDisconnected}`。网络类错误（连接被拒/超时/DNS/reset）**不算停手**，继续退避重试；凭据文件读失败也算"这次尝试失败"（瞬时故障不能让循环永久退出 —— 那正是"重启后接不回来还不报错"的成因）。

**为什么这样最好（四条不变量）**：

1. **不会死锁**：门控用的探活是双向的（`/pair/info` 公开端点，双方各探各的），所以"双方同时重启 / DHCP 换地址 / 防火墙授权窗口过了"这些场景里，只要双方 App 都开着，至少一侧会看到对方并动手；加上"谁没忘这次长连接谁拨"（记忆双向写入），不需要无限死磕。
2. **不对空气发 TCP**：对端关机的那些分钟里每次拨号都要等满 TCP 超时（Windows 上就是 `os error 10060`），现在压成"每 15s 一次内存里的在线标记检查"。
3. **不会静默失败**：每个状态都能自证，并且**透出到界面**——`list_devices` 带 `linkState`（`connected|retrying|waiting|stopped{reason}`），设备行显示"重连中… / 等对端上线 / 对端已断开 / 需重新配对 / 已断开"，悬停给出下一步该做什么。
4. **资源有界**：每设备一个常驻任务、稳态一分钟一次连接尝试封顶、日志前 5 次 warn 之后降级 debug。

> 反例（都不要）：无限重试 = 对空气发 TCP + 日志噪音；硬上限后彻底不管 = 上面第 1 条的三个场景变成静默死胡同；"谁 device id 小谁拨" = 项目已明确拒绝（F-16），且双拨已被多会话支持消化。

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
| 重连尝试 | 指数退避 5-60s | 用户点过「连接」的链路断开后重连；对端处于「断开」暂停时被拒 |
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
3. 长连接**只由用户点「连接」或启动恢复建立**（F-16）：配对只确立"已配对"关系，不碰链路。
   - 配对完成、mDNS 发现、5s 探活 —— 一律**不拨号**（前两条只补齐地址/在线状态）
   - 任一台点「连接」= `ws::ensure_link_now`：带 `manual=1` 拨号；被拨号的那一侧、
     两端同时点、对端点过「断开」，都能靠这条路径连回来
   - **启动恢复** = `ws::restore_links_on_startup`（`lib::setup_server_task` 里调用）：
     地址簿里 `paired && autoConnect` 的设备逐个拨回来，**不带** `manual=1`（本机自己的
     记忆，不替对端解除它的「断开」）。地址还没到就等——拨号循环拿到地址（mDNS/探活）
     再接上，所以对端晚上线也没关系
   - 记忆的落点是地址簿条目 `PeerDevice::auto_connect`：链路建立时 `set_auto_connect(true)`
     （出站会话在 `spawn_outbound_link`、入站会话在 `run_inbound_session`，两侧对称），
     用户点「断开」时 `disconnect_peer` 清零。**写点在链路建立那一刻而不是退出时** ——
     强杀进程/断电/任务管理器结束都不会漏掉这次长连接；反之点过「断开」的设备重启后
     不会自己连回来
   - 「断开」= 断链 + 本机暂停（`WsManager::pause`）：**连接由单任务持有**，
     丢掉事件发送端即关掉 socket，对端立刻看到掉线；此后对端那个拨号循环的重拨被
     `events_ws_handler` 拒（403），否则点"断开"一秒后就被对端撤销
   - 配对请求仍带回来源 IP + 对端自报端口（`peerPort`）：地址先补齐，
     所以即使从未见过对端，点「连接」也能立刻拨出去
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

> 长连接的入口只有两个：**用户点「连接」**、以及**启动时恢复上次的长连接**——配对只完成 token 交换、把设备变"已配对"，不碰链路，没有"id 较小的一方自动拨号"，也不会因为 mDNS 发现/5s 探活就后台拉起。界面上"长连接"= WS 会话在，不是"HTTP 可达"。链路断了之后，当初点「连接」的拨号循环按指数退避继续重连（用户要的会话该被保住）；本机点「断开」则连它一起停掉、并拒掉对端重拨，任一台点「连接」即可恢复。
>
> 后台任务的启动一律走 `task::spawn`（没有 Tokio 运行时则记录并跳过）：Tauri 的**同步**命令跑在主线程，直接 `tokio::spawn` 会 panic 掉整个进程（点按钮"闪退"）。
>
> 文件日志的 `WorkerGuard` 存在静态里：它被提前 drop 就等于提前结束刷盘职责，日志文件会一直是 0 字节（出问题时没有任何线索）。
>
> 两条硬规矩：`/pair/info` 只报**实际监听端口**（端口被占会顺延到 +1..+10，报配置值等于让对端去连别的进程）；**单实例**用 `tauri-plugin-single-instance`（OS 级锁，进程退出即释放）—— 第二份进程把已在跑的实例调到前台并 emit `orb://show` 让面板展开。**绝不能**让第二份进程自己静默 `exit`：那在用户侧就是"启动一下就闪退"，而且日志都来不及落盘。
>
> **长连接必须带能反解出身份的 token**：`/events` 没有"本机还没配对过任何设备就放行"的后门 —— 那个后门会让刚被解除配对的对端拿旧 token 连上、还被登记成本机的 `self`（对端显示"长连接"，本机却把它当"未绑定"）。旧版对端本地仍存着 token 时，本机一律 401 拒绝它的 WS 与 HTTP。

可行验证：`tests/p1_integration.rs` —— 配对后（地址/token 都齐）**不许**自己建链、点「连接」才建链、被拨号方断开后双方掉线、断开方/对端点"连接"都能恢复、对端重拨被拒、重连后链路稳定（含两端同时点「连接」）、**重启后按磁盘记忆自动接回长连接（点过「断开」的不接）**、**对端当时没上线时自动恢复先失败、对端上线后不点任何按钮自己接回**、`/pair/info` 报实际端口、没有运行时点按钮不 panic。重试预算的边界（用户点「连接」不受限 / 自动够 5 次且对端不在线则待命 / 对端一上线即放行）由 `ws::tests::auto_dial_backs_off_once_the_budget_is_spent_and_the_peer_is_absent` 钉住。

### Step 3：配对

```
pair.rs + routes_pair.rs
1. pair_start 本机生成码 + 前端显示（2 分钟过期，过期自动换码）
2. B 端输入该码 → POST /pair 验证 + 生成 token + 双方存凭据文件
3. 配对成功后刷新设备列表（设备变"已配对"；长连接由用户点「连接」建立）
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

可行验证：A 推送文件 → B 收到通知 → 同意 → 文件落到 B 下载目录。

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
☐ 长连接：配对**不**建链（设备停在"已配对"），点「连接」才建立 WS → 设备变"长连接"；「断开」→ 双方都回到"已配对"且对端重拨被拒；任一端点「连接」→ 恢复
☐ 重启恢复：处于长连接时关掉 App（两端任一为长连接即可）→ 重新启动 → 链接自己接回来（设备行回到"长连接"，不用再点「连接」）；点过「断开」的设备重启后仍是"已配对"
☐ 重启恢复（对端也重启）：两端都关掉再先后启动 → 先起的那台自动拨、后起的接上，两端都回到"长连接"
☐ 后台任务不 panic：同步命令（点「连接」/「同意接收推送」）触发的 spawn 在无 Tokio 运行时下只记日志
☐ 解除配对：任一端解除 → 双方凭据都清、长连接断开；两端设备行都保留为"未绑定"（仍在线）或留在列表下方的"离线"行（对方离线时），可再次配对（HTTP `POST /unpair`，不依赖长连接）
☐ 解除配对（对端离线）：把对端进程/机器停掉 → 本机仍能对那台设备点「解除配对」（本机动作为主，`POST /unpair` 通知失败只记一条 warn）→ 本机凭据已清、行消失或回到"未绑定"；对端回来时拿旧 token 拨号被 401 拒（显示"需重新配对"）
☐ 设备列表排序：在线在前（长连接 → 已配对 → 未绑定）、已配对但离线的排在最下方（行内只有「解除配对」）
 需配对（token 失效）：该行给「配对」（右键「重新配对」）而不是「连接」—— 拨号必然 401；走完配对后行回到「已配对」+「连接」，不再停在「需配对」
☐ 推送邀请（长连接设备）：A 推送 → B 通过 WS 收到通知 + 弹窗
☐ 推送邀请（未绑定设备）：A 推送 → B 通过 HTTP 收到通知 + 弹窗，无需预先配对
☐ 推送同意：B 点同意 → 文件流式落盘到下载目录（未绑定走一次性 download_token）
☐ 推送拒绝：B 点拒绝 → A 收到失败提示
☐ 推送离线：B 离线 → A 收到"对方离线"提示，不排队
☐ 推送进度条：A 端实时看到传输进度
☐ 断点续传：中断后重新同意 → 从断处继续
☐ 目录变更：A 修改共享目录文件 → B 的树自动刷新
☐ WS 心跳：30s ping/pong；90s 无响应 → 标记离线
☐ 掉线重连：长连接断开后自动尝试重连（指数退避 5→60s），设备状态保持"长连接"
☐ 对端未开机：自动恢复先试 5 次（5/10/20/40/60s，日志各一行 warn），随后转入待命（每 15s 看一次在线，不再发 TCP、不再刷 warn）；对端一上线（探活/mDNS）或自己启动拨过来 → 自动接回，无需点「连接」
☐ 明确拒绝即停手：对端点过「断开」→ 本机不再重拨（设备行显示"对端已断开"），它点「连接」拨过来或本机点「连接」即恢复；对端不认 token → 显示"需重新配对"，不再重拨
☐ 链路状态可见：设备行在没连着时能看出"重连中…/等对端上线/对端已断开/需重新配对/已断开"，悬停给出下一步（排障时不再只看到"已配对"）
☐ 开机自启：开关生效，重启后 App 自动启动
☐ 拖拽下载：树里任意文件/目录拖到资源管理器目录 → 落在该目录（拖目录含子目录与文件）；
   源文件/共享项不受影响（是复制不是移动）；对端项起拖前只走一遍元数据，内容在落下时直接写进目标目录（本机无暂存副本）
```

## 15. 拖拽下载（F-46）

把树里（本机共享项 / 对端共享项）的**任意文件或目录**拖到资源管理器/Finder 的某个目录里，
内容直接落在那个目录；拖目录 = 整个目录含子目录与文件；落下的动作**只可能是复制**。

### 为什么必须是原生拖放

网页里的 HTML5 拖拽（`draggable` / `dragstart`）出不了 webview：目的地（资源管理器）是
**另一个进程**，只有 OS 的原生拖放跨得过去。所以前端只负责**认手势**（`src/lib/dragOut.ts`
只做"按下 + 移动过阈值"的判定），起拖由 Rust 侧发起：`DoDragDrop`（`src-tauri/src/drag.rs`）。

### 两种内容，两条路

| 源 | 数据格式 | 内容从哪来 |
|---|---|---|
| 本机共享项 | `CF_HDROP` | 磁盘上的真实路径，资源管理器直接复制 |
| 对端共享项 | `CFSTR_FILEDESCRIPTOR` + `CFSTR_FILECONTENTS`（虚拟文件） | 自实现的 `IStream`，**落下那一刻**才按需从对端拉（`GET /file?offset=&len=`） |

对端这条路**不落暂存盘**：起拖前只走一遍**元数据**（`/tree` 递归 + 单个文件走 `/stat`），
把名字/大小/修改时间填进 `FILEGROUPDESCRIPTORW`；资源管理器在落下时把文件**直接写进拖到的
目录**，字节在复制过程中才流过去。所以拖一个 10GB 的目录也是秒起拖，也不会有"先下到
`%LOCALAPPDATA%` 再转存一次"。

> 早期版本是"先递归下到暂存目录、再拖副本"，为的是绕开"跨进程编组拿不了自定义 `IStream`"
> 的顾虑。**实测这个顾虑不成立**（探针：真手势 + 真资源管理器，内容直接写进目标目录），
> 所以改成了虚拟文件。

### 时序（为什么分两步）

拖放循环跟着**按住的主键**走，所以是"先备齐、再起拖"：

1. 前端：按下 + 移动过 6px → `prepare_drag_out` 登记票据；对端项同时开始走目录（元数据）。
2. 前端：`start_drag_out` → 等元数据备齐（本机项立即就有）→ 确认主键还按着 →
   `run_on_main_thread` 里跑 `DoDragDrop`（它自带模态循环，阻塞主线程直到拖放结束）。
3. 落下之后票据就丢；`Read` 失败（对端掉了）会让资源管理器报复制失败，不做重试。

### 实测踩到的坑（别按直觉改）

- **`formatetc.tymed` 是位掩码**，不是枚举：资源管理器要文件内容时给的是
  `13`（`ISTREAM | ISTORAGE | HGLOBAL`）。按等号判会把这种请求全部打回 `DV_E_FORMATETC`，
  表现是"目录建出来了、文件一个都没复制"。
- **枚举表里列了什么，`GetData` 就必须给得出来**：目录项也要在 `CFSTR_FILECONTENTS` 里
  出现，并且要给一个**空流**（内容为 0 字节的 `IStream`）—— 只列不交付会让协商卡住。
- **`QueryContinueDrag` 的返回值**：Esc → `DRAGDROP_S_CANCEL`；主键松开 → `DRAGDROP_S_DROP`；
  否则 `S_OK`。一直回 `S_OK` 时循环**不会结束**（主线程卡住）；而一进循环就回
  `DRAGDROP_S_DROP`（键状态没读到按下）则当场"落下"、什么都不发生 —— 判据是
  `grfKeyState` 里的 `MK_LBUTTON`，它是**本线程消息队列**里的键状态。
- **`DoDragDrop` 跑在哪个线程很关键**：必须在**窗口所在线程**（本应用＝Tauri 主线程）
  且该窗口是**前台窗口**。实测在 tokio 工作线程上、或前台窗口是别的进程时起拖，
  拖放循环一动不动（连一次 `QueryContinueDrag` 都收不到）。
- **只给 `DROPEFFECT_COPY` 还不够**：同一块盘内拖拽的默认动作是"移动"，所以数据对象同时
  提供 `Preferred DropEffect` = COPY，资源管理器才会复制而不是把源文件搬走。
- **`GMEM_MOVEABLE`**：`CF_HDROP`/描述符这两块 HGLOBAL 必须是可移动内存，跨进程编组的要求。
- 落下后资源管理器会调 `SetData`（`DropDescription` / `DisableDragText` …）并忽略失败，
  源侧一律回 `E_NOTIMPL`，复制本身照常完成（实测）。
- **`prepare_drag_out` 必须是 async 命令**：对端项要 `task::spawn` 一个走目录的任务，而 Tauri
  的同步命令跑在主线程（那里没有 Tokio reactor），`task::spawn` 只会记一行
  "no tokio runtime here; task skipped" 就跳过（同 `connect_peer` 的注释）。同时
  `DragStore::prepare` 在 spawn 失败时当场把票据判失败，让界面立刻看到原因。
- **`/shares` 的 `isDir` 必须是真元数据**，不能拿"名字里有没有点"猜：没有扩展名的**文件**
  被报成目录后，对端一点开就是 `Path is not a directory`，拖拽也会被当成目录走。
  （同一类猜测在 `LocalTab.isDir` 里还有一份，只影响本机列表的图标/展开，不影响拖拽：
  本机项拖的是真实路径，不看这个标志。）
- **别信前端带来的 `is_dir`**：它是别处的判断（对端 `/shares`、树里那条记录），可能过时。
  起拖前真去问一次对端（`/stat`），以对端为准；对端说"不是目录"而我们在走目录时，
  退化成"拖这一个文件"而不是把用户卡在一句错误上。
- **对端版本可能旧**（没有 `/stat`）：那就退回"列父目录再挑出那一条"（`/tree` 哪个版本都有）
  拿大小/时间/是否目录。两条都问不到时：共享项根按调用方的判断走、大小未知
  （描述符里不写 `FD_FILESIZE`，内容照样读到流末尾）；**非根路径则判失败** ——
  不能被越界路径（`../x`）骗过去，否则拖拽"看着成功"、复制到一半才报错。
- 非 Windows 还没有原生拖放源（macOS 要走 `NSDraggingSession`），这时接口返回明确错误。

### 服务端

拖单个**文件**时 `FILEDESCRIPTOR` 要报出它的大小与修改时间，而 `/tree` 只列目录、
对"共享项根就是一个文件"的情况直接回 400 —— 所以补了 `GET /stat?share=&path=`
（协议草案里本来就有这个端点），返回 `{name, isDir, size, modified}`。

### 校验

```bash
cargo test --offline drag::                 # CF_HDROP 字节布局、描述符布局、数据对象接口面、路径/名字
cargo test --offline --test drag_virtual    # 对端走目录（真 HTTP）：清单结构、单个文件走 /stat、越界拒绝
```

> 端到端的"拖到资源管理器"是 OS 的模态循环，只能合成鼠标输入来跑（本次改动就是这么验的：
> 真手势 + 真资源管理器 + 真服务端，目录树连同子目录与文件内容全部落到目标目录，
> 源目录不动、`%LOCALAPPDATA%` 下没有任何暂存）。
