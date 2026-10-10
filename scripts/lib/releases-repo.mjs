/**
 * 发布仓（`E:/177plus/Apps/OrbDrop-releases`）的四个文件：README / LICENSE / CHANGELOG / update.json。
 *
 * update.json 由 release.mjs 自己拼（要 sha256、sig、mirrors），这里只管另外三个文本文件。
 *
 * 两个刻意的设计：
 *  1. README 每次发布都**重写**：里面的下载表格是用配置里的 owner/repo 拼的，
 *     冷启动时 owner 还是空的（占位链接），填上以后必须自动变成真链接，不能靠人肉改文档。
 *  2. 发布仓**不提交** `assets/`：那是本机演练用的产物缓存（镜像台直接伺服它），
 *     几 GB 的二进制进 git 会让仓库爆掉 —— 所以发布仓根目录有一个 `.gitignore` 只忽略它。
 */
import fs from "node:fs";
import path from "node:path";
import { ASSET_NAMES, ROOT, note, ok, rel } from "./release-common.mjs";

/**
 * 许可证：**直接引用源码仓根目录的 `LICENSE`**（GPL-3.0-or-later 全文），不在这里另抄一份。
 *
 * 两处一份才不会有"源码仓换了协议、发布仓还写着旧的"这种事 —— 而协议写错是要出事的：
 * 已经发出去的二进制，收件人拿到的就是那一刻的授权，改不回来。
 * 发布仓那份是**副本**（发布仓独立成仓，读者不该被要求去别的仓找许可证）。
 */
function licenseText() {
  const src = path.join(ROOT, "LICENSE");
  if (!fs.existsSync(src)) {
    throw new Error(`源码仓根目录没有 LICENSE（${src}）：发布仓的许可证从这里取，别各写一份`);
  }
  return fs.readFileSync(src, "utf8");
}

const GITIGNORE = `# 本机演练用的产物缓存：镜像台（scripts/dev-mirror.mjs）直接伺服这个目录。
# 正式发布的二进制在 GitHub/Gitee 的 Release 附件里，不进 git（十几 MB × 每个版本）。
/assets/
`;

const CHANGELOG_HEAD = "# 更新日志\n";

/** owner/repo 都没填（冷启动）时，表格里的链接只能是占位。 */
function repoBases(cfg) {
  const ghReady = Boolean(cfg.github.owner && cfg.github.repo);
  const gtReady = Boolean(cfg.gitee.owner && cfg.gitee.repo);
  return {
    ghReady,
    gtReady,
    gh: ghReady ? `https://github.com/${cfg.github.owner}/${cfg.github.repo}` : "https://github.com/<owner>/<repo>",
    gt: gtReady ? `https://gitee.com/${cfg.gitee.owner}/${cfg.gitee.repo}` : "https://gitee.com/<owner>/<repo>",
  };
}

/** 生成/刷新发布仓 README：产品简介 + 下载表格 + 清单地址 + 「只放产物」声明。 */
export function renderReadme(cfg) {
  const { ghReady, gtReady, gh, gt } = repoBases(cfg);
  const pending = !ghReady || !gtReady;
  const lines = [
    "# OrbDrop 发布仓",
    "",
    "OrbDrop 是一个**局域网文件中转**工具：主体是一个始终置顶的悬浮球，点开就是主界面。",
    "两台设备配对之后有两条文件通路：",
    "",
    "- **拖入可见（零拷贝）**：把目录/文件拖进悬浮球，指定给哪些已配对设备可见 —— 对端实时浏览、下载、",
    "  比较，改动直接写回源文件，文件始终留在本机。",
    "- **推送（真传输）**：选中文件/目录指定目标设备推送 —— 已配对设备直接收，未配对才弹同意，",
    "  文件真的落到对端磁盘。",
    "",
    "只在局域网内使用，不做公网穿透。",
    "本仓只放**发行版产物与文档**，源码暂未公开。",
    "",
    "- Windows 桌面端：单文件 `orbdrop.exe`（绿色版，不带安装器）",
    "- Android 端：`app-universal-release.apk`（通用包）与 `app-arm64-release.apk`（arm64 包）",
    "",
  ];
  if (pending) {
    lines.push("> **仓库地址待填**：发布仓的 GitHub/Gitee 仓库还没建，下面表格里的 `<owner>/<repo>` 是占位。");
    lines.push("> 建好仓库后把 `owner`/`repo` 填进发布配置，再跑一次发布（`scripts/release.mjs` 会重写本文件）。");
    lines.push("");
  }
  lines.push(
    "## 下载",
    "",
    "| 平台 | 文件 | GitHub | Gitee |",
    "| --- | --- | --- | --- |",
    `| Windows 桌面端（x86_64） | \`${ASSET_NAMES.exe}\` | [最新版](${gh}/releases/latest) | [最新版](${gt}/releases) |`,
    `| Android（通用包） | \`${ASSET_NAMES.universal}\` | [最新版](${gh}/releases/latest) | [最新版](${gt}/releases) |`,
    `| Android（arm64） | \`${ASSET_NAMES.arm64}\` | [最新版](${gh}/releases/latest) | [最新版](${gt}/releases) |`,
    "",
    "APK 装不上时按机型选包：手机上绝大多数是 arm64（`app-arm64-release.apk` 更小、更省内存），",
    "拿不准就用通用包。",
    "",
    "## 更新清单",
    "",
    "客户端里的清单地址是编译期常量（不可改），所以本仓承担两个固定位置：",
    "",
    `- GitHub：Release 附件 \`${ASSET_NAMES.manifest}\`（\`${gh}/releases/latest/download/${ASSET_NAMES.manifest}\`）`,
    `- Gitee：仓库根目录文件 \`${ASSET_NAMES.manifest}\`（\`${gt}/raw/${cfg.gitee.manifestBranch}/${ASSET_NAMES.manifest}\`）`,
    "",
    "清单里的下载地址都钉了 tag（`releases/download/vX.Y.Z/...`），老版本客户端不会因为新版本发布而下错包。",
    "",
    "## 校验",
    "",
    "`update.json` 里每个产物都带 `size` 与 `sha256`，Windows 版另有 Tauri 更新签名（`sig`）；",
    "下载后可以自己核对：`sha256sum orbdrop.exe`。",
    "",
    `## 许可证`,
    "",
    "**GNU General Public License v3.0 或更高版本**（`GPL-3.0-or-later`），全文见 [LICENSE](./LICENSE)。",
    "",
    "Copyright (C) 2026 x177plus",
    "",
    "This program is free software: you can redistribute it and/or modify it under the terms of",
    "the GNU General Public License as published by the Free Software Foundation, either version 3",
    "of the License, or (at your option) any later version.",
    "",
    "This program is distributed in the hope that it will be useful, but WITHOUT ANY WARRANTY;",
    "without even the implied warranty of MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.",
    "See the GNU General Public License for more details.",
    "",
    "用大白话说清它管什么、不管什么（原文为准）：",
    "",
    "- **可以**：随便用（含公司内部使用）、随便改、随便分发，**包括拿它赚钱**（卖拷贝、卖技术支持都行）。",
    "- **必须**：分发（含分发改过的版本、把改过的版本做成产品给别人）时要**给出源码**，且整体仍按 GPL 授权；",
    "  改了要说清楚改了哪儿，不能把原作者的名字拿去给你自己的版本背书。",
    "- **拿不到什么**：GPL **不禁止**别人商用，也不会自动让原作者分到钱 —— 想从商用里获益得走双授权",
    "  （即「公开版 GPL + 商业版另行授权」），那需要作者保留全部版权（接受外部贡献时先签 CLA）。",
    "- **管不到**：程序名称与图标不随代码授权走。别人可以合法地用这份代码做一个改过的版本，但不该冒用",
    "  「OrbDrop」这个名字与图标让人误以为是官方发布。",
    "",
  );
  return lines.join("\n");
}

/** 建/补齐发布仓骨架文件（README 每次都重写，其余只在缺失时创建，不覆盖人写的内容）。 */
export function ensureRepoFiles(repo, cfg) {
  fs.mkdirSync(repo, { recursive: true });
  const abs = (f) => path.join(repo, f);
  fs.writeFileSync(abs("README.md"), renderReadme(cfg));
  fs.writeFileSync(abs(".gitignore"), GITIGNORE);
  // LICENSE **每次都覆盖**（不是"缺失才建"）：协议换过一次（MIT → GPL-3.0-or-later），
  // 而旧的那份还躺在发布仓里 —— 读者看到哪一份就是哪一份授权，不能靠"它已经存在"就跳过。
  fs.writeFileSync(abs("LICENSE"), licenseText());
  for (const [file, content] of [["CHANGELOG.md", CHANGELOG_HEAD]]) {
    if (fs.existsSync(abs(file))) continue;
    fs.writeFileSync(abs(file), content);
    ok(`创建 ${rel(abs(file))}`);
  }
  return repo;
}

const entryOf = (cfg, version, notes, date) =>
  [`## ${cfg.tagPrefix}${version} — ${date}`, "", notes.trim(), ""].join("\n");

/**
 * 写 CHANGELOG：同版本已存在就替换那一段（重跑同版本时说明文案会变），否则插到最前。
 * 只是文本操作，纯 Node 实现，不引依赖。
 */
export function upsertChangelog(repo, cfg, version, notes, date) {
  const file = path.join(repo, "CHANGELOG.md");
  const head = `## ${cfg.tagPrefix}${version} `;
  const text = fs.existsSync(file) ? fs.readFileSync(file, "utf8") : CHANGELOG_HEAD;
  // 按 `## ` 二级标题切块；第一块是标题前言（`# 更新日志`），原样保留在最前面。
  // 每块都 trimEnd 后补一个换行、再用 `\n` 拼 —— 否则每次重跑同一版本都会多攒出一个空行。
  const parts = text.replace(/\r\n/g, "\n").split(/^(?=## )/m);
  const preamble = `${parts[0].trimEnd() || CHANGELOG_HEAD.trimEnd()}\n`;
  const blocks = parts
    .slice(1)
    .filter((b) => b.trim() && !b.startsWith(head))
    .map((b) => `${b.trimEnd()}\n`);
  const next = [preamble, `${entryOf(cfg, version, notes, date).trimEnd()}\n`, ...blocks].join("\n");
  fs.writeFileSync(file, next);
  note(`CHANGELOG.md 已记录 ${cfg.tagPrefix}${version}`);
}

/** 老版本号在 CHANGELOG 里有没有条目（只用于提示，不做硬校验）。 */
export function changelogHas(repo, cfg, version) {
  const file = path.join(repo, "CHANGELOG.md");
  if (!fs.existsSync(file)) return false;
  return fs.readFileSync(file, "utf8").includes(`## ${cfg.tagPrefix}${version} `);
}

/** 打印发布仓里被跟踪的文件（回读用，顺便让人看清这一版到底落了什么）。 */
export function listTracked(repo) {
  const out = [];
  const walk = (dir) => {
    for (const e of fs.readdirSync(dir, { withFileTypes: true })) {
      if (e.name === ".git" || e.name === "assets") continue;
      const p = path.join(dir, e.name);
      if (e.isDirectory()) walk(p);
      else out.push(path.relative(repo, p).split(path.sep).join("/"));
    }
  };
  walk(repo);
  return out.sort();
}