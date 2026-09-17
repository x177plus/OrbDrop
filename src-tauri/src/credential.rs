use std::collections::HashMap;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use base64::Engine as _;
use parking_lot::Mutex;
use tracing::warn;

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
/// 本版本写出的格式版本。版本号是**两种格式共用的一个计数器**：
/// - `1` = 明文遮挡（`ODBF <版本>` 头 + base64 正文 + 署名），只读兼容，不再写出；
/// - `2` = 整块混淆（内层明文整体异或后 base64），**写一律是这个**。
const FORMAT_VERSION: u32 = 2;
/// 外层混淆的密钥。**编译期常量 = 没有秘密**：想解开它，读一眼这段源码就够。
/// 它只干一件事 —— 让双击打开文件的人看到一整块"读不出是什么"的文本，而不是
/// "头部 + 一眼可解的 base64 + 署名"。**不是加密，别当保护用**。
const SCRAMBLE_KEY: &[u8] = b"OrbDrop/177plus/credentials.dat";
/// base64 正文的换行宽度（纯为让人打开时看得下去）。
const LINE_WIDTH: usize = 76;
/// 老版本把整份 id→token 映射放在系统凭据库里的**那一条**（服务名/用户名）。
/// 现在只为迁移而存在：读出来、写进 vault、把那条删掉。
const LEGACY_SERVICE: &str = "orbdrop_peers";
const LEGACY_USER: &str = "peers";

/// 整份映射在内存和 vault 文件之间是"读-改-写"，同进程内必须串行：两个配对流程
/// （或一个配对撞上一次解除配对）同时收尾时，各自基于同一份旧映射写回，会**静默丢掉**
/// 对方那条 token。写路径自己用不取锁的 `read_map_unlocked`（parking_lot 的锁不可重入）。
static STORE_LOCK: Mutex<()> = Mutex::new(());

fn b64() -> base64::engine::general_purpose::GeneralPurpose {
    base64::engine::general_purpose::STANDARD
}

/// vault 文件的位置（跟随 `ORBDROP_DATA_DIR`，测试能指到临时目录）。
pub fn vault_path() -> PathBuf {
    // 单元测试（lib 内）一律走临时文件：测试跑起来的机器上可能有用户**真实**的凭据，
    // 一个测试碰它就可能把人家配对关系弄没。集成测试（tests/）各自把
    // `ORBDROP_DATA_DIR` 指到临时目录，效果一样。
    #[cfg(test)]
    {
        return std::env::temp_dir().join(format!("orbdrop_vault_unit_{}.dat", std::process::id()));
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
/// ODBF 2
/// {"<device_id>":"<token>",…}
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
pub fn render_vault(peers: &HashMap<String, String>) -> Result<String> {
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

/// 解析 vault 文件内容。两种格式都认，按首行派发：
///
/// - 首行是 `ODBF <版本>`（含空格）= 老格式（v1，明文遮挡）：只读兼容。用户机器上
///   可能正躺着这么一份，丢了它就等于把已配对的设备全变成未配对；启动时
///   `upgrade_vault_format` 会把它就地改写成新格式。
/// - 其余 = 新格式（v2，整块混淆）：去掉空白 → base64 解码 → 解异或 → 露出内层明文再解析。
///
/// 两种格式的首行不可能撞：新格式的首行是 base64，**不含空格**；老格式的首行必须是
/// `ODBF` + 空格。派发只看首行，不靠"先试一下能不能解"。
///
/// 解析失败**必须报错**，不能当成空映射：那会把"文件读不出来"变成"本机没有配对"，
/// 用户看到的就是"配对全没了"，而且下一次写回就把别人的 token 抹掉了。
pub fn parse_vault(text: &str) -> Result<HashMap<String, String>> {
    if is_legacy_vault(text) {
        return parse_legacy_vault(text);
    }
    // 换行、行尾空白都不参与 base64。
    let body: String = text.chars().filter(|c| !c.is_whitespace()).collect();
    anyhow::ensure!(!body.is_empty(), "凭据文件是空的");
    let raw = b64()
        .decode(body.as_bytes())
        .context("凭据正文不是合法 base64")?;
    parse_inner(&scramble(&raw))
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

/// 校验 `ODBF <版本>` 头，返回版本号。两种格式共用同一道版本闸门：
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

/// 老格式（v1）：`ODBF <版本>` 头 + base64 正文 + 署名。空行与 `#` 注释行一律跳过。
fn parse_legacy_vault(text: &str) -> Result<HashMap<String, String>> {
    let mut lines = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'));

    let header = lines.next().context("凭据文件是空的")?;
    parse_header(header)?;

    let body: String = lines.fold(String::new(), |mut acc, line| {
        acc.push_str(line);
        acc
    });
    let raw = b64()
        .decode(body.as_bytes())
        .context("凭据正文不是合法 base64")?;
    serde_json::from_slice(&raw).context("凭据正文不是合法的 id→token 映射")
}

/// 新格式（v2）解混淆后的内层明文：`ODBF <版本>` 头 + **裸 JSON** + 署名。
fn parse_inner(raw: &[u8]) -> Result<HashMap<String, String>> {
    let text = std::str::from_utf8(raw).context("凭据文件解混淆后不是文本")?;
    let mut lines = text
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with('#'));

    let header = lines.next().context("凭据文件解混淆后没有内容")?;
    parse_header(header)?;

    let body: String = lines.fold(String::new(), |mut acc, line| {
        acc.push_str(line);
        acc
    });
    serde_json::from_str(&body).context("凭据正文不是合法的 id→token 映射")
}

fn read_vault_file(path: &Path) -> Result<HashMap<String, String>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("读取凭据文件失败: {}", path.display()))?;
    parse_vault(&text).with_context(|| format!("解析凭据文件失败: {}", path.display()))
}

fn write_vault(path: &Path, peers: &HashMap<String, String>) -> Result<()> {
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
// 老格式改写（v1 明文遮挡 → v2 整块混淆）
// ---------------------------------------------------------------------------

/// 把磁盘上的老格式（v1）文件就地改写成新格式（v2）；已经是新格式就什么都不做。
///
/// **只由应用启动时显式调用**（`lib.rs` 的 setup，紧挨着 `migrate_legacy_entry`）——
/// 和迁移同一条规矩：读一个字节都不该改用户的东西，改写必须是显式动作。
///
/// 返回 `Some(条数)` 表示这次真的改写了。返回 `None` = 不用改。失败**不是**致命错误
/// （老格式的读兼容一直在，功能照旧），调用方记一条 warn 即可，下次启动重来。
///
/// `path` 做成参数是为了让测试在临时目录里跑**同一条**改写路径。
fn upgrade_vault_at(path: &Path) -> Result<Option<usize>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("读取凭据文件失败: {}", path.display()))?;
    if !is_legacy_vault(&text) {
        return Ok(None);
    }
    // 这是"读-改-写"，和别处的写路径一样要串行：启动期本来没别人在跑，锁在这儿不花钱，
    // 也免得以后有人挪动调用点。
    let _guard = STORE_LOCK.lock();
    let peers = parse_legacy_vault(&text)
        .with_context(|| format!("解析老格式凭据文件失败: {}", path.display()))?;
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
// 老数据迁移（系统凭据库那一条 → vault 文件）
// ---------------------------------------------------------------------------

/// 解码老系统凭据库条目里的字节串。
///
/// 两种形状都要认：macOS 的条目数据是字符串的 UTF-8 字节；Windows 的 `set_password`
/// 把字符串按 UTF-16LE 存进 blob。升级一次就让用户已配对的设备全部失效，代价太大。
pub fn decode_legacy_blob(blob: &[u8]) -> Result<HashMap<String, String>> {
    if blob.is_empty() {
        // 可能存在一条内容为空的凭据（被别的工具清过内容）：空 = 没有配对。
        return Ok(HashMap::new());
    }
    // UTF-16LE 必须先判：`{` 的 UTF-16LE 是 `7B 00`，去撞下面那条"以 `{` 开头 = UTF-8 JSON"
    // 就会把整串 UTF-16 当 JSON 解，报一句莫名其妙的分词错误（`key must be a string`）。
    // 反过来 UTF-8 的 JSON 不可能以 `7B 00` 开头（正文里没有 NUL），所以这个顺序是安全的。
    if blob.len() % 2 == 0 && blob.starts_with(&[b'{', 0]) {
        let units: Vec<u16> = blob
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        let json = String::from_utf16(&units).context("老凭据不是合法 UTF-16")?;
        return serde_json::from_str(&json).context("老凭据不是合法的 id→token 映射");
    }
    if blob.first() == Some(&b'{') {
        return serde_json::from_slice(blob).context("老凭据不是合法的 id→token 映射");
    }
    anyhow::bail!("老凭据内容无法识别（{} 字节）", blob.len());
}

/// **只读**地取老系统凭据库里那条映射（不解码成别的形状，也不删任何东西）。
///
/// service/user 做成参数只为一件事：测试拿一个一次性的服务名跑**同一条**读取路径，
/// 不去碰用户真实的那一条凭据。
pub fn read_legacy_entry(service: &str, user: &str) -> Result<Option<HashMap<String, String>>> {
    let entry = keyring::Entry::new(service, user)
        .map_err(|e| anyhow::anyhow!("keyring entry creation failed: {e}"))?;
    match entry.get_secret() {
        Ok(blob) => Ok(Some(decode_legacy_blob(&blob)?)),
        Err(keyring::Error::NoEntry) => Ok(None),
        Err(e) => Err(anyhow::anyhow!("读取老凭据失败: {e}")),
    }
}

/// 把老系统凭据库里的那一条搬进 vault 文件，**写成功之后**才删掉那条。
///
/// 顺序不能反：先删后写，中间崩一次就永久丢配对；先写后删，中途失败只是两处都在，
/// 下次启动重来。`service`/`user` 做成参数是为了让测试用一次性服务名跑同一条路径。
///
/// **这个动作只由应用启动时显式调用**（`migrate_legacy_entry`）—— 绝不挂在"读一次凭据"
/// 上：读一个字节都不该改用户的东西（测试跑一次就把用户真实配对搬走/删掉，正是这么来的）。
pub fn migrate_legacy(
    service: &str,
    user: &str,
    vault: &Path,
) -> Result<Option<HashMap<String, String>>> {
    let Some(peers) = read_legacy_entry(service, user)? else {
        return Ok(None);
    };
    if !peers.is_empty() {
        write_vault(vault, &peers)?;
    }
    // 走到这儿文件已经在了，删条目失败也只是留下一条没人再读的旧数据（`wipe` 会清它）。
    if let Err(e) = wipe_keyring_entry(service, user) {
        warn!("老凭据条目删除失败（vault 已经接管，留着无害）: {e}");
    }
    Ok(Some(peers))
}

/// 迁移**本机那一条**老凭据（应用启动时调一次，见 `lib.rs` 的 setup）。
pub fn migrate_legacy_entry() -> Result<Option<HashMap<String, String>>> {
    migrate_legacy(LEGACY_SERVICE, LEGACY_USER, &vault_path())
}

// ---------------------------------------------------------------------------
// 对外 API（与"存在系统凭据库"时代完全一致：30+ 个调用点一行都不用改）
// ---------------------------------------------------------------------------

/// 读整份映射。**不**取锁：只有已经持有 `STORE_LOCK` 的写路径可以用它。
///
/// **读不改任何东西**：vault 文件不在时（新装，或老版本刚升级上来）只是**只读**地回落到
/// 系统凭据库里那一条 —— 迁移是启动时显式动作（`migrate_legacy_entry`），不能藏在读里。
fn read_map_unlocked() -> Result<HashMap<String, String>> {
    let path = vault_path();
    if path.exists() {
        return read_vault_file(&path);
    }
    match read_legacy_entry(LEGACY_SERVICE, LEGACY_USER)? {
        Some(peers) => Ok(peers),
        None => Ok(HashMap::new()),
    }
}

/// 覆盖写入整份映射。
pub fn save_peers(peers: &HashMap<String, String>) -> Result<()> {
    write_vault(&vault_path(), peers)
}

/// Load peer token map from the credential file.
/// Returns an empty map if the file does not exist yet.
///
/// **读也要和写串行**：整份映射是**一个**文件，谁在改写（读-改-写）的时候不加锁地读，
/// 就会读到半截 —— 表现为"刚刚配对成功的 token 查不到"（has_peer/get_peer_token
/// 突然说没有这台设备）。同进程内所有读写共用 `STORE_LOCK`，这种半截读取就没了。
pub fn load_peers() -> Result<HashMap<String, String>> {
    let _guard = STORE_LOCK.lock();
    read_map_unlocked()
}

/// Set (insert or update) a single peer's token.
pub fn set_peer(id: &str, token: &str) -> Result<()> {
    let _guard = STORE_LOCK.lock();
    let mut peers = read_map_unlocked()?;
    peers.insert(id.to_string(), token.to_string());
    save_peers(&peers)
}

/// Remove a single peer's token from the stored map.
pub fn remove_peer(id: &str) -> Result<()> {
    let _guard = STORE_LOCK.lock();
    let mut peers = read_map_unlocked()?;
    peers.remove(id);
    save_peers(&peers)
}

/// Get token for a peer, if any.
pub fn get_peer_token(id: &str) -> Result<Option<String>> {
    let peers = load_peers()?;
    Ok(peers.get(id).cloned())
}

/// 把一条凭据换个设备 id。对端被重置过（数据目录清了、本机凭据还在）时，device id
/// 变了而 token 值没变，映射的**键**要跟着换。
///
/// 一次读改写，不能 remove + set 分两步：中间那一刻别的配对流程读到的就是半份映射。
pub fn rekey_peer(from: &str, to: &str) -> Result<()> {
    let _guard = STORE_LOCK.lock();
    let mut peers = read_map_unlocked()?;
    let Some(token) = peers.remove(from) else {
        return Ok(());
    };
    peers.insert(to.to_string(), token);
    save_peers(&peers)
}

/// Check if a specific peer is paired (has a stored token).
pub fn has_peer(id: &str) -> bool {
    load_peers()
        .map(|p| p.contains_key(id))
        .unwrap_or(false)
}

fn wipe_keyring_entry(service: &str, user: &str) -> Result<bool> {
    let entry = keyring::Entry::new(service, user)
        .map_err(|e| anyhow::anyhow!("keyring entry creation failed: {e}"))?;
    match entry.delete_credential() {
        Ok(()) => Ok(true),
        Err(keyring::Error::NoEntry) => Ok(false),
        Err(e) => Err(anyhow::anyhow!("keyring delete_credential failed: {e}")),
    }
}

/// 清空本机全部配对凭据（F-15）—— 删掉那个文件就是全清，不需要枚举系统凭据
/// （keyring 3 也没有这个 API）。
///
/// 顺手删掉老版本留在系统凭据库里的那一条：还没迁移过的机器上，**它**才是凭据所在；
/// 迁移过的机器上它可能因为删失败而残留。先删它再删文件：它失败就整个退出，
/// 别出现"文件删了、那条还在"的半清状态。
///
/// 返回是否真的清掉了东西（本来就没有 = `Ok(false)`，不是错误）。调用方还要清地址簿里的
/// 配对标记、剪掉 BC 里那条 WebDAV profile（见 `wipe_all_credentials`）。
pub fn wipe() -> Result<bool> {
    let _guard = STORE_LOCK.lock();
    let mut removed = wipe_keyring_entry(LEGACY_SERVICE, LEGACY_USER)?;
    let path = vault_path();
    if path.exists() {
        std::fs::remove_file(&path)
            .with_context(|| format!("删除凭据文件失败: {}", path.display()))?;
        removed = true;
    }
    Ok(removed)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn map(pairs: &[(&str, &str)]) -> HashMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    /// 内层明文 → 文件正文（v2）。测试用它构造任意版本号的文件。
    fn blob_of(inner: &str) -> String {
        b64().encode(scramble(inner.as_bytes()))
    }

    /// 写出 → 读回是同一份映射；换行、内层的头/署名都不影响解析。
    #[test]
    fn vault_round_trips() {
        let peers = map(&[
            ("6f1c1a8e-4b3f-4a1e-9a3b-2c5d8e7f0a11", &"a".repeat(64)),
            ("device-b", "short"),
            ("中文设备", "tok:with:colons"),
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
        let peers = map(&[("device-a", &"a".repeat(64)), ("device-b", "tok-b")]);
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
        let naive = String::from_utf8_lossy(&b64().decode(squashed.as_bytes()).unwrap()).to_string();
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
        assert_eq!(parse_vault(&text).unwrap(), map(&[("device-a", "tok-a")]));

        let newer = format!("ODBF 100\n{body}\n");
        let err = parse_vault(&newer).unwrap_err().to_string();
        assert!(err.contains("请升级"), "{err}");
    }

    /// 新格式的内层头同样按十进制读、同样拒绝更新的版本 —— 换格式不等于绕过版本闸门。
    #[test]
    fn inner_header_version_is_decimal_and_rejects_newer() {
        let inner =
            |v: &str| blob_of(&format!("ODBF {v}\n{{\"device-a\":\"tok-a\"}}\n# 177plus\n"));
        assert_eq!(parse_vault(&inner("2")).unwrap(), map(&[("device-a", "tok-a")]));

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
        assert!(parse_vault(&blob_of("ODBF 2\nnot json\n# 177plus\n")).is_err());
    }

    /// 正文坏掉（被截断、被改过）必须报错：把"读不出来"当成"没有配对"，
    /// 会让下一次写回把别人的 token 抹掉。
    #[test]
    fn broken_body_is_an_error_not_an_empty_map() {
        let text = render_vault(&map(&[("device-a", &"a".repeat(64))])).unwrap();
        assert!(parse_vault(&format!("ODBF 1\n!!!not base64!!!\n")).is_err());
        // 截断：base64 长度不对，或解出来不是 `ODBF` 头
        assert!(parse_vault(&text[..text.len() / 2]).is_err());
        // 整块换成合法 base64 的别的东西：解混淆后没有 `ODBF` 头
        assert!(parse_vault(&b64().encode(b"not our payload at all")).is_err());
    }

    /// 启动时的格式改写：老格式文件就地改写成新格式、内容不变；再跑一次什么都不做。
    /// 跑在临时目录里，不碰 `vault_path()`。
    #[test]
    fn legacy_file_is_rewritten_into_the_blob_format() {
        let dir = std::env::temp_dir().join(format!("orbdrop_upgrade_{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let vault = dir.join(VAULT_FILE);
        let json = r#"{"device-a":"tok-a","device-b":"tok-b"}"#;
        std::fs::write(&vault, format!("ODBF 1\n{}\n\n# 177plus\n", b64().encode(json))).unwrap();

        let peers = map(&[("device-a", "tok-a"), ("device-b", "tok-b")]);
        assert_eq!(upgrade_vault_at(&vault).unwrap(), Some(2), "老格式文件应当被改写");
        let text = std::fs::read_to_string(&vault).unwrap();
        assert!(
            !text.contains("ODBF") && !text.contains("177plus"),
            "改写后不该还剩可读标记:\n{text}"
        );
        assert_eq!(parse_vault(&text).unwrap(), peers);

        assert_eq!(upgrade_vault_at(&vault).unwrap(), None, "已经是新格式就不再动它");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 老格式（macOS：条目数据是字符串的 UTF-8 字节）读得回来。
    #[test]
    fn legacy_utf8_blob_still_reads() {
        let json = br#"{"device-a":"tok-a","device-b":"tok-b"}"#;
        assert_eq!(
            decode_legacy_blob(json).unwrap(),
            map(&[("device-a", "tok-a"), ("device-b", "tok-b")])
        );
    }

    /// 老格式（Windows：`set_password` 写下的 UTF-16LE）同样读得回来 ——
    /// 这台机器上真实那一条就是这种形状。
    #[test]
    fn legacy_utf16_blob_still_reads() {
        let json = r#"{"device-a":"tok-a"}"#;
        let mut blob = Vec::new();
        for unit in json.encode_utf16() {
            blob.extend_from_slice(&unit.to_le_bytes());
        }
        assert_eq!(decode_legacy_blob(&blob).unwrap(), map(&[("device-a", "tok-a")]));
    }

    /// 空条目 = 没有配对；说不清的字节串 = 报错（不是空映射）。
    #[test]
    fn legacy_blob_empty_ok_unknown_errors() {
        assert!(decode_legacy_blob(&[]).unwrap().is_empty());
        assert!(decode_legacy_blob(&[0xFF, 0x00, 0x12]).is_err());
        assert!(decode_legacy_blob(b"not a credential").is_err());
    }

    /// 只读回落：老条目不存在就是 `None`，而且两次读之间**什么都没变**（这一路不写不删）。
    /// 用一次性服务名跑，绝不碰 `orbdrop_peers` 那一条。
    #[test]
    fn reading_a_missing_legacy_entry_changes_nothing() {
        let service = format!("orbdrop_read_test_{}", uuid::Uuid::new_v4());
        assert!(read_legacy_entry(&service, "peers").unwrap().is_none());
        assert!(
            read_legacy_entry(&service, "peers").unwrap().is_none(),
            "读一次不该把条目建出来"
        );
    }
}