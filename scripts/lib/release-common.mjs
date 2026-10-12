/**
 * 发布工具链的共用底座：日志、外部命令、semver/versionCode、配置加载、清单 URL 拼接。
 *
 * 为什么单独抽一层：`release.mjs` 与 `dev-mirror.mjs` 必须读**同一份** `release.local.json`
 * （发布仓路径、镜像台地址前缀要对得上），各写一份迟早会漂；而 `update.json` 里的
 * `mirrors` 顺序（GitHub 前、Gitee 后）与 `versionCode` 算法也都是两处共用的契约。
 */
import { spawnSync } from "node:child_process";
import crypto from "node:crypto";
import fs from "node:fs";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

export const ROOT = path.resolve(path.dirname(fileURLToPath(import.meta.url)), "..", "..");
export const CONFIG_LOCAL = path.join(ROOT, "scripts", "release.local.json");
export const CONFIG_EXAMPLE = path.join(ROOT, "scripts", "release.config.example.json");
export const TAURI_CONF = path.join(ROOT, "src-tauri", "tauri.conf.json");
export const CARGO_TOML = path.join(ROOT, "src-tauri", "Cargo.toml");
export const PACKAGE_JSON = path.join(ROOT, "package.json");

/** 安卓 versionCode 上限（Google Play 的硬限制，Tauri 也照这个卡）。 */
export const MAX_VERSION_CODE = 2_100_000_000;

export const log = (s = "") => process.stdout.write(`${s}\n`);
export const step = (s) => log(`\n▶ ${s}`);
export const note = (s) => log(`  ${s}`);
export const ok = (s) => log(`  ✓ ${s}`);
export const warn = (s) => process.stderr.write(`  ! ${s}\n`);

/** 致命错误：说清是哪一步、哪个文件/URL，然后立刻非零退出（失败绝不静默吞掉）。 */
export function die(msg, hint) {
  process.stderr.write(`\n✗ ${msg}\n`);
  if (hint) process.stderr.write(`  ${hint}\n`);
  process.exit(1);
}

/** 相对源码仓的短路径，日志里好看；仓外的绝对路径原样返回。 */
export const rel = (p) => {
  const r = path.relative(ROOT, p);
  return r && !r.startsWith("..") ? r.split(path.sep).join("/") : p.split(path.sep).join("/");
};

export const humanSize = (n) => `${(n / 1024 / 1024).toFixed(2)} MB`;

/**
 * 跑一条外部命令。Windows 上 `npm`/`tauri` 实际是 `npm.cmd`/`tauri.cmd`，
 * 不经 shell 会 ENOENT，所以 win32 一律走 shell —— 走 shell 时 Node 只做空格拼接、
 * 不做转义，含空格的参数必须自己加引号（本机路径理论上有空格）。
 */
export function run(cmd, args = [], { cwd = ROOT, env, inherit = false, allowFail = false, label } = {}) {
  const useShell = process.platform === "win32";
  const finalArgs = useShell ? args.map((a) => (/[\s"]/.test(a) ? `"${a.replace(/"/g, '\\"')}"` : a)) : args;
  const stdio = inherit ? "inherit" : ["ignore", "pipe", "pipe"];
  // shell 模式必须把命令拼成一整条字符串：spawn(cmd, args, {shell:true}) 在 Node 里已废弃并告警
  const res = useShell
    ? spawnSync([cmd, ...finalArgs].join(" "), { cwd, env: env ? { ...process.env, ...env } : process.env, shell: true, stdio, encoding: "utf8" })
    : spawnSync(cmd, finalArgs, { cwd, env: env ? { ...process.env, ...env } : process.env, stdio, encoding: "utf8" });
  const shown = [cmd, ...args].join(" ");
  if (res.error) {
    if (allowFail) return { status: -1, stdout: "", stderr: res.error.message };
    die(`${label ?? "命令"} 无法执行：${shown}`, res.error.message);
  }
  const out = { status: res.status ?? -1, stdout: res.stdout ?? "", stderr: res.stderr ?? "" };
  if (out.status !== 0 && !allowFail) {
    const tail = (out.stderr || out.stdout).trim().split(/\r?\n/).slice(-25).join("\n");
    die(`${label ?? "命令"} 失败（退出码 ${out.status}）：${shown}`, tail || undefined);
  }
  return out;
}

/** 文件 sha256（hex 小写）。流式算，别把十几 MB 一次性读进内存。 */
export async function sha256File(file) {
  const hash = crypto.createHash("sha256");
  const stream = fs.createReadStream(file);
  for await (const chunk of stream) hash.update(chunk);
  return hash.digest("hex");
}

export const fileSize = (file) => fs.statSync(file).size;

/**
 * semver 解析。只接受 `X.Y.Z`（可带 `-预发布`/`+构建` 后缀）——
 * tauri / cargo 都按 semver 解析，放行别的写法只会在后面构建阶段炸。
 */
export function parseVersion(input) {
  const raw = String(input ?? "").trim();
  const m = /^(\d+)\.(\d+)\.(\d+)(?:[-+][0-9A-Za-z.-]+)?$/.exec(raw);
  if (!m) die(`版本号不合法：${JSON.stringify(raw)}`, "要形如 0.0.2 或 1.2.3-beta.1（三段数字）。");
  const major = Number(m[1]);
  const minor = Number(m[2]);
  const patch = Number(m[3]);
  const code = major * 1_000_000 + minor * 1_000 + patch;
  if (code < 1 || code > MAX_VERSION_CODE) {
    die(`按 semver 推出的 versionCode ${code} 超出安卓上限 ${MAX_VERSION_CODE}：${raw}`);
  }
  return { raw, major, minor, patch, code };
}

/** 从 tauri.conf.json 读版本号（构建产物里烙的就是它）。 */
export function tauriVersion() {
  try {
    return JSON.parse(fs.readFileSync(TAURI_CONF, "utf8")).version;
  } catch (e) {
    die(`读不出 ${rel(TAURI_CONF)} 的 version`, e.message);
  }
}

/**
 * 读三个文件里的当前版本号，发布前拿它跟 `--version` 对账。
 * Cargo.toml 只认 `[package]` 段里的 version —— 依赖项里也有一堆 `version = "2"`。
 */
export function readVersions() {
  const cargoText = fs.readFileSync(CARGO_TOML, "utf8");
  const pkgAt = cargoText.indexOf("[package]");
  if (pkgAt < 0) die(`${rel(CARGO_TOML)} 里找不到 [package] 段`);
  return {
    "package.json": JSON.parse(fs.readFileSync(PACKAGE_JSON, "utf8")).version,
    "src-tauri/tauri.conf.json": JSON.parse(fs.readFileSync(TAURI_CONF, "utf8")).version,
    "src-tauri/Cargo.toml": /^version\s*=\s*"([^"]+)"/m.exec(cargoText.slice(pkgAt))?.[1],
  };
}

const isPlainObject = (v) => v !== null && typeof v === "object" && !Array.isArray(v);
function deepMerge(base, patch) {
  const out = { ...base };
  for (const [k, v] of Object.entries(patch ?? {})) {
    out[k] = isPlainObject(v) && isPlainObject(base[k]) ? deepMerge(base[k], v) : v;
  }
  return out;
}

const DEFAULTS = {
  // 发布仓默认放在源码仓旁边（源码仓之外，独立 git 仓库）
  releasesRepo: path.resolve(ROOT, "..", "OrbDrop-releases"),
  // owner/repo 必须与客户端**编译期常量**一致（src-tauri/src/update/mod.rs 的 MANIFEST_ENDPOINTS：
  // github.com/x177plus/OrbDrop/releases/latest/download/update.json 与 gitee.com/x177plus/OrbDrop/raw/master/update.json），
  // 否则清单与资产 URL 会指到别的仓库，客户端读不到。配置里显式写了就以配置为准（写空字符串也能覆盖默认值）。
  github: { owner: "x177plus", repo: "OrbDrop", remote: "github", tokenEnv: "GITHUB_TOKEN" },
  gitee: { owner: "x177plus", repo: "OrbDrop", remote: "gitee", tokenEnv: "GITEE_TOKEN", manifestBranch: "master" },
  tagPrefix: "v",
  signingKeyPath: "",
  signingKeyPassword: "",
  localMirrorBase: "http://127.0.0.1:8788",
  artifacts: {
    desktopExe: "src-tauri/target/release/orbdrop.exe",
    androidUniversalApk: "src-tauri/gen/android/app/build/outputs/apk/universal/release/app-universal-release.apk",
    androidArm64Apk: "src-tauri/gen/android/app/build/outputs/apk/arm64/release/app-arm64-release.apk",
  },
};

/**
 * 读配置：优先 `scripts/release.local.json`（已 gitignore，含本机路径与远端信息），
 * 没有就退到提交进仓库的 `release.config.example.json`（owner/repo 为空 → 只够本机演练）。
 */
export function loadConfig() {
  const src = fs.existsSync(CONFIG_LOCAL) ? CONFIG_LOCAL : CONFIG_EXAMPLE;
  if (!fs.existsSync(src)) {
    die(`找不到发布配置：${rel(CONFIG_LOCAL)} 与 ${rel(CONFIG_EXAMPLE)} 都没有`,
      `先复制模板：cp "${rel(CONFIG_EXAMPLE)}" "${rel(CONFIG_LOCAL)}"`);
  }
  let raw;
  try {
    raw = JSON.parse(fs.readFileSync(src, "utf8"));
  } catch (e) {
    die(`发布配置不是合法 JSON：${rel(src)}`, e.message);
  }
  const cfg = deepMerge(DEFAULTS, raw);
  cfg.__source = src;
  // 相对路径按**源码仓根目录**解析：不跟当前工作目录挂钩，从哪个目录调都一样
  cfg.repoPath = path.resolve(ROOT, String(cfg.releasesRepo));
  return cfg;
}

/** 三个产物的最终发布文件名（也是 Release 附件名、也是清单 mirrors 里 URL 的末段，必须一致）。 */
export const ASSET_NAMES = {
  exe: "orbdrop.exe",
  universal: "app-universal-release.apk",
  arm64: "app-arm64-release.apk",
  manifest: "update.json",
};

export const tagOf = (cfg, version) => `${cfg.tagPrefix}${version}`;

/**
 * 清单里的 mirrors：**GitHub 在前、Gitee 在后**（客户端先试 GitHub，超时/太慢再切 Gitee）。
 * 本机演练时换成镜像台地址：`/gh/**` 与 `/gitee/**` 分别映射到发布仓根目录，所以
 * `assets/v{ver}/orbdrop.exe` 在这里就是 `<base>/gh/assets/v{ver}/orbdrop.exe`。
 */
export function mirrorsFor(cfg, version, file, { local = false } = {}) {
  const tag = tagOf(cfg, version);
  if (local) {
    const base = String(cfg.localMirrorBase).replace(/\/+$/, "");
    return [`${base}/gh/assets/${tag}/${file}`, `${base}/gitee/assets/${tag}/${file}`];
  }
  return [
    `https://github.com/${cfg.github.owner}/${cfg.github.repo}/releases/download/${tag}/${file}`,
    `https://gitee.com/${cfg.gitee.owner}/${cfg.gitee.repo}/releases/download/${tag}/${file}`,
  ];
}

/**
 * 客户端的清单地址是**编译期常量**（不进清单），所以发布侧必须往这两个地方都放 update.json：
 *  - GitHub：本次 Release 的附件（`latest/download` 别名要求该 Release 不是 prerelease/draft）
 *  - Gitee：发布仓分支上的仓库文件（raw 直出）
 */
export function manifestUrls(cfg) {
  return [
    `https://github.com/${cfg.github.owner}/${cfg.github.repo}/releases/latest/download/${ASSET_NAMES.manifest}`,
    `https://gitee.com/${cfg.gitee.owner}/${cfg.gitee.repo}/raw/${cfg.gitee.manifestBranch}/${ASSET_NAMES.manifest}`,
  ];
}

/** 远端模式前置校验：owner/repo、token、私钥。**构建之前**就要失败的，别等半小时编译完才报。 */
export function requireRemoteConfig(cfg) {
  const missing = [];
  if (!cfg.github.owner) missing.push("github.owner");
  if (!cfg.github.repo) missing.push("github.repo");
  if (!cfg.gitee.owner) missing.push("gitee.owner");
  if (!cfg.gitee.repo) missing.push("gitee.repo");
  if (missing.length) {
    die(`远端模式下配置项为空：${missing.join("、")}（配置文件 ${rel(cfg.__source)}）`,
      "owner 要从 GitHub/Gitee 账号里填；owner 为空时只能用 --local 本机演练。");
  }
  const tokens = {};
  for (const platform of ["github", "gitee"]) {
    const envName = cfg[platform].tokenEnv;
    // 环境变量优先（CI、别人的机器、一次性发布会话），其次本机那个不进仓库的 token 文件。
    const value = process.env[envName] ?? tokenFromSecretsFile(envName);
    if (!value) {
      die(
        `${envName} 没有值（远端模式要它来调 ${platform} API：建 Release、传附件）`,
        `两种给法：环境变量（${envName}=xxx node scripts/release.mjs …），或写进 ${rel(SECRETS_LOCAL)}（已 gitignore，一行一个 KEY=VALUE）。`,
      );
    }
    tokens[platform] = value;
  }
  return tokens;
}

/** 本机 token 文件的位置。整体换个地方就设 `ORBDROP_RELEASE_SECRETS=<路径>`。 */
export const SECRETS_LOCAL = path.join(ROOT, "scripts", "release.local.env");

/**
 * 从本机 token 文件里取一个值（没有这个文件、或里面没有这一项 → null）。
 *
 * 为什么要有这条：`release.local.json` 刻意不装 token（那是要提交进仓库的模板的同名文件，
 * 而且 token 混进 JSON 很容易被顺手贴进 issue），环境变量又只活在当前这个终端里 ——
 * 天天在这台机器上发版时，"一个不进仓库的 env 文件"是最好用的中间形态。
 * **它绝不能进仓库**：`.gitignore` 里那条与本注释是一对。
 */
function tokenFromSecretsFile(envName) {
  const file = process.env.ORBDROP_RELEASE_SECRETS || SECRETS_LOCAL;
  if (!fs.existsSync(file)) return null;
  for (const line of fs.readFileSync(file, "utf8").split(/\r?\n/)) {
    const text = line.trim();
    if (!text || text.startsWith("#")) continue;
    const eq = text.indexOf("=");
    if (eq < 0) continue;
    if (text.slice(0, eq).trim() !== envName) continue;
    const value = text.slice(eq + 1).trim().replace(/^(["'])(.*)\1$/, "$2");
    if (value) return value;
  }
  return null;
}

/** 私钥校验：不在仓库里，所以只能查本机路径与配置。 */
export function requireSigningKey(cfg) {
  const keyPath = cfg.signingKeyPath;
  if (!keyPath) {
    die("配置里没有 signingKeyPath（tauri 签名私钥路径）",
      `在 ${rel(CONFIG_LOCAL)} 里填上，例如 "C:/Users/你/.tauri/orbdrop.key"（该文件不进仓库）。`);
  }
  if (!fs.existsSync(keyPath)) {
    die(`签名私钥不存在：${keyPath}`,
      "确认路径；私钥丢了就只能换公钥重发客户端（公钥是编译进 Rust 的）。");
  }
  return keyPath;
}

export const sleep = (ms) => new Promise((r) => setTimeout(r, ms));