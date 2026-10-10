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

## 下载

| 平台 | 文件 | GitHub | Gitee |
| --- | --- | --- | --- |
| Windows 桌面端（x86_64） | `orbdrop.exe` | [最新版](https://github.com/x177plus/OrbDrop/releases/latest) | [最新版](https://gitee.com/x177plus/OrbDrop/releases) |
| Android（通用包） | `app-universal-release.apk` | [最新版](https://github.com/x177plus/OrbDrop/releases/latest) | [最新版](https://gitee.com/x177plus/OrbDrop/releases) |
| Android（arm64） | `app-arm64-release.apk` | [最新版](https://github.com/x177plus/OrbDrop/releases/latest) | [最新版](https://gitee.com/x177plus/OrbDrop/releases) |

APK 装不上时按机型选包：手机上绝大多数是 arm64（`app-arm64-release.apk` 更小、更省内存），
拿不准就用通用包。

## 更新清单

客户端里的清单地址是编译期常量（不可改），所以本仓承担两个固定位置：

- GitHub：Release 附件 `update.json`（`https://github.com/x177plus/OrbDrop/releases/latest/download/update.json`）
- Gitee：仓库根目录文件 `update.json`（`https://gitee.com/x177plus/OrbDrop/raw/master/update.json`）

清单里的下载地址都钉了 tag（`releases/download/vX.Y.Z/...`），老版本客户端不会因为新版本发布而下错包。

## 校验

`update.json` 里每个产物都带 `size` 与 `sha256`，Windows 版另有 Tauri 更新签名（`sig`）；
下载后可以自己核对：`sha256sum orbdrop.exe`。

## 许可证

**GNU General Public License v3.0 或更高版本**（`GPL-3.0-or-later`），全文见 [LICENSE](./LICENSE)。

Copyright (C) 2026 x177plus

This program is free software: you can redistribute it and/or modify it under the terms of
the GNU General Public License as published by the Free Software Foundation, either version 3
of the License, or (at your option) any later version.

This program is distributed in the hope that it will be useful, but WITHOUT ANY WARRANTY;
without even the implied warranty of MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.
See the GNU General Public License for more details.

用大白话说清它管什么、不管什么（原文为准）：

- **可以**：随便用（含公司内部使用）、随便改、随便分发，**包括拿它赚钱**（卖拷贝、卖技术支持都行）。
- **必须**：分发（含分发改过的版本、把改过的版本做成产品给别人）时要**给出源码**，且整体仍按 GPL 授权；
  改了要说清楚改了哪儿，不能把原作者的名字拿去给你自己的版本背书。
- **拿不到什么**：GPL **不禁止**别人商用，也不会自动让原作者分到钱 —— 想从商用里获益得走双授权
  （即「公开版 GPL + 商业版另行授权」），那需要作者保留全部版权（接受外部贡献时先签 CLA）。
- **管不到**：程序名称与图标不随代码授权走。别人可以合法地用这份代码做一个改过的版本，但不该冒用
  「OrbDrop」这个名字与图标让人误以为是官方发布。
