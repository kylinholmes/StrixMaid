//! 网络：`/proc/net/dev` 差分 → 每接口的字节 / 包 / 错误 / 丢包速率。
//!
//! 排除 `lo`；也排除 `veth*`——容器每起一个就多一对、名字随机，既撑大 series 表
//! 又没有观测价值（容器侧流量看宿主网桥即可）。

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::Instant;

use super::{CollectError, Collector, Sample, elapsed_secs, rate, read_text, sanitize_label};
use crate::metrics::catalog::{self as cat, label};

const NET_DEV_PATH: &str = "/proc/net/dev";

/// 一个接口的累计计数。
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct NetDev {
    pub iface: String,
    pub rx_bytes: u64,
    pub rx_packets: u64,
    pub rx_errors: u64,
    pub rx_drops: u64,
    pub tx_bytes: u64,
    pub tx_packets: u64,
    pub tx_errors: u64,
    pub tx_drops: u64,
}

/// 解析 `/proc/net/dev`。前两行是表头；接口名后紧跟冒号，冒号后可能没有空格。
pub fn parse_net_dev(text: &str) -> Vec<NetDev> {
    let mut out = Vec::new();
    for line in text.lines() {
        let Some((name, rest)) = line.split_once(':') else {
            continue;
        };
        let name = name.trim();
        let f: Vec<u64> = rest
            .split_whitespace()
            .map(|x| x.parse().unwrap_or(0))
            .collect();
        if f.len() < 16 || name.is_empty() {
            continue;
        }
        out.push(NetDev {
            iface: name.to_owned(),
            rx_bytes: f[0],
            rx_packets: f[1],
            rx_errors: f[2],
            rx_drops: f[3],
            tx_bytes: f[8],
            tx_packets: f[9],
            tx_errors: f[10],
            tx_drops: f[11],
        });
    }
    out
}

/// 不采的接口。
pub fn is_excluded_iface(name: &str) -> bool {
    name == "lo" || name.starts_with("veth")
}

/// 网络采集器。
pub struct NetCollector {
    path: PathBuf,
    prev: HashMap<String, NetDev>,
    prev_at: Option<Instant>,
}

impl Default for NetCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl NetCollector {
    /// 读 `/proc/net/dev`。
    pub fn new() -> Self {
        NetCollector {
            path: PathBuf::from(NET_DEV_PATH),
            prev: HashMap::new(),
            prev_at: None,
        }
    }

    /// 喂入一轮解析结果，产出与上一轮的差分样本；第一轮返回空。
    pub fn ingest(&mut self, devs: Vec<NetDev>, now: Instant) -> Vec<Sample> {
        let secs = self.prev_at.map(|p| elapsed_secs(p, now));
        let mut out = Vec::new();
        let mut next = HashMap::with_capacity(devs.len());
        for d in devs {
            if is_excluded_iface(&d.iface) {
                continue;
            }
            if let Some(secs) = secs
                && let Some(p) = self.prev.get(&d.iface)
            {
                let iface = sanitize_label(&d.iface);
                let pairs = [
                    (cat::NET_RX_BYTES, p.rx_bytes, d.rx_bytes),
                    (cat::NET_TX_BYTES, p.tx_bytes, d.tx_bytes),
                    (cat::NET_RX_PACKETS, p.rx_packets, d.rx_packets),
                    (cat::NET_TX_PACKETS, p.tx_packets, d.tx_packets),
                    (cat::NET_RX_ERRORS, p.rx_errors, d.rx_errors),
                    (cat::NET_TX_ERRORS, p.tx_errors, d.tx_errors),
                    (cat::NET_RX_DROPS, p.rx_drops, d.rx_drops),
                    (cat::NET_TX_DROPS, p.tx_drops, d.tx_drops),
                ];
                // 任一计数器回退说明接口被重建，整组跳过。
                let rates: Option<Vec<(&'static str, f64)>> = pairs
                    .iter()
                    .map(|(m, a, b)| rate(*a, *b, secs).map(|r| (*m, r)))
                    .collect();
                if let Some(rates) = rates {
                    out.extend(
                        rates
                            .into_iter()
                            .map(|(m, v)| Sample::labeled(m, label::IFACE, iface.clone(), v)),
                    );
                }
            }
            next.insert(d.iface.clone(), d);
        }
        self.prev = next;
        self.prev_at = Some(now);
        out
    }
}

impl Collector for NetCollector {
    fn name(&self) -> &'static str {
        "net"
    }

    fn collect(&mut self, now: Instant) -> Result<Vec<Sample>, CollectError> {
        let text =
            read_text(&self.path).map_err(|e| CollectError::io(self.name(), &self.path, &e))?;
        Ok(self.ingest(parse_net_dev(&text), now))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    const A: &str = "\
Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
    lo: 1000 10 0 0 0 0 0 0 1000 10 0 0 0 0 0 0
  eth0: 5000 50 1 2 0 0 0 0 3000 30 0 1 0 0 0 0
veth1a:  100  1 0 0 0 0 0 0  100  1 0 0 0 0 0 0
";
    const B: &str = "\
Inter-|   Receive                                                |  Transmit
 face |bytes    packets errs drop fifo frame compressed multicast|bytes    packets errs drop fifo colls carrier compressed
    lo: 2000 20 0 0 0 0 0 0 2000 20 0 0 0 0 0 0
  eth0:15000 150 3 2 0 0 0 0 7000 70 0 5 0 0 0 0
veth1a:  200  2 0 0 0 0 0 0  200  2 0 0 0 0 0 0
";

    #[test]
    fn 解析固定文本() {
        let d = parse_net_dev(B);
        assert_eq!(d.len(), 3);
        assert_eq!(d[1].iface, "eth0");
        assert_eq!(d[1].rx_bytes, 15000, "冒号后无空格也能解析");
        assert_eq!(d[1].tx_drops, 5);
    }

    #[test]
    fn 差分与过滤() {
        let mut c = NetCollector::new();
        let t0 = Instant::now();
        assert!(c.ingest(parse_net_dev(A), t0).is_empty());
        let out = c.ingest(parse_net_dev(B), t0 + Duration::from_secs(5));
        assert!(
            out.iter().all(|s| s.labels[0].1 == "eth0"),
            "lo 与 veth 被过滤: {out:?}"
        );
        assert_eq!(out.len(), 8);
        let get = |m: &str| out.iter().find(|s| s.metric == m).unwrap().value;
        assert_eq!(get(cat::NET_RX_BYTES), 2000.0);
        assert_eq!(get(cat::NET_TX_BYTES), 800.0);
        assert_eq!(get(cat::NET_RX_ERRORS), 0.4);
        assert_eq!(get(cat::NET_TX_DROPS), 0.8);
        assert_eq!(get(cat::NET_RX_DROPS), 0.0);

        // 计数器回退（接口重建）→ 该接口本轮无样本
        let out = c.ingest(parse_net_dev(A), t0 + Duration::from_secs(10));
        assert!(out.is_empty());
    }

    #[test]
    fn 本机两轮值域合理() {
        let mut c = NetCollector::new();
        c.collect(Instant::now()).expect("读 /proc/net/dev");
        std::thread::sleep(Duration::from_millis(100));
        let out = c.collect(Instant::now()).expect("读 /proc/net/dev");
        for s in &out {
            assert!(s.value >= 0.0, "{} {:?}", s.metric, s.labels);
            assert_ne!(s.labels[0].1, "lo");
        }
    }
}
