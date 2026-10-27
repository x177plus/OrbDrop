#!/usr/bin/env node
/**
 * 在 macOS 上构建通用 `.app` 并打成自更新用的 zip。
 *
 * 为什么单独一条脚本：Windows 发布机编不了 macOS 包。macOS 这一侧只负责「产出 + 打包 +
 * 校验和」，把这个 zip 交给 `release.mjs --macos-zip <文件>`，由发布机统一签名、写清单、
 * 上传两个 Release。
 *
 * 用法（必须在 macOS 上跑）：
 *   node scripts/macos-artifact.mjs                # 构建 universal + 打包
 *   node scripts/macos-artifact.mjs --no-build     # 用现有产物重新打包
 *
 * 产物：
 *   `assets/<tag>/OrbDrop_<版本>_macos_universal.app.zip`（自更新用）
 *   `assets/<tag>/OrbDrop_<版本>_macos_universal.dmg`（直接安装用）
 *
 * 为什么 `.dmg` 不交给 `tauri build` 的 dmg bundler：它在 SSH / 无 Finder 会话的机器上
 * 会调 AppleScript 摆窗口，直接失败（本机实测）。这里用 `hdiutil` 做标准 DMG：应用 +
 * 指向 `/Applications` 的链接，不依赖 GUI。
 */
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import process from "node:process";

import {
  ROOT,
  assetNames,
  die,
  humanSize,
  ok,
  run,
  sha256File,
  step,
  tauriVersion,
} from "./lib/release-common.mjs";

const args = process.argv.slice(2);
const noBuild = args.includes("--no-build");
if (args.includes("--help") || args.includes("-h")) {
  console.log("用法：node scripts/macos-artifact.mjs [--no-build]");
  process.exit(0);
}
const unknown = args.filter((a) => a.startsWith("-") && a !== "--no-build" && a !== "--help" && a !== "-h");
if (unknown.length) die(`无法识别的参数：${unknown.join("、")}`);

if (process.platform !== "darwin") {
  die(
    "macOS 的 .app 只能在 macOS 上构建（Tauri 需要 Xcode 命令行工具与 Apple SDK）",
    "在 Mac 或 GitHub Actions 的 macos runner 上运行本脚本，再把 zip 交给 release.mjs。",
  );
}

const version = tauriVersion();
const tag = `v${version}`;
const NAMES = assetNames(version);
const app = path.join(
  ROOT,
  "src-tauri",
  "target",
  "universal-apple-darwin",
  "release",
  "bundle",
  "macos",
  "OrbDrop.app",
);
const outDir = path.join(ROOT, "assets", tag);
const zip = path.join(outDir, NAMES.macos);

if (!noBuild) {
  step(`构建 macOS 通用包（${app}）`);
  run(
    "npm",
    [
      "run",
      "tauri",
      "--",
      "build",
      "--target",
      "universal-apple-darwin",
      // 只出 .app：DMG 由下面的 hdiutil 自己做，绕开 Tauri 的 Finder/AppleScript 打包器。
      "--bundles",
      "app",
    ],
    {
      inherit: true,
      label: "macOS 通用包构建（tauri build --target universal-apple-darwin --bundles app）",
    },
  );
  ok("构建完成");
} else {
  step("跳过构建（--no-build）");
}

if (!fs.existsSync(app)) {
  die(
    `找不到 .app：${app}`,
    "先去掉 --no-build 构建一次；或确认 Tauri 的 universal 产物目录没有变。",
  );
}

step("打包（ditto -c -k --keepParent）");
fs.mkdirSync(outDir, { recursive: true });
fs.rmSync(zip, { force: true });
run("ditto", ["-c", "-k", "--keepParent", app, zip], {
  inherit: true,
  label: "打包 .app（ditto）",
});
const size = fs.statSync(zip).size;
if (size === 0) die("打出来的 zip 是空文件");
const sha = await sha256File(zip);

ok(`zip 产物：${zip}`);
ok(`zip 大小：${humanSize(size)}`);
ok(`zip sha256：${sha}`);

// DMG：应用 + /Applications 链接，hdiutil 不需要 Finder/AppleScript。
step("生成 DMG（hdiutil）");
const dmg = path.join(outDir, NAMES.macosDmg);
const dmgRoot = fs.mkdtempSync(path.join(os.tmpdir(), "orbdrop-dmg-"));
try {
  fs.rmSync(dmg, { force: true });
  run("ditto", [app, path.join(dmgRoot, "OrbDrop.app")], {
    inherit: true,
    label: "把 .app 放进 DMG 暂存目录（ditto）",
  });
  fs.symlinkSync("/Applications", path.join(dmgRoot, "Applications"), "dir");
  run(
    "hdiutil",
    ["create", "-volname", "OrbDrop", "-srcfolder", dmgRoot, "-ov", "-format", "UDZO", dmg],
    { inherit: true, label: "生成 DMG（hdiutil create）" },
  );
} finally {
  fs.rmSync(dmgRoot, { recursive: true, force: true });
}
const dmgSize = fs.statSync(dmg).size;
if (dmgSize === 0) die("打出来的 DMG 是空文件");
const dmgSha = await sha256File(dmg);
ok(`dmg 产物：${dmg}`);
ok(`dmg 大小：${humanSize(dmgSize)}`);
ok(`dmg sha256：${dmgSha}`);

console.log(`
下一步（在发布机上）：
  node scripts/release.mjs --version ${version} --notes "..." \\
    --macos-zip "${zip}" \\
    --macos-dmg "${dmg}"
`);