//! 网络接口拓扑（roadmap/08 §5.4）：`/sys/class/net/*` + `getifaddrs`。
//!
//! **只描述接口本身**（MAC / 速率 / MTU / 载波 / 驱动 / IP）——实时吞吐是
//! `net.*` series。排除 `lo` 与 `veth*`，与指标采集口径一致：容器每起一个就多
//! 一对随机名 veth，既撑大列表又无观测价值。

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use nix::sys::socket::{AddressFamily, SockaddrLike};
use strixmaid_types::system::NetInfo;

use super::util::{read_bool, read_trimmed};

const NET_PATH: &str = "/sys/class/net";

/// 不采集的接口（与 `metrics/collect/linux/net.rs` 同口径）。
pub fn is_excluded_iface(name: &str) -> bool {
    name == "lo" || name.starts_with("veth")
}

/// 枚举网络接口。
pub fn read_networks() -> Vec<NetInfo> {
    let Ok(rd) = fs::read_dir(NET_PATH) else {
        return Vec::new();
    };
    let addrs = interface_addrs();
    let mut ifaces: Vec<String> = rd
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .filter(|n| !is_excluded_iface(n))
        .collect();
    ifaces.sort();
    ifaces
        .into_iter()
        .map(|name| read_iface(&name, addrs.get(&name).cloned().unwrap_or_default()))
        .collect()
}

fn read_iface(name: &str, addrs: Vec<String>) -> NetInfo {
    let dir = Path::new(NET_PATH).join(name);
    NetInfo {
        name: name.to_owned(),
        mac: read_trimmed(dir.join("address")).filter(|m| m != "00:00:00:00:00:00"),
        // speed 在虚拟 / 断链 / 无线接口上常报 -1 或读取报错。
        speed_mbps: read_trimmed(dir.join("speed"))
            .and_then(|v| v.parse::<i64>().ok())
            .filter(|s| *s > 0)
            .map(|s| s as u32),
        duplex: read_trimmed(dir.join("duplex")).filter(|d| d != "unknown"),
        mtu: read_trimmed(dir.join("mtu"))
            .and_then(|v| v.parse().ok())
            .unwrap_or(0),
        // carrier 在接口 down 时读取会报 EINVAL，视为无载波。
        carrier: read_bool(dir.join("carrier")).unwrap_or(false),
        driver: fs::read_link(dir.join("device/driver"))
            .ok()
            .and_then(|t| t.file_name().map(|n| n.to_string_lossy().into_owned())),
        addrs,
    }
}

/// 用 `getifaddrs` 建「接口名 → IP 列表」。只取 IPv4 / IPv6，去掉端口。
fn interface_addrs() -> BTreeMap<String, Vec<String>> {
    let mut map: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let Ok(iter) = nix::ifaddrs::getifaddrs() else {
        return map;
    };
    for ifa in iter {
        let Some(sa) = ifa.address else { continue };
        let ip = match sa.family() {
            Some(AddressFamily::Inet) => sa.as_sockaddr_in().map(|s| s.ip().to_string()),
            Some(AddressFamily::Inet6) => sa.as_sockaddr_in6().map(|s| s.ip().to_string()),
            _ => None,
        };
        if let Some(ip) = ip {
            map.entry(ifa.interface_name).or_default().push(ip);
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 排除规则() {
        assert!(is_excluded_iface("lo"));
        assert!(is_excluded_iface("veth1a2b"));
        assert!(!is_excluded_iface("eth0"));
        assert!(!is_excluded_iface("enp3s0"));
        assert!(!is_excluded_iface("wlan0"));
    }

    #[test]
    fn 本机枚举不报错且形状合理() {
        let nets = read_networks();
        // 本机通常至少有一个真实接口（排除 lo 后可能为空，容器里也可能空）。
        for n in &nets {
            assert!(!is_excluded_iface(&n.name));
            if let Some(mac) = &n.mac {
                assert_eq!(mac.split(':').count(), 6, "MAC 应是六段：{mac}");
            }
            for a in &n.addrs {
                assert!(!a.is_empty());
            }
        }
    }
}
