//! 网络接口拓扑（roadmap/08 §5.4）：`GetAdaptersAddresses` + `GetIfTable2`。
//!
//! **只描述接口本身**（名字 / MAC / 速率 / MTU / 载波 / 驱动描述 / IP），
//! 实时吞吐是 `net.*` series 的事。
//!
//! # 为什么要两个 API
//!
//! 一个都不够：
//!
//! | 字段 | 来源 | 为什么不是另一个 |
//! |---|---|---|
//! | 接口名 / 描述 / MAC / MTU / IP 列表 | `GetAdaptersAddresses` | `MIB_IF_ROW2` 没有 IP 列表 |
//! | `carrier`（物理链路是否连通） | `GetIfTable2` 的 `MediaConnectState` | `IP_ADAPTER_ADDRESSES` 只有 `OperStatus`，那是**管理态 + 运行态**的合并结果，网线拔了但接口没禁用时它未必翻转 |
//! | `speed_mbps` | `GetIfTable2` 的 `TransmitLinkSpeed` | 与指标侧（同样走 `GetIfTable2`）同源，避免两处口径不一致 |
//!
//! 两张表用 `InterfaceIndex` / `IfIndex` 关联——同一个接口在两个 API 里是同一个序号。
//!
//! # 排除哪些接口
//!
//! 回环（`IF_TYPE_SOFTWARE_LOOPBACK`）与隧道（`IF_TYPE_TUNNEL`，Teredo / ISATAP
//! 这类自动隧道）。口径对应 Linux 侧的「排除 `lo` 与 `veth*`」：回环没有观测价值，
//! 隧道接口在 Windows 上会随网络环境自动增减，既撑大列表又无意义。
//!
//! **不**按名字前缀排除虚拟网卡（Hyper-V 的 `vEthernet (…)`、VPN 的 TAP）：
//! 它们承载真实流量，与 Linux 上 `br0` / `bond0` 会被列出是同一道理。
//!
//! # 拿不到的字段
//!
//! | 字段 | 原因 |
//! |---|---|
//! | `duplex` | Windows 的网络栈**不暴露双工状态**。NDIS 的 `OID_GEN_LINK_STATE` 里有 `MediaDuplexState`，但那是驱动级 OID，要发 IOCTL 给具体网卡驱动且需要管理员；`MIB_IF_ROW2` 与 `IP_ADAPTER_ADDRESSES` 都没有这一项。填 `None`，不拿「速率大于 0 就算全双工」这种推断冒充 |
//! | `driver` | Windows 没有「内核驱动名」这个用户可见标识（对应物是 `.sys` 文件名，只在设备管理器/注册表的驱动键里）。这里填**适配器描述串**（`Intel(R) Ethernet Connection I219-V`），它是 `MIB_IF_ROW2::Description` / `IP_ADAPTER_ADDRESSES::Description`，语义上最接近「这块网卡由谁驱动」 |
//!
//! # 接口名用 `FriendlyName`
//!
//! `NetInfo::name` 取 `FriendlyName`（「以太网」「Wi-Fi」「vEthernet (Default Switch)」），
//! 它与 `MIB_IF_ROW2::Alias` 是同一个串。**指标采集侧的 `iface` 标签必须用同一个**，
//! 否则拓扑与曲线对不上。另一个候选是 `AdapterName`（`{GUID}` 形态），那是稳定但
//! 不可读的标识，放进界面只会是一串十六进制。

use std::collections::BTreeMap;
use std::net::{Ipv4Addr, Ipv6Addr};

use strixmaid_types::system::NetInfo;

use crate::platform::windows::wide::from_wide_ptr;

/// 回环接口类型（`IF_TYPE_SOFTWARE_LOOPBACK`）。
const IF_TYPE_SOFTWARE_LOOPBACK: u32 = 24;
/// 隧道接口类型（`IF_TYPE_TUNNEL`）。
const IF_TYPE_TUNNEL: u32 = 131;

/// `GetAdaptersAddresses` 的首次缓冲大小。MSDN 建议 15 KiB 起步——
/// 接口多的机器会返回 `ERROR_BUFFER_OVERFLOW` 并回填实际所需大小，再试一次。
const INITIAL_BUFFER: usize = 15 * 1024;
/// 缓冲重试次数上限。接口表在两次调用之间可能变大，但不会无限变大。
const MAX_TRIES: usize = 4;

/// 是否排除这个接口，见模块文档。
pub fn is_excluded_iface(if_type: u32) -> bool {
    if_type == IF_TYPE_SOFTWARE_LOOPBACK || if_type == IF_TYPE_TUNNEL
}

/// 从 `GetAdaptersAddresses` 取到的一个适配器。
#[derive(Debug, Clone, PartialEq, Eq)]
struct Adapter {
    if_index: u32,
    name: String,
    description: String,
    mac: Option<String>,
    mtu: u32,
    if_type: u32,
    addrs: Vec<String>,
}

/// 从 `GetIfTable2` 取到的链路状态。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
struct LinkState {
    speed_mbps: Option<u32>,
    carrier: bool,
}

/// 枚举网络接口，按名字排序。任何一步失败都退化成空列表。
pub fn read_networks() -> Vec<NetInfo> {
    let links = if_table();
    let mut out: Vec<NetInfo> = adapters()
        .into_iter()
        .filter(|a| !is_excluded_iface(a.if_type))
        .map(|a| {
            let link = links.get(&a.if_index).copied().unwrap_or_default();
            NetInfo {
                name: a.name,
                mac: a.mac,
                speed_mbps: link.speed_mbps,
                // 见模块文档「拿不到的字段」
                duplex: None,
                mtu: a.mtu,
                carrier: link.carrier,
                driver: (!a.description.is_empty()).then_some(a.description),
                addrs: a.addrs,
            }
        })
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

/// 走一遍 `GetAdaptersAddresses` 的链表。
fn adapters() -> Vec<Adapter> {
    use windows_sys::Win32::Foundation::{ERROR_BUFFER_OVERFLOW, NO_ERROR};
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        GAA_FLAG_SKIP_ANYCAST, GAA_FLAG_SKIP_DNS_SERVER, GAA_FLAG_SKIP_MULTICAST,
        GetAdaptersAddresses, IP_ADAPTER_ADDRESSES_LH,
    };

    const AF_UNSPEC: u32 = 0;
    let flags = GAA_FLAG_SKIP_ANYCAST | GAA_FLAG_SKIP_MULTICAST | GAA_FLAG_SKIP_DNS_SERVER;

    let mut size = INITIAL_BUFFER as u32;
    for _ in 0..MAX_TRIES {
        // 缓冲里放的是含指针与 u64 的结构体，必须按 8 字节对齐；`Vec<u8>`
        // 只保证 1 字节对齐，所以用 `Vec<u64>` 占位。
        let mut buf: Vec<u64> = vec![0; size as usize / 8 + 1];
        let mut cap = (buf.len() * 8) as u32;
        // SAFETY: buf 有 cap 字节可写且按 8 字节对齐，cap 如实描述其大小；
        // reserved 按文档传空。
        let rc = unsafe {
            GetAdaptersAddresses(
                AF_UNSPEC,
                flags,
                std::ptr::null(),
                buf.as_mut_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>(),
                &raw mut cap,
            )
        };
        match rc {
            NO_ERROR => {
                // SAFETY: 调用成功，buf 首地址就是链表头；链表全部落在 buf 内，
                // 而 buf 在本函数返回前一直存活（walk 只在此期间解引用）。
                return unsafe { walk_adapters(buf.as_ptr().cast::<IP_ADAPTER_ADDRESSES_LH>()) };
            }
            ERROR_BUFFER_OVERFLOW => size = cap.max(size.saturating_mul(2)),
            // 没有接口（ERROR_NO_DATA）或别的错误：如实返回空表。
            _ => return Vec::new(),
        }
    }
    Vec::new()
}

/// 遍历适配器链表。
///
/// # Safety
///
/// `head` 必须是 `GetAdaptersAddresses` 成功回填的缓冲首地址，且该缓冲在本次
/// 调用期间保持有效。
unsafe fn walk_adapters(
    head: *const windows_sys::Win32::NetworkManagement::IpHelper::IP_ADAPTER_ADDRESSES_LH,
) -> Vec<Adapter> {
    let mut out = Vec::new();
    let mut cur = head;
    while !cur.is_null() {
        // SAFETY: cur 非空且指向缓冲内一条完整的适配器记录。
        let a = unsafe { std::ptr::read_unaligned(cur) };
        // SAFETY: 两个字段都是缓冲内以 NUL 结尾的 UTF-16 串（可能为空指针）。
        let name = unsafe { from_wide_ptr(a.FriendlyName) };
        // SAFETY: 同上。
        let description = unsafe { from_wide_ptr(a.Description) };
        // SAFETY: Anonymous1 是一个含 Length/IfIndex 的联合，读它的具名分支是
        // windows-sys 规定的用法（另一分支只是对齐用的 u64）。
        let if_index = unsafe { a.Anonymous1.Anonymous.IfIndex };
        out.push(Adapter {
            if_index,
            name,
            description,
            mac: format_mac(&a.PhysicalAddress, a.PhysicalAddressLength as usize),
            mtu: a.Mtu,
            if_type: a.IfType,
            // SAFETY: 单播地址链表同样落在同一缓冲内。
            addrs: unsafe { walk_unicast(a.FirstUnicastAddress) },
        });
        cur = a.Next;
    }
    out
}

/// 遍历一个适配器的单播地址链表。
///
/// # Safety
///
/// `head` 必须来自 `GetAdaptersAddresses` 回填的缓冲，且该缓冲仍然有效。
unsafe fn walk_unicast(
    head: *const windows_sys::Win32::NetworkManagement::IpHelper::IP_ADAPTER_UNICAST_ADDRESS_LH,
) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = head;
    while !cur.is_null() {
        // SAFETY: cur 非空且指向缓冲内一条完整的单播地址记录。
        let u = unsafe { std::ptr::read_unaligned(cur) };
        let sa = u.Address.lpSockaddr;
        let len = u.Address.iSockaddrLength.max(0) as usize;
        if !sa.is_null() && len >= 4 {
            // SAFETY: lpSockaddr 指向缓冲内 len 字节的 sockaddr，len 由内核给出。
            let raw = unsafe { std::slice::from_raw_parts(sa.cast::<u8>(), len) };
            if let Some(ip) = sockaddr_to_ip(raw) {
                out.push(ip);
            }
        }
        cur = u.Next;
    }
    out
}

/// `sockaddr` 字节 → IP 字符串。纯函数，可用固定字节单测。
///
/// 只认 `AF_INET` 与 `AF_INET6`；其它地址族（AF_LINK 之类）返回 `None`。
/// IPv6 不带 `%zone` 后缀——`sin6_scope_id` 只对链路本地地址有意义，
/// 而 DTO 这一项是「该接口上配置的 IP 地址」，接口本身已经在 `name` 里了。
pub fn sockaddr_to_ip(raw: &[u8]) -> Option<String> {
    /// `AF_INET`
    const AF_INET: u16 = 2;
    /// `AF_INET6`
    const AF_INET6: u16 = 23;

    let family = u16::from_ne_bytes(raw.get(0..2)?.try_into().ok()?);
    match family {
        // sockaddr_in：family(2) + port(2) + addr(4)
        AF_INET => {
            let b: [u8; 4] = raw.get(4..8)?.try_into().ok()?;
            Some(Ipv4Addr::from(b).to_string())
        }
        // sockaddr_in6：family(2) + port(2) + flowinfo(4) + addr(16) + scope_id(4)
        AF_INET6 => {
            let b: [u8; 16] = raw.get(8..24)?.try_into().ok()?;
            Some(Ipv6Addr::from(b).to_string())
        }
        _ => None,
    }
}

/// 物理地址字节 → `aa:bb:cc:dd:ee:ff`。
///
/// 长度为 0（虚拟接口）或全零（未初始化）时返回 `None`，口径与 Linux 侧
/// 过滤 `00:00:00:00:00:00` 一致。纯函数。
pub fn format_mac(bytes: &[u8], len: usize) -> Option<String> {
    let len = len.min(bytes.len());
    if len == 0 || bytes[..len].iter().all(|b| *b == 0) {
        return None;
    }
    Some(
        bytes[..len]
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect::<Vec<_>>()
            .join(":"),
    )
}

/// 链路速率（bit/s）→ Mb/s。
///
/// `u64::MAX` 是 NDIS 规定的「未知」，0 是「没有链路」，两者都给 `None`——
/// 这一项的唯一用途是把吞吐归一成利用率，给个假值不如不给。纯函数。
pub fn speed_to_mbps(bits_per_sec: u64) -> Option<u32> {
    if bits_per_sec == 0 || bits_per_sec == u64::MAX {
        return None;
    }
    u32::try_from(bits_per_sec / 1_000_000).ok().filter(|v| *v > 0)
}

/// `GetIfTable2`：接口序号 → 链路状态。
fn if_table() -> BTreeMap<u32, LinkState> {
    use windows_sys::Win32::Foundation::NO_ERROR;
    use windows_sys::Win32::NetworkManagement::IpHelper::{
        FreeMibTable, GetIfTable2, MIB_IF_ROW2, MIB_IF_TABLE2,
    };
    use windows_sys::Win32::NetworkManagement::Ndis::MediaConnectStateConnected;

    let mut out = BTreeMap::new();
    let mut table: *mut MIB_IF_TABLE2 = std::ptr::null_mut();
    // SAFETY: table 是输出参数；成功时由系统分配一块需要 FreeMibTable 归还的缓冲。
    let rc = unsafe { GetIfTable2(&raw mut table) };
    if rc != NO_ERROR || table.is_null() {
        return out;
    }

    // SAFETY: 调用成功，table 指向一块完整的 MIB_IF_TABLE2。
    let count = unsafe { (*table).NumEntries } as usize;
    // SAFETY: Table 是 MIB_IF_TABLE2 尾部的变长数组，取它的首元素地址。
    let rows = unsafe { (&raw const (*table).Table).cast::<MIB_IF_ROW2>() };
    for i in 0..count {
        // SAFETY: i < NumEntries，第 i 项完整落在系统分配的缓冲内。
        let row = unsafe { std::ptr::read_unaligned(rows.add(i)) };
        out.insert(
            row.InterfaceIndex,
            LinkState {
                speed_mbps: speed_to_mbps(row.TransmitLinkSpeed),
                carrier: row.MediaConnectState == MediaConnectStateConnected,
            },
        );
    }

    // SAFETY: table 由 GetIfTable2 分配，按文档用 FreeMibTable 归还，只归还一次。
    unsafe { FreeMibTable(table.cast::<core::ffi::c_void>()) };
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 排除规则() {
        assert!(is_excluded_iface(IF_TYPE_SOFTWARE_LOOPBACK));
        assert!(is_excluded_iface(IF_TYPE_TUNNEL));
        assert!(!is_excluded_iface(6), "以太网");
        assert!(!is_excluded_iface(71), "802.11 无线");
        assert!(!is_excluded_iface(53), "虚拟网卡（Hyper-V vEthernet）照列");
    }

    #[test]
    fn mac_格式化() {
        assert_eq!(
            format_mac(&[0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff, 0, 0], 6).as_deref(),
            Some("aa:bb:cc:dd:ee:ff")
        );
        // 长度为 0 的虚拟接口
        assert_eq!(format_mac(&[0; 8], 0), None);
        // 全零地址视为「没有」
        assert_eq!(format_mac(&[0; 8], 6), None);
        // 声称的长度超出缓冲时按缓冲长度截断，不越界
        assert_eq!(format_mac(&[1, 2], 6).as_deref(), Some("01:02"));
        // InfiniBand 的硬件地址有 20 字节，照样格式化
        assert_eq!(format_mac(&[1; 20], 20).unwrap().split(':').count(), 20);
    }

    #[test]
    fn 速率换算() {
        assert_eq!(speed_to_mbps(1_000_000_000), Some(1000));
        assert_eq!(speed_to_mbps(10_000_000_000), Some(10_000));
        assert_eq!(speed_to_mbps(100_000_000), Some(100));
        // NDIS 的「未知」与「无链路」都不编值
        assert_eq!(speed_to_mbps(u64::MAX), None);
        assert_eq!(speed_to_mbps(0), None);
        // 不足 1 Mb/s 的链路（老式拨号）取整成 0，按未知处理
        assert_eq!(speed_to_mbps(56_000), None);
    }

    #[test]
    fn sockaddr_解析() {
        // sockaddr_in：AF_INET(2) + port + 192.168.1.10
        let mut v4 = vec![0u8; 16];
        v4[0..2].copy_from_slice(&2u16.to_ne_bytes());
        v4[4..8].copy_from_slice(&[192, 168, 1, 10]);
        assert_eq!(sockaddr_to_ip(&v4).as_deref(), Some("192.168.1.10"));

        // sockaddr_in6：AF_INET6(23) + port + flowinfo + fe80::1
        let mut v6 = vec![0u8; 28];
        v6[0..2].copy_from_slice(&23u16.to_ne_bytes());
        v6[8] = 0xfe;
        v6[9] = 0x80;
        v6[23] = 1;
        assert_eq!(sockaddr_to_ip(&v6).as_deref(), Some("fe80::1"));

        // 未知地址族
        let mut other = vec![0u8; 16];
        other[0..2].copy_from_slice(&17u16.to_ne_bytes());
        assert_eq!(sockaddr_to_ip(&other), None);
        // 缓冲短得读不出地址：返回 None 而不是 panic
        assert_eq!(sockaddr_to_ip(&[2, 0, 0, 0]), None);
        assert_eq!(sockaddr_to_ip(&[]), None);
    }

    #[test]
    fn 本机接口枚举() {
        let nets = read_networks();
        // 任何联网或没联网的 Windows 都至少有一块网卡（哪怕是禁用的）；
        // 但在极简容器里可能真的一个都没有，所以只在非空时断言形状。
        for n in &nets {
            assert!(!n.name.is_empty(), "接口名不能为空");
            assert_eq!(n.duplex, None, "Windows 不暴露双工状态");
            if let Some(mac) = &n.mac {
                assert!(
                    mac.split(':').count() >= 6,
                    "MAC 至少六段：{mac}"
                );
                assert!(mac.chars().all(|c| c.is_ascii_hexdigit() || c == ':'));
            }
            for a in &n.addrs {
                assert!(
                    a.parse::<std::net::IpAddr>().is_ok(),
                    "不是合法 IP：{a}"
                );
            }
            if let Some(s) = n.speed_mbps {
                assert!(s > 0);
            }
        }
        // 必须按名字排序
        let names: Vec<&str> = nets.iter().map(|n| n.name.as_str()).collect();
        let mut sorted = names.clone();
        sorted.sort_unstable();
        assert_eq!(names, sorted, "必须按接口名排序");
        eprintln!(
            "本机网络接口：{}",
            serde_json::to_string(&nets).unwrap()
        );
    }

    /// 回环一定被排除：本机必有 `Loopback Pseudo-Interface`，它不该出现在结果里。
    #[test]
    fn 回环被排除() {
        let nets = read_networks();
        assert!(
            !nets.iter().any(|n| n.name.to_lowercase().contains("loopback")),
            "回环接口没被排除：{:?}",
            nets.iter().map(|n| &n.name).collect::<Vec<_>>()
        );
    }
}
