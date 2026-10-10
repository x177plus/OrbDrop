//! 应用自更新（桌面端与安卓端共用同一套检查/下载逻辑）。
//!
//! # 为什么不用官方的 `tauri-plugin-updater`
//!
//! 两条硬约束把它排除了：
//!
//! 1. **桌面端是单文件 exe**（`tauri build --no-bundle` 的产物，没有安装器）。官方插件在
//!    Windows 上唯一的落地方式是"下载 NSIS/MSI 安装包、把它拉起来、自己退出"——我们没有
//!    安装包，我们要的是**用新 exe 覆盖当前 exe**（覆盖完仍只有一个文件）。这件事必须自己写
//!    （见 [`apply`]：Windows 上运行中的 exe 不能被写，所以由"新 exe 本身"当 helper 去等旧
//!    进程退出再覆盖）。
//! 2. **镜像切换要按"快慢"判，不只是按状态码**。官方插件的 `endpoints` 是"第一个返回 2xx
//!    且能解析的赢"（源码里 `for url in &self.endpoints`），拉不出"连上了但只有几 B/s 就换"
//!    这个判断——而这正是本应用的网络现实：国内直连 github.com 实测 connect 21 秒才失败，
//!    连上时也可能慢到几 KB/s。所以下载环节自己带测速看门狗（见 [`download`]）。
//!
//! 另外官方插件**根本不支持移动端**（plugins-workspace v2 的 README 平台表里 Android 是
//! `x`），安卓那一档无论如何都得自己写。
//!
//! # 信任链
//!
//! 清单（`update.json`）本身只靠 HTTPS 保护，所以**清单不可信**：真正决定"能不能装"的是
//! 产物上的 minisign 签名（私钥只在本机，公钥编进二进制，见 [`UPDATE_PUBKEY`]）。安卓端的
//! 泡影签由系统在安装那一刻校验（包名相同、证书不同直接拒装），清单里的 sha256 只当"下载
//! 完整性"用。
//!
//! # 清单与地址
//!
//! 清单地址是**编译期常量**（[`MANIFEST_ENDPOINTS`]）：检查更新用的地址必须与版本无关，
//! 所以 GitHub 那条走 `releases/latest/download/update.json`（要求最新一版不是 prerelease），
//! Gitee 那条走仓库根目录的 `raw/master/update.json`。而**产物**地址由清单里的 `mirrors`
//! 给（钉住 tag，避免"清单说是 0.0.2、下到的却是别的东西"）。
//!
//! 本机演练（还没有远端仓库时）用环境变量 `ORBDROP_UPDATE_MANIFEST` 覆盖上面那份常量，
//! 指向本机镜像台 —— 见 `scripts/dev-mirror.mjs`。

pub mod apply;

#[cfg(desktop)]
pub use apply::run_helper_from_args;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tauri::{AppHandle, Emitter, Manager};

use crate::state::AppState;
use crate::ws::events::ui;

// ---------------------------------------------------------------------------
// 常量（要改的地方都在这）
// ---------------------------------------------------------------------------

/// 检查更新用的清单地址，**顺序即优先级**（先 GitHub，失败/太慢再 Gitee）。
///
/// ⚠️ `x177plus/OrbDrop` 是待填的仓库位：GitHub/Gitee 上的发布仓建好后，如果 owner 或仓库名
/// 不是这个，改这两行就够了（清单里的产物地址由发布脚本生成，不在这里）。
const MANIFEST_ENDPOINTS: &[&str] = &[
    "https://github.com/x177plus/OrbDrop/releases/latest/download/update.json",
    "https://gitee.com/x177plus/OrbDrop/raw/master/update.json",
];

/// 更新包的签名公钥（minisign，`npm run tauri -- signer generate` 生成，**base64 的
/// minisign 公钥文件内容**——与 Tauri 自己那份 `pubkey` 同一个格式）。
///
/// 私钥在 `~/.tauri/orbdrop.key`，**不在仓库里**：丢了就再也发不出能被老用户接受的更新。
/// 换公钥 = 老版本一律拒装，等于让所有用户重装一次。
const UPDATE_PUBKEY: &str = "dW50cnVzdGVkIGNvbW1lbnQ6IG1pbmlzaWduIHB1YmxpYyBrZXk6IEUyQjg3QkM5NzM2N0NGRDcKUldUWHoyZHp5WHU0NHN4a0dFc3liWjhjMi9RSm11WEE0QlJaRktWWjROSmdaU01tT0RXNUJTaFAK";

/// 建立连接的超时：用户要的就是"短暂超时"。国内直连 github.com 实测 21 秒才失败，
/// 3 秒就放弃能让"切到 Gitee"快一个数量级。
const CONNECT_TIMEOUT: Duration = Duration::from_secs(3);
/// 清单这一趟（连接 + 读完 body）的总超时：清单只有几百字节，8 秒还没完就是这条路不行。
const MANIFEST_TIMEOUT: Duration = Duration::from_secs(8);
/// 首字节超时（连接建立了但对方不回数据 —— 比"连不上"更常见的坏法）。
///
/// **它管两处**：等响应头、以及等第一个数据块。两处都要管：本机演练台上"挂起"那档就是
/// 只收请求、永不回响应头 —— 只在 body 流上加超时的话，`send()` 会一直等下去，
/// 换镜像那一步永远走不到（实测过，整个更新就那么卡死了）。
const FIRST_BYTE_TIMEOUT: Duration = Duration::from_secs(5);
/// 静默超时：连续这么久一个字节都没来，判这条镜像死了。
const STALL_TIMEOUT: Duration = Duration::from_secs(10);
/// 速率下限（B/s）：低于它换镜像。用户的原话是"下载速度很慢几 B"——定 32 KB/s，
/// 一个 10MB 的包 32KB/s 要 5 分钟，比"换一条重下"还慢。
const MIN_BPS: u64 = 32 * 1024;
/// 速率判定的统计窗口：窗口太短会被"TCP 慢启动 + 突发"骗过去，太长又白等。
const SPEED_WINDOW: Duration = Duration::from_secs(10);
/// 镜像记忆的有效期：上次成功的镜像 24 小时内优先用它。没有这条，国内用户每次检查都要
/// 先把 GitHub 那条超时路径走一遍。
const MIRROR_PREF_TTL: Duration = Duration::from_secs(24 * 3600);
/// 静默检查（启动后自动跑的那次）多久内复用上次结论：设置面板每开一次都重新联网没必要。
const SILENT_CHECK_CACHE: Duration = Duration::from_secs(30 * 60);
/// 进度事件的节流：按"距上次发事件多久"发，而不是按字节 —— 高速镜像上按字节发会把
/// webview 刷爆，而界面上要的只是"进度条在动"。
const PROGRESS_EMIT_EVERY: Duration = Duration::from_millis(120);

// ---------------------------------------------------------------------------
// 清单（`update.json`）
// ---------------------------------------------------------------------------

/// 一份产物：`mirrors` 是要依次尝试的下载地址（顺序 = 优先级）。
#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Artifact {
    /// 字节数：只做"下完了吗"的对账（真正的把关是 sha256 与签名）。
    size: u64,
    /// 小写十六进制 sha256。
    sha256: String,
    /// minisign 签名（`.sig` 文件的内容，base64）。桌面端**必须有**；安卓端不用它。
    #[serde(default)]
    sig: Option<String>,
    /// 下载地址，按优先级排列（GitHub 在前、Gitee 在后）。
    mirrors: Vec<String>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Manifest {
    /// 远端版本（semver，可带前导 `v`）。
    version: String,
    /// 安卓的 versionCode：**安装的硬门槛**（系统拒装比当前小的），所以它单独列出来，
    /// 让"版本号看着更高但 versionCode 没涨"这种发布事故能在检查阶段就被看出来。
    ///
    /// 桌面端不读它（那边的门槛是 exe 自己的版本号），所以只在移动端被用到。
    #[cfg_attr(desktop, allow(dead_code))]
    #[serde(default)]
    version_code: u32,
    #[serde(default)]
    notes: Option<String>,
    /// 发布日期（RFC 3339）。只用于显示。
    #[serde(default)]
    pub_date: Option<String>,
    /// 桌面端产物：`windows-x86_64`（将来要加平台就加键，客户端按键取）。
    #[serde(default)]
    desktop: HashMap<String, Artifact>,
    /// 安卓端产物：`universal` / `arm64-v8a`。
    #[serde(default)]
    android: HashMap<String, Artifact>,
}

impl Manifest {
    /// 本机该拿哪一个产物。取不到就是"这版没为本机出包"，检查阶段直接报错——
    /// 比下载到一半才发现要干净。
    fn artifact_for_this_platform(&self) -> Result<Artifact, String> {
        let (table, key) = if cfg!(desktop) {
            (&self.desktop, desktop_key())
        } else {
            (&self.android, android_key())
        };
        table
            .get(key)
            .cloned()
            .ok_or_else(|| format!("这一版没有为 {key} 出包（清单里缺这一档）"))
    }
}

/// 桌面端在清单里的键。只出 Windows 一份：本应用是 Windows 桌面端 + 安卓端
/// （将来要出别的平台就往清单里加键，客户端按键取）。
fn desktop_key() -> &'static str {
    "windows-x86_64"
}

/// 安卓端在清单里的键：**按本机架构选包**。
///
/// 用编译期架构而不是问系统要 `Build.SUPPORTED_ABIS`：能跑到这里的进程本来就说明
/// "这个 ABI 的 so 装得进去"（arm64 设备上跑的就是 arm64 的库），而 `universal` 包
/// 内部也带 aarch64 的库 —— 两种装法的 `ARCH` 都是 `aarch64`，判断一致。
/// 非 aarch64（armv7 / x86 模拟器）一律退到 universal 包。
fn android_key() -> &'static str {
    if std::env::consts::ARCH == "aarch64" {
        "arm64-v8a"
    } else {
        "universal"
    }
}

// ---------------------------------------------------------------------------
// 状态机（前端只认这一份快照 + `update://state` 事件）
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Phase {
    Idle,
    Checking,
    /// 已是最新版（语义上"检查完成了、没得更新"）。
    Uptodate,
    /// 有新版本，等用户点。
    Available,
    Downloading,
    Verifying,
    /// 正在交给 helper/安装器 —— 桌面端到这一步进程就快没了，界面只来得及显示一句。
    Applying,
    Error,
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct UpdateState {
    /// 本机版本。
    pub current: String,
    /// `desktop` / `mobile`：界面据此决定文案（"更新并重启" / "下载并安装"）。
    pub platform: &'static str,
    pub phase: Phase,
    /// 远端版本（检查到之后才有）。
    pub version: Option<String>,
    pub notes: Option<String>,
    pub pub_date: Option<String>,
    /// 要下载的字节数（有更新时才有）。
    pub size: Option<u64>,
    /// 已下载字节（`Downloading` 时界面用它画进度条）。
    pub received: u64,
    /// 当前正在用的镜像主机名（界面显示"正在从 gitee.com 下载"）。
    pub mirror: Option<String>,
    pub error: Option<String>,
}

impl Default for UpdateState {
    fn default() -> Self {
        Self {
            current: String::new(),
            platform: platform_name(),
            phase: Phase::Idle,
            version: None,
            notes: None,
            pub_date: None,
            size: None,
            received: 0,
            mirror: None,
            error: None,
        }
    }
}

fn platform_name() -> &'static str {
    if cfg!(desktop) {
        "desktop"
    } else {
        "mobile"
    }
}

/// 内部状态：给前端的快照 + "检查到的这一版"（`apply` 直接用它，不重新检查一遍）。
#[derive(Default)]
struct Ctl {
    state: UpdateState,
    manifest: Option<Manifest>,
    artifact: Option<Artifact>,
    /// 上次检查完成的时刻（静默检查的缓存判据）。
    last_check: Option<Instant>,
}

static CTL: std::sync::LazyLock<Mutex<Ctl>> = std::sync::LazyLock::new(|| Mutex::new(Ctl::default()));

fn ctl() -> &'static Mutex<Ctl> {
    &CTL
}

/// 改状态 + 把新快照发一份给界面。
///
/// **先解锁再发事件**：`emit` 会走到 webview 那边，前端可能立刻回调 `update_state`，
/// 拿着锁发事件就是自己等自己。
fn put(app: &AppHandle, f: impl FnOnce(&mut Ctl)) {
    let snapshot = {
        let mut c = ctl().lock();
        f(&mut c);
        c.state.clone()
    };
    let _ = app.emit(ui::UPDATE_STATE, snapshot);
}

/// 当前快照（界面挂载时拿一次初值，之后靠事件）。
pub fn snapshot() -> UpdateState {
    ctl().lock().state.clone()
}

fn in_flight(phase: Phase) -> bool {
    matches!(
        phase,
        Phase::Checking | Phase::Downloading | Phase::Verifying | Phase::Applying
    )
}

// ---------------------------------------------------------------------------
// 启动
// ---------------------------------------------------------------------------

/// 启动时的一次性装配：填本机版本、清上次更新留下的残渣、安排一次静默检查。
pub fn init(app: &AppHandle) {
    let current = app.package_info().version.to_string();
    {
        let mut c = ctl().lock();
        c.state.current = current;
        c.state.platform = platform_name();
    }
    // 上次更新可能留下 `<exe>.old` 或临时目录（helper 自己删不掉的那些）。
    #[cfg(desktop)]
    apply::cleanup_stale();

    // 静默检查：**延迟 8 秒**。启动瞬间是应用自己最忙的时候（起服务器、注册 mDNS、
    // 前端首帧），没必要再挤一条 HTTPS 进去；而用户真要点"检查更新"也是一分钟后的事。
    let handle = app.clone();
    tauri::async_runtime::spawn(async move {
        tokio::time::sleep(Duration::from_secs(8)).await;
        if let Err(e) = check(handle.clone(), false).await {
            // 静默检查失败**不弹错**：没网、被墙、镜子挂了都是常态，用户没要求这一趟。
            // 记日志即可（真要看结果时，设置面板里点一次"检查更新"会给明确报错）。
            tracing::debug!("启动时的更新检查没成功：{e}");
        }
    });
}

// ---------------------------------------------------------------------------
// 检查
// ---------------------------------------------------------------------------

/// 检查更新。
///
/// `force = true`（用户手动点）忽略静默检查的 30 分钟缓存与镜像记忆；`false` 是启动那次。
/// 已经有检查/下载在跑时**不排队、不报错**，直接把当前快照还回去 —— 界面跟着事件走就行。
pub async fn check(app: AppHandle, force: bool) -> Result<UpdateState, String> {
    {
        let c = ctl().lock();
        if in_flight(c.state.phase) {
            return Ok(c.state.clone());
        }
        // 静默检查：上一次结论还在有效期内就直接复用，不联网。
        if !force {
            if let Some(at) = c.last_check {
                if at.elapsed() < SILENT_CHECK_CACHE {
                    return Ok(c.state.clone());
                }
            }
        }
    }

    put(&app, |c| {
        c.state.phase = Phase::Checking;
        c.state.error = None;
        c.state.received = 0;
    });

    let manifest = match fetch_manifest(&app).await {
        Ok(m) => m,
        Err(e) => {
            put(&app, |c| {
                c.state.phase = Phase::Error;
                c.state.error = Some(e.clone());
                c.last_check = None;
            });
            return Err(e);
        }
    };

    let current = parse_version(&snapshot().current)?;
    let remote = parse_version(&manifest.version)?;

    // 版本比本机高才算有更新。**降级不装**（用户自测时把 0.1.0 退回 0.0.1 那次就是靠这条
    // 兜的）：自更新一旦会降级，线上出事故想回滚就得先发一个"版本号更高但内容是旧的"版本，
    // 那是另一套流程，这里不掺进来。
    if remote <= current {
        put(&app, |c| {
            c.manifest = None;
            c.artifact = None;
            c.state.phase = Phase::Uptodate;
            c.state.version = Some(manifest.version.clone());
            c.state.notes = manifest.notes.clone();
            c.state.pub_date = manifest.pub_date.clone();
            c.state.size = None;
            c.last_check = Some(Instant::now());
        });
        return Ok(snapshot());
    }

    let artifact = manifest.artifact_for_this_platform()?;
    // 安卓的安装门槛在 versionCode 上，不在版本号上：清单的版本号更高但 versionCode 没涨时，
    // 系统会以 `INSTALL_FAILED_VERSION_DOWNGRADE` 拒装（用户看到的是"安装失败"，看不出原因）。
    // 这种事只会发生在发布方漏了递增，所以在**检查阶段**就说清楚。
    #[cfg(mobile)]
    ensure_installable(&current, &manifest)?;
    put(&app, |c| {
        c.state.phase = Phase::Available;
        c.state.version = Some(manifest.version.clone());
        c.state.notes = manifest.notes.clone();
        c.state.pub_date = manifest.pub_date.clone();
        c.state.size = Some(artifact.size);
        c.last_check = Some(Instant::now());
        c.artifact = Some(artifact);
        c.manifest = Some(manifest);
    });
    Ok(snapshot())
}

/// 安卓端的安装门槛：清单里的 versionCode 必须**大于**本机已装的那个。
///
/// 本机的 versionCode 由版本号推出来（Tauri 的算法，写在它自己的配置 schema 里：
/// `versionCode = major * 1000000 + minor * 1000 + patch`，`gen/android/app/tauri.properties`
/// 里就有现成的一份）。这里不额外向系统要真实的 versionCode：那是 Kotlin 侧的事，
/// 而这道校验要挡的是"发布方没递增"，用同一个公式算出来的值足以对齐。
#[cfg(mobile)]
fn ensure_installable(current: &semver::Version, manifest: &Manifest) -> Result<(), String> {
    let mine = version_code_of(current);
    if u64::from(manifest.version_code) <= mine {
        return Err(format!(
            "这一版的 versionCode（{}）没有超过本机的（{}），系统会拒装 —— 发布方漏了递增",
            manifest.version_code, mine
        ));
    }
    Ok(())
}

#[cfg(mobile)]
fn version_code_of(v: &semver::Version) -> u64 {
    v.major as u64 * 1_000_000 + v.minor as u64 * 1_000 + v.patch as u64
}

/// 拉清单：按优先级依次试每个地址，第一个能解析成清单的赢。
///
/// 与下载不同的是：清单很小，这里**不需要测速**（8 秒总超时已经覆盖"连上了但不回数据"）。
async fn fetch_manifest(app: &AppHandle) -> Result<Manifest, String> {
    let urls = manifest_endpoints();
    let mut last = String::from("没有任何清单地址");
    for url in ordered_by_pref(app, urls) {
        if let Err(e) = ensure_url_ok(&url) {
            last = e;
            continue;
        }
        match fetch_manifest_at(&url).await {
            Ok(m) => {
                remember_mirror(app, &url);
                return Ok(m);
            }
            Err(e) => {
                tracing::debug!("清单地址 {url} 不行：{e}");
                last = format!("{url}：{e}");
            }
        }
    }
    Err(format!("所有清单地址都拿不到更新信息（{last}）"))
}

async fn fetch_manifest_at(url: &str) -> Result<Manifest, String> {
    let client = http_client()?;
    let fut = async {
        let res = client.get(url).send().await.map_err(|e| e.to_string())?;
        if !res.status().is_success() {
            return Err(format!("HTTP {}", res.status()));
        }
        let body = res.bytes().await.map_err(|e| e.to_string())?;
        serde_json::from_slice::<Manifest>(&body).map_err(|e| format!("清单格式不对：{e}"))
    };
    match tokio::time::timeout(MANIFEST_TIMEOUT, fut).await {
        Ok(r) => r,
        Err(_) => Err(format!("{} 秒没读完", MANIFEST_TIMEOUT.as_secs())),
    }
}

/// 清单地址：默认是编译期常量，演练时可用 `ORBDROP_UPDATE_MANIFEST` 覆盖（逗号分隔）。
fn manifest_endpoints() -> Vec<String> {
    match std::env::var("ORBDROP_UPDATE_MANIFEST") {
        Ok(v) => v
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .collect(),
        Err(_) => MANIFEST_ENDPOINTS.iter().map(|s| s.to_string()).collect(),
    }
}

// ---------------------------------------------------------------------------
// 下载
// ---------------------------------------------------------------------------

/// 下载到 `dest`：按 `mirrors` 顺序试，"连不上 / 太慢 / 校验不过"都接着换下一个。
///
/// 换镜像时**从零重下**（`File::create` 会截断）：Gitee 的发行版附件实测**不支持 Range**
/// （发 `Range: bytes=…` 回来的是 `200` + 完整 `Content-Length`，没有 `Content-Range`），
/// 而断点续传一旦跨了镜像，"已下多少"就没有意义了 —— 两个地址是两份独立文件。
async fn download(app: &AppHandle, mirrors: &[String], dest: &Path) -> Result<(), String> {
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("建不了下载目录：{e}"))?;
    }
    let mut last = String::from("清单里没有可用的下载地址");
    let mut first = true;
    for url in ordered_by_pref(app, mirrors.to_vec()) {
        if let Err(e) = ensure_url_ok(&url) {
            last = e;
            continue;
        }
        if !first {
            tracing::info!("换镜像：{url}");
        }
        first = false;
        match download_one(app, &url, dest).await {
            Ok(()) => {
                remember_mirror(app, &url);
                return Ok(());
            }
            Err(e) => {
                tracing::warn!("镜像 {url} 没下成：{e}");
                put(app, |c| {
                    c.state.error = Some(e.clone());
                });
                last = format!("{url}：{e}");
            }
        }
    }
    Err(format!("所有镜像都没下成（{last}）"))
}

/// 单镜像下载：带首字节超时、静默超时与速率看门狗。
async fn download_one(app: &AppHandle, url: &str, dest: &Path) -> Result<(), String> {
    use futures_util::StreamExt;
    use tokio::io::AsyncWriteExt;

    let host = host_of(url);
    let client = http_client()?;
    // 等响应头这一跳也要有超时：对方可以建好连接、收下请求，然后什么都不回。
    // 少了这一层，下面那句"换镜像"就永远轮不到（`send()` 会一直等）。
    let res = match tokio::time::timeout(FIRST_BYTE_TIMEOUT, client.get(url).send()).await {
        Err(_) => {
            return Err(format!(
                "{} 秒没有响应（连接建立了但不回数据）",
                FIRST_BYTE_TIMEOUT.as_secs()
            ))
        }
        Ok(Err(e)) => return Err(e.to_string()),
        Ok(Ok(r)) => r,
    };
    if !res.status().is_success() {
        return Err(format!("HTTP {}", res.status()));
    }
    let total = res.content_length();
    put(app, |c| {
        c.state.phase = Phase::Downloading;
        c.state.mirror = Some(host.clone());
        c.state.received = 0;
        c.state.error = None;
    });

    let mut file = tokio::fs::File::create(dest)
        .await
        .map_err(|e| format!("写不了 {dest:?}：{e}"))?;
    let mut stream = res.bytes_stream();

    let mut received: u64 = 0;
    let mut win_start = Instant::now();
    let mut win_bytes: u64 = 0;
    let mut last_emit = Instant::now();

    loop {
        // 第一个字节用更短的超时：连接建立了但对方不回数据，是最常见的"这条镜像不行"。
        let wait = if received == 0 {
            FIRST_BYTE_TIMEOUT
        } else {
            STALL_TIMEOUT
        };
        let chunk = match tokio::time::timeout(wait, stream.next()).await {
            Err(_) => {
                return Err(format!(
                    "{} 秒没有数据（{}）",
                    wait.as_secs(),
                    if received == 0 { "还没开始" } else { "中途卡住" }
                ))
            }
            Ok(None) => break,
            Ok(Some(Err(e))) => return Err(format!("传输中断：{e}")),
            Ok(Some(Ok(c))) => c,
        };

        received += chunk.len() as u64;
        win_bytes += chunk.len() as u64;
        file.write_all(&chunk)
            .await
            .map_err(|e| format!("写文件失败：{e}"))?;

        // 速率看门狗：满一个窗口就判一次。判据是**窗口内平均速率**而不是瞬时值——
        // 瞬时值会被每次 TCP 突发骗过去（一坨数据到了就是几十 MB/s）。
        let elapsed = win_start.elapsed();
        if elapsed >= SPEED_WINDOW {
            let bps = win_bytes as f64 / elapsed.as_secs_f64();
            if bps < MIN_BPS as f64 {
                return Err(format!(
                    "平均 {:.1} KB/s（低于 {} KB/s），这条镜像太慢",
                    bps / 1024.0,
                    MIN_BPS / 1024
                ));
            }
            win_start = Instant::now();
            win_bytes = 0;
        }

        if last_emit.elapsed() >= PROGRESS_EMIT_EVERY {
            last_emit = Instant::now();
            put(app, |c| {
                c.state.received = received;
                c.state.size = total.or(c.state.size);
            });
        }
    }

    file.flush().await.map_err(|e| format!("落盘失败：{e}"))?;
    drop(file);
    put(app, |c| {
        c.state.received = received;
    });
    Ok(())
}

// ---------------------------------------------------------------------------
// 校验
// ---------------------------------------------------------------------------

/// sha256 + minisign 两道校验。**少了任何一道都不许装**：
/// sha256 防传输损坏，签名防"镜像/账号被拿去换了包"（清单不可信，见模块文档）。
fn verify_file(path: &Path, art: &Artifact, require_sig: bool) -> Result<(), String> {
    let bytes = std::fs::read(path).map_err(|e| format!("读不回下载的文件：{e}"))?;
    let got = hex_lower(&Sha256::digest(&bytes));
    if !got.eq_ignore_ascii_case(art.sha256.trim()) {
        return Err(format!("sha256 对不上（清单 {} ≠ 实际 {}）", art.sha256.trim(), got));
    }
    if bytes.len() as u64 != art.size {
        return Err(format!("大小对不上（清单 {} ≠ 实际 {}）", art.size, bytes.len()));
    }
    if require_sig {
        let sig = art
            .sig
            .as_deref()
            .ok_or("清单里没有签名，拒绝安装")?;
        verify_signature(&bytes, sig)?;
    }
    Ok(())
}

/// 按 Tauri 自己那份实现校验（`tauri-plugin-updater` 的 `verify_signature`）：
/// 公钥与签名都是**base64 包着 minisign 文本**，不是裸的 minisign 文件。
/// `allow_legacy = true` 与它保持一致 —— 老版本 minisign 生成的签名也接受。
fn verify_signature(data: &[u8], sig_b64: &str) -> Result<(), String> {
    use base64::Engine as _;
    let engine = base64::engine::general_purpose::STANDARD;
    let pub_text = String::from_utf8(
        engine
            .decode(UPDATE_PUBKEY)
            .map_err(|e| format!("内置公钥坏了：{e}"))?,
    )
    .map_err(|e| format!("内置公钥不是文本：{e}"))?;
    let key = minisign_verify::PublicKey::decode(&pub_text)
        .map_err(|e| format!("公钥格式不对：{e}"))?;
    let sig_text = String::from_utf8(
        engine
            .decode(sig_b64.trim())
            .map_err(|e| format!("签名不是 base64：{e}"))?,
    )
    .map_err(|e| format!("签名不是文本：{e}"))?;
    let sig = minisign_verify::Signature::decode(&sig_text)
        .map_err(|e| format!("签名格式不对：{e}"))?;
    key.verify(data, &sig, true)
        .map_err(|e| format!("签名校验不过：{e}（这个包不是本项目的私钥签的）"))
}

fn hex_lower(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        use std::fmt::Write as _;
        let _ = write!(s, "{b:02x}");
    }
    s
}

// ---------------------------------------------------------------------------
// 落地（覆盖当前 exe / 拉起系统安装器）
// ---------------------------------------------------------------------------

/// 下载并安装。桌面端会**覆盖当前 exe 并重启**（进程随之中断，函数返回前就退出）；
/// 安卓端下到应用私有目录后交给系统安装器。
pub async fn apply(app: AppHandle) -> Result<(), String> {
    {
        let c = ctl().lock();
        if in_flight(c.state.phase) {
            return Err("上一个更新动作还没结束".into());
        }
    }
    // 没检查过（或者检查完已经过期）就先查一次：界面可能允许用户直接点"更新"。
    let art = {
        let existing = ctl().lock().artifact.clone();
        match existing {
            Some(a) => a,
            None => {
                check(app.clone(), true).await?;
                ctl().lock()
                    .artifact
                    .clone()
                    .ok_or("当前没有可安装的新版本")?
            }
        }
    };
    let version = ctl()
        .lock()
        .state
        .version
        .clone()
        .ok_or("当前没有可安装的新版本")?;

    let dest = download_dest(&version);
    put(&app, |c| {
        c.state.phase = Phase::Downloading;
        c.state.error = None;
    });
    if let Err(e) = download(&app, &art.mirrors, &dest).await {
        // 下载整条路走完都没成：**必须把阶段推成 `error`**。停在 `downloading` 的话界面会
        // 一直显示"正在下载"（进度条再也不动），用户看到的是一台卡住的应用而不是一条失败。
        put(&app, |c| {
            c.state.phase = Phase::Error;
            c.state.error = Some(e.clone());
        });
        return Err(e);
    }

    put(&app, |c| {
        c.state.phase = Phase::Verifying;
    });
    let checked = {
        #[cfg(desktop)]
        {
            verify_file(&dest, &art, true)
        }
        #[cfg(mobile)]
        {
            verify_file(&dest, &art, false)
        }
    };
    if let Err(e) = checked {
        put(&app, |c| {
            c.state.phase = Phase::Error;
            c.state.error = Some(e.clone());
        });
        return Err(e);
    }

    put(&app, |c| {
        c.state.phase = Phase::Applying;
    });

    #[cfg(desktop)]
    {
        let target = std::env::current_exe().map_err(|e| format!("找不到当前 exe：{e}"))?;
        if let Err(e) = apply::spawn_helper(&dest, &target) {
            put(&app, |c| {
                c.state.phase = Phase::Error;
                c.state.error = Some(e.clone());
            });
            return Err(e);
        }
        tracing::info!(from = %target.display(), to = %dest.display(), "已交给 helper 覆盖并重启");
        // 日志是**非阻塞**写入（`tracing-appender`）：不刷这一下，"已交给 helper"这行可能
        // 还没落盘进程就走了 —— 而这行正是排查"更新到底走到哪一步"的锚点。
        crate::log::flush();
        // helper 是**新 exe 自己**：它等本进程退出后把新文件覆盖到当前路径再拉起来。
        // 这里立刻退出，别拖 —— 拖一秒用户就多看一秒"正在更新"。
        //
        // `exit()` 只是**排队**退出（触发 RunEvent::ExitRequested → Exit），中间还有谁
        // 拦一把的话进程就留着，而进程还在 = 当前 exe 还锁着 = helper 覆盖失败。所以给它
        // 一小段体面时间走完自己的退出，再硬退保底：文件锁必须立刻释放。
        app.exit(0);
        std::thread::sleep(Duration::from_millis(300));
        std::process::exit(0);
    }
    #[cfg(mobile)]
    {
        // 系统安装器（PackageInstaller）：非系统应用**没有**静默安装这条路，
        // 用户必须自己确认一次系统弹窗 —— 这就是侧载的标准姿势。
        // APK 已经下好且校验过了，所以这一步失败（比如没开"安装未知应用"）也要说清楚位置。
        match crate::mobile::open_file(dest.to_string_lossy().to_string()).await {
            Ok(()) => Ok(()),
            Err(e) => {
                let msg = format!("{e}（安装包已下好，在 {}）", dest.display());
                put(&app, |c| {
                    c.state.phase = Phase::Error;
                    c.state.error = Some(msg.clone());
                });
                Err(msg)
            }
        }
    }
}

#[cfg(test)]
mod tests;

/// 下到哪：桌面落到临时目录（helper 要从那里把自己覆盖过去），安卓落到应用私有目录下的
/// `updates/`（FileProvider 用的是 `root-path`，覆盖得到）。
fn download_dest(version: &str) -> PathBuf {
    #[cfg(desktop)]
    {
        std::env::temp_dir()
            .join(format!("orbdrop-update-{version}"))
            .join("orbdrop.exe")
    }
    #[cfg(mobile)]
    {
        crate::config::data_dir()
            .join("updates")
            .join(format!("orbdrop-{version}.apk"))
    }
}

// ---------------------------------------------------------------------------
// 镜像偏好（落盘在配置里，见 `config::UpdateMirror`）
// ---------------------------------------------------------------------------

/// 把候选地址按"上次成功的镜像"重排：命中的排前面，其余保持原顺序（稳定）。
fn ordered_by_pref(app: &AppHandle, urls: Vec<String>) -> Vec<String> {
    let Some(pref) = read_pref(app) else {
        return urls;
    };
    let (mut first, rest): (Vec<_>, Vec<_>) =
        urls.into_iter().partition(|u| host_of(u) == pref);
    first.extend(rest);
    first
}

fn read_pref(app: &AppHandle) -> Option<String> {
    let state = app.state::<Arc<AppState>>();
    let cfg = state.config.lock();
    let pref = cfg.update_mirror.as_ref()?;
    let age = now_ms().saturating_sub(pref.at);
    if age < 0 || age as u64 > MIRROR_PREF_TTL.as_millis() as u64 {
        // 过期就当作没有：网络环境可能变了（挂上代理、Gitee 抽风），别把用户永久钉在
        // 一条坏路上。
        return None;
    }
    Some(pref.host.clone())
}

fn remember_mirror(app: &AppHandle, url: &str) {
    let host = host_of(url);
    if host.is_empty() {
        return;
    }
    let state = app.state::<Arc<AppState>>();
    let mut cfg = state.config.lock();
    if cfg.update_mirror.as_ref().is_some_and(|p| p.host == host) {
        return;
    }
    cfg.update_mirror = Some(crate::config::UpdateMirror {
        host,
        at: now_ms(),
    });
    if let Err(e) = crate::config::save_config_at(&state.config_path, &cfg) {
        tracing::warn!("记不住更新镜像偏好：{e}");
    }
}

// ---------------------------------------------------------------------------
// 小工具
// ---------------------------------------------------------------------------

fn http_client() -> Result<reqwest::Client, String> {
    // 只设连接超时：读的超时由下载看门狗按"有没有数据进来"判，比一刀切的总超时准得多
    // （大文件在慢网上下 5 分钟是正常的，只要它一直在动）。
    reqwest::Client::builder()
        .connect_timeout(CONNECT_TIMEOUT)
        .build()
        .map_err(|e| format!("建 HTTP 客户端失败：{e}"))
}

fn host_of(url: &str) -> String {
    url::Url::parse(url)
        .ok()
        .and_then(|u| u.host_str().map(str::to_string))
        .unwrap_or_default()
}

/// 只放行 https（本机演练要 http，所以放行环回地址）。
///
/// 清单里的地址是**远端可控的字符串**：不卡这一关，"清单被改"就能把用户导到任意 http 源上，
/// 而 http 之上连"这份清单还是那个清单"都不能保证。
fn ensure_url_ok(url: &str) -> Result<(), String> {
    let u = url::Url::parse(url).map_err(|e| format!("地址不合法（{url}）：{e}"))?;
    match u.scheme() {
        "https" => Ok(()),
        "http" => {
            let host = u.host_str().unwrap_or_default();
            if host == "127.0.0.1" || host == "localhost" || host == "[::1]" || host == "::1" {
                Ok(())
            } else {
                Err(format!("拒绝非 https 的更新地址（{url}）"))
            }
        }
        other => Err(format!("拒绝 {other} 协议的更新地址（{url}）")),
    }
}

/// semver 解析：容忍前导 `v`（清单里写 `v0.0.2` 也认）。
fn parse_version(s: &str) -> Result<semver::Version, String> {
    semver::Version::parse(s.trim().trim_start_matches('v'))
        .map_err(|e| format!("版本号 {s:?} 不是合法的 semver：{e}"))
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// 命令
// ---------------------------------------------------------------------------

#[tauri::command]
pub fn update_state() -> UpdateState {
    snapshot()
}

#[tauri::command]
pub async fn update_check(app: AppHandle, force: bool) -> Result<UpdateState, String> {
    check(app, force).await
}

#[tauri::command]
pub async fn update_apply(app: AppHandle) -> Result<(), String> {
    apply(app).await
}

