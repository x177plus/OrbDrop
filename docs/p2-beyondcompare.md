# OrbDrop × BeyondCompare（右键 BC 集成）

> 版本：2026-09-16　状态：**读路径已端到端验证**（BC 5.2 实测）；写回（PUT/DELETE/MKCOL）已实现但尚未用 BC 实际操作验证。
> 本文记的是"靠实验才逼出来"的事实，改动前先读第 1 节，别按直觉写。
>
> ⚠️ **后续改动（传输加密那一批，2026-09-22）**：设备之间的传输改成 TLS 之后，BC **不再直连对端** ——
> profile 的 BaseURL 从 `http://<对端 ip:端口>/dav/` 变成
> **`http://127.0.0.1:<本机端口>/proxy/<设备id>/`**（本机回环代理，见 `server::proxy`），
> 由本机加密转发到对端 `/dav/`。下面的实测结论（位置语法、profile 生命周期、密码信封、
> `401 + WWW-Authenticate` 挑战、OPTIONS/PROPFIND 顺序）**一条都没变**，变的只是 BaseURL
> 指向哪儿；文里出现的 `http://<对端 ip:端口>/dav/` 一律按上面的新形状读。
>
> **代理不只要转发，还要改写两处命名空间**（都是实测踩出来的）：
> 1. 请求的 `Destination`（`COPY`/`MOVE`）—— 否则对端收到一个指向 `127.0.0.1` 的目标；
> 2. **响应正文里的 href**（`<D:href>/dav/…`）—— 它是**对端命名空间**里的绝对路径，
>    原样交给 BC 会被按"响应所在的主机"解析成 `127.0.0.1:<本机端口>/dav/…`，也就是本机
>    共享项那一面：症状是**对端一个条目都看不到、往里点也进不去**。
>    `proxy::rewrite_hrefs` 只换 `href>` 后面紧跟的前缀（路径中间恰好叫 `dav` 的目录不受影响）。
>    回归测试：`server::proxy::tests::bc_reaches_the_peer_through_the_loopback_proxy`
>    （两个实例真起服务，走"裸探 401 → PROPFIND 列表 → 按 href GET → 钻子目录"）。
> **这条新路径还没有用真机 BC 跑过**（开发机上没装 BC），是本功能当前唯一未验证的环节。

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
- **文件读不出来或解析不了 → 放弃本次写入（`upsert` 上抛错误、`prune` 直接跳过），绝不重建**：只有"文件不存在"或"确实是空文件"才走重建。读失败（非 UTF-8 / 权限 / 写了一半）如果被当成空文件，重写出来的就是一份"只有我们那条"的 `BCProfiles.xml`，用户自己的 profile 全丢 —— 这是唯一真能删掉别人配置的路径，`bc::tests::unreadable_or_corrupt_file_is_left_untouched` 钉住它。
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
  - `read_slot()/write_slot()`：左侧槽的读写（见 2.3）——**两个值名都写**、读按右键项类型取（`pick_slot`）、只写 HKCU 且只碰那两个值名。
  - `status()/set_left()/compare_with_left()`：槽可写判据（父键在不在）、放入左侧（覆盖）、与左侧比对（左边读槽的**原文**）。
- 配置：`Config.bc_exe_path`（留空=自动探测）。**左侧槽不在配置里**：它只有一处存储 —— BC 自己的 `Saved*Left`（见 2.3）。
- Tauri 命令：`bc_status` / `bc_set_left` / `bc_compare_with_left`。
- DAV 端点：`src-tauri/src/server/routes_dav.rs`，命名空间 `/dav/<shareId>/<相对路径>`，方法 `OPTIONS/GET/HEAD/PUT/DELETE/MKCOL/MOVE/COPY/PROPFIND/PROPPATCH`，可见性复用 `Caller::sees_share`（不可见与不存在都回 404），路径逃逸防护走"逐级向上 canonicalize"（PUT/MKCOL 的目标可以还不存在）。

### 2.2 两个必须记住的坑

1. **DAV 路由不能挂 CORS 层**。tower-http 的 CORS 会把 `OPTIONS` 直接抢答成 preflight 响应，而 BC 正是用 `OPTIONS` 协商 DAV 能力——被抢答 = BC 认为"这不是 WebDAV"，远端目录直接加载不了。所以 `build_router` 里 REST 走 CORS、DAV 单独一棵树。
2. **带尾斜杠的路由要单独注册**：BC 对目录用 `/dav/<shareId>/`，只注册 `/dav/{share_id}` 会让目录级请求 404。

### 2.3 左侧槽 = BC 自己的 `Saved*Left`（全系统只有一个左侧）

资源管理器那套右键（BC 5.2 的 `BCShellEx64.dll`）与 OrbDrop 共用**同一处**左侧状态：

- **值**：`HKCU\SOFTWARE\Scooter Software\Beyond Compare 5\BcShellEx` 下的 `SavedFolderLeft` / `SavedFileLeft`
  （本机 DLL 串表里左侧只有这两个值名，另有合并用的 `Saved*Center`；**没有 `Saved*Right`** —— 官方
  Explorer Integration 一页列全了：`Select Left File/Folder` + `Select Center File/Folder`，
  "右侧"永远是你在资源管理器里右键点的那一项）。
- **写**：OrbDrop 的「添加到左侧比较位」→ `write_slot()`，**两个值名写同一个值**。BC 按右键项类型取槽
  （右键文件看 `SavedFileLeft`、右键文件夹看 `SavedFolderLeft`），只写一个的后果是**另一类条目在资源管理器里
  看不到「对比」项**（真机实测踩到：OrbDrop 放了个目录进去，右键**文件**就没有对比项了）。BC 自己的扩展也
  不清理另一个槽（实测：资源管理器"选择左侧文件"之后，之前那个文件夹左侧还在），我们跟着它走。这就是"覆盖"语义。
- **读**：`read_slot(prefer)` —— `prefer` 就是"右键的那一项是文件还是文件夹"，**跟着 BC 的类型规矩取**
  （要的那一档空着退另一个，`pick_slot`）；没有上下文（启动/设置面板）时文件夹槽优先。HKCU 优先、32 位那份
  （`HKLM\SOFTWARE\WOW6432Node\…`）只作回退。读到什么就**原样喂给 BC**（`resolve_side` 的 `Raw` 分支）；
  反解成 `Share`/`Peer`/`Raw` 只影响菜单显示与"是不是同一项"的判定，反解不出来也照样能比。
- **两个槽可能不一致**，只有一种来路：你在资源管理器里选过左侧（BC 只更新它那一档），我们镜像的另一个还在。
  那时我们的行为与 BC 的右键菜单一致：右键文件用文件槽、右键文件夹用文件夹槽。
- **保护**（"不能让 BC 出问题"）：只写 HKCU、只碰这两个值名（同一把键里还住着 BC 自己的 `Viewers` /
  `ArchiveMasks` / `PictureMasks` / `RegistryViewer`，一个都不动）、不删键、不写掩码、值类型固定
  REG_SZ、覆盖前把旧值写进日志；`HKCU\…\Beyond Compare 5` 父键不存在（= 本机没装过 BC5）或
  **父键开不出写权限**时**一个值都不写**，前端整个 BC 菜单区也不显示。
- **写权限要用写句柄探**：`winreg` 的 `open_subkey` 只给 `KEY_READ`，拿它写注册表会
  `ERROR_ACCESS_DENIED`（实测踩过一次）；所以 `slot_writable()` 用 `open_subkey_with_flags(…,
  KEY_READ | KEY_WRITE)` 真开一次，写路径也从父键的写句柄 `create_subkey("BcShellEx")` 拿子键。
- **没有"移除左侧"**：与官方一致（BC 也只有"选择左侧"覆盖）。

## 3. 验证记录

- `cargo test`：25 个单元测试（含 BC 加密互通 fixture、profile 合并/幂等/清理、Basic 凭据拆分、spec 语法、DAV 方法/可见性/非回环 401 挑战/局域网 Basic 认证）+ 7 个 P1 集成（含解除配对清理 profile、重启恢复长连接）+ 4 个共享可见性，全绿。
  - 左侧槽另有 6 条：`clean_slot_value`（引号/空白）、`split_profile_spec`、`share_of_path`（分量边界 + 大小写 + 嵌套）、`compare_command` 的 argv 原样传递、`pick_slot`（按右键项类型取槽 + 退另一档）、以及**拿临时键走同一条代码路径**的 `write_slot_values`/`read_slot_values`（钉住"两个值名都写"）。
  - 注：DAV 测试各自持有**独立端口段**（4100/4110/…）。都从同一基址往上探会在并行跑时互相抢端口，表现为偶发 `IncompleteMessage`。
- 真机取证（左侧槽，2026-10-13，脚本/命令驱动，非 GUI）：
  - 生产代码 `write_slot()` 真写 `HKCU\…\Beyond Compare 5\BcShellEx`：`reg query` 读出 `SavedFileLeft` / `SavedFolderLeft` 两条 `REG_SZ`，同一把键里的 `Viewers` / `ArchiveMasks` / `PictureMasks` / `RegistryViewer` / `SavedLeft` 一个字节未动。
  - **两个槽是独立的**（用户实测修正了第一版设计）：OrbDrop 往文件夹槽写了目录之后，资源管理器右键**文件**没有「对比」项；而同一时刻注册表里 `SavedFileLeft` 还留着上一次"选择左侧文件"的路径 —— 说明 BC 的扩展只更新与右键项同类型的那一档、且**不清另一个**（第一版"写一个删另一个"因此是错的：另一类条目直接失去了左侧）。现在 `write_slot()` 两个值名都写，读按右键项类型取（`pick_slot`，与 BC 的右键菜单同一规矩）。
  - 踩到的坑：`winreg::open_subkey` 只给 `KEY_READ`，拿那个句柄写注册表必然 `ERROR_ACCESS_DENIED` —— 写路径改成父键的 `KEY_READ|KEY_WRITE` 句柄 + `create_subkey("BcShellEx")`，`slot_writable()` 也用同一把写句柄探（"键存在"不等于"能写"）。
  - `launch_compare` 那两个裸参数：`BCompare.exe "E:\…\docs" "E:\…\src"` → BC 窗口标题 `docs <--> src - 文件夹比较(F)`（参数顺序 = 左、右，没引号、没拼接）。
  - **还没跑到的一层**：OrbDrop 菜单在真窗口里的渲染与点击（本环境没有可用的桌面 UI 自动化），以及下面 Test 1/2/3（都必须人在资源管理器里点 BC 菜单）。
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
| 未装 BC | `bc_status().exePath === null` ⇒ 前端 **整个 BC 菜单区不显示**（`ContextMenu.svelte` 的 `bcAvailable`；「左：X」/「用 BeyondCompare 比较 · 与左侧：X」/「添加到左侧比较位」连同分隔线一起隐藏）。设置面板保存/取消后重连一次 `bc_status`，所以刚填上的 BC 路径不用重启就生效。后端 `bc_compare_with_left` 的"未找到…请在设置里指定路径"只剩兜底作用（前端已挡住入口） |
| 槽不可写 | `bc_status().slotWritable === false`（没装过 BC5，或那把键开不出写权限；前端判据是 `exePath && slotWritable`）⇒ 整区同样不显示：没有卡槽的地方，我们也不去造 |
| 左侧槽未真机联动 | Test 1/2/3 还没跑（都要人在资源管理器里点 BC 菜单）：①BC 的「选择左侧」是否真写 `SavedFolderLeft`/`SavedFileLeft`、值的格式（带不带引号）；②我们写进去的值，资源管理器里的「对比」是否立刻认；③把 `profile:` 位置写进槽后资源管理器里点「对比」时 BC 的行为（若异常，退化成"对端目标不写槽"） |
| 对端槽会陈旧 | 槽里那条 `profile:<ip:端口>` 在设备离线 / 端口变了以后失效（BC 报"配置文件找不到"）——与 BC 自己的最近比较同一类陈旧，重新「添加到左侧比较位」覆盖即可 |