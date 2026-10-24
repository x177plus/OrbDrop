#!/usr/bin/env node
/**
 * 一条命令发布一版：
 *   构建 → 签名 → 生成 update.json → 落入本仓根目录（update.json + CHANGELOG.md）→
 *   （远端）commit + tag → GitHub 推 master/tag；Gitee 推 gitee-mirror 快照与镜像 tag
 *   → 建 Release 传附件 → 回读校验。
 *
 * 两种模式：
 *   默认（远端）：上面全走。需要配置里的 owner/repo，以及环境变量 GITHUB_TOKEN / GITEE_TOKEN。
 *   `--local`：不联网、不 push、不调任何 API，只把产物与清单落到本仓根目录，
 *              清单里的 mirrors 指向本机镜像台（scripts/dev-mirror.mjs），
 *              并在 stdout 打印可直接照抄的客户端启动命令。
 *
 * 几个反直觉的地方，写在这儿免得下次再踩：
 *  1. 桌面端**不要安装器**：`tauri build --no-bundle` 只出 `target/release/orbdrop.exe` 单文件。
 *  2. 安卓必须出**两个** APK：通用包（`--apk`）落在 `apk/universal/release/`，
 *     arm64 包（`--apk --target aarch64`）落在 `apk/arm64/release/` —— 不是同一个目录。
 *  3. `tauri signer sign` 会在**文件旁边**写 `<文件>.sig`。私钥不在仓库里，构建产物在
 *     `src-tauri/` 下（不该多出文件），而清单里的 sig 是**内联字符串**，所以这里先把 exe
 *     拷到临时目录去签，读完 .sig 就地删掉临时文件，磁盘上不留多余文件。
 *  4. update.json 要写**两份**：本仓根目录（客户端走 Gitee raw 读它）＋ 本次 Release 的附件
 *     （客户端走 GitHub `releases/latest/download/` 读它，所以 Release 绝不能是 draft/prerelease，
 *     否则 `latest` 别名不指它）。
 *  5. 本仓**不提交** assets/ —— 那是本机演练用的产物缓存，十几 MB × N 个版本进 git 会把仓库搞爆
 *     （本仓根 `.gitignore` 已经忽略 `/assets/`）。
 *
 * 用法：
 *   node scripts/release.mjs --local --skip-build --version 0.0.1        # 本机演练（不联网）
 *   node scripts/release.mjs --version 0.0.2 --notes "修了 xxx"           # 正式发布
 *   npm run release:local -- --version 0.0.1
 */
import fs from "node:fs";
import os from "node:os";
import path from "node:path";
import process from "node:process";
import {
  ASSET_NAMES,
  ROOT,
  die,
  fileSize,
  humanSize,
  loadConfig,
  log,
  manifestUrls,
  mirrorsFor,
  note,
  ok,
  parseVersion,
  readVersions,
  rel,
  requireRemoteConfig,
  requireSigningKey,
  run,
  sha256File,
  sleep,
  step,
  tagOf,
  tauriVersion,
  warn,
} from "./lib/release-common.mjs";
import { ensureRepoFiles, listTracked, upsertChangelog } from "./lib/releases-repo.mjs";

const USAGE = `用法：node scripts/release.mjs [选项]

  --version <版本号>   要发布的版本（默认取 src-tauri/tauri.conf.json 里的 version）
  --notes "<说明>"     本次更新说明（进 update.json 与 Release 说明；远端模式必填）
  --local              本机演练：不联网、不 push、不调 API，mirrors 指向本机镜像台
  --verify-only        只回读校验某个已发布的版本（两个清单地址 + 每个资产 URL），不构建、不 push、不调 API
  --skip-build         不构建，直接发布现有产物（本机演练、或刚手动构建过）
  --republish          同版本重发：复用已发布的 tag/Release，只把清单与四个附件换成这次构建的
                       （给"版本号没变、但产物必须换掉"的补发用，比如发布后才发现的客户端缺陷）。
                       仓库里 tag 不动，所以 tag 指向的提交与 Release 附件会不一致 —— 见 docs/update.md

示例：
  node scripts/release.mjs --local --skip-build --version 0.0.1
  node scripts/release.mjs --version 0.0.2 --notes "新增 xxx"
  node scripts/release.mjs --version 0.0.1 --republish --notes "修 xxx（同版本重发）"
  node scripts/release.mjs --verify-only --version 0.0.1`;

/** Release 阶段失败时，git 侧（代码 + 清单）已经推上去了：说清这一点，别让人以为什么都没发生。 */
const PUSHED_HINT = "git 侧已经推送完成（本仓的提交与 tag 都在远端了）：修好之后重跑发布；若 tag 已存在，先删本地和远端的 tag 再发。";

const argv = process.argv.slice(2);
const flag = (n) => argv.includes(n);
const opt = (n) => {
  const i = argv.indexOf(n);
  if (i < 0) return undefined;
  const v = argv[i + 1];
  if (v === undefined || v.startsWith("--")) die(`参数 ${n} 后面要跟一个值`, USAGE);
  return v;
};

if (flag("--help") || flag("-h")) {
  log(USAGE);
  process.exit(0);
}
const KNOWN_FLAGS = ["--local", "--skip-build", "--verify-only", "--republish", "--help", "-h", "--version", "--notes"];
// 跳过 `--version` / `--notes` 后面的那个值：更新说明里以 `-` 开头也不该被当成未知参数
// （只有这两个选项真的出现时才登记下标，否则 indexOf 返回 -1，+1 会误把 argv[0] 排除）
const valueIdx = new Set();
for (const name of ["--version", "--notes"]) {
  const i = argv.indexOf(name);
  if (i >= 0) valueIdx.add(i + 1);
}
const unknown = argv.filter((a, i) => a.startsWith("-") && !KNOWN_FLAGS.includes(a) && !valueIdx.has(i));
if (unknown.length) die(`无法识别的参数：${unknown.join("、")}`, USAGE);

/* ---------- 配置与前置校验（放在构建之前：别等编译半小时才报缺 token） ---------- */

const cfg = loadConfig();
const LOCAL = flag("--local");
const SKIP_BUILD = flag("--skip-build");
const VERIFY_ONLY = flag("--verify-only");
/**
 * 同版本重发：版本号不动、**产物必须换掉**的那种补发（例如发布之后才发现客户端有缺陷）。
 *
 * 与普通远端发布的差别只有两处，都跟"这一版已经存在"有关：
 *  - **tag 不新建、也不移动**（它已经指向那一版最初那次提交）：只往本仓的发布分支上追加一个
 *    提交（清单与 CHANGELOG 变了）。所以远端 Release 的附件与 tag 指向的提交会不一致 ——
 *    客户端读的是清单（GitHub 走 Release 附件、Gitee 走分支上的仓库文件），两边都是新的。
 *  - **Release 是复用的**：GitHub 先删掉旧附件再传（同名附件会 422）；Gitee 直接删掉整个
 *    Release 再建一个同 tag 的（它没有"删单个附件"这一步，而且这样也顺带释放配额）。
 */
const REPUBLISH = flag("--republish");
if (REPUBLISH && LOCAL) die("--republish 只在远端模式有意义", "本机演练直接覆盖本仓根目录的清单即可，不用它。");
// --verify-only 只读远端（清单 + 资产 URL），不需要 token，也不碰构建与 git。
const tokens = LOCAL || VERIFY_ONLY ? null : requireRemoteConfig(cfg);
const keyPath = requireSigningKey(cfg);
const git = (args, opts = {}) => run("git", ["-C", cfg.repoPath, ...args], opts);

const ver = parseVersion(opt("--version") ?? tauriVersion());
const versions = readVersions();
const drifted = Object.entries(versions).filter(([, v]) => v !== ver.raw);

step(`发布 OrbDrop ${ver.raw}（versionCode ${ver.code}）· ${LOCAL ? "本机演练（--local）" : "远端发布"}`);
note(`配置 ${rel(cfg.__source)} · 本仓 ${cfg.repoPath} · 私钥 ${keyPath}`);
note(`模式 ${VERIFY_ONLY ? "只回读校验（不构建 / 不 push / 不调 API）" : LOCAL ? "不联网 / 不 push / mirrors 指向本机镜像台" : "构建 + push GitHub + Gitee 快照 + 调 API + 回读校验"}${SKIP_BUILD ? " · --skip-build（用现有产物）" : ""}${REPUBLISH ? " · --republish（同版本重发：复用已发布的 tag 与 Release）" : ""}`);

if (drifted.length) {
  const detail = drifted.map(([f, v]) => `${f}=${v}`).join("、");
  if (LOCAL) warn(`版本号没对齐：${detail}（--local 只提示，演练可以拿旧产物）`);
  else die(`版本号没对齐：${detail}，但这次要发布 ${ver.raw}`, `先同步三处版本号：npm run bump -- ${ver.raw}`);
}

const notes =
  opt("--notes") ??
  (cfg.notes && String(cfg.notes).trim()) ??
  (LOCAL ? "本机演练：只用于验证发布链路与镜像切换，不是正式发布说明。" : null);
if (!notes) {
  die("远端发布必须给更新说明：--notes \"...\"", "它会进 update.json 的 notes 和 Release 说明，别让客户端弹一句空话。");
}

/* ---------- 只回读校验（--verify-only）：不构建、不 push、不调 API ---------- */

if (VERIFY_ONLY) {
  step(`只回读校验 v${ver.raw}：两个清单地址 + 每个资产 URL`);
  await verifyRemote(cfg, ver);
  log();
  process.exit(0);
}

/* ---------- 构建 ---------- */

let buildStart = null;
if (SKIP_BUILD) {
  step("跳过构建（--skip-build）：直接用现有产物");
} else {
  step("构建：桌面端 exe + 两个安卓 APK（时间较长，中途别 Ctrl+C）");
  buildStart = Date.now();
  run("npm", ["run", "tauri", "--", "build", "--no-bundle"], { inherit: true, label: "桌面端构建（tauri build --no-bundle）" });
  ok(`桌面端 exe：${cfg.artifacts.desktopExe}`);
  run("npm", ["run", "tauri", "--", "android", "build", "--apk"], { inherit: true, label: "Android 通用包构建（tauri android build --apk）" });
  ok(`Android 通用包：${cfg.artifacts.androidUniversalApk}`);
  // arm64 这一份**必须用 `--split-per-abi`**，不能用 `--target aarch64`：
  // `--target` 只是限制 Rust 编哪个 ABI，APK 的 flavor 仍然是 `universal` —— 实测那样会把
  // **通用包目录里的那个文件覆盖成 arm64 单包**（53MB → 15MB），于是清单里"通用包"和"arm64"
  // 两个条目指向同一份 arm64 包：armv7/x86 的机器装上去只会得到 INSTALL_FAILED_NO_MATCHING_ABIS。
  // `--split-per-abi` 才是每种 ABI 各出一个包，落在 `apk/<abi>/release/`（arm64 那份在 `apk/arm64/`）。
  // 顺序要紧：先 `--apk` 出通用包，再 `--split-per-abi`（后者不会碰通用包那个目录）。
  run("npm", ["run", "tauri", "--", "android", "build", "--apk", "--split-per-abi"], {
    inherit: true,
    label: "Android 分 ABI 构建（tauri android build --apk --split-per-abi）",
  });
  ok(`Android arm64：${cfg.artifacts.androidArm64Apk}`);
}

/* ---------- 产物定位与核对 ---------- */

const ARTIFACTS = [
  { kind: "exe", label: "Windows 桌面端 exe", src: path.resolve(ROOT, cfg.artifacts.desktopExe), name: ASSET_NAMES.exe },
  { kind: "universal", label: "Android 通用包 APK", src: path.resolve(ROOT, cfg.artifacts.androidUniversalApk), name: ASSET_NAMES.universal },
  { kind: "arm64", label: "Android arm64 APK", src: path.resolve(ROOT, cfg.artifacts.androidArm64Apk), name: ASSET_NAMES.arm64 },
];

step("核对产物");
for (const a of ARTIFACTS) {
  if (!fs.existsSync(a.src)) {
    die(
      `产物不存在：${rel(a.src)}（${a.label}）`,
      SKIP_BUILD
        ? "--skip-build 只是不构建，文件还是得在：先构建，或把配置里的产物路径改对。"
        : "刚构建完却没有这个文件：多半是构建参数或产物目录变了，核对一下 tauri / apk outputs 布局。",
    );
  }
  a.size = fileSize(a.src);
  if (a.size === 0) die(`产物是空文件：${rel(a.src)}（${a.label}）`);
  // mtime **只当提示，不当把关**：增量构建在"输入没变"时本来就不会重写输出
  // （cargo 判定 crate 无事可做、Gradle 判定打包任务 up-to-date，字节相同的产物连 mtime
  // 都不会动），所以"没被本次构建碰过"并不等于"它是旧的"——实测就撞上过这一次。
  // 真正把关的两条在下面：APK 里烙的 versionCode/versionName 必须等于本次要发的版本；
  // 清单里的 size/sha256 是发布后回读校验的依据。
  const mtime = fs.statSync(a.src).mtimeMs;
  if (buildStart !== null && mtime < buildStart - 2000) {
    warn(
      `${rel(a.src)} 没被本次构建重写（mtime ${new Date(mtime).toLocaleString()}）：` +
        "增量构建在内容没变时就是这样，不一定是旧的 —— 版本是否对得上由下面的 APK 内部核对把关。",
    );
  }
  ok(`${a.label}：${rel(a.src)} · ${humanSize(a.size)}`);
}

/** APK 里实际烙的 versionCode/versionName（gradle 写的 output-metadata.json）。 */
function apkMeta(apkPath) {
  const metaPath = path.join(path.dirname(apkPath), "output-metadata.json");
  if (!fs.existsSync(metaPath)) return null;
  try {
    const el = JSON.parse(fs.readFileSync(metaPath, "utf8")).elements?.[0];
    return el ? { versionCode: el.versionCode, versionName: el.versionName } : null;
  } catch {
    return null;
  }
}

for (const a of ARTIFACTS.filter((x) => x.kind !== "exe")) {
  const meta = apkMeta(a.src);
  if (!meta) {
    warn(`${rel(a.src)} 旁边没有 output-metadata.json，跳过 APK 内部版本核对`);
    continue;
  }
  if (meta.versionName !== ver.raw || meta.versionCode !== ver.code) {
    const msg = `${a.label} 里烙的是 ${meta.versionName}（versionCode ${meta.versionCode}），清单要写的是 ${ver.raw}（versionCode ${ver.code}）`;
    if (LOCAL) warn(`${msg} —— --local 只提示（演练拿旧产物是正常的）`);
    else die(msg, `对齐版本号后重新构建：npm run bump -- ${ver.raw} && node scripts/release.mjs --version ${ver.raw}`);
  } else {
    ok(`${a.label} 内部版本 ${meta.versionName} / ${meta.versionCode} 与清单一致`);
  }
}

/* ---------- 签名（桌面端 exe；客户端用这个 sig 校验更新包） ---------- */

step("签名桌面端 exe（tauri signer sign）");
const sig = signFile(ARTIFACTS[0].src, keyPath, cfg.signingKeyPassword);
ok(`签名 OK：${sig.split("\n").length} 行 / ${sig.length} 字符（清单里内联）`);

/**
 * 在临时目录的副本上签名：signer 会在文件旁边写 `<文件>.sig`，
 * 留在 src-tauri/ 下是多余文件（且那目录本次任务不许碰），留在仓库根 assets/v{ver}/ 里又会让
 * assets/v{ver}/ 多出一个清单并不引用的文件。
 */
function signFile(file, keyPath, password) {
  const tmp = fs.mkdtempSync(path.join(os.tmpdir(), "orbdrop-sign-"));
  try {
    const copy = path.join(tmp, path.basename(file));
    fs.copyFileSync(file, copy);
    run("npm", ["run", "tauri", "--", "signer", "sign", copy], {
      label: "签名（tauri signer sign）",
      env: { TAURI_SIGNING_PRIVATE_KEY_PATH: keyPath, TAURI_SIGNING_PRIVATE_KEY_PASSWORD: password ?? "" },
    });
    const sigPath = `${copy}.sig`;
    if (!fs.existsSync(sigPath)) die(`签名命令跑完了，却没有产物 ${path.basename(sigPath)}`, "signer 的版本行为变了？检查 tauri CLI 版本。");
    const content = fs.readFileSync(sigPath, "utf8").trim();
    if (content.length < 40) die(`签名内容看起来不对（${content.length} 字符）：${sigPath}`);
    return content;
  } finally {
    fs.rmSync(tmp, { recursive: true, force: true });
  }
}

/* ---------- 生成 update.json ---------- */

const tag = tagOf(cfg, ver.raw);
const pubDate = new Date().toISOString().replace(/\.\d{3}Z$/, "Z");

step("生成 update.json（size + sha256 + sig + mirrors）");
for (const a of ARTIFACTS) {
  a.sha256 = await sha256File(a.src);
  ok(`${a.name} · ${a.sha256}`);
}

const entry = (a, extra = {}) => ({
  size: a.size,
  sha256: a.sha256,
  ...extra,
  // GitHub 在前、Gitee 在后：客户端先试 GitHub，超时/太慢再切 Gitee
  mirrors: mirrorsFor(cfg, ver.raw, a.name, { local: LOCAL }),
});
const byKind = Object.fromEntries(ARTIFACTS.map((a) => [a.kind, a]));
const manifest = {
  version: ver.raw,
  versionCode: ver.code,
  notes,
  pubDate,
  desktop: { "windows-x86_64": entry(byKind.exe, { sig }) },
  android: { universal: entry(byKind.universal), "arm64-v8a": entry(byKind.arm64) },
};

/* ---------- 落入本仓根目录 ---------- */

step(`落入本仓：${cfg.repoPath}`);
ensureRepoFiles(cfg.repoPath);
const verDir = path.join(cfg.repoPath, "assets", tag);
fs.mkdirSync(verDir, { recursive: true });
for (const a of ARTIFACTS) {
  fs.copyFileSync(a.src, path.join(verDir, a.name));
  ok(`assets/${tag}/${a.name} · ${humanSize(a.size)}`);
}
upsertChangelog(cfg.repoPath, cfg, ver.raw, notes, pubDate.slice(0, 10));
const manifestPath = path.join(cfg.repoPath, ASSET_NAMES.manifest);
fs.writeFileSync(manifestPath, `${JSON.stringify(manifest, null, 2)}\n`);
ok(`${ASSET_NAMES.manifest} · 本仓根目录（Gitee raw 走这里）`);
for (const f of listTracked(cfg.repoPath)) note(`  跟踪文件 ${f}`);
if (LOCAL) warn("根目录 update.json 现在指向本机镜像台；正式发布要重跑一次远端模式才会换回真实地址。");

const uploadFiles = [
  ...ARTIFACTS.map((a) => ({ name: a.name, src: a.src, size: a.size })),
  { name: ASSET_NAMES.manifest, src: manifestPath, size: fileSize(manifestPath) },
];

/* ---------- 远端：git → Release → 回读校验 ---------- */

if (LOCAL) {
  printLocalRecipe(cfg, manifest);
} else {
  await publishRemote(cfg, tokens, ver, tag, notes, branchOfRepo(cfg), uploadFiles);
}

step("完成");
note(`${LOCAL ? "本机演练" : "远端发布"} · OrbDrop ${ver.raw}（versionCode ${ver.code}）· ${pubDate}`);
note(`本仓 ${cfg.repoPath}/（assets/${tag}/ 三个产物 + 清单）`);
for (const a of ARTIFACTS) note(`  ${a.name.padEnd(26)} ${humanSize(a.size)}  sha256=${a.sha256.slice(0, 16)}…`);
note("mirrors（GitHub → Gitee）：");
for (const m of manifest.desktop["windows-x86_64"].mirrors) note(`  ${m}`);
log();

/* =======================================================================
 * 下面都是函数定义（上面是主流程，Node 的 ESM 会提升函数声明，所以放在后面没关系）
 * ======================================================================= */

/** 本机演练：打印镜像台 + 客户端启动命令，直接照抄。 */
function printLocalRecipe(cfg, manifest) {
  const base = String(cfg.localMirrorBase).replace(/\/+$/, "");
  const port = new URL(base).port || "80";
  step("本机演练：让客户端读到这份清单（客户端的清单地址是编译期常量，只能这样覆盖）");
  note("1) 另开一个终端起镜像台（GitHub 档挂起、Gitee 档限速）：");
  log(`     node scripts/dev-mirror.mjs --port ${port} --gh-hang --gitee-kbps 20`);
  note("2) 覆盖清单地址后启动客户端：");
  log(`     ORBDROP_UPDATE_MANIFEST=${base}/manifest.json npm run tauri dev`);
  note("   Windows PowerShell 写法：");
  log(`     $env:ORBDROP_UPDATE_MANIFEST="${base}/manifest.json"; npm run tauri dev`);
  note("   安卓端把启动命令换成 npm run tauri -- android dev（设备侧 adb reverse 见项目文档）。");
  note("3) 清单里这三条就是这个演练要打的地址（/gh 挂起 → 客户端应超时切 /gitee）：");
  for (const m of manifest.desktop["windows-x86_64"].mirrors) log(`     ${m}`);
  note("   镜像台还会直接吐清单：GET /manifest.json");
}

/** 本仓当前分支。Gitee 的 raw 清单地址是 `raw/<分支>/update.json`，只能发在默认分支上。 */
function branchOfRepo(cfg) {
  const r = git(["rev-parse", "--abbrev-ref", "HEAD"], { allowFail: true });
  if (r.status !== 0) {
    die(`读不出本仓的当前分支：${cfg.repoPath}`, "本仓还不是 git 仓库？先 git init 并提交。");
  }
  return r.stdout.trim();
}

/* ---------- 远端模式 ---------- */

/**
 * Gitee 侧只推“发布快照”，不推开发历史。
 *
 * GitHub 的 master 里是开发提交；Gitee 的 master 必须是一条从零开始的快照链，否则一推就把
 * 旧历史带回 Gitee。这里用 `commit-tree HEAD^{tree}` 造一个父提交为 `gitee-mirror` 的新提交
 * （整棵树 = 本次发布提交），再把它推到 Gitee 的 manifestBranch。发布 tag 也打在这个快照提交上，
 * Release 附件照旧挂同名 tag。一次性建立快照链见 docs/update.md §19。
 */
function pushGiteeMirror(cfg, tag) {
  const remote = cfg.gitee.remote;
  const mirrorBranch = cfg.gitee.mirrorBranch;
  const parent = git(["rev-parse", "--verify", `refs/heads/${mirrorBranch}`], { allowFail: true });
  if (parent.status !== 0) {
    die(
      `本地没有 Gitee 快照分支 ${mirrorBranch}`,
      "先按 docs/update.md §19 建立快照链（只需一次），再发版。",
    );
  }
  const tree = git(["rev-parse", "HEAD^{tree}"]).stdout.trim();
  const mirror = git([
    "-c", `user.name=${cfg.gitee.mirrorAuthorName}`,
    "-c", `user.email=${cfg.gitee.mirrorAuthorEmail}`,
    "commit-tree", tree,
    "-p", parent.stdout.trim(),
    "-m", `OrbDrop ${tag} snapshot`,
  ]).stdout.trim();
  if (!mirror) die("Gitee 快照提交创建失败", "git commit-tree 没有输出 SHA。");
  git(["branch", "-f", mirrorBranch, mirror]);
  git(["push", remote, `refs/heads/${mirrorBranch}:refs/heads/${cfg.gitee.manifestBranch}`], {
    inherit: true,
    label: `git push ${remote} ${mirrorBranch} → ${cfg.gitee.manifestBranch}（快照）`,
  });
  if (!REPUBLISH) {
    const mirrorTag = `gitee-${tag}`;
    if (git(["tag", "-l", mirrorTag]).stdout.trim()) {
      die(`本地镜像 tag ${mirrorTag} 已存在`, "确认这次不是重复发布；重复发布请用 --republish。");
    }
    git([
      "-c", `user.name=${cfg.gitee.mirrorAuthorName}`,
      "-c", `user.email=${cfg.gitee.mirrorAuthorEmail}`,
      "tag", "-a", mirrorTag, "-m", `OrbDrop ${tag} (Gitee mirror)`, mirror,
    ]);
    git(["push", remote, `refs/tags/${mirrorTag}:refs/tags/${tag}`], {
      inherit: true,
      label: `git push ${remote} ${tag}（Gitee 镜像 tag）`,
    });
  }
  return mirror;
}

async function publishRemote(cfg, tokens, ver, tag, notes, branch, files) {
  /* git：commit + tag + push（先落仓库，再建 Release —— GitHub 建 Release 时 tag 必须已存在） */
  step("本仓：commit + tag + push");
  if (!fs.existsSync(path.join(cfg.repoPath, ".git"))) {
    die(`本仓还不是 git 仓库：${cfg.repoPath}`, "先 git init 并提交。");
  }
  if (branch !== cfg.gitee.manifestBranch) {
    die(
      `本仓当前分支是 ${branch}，但客户端读 Gitee 清单的地址固定是 raw/${cfg.gitee.manifestBranch}/update.json`,
      `git -C "${cfg.repoPath}" checkout -B ${cfg.gitee.manifestBranch} 之后再造提交（raw 只走默认分支）。`,
    );
  }
  if (git(["tag", "-l", tag]).stdout.trim()) {
    if (!REPUBLISH) {
      die(
        `tag ${tag} 已存在（本地）`,
        `同版本重发：加 --republish（复用已发布的 tag 与 Release，只换清单与附件）；或换个版本号。`,
      );
    }
  } else if (REPUBLISH) {
    die(`--republish 要复用的 tag ${tag} 不存在`, "这一版还没发过 —— 去掉 --republish 走普通发布。");
  }
  // 只加发布相关路径：本仓工作树里常年有源码改动，`add -A` 会把它们一起卷进
  // `Released vX（产物与更新清单）` 这个提交里。assets/ 由根 `.gitignore` 忽略，不进 git。
  git(["add", "--", ASSET_NAMES.manifest, "CHANGELOG.md"]);
  if (!git(["status", "--porcelain", "--", ASSET_NAMES.manifest, "CHANGELOG.md"]).stdout.trim()) {
    die("本仓没有任何变更（要发的内容跟已提交的一模一样）", "换个版本号，或确认产物/清单确实更新了。");
  }
  git(["commit", "-m", `Released ${tag}（产物与更新清单）`], { inherit: true, label: "git commit" });
  // 重发不碰 tag：它已经指向这一版最初那次提交，"移动已发布的 tag"会把两条历史打乱。
  if (!REPUBLISH) {
    git(["tag", "-a", tag, "-m", `OrbDrop ${tag}`], { inherit: true, label: "git tag" });
    ok(`已提交并打 tag ${tag}`);
  } else {
    ok(`已提交（--republish：tag ${tag} 保持不动）`);
  }
  for (const [platform, conf] of [["github", cfg.github], ["gitee", cfg.gitee]]) {
    const remote = conf.remote;
    const url = git(["remote", "get-url", remote], { allowFail: true });
    if (url.status !== 0) {
      die(`本仓没有配置远端 ${remote}`,
        `git -C "${cfg.repoPath}" remote add ${remote} git@${platform}.com:${conf.owner}/${conf.repo}.git`);
    }
    if (platform === "gitee" && cfg.gitee.mirrorBranch) {
      const mirror = pushGiteeMirror(cfg, tag);
      ok(`已推送 ${remote} → ${url.stdout.trim()}（${cfg.gitee.mirrorBranch} → ${cfg.gitee.manifestBranch} @ ${mirror.slice(0, 12)}）`);
      continue;
    }
    git(["push", remote, branch], { inherit: true, label: `git push ${remote} ${branch}（推送失败：代理/权限/仓库不存在，别忽略）` });
    if (!REPUBLISH) {
      git(["push", remote, `refs/tags/${tag}`], { inherit: true, label: `git push ${remote} ${tag}` });
    }
    ok(`已推送 ${remote} → ${url.stdout.trim()}`);
  }

  await publishGithub(cfg, tokens.github, tag, notes, files);
  await publishGitee(cfg, tokens.gitee, tag, notes, branch, files);
  await verifyRemote(cfg, ver);
}

/* ---------- GitHub ---------- */

async function publishGithub(cfg, token, tag, notes, files) {
  const gh = cfg.github;
  step(`GitHub：${REPUBLISH ? `复用 Release ${tag}（先删旧附件再传新的）` : "建 Release 并上传附件"}`);
  let release;
  if (REPUBLISH) {
    const got = await fetchOrDie(
      `https://api.github.com/repos/${gh.owner}/${gh.repo}/releases/tags/${tag}`,
      { headers: githubHeaders(token), timeoutMs: 120_000 },
      `GitHub 取 Release ${tag}`,
    );
    if (got.status !== 200) die(`GitHub 上没有 tag ${tag} 的 Release（HTTP ${got.status}）`, [await bodySnippet(got), PUSHED_HINT].join("\n"));
    release = await got.json();
    ok(`Release #${release.id}（复用） · ${release.html_url ?? ""}`);
    // 说明也跟着刷新一次：清单里的 notes 变了，Release 页面上那句不该还是旧的。
    const patched = await fetchOrDie(
      `https://api.github.com/repos/${gh.owner}/${gh.repo}/releases/${release.id}`,
      {
        method: "PATCH",
        headers: { ...githubHeaders(token), "Content-Type": "application/json" },
        body: JSON.stringify({ name: tag, body: notes, draft: false, prerelease: false }),
        timeoutMs: 120_000,
      },
      "GitHub 更新 Release 说明",
    );
    if (patched.status !== 200) warn(`更新 Release 说明失败（HTTP ${patched.status}），附件照旧上传`);
    for (const asset of release.assets ?? []) {
      const del = await fetchOrDie(
        `https://api.github.com/repos/${gh.owner}/${gh.repo}/releases/assets/${asset.id}`,
        { method: "DELETE", headers: githubHeaders(token), timeoutMs: 120_000 },
        `GitHub 删附件 ${asset.name}`,
      );
      // 404 = 已经不在了（重跑发布时正常）；其余非 2xx 要拦住 —— 留着旧附件的话，同名上传会 422。
      if (del.status !== 204 && del.status !== 404) {
        die(`GitHub 删旧附件失败（HTTP ${del.status}）：${asset.name}`, [await bodySnippet(del), PUSHED_HINT].join("\n"));
      }
      ok(`删掉旧附件 ${asset.name}`);
    }
  } else {
    const res = await fetchOrDie(
      `https://api.github.com/repos/${gh.owner}/${gh.repo}/releases`,
      {
        method: "POST",
        headers: { ...githubHeaders(token), "Content-Type": "application/json" },
        body: JSON.stringify({ tag_name: tag, name: tag, body: notes, draft: false, prerelease: false }),
        timeoutMs: 120_000,
      },
      "GitHub 建 Release",
    );
    if (res.status !== 201) die(`GitHub 建 Release 失败（HTTP ${res.status}）`, [await bodySnippet(res), PUSHED_HINT].join("\n"));
    release = await res.json();
    ok(`Release #${release.id} · ${release.html_url ?? ""}`);
  }
  // upload_url 形如 https://uploads.github.com/.../assets{?name,label}，模板段要去掉
  const uploadBase = String(release.upload_url).replace(/\{\?.*\}$/, "");
  for (const file of files) {
    const url = `${uploadBase}?name=${encodeURIComponent(file.name)}`;
    const up = await fetchOrDie(
      url,
      {
        method: "POST",
        headers: { ...githubHeaders(token), "Content-Type": ctypeOf(file.name) },
        body: fs.readFileSync(file.src),
        timeoutMs: 15 * 60 * 1000,
      },
      `GitHub 上传附件 ${file.name}`,
    );
    if (up.status !== 201) {
      const hint = up.status === 422 ? "同名附件已存在：先在 Release 页面删掉旧附件再重跑。" : undefined;
      die(
        `GitHub 上传附件失败（HTTP ${up.status}）：${file.name} → ${url}`,
        [hint, await bodySnippet(up), PUSHED_HINT].filter(Boolean).join("\n"),
      );
    }
    const info = await up.json();
    ok(`${file.name} · ${humanSize(file.size)} → ${info.browser_download_url ?? "uploaded"}`);
  }
}

/**
 * GitHub API 的请求头。
 *
 * **写成函数声明，不写 `const` 箭头**：调用它的 `publishGithub()` 是在**上面**的主流程里被调用的，
 * 而 `const` 不提升（在初始化之前访问就是 `ReferenceError: Cannot access 'githubHeaders' before
 * initialization`）。这个文件里所有被主流程调用的辅助函数都必须是函数声明 —— 实测踩过这一次，
 * 且只有真跑远端发布才会暴露（本机演练根本不走这条路）。
 */
function githubHeaders(token) {
  return {
    Authorization: `Bearer ${token}`,
    Accept: "application/vnd.github+json",
    "X-GitHub-Api-Version": "2022-11-28",
    "User-Agent": "orbdrop-release",
  };
}

/* ---------- Gitee ---------- */

async function publishGitee(cfg, token, tag, notes, branch, files) {
  const gt = cfg.gitee;
  step(`Gitee：${REPUBLISH ? `重建 Release ${tag}（旧的那个连同附件一起删掉）` : "建 Release 并上传附件"}`);

  // 重发先删旧的：Gitee 没有"删单个附件"，而且建同 tag 的 Release 要先让位；
  // 先删也顺带把配额释放掉（下面的 checkGiteeQuota 才数得准）。
  if (REPUBLISH) {
    const old = await findGiteeRelease(gt, token, tag);
    if (!old) die(`Gitee 上找不到 tag ${tag} 的 Release`, "这一版在 Gitee 上还没发过？去掉 --republish 走普通发布。");
    const del = await fetchOrDie(giteeApiUrl(gt, token, `releases/${old.id}`), { method: "DELETE", timeoutMs: 120_000 }, `Gitee 删 Release ${tag}`);
    // 404 = 已经不在了（重跑发布时正常）
    if (del.status !== 204 && del.status !== 200 && del.status !== 404) {
      die(`Gitee 删旧 Release 失败（HTTP ${del.status}）`, [await bodySnippet(del), PUSHED_HINT].join("\n"));
    }
    ok(`已删旧 Release #${old.id}（${tag}，连同它的附件）`);
  }

  await checkGiteeQuota(gt, token, files);

  // 官方 spec：POST /repos/{o}/{r}/releases 是 x-www-form-urlencoded，
  // 参数 access_token / tag_name / name / body / prerelease / target_commitish（后两个有默认值）
  const res = await fetchOrDie(
    `https://gitee.com/api/v5/repos/${gt.owner}/${gt.repo}/releases`,
    {
      method: "POST",
      headers: { "Content-Type": "application/x-www-form-urlencoded" },
      body: new URLSearchParams({
        access_token: token,
        tag_name: tag,
        name: tag,
        body: notes,
        prerelease: "false",
        target_commitish: branch,
      }),
      timeoutMs: 120_000,
    },
    "Gitee 建 Release",
  );
  if (res.status !== 201 && res.status !== 200) {
    die(`Gitee 建 Release 失败（HTTP ${res.status}）`, [await bodySnippet(res), PUSHED_HINT].join("\n"));
  }
  const release = await res.json();
  if (!release?.id) die("Gitee 建 Release 的响应里没有 id", JSON.stringify(release).slice(0, 400));
  ok(`Release #${release.id} · ${tag}`);

  for (const file of files) {
    // 附件是 multipart/form-data，字段名固定 `file`，access_token 也可以放在表单里
    const form = new FormData();
    form.append("access_token", token);
    form.append("file", new Blob([fs.readFileSync(file.src)]), file.name);
    const up = await fetchOrDie(
      `https://gitee.com/api/v5/repos/${gt.owner}/${gt.repo}/releases/${release.id}/attach_files`,
      { method: "POST", body: form, timeoutMs: 30 * 60 * 1000 },
      `Gitee 上传附件 ${file.name}`,
    );
    if (up.status !== 201 && up.status !== 200) {
      die(`Gitee 上传附件失败（HTTP ${up.status}）：${file.name}`, [await bodySnippet(up), PUSHED_HINT].join("\n"));
    }
    const info = await up.json();
    ok(`${file.name} · ${humanSize(file.size)} → ${info.browser_download_url ?? "uploaded"}`);
  }
}

/** Gitee API 的带 token 地址（v5 的多数接口认查询串里的 `access_token`，不认 Authorization 头）。 */
function giteeApiUrl(gt, token, rest) {
  return `https://gitee.com/api/v5/repos/${gt.owner}/${gt.repo}/${rest}${rest.includes("?") ? "&" : "?"}access_token=${encodeURIComponent(token)}`;
}

/**
 * 按 tag 找 Gitee 上已经发过的 Release（没有就返回 null）。
 *
 * 先试官方的按 tag 取（`releases/tags/<tag>`）；它拿不到时翻列表兜底 —— 这个接口在
 * "tag 名里带 `v` 前缀"这类情况下偶尔认不出来，而重发时必须准确定位到要删的那一个。
 */
async function findGiteeRelease(gt, token, tag) {
  const byTag = await fetchOrDie(giteeApiUrl(gt, token, `releases/tags/${encodeURIComponent(tag)}`), { timeoutMs: 120_000 }, `Gitee 取 Release ${tag}`);
  if (byTag.status === 200) {
    const rel = await byTag.json();
    if (rel?.id) return rel;
  } else if (byTag.status !== 404) {
    warn(`按 tag 取 Release 返回 HTTP ${byTag.status}，改用列表兜底`);
  }
  for (let page = 1; page <= 20; page++) {
    const listRes = await fetchOrDie(giteeApiUrl(gt, token, `releases?per_page=100&page=${page}`), { timeoutMs: 120_000 }, "Gitee 列 Release");
    if (!listRes.ok) {
      warn(`拿不到 Release 列表（HTTP ${listRes.status}）`);
      break;
    }
    const list = await listRes.json();
    if (!Array.isArray(list) || list.length === 0) break;
    const hit = list.find((r) => r.tag_name === tag);
    if (hit) return hit;
    if (list.length < 100) break;
  }
  return null;
}

/** Gitee 的硬限制：单附件 ≤100 MB、仓库附件总量 ≤1 GB。超了要在上传前就报清楚。 */
async function checkGiteeQuota(gt, token, files) {
  const LIMIT_FILE = 100 * 1024 * 1024;
  const LIMIT_REPO = 1024 * 1024 * 1024;
  let incoming = 0;
  for (const f of files) {
    if (f.size > LIMIT_FILE) {
      die(`Gitee 单个附件上限 100 MB，${f.name} 有 ${humanSize(f.size)}`, "Gitee 放不下：客户端清单里 mirrors 会先试 GitHub，把大包只挂 GitHub。");
    }
    incoming += f.size;
  }
  const api = (u) => giteeApiUrl(gt, token, u);
  let used = 0;
  let releases = 0;
  let truncated = false;
  for (let page = 1; page <= 20; page++) {
    const listRes = await fetchOrDie(api(`releases?per_page=100&page=${page}`), { timeoutMs: 60_000 }, "Gitee 列 Release（统计附件总量）");
    if (!listRes.ok) {
      warn(`拿不到已有 Release 列表（HTTP ${listRes.status}），跳过「仓库附件总量」估算`);
      return;
    }
    const list = await listRes.json();
    if (!Array.isArray(list) || list.length === 0) break;
    if (list.length === 100 && page === 20) truncated = true;
    for (const rel of list) {
      releases++;
      const attRes = await fetchOrDie(api(`releases/${rel.id}/attach_files?per_page=100`), { timeoutMs: 60_000 }, "Gitee 列附件");
      if (!attRes.ok) continue;
      const att = await attRes.json();
      if (Array.isArray(att)) for (const f of att) used += Number(f.size) || 0;
    }
  }
  note(`Gitee 现有附件约 ${humanSize(used)}（${releases} 个 Release${truncated ? "，只统计了前 2000 个" : ""}）+ 本次 ${humanSize(incoming)}`);
  if (used + incoming > LIMIT_REPO) {
    die(
      `Gitee 仓库附件总量上限 1 GB：现有约 ${humanSize(used)} + 本次 ${humanSize(incoming)} 会超`,
      "先在 Gitee 网页上删掉旧版本的附件（Release 编辑页里），再重跑发布。",
    );
  }
}

/* ---------- 回读校验 ---------- */

/**
 * 只读校验用的请求：**网络层失败不当失败**。
 *
 * 发布是从**你本机**发起的，而国内直连 github.com 时通时不通（实测：同一条命令上一秒 200、
 * 下一秒 connect timeout）。那种情况下"回读校验 GitHub 那两条"失败只说明**本机此刻够不着它**，
 * 不是发布出了问题 —— 所以降级成警告并把它要人工确认的 URL 打出来。HTTP 层的问题（404、
 * 版本对不上、资产缺失）照旧**硬失败**：那才是真的发错了。
 */
async function fetchSoft(url, { timeoutMs = 60_000, ...init } = {}) {
  try {
    return { res: await fetch(url, { redirect: "follow", signal: AbortSignal.timeout(timeoutMs), ...init }) };
  } catch (e) {
    const cause = e.cause?.message ? ` / ${e.cause.message}` : "";
    return { netError: `${e.name === "TimeoutError" ? `超时（${timeoutMs}ms）` : e.message}${cause}` };
  }
}

function unreachable(url, why) {
  warn(`本机够不着 ${url}（${why}）`);
  warn(`  这只说明**这台机器**到那条路的网络不通，不代表发布失败 —— 稍后自己打开这个 URL 确认一下。`);
}

async function verifyRemote(cfg, ver) {
  step("回读校验：两个清单地址 + 每个资产 URL（GitHub 资产 + Gitee 资产）");
  for (const url of manifestUrls(cfg)) {
    const got = await readManifest(url, 3);
    if (got.netError) {
      unreachable(url, got.netError);
      continue;
    }
    let json;
    try {
      json = JSON.parse(got.body);
    } catch (e) {
      die(`清单不是合法 JSON：${url}`, `${e.message}\n前 200 字符：${got.body.slice(0, 200)}`);
    }
    if (json.version !== ver.raw) {
      die(
        `清单版本对不上：${url} 里是 ${json.version}，本次发布 ${ver.raw}`,
        `多半是 CDN 缓存，或 GitHub 的 \`latest\` 别名指到了别的 Release。\n${PUSHED_HINT}`,
      );
    }
    ok(`清单 OK（version=${json.version}）：${url}`);
  }
  for (const file of [ASSET_NAMES.exe, ASSET_NAMES.universal, ASSET_NAMES.arm64]) {
    for (const url of mirrorsFor(cfg, ver.raw, file)) {
      const got = await fetchSoft(url, { headers: { Range: "bytes=0-0" }, timeoutMs: 60_000 });
      if (got.netError) {
        unreachable(url, got.netError);
        continue;
      }
      const res = got.res;
      if (res.status !== 200 && res.status !== 206) {
        die(`资产 URL 校验失败（HTTP ${res.status}）：${url}`, [await bodySnippet(res), PUSHED_HINT].join("\n"));
      }
      try {
        await res.body?.cancel();
      } catch {
        /* 只取了一个字节就断开，cancel 报错无所谓 */
      }
      ok(`HTTP ${res.status} ${url}`);
    }
  }
}

/** 读清单：HTTP 层失败重试（GitHub 的 `latest/download` 别名刚建完 Release 有缓存延迟）。 */
async function readManifest(url, attempts) {
  let last = "";
  for (let i = 1; i <= attempts; i++) {
    const got = await fetchSoft(url, { timeoutMs: 60_000 });
    if (got.netError) return { netError: got.netError }; // 网络不通：重试也没有意义，交给调用方降级
    if (got.res.ok) return { body: await got.res.text() };
    last = `HTTP ${got.res.status} ${await bodySnippet(got.res)}`;
    if (i < attempts) {
      warn(`第 ${i}/${attempts} 次读失败（${last}），5s 后重试`);
      await sleep(5000);
    }
  }
  die(`读清单失败：${url}`, `${last}\n确认该 Release 不是 draft/prerelease（否则 latest 别名不指它）。`);
}

/* ---------- HTTP 小工具 ---------- */

async function fetchOrDie(url, { timeoutMs = 60_000, ...init } = {}, label) {
  try {
    return await fetch(url, { redirect: "follow", signal: AbortSignal.timeout(timeoutMs), ...init });
  } catch (e) {
    const cause = e.cause?.message ? ` / ${e.cause.message}` : "";
    die(
      `${label} 请求失败：${url}`,
      `${e.name === "TimeoutError" ? `超时（${timeoutMs}ms）` : e.message}${cause}${
        process.env.HTTPS_PROXY || process.env.https_proxy ? `（当前代理 ${process.env.HTTPS_PROXY ?? process.env.https_proxy}）` : "（github.com 本机连不上时，只有配了代理才能走远端模式）"
      }`,
    );
  }
}

async function bodySnippet(res) {
  try {
    return (await res.text()).slice(0, 400);
  } catch {
    return "";
  }
}

/** 附件上传的 Content-Type（同样必须是函数声明：调用点在主流程里，见 `githubHeaders` 的注释）。 */
function ctypeOf(name) {
  return name.endsWith(".json") ? "application/json" : "application/octet-stream";
}