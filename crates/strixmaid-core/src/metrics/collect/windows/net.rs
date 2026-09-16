//! 网络：`GetIfTable2` → 每接口的收发速率（两轮差分）。
//!
//! # 为什么是 `GetIfTable2` 而不是 `GetIfTable`
//!
//! 老的 `GetIfTable` 返回 `MIB_IFROW`，里面的 `dwInOctets` / `dwOutOctets` 是
//! **32 位**：万兆网卡上约 3.5 秒就绕一圈，差分出来的速率全是噪声。
//! `GetIfTable2`（Vista+）返回 `MIB_IF_ROW2`，收发字节与各类错误计数全是 64 位，
//! 在任何现实速率下都不会回绕。这与 macOS 侧不用 `getifaddrs`、改用
//! `NET_RT_IFLIST2` 是同一个理由。
//!
//! 也没走 PDH 的 `\Network Interface(*)\Bytes Received/sec`：那条计数器的实例名
//! 是把接口描述里的 `(` `)` `#` `/` 替换过的「PDH 化」名字，与任何别处显示的
//! 接口名都对不上，且速率由 PDH 替我们算（要两轮采集），不如自己拿原始计数差分。
//!
//! # 标签用 `Alias` 而不是 `Description`
//!
//! `MIB_IF_ROW2` 有三个名字：
//!
//! | 字段 | 例子 | 是否适合当标签 |
//! |---|---|---|
//! | `Alias` | `以太网`、`Wi-Fi`、`vEthernet (Default Switch)` | ✅ 用户在「网络连接」里看到的那个名字，可改名，同一台机器上唯一 |
//! | `Description` | `Intel(R) Ethernet Connection I219-V` | ❌ 是网卡型号，两张同型号网卡会重名 |
//! | `InterfaceGuid` | `{3A5B1C...}` | ❌ 唯一但没人认得，换驱动还会变 |
//!
//! 取 `Alias`，与 Linux 的 `eth0` / macOS 的 `en0` 一样是「人能认出来的接口名」。
//! 极端情况下两个接口的 `Alias` 相同（罕见，系统一般不允许）时按后来者覆盖，
//! 保证同一轮里不会出现两条同名 series。
//!
//! # 过滤与合并项
//!
//! - 排除回环（`Type == IF_TYPE_SOFTWARE_LOOPBACK`）。
//! - **排除 NDIS 过滤层接口**（`InterfaceAndOperStatusFlags.FilterInterface`）。
//!   `GetIfTable2` 把每个挂在网卡上的 NDIS 轻量过滤驱动也当成一个接口返回，
//!   于是一块物理网卡会变出四五行：
//!
//!   ```text
//!   以太网                                         rx=19827  tx=14273
//!   以太网-QoS Packet Scheduler-0000               rx=19827  tx=14273
//!   以太网-WFP 802.3 MAC Layer LightWeight Filter-0000   rx=19827  tx=14273
//!   以太网-WFP Native MAC Layer LightWeight Filter-0000  rx=19827  tx=14273
//!   ```
//!
//!   这些行的计数**是同一批字节在不同层上再数一遍**，不是另外的流量。留着它们，
//!   一块网卡会画出四条一模一样的曲线，「全网卡求和」还会把吞吐虚报成四倍。
//!   本机实测：不过滤 54 个接口，过滤后 22 个。
//! - 其余接口（物理网卡、Hyper-V 虚拟交换机、WSL、VPN 与 6to4/Teredo 隧道）
//!   全部保留：哪些有意义取决于机器角色，采集端不替使用者做判断——
//!   与 Linux / macOS 版一致。
//! - `net.errors`（roadmap/08 §4.2 的合并项）= `InErrors + OutErrors +
//!   InDiscards + OutDiscards`。**Windows 侧四个计数齐全**，不像 macOS 缺发送
//!   方向的丢包，所以这条曲线在语义上与 Linux 版完全等价。
//!
//! 首轮只建基线不产出；任一计数器回退（接口被重建、驱动重载）时跳过该接口本轮。

use std::collections::HashMap;
use std::time::Instant;

use windows_sys::Win32::Foundation::ERROR_SUCCESS;
use windows_sys::Win32::NetworkManagement::IpHelper::{
    FreeMibTable, GetIfTable2, IF_TYPE_SOFTWARE_LOOPBACK, MIB_IF_ROW2, MIB_IF_TABLE2,
};

use super::{CollectError, Collector, Sample, elapsed_secs, rate, sanitize_label};
use crate::metrics::catalog::{self as cat, label};
use crate::platform::windows::wide::from_wide_nul;

/// 一个接口的累计计数。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IfCounters {
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    pub rx_errors: u64,
    pub tx_errors: u64,
    pub rx_discards: u64,
    pub tx_discards: u64,
}

/// 一轮全部接口的计数，键为接口别名。
pub type IfSnapshot = HashMap<String, IfCounters>;

/// `MIB_IF_ROW2::InterfaceAndOperStatusFlags` 里 `FilterInterface` 那一位。
///
/// `netioapi.h` 把这个字段声明成一串 1 位的 `boolean`，MSVC 从最低位开始分配：
///
/// ```text
/// bit 0  HardwareInterface
/// bit 1  FilterInterface      ← 本常量
/// bit 2  ConnectorPresent
/// bit 3  NotAuthenticated
/// bit 4  NotMediaConnected
/// bit 5  Paused
/// bit 6  LowPower
/// bit 7  EndPointInterface
/// ```
///
/// `windows-sys` 把整个位域投影成一个 `_bitfield: u8`，没有给出取位的方法，
/// 所以在这里自己定义。
const FLAG_FILTER_INTERFACE: u8 = 1 << 1;

/// 是否采集这个接口。
///
/// `if_type` 是 IANA 的 `ifType`，`flags` 是 `InterfaceAndOperStatusFlags` 的位域字节。
pub fn should_collect(if_type: u32, flags: u8) -> bool {
    if_type != IF_TYPE_SOFTWARE_LOOPBACK && flags & FLAG_FILTER_INTERFACE == 0
}

/// `FreeMibTable` 的 RAII。
struct TableGuard(*mut MIB_IF_TABLE2);

impl Drop for TableGuard {
    fn drop(&mut self) {
        // SAFETY: 指针由 GetIfTable2 成功返回，只释放一次。
        unsafe {
            FreeMibTable(self.0.cast::<core::ffi::c_void>());
        }
    }
}

/// 读一轮 `GetIfTable2`。
pub fn read_interfaces() -> std::io::Result<IfSnapshot> {
    let mut table: *mut MIB_IF_TABLE2 = std::ptr::null_mut();
    // SAFETY: table 是输出参数，函数成功时把一块自己分配的内存的地址写进去，
    // 所有权转移给我们，下面的 TableGuard 负责 FreeMibTable。
    let rc = unsafe { GetIfTable2(&raw mut table) };
    if rc != ERROR_SUCCESS || table.is_null() {
        return Err(crate::platform::windows::error_from_code(rc));
    }
    let _guard = TableGuard(table);

    // SAFETY: table 非空且指向一个完整的 MIB_IF_TABLE2。
    let count = unsafe { (*table).NumEntries } as usize;
    // SAFETY: Table 是声明成 [MIB_IF_ROW2; 1] 的变长数组头，内核在其后紧接着放了
    // NumEntries 项；取字段地址不解引用，转成切片后按 count 项访问，正是该 API 的约定。
    let rows = unsafe {
        std::slice::from_raw_parts((&raw const (*table).Table).cast::<MIB_IF_ROW2>(), count)
    };

    let mut out = IfSnapshot::with_capacity(count);
    for row in rows {
        if !should_collect(row.Type, row.InterfaceAndOperStatusFlags._bitfield) {
            continue;
        }
        let name = from_wide_nul(&row.Alias);
        if name.is_empty() {
            continue;
        }
        out.insert(
            name,
            IfCounters {
                rx_bytes: row.InOctets,
                tx_bytes: row.OutOctets,
                rx_errors: row.InErrors,
                tx_errors: row.OutErrors,
                rx_discards: row.InDiscards,
                tx_discards: row.OutDiscards,
            },
        );
    }
    Ok(out)
}

/// 网络采集器。持有上一轮计数用于差分。
#[derive(Debug, Default)]
pub struct NetCollector {
    prev: Option<(Instant, IfSnapshot)>,
}

impl NetCollector {
    pub fn new() -> Self {
        NetCollector { prev: None }
    }

    /// 喂入一轮计数，产出与上一轮的速率样本；第一轮返回空。
    ///
    /// 接口在两轮之间消失、或计数器倒退（接口被重建）时跳过该接口本轮的样本。
    pub fn ingest(&mut self, now: Instant, snapshot: IfSnapshot) -> Vec<Sample> {
        let mut out = Vec::new();
        if let Some((prev_at, prev)) = &self.prev {
            let secs = elapsed_secs(*prev_at, now);
            for (name, cur) in &snapshot {
                let Some(p) = prev.get(name) else { continue };
                let iface = sanitize_label(name);
                let rx = rate(p.rx_bytes, cur.rx_bytes, secs);
                let tx = rate(p.tx_bytes, cur.tx_bytes, secs);
                // 合并项（roadmap/08 §4.2）：收发错误 + 收发丢包，四个计数都有。
                let errors = [
                    (p.rx_errors, cur.rx_errors),
                    (p.tx_errors, cur.tx_errors),
                    (p.rx_discards, cur.rx_discards),
                    (p.tx_discards, cur.tx_discards),
                ]
                .iter()
                .map(|(a, b)| rate(*a, *b, secs))
                .try_fold(0.0, |acc, r| r.map(|v| acc + v));
                // 任一计数器回退说明接口被重建，整组跳过。
                if let (Some(rx), Some(tx), Some(errors)) = (rx, tx, errors) {
                    out.extend([
                        Sample::labeled(cat::NET_RX_BYTES, label::IFACE, iface.clone(), rx),
                        Sample::labeled(cat::NET_TX_BYTES, label::IFACE, iface.clone(), tx),
                        Sample::labeled(cat::NET_ERRORS, label::IFACE, iface, errors),
                    ]);
                }
            }
        }
        self.prev = Some((now, snapshot));
        out
    }
}

impl Collector for NetCollector {
    fn name(&self) -> &'static str {
        "net"
    }

    fn collect(&mut self, now: Instant) -> Result<Vec<Sample>, CollectError> {
        let snapshot = read_interfaces()
            .map_err(|e| CollectError::new(self.name(), format!("GetIfTable2 失败: {e}")))?;
        Ok(self.ingest(now, snapshot))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn snap(name: &str, rx: u64, tx: u64) -> IfSnapshot {
        let mut m = IfSnapshot::new();
        m.insert(
            name.to_owned(),
            IfCounters {
                rx_bytes: rx,
                tx_bytes: tx,
                ..Default::default()
            },
        );
        m
    }

    /// `HardwareInterface | ConnectorPresent`，一块真网卡的典型位域。
    const HARDWARE: u8 = 0b0000_0101;

    #[test]
    fn 回环与过滤层接口都被排除() {
        // 6 = IF_TYPE_ETHERNET_CSMACD，71 = IEEE80211
        assert!(should_collect(6, HARDWARE));
        assert!(should_collect(71, HARDWARE));
        // 虚拟接口（Hyper-V 虚拟交换机、隧道）不带 HardwareInterface 位，照采
        assert!(should_collect(6, 0));
        assert!(!should_collect(IF_TYPE_SOFTWARE_LOOPBACK, HARDWARE));
        // NDIS 过滤层：计数与它下面那块网卡逐字节相同，留着就是四条一样的曲线
        assert!(!should_collect(6, FLAG_FILTER_INTERFACE));
        assert!(!should_collect(6, HARDWARE | FLAG_FILTER_INTERFACE));
    }

    #[test]
    fn 两轮差分出速率() {
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_secs(2);
        let mut c = NetCollector::new();
        assert!(
            c.ingest(t0, snap("以太网", 1000, 2000)).is_empty(),
            "第一轮无基线"
        );
        let out = c.ingest(t1, snap("以太网", 3000, 2000));
        assert_eq!(out.len(), 3);
        let rx = out.iter().find(|s| s.metric == cat::NET_RX_BYTES).unwrap();
        assert_eq!(rx.value, 1000.0, "2000 字节 / 2 秒");
        assert_eq!(rx.labels, vec![(label::IFACE, "以太网".to_string())]);
        // tx 没变，速率为 0 但仍然产出（0 是有效观测值，不是缺失）
        let tx = out.iter().find(|s| s.metric == cat::NET_TX_BYTES).unwrap();
        assert_eq!(tx.value, 0.0);
        let errs = out.iter().find(|s| s.metric == cat::NET_ERRORS).unwrap();
        assert_eq!(errs.value, 0.0);
    }

    #[test]
    fn 异常包是四个计数之和() {
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_secs(1);
        let mut c = NetCollector::new();
        let mut a = IfSnapshot::new();
        a.insert("以太网".into(), IfCounters::default());
        c.ingest(t0, a);
        let mut b = IfSnapshot::new();
        b.insert(
            "以太网".into(),
            IfCounters {
                rx_errors: 1,
                tx_errors: 2,
                rx_discards: 4,
                tx_discards: 8,
                ..Default::default()
            },
        );
        let out = c.ingest(t1, b);
        let errs = out.iter().find(|s| s.metric == cat::NET_ERRORS).unwrap();
        assert_eq!(errs.value, 15.0, "1 + 2 + 4 + 8");
    }

    #[test]
    fn 计数器倒退与接口消失都跳过() {
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_secs(2);
        let mut c = NetCollector::new();
        c.ingest(t0, snap("以太网", 5000, 0));
        // 计数器倒退（接口被重建）
        let out = c.ingest(t1, snap("以太网", 1000, 0));
        assert!(!out.iter().any(|s| s.metric == cat::NET_RX_BYTES));

        // 上一轮没见过的接口
        let mut c = NetCollector::new();
        c.ingest(t0, snap("以太网", 0, 0));
        assert!(c.ingest(t1, snap("Wi-Fi", 100, 0)).is_empty());
    }

    #[test]
    fn 本机能枚举出接口() {
        let snapshot = read_interfaces().expect("GetIfTable2 在任何 Windows 上都该成功");
        assert!(!snapshot.is_empty(), "至少有一个非回环接口");
        for (name, c) in &snapshot {
            assert!(!name.is_empty());
            assert!(c.rx_bytes < 1 << 62, "{name} 的收字节数明显不对");
        }
        eprintln!(
            "本机接口：{}",
            snapshot
                .iter()
                .map(|(n, c)| format!("{n}(rx={} tx={})", c.rx_bytes, c.tx_bytes))
                .collect::<Vec<_>>()
                .join(" ")
        );
    }

    #[test]
    fn 本机两轮采集() {
        let mut c = NetCollector::new();
        let first = c.collect(Instant::now()).expect("GetIfTable2");
        assert!(first.is_empty(), "第一轮无基线");
        std::thread::sleep(Duration::from_millis(200));
        let out = c.collect(Instant::now()).expect("GetIfTable2");
        for s in &out {
            assert!(
                s.value.is_finite() && s.value >= 0.0,
                "{} = {}",
                s.metric,
                s.value
            );
            assert_eq!(s.labels.len(), 1);
            assert_eq!(s.labels[0].0, label::IFACE);
            // 裁剪后只有三条网络指标
            assert!(
                [cat::NET_RX_BYTES, cat::NET_TX_BYTES, cat::NET_ERRORS].contains(&s.metric),
                "{} 不该产出",
                s.metric
            );
        }
        assert_eq!(out.len() % 3, 0, "每个接口三条");
    }
}
