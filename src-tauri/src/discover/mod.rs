use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr};

use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use tokio::sync::mpsc;
use tracing::{info, warn};

// ---------------------------------------------------------------------------
// Registration (advertise)
// ---------------------------------------------------------------------------

/// mDNS 服务类型（`register` 与注销共用一份字面量）。
const SERVICE_TYPE: &str = "_orbdrop._tcp.local.";

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
/// F-107 之后还带两项**自报**：
/// - `relay`：本机此刻愿不愿意替别人中转（= `Config::allow_relay`，S-09）。与 `/pair/info`
///   的 `relay` 同义，放在 TXT 里是为了让同段的观察者**零请求**就能知道（否则要等它各自的
///   5s 探活）。它是**能力**不是**关系**，谁看都一样。
/// - `nets`：本机非回环 IPv4 网卡的网段（`192.168.1.0/24,10.0.0.0/8`）。跨网段时"这台设备
///   属于哪一段"只能由它自己说 —— 本机的掩码对远端网段不适用。
///
/// `nets` 是**提示**不是权威：网卡换了（插网线/开热点）不会重播 TXT（`enable_addr_auto`
/// 只管地址记录），所以活回话（`/pair/info` 与 `/discover` 的 `nets`）才是真值。
pub fn register(port: u16, device_name: &str, device_id: &str, allow_relay: bool) -> Option<Registration> {
    let daemon = match ServiceDaemon::new() {
        Ok(d) => d,
        Err(e) => {
            warn!("mDNS daemon failed to start: {e}");
            return None;
        }
    };

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

    let service = match ServiceInfo::new(
        service_type,
        &instance_name,
        &host,
        "", // ip (empty → addr_auto fills in)
        port,
        properties,
    ) {
        Ok(s) => s.enable_addr_auto(),
        Err(e) => {
            warn!("Failed to build mDNS service info: {e}");
            return None;
        }
    };

    // 全名**取 mdns-sd 自己报的那一份**，不按设备名自己拼：实例名里可能有它转义过的
    // 字符，自己拼出来的串与线上那一条不一致时，注销会找不到目标（记录只能等 TTL 过期，
    // 那段时间里对端还看得见旧名字）。
    let fullname = service.get_fullname().to_string();

    match daemon.register(service) {
        Ok(_) => {
            info!(port, device_name, device_id, "mDNS service registered");
            Some(Registration { daemon, fullname })
        }
        Err(e) => {
            warn!("mDNS registration failed: {e}");
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
    },
    Offline(String), // id
}

/// Spawn a background task that browses `_orbdrop._tcp` and sends peer
/// lifecycle events into `tx`.  The caller (e.g. lib.rs setup) routes these
/// to `AppState` and Tauri events.
///
/// Skips the local device (matched by `own_device_id`).
pub async fn scan(tx: mpsc::Sender<PeerEvent>, own_device_id: String) {
    let daemon = match ServiceDaemon::new() {
        Ok(d) => d,
        Err(e) => {
            warn!("mDNS scan daemon failed: {e}");
            return;
        }
    };

    let service_type = "_orbdrop._tcp.local.";
    let receiver = match daemon.browse(service_type) {
        Ok(r) => r,
        Err(e) => {
            warn!("mDNS browse failed: {e}");
            return;
        }
    };

    info!("mDNS scan started for {service_type}");

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

                if let (Some(ip), port) = (ip, port) {
                    let _ = tx.send(PeerEvent::Online {
                        id: peer_id,
                        name,
                        ip,
                        port,
                        nets,
                        relay,
                    }).await;
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
fn pick_address(addrs: impl IntoIterator<Item = Ipv4Addr>, local: &[(u32, u32)]) -> Option<Ipv4Addr> {
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
        vec![(u32::from(Ipv4Addr::new(10, 0, 0, 0)), u32::from(Ipv4Addr::new(255, 255, 255, 0)))]
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
        let local = vec![(u32::from(Ipv4Addr::new(10, 0, 0, 0)), u32::from(Ipv4Addr::new(255, 255, 0, 0)))];
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
}