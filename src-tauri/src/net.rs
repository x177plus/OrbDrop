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
//! - 首次见到某地址（没有钉住的指纹）时按 **TOFU** 接受并记住 —— 这是"零点击"的代价，
//!   首次配对若正好有人在中间，本机认下的就是中间人的证书。要堵这个缝只能靠配对码那条
//!   带外信道（PAKE），那是另一件事。
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
use rustls::{DigitallySignedStruct, SignatureScheme};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::net::{TcpListener, TcpStream};
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
    if let Ok(pem) = std::fs::read_to_string(&path) {
        match identity_from_pem(&pem) {
            Ok(id) => {
                debug!(fingerprint = %short_fingerprint(&id.fingerprint), "本机身份已加载");
                return Ok(id);
            }
            Err(e) => warn!(
                "身份文件读不出来（{e}）：将重新生成一把，所有对端都会看到指纹变化并要求重新配对"
            ),
        }
    }
    let id = generate_identity()?;
    let pem = format!(
        "{}{}",
        to_pem(id.cert_der.as_ref(), "CERTIFICATE"),
        to_pem(id.key_der.secret_der(), "PRIVATE KEY")
    );
    config::write_atomic(&path, pem.as_bytes()).map_err(|e| format!("身份文件写入失败: {e}"))?;
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

/// 已知对端的证书指纹，按**地址**索引。
///
/// 为什么按地址而不是按设备 id：TLS 校验发生在拿到任何应用层数据**之前**，此刻手上只有
/// 一个 IP。一台机器一份身份（同机多实例共用同一个数据目录，也就共用同一把身份），
/// 所以"一个地址一把指纹"是准的；真正的身份归属由握手之后的 token 认证负责。
#[derive(Debug, Default)]
pub struct Trust {
    pinned: Mutex<HashMap<StdIpAddr, String>>,
    /// 这次进程里实际见到过的指纹。TOFU 靠它把"刚认下的那枚"交回给调用方落库——
    /// 校验器只负责放行/拒绝，**不写凭据库**（它在握手线程里，不该碰磁盘）。
    observed: Mutex<HashMap<StdIpAddr, String>>,
}

impl Trust {
    pub fn new() -> Self {
        Self::default()
    }

    /// 用「地址 → 指纹」重建这份表（启动时、配对成功/解除配对后各调一次）。
    ///
    /// 整份替换而不是增量合并：地址簿与凭据库才是真相，这里只是一份供握手线程只读的快照。
    ///
    /// **同一个地址上出现两枚不同指纹 = 不钉**（并留一行 warn）。这种事只可能来自"本机台账
    /// 里有两台已配对设备共用一个地址"（对端重装换了 id、地址被另一台机器接手、上次配对
    /// 留下的残影）。随便挑一个钉住，另一台就永远被"指纹不一致"挡在门外，而且那条错误
    /// 只在日志里 —— 表现就是"这台设备怎么都连不上，重新配对也没用"。不钉 = 退到 TOFU，
    /// 下一次成功连线会把真正的那枚认下来（同时 `remember_peer_fingerprint` 落库）。
    pub fn seed(&self, entries: impl IntoIterator<Item = (String, String)>) {
        let mut map: HashMap<StdIpAddr, String> = HashMap::new();
        let mut conflicted: Vec<StdIpAddr> = Vec::new();
        for (ip, fp) in entries {
            let Some(ip) = parse_host(&ip) else { continue };
            match map.get(&ip) {
                Some(existing) if *existing != fp => {
                    if !conflicted.contains(&ip) {
                        warn!(
                            %ip,
                            pinned = %short_fingerprint(existing),
                            seen = %short_fingerprint(&fp),
                            "同一地址上钉着两枚不同指纹：这一轮不钉它（退到 TOFU，下次连上重新认）"
                        );
                        conflicted.push(ip);
                    }
                }
                _ => {
                    map.insert(ip, fp);
                }
            }
        }
        for ip in conflicted {
            map.remove(&ip);
        }
        *self.pinned.lock() = map;
    }

    pub fn get(&self, ip: StdIpAddr) -> Option<String> {
        self.pinned.lock().get(&ip).cloned()
    }

    /// 记下这次见到的指纹（不落库）。
    fn observe(&self, ip: StdIpAddr, fp: String) {
        self.observed.lock().insert(ip, fp);
    }

    /// 本次进程里为某个地址见过的指纹（落库用）。
    pub fn observed_at(&self, ip: &str) -> Option<String> {
        parse_host(ip).and_then(|ip| self.observed.lock().get(&ip).cloned())
    }

    /// 忘掉某个地址的指纹（解除配对时用；地址换了主人也一样）。
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

        match self.trust.get(ip) {
            Some(expected) if expected == seen => {}
            Some(expected) => {
                warn!(
                    %ip,
                    expected = %short_fingerprint(&expected),
                    seen = %short_fingerprint(&seen),
                    "对端证书指纹与钉住的不一致，拒绝连接"
                );
                return Err(rejected());
            }
            None => info!(
                %ip,
                seen = %short_fingerprint(&seen),
                "首次见到该地址的证书，按 TOFU 认下并记住"
            ),
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

/// 拨号方向（我们是对端）用的 TLS 配置。
pub fn client_config(trust: Arc<Trust>) -> Arc<rustls::ClientConfig> {
    let config = rustls::ClientConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .expect("ring provider 支持默认协议版本")
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(PinnedVerifier { trust }))
        .with_no_client_auth();
    Arc::new(config)
}

/// 监听方向（对端拨我们）用的 TLS 配置。
pub fn server_config(identity: &Identity) -> Arc<rustls::ServerConfig> {
    let config = rustls::ServerConfig::builder_with_provider(provider())
        .with_safe_default_protocol_versions()
        .expect("ring provider 支持默认协议版本")
        .with_no_client_auth()
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
    install_crypto_provider();
    reqwest::Client::builder()
        .use_preconfigured_tls((*client_config(trust())).clone())
        // 数据面全是小写入（256KB 帧、4MB 块），默认的 Nagle 会和延迟 ACK 互相等：
        // 关掉它，写多少发多少。对端那一侧另有 `set_nodelay`（见 `dispatch_conn`）。
        .tcp_nodelay(true)
}

pub fn client() -> reqwest::Client {
    CLIENT.clone()
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
    if is_own_endpoint(source, reported_port, listening, &crate::server::interface_addresses()) {
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
        let router = router.clone();
        let tls = tls.clone();
        tokio::spawn(async move {
            if let Err(e) = dispatch_conn(stream, addr, router, tls).await {
                debug!(%addr, %e, "连接结束");
            }
        });
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
        let stream = acceptor.accept(stream).await.map_err(|e| e.to_string())?;
        return serve_http(stream, addr, router).await;
    }

    // 明文只服务回环：本机 UI 的自验路径、以及 Beyond Compare 那条 WebDAV 入口
    // （`server::proxy` 把它转成到对端的加密连接）。局域网上来的明文连接一律丢 ——
    // 否则"加密"就是可选的，谁都能退回去用明文。
    if !addr.ip().is_loopback() {
        warn!(%addr, "非回环地址以明文 HTTP 连接，已拒绝");
        return Ok(());
    }
    serve_http(stream, addr, router).await
}

async fn serve_http<I>(io: I, addr: SocketAddr, router: Router) -> Result<(), String>
where
    I: AsyncRead + AsyncWrite + Unpin + Send + 'static,
{
    let service = hyper::service::service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
        let mut router = router.clone();
        async move {
            let (mut parts, body) = req.into_parts();
            // `ConnectInfo` 是 `auth_middleware` 认"回环 = 本机"的依据，也是代理路由
            // 挡非回环的依据 —— 手写 accept 循环就得自己把它塞回去（`axum::serve` 是
            // 靠 `into_make_service_with_connect_info` 做的同一件事）。
            parts.extensions.insert(axum::extract::ConnectInfo(addr));
            let req = hyper::Request::from_parts(parts, axum::body::Body::new(body));
            router.call(req).await
        }
    });

    hyper::server::conn::http1::Builder::new()
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
    let mut current = Some(err);
    while let Some(err) = current {
        if let Some(rustls_err) = err.downcast_ref::<rustls::Error>() {
            return matches!(
                rustls_err,
                rustls::Error::InvalidCertificate(
                    rustls::CertificateError::ApplicationVerificationFailure
                )
            );
        }
        current = err.source();
    }
    false
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
) -> Result<
    tokio_tungstenite::WebSocketStream<tokio_rustls::client::TlsStream<tokio::net::TcpStream>>,
    DialError,
> {
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;

    let request = url
        .into_client_request()
        .map_err(|e| DialError::Unreachable(format!("WS 地址无效: {e}")))?;
    let ip: StdIpAddr = parse_host(ip)
        .ok_or_else(|| DialError::Unreachable(format!("地址无效: {ip}")))?;

    let tcp = TcpStream::connect(SocketAddr::new(ip, port))
        .await
        .map_err(|e| DialError::Unreachable(e.to_string()))?;
    let connector = tokio_rustls::TlsConnector::from(client_config(trust()));
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
    fn seed_refuses_an_ambiguous_address() {
        let trust = Trust::new();
        trust.seed([
            ("10.0.0.9".to_string(), "aa".to_string()),
            ("10.0.0.9".to_string(), "bb".to_string()),
            ("10.0.0.8".to_string(), "cc".to_string()),
        ]);
        assert_eq!(trust.get("10.0.0.9".parse().unwrap()), None, "有歧义的地址不钉");
        assert_eq!(
            trust.get("10.0.0.8".parse().unwrap()).as_deref(),
            Some("cc"),
            "没歧义的照旧钉"
        );
        // 歧义消失（比如那台旧的被解除配对了）→ 下一轮 seed 就正常钉上。
        trust.seed([("10.0.0.9".to_string(), "bb".to_string())]);
        assert_eq!(trust.get("10.0.0.9".parse().unwrap()).as_deref(), Some("bb"));
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

    #[test]
    fn trust_only_matches_the_pinned_fingerprint() {
        let trust = Trust::new();
        trust.seed([("10.0.0.5".to_string(), "aa".to_string())]);
        let ip: StdIpAddr = "10.0.0.5".parse().unwrap();
        assert_eq!(trust.get(ip).as_deref(), Some("aa"));
        assert_eq!(trust.get("10.0.0.6".parse().unwrap()), None);
        trust.unpin(ip);
        assert_eq!(trust.get(ip), None);
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
        let host = relay_host("10.198.71.106").expect("IPv4 能转");
        assert_eq!(host, "[::ffff:10.198.71.106]", "URL 主机必须带方括号");

        let direct = parse_host("10.198.71.106").expect("直连字面量");
        let relay = parse_host(&host).expect("带方括号的中转字面量也要认得");
        assert_ne!(
            direct, relay,
            "两者必须是**不同的键**，否则同一地址上钉两枚指纹 → 不钉 → TOFU"
        );
        assert!(
            matches!(relay, StdIpAddr::V6(v6) if v6.to_ipv4_mapped() == Some("10.198.71.106".parse().unwrap())),
            "键位是 IPv4 映射的 IPv6，地址还是同一个: {relay:?}"
        );

        // 非 IPv4 的字面量（域名/垃圾）不产出中转主机：这条路上我们只认 IPv4。
        assert!(relay_host("example.com").is_none());
        assert!(relay_host("").is_none());
        assert!(parse_host("[::ffff:10.198.71.106]").is_some());
        assert!(parse_host("10.198.71.106").is_some());
        assert!(parse_host("[]").is_none());
    }
}