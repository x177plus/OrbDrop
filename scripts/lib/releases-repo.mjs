/**
 * 清单仓 = 源码仓（`E:/177plus/Apps/OrbDrop`）的落盘逻辑。
 *
 * 取消独立发布仓后，清单（`update.json`）与更新日志（`CHANGELOG.md`）直接落在**本仓根目录**：
 *  - Gitee 的清单地址是编译期常量 `raw/master/update.json`，所以文件必须在仓库根、发布分支必须 `master`；
 *  - 产物缓存落在根目录 `assets/v{ver}/`（本仓根 `.gitignore` 已忽略 `/assets/`，不进 git）。
 *
 * `update.json` 由 release.mjs 自己拼（要 sha256、sig、mirrors），这里只管文本文件的骨架与 CHANGELOG。
 * 本仓手写/既有的 `README.md` / `README.zh-CN.md` / `LICENSE` / `.gitignore` 脚本绝不碰。
 */
import fs from "node:fs";
import path from "node:path";
import { ASSET_NAMES, note, ok, rel } from "./release-common.mjs";

const CHANGELOG_HEAD = "# 更新日志\n";

/**
 * 建/补齐本仓根目录该有的骨架文件。
 *
 * 只有 `CHANGELOG.md` 归这里管（缺失时建出表头，内容由 `upsertChangelog` 维护）；
 * `README.md` / `README.zh-CN.md` / `LICENSE` / `.gitignore` 都是本仓手写/既有的，脚本不写。
 * 目录（本仓根）已经存在，`mkdirSync` 只是保住幂等。
 */
export function ensureRepoFiles(repo) {
  fs.mkdirSync(repo, { recursive: true });
  const changelog = path.join(repo, "CHANGELOG.md");
  if (!fs.existsSync(changelog)) {
    fs.writeFileSync(changelog, CHANGELOG_HEAD);
    ok(`创建 ${rel(changelog)}`);
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

/** 这一版真正落进仓库根的文件（回读用，顺便让人看清到底落了什么）。 */
export function listTracked(repo) {
  return [ASSET_NAMES.manifest, "CHANGELOG.md"].filter((f) => fs.existsSync(path.join(repo, f)));
}