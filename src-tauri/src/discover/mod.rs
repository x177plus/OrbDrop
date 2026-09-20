use std::collections::HashMap;
use std::net::IpAddr;

use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use tokio::sync::mpsc;
use tracing::{info, warn};

// ---------------------------------------------------------------------------
// Registration (advertise)
// ---------------------------------------------------------------------------

/// mDNS 服务类型（`register` 与注销共用一份字面量）。
const SERVICE_TYPE: &str = "_orbdrop._tcp.local.";

/// 本机在 mDNS 上的服务全名：`register` 注册用的就是它，「离线」时按它精确注销。
///
/// 注销必须点名到这一条记录：只把 `ServiceDaemon` 丢掉，对端缓存里的记录要等 TTL
/// （约 2 分钟）自然过期才消失 —— 那段时间里它仍然看得见本机。`unregister` 会立刻
/// 发一条 TTL=0 的告别包。
pub fn service_fullname(device_name: &str) -> String {
    format!("OrbDrop-{}.{}", device_name, SERVICE_TYPE)
}

/// Register the OrbDrop service via mDNS so same-subnet peers can discover us.
///
/// With `enable_addr_auto()`, the daemon fills in the host's non-loopback IPv4
/// addresses automatically. The TXT record carries the stable device id and
/// display name so browsing peers can identify us without a service resolution.
pub fn register(port: u16, device_name: &str, device_id: &str) -> Option<ServiceDaemon> {
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

    match daemon.register(service) {
        Ok(_) => {
            info!(port, device_name, device_id, "mDNS service registered");
            Some(daemon)
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

                let ip = info.get_addresses_v4().into_iter().next().copied().map(IpAddr::V4).or_else(|| {
                    info.get_addresses().iter().next().copied()
                });

                let port = info.get_port();

                if let (Some(ip), port) = (ip, port) {
                    let _ = tx.send(PeerEvent::Online {
                        id: peer_id,
                        name,
                        ip,
                        port,
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