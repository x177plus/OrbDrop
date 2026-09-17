# OrbDrop × BeyondCompare（右键 BC 集成）

> 版本：2026-09-16　状态：**读路径已端到端验证**（BC 5.2 实测）；写回（PUT/DELETE/MKCOL）已实现但尚未用 BC 实际操作验证。
> 本文记的是"靠实验才逼出来"的事实，改动前先读第 1 节，别按直觉写。

## 1. BC 侧的事实（全部实测）

### 1.1 位置语法：只有 profile 形式可用

| 写法 | BC 反应 |
|---|---|
| `profile:<profile名>` | ✅ 打开该 profile 的 BaseURL |
| `profile:<profile名>?<子路径>` | ✅ BaseURL + 子路径（**`?` 是唯一有效的分隔符**） |
| `profile:<名>:/子路径`、`profile:<名>/子路径`、`\|`、`!`、空格 |  `配置文件找不到` |
| `http://x:token@IP:port/dav/…`（裸 URL，profile 存在也无效） | ❌ 只发 HEAD/GET，报**不能加载基文件夹**；BC 从不发 PROPFIND |
| `webdav://…`、`dav://…`、`http://profile名…` | ❌ `不被支持的配置文件类型` / 无效 URL（`webdav` 不是合法类型 token） |
| `ftp://x:token@host/…`、`sftp://…` | ✅ 类型合法，BC 真的去连（**所以"绕开 profile"只能靠 FTP，不是 WebDAV**） |

即：**WebDAV 位置必须由 profile 承载，用户名/密码也就必须存进 profile**。

**用户名是我们自己挑的：`OrbDrop`**（`bc::DAV_USER`）。服务端不校验它（`auth.rs` 只取 Basic 第一个冒号之后的部分当 token），挑成应用名有两个用处：
① BC 的 profile 列表 / 对端 HTTP 侧的 Basic 账号记录里认得出"这条连接是 OrbDrop 发起的"（`auth.rs` 认到 Basic 认证成功会把发起方写进日志，每台设备每进程一行）；
② 它同时是**我们自己的条目标记** —— 清理只删带这个用户名的条目（见下）。

### 1.2 profile 的生命周期：谁写、谁删

- **写**：应用启动、配对完成、点比较时（`provision_profiles` / `resolve_side`），幂等刷新（内容一致就不动文件）。
- **删**：只在**关系结束**时删 —— 解除配对（本机解除 / 对端通知解除，两条路径都会跑一遍 `provision_profiles`），以及**地址漂移**后留下的旧名字（名字 = `IP:端口`，换了 IP/端口那条就永远连不上）。
  判定"是我们的"只看 `Username == "OrbDrop"`，用户手建的 WebDAV/FTP/S3 profile（哪怕名字也长成 `IP:端口`）一个都不动；没有可删条目时一个字都不写。
- **"比较结束就删"做不到，也不该做**：
  1. **没有"比较结束"这个信号**：`launch_compare` 拉起 `BCompare.exe` 就返回（BC 是单实例，我们起的那个进程把参数交给已在运行的实例后立刻退出），比较什么时候结束我们根本不知道；
  2. **BC 的历史/标签页还在用这条位置**：BC 的"最近比较"保存的就是 `profile:<IP:端口>?<共享项id>/<路径>`，条目一删，用户再点一次历史记录就变成**弹身份验证/配置文件找不到**（§1.5 的实测：该实例看不见这个条目就走认证流程）；
  3. **删了也收不回权限**：profile 里只是一份 token 的副本，真正的闸门是"长连接 + 广播开关 + token 有效"（`Caller::sees_share`）。要真正断开，正确动作是**解除配对**（token 作废）——那时清理才有意义，也才是我们真正实现的那一条。

### 1.3 profile 文件

`BCProfiles.xml`，位置：

- **便携版：BC exe 同目录**（判断依据：同目录存在 `BCState.xml` / `BCPreferences.xml` / `BCProfiles.xml` 之一）
- 常规安装：`%APPDATA%\Scooter Software\Beyond Compare 5\`

结构（照抄 BC 自己写出来的即可）：

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!-- Produced by Beyond Compare 5 from Scooter Software -->
<BCProfiles Encryption="BCmp|1|<32 字节密钥 base64>" Version="2" MinVersion="2">
	<TWebDAVProfile Value="192.168.1.4:3890">
		<BaseURL Value="http://192.168.1.4:3890/dav/"/>
		<Password Value="BCmp|1|<base64(IV16 ‖ 密文 ‖ tag16)>"/>
		<Username Value="x"/>
	</TWebDAVProfile>
</BCProfiles>
```

写这个文件的两条铁律：

1. **文件级密钥不能换**：换了会让 BC 已存在的其它 profile 密码解不开。已有 `Encryption` 就沿用它，没有才生成。
2. **其它 profile 原样保留**：用户自己的 FTP/S3/别的 WebDAV 条目一个字都不该动。

### 1.4 密码信封 = AES-256-GCM

- 密钥：根节点 `Encryption="BCmp|1|<base64(32 字节)>"`。
- 值：`BCmp|1|<base64(IV(16 字节) ‖ AES-256-GCM 密文 ‖ tag(16 字节))>`。
- **明文密码会被 BC 直接丢弃**（实测：写 `<Password Value="tok123"/>`，BC 下次写回时那行就没了）。
- 互通性证据：用本机 BC 自己写出的"密钥 + 密文"样本能解出明文（`bc.rs` 里有该 fixture 测试），说明我们的实现与 BC 对称一致。

### 1.5 BC 什么时候读 `BCProfiles.xml`（实测，容易想错）

- **运行中的 BC 是跟着文件实时走的**：BC 启动之后才写进去的 profile，下一次用 `profile:<名字>`
  位置时立刻就能解析（实测：`OrbDropMechA`~`D2` 全部在 BC 运行期间写入，随后被正确解析并带着
  凭据访问到服务端；其中 D2 甚至是在用户打开过 `工具 ▸ Profiles` 之后才写的）。
  所以"打开配置文件管理器会把 profile 集合固化、之后不认文件改动"这个猜测是**错的**。
- 但 **BC 只在进程启动时"登记"这个文件**：如果 BC 启动时 `BCProfiles.xml` 还不存在
  （应用第一次运行才创建它、或用户把它删了），那个实例就再也不看它，之后所有 `profile:`
  位置都被当成未知远端位置——表现为**弹身份验证**（BC 会把这个位置当"需要登录的远端位置"）。
  实测证实：在 BC 运行期间把文件删掉再重建，随后用新 profile 打开比较 → **弹身份验证**；
  重启 BC 后同样的比较 → **直接打开**。这也解释了实机上"应用刚写好 profile、点比较却弹认证"
  的现象（用户那个 BC 实例是几天前起的，当时这个文件根本不存在）。

⇒ **对策（已实现）**：`bc::ensure_profile_file()` + `bc::provision_profiles()` 在**应用启动时**
和**配对成功时**就把 profile 文件（含所有已配对设备）预写好，而不是等用户点比较那一刻才创建——
这样无论 BC 何时启动，文件都先在了。剩下的唯一窗口是"用户的 BC 比本应用第一次写文件还早"，
那种情况需要**重启一次 BC**（一次性）。

### 1.6 BC 对我们的 DAV 端点做了什么（请求序列）

1. `HEAD` / `OPTIONS` **先不带凭据**发一次 → 服务端必须回 `401 + WWW-Authenticate: Basic`，BC 才会带 profile 里的账号密码重试（用 WinHTTP + 系统凭据 UI，UA 形如 `BeyondCompare/5.2 SecureBlackBox/24.0.9430.0`）。
2. `OPTIONS`（要 `DAV: 1,2`）→ **不能被 CORS 层抢答**（见 2.2）。
3. `PROPFIND Depth: 0`，路径**不带**尾斜杠（判断目标是文件还是目录）。
4. `PROPFIND Depth: 1`，路径**带**尾斜杠（列一层）。请求体固定索要这些属性：
   `creationdate, displayname, getcontentlanguage, getcontentlength, getcontenttype, getetag, getlastmodified, lockdiscovery, resourcetype, supportedlock`
   （返回其中子集即可，BC 不挑；`lockdiscovery`/`supportedlock` 可以不给。）
5. 目录的 `GET` **不能回 405**：BC 会用它探目录，405 会让它直接判定"加载失败"。
6. 目录是**惰性**逐层 PROPFIND，不需要递归 listing；`Depth: infinity` 可以按 1 处理。

## 2. OrbDrop 侧实现

### 2.1 Rust

- `src-tauri/src/bc.rs`
  - `locate_exe()`：设置项 → Windows 注册表（`App Paths\BCompare.exe`、`Scooter Software\Beyond Compare 5\ExePath`，便携版脚本也写这里）→ 常见安装路径；macOS 走 `/Applications/Beyond Compare.app/Contents/MacOS/bcompare`。
  - `config_dir()`：见 1.3 的判断规则。
  - `ensure_profile_file()` / `provision_profiles()`：**预写**（见 1.5）——应用启动、配对完成时各跑一次；没有已配对设备时也要把骨架文件立起来，好让 BC 下次启动把它纳入跟踪。
  - `encrypt_password()/decrypt_password()`：见 1.4。
  - `upsert_webdav_profile()`：合并式写入（保留别人的 profile 与文件级密钥），内容已一致时**不重写**（避免平白让 BC 写 `.bak`）。
  - `prune_webdav_profiles()`：清理**我们自己写下、但已失效**的条目（解除配对后 / 地址漂移的旧名字），见 1.2。
  - `profile_spec()`：`profile:<ip:port>` / `profile:<ip:port>?<shareId>/<子路径>`。
  - `resolve_side()`：本机共享项 → 本地绝对路径；对端共享项 → 要求**长连接** + 取出该设备 token → 写 profile → 返回 spec。
- 配置：`Config.bc_exe_path`（留空=自动探测）、`Config.bc_left` / `Config.bc_right`（"添加到左侧/右侧比较位"，跨重启保留）。
- Tauri 命令：`bc_status` / `bc_set_side` / `bc_clear_side` / `bc_compare`。
- DAV 端点：`src-tauri/src/server/routes_dav.rs`，命名空间 `/dav/<shareId>/<相对路径>`，方法 `OPTIONS/GET/HEAD/PUT/DELETE/MKCOL/MOVE/COPY/PROPFIND/PROPPATCH`，可见性复用 `Caller::sees_share`（不可见与不存在都回 404），路径逃逸防护走"逐级向上 canonicalize"（PUT/MKCOL 的目标可以还不存在）。

### 2.2 两个必须记住的坑

1. **DAV 路由不能挂 CORS 层**。tower-http 的 CORS 会把 `OPTIONS` 直接抢答成 preflight 响应，而 BC 正是用 `OPTIONS` 协商 DAV 能力——被抢答 = BC 认为"这不是 WebDAV"，远端目录直接加载不了。所以 `build_router` 里 REST 走 CORS、DAV 单独一棵树。
2. **带尾斜杠的路由要单独注册**：BC 对目录用 `/dav/<shareId>/`，只注册 `/dav/{share_id}` 会让目录级请求 404。

## 3. 验证记录

- `cargo test`：25 个单元测试（含 BC 加密互通 fixture、profile 合并/幂等/清理、Basic 凭据拆分、spec 语法、DAV 方法/可见性/非回环 401 挑战/局域网 Basic 认证）+ 7 个 P1 集成（含解除配对清理 profile、重启恢复长连接）+ 4 个共享可见性，全绿。
  - 注：DAV 测试各自持有**独立端口段**（4100/4110/…）。都从同一基址往上探会在并行跑时互相抢端口，表现为偶发 `IncompleteMessage`。
- 端到端冒烟（真机 BC 5.2，脚本驱动，非 GUI）：
  - spec `profile:127.0.0.1:3997?<shareId>/共享 目录`（含中文与空格）
  - BC 报告：左边基点 `http://127.0.0.1:3997/dav/<shareId>/共享 目录`，左边独有 `remote-only.txt`、右边独有 `local-only.txt` —— 真实比较结果
  - profile 由产品代码写入（自算密钥 + 自加密密码），BC 接受

## 4. 未完成 / 风险

| 项 | 说明 |
|---|---|
| 写回未实测 | PUT/DELETE/MKCOL/MOVE/COPY 已实现并有单测，但**还没让 BC 真的保存改动**验证过（BC 保存时才发这些请求） |
| profile 新鲜度 | 已实测：**运行中的 BC 实时跟随文件**（BC 启动后写入的 profile 立刻可用，打开过配置文件管理器也一样）。唯一失效情形是"BC 启动时 `BCProfiles.xml` 还不存在"——那个实例会一直无视它，需**重启一次 BC**；已用"应用启动/配对时预写"把这个窗口消掉 |
| 非便携安装 | 本机是便携版；`%APPDATA%` 分支未在本机实测 |
| macOS | `bcomp` 路径与行为未验证 |
| 只读策略 | F-73（共享方可设"允许写回/只读"）尚未接入 DAV 端点，目前 DAV 一律可写 |
| 未装 BC | `bc_status().exePath === null` 时菜单仍可点，会得到"未找到 BeyondCompare…请在设置里指定路径"的提示 |