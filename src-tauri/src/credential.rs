use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use base64::Engine as _;
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

/// 凭据文件（vault）的名字：落在 `config::data_dir()` 里，也就是 `config.toml` 旁边
/// （Windows `%LOCALAPPDATA%\OrbDrop\`，macOS `~/Library/Application Support/OrbDrop/`）。
const VAULT_FILE: &str = "credentials.dat";
/// 内层明文的头：| 魔数 | 空格 | 十进制版本 |。魔数固定 4 字节，版本**按十进制文本读**
/// （不按宽度），所以 `ODBF 2`、`ODBF 100` 都解得了；比 `FORMAT_VERSION` 更新的版本
/// 明确拒绝，不瞎猜。
///
/// 魔数与署名只存在于**内层明文**（混淆前的那份内容）里：文件本身看不到它们 ——
/// 这正是"双击打开看不出这是 OrbDrop 写的、也看不出正文从哪一行开始"的那部分。
const INNER_MAGIC: &str = "ODBF";
/// 内层明文里的尾署名。`#` 开头的行都是注释，解析时跳过 —— base64 字母表里没有 `#`，
/// 所以注释行不可能和正文撞。
const INNER_FOOTER: &str = "# 177plus";
/// 本版本写出的格式版本。版本号是**三种格式共用的一个计数器**：
/// - `1` = 明文遮挡（`ODBF <版本>` 头 + base64 正文 + 署名），只读兼容；
/// - `2` = 整块混淆（内层明文整体异或后 base64），正文是裸的 `{"<设备id>":"<token>"}`；
/// - `3` = 整块混淆，正文是 `{"<设备id>":{"token":…,"fingerprint":…}}` —— **写一律是这个**。
///   指纹与 token 同一条记录：它们是同一份关系的两面（换机器 = 换指纹 = 必须重新配对），
///   分两处存只会多出一种"token 在、指纹不在"的中间态。
const FORMAT_VERSION: u32 = 3;
/// 外层混淆的密钥。**编译期常量 = 没有秘密**：想解开它，读一眼这段源码就够。
/// 它只干一件事 —— 让双击打开文件的人看到一整块"读不出是什么"的文本，而不是
/// "头部 + 一眼可解的 base64 + 署名"。**不是加密，别当保护用**。
const SCRAMBLE_KEY: &[u8] = b"OrbDrop/177plus/credentials.dat";
/// base64 正文的换行宽度（纯为让人打开时看得下去）。
const LINE_WIDTH: usize = 76;

/// 一台对端的配对凭据。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerCredential {
    /// 对端认我们用的那枚 token（本机拿它去认证自己）。
    pub token: String,
    /// 对端证书的指纹（DER 的 SHA-256，小写 hex，见 `net::fingerprint_of`）。
    ///
    /// 空串 = 还没有钉住：老版本（格式 1/2）写下的记录没有这一项，用户升级上来时
    /// 就是这种状态；下一次成功连上对端时按 TOFU 补上（见 `net::PinnedVerifier`）。
    /// 空串**不是**"不校验"，而是"这次连上什么就认什么"——一次性。
    #[serde(default)]
    pub fingerprint: String,
}

/// 整份映射在内存和 vault 文件之间是"读-改-写"，同进程内必须串行：两个配对流程
/// （或一个配对撞上一次解除配对）同时收尾时，各自基于同一份旧映射写回，会**静默丢掉**
/// 对方那条 token。写路径自己用不取锁的 `read_unlocked`（parking_lot 的锁不可重入）。
static STORE_LOCK: Mutex<()> = Mutex::new(());

fn b64() -> base64::engine::general_purpose::GeneralPurpose {
    base64::engine::general_purpose::STANDARD
}

/// vault 文件的位置（跟随 `ORBDROP_DATA_DIR`，测试能指到临时目录）。
pub fn vault_path() -> PathBuf {
    // 单元测试（lib 内）一律走临时文件：测试跑起来的机器上可能有用户**真实**的凭据，
    // 一个测试碰它就可能把人家配对关系弄没。集成测试（tests/）各自把
    // `ORBDROP_DATA_DIR` 指到临时目录，效果一样。
    //
    // 按**线程**分文件（不是一个进程一个）：测试是并行跑的，而下面有几个用例会真的
    // 写/清这份文件 —— 共用一个路径就成了互相清对方的凭据（表现为随机失败）。
    // 库内测试都是同步的，所以"一个用例一个线程"成立。
    #[cfg(test)]
    {
        return std::env::temp_dir().join(format!(
            "orbdrop_vault_unit_{}_{:?}.dat",
            std::process::id(),
            std::thread::current().id()
        ));
    }
    #[cfg(not(test))]
    {
        crate::config::data_dir().join(VAULT_FILE)
    }
}

// ---------------------------------------------------------------------------
// 文件格式（**遮挡，不是加密**）
// ---------------------------------------------------------------------------

/// 渲染出 vault 文件的内容：**一整块读不出结构的文本**。
///
/// 内层明文（混淆前）是：
///
/// ```text
/// ODBF 3
/// {"<设备id>":{"token":"…","fingerprint":"…"},…}
/// # 177plus
/// ```
///
/// 这份内层明文先按 `SCRAMBLE_KEY` 逐字节异或，再 base64、每 76 字符换行 —— 写出来的
/// 文件里**没有** `ODBF <版本>` 头、**没有** `# 177plus` 署名、也没有"标准 base64 一解
/// 就是 JSON"的正文：头和署名是**混进混淆内容里**的，要解混淆之后才看得见。
///
/// **这一层只是遮挡，不是加密**，而且混淆密钥就写在源码里：防的是"双击打开一眼看见
/// token / 误截图 / 误贴进聊天"。任何人拿到这个文件（以及这个仓库）都能把它解回明文 ——
/// 和 `BCProfiles.xml` 里那份 token 拷贝同级（那边的文件级密钥也写在同一个文件里）。
/// 所以**不要**把这段混淆当保护，也不要在它上面继续叠"更复杂的"假加密：零保护，却会让
/// 后来的人以为它有保护。真正要保护只能引入密钥保管（Windows DPAPI / macOS Keychain），
/// 本版不引。
pub fn render_vault(peers: &HashMap<String, PeerCredential>) -> Result<String> {
    let json = serde_json::to_string(peers).context("Failed to serialize peer map")?;
    let mut inner = String::with_capacity(json.len() + 48);
    inner.push_str(INNER_MAGIC);
    inner.push(' ');
    inner.push_str(&FORMAT_VERSION.to_string());
    inner.push('\n');
    inner.push_str(&json);
    inner.push('\n');
    inner.push_str(INNER_FOOTER);
    inner.push('\n');

    let body = b64().encode(scramble(inner.as_bytes()));
    let mut out = String::with_capacity(body.len() + body.len() / LINE_WIDTH + 2);
    for chunk in body.as_bytes().chunks(LINE_WIDTH) {
        out.push_str(std::str::from_utf8(chunk).expect("base64 is ASCII"));
        out.push('\n');
    }
    Ok(out)
}

/// 固定密钥循环异或 —— 编码与解码是**同一个函数**（异或自逆）。
///
/// 没有 IV、没有随机数、没有密钥派生，所以同一份映射两次写出得到逐字节相同的文件：
/// 这层是遮挡，写出内容必须可复现（否则每写一次凭据，文件的 diff 里都是噪声）。
fn scramble(data: &[u8]) -> Vec<u8> {
    data.iter()
        .enumerate()
        .map(|(i, b)| b ^ SCRAMBLE_KEY[i % SCRAMBLE_KEY.len()])
        .collect()
}

/// 解析 vault 文件内容（只关心内容，不关心它是哪一版写的）。
pub fn parse_vault(text: &str) -> Result<HashMap<String, PeerCredential>> {
    Ok(parse_vault_versioned(text)?.1)
}

/// 解析 vault 文件，顺带把它的**格式版本**带出来（启动时的就地改写要看它够不够新）。
///
/// 三种格式都认，按首行/头部派发：
///
/// - 首行是 `ODBF <版本>`（含空格）= 老格式（v1，明文遮挡）：只读兼容；
/// - 其余 = 新格式（v2/v3，整块混淆）：去掉空白 → base64 解码 → 解异或 → 露出内层明文，
///   再由头里的版本决定正文怎么解（v2 是裸的 id→token，v3 是每条带指纹的记录）。
///
/// 两种格式的首行不可能撞：新格式的首行是 base64，**不含空格**；老格式的首行必须是
/// `ODBF` + 空格。派发只看首行，不靠"先试一下能不能解"。
///
/// 解析失败**必须报错**，不能当成空映射：那会把"文件读不出来"变成"本机没有配对"，
/// 用户看到的就是"配对全没了"，而且下一次写回就把别人的 token 抹掉了。
pub fn parse_vault_versioned(text: &str) -> Result<(u32, HashMap<String, PeerCredential>)> {
    if is_legacy_vault(text) {
        let (version, body) = split_sections(text, "凭据文件是空的")?;
        let raw = b64()
            .decode(body.as_bytes())
            .context("凭据正文不是合法 base64")?;
        return Ok((version, parse_body(&raw, version)?));
    }
    let body: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    anyhow::ensure!(!body.is_empty(), "凭据文件是空的");
    let raw = b64()
        .decode(body.as_bytes())
        .context("凭据正文不是合法 base64")?;
    let inner = scramble(&raw);
    let inner = std::str::from_utf8(&inner).context("凭据文件解混淆后不是文本")?;
    let (version, body) = split_sections(inner, "凭据文件解混淆后没有内容")?;
    Ok((version, parse_body(body.as_bytes(), version)?))
}

/// 首行是不是老格式的 `ODBF <版本>` 头。
fn is_legacy_vault(text: &str) -> bool {
    text.lines()
        .map(str::trim)
        .find(|l| !l.is_empty())
        .and_then(|l| l.split_once(char::is_whitespace))
        .map(|(magic, _)| magic == INNER_MAGIC)
        .unwrap_or(false)
}

/// 把一份内层明文拆成「校验过的版本号 + 正文」。空行与 `#` 注释行一律跳过。
fn split_sections(text: &str, empty_msg: &str) -> Result<(u32, String)> {
    let mut lines = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'));

    let header = lines.next().context(empty_msg.to_string())?;
    let version = parse_header(header)?;
    // 正文里可能有换行（base64 折行留下的），拼回一行；解混淆后的 JSON 本身就是一行。
    let body = lines.collect::<Vec<_>>().join("");
    anyhow::ensure!(!body.is_empty(), "凭据文件没有正文");
    Ok((version, body))
}

/// 校验 `ODBF <版本>` 头，返回版本号。所有格式共用同一道版本闸门：
/// 换格式不等于绕过"比本版新的格式明确拒绝"。
fn parse_header(line: &str) -> Result<u32> {
    let (magic, version) = line
        .split_once(char::is_whitespace)
        .context("凭据文件缺少格式头")?;
    anyhow::ensure!(
        magic == INNER_MAGIC,
        "凭据文件不是 OrbDrop 写的（头部是 {magic:?}）"
    );
    let version: u32 = version
        .trim()
        .parse()
        .context("凭据文件的格式版本读不出来")?;
    anyhow::ensure!(
        version <= FORMAT_VERSION,
        "凭据文件是更新版本的 OrbDrop 写的（格式 {version}，本版只懂到 {FORMAT_VERSION}），请升级 OrbDrop"
    );
    Ok(version)
}

/// 按版本解正文。
///
/// v2 及更早是**裸的 id→token 映射**：读成"只有 token、没有指纹"的记录 —— 老用户升级
/// 上来时就是这种状态，指纹由下一次连上时按 TOFU 补（见 `PeerCredential::fingerprint`）。
fn parse_body(raw: &[u8], version: u32) -> Result<HashMap<String, PeerCredential>> {
    if version >= 3 {
        return serde_json::from_slice(raw).context("凭据正文不是合法的设备凭据映射");
    }
    let tokens: HashMap<String, String> =
        serde_json::from_slice(raw).context("凭据正文不是合法的 id→token 映射")?;
    Ok(tokens
        .into_iter()
        .map(|(id, token)| {
            (
                id,
                PeerCredential {
                    token,
                    fingerprint: String::new(),
                },
            )
        })
        .collect())
}

fn read_vault_file(path: &Path) -> Result<HashMap<String, PeerCredential>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("读取凭据文件失败: {}", path.display()))?;
    parse_vault(&text).with_context(|| format!("解析凭据文件失败: {}", path.display()))
}

fn write_vault(path: &Path, peers: &HashMap<String, PeerCredential>) -> Result<()> {
    let text = render_vault(peers)?;
    crate::config::write_atomic(path, text.as_bytes())
        .with_context(|| format!("写入凭据文件失败: {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // 顺手收一下权限：遮挡不是加密，但至少别让同机别的用户顺手读到。
        let _ = std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 就地改写（老格式 → 当前格式）
// ---------------------------------------------------------------------------

/// 把磁盘上的老格式（v1/v2）文件就地改写成当前格式；已经是最新的就什么都不做。
///
/// **只由应用启动时显式调用**（`lib.rs` 的 setup）—— 读一个字节都不该改用户的东西，
/// 改写必须是显式动作。v2 → v3 顺带把"只有 token"的记录补成"token + 空指纹"，
/// 内容一条不丢。
///
/// 返回 `Some(条数)` 表示这次真的改写了。返回 `None` = 不用改。失败**不是**致命错误
/// （老格式的读兼容一直在，功能照旧），调用方记一条 warn 即可，下次启动重来。
///
/// `path` 做成参数是为了让测试在临时目录里跑**同一条**改写路径。
fn upgrade_vault_at(path: &Path) -> Result<Option<usize>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("读取凭据文件失败: {}", path.display()))?;
    // 这是"读-改-写"，和别处的写路径一样要串行：启动期本来没别人在跑，锁在这儿不花钱，
    // 也免得以后有人挪动调用点。
    let _guard = STORE_LOCK.lock();
    let (version, peers) = parse_vault_versioned(&text)
        .with_context(|| format!("解析老格式凭据文件失败: {}", path.display()))?;
    if version >= FORMAT_VERSION {
        return Ok(None);
    }
    write_vault(path, &peers)?;
    Ok(Some(peers.len()))
}

/// 改写**本机那份** vault 文件（应用启动时调一次，见 `lib.rs` 的 setup）。
pub fn upgrade_vault_format() -> Result<Option<usize>> {
    let path = vault_path();
    if !path.exists() {
        return Ok(None);
    }
    upgrade_vault_at(&path)
}

// ---------------------------------------------------------------------------
// 对外 API
// ---------------------------------------------------------------------------

/// 读整份映射。**不**取锁：只有已经持有 `STORE_LOCK` 的写路径可以用它。
///
/// vault 文件不在 = 本机没有任何配对（新装就是这种状态）。**读不改任何东西**：
/// 这个版本里读路径没有任何回退存储可查（系统凭据库那条路已经删了）。
fn read_unlocked() -> Result<HashMap<String, PeerCredential>> {
    let path = vault_path();
    if path.exists() {
        return read_vault_file(&path);
    }
    Ok(HashMap::new())
}

/// 整份记录（token + 指纹）。给"把指纹灌进 `net::Trust`"这类需要全量的调用方用。
pub fn load_credentials() -> Result<HashMap<String, PeerCredential>> {
    let _guard = STORE_LOCK.lock();
    read_unlocked()
}

/// 覆盖写入整份记录。
pub fn save_credentials(peers: &HashMap<String, PeerCredential>) -> Result<()> {
    write_vault(&vault_path(), peers)
}

/// Load peer token map from the credential file.
/// Returns an empty map if the file does not exist yet.
///
/// **读也要和写串行**：整份映射是**一个**文件，谁在改写（读-改-写）的时候不加锁地读，
/// 就会读到半截 —— 表现为"刚刚配对成功的 token 查不到"（has_peer/get_peer_token
/// 突然说没有这台设备）。同进程内所有读写共用 `STORE_LOCK`，这种半截读取就没了。
pub fn load_peers() -> Result<HashMap<String, String>> {
    Ok(load_credentials()?
        .into_iter()
        .map(|(id, cred)| (id, cred.token))
        .collect())
}

/// Set (insert or update) a single peer's token. 指纹不在时（老记录）保持不变。
pub fn set_peer(id: &str, token: &str) -> Result<()> {
    let _guard = STORE_LOCK.lock();
    let mut peers = read_unlocked()?;
    let entry = peers.entry(id.to_string()).or_default();
    entry.token = token.to_string();
    save_credentials(&peers)
}

/// 配对成功时一次写全：token 与对端指纹是同一份关系，分两次写只会多出一个中间态。
pub fn set_peer_credential(id: &str, token: &str, fingerprint: &str) -> Result<()> {
    let _guard = STORE_LOCK.lock();
    let mut peers = read_unlocked()?;
    peers.insert(
        id.to_string(),
        PeerCredential {
            token: token.to_string(),
            fingerprint: fingerprint.to_string(),
        },
    );
    save_credentials(&peers)
}

/// 对端证书指纹（`None` = 还没钉住，或这台设备本机没记录）。
pub fn get_peer_fingerprint(id: &str) -> Result<Option<String>> {
    let peers = load_credentials()?;
    Ok(peers
        .get(id)
        .map(|c| c.fingerprint.clone())
        .filter(|fp| !fp.is_empty()))
}

/// 钉住（或更新）某台对端的证书指纹。
///
/// **只该在两处调用**：配对成功时（那是这段关系成立的那一刻）、以及老记录第一次连上时
/// 补写。绝不能在"指纹对不上"时调用 —— 那正是它要拦下来的情况（对方换机器、或有人在中间）。
pub fn set_peer_fingerprint(id: &str, fingerprint: &str) -> Result<()> {
    let _guard = STORE_LOCK.lock();
    let mut peers = read_unlocked()?;
    let Some(entry) = peers.get_mut(id) else {
        return Ok(()); // 没有这台设备（已解除配对）：没有可钉的东西
    };
    entry.fingerprint = fingerprint.to_string();
    save_credentials(&peers)
}

/// Remove a single peer's token from the stored map.
pub fn remove_peer(id: &str) -> Result<()> {
    let _guard = STORE_LOCK.lock();
    let mut peers = read_unlocked()?;
    peers.remove(id);
    save_credentials(&peers)
}

/// Get token for a peer, if any.
pub fn get_peer_token(id: &str) -> Result<Option<String>> {
    let peers = load_credentials()?;
    Ok(peers.get(id).map(|c| c.token.clone()))
}

/// 把一条凭据换个设备 id。对端被重置过（数据目录清了、本机凭据还在）时，device id
/// 变了而 token 值没变，映射的**键**要跟着换。
///
/// 一次读改写，不能 remove + set 分两步：中间那一刻别的配对流程读到的就是半份映射。
pub fn rekey_peer(from: &str, to: &str) -> Result<()> {
    let _guard = STORE_LOCK.lock();
    let mut peers = read_unlocked()?;
    let Some(cred) = peers.remove(from) else {
        return Ok(());
    };
    peers.insert(to.to_string(), cred);
    save_credentials(&peers)
}

/// Check if a specific peer is paired (has a stored token).
pub fn has_peer(id: &str) -> bool {
    load_credentials()
        .map(|p| p.contains_key(id))
        .unwrap_or(false)
}

/// 抹掉"钉着的是**本机自己**那张证书"的记录。
///
/// 这种记录只可能来自一条已经修掉的路：把**本机自己的监听端点**当成对端地址
/// （安卓模拟器 / NAT 后面的对端把自己看到的本机地址回连进来，见 `net::observed_dial_target`）。
/// 那时探活会问回自己，于是把**本机的**证书指纹钉到了对端名下。它是错的，而且有害：
/// 以后真去连那台对端时（它换了网络、或换了真机）会判成"对端换了证书"
/// （`StopReason::IdentityChanged` →「需配对」）—— 明明是我们的记录脏了。
///
/// 只认"与本机指纹逐字相同"这一种，启动时做一次；正常记录一个字节都不动。
/// 抹掉之后那枚指纹回到"没钉住"（TOFU：下一次成功连上什么就认什么）。
pub fn drop_own_fingerprint(own: &str) -> Result<usize> {
    if own.is_empty() {
        return Ok(0);
    }
    let _guard = STORE_LOCK.lock();
    let mut peers = read_unlocked()?;
    let mut dropped = 0;
    for cred in peers.values_mut() {
        if cred.fingerprint == own {
            cred.fingerprint.clear();
            dropped += 1;
        }
    }
    if dropped > 0 {
        save_credentials(&peers)?;
    }
    Ok(dropped)
}

/// 清空本机全部配对凭据（F-15）—— 删掉那个文件就是全清。
///
/// 返回是否真的清掉了东西（本来就没有 = `Ok(false)`，不是错误）。调用方还要清地址簿里的
/// 配对标记、剪掉 BC 里那条 WebDAV profile（见 `wipe_all_credentials`）。
///
/// **不动本机身份**（`net` 的 `identity.pem`）：那是本机的身份，不是任何一段配对关系。
/// 清凭据是把"和谁配对过"清掉，不是"换一台机器"—— 身份换一把会让所有对端都看到指纹变化。
pub fn wipe() -> Result<bool> {
    let _guard = STORE_LOCK.lock();
    let path = vault_path();
    if !path.exists() {
        return Ok(false);
    }
    std::fs::remove_file(&path).with_context(|| format!("删除凭据文件失败: {}", path.display()))?;
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn creds(pairs: &[(&str, &str)]) -> HashMap<String, PeerCredential> {
        pairs
            .iter()
            .map(|(k, token)| {
                (
                    k.to_string(),
                    PeerCredential {
                        token: token.to_string(),
                        fingerprint: String::new(),
                    },
                )
            })
            .collect()
    }

    /// 内层明文 → 文件正文（当前格式）。测试用它构造任意版本号的文件。
    fn blob_of(inner: &str) -> String {
        b64().encode(scramble(inner.as_bytes()))
    }

    /// 写出 → 读回是同一份映射；换行、内层的头/署名都不影响解析。
    #[test]
    fn vault_round_trips() {
        let peers = HashMap::from([
            (
                "6f1c1a8e-4b3f-4a1e-9a3b-2c5d8e7f0a11".to_string(),
                PeerCredential {
                    token: "a".repeat(64),
                    fingerprint: "b".repeat(64),
                },
            ),
            (
                "device-b".to_string(),
                PeerCredential {
                    token: "short".into(),
                    fingerprint: String::new(),
                },
            ),
            (
                "中文设备".to_string(),
                PeerCredential {
                    token: "tok:with:colons".into(),
                    fingerprint: "f".repeat(64),
                },
            ),
        ]);
        let text = render_vault(&peers).unwrap();
        assert_eq!(parse_vault(&text).unwrap(), peers);
    }

    /// 空映射也要能往返（`{}` 也是合法内容）。
    #[test]
    fn empty_vault_round_trips() {
        let text = render_vault(&HashMap::new()).unwrap();
        assert!(parse_vault(&text).unwrap().is_empty());
    }

    /// 文件里**看不到**任何可读标记：没有 `ODBF` 版本头、没有 `# 177plus` 署名、没有明文
    /// token；而且正文不是"标准 base64 一解就是 JSON"—— 解出来是异或后的字节。
    /// 这一条就是"看起来像密文"这个需求的全部内容（也是它的边界：**只是看起来**）。
    #[test]
    fn vault_shows_no_readable_marker_and_no_plain_base64() {
        let peers = creds(&[("device-a", &"a".repeat(64)), ("device-b", "tok-b")]);
        let text = render_vault(&peers).unwrap();

        for marker in ["ODBF", "177plus", "device-a", "tok-b", "{\""] {
            assert!(!text.contains(marker), "{marker:?} 不该出现在文件里:\n{text}");
        }
        // 每行都是 base64 字符、且没有空行：文件里除正文没有别的结构。
        for line in text.lines() {
            assert!(!line.is_empty(), "文件里不该有空行:\n{text}");
            assert!(
                line.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '+' || c == '/' || c == '='),
                "{line}"
            );
        }
        // 标准 base64 解不出内容：那一层还隔着混淆。
        let squashed: String = text.chars().filter(|c| !c.is_whitespace()).collect();
        let naive =
            String::from_utf8_lossy(&b64().decode(squashed.as_bytes()).unwrap()).to_string();
        assert!(
            !naive.contains("device-a") && !naive.contains("tok-b"),
            "{naive}"
        );

        assert_eq!(parse_vault(&text).unwrap(), peers);
    }

    /// 老格式（v1，明文遮挡：`ODBF <版本>` 头 + base64 正文 + 署名）**仍然读得回来**：
    /// 用户机器上可能正躺着这么一份，丢了它就等于把已配对的设备全变成未配对。
    /// 版本号按十进制文本读（三位的也解得了），比本版新的明确拒绝。
    #[test]
    fn legacy_plain_vault_still_reads() {
        let body = b64().encode(br#"{"device-a":"tok-a"}"#);
        let text = format!("ODBF 1\n{body}\n\n# 177plus\n");
        assert_eq!(parse_vault(&text).unwrap(), creds(&[("device-a", "tok-a")]));

        let newer = format!("ODBF 100\n{body}\n");
        let err = parse_vault(&newer).unwrap_err().to_string();
        assert!(err.contains("请升级"), "{err}");
    }

    /// **v2（只有 token、没有指纹）读出来是"空指纹"的记录**，一条都不丢：老用户升级
    /// 上来时就是这个状态，指纹由下一次连上对端时按 TOFU 补写。
    #[test]
    fn v2_token_map_reads_as_credentials_without_fingerprint() {
        let inner = blob_of("ODBF 2\n{\"device-a\":\"tok-a\",\"device-b\":\"tok-b\"}\n# 177plus\n");
        let (version, peers) = parse_vault_versioned(&inner).unwrap();
        assert_eq!(version, 2);
        assert_eq!(peers, creds(&[("device-a", "tok-a"), ("device-b", "tok-b")]));
        assert!(peers.values().all(|c| c.fingerprint.is_empty()));
    }

    /// 新格式的内层头同样按十进制读、同样拒绝更新的版本 —— 换格式不等于绕过版本闸门。
    #[test]
    fn inner_header_version_is_decimal_and_rejects_newer() {
        let inner =
            |v: &str| blob_of(&format!("ODBF {v}\n{{\"device-a\":\"tok-a\"}}\n# 177plus\n"));
        assert_eq!(
            parse_vault(&inner("2")).unwrap(),
            creds(&[("device-a", "tok-a")])
        );

        let err = parse_vault(&inner("100")).unwrap_err().to_string();
        assert!(err.contains("请升级"), "{err}");
    }

    /// 不是我们的文件 / 头被改坏：报错，不是空映射。
    #[test]
    fn foreign_or_broken_header_is_an_error() {
        assert!(parse_vault("").is_err());
        assert!(parse_vault("not our file").is_err());
        // 老格式的头被改坏（版本读不出来）：走老格式分支，照样是错
        assert!(parse_vault("ODBF abc\ne30=\n").is_err());
        // 别的文件（首行不是 `ODBF ` 头）：走新格式分支，解出来没有 `ODBF` 头
        assert!(parse_vault(&blob_of("some other file\n")).is_err());
        // 头在、正文不是 JSON
        assert!(parse_vault(&blob_of("ODBF 3\nnot json\n# 177plus\n")).is_err());
        // 头在、版本是 v3、正文却是 v2 的形状（裸 token 映射）：必须是错，不能瞎猜
        assert!(parse_vault(&blob_of("ODBF 3\n{\"a\":\"b\"}\n# 177plus\n")).is_err());
    }

    /// 正文坏掉（被截断、被改过）必须报错：把"读不出来"当成"没有配对"，
    /// 会让下一次写回把别人的 token 抹掉。
    #[test]
    fn broken_body_is_an_error_not_an_empty_map() {
        let text = render_vault(&creds(&[("device-a", &"a".repeat(64))])).unwrap();
        assert!(parse_vault("ODBF 1\n!!!not base64!!!\n").is_err());
        // 截断：base64 长度不对，或解出来不是 `ODBF` 头
        assert!(parse_vault(&text[..text.len() / 2]).is_err());
        // 整块换成合法 base64 的别的东西：解混淆后没有 `ODBF` 头
        assert!(parse_vault(&b64().encode(b"not our payload at all")).is_err());
    }

    /// 启动时的格式改写：v1 与 v2 的文件就地改写成当前格式、内容不变；再跑一次什么都不做。
    /// 跑在临时目录里，不碰 `vault_path()`。
    #[test]
    fn older_files_are_rewritten_into_the_current_format() {
        let dir = std::env::temp_dir().join(format!("orbdrop_upgrade_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let vault = dir.join(VAULT_FILE);

        // v1（明文遮挡）
        let json = r#"{"device-a":"tok-a","device-b":"tok-b"}"#;
        std::fs::write(&vault, format!("ODBF 1\n{}\n\n# 177plus\n", b64().encode(json))).unwrap();
        assert_eq!(upgrade_vault_at(&vault).unwrap(), Some(2), "老格式文件应当被改写");
        let text = std::fs::read_to_string(&vault).unwrap();
        assert!(
            !text.contains("ODBF") && !text.contains("177plus"),
            "改写后不该还剩可读标记:\n{text}"
        );
        assert_eq!(
            parse_vault(&text).unwrap(),
            creds(&[("device-a", "tok-a"), ("device-b", "tok-b")])
        );
        assert_eq!(upgrade_vault_at(&vault).unwrap(), None, "已经是最新格式就不再动它");

        // v2（有 token 没有指纹）
        std::fs::write(&vault, blob_of("ODBF 2\n{\"device-c\":\"tok-c\"}\n# 177plus\n")).unwrap();
        assert_eq!(upgrade_vault_at(&vault).unwrap(), Some(1), "v2 也要升到当前格式");
        let upgraded = std::fs::read_to_string(&vault).unwrap();
        assert_eq!(
            parse_vault_versioned(&upgraded).unwrap().0,
            FORMAT_VERSION,
            "改写后文件里的版本号要是当前版本"
        );
        assert_eq!(parse_vault(&upgraded).unwrap(), creds(&[("device-c", "tok-c")]));

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 指纹与 token 同一条记录：写进去读得回来，空指纹读出来是 `None`（而不是空串）。
    #[test]
    fn fingerprint_round_trips_through_the_vault() {
        set_peer("device-a", "tok-a").unwrap();
        assert_eq!(get_peer_fingerprint("device-a").unwrap(), None);
        set_peer_fingerprint("device-a", &"c".repeat(64)).unwrap();
        assert_eq!(
            get_peer_fingerprint("device-a").unwrap(),
            Some("c".repeat(64))
        );
        // token 没被指纹那次写入碰掉
        assert_eq!(get_peer_token("device-a").unwrap(), Some("tok-a".into()));

        // 没有这台设备（已解除配对）时钉指纹是空操作，不该新建一条
        set_peer_fingerprint("device-gone", &"d".repeat(64)).unwrap();
        assert!(!has_peer("device-gone"));

        // 解除配对把整条（token + 指纹）一起清掉
        remove_peer("device-a").unwrap();
        assert_eq!(get_peer_fingerprint("device-a").unwrap(), None);

        let _ = std::fs::remove_file(vault_path());
    }

    /// 配对时一次写全 token + 指纹；换 id（对端被重置）时整条记录一起迁过去。
    #[test]
    fn credential_is_written_whole_and_rekeyed_whole() {
        set_peer_credential("old-id", "tok-a", &"e".repeat(64)).unwrap();
        assert_eq!(get_peer_token("old-id").unwrap(), Some("tok-a".into()));
        assert_eq!(
            get_peer_fingerprint("old-id").unwrap(),
            Some("e".repeat(64))
        );

        rekey_peer("old-id", "new-id").unwrap();
        assert!(!has_peer("old-id"));
        assert_eq!(get_peer_token("new-id").unwrap(), Some("tok-a".into()));
        assert_eq!(
            get_peer_fingerprint("new-id").unwrap(),
            Some("e".repeat(64)),
            "换 id 要把指纹一起带过去：它属于这段关系，不属于那个 id"
        );

        let _ = std::fs::remove_file(vault_path());
    }

    /// 清空凭据：删掉整份文件（token 与指纹一起没），且**不碰本机身份**。
    #[test]
    fn wipe_removes_credentials_but_never_the_identity() {
        let identity = crate::net::load_or_create_identity().unwrap();
        set_peer_credential("device-a", "tok-a", &"f".repeat(64)).unwrap();
        assert!(wipe().unwrap());
        assert!(!has_peer("device-a"));
        assert!(!wipe().unwrap(), "本来就没有凭据 = 没清掉东西，不是错误");

        // 身份还在，而且就是同一把（清凭据不等于换机器 —— 换一把会让所有对端要求重新配对）
        let after = crate::net::load_or_create_identity().unwrap();
        assert_eq!(identity.fingerprint, after.fingerprint);

        let _ = std::fs::remove_file(crate::net::identity_path());
    }
}