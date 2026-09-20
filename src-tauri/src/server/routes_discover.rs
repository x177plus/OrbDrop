//! 跨网段发现（F-10 的延伸）。
//!
//! mDNS 只在同一个 L2 段里有效 —— 组播包按 RFC 6762 要求 IP TTL=255，接收端还校验它，
//! 任何被路由器转发过的包都会被丢掉。所以**跨网段**的设备，mDNS 永远看不见。但**单播**
//! 是通的：手动添加 `IP:端口` 能连上就证明了这一点。
//!
//! 于是跨段那半边换个传输：**已配对的两台设备，各自把自己 mDNS 视野里的同伴报给对方**
//! —— 一台锚点换回一整个网段，之后本机直连那些设备（不经过锚点转发）。
//!
//! 三条边界决定了它必须长这样：
//!
//! - **要 token**：只有已配对才拿得到表。没交换过凭据的设备只能看见本机自己
//!   （`/pair/info`），看不到本机认识了谁。这既是安全边界，也正是"匹配上之后才发现"。
//! - **只回本机这一段的**：A 是用哪个地址拨过来的（看 `Host` 头），就只给那个地址所在
//!   子网的设备。本机多网卡（有线 + 无线 + VPN）上其它网段的设备，对 A 未必可达 ——
//!   那台机器可能正是它自己内网的那扇门（NAT/二级路由），回过去只会让 A 拿着一个
//!   通不了的地址空转。
//! - **只回在线的**：这一层不判断"这个地址通不通"（那是拿到表那一方 `state::probe_*`
//!   的职责），只回答"本机这一段此刻有谁在"。

use std::net::Ipv4Addr;
use std::sync::Arc;

use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde::{Deserialize, Serialize};
use tracing::info;

use crate::server::auth::Caller;
use crate::state::AppState;

/// 表里的一台设备：**只有身份和地址**。
///
/// 没有 token、没有配对状态、没有广播标志 —— 那些是"本机跟它之间的关系"，换一台设备
/// 看就是另一回事，不该跟着表走。在线与否也不在这里：能进这张表就意味着报出它的那一方
/// 此刻看得见它（mDNS 视野本来就是实时的）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct PeerEntry {
    pub id: String,
    pub name: String,
    pub ip: String,
    pub port: u16,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoverResponse {
    pub id: String,
    pub name: String,
    pub port: u16,
    pub ver: u32,
    pub peers: Vec<PeerEntry>,
}

/// GET /discover — 已配对对端来要「本机这一段里在线的设备」。
///
/// 失败路径都要想清楚：**本机离线（隐身）时**由 `presence_middleware` 拦成 503，
/// 对端据此判本机不在网上（与探活同一套语义）；**没带/带错 token** 由 `auth_middleware`
/// 拦成 401 —— 对端会退回 `/pair/info` 去问"它还认不认我们"，这一问不带 token 也答。
pub async fn discover(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<Caller>,
    headers: HeaderMap,
) -> Response {
    // 本机 UI（回环）没有"从哪个网段拨进来"这回事，也不需要这张表：它读共享项走 /shares。
    let Caller::Peer(caller_id) = caller else {
        return (StatusCode::FORBIDDEN, "discover 只对已配对对端开放").into_response();
    };

    let (own_id, own_name) = {
        let config = state.config.lock();
        (config.device_id.clone(), config.device_name.clone())
    };
    let port = listening_port(&state);

    // A 拨的是本机的哪个地址，就按哪个地址划段 —— `Host` 头里那个 IP 就是它。
    let local_ip = match header_host_ip(&headers) {
        Some(ip) => ip,
        None => {
            // 认不出拨的是哪个地址（自定义 Host / 走代理 / IPv6 字面量）：给不出可靠的
            // 网段。宁可不回，也不回一批对面根本到不了的地址。
            info!("discover: Host 头里取不出本机地址，回空表");
            return Json(DiscoverResponse {
                id: own_id,
                name: own_name,
                port,
                ver: 1,
                peers: vec![],
            })
            .into_response();
        }
    };

    let mask = netmask_for(local_ip).unwrap_or(Ipv4Addr::new(255, 255, 255, 0));
    let subnet = u32::from(local_ip) & u32::from(mask);

    let peers: Vec<PeerEntry> = {
        let peers = state.peers.lock();
        peers
            .iter()
            .filter(|p| {
                p.id != own_id
                    && p.id != caller_id
                    && p.online
                    && p.port != 0
                    && same_subnet(&p.ip, subnet, mask)
            })
            .map(|p| PeerEntry {
                id: p.id.clone(),
                name: p.name.clone(),
                ip: p.ip.clone(),
                port: p.port,
            })
            .collect()
    };

    Json(DiscoverResponse {
        id: own_id,
        name: own_name,
        port,
        ver: 1,
        peers,
    })
    .into_response()
}

/// 本机**实际监听**的端口：服务器在配置端口被占时会顺延到 +1..+10，报配置值会让对端
/// 把请求打进别的进程（或打到空处）。与 `/pair/info` 同一套算法。
fn listening_port(state: &AppState) -> u16 {
    let serving = state.server_port.load(std::sync::atomic::Ordering::SeqCst);
    if serving == 0 {
        state.config.lock().port
    } else {
        serving
    }
}

/// `Host: 172.16.0.7:3890` → `172.16.0.7`。取不出来（名称、IPv6 字面量、代理改写过的头）
/// 就回 `None`。
fn header_host_ip(headers: &HeaderMap) -> Option<Ipv4Addr> {
    let host = headers.get(header::HOST)?.to_str().ok()?;
    let host = host.trim_start_matches('[').trim_end_matches(']');
    // 端口是可选的：`ip` 与 `ip:port` 两种写法都要认。
    let host = match host.rsplit_once(':') {
        Some((head, tail)) if tail.parse::<u16>().is_ok() => head,
        _ => host,
    };
    host.parse().ok()
}

/// 本机哪块网卡上带着这个地址 —— 拿它的掩码，才知道"同一个网段"的边界在哪
/// （可能是 /24，也可能被管理员配成 /22、/16）。
fn netmask_for(ip: Ipv4Addr) -> Option<Ipv4Addr> {
    if_addrs::get_if_addrs()
        .ok()?
        .into_iter()
        .find_map(|i| match i.addr {
            if_addrs::IfAddr::V4(v4) if v4.ip == ip => Some(v4.netmask),
            _ => None,
        })
}

fn same_subnet(ip: &str, subnet: u32, mask: Ipv4Addr) -> bool {
    match ip.parse::<Ipv4Addr>() {
        Ok(v4) => u32::from(v4) & u32::from(mask) == subnet,
        Err(_) => false,
    }
}