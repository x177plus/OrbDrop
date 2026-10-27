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
//!
//! **这两条地址连的是公网域名，靠的是 reqwest 内置的公共 CA 列表**（`Cargo.toml` 里的
//! `rustls-tls-webpki-roots-no-provider`）：到对端那一跳用指纹判身份、不需要信任库，而这里没有
//! pinning 可用，只能走普通证书校验。所以那份 feature 一被改回"只装 provider、不带 roots"
//! （`rustls-tls-no-provider` = manual-roots = 空信任库），这里每条 https 都会
//! `invalid peer certificate: UnknownIssuer` —— 而本机演练走的是**明文**回环 http，
//! 整条链路照样绿。真机上出事的那种"所有清单地址都拿不到更新信息"就是这么来的。

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
/// 静默检查两次之间的**最短间隔**：上一次成功还在这个窗口内就直接复用，不联网。
///
/// 判据跨进程（落盘），所以崩溃重启 / 反复拉起不会把镜像打成筛子。
const SILENT_CHECK_CACHE: Duration = Duration::from_secs(30 * 60);
/// 启动后第一次静默检查的延迟：启动瞬间是应用最忙的时候（起服务器、注册 mDNS、前端首帧）。
const STARTUP_CHECK_DELAY: Duration = Duration::from_secs(8);
/// 运行期间的常态检查间隔。
///
/// 6 小时的理由：更新不是急事（本应用刻意不发系统通知）；清单只有几 KB，一天 4 次对
/// 镜像没有压力；而托盘常驻 / 安卓前台服务都可能几周不重启，"只在启动查"会让更新晚到几天。
const CHECK_INTERVAL: Duration = Duration::from_secs(6 * 3600);
/// 常态间隔的抖动幅度（±30 分钟）：发布当刻所有客户端不会同一秒打同一个镜像。
const CHECK_JITTER: Duration = Duration::from_secs(30 * 60);
/// "用户回到前台 / 再次拉起"这一档的最小复查间隔：用户反复切前后台不该反复联网。
const RESUME_MIN_GAP: Duration = Duration::from_secs(60 * 60);
/// 静默检查连续失败后的重试间隔；用完之后按 [`CHECK_INTERVAL`]（即 6 小时封顶）。
const RETRY_BACKOFF: &[Duration] = &[
    Duration::from_secs(5 * 60),
    Duration::from_secs(30 * 60),
    Duration::from_secs(2 * 3600),
];
/// 调度器一次最多睡这么久，醒来按墙钟重算。
///
/// 休眠/挂起期间单调时钟会停（Linux 的 `CLOCK_MONOTONIC` 就不算挂起时间），一个 6 小时的
/// `sleep` 在睡了一晚之后可能还剩 5 小时。分段睡 + 墙钟判定才是对的。
const MAX_SLEEP: Duration = Duration::from_secs(5 * 60);
/// 进度事件的节流：按"距上次发事件多久"发，而不是按字节 —— 高速镜像上按字节发会把
/// webview 刷爆，而界面上要的只是"进度条在动"。
const PROGRESS_EMIT_EVERY: Duration = Duration::from_millis(120);

/// 清单体积上限：正常 `update.json` 只有几 KB（带全部安卓 ABI 也就几十 KB）。
const MANIFEST_MAX_BYTES: usize = 1024 * 1024;
/// 安装包体积的硬上限：清单不可信（见模块文档），体积字段本身也可能是任意值。
/// 桌面端单个 exe / 安卓 apk 都在几十 MB 量级，512MB 留足余量。
const ARTIFACT_MAX_BYTES: u64 = 512 * 1024 * 1024;

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
    /// 桌面端产物：`windows-x86_64` / `macos-aarch64` / `macos-x86_64` / `macos-universal`
    /// （客户端按键取，见 `desktop_keys`）。
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
        if cfg!(desktop) {
            // 优先精确架构，其次通用包（macOS 可以只发一份 universal）。找不到时把
            // **本机认过的键**都报出来，别只说一个，否则用户看不出清单到底缺哪一档。
            let keys = desktop_keys();
            for key in keys {
                if let Some(artifact) = self.desktop.get(*key) {
                    return Ok(artifact.clone());
                }
            }
            return Err(format!(
                "这一版没有为 {} 出包（清单 desktop 里缺这一档）",
                keys.join(" / ")
            ));
        }
        self.android
            .get(android_key())
            .cloned()
            .ok_or_else(|| format!("这一版没有为 {} 出包（清单里缺这一档）", android_key()))
    }
}

/// 桌面端在清单里的候选键，按优先级排列。
///
/// Windows 只有 `windows-x86_64`；macOS 先认本机架构，再认通用包 —— 发布侧可以只编一份
/// `macos-universal`，也可以为两种架构分别出包。
fn desktop_keys() -> &'static [&'static str] {
    #[cfg(target_os = "macos")]
    {
        if cfg!(target_arch = "aarch64") {
            return &["macos-aarch64", "macos-universal"];
        }
        return &["macos-x86_64", "macos-universal"];
    }
    #[cfg(not(target_os = "macos"))]
    {
        &["windows-x86_64"]
    }
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
    /// 这份结论是不是**启动时那次静默检查**得的（[`check`] 的 `force = false`）。
    ///
    /// 界面据此决定要不要**主动弹一格**：静默检查是后台自己跑的，用户没问过这件事，所以
    /// 查到新版得有地方说出来；而用户自己点的那次（`force = true`）本来就在设置面板里显示着，
    /// 再弹一遍就是把同一件事说两次。
    pub silent: bool,
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
    /// 用户按过「稍后」的那一版：**同版本不再自动弹**（角标仍挂着），更新的一版照弹。
    /// 落盘在 `update.json`，重启之后同样有效。
    pub dismissed_version: Option<String>,
    pub error: Option<String>,
}

impl Default for UpdateState {
    fn default() -> Self {
        Self {
            current: String::new(),
            platform: platform_name(),
            phase: Phase::Idle,
            // 默认不是静默检查（构建期还没有任何一次检查，界面也就没得弹）。
            silent: false,
            version: None,
            notes: None,
            pub_date: None,
            size: None,
            received: 0,
            mirror: None,
            dismissed_version: None,
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

/// 跨进程保存的一小份更新状态（数据目录下的 `update.json`）。
///
/// **不塞进 `config.toml`**：它不是用户设置，不该走 `update_settings` 的写路径；与
/// `push_out.json` 从 `chat.json` 分家同一个理由（"读进来 → 原样写回"会把不认识的顶层键抹掉）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
struct Persisted {
    /// 上一次**成功**检查的墙钟时刻（Unix 毫秒）。
    ///
    /// 必须用墙钟：`Instant` 在系统挂起/休眠期间不前进，笔记本睡一晚醒来会被当成
    /// "才过了一小时"，周期检查就被无限推后。
    last_success_ms: Option<i64>,
    /// 上次成功检查看到的远端版本（不判断是否比本机新，只用于日志与恢复）。
    latest_version: Option<String>,
    /// 上次查到、还没装的那一版：重启后先按它画，等新的检查结果回来再改。
    available: Option<PersistedAvailable>,
    /// 用户按过「稍后」的那一版：同版本不再自动弹，新版本照弹。
    dismissed_version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
struct PersistedAvailable {
    version: String,
    #[serde(default)]
    notes: Option<String>,
    #[serde(default)]
    pub_date: Option<String>,
    size: u64,
}

/// 内部状态：给前端的快照 + "检查到的这一版"（`apply` 直接用它，不重新检查一遍）。
#[derive(Default)]
struct Ctl {
    state: UpdateState,
    manifest: Option<Manifest>,
    artifact: Option<Artifact>,
    /// 跨进程的那份小状态（成功时刻 / 上次版本 / 按掉的版本）。
    persisted: Persisted,
    /// 连续失败次数：静默检查的退避判据，成功一次归零（不落盘）。
    failures: u32,
}

static CTL: std::sync::LazyLock<Mutex<Ctl>> =
    std::sync::LazyLock::new(|| Mutex::new(Ctl::default()));

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

/// 启动时的一次性装配：填本机版本、恢复上次结论、清残渣、把调度器拉起来。
pub fn init(app: &AppHandle) {
    let current = app.package_info().version.to_string();
    let mut persisted = load_persisted();
    // 上次查到的那一版先画出来：新的检查回来之前（以及这趟失败时）界面不至于空着。
    let restored = restored_available(&persisted, &current);
    if restored.is_none() && persisted.available.take().is_some() {
        // 本机已经装到那一版（或更晚）：过期的"待安装"记录留着只会误报。
        save_persisted(&persisted);
    }
    {
        let mut c = ctl().lock();
        c.state.current = current;
        c.state.platform = platform_name();
        c.state.dismissed_version = persisted.dismissed_version.clone();
        if let Some(a) = restored {
            tracing::debug!(version = %a.version, "恢复上次查到的待安装版本");
            c.state.phase = Phase::Available;
            c.state.version = Some(a.version);
            c.state.notes = a.notes;
            c.state.pub_date = a.pub_date;
            c.state.size = Some(a.size);
            // 上一次运行的后台结论：允许主动提示（按掉的版本由 `dismissedVersion` 挡）。
            c.state.silent = true;
        }
        c.persisted = persisted;
    }
    // 上次更新可能留下 `<exe>.old` 或临时目录（helper 自己删不掉的那些）。
    #[cfg(desktop)]
    apply::cleanup_stale();

    let handle = app.clone();
    tauri::async_runtime::spawn(async move { supervise(handle).await });
}

// ---------------------------------------------------------------------------
// 调度
// ---------------------------------------------------------------------------

static WAKE: std::sync::LazyLock<tokio::sync::Notify> =
    std::sync::LazyLock::new(tokio::sync::Notify::new);

/// 叫醒调度器：用户回到前台 / 再次拉起（安卓 Activity Resumed、桌面单实例、macOS Reopen）。
///
/// **不是"立刻查"**：查不查由调度器按 [`RESUME_MIN_GAP`] 与失败退避决定 —— 用户反复
/// 切前后台不该变成反复联网。
pub fn wake() {
    WAKE.notify_one();
}

/// 静默检查的调度循环。
///
/// 一条时间线管四档：启动 8 秒、常态 6 小时（带抖动）、回前台（距上次成功 ≥1 小时）、
/// 失败退避（5m → 30m → 2h → 6h）。[`check`] 自己带着"正在跑就不排队"的闸门，所以这里的
/// 重复唤醒最多只是白跑一次空转，不会并发打两次网络。
async fn supervise(app: AppHandle) {
    let started = now_ms();
    let startup_delay = STARTUP_CHECK_DELAY.as_millis() as i64;
    let mut next_at = started + startup_delay;

    loop {
        let woken = wait_until(next_at, started).await;
        if woken && !resume_due() {
            // 回来得太勤（或正在退避）：不动原定时刻。
            continue;
        }

        match check(app.clone(), false).await {
            Ok(_) => {}
            Err(e) => tracing::debug!("静默检查没成功：{e}"),
        }

        let now = now_ms();
        let (phase, failures, last_success) = {
            let c = ctl().lock();
            (c.state.phase, c.failures, c.persisted.last_success_ms)
        };
        next_at = if in_flight(phase) {
            // 手动检查 / 下载正在跑：不跟它抢，过一会儿再看结论。
            now + 60_000
        } else if failures > 0 {
            now + retry_delay(failures).as_millis() as i64
        } else {
            due_after_success(
                now,
                last_success,
                check_interval(),
                random_jitter(jitter_span()),
            )
        };
        tracing::debug!(?phase, in_ms = next_at - now, failures, "下一次更新检查已排好");
    }
}

/// 睡到 `next_at`；被 [`wake`] 叫醒时返回 `true`。
///
/// 启动后的前 8 秒叫醒**不算**：安卓 Activity 起来时本来就会来一拍，不能让它绕过
/// "启动让一让"的延迟。
async fn wait_until(next_at: i64, started: i64) -> bool {
    loop {
        let now = now_ms();
        if now >= next_at {
            return false;
        }
        let sleep_ms = (next_at - now).min(MAX_SLEEP.as_millis() as i64) as u64;
        tokio::select! {
            _ = tokio::time::sleep(Duration::from_millis(sleep_ms)) => {}
            _ = WAKE.notified() => {
                if now_ms() - started < STARTUP_CHECK_DELAY.as_millis() as i64 {
                    continue;
                }
                return true;
            }
        }
    }
}

/// 回前台这一档要不要查：正在跑就不插队；成功过一次且不足 [`RESUME_MIN_GAP`] 就不查；
/// 失败退避期间也不查（别让"反复切前后台"把退避冲掉）。
fn resume_due() -> bool {
    let c = ctl().lock();
    if in_flight(c.state.phase) || c.failures > 0 {
        return false;
    }
    match c.persisted.last_success_ms {
        Some(ms) => now_ms().saturating_sub(ms) >= RESUME_MIN_GAP.as_millis() as i64,
        None => true,
    }
}

/// 成功后下一次：以**上次成功时刻**为基准 + 间隔 + 抖动；时钟往回跳时以 `now` 为基准兜底。
///
/// 用上次成功而不是"现在"：启动时那次可能只是命中缓存（`SILENT_CHECK_CACHE`），
/// 没有真的联网，按"现在 + 6h"会把周期往后推。
///
/// 下限取 `min(间隔, 60 秒)`：正常 6 小时间隔下这是防"时钟回拨 → 立刻重查"的护栏；
/// 演练把间隔缩到 15 秒时不能还被 60 秒压住（否则验证不了周期）。
fn due_after_success(
    now: i64,
    last_success_ms: Option<i64>,
    interval: Duration,
    jitter: Duration,
) -> i64 {
    let interval = interval.as_millis() as i64;
    let jitter = jitter.as_millis() as i64;
    let from_now = now.saturating_add(interval).saturating_add(jitter);
    let from_success = last_success_ms
        .map(|ms| ms.saturating_add(interval).saturating_add(jitter))
        .unwrap_or(from_now);
    let floor = now.saturating_add(interval.min(60_000).max(1_000));
    from_success.min(from_now).max(floor)
}

/// 第 `failures` 次连续失败之后等多久再试；退避用完就按常态间隔（6 小时封顶）。
fn retry_delay(failures: u32) -> Duration {
    let idx = failures.saturating_sub(1) as usize;
    RETRY_BACKOFF.get(idx).copied().unwrap_or_else(check_interval)
}

/// 常态间隔；演练/冒烟时用 `ORBDROP_UPDATE_INTERVAL_SECS` 缩短（只用来验证调度）。
fn check_interval() -> Duration {
    std::env::var("ORBDROP_UPDATE_INTERVAL_SECS")
        .ok()
        .and_then(|v| v.parse::<u64>().ok())
        .filter(|s| *s > 0)
        .map(Duration::from_secs)
        .unwrap_or(CHECK_INTERVAL)
}

/// 静默检查的最短间隔：默认 30 分钟；演练把周期缩到秒级时跟着缩（`interval / 2`），
/// 否则每一拍都会被缓存拦掉，验证不了周期检查。
fn silent_cache_window() -> Duration {
    SILENT_CHECK_CACHE.min(check_interval() / 2)
}

/// 抖动窗口：默认 ±30 分钟（`CHECK_JITTER × 2` 的跨度）。
///
/// 演练时（设了 `ORBDROP_UPDATE_INTERVAL_SECS`）跟着间隔缩到 1/5：15 秒的间隔配 30 分钟
/// 抖动，等于把周期检查推迟到本轮运行之外，没法验证。
fn jitter_span() -> Duration {
    if std::env::var_os("ORBDROP_UPDATE_INTERVAL_SECS").is_some() {
        Duration::from_millis((check_interval().as_millis() as u64 / 5).max(1))
    } else {
        CHECK_JITTER * 2
    }
}

/// `[0, span)` 的随机抖动。
fn random_jitter(span: Duration) -> Duration {
    use rand::Rng;
    Duration::from_millis(rand::thread_rng().gen_range(0..span.as_millis().max(1) as u64))
}

// ---------------------------------------------------------------------------
// 跨进程状态（`update.json`）
// ---------------------------------------------------------------------------

fn persisted_path() -> PathBuf {
    crate::config::data_dir().join("update.json")
}

fn load_persisted() -> Persisted {
    let path = persisted_path();
    match std::fs::read(&path) {
        Ok(bytes) => serde_json::from_slice(&bytes).unwrap_or_else(|e| {
            tracing::warn!(path = %path.display(), "更新状态文件读不动（忽略）：{e}");
            Persisted::default()
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Persisted::default(),
        Err(e) => {
            tracing::warn!(path = %path.display(), "读更新状态文件失败（忽略）：{e}");
            Persisted::default()
        }
    }
}

fn save_persisted(p: &Persisted) {
    let path = persisted_path();
    let bytes = match serde_json::to_vec_pretty(p) {
        Ok(b) => b,
        Err(e) => {
            tracing::warn!("更新状态序列化失败：{e}");
            return;
        }
    };
    if let Some(dir) = path.parent() {
        if let Err(e) = std::fs::create_dir_all(dir) {
            tracing::warn!(dir = %dir.display(), "建不了数据目录：{e}");
            return;
        }
    }
    if let Err(e) = crate::config::write_atomic(&path, &bytes) {
        tracing::warn!(path = %path.display(), "写更新状态文件失败：{e}");
    }
}

fn save_persisted_now() {
    let p = ctl().lock().persisted.clone();
    save_persisted(&p);
}

/// 恢复上次查到的"待安装"版本：只在本机还没装上（远端更高）时返回。
fn restored_available(p: &Persisted, current: &str) -> Option<PersistedAvailable> {
    let a = p.available.as_ref()?;
    let mine = parse_version(current).ok()?;
    let remote = parse_version(&a.version).ok()?;
    (remote > mine).then(|| a.clone())
}

// ---------------------------------------------------------------------------
// 检查
// ---------------------------------------------------------------------------

/// 检查更新。
///
/// `force = true`（用户手动点）忽略静默检查的 30 分钟缓存与失败退避；`false` 是后台调度。
/// 这一位同时决定了快照里的 [`UpdateState::silent`] —— 界面据它区分"后台自己查到的"与
/// "用户要看的结果"，前者要主动弹一格。
/// 已经有检查/下载在跑时**不排队、不报错**，直接把当前快照还回去 —— 界面跟着事件走就行。
pub async fn check(app: AppHandle, force: bool) -> Result<UpdateState, String> {
    let preserve = {
        let c = ctl().lock();
        if in_flight(c.state.phase) {
            return Ok(c.state.clone());
        }
        // 静默检查：上一次结论还在有效期内就直接复用，不联网（判据跨进程）。
        if !force {
            if let Some(ms) = c.persisted.last_success_ms {
                if now_ms().saturating_sub(ms) < silent_cache_window().as_millis() as i64 {
                    return Ok(c.state.clone());
                }
            }
        }
        // 已经查到一版、等用户决定时，后台这趟**不把界面推回 `checking`**：那会让
        // 气泡/角标消失几百毫秒再回来。结果照常更新；失败则原样保留。
        !force && c.state.phase == Phase::Available
    };

    if !preserve {
        put(&app, |c| {
            c.state.phase = Phase::Checking;
            c.state.error = None;
            c.state.received = 0;
            // **整个检查过程都带着这个标记**（后面几步只改阶段，不动它）：它说的是"这份结论
            // 是谁要的"，不是一个阶段的属性 —— 下载/校验那几步下面接着跑，界面靠它认得这一格
            // 是不是自己弹出来的。
            c.state.silent = !force;
        });
    }

    let manifest = match fetch_manifest(&app).await {
        Ok(m) => m,
        Err(e) => return fail_check(&app, preserve, !force, e),
    };

    let current = match parse_version(&snapshot().current) {
        Ok(v) => v,
        Err(e) => return fail_check(&app, preserve, !force, e),
    };
    let remote = match parse_version(&manifest.version) {
        Ok(v) => v,
        Err(e) => return fail_check(&app, preserve, !force, e),
    };

    // 版本比本机高才算有更新。**降级不装**（用户自测时把 0.1.0 退回 0.0.1 那次就是靠这条
    // 兜的）：自更新一旦会降级，线上出事故想回滚就得先发一个"版本号更高但内容是旧的"版本，
    // 那是另一套流程，这里不掺进来。
    if remote <= current {
        let state = {
            let mut c = ctl().lock();
            let now = now_ms();
            c.manifest = None;
            c.artifact = None;
            c.state.phase = Phase::Uptodate;
            c.state.version = Some(manifest.version.clone());
            c.state.notes = manifest.notes.clone();
            c.state.pub_date = manifest.pub_date.clone();
            c.state.size = None;
            c.state.received = 0;
            c.state.error = None;
            c.state.silent = !force;
            c.state.dismissed_version = c.persisted.dismissed_version.clone();
            c.persisted.last_success_ms = Some(now);
            c.persisted.latest_version = Some(manifest.version.clone());
            c.persisted.available = None;
            c.failures = 0;
            c.state.clone()
        };
        let _ = app.emit(ui::UPDATE_STATE, state.clone());
        save_persisted_now();
        return Ok(state);
    }

    let artifact = match manifest.artifact_for_this_platform() {
        Ok(a) => a,
        Err(e) => return fail_check(&app, preserve, !force, e),
    };
    // 安卓的安装门槛在 versionCode 上，不在版本号上：清单的版本号更高但 versionCode 没涨时，
    // 系统会以 `INSTALL_FAILED_VERSION_DOWNGRADE` 拒装（用户看到的是"安装失败"，看不出原因）。
    // 这种事只会发生在发布方漏了递增，所以在**检查阶段**就说清楚。
    #[cfg(mobile)]
    if let Err(e) = ensure_installable(&current, &manifest) {
        return fail_check(&app, preserve, !force, e);
    }

    let state = {
        let mut c = ctl().lock();
        let now = now_ms();
        tracing::info!(version = %manifest.version, "检查到新版本");
        c.state.phase = Phase::Available;
        c.state.version = Some(manifest.version.clone());
        c.state.notes = manifest.notes.clone();
        c.state.pub_date = manifest.pub_date.clone();
        c.state.size = Some(artifact.size);
        c.state.received = 0;
        c.state.error = None;
        c.state.silent = !force;
        // 用户手动查的那次是明确想看结论：之前按过「稍后」也重新给一次。
        if force {
            c.persisted.dismissed_version = None;
        }
        c.state.dismissed_version = c.persisted.dismissed_version.clone();
        c.persisted.last_success_ms = Some(now);
        c.persisted.latest_version = Some(manifest.version.clone());
        c.persisted.available = Some(PersistedAvailable {
            version: manifest.version.clone(),
            notes: manifest.notes.clone(),
            pub_date: manifest.pub_date.clone(),
            size: artifact.size,
        });
        c.artifact = Some(artifact);
        c.manifest = Some(manifest);
        c.failures = 0;
        c.state.clone()
    };
    let _ = app.emit(ui::UPDATE_STATE, state.clone());
    save_persisted_now();
    Ok(state)
}

/// 检查失败：**后台这趟**（`preserve`）不动界面，只把错误交给调用方（调度器据此退避）；
/// 用户自己点的那次才推 `error`（界面要看见原因）。
fn fail_check(
    app: &AppHandle,
    preserve: bool,
    count_failure: bool,
    e: String,
) -> Result<UpdateState, String> {
    {
        let mut c = ctl().lock();
        // 只有后台那趟算"连续失败"：用户手动点的那次失败了，不该替后台排一条退避
        // （他可能只是在没网的时候试了一下，后台该按自己的节奏走）。
        if count_failure {
            c.failures = c.failures.saturating_add(1);
        }
        if preserve {
            tracing::debug!("后台检查失败，保留已查到的版本：{e}");
            return Err(e);
        }
    }
    put(app, |c| {
        c.state.phase = Phase::Error;
        c.state.error = Some(e.clone());
    });
    Err(e)
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
        let res = client.get(url).send().await.map_err(|e| err_chain(&e))?;
        if !res.status().is_success() {
            return Err(format!("HTTP {}", res.status()));
        }
        // `.bytes()` 会先把整个身体读进内存；清单地址虽然固定，回话体积仍要卡死
        // （错配的 CDN / 演练台 / 被改的镜像都不该能把进程撑爆）。
        let body = crate::net::read_body_capped(res, MANIFEST_MAX_BYTES).await?;
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
async fn download(app: &AppHandle, art: &Artifact, dest: &Path) -> Result<(), String> {
    // 清单自身只受 HTTPS 保护，`size` 是远端可控的数字：超过硬上限直接拒，
    // 否则它能把"按 size 截断"变成"按 8GB 截断"。
    if art.size == 0 || art.size > ARTIFACT_MAX_BYTES {
        return Err(format!(
            "清单里的安装包大小不合理（{} 字节，上限 {ARTIFACT_MAX_BYTES}）",
            art.size
        ));
    }
    if let Some(parent) = dest.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("建不了下载目录：{e}"))?;
    }
    let mut last = String::from("清单里没有可用的下载地址");
    let mut first = true;
    for url in ordered_by_pref(app, art.mirrors.clone()) {
        if let Err(e) = ensure_url_ok(&url) {
            last = e;
            continue;
        }
        if !first {
            tracing::info!("换镜像：{url}");
        }
        first = false;
        match download_one(app, &url, dest, art.size).await {
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
///
/// `expected` 是清单声明的体积：超过它立刻中止——镜像不可信，一个"无限长"的响应
/// 能把磁盘写满，也能把校验时的整份读取撑爆。
async fn download_one(
    app: &AppHandle,
    url: &str,
    dest: &Path,
    expected: u64,
) -> Result<(), String> {
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
        Ok(Err(e)) => return Err(err_chain(&e)),
        Ok(Ok(r)) => r,
    };
    if !res.status().is_success() {
        return Err(format!("HTTP {}", res.status()));
    }
    put(app, |c| {
        c.state.phase = Phase::Downloading;
        c.state.mirror = Some(host.clone());
        c.state.received = 0;
        c.state.error = None;
    });
    let received = stream_to_file(res, dest, expected, |received, total| {
        put(app, |c| {
            c.state.received = received;
            c.state.size = total.or(c.state.size);
        });
    })
    .await?;
    put(app, |c| {
        c.state.received = received;
    });
    Ok(())
}

/// 把响应体写进 `dest`，**超过 `expected` 立刻停**；进度按节流后的频率回报。
async fn stream_to_file(
    res: reqwest::Response,
    dest: &Path,
    expected: u64,
    mut progress: impl FnMut(u64, Option<u64>),
) -> Result<u64, String> {
    use futures_util::StreamExt;
    use tokio::io::AsyncWriteExt;

    let total = res.content_length();
    // 声明值只用于尽早拒绝：它可以是假的，所以下面按块写时还要再卡一次。
    if let Some(len) = total {
        if len > expected {
            return Err(format!("响应比清单说的大（{len} > {expected} 字节）"));
        }
    }

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
                    if received == 0 {
                        "还没开始"
                    } else {
                        "中途卡住"
                    }
                ))
            }
            Ok(None) => break,
            Ok(Some(Err(e))) => return Err(format!("传输中断：{}", err_chain(&e))),
            Ok(Some(Ok(c))) => c,
        };

        let next = received.saturating_add(chunk.len() as u64);
        if next > expected {
            return Err(format!("下载超过清单声明的大小（{expected} 字节），中止"));
        }
        received = next;
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
            progress(received, total);
        }
    }

    file.flush().await.map_err(|e| format!("落盘失败：{e}"))?;
    drop(file);
    Ok(received)
}

// ---------------------------------------------------------------------------
// 校验
// ---------------------------------------------------------------------------

/// sha256 + minisign 两道校验。**少了任何一道都不许装**：
/// sha256 防传输损坏，签名防"镜像/账号被拿去换了包"（清单不可信，见模块文档）。
fn verify_file(path: &Path, art: &Artifact, require_sig: bool) -> Result<(), String> {
    // 先对账再读：清单不可信，`art.size` 是**我们自己**准备接受的字节数，不能让镜像
    // 给的文件决定要往内存里搬多少。元数据对不上就直接判死，连一个字节都不读。
    if art.size > ARTIFACT_MAX_BYTES {
        return Err(format!(
            "安装包大得离谱（{} 字节，上限 {ARTIFACT_MAX_BYTES}），拒绝校验",
            art.size
        ));
    }
    let len = std::fs::metadata(path)
        .map_err(|e| format!("读不到下载的文件：{e}"))?
        .len();
    if len != art.size {
        return Err(format!("大小对不上（清单 {} ≠ 实际 {len}）", art.size));
    }
    // 即便元数据对上了，也只读 `art.size + 1` 字节：校验前文件被换大时，多出来的那一个
    // 字节会让下面的长度检查失败，而不会把 `fs::read` 那套"整份进内存"的失败模式带回来。
    use std::io::Read as _;
    let mut bytes = Vec::with_capacity(art.size as usize);
    std::fs::File::open(path)
        .and_then(|f| f.take(art.size + 1).read_to_end(&mut bytes))
        .map_err(|e| format!("读不回下载的文件：{e}"))?;
    if bytes.len() as u64 != art.size {
        return Err(format!(
            "大小对不上（清单 {} ≠ 实际 {}）",
            art.size,
            bytes.len()
        ));
    }
    let got = hex_lower(&Sha256::digest(&bytes));
    if !got.eq_ignore_ascii_case(art.sha256.trim()) {
        return Err(format!(
            "sha256 对不上（清单 {} ≠ 实际 {}）",
            art.sha256.trim(),
            got
        ));
    }
    if require_sig {
        let sig = art.sig.as_deref().ok_or("清单里没有签名，拒绝安装")?;
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
    let key =
        minisign_verify::PublicKey::decode(&pub_text).map_err(|e| format!("公钥格式不对：{e}"))?;
    let sig_text = String::from_utf8(
        engine
            .decode(sig_b64.trim())
            .map_err(|e| format!("签名不是 base64：{e}"))?,
    )
    .map_err(|e| format!("签名不是文本：{e}"))?;
    let sig =
        minisign_verify::Signature::decode(&sig_text).map_err(|e| format!("签名格式不对：{e}"))?;
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
                ctl()
                    .lock()
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
    if let Err(e) = download(&app, &art, &dest).await {
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

/// 下到哪：Windows 落到临时目录（helper 从那里把自己覆盖过去）；macOS 下 `.app` zip 到临时
/// 目录（解开后整体替换）；安卓落到应用私有目录下的 `updates/`（FileProvider 用的是
/// `root-path`，覆盖得到）。
fn download_dest(version: &str) -> PathBuf {
    #[cfg(all(desktop, target_os = "macos"))]
    {
        // macOS 的更新产物是 `.app` 的 zip（保留符号链接与资源分叉，必须用 `ditto -c -k`）。
        std::env::temp_dir()
            .join(format!("orbdrop-update-{version}"))
            .join("orbdrop-macos-universal.app.zip")
    }
    #[cfg(all(desktop, not(target_os = "macos")))]
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
    let (mut first, rest): (Vec<_>, Vec<_>) = urls.into_iter().partition(|u| host_of(u) == pref);
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
    cfg.update_mirror = Some(crate::config::UpdateMirror { host, at: now_ms() });
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

/// 拼出 reqwest 错误的**完整**原因链。
///
/// `reqwest::Error` 的 `Display` 只有一句 `error sending request for url (...)` —— 真正的原因
/// （证书不受信、连接超时、DNS 失败）全挂在 `source()` 链上，不走一遍就看不见。而这行字是用户
/// 排障时**唯一**的线索：真机上出过一次"两条清单地址都拿不到更新信息"，链上写的是
/// `invalid peer certificate: UnknownIssuer`（信任库是空的，见 `Cargo.toml` 里 reqwest 那段），
/// 只看 `Display` 的话根本无从下手。
fn err_chain(e: &(dyn std::error::Error + 'static)) -> String {
    let mut out = e.to_string();
    let mut cur = e.source();
    while let Some(s) = cur {
        let text = s.to_string();
        // reqwest 偶尔把同一句话套两层，重复的不再写第二遍。
        if !out.contains(&text) {
            out.push_str("：");
            out.push_str(&text);
        }
        cur = s.source();
    }
    out
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

/// 记下「稍后」的是哪一版：快照与落盘两份一起改（只有这一个入口，别处不要各写一份）。
fn mark_dismissed(c: &mut Ctl, version: String) {
    c.persisted.dismissed_version = Some(version.clone());
    c.state.dismissed_version = Some(version);
}

/// 用户按掉「稍后」/「知道了」：记下**这一版**（落盘），同版本不再自动弹、角标照旧；
/// 更新的一版仍然会弹。手动「检查更新」会把它清掉，让用户重新看见结论。
#[tauri::command]
pub fn update_dismiss(app: AppHandle, version: Option<String>) -> UpdateState {
    let chosen = version.or_else(|| snapshot().version);
    let Some(version) = chosen else {
        return snapshot();
    };
    put(&app, |c| mark_dismissed(c, version));
    save_persisted_now();
    snapshot()
}

#[tauri::command]
pub async fn update_apply(app: AppHandle) -> Result<(), String> {
    apply(app).await
}
