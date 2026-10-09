#!/usr/bin/env node
/**
 * 把**已经打好的 release APK**装到连接着的安卓设备上，并启动。
 *
 * 为什么需要它：`tauri android build` 只管打包（产物在 `gen/android/app/build/outputs/apk/*\/release/`），
 * 没有任何 `--install`；`tauri android dev` 会装，但装的是 debug 包并指到 vite dev server。
 * 所以"打 release 装到手机"只能自己接：`npm run android:release` = 打包 + 本脚本。
 *
 * 另外两件本机实情：
 *  1. `adb` 不在 PATH 里（只有 `ANDROID_HOME` 指到 SDK）—— 这里自己拼 `platform-tools/adb`；
 *  2. **debug 版与 release 版签名不同**（debug 用 `~/.android/debug.keystore`，release 用
 *     `gen/android/keystore/release.jks`），同一 `applicationId` 上互装会
 *     `INSTALL_FAILED_UPDATE_INCOMPATIBLE`。卸掉旧版会**清空应用数据**（配对、身份、收到的文件都没了），
 *     所以那一步必须显式：`npm run android:release -- --replace`。
 *
 * 用法：
 *   npm run android:install              # 装 outputs 下最新的 release APK（不重新打包）
 *   npm run android:release              # 打包 + 装 + 启动
 *   npm run android:release -- --replace # 签名冲突时先卸载（会清应用数据）
 *   npm run android:install -- --device <serial>   # 接了多台设备时点名
 *   npm run android:install -- --apk <path>        # 指定 APK（默认取 universal/release 下最新的）
 *   npm run android:install -- --no-launch         # 只装不启动
 */
import { spawnSync } from "node:child_process";
import fs from "node:fs";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..");
const APK_ROOT = path.join(ROOT, "src-tauri/gen/android/app/build/outputs/apk");
const MAIN_ACTIVITY = ".MainActivity"; // 与 gen/android/app/src/main/AndroidManifest.xml 里的 android:name 对应

/** 设备 ABI → Rust target 名（只用于报错时给出可照抄的补救命令）。 */
const ABI_TO_TARGET = {
  "arm64-v8a": "aarch64",
  "armeabi-v7a": "armv7",
  x86_64: "x86_64",
  x86: "i686",
};

const argv = process.argv.slice(2);
const flag = (name) => argv.includes(name);
const opt = (name) => {
  const i = argv.indexOf(name);
  return i >= 0 ? argv[i + 1] : undefined;
};

const die = (msg, hint) => {
  process.stderr.write(`\n✗ ${msg}\n`);
  if (hint) process.stderr.write(`  ${hint}\n`);
  process.exit(1);
};

/** `adb` 自身：优先 SDK 里的绝对路径，其次 PATH。 */
function findAdb() {
  const exe = process.platform === "win32" ? "adb.exe" : "adb";
  const fromSdk = [process.env.ANDROID_HOME, process.env.ANDROID_SDK_ROOT]
    .filter(Boolean)
    .map((sdk) => path.join(sdk, "platform-tools", exe));
  for (const candidate of [...fromSdk, exe]) {
    const probe = spawnSync(candidate, ["version"], { encoding: "utf8" });
    if (!probe.error && probe.status === 0) return candidate;
  }
  die("找不到 adb。", `已尝试：${[...fromSdk, exe].join("、")}；请设置 ANDROID_HOME 或把 platform-tools 加进 PATH。`);
}

const adb = findAdb();
/** 跑 adb；输出统一成字符串（`adb` 没跑起来时 stdout/stderr 是 null，别把 "null" 打进去）。 */
const run = (args) => {
  const r = spawnSync(adb, args, { encoding: "utf8" });
  if (r.error) die(`adb 执行失败（${adb} ${args.join(" ")}）：${r.error.message}`);
  return { status: r.status, stdout: r.stdout ?? "", stderr: r.stderr ?? "" };
};

/** 已连接（`device` 状态）的设备序列号。 */
function connectedDevices() {
  const { status, stdout, stderr } = run(["devices"]);
  if (status !== 0) die(`adb devices 失败：${stderr.trim() || stdout.trim()}`);
  const lines = stdout.split(/\r?\n/).slice(1).filter((l) => l.trim());
  const [ready, other] = [[], []];
  for (const line of lines) {
    const [serial, state] = line.trim().split(/\s+/);
    if (state === "device") ready.push(serial);
    else other.push(`${serial}（${state}）`);
  }
  return { ready, other };
}

const { ready, other } = connectedDevices();
const serial = opt("--device");
if (!ready.length) {
  die(
    other.length ? `没有可用的设备：${other.join("、")}` : "没有检测到已连接的安卓设备。",
    "检查数据线/USB 调试；手机上若弹「允许 USB 调试」要点允许，状态要是 device 而不是 unauthorized。",
  );
}
if (ready.length > 1 && !serial) {
  die(`连接了多台设备：${ready.join("、")}`, `用 --device <serial> 点名，例如：npm run android:install -- --device ${ready[0]}`);
}
if (serial && !ready.includes(serial)) die(`设备 ${serial} 不在已连接列表里：${ready.join("、")}`);
const device = serial ?? ready[0];
/** 点名设备（只有一台时不用 `-s`，也就不会因为序列号写错而误伤别的设备）。 */
const sel = serial ? ["-s", serial] : [];

/**
 * 选 APK：`tauri android build --apk` 的产物在 `universal/release/`，而 `tauri android dev`
 * （哪怕带 `--release`）落在 `<arch>/release/` —— 后者前端仍指到 vite dev server，不是独立包。
 * 所以优先 universal，只有它不存在时才退到别的 flavor。
 */
function pickApk() {
  const override = opt("--apk");
  if (override) {
    const p = path.resolve(ROOT, override);
    if (!fs.existsSync(p)) die(`--apk 指的文件不存在：${override}`);
    return p;
  }
  const apksIn = (flavor) => {
    const dir = path.join(APK_ROOT, flavor, "release");
    return (fs.existsSync(dir) ? fs.readdirSync(dir) : [])
      .filter((f) => f.endsWith(".apk"))
      .map((f) => path.join(dir, f));
  };
  const newest = (files) =>
    files
      .map((p) => ({ p, mtime: fs.statSync(p).mtimeMs }))
      .sort((a, b) => b.mtime - a.mtime)[0]?.p;
  const flavors = fs.existsSync(APK_ROOT) ? fs.readdirSync(APK_ROOT) : [];
  return newest(apksIn("universal")) ?? newest(flavors.flatMap(apksIn));
}

const apk = pickApk();
if (!apk) {
  die(
    `没有找到 release APK（${path.relative(ROOT, APK_ROOT)}/*/release/ 是空的）。`,
    "先打包：npm run android:release（或 npx tauri android build --apk --target aarch64）。",
  );
}

const identifier = JSON.parse(fs.readFileSync(path.join(ROOT, "src-tauri/tauri.conf.json"), "utf8")).identifier;
const deviceAbi = run([...sel, "shell", "getprop", "ro.product.cpu.abi"]).stdout.trim();

process.stdout.write(`设备 ${device} · ${deviceAbi}\n`);
process.stdout.write(`APK  ${path.relative(ROOT, apk)}（${(fs.statSync(apk).size / 1e6).toFixed(1)} MB）\n`);

if (flag("--replace")) {
  const rm = run([...sel, "uninstall", identifier]);
  process.stdout.write(`卸载旧版：${(rm.stdout || rm.stderr).trim() || "（本来就没装）"}\n`);
}

const install = run([...sel, "install", "-r", apk]);
process.stdout.write(install.stdout + install.stderr);
if (install.status !== 0 || /Failure \[/.test(install.stdout + install.stderr)) {
  const out = install.stdout + install.stderr;
  if (out.includes("INSTALL_FAILED_UPDATE_INCOMPATIBLE")) {
    die(
      `签名不一致：手机上装的 ${identifier} 与这个 APK 不是同一个签名（debug ↔ release 切换的必然结果）。`,
      "加 --replace 先卸载再装（**会清空应用数据**）：npm run android:release -- --replace",
    );
  }
  if (out.includes("INSTALL_FAILED_USER_RESTRICTED")) {
    die(
      "设备（Flyme/MIUI 一类 ROM）拒绝了 USB 安装。",
      "手机亮屏解锁着，看到「是否允许通过 USB 安装」时点允许；并在开发者选项里打开「USB 安装」。",
    );
  }
  if (out.includes("INSTALL_FAILED_NO_MATCHING_ABIS")) {
    die(
      `这个 APK 不含本机 ABI（${deviceAbi}）的 .so，多办是打包时没指定 target。`,
      `重打：npx tauri android build --apk --target ${ABI_TO_TARGET[deviceAbi] ?? "<aarch64|armv7|x86_64|i686>"}`,
    );
  }
  die("安装失败，见上面的 adb 输出。");
}

if (!flag("--no-launch")) {
  const start = run([...sel, "shell", "am", "start", "-n", `${identifier}/${MAIN_ACTIVITY}`]);
  process.stdout.write(start.stdout + start.stderr);
}
process.stdout.write(`✓ ${identifier} 已安装${flag("--no-launch") ? "" : "并启动"}\n`);