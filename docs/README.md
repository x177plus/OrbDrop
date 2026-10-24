# OrbDrop 文档

这个目录是**改代码的人**的地图：8 份文档 + 1 份定点审计 + 本索引，全部为简体中文。
面向使用者/下载者的入口在根目录：[README.zh-CN.md](../README.zh-CN.md)（中文）、[README.md](../README.md)（英文）。

## 一、文档清单

| 文档 | 内容 | 状态 | 配套代码 |
|---|---|---|---|
| [requirements.md](requirements.md) | 需求文档 v0.5：功能需求 F-01…F-110、开关与策略 S-01…S-12、非功能需求 N-01…，附风险与取舍表；§5 技术方案 | **现行**（活文档，需求条目是引用单位；§5 仍是草案） | 需求编号（F-xx / S-xx / N-xx）是整个仓库的引用单位 |
| [orb-functional-design.md](orb-functional-design.md) | 悬浮球形态提案：形态与状态机、条目盘布局、手势消歧、贴边/角标/多显示器 | **提案** —— 方向已定、细节待评审；§9 的需求增量**未并入** `requirements.md` | `src/lib/components/Orb.svelte`、`RectOrb.svelte`、`src/lib/orbShape.ts` |
| [p0-implementation.md](p0-implementation.md) | P0（单机 MVP）实现起点：脚手架、Rust 依赖与模块布局、Tauri 配置要点、构建顺序、校验清单 | **历史阶段记录**（P0 已完成；现行实现见 `p1-implementation.md`） | — |
| [p1-implementation.md](p1-implementation.md) | 实现主记录（38 节 / 313 KB）：发现、配对、推送与续传、变更通知、长连接、群聊与图片消息、桌面 UI…… | **现行**；文首有「后续改动」更正块，逐条标出已失效的旧结论 | 全仓 —— 代码注释按 `§节号` 引用本文，**节序是接口** |
| [cfapi-mirror.md](cfapi-mirror.md) | F-46 拖拽下载的第二条实现：CFAPI 占位符（0.22 ms / 0 字节）+ 目标进程真读时才从对端拉字节 | **现行**；核心链路已端到端验证，**实机拖放手势未验证**（§6） | `src-tauri/src/mirror/` |
| [p2-beyondcompare.md](p2-beyondcompare.md) | Beyond Compare 集成：`BCProfiles.xml` 注入与密码信封、回环代理与命名空间改写、写回 | **现行**；读路径已实测，**写回未用真机 BC 验证**（§4） | `src-tauri/src/bc.rs`、`src-tauri/src/server/proxy.rs` |
| [relay.md](relay.md) | 局域网中转：盲转（F-106）、跨网段中转记忆与失效转移（F-107）、锚点与反向传播、安卓当锚点 | **现行**（已实现；与设计的偏离与未验部分见 §17） | `src-tauri/src/relay.rs`、`server/routes_relay.rs`、`server/routes_discover.rs`、`config.rs` |
| [update.md](update.md) | 自更新与发行链路（F-110）：不用官方 updater 的理由、清单与信任链、Gitee 镜像切换、覆盖 exe、发布流程 | **现行**（运维向） | `src-tauri/src/update/`、`scripts/release.mjs` |
| [audit-2026-09-26.md](audit-2026-09-26.md) | 定点全项目审计（只读）：P0 致命、P1、P2、功能缺失、未验证、修复状态 | **快照**（2026-09-26）；P0 与 P1 已全部修复，逐条见 §7 | — |

## 二、建议阅读顺序

1. [requirements.md](requirements.md) §1–3 —— 这个产品要做什么，两条文件通路（引用模式 / 推送）分别是什么语义。
2. [orb-functional-design.md](orb-functional-design.md) —— 悬浮球的形态与手势为什么是现在这样。
3. [p1-implementation.md](p1-implementation.md) —— 实现怎么长起来的；最大的一份，按需要挑节读，别通读。
4. 三个专题：[cfapi-mirror.md](cfapi-mirror.md)（拖出）、[p2-beyondcompare.md](p2-beyondcompare.md)（BC）、[relay.md](relay.md)（中转）。
5. [update.md](update.md) —— 发一版要动什么（发布链路与信任链）。
6. [audit-2026-09-26.md](audit-2026-09-26.md) —— 体检结论、安全面的取舍与已知缝。

## 三、引用约定（改文档之前先读这节）

- 代码、脚本与文档互引，一律写成 `docs/<文件>.md`；要指到节就带 `§<节号>`（例：`docs/p1-implementation.md §33`）。
- **`§` 节号与 `p1-implementation.md` 的节序是接口，不是排版**：全仓目前 42 处 `docs/<文件>.md` 形式的引用（含代码注释、脚本与文档互引；这个数字会随文档增加而变大）。**追加内容随便加，重排或重编号会打断引用。**
- **不要写行号引用**（`docs/x.md:412` 这种）—— 文档一增删就指错行；要指位置就写需求编号或节号。
- 每份文档标题下的 `> 版本 / 状态` 块是它自己的状态声明；上面第一节的「状态」列是同一件事的汇总。

## 四、还没验证的部分（欢迎认领）

| 事项 | 出处 |
|---|---|
| 真机 Beyond Compare 的写回（BC 保存时的 `PUT` / `DELETE` / `MKCOL` / `MOVE`）与 macOS `bcomp` | `p2-beyondcompare.md` §4、`audit-2026-09-26.md` §5 |
| 实机拖放手势：OLE 拖到资源管理器 / 微信 / QQ | `cfapi-mirror.md` §6、`audit-2026-09-26.md` §5 |
| 两台真实设备之间的表现：带宽、续传、群聊收敛、离线排队 | `audit-2026-09-26.md` §5 |
| Android 真机：mDNS 可达性（受 ROM 影响）、后台存活、SAF | `audit-2026-09-26.md` §5 |
| Android 真机：应用表（包可见性 + 直读别人的 `base.apk`）与「发送应用」端到端 | `p1-implementation.md` §41 |
| macOS 全部（开发机是 Windows） | `audit-2026-09-26.md` §5、`requirements.md` |

## 五、脱敏约定

文档里的机型、网段、人名都是**占位**，不是实测当时的那台设备/那张网：

| 文档里写的 | 含义 |
|---|---|
| `HUAWEI ABC-AL00`、`华为 ABC-AL00`、`手机甲` | 对端那台手机（`ABC-AL00` 不是任何在售机型） |
| `手机乙` | 本机那台手机 |
| `小明` | 示例用户名 / 示例设备名（原型与面包屑里） |
| `203.0.113.x`、`198.51.100.x` | 实测那两段的替代地址（RFC 5737 文档地址段） |
| `192.168.1.x`、`192.168.50.x`、`10.0.2.x` | 本来就是示例，不是实测环境 |

替换是**等长替换**，所以像素测量（`HUAWEI ABC-AL00` 要 151px、`203.0.113.198:3890` = 127.5px）与协议行为结论都不受影响。**面向用户的说明一律不点名品牌**；文档里保留的厂商名只出现在三类地方：① 功能标识（包名、组件名、系统属性键），② 历史量测夹具（`HUAWEI ABC-AL00` 这类等长占位，保证像素数据可复现），③ 外部商店/服务名等平台事实。它们不构成对具体设备或品牌的推荐。

## 六、语言

正文只有简体中文，没有译文。英文入口见根 [README.md](../README.md)；要做英文文档，请**按节翻译、
不要重排编号**（见第三节的引用约定）。