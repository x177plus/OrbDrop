//! IPv4 网段（CIDR）：自报、解析、归属判定（F-107 §5）。
//!
//! 中转记忆要回答两个问题：「这台设备属于哪一段」与「这一跳的中转段是哪一段」。两者都
//! 靠**对端自报**（mDNS TXT `nets`、`/pair/info`、`/discover` 的 `nets` 与 `PeerEntry.cidr`）
//! —— 本机自己的掩码对远端网段不适用（对端可能是 /16，本机是 /24），算出来的"同段"是错的。
//!
//! 因此这里的形状带**掩码**而不是前缀长度：`(网络号, 掩码)` 两个 u32，判归属就是一次与运算。
//! 掩码取真实前缀（/24、/22、/16 都可能）—— 与 `discover::local_subnets`（挑拨号地址时的
//! 同段判定）是**同一套算理**，那边现在直接复用这里。
//!
//! **配置里一律存字符串**（`"192.168.1.0/24"`），不存这个类型：`Cidr` 若实现 `Deserialize`
//! 并在手改坏一行时解析失败，`config::load_config_at` 会把整份 `config.toml` 改名成
//! `.toml.bad` 再写默认值 —— 用户的共享项与配对表一起没了。宁可在这里按需 `parse` 失败即
//! 忽略这一条（最坏是那一台设备退回直连），也不让一份配置因为一个网段字符串整体作废。

use std::fmt;
use std::net::Ipv4Addr;
use std::str::FromStr;

/// 一个 IPv4 网段：网络号 + 掩码（都是主机字节序的 u32）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Cidr {
    net: u32,
    mask: u32,
}

impl Cidr {
    /// `(网络号, 掩码)`：网络号按掩码规整（多给的位清掉），所以 `10.0.1.7/24` 与
    /// `10.0.1.0/24` 相等 —— 对端把"我自己的地址"当网段报过来时也不会建出两条。
    pub fn new(net: u32, mask: u32) -> Cidr {
        Cidr {
            net: net & mask,
            mask,
        }
    }

    /// 一个地址 + 掩码 → 它所在的那一段。
    pub fn from_ip_mask(ip: Ipv4Addr, mask: Ipv4Addr) -> Cidr {
        Cidr::new(u32::from(ip), u32::from(mask))
    }

    /// 前缀长度（`/24` 的 24）→ 掩码。`0` 是合法的（`0.0.0.0/0` = 全部）。
    pub fn mask_for_prefix(prefix: u8) -> Option<u32> {
        if prefix > 32 {
            return None;
        }
        Some(if prefix == 0 {
            0
        } else {
            u32::MAX << (32 - u32::from(prefix))
        })
    }

    /// 地址 + 前缀长度。
    pub fn of(ip: Ipv4Addr, prefix: u8) -> Option<Cidr> {
        Some(Cidr::new(u32::from(ip), Self::mask_for_prefix(prefix)?))
    }

    pub fn network(&self) -> u32 {
        self.net
    }

    pub fn mask(&self) -> u32 {
        self.mask
    }

    /// 网络地址（`10.0.0.0`）。
    pub fn addr(&self) -> Ipv4Addr {
        Ipv4Addr::from(self.net)
    }

    /// 掩码里 1 的个数（`/24` 的 24）。掩码非连续时按 1 的个数算 —— 只用于显示回退。
    pub fn prefix(&self) -> u8 {
        self.mask.count_ones() as u8
    }

    /// 这个地址落在这一段里吗。
    pub fn contains(&self, ip: Ipv4Addr) -> bool {
        u32::from(ip) & self.mask == self.net
    }

    /// 两个段的网络号与掩码都一样（同一段）。
    pub fn same_as(&self, other: &Cidr) -> bool {
        self.net == other.net && self.mask == other.mask
    }

    /// `192.168.1.0/24`。掩码非连续（手改坏的配置）时输出输入时的原貌 —— 不假装规范。
    pub fn to_cidr_string(&self) -> String {
        format!("{}/{}", Ipv4Addr::from(self.net), self.prefix())
    }
}

impl fmt::Display for Cidr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.to_cidr_string())
    }
}

impl FromStr for Cidr {
    type Err = ();

    /// 接受 `192.168.1.0/24` 与 `192.168.1.7/24`（后者按掩码规整成 `.0`）。
    /// 不接受别的写法（域名、IPv6、裸地址）—— 这一段路上只认 IPv4 网段字面量。
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let (addr, prefix) = s.trim().split_once('/').ok_or(())?;
        let ip: Ipv4Addr = addr.trim().parse().map_err(|_| ())?;
        let prefix: u8 = prefix.trim().parse().map_err(|_| ())?;
        Cidr::of(ip, prefix).ok_or(())
    }
}

/// 解析一个网段字符串（失败即 `None`：调用方忽略这一条，绝不因此让整份配置作废）。
pub fn parse(s: &str) -> Option<Cidr> {
    s.parse().ok()
}

/// 一串网段字符串（协议里的 `nets` 字段：`"192.168.1.0/24,10.0.0.0/8"`）。
pub fn parse_list(s: &str) -> Vec<Cidr> {
    s.split(',')
        .map(str::trim)
        .filter(|part| !part.is_empty())
        .filter_map(|part| parse(part))
        .collect()
}

/// 拼回协议里的 `nets` 字符串。
pub fn format_list(list: &[Cidr]) -> String {
    list.iter()
        .map(|c| c.to_cidr_string())
        .collect::<Vec<_>>()
        .join(",")
}

/// 本机非回环 IPv4 网卡所在的网段（**自报给对端的 `nets`**，也是判"同段"的本机半边）。
///
/// 回环不算：对端报 `127.0.0.1`（安卓模拟器把本机回环别名拍过来）时那不是"本机网卡上的
/// 地址"。去掉重复（同一段上两块网卡只报一条）。
pub fn host_nets() -> Vec<Cidr> {
    let mut nets: Vec<Cidr> = if_addrs::get_if_addrs()
        .map(|list| {
            list.into_iter()
                .filter_map(|i| match i.addr {
                    if_addrs::IfAddr::V4(v4) if !v4.ip.is_loopback() => {
                        Some(Cidr::from_ip_mask(v4.ip, v4.netmask))
                    }
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();
    nets.sort_by_key(|c| (c.mask(), c.network()));
    nets.dedup_by(|a, b| a.same_as(b));
    nets
}

/// 本机自己的地址里，有哪一个落在 `cidr` 这一段（接收方校验反向记录时用：与本机无关的
/// 路由不该被采纳）。
pub fn own_ip_in(cidr: &Cidr) -> bool {
    host_nets().iter().any(|net| net.same_as(cidr))
}

/// 本机视角：`ip` 落在本机**哪块网卡**的段里（同网段）。跨段的地址返回 `None`
/// —— 那时只能靠对端自报的网段。
pub fn local_net_of(ip: Ipv4Addr) -> Option<Cidr> {
    host_nets().into_iter().find(|c| c.contains(ip))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cidr(s: &str) -> Cidr {
        s.parse().expect("测试里的网段字面量必须合法")
    }

    fn ip(s: &str) -> Ipv4Addr {
        s.parse().unwrap()
    }

    #[test]
    fn parses_and_normalizes() {
        assert_eq!(
            cidr("192.168.1.0/24"),
            Cidr::new(u32::from(ip("192.168.1.0")), u32::from(ip("255.255.255.0")))
        );
        // 给的是主机地址：按掩码规整，不建出第二条。
        assert_eq!(cidr("192.168.1.7/24"), cidr("192.168.1.0/24"));
        assert_eq!(cidr("10.1.2.3/8"), cidr("10.0.0.0/8"));
        assert_eq!(cidr("0.0.0.0/0").prefix(), 0);
        assert_eq!(cidr("10.0.0.0/32").mask(), u32::MAX);
    }

    #[test]
    fn rejects_junk_without_panicking() {
        for bad in [
            "",
            "192.168.1.0",
            "192.168.1.0/33",
            "192.168.1.0/",
            "example.com/24",
            "::1/128",
            "192.168.1.0/x",
        ] {
            assert!(parse(bad).is_none(), "{bad:?} 不该被解析成网段");
        }
    }

    #[test]
    fn membership_respects_the_real_mask() {
        let slash24 = cidr("10.0.0.0/24");
        assert!(slash24.contains(ip("10.0.0.255")));
        assert!(!slash24.contains(ip("10.0.1.1")));
        // /22 是真的跨过第三段的边界（管理员把掩码配小了也判得对）。
        let slash22 = cidr("10.0.0.0/22");
        assert!(slash22.contains(ip("10.0.3.7")));
        assert!(!slash22.contains(ip("10.0.4.1")));
    }

    #[test]
    fn list_roundtrip_skips_junk() {
        let list = parse_list(" 192.168.1.0/24 ,, 10.0.0.0/8 , nope ");
        assert_eq!(list.len(), 2);
        assert_eq!(format_list(&list), "192.168.1.0/24,10.0.0.0/8");
        assert!(parse_list("").is_empty());
    }

    #[test]
    fn host_nets_exclude_loopback_and_dedupe() {
        let nets = host_nets();
        assert!(
            !nets
                .iter()
                .any(|c| c.contains(ip("127.0.0.1"))
                    && c.mask() == Cidr::mask_for_prefix(8).unwrap()),
            "127.0.0.0/8 不该出现在本机网段表里: {nets:?}"
        );
        let mut keys: Vec<(u32, u32)> = nets.iter().map(|c| (c.network(), c.mask())).collect();
        let before = keys.len();
        keys.dedup();
        assert_eq!(keys.len(), before, "同一段不该报两条: {nets:?}");
    }
}
