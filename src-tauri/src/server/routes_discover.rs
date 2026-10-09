//! 跨网段发现（F-10 的延伸）。
//!
//! mDNS 只在同一个 L2 段里有效 —— 组播包按 RFC 6762 要求 IP TTL=255，接收端还校验它，
//! 任何被路由器转发过的包都会被丢掉。所以**跨网段**的设备，mDNS 永远看不见。但**单播**
//! 是通的：手动添加 `IP:端口` 能连上就证明了这一点。
//!
//! 于是跨段那半边换个传输：**已配对的两台设备，各自把自己 mDNS 视野里的同伴报给对方**
//! —— 一台锚点换回一整个网段，之后本机直连那些设备（不经过锚点转发）。
//!
//! 四条边界决定了它必须长这样：
//!
//! - **要 token**：只有已配对才拿得到表。没交换过凭据的设备只能看见本机自己
//!   （`/pair/info`），看不到本机认识了谁。这既是安全边界，也正是"匹配上之后才发现"。
//! - **整份视野，不按拨号地址划段**：本机多网卡（有线 + 无线 + 热点 + VPN）时，每一块
//!   网卡上看到的设备都要报出去。只回"呼叫方拨过来的那个地址所在的段"是旧的写法，
//!   它让一台锚点只能换回**一个**网段：对端明明还有一块网卡看得见别的设备，那些设备却
//!   永远换不过来（用户报的就是这个）。要哪一段就都给它，由拿到表的那一方自己判断
//!   可达性（`state::probe_*`，以及"本机压根不会去拨学来的行"这条）。
//! - **只回本机自己发现的**：表里的行如果不是本机 mDNS 看到的、而是**别台设备转述**来的
//!   （`state.learned`），就不再往外报 —— 发现只走单播、**不做多跳**：不然 A 从 B 学到的
//!   东西会被 C 再学一遍，链路上就是"一台锚点自动蔓延到整张网"，与 F-11a 的边界不符。
//!   要看第三段就再与那一段的某台配对。
//! - **只回在线的**：这一层只回答"本机此刻看得见谁"（mDNS 视野本来就是实时的）。

use std::collections::HashSet;
use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::{Extension, Json};
use serde::{Deserialize, Serialize};
use tracing::debug;

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
    /// F-107：**报出它的那一方**认为它属于哪一段（`192.168.1.0/24`）。
    ///
    /// 由报出方填：它只报自己 L2 视野里的设备（F-11a 的"只回本机自己发现的"），那些设备
    /// 必然落在它某块网卡的段里 —— 但**跨网段那一侧只有对端能说**（本机的掩码对远端网段
    /// 不适用），所以这一格是"这一段是哪一段"的权威来源。算不出来时省略（老版本读成未知）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cidr: Option<String>,
    /// F-107：它是否允许替别人中转（报出方所知：`/pair/info`、mDNS TXT 或它自己的探活）。
    /// 老版本不报 = `false`（与 S-09 默认关一致）。收到的一侧据此构造"中转池"。
    #[serde(default)]
    pub relay: bool,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct DiscoverResponse {
    pub id: String,
    pub name: String,
    pub port: u16,
    pub ver: u32,
    /// 本机此刻是否愿意替别人中转（F-106，设置项 S-09）。与 `/pair/info` 的 `relay` 同义：
    /// 请求方靠它挑中转方（已配对的对端每 5s 走这条探活，能力位跟着这一拍刷新）。
    pub relay: bool,
    /// F-107：本机**自报**的网段（`192.168.1.0/24,10.0.0.0/8`）。跨网段时"这台设备属于哪
    /// 一段"只能由它自己说 —— 收到的一侧据此把设备分到各段、构造中转池。
    #[serde(default)]
    pub nets: String,
    pub peers: Vec<PeerEntry>,
}

/// GET /discover — 已配对对端来要「本机此刻看得见的设备」。
///
/// 失败路径都要想清楚：**本机离线（隐身）时**由 `presence_middleware` 拦成 503，
/// 对端据此判本机不在网上（与探活同一套语义）；**没带/带错 token** 由 `auth_middleware`
/// 拦成 401 —— 对端会退回 `/pair/info` 去问"它还认不认我们"，这一问不带 token 也答。
pub async fn discover(
    State(state): State<Arc<AppState>>,
    Extension(caller): Extension<Caller>,
) -> Response {
    // 本机 UI（回环）没有"从哪个网段拨进来"这回事，也不需要这张表：它读共享项走 /shares。
    let Caller::Peer(caller_id) = caller else {
        return (StatusCode::FORBIDDEN, "discover 只对已配对对端开放").into_response();
    };

    let (own_id, own_name, allow_relay) = {
        let config = state.config.lock();
        (config.device_id.clone(), config.device_name.clone(), config.allow_relay)
    };
    let port = listening_port(&state);

    // 「学来的」行不进表（发现只走单播、不做多跳）。**先锁 `learned` 再锁 `peers`**：
    // `state::learn_peers` 就是那个顺序，反着来是白送一个死锁面。
    let learned: HashSet<String> = state.learned.lock().keys().cloned().collect();

    let peers: Vec<PeerEntry> = {
        let peers = state.peers.lock();
        peers
            .iter()
            .filter(|p| {
                p.id != own_id
                    && p.id != caller_id
                    && p.online
                    && p.port != 0
                    && !p.ip.is_empty()
                    && !learned.contains(&p.id)
            })
            .map(|p| PeerEntry {
                id: p.id.clone(),
                name: p.name.clone(),
                ip: p.ip.clone(),
                port: p.port,
                // F-107：这一段是哪一段 —— 对端自报的优先，否则按本机掩码算（同段场景）。
                cidr: state.cidr_of(&p.id, &p.ip),
                // 它允不允许中转：本机从 `/pair/info` / TXT 学到的（5s 探活刷新）。
                relay: state.is_relay_capable(&p.id),
            })
            .collect()
    };

    // 每 5s 一拍都会走这里（长连接活着也走），记成 debug：info 会把日志刷满。
    debug!(caller = %caller_id, peers = peers.len(), "discover: 报出本机视野");

    Json(DiscoverResponse {
        id: own_id,
        name: own_name,
        port,
        // F-107 起表里多了 `cidr`/`relay`、回话里多了 `nets`。老对端不读这些字段（serde
        // 忽略未知键），新对端按 `ver >= 2` 记日志用。
        ver: 2,
        relay: allow_relay,
        nets: crate::subnet::format_list(&crate::subnet::host_nets()),
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