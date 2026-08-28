//! 网络：`sysctl NET_RT_IFLIST2` → 每接口的收发速率（两轮差分）。
//!
//! # 为什么不用 `getifaddrs`
//!
//! `getifaddrs(3)` 的 `AF_LINK` 项也带统计数据，但那里的 `ifa_data` 是 **32 位** 的
//! `struct if_data`：万兆网卡上 `ifi_ibytes` 约 3.5 秒就绕一圈，差分出来的速率全是噪声。
//! `NET_RT_IFLIST2` 返回 `if_msghdr2`，内含 64 位计数的 [`libc::if_data64`]，不会回绕。
//!
//! # 与 Linux 的差异
//!
//! `/proc/net/dev` 收发两个方向各有 errors 与 drops。`if_data64` 只有 `ifi_iqdrops`
//! （接收方向的队列丢包），**没有发送方向的丢包计数**，故 `net.errors`
//! （roadmap/08 §4.2 的合并项）在 macOS 上由收发错误 + 接收丢包三个计数构成。
//!
//! # 过滤
//!
//! 排除回环 `lo*`。其余接口（`en*` / `utun*` / `awdl*` / `bridge*` …）全部保留：
//! 哪些有意义取决于机器角色，采集端不替使用者做判断。

use std::collections::HashMap;
use std::time::Instant;

use super::{CollectError, Collector, Sample, elapsed_secs, rate, sanitize_label};
use crate::metrics::catalog::{self as cat, label};
use crate::platform::macos::sysctl_raw;

/// 一个接口的累计计数。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct IfCounters {
    pub rx_bytes: u64,
    pub tx_bytes: u64,
    pub rx_packets: u64,
    pub tx_packets: u64,
    pub rx_errors: u64,
    pub tx_errors: u64,
    pub rx_drops: u64,
}

/// 一轮全部接口的计数，键为接口名。
pub type IfSnapshot = HashMap<String, IfCounters>;

/// 是否采集该接口。
pub fn should_collect(name: &str) -> bool {
    !name.starts_with("lo")
}

/// 接口序号转名字。
fn if_name(index: u32) -> Option<String> {
    let mut buf = [0 as libc::c_char; libc::IF_NAMESIZE];
    // SAFETY: buf 有 IF_NAMESIZE 字节，正是 if_indextoname 约定的最小尺寸。
    let p = unsafe { libc::if_indextoname(index, buf.as_mut_ptr()) };
    if p.is_null() {
        return None;
    }
    // SAFETY: 调用成功时 buf 内是以 NUL 结尾的接口名。
    Some(
        unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) }
            .to_string_lossy()
            .into_owned(),
    )
}

/// 读一轮 `NET_RT_IFLIST2`。
///
/// 返回的是一串变长的路由消息，每条以 `ifm_msglen` 给出自身长度；我们只认
/// `RTM_IFINFO2` 类型的那些，其余（地址项等）按长度跳过。
pub fn read_interfaces() -> Option<IfSnapshot> {
    let mut mib = [libc::CTL_NET, libc::AF_ROUTE, 0, 0, libc::NET_RT_IFLIST2, 0];
    let buf = sysctl_raw(&mut mib, 4096)?;

    let mut out = IfSnapshot::new();
    let mut offset = 0usize;
    let header = std::mem::size_of::<libc::if_msghdr2>();
    while offset + std::mem::size_of::<libc::if_msghdr>() <= buf.len() {
        // 先只读通用头部的长度与类型；两种 if_msghdr* 的前四个字段布局相同。
        // SAFETY: 上面确认了剩余字节足够容纳通用头部。read_unaligned 是必须的——
        // buf 是 u8 向量，其中的结构体没有对齐保证，且 if_msghdr2 本身是 packed(4)。
        let hdr =
            unsafe { std::ptr::read_unaligned(buf.as_ptr().add(offset).cast::<libc::if_msghdr>()) };
        let len = hdr.ifm_msglen as usize;
        if len == 0 || offset + len > buf.len() {
            break;
        }
        if libc::c_int::from(hdr.ifm_type) == libc::RTM_IFINFO2 && offset + header <= buf.len() {
            // SAFETY: 类型标记为 RTM_IFINFO2 即表示这段是一个完整的 if_msghdr2，
            // 且上面确认了剩余字节足够。
            let m = unsafe {
                std::ptr::read_unaligned(buf.as_ptr().add(offset).cast::<libc::if_msghdr2>())
            };
            let d = m.ifm_data;
            if let Some(name) = if_name(u32::from(m.ifm_index)) {
                out.insert(
                    name,
                    IfCounters {
                        rx_bytes: d.ifi_ibytes,
                        tx_bytes: d.ifi_obytes,
                        rx_packets: d.ifi_ipackets,
                        tx_packets: d.ifi_opackets,
                        rx_errors: d.ifi_ierrors,
                        tx_errors: d.ifi_oerrors,
                        rx_drops: d.ifi_iqdrops,
                    },
                );
            }
        }
        offset += len;
    }
    (!out.is_empty()).then_some(out)
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
                if !should_collect(name) {
                    continue;
                }
                let Some(p) = prev.get(name) else { continue };
                let iface = sanitize_label(name);
                let rx = rate(p.rx_bytes, cur.rx_bytes, secs);
                let tx = rate(p.tx_bytes, cur.tx_bytes, secs);
                // 合并项（roadmap/08 §4.2）；if_data64 没有发送方向的丢包计数。
                let errors = [
                    (p.rx_errors, cur.rx_errors),
                    (p.tx_errors, cur.tx_errors),
                    (p.rx_drops, cur.rx_drops),
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
            .ok_or_else(|| CollectError::new(self.name(), "sysctl NET_RT_IFLIST2 未返回接口"))?;
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

    #[test]
    fn 回环被排除() {
        assert!(should_collect("en0"));
        assert!(should_collect("utun3"));
        assert!(!should_collect("lo0"));
    }

    #[test]
    fn 两轮差分出速率() {
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_secs(2);
        let mut c = NetCollector::new();
        assert!(
            c.ingest(t0, snap("en0", 1000, 2000)).is_empty(),
            "第一轮无基线"
        );
        let out = c.ingest(t1, snap("en0", 3000, 2000));
        assert_eq!(out.len(), 3);
        let rx = out.iter().find(|s| s.metric == cat::NET_RX_BYTES).unwrap();
        assert_eq!(rx.value, 1000.0, "2000 字节 / 2 秒");
        assert_eq!(rx.labels, vec![(label::IFACE, "en0".to_string())]);
        // tx 没变，速率为 0 但仍然产出（0 是有效观测值，不是缺失）
        let tx = out.iter().find(|s| s.metric == cat::NET_TX_BYTES).unwrap();
        assert_eq!(tx.value, 0.0);
        let errs = out.iter().find(|s| s.metric == cat::NET_ERRORS).unwrap();
        assert_eq!(errs.value, 0.0);
    }

    #[test]
    fn 计数器倒退与接口消失都跳过() {
        let t0 = Instant::now();
        let t1 = t0 + Duration::from_secs(2);
        let mut c = NetCollector::new();
        c.ingest(t0, snap("en0", 5000, 0));
        // 计数器倒退（接口被重建）
        let out = c.ingest(t1, snap("en0", 1000, 0));
        assert!(!out.iter().any(|s| s.metric == cat::NET_RX_BYTES));

        // 上一轮没见过的接口
        let mut c = NetCollector::new();
        c.ingest(t0, snap("en0", 0, 0));
        assert!(c.ingest(t1, snap("en1", 100, 0)).is_empty());
    }

    #[test]
    fn 本机两轮采集() {
        let mut c = NetCollector::new();
        let first = c.collect(Instant::now()).expect("NET_RT_IFLIST2");
        assert!(first.is_empty(), "第一轮无基线");
        std::thread::sleep(Duration::from_millis(150));
        let out = c.collect(Instant::now()).expect("NET_RT_IFLIST2");
        for s in &out {
            assert!(
                s.value.is_finite() && s.value >= 0.0,
                "{} = {}",
                s.metric,
                s.value
            );
            assert_eq!(s.labels.len(), 1);
            assert_eq!(s.labels[0].0, label::IFACE);
            assert!(!s.labels[0].1.starts_with("lo"), "回环不该出现");
        }
        // 裁剪后只有三条网络指标
        for s in &out {
            assert!(
                [cat::NET_RX_BYTES, cat::NET_TX_BYTES, cat::NET_ERRORS].contains(&s.metric),
                "{} 不该产出",
                s.metric
            );
        }
    }

    #[test]
    fn 本机能枚举出接口() {
        let snapshot = read_interfaces().expect("至少有 lo0");
        assert!(
            snapshot.contains_key("lo0"),
            "实际接口：{:?}",
            snapshot.keys()
        );
    }
}
