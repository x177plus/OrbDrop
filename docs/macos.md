# macOS 版（桌面端）

> 状态：**代码已落地，并已完成真机构建与启动冒烟**（macOS 12.7.6 Intel，universal .app
> 启动成功、监听 3890、mDNS 已注册、托盘/窗口外壳初始化无报错）。拖出、BC 写回、自更新等
> 功能项仍按 §7 逐项验证。开发机是 Windows，本地只能做交叉类型检查。

## 1. 目标与范围

macOS 版与 Windows 桌面端**共用同一份 Rust 核心与 Svelte 前端**，只替换平台外壳：

| 能力 | Windows | macOS |
|---|---|---|
| 悬浮球窗口 | 无边框 + 透明 + `skipTaskbar` + DWM 边框抹除 | 无边框 + 透明 + `LSUIElement` + `ActivationPolicy::Accessory` |
| 始终置顶 / 全空间 | `HWND_TOPMOST` | `NSFloatingWindowLevel` + `CanJoinAllSpaces` + `Stationary` + `FullScreenAuxiliary` |
| 托盘 | 通知区图标 + 右键菜单 | 菜单栏图标 + 右键菜单（`tray-icon`） |
| 拖出本机共享项 | OLE `DoDragDrop` + `CF_HDROP` | `NSDraggingSession` + `public.file-url`（`drag` crate） |
| 拖出对端共享项 | CFAPI 占位符，目标读取时水合 | **起拖前整份暂存**到镜像目录（没有 CFAPI 的公开等价物） |
| 拖入文件 | Tauri drag-drop 事件 | 同左 |
| Beyond Compare | 注册表 `Saved*Left` 槽 + `BCProfiles.xml` | 应用内 `bc-left.json` 槽 + 同一套 `BCProfiles.xml` |
| 开机自启 | `HKCU\...\Run` | `~/Library/LaunchAgents/com.x177plus.orbdrop.plist` |
| 自更新 | 单文件 exe 由新 exe 当 helper 覆盖 | `.app` zip 解压后由 `/bin/sh` helper 等旧进程退出再整体替换 |
| 单实例 / 再次打开 | 单实例插件 | 单实例插件 + `RunEvent::Reopen` 叫回悬浮球 |

数据目录（设置、日志、凭据、续传、镜像、`bc-left.json`）在
`~/Library/Application Support/OrbDrop`，与 Windows 的 `%LOCALAPPDATA%\OrbDrop` 同构。

### 1.1 样式回退：WKWebView 支持 `oklch()`，但不一定有 `color-mix()`

**不能按 Safari 版本推这个能力。** macOS 12.7.6 的系统 WKWebView（Safari 17.6 内核）实测：

```js
CSS.supports('color', 'oklch(58% 0.18 255)')                        // true
CSS.supports('background', 'color-mix(in oklab, red, blue)')        // false
```

而桌面样式表原先按 WebView2（Chromium 111+）写，只用了 `oklch()` / `color-mix()`，没有 sRGB 回退。
`color-mix()` 的失效发生在**计算期**（解析期不报错）→ 结果是 `unset`，不会回退到同属性的上一条
声明。于是所有派生令牌（`--accent-deep` / `--accent-soft` / `--ok-soft` …）整条失效：

- `.btn-primary` 计算成 **透明底 + 白字** —— 配对对话框的「发起邀请 / 确认配对 / 添加并配对」、
  设置面板的「保存设置」在 Mac 上因此**看不见**（用户看到的就是"没有配对按钮、没有保存按钮"）；
- 状态章、悬停底、遮罩、滚动条等凡是用 `color-mix()` 的地方一起失色（"UI 样式有点不一样"）。

处置与移动端同一套：`src/styles/main.css` 也交给 `scripts/gen-color-tokens.mjs`（`npm run tokens:gen`
对移动/桌面两份表各跑一次）。主 `:root` 与普通声明只留 sRGB，现代写法收进文件末尾的
`@supports (color: oklch(…)) and (background: color-mix(…))` 生成区；`scripts/check-compat.mjs` 现在
同时检查 `src/styles/main.css` 与 `dist/assets/*.css`（桌面端不查 Chrome 81 的 `:has()`/JS 语法，
但"基础表不许出现 `color-mix()`、纯 `oklch()` 必须成对"这两条照查）。构建期 CSS 两端都不压缩：
压缩器会去重同属性声明、也会删掉生成区标记，回退就没了。

真机验证（macOS 12.7.6 的系统 WKWebView，同一份产物 CSS）：`--accent-deep` 解析成
`rgb(15, 91, 173)`，`.btn-primary` / 配对主按钮 / 设置页保存按钮的 `background` 都是这个实色、
`color` 是 `rgb(255, 255, 255)`；现代 Chromium 上仍走 `@supports` 路径（计算值是 `oklab(…)`）。

### 1.2 文本选择、字体、贴边竖条与焦点环

四处都是"在 Windows/Chromium 上看不出来、在 macOS WebKit 上才露"的差异。前三条的实测值来自
macOS 12.7.6 的系统 WKWebView（Safari 17.6 内核）：

1. **`user-select` 必须带 WebKit 前缀**：`CSS.supports('user-select','none')` = `false`，
   `CSS.supports('-webkit-user-select','none')` = `true`。只写无前缀时 `body` 的计算值
   `-webkit-user-select` 仍是 `text` —— 鼠标划过正文变 I 形、按住就能拖选。现在 `body` 两条都写
   （`-webkit-user-select: none` + `user-select: none`），聊天记录 / 文件行 / 输入区用
   `-webkit-user-select: text` 明确开回来；`input` / `textarea` / `[contenteditable]` 一并开。
2. **贴边竖条的列宽就是行高**：竖排时 `line-height` 沿水平方向量。`.rq-dock-name` /
   `.rq-dock-ip` 原先继承 `body` 的 1.6 → 12px×1.6 = 19.2px / 10px×1.6 = 16px，而列宽只有
   16px / 13px，实测 `scrollWidth / clientWidth` 是 **19/16** 与 **16/13** —— 每列被裁掉 3px。
   现在两列的 `line-height` 直接等于各自的列宽令牌（`--rq-dock-name-w` / `--rq-dock-ip-w`），
   左右贴边都不再切字（`scrollWidth` 与 `clientWidth` 相等）。
3. **字体就是 macOS 系统字体**：`--font-body` 以 `-apple-system` 开头，拉丁用 SF Pro、汉字用
   PingFang SC（`document.fonts.check('12px "PingFang SC"')` / `"SF Pro Text"` / `"SF Mono"` 均为
   `true`）。之前全局 `-webkit-font-smoothing: antialiased` 是 WebKit 专属覆盖，会把正文压成更细的
   灰度抗锯齿 —— 1x 屏（1280×1024）上尤其发虚。已去掉，恢复系统默认抗锯齿；Windows 不认这条，
   行为不变。
4. **输入框的焦点环不能用 `outline` 画**：WebKit 对非 `auto` 的 `outline-style` 长期不跟
   `border-radius`（#20807 / #231433；到 Safari 27 才补完 "outline radii rendering for elements
   with a non-auto outline-style"，#174328839）。`.input` / `.chat-input` / `.dev-search` 都是圆角，
   聚焦时 `outline` 会画成**直角方框**；全局 `:focus-visible` 还带 `outline-offset: 2px`，方框
   离输入框 2px，看起来就是"输入框外面多了一个空框"。Chromium 的 outline 跟着圆角、像一圈柔光，
   所以在 Windows 上看不出来。现在三个输入框的焦点环改成
   `box-shadow: 0 0 0 2px var(--accent-soft)` + `outline: none`（`.chat-compose.bc` 里换成
   `--bc-soft`）：`box-shadow` 在所有引擎上都跟圆角；`outline: none` 的特异性（0,2,0）高于全局
   `:focus-visible`（0,1,0），把 UA/全局那圈一起挡掉，键盘焦点仍有这圈柔光 + 描边变色。

### 1.3 fd 上限、探活与中转并发

macOS 的 `RLIMIT_NOFILE` **软上限默认 256**（硬上限 unlimited）。实测进程在 18 条 `knownPeers`
+ 5 个中转端点时已经占 152–223 个 fd；再叠一次中转/传输突发就撞到 256，`accept()` 报
`Too many open files`（2026-10-26 日志 09:25–09:27）。**不是泄漏**：`CLOSE_WAIT = 0`，fd 数在
152–223 之间波动；是"上限低 + 无效探活/中转峰值"。

三处处置：

1. **启动抬上限**（`src-tauri/src/resource.rs`）：`RLIMIT_NOFILE` 软上限抬到
   `min(hard, 65535)`；每 30s 采一次 `/dev/fd`，到 80% 打 warning（5 分钟内不重复）。
   Windows 没有这种低句柄上限，模块是空操作。
2. **探活去重与清理**（`state.rs`）：同一 `ip:port` 一轮只探一次；启动与每轮探活后
   `dedupe_known_addresses()` 合并同一地址上的重复未配对行；未配对行连续 12 拍（60s）失败后
   整行忘掉（`forget_peer`），已配对行保留地址、60 拍只记一条 debug。设备右键给未配对行新增
   「忘记设备」（只清本机记录，不通知对端）。
   实测配置里的 18 条 `knownPeers` 中，**7 条 `D1159@10.198.71.106:3891` + 2 条「演示机」
   (:3990/:3991)** 是主要死探活来源；它们每 5s 各拨一次、1.5s 超时，24 条 `SYN_SENT` 里大半是这些。
3. **中转复用与限流**（`relay.rs`）：同一对 (请求方, 目标) 已有端点时**复用**（不再每 30s
   新开监听）；目标连不上后 60s 冷却，期间不新开端点；空闲回收 120s → 60s；全局最多 24 条、
   单端点最多 8 条同时搬运的连接（每条占 2 个 socket），超出丢连接。请求方 30s 重试的模型
   下，丢一条比把本机 fd 打满更可控。

真机验证（2026-10-26 10:18，新版 `.app` 启动后 75s）：

- 启动日志：`fd 上限已就绪（soft=65535, hard=9223372036854775807）`；
- `knownPeers` 启动瞬间 18 → 12（合并掉 6 条同地址的重复 D1159），7 条 `D1159@:3891` 只剩 1 条；
- 60s 后未配对不可达行被清掉（日志里能看到 `连续探活失败：忘掉未配对设备行`）；
  如果那台设备其实还在网上，mDNS 会把它重新发现并补回一行 —— **清掉的是"死地址的观察记忆"，
  不是屏蔽设备**。2 条「演示机」若仍有测试实例在广播，也会被重新发现；停掉测试实例或右键
  「忘记设备」即可；
- `lsof`：`SYN_SENT` 23 → 8、总 fd 129（之前 152–223）、UDP 22；未再出现 `Too many open files`；
- 中转复用/限流：单元测试覆盖「同一对复用、失败冷却、全局/单端点额度」；运行期日志出现
  `复用已开的中转端点` 即表示 30s 重试没有再开新监听。

## 2. 悬浮窗外壳

`tauri.conf.json` 里已有的 `transparent` / `alwaysOnTop` / `decorations: false` 在 macOS 上额外需要：

- `app.macOSPrivateApi: true` 与 Cargo feature `macos-private-api`：透明窗口走私有 API，
  否则阴影余量会变成一块实心背景；
- `windows[].visibleOnAllWorkspaces: true`；
- `src-tauri/Info.plist` 的 `LSUIElement = true`：启动那一刻就不占 Dock；
- `src-tauri/src/orb_window.rs` 的 `setup_macos`：`ActivationPolicy::Accessory`、
  `NSWindowCollectionBehavior::{CanJoinAllSpaces, Stationary, FullScreenAuxiliary}`。

`Info.plist` 同时声明了局域网与 Bonjour：

```xml
<key>NSLocalNetworkUsageDescription</key>
<string>…</string>
<key>NSBonjourServices</key>
<array><string>_orbdrop._tcp</string></array>
```

macOS 15 起没有这两项，系统不会给局域网授权提示，mDNS 发现会静默失败。macOS 自带的
Application Firewall 首次还会问一次「是否允许 OrbDrop 接受传入连接」，拒绝的话同样只能
手动添加 / 出站可用，入站全挂。

窗口工作区从 `Monitor::work_area()` 取（Tauri 在 macOS 上走 `NSScreen.visibleFrame`，
已扣除 Dock 与菜单栏），不再假设整块屏幕。

`⌘W` / 菜单「关闭窗口」在 `lib.rs` 里被拦下改成隐藏：窗口销毁后托盘仍在，但前端与
`get_webview_window("main")` 再也找不回它。退出走托盘「退出」或 `⌘Q`。

## 3. 拖出：macOS 的两条路

### 3.1 本机共享项

`src-tauri/src/drag.rs` 的 macOS `imp` 用 `drag` crate 起 `NSDraggingSession`，粘贴板里是
`public.file-url`，只声明 `Copy`。交给 Finder 的就是磁盘上的原文件。

### 3.2 对端共享项

没有 CFAPI。可选方案与取舍：

| 方案 | 结论 |
|---|---|
| File Provider（`NSFileProviderReplicatedExtension`） | 官方按需文件方案，但要独立 app extension、App Group、单独签名；单文件 Tauri 应用装不下 |
| macFUSE | 要用户装内核扩展/系统扩展，不能作为默认路径 |
| `NSFilePromiseProvider` | Finder/邮件支持，很多第三方应用不认；目录承诺行为不统一。违背「交给目标真实路径」的硬约定 |
| **起拖前整份暂存** | 到处都能用，目标拿到真实路径；代价是起拖要等下载、磁盘留副本 |

当前实现（`src-tauri/src/mirror/mac.rs`）：

1. `drag::prepare` 先问对端 `/stat`（老版本退回 `/tree`），确定文件/目录与大小；
2. 下载到 `<数据目录>/mirror/<peer>/<share>/`，文件名清洗规则与 Windows 镜像同源；
3. 每次拖拽用**独立槽目录**（文件名仍是原名），绝不在目标可能正在读取时覆盖/删除；整槽按 TTL 回收；
4. 同一项 10 分钟内重拖直接复用（手松早了不用再下一遍）；
5. `mirror::sweep` 按本地放入时刻做 24 小时 TTL 回收；目录递归下载有 200k 项 / 64 层上限。

大文件/大目录会在界面显示「正在准备…（请按住不放）」期间等待。这是 macOS 与 Windows
的**已知差异**，不是漏实现。`prepare` 的整体等待上限是 120 秒：超时会报「准备超时」，
但后台下载会继续，完成后 10 分钟内重拖直接复用，不会从头再下一遍。

另一个已知差异：Windows 的 `hit_region` 可以让悬浮窗外圈那圈**阴影余量**穿透点击；
macOS 的 `ignoresMouseEvents` 是整窗级开关，做不到「只让余量穿透」，所以余量照旧会吃掉
点击 —— 与这次改造前一致（`src-tauri/src/hit_region.rs` 的非 Windows 分支）。

## 4. Beyond Compare

- 可执行文件：`locate_exe` 除常见路径外，接受用户填的 `.app` 包（自动取
  `Contents/MacOS/bcomp`，兼容部分包里的 `bcompare`）；
- 配置目录：默认 `~/Library/Application Support/Beyond Compare`，但 BC 装过多个大版本
  （或按官方说明重命名过）时，**BC5 真正读写的是 `Beyond Compare 5`**、BC4 是
  `Beyond Compare 4`。`config_dir` 从 `.app/Contents/Info.plist` 读
  `CFBundleShortVersionString`，优先用与 exe 大版本匹配的版本化目录，再退回默认目录。
  走错目录的表现是 BC 报「配置文件找不到」/「文件夹不可用」（真机 BC 5.2.6 踩到）。
  BC 只在启动时登记 `BCProfiles.xml`，更新后第一次需要**重启一次 BC**；服务监听是异步起的，
  启动预写可能只写下骨架，服务端口落下来后会再补写一次；
- 左侧比较位：macOS 没有资源管理器右键扩展，应用自己维护
  `<数据目录>/bc-left.json`（原子写入），菜单里的「添加到左侧比较位 / 与左侧比较」照旧；
- WebDAV profile 的注入、加密信封与回环代理与 Windows 共用 `bc.rs` / `server::proxy`，
  新增的只是 macOS 的路径与槽存储。

真机实测（BC 5.2.6，macOS 12.7.6，Intel）：入口是
`/Applications/Beyond Compare.app/Contents/MacOS/bcomp`，设置目录是
`~/Library/Application Support/Beyond Compare 5/`（不是不带版本号的那个）。
`bcomp` 两个参数 + `profile:<ip:端口>?<共享项id>` 经本机回环代理读取对端 `/dav/`
已跑通（PROPFIND Depth 0/1、Basic 鉴权、`href` 改写都正常）；`BCmp|1|` 信封也能被
BC 5.2.6 解开。BC 4.2.8 的入口与信封仍未验证。

写回（BC 保存时的 `PUT` / `DELETE` / `MKCOL` / `MOVE`）仍未用真机 BC 实际保存验证。

## 5. 自更新

### 5.1 产物

macOS 的更新单位是整个 `.app`：

```bash
npm run mac:artifact
# → assets/v<版本>/OrbDrop_<版本>_macos_universal.app.zip   （自更新）
# → assets/v<版本>/OrbDrop_<版本>_macos_universal.dmg      （直接安装）
```

`macos-artifact.mjs` 只让 Tauri 出 `.app`（`--bundles app`），然后：

- `ditto -c -k --keepParent` 打自更新 zip（保留符号链接、资源分叉与签名）；
- `hdiutil` 做 DMG（应用 + `/Applications` 链接）。

DMG 不走 Tauri 自带的 dmg bundler：那个 bundler 会调 Finder/AppleScript 摆窗口，
在 SSH / 无 Finder 会话的机器上会直接失败（本机实测）。通用包同时包含 arm64 与 x86_64。

### 5.2 客户端落地

1. 下载 zip → sha256 + minisign 校验（与 Windows 同两道）；
2. `ditto -x -k` 解压到临时目录；
3. 写入 `/bin/sh` helper，参数为 `pid / 旧 .app / 新 .app / 日志`；
4. helper 等旧进程退出 → `ditto` 到同卷 `.new` → 两次 `mv` 原子替换 → `open` 新版本；
5. 失败则把旧包改名回去并重新打开旧版本。日志在 `<数据目录>/logs/update.log`。

限制：

- 应用必须放在**可写**位置（通常 `/Applications`），且不能从 DMG / 下载目录直接运行
  （App Translocation 会拒绝并提示先拖进「应用程序」）；
- 未签名/未公证的包首次运行需要用户右键「打开」一次；正式发行应在 CI 里配置
  `APPLE_CERTIFICATE` / `APPLE_SIGNING_IDENTITY` / `APPLE_ID` / `APPLE_PASSWORD` /
  `APPLE_TEAM_ID`。

### 5.3 清单

`update.json` 的 `desktop` 里加 `macos-universal`（也可以为两种架构各出 `macos-aarch64` /
`macos-x86_64`）。客户端优先本机架构键，再退通用包：

```json
"macos-universal": {
  "size": 18874368,
  "sha256": "…",
  "sig": "…",
  "mirrors": ["https://github.com/…/OrbDrop_X.Y.Z_macos_universal.app.zip", "https://gitee.com/…"]
}
```

发布时在 Windows 发布机上执行：

```bash
node scripts/release.mjs --version X.Y.Z --notes "..." \
  --macos-zip assets/vX.Y.Z/OrbDrop_X.Y.Z_macos_universal.app.zip \
  --macos-dmg assets/vX.Y.Z/OrbDrop_X.Y.Z_macos_universal.dmg
```

`--macos-zip` 是自更新单元，会写进 `update.json` 并被客户端校验/解压；`--macos-dmg` 只随
Release 上传（GitHub/Gitee 各一份）并落进 `assets/vX.Y.Z/`，**不进清单**，供用户首次安装。

macOS 包只能在 Mac 上构建：用 `macos-artifact.mjs` 或
`.github/workflows/macos.yml`（手动触发，上传 `OrbDrop_<版本>_macos_universal.app.zip` 与 dmg）。

## 6. 构建

要求：macOS 11+、Xcode Command Line Tools、Node.js、Rust。

```bash
npm ci
npm run mac:dev                  # 开发（vite + tauri）
npm run mac:build                # universal .app + .dmg
npm run mac:build:arm64          # 仅 Apple Silicon
npm run mac:build:x64            # 仅 Intel
npm run mac:artifact             # universal + 自更新 zip + DMG
```

`npm run mac:build` 的产物在：

```
src-tauri/target/universal-apple-darwin/release/bundle/macos/OrbDrop.app
src-tauri/target/universal-apple-darwin/release/bundle/dmg/OrbDrop_<版本>_universal.dmg
```

`npm run mac:build` 的 DMG 由 Tauri 的 dmg bundler 生成，需要当前 SSH 会话能驱动 Finder；
在纯 SSH / CI 机器上建议用 `npm run mac:artifact`（它用 `hdiutil`，不依赖 Finder），产物在
`assets/v<版本>/`。

## 7. 真机验证

### 7.1 已完成的构建冒烟（macOS 12.7.6 / Intel / CLT 14）

- `npm ci` + `npm run mac:artifact` 成功；Rust 1.99.1，Node 24.14.1。
- 产物：universal `.app`（x86_64 + arm64，32 MB）、自更新 zip（约 13.2 MB）、DMG（约 14.5 MB）。
- `.app` 启动成功：监听 `3890`，mDNS `_orbdrop._tcp` 已注册，日志出现
  「macOS 窗口外壳已就绪」「托盘菜单已就绪」「HTTP server started」；未出现 panic。
- 从 DMG 挂载复制出来的 `.app` 也能启动；DMG 内有 `/Applications` 链接；zip 解压结构正确。
- 当前包**未签名**（`codesign -dv` = not signed），本机直接运行正常；对外分发需签名/公证。
- **样式回退**（§1.1）：同一版产物 CSS 在系统 WKWebView 上算出的主按钮是实色蓝 + 白字
  （`rgb(15, 91, 173)` / `rgb(255, 255, 255)`），不再透明；现代 Chromium 仍取 `@supports` 的
  `oklab(…)` 值。
- **文本 / 竖条 / 字体**（§1.2）：同一版产物 CSS 在系统 WKWebView 上，`body` 的
  `-webkit-user-select` = `none`、`.chat-msg` = `text`；贴边两列的 `scrollWidth` 与 `clientWidth`
  相等（16/13）；`-webkit-font-smoothing` = `auto`。

### 7.2 仍需逐项验证

1. **启动与外壳**：不占 Dock；菜单栏有图标；悬浮球在别的应用全屏时仍可见；`⌘W` 只隐藏。
2. **拖入**：Finder 拖文件/目录到球 → 变成共享项；拖到展开的会话浮层 → 推送。
3. **拖出本机项**：拖到 Finder / 桌面 → 复制成功；拖到微信等只认路径的应用。
4. **拖出对端项**：小文件、大文件、空目录、多层目录、重名重拖；下载中断的报错文案。
5. **回收**：24h 后/重启后镜像目录被清；拖出后立刻再拖不应拿到空文件。
6. **mDNS / 局域网**：首次弹本地网络授权；允许后能发现 Windows/Android 设备；
   拒绝后手动添加仍可用。（`tauri dev` 运行的是裸二进制、没有打包的 `Info.plist`，
   这一项要用 `npm run mac:build` 出来的 `.app` 验证。）
7. **托盘**：左键单击/双击、右键菜单、在线/离线勾选、红点闪烁。
8. **自启动**：`LaunchAgents` plist 生成/删除；重启登录后只起一份（单实例）。
9. **Beyond Compare**：macOS 版 BC 的 profile 文件位置、远端共享项比较、保存/写回。
10. **自更新**：从 `--macos-zip` 发布的版本升级；失败回滚；日志可读。

## 8. 代码地图

| 文件 | 作用 |
|---|---|
| `src-tauri/Info.plist` | `LSUIElement`、局域网/Bonjour、TCC 文件夹说明 |
| `src-tauri/tauri.conf.json` | 透明窗口、全空间、macOS 最低版本 |
| `src-tauri/src/orb_window.rs` | Accessory、NSWindow collection behavior、work_area、鼠标键状态 |
| `src-tauri/src/autostart.rs` | LaunchAgent（`open -a` 拉起 .app） |
| `src-tauri/src/drag.rs` | macOS `NSDraggingSession` 起拖 |
| `src-tauri/src/mirror/mac.rs` | 对端项暂存下载 |
| `src-tauri/src/bc.rs` | `.app` 探测、BC 配置目录、macOS 左侧槽 |
| `src-tauri/src/update/apply.rs` | `.app` zip 替换 helper |
| `src-tauri/src/update/mod.rs` | `macos-*` 清单键、zip 下载落点 |
| `src/lib/stores/platform.svelte.ts` | 前端平台文案（访达 / ⌘V / 本机） |
| `scripts/macos-artifact.mjs` | macOS 构建 + zip 打包 |
| `.github/workflows/macos.yml` | macOS 构建工作流（可选签名） |