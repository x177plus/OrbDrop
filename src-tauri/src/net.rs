//! 传输加密：本机自签身份 + 对端证书**指纹 pinning**。
//!
//! ## 为什么是自签 + pinning，而不是 CA
//!
//! 对端地址是内网 IP（`192.168.x` / `10.x`），公共 CA 签不了（私网地址没法做公开验证）；
//! 企业自建 CA 要求域管理员推 GPO，还要把"能签任何内网证书"的私钥交给网管。所以只剩自签。
//!
//! 自签不代表"不校验"——校验从"信任某条证书链"换成"信任这一枚指纹"：
//!
//! - 本机首启生成一对自签证书，落 `data_dir()/identity.pem`，**永不轮换**（轮换 = 换身份，
//!   所有对端都会看到指纹变了，抽象上等同于重装）；
//! - 每个已配对对端的证书指纹存在凭据库（`credential`，与 token 同一条记录、同生命周期）；
//! - 拨号时比对指纹：一致才继续，不一致直接拒绝；
//! - 首次见到某地址（没有钉住的指纹）时按 **TOFU** 接受并记住 —— 这是"零点击"的代价。
//!   配对码那条路现在用 SPAKE2（`pake` 模块）把 6 位码当口令：码不上网、不能离线枚举，
//!   确认值里还绑了双方证书指纹，中间人转发报文也过不了；**配对邀请**那条路仍只有
//!   用户「接收」+ TLS，这个缝还在（下一步用 SAS 比对）。
//! - 出站连接会同时出示本机证书（服务端只**请求**、不强制），配对路由据此核对请求体里
//!   自报的指纹就是这条连接出示的那一枚。它保证"谁说的就是谁出示的"，**不**单独阻止
//!   中间人（中间人用自己的证书终止两端时，两边仍会各自钉住它）—— PAKE 的 AAD 才是
//!   配对码那条路上的那一半。
//!
//! ## 不做主机名校验，只比指纹
//!
//! 对端地址会漂（DHCP、多网卡、跨网段手动 `IP:端口`，见 `server::local_ipv4_addresses`），
//! 把地址写进证书 SAN 再按名字校验，等于"只要换了网卡就握手失败"。指纹只跟着对方的私钥走，
//! 地址变了照连、身份变了才拦 —— 这正是我们要的语义。
//!
//! ## 同一个端口上接两种连接
//!
//! 明文 HTTP 只留给**回环**（本机 UI 的自验路径、Beyond Compare 的 WebDAV 入口，见
//! `server::proxy`）。判据是 TLS 记录层的第一个字节：`0x16`（handshake）走 TLS，
//! 其余且来源是回环才走明文 —— 局域网上的明文连接一律丢弃。

use std::collections::HashMap;
use std::fmt::Write as _;
use std::future::Future;
use std::net::{IpAddr as StdIpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use axum::Router;
use parking_lot::Mutex;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{DigitallySignedStruct, DistinguishedName, SignatureScheme};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tower::Service;
use tracing::{debug, info, warn};

use crate::config;

/// 本机身份所在的文件（与本机专属数据目录的其它文件同级）。
const IDENTITY_FILE: &str = "identity.pem";

/// TLS 记录层的 handshake 记录类型。明文 HTTP 的请求行第一个字节是 ASCII 字母，
/// 两者不可能撞（`0x16` 不是可打印字符）。
const TLS_HANDSHAKE: u8 = 0x16;

/// 等首字节的时限。明文 HTTP 客户端一定会先发请求行；超时说明对面连上就不说话
/// （端口扫描、坏掉的客户端），别让它一直占着一个任务。
const PEEK_TIMEOUT: Duration = Duration::from_secs(10);

/// TLS 握手（`TlsAcceptor::accept`）的时限。**与首字节时限是两件事**：`PEEK_TIMEOUT`
/// 只盖住"第一个字节到没到"，而这一个盖住"握手谈不谈得完"。
///
/// 为什么必须有：局域网上任何一台机器连上来、发一个 `0x16`（握手记录类型）就挂住
/// （笔记本休眠、进程被挂起、敌意客户端），`accept` 就永远等不到对方的后续记录 ——
/// 那条连接会带着一份 TLS 缓冲**永久**占着一个任务。挂住的任务没有任何东西会来收尾
/// （`stop_server` 也不 abort 它们）。
///
/// 10s 的依据：局域网 RTT 亚毫秒，握手两个 RTT；唯一能把合法握手拖到这个量级的是
/// 连续丢包 —— TCP 的重传阶梯（RTO 0.3/0.6/1.2/2.4/4.8s）累积约 9.3s，也就是连续丢近
/// 五个包才够得着。到点就丢：那种连接不会有下文，对端下次拨号会重来（拨号一侧本来就有
/// 退避重试）。
const TLS_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// 同时驻留的连接上限（本机监听端）。**一台机器连上就不说话**能开的门数才是内存与任务数
/// 的上界，没有它，"连上"这个动作本身就是一次无界分配（任务 + TLS 缓冲，几十~上百 KB）。
///
/// 512 的依据是正常用量的逐项账：
///
/// - 本机前端（WebView）：一条 `/events` 的 UI 长连接 + 几条 REST（同一个 client 复用连接）；
/// - BC 的 WebDAV 入口（回环明文，转成到对端的加密连接）：几条；
/// - 每台**已配对对端**：1 条入站长连接，推送/镜像水合期间再加上并发的几条 HTTPS；
/// - 5s 探活：每台有地址的设备一条短连接（毫秒级就结束）。
///
/// 局域网里十几台设备的量级下，正常同时打开在**几十条**；512 留了一个数量级的余量，
/// 正常路径碰不到它（碰到了也不是"用得多"，是有人不开门地占着）。而按上面的单连接量级，
/// 上限本身把驻留内存钉在**几十 MB** 这一档。
///
/// 超限时**丢弃新连接**（`try_acquire`，不排队）：本机已经满了，把新连接排队只是把
/// "占内存"换个地方继续涨。
const MAX_CONNECTIONS: usize = 512;

/// 回环连接单独留一份名额。UI/BC/本机自验都走回环；如果不留，局域网里任意主机
/// 用 512 条空闲连接就能把本机界面和 BC 一起饿死。
const LOOPBACK_CONNECTIONS: usize = 64;

/// 单个来源 IP 的并发连接上限（远程）。全局名额防"总量无界"，这一条防"单点占满"。
///
/// 正常用量的一本账：一台已配对设备 = 1 条入站长连接 + 传输期间至多 8 个并发 worker
/// （`config::clamp_transfer_workers` 的上限）+ 几条毫秒级的探活，合计十几条。
/// 64 留了数倍余量，同时保证一台主机占不满 [`MAX_CONNECTIONS`] 的远程名额 ——
/// 公开路由读请求体时没有"整条连接"的时限（body 可以停在半路），少了这道闸门，
/// 局域网里任意一台机器开 448 条连接就能让所有设备都连不进来。
const MAX_CONNECTIONS_PER_REMOTE_IP: usize = 64;

// ---------------------------------------------------------------------------
// 指纹
// ---------------------------------------------------------------------------

/// 证书指纹 = DER 的 SHA-256，小写十六进制（64 字符）。
///
/// 用整份 DER 而不是"公钥的哈希"：DER 同时钉住了公钥与证书里那些参数，且实现上不必
/// 再去解析一层 ASN.1。两边比对的是同一个函数算出来的同一个值，够用。
pub fn fingerprint_of(der: &[u8]) -> String {
    let digest = Sha256::digest(der);
    let mut out = String::with_capacity(digest.len() * 2);
    for byte in digest {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// 给人看/给人核对的短指纹：前 16 个十六进制字符，大写、4 个一组。
///
/// 只用于界面展示（设备详情里那一行、以及"指纹变了"的确认框）。**不要**用它做校验 ——
/// 16 个十六进制字符只有 64 bit，而完整指纹就在手边，没理由在判断上省这点。
pub fn short_fingerprint(fp: &str) -> String {
    let head: String = fp.chars().take(16).collect::<String>().to_uppercase();
    head.as_bytes()
        .chunks(4)
        .map(|c| String::from_utf8_lossy(c).to_string())
        .collect::<Vec<_>>()
        .join("-")
}

// ---------------------------------------------------------------------------
// 本机身份
// ---------------------------------------------------------------------------

/// 本机身份：自签证书 + 私钥 + 它的指纹。
pub struct Identity {
    cert_der: CertificateDer<'static>,
    key_der: PrivateKeyDer<'static>,
    /// 本机证书指纹（明文 hex）。对端会把它钉在自己的凭据库里。
    pub fingerprint: String,
}

pub(crate) fn identity_path() -> PathBuf {
    // 同 `credential::vault_path`：库内测试走临时文件、且**按线程**分一个（测试并行，
    // 而"身份文件已存在就不能重新生成"这类用例会真的写它）。集成测试各自把
    // `ORBDROP_DATA_DIR` 指到临时目录。
    #[cfg(test)]
    {
        return std::env::temp_dir().join(format!(
            "unit_{}_{:?}_{IDENTITY_FILE}",
            std::process::id(),
            std::thread::current().id()
        ));
    }
    #[cfg(not(test))]
    {
        config::data_dir().join(IDENTITY_FILE)
    }
}

/// 读出本机身份，没有就生成一份并落盘。
///
/// 生成只发生一次：文件在就绝不重新生成 —— 指纹就是身份，悄悄换一把会让**所有**对端
/// 看到"证书变了"而拒绝连接（用户侧就是"谁都连不上了"，且看不出原因）。文件损坏时
/// 只能重新生成，那条 warn 是唯一能解释后续症状的线索。
pub fn load_or_create_identity() -> Result<Identity, String> {
    // 身份 = 这个进程要用 TLS 了：先把 crypto provider 装上（幂等）。放在这里是因为
    // "起服务"与"拨对端"两条路都要先有身份，而 reqwest 的默认 TLS 配置是急切建的 ——
    // 漏装一次，之后随手 new 的一个 client 就会在 build 时 panic。
    install_crypto_provider();
    let path = identity_path();
    match std::fs::read_to_string(&path) {
        Ok(pem) => match identity_from_pem(&pem) {
            Ok(id) => {
                debug!(fingerprint = %short_fingerprint(&id.fingerprint), "本机身份已加载");
                return Ok(id);
            }
            Err(e) => {
                // 损坏 ≠ 不存在：先把旧文件留成 `.pem.bad`，再生成新的。绝不静默覆盖
                // —— 旧私钥是用户唯一能手工恢复身份的东西。
                let backup = path.with_extension("pem.bad");
                std::fs::rename(&path, &backup).map_err(|rename_err| {
                    format!(
                        "身份文件损坏（{e}），且无法备份到 {backup:?}（{rename_err}）；拒绝覆盖，请手工处理"
                    )
                })?;
                warn!(
                    "身份文件损坏（{e}）：已备份到 {backup:?}，将重新生成一把 —— 所有对端都会看到指纹变化并要求重新配对"
                );
            }
        },
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            // 首次运行：正常生成。
        }
        Err(e) => {
            // 读失败（被占用/权限/IO）不是"没有身份"：静默重新生成会覆盖掉一个可能
            // 还能恢复的身份。把选择权交给用户/运维。
            return Err(format!("身份文件读取失败（{path:?}）：{e}"));
        }
    }
    let id = generate_identity()?;
    let pem = format!(
        "{}{}",
        to_pem(id.cert_der.as_ref(), "CERTIFICATE"),
        to_pem(id.key_der.secret_der(), "PRIVATE KEY")
    );
    config::write_atomic_private(&path, pem.as_bytes())
        .map_err(|e| format!("身份文件写入失败: {e}"))?;
    info!(
        path = %path.display(),
        fingerprint = %short_fingerprint(&id.fingerprint),
        "已生成本机自签身份"
    );
    Ok(id)
}

fn to_pem(der: &[u8], label: &str) -> String {
    use base64::Engine;
    let b64 = base64::engine::general_purpose::STANDARD.encode(der);
    let mut out = format!("-----BEGIN {label}-----\n");
    for chunk in b64.as_bytes().chunks(64) {
        out.push_str(std::str::from_utf8(chunk).unwrap_or_default());
        out.push('\n');
    }
    let _ = writeln!(out, "-----END {label}-----");
    out
}

fn identity_from_pem(pem: &str) -> Result<Identity, String> {
    let mut reader = std::io::BufReader::new(pem.as_bytes());
    let cert = rustls_pemfile::certs(&mut reader)
        .next()
        .transpose()
        .map_err(|e| e.to_string())?
        .ok_or("身份文件里没有证书")?;
    let mut reader = std::io::BufReader::new(pem.as_bytes());
    let key = rustls_pemfile::private_key(&mut reader)
        .map_err(|e| e.to_string())?
        .ok_or("身份文件里没有私钥")?;
    let fingerprint = fingerprint_of(cert.as_ref());
    Ok(Identity {
        cert_der: cert,
        key_der: key,
        fingerprint,
    })
}

fn generate_identity() -> Result<Identity, String> {
    let mut params = rcgen::CertificateParams::new(Vec::<String>::new())
        .map_err(|e| format!("证书参数: {e}"))?;
    // CN 只是个标签：本机不做主机名校验（见模块头），它不会参与任何判断。
    params
        .distinguished_name
        .push(rcgen::DnType::CommonName, "OrbDrop");
    params.is_ca = rcgen::IsCa::NoCa;

    let key_pair = rcgen::KeyPair::generate().map_err(|e| format!("密钥生成: {e}"))?;
    let cert = params
        .self_signed(&key_pair)
        .map_err(|e| format!("自签证书: {e}"))?;

    let cert_der = CertificateDer::from(cert.der().to_vec());
    let key_der = PrivateKeyDer::Pkcs8(key_pair.serialize_der().into());
    let fingerprint = fingerprint_of(cert_der.as_ref());
    Ok(Identity {
        cert_der,
        key_der,
        fingerprint,
    })
}

// ---------------------------------------------------------------------------
// 钉住的指纹（Trust）
// ---------------------------------------------------------------------------

/// 空指纹的已配对记录用这枚"不可能匹配"的哨兵占位：TLS 校验必然拒绝，
/// 从而在重新配对/补指纹之前阻止任何出站连接把 token 发出去。
pub const DENY_FINGERPRINT: &str = "__orbdrop_unpinned__";

/// 一次拨号应当接受的证书指纹。
///
/// 全局 [`Trust`] 按 **IP** 聚合多枚指纹（共享中转端点、同机多实例都会出现），校验器
/// 命中集合里任意一枚就放行 —— 它无法知道"这次拨号本来要找哪一台设备"，因为 TLS 校验
/// 发生在任何应用层数据之前。所以**带 token 的拨号必须按目标设备建一张只含它那枚指纹的
/// 临时表**（[`dial_client`] / [`Trust::scoped`]）：否则同一 IP 键下的另一台已配对设备
/// 出示自己的证书也能通过校验，随后目标设备的 token 就发给了它。
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum DialPin {
    /// 钉死这一枚：证书必须是它。
    Fingerprint(String),
    /// 没有可钉的指纹（未配对 / 没有关系）：按 TOFU 接受。
    ///
    /// 只用于确实没有建立关系的设备（含不带 token 的发现/学习）；已配对设备绝不走
    /// 这一档（见 `credential::dial_pin`）。
    Tofu,
    /// 已配对但凭据里没有指纹：拒绝一切 TLS，直到重新配对。哨兵见 [`DENY_FINGERPRINT`]。
    Deny,
}

/// 已知对端的证书指纹，按**地址**索引。
///
/// 为什么按地址而不是按设备 id：TLS 校验发生在拿到任何应用层数据**之前**，此刻手上只有
/// 一个 IP。一个地址可能对应多台设备（中转端点、同机多实例），所以每格是**一组**指纹；
/// 真正的身份归属仍由握手之后的 token 认证负责。
#[derive(Debug, Default)]
pub struct Trust {
    /// 一个地址可以对应**多枚**指纹：中转端点就是这种形状（同一台中转机的地址上，
    /// 呈现的是不同目标的证书）。全部保留并逐一放行；任何一枚都不匹配才拒绝。
    /// 空指纹的已配对记录会放入 `DENY_FINGERPRINT`（只对非回环地址），从而在指纹
    /// 迁移完成前**拒绝一切出站 TLS**，避免 token 被 TOFU 发出去。
    pinned: Mutex<HashMap<StdIpAddr, Vec<String>>>,
    /// 这次进程里实际见到过的指纹。TOFU 靠它把"刚认下的那枚"交回给调用方落库——
    /// 校验器只负责放行/拒绝，**不写凭据库**（它在握手线程里，不该碰磁盘）。
    ///
    /// **故意不按 `pinned` 集合 prune**（别在这里"顺手清理"）：它的读取点只有两个，两个
    /// 都只在"这个地址**还没有**钉住的指纹"时才读 —— `state::remember_peer_fingerprint`
    /// （库里已有指纹就早退）与 `pair::commit_pairing`（未钉住的地址靠这次 TLS 握手
    /// 见到的证书落库，而那次拨号正需要 TOFU）。
    /// 按 `pinned` 去 prune，删掉的正好是**还有读者的那些**（未钉住的
    /// 地址），留下的没人读；后果是"配对码那条路本机拨过去、明明见了对端证书，却把指纹写成
    /// 空"，退回 TOFU。另外 `refresh_trust` 会被任意别的设备（配对/解除配对/指纹落库）在
    /// 任意时刻调用，prune 还会把刚握手完、**还没被读走**的那条观察删掉 —— 那正是
    /// `remember_peer_fingerprint` 唯一工作的窗口。
    ///
    /// 是 `Arc` 而不是裸 `Mutex`：按目标建的临时表（[`Trust::scoped`]）与全局表**共享**
    /// 这一份观察值，于是 `remember_peer_fingerprint` 在两条路上读到的是同一份记录。
    observed: Arc<Mutex<HashMap<StdIpAddr, String>>>,
}

impl Trust {
    pub fn new() -> Self {
        Self::default()
    }

    /// 只认**一台目标**的临时信任表：带 token 的拨号专用。
    ///
    /// `host` 是这次实际拨的地址字面量（直连 IP，或中转端点的 `[::ffff:…]`）。表里只放
    /// 目标设备那一枚指纹，于是校验器的"任意一枚命中"退化成"必须是它"。
    ///
    /// - 回环沿用既有豁免（本机 UI / 测试 / 同机多实例）：不钉，按 TOFU；
    /// - `Tofu`：空表（带 token 的路径只在"确实没有关系"时才会走到这里）；
    /// - `Deny`：放哨兵，校验必然失败（已配对却没有指纹 = 先重新配对）。
    ///
    /// `observed` 与全局表共享：这条拨号见到的证书仍会被
    /// `state::remember_peer_fingerprint` / `pair::commit_pairing` 读到（TOFU 落库）。
    pub fn scoped(host: &str, pin: &DialPin) -> Arc<Trust> {
        let mut scoped = Trust::new();
        scoped.observed = trust().observed.clone();
        // 回环沿用既有豁免：本机 UI / 测试 / 同机多实例没有"局域网地址被投毒"这一层。
        if parse_host(host).is_some_and(|ip| ip.is_loopback()) {
            return Arc::new(scoped);
        }
        scoped.seed_pin(host, pin);
        Arc::new(scoped)
    }

    /// 只属于这次拨号的信任表，且**不共享**进程级 `observed`。
    ///
    /// 与 [`Trust::scoped`] 的唯一区别是观察值不再并进全局表。LocalSend 是另一套协议：
    /// 同一个地址上常常同时住着一台 OrbDrop 设备（真机场景：对端既跑 OrbDrop 又开着
    /// 第三方 LocalSend）。LocalSend 握手见到的证书如果写进全局观察值，
    /// `state::remember_peer_fingerprint` 就可能把**另一套证书**记到 OrbDrop 设备名下
    /// （那格观察值是未配对设备地址变更的连续性锚）。两套协议的握手因此各记各的。
    ///
    /// **回环也照钉**：LocalSend 对端没有配对关系，回环上的另一份实例同样是陌生人，
    /// 不该退回 TOFU。
    pub fn isolated(host: &str, pin: &DialPin) -> Arc<Trust> {
        let isolated = Trust::new();
        isolated.seed_pin(host, pin);
        Arc::new(isolated)
    }

    /// 按 `pin` 决定这张临时表钉什么。`Tofu` 留空（校验器接受任何证书并记下它）。
    fn seed_pin(&self, host: &str, pin: &DialPin) {
        match pin {
            DialPin::Fingerprint(fp) => {
                self.seed(std::iter::once((host.to_string(), fp.clone())));
            }
            DialPin::Tofu => {}
            DialPin::Deny => {
                self.seed(std::iter::once((
                    host.to_string(),
                    DENY_FINGERPRINT.to_string(),
                )));
            }
        }
    }

    /// 用「地址 → 指纹」重建这份表（启动时、配对成功/解除配对后各调一次）。
    ///
    /// 整份替换而不是增量合并：地址簿与凭据库才是真相，这里只是一份供握手线程只读的快照。
    ///
    /// 同一地址上的多枚指纹**全部保留**（中转端点、同机多实例都会出现），校验时任一匹配
    /// 即放行。不能再"冲突就整条不钉"——那会让中转 TLS 退回 TOFU。空指纹的已配对记录
    /// 会放 `DENY_FINGERPRINT`（见 `state::refresh_trust_from_credentials`），确保迁移前
    /// 不会把 token 发给任何 TOFU 证书。
    pub fn seed(&self, entries: impl IntoIterator<Item = (String, String)>) {
        let mut map: HashMap<StdIpAddr, Vec<String>> = HashMap::new();
        for (ip, fp) in entries {
            let Some(ip) = parse_host(&ip) else { continue };
            let list = map.entry(ip).or_default();
            if !list.iter().any(|x| x == &fp) {
                list.push(fp);
            }
        }
        *self.pinned.lock() = map;
    }

    /// 这个地址上钉住的所有指纹（空 = 没钉）。
    pub fn get(&self, ip: StdIpAddr) -> Vec<String> {
        self.pinned.lock().get(&ip).cloned().unwrap_or_default()
    }

    /// 这个地址上是否钉着某一枚指纹。
    pub fn accepts(&self, ip: StdIpAddr, fingerprint: &str) -> bool {
        self.pinned
            .lock()
            .get(&ip)
            .is_some_and(|list| list.iter().any(|fp| fp == fingerprint))
    }

    /// 记下这次见到的指纹（不落库）。
    fn observe(&self, ip: StdIpAddr, fp: String) {
        self.observed.lock().insert(ip, fp);
    }

    /// 本次进程里为某个地址见过的指纹（落库用）。
    pub fn observed_at(&self, ip: &str) -> Option<String> {
        parse_host(ip).and_then(|ip| self.observed.lock().get(&ip).cloned())
    }

    /// 忘掉某个地址上的**某一枚**指纹（解除配对/重新配对用）。地址上还有别的
    /// 指纹时保留它们（中转端点/同机多实例）。
    pub fn unpin_fingerprint(&self, ip: StdIpAddr, fingerprint: &str) {
        let mut pinned = self.pinned.lock();
        if let Some(list) = pinned.get_mut(&ip) {
            list.retain(|fp| fp != fingerprint);
            if list.is_empty() {
                pinned.remove(&ip);
            }
        }
    }

    /// 忘掉某个地址的**全部**指纹（地址换了主人、或解除配对时用）。
    pub fn unpin(&self, ip: StdIpAddr) {
        self.pinned.lock().remove(&ip);
    }
}

// ---------------------------------------------------------------------------
// 校验器
// ---------------------------------------------------------------------------

fn provider() -> Arc<rustls::crypto::CryptoProvider> {
    install_crypto_provider();
    Arc::new(rustls::crypto::ring::default_provider())
}

/// 把 `ring` 装成 rustls 的**进程默认** crypto provider。
///
/// 为什么需要这一步：`reqwest` 用的是 `rustls-tls-no-provider`，它建自己的默认 TLS 配置是
/// **急切**的 —— 没有默认 provider 时，连一次明文回环调用的 `Client::new()` 都会当场
/// panic（"No provider set"）。我们到对端那几条路另有预配置的 `ClientConfig`（装着 pinning
/// 校验器），但那不解决"别人随手 new 一个 client"这件事。
///
/// 幂等：已经装过（或别处先装了）就什么都不做。所有会碰 TLS 的入口都调一遍，省得排顺序。
pub fn install_crypto_provider() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // 重复安装会返回 Err（已经有默认了），不是错误。
        let _ = rustls::crypto::ring::default_provider().install_default();
    });
}

fn rejected() -> rustls::Error {
    rustls::Error::InvalidCertificate(rustls::CertificateError::ApplicationVerificationFailure)
}

/// 只认钉住的指纹：不比名字、不建信任链、不看有效期。
///
/// 有效期不校验是刻意的：自签证书的 `not_after` 是本机自己写的（rcgen 默认给到很远的
/// 将来），校验它既没有第三方背书，又会在"证书过期 = 谁都连不上"时给不出任何可操作的
/// 提示。真正决定信任的是那枚指纹。
#[derive(Debug)]
pub struct PinnedVerifier {
    trust: Arc<Trust>,
}

impl ServerCertVerifier for PinnedVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        // 只按 IP 拨号（见模块头），所以拿不到 IP 的情形在这里就是"不认识的对端"。
        let ServerName::IpAddress(ip) = server_name else {
            warn!("对端使用了域名，本机只按 IP 校验指纹，拒绝");
            return Err(rejected());
        };
        let ip = StdIpAddr::from(*ip);
        let seen = fingerprint_of(end_entity.as_ref());

        let pinned = self.trust.get(ip);
        if pinned.is_empty() {
            // **未配对**的设备永远进不了 `pinned`（那张表只从凭据库来，见 `seed`），
            // 于是它的每一次握手都落在这一支。LocalSend 的信任表还是每个客户端一份
            // （`Trust::isolated`），"第一次"对它来说每拨一次都成立 —— 按 `info` 记就是
            // 每天上千条。重复同一枚证书不是新闻，"这个地址上的证书换了"才是。
            let before = self.trust.observed.lock().get(&ip).cloned();
            match before.as_deref() {
                None => debug!(
                    %ip,
                    seen = %short_fingerprint(&seen),
                    "首次见到该地址的证书，按 TOFU 认下并记住"
                ),
                Some(prev) if prev != seen.as_str() => info!(
                    %ip,
                    was = %short_fingerprint(prev),
                    seen = %short_fingerprint(&seen),
                    "该地址上的证书变了（未配对），按 TOFU 更新"
                ),
                Some(_) => debug!(
                    %ip,
                    seen = %short_fingerprint(&seen),
                    "这一跳仍是那枚没钉住的证书（这个地址上已经见过）"
                ),
            }
        } else if !self.trust.accepts(ip, &seen) {
            warn!(
                %ip,
                pinned = ?pinned.iter().map(|fp| short_fingerprint(fp)).collect::<Vec<_>>(),
                seen = %short_fingerprint(&seen),
                "对端证书指纹与钉住的不一致，拒绝连接"
            );
            return Err(rejected());
        }
        self.trust.observe(ip, seen);
        Ok(ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        // 签名本身还是要按标准验：指纹管"是不是它"，签名管"这份握手是不是它签的"。
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &provider().signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// 这条 TLS 连接上出示的客户端证书指纹（没有出示 = `None`）。
///
/// 由 `dispatch_conn` 在握手完成后塞进每个请求的 extensions：配对那一跳用它核对
/// 请求体里自报的指纹（见 `server::routes_pair::bound_peer_fingerprint`）。
///
/// **它不是身份认证**：证书是任意自签证书，本机只认"这条连接确实持有这把私钥"。
/// 真正的身份锚仍是凭据库里钉住的指纹；把自报指纹绑到连接上只保证"谁说的就是谁出示的"，
/// **不解决**配对那一刻有人在中间（两端各看到中间人的证书时，两边仍会各自钉住它 ——
/// 那需要配对码带外核对，见模块头）。
#[derive(Debug, Clone, Default)]
pub struct ClientCertFingerprint(pub Option<String>);

/// 接受任意客户端证书的校验器：本机不用证书链判断身份，只把证书交给应用层。
///
/// 自签证书没有 CA 可验，放行任意证书是刻意的；但握手签名仍按标准验 —— 证书是谁不重要，
/// "它确实持有这把私钥"很重要。`offer_client_auth` 打开、`client_auth_mandatory` 关掉：
/// 新客户端会带证书，老客户端不带也照常握手（配对那一跳再决定要不要信它）。
#[derive(Debug)]
struct AnyClientCert;

impl ClientCertVerifier for AnyClientCert {
    fn offer_client_auth(&self) -> bool {
        true
    }

    fn client_auth_mandatory(&self) -> bool {
        false
    }

    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        Ok(ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            cert,
            dss,
            &provider().signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            cert,
            dss,
            &provider().signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        provider()
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// 拨号方向（我们是对端）用的 TLS 配置。
///
/// **同时出示本机证书**：对端在配对那一跳据此看到本机真实的证书指纹，不必再相信
/// 请求体里自报的那一句。对端不请求客户端证书时（老版本）rustls 不会发，握手照旧。
pub fn client_config(trust: Arc<Trust>) -> Arc<rustls::ClientConfig> {
    let config = match load_or_create_identity() {
        Ok(identity) => {
            let builder = rustls::ClientConfig::builder_with_provider(provider())
                .with_safe_default_protocol_versions()
                .expect("ring provider 支持默认协议版本")
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(PinnedVerifier {
                    trust: trust.clone(),
                }));
            match builder.with_client_auth_cert(
                vec![identity.cert_der.clone()],
                identity.key_der.clone_key(),
            ) {
                Ok(config) => config,
                Err(e) => {
                    // 证书与私钥不配套（身份文件被手改过）：不带证书继续，配对那一跳会
                    // 因为"自报了指纹却拿不出证书"被拒，日志里能对上。
                    warn!(%e, "本机证书与私钥不配套，出站连接不带客户端证书");
                    client_config_without_cert(trust)
                }
            }
        }
        Err(e) => {
            warn!(%e, "本机身份读取失败，出站连接不带客户端证书");
            client_config_without_cert(trust)
        }
    };
    Arc::new(config)
}

/// 读不出身份时的退路：与改动前的客户端配置一字不差（不带客户端证书）。
fn client_config_without_cert(trust: Arc<Trust>) -> rustls::ClientConfig {
    rustls::ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .expect("ring provider 支持默认协议版本")
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedVerifier { trust }))
        .with_no_client_auth()
}

/// 监听方向（对端拨我们）用的 TLS 配置。
///
/// 请求客户端证书但不强制：新客户端带、老客户端不带。握手完成后
/// `dispatch_conn` 把出示的证书指纹塞进请求 extensions，配对路由据此核对自报值。
pub fn server_config(identity: &Identity) -> Arc<rustls::ServerConfig> {
    let config = rustls::ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .expect("ring provider 支持默认协议版本")
        .with_client_cert_verifier(Arc::new(AnyClientCert))
        .with_single_cert(
            vec![identity.cert_der.clone()],
            identity.key_der.clone_key(),
        )
        .expect("本机身份证书与私钥是配套生成的");
    Arc::new(config)
}

// ---------------------------------------------------------------------------
// 进程级单例
// ---------------------------------------------------------------------------

/// 校验器在**每一次 TLS 握手**里都要读它，所以必须是一份进程级的共享表。
static TRUST: LazyLock<Arc<Trust>> = LazyLock::new(|| Arc::new(Trust::new()));

static CLIENT: LazyLock<reqwest::Client> =
    LazyLock::new(|| client_builder().build().unwrap_or_default());

/// 进程启动时调用一次：把身份**当场**读出来/生成出来，失败要在启动日志里看得见，
/// 而不是等到某次拨号才暴露。
///
/// 身份本身不做进程级缓存：它只在启动时（`server::start_server`）与"给界面看本机指纹"
/// 这类少数场合需要，每次读一遍 PEM 是微秒级的事，比多一份可能失同步的全局状态划算。
pub fn init() -> Result<(), String> {
    let identity = load_or_create_identity()?;
    info!(
        fingerprint = %short_fingerprint(&identity.fingerprint),
        "本机证书指纹"
    );
    Ok(())
}

pub fn trust() -> Arc<Trust> {
    TRUST.clone()
}

/// 本机证书指纹。配对请求/回执带上它，双方可以在**配对完成时**就钉住对方，
/// 不必等下一次 TLS 连接再按 TOFU 认（那样会留下一个空指纹窗口）。
pub fn local_fingerprint() -> Result<String, String> {
    Ok(load_or_create_identity()?.fingerprint)
}

/// 流式响应一帧多大（`ReaderStream::with_capacity` 的容量）。
///
/// `ReaderStream::new` 用的是 tokio-util 的默认值 **4096**：每 4KB 就要过一次
/// `tokio::fs::File` 的阻塞任务派发（`spawn_blocking`），而且 rustls 的 TLS 记录被压到 4KB
/// —— 1GB 文件就是 26 万次派发 + 26 万条小记录。256KB 把这两项都除 64，且不会推迟首字节
/// 送达（读满多少发多少，不等填满）。
pub const STREAM_CHUNK: usize = 256 * 1024;

/// 拨号到对端用的 reqwest 客户端构造器（已经装好 pinning 校验器）。
///
/// 各处超时不同（探活 1.5s、配对邀请 75s、拖拽 30s），所以给的是 builder 而不是
/// 一个成品客户端；`client()` 是"没有特殊超时需求"时的那一个。
pub fn client_builder() -> reqwest::ClientBuilder {
    client_builder_with_trust(trust())
}

/// 用一份**指定的**信任表建 client（配对那一跳、以及按目标钉住的带 token 拨号）。
///
/// 配对要从"这一次握手实际看到的证书"算出 AAD，而进程级 `Trust` 的 `observed`
/// 是"每个地址一格"——并发的探活/别的配对会把它覆盖掉。给配对一把只属于这次流程的
/// 空表（`Trust::new()`）：TOFU 照常接受，`observed_at` 只记录这条 client 看到的证书，
/// 互不打架。带 token 的拨号则用 [`Trust::scoped`]：只钉目标那一枚。
pub fn client_builder_with_trust(trust: Arc<Trust>) -> reqwest::ClientBuilder {
    install_crypto_provider();
    reqwest::Client::builder()
        .use_preconfigured_tls((*client_config(trust)).clone())
        // 数据面全是小写入（256KB 帧、4MB 块），默认的 Nagle 会和延迟 ACK 互相等：
        // 关掉它，写多少发多少。对端那一侧另有 `set_nodelay`（见 `dispatch_conn`）。
        .tcp_nodelay(true)
}

pub fn client() -> reqwest::Client {
    CLIENT.clone()
}

/// `/pair/info` 回话的体积上限：正常只有 id/name/port 几百字节。
pub const PAIR_INFO_BODY_MAX: usize = 64 * 1024;

/// 控制面回话（`/discover`、`/relay/open`、路由回执）的体积上限：形状是设备列表，
/// 正常只有几 KB；1MB 已能装下几千台设备。
pub const CONTROL_BODY_MAX: usize = 1024 * 1024;

/// 在上限内读完一个响应体。
///
/// **别用 `Response::bytes()` / `.json()`**：它们先把整个身体读进内存再解析，而对端与
/// 镜像都是不可信的——对方只要不停写（或者不给 `Content-Length`、一点点地滴），进程就会
/// 被撑爆。这里先看声明值尽早拒绝，再按块累计，越过上限立刻放弃（声明值可以是假的）。
///
/// 调用方负责总超时：本函数只保证内存有界，不保证时间有界。
pub async fn read_body_capped(
    mut resp: reqwest::Response,
    max: usize,
) -> Result<Vec<u8>, String> {
    if let Some(len) = resp.content_length() {
        if len > max as u64 {
            return Err(format!("响应太大（{len} 字节，上限 {max} 字节）"));
        }
    }
    let hint = resp.content_length().unwrap_or(0).min(max as u64) as usize;
    let mut body = Vec::with_capacity(hint);
    while let Some(chunk) = resp
        .chunk()
        .await
        .map_err(|e| format!("读响应失败：{e}"))?
    {
        if body.len().saturating_add(chunk.len()) > max {
            return Err(format!("响应超过上限（{max} 字节）"));
        }
        body.extend_from_slice(&chunk);
    }
    Ok(body)
}

/// [`read_body_capped`] + 文本（对端拒绝时的错误正文）：正文只用于显示原因，不能被
/// 对端用一个无限长的字符串撑爆内存。
pub async fn read_text_capped(resp: reqwest::Response, max: usize) -> Result<String, String> {
    let body = read_body_capped(resp, max).await?;
    Ok(String::from_utf8_lossy(&body).to_string())
}

/// [`read_body_capped`] + JSON 解析（`/pair/info` 这类对端回话）。
pub async fn read_json_capped<T: serde::de::DeserializeOwned>(
    resp: reqwest::Response,
    max: usize,
) -> Result<T, String> {
    let body = read_body_capped(resp, max).await?;
    serde_json::from_slice(&body).map_err(|e| format!("响应不是合法 JSON：{e}"))
}

/// 按 `(地址, 期望指纹, 超时)` 缓存的、**按目标钉住**的客户端。
///
/// 缓存而不是每次新建：探活是 5s 一拍、镜像水合是每个区间一次 —— 每次都建客户端等于
/// 每次重新握手（真机实测每段多几十毫秒）。键里带指纹，所以凭据一变自然换一把 client，
/// 旧连接池不会被新的期望复用。
static DIAL_CLIENTS: LazyLock<Mutex<HashMap<(String, String, u64, u64, u64), reqwest::Client>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// 缓存上限：正常机器上一台设备一条（地址/指纹变了会多留一条旧键）。满了整表清掉 ——
/// 重建一次握手而已，比维护 LRU 简单。
const DIAL_CLIENT_CACHE_MAX: usize = 128;

/// [`DialPin`] 在客户端缓存键里的写法。
fn pin_cache_key(pin: &DialPin) -> String {
    match pin {
        DialPin::Fingerprint(fp) => format!("fp:{fp}"),
        DialPin::Tofu => "tofu".to_string(),
        DialPin::Deny => "deny".to_string(),
    }
}

/// 到**这一台目标设备**的客户端（见 [`DialPin`] 与 [`Trust::scoped`]）。
///
/// 所有**带 token** 的对端拨号都必须用它，不能复用全局 [`client`]：全局表的键只有 IP，
/// 同一地址上有另一台已配对设备的指纹时，那张证书也会被放行 —— 目标设备的 token 就
/// 发给了它。这里按 `host + 目标指纹` 建表，那个地址上只认目标这一枚。
///
/// 不设默认超时：调用点一律在请求上写自己的 `.timeout(…)`（探活 1.5s、邀请 75s、
/// 中转控制面 6s…）；需要默认超时的用 [`dial_client_timeout`]。
pub fn dial_client(host: &str, pin: &DialPin) -> reqwest::Client {
    dial_client_inner(host, pin, None, None, None)
}

/// [`dial_client`] 的带默认总超时版本（同一套缓存，超时参与键）。
pub fn dial_client_timeout(host: &str, pin: &DialPin, timeout: Duration) -> reqwest::Client {
    dial_client_inner(host, pin, None, Some(timeout), None)
}

/// [`dial_client`] 的"连接 + 总时长"版本（文件的元数据/分块请求要短连接超时、宽总超时）。
pub fn dial_client_with_timeouts(
    host: &str,
    pin: &DialPin,
    connect_timeout: Duration,
    timeout: Duration,
) -> reqwest::Client {
    dial_client_inner(host, pin, Some(connect_timeout), Some(timeout), None)
}

/// 流式下载用的客户端：**不设总时长**（一个几十 GB 的传输本来就要跑很久），但给底层
/// socket 每次读加一个空闲上限。对端收下请求后不再发字节时，`req.send()` / 响应流读取
/// 会在 `read_timeout` 后失败，而不是永久挂住 worker（P1：接收数据面无限等待）。
pub fn dial_client_streaming(
    host: &str,
    pin: &DialPin,
    connect_timeout: Duration,
    read_timeout: Duration,
) -> reqwest::Client {
    dial_client_inner(
        host,
        pin,
        Some(connect_timeout),
        None,
        Some(read_timeout),
    )
}

fn dial_client_inner(
    host: &str,
    pin: &DialPin,
    connect_timeout: Option<Duration>,
    timeout: Option<Duration>,
    read_timeout: Option<Duration>,
) -> reqwest::Client {
    // 回环沿用既有豁免：本机测试 / 同机多实例没有"局域网地址被投毒"这一层。
    let pin = if parse_host(host).is_some_and(|ip| ip.is_loopback()) {
        DialPin::Tofu
    } else {
        pin.clone()
    };
    let pin_key = pin_cache_key(&pin);
    let key = (
        host.to_string(),
        pin_key,
        connect_timeout.map(|t| t.as_millis() as u64).unwrap_or(0),
        timeout.map(|t| t.as_millis() as u64).unwrap_or(0),
        read_timeout.map(|t| t.as_millis() as u64).unwrap_or(0),
    );
    if let Some(client) = DIAL_CLIENTS.lock().get(&key) {
        return client.clone();
    }
    let mut builder = client_builder_with_trust(Trust::scoped(host, &pin));
    if let Some(connect_timeout) = connect_timeout {
        builder = builder.connect_timeout(connect_timeout);
    }
    if let Some(timeout) = timeout {
        builder = builder.timeout(timeout);
    }
    if let Some(read_timeout) = read_timeout {
        builder = builder.read_timeout(read_timeout);
    }
    let client = match builder.build() {
        Ok(client) => client,
        Err(e) => {
            // 退回默认客户端（只信公共根证书）：自签对端本来就连不上，最多是这一跳失败；
            // 绝不退回"不校验"，那等于把 token 发给任何出示证书的一方。
            warn!(%host, "按目标钉住的 HTTP 客户端建不起来: {e}");
            reqwest::Client::new()
        }
    };
    let mut clients = DIAL_CLIENTS.lock();
    if clients.len() >= DIAL_CLIENT_CACHE_MAX {
        clients.clear();
    }
    clients.insert(key, client.clone());
    client
}

/// 按 `(地址, 端口, 期望指纹)` 缓存的 **LocalSend 专用**客户端，**不带** OrbDrop 的观察值。
///
/// 与 [`dial_client`] 的两点不同（都见 [`Trust::isolated`]）：
/// - 握手见到的证书不写进程级 `observed` —— 同一地址上的 OrbDrop 设备有自己的一格；
/// - 回环也照钉（LocalSend 对端没有配对关系）。
///
/// **端口进缓存键**：LocalSend 的身份是 `ip:port`（同一台机器可以开多份实例），而
/// `Trust` 的观察值按 IP 记 —— 每份实例一张独立的临时表，读到的才是**这个端点**的
/// 证书（共表会让后一次握手覆盖前一个端点的记录）。
///
/// 返回值带上这次拨号用的信任表：握手成功后调用方从 [`Trust::observed_at`] 读到对端
/// **真实证书**的指纹。第三方实现的自报值不一定是证书 SHA-256（真有填设备 UUID 的），
/// 那一档无法按公告钉，只能以握手证书为准。
pub fn dial_client_isolated(
    host: &str,
    port: u16,
    pin: &DialPin,
) -> Result<(reqwest::Client, Arc<Trust>), String> {
    let key = (host.to_string(), port, pin_cache_key(pin));
    if let Some((client, trust)) = ISOLATED_CLIENTS.lock().get(&key) {
        return Ok((client.clone(), trust.clone()));
    }
    let trust = Trust::isolated(host, pin);
    let client = client_builder_with_trust(trust.clone())
        .build()
        .map_err(|e| format!("建 HTTPS 客户端失败：{e}"))?;
    let mut clients = ISOLATED_CLIENTS.lock();
    if clients.len() >= DIAL_CLIENT_CACHE_MAX {
        clients.clear();
    }
    clients.insert(key, (client.clone(), trust.clone()));
    Ok((client, trust))
}

/// [`dial_client_isolated`] 的缓存：键是「地址 + 端口 + 期望指纹」，超时写在请求上。
static ISOLATED_CLIENTS: LazyLock<
    Mutex<HashMap<(String, u16, String), (reqwest::Client, Arc<Trust>)>>,
> = LazyLock::new(|| Mutex::new(HashMap::new()));

/// LocalSend `/upload` 的数据面客户端：同 [`dial_client_streaming`] 的超时组合，但信任表
/// 用 [`Trust::isolated`] —— 上传那一跳见到的证书同样不能写进 OrbDrop 的观察值。
///
/// 按批建一次（每条发送流水线复用同一条），不做缓存：上传本来就要占住一条连接，
/// 复用客户端省下的那次握手可以忽略。
pub fn dial_client_streaming_isolated(
    host: &str,
    pin: &DialPin,
    connect_timeout: Duration,
    read_timeout: Duration,
) -> Result<reqwest::Client, String> {
    client_builder_with_trust(Trust::isolated(host, pin))
        .connect_timeout(connect_timeout)
        .read_timeout(read_timeout)
        .build()
        .map_err(|e| format!("建 HTTPS 客户端失败：{e}"))
}

/// 对端的入口：`https://ip:port`。
///
/// **唯一的拼法**：从明文改成 TLS 不只是换个字面量 —— 一个漏掉的 `http://` 会让那条路
/// 静默变成明文（服务端会把局域网上来的明文连接丢掉，表现为"这一步连不上"）。所以
/// 全仓库的对端地址都从这一个函数出。
pub fn peer_base(ip: &str, port: u16) -> String {
    format!("https://{ip}:{port}")
}

/// 对端 WS 入口（`/events` 的完整 URL，带 token）。
pub fn peer_ws_url(ip: &str, port: u16, token: &str, manual: bool) -> String {
    format!(
        "wss://{ip}:{port}/events?token={token}{}",
        if manual { "&manual=1" } else { "" }
    )
}

/// 地址字面量 → `IpAddr`，**容忍 URL 主机里的方括号写法**（`[::ffff:10.0.0.1]`）。
///
/// 中转端点的字面量是 IPv4 映射的 IPv6（见 [`relay_host`]）：拼进 URL 必须带方括号
/// （RFC 3986），而指纹表要拿 `IpAddr` 当键 —— 两处必须认同一个写法，否则钉不住。
pub fn parse_host(host: &str) -> Option<StdIpAddr> {
    let trimmed = host.trim();
    let inner = trimmed
        .strip_prefix('[')
        .and_then(|h| h.strip_suffix(']'))
        .unwrap_or(trimmed);
    inner.parse().ok()
}

/// 中转端点的主机字面量：**同一个 IPv4，写成 IPv4 映射的 IPv6**（`[::ffff:a.b.c.d]`）。
///
/// 为什么要换字面量（F-106）：中转端点上呈现的是**被中转那台设备**的证书，而指纹表
/// （`net::Trust`）按**地址**索引 —— 直接用中转方的 IPv4，就会与"中转方自己的服务"
/// 共用同一个键位：同一地址上出现两枚不同指纹时按现有规则**不钉**（退到 TOFU，见
/// `Trust::seed`），等于把中转方和自己的两跳身份都放松了。换一种字面量（同一个地址）
/// 就让中转端点各占一档键位，TLS 端到端不变、信任模型一个字都不用改。
///
/// 传输层也认：IPv4 映射地址的 `connect()` 会落到同一个 IPv4（Windows/macOS/Android 实测）。
pub fn relay_host(ipv4: &str) -> Option<String> {
    let v4: std::net::Ipv4Addr = ipv4.trim().parse().ok()?;
    Some(format!("[{}]", v4.to_ipv6_mapped()))
}

/// 观察到的一对「TCP 来源地址 + 对端自报的服务端口」能不能当**对端的拨号地址**记下来。
/// `None` = 那对地址指的其实是本机自己，记下来只会一路错下去（见 `is_own_endpoint`）。
pub fn observed_dial_target(
    source: StdIpAddr,
    reported_port: u16,
    listening: u16,
) -> Option<(String, u16)> {
    if is_own_endpoint(
        source,
        reported_port,
        listening,
        &crate::server::interface_addresses(),
    ) {
        return None;
    }
    Some((source.to_string(), reported_port))
}

/// 这个「来源地址 + 对端自报端口」指的其实是本机自己在听的那个端点吗？
///
/// 反例是安卓模拟器（以及任何把自己看到的本机地址回连进来的对端）：模拟器把宿主回环
/// 别名 `10.0.2.2` 拍成宿主的 `127.0.0.1`，于是**本机**看到的来源地址就是本机自己，
/// 配上对端自报的端口（默认同样是 3890）正好是本机的监听端点。把它记进地址簿是一连串
/// 错误的起点：探活问回自己、把**本机的**证书指纹钉到对端名下、最后那一行被当成
/// "这个地址上换了设备"作废 —— 而它此后再没有任何地址可问，设备行就永远停在「离线」，
/// 尽管对方的长连接一直好好地在。
///
/// 判据必须带上端口：同机多开（`ORBDROP_ALLOW_MULTI`）时 `127.0.0.1:<另一个端口>` 是
/// **合法**的对端地址 —— 只有"地址是本机的、端口也正好是本机在听的那个"才是自己。
///
/// 拆成"判据 + 取本机地址"两步是为了能单测：真实网卡列表在测试里没法摆。
fn is_own_endpoint(
    source: StdIpAddr,
    reported_port: u16,
    listening: u16,
    local: &[StdIpAddr],
) -> bool {
    reported_port != 0
        && reported_port == listening
        && (source.is_loopback() || local.contains(&source))
}

// ---------------------------------------------------------------------------
// 监听：一个端口上分派 TLS / 明文
// ---------------------------------------------------------------------------

/// 接受连接，逐个分派。`router` 两条路共用（明文那条只可能来自回环）。
pub async fn serve(
    listener: TcpListener,
    router: Router,
    tls: Arc<rustls::ServerConfig>,
    shutdown: impl Future<Output = ()> + Send + 'static,
) {
    let mut shutdown = std::pin::pin!(shutdown);
    // 同时驻留的连接上限（[`MAX_CONNECTIONS`]）：每个监听循环一份。重启（`stop_server` +
    // `start_server`）会换一份新的，而上一代已经在跑的任务本来就不归这里管 —— 两代合起来
    // 也仍是 2×N 的有界值，不是无界。
    let remote_slots = Arc::new(Semaphore::new(MAX_CONNECTIONS - LOOPBACK_CONNECTIONS));
    let loopback_slots = Arc::new(Semaphore::new(LOOPBACK_CONNECTIONS));
    // 每个远程来源单独记账：先过这道闸门，再过全局名额。反过来的话，一台机器占满
    // 448 个名额之后，别的设备连"被按 IP 记账"这一步都走不到。
    let per_ip_slots = Arc::new(IpConnLimit::new(MAX_CONNECTIONS_PER_REMOTE_IP));
    loop {
        let accepted = tokio::select! {
            _ = &mut shutdown => {
                info!("监听循环退出");
                return;
            }
            accepted = listener.accept() => accepted,
        };
        let (stream, addr) = match accepted {
            Ok(v) => v,
            Err(e) => {
                warn!(%e, "accept 失败");
                continue;
            }
        };
        // 先过"单 IP"这道闸门：回环不记（UI/BC 单独有名额），远程一台主机最多
        // `MAX_CONNECTIONS_PER_REMOTE_IP` 条。到顶就把这条连接丢掉（`stream` 在这里
        // drop = 关掉它），不排队。
        let ip_guard = if addr.ip().is_loopback() {
            None
        } else {
            match per_ip_slots.try_acquire(addr.ip()) {
                Some(guard) => Some(guard),
                None => {
                    debug!(
                        %addr,
                        limit = MAX_CONNECTIONS_PER_REMOTE_IP,
                        "该来源的并发连接已达上限，丢弃这条新连接"
                    );
                    continue;
                }
            }
        };

        // 名额拿到才建任务；拿不到就把这条连接丢掉（`stream` 在这里 drop = 关掉它）。
        let slots = if addr.ip().is_loopback() {
            &loopback_slots
        } else {
            &remote_slots
        };
        let Some(permit) = acquire_slot(slots) else {
            // 用 debug 而不是 warn：上限被打满时这里是**每连接一条**日志，而这条日志要防的
            // 正是"无界增长"—— 攻击期间别把内存的账挪到磁盘上。排障要看它时把日志级别调下来。
            debug!(%addr, limit = MAX_CONNECTIONS, "并发连接已达上限，丢弃这条新连接");
            continue;
        };
        let router = router.clone();
        let tls = tls.clone();
        tokio::spawn(async move {
            // 名额跟着这条连接走到头：任务收尾（正常/出错/握手超时）就自动还回去。
            let _permit = permit;
            let _ip_guard = ip_guard;
            if let Err(e) = dispatch_conn(stream, addr, router, tls).await {
                debug!(%addr, %e, "连接结束");
            }
        });
    }
}

/// 取一个连接名额：拿到 = 可以服务，`None` = 已满，**调用方丢弃这条连接**（不排队）。
///
/// 单独成函数是为了能被单测直接钉住"只判能不能，不等"这个语义（见 `net.rs` 的测试）。
fn acquire_slot(slots: &Arc<Semaphore>) -> Option<OwnedSemaphorePermit> {
    slots.clone().try_acquire_owned().ok()
}

/// 按来源 IP 记账的并发连接数（远程）。到顶同样**不排队**：丢新连接，名额跟着
/// [`IpConnGuard`] 走到连接收尾自动还回去。
///
/// 单独一层是因为全局名额与这里防的不是一回事：一个防总量无界，一个防单点占满。
#[derive(Debug)]
struct IpConnLimit {
    limit: usize,
    counts: Mutex<HashMap<StdIpAddr, usize>>,
}

impl IpConnLimit {
    fn new(limit: usize) -> Self {
        Self {
            limit,
            counts: Mutex::new(HashMap::new()),
        }
    }

    /// 拿到 = 这条连接可以继续；`None` = 该 IP 已到上限，调用方丢弃这条连接。
    fn try_acquire(self: &Arc<Self>, ip: StdIpAddr) -> Option<IpConnGuard> {
        let mut counts = self.counts.lock();
        let count = counts.entry(ip).or_insert(0);
        if *count >= self.limit {
            return None;
        }
        *count += 1;
        Some(IpConnGuard {
            ip,
            slots: Arc::clone(self),
        })
    }
}

/// 一条远程连接占着的 IP 名额。drop = 还账；计数归零时顺手删表项，表不随历史 IP 长大。
struct IpConnGuard {
    ip: StdIpAddr,
    slots: Arc<IpConnLimit>,
}

impl Drop for IpConnGuard {
    fn drop(&mut self) {
        let mut counts = self.slots.counts.lock();
        if let Some(count) = counts.get_mut(&self.ip) {
            *count -= 1;
            if *count == 0 {
                counts.remove(&self.ip);
            }
        }
    }
}

async fn dispatch_conn(
    stream: TcpStream,
    addr: SocketAddr,
    router: Router,
    tls: Arc<rustls::ServerConfig>,
) -> Result<(), String> {
    let mut head = [0u8; 1];
    let read = tokio::time::timeout(PEEK_TIMEOUT, stream.peek(&mut head))
        .await
        .map_err(|_| "等待首字节超时".to_string())?
        .map_err(|e| e.to_string())?;
    if read == 0 {
        return Ok(()); // 对面连上就关了
    }

    // 一进来就关 Nagle：这条连接上跑的是文件字节（256KB 帧）与 WebDAV 的小请求，两者都不
    // 需要"攒一攒再发"，而 Nagle 撞上对端延迟 ACK 会平白多出几十毫秒的空等。
    let _ = stream.set_nodelay(true);

    if head[0] == TLS_HANDSHAKE {
        let acceptor = tokio_rustls::TlsAcceptor::from(tls);
        // 超时就把这条连接丢掉（`accept` 的 future 连同 `stream` 一起 drop = 关掉它）：
        // 半途而废的握手不会有下文，钉在这里的只有本机的任务与 TLS 缓冲（见
        // `TLS_HANDSHAKE_TIMEOUT`）。正常握手是亚毫秒级，这个时限在正常路径上不可达。
        let stream = tokio::time::timeout(TLS_HANDSHAKE_TIMEOUT, acceptor.accept(stream))
            .await
            .map_err(|_| "TLS 握手超时".to_string())?
            .map_err(|e| e.to_string())?;
        // 握手已经完成：把对端出示的客户端证书（如果有）摘出来，交给请求处理层。
        // 新客户端一定会带（`client_config` 里配了 `with_client_auth_cert`）；老版本不带。
        let client_cert = stream
            .get_ref()
            .1
            .peer_certificates()
            .and_then(|certs| certs.first())
            .map(|cert| ClientCertFingerprint(Some(fingerprint_of(cert.as_ref()))))
            .unwrap_or_default();
        return serve_http(stream, addr, router, client_cert).await;
    }

    // 明文只服务回环：本机 UI 的自验路径、以及 Beyond Compare 那条 WebDAV 入口
    // （`server::proxy` 把它转成到对端的加密连接）。局域网上来的明文连接一律丢 ——
    // 否则"加密"就是可选的，谁都能退回去用明文。
    if !addr.ip().is_loopback() {
        warn!(%addr, "非回环地址以明文 HTTP 连接，已拒绝");
        return Ok(());
    }
    serve_http(stream, addr, router, ClientCertFingerprint(None)).await
}

async fn serve_http<I>(
    io: I,
    addr: SocketAddr,
    router: Router,
    client_cert: ClientCertFingerprint,
) -> Result<(), String>
where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let service = hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
        let mut router = router.clone();
        let client_cert = client_cert.clone();
        async move {
            let (mut parts, body) = req.into_parts();
            // `ConnectInfo` 是 `auth_middleware` 认"回环 = 本机"的依据，也是代理路由
            // 挡非回环的依据 —— 手写 accept 循环就得自己把它塞回去（`axum::serve` 是
            // 靠 `into_make_service_with_connect_info` 做的同一件事）。
            parts.extensions.insert(axum::extract::ConnectInfo(addr));
            // 配对路由要核对"请求体里自报的指纹"是不是这条连接出示的那一枚。
            parts.extensions.insert(client_cert);
            let req = hyper::Request::from_parts(parts, axum::body::Body::new(body));
            router.call(req).await
        }
    });

    hyper::server::conn::http1::Builder::new()
        // 未认证连接可以先建 TCP 再慢慢发（或不发）请求头。没有这一条，512 个空闲
        // 连接就能把整个监听占死；15 秒足够正常的慢网络完成请求头。
        .timer(hyper_util::rt::TokioTimer::new())
        .header_read_timeout(Duration::from_secs(15))
        .serve_connection(hyper_util::rt::TokioIo::new(io), service)
        .with_upgrades()
        .await
        .map_err(|e| e.to_string())
}

// ---------------------------------------------------------------------------
// 到对端的 WS 拨号
// ---------------------------------------------------------------------------

/// 拨号失败的三类结果。**"身份不对"必须与"现在连不上"分开**：前者重试一万次也是同一个
/// 结果，后者退避重试才对 —— 混成一条字符串，上层就只能靠猜。
#[derive(Debug)]
pub enum DialError {
    /// 对端证书指纹与本机钉住的不一致：对方换了机器/重装过，或者有人在中间。
    /// 唯一出路是重新配对（那会重新钉一次指纹）。
    IdentityRejected,
    /// 连不上、超时、TLS 握手失败之类：现在不行，等下次。
    Unreachable(String),
    /// WS 握手本身的失败（最要紧的是 401/403 —— 对端明确拒绝）。
    Ws(tokio_tungstenite::tungstenite::Error),
}

impl std::fmt::Display for DialError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DialError::IdentityRejected => write!(f, "对端证书指纹与钉住的不一致"),
            DialError::Unreachable(msg) => write!(f, "{msg}"),
            DialError::Ws(err) => write!(f, "{err}"),
        }
    }
}

/// 这条错误的根因是不是"对端身份不是本机认的那一个"。
///
/// 顺着 source 链找 `rustls::Error`：reqwest / tokio-rustls 都会把它包上一层（甚至两层）
/// io/超时错误，类型化判断只能这么找回来。判据是我们自己那个校验器返回的
/// `ApplicationVerificationFailure`（见 `PinnedVerifier`）。
pub fn is_identity_rejected(err: &(dyn std::error::Error + 'static)) -> bool {
    fn visit(err: &(dyn std::error::Error + 'static)) -> bool {
        if let Some(rustls_err) = err.downcast_ref::<rustls::Error>() {
            return matches!(
                rustls_err,
                rustls::Error::InvalidCertificate(
                    rustls::CertificateError::ApplicationVerificationFailure
                )
            );
        }
        // `io::Error` 的 `source()` 对自定义错误返回的是**它包着那个错误的 source**，
        // 不是它本身 —— reqwest/hyper 把 rustls 的错误包在 `io::Error::new(kind, rustls)`
        // 里时，`source()` 链会静默跳过 rustls。`get_ref()` 能把它捞回来。
        if let Some(io) = err.downcast_ref::<std::io::Error>() {
            if let Some(inner) = io.get_ref() {
                if visit(inner) {
                    return true;
                }
            }
        }
        err.source().is_some_and(visit)
    }
    visit(err)
}

/// 拨一条到对端的加密 WS，并把 URL 里的 `wss://` 兑现成真的 TLS 连接。
///
/// 不用 `tokio-tungstenite` 自带的 TLS feature：那会把"信任什么"交给它的 feature 组合，
/// 而这里的信任决策必须是我们自己那份 pinning 校验器。所以先自己握手，再把**已经加密
/// 的流**交给 `client_async`。
pub async fn connect_ws(
    url: &str,
    ip: &str,
    port: u16,
    pin: &DialPin,
) -> Result<
    tokio_tungstenite::WebSocketStream<tokio_rustls::client::TlsStream<tokio::net::TcpStream>>,
    DialError,
> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    let request = url
        .into_client_request()
        .map_err(|e| DialError::Unreachable(format!("WS 地址无效: {e}")))?;
    let host = ip;
    let ip: StdIpAddr =
        parse_host(host).ok_or_else(|| DialError::Unreachable(format!("地址无效: {host}")))?;

    let tcp = TcpStream::connect(SocketAddr::new(ip, port))
        .await
        .map_err(|e| DialError::Unreachable(e.to_string()))?;
    // 带 token 的链路必须按目标建表：全局表在共享地址上会接受别的设备的证书。
    let connector = tokio_rustls::TlsConnector::from(client_config(Trust::scoped(host, pin)));
    let tls = connector
        .connect(ServerName::IpAddress(ip.into()), tcp)
        .await
        .map_err(|e| {
            if is_identity_rejected(&e) {
                DialError::IdentityRejected
            } else {
                DialError::Unreachable(e.to_string())
            }
        })?;

    let (stream, _resp) = tokio_tungstenite::client_async(request, tls)
        .await
        .map_err(DialError::Ws)?;
    Ok(stream)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_is_stable_and_short_form_is_readable() {
        let a = fingerprint_of(b"hello");
        assert_eq!(a.len(), 64);
        assert_eq!(a, fingerprint_of(b"hello"));
        assert_ne!(a, fingerprint_of(b"hellp"));
        assert_eq!(
            short_fingerprint("0123456789abcdef0123"),
            "0123-4567-89AB-CDEF"
        );
    }

    /// 同一地址两枚不同指纹：**一枚都不钉**（挑一枚就会把另一台永远挡在门外）；
    /// 只有一枚时照旧钉住。
    #[test]
    fn seed_keeps_all_fingerprints_for_a_shared_relay_address() {
        let trust = Trust::new();
        trust.seed([
            ("10.0.0.9".to_string(), "aa".to_string()),
            ("10.0.0.9".to_string(), "bb".to_string()),
            ("10.0.0.8".to_string(), "cc".to_string()),
        ]);
        let shared = "10.0.0.9".parse().unwrap();
        assert!(
            trust.accepts(shared, "aa"),
            "中转地址上两枚目标指纹都要保留"
        );
        assert!(trust.accepts(shared, "bb"));
        assert!(!trust.accepts(shared, "cc"));
        assert!(trust.accepts("10.0.0.8".parse().unwrap(), "cc"));
        // 只剩一台目标时，旧的那枚不应再被接受。
        trust.seed([("10.0.0.9".to_string(), "bb".to_string())]);
        assert!(!trust.accepts(shared, "aa"));
        assert!(trust.accepts(shared, "bb"));
    }

    /// 共享地址上两枚指纹：**按目标建的临时表只认它自己那一枚**。
    ///
    /// 这是"带 token 的拨号不能复用全局表"的核心：全局表任一命中即放行，于是同一 IP
    /// 上另一台已配对设备出示自己的证书也能过校验，随后目标设备的 token 就发给了它。
    #[test]
    fn scoped_trust_rejects_the_other_device_on_a_shared_address() {
        let ip = "10.0.0.9".parse().unwrap();
        let scoped = Trust::scoped("10.0.0.9", &DialPin::Fingerprint("aa".to_string()));
        assert!(scoped.accepts(ip, "aa"), "目标自己的证书必须放行");
        assert!(
            !scoped.accepts(ip, "bb"),
            "同一地址上另一台设备的证书必须拒绝 —— 否则 token 会发给它"
        );

        // 已配对但没有指纹：哨兵，任何证书都过不了。
        let denied = Trust::scoped("10.0.0.9", &DialPin::Deny);
        assert!(!denied.accepts(ip, "aa"));
        assert!(!denied.accepts(ip, "bb"));

        // 未配对：空表 = TOFU（沿用既有语义；`accepts` 只回答"是不是钉住的那枚"，
        // 空表由校验器走 `pinned.is_empty()` 那一支放行）。
        let tofu = Trust::scoped("10.0.0.9", &DialPin::Tofu);
        assert!(tofu.get(ip).is_empty());
    }

    /// reqwest/hyper 把 rustls 的错误包进两层 `io::Error`，而 `io::Error::source()` 会跳过
    /// 自定义错误的**内层本体**。这条链必须还能认出来，否则界面会把"指纹不符"画成"离线"。
    #[test]
    fn identity_rejection_is_found_through_nested_io_errors() {
        let rejected = rustls::Error::InvalidCertificate(
            rustls::CertificateError::ApplicationVerificationFailure,
        );
        let outer = std::io::Error::new(
            std::io::ErrorKind::Other,
            std::io::Error::new(std::io::ErrorKind::InvalidData, rejected),
        );
        assert!(is_identity_rejected(&outer));

        let other = std::io::Error::new(
            std::io::ErrorKind::Other,
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                rustls::Error::InvalidCertificate(rustls::CertificateError::Expired),
            ),
        );
        assert!(
            !is_identity_rejected(&other),
            "别把过期/其它证书错误当成身份不符"
        );
    }

    /// 回环豁免不能被"按目标钉住"顺手收紧：本机测试 / 同机多实例一直靠它。
    #[test]
    fn scoped_trust_keeps_the_loopback_exemption() {
        let ip = "127.0.0.1".parse().unwrap();
        let scoped = Trust::scoped("127.0.0.1", &DialPin::Deny);
        assert!(
            scoped.get(ip).is_empty(),
            "回环不钉指纹：空表让校验器走 TOFU 那一支"
        );
    }

    /// LocalSend 的信任表与 OrbDrop 的观察值**各记各的**：同一地址上既可能是一台 OrbDrop
    /// 设备，也可能是一台第三方 LocalSend；把后者的证书写进前者那格，会让
    /// `remember_peer_fingerprint` 把错的身份锚记到 OrbDrop 设备名下。
    #[test]
    fn isolated_trust_does_not_write_the_shared_observed_map() {
        let host = "203.0.113.254";
        let ip: StdIpAddr = host.parse().unwrap();
        let isolated = Trust::isolated(host, &DialPin::Tofu);
        isolated.observe(ip, "aa".repeat(32));
        assert!(
            isolated.observed_at(host).is_some(),
            "这次拨号自己的观察值要留着给调用方读"
        );
        assert!(
            trust().observed_at(host).is_none(),
            "LocalSend 的握手不能污染 OrbDrop 的进程级观察值"
        );
        // 回环也照钉：LocalSend 对端没有配对关系，回环上的另一个实例同样是陌生人。
        let loopback = Trust::isolated("127.0.0.1", &DialPin::Deny);
        assert_eq!(
            loopback.get("127.0.0.1".parse().unwrap()),
            vec![DENY_FINGERPRINT.to_string()]
        );
    }

    #[test]
    fn generated_identity_round_trips_through_pem() {
        let id = generate_identity().unwrap();
        let pem = format!(
            "{}{}",
            to_pem(id.cert_der.as_ref(), "CERTIFICATE"),
            to_pem(id.key_der.secret_der(), "PRIVATE KEY")
        );
        let again = identity_from_pem(&pem).unwrap();
        assert_eq!(id.fingerprint, again.fingerprint);
    }

    #[test]
    fn identity_file_is_reused_not_regenerated() {
        let first = load_or_create_identity().unwrap();
        let second = load_or_create_identity().unwrap();
        assert_eq!(
            first.fingerprint, second.fingerprint,
            "身份文件在就不能重新生成：换一把等于所有对端都要重新配对"
        );
        let _ = std::fs::remove_file(identity_path());
    }

    /// 空指纹的已配对记录用哨兵占位：任何真实证书都不匹配，出站 TLS 直接被拒。
    #[test]
    fn deny_fingerprint_rejects_every_real_certificate() {
        let trust = Trust::new();
        let ip: StdIpAddr = "10.0.0.9".parse().unwrap();
        trust.seed([(ip.to_string(), DENY_FINGERPRINT.to_string())]);
        assert!(!trust.accepts(ip, "aa"), "哨兵不能匹配任何真实指纹");
        assert_eq!(trust.get(ip), vec![DENY_FINGERPRINT.to_string()]);
    }

    #[test]
    fn trust_only_matches_the_pinned_fingerprint() {
        let trust = Trust::new();
        trust.seed([("10.0.0.5".to_string(), "aa".to_string())]);
        let ip: StdIpAddr = "10.0.0.5".parse().unwrap();
        assert!(trust.accepts(ip, "aa"));
        assert!(!trust.accepts(ip, "bb"));
        assert!(trust.get("10.0.0.6".parse().unwrap()).is_empty());
        trust.unpin(ip);
        assert!(trust.get(ip).is_empty());
    }

    /// 来源地址什么时候**不能**当对端的拨号地址：它指的就是本机在听的那个端点。
    ///
    /// 判据里的端口是全部要害：同机多开时 `127.0.0.1:<另一个端口>` 是合法对端（联调就是
    /// 这么用的），一刀切按"回环就不是对端"会把这条路砍掉。
    #[test]
    fn own_endpoint_needs_the_port_and_a_local_address() {
        let lo: StdIpAddr = "127.0.0.1".parse().unwrap();
        let lan: StdIpAddr = "192.168.1.5".parse().unwrap();
        let other: StdIpAddr = "10.0.0.7".parse().unwrap();
        let local = [lan];

        // 模拟器那条路：回环 + 本机端口 = 自己。
        assert!(is_own_endpoint(lo, 3890, 3890, &local));
        // 同机多开：回环 + 别的端口 = 另一份实例，地址可用。
        assert!(!is_own_endpoint(lo, 3991, 3890, &local));
        // 本机网卡地址 + 本机端口 = 自己（NAT 把来源改写成宿主自己的地址）。
        assert!(is_own_endpoint(lan, 3890, 3890, &local));
        // 别人的地址：哪怕端口撞上本机的，也是别人的。
        assert!(!is_own_endpoint(other, 3890, 3890, &local));
        // 端口未知（对端没自报）时不判成自己，否则会把一次正常的观察扔掉。
        assert!(!is_own_endpoint(lo, 0, 3890, &local));
    }

    /// 校验器：没钉过的按 TOFU 认下并记住，钉过的必须一致，换了一枚就拒。
    ///
    /// 这条是整个"传输加密"里唯一真正的安全判断，所以直接对着校验器测 —— 端到端的
    /// 那个测试里两台实例共用同一份身份（同机同 IP），撞不出"指纹变了"这条路。
    #[test]
    fn verifier_pins_on_first_use_and_rejects_a_changed_certificate() {
        let peer = generate_identity().unwrap();
        let impostor = generate_identity().unwrap();
        let trust = Arc::new(Trust::new());
        let verifier = PinnedVerifier {
            trust: trust.clone(),
        };
        let name = ServerName::IpAddress("10.0.0.7".parse::<StdIpAddr>().unwrap().into());
        let now = UnixTime::since_unix_epoch(Duration::from_secs(1_700_000_000));
        let cert = |id: &Identity| id.cert_der.clone();

        // 没见过这个地址 → 放行，并把这一枚记下来（TOFU 的"认下"那一半）
        assert!(verifier
            .verify_server_cert(&cert(&peer), &[], &name, &[], now)
            .is_ok());
        assert_eq!(
            trust.observed_at("10.0.0.7").as_deref(),
            Some(peer.fingerprint.as_str())
        );

        // 钉住之后：同一枚继续放行
        trust.seed([("10.0.0.7".to_string(), peer.fingerprint.clone())]);
        assert!(verifier
            .verify_server_cert(&cert(&peer), &[], &name, &[], now)
            .is_ok());

        // 换了一枚（对方重装 / 有人在中间）→ 拒绝
        assert!(verifier
            .verify_server_cert(&cert(&impostor), &[], &name, &[], now)
            .is_err());
    }

    /// 中转端点的字面量（F-106）：**同一个地址的另一种写法**，但在指纹表里占独立键位。
    ///
    /// 这一条是整个中转方案的地基：端点上呈现的是被中转那台设备的证书，要是它与中转方
    /// 自己的服务共用键位，`Trust::seed` 就会因为"同一地址两枚指纹"把那条路整个放弃
    /// （退到 TOFU）—— 也就是把中转这一跳的身份校验悄悄关掉。
    #[test]
    fn relay_host_is_a_distinct_pinning_key() {
        let host = relay_host("198.51.100.6").expect("IPv4 能转");
        assert_eq!(host, "[::ffff:198.51.100.6]", "URL 主机必须带方括号");

        let direct = parse_host("198.51.100.6").expect("直连字面量");
        let relay = parse_host(&host).expect("带方括号的中转字面量也要认得");
        assert_ne!(
            direct, relay,
            "两者必须是**不同的键**，否则同一地址上钉两枚指纹 → 不钉 → TOFU"
        );
        assert!(
            matches!(relay, StdIpAddr::V6(v6) if v6.to_ipv4_mapped() == Some("198.51.100.6".parse().unwrap())),
            "键位是 IPv4 映射的 IPv6，地址还是同一个: {relay:?}"
        );

        // 非 IPv4 的字面量（域名/垃圾）不产出中转主机：这条路上我们只认 IPv4。
        assert!(relay_host("example.com").is_none());
        assert!(relay_host("").is_none());
        assert!(parse_host("[::ffff:198.51.100.6]").is_some());
        assert!(parse_host("198.51.100.6").is_some());
        assert!(parse_host("[]").is_none());
    }

    /// 并发上限只有"拿到名额"和"没有名额"两种结果：**不排队**（排队等于把连接堆在内存里
    /// 等，涨的还是内存），名额跟着连接走（连接收尾就还回来，不是一次性的）。
    #[test]
    fn connection_slots_are_an_admission_check_never_a_queue() {
        let slots = Arc::new(Semaphore::new(2));
        let first = acquire_slot(&slots).expect("第一个名额");
        let second = acquire_slot(&slots).expect("第二个名额");
        assert!(
            acquire_slot(&slots).is_none(),
            "满了就是没有名额，不是等一个出来"
        );
        drop(first);
        assert!(acquire_slot(&slots).is_some(), "连接收尾要把名额还回来");
        drop(second);
        // 上限本身是"远大于正常用量"的：正常同时打开在几十条（见 `MAX_CONNECTIONS`）。
        assert!(
            MAX_CONNECTIONS >= 256,
            "上限不能小到正常用量就撞上：{MAX_CONNECTIONS}"
        );
    }

    /// 一台主机不能把所有远程名额都占掉：按 IP 记账，到顶就丢新连接（不排队），
    /// 别的地址不受影响，连接收尾还账、表项随计数归零删掉。
    #[test]
    fn one_source_ip_cannot_take_every_remote_slot() {
        let slots = Arc::new(IpConnLimit::new(2));
        let a: StdIpAddr = "192.168.1.10".parse().unwrap();
        let b: StdIpAddr = "192.168.1.11".parse().unwrap();

        let first = slots.try_acquire(a).expect("第一条");
        let second = slots.try_acquire(a).expect("第二条");
        assert!(
            slots.try_acquire(a).is_none(),
            "同一 IP 到顶就是拒绝，不是等一个出来"
        );
        let other = slots.try_acquire(b).expect("别的设备不能被牵连");
        assert_eq!(slots.counts.lock().get(&b).copied(), Some(1));

        drop(first);
        let third = slots.try_acquire(a).expect("连接收尾要把名额还回来");

        drop(second);
        assert_eq!(
            slots.counts.lock().get(&a).copied(),
            Some(1),
            "还回来一条之后仍有一条在占着"
        );
        drop(third);
        assert!(
            slots.counts.lock().get(&a).is_none(),
            "计数归零要删表项，表不能随历史 IP 长大"
        );
        drop(other);
        assert!(slots.counts.lock().is_empty());

        // 常量本身：一台主机的上限必须明显小于全局远程名额，否则这道闸门没有意义；
        // 同时要盖住正常用量（长连接 + 至多 8 个传输 worker + 探活 ≈ 十几条）。
        assert!(MAX_CONNECTIONS_PER_REMOTE_IP < MAX_CONNECTIONS - LOOPBACK_CONNECTIONS);
        assert!(
            MAX_CONNECTIONS_PER_REMOTE_IP >= 32,
            "上限不能小到正常并发就撞上：{MAX_CONNECTIONS_PER_REMOTE_IP}"
        );
    }

    /// TLS 握手没有下文时**必须**放手：连上、发一个 `0x16`（握手记录类型）就挂住 ——
    /// 这正是"笔记本休眠/进程被挂起"那条路，没有超时它就会带着一份 TLS 缓冲永久占着任务。
    ///
    /// 断言外面再包一层 `timeout` 是因为这里没得商量：`TLS_HANDSHAKE_TIMEOUT` 是刻意的
    /// 10s 下界（正常路径上是给一台慢机器的余量），测试不能为了跑得快把它调小。
    #[tokio::test]
    async fn a_handshake_that_never_finishes_is_dropped() {
        use tokio::io::AsyncWriteExt;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let tls = server_config(&generate_identity().unwrap());
        let server = tokio::spawn(async move {
            let (stream, peer) = listener.accept().await.unwrap();
            dispatch_conn(stream, peer, Router::new(), tls).await
        });

        let mut client = TcpStream::connect(addr).await.unwrap();
        client.write_all(&[TLS_HANDSHAKE]).await.unwrap(); // 发完就什么都不说了
        let result = tokio::time::timeout(TLS_HANDSHAKE_TIMEOUT + Duration::from_secs(5), server)
            .await
            .expect("握手谈不完也必须收尾，不能永远挂着")
            .expect("任务不该 panic");
        assert_eq!(result.unwrap_err(), "TLS 握手超时");
    }

    /// 一个只答一次的裸 HTTP 服务端（`/pair/info` 的上限测试用）。
    async fn one_shot_http(response: String) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = socket.read(&mut buf).await;
            socket.write_all(response.as_bytes()).await.unwrap();
            let _ = socket.shutdown().await;
        });
        format!("http://{addr}/pair/info")
    }

    /// 声明值超限：**不读身体**就该失败（响应头到了、正文一个字节都没发）。
    ///
    /// 旧写法 `Response::json()` 会先把身体读进内存再解析：这里它会读到 EOF、报"不是 JSON"，
    /// 而在真机上对方只要继续写，读到的就是一个任意大的 `Vec`。
    #[tokio::test]
    async fn capped_json_rejects_a_declared_oversize_body() {
        install_crypto_provider();
        let url = one_shot_http(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 1048576\r\n\r\n"
                .to_string(),
        )
        .await;
        let resp = reqwest::Client::new().get(&url).send().await.unwrap();
        let err = read_json_capped::<serde_json::Value>(resp, 1024)
            .await
            .unwrap_err();
        assert!(err.contains("太大"), "报错要指到体积上，实际：{err}");
    }

    /// 不给 `Content-Length`（chunked）时，按块累计也必须在上限处停下。
    #[tokio::test]
    async fn capped_json_stops_at_the_limit_without_a_length() {
        install_crypto_provider();
        let chunk = "x".repeat(4096);
        let url = one_shot_http(format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{chunk}\r\n0\r\n\r\n",
            chunk.len()
        ))
        .await;
        let resp = reqwest::Client::new().get(&url).send().await.unwrap();
        let err = read_json_capped::<serde_json::Value>(resp, 1024)
            .await
            .unwrap_err();
        assert!(err.contains("上限"), "报错要指到上限上，实际：{err}");
    }

    /// 正常的 `/pair/info` 回话照常解析（上限不能误伤）。
    #[tokio::test]
    async fn capped_json_reads_a_normal_pair_info() {
        install_crypto_provider();
        let body = r#"{"id":"abc","name":"小明"}"#;
        let url = one_shot_http(format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        ))
        .await;
        let resp = reqwest::Client::new().get(&url).send().await.unwrap();
        let value = read_json_capped::<serde_json::Value>(resp, PAIR_INFO_BODY_MAX)
            .await
            .unwrap();
        assert_eq!(value["id"], "abc");
        assert_eq!(value["name"], "小明");
    }

    /// 流式下载客户端：对端收下请求、发完响应头后停住，读取必须在上限内失败。
    /// 这是 P1“接收数据面无限等待”的回归；正常持续出字节的传输不受影响。
    #[tokio::test]
    async fn streaming_client_times_out_a_stalled_body() {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        install_crypto_provider();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut buf = [0u8; 1024];
            let _ = socket.read(&mut buf).await;
            socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 10\r\n\r\n")
                .await
                .unwrap();
            socket.flush().await.unwrap();
            tokio::time::sleep(Duration::from_secs(3)).await;
        });

        let client = dial_client_streaming(
            "127.0.0.1",
            &DialPin::Tofu,
            Duration::from_secs(1),
            Duration::from_millis(100),
        );
        let resp = client
            .get(format!("http://{addr}/"))
            .send()
            .await
            .unwrap();
        let err = tokio::time::timeout(Duration::from_secs(2), resp.bytes())
            .await
            .expect("读取必须在上限内返回")
            .unwrap_err();
        assert!(
            err.is_timeout() || err.to_string().to_lowercase().contains("timed out"),
            "应当是读超时，实际：{err}"
        );
    }
}
