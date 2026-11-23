#!/usr/bin/env node
/**
 * 清理可再生的构建缓存，保留发布产物。
 *
 * 为什么需要：Cargo 的 `target/` 按 metadata hash **追加**，从不回收 —— 版本号、Cargo.lock、
 * 依赖或工具链一变就留下整套旧产物。本项目 dev profile 默认全量 debuginfo，9 个集成测试每个都
 * 链整个 crate，单份 PDB 300MB+；两三个月就能堆到 100G+（2026-11-17 实测 `target/` 157G）。
 * Gradle 的 `app/build` 也会把 4 个 ABI 的原生库重复存三份（jni / merged / stripped）。
 *
 * 删（都可再生产）：
 *   · `src-tauri/target/debug/`（deps / incremental / build 全在里面）
 *   · `src-tauri/target/` 下所有 `*-linux-android*` 与 `*-apple-darwin*` 目录
 *   · `src-tauri/gen/android/app/build/` 下除 `outputs/` 以外的所有内容
 *
 * 不删（删了要么不可恢复，要么让现有发布/镜像流程失效）：
 *   · `src-tauri/target/release/`：含 `--skip-build` 要读的 `orbdrop.exe`，也保留下次发版的依赖缓存
 *   · `src-tauri/gen/android/app/build/outputs/`：通用包 / 分 ABI APK / AAB
 *   · `src-tauri/gen/android/keystore/`：`release.jks` 被 gitignore，删了不可恢复
 *   · `assets/`：`scripts/dev-mirror.mjs` 直接伺服这个目录
 *
 * 因此本脚本在发版前后都能跑：发版后跑不会破坏 `release:local --skip-build` / `--republish`。
 *
 * 用法：
 *   npm run clean:builds        # 真删
 *   npm run clean:builds:dry    # 只统计体积，不删
 */
import fs from "node:fs";
import path from "node:path";
import process from "node:process";
import { ROOT, note, ok, rel, step, warn } from "./lib/release-common.mjs";

const ARGS = process.argv.slice(2);
const UNKNOWN = ARGS.filter((a) => a !== "--dry-run");
if (UNKNOWN.length) {
  warn(`无法识别的参数：${UNKNOWN.join(" ")}`);
  warn("用法：npm run clean:builds [-- --dry-run]");
  process.exit(1);
}
const DRY = ARGS.includes("--dry-run");
const TARGET = path.join(ROOT, "src-tauri", "target");
const GRADLE_BUILD = path.join(ROOT, "src-tauri", "gen", "android", "app", "build");
const KEEP_IN_GRADLE = new Set(["outputs"]);

/** `--skip-build` / `--republish` 实际读取的三个文件：清理前后都必须还在。 */
const PRESERVED = [
  path.join(TARGET, "release", "orbdrop.exe"),
  path.join(GRADLE_BUILD, "outputs", "apk", "universal", "release", "app-universal-release.apk"),
  path.join(GRADLE_BUILD, "outputs", "apk", "arm64", "release", "app-arm64-release.apk"),
];

const fmt = (n) => {
  if (n >= 1024 ** 3) return `${(n / 1024 ** 3).toFixed(2)} GB`;
  if (n >= 1024 ** 2) return `${(n / 1024 ** 2).toFixed(1)} MB`;
  return `${(n / 1024).toFixed(0)} KB`;
};

/**
 * 递归累加文件大小；符号链接不计（跨盘链接不该被当成缓存体积）。
 *
 * Cargo 会把同一份产物 hardlink 到 `target/<三元组>/` 与 `deps/`（`.pdb` / `.exe` / `.so`），
 * 增量目录里也有复用硬链接。按 inode 去重，否则统计值会比 `du` 大几十 GB，清理结论失真。
 */
function dirSize(dir, seen) {
  let total = 0;
  const stack = [dir];
  while (stack.length) {
    const d = stack.pop();
    let entries;
    try {
      entries = fs.readdirSync(d, { withFileTypes: true });
    } catch {
      continue;
    }
    for (const e of entries) {
      const p = path.join(d, e.name);
      if (e.isDirectory()) {
        stack.push(p);
      } else if (e.isFile()) {
        try {
          const st = fs.statSync(p, { bigint: true });
          if (st.nlink > 1n) {
            const key = `${st.dev}:${st.ino}`;
            if (seen.has(key)) continue;
            seen.add(key);
          }
          total += Number(st.size);
        } catch {
          /* 扫描期间被别的进程删了：忽略这一项 */
        }
      }
    }
  }
  return total;
}

/** 只接受脚本自己挑出来的缓存目录，防止将来改错一行把 release/keystore 带进去。 */
function assertDeletable(p) {
  const abs = path.resolve(p);
  const inside = abs.startsWith(ROOT + path.sep);
  const forbidden = [ROOT, path.join(ROOT, "src-tauri"), TARGET, GRADLE_BUILD].includes(abs);
  const keystore = abs === path.join(ROOT, "src-tauri", "gen", "android", "keystore") || abs.includes(`${path.sep}keystore${path.sep}`);
  if (!inside || forbidden || keystore) {
    throw new Error(`拒绝删除保护路径：${abs}`);
  }
}

function cargoTargets() {
  if (!fs.existsSync(TARGET)) return [];
  return fs
    .readdirSync(TARGET, { withFileTypes: true })
    .filter((e) => e.isDirectory() && !e.isSymbolicLink())
    .map((e) => e.name)
    .filter((n) => n === "debug" || n.includes("linux-android") || n.endsWith("-apple-darwin"))
    .map((n) => path.join(TARGET, n));
}

function gradleEntries() {
  if (!fs.existsSync(GRADLE_BUILD)) return [];
  return fs
    .readdirSync(GRADLE_BUILD, { withFileTypes: true })
    .filter((e) => !KEEP_IN_GRADLE.has(e.name))
    .map((e) => path.join(GRADLE_BUILD, e.name));
}

const missing = PRESERVED.filter((p) => {
  try {
    return !fs.statSync(p).isFile() || fs.statSync(p).size === 0;
  } catch {
    return true;
  }
});

step(`清理可再生构建缓存${DRY ? "（dry-run：只统计，不删）" : ""}`);
if (missing.length) {
  warn(`以下发布产物当前不存在，清理不会动它们，但 --skip-build 现在本来就用不了：`);
  for (const p of missing) warn(`  ${rel(p)}`);
}
note("保留：src-tauri/target/release/、gen/android/app/build/outputs/、gen/android/keystore/、assets/");

const jobs = [
  ...cargoTargets().map((p) => ({ path: p, why: "Cargo 目标目录" })),
  ...gradleEntries().map((p) => ({ path: p, why: "Gradle 中间产物" })),
];

if (jobs.length === 0) {
  note("没有可清理的目录。");
  process.exit(0);
}

note("统计体积中（大目录需要几十秒）…");
const seen = new Set();
for (const job of jobs) job.size = dirSize(job.path, seen);

for (const job of jobs) note(`${rel(job.path).padEnd(52)} ${fmt(job.size).padStart(9)}  ${job.why}`);
const total = jobs.reduce((sum, j) => sum + j.size, 0);
note(`合计可释放：${fmt(total)}`);

if (DRY) {
  ok("dry-run 结束，未删除任何文件。");
  process.exit(0);
}

let freed = 0;
let failed = 0;
for (const job of jobs) {
  try {
    assertDeletable(job.path);
    fs.rmSync(job.path, { recursive: true, force: true, maxRetries: 10, retryDelay: 200 });
    freed += job.size;
  } catch (e) {
    failed += 1;
    warn(`删除失败（${rel(job.path)}）：${e.message}`);
  }
}

for (const p of PRESERVED) {
  // 清理前就不存在的产物不算“被清理弄丢”：上面已经警告过 --skip-build 本来就用不了，
  // 不能因为“还没构建”就让一次纯清理失败。
  if (missing.includes(p)) continue;
  let size = 0;
  try {
    size = fs.statSync(p).size;
  } catch {
    /* 下面统一报错 */
  }
  if (!size) {
    warn(`发布产物丢失：${rel(p)}`);
    process.exit(1);
  }
  ok(`保留 ${rel(p)}（${fmt(size)}）`);
}

ok(`已释放约 ${fmt(freed)}${failed ? `；${failed} 项删除失败，见上方警告` : ""}`);
if (failed) process.exit(1);