# OrbDrop 发布仓

OrbDrop 是一个**局域网文件中转**工具：主体是一个始终置顶的悬浮球，点开就是主界面。
两台设备配对之后有两条文件通路：

- **拖入可见（零拷贝）**：把目录/文件拖进悬浮球，指定给哪些已配对设备可见 —— 对端实时浏览、下载、
  比较，改动直接写回源文件，文件始终留在本机。
- **推送（真传输）**：选中文件/目录指定目标设备推送 —— 已配对设备直接收，未配对才弹同意，
  文件真的落到对端磁盘。

只在局域网内使用，不做公网穿透。
本仓只放**发行版产物与文档**，源码暂未公开。

- Windows 桌面端：单文件 `orbdrop.exe`（绿色版，不带安装器）
- Android 端：`app-universal-release.apk`（通用包）与 `app-arm64-release.apk`（arm64 包）

> **仓库地址待填**：发布仓的 GitHub/Gitee 仓库还没建，下面表格里的 `<owner>/<repo>` 是占位。
> 建好仓库后把 `owner`/`repo` 填进发布配置，再跑一次发布（`scripts/release.mjs` 会重写本文件）。

## 下载

| 平台 | 文件 | GitHub | Gitee |
| --- | --- | --- | --- |
| Windows 桌面端（x86_64） | `orbdrop.exe` | [最新版](https://github.com/<owner>/<repo>/releases/latest) | [最新版](https://gitee.com/<owner>/<repo>/releases) |
| Android（通用包） | `app-universal-release.apk` | [最新版](https://github.com/<owner>/<repo>/releases/latest) | [最新版](https://gitee.com/<owner>/<repo>/releases) |
| Android（arm64） | `app-arm64-release.apk` | [最新版](https://github.com/<owner>/<repo>/releases/latest) | [最新版](https://gitee.com/<owner>/<repo>/releases) |

APK 装不上时按机型选包：手机上绝大多数是 arm64（`app-arm64-release.apk` 更小、更省内存），
拿不准就用通用包。

## 更新清单

客户端里的清单地址是编译期常量（不可改），所以本仓承担两个固定位置：

- GitHub：Release 附件 `update.json`（`https://github.com/<owner>/<repo>/releases/latest/download/update.json`）
- Gitee：仓库根目录文件 `update.json`（`https://gitee.com/<owner>/<repo>/raw/master/update.json`）

清单里的下载地址都钉了 tag（`releases/download/vX.Y.Z/...`），老版本客户端不会因为新版本发布而下错包。

## 校验

`update.json` 里每个产物都带 `size` 与 `sha256`，Windows 版另有 Tauri 更新签名（`sig`）；
下载后可以自己核对：`sha256sum orbdrop.exe`。

## 许可证

MIT，见 [LICENSE](./LICENSE)。
