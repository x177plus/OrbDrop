#!/usr/bin/env node
/**
 * 一处改版本号，三处同步写：`package.json`、`src-tauri/tauri.conf.json`、`src-tauri/Cargo.toml`。
 *
 * 为什么要脚本而不是手改：这三个文件任何一个漏改，产物里的版本号就跟你以为的不一致 ——
 * `tauri.conf.json` 决定 exe / APK 内部烙的版本与安卓 versionCode，`Cargo.toml` 决定 crate 版本。
 * `scripts/release.mjs` 会拿它们跟 `--version` 对账，对不上直接失败（宁可在这一步炸，
 * 也别发一个"清单写 0.0.2、APK 里其实是 0.0.1"的版本出去）。
 *
 * Cargo.lock **故意不碰**：里面是 cargo 算出来的依赖图，手改版本行只会让下次 cargo 报
 * 不一致；跑一次 cargo 命令它自己会同步（改完 Cargo.toml 后随手 `cargo check` 一下即可）。
 *
 * 用法：
 *   node scripts/bump-version.mjs 0.0.2
 *   npm run bump -- 0.0.2
 */
import fs from "node:fs";
import path from "node:path";
import process from "node:process";
import { CARGO_TOML, PACKAGE_JSON, ROOT, TAURI_CONF, die, log, note, ok, parseVersion, readVersions, rel, step } from "./lib/release-common.mjs";

const USAGE = `用法：node scripts/bump-version.mjs <版本号>

  版本号要形如 0.0.2 / 1.2.3 / 1.2.3-beta.1（semver，三段数字）。
  会同时写入：
    package.json
    src-tauri/tauri.conf.json
    src-tauri/Cargo.toml
  Cargo.lock 不手改（交给 cargo 自己更新）。`;

const argv = process.argv.slice(2);
if (argv.includes("--help") || argv.includes("-h")) {
  log(USAGE);
  process.exit(0);
}
const input = argv.find((a) => !a.startsWith("-"));
if (!input) die("没有给版本号", USAGE);
if (argv.some((a) => a.startsWith("-"))) die(`无法识别的参数：${argv.filter((a) => a.startsWith("-")).join("、")}`, USAGE);

const version = parseVersion(input);

/** 把 JSON 文件里**第一个** `"version": "<current>"` 换掉（顶层 version 就是第一个）。 */
function bumpJson(file, current) {
  const text = fs.readFileSync(file, "utf8");
  const re = new RegExp(`("version"\\s*:\\s*")${current.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}(")`);
  const hits = text.match(new RegExp(re, "g")) ?? [];
  if (hits.length === 0) {
    die(`${rel(file)} 里找不到当前版本 "version": "${current}"`,
      "文件被改过格式？先手动确认这个文件里的 version 字段。");
  }
  fs.writeFileSync(file, text.replace(re, `$1${version.raw}$2`));
}

/**
 * Cargo.toml 只改 `[package]` 段里的 version —— 依赖项里也有一堆 `version = "2"`，
 * 全文替换会把 `tauri = { version = "2" }` 之类的改坏。
 */
function bumpCargo(current) {
  const text = fs.readFileSync(CARGO_TOML, "utf8");
  const pkgAt = text.search(/^\[package\]\s*$/m);
  if (pkgAt < 0) die(`${rel(CARGO_TOML)} 里找不到 [package] 段`, "文件结构不是标准 Cargo 清单？");
  const after = text.slice(pkgAt);
  const nextSection = after.slice(1).search(/^\[/m);
  const sectionEnd = nextSection < 0 ? after.length : pkgAt + 1 + nextSection;
  const section = text.slice(pkgAt, sectionEnd);
  const re = new RegExp(`^(version\\s*=\\s*")${current.replace(/[.*+?^${}()|[\]\\]/g, "\\$&")}(")`, "m");
  if (!re.test(section)) {
    die(`${rel(CARGO_TOML)} 的 [package] 段里找不到 version = "${current}"`);
  }
  fs.writeFileSync(CARGO_TOML, text.slice(0, pkgAt) + section.replace(re, `$1${version.raw}$2`) + text.slice(sectionEnd));
}

const versions = readVersions();
if (!versions["package.json"] || !versions["src-tauri/tauri.conf.json"] || !versions["src-tauri/Cargo.toml"]) {
  die("三个文件里有一个读不出当前版本号", `${rel(PACKAGE_JSON)} / ${rel(TAURI_CONF)} / ${rel(CARGO_TOML)} 的 version 字段。`);
}
const current = {
  pkg: versions["package.json"],
  tauri: versions["src-tauri/tauri.conf.json"],
  cargo: versions["src-tauri/Cargo.toml"],
};

step(`版本号 ${current.pkg} → ${version.raw}`);
if (current.tauri === version.raw && current.pkg === version.raw && current.cargo === version.raw) {
  ok("三处已经是这个版本号，没改它们。");
  // 但 lockfile 可能落后（手改 package.json 时它不会自己跟），照样对齐一次。
  bumpLockfile(version.raw);
  log();
  process.exit(0);
}

bumpJson(PACKAGE_JSON, current.pkg);
ok(`${rel(PACKAGE_JSON).padEnd(28)} ${current.pkg} → ${version.raw}`);

bumpLockfile(version.raw);

/**
 * `package-lock.json` 的**根条目**也要跟着走。
 *
 * 它是 npm 从 package.json 抄下来的一格（名字/版本/许可），而我们改版本号是手改 package.json
 * （没走 `npm version`），于是 lockfile 会留在旧值上 —— 同一份仓库里两个版本号，在下一次
 * `npm install` 之前没人看得见，之后又会被 npm 顺手搅一遍。只动根条目，依赖树一个字节都不碰
 * （那部分是 npm 算的，手改等于埋雷）。许可也顺手对齐：`package.json` 的 `license` 是唯一真源。
 */
function bumpLockfile(version) {
  const file = path.join(ROOT, "package-lock.json");
  if (!fs.existsSync(file)) {
    note("没有 package-lock.json，跳过");
    return;
  }
  const lock = JSON.parse(fs.readFileSync(file, "utf8"));
  const root = lock.packages?.[""];
  if (!root) {
    note("package-lock.json 里没有根条目（旧格式？），跳过");
    return;
  }
  const before = root.version;
  const license = JSON.parse(fs.readFileSync(PACKAGE_JSON, "utf8")).license;
  root.version = version;
  if (license) root.license = license;
  fs.writeFileSync(file, `${JSON.stringify(lock, null, 2)}\n`);
  ok(`${rel(file).padEnd(28)} ${before} → ${version}`);
}

bumpJson(TAURI_CONF, current.tauri);
ok(`${rel(TAURI_CONF).padEnd(28)} ${current.tauri} → ${version.raw}`);

bumpCargo(current.cargo);
ok(`${rel(CARGO_TOML).padEnd(28)} ${current.cargo} → ${version.raw}`);

note(`安卓 versionCode（major*1000000 + minor*1000 + patch，Tauri 的算法）：${version.code}`);
note("Cargo.lock 没动：在 src-tauri/ 下跑一次 cargo check（或 cargo build）它会自己同步。");
log();