//! BeyondCompare 集成（F-41 ~ F-43：右键用 BC 比较/写回）。
//!
//! 事实基线全部来自对 BC 5.2 的实测（改之前先看这几条，别照直觉下手）：
//!
//! - BC 只认 `profile:<名字>` 与 `profile:<名字>?<子路径>` 这种位置写法。裸 `http://…`
//!   URL **不会**进 DAV 客户端：它只发 HEAD/GET，然后判定"不能加载基文件夹"。
//!   子路径分隔符实测只有 `?` 有效（`:/`、`/`、`|`、`!`、空格都报"配置文件找不到"）。
//! - profile 存在 `BCProfiles.xml`：**便携版＝BC exe 同目录**（本机就是这种），
//!   常规安装＝用户配置目录（`%APPDATA%\Scooter Software\Beyond Compare 5`）。
//! - 密码字段是 `BCmp|1|<base64(IV16 ‖ AES-256-GCM 密文 ‖ tag16)>`，密钥是根节点
//!   `Encryption="BCmp|1|<32 字节 base64>"`。**明文密码会被 BC 直接丢弃**，所以必须自己加密。
//! - 运行中的 BC **实时跟随**这个文件（后写入的 profile 立刻可用），但**只在启动那一刻
//!   登记**它：启动时文件不存在，那个实例就再也不看它（表现为"弹身份验证"，需重启一次 BC）。
//!   所以预写要趁早（`ensure_profile_file` + `provision_profiles`），而改/删文件都不该被
//!   当成"马上生效"的手段。

use std::path::{Path, PathBuf};
use std::process::Command;

use aes_gcm::aead::consts::U16;
use aes_gcm::aead::{Aead, KeyInit};
use aes_gcm::aes::Aes256;
use aes_gcm::AesGcm;
use base64::Engine as _;
use rand::RngCore;
use serde::Serialize;
use tracing::{info, warn};

use crate::config::BcTarget;
use crate::state::AppState;

/// BC 的 WebDAV profile 必须有用户名才有密码字段；服务端**不校验用户名**（只取 Basic 冒号
/// 后面的 token），所以这个值是我们自己挑的，挑成应用名有两个用处：
///
/// 1. **记录里认得出是谁发起的**：BC 的 profile 列表、以及对端 HTTP 侧的 Basic 账号都会
///    显示 `OrbDrop`（`server::auth` 认到 Basic 时会把发起方写进日志）。
/// 2. **它同时是我们自己的条目标记**：`prune_webdav_profiles` 只删带这个用户名的条目，
///    用户手建的 WebDAV/FTP/S3 profile（哪怕名字也长成 `IP:端口`）一个都不动。
pub const DAV_USER: &str = "OrbDrop";
const PROFILE_FILE: &str = "BCProfiles.xml";
const PROFILE_ELEMENT: &str = "TWebDAVProfile";
/// BC 的加密信封前缀：`BCmp|1|` + base64。
const ENC_PREFIX: &str = "BCmp|1|";

fn b64() -> base64::engine::general_purpose::GeneralPurpose {
    base64::engine::general_purpose::STANDARD
}

// ---------------------------------------------------------------------------
// BC 可执行文件与配置目录
// ---------------------------------------------------------------------------

/// 定位 BC 可执行文件：设置里指定的优先，其次注册表（安装版/便携版脚本都会写），
/// 最后按常见安装路径兜底。
pub fn locate_exe(configured: Option<&Path>) -> Option<PathBuf> {
    if let Some(p) = configured.filter(|p| p.is_file()) {
        return Some(p.to_path_buf());
    }
    registry_candidates()
        .into_iter()
        .chain(common_candidates())
        .find(|p| p.is_file())
}

#[cfg(target_os = "windows")]
fn registry_candidates() -> Vec<PathBuf> {
    use winreg::enums::{HKEY_CURRENT_USER, HKEY_LOCAL_MACHINE};
    use winreg::RegKey;

    let mut out: Vec<PathBuf> = Vec::new();
    let roots = [RegKey::predef(HKEY_CURRENT_USER), RegKey::predef(HKEY_LOCAL_MACHINE)];

    for root in roots.iter() {
        if let Ok(key) = root.open_subkey(r"SOFTWARE\Microsoft\Windows\CurrentVersion\App Paths\BCompare.exe") {
            if let Ok(v) = key.get_value::<String, _>("") {
                if let Some(p) = clean_path(&v) {
                    out.push(p);
                }
            }
        }
    }
    for root in roots.iter() {
        for sub in [
            r"SOFTWARE\Scooter Software\Beyond Compare 5",
            r"SOFTWARE\Scooter Software\Beyond Compare",
        ] {
            if let Ok(key) = root.open_subkey(sub) {
                if let Ok(v) = key.get_value::<String, _>("ExePath") {
                    if let Some(p) = clean_path(&v) {
                        out.push(p);
                    }
                }
            }
        }
    }
    out
}

#[cfg(not(target_os = "windows"))]
fn registry_candidates() -> Vec<PathBuf> {
    Vec::new()
}

/// 注册表里的路径常带引号（`"D:\...\BCompare.exe"`），顺手清掉。
#[cfg(target_os = "windows")]
fn clean_path(raw: &str) -> Option<PathBuf> {
    let trimmed = raw.trim().trim_matches('"').trim();
    (!trimmed.is_empty()).then(|| PathBuf::from(trimmed))
}

fn common_candidates() -> Vec<PathBuf> {
    let mut out = Vec::new();
    #[cfg(target_os = "windows")]
    {
        for var in ["ProgramFiles", "ProgramFiles(x86)"] {
            if let Ok(base) = std::env::var(var) {
                for sub in [r"Beyond Compare 5\BCompare.exe", r"Beyond Compare 4\BCompare.exe"] {
                    out.push(PathBuf::from(&base).join(sub));
                }
            }
        }
    }
    #[cfg(target_os = "macos")]
    {
        out.push(PathBuf::from(
            "/Applications/Beyond Compare.app/Contents/MacOS/bcompare",
        ));
    }
    out
}

/// BC 的配置文件目录：便携版把设置写在 exe 同目录（判断依据就是那几个 `BC*.xml`），
/// 常规安装写在用户配置目录。写 profile 前必须先问它。
pub fn config_dir(exe: &Path) -> PathBuf {
    if let Some(dir) = exe.parent() {
        let portable = ["BCState.xml", "BCPreferences.xml", "BCProfiles.xml"]
            .iter()
            .any(|f| dir.join(f).is_file());
        if portable {
            return dir.to_path_buf();
        }
    }
    dirs::config_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("Scooter Software")
        .join("Beyond Compare 5")
}

// ---------------------------------------------------------------------------
// 密码信封（BCmp|1|，AES-256-GCM）
// ---------------------------------------------------------------------------

fn random_bytes<const N: usize>() -> [u8; N] {
    let mut buf = [0u8; N];
    rand::thread_rng().fill_bytes(&mut buf);
    buf
}

/// 建立空的 `BCProfiles.xml`（只在不存在时写）。
///
/// 为什么必须让它**尽早存在**：BC 只在启动时登记这个配置文件；文件是在它启动之后才出现的
/// （哪怕之后内容再变）它就当没有，之后所有 `profile:` 位置都会被当成未知远端位置——
/// 表现为弹登录框或"文件夹不可用"。所以应用一启动就把骨架立起来，
/// 而不是等到用户点比较的那一刻才第一次创建它。
pub fn ensure_profile_file(dir: &Path) -> Result<PathBuf, String> {
    let path = dir.join(PROFILE_FILE);
    if path.exists() {
        return Ok(path);
    }
    let key = random_bytes::<32>();
    let text = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <!-- Produced by Beyond Compare 5 from Scooter Software -->\n\
         <BCProfiles Encryption=\"{ENC_PREFIX}{}\" Version=\"2\" MinVersion=\"2\">\n</BCProfiles>\n",
        b64().encode(key)
    );
    std::fs::create_dir_all(dir).map_err(|e| format!("创建 BC 配置目录失败: {e}"))?;
    write_atomically(&path, &text)?;
    info!(path = %path.display(), "已建立 BeyondCompare profile 文件骨架");
    Ok(path)
}

/// 预写/刷新所有**已配对**设备的 WebDAV profile。
///
/// 在应用启动、配对成功时各跑一次：这样无论用户何时启动 BC，profile 都已经在文件里了。
/// 点比较时还会再幂等刷新一次（token 重新配对后会变）。
pub fn provision_profiles(state: &AppState) -> usize {
    let exe = match locate_exe(state.config.lock().bc_exe_path.as_deref()) {
        Some(e) => e,
        // 没装 BC 就什么都不用做（不是错误：用户可能根本不用这个功能）
        None => return 0,
    };
    let dir = config_dir(&exe);
    if let Err(e) = ensure_profile_file(&dir) {
        warn!("建立 BC profile 文件失败: {e}");
        return 0;
    }

    let peers: Vec<(String, String, u16)> = state
        .peers
        .lock()
        .iter()
        .filter(|p| p.paired)
        .map(|p| (p.id.clone(), p.ip.clone(), p.port))
        .collect();

    let mut written = 0;
    // 「现在还在册」的 profile 名：已配对且有地址的设备。prune 只删不在这个集合里的
    // 我们自己的条目（设备已解除配对，或地址漂移后留下的旧名字）。
    let mut keep: Vec<String> = Vec::new();
    for (peer_id, ip, port) in peers {
        // 地址簿里可能只有 id（来自凭据库但没有 ip:端口）：写不出 profile 名，跳过。
        if ip.is_empty() || port == 0 {
            continue;
        }
        let host_port = format!("{ip}:{port}");
        let token = match crate::credential::get_peer_token(&peer_id) {
            Ok(Some(t)) => t,
            _ => continue, // 没 token（未真正配对完成）就跳过，不该因此中断
        };
        // 先记进 keep 再写：写失败也不该让这条（仍然有效的）profile 被清掉。
        keep.push(host_port.clone());
        let base_url = format!("http://{host_port}/dav/");
        match upsert_webdav_profile(&dir, &host_port, &base_url, DAV_USER, &token) {
            Ok(_) => written += 1,
            Err(e) => warn!(peer = %host_port, "预写 BC profile 失败: {e}"),
        }
    }
    prune_webdav_profiles(&dir, &keep);
    written
}

fn write_atomically(path: &Path, text: &str) -> Result<(), String> {
    crate::config::write_atomic(path, text.as_bytes())
        .map_err(|e| format!("写入 BCProfiles.xml 失败: {e}"))
}

/// 加密成 BC 认的密码值。密钥就是文件级 `Encryption` 里的 32 字节。
pub fn encrypt_password(key: &[u8], password: &str) -> Option<String> {
    let cipher = AesGcm::<Aes256, U16>::new_from_slice(key).ok()?;
    let iv = random_bytes::<16>();
    let ciphertext = cipher
        .encrypt(aes_gcm::Nonce::<U16>::from_slice(&iv), password.as_bytes())
        .ok()?;
    let payload = [iv.as_slice(), ciphertext.as_slice()].concat();
    Some(format!("{ENC_PREFIX}{}", b64().encode(payload)))
}

/// 解回明文（读回自己写过的值、给 BC 写的值做互通校验都用它）。
pub fn decrypt_password(key: &[u8], value: &str) -> Option<String> {
    let payload = b64().decode(value.strip_prefix(ENC_PREFIX)?).ok()?;
    if payload.len() < 32 {
        return None;
    }
    let (iv, rest) = payload.split_at(16);
    let cipher = AesGcm::<Aes256, U16>::new_from_slice(key).ok()?;
    let plain = cipher.decrypt(aes_gcm::Nonce::<U16>::from_slice(iv), rest).ok()?;
    String::from_utf8(plain).ok()
}

fn decode_key(raw: &str) -> Option<Vec<u8>> {
    let key = b64().decode(raw.strip_prefix(ENC_PREFIX)?).ok()?;
    (key.len() == 32).then_some(key)
}

// ---------------------------------------------------------------------------
// BCProfiles.xml 的读写（合并式：保留别人的 profile 与文件级密钥）
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct ProfileEntry {
    element: String,
    value: String,
    /// 原样保留的元素文本：不认识的 profile 类型（FTP/S3/…）一个字都不该被我们改动。
    raw: String,
}

#[derive(Default, Clone)]
struct ProfileFile {
    version: String,
    min_version: String,
    key: Option<Vec<u8>>,
    entries: Vec<ProfileEntry>,
}

/// 空文件 / 文件不存在时的起点（走重建那条路时才用）。
fn empty_profile_file() -> ProfileFile {
    ProfileFile {
        version: "2".into(),
        min_version: "2".into(),
        ..Default::default()
    }
}

/// 解析现有文件。**解析不了必须报错，不能退化成空表** —— 空表一旦往下走到
/// `write_profile_file`，写出来的就是"只有我们那条"的文件，用户自己的 FTP/S3 profile
/// 全丢。所以这里返回 `Err`，由调用方决定"放弃本次写入"（`upsert` 直接上抛）而不是重建。
fn parse_profile_file(text: &str) -> Result<ProfileFile, String> {
    use quick_xml::events::Event;

    let mut file = empty_profile_file();
    let mut reader = quick_xml::Reader::from_str(text);
    reader.config_mut().trim_text(false);

    let mut depth = 0usize;
    // (元素名, Value 属性, 起始字节偏移)
    let mut pending: Option<(String, String, usize)> = None;

    loop {
        let before = reader.buffer_position() as usize;
        let event = match reader.read_event() {
            Ok(e) => e,
            // BC 自己那份永远是合法的，走到这里说明文件被改坏了/写了一半：报错，让调用方
            // 一个字都不要写。删掉重来等于把用户自己的 profile 一起删掉。
            Err(e) => return Err(format!("{PROFILE_FILE} 解析失败: {e}")),
        };
        match event {
            Event::Start(e) => {
                depth += 1;
                let attrs = element_attrs(&e);
                if depth == 1 {
                    if let Some(v) = attr(&attrs, "Encryption") {
                        file.key = decode_key(&v);
                    }
                    if let Some(v) = attr(&attrs, "Version") {
                        file.version = v;
                    }
                    if let Some(v) = attr(&attrs, "MinVersion") {
                        file.min_version = v;
                    }
                } else if depth == 2 {
                    let name = String::from_utf8_lossy(e.name().as_ref()).to_string();
                    pending = Some((name, attr(&attrs, "Value").unwrap_or_default(), before));
                }
            }
            Event::Empty(e) => {
                if depth == 1 {
                    let after = reader.buffer_position() as usize;
                    file.entries.push(ProfileEntry {
                        element: String::from_utf8_lossy(e.name().as_ref()).to_string(),
                        value: attr(&element_attrs(&e), "Value").unwrap_or_default(),
                        raw: text[before..after].to_string(),
                    });
                }
            }
            Event::End(_) => {
                if depth == 2 {
                    if let Some((element, value, start)) = pending.take() {
                        let after = reader.buffer_position() as usize;
                        file.entries.push(ProfileEntry {
                            element,
                            value,
                            raw: text[start..after].to_string(),
                        });
                    }
                }
                depth = depth.saturating_sub(1);
            }
            Event::Eof => break,
            _ => {}
        }
    }
    Ok(file)
}

fn element_attrs(e: &quick_xml::events::BytesStart<'_>) -> Vec<(String, String)> {
    e.attributes()
        .flatten()
        .map(|a| {
            (
                String::from_utf8_lossy(a.key.as_ref()).to_string(),
                a.unescape_value().map(|v| v.to_string()).unwrap_or_default(),
            )
        })
        .collect()
}

fn attr(attrs: &[(String, String)], name: &str) -> Option<String> {
    attrs.iter().find(|(k, _)| k == name).map(|(_, v)| v.clone())
}

/// 从元素原文里抠一个属性值（用于判断"要不要重写"）。
fn attr_in_raw(raw: &str, name: &str) -> Option<String> {
    let needle = format!("{name} Value=\"");
    let start = raw.find(&needle)? + needle.len();
    let rest = &raw[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// 把解析出来的条目写回 `BCProfiles.xml`：**保留文件级密钥与版本号**、条目原文照抄，
/// 需要时再补一条新的 WebDAV 条目。upsert 与 prune 共用这一处输出，免得两套格式漂移。
fn write_profile_file(
    path: &Path,
    file: &ProfileFile,
    key: &[u8],
    append: Option<&str>,
) -> Result<(), String> {
    let mut out = String::new();
    out.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    out.push_str("<!-- Produced by Beyond Compare 5 from Scooter Software -->\n");
    out.push_str(&format!(
        "<BCProfiles Encryption=\"{ENC_PREFIX}{}\" Version=\"{}\" MinVersion=\"{}\">\n",
        b64().encode(key),
        xml_escape(&file.version),
        xml_escape(&file.min_version),
    ));
    for entry in &file.entries {
        out.push('\t');
        out.push_str(entry.raw.trim());
        out.push('\n');
    }
    if let Some(entry) = append {
        out.push_str(entry);
    }
    out.push_str("</BCProfiles>\n");

    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("创建 BC 配置目录失败: {e}"))?;
    }
    write_atomically(path, &out)
}

/// 合并写入一个 WebDAV profile，返回 `BCProfiles.xml` 的路径。
///
/// 两条不可动摇的规则：**保留文件级密钥**（换密钥会让 BC 已存的其它密码解不开）、
/// **原样保留其它 profile**（用户自己的 FTP/S3/别的 WebDAV 条目）。
pub fn upsert_webdav_profile(
    dir: &Path,
    name: &str,
    base_url: &str,
    username: &str,
    password: &str,
) -> Result<PathBuf, String> {
    let path = dir.join(PROFILE_FILE);
    // 只有"文件不存在"或"确实是空文件"才允许重建（本来就没有别人的东西可丢）。
    // 文件在、但读不出来（非 UTF-8 / 权限 / 损坏）时必须**放弃写入**：以前这里
    // `unwrap_or_default()` 会把读失败当成空文件，随后重写出一份"只有我们那条"的文件，
    // 用户的 FTP/S3 profile 就这么没了。
    let file = match std::fs::read_to_string(&path) {
        Ok(text) if !text.trim().is_empty() => parse_profile_file(&text)?,
        Ok(_) => empty_profile_file(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => empty_profile_file(),
        Err(e) => {
            return Err(format!(
                "读取 {PROFILE_FILE} 失败，已放弃写入以免覆盖用户自己的 profile：{e}"
            ))
        }
    };

    let key = file
        .key
        .clone()
        .unwrap_or_else(|| random_bytes::<32>().to_vec());

    // 已经写对了就别动文件：每次比较都重写会平白让 BC 写 .bak，也可能干扰正在运行的实例。
    if let Some(entry) = file
        .entries
        .iter()
        .find(|e| e.element == PROFILE_ELEMENT && e.value == name)
    {
        let same_base = attr_in_raw(&entry.raw, "BaseURL").as_deref() == Some(base_url);
        let same_user = attr_in_raw(&entry.raw, "Username").as_deref() == Some(username);
        let same_pw = attr_in_raw(&entry.raw, "Password")
            .and_then(|v| decrypt_password(&key, &v))
            .as_deref()
            == Some(password);
        if same_base && same_user && same_pw {
            return Ok(path);
        }
    }

    let encrypted = encrypt_password(&key, password).ok_or_else(|| "密码加密失败".to_string())?;

    let mut kept = file.clone();
    kept.entries
        .retain(|e| !(e.element == PROFILE_ELEMENT && e.value == name));
    let appended = format!(
        "\t<{PROFILE_ELEMENT} Value=\"{}\">\n\
         \t\t<BaseURL Value=\"{}\"/>\n\
         \t\t<Password Value=\"{}\"/>\n\
         \t\t<Username Value=\"{}\"/>\n\
         \t</{PROFILE_ELEMENT}>\n",
        xml_escape(name),
        xml_escape(base_url),
        xml_escape(&encrypted),
        xml_escape(username),
    );

    write_profile_file(&path, &kept, &key, Some(&appended))?;
    info!(path = %path.display(), name, username, "已写入 BeyondCompare WebDAV profile");
    Ok(path)
}

/// 删掉**我们自己写下、但已经不可能再用**的 profile（设备解除配对后 / 地址变了）。
///
/// 判定"是我们的"只有一个依据：`Username == DAV_USER`。名字长得像 `IP:端口` 但用户名不是
/// 我们的条目 —— 也就是用户手建的 WebDAV/FTP/S3 profile —— **一个都不动**。
/// 名字不在 `keep` 里 = 这条 profile 指向的关系已经不存在（token 已作废，或对端地址漂移，
/// 旧名字永远连不上），留着只会让 BC 拿着死凭据去撞。
///
/// 没有可删的条目时**一个字都不写**：BC 可能正在跑，这个文件只在真要改内容时才碰。
/// 文件读不出来或解析不了时同样一个字都不写（重写会把用户自己的 profile 一并抹掉）。
///
/// 返回删掉的条目数。
pub fn prune_webdav_profiles(dir: &Path, keep: &[String]) -> usize {
    let path = dir.join(PROFILE_FILE);
    // 读不出来（不存在/空/非 UTF-8/权限）：没东西可删，碰都别碰。
    let existing = match std::fs::read_to_string(&path) {
        Ok(text) if !text.trim().is_empty() => text,
        _ => return 0,
    };
    // 解析不了同样一个字都不写（重写会把用户自己的 profile 一并抹掉）。
    let mut file = match parse_profile_file(&existing) {
        Ok(f) => f,
        Err(e) => {
            warn!(path = %path.display(), "清理 BC profile 跳过：{e}");
            return 0;
        }
    };
    let before = file.entries.len();
    file.entries.retain(|e| {
        let ours = e.element == PROFILE_ELEMENT && attr_in_raw(&e.raw, "Username").as_deref() == Some(DAV_USER);
        !ours || keep.iter().any(|k| k == &e.value)
    });
    let removed = before - file.entries.len();
    if removed == 0 {
        return 0;
    }
    // 文件级密钥照旧（`parse` 出来没有才生成）。
    let key = file.key.clone().unwrap_or_else(|| random_bytes::<32>().to_vec());
    match write_profile_file(&path, &file, &key, None) {
        Ok(()) => {
            info!(path = %path.display(), removed, "已清理失效的 BeyondCompare WebDAV profile");
            removed
        }
        Err(e) => {
            warn!(path = %path.display(), "清理 BC profile 失败: {e}");
            0
        }
    }
}

// ---------------------------------------------------------------------------
// 比较位 → BC 命令行参数
// ---------------------------------------------------------------------------

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SideDto {
    pub target: BcTarget,
    pub label: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BcStatusDto {
    pub exe_path: Option<String>,
    pub left: Option<SideDto>,
    pub right: Option<SideDto>,
}

pub struct ResolvedSide {
    /// 直接喂给 BC 的路径或 profile spec。
    pub arg: String,
    pub label: String,
}

pub fn status(state: &AppState) -> BcStatusDto {
    let (exe, left, right) = {
        let cfg = state.config.lock();
        (
            locate_exe(cfg.bc_exe_path.as_deref()),
            cfg.bc_left.clone(),
            cfg.bc_right.clone(),
        )
    };
    BcStatusDto {
        exe_path: exe.map(|p| p.to_string_lossy().to_string()),
        left: left.map(|t| SideDto {
            label: label_of(state, &t),
            target: t,
        }),
        right: right.map(|t| SideDto {
            label: label_of(state, &t),
            target: t,
        }),
    }
}

/// 显示名：优先用界面带下来的（对端共享项的名字本机不知道），其次自己凑。
pub fn label_of(state: &AppState, target: &BcTarget) -> String {
    match target {
        BcTarget::Share {
            share_id,
            path,
            label,
        } => label.clone().unwrap_or_else(|| {
            let cfg = state.config.lock();
            match cfg.shares.iter().find(|s| s.id == *share_id) {
                Some(s) if path.is_empty() => s.name.clone(),
                Some(s) => format!("{}/{}", s.name, path),
                None => "（已移除的共享项）".to_string(),
            }
        }),
        BcTarget::Peer {
            peer_id,
            share_id,
            path,
            label,
        } => label.clone().unwrap_or_else(|| {
            let peer = state
                .get_peer(peer_id)
                .map(|p| p.name)
                .unwrap_or_else(|| "（已离线设备）".to_string());
            if path.is_empty() {
                format!("{peer} · {share_id}")
            } else {
                format!("{peer} · {share_id}/{path}")
            }
        }),
    }
}

/// BC 的 profile 位置写法。子路径分隔符只能是 `?`（实测）。
pub fn profile_spec(profile_name: &str, rel: &str) -> String {
    if rel.is_empty() {
        format!("profile:{profile_name}")
    } else {
        format!("profile:{profile_name}?{rel}")
    }
}

/// 共享项内的相对路径 → 本机绝对路径（顺带挡掉 `..` 逃逸）。
fn join_relative(root: &Path, rel: &str) -> PathBuf {
    let mut out = root.to_path_buf();
    for part in rel.split(['/', '\\']) {
        if part.is_empty() || part == "." || part == ".." {
            continue;
        }
        out.push(part);
    }
    out
}

/// 把比较位解析成 BC 参数：本机共享项 → 本地绝对路径；对端共享项 → `profile:<ip:port>?<shareId>/<路径>`。
///
/// 对端这一路会顺手写好 WebDAV profile——每台设备一条，名字就是 `ip:port`（BC 自己的命名习惯），
/// BaseURL 指向对端 DAV 根，密码是该设备的 token。
pub fn resolve_side(state: &AppState, exe: &Path, target: &BcTarget) -> Result<ResolvedSide, String> {
    let label = label_of(state, target);
    match target {
        BcTarget::Share {
            share_id, path, ..
        } => {
            let share_path = state
                .config
                .lock()
                .shares
                .iter()
                .find(|s| s.id == *share_id)
                .map(|s| s.path.clone())
                .ok_or_else(|| "共享项不存在（可能已被移除）".to_string())?;
            let abs = join_relative(&share_path, path);
            if !abs.exists() {
                return Err(format!("路径不存在：{}", abs.display()));
            }
            Ok(ResolvedSide {
                arg: abs.to_string_lossy().to_string(),
                label,
            })
        }
        BcTarget::Peer {
            peer_id,
            share_id,
            path,
            ..
        } => {
            let peer = state
                .get_peer(peer_id)
                .ok_or_else(|| "设备不在列表中".to_string())?;
            if !state.ws.has_connection(peer_id) {
                return Err(format!(
                    "「{}」不是长连接状态：WebDAV 只在长连接期间对它开放",
                    peer.name
                ));
            }
            let token = crate::credential::get_peer_token(peer_id)
                .map_err(|e| e.to_string())?
                .ok_or_else(|| "未与对方配对，拿不到 token".to_string())?;

            let host_port = format!("{}:{}", peer.ip, peer.port);
            let base_url = format!("http://{host_port}/dav/");
            upsert_webdav_profile(&config_dir(exe), &host_port, &base_url, DAV_USER, &token)?;

            let rel = if path.is_empty() {
                share_id.clone()
            } else {
                format!("{share_id}/{path}")
            };
            Ok(ResolvedSide {
                arg: profile_spec(&host_port, &rel),
                label,
            })
        }
    }
}

/// 拉起 BC：左、右两个参数（本地路径或 profile spec）。BC 自己保证单实例。
pub fn launch_compare(exe: &Path, left: &str, right: &str) -> Result<(), String> {
    let mut cmd = Command::new(exe);
    cmd.arg(left).arg(right);
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        // CREATE_NO_WINDOW：BC 是 GUI 程序，别在我们这边闪一个控制台窗口。
        cmd.creation_flags(0x0800_0000);
    }
    cmd.spawn()
        .map(|_| ())
        .map_err(|e| format!("启动 {} 失败：{e}", exe.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// BC 自己写出来的密钥/密文（本机 GUI 建 profile 时落盘的原文）→ 我们必须能解开。
    /// 这条是"我们的加密算法与 BC 互通"的证据；算法一旦写错，它立刻红。
    #[test]
    fn decrypts_password_written_by_bc() {
        let key = b64()
            .decode("mB6zbBLIkslSsAw1FZyKRJah/3qhLZCcQjyP/RE0Gxg=")
            .unwrap();
        let value = "BCmp|1|qxl8HwrDkt3VScUcvud3vEp6fFri092tKY6jC8UlU9LHLnkI/Y8=";
        assert_eq!(decrypt_password(&key, value).as_deref(), Some("tok123"));
    }

    #[test]
    fn encrypt_round_trips() {
        let key = random_bytes::<32>();
        let value = encrypt_password(&key, "头文字D-秘密").unwrap();
        assert!(value.starts_with(ENC_PREFIX));
        assert_eq!(decrypt_password(&key, &value).as_deref(), Some("头文字D-秘密"));
    }

    /// 写入必须**合并**：别的 profile 原样保留、文件级密钥不换（换掉会让 BC 已存的密码解不开）。
    #[test]
    fn upsert_preserves_other_profiles_and_file_key() {
        let dir = std::env::temp_dir().join(format!("orbdrop_bc_test_{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let other = "\t<TFtpProfile Value=\"SomeFtp\">\n\t\t<Host Value=\"example.com\"/>\n\t</TFtpProfile>";
        let original_key = random_bytes::<32>();
        fs::write(
            dir.join(PROFILE_FILE),
            format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<BCProfiles Encryption=\"BCmp|1|{}\" Version=\"2\" MinVersion=\"2\">\n{other}\n</BCProfiles>\n",
                b64().encode(original_key)
            ),
        )
        .unwrap();

        upsert_webdav_profile(&dir, "192.168.1.9:3890", "http://192.168.1.9:3890/dav/", "x", "tok-9")
            .unwrap();

        let written = fs::read_to_string(dir.join(PROFILE_FILE)).unwrap();
        assert!(written.contains(other.trim()), "别人的 profile 必须原样保留:\n{written}");
        assert!(written.contains(&format!("Encryption=\"{ENC_PREFIX}{}\"", b64().encode(original_key))));

        let parsed = parse_profile_file(&written).unwrap();
        assert_eq!(parsed.key.as_deref(), Some(original_key.as_slice()));
        let entry = parsed
            .entries
            .iter()
            .find(|e| e.value == "192.168.1.9:3890")
            .expect("新 profile 应该写进去");
        assert_eq!(
            attr_in_raw(&entry.raw, "BaseURL").as_deref(),
            Some("http://192.168.1.9:3890/dav/")
        );
        let pw = attr_in_raw(&entry.raw, "Password").unwrap();
        assert_eq!(decrypt_password(&original_key, &pw).as_deref(), Some("tok-9"));
        assert!(entry.raw.contains(&format!("<{PROFILE_ELEMENT}")));

        let _ = fs::remove_dir_all(dir);
    }

    /// 同样的内容重复写不该改动文件（每次比较都重写会平白触发 BC 写 .bak）。
    #[test]
    fn upsert_is_idempotent_for_same_values() {
        let dir = std::env::temp_dir().join(format!("orbdrop_bc_test_{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let args = ("10.0.0.5:3890", "http://10.0.0.5:3890/dav/", "x", "tok");
        upsert_webdav_profile(&dir, args.0, args.1, args.2, args.3).unwrap();
        let first = fs::read_to_string(dir.join(PROFILE_FILE)).unwrap();
        upsert_webdav_profile(&dir, args.0, args.1, args.2, args.3).unwrap();
        assert_eq!(first, fs::read_to_string(dir.join(PROFILE_FILE)).unwrap());

        // token 变了（重新配对）→ 必须更新
        upsert_webdav_profile(&dir, args.0, args.1, args.2, "tok2").unwrap();
        let updated = fs::read_to_string(dir.join(PROFILE_FILE)).unwrap();
        assert_ne!(first, updated);
        let entry = parse_profile_file(&updated)
            .unwrap()
            .entries
            .into_iter()
            .find(|e| e.value == args.0)
            .unwrap();
        let key = parse_profile_file(&updated).unwrap().key.unwrap();
        assert_eq!(
            decrypt_password(&key, &attr_in_raw(&entry.raw, "Password").unwrap()).as_deref(),
            Some("tok2")
        );
        let _ = fs::remove_dir_all(dir);
    }

    /// 清理只认「我们写下的」条目：`Username == DAV_USER` 且名字已不在册。用户手建的
    /// profile（名字哪怕也长成 `IP:端口`）必须原样留着，文件级密钥也不能换。
    #[test]
    fn prune_removes_only_our_orphans() {
        let dir = std::env::temp_dir().join(format!("orbdrop_bc_prune_{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let ftp = "\t<TFtpProfile Value=\"SomeFtp\">\n\t\t<Host Value=\"example.com\"/>\n\t</TFtpProfile>";

        // 我们的两条：一条仍在册，一条是设备解除配对后留下的孤儿
        upsert_webdav_profile(&dir, "10.0.0.1:3890", "http://10.0.0.1:3890/dav/", DAV_USER, "tok-a").unwrap();
        upsert_webdav_profile(&dir, "10.0.0.2:3890", "http://10.0.0.2:3890/dav/", DAV_USER, "tok-b").unwrap();
        // 用户自己手建的：名字同样像 IP:端口，但用户名不是我们的
        upsert_webdav_profile(&dir, "10.0.0.3:1080", "http://10.0.0.3:1080/dav/", "bob", "pw").unwrap();
        // 再加一条别的类型（FTP），证明整个文件里的其它 profile 也不受影响
        let path = dir.join(PROFILE_FILE);
        let text = fs::read_to_string(&path).unwrap();
        let with_ftp = text.replace("</BCProfiles>", &format!("{ftp}\n</BCProfiles>"));
        fs::write(&path, &with_ftp).unwrap();
        let key = parse_profile_file(&with_ftp).unwrap().key.unwrap();

        let removed = prune_webdav_profiles(&dir, &["10.0.0.1:3890".to_string()]);
        assert_eq!(removed, 1, "只该删掉已解除配对的那一条");

        let written = fs::read_to_string(&path).unwrap();
        let parsed = parse_profile_file(&written).unwrap();
        let by_name = |n: &str| parsed.entries.iter().find(|e| e.value == n);
        assert!(by_name("10.0.0.1:3890").is_some(), "在册设备必须保留:\n{written}");
        assert!(by_name("10.0.0.2:3890").is_none(), "孤儿条目该被清掉:\n{written}");
        let users = by_name("10.0.0.3:1080").expect("用户手建的条目一个字都不该动");
        assert_eq!(attr_in_raw(&users.raw, "Username").as_deref(), Some("bob"));
        assert!(written.contains(ftp.trim()), "其它 profile 类型原样保留:\n{written}");
        assert_eq!(parsed.key.as_deref(), Some(key.as_slice()), "文件级密钥不能换");

        // 没有孤儿条目时一个字都不写（BC 可能在跑，别平白触发它写 .bak）
        let stable = fs::read_to_string(&path).unwrap();
        assert_eq!(
            prune_webdav_profiles(&dir, &["10.0.0.1:3890".into(), "10.0.0.3:1080".into()]),
            0
        );
        assert_eq!(stable, fs::read_to_string(&path).unwrap(), "无可删条目时不该改动文件");

        let _ = fs::remove_dir_all(dir);
    }

    /// 老版本写下的 `Username="x"` 条目：下一次预写会把它改成 `DAV_USER`（名字相同即同一条），
    /// 这样"我们的条目标记"对老用户也成立，之后解除配对时它才清得掉。
    #[test]
    fn legacy_x_username_entries_are_migrated_to_the_app_user() {
        let dir = std::env::temp_dir().join(format!("orbdrop_bc_legacy_{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        upsert_webdav_profile(&dir, "10.0.0.9:3890", "http://10.0.0.9:3890/dav/", "x", "tok-old").unwrap();

        upsert_webdav_profile(&dir, "10.0.0.9:3890", "http://10.0.0.9:3890/dav/", DAV_USER, "tok-old").unwrap();

        let parsed = parse_profile_file(&fs::read_to_string(dir.join(PROFILE_FILE)).unwrap()).unwrap();
        let ours: Vec<&ProfileEntry> = parsed
            .entries
            .iter()
            .filter(|e| e.value == "10.0.0.9:3890")
            .collect();
        assert_eq!(ours.len(), 1, "同一个名字只该有一条");
        assert_eq!(attr_in_raw(&ours[0].raw, "Username").as_deref(), Some(DAV_USER));

        let _ = fs::remove_dir_all(dir);
    }

    /// 用户手工删掉 profile 之后，下一次比较必须把它补回来：
    /// 条目被删但文件还在 → 沿用文件原来的密钥（别人的密码才解得开）；整份文件都没了 → 重建。
    #[test]
    fn a_deleted_profile_is_rewritten_on_the_next_comparison() {
        let dir = std::env::temp_dir().join(format!("orbdrop_bc_del_{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(PROFILE_FILE);
        let original_key = random_bytes::<32>();
        let other = "\t<TFtpProfile Value=\"MyFtp\">\n\t\t<Host Value=\"example.com\"/>\n\t</TFtpProfile>";
        // 用户把我们那条 WebDAV 条目删掉了，只留下自己的；文件级密钥还在
        fs::write(
            &path,
            format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<BCProfiles Encryption=\"BCmp|1|{}\" Version=\"2\" MinVersion=\"2\">\n{other}\n</BCProfiles>\n",
                b64().encode(original_key)
            ),
        )
        .unwrap();

        upsert_webdav_profile(&dir, "10.2.0.1:3890", "http://10.2.0.1:3890/dav/", DAV_USER, "tok-1").unwrap();
        let written = fs::read_to_string(&path).unwrap();
        let parsed = parse_profile_file(&written).unwrap();
        assert_eq!(
            parsed.entries.iter().filter(|e| e.value == "10.2.0.1:3890").count(),
            1,
            "删掉的条目该被补回来，而且只有一条:\n{written}"
        );
        assert!(written.contains(other.trim()), "别的 profile 不受影响:\n{written}");
        assert_eq!(parsed.key.as_deref(), Some(original_key.as_slice()), "密钥不能换成新的");

        // 整份文件被删：下一次比较要把文件重建出来（新密钥），不能报错
        fs::remove_file(&path).unwrap();
        upsert_webdav_profile(&dir, "10.2.0.1:3890", "http://10.2.0.1:3890/dav/", DAV_USER, "tok-1").unwrap();
        let recreated = parse_profile_file(&fs::read_to_string(&path).unwrap()).unwrap();
        assert!(recreated.key.is_some(), "重建的文件也要有文件级密钥");
        assert_eq!(
            recreated.entries.iter().filter(|e| e.value == "10.2.0.1:3890").count(),
            1
        );

        let _ = fs::remove_dir_all(dir);
    }

    /// 同一台设备比多少次都只留一条 profile：名字是 `IP:端口`，共享项/子路径只出现在
    /// `profile:<名字>?<子路径>` 里；不同设备各一条。内容没变化时连文件都不写。
    #[test]
    fn repeated_comparisons_do_not_accumulate_profiles() {
        let dir = std::env::temp_dir().join(format!("orbdrop_bc_multi_{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        for _ in 0..5 {
            upsert_webdav_profile(&dir, "10.3.0.1:3890", "http://10.3.0.1:3890/dav/", DAV_USER, "tok-a").unwrap();
        }
        upsert_webdav_profile(&dir, "10.3.0.2:3890", "http://10.3.0.2:3890/dav/", DAV_USER, "tok-b").unwrap();

        let path = dir.join(PROFILE_FILE);
        let written = fs::read_to_string(&path).unwrap();
        let parsed = parse_profile_file(&written).unwrap();
        let ours: Vec<&str> = parsed
            .entries
            .iter()
            .filter(|e| e.element == PROFILE_ELEMENT)
            .map(|e| e.value.as_str())
            .collect();
        assert_eq!(ours.len(), 2, "两台设备 = 两条，比五次也不例外：{ours:?}");

        // 同一台设备的多个共享项/子路径共用同一条 profile
        assert_eq!(profile_spec("10.3.0.1:3890", "share-a/sub"), "profile:10.3.0.1:3890?share-a/sub");
        assert_eq!(profile_spec("10.3.0.1:3890", "share-b"), "profile:10.3.0.1:3890?share-b");

        // 再比一次：内容已经对了，文件一个字节都不该动
        upsert_webdav_profile(&dir, "10.3.0.1:3890", "http://10.3.0.1:3890/dav/", DAV_USER, "tok-a").unwrap();
        assert_eq!(written, fs::read_to_string(&path).unwrap());

        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn profile_spec_separates_subpath_with_question_mark() {
        assert_eq!(profile_spec("192.168.1.9:3890", ""), "profile:192.168.1.9:3890");
        assert_eq!(
            profile_spec("192.168.1.9:3890", "share-1/src/lib"),
            "profile:192.168.1.9:3890?share-1/src/lib"
        );
    }

    #[test]
    fn portable_bc_config_lives_next_to_the_exe() {
        let dir = std::env::temp_dir().join(format!("orbdrop_bc_dir_{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("BCompare.exe");
        fs::write(&exe, b"stub").unwrap();
        // 没有 BC*.xml → 用户配置目录
        assert!(config_dir(&exe).ends_with("Beyond Compare 5"));

        fs::write(dir.join("BCState.xml"), b"<BCState/>").unwrap();
        assert_eq!(config_dir(&exe), dir);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn configured_exe_wins_over_detection() {
        let dir = std::env::temp_dir().join(format!("orbdrop_bc_exe_{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let exe = dir.join("BCompare.exe");
        fs::write(&exe, b"stub").unwrap();
        assert_eq!(locate_exe(Some(&exe)), Some(exe.clone()));
        // 配的路径不存在 → 不能拿它当结果（退回自动探测）
        assert_ne!(locate_exe(Some(&dir.join("nope.exe"))), Some(dir.join("nope.exe")));
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn join_relative_cannot_escape_the_share_root() {
        let root = PathBuf::from("/srv/share");
        assert_eq!(join_relative(&root, "a/b.txt"), PathBuf::from("/srv/share/a/b.txt"));
        assert_eq!(join_relative(&root, "../../etc/passwd"), PathBuf::from("/srv/share/etc/passwd"));
    }

    /// 便携版布局的临时环境：`<dir>\BCompare.exe` + `<dir>\BCState.xml` → 配置目录就是 `<dir>`。
    fn portable_bc_dir() -> PathBuf {
        let dir = std::env::temp_dir().join(format!("orbdrop_bc_prov_{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("BCompare.exe"), b"stub").unwrap();
        fs::write(dir.join("BCState.xml"), b"<BCState/>").unwrap();
        dir
    }

    /// 启动时就要把 profile 文件立起来：BC 只在启动时登记这个文件，
    /// 文件晚出现（由我们点比较时才创建）它就不认——这正是实机"弹身份验证"的成因。
    #[test]
    fn provision_creates_skeleton_and_is_idempotent() {
        let dir = portable_bc_dir();
        let mut config = crate::config::Config::default();
        config.bc_exe_path = Some(dir.join("BCompare.exe"));
        let config_path = dir.join("orbdrop.toml");
        let state = crate::state::AppState::new(config, config_path);

        assert_eq!(provision_profiles(&state), 0, "没有已配对设备时不该写出任何 profile");
        let file = dir.join(PROFILE_FILE);
        assert!(file.is_file(), "没有设备也要把骨架文件立起来");
        let skeleton = fs::read_to_string(&file).unwrap();
        assert!(skeleton.contains("<BCProfiles"), "骨架必须是合法文件: {skeleton}");

        assert_eq!(provision_profiles(&state), 0);
        assert_eq!(
            skeleton,
            fs::read_to_string(&file).unwrap(),
            "重复预写不该改动已有文件"
        );

        // 用户自己的 profile 不会被预写动到
        let with_user = format!(
            "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<BCProfiles Encryption=\"{ENC_PREFIX}{}\" Version=\"2\" MinVersion=\"2\">\n\t<TFtpProfile Value=\"MyFtp\"/>\n</BCProfiles>\n",
            b64().encode(random_bytes::<32>())
        );
        fs::write(&file, &with_user).unwrap();
        provision_profiles(&state);
        assert_eq!(fs::read_to_string(&file).unwrap(), with_user);

        let _ = fs::remove_dir_all(dir);
    }

    /// 文件在、但**读不出来或解析不了**时必须放弃写入：这是唯一真能删掉用户自己 profile 的
    /// 路径（旧实现把读失败/解析失败当成空文件，重写出一份"只有我们那条"的文件）。
    /// 文件真的不存在时仍要重建（否则首次比较直接失败）。
    #[test]
    fn unreadable_or_corrupt_file_is_left_untouched() {
        let dir = std::env::temp_dir().join(format!("orbdrop_bc_bad_{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(PROFILE_FILE);
        let args = ("10.0.0.1:3890", "http://10.0.0.1:3890/dav/", DAV_USER, "tok");

        // ① 非 UTF-8（UTF-16 落盘的 BCProfiles.xml，带 BOM）：`read_to_string` 失败 → 放弃写入
        let mut utf16: Vec<u8> = vec![0xFF, 0xFE];
        utf16.extend(
            "<?xml version=\"1.0\"?><BCProfiles><TFtpProfile Value=\"KeepMe\"/></BCProfiles>"
                .encode_utf16()
                .flat_map(|u| u.to_le_bytes()),
        );
        fs::write(&path, &utf16).unwrap();
        assert!(fs::read_to_string(&path).is_err(), "前提：这份文件不是合法 UTF-8");
        assert!(
            upsert_webdav_profile(&dir, args.0, args.1, args.2, args.3).is_err(),
            "读不出来的文件必须上抛错误，而不是重建"
        );
        assert_eq!(fs::read(&path).unwrap(), utf16, "读不出来时一个字节都不能改");
        assert_eq!(prune_webdav_profiles(&dir, &[]), 0);
        assert_eq!(fs::read(&path).unwrap(), utf16);

        // ② 半截/串标签的 XML：解析失败 → 同样放弃写入
        let broken = "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
                      <BCProfiles Encryption=\"BCmp|1|AAAA\" Version=\"2\">\n\
                      \t<TFtpProfile Value=\"KeepMe\">\n\t</TWebDAVProfile>\n</BCProfiles>\n";
        fs::write(&path, broken).unwrap();
        assert!(
            upsert_webdav_profile(&dir, args.0, args.1, args.2, args.3).is_err(),
            "解析失败必须上抛错误，而不是重建"
        );
        assert_eq!(fs::read_to_string(&path).unwrap(), broken, "解析失败时一个字节都不能改");
        assert_eq!(prune_webdav_profiles(&dir, &[]), 0);
        assert_eq!(fs::read_to_string(&path).unwrap(), broken);

        // ③ 文件真的不存在：仍要重建（这条不能因为上面的加固而退化）
        fs::remove_file(&path).unwrap();
        upsert_webdav_profile(&dir, args.0, args.1, args.2, args.3).unwrap();
        assert!(fs::read_to_string(&path).unwrap().contains(args.0));

        let _ = fs::remove_dir_all(dir);
    }
}