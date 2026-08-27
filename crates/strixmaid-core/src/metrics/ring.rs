//! 内存环形缓冲（design.md §7.2：默认 2s 一点、保留 1 小时）。
//!
//! 每条 series 一个固定容量的环，元素是 16 字节的 [`Point`]（`i64` 时间戳 + `f64` 值），
//! 容量在创建时一次性分配、之后不再增长：200 series × 1800 点 × 16B ≈ 5.8MB。
//!
//! 环内时间戳**单调不减**：调度器每轮用同一个 `now` 打点，时钟被 NTP 往回拨时
//! 新点会被丢弃（[`Ring::push`] 返回 `false`），直到时钟追上为止。这换来
//! [`Ring::range`] 可以二分查找。

use std::collections::HashMap;

/// 一个采样点。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Point {
    /// unix 秒。
    pub ts: i64,
    /// 值。
    pub value: f64,
}

/// 单条 series 的环。
#[derive(Debug, Clone)]
pub struct Ring {
    /// 物理存储。未满时长度 < `cap` 且 `head == 0`；满后长度恒为 `cap`。
    buf: Vec<Point>,
    cap: usize,
    /// 最旧元素的物理下标。
    head: usize,
}

impl Ring {
    /// 固定容量的空环。容量至少为 1。
    pub fn new(cap: usize) -> Ring {
        let cap = cap.max(1);
        Ring {
            buf: Vec::with_capacity(cap),
            cap,
            head: 0,
        }
    }

    /// 容量（点数）。
    pub fn capacity(&self) -> usize {
        self.cap
    }

    /// 当前点数。
    pub fn len(&self) -> usize {
        self.buf.len()
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.buf.is_empty()
    }

    /// 已分配的字节数（与是否填满无关）。
    pub fn bytes(&self) -> usize {
        self.cap * size_of::<Point>()
    }

    /// 逻辑下标 → 点（0 为最旧）。
    fn at(&self, i: usize) -> Point {
        self.buf[(self.head + i) % self.cap]
    }

    /// 追加一点；满则覆盖最旧的。时间戳早于当前最新点时丢弃并返回 `false`。
    pub fn push(&mut self, p: Point) -> bool {
        if let Some(last) = self.latest()
            && p.ts < last.ts
        {
            return false;
        }
        if self.buf.len() < self.cap {
            self.buf.push(p);
        } else {
            self.buf[self.head] = p;
            self.head = (self.head + 1) % self.cap;
        }
        true
    }

    /// 最新的点。
    pub fn latest(&self) -> Option<Point> {
        if self.is_empty() {
            None
        } else {
            Some(self.at(self.len() - 1))
        }
    }

    /// 最旧的点。
    pub fn oldest(&self) -> Option<Point> {
        if self.is_empty() {
            None
        } else {
            Some(self.at(0))
        }
    }

    /// 从旧到新遍历。
    pub fn iter(&self) -> impl Iterator<Item = Point> + '_ {
        (0..self.len()).map(|i| self.at(i))
    }

    /// 第一个 `ts >= t` 的逻辑下标（二分）。
    fn lower_bound(&self, t: i64) -> usize {
        let (mut lo, mut hi) = (0, self.len());
        while lo < hi {
            let mid = lo + (hi - lo) / 2;
            if self.at(mid).ts < t {
                lo = mid + 1;
            } else {
                hi = mid;
            }
        }
        lo
    }

    /// 取 `[from, to)` 内的点，按时间升序。
    pub fn range(&self, from: i64, to: i64) -> Vec<Point> {
        if to <= from || self.is_empty() {
            return Vec::new();
        }
        let a = self.lower_bound(from);
        let b = self.lower_bound(to);
        (a..b).map(|i| self.at(i)).collect()
    }
}

// ============================ 桶统计 ============================

/// 一段样本的 cnt / min / max / sum / med，与 `m_1m` 的行一一对应。
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Bucket {
    pub cnt: u32,
    pub min: f64,
    pub max: f64,
    pub sum: f64,
    /// 真中位数：奇数个取中间那个，偶数个取中间两个的平均（design.md §7.2）。
    pub med: f64,
}

/// 统计一组值。会原地排序。空切片返回 `None`。
pub fn summarize(values: &mut [f64]) -> Option<Bucket> {
    if values.is_empty() {
        return None;
    }
    values.sort_by(f64::total_cmp);
    let n = values.len();
    let med = if n % 2 == 1 {
        values[n / 2]
    } else {
        (values[n / 2 - 1] + values[n / 2]) / 2.0
    };
    Some(Bucket {
        cnt: n as u32,
        min: values[0],
        max: values[n - 1],
        sum: values.iter().sum(),
        med,
    })
}

/// 统计一组点的值。
pub fn summarize_points(points: &[Point]) -> Option<Bucket> {
    let mut v: Vec<f64> = points.iter().map(|p| p.value).collect();
    summarize(&mut v)
}

// ============================ 多 series ============================

/// series 键：指标名 + 规范化标签串，与 `series` 表的 `(metric, labels)` 对应。
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct SeriesKey {
    pub metric: String,
    pub labels: String,
}

impl SeriesKey {
    /// 构造。`labels` 须已是规范形式。
    pub fn new(metric: impl Into<String>, labels: impl Into<String>) -> Self {
        SeriesKey {
            metric: metric.into(),
            labels: labels.into(),
        }
    }
}

/// 一条 series 在环集里的条目。
#[derive(Debug)]
pub struct Entry {
    pub ring: Ring,
    /// 单位，来自常量表。
    pub unit: Option<&'static str>,
    /// `series.id`；尚未落库时为 `None`。
    pub id: Option<i64>,
}

/// 环集的内存统计。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Serialize)]
pub struct RingStats {
    /// series 条数。
    pub series: usize,
    /// 当前点数总和。
    pub points: usize,
    /// 已分配字节数（每个环按容量计）。
    pub bytes: usize,
}

/// 全部 series 的环。
#[derive(Debug)]
pub struct RingSet {
    cap: usize,
    map: HashMap<SeriesKey, Entry>,
}

impl RingSet {
    /// 每个环的容量。
    pub fn new(cap: usize) -> RingSet {
        RingSet {
            cap: cap.max(1),
            map: HashMap::new(),
        }
    }

    /// 每个环的容量。
    pub fn capacity(&self) -> usize {
        self.cap
    }

    /// series 数。
    pub fn len(&self) -> usize {
        self.map.len()
    }

    /// 是否没有任何 series。
    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }

    /// 写一点；series 不存在则创建。返回值同 [`Ring::push`]。
    pub fn push(&mut self, key: SeriesKey, unit: Option<&'static str>, p: Point) -> bool {
        let cap = self.cap;
        self.map
            .entry(key)
            .or_insert_with(|| Entry {
                ring: Ring::new(cap),
                unit,
                id: None,
            })
            .ring
            .push(p)
    }

    /// 查找。
    pub fn get(&self, key: &SeriesKey) -> Option<&Entry> {
        self.map.get(key)
    }

    /// 可变查找。
    pub fn get_mut(&mut self, key: &SeriesKey) -> Option<&mut Entry> {
        self.map.get_mut(key)
    }

    /// 按 `series.id` 查找（线性，只用于 API 查询）。
    pub fn find_by_id(&self, id: i64) -> Option<(&SeriesKey, &Entry)> {
        self.map.iter().find(|(_, e)| e.id == Some(id))
    }

    /// 遍历（无序）。
    pub fn iter(&self) -> impl Iterator<Item = (&SeriesKey, &Entry)> {
        self.map.iter()
    }

    /// 可变遍历（无序）。
    pub fn iter_mut(&mut self) -> impl Iterator<Item = (&SeriesKey, &mut Entry)> {
        self.map.iter_mut()
    }

    /// 内存统计。
    pub fn stats(&self) -> RingStats {
        RingStats {
            series: self.map.len(),
            points: self.map.values().map(|e| e.ring.len()).sum(),
            bytes: self.map.values().map(|e| e.ring.bytes()).sum(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pt(ts: i64) -> Point {
        Point {
            ts,
            value: ts as f64,
        }
    }

    #[test]
    fn 满环覆盖最旧的点() {
        let mut r = Ring::new(4);
        assert!(r.is_empty());
        for ts in 0..6 {
            assert!(r.push(pt(ts)));
        }
        assert_eq!(r.len(), 4);
        assert_eq!(r.capacity(), 4);
        let ts: Vec<i64> = r.iter().map(|p| p.ts).collect();
        assert_eq!(ts, [2, 3, 4, 5]);
        assert_eq!(r.oldest().unwrap().ts, 2);
        assert_eq!(r.latest().unwrap().ts, 5);
        assert_eq!(r.bytes(), 4 * 16);
        // 容量 0 视作 1
        let mut one = Ring::new(0);
        one.push(pt(1));
        one.push(pt(2));
        assert_eq!(one.len(), 1);
        assert_eq!(one.latest().unwrap().ts, 2);
    }

    #[test]
    fn 按时间范围切片() {
        let mut r = Ring::new(5);
        for ts in [10, 12, 14, 16, 18, 20, 22] {
            r.push(pt(ts));
        }
        // 环里现在是 14..=22（绕了一圈，head != 0）
        let ts = |v: Vec<Point>| v.into_iter().map(|p| p.ts).collect::<Vec<_>>();
        assert_eq!(ts(r.range(14, 19)), [14, 16, 18], "左闭右开");
        assert_eq!(ts(r.range(0, 100)), [14, 16, 18, 20, 22]);
        assert_eq!(ts(r.range(15, 16)), Vec::<i64>::new());
        assert_eq!(ts(r.range(22, 23)), [22]);
        assert_eq!(ts(r.range(23, 30)), Vec::<i64>::new());
        assert_eq!(ts(r.range(20, 20)), Vec::<i64>::new(), "空区间");
        assert!(Ring::new(3).range(0, 10).is_empty());
    }

    #[test]
    fn 时钟倒退的点被丢弃() {
        let mut r = Ring::new(3);
        assert!(r.push(pt(10)));
        assert!(!r.push(pt(9)));
        assert!(r.push(pt(10)), "相同时间戳允许");
        assert_eq!(r.len(), 2);
    }

    #[test]
    fn 中位数_奇偶各一例() {
        let mut odd = [5.0, 1.0, 3.0];
        let b = summarize(&mut odd).unwrap();
        assert_eq!((b.cnt, b.min, b.max, b.sum, b.med), (3, 1.0, 5.0, 9.0, 3.0));
        let mut even = [4.0, 1.0, 3.0, 2.0];
        let b = summarize(&mut even).unwrap();
        assert_eq!((b.cnt, b.med), (4, 2.5));
        assert!(summarize(&mut []).is_none());
        let single = summarize_points(&[pt(7)]).unwrap();
        assert_eq!((single.cnt, single.med, single.sum), (1, 7.0, 7.0));
    }

    #[test]
    fn 环集统计与查找() {
        let mut set = RingSet::new(10);
        let k = SeriesKey::new("cpu.usage", "");
        set.push(k.clone(), Some("percent"), pt(1));
        set.push(k.clone(), Some("percent"), pt(2));
        set.push(
            SeriesKey::new("disk.util", "dev=sda"),
            Some("percent"),
            pt(1),
        );
        assert_eq!(set.len(), 2);
        let s = set.stats();
        assert_eq!(
            s,
            RingStats {
                series: 2,
                points: 3,
                bytes: 2 * 10 * 16
            }
        );
        assert_eq!(set.get(&k).unwrap().ring.len(), 2);
        assert!(set.find_by_id(1).is_none());
        set.get_mut(&k).unwrap().id = Some(1);
        assert_eq!(set.find_by_id(1).unwrap().0, &k);
    }
}
