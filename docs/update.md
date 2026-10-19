# F-110 自更新与发行版发布

本文写两件事：**应用怎么更新自己**（桌面端覆盖 exe、安卓端拉起系统安装器），以及**版本怎么发出去**
（只往公开仓放 README / LICENSE / Release 产物与 `update.json`；源码同址公开 —— 见 §2 的口径说明）。

## 1. 为什么不用 `tauri-plugin-updater`

两条硬约束把它排除了：

1. **桌面端是单文件 exe**（`tauri build --no-bundle` 的产物，没有安装器）。官方插件在 Windows 上
   唯一的落地方式是"下载 NSIS/MSI、把它拉起来、自己退出"——我们没有安装包；要的是**用新 exe 覆盖
   当前 exe**。这件事必须自己写（见 §6）。
2. **镜像切换要按"快慢"判，不只是按状态码**。官方插件的 `endpoints` 是"第一个返回 2xx 且能解析的
   赢"，拉不出"连上了但只有几 B/s 就换"这个判断——而这正是本应用的网络现实（国内直连 github.com
   实测 connect 21 秒才失败）。

另外官方插件**不支持移动端**（plugins-workspace v2 的 README 平台表里 Android 是 `x`），安卓那一档
无论如何都得自己写。两端因此共用同一套 Rust 逻辑（`src-tauri/src/update/`），差别只在最后一步落地。

## 2. 发行形态与仓库布局

| 一个仓 | 两件事 |
| --- | --- |
| **源码** | 全部源码，含 `scripts/release.mjs`（发布流程） |
| **发布** | `README.md` / `LICENSE` / `CHANGELOG.md` / `update.json` + 每个 Release 的附件 |

**单仓**：发布与源码是**同一个仓库**（GitHub / Gitee 两个远端同址），发布脚本直接在**本仓** `master`
上追加提交、打 tag、push。清单地址是编译期常量里的那两条（GitHub 走 Release 附件 `update.json`，
Gitee 走仓库根 `raw/master/update.json`）—— 所以**发布必须落在 `master` 分支**，`update.json` 必须
在仓库根。tag 分两套：发布的 `vX.Y.Z` 指向那次发布提交、Release 挂在它上面；源码侧的对应关系用
`src-vX.Y.Z`（0.0.1 / 0.0.2 的源码提交是 `0f4a760` / `8b4582a`），两套不撞名。

> **口径变更（2026-10-14）**：本节原写「发布仓（公开）+ 源码仓（私有）」与「源码将来要公开时，把源码
> 推同一个仓（`main`）即可」，后来一版写「源码在 `main`、产物与清单在 `master`」。现在只有**一个仓、
> 一个分支 `master`**：源码与发布文件都在上面，本机不再有 `../OrbDrop-releases` 这个目录。本文下方
> 发布记录里的「发布仓 / 源码仓（私有）」字样是当时的事实，不回改。

产物三件：`orbdrop.exe`（Windows 单文件）、`app-universal-release.apk`、`app-arm64-release.apk`。
安卓两个包是因为 arm64 单包更小（14.6MB vs 50.7MB），而通用包兜住老机型与模拟器；客户端按本机架构
选（`update::android_key`）。

构建参数有个坑（实测踩过）：**通用包用 `tauri android build --apk`，arm64 包必须用
`--apk --split-per-abi`**。`--target aarch64` 只是限制 Rust 编哪个 ABI，APK 的 flavor 仍是
`universal` —— 那样会把通用包目录里的文件覆盖成 arm64 单包，于是清单里两个条目指向同一份
arm64 包，armv7/x86 的机器装上去只会得到 `INSTALL_FAILED_NO_MATCHING_ABIS`。发布脚本里这两条
命令与注释都写着这件事。

## 3. 信任链

清单（`update.json`）只靠 HTTPS 保护，所以**清单本身不可信**。真正决定"能不能装"的是：

- **桌面端**：产物上的 minisign 签名。私钥只在本机（`~/.tauri/orbdrop.key`），公钥编进二进制
  （`update::UPDATE_PUBKEY`）。清单被改也只能把用户导向一个**签不出名**的包，装不进来。
- **安卓端**：系统在安装那一刻校验包签名（包名相同、证书不同直接拒装），清单里的 sha256 只当
  传输完整性用。

两道都过才装：`sha256`（防传输损坏/换包）+ `sig`（防镜像或账号被拿去改清单）。校验实现在
`update::verify_file` / `verify_signature`，与 `tauri-plugin-updater` 自己那份**同一个 crate、
同一套调用**（`base64 解码 → minisign 文本 → Signature::decode → verify(.., allow_legacy=true)`）——
格式细节有夹具测试钉着（`src-tauri/tests/fixtures/`）。

> 私钥与安卓 keystore 都**不在仓库里**，必须各自离线备份：丢了 = 老用户再也收不到更新。

## 4. 更新清单

```json
{
  "version": "0.0.2", "versionCode": 2, "notes": "…", "pubDate": "2026-10-09T12:00:00Z",
  "desktop": { "windows-x86_64": { "size": 0, "sha256": "…", "sig": "…", "mirrors": ["<GitHub>", "<Gitee>"] } },
  "android": { "universal": { "size": 0, "sha256": "…", "mirrors": ["<GitHub>", "<Gitee>"] },
               "arm64-v8a": { "size": 0, "sha256": "…", "mirrors": ["<GitHub>", "<Gitee>"] } }
}
```

- **清单地址是编译期常量**（`update::MANIFEST_ENDPOINTS`），与版本无关：GitHub 走
  `releases/latest/download/update.json`、Gitee 走仓库根 `raw/master/update.json`。
  - 发行版页上那个 `releases/download/v0.0.1/update.json` 与 GitHub 这条是**同一份文件**：`latest`
    是别名，服务器 302 到当前最新那一版（`draft`/`prerelease` 不指）。Gitee 那条则**不是**发行版附件，
    而是仓库文件 —— 发布脚本会把同一份清单**两处都写**（仓库根 + 本次发行版附件），所以内容一致、
    地址不同。
- **这两条连的是公网域名，靠 reqwest 内置的公共 CA 列表校验**（`rustls-tls-webpki-roots-no-provider`）。
  到对端那一跳用指纹判身份、用不到信任库，但自更新只能走普通证书校验 —— 别把那条 reqwest 特性
  改回 `rustls-tls-no-provider`（= manual-roots = **空**信任库），出事的样子见 §13。
- **产物地址由清单给**，且钉住 tag（`releases/download/v0.0.2/...`）：老客户端不会因为新版本发布
  而下错包。两条镜像都指向同一份内容。
- `versionCode` 是安卓的安装门槛（`major*1000000 + minor*1000 + patch`，Tauri 的算法）。客户端在
  检查阶段就比对它，挡住"版本号涨了、versionCode 没涨"这种发布事故（真装下去系统只会报一句
  `INSTALL_FAILED_VERSION_DOWNGRADE`）。
- 地址策略：只放行 `https` 与**环回** `http`（本机演练台要 http）。清单里的地址是远端可控的字符串，
  这一关松了就等于允许别人把用户导到任意 http 源上。

## 5. 镜像策略：GitHub 优先，慢/挂就切 Gitee

顺序 = 清单里 `mirrors` 的顺序（发布脚本写的是 GitHub 在前）。下载时逐条试，任一条"连不上 / 太慢 /
挂了"就换下一条：

| 关卡 | 值 | 管什么 |
| --- | --- | --- |
| 连接超时 | 3s | SYN 被吞（国内直连 GitHub 的常态） |
| 响应头超时 | 5s | **连上了但不回数据**（本机演练台上"挂起"那一档） |
| 首字节超时 | 5s | 同上，给 body 流的第一块 |
| 静默超时 | 10s | 传到一半卡住 |
| 速率下限 | 32 KB/s（10s 窗口） | "能下但只有几 B" |

清单那一趟是 8 秒总超时（几百字节的东西）。

换镜像时**从零重下**：Gitee 的发行版附件实测**不支持 Range**（发 `Range: bytes=…` 回来的是 `200`
+ 完整 `Content-Length`，没有 `Content-Range`），跨镜像续传没有意义。

**镜像记忆**：成功的镜像主机名落盘在 `config.toml` 的 `updateMirror`，24 小时内优先用它。没有这条，
国内用户每次检查都要先白等一遍 GitHub 那条超时路径。过期自动作废（挂上代理、Gitee 抽风的场景能
自己爬回来）。

清单与下载都会在**演练台**（`scripts/dev-mirror.mjs`）上实测三档：挂起（`--gh-hang`）、限速
（`--gh-kbps` / `--gitee-kbps`）、正常。

## 6. 桌面端：用新 exe 覆盖当前 exe

Windows 上**运行中的 exe 不能被写**（进程映像还映射着它，是共享冲突而非权限问题）。所以"覆盖自己"
必须由**别的进程**做，且我们不想多发一个更新器（那就不是单文件了）。做法是让**刚下好的新 exe 自己
当 helper**：

1. 旧进程把新 exe 下到 `%TEMP%\orbdrop-update-<版本>\orbdrop.exe`，校验 sha256 + 签名；
2. 旧进程带 `--orbdrop-apply-update --target <当前 exe> --pid <自己> --self <新 exe>` 把**新 exe**
   拉起来（它在临时目录里，与目标不是同一个文件），然后退出；
3. 新 exe 在 `main.rs` 一进门就识别这个参数并返回（**绝不能初始化 Tauri**：那会多一个托盘图标、
   还会和单实例锁打架），等旧进程退出 → 旧 exe 改名 `.old` → 把自己拷到目标路径 → 拉起目标 → 删
   `.old` 与临时目录。

失败也要能退回去：任一步失败就把 `.old` 还原成原路径并拉起**旧版本**——宁可停在旧版，也不能让用户
手里一个 exe 都没有。整个过程写 `<数据目录>/logs/update.log`（helper 阶段没有控制台，日志是唯一出口）。

**落地后依旧只有一个文件**：`.old` 与临时目录都是过渡态。helper 自己删不掉自己（它正跑在临时目录里、
`.old` 也可能还被占着），所以这两样由**下一次启动**的 `update::apply::cleanup_stale` 收掉。

数据不落在 exe 旁边（设置/日志/传输记录都在 `%LOCALAPPDATA%\OrbDrop`），覆盖 exe 不动用户数据；
开机自启注册的是 exe 路径，路径不变就照旧有效。

## 7. 安卓端：下载 → 拉起系统安装器

安卓这一段没有静默安装这条路（非系统应用不给），标准姿势是：

1. Rust 侧照同一套逻辑选包、下载到应用私有目录 `updates/orbdrop-<版本>.apk`、校验 sha256；
2. 交给既有的 `FileOpenPlugin`（F-109）：`FileProvider` 的 `content://` + `application/vnd.android.package-archive`
   + `FLAG_GRANT_READ_URI_PERMISSION`，落到系统安装器上，用户确认一次。
   `REQUEST_INSTALL_PACKAGES` 已在清单里（它让 OrbDrop 出现在「安装未知应用」那张列表里），
   `file_paths.xml` 用的是 `root-path`，覆盖得到私有目录。

安装会**杀掉进程**（前台服务随之中断），这是系统行为，不是缺陷。

## 8. 界面

- 桌面端：设置面板的「版本与更新」一格（当前版本 + 检查更新 + 立即更新 + 进度条 + 状态文案）。
- 安卓端：设置页的「版本与更新」一节，同一个共用 store（`src/lib/stores/update.svelte.ts`）。
- 状态归后端、文案归 `types.ts` 的 `updateStatusText`（两端一份），进展靠 `update://state` 事件推
  **整份快照**（进度是高频事件，增量事件会让前端自己攒状态、迟早对不上）。
- 启动后有**一次静默检查**（延迟 8 秒，30 分钟内复用结论）：它失败不打扰用户（没网、被墙都是常态），
  只有用户手动点的那次才弹错误。
- **它查到新版本时会主动弹一格**（2026-10-12 加的，见 §16）：桌面是右下角那组通知里的一条气泡
  （与"等你确认"的通知同一个形状、同一个铃铛与角标 —— 面板收着时，悬浮球左上角那颗黄标就是唯一
  还看得见的提示），安卓是一张「发现新版本」底板（移动端没有那组气泡，底板就是它的通知位）。
  点下「立即更新」之后**同一格就地变成进度**，失败也留在那一格里（带「重试」）。按掉（「稍后」/
  关掉）= 本次运行内不再自动弹 —— 下次启动是另一个进程，会重新查、重新弹。
- 区分"后台自己查到的"与"用户要看的结果"靠快照里的 `silent`（`check(force = false)` 就是启动那趟）：
  用户自己点的那次已经有设置面板在显示，再弹一遍就是把同一件事说两次。
- 那一格的标题走 `types.ts` 的 `updateTitleText`（有新版本 / 正在更新到 / 更新失败），正文与状态行
  走 `updateStatusText` —— 与状态机同口径，**两端一份**（各写一份的话，"正在校验"迟早两处叫法不同）。

## 9. 发布流程

`node scripts/release.mjs --version 0.0.2 --notes "…"`（构建 → 签名 → 生成清单 → 写本仓根 →
commit/tag → push → 建 Release 传附件 → 回读校验）；本机演练用 `--local --skip-build`（不联网、不 push），
配套 `scripts/dev-mirror.mjs` 起镜像台，按打印出来的命令启动客户端即可。
远端模式要两个 token 调 API：**先读环境变量**（`GITHUB_TOKEN` / `GITEE_TOKEN`），**没有就读
`scripts/release.local.env`**（已 gitignore，一行一个 `KEY=VALUE`，模板里写了去哪申请与要勾的权限；
整体换路径用 `ORBDROP_RELEASE_SECRETS`）。脚本只会 `git add` 清单与 `CHANGELOG.md` 两处 ——
本仓工作树里常年有源码改动，它们不会被卷进 `Released vX` 那个提交；token 只给这一个仓的写权限就够。
随时复核某个已发布的版本：`node scripts/release.mjs --verify-only --version 0.0.1`（只读远端，
不构建、不 push、不调 API）。

**同版本重发**（版本号不动、**产物必须换掉**的那种补发 —— 比如发布之后才发现客户端有缺陷）：
`node scripts/release.mjs --version 0.0.1 --republish --notes "…"`。它与普通发布的差别只有两处，
都跟"这一版已经存在"有关：

- **仓库里的 tag 不新建、也不移动**：只往本仓 `master` 上追加一个提交（`update.json` 与 `CHANGELOG.md`
  变了）。于是 tag 指向的那次提交与 Release 的附件不再一致 —— 客户端读的是清单（GitHub 走 Release
  附件、Gitee 走分支上的仓库文件），两处都是新的；只有"拿 git 去核对 tag 内容"的场合要知道这一层。
- **Release 是复用的**：GitHub 先 `DELETE` 掉旧附件再传新的（不删的话同名附件会 422）；Gitee 直接
  删掉整个 Release（连同附件，顺带释放配额）再建一个同 tag 的。**这是脚本唯一会删远端东西的地方**，
  只在显式给了 `--republish` 时才发生。

注意它**解决不了"已装的旧客户端"**：客户端拿去和本机版本比的是清单里的版本号，同版本重发之后
0.0.1 的客户端只会说"已是最新版"。重发的作用是"让**新装**的人拿到修好的包"（本机与真机重装、
README/发行版页的下载链接）；要让老客户端自己升上来，只能发一个更高的版本号。

回读校验里 **GitHub 那几条"本机够不着"只报警告、不算失败**：发布是从你本机发起的，而国内直连
github.com 时通时不通（实测同一条命令上一秒 200、下一秒 connect timeout）——那说明这台机器此刻的
网络不通，不是发布发错了。HTTP 层的问题（404、版本对不上）照旧硬失败。

Gitee 侧有两道容量约束（官方帮助中心）：单附件 ≤ 100MB、仓库附件总量 ≤ 1GB。一次发布约 78MB
（exe 13 + 通用包 51 + arm64 15），**所以攒到十来个版本就该去 Gitee 删旧发行版**；脚本每次发布都会
打印三个附件的实际大小，但它**不自动删**任何东西（删发行版是不可逆操作，交给用户自己按；唯一例外是
显式给了 `--republish` 的同版本重发，见上)。
脚本反而会拦住几类"发错包"：产物不存在、产物是空的，以及 **APK 里烙的 versionCode/versionName 与清单要写的不一致**（最常见的事故就是忘了 bump）。产物 mtime 只当**提示**不当把关：增量构建在输入没变时本来就不会重写输出（cargo 判定 crate 无事可做、Gradle 判定打包任务 up-to-date，字节相同的产物连 mtime 都不动），拿它当失败条件会误伤 —— 实测撞过一次，那次两个 APK 确实是当前源码编出来的。

Gitee 的公开仓需人工审核，审核期仓库"仅限成员访问"——所以首版发布前先把仓建好过审，GitHub 那条路
同时存在。

## 10. 本机演练记录（2026-10-09）

桌面端在**本机镜像台**上跑通了整条链路，用的是"已装 0.0.1 → 更新到 0.0.2"的真实进程：

| 场景 | 做法 | 结果 |
| --- | --- | --- |
| 基本自替换 | `update.json` 指向本机镜像，0.0.1 实例启动 → 检查 → 下载 → 校验 → helper 覆盖 → 重启 | 1.2s 原进程退出；覆盖后 `orbdrop.exe` 与更新包**逐字节相同**（13692416 → 13692452 字节） |
| GitHub 挂起 | `dev-mirror --gh-hang`（只收请求、永不回数据） | 5 秒判死 → 换 Gitee → 8.4s 完成覆盖 |
| GitHub 慢 | `--gh-kbps 5` | 10 秒窗口实测 5.7 KB/s < 32 KB/s → 换 Gitee → 14.1s 完成覆盖 |
| 两条都坏 | `--gh-hang --gitee-kbps 5` | "所有镜像都没下成"，**不覆盖**、进程留在旧版本（安装文件哈希未变） |
| 包体篡改（sha256 不符） | 改包体、清单不动 | 拒装：`sha256 对不上（清单 … ≠ 实际 …）` |
| 包体篡改（sha256 对齐、签名过期） | 清单的 sha256/size 改成新值、签名仍是旧包的 | 拒装：`签名校验不过：The signature verification failed` |
| 界面接线 | 桌面设置面板在真实浏览器 + 打桩 IPC（`__TAURI_INTERNALS__`）里跑 | 渲染出「当前 v0.0.1 / 检查更新 / 立即更新 / 有新版 v0.0.2 · 13.1 MB」，点「检查更新」发出 `update_check`、点「立即更新」发出 `update_apply` |
| 单元测试 | `cargo test --lib update::` | 8 项全过（含 `tauri signer sign` 产物能被我们的校验器认下、篡改必拒、地址策略、版本解析） |
| 发布流程 | `node scripts/release.mjs --local --skip-build --version 0.0.1` | 三个产物（exe 13.06MB / 通用包 50.71MB / arm64 14.59MB）与清单一次落盘，清单里的 size/sha256 与磁盘文件逐项一致 |

安卓端（真机，手机乙 / arm64-v8a）：

| 场景 | 做法 | 结果 |
| --- | --- | --- |
| 装新版本 | 先卸载旧的 0.1.0（versionCode 1000），再装 0.0.1 | 卸载前装不上（系统拒降级），卸载后 `versionCode=1 / versionName=0.0.1` ✓ |
| 起来 | `am start` 后看进程与前台服务 | 进程在、`OrbDropService` `isForeground=true` ✓ |

**没能在设备上自动验完的一步**：清单地址是编译期常量，而两个仓库还没建，所以设备上跑不了"检查到
新版本 → 下载 → 拉起系统安装器"这一段；而且 WebView 的内容不进 Android 的无障碍树，脚本也没法替
用户点那颗「下载并安装」。仓库建好之后的第一次真机验证就是：手机上点一次「检查更新 → 下载并安装」，
确认走到系统安装器（见 §11 第 6 条）。

**演练抓到并修掉的真问题**：`--gh-hang` 那一档最初把整个更新**永久卡死**——只在 body 流上加了首字节
超时，而 `send()` 等响应头那一步没人管（对方建好连接、收下请求、什么都不回）。现在等响应头与等第一块
数据共用同一个 5 秒超时（见 §5 的表）。没有这一条，"GitHub 连上了但不回数据"的用户会卡在"正在下载"
上永远不动。

## 11. 验收清单（改更新链路时重跑）

1. `node scripts/release.mjs --local --skip-build --version <版本>`：本机发布一次，拿到镜像台命令。
2. `node scripts/dev-mirror.mjs --port 8788 --gh-hang --gitee-kbps 0`：起镜像台（默认伺服本仓根的 `assets/`）。
3. 桌面：把上一版 exe 放到一个临时目录、按 §10 的方式启动它（`ORBDROP_UPDATE_MANIFEST` 指向
   `/manifest.json`、`ORBDROP_ALLOW_MULTI=1`、`ORBDROP_DATA_DIR` 指到临时目录），点界面上的
   「立即更新」；确认 exe 被换掉、重启后版本变了、`<数据目录>/logs/update.log` 里有 helper 的记录。
4. 重跑 §10 的镜像三档（`--gh-hang` / `--gh-kbps 5` / 两者都坏）：该成功的成功，该失败的一次都不装。
5. `cargo test --lib update::`：签名与地址策略的回归。**联网的那条另跑**：
   `cargo test --lib update:: -- --ignored`（`manifest_endpoints_really_load_over_https` —— 用客户端
   自己的 reqwest 拿一遍编译期那两条清单地址，钉住 §13 那个信任库；两条里至少一条通就算过）。
6. 安卓：`npm run tauri -- android build --apk`（通用包）与 `--apk --split-per-abi`（arm64 包）→ 装到
   设备上确认能起来（旧的 versionCode 更大时要先卸载）；清单地址与仓库建好后，在手机上点一次
   「检查更新 → 下载并安装」，确认走到系统安装器。
7. `node scripts/release.mjs --verify-only --version <版本>`：随时复核已发布的那一版（只读远端）。
8. 主动提示那一格（§16）：清单版本**高于**本机时，桌面右下角要自己多出一条「有新版本」气泡、悬浮球
   角标 +1（手机是一张「发现新版本」底板）；清单版本**等于**本机时一格都不该出现。点「立即更新」
   之后同一格要变成进度、失败要留在那一格里（打桩包的 sha256 必然对不上，正好当失败那一档的反例）。

## 12. 首次真实发布（2026-10-10）

`node scripts/release.mjs --version 0.0.1`（远端模式）跑通，两个平台都上线：

- GitHub Release `#408765799`（`draft=false` / `prerelease=false`，`latest` 别名可用）与 Gitee
  Release `#1194710`，各 4 个附件：`orbdrop.exe` 13.82MB、通用包 53.17MB、arm64 包 15.30MB、
  `update.json`。
- 发布仓：`master` 上两个提交（骨架 + `Released v0.0.1`），tag `v0.0.1` 推到两个远端。
- **独立核验**（不信脚本自己的输出）：Gitee 侧三个产物下载回来与本地**逐字节相同**（sha256 一致）、
  仓库根的 `update.json` 与本地清单逐字段一致；GitHub 侧用 `api.github.com` 核对 Release 与四个附件体积。
- 这台机器到 github.com 的下载域名时通时不通（Gitee 那几条全 200；GitHub 三条里一条 206、两条超时）
  —— 这正是客户端"GitHub 优先、失败/太慢切 Gitee"存在的理由。

首次真跑远端发布还修掉两个**只有真跑才会暴露**的脚本缺陷（都已修，注释留在代码里）：

1. `githubHeaders` / `ctypeOf` 用 `const` 箭头定义在调用点**之后** —— ESM 里 `const` 不提升，
   `publishGithub()` 一进去就 `ReferenceError`。现在全改成函数声明。
2. 产物守卫拿 mtime 当失败条件：**增量构建在输入没变时本来就不会重写输出**（cargo 判定无事可做、
   Gradle 判定打包任务 up-to-date），于是"包确实是当前源码编的"也被判成旧包。现在 mtime 只当提示，
   真正的把关是 APK 内部 versionCode/versionName 与清单一致。

## 13. https 清单这条路走过一次真机（2026-10-10）

**0.0.1 发出去的两端，检查更新一条地址都拿不到**，界面上只有一句：

```
所有清单地址都拿不到更新信息（https://github.com/x177plus/OrbDrop/releases/latest/download/update.json：
error sending request for url (…)：client error (Connect)：tcp connect error：deadline has elapsed；
https://gitee.com/x177plus/OrbDrop/raw/master/update.json：
error sending request for url (…)：client error (Connect)：invalid peer certificate: UnknownIssuer）
```

两条都失败，但**原因不是同一个**，而且 Gitee 那条根本不是网络问题：

- **Gitee：`invalid peer certificate: UnknownIssuer`**。`reqwest` 是按 `rustls-tls-no-provider`
  （= manual-roots）编的，**信任库是空的**。到对端那一跳（`net::client_builder`）自带 pinning
  校验器、用不到信任库，所以整条内网链路一直是好的；而自更新这两条清单地址连的是**公网域名**，
  只能走普通证书校验 —— 于是每条 https 都在握手这一步死掉。改成 reqwest 的
  `rustls-tls-webpki-roots-no-provider` 后修复（默认 `Client::builder()` 带上内置的公共 CA 列表；
  带 `-no-provider` 的那个变体才不会顺带把 quinn 一族解进 `Cargo.lock`）。同时也挡掉了
  "设备上装个根证书就能 MITM 更新"。
- **GitHub：`tcp connect error … deadline has elapsed`**。3 秒连接超时按设计走完（国内直连
  github.com 的常态），切 Gitee —— 切过去撞上上面那条。

**为什么之前三次演练都没抓到**：本机镜像台是**明文 http**（`http://127.0.0.1:8788/manifest.json`），
§10 那三档镜像行为演练全跑在明文上（明文不碰证书，所以对这条缺陷完全免疫）；发布侧的回读核验用的是
`curl`，走的是系统信任库，不是客户端那条路。结论写进验收清单：**§11 第 6 条那句"点一次检查更新"
是这条路上唯一有效的验证**，别用明文镜像台替代它。

同一趟还修掉了**报错本身**：`reqwest::Error` 的 `Display` 只有一句 `error sending request for url`，
真因全挂在 `source()` 链上，客户端原先直接 `to_string()` —— 所以连 `UnknownIssuer` 都看不见。
现在 `update::err_chain()` 把整条链拼进错误信息（清单与下载两条路都用它）。

**影响面（必须知道）**：**发出去了的** 0.0.1（exe 与两个 APK）都带着这个缺陷，它们**无法从应用内更新** ——
发布 0.0.2 解决不了已装 0.0.1 的用户，README/发行版页手动下一次装上是唯一的路。0.0.1 的清单里
`version` 又正好等于本机版本，所以修好之后的第一台 0.0.1 客户端也只会报"已是最新版"：这条链路
只能用**更高的版本号**验。

**补发记录（2026-10-10，同版本重发）**：用 `--republish` 把修好的 0.0.1 重发了一遍 ——
GitHub Release `#408765799`（复用，四个附件先删后传）、Gitee Release `#1195098`（旧的 `#1194710`
删掉重建），发布仓 `master` 前进一个提交 `2459e41`（**tag `v0.0.1` 不动**，仍指向 `bf6537e`）。
独立核验（不信脚本自己的输出）：

- Gitee 三个附件下载回来与本地构建产物**逐字节相同**，且与清单里的 sha256/大小逐项一致；
- GitHub 侧用 `api.github.com` 读四项附件的体积与**服务端算的 sha256 digest**，与清单逐项一致；
- 仓库根 `update.json`（Gitee raw 那条路）与本地清单**逐字段一致**；
- 两个安卓包与**两台真机上跑的那份 APK**哈希相同（arm64 `0a9f0014…46d947`）—— 真机验过的那一份
  就是发出去的那一份。

于是"新装的人拿到修好的 0.0.1"，而"已经装着旧 0.0.1 的人"仍需手动装一次（同版本号重发不会让
客户端认为自己有更新）。

## 14. 还没被真实验证过的那一格

**安卓端"检查到新版本 → 下载 → 拉起系统安装器"**：需要一台装着**更低版本**的真机。0.0.2 已经发出去
（见 §15），清单地址两端都可读（本机侧核验过），缺的只是设备上那一下：

- 装一个 0.0.1 的 APK（或在 0.0.2 上改清单地址指到本机镜像台重建 APK —— 但那条是明文 http，
  验不了 §13 那个信任库，只能验"下载 → 拉起系统安装器"那一段）；
- 在手机上点一次「检查更新 → 下载并安装」，确认走到系统安装器（§11 第 6 条）。

**桌面端同理**：0.0.1 的桌面端因为 §13 那个缺陷升不上来，所以"0.0.1 应用内升到 0.0.2"这条路
在真机上也没被走过 —— 本机镜像演练（§10）验的是同一套代码，清单地址那条路现在有了 §15 的核验。

## 15. 0.0.2 发布记录（2026-10-12）

产物先手动构建（`tauri build --no-bundle` → `tauri android build --apk` → `--apk --split-per-abi`），
再跑 `node scripts/release.mjs --version 0.0.2 --skip-build --notes "…"`（签名 → 清单 → 落发布仓 →
commit/tag/push → 建 Release 传附件 → 回读校验）。两个平台都上线：

- GitHub Release `#410149067`（`draft=false` / `prerelease=false`，`latest` 可用）、Gitee Release
  `#1199061`，各 4 个附件：`orbdrop.exe` 13.26MB、通用包 23.91MB、arm64 包 7.79MB、`update.json`。
  （0.0.1 那两个包是 53.17MB / 15.30MB —— 这一版安卓原生库改压缩存储，体积约减半。）
- 发布仓 `master` 前进一个提交 `24ee755`，tag `v0.0.2` 推到两个远端；源码仓（私有）同步提交
  `8b4582a` 并打同名 tag —— 这一份就是 0.0.2 各平台二进制的源码。
- 版本号四件套对齐：`package.json` / `package-lock.json` / `tauri.conf.json` / `Cargo.toml` 都是
  0.0.2，`Cargo.lock` 随构建同步；exe 的 `FileVersion` 与两个 APK 的 `versionName`/`versionCode`
  （`0.0.2` / `2`）都在构建产物里核过。

**独立核验（不信脚本自己的输出）**：

- GitHub 侧用 `api.github.com` 读三个附件的体积与**服务端算的 sha256 digest**，与清单逐项一致；
  `update.json` 附件的字节与发布仓根目录那份**逐字段一致**（两条清单地址指向同一份内容）。
- Gitee raw 的 `update.json` 与本地清单逐字段一致；三个附件下载回来与本地构建产物**逐字节相同**。
- 把发出去的 `orbdrop.exe`（从 Gitee 下载）喂给**客户端自己的** `update::verify_file`：sha256 与
  minisign（公钥 `UPDATE_PUBKEY` 编在二进制里）都过 —— 本仓第一次拿真实产物走完整条校验链。
- `cargo test --lib update:: -- --ignored`：Gitee 那条清单地址被**客户端自己的 reqwest** 拿到了
  （GitHub 那条本机连不上，按设计不判失败）—— §13 那个空信任库的缺陷，在这一版上验过是修好的。
- 夹具 `tests/fixtures/release-update.json` 换成 0.0.2 这份**真实发布清单**（原先那份是 `--local`
  演练产物）：接缝测试自此钉的是"发出去的那份清单能被客户端解析、地址策略放行"。

**同版本重发（2026-10-12）**：0.0.2 发出之后又加了"启动时的静默检查查到新版要主动弹一格"（§16），
用 `--republish` 把产物换掉、**版本号不动**：

- **发布仓** `master` 前进一个提交 `ee8b507`（tag `v0.0.2` 不动，仍指向 `24ee755`）；**源码仓**的改动
  是提交 `34e9751`（源仓那个 tag `v0.0.2` 同样不动，仍指向 `8b4582a` —— 源仓 tag 自此不再与发出去的
  二进制一一对应，取舍见 §13）。
- **GitHub 复用** Release `#410149067`（四个附件先 DELETE 再传）；**Gitee 重建** —— 旧的 `#1199061`
  连同附件删掉、换成 `#1199193`。两边的四个附件与仓库根清单都换成新的这一份。
- 三个产物的 sha256：`orbdrop.exe` `54f3b254…`、通用包 `a009e1bf…`、arm64 包 `c0e2a205…`
  （**与第一批不同**：0.0.2 首发那一份是 `5391b24e…` / `42fe51cb…` / `02b93979…`）。
- 独立核验照跑：GitHub 侧 API 报的体积与**服务端算的 digest** 与清单逐项一致（其中两份还把 CDN 上的
  字节拉回来逐字节对过，arm64 那一份这次本机连不上那个 CDN，用 Gitee 侧同内容的产物兜住）；
  GitHub 的 `update.json` 附件与发布仓根目录那份**逐字节相同**；Gitee 三个附件下载回来与本地构建
  产物逐字节相同。
- 把重发后的 `orbdrop.exe`（从 Gitee 下载）喂给客户端自己的 `update::verify_file`：sha256 与 minisign
  都过；夹具 `tests/fixtures/release-update.json` 同步换成这份重发后的清单（`update::` 10 项全过）。

## 16. 启动时的静默检查发现新版会主动弹一格（2026-10-12）

原话是"每次启动静默检测一次版本，要更新就弹通知提示"。§8 那次静默检查本来就在跑，缺的是"查到了
有人告诉用户"——原先它只把阶段推成 `available`，而那个阶段**只有打开设置面板才看得见**。

改法（两端各用自己那套通知位，状态与文案仍是同一份）：

- 后端快照多一个 `silent`（`check(force = false)` 添真，见 `update::UpdateState`）：界面靠它区分
  "后台自己查到的"与"用户要看的结果"——用户自己点的那次已经有设置面板在显示，不该再弹一遍。
- 共用 store（`src/lib/stores/update.svelte.ts`）多三个出口：`updatePrompt()`（该不该主动弹）、
  `updateTask()`（那一格此刻该不该占位：提示，或点过「立即更新」之后的进度/失败）、
  `dismissUpdatePrompt()`（按掉了，本次运行不再自动弹；下次启动是另一个进程，重新查、重新弹）。
- 桌面：那一格就是右下角通知组里的一条气泡（排在"等你确认"那一档，见 `src/App.svelte`），并进
  同一个铃铛与悬浮球角标 —— 面板收着时那颗黄标是唯一还看得见的提示。
- 安卓：启动起就订阅 `update://state`（原先只有设置页挂载才订阅），`updatePrompt()` 一非空就开一张
  「发现新版本」底板（`src/mobile/sheets/UpdateSheet.svelte`）；**只在没有别的底板时弹** ——
  配对邀请只有 60 秒，它优先，等它关掉这张会自己补上。

校验（都不信"看起来对"）：

- `cargo test --lib update::`：9 项全过（含这一改之后的编译）。
- `npm run check`：svelte-check 141 文件 0 错。
- **移动端界面在真实 Chromium 里跑**（`npm run dev:mobile` + 一份 `window.__TAURI_INTERNALS__`
  打桩 IPC，事件由打桩的 `__OD_EMIT__` 按真实回调形状 `{event, id, payload}` 派发）：idle 不弹；
  `silent:true` + `available` → 弹出「有新版本 v0.0.3 / v0.0.3 · 8.0 MB」+ 稍后/下载并安装；
  点「下载并安装」发出 `update_apply`，收到 downloading 快照后**同一张板**变进度
  （"正在从 gitee.com 下载 3.0 MB / 8.0 MB"、进度条 38%），收到 error 快照后变「更新失败 +
  关闭/重试」；按掉之后再来一份 `silent:true` 不再弹；`silent:false`（用户自己点的那次）
  **从不弹**；配对邀请那张板开着时不抢位置、它关掉之后补上。全程 0 报错。
- **真桌面端**（`tauri dev` + `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS=--remote-debugging-port=9222`，
  CDP 挂上真 webview 读 DOM）：`ORBDROP_UPDATE_MANIFEST` 指向本机一份打桩清单（0.0.3）→ 起来 8 秒后
  右下角自己多出「有新版本 v0.0.3 · 20 B · 本机演练…」+ 稍后/立即更新，悬浮球角标 `1`；点「立即更新」
  → **真下载** 20 B → 校验拒装 → **同一格**变「更新失败：sha256 对不上（清单 0000… ≠ 实际 6bc1…）」
  + 知道了/重试；清单换成 0.0.2（等于本机版本）→ 一格都不出现、角标 `0`。两次都是 0 报错。
- 演练抓到一个真问题：失败之后那一格右上角还写着 `100%`（百分比原先只按"有没有 size"画）——
  现在只在 downloading/verifying 两档报百分比，其余报"要下多大"。

## 17. 单仓化与 0.0.3 发布记录（2026-10-19）

**单仓化**（取消 `../OrbDrop-releases`）：发布与源码合到**同一个仓、同一个分支 `master`**，两个远端
（GitHub / Gitee）同址。并入方式是把发布仓的历史**合并**进 master（`git merge --allow-unrelated-histories
releases/master`；冲突只有 `README.md` 与 `.gitignore`，都取本仓版本），所以发布仓那 7 个提交与
`v0.0.1` / `v0.0.2` 两个 tag 都还在、**没有 force push**；源码侧的对应关系统一用 `src-vX.Y.Z`
（`v0.0.2` → `8b4582a`）。构建产物 `dist/`、`dist-mobile/` 与演练缓存 `assets/` 不进库；
`update.json` 与 `CHANGELOG.md` 落在仓库根，后者由发布脚本自己维护。

**发布**：`node scripts/release.mjs --version 0.0.3 --notes "…"` 一次跑完（构建 → 签名 → 清单 → 提交
`7152f1a` + tag `v0.0.3` → 推两个远端 → 建 Release → 回读）。

- GitHub Release `#416841298`（`v0.0.3`）：`orbdrop.exe` 16,179,200 B、通用包 27,830,179 B、
  arm64 包 8,851,243 B、`update.json` 3,749 B。
- Gitee Release `#1217281`：同样四个附件（Gitee 侧累计附件约 174 MB）。
- 三个产物的 sha256：`orbdrop.exe` `143c081a…`、通用包 `205316f9…`、arm64 包 `21f56174…`。
- 核验：Gitee `raw/master/update.json` HTTP 200、`version 0.0.3 / versionCode 3`，清单里的 exe
  sha256 与本机 `target/release/orbdrop.exe` 现算的一致；GitHub 侧走 API 看到的四个附件体积与 Gitee
  那份清单逐项对上。
- **本机网络如实记一条**：这台机器到 `github.com` 的**下载**路径（`releases/latest/download/*` 与附件
  CDN）会 connect timeout，而走 `api.github.com` 的**上传**是通的 —— 所以 `--verify-only` 里 GitHub 那
  几条"够不着"只是本机出口的问题；客户端在这台机器上会按设计退到 Gitee 那条清单。