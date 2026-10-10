#!/usr/bin/env node
/**
 * 本机镜像演练台：把发布仓当静态站点伺服，用来实测客户端的**镜像切换**。
 *
 *   node scripts/dev-mirror.mjs --port 8788 --gh-hang --gitee-kbps 20
 *
 * 路由（两个前缀映射的是**同一份文件**，模拟 GitHub / Gitee 两处放的东西一样）：
 *   /gh/**      → <发布仓>/<路径>   --gh-hang 时只收请求、永不回数据（模拟连不上 GitHub）
 *                                   --gh-kbps N 时按 N KB/s 限速
 *   /gitee/**   → <发布仓>/<路径>   默认按 --gitee-kbps 限速（模拟 Gitee 只有几 KB/s）
 *   /manifest.json 直接吐发布仓根目录的 update.json（客户端用 ORBDROP_UPDATE_MANIFEST 覆盖它）
 *
 * 为什么要有它：客户端的清单地址是**编译期常量**，两个镜像的"快 / 慢 / 挂"三种情况只有
 * 在真机拔网线、改 hosts 才能复现 —— 这里用限速与"挂起"在本地精确复现，改一行参数就能重来。
 *
 * 纯 node:http + node:fs，没有依赖（http 的请求限速靠"按已发字节数推算下一个分片的时刻"实现）。
 *
 * 用法：
 *   node scripts/dev-mirror.mjs --port 8788 --gh-hang --gitee-kbps 20
 *   node scripts/dev-mirror.mjs --port 8788 --gitee-kbps 10 --gh-kbps 200
 *   npm run mirror -- --gh-hang
 */
import fs from "node:fs";
import http from "node:http";
import path from "node:path";
import process from "node:process";
import { once } from "node:events";
import { die, humanSize, loadConfig, log, note, parseVersion, rel, step } from "./lib/release-common.mjs";

const USAGE = `用法：node scripts/dev-mirror.mjs [选项]

  --port <端口>        监听端口（默认取配置里 localMirrorBase 的端口，通常 8788）
  --gitee-kbps <N>     /gitee/** 限速（KB/s，默认 10；0 = 不限速）
  --gh-kbps <N>        /gh/** 限速（KB/s，默认 0 = 不限速）
  --gh-hang            /gh/** 只接受连接、永不回数据（模拟连不上 GitHub；优先于 --gh-kbps）
  --root <路径>        发布仓路径（默认取配置里的 releasesRepo）
  --version <版本号>   期望清单里的版本（只用于启动时提示，对不上会警告）`;

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

const cfg = loadConfig();
const root = path.resolve(opt("--root") ?? cfg.repoPath);
const port = Number(opt("--port") ?? (new URL(cfg.localMirrorBase).port || "8788"));
const giteeKbps = Number(opt("--gitee-kbps") ?? 10);
const ghKbps = Number(opt("--gh-kbps") ?? 0);
const ghHang = flag("--gh-hang");
const wantVersion = opt("--version") ? parseVersion(opt("--version")).raw : null;

if (!Number.isInteger(port) || port <= 0 || port > 65535) die(`端口不合法：${opt("--port")}`);
if (!Number.isFinite(giteeKbps) || giteeKbps < 0) die(`--gitee-kbps 不合法：${opt("--gitee-kbps")}`);
if (!Number.isFinite(ghKbps) || ghKbps < 0) die(`--gh-kbps 不合法：${opt("--gh-kbps")}`);
if (!fs.existsSync(root)) {
  die(`发布仓不存在：${root}`, `先跑本机发布（会顺便建出目录）：node scripts/release.mjs --local --skip-build --version 0.0.1`);
}

/** KB/s → B/s。这里按 1 KB = 1024 B 算，说"20 KB/s"就是 20480 B/s。 */
const bytesPerSec = (kbps) => Math.round(kbps * 1024);
const GH_RATE = bytesPerSec(ghKbps);
const GITEE_RATE = bytesPerSec(giteeKbps);

const MANIFEST = path.join(root, "update.json");

/** 只允许读发布仓里的文件：防止 `..` 穿出去（静态服务器最容易翻车的地方）。 */
function resolveInRoot(relPath) {
  const abs = path.resolve(root, relPath);
  const prefix = root.endsWith(path.sep) ? root : root + path.sep;
  return abs.startsWith(prefix) ? abs : null;
}

const CTYPE = {
  ".exe": "application/octet-stream",
  ".apk": "application/vnd.android.package-archive",
  ".json": "application/json; charset=utf-8",
  ".sig": "text/plain; charset=utf-8",
  ".md": "text/markdown; charset=utf-8",
  ".txt": "text/plain; charset=utf-8",
};
const ctypeOf = (file) => CTYPE[path.extname(file).toLowerCase()] ?? "application/octet-stream";

const sleep = (ms) => new Promise((r) => setTimeout(r, ms));

/** 未限速：一次写完（文件最大十几 MB，直接读进内存最省事）。 */
function sendFull(req, res, file, label, pathname) {
  const stat = fs.statSync(file);
  const head = { "Content-Type": ctypeOf(file), "Content-Length": String(stat.size), "Cache-Control": "no-store" };
  if (req.method === "HEAD") {
    res.writeHead(200, head);
    res.end();
    log(`[${label}] HEAD ${pathname} → 200 ${stat.size} B（不限速）`);
    return;
  }
  res.writeHead(200, head);
  res.end(fs.readFileSync(file));
  log(`[${label}] ${req.method} ${pathname} → 200 ${stat.size} B（不限速）`);
}

/**
 * 限速发送：按"已发送字节数 / 速率"推算每一片该在什么时候发，
 * 而不是固定 sleep —— 中间卡顿（drain 等）也会被自动补偿掉，实测速度才贴近设定值。
 */
async function sendThrottled(req, res, file, rate, label, pathname) {
  const stat = fs.statSync(file);
  const head = { "Content-Type": ctypeOf(file), "Content-Length": String(stat.size), "Cache-Control": "no-store" };
  if (req.method === "HEAD") {
    res.writeHead(200, head);
    res.end();
    log(`[${label}] HEAD ${pathname} → 200 ${stat.size} B（限速 ${rate / 1024} KB/s）`);
    return;
  }
  res.writeHead(200, head);
  const data = fs.readFileSync(file);
  const eta = stat.size / rate;
  log(`[${label}] ${req.method} ${pathname} → 200 开始发送 ${humanSize(stat.size)} · 限速 ${rate / 1024} KB/s（预计 ${eta.toFixed(0)}s）`);

  const t0 = Date.now();
  let sent = 0;
  let aborted = false;
  let finished = false;
  res.on("close", () => {
    if (!finished) aborted = true;
  });
  while (sent < data.length && !aborted) {
    const end = Math.min(sent + 8192, data.length);
    const canWriteMore = res.write(data.subarray(sent, end));
    sent = end;
    const due = (sent / rate) * 1000 - (Date.now() - t0);
    if (due > 0) await sleep(due);
    if (!canWriteMore) await once(res, "drain").catch(() => {});
  }
  finished = true;
  if (!aborted) res.end();

  const secs = (Date.now() - t0) / 1000;
  log(
    `[${label}] ${req.method} ${pathname} → ${aborted ? "客户端断开" : "发送完成"}：${sent}/${stat.size} B · 用时 ${secs.toFixed(1)}s · 实速 ${(sent / 1024 / secs).toFixed(1)} KB/s`,
  );
}

function sendJson(res, status, text) {
  res.writeHead(status, { "Content-Type": "application/json; charset=utf-8", "Content-Length": Buffer.byteLength(text), "Cache-Control": "no-store" });
  res.end(text);
}

function sendText(res, status, text) {
  res.writeHead(status, { "Content-Type": "text/plain; charset=utf-8", "Content-Length": Buffer.byteLength(text), "Cache-Control": "no-store" });
  res.end(text);
}

/** /manifest.json：客户端会先读它，所以这里**不限速、不挂起** —— 挂了就什么也测不出来。 */
function handleManifest(res) {
  if (!fs.existsSync(MANIFEST)) {
    sendText(res, 404, `找不到 ${MANIFEST}\n先跑一次本机发布：node scripts/release.mjs --local --skip-build --version 0.0.1\n`);
    log(`[manifest] GET /manifest.json → 404（${rel(MANIFEST)} 不存在）`);
    return;
  }
  const text = fs.readFileSync(MANIFEST, "utf8");
  sendJson(res, 200, text);
  log(`[manifest] GET /manifest.json → 200 ${Buffer.byteLength(text)} B`);
}

function handleIndex(req, res) {
  const text = [
    `OrbDrop 镜像演练台 · 发布仓 ${root}`,
    "",
    `  /gh/**         → <发布仓>/<路径>${ghHang ? "   当前：挂起（收请求，永不回数据）" : GH_RATE ? `   当前：限速 ${GH_RATE / 1024} KB/s` : "   当前：不限速"}`,
    `  /gitee/**      → <发布仓>/<路径>${GITEE_RATE ? `   当前：限速 ${GITEE_RATE / 1024} KB/s` : "   当前：不限速"}`,
    `  /manifest.json → ${rel(MANIFEST)}`,
    "",
    "客户端启动（清单地址是编译期常量，只能用环境变量覆盖）：",
    `  ORBDROP_UPDATE_MANIFEST=http://127.0.0.1:${port}/manifest.json npm run tauri dev`,
    "",
  ].join("\n");
  sendText(res, 200, text);
  log(`[index] ${req.method} / → 200`);
}

async function handler(req, res) {
  let pathname;
  try {
    pathname = decodeURIComponent(new URL(req.url, `http://127.0.0.1:${port}`).pathname);
  } catch {
    sendText(res, 400, "URL 解析不了\n");
    return;
  }
  if (pathname === "/" || pathname === "/index.html") return handleIndex(req, res);
  if (pathname === "/manifest.json") return handleManifest(res);

  const tier = pathname.startsWith("/gh/") ? "gh" : pathname.startsWith("/gitee/") ? "gitee" : null;
  if (!tier) {
    sendText(res, 404, `只认 /gh/**、/gitee/**、/manifest.json，收到 ${pathname}\n`);
    log(`[?] ${req.method} ${pathname} → 404（未知前缀）`);
    return;
  }

  if (tier === "gh" && ghHang) {
    // 挂起：不写任何响应头，也不结束响应。server.requestTimeout 已置 0，所以会一直吊着，
    // 直到客户端自己超时断开 —— 这正是"连不上 GitHub"的复现方式。
    log(`[gh] ${req.method} ${pathname} → 挂起（模拟连不上 GitHub：收下请求，永不回数据）`);
    req.socket.setKeepAlive(true);
    return;
  }

  const file = resolveInRoot(pathname.slice(tier === "gh" ? 4 : 7));
  if (!file) {
    sendText(res, 403, `路径越界：${pathname}\n`);
    log(`[${tier}] ${req.method} ${pathname} → 403（路径越界）`);
    return;
  }
  if (!fs.existsSync(file) || !fs.statSync(file).isFile()) {
    sendText(res, 404, `发布仓里没有这个文件：${rel(file)}\n`);
    log(`[${tier}] ${req.method} ${pathname} → 404（${rel(file)} 不存在）`);
    return;
  }
  const rate = tier === "gh" ? GH_RATE : GITEE_RATE;
  if (rate > 0) await sendThrottled(req, res, file, rate, tier, pathname);
  else sendFull(req, res, file, tier, pathname);
}

const server = http.createServer((req, res) => {
  handler(req, res).catch((e) => {
    log(`[!] 处理 ${req.method} ${req.url} 时出错：${e.message}`);
    if (!res.headersSent) sendText(res, 500, `${e.message}\n`);
    else res.destroy();
  });
});
// 挂起模式要一直吊着；Node 默认 requestTimeout 是 300s，会把"挂起"变成"5 分钟后 408"。
server.requestTimeout = 0;
server.headersTimeout = 0;
server.on("error", (e) => die(`镜像台启动失败（端口 ${port}）`, e.message));

server.listen(port, "127.0.0.1", () => {
  step(`镜像演练台已启动：http://127.0.0.1:${port}`);
  note(`发布仓 ${root}`);
  note(`/gh/**    ${ghHang ? "挂起（收请求，永不回数据）" : GH_RATE ? `限速 ${GH_RATE / 1024} KB/s` : "不限速"}`);
  note(`/gitee/** ${GITEE_RATE ? `限速 ${GITEE_RATE / 1024} KB/s` : "不限速"}`);
  if (ghHang && ghKbps) note("--gh-hang 优先于 --gh-kbps：GitHub 档现在是挂起。");
  if (fs.existsSync(MANIFEST)) {
    const listed = JSON.parse(fs.readFileSync(MANIFEST, "utf8"));
    note(`/manifest.json → update.json（version=${listed.version}）`);
    if (wantVersion && listed.version !== wantVersion) {
      note(`注意：你 --version ${wantVersion}，但清单里是 ${listed.version}；客户端会按清单里的版本来。`);
    }
  } else {
    note(`/manifest.json → 还没有 update.json（先跑 node scripts/release.mjs --local --skip-build --version <版本>）`);
  }
  note(`客户端：ORBDROP_UPDATE_MANIFEST=http://127.0.0.1:${port}/manifest.json npm run tauri dev`);
  note(`演练 Gitee 附件路径：http://127.0.0.1:${port}/gitee/assets/v${wantVersion ?? "<版本>"}/orbdrop.exe`);
  note("Ctrl+C 结束。");
});