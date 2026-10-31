use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};

use mdns_sd::{IfKind, ServiceDaemon, ServiceEvent, ServiceInfo};
use tokio::sync::mpsc;
use tracing::{info, warn};

// ---------------------------------------------------------------------------
// Registration (advertise)
// ---------------------------------------------------------------------------

/// mDNS 服务类型（`register` 与注销共用一份字面量）。
const SERVICE_TYPE: &str = "_orbdrop._tcp.local.";

/// 回环作用域下写进 SRV/A 记录的地址（不调 `enable_addr_auto`，避免把内网地址也报出去）。
const LOOPBACK_IP: &str = "127.0.0.1";

/// mDNS 作用的范围。
///
/// 默认（正式版）是 [`Scope::Lan`]：同网段的其他机器能发现本机。
/// 同机多开联调（`ORBDROP_ALLOW_MULTI`）默认切成 [`Scope::Local`]：只走回环，
/// 两份实例能互相发现，但**局域网上的其他用户收不到任何组播** —— 否则每跑一次调试，
/// 所有用户的设备列表里都会多出一台同名、新 device id 的"幽灵设备"，退出后还要等
/// mDNS TTL（PTR 4500s）才消失。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scope {
    /// 所有网卡（正式版的局域网发现）。
    Lan,
    /// 只走回环（同机多开联调；其他机器看不见）。
    Local,
    /// 不广播、不浏览（手动添加仍可用）。
    Off,
}

impl Scope {
    /// 当前进程该用哪个作用域。
    ///
    /// 优先级：`ORBDROP_MDNS_SCOPE=lan|local|off` 显式指定；否则
    /// `ORBDROP_ALLOW_MULTI` 存在时为 `Local`，其余为 `Lan`。
    pub fn current() -> Self {
        let scope_env = std::env::var_os("ORBDROP_MDNS_SCOPE");
        let scope = scope_env.as_deref().map(|raw| raw.to_string_lossy());
        Self::resolve(
            scope.as_deref(),
            std::env::var_os("ORBDROP_ALLOW_MULTI").is_some(),
        )
    }

    /// `current` 的纯策略部分（单测不走进程环境变量）：显式 scope 优先，
    /// 否则同机多开 = 回环，单实例 = 局域网。
    fn resolve(scope: Option<&str>, multi: bool) -> Self {
        if let Some(raw) = scope {
            return Self::parse(raw).unwrap_or_else(|| {
                warn!(
                    value = raw,
                    "ORBDROP_MDNS_SCOPE 不认识（只认 lan/local/off），按 local 处理"
                );
                Self::Local
            });
        }
        if multi {
            Self::Local
        } else {
            Self::Lan
        }
    }

    /// 纯字符串解析（`current` 要读进程环境，单测走这里）。
    fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "lan" => Some(Self::Lan),
            "local" | "loopback" => Some(Self::Local),
            "off" | "none" | "disabled" => Some(Self::Off),
            _ => None,
        }
    }
}

/// 按作用域挑 daemon 的网卡。
///
/// `Local` 只看回环：mdns-sd 默认关闭回环，要先禁用整族再把 `LoopbackV4` 打开。
/// 这样注册与浏览的组播都只落在 `127.0.0.1` 上，不会从任何内网网卡发出去。
fn select_interfaces(daemon: &ServiceDaemon, scope: Scope) -> bool {
    if scope != Scope::Local {
        return true;
    }
    for (what, result) in [
        ("disable IPv4", daemon.disable_interface(IfKind::IPv4)),
        ("disable IPv6", daemon.disable_interface(IfKind::IPv6)),
        (
            "enable loopback IPv4",
            daemon.enable_interface(IfKind::LoopbackV4),
        ),
    ] {
        if let Err(e) = result {
            warn!("mDNS {what} failed: {e}");
            return false;
        }
    }
    true
}

/// 一次**挂着的** mDNS 注册：daemon 句柄 + 它自己报出来的服务全名。
///
/// 两样必须一起存，缺一样就注销不掉：
/// - 全名认的是**注册那一刻**的设备名。用户改了设备名之后，按当前配置拼出来的串已经不是
///   线上那一条了（`unregister` 要点名到具体记录）；
/// - 全名还必须取 mdns-sd 自己报的那一份（`ServiceInfo::get_fullname`）：实例名里可能有
///   它转义过的字符，自己拼出来的串未必与线上一致。
pub struct Registration {
    pub daemon: ServiceDaemon,
    pub fullname: String,
}

/// 摘掉一次注册：发一条 TTL=0 的告别包（对端立刻看不见），并**关掉**那个 daemon。
///
/// `shutdown` 这一步不能省。`ServiceDaemon` **没有 `Drop`**：只把句柄丢掉的话，daemon 线程
/// 还活着、注册还在，对端照样能解析到那一条 —— 用户改了设备名，别人的设备行却还是旧名
/// （旧实例每次答一次 query 就把旧名字写回去一次）。回归见 `lib.rs` 的
/// `renaming_releases_the_previous_mdns_registration`（真组播）。
///
/// 注销失败不是错误（记录可能早已过期 / daemon 已经关了），关掉线程才是这一步的目的。
pub fn release(reg: Registration) {
    if let Err(e) = reg.daemon.unregister(&reg.fullname) {
        warn!(fullname = %reg.fullname, "mDNS unregister failed: {e}");
    }
    // `shutdown` 是异步收尾（返回 DaemonStatus 的接收端），这里不等它 —— 句柄一丢，
    // 线程自己会收尾退出。
    if let Err(e) = reg.daemon.shutdown() {
        warn!(fullname = %reg.fullname, "mDNS daemon shutdown failed: {e}");
    }
}

/// Register the OrbDrop service via mDNS so same-subnet peers can discover us.
///
/// With `enable_addr_auto()`, the daemon fills in the host's non-loopback IPv4
/// addresses automatically. The TXT record carries the stable device id and
/// display name so browsing peers can identify us without a service resolution.
///
/// `scope = Scope::Local` 时反着来：daemon 只留回环网卡，记录里也只写 `127.0.0.1`
/// （不调 `enable_addr_auto`），于是同机其他实例看得见本机，局域网上的机器看不见。
///
/// F-107 之后还带两项**自报**：
/// - `relay`：本机此刻愿不愿意替别人中转（= `Config::allow_relay`，S-09）。与 `/pair/info`
///   的 `relay` 同义，放在 TXT 里是为了让同段的观察者**零请求**就能知道（否则要等它各自的
///   5s 探活）。它是**能力**不是**关系**，谁看都一样。
/// - `nets`：本机非回环 IPv4 网卡的网段（`192.168.1.0/24,10.0.0.0/8`）。跨网段时"这台设备
///   属于哪一段"只能由它自己说 —— 本机的掩码对远端网段不适用。
/// - `fp`：本机 TLS 证书指纹（小写 SHA-256）。与 LocalSend 公告里的 `fingerprint` 是同一枚
///   证书：同一台机器既播 `_orbdrop` 又播 `_localsend` 时，对端据此把两条发现结果认成一台
///   设备。TXT 未经认证，这一项**只用于设备列表去重**，不参与任何信任判断。
///
/// `nets` 是**提示**不是权威：网卡换了（插网线/开热点）不会重播 TXT（`enable_addr_auto`
/// 只管地址记录），所以活回话（`/pair/info` 与 `/discover` 的 `nets`）才是真值。
pub fn register(
    port: u16,
    device_name: &str,
    device_id: &str,
    allow_relay: bool,
    scope: Scope,
) -> Option<Registration> {
    if scope == Scope::Off {
        info!("mDNS registration disabled (scope=off)");
        return None;
    }
    let daemon = match ServiceDaemon::new() {
        Ok(d) => d,
        Err(e) => {
            warn!("mDNS daemon failed to start: {e}");
            return None;
        }
    };
    if !select_interfaces(&daemon, scope) {
        let _ = daemon.shutdown();
        return None;
    }

    let instance_name = format!("OrbDrop-{}", device_name);
    let service_type = SERVICE_TYPE;
    let host = format!("{}.local.", device_name.replace(' ', "-"));

    let mut properties = HashMap::new();
    properties.insert("id".to_string(), device_id.to_string());
    properties.insert("name".to_string(), device_name.to_string());
    properties.insert("ver".to_string(), "1".to_string());
    // F-107：中转能力位 + 本机网段自报（见 `register` 的文档）。
    properties.insert(
        "relay".to_string(),
        if allow_relay { "1" } else { "0" }.to_string(),
    );
    properties.insert(
        "nets".to_string(),
        crate::subnet::format_list(&crate::subnet::host_nets()),
    );
    // 与 LocalSend 公告里的 `fingerprint` 同一枚证书：对端用它把 `_orbdrop` 行与
    // `_localsend` 行认成同一台设备（见 `AppState::mdns_fingerprints`）。拿不到身份
    // 就不写这一项 —— 空值比一个错的指纹好。
    if let Ok(fingerprint) = crate::net::local_fingerprint() {
        if !fingerprint.is_empty() {
            properties.insert("fp".to_string(), fingerprint.to_ascii_lowercase());
        }
    }

    let service = if scope == Scope::Local {
        // 只报回环地址：不调 `enable_addr_auto`，否则内网地址也会被塞进 A 记录。
        ServiceInfo::new(
            service_type,
            &instance_name,
            &host,
            LOOPBACK_IP,
            port,
            properties,
        )
    } else {
        ServiceInfo::new(
            service_type,
            &instance_name,
            &host,
            "", // ip (empty → addr_auto fills in)
            port,
            properties,
        )
        .map(|s| s.enable_addr_auto())
    };
    let service = match service {
        Ok(s) => s,
        Err(e) => {
            warn!("Failed to build mDNS service info: {e}");
            let _ = daemon.shutdown();
            return None;
        }
    };

    // 全名**取 mdns-sd 自己报的那一份**，不按设备名自己拼：实例名里可能有它转义过的
    // 字符，自己拼出来的串与线上那一条不一致时，注销会找不到目标（记录只能等 TTL 过期，
    // 那段时间里对端还看得见旧名字）。
    let fullname = service.get_fullname().to_string();

    match daemon.register(service) {
        Ok(_) => {
            info!(
                port,
                device_name,
                device_id,
                ?scope,
                "mDNS service registered"
            );
            Some(Registration { daemon, fullname })
        }
        Err(e) => {
            warn!("mDNS registration failed: {e}");
            let _ = daemon.shutdown();
            None
        }
    }
}

// ---------------------------------------------------------------------------
// Scan (discover)
// ---------------------------------------------------------------------------

/// Event emitted when the mDNS scanner detects a peer state change.
#[derive(Debug, Clone)]
pub enum PeerEvent {
    Online {
        id: String,
        name: String,
        ip: IpAddr,
        port: u16,
        /// F-107：对端在 TXT 里自报的网段（可能为空 —— 老版本不报，或网卡刚换还没重播）。
        /// 只当提示：权威值在 `/pair/info` / `/discover` 的活回话里。
        nets: Vec<String>,
        /// F-107：对端在 TXT 里自报的中转能力（`relay=1`）。缺字段按"不允许"读 ——
        /// 与 S-09 默认关一致，也是老版本的行为。
        relay: bool,
        /// 对端在 TXT 里自报的 TLS 证书指纹（小写 hex；老版本不报 = 空）。
        /// **只用于跨协议去重**（同一台机器的 `_orbdrop` 行 / `_localsend` 行），
        /// 不参与信任判断。
        fingerprint: String,
    },
    Offline(String), // id
}

/// Spawn a background task that browses `_orbdrop._tcp` and sends peer
/// lifecycle events into `tx`.  The caller (e.g. lib.rs setup) routes these
/// to `AppState` and Tauri events.
///
/// Skips the local device (matched by `own_device_id`).
pub async fn scan(tx: mpsc::Sender<PeerEvent>, own_device_id: String, scope: Scope) {
    if scope == Scope::Off {
        info!("mDNS scan disabled (scope=off)");
        return;
    }
    let daemon = match ServiceDaemon::new() {
        Ok(d) => d,
        Err(e) => {
            warn!("mDNS scan daemon failed: {e}");
            return;
        }
    };
    if !select_interfaces(&daemon, scope) {
        let _ = daemon.shutdown();
        return;
    }

    let service_type = "_orbdrop._tcp.local.";
    let receiver = match daemon.browse(service_type) {
        Ok(r) => r,
        Err(e) => {
            warn!("mDNS browse failed: {e}");
            let _ = daemon.shutdown();
            return;
        }
    };

    info!(?scope, "mDNS scan started for {service_type}");

    // Remember instance fullname → peer id so ServiceRemoved can emit offline.
    let mut known: HashMap<String, String> = HashMap::new();

    loop {
        match receiver.recv_async().await {
            Ok(ServiceEvent::ServiceResolved(info)) => {
                let peer_id = info
                    .get_property_val("id")
                    .and_then(|v| v.map(|b| String::from_utf8_lossy(b).to_string()))
                    .unwrap_or_default();

                // Skip self.
                if peer_id == own_device_id {
                    continue;
                }

                known.insert(info.get_fullname().to_string(), peer_id.clone());

                let name = info
                    .get_property_val("name")
                    .and_then(|v| v.map(|b| String::from_utf8_lossy(b).to_string()))
                    .unwrap_or_else(|| info.get_hostname().to_string());

                // 对端报的是**一块网卡一个地址**，得挑一个当拨号目标（见 `pick_address`）。
                let ip = pick_address(
                    info.get_addresses_v4().into_iter().copied(),
                    &local_subnets(),
                )
                .map(IpAddr::V4)
                .or_else(|| info.get_addresses().iter().next().copied());

                let port = info.get_port();

                // F-107：TXT 里的自报网段与中转能力。缺字段按空 / 不允许读（老版本）。
                let txt = |key: &str| {
                    info.get_property_val(key)
                        .and_then(|v| v.map(|b| String::from_utf8_lossy(b).to_string()))
                        .unwrap_or_default()
                };
                let nets = crate::subnet::parse_list(&txt("nets"))
                    .iter()
                    .map(|c| c.to_cidr_string())
                    .collect::<Vec<String>>();
                let relay = matches!(txt("relay").as_str(), "1" | "true");
                // 老版本不报 `fp`：空串。归一化在 `AppState::note_mdns_fingerprint` 里做，
                // 这里只搬运行上的形状。
                let fingerprint = txt("fp");

                if let (Some(ip), port) = (ip, port) {
                    let _ = tx
                        .send(PeerEvent::Online {
                            id: peer_id,
                            name,
                            ip,
                            port,
                            nets,
                            relay,
                            fingerprint,
                        })
                        .await;
                }
            }
            Ok(ServiceEvent::ServiceRemoved(_svc_type, full_name)) => {
                // browse() auto-resolves instances, so we track fullname→id.
                if let Some(id) = known.remove(full_name.as_str()) {
                    let _ = tx.send(PeerEvent::Offline(id)).await;
                }
            }
            Ok(_) => {
                // SearchStarted / SearchStopped / ServiceFound: ignore.
            }
            Err(e) => {
                warn!("mDNS recv error: {e}");
                // Daemon is still running; resume.
            }
        }
    }
}

/// 从对端报出的 IPv4 里挑一个**当拨号目标**。
///
/// mDNS 的地址记录是**一块网卡一条**：对端的有线/无线/热点/VPN/WSL 各有各的地址，
/// 而对端这一侧把它们合成一个 `HashSet` —— 随手 `next()` 等于随机选一块网卡当目标，
/// 而且选谁随集合内容变。表现正是"对端插网线/开热点/换 Wi-Fi 之后指向一个到不了的地址"：
/// 记录其实刷新了（mdns-sd 每 5s 查网卡、逐网卡重新播），是本机挑错了那一条。
///
/// 排序（前面的优先）：
/// 1. 与本机某块网卡**同网段** —— 本机直接能到；对端换网段之后这一档跟着换，
///    不必等它的旧地址记录自然过期（A 记录 TTL 120s，见 `pick_address` 的调用点）；
/// 2. 其它私网地址（家用/办公网段，通常可路由）；
/// 3. 其它全局地址；
/// 4. 链路本地（169.254，通常是没拿到 DHCP 的残留）。
///
/// 同级按地址升序：集合内容不变时结果相同，设备行的地址不会自己跳。
fn pick_address(
    addrs: impl IntoIterator<Item = Ipv4Addr>,
    local: &[(u32, u32)],
) -> Option<Ipv4Addr> {
    let mut best: Option<(u8, u32)> = None;
    let mut best_ip = None;
    for ip in addrs {
        let bits = u32::from(ip);
        let rank = if local.iter().any(|(net, mask)| *net == bits & mask) {
            0
        } else if ip.is_link_local() {
            3
        } else if ip.is_private() {
            1
        } else {
            2
        };
        if best.map_or(true, |b| (rank, bits) < b) {
            best = Some((rank, bits));
            best_ip = Some(ip);
        }
    }
    best_ip
}

/// 本机各网卡所在网段：`(网络号, 掩码)`。带掩码是因为它可能是 /24，也可能被管理员配成
/// /22、/16 —— 判"同网段"必须用真实边界（与 `routes_discover` 同一套算理）。
///
/// 回环不算：对端报 `127.0.0.1` 时（安卓模拟器把本机回环别名拍过来）那不是"本机网卡上的
/// 地址"，不该因此被排到第一档。
fn local_subnets() -> Vec<(u32, u32)> {
    crate::subnet::host_nets()
        .into_iter()
        .map(|c| (c.network(), c.mask()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 本机在 `10.0.0.0/24`（另一块网卡才有的网段不进这一档）。
    fn local() -> Vec<(u32, u32)> {
        vec![(
            u32::from(Ipv4Addr::new(10, 0, 0, 0)),
            u32::from(Ipv4Addr::new(255, 255, 255, 0)),
        )]
    }

    fn ip(s: &str) -> Ipv4Addr {
        s.parse().unwrap()
    }

    /// 同网段优先：对端同时报着热点地址（192.168.137.1）与本机所在网段的地址时，
    /// 选后者 —— 这正是"开热点之后新增一个 ip"那个场景里挑错会连不上的地方。
    #[test]
    fn same_subnet_beats_a_newly_added_private_address() {
        let picked = pick_address([ip("192.168.137.1"), ip("10.0.0.7")], &local());
        assert_eq!(picked, Some(ip("10.0.0.7")));
    }

    /// 掩码不是 /24 时也按真实边界判同网段。
    #[test]
    fn same_subnet_respects_the_real_netmask() {
        let local = vec![(
            u32::from(Ipv4Addr::new(10, 0, 0, 0)),
            u32::from(Ipv4Addr::new(255, 255, 0, 0)),
        )];
        assert_eq!(pick_address([ip("10.0.9.7")], &local), Some(ip("10.0.9.7")));
    }

    /// 没有同网段的地址时：私网 → 全局 → 链路本地（169.254 排在最后）。
    #[test]
    fn private_then_global_then_link_local() {
        let picked = pick_address(
            [ip("169.254.3.4"), ip("8.8.8.8"), ip("192.168.1.5")],
            &local(),
        );
        assert_eq!(picked, Some(ip("192.168.1.5")));
        let picked = pick_address([ip("169.254.3.4"), ip("8.8.8.8")], &local());
        assert_eq!(picked, Some(ip("8.8.8.8")));
        let picked = pick_address([ip("169.254.3.4")], &local());
        assert_eq!(picked, Some(ip("169.254.3.4")));
    }

    /// 同级按地址升序：集合内容不变，结果就不变（设备行的地址不会自己跳）。
    #[test]
    fn ties_are_stable_and_address_ordered() {
        let a = [ip("10.0.0.9"), ip("10.0.0.7"), ip("10.0.0.8")];
        assert_eq!(pick_address(a, &local()), Some(ip("10.0.0.7")));
        let b = [ip("10.0.0.8"), ip("10.0.0.7"), ip("10.0.0.9")];
        assert_eq!(pick_address(b, &local()), Some(ip("10.0.0.7")));
    }

    /// 回环不算"本机网卡上的地址"：模拟器把本机回环拍过来时，它不该压过真正的内网地址。
    /// （排除发生在 `local_subnets` 那一层 —— `pick_address` 只认传进去的网段表。）
    #[test]
    fn loopback_is_not_treated_as_a_local_interface() {
        let lo = u32::from(Ipv4Addr::new(127, 0, 0, 0));
        assert!(
            !local_subnets().iter().any(|(net, _)| *net == lo),
            "本机网段表里不许有回环：{:?}",
            local_subnets()
        );
        assert_eq!(
            pick_address([ip("127.0.0.1"), ip("192.168.1.5")], &local_subnets()),
            Some(ip("192.168.1.5"))
        );
    }

    #[test]
    fn no_addresses_means_no_target() {
        assert_eq!(pick_address([], &local()), None);
    }

    #[test]
    fn scope_parsing_is_explicit() {
        assert_eq!(Scope::parse("lan"), Some(Scope::Lan));
        assert_eq!(Scope::parse(" LOCAL "), Some(Scope::Local));
        assert_eq!(Scope::parse("loopback"), Some(Scope::Local));
        assert_eq!(Scope::parse("off"), Some(Scope::Off));
        assert_eq!(Scope::parse("none"), Some(Scope::Off));
        assert_eq!(Scope::parse("nonsense"), None);
    }

    /// 默认策略：同机多开（`ORBDROP_ALLOW_MULTI`）只走回环；显式 scope 永远优先；
    /// 写错的显式值按 local 处理（宁可看不见，也不要把临时实例撒到局域网）。
    #[test]
    fn multi_instance_defaults_to_loopback_and_explicit_scope_wins() {
        assert_eq!(Scope::resolve(None, false), Scope::Lan);
        assert_eq!(Scope::resolve(None, true), Scope::Local);
        assert_eq!(Scope::resolve(Some("lan"), true), Scope::Lan);
        assert_eq!(Scope::resolve(Some("off"), true), Scope::Off);
        assert_eq!(Scope::resolve(Some("local"), false), Scope::Local);
        assert_eq!(Scope::resolve(Some("nonsense"), false), Scope::Local);
    }

    /// `Scope::Local` 的记录只报回环地址；默认（LAN）扫描端回环是关着的，看不见它。
    ///
    /// 这是"同机多开联调不污染局域网"的回归：以前调试实例会以一个新 device id 出现在
    /// 同一网段**所有用户**的设备列表里，退出后还要等 mDNS TTL 才消失。
    #[test]
    fn local_scope_keeps_advertisements_on_loopback() {
        use std::time::{Duration, Instant};

        let name = format!("lo-{}", &uuid::Uuid::new_v4().to_string()[..8]);
        let Some(reg) = register(39_123, &name, "lo-test-id", false, Scope::Local) else {
            eprintln!("mDNS 起不来，跳过");
            return;
        };

        let loopback = ServiceDaemon::new().expect("loopback daemon");
        assert!(select_interfaces(&loopback, Scope::Local));
        let rx = loopback.browse(SERVICE_TYPE).expect("browse");
        let deadline = Instant::now() + Duration::from_secs(4);
        let mut seen = None;
        while Instant::now() < deadline {
            let left = deadline.saturating_duration_since(Instant::now());
            match rx.recv_timeout(left) {
                Ok(ServiceEvent::ServiceResolved(info))
                    if info.get_fullname() == reg.fullname.as_str() =>
                {
                    seen = Some(info);
                    break;
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }

        // 默认网卡（LAN）的浏览端不该看见这条只走回环的记录。
        let lan = ServiceDaemon::new().expect("lan daemon");
        let lrx = lan.browse(SERVICE_TYPE).expect("lan browse");
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut leaked = false;
        while Instant::now() < deadline {
            let left = deadline.saturating_duration_since(Instant::now());
            match lrx.recv_timeout(left) {
                Ok(ServiceEvent::ServiceResolved(info))
                    if info.get_fullname() == reg.fullname.as_str() =>
                {
                    leaked = true;
                    break;
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }

        release(reg);
        let _ = loopback.shutdown();
        let _ = lan.shutdown();

        let Some(info) = seen else {
            eprintln!("回环 mDNS 在这台机器上没解析到，跳过（{name}）");
            return;
        };
        assert!(
            !info.get_addresses().is_empty()
                && info.get_addresses().iter().all(|a| a.is_loopback()),
            "Local 作用域只能报回环地址：{:?}",
            info.get_addresses()
        );
        assert!(!leaked, "Local 作用域的记录从局域网网卡漏出去了");
    }

    /// `scan(Scope::Local)` 能看见同机另一个 `register(Scope::Local)` 的实例 ——
    /// 这就是同机多开联调依赖的那条路（两份都不出网卡）。
    #[tokio::test]
    async fn local_scan_sees_another_local_instance() {
        use std::time::Duration;

        let tag = uuid::Uuid::new_v4().to_string();
        let name = format!("scan-{}", &tag[..8]);
        let peer_id = format!("scan-peer-{tag}");
        let Some(reg) = register(39_124, &name, &peer_id, false, Scope::Local) else {
            eprintln!("mDNS 起不来，跳过");
            return;
        };

        let (tx, mut rx) = mpsc::channel(16);
        let task = tokio::spawn(scan(tx, "own-id".to_string(), Scope::Local));
        let found = tokio::time::timeout(Duration::from_secs(5), async {
            while let Some(event) = rx.recv().await {
                if let PeerEvent::Online {
                    id, fingerprint, ..
                } = event
                {
                    if id == peer_id {
                        return Some(fingerprint);
                    }
                }
            }
            None
        })
        .await
        .unwrap_or(None);

        task.abort();
        release(reg);
        let Some(fingerprint) = found else {
            panic!("Local 扫描端应该能看见同机注册的实例");
        };
        // 跨协议去重靠这一项：TXT 里必须是本机 TLS 证书指纹（小写）。
        assert_eq!(
            fingerprint,
            crate::net::local_fingerprint()
                .expect("identity")
                .to_ascii_lowercase(),
            "mDNS TXT 要带上本机证书指纹"
        );
    }
}
