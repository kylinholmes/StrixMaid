//! GPU：PDH 的 `\GPU Engine(*)` / `\GPU Adapter Memory(*)` + 注册表里的显存总量。
//!
//! # 为什么只有这一项走 PDH
//!
//! WDDM 在 Win10 1709+ 把 GPU 的利用率与显存开销**只**暴露成性能计数器，
//! 没有别的非 COM 入口（DXGI 的 `QueryVideoMemoryInfo` 要起 COM，且只看得到
//! 本进程视角的显存预算，给不出整机利用率）。其余指标都有更直接的来源，
//! 见 [`crate::platform::windows::pdh`] 的模块文档。
//!
//! # 指标与来源
//!
//! | 指标 | 来源 | 口径 |
//! |---|---|---|
//! | `gpu.usage` | `\GPU Engine(*)\Utilization Percentage` | 同一块卡的**各引擎求和**后封顶 100 |
//! | `gpu.engine.usage` | 同上，按实例名里的 `engtype_*` 分组 | 同一引擎类型上各进程之和，封顶 100 |
//! | `gpu.mem_used` | `\GPU Adapter Memory(*)\Dedicated Usage` | 专用显存占用，字节 |
//! | `gpu.mem_total` | 注册表 `HardwareInformation.qwMemorySize` | 见下 |
//!
//! 实例名形如 `pid_10788_luid_0x00000000_0x0000EDA7_phys_0_eng_1_engtype_Compute 0`：
//! **每个用 GPU 的进程、每条引擎各一个实例**（本机实测一次查询 497 项），
//! 所以「这块卡有多忙」必须把同一块卡的全部实例加起来。求和后可能超过 100
//! （几条引擎同时满载），封顶到 100，与任务管理器「GPU」那一栏的显示一致。
//!
//! 注意引擎名**自带空格**（`Compute 0` / `Internal 0`），`engtype_` 之后到结尾
//! 整段都是引擎名，不能按 `_` 再切一刀。
//!
//! # 缺席的两项
//!
//! - **不产出 `gpu.temp`**。Windows 没有统一的 GPU 温度接口：NVIDIA 要 NVML、
//!   AMD 要 ADL、Intel 要 IGCL，三家各一套私有 DLL，且都不保证装了驱动就有。
//!   WDDM 的性能计数器里没有任何温度项。与其在装了某家驱动的机器上有、别的机器
//!   上没有，不如统一缺席（NVML 的取舍与 Linux 侧同因，见 `linux/gpu.rs`）。
//! - **不产出 `gpu.mem_alloc`**。那是统一内存架构（Apple Silicon）下「显存条的
//!   分母」，独显有真正的 `mem_total`，不需要这个替代品。
//!
//! # `gpu` 标签取 `phys_N` 的理由与代价
//!
//! 实例名里真正唯一标识一块适配器的是 **LUID**，但 LUID 是**每次开机重新分配**的，
//! 拿它当标签会让同一块卡在每次重启后变成一条新 series，时序库被切得粉碎。
//! 因此标签取 `phys_N` 里的 N（单卡机器恒为 0）。
//!
//! 代价写在这里：一台装了两块**独立适配器**的机器上，两块卡的实例名里都是
//! `phys_0`（`phys_N` 是同一适配器内的物理 GPU 序号，不是全局序号），本采集器会把
//! 它们**合并成一条 `gpu=0` 曲线**。本机实测就是这个样子：
//!
//! ```text
//! \GPU Adapter Memory(*)\Dedicated Usage
//!     luid_0x00000000_0x0000EDA7_phys_0 = 2216079360   ← 真显卡
//!     luid_0x00000000_0x0000FDFD_phys_0 = 0            ← 另一个适配器（软件渲染器一类）
//! ```
//!
//! 这是已知限制，不是错误的数据——曲线的含义是「这台机器 GPU 引擎的总忙碌度」，
//! 而且实践中第二个适配器几乎总是闲着的软件渲染器。多卡机器要分卡观测需要引入
//! 一张 LUID → 稳定编号的持久映射表，那是一项独立的设计，不在 P0 范围内。
//!
//! # `gpu.mem_total` 的配对规则
//!
//! 显存总量不在任何性能计数器里，只能读注册表的显卡类键
//! `HKLM\SYSTEM\CurrentControlSet\Control\Class\{4d36e968-…}\NNNN` 下的
//! `HardwareInformation.qwMemorySize`。问题是**那里的 NNNN 与 PDH 的 `phys_N`
//! 之间没有任何官方对应关系**（注册表键里没有 LUID）。
//!
//! 所以规则是：只有当「注册表里读出显存的适配器数」与「PDH 观测到的 GPU 数」
//! **相等**时，才按各自的排序位置配对；数目对不上就**一条 `gpu.mem_total` 都不产出**。
//! 绝大多数机器是单卡，这时配对是确定的；对不上的时候少一条曲线，好过把
//! 独显的 8 GiB 安到核显头上。
//!
//! # PDH 的代价与本采集器的缓存
//!
//! [`crate::platform::windows::pdh::query_wildcard`] 每次调用都要采两轮、
//! 中间**必须真的睡一段时间**（利用率计数器是「两次采样之间的占比」）。
//! 默认采集间隔是 2 秒，若每轮都查两条计数器，光 GPU 就要占掉整轮墙钟的 5%~10%，
//! 而且这段 sleep 发生在同步的 `collect` 里，会把它后面的采集器一起推迟。
//!
//! 取舍：本采集器内部缓存上一次的结果，**每 [`REFRESH`] 秒才真的查一次 PDH**，
//! 中间的轮次原样复用上次的样本。代价是 GPU 曲线在刷新间隔内是阶梯状的
//! （同一个值重复若干个点），分辨率从 2 秒降到 10 秒。对「这块卡在忙吗」这个
//! 问题 10 秒足够，而让每一轮采集都多花 200 毫秒不值得。
//!
//! # 没有 WDDM GPU 的机器
//!
//! 服务器与虚拟机上这两条计数器根本不存在，`query_wildcard` 返回 `Err`。
//! 那**不是失败**：本采集器如实返回空表，不报错也不记日志——缺席是常态。

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use super::{CollectError, Collector, Sample, sanitize_label};
use crate::metrics::catalog::{self as cat, label};
use crate::platform::windows::pdh::{self, CounterItem};
use crate::platform::windows::registry::{HKLM, reg_dword, reg_qword, reg_subkeys};

/// 各引擎的利用率，实例 = 每进程每引擎。
const ENGINE_COUNTER: &str = r"\GPU Engine(*)\Utilization Percentage";
/// 专用显存占用，实例 = 每适配器。
const MEMORY_COUNTER: &str = r"\GPU Adapter Memory(*)\Dedicated Usage";

/// 显卡设备类的 GUID 键。
const DISPLAY_CLASS: &str =
    r"SYSTEM\CurrentControlSet\Control\Class\{4d36e968-e325-11ce-bfc1-08002be10318}";
/// 显存总量的值名。
const MEMORY_SIZE_VALUE: &str = "HardwareInformation.qwMemorySize";

/// 利用率计数器两轮采集之间的间隔。任务管理器自己也是这个量级。
const ENGINE_GAP: Duration = Duration::from_millis(100);
/// 显存占用是瞬时量（PDH 的 RAW 计数器），两轮之间不需要真实的时间流逝，
/// 给最小间隔即可，省下一次 100 毫秒的 sleep。
const MEMORY_GAP: Duration = Duration::from_millis(1);
/// 真正查一次 PDH 的最小间隔，见模块文档。
const REFRESH: Duration = Duration::from_secs(10);

// ===========================================================================
// 实例名解析
// ===========================================================================

/// 实例名里的 `phys_N`。
///
/// `pid_1234_luid_0x00000000_0x0000C3F5_phys_0_eng_3_engtype_3D` → `Some(0)`；
/// 显存计数器的 `luid_0x00000000_0x0000C3F5_phys_0` 同样适用。
pub fn phys_index(instance: &str) -> Option<u32> {
    let segs: Vec<&str> = instance.split('_').collect();
    segs.windows(2)
        .find(|w| w[0] == "phys")
        .and_then(|w| w[1].parse().ok())
}

/// 实例名里 `engtype_` 之后的部分（`3D` / `Copy` / `VideoDecode` / `Compute 0` …）。
///
/// 整段取到结尾而不是按分隔符再切：引擎名自己可能带空格（`Compute 0`、`Internal 0`），
/// 也不排除将来出现带下划线的名字。
pub fn engine_type(instance: &str) -> Option<String> {
    const MARK: &str = "_engtype_";
    let at = instance.find(MARK)?;
    let s = &instance[at + MARK.len()..];
    (!s.is_empty()).then(|| s.to_owned())
}

// ===========================================================================
// 注册表：显存总量
// ===========================================================================

/// 注册表里各显示适配器的显存总量（字节），按子键号排序。
///
/// 只认四位数字的子键（`0000`、`0001`…）——同一个键下还有 `Properties`、
/// `Configuration` 这类非设备子键。读不出显存的适配器（Basic Display Adapter、
/// 远程桌面镜像驱动）不计入。
pub fn adapter_memory_sizes() -> Vec<u64> {
    let mut keys: Vec<String> = reg_subkeys(HKLM, DISPLAY_CLASS)
        .into_iter()
        .filter(|k| k.len() == 4 && k.chars().all(|c| c.is_ascii_digit()))
        .collect();
    keys.sort();
    keys.iter()
        .filter_map(|k| {
            let path = format!(r"{DISPLAY_CLASS}\{k}");
            // 多数驱动写成 REG_QWORD；少数老驱动写成 REG_DWORD（同一个值名）。
            reg_qword(HKLM, &path, MEMORY_SIZE_VALUE)
                .or_else(|| reg_dword(HKLM, &path, MEMORY_SIZE_VALUE).map(u64::from))
        })
        .filter(|v| *v > 0)
        .collect()
}

// ===========================================================================
// 汇总
// ===========================================================================

/// 把两条计数器的原始项与注册表显存表汇总成样本。纯函数，便于用固定输入单测。
pub fn build_samples(
    engines: &[CounterItem],
    memory: &[CounterItem],
    adapter_sizes: &[u64],
) -> Vec<Sample> {
    // 同一块卡的各引擎求和；BTreeMap 保证产出顺序稳定。
    let mut per_gpu: BTreeMap<u32, f64> = BTreeMap::new();
    let mut per_engine: BTreeMap<(u32, String), f64> = BTreeMap::new();
    for item in engines {
        let Some(phys) = phys_index(&item.instance) else {
            continue;
        };
        let v = item.value.max(0.0);
        *per_gpu.entry(phys).or_insert(0.0) += v;
        if let Some(engine) = engine_type(&item.instance) {
            *per_engine.entry((phys, engine)).or_insert(0.0) += v;
        }
    }

    let mut mem_used: BTreeMap<u32, f64> = BTreeMap::new();
    for item in memory {
        let Some(phys) = phys_index(&item.instance) else {
            continue;
        };
        *mem_used.entry(phys).or_insert(0.0) += item.value.max(0.0);
    }

    let mut out = Vec::new();
    for (phys, v) in &per_gpu {
        out.push(Sample::labeled(
            cat::GPU_USAGE,
            label::GPU,
            phys.to_string(),
            v.min(100.0),
        ));
    }
    for ((phys, engine), v) in &per_engine {
        out.push(Sample {
            metric: cat::GPU_ENGINE_USAGE,
            // 键序 = canonical 序（engine < gpu），与常量表 GPU_ENGINE 一致
            labels: vec![
                (label::ENGINE, sanitize_label(engine)),
                (label::GPU, phys.to_string()),
            ],
            value: v.min(100.0),
        });
    }
    for (phys, v) in &mem_used {
        out.push(Sample::labeled(
            cat::GPU_MEM_USED,
            label::GPU,
            phys.to_string(),
            *v,
        ));
    }

    // 显存总量：只有数目对得上才敢配对，见模块文档。
    let gpus: BTreeSet<u32> = per_gpu.keys().chain(mem_used.keys()).copied().collect();
    if !adapter_sizes.is_empty() && adapter_sizes.len() == gpus.len() {
        for (phys, size) in gpus.iter().zip(adapter_sizes.iter()) {
            out.push(Sample::labeled(
                cat::GPU_MEM_TOTAL,
                label::GPU,
                phys.to_string(),
                *size as f64,
            ));
        }
    }
    out
}

// ===========================================================================
// 采集器
// ===========================================================================

/// GPU 采集器。内部缓存上一次的 PDH 结果，见模块文档。
#[derive(Debug)]
pub struct GpuCollector {
    /// 真正查一次 PDH 的最小间隔。
    refresh: Duration,
    /// 利用率计数器两轮之间的间隔。
    engine_gap: Duration,
    /// `(查询时刻, 样本)`。
    cache: Option<(Instant, Vec<Sample>)>,
}

impl Default for GpuCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl GpuCollector {
    pub fn new() -> Self {
        GpuCollector {
            refresh: REFRESH,
            engine_gap: ENGINE_GAP,
            cache: None,
        }
    }

    /// 改刷新间隔与采样间隔（测试用；生产走 [`GpuCollector::new`] 的默认值）。
    pub fn with_intervals(refresh: Duration, engine_gap: Duration) -> Self {
        GpuCollector {
            refresh,
            engine_gap,
            cache: None,
        }
    }

    /// 真的查一次 PDH。计数器不存在（没有 WDDM GPU）时两条都退化成空表，
    /// 于是整个采集器返回空——如实缺席，不报错。
    fn query(&self) -> Vec<Sample> {
        let engines = pdh::query_wildcard(ENGINE_COUNTER, self.engine_gap).unwrap_or_default();
        let memory = pdh::query_wildcard(MEMORY_COUNTER, MEMORY_GAP).unwrap_or_default();
        if engines.is_empty() && memory.is_empty() {
            // 连一个实例都没有：不必再去读注册表。
            return Vec::new();
        }
        build_samples(&engines, &memory, &adapter_memory_sizes())
    }
}

impl Collector for GpuCollector {
    fn name(&self) -> &'static str {
        "gpu"
    }

    fn collect(&mut self, now: Instant) -> Result<Vec<Sample>, CollectError> {
        if let Some((at, cached)) = &self.cache
            && now.saturating_duration_since(*at) < self.refresh
        {
            return Ok(cached.clone());
        }
        let samples = self.query();
        self.cache = Some((now, samples.clone()));
        Ok(samples)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(instance: &str, value: f64) -> CounterItem {
        CounterItem {
            instance: instance.to_owned(),
            value,
        }
    }

    #[test]
    fn 解析实例名() {
        let inst = "pid_1234_luid_0x00000000_0x0000C3F5_phys_0_eng_3_engtype_3D";
        assert_eq!(phys_index(inst), Some(0));
        assert_eq!(engine_type(inst).as_deref(), Some("3D"));

        // 显存计数器的实例名没有 pid / eng 段
        assert_eq!(phys_index("luid_0x00000000_0x0000C3F5_phys_1"), Some(1));
        assert_eq!(engine_type("luid_0x00000000_0x0000C3F5_phys_1"), None);

        // 引擎名自带下划线时要整段取到结尾
        assert_eq!(
            engine_type("pid_1_luid_0x0_0x1_phys_0_eng_5_engtype_Compute_0").as_deref(),
            Some("Compute_0")
        );

        // 认不出来的实例名一律跳过，不猜
        assert_eq!(phys_index("_Total"), None);
        assert_eq!(phys_index("phys_"), None);
        assert_eq!(phys_index(""), None);
    }

    #[test]
    fn 同一块卡的各引擎求和并封顶() {
        let engines = vec![
            item("pid_1_luid_0x0_0x1_phys_0_eng_0_engtype_3D", 40.0),
            item("pid_2_luid_0x0_0x1_phys_0_eng_0_engtype_3D", 30.0),
            item("pid_3_luid_0x0_0x1_phys_0_eng_1_engtype_Copy", 10.0),
        ];
        let out = build_samples(&engines, &[], &[]);
        let usage = out.iter().find(|s| s.metric == cat::GPU_USAGE).unwrap();
        assert_eq!(usage.value, 80.0, "40 + 30 + 10");
        assert_eq!(usage.labels, vec![(label::GPU, "0".to_string())]);

        // 引擎细分：3D 上两个进程之和
        let d3 = out
            .iter()
            .find(|s| s.metric == cat::GPU_ENGINE_USAGE && s.labels[0].1 == "3D")
            .unwrap();
        assert_eq!(d3.value, 70.0);
        assert_eq!(d3.labels[0].0, label::ENGINE);
        assert_eq!(d3.labels[1].0, label::GPU, "键序必须是 engine < gpu");
    }

    #[test]
    fn 求和超过一百时封顶() {
        let engines = vec![
            item("pid_1_luid_0x0_0x1_phys_0_eng_0_engtype_3D", 99.0),
            item("pid_2_luid_0x0_0x1_phys_0_eng_1_engtype_Copy", 80.0),
        ];
        let out = build_samples(&engines, &[], &[]);
        let usage = out.iter().find(|s| s.metric == cat::GPU_USAGE).unwrap();
        assert_eq!(usage.value, 100.0);
    }

    #[test]
    fn 显存总量只在数目对得上时产出() {
        let memory = vec![item("luid_0x0_0x1_phys_0", 1024.0)];
        // 一块卡 + 一条注册表记录 → 配对
        let out = build_samples(&[], &memory, &[8 << 30]);
        let total = out.iter().find(|s| s.metric == cat::GPU_MEM_TOTAL).unwrap();
        assert_eq!(total.value, (8u64 << 30) as f64);
        assert_eq!(total.labels, vec![(label::GPU, "0".to_string())]);

        // 一块卡 + 两条注册表记录 → 配不上，一条都不产出（宁缺毋滥）
        let out = build_samples(&[], &memory, &[8 << 30, 2 << 30]);
        assert!(!out.iter().any(|s| s.metric == cat::GPU_MEM_TOTAL));

        // 注册表读不到 → 同样不产出
        let out = build_samples(&[], &memory, &[]);
        assert!(!out.iter().any(|s| s.metric == cat::GPU_MEM_TOTAL));
        // 但 mem_used 照出
        assert!(out.iter().any(|s| s.metric == cat::GPU_MEM_USED));
    }

    #[test]
    fn 没有实例时整个采集器返回空() {
        assert!(build_samples(&[], &[], &[8 << 30]).is_empty());
    }

    #[test]
    fn 温度与_mem_alloc_绝不产出() {
        let engines = vec![item("pid_1_luid_0x0_0x1_phys_0_eng_0_engtype_3D", 10.0)];
        let memory = vec![item("luid_0x0_0x1_phys_0", 1024.0)];
        let out = build_samples(&engines, &memory, &[8 << 30]);
        assert!(!out.iter().any(|s| s.metric == cat::GPU_TEMP));
        assert!(!out.iter().any(|s| s.metric == cat::GPU_MEM_ALLOC));
    }

    #[test]
    fn 缓存期内不重复查询() {
        let t0 = Instant::now();
        let mut c = GpuCollector::with_intervals(Duration::from_secs(10), ENGINE_GAP);
        let first = c.collect(t0).expect("采集不应失败");
        // 刷新间隔内：原样复用，值一模一样
        let second = c
            .collect(t0 + Duration::from_secs(2))
            .expect("采集不应失败");
        assert_eq!(first, second, "刷新间隔内应当复用缓存");
        // 越过刷新间隔：重新查（本机没有 GPU 时仍是空表，不会失败）
        let third = c
            .collect(t0 + Duration::from_secs(11))
            .expect("采集不应失败");
        assert!(third.iter().all(|s| s.value.is_finite()));
    }

    #[test]
    fn 本机注册表显存总量() {
        let sizes = adapter_memory_sizes();
        for s in &sizes {
            assert!(*s >= 1 << 20, "显存 {s} 字节，明显不对");
            assert!(*s < 1 << 44, "显存 {s} 字节，明显不对");
        }
        eprintln!(
            "本机注册表显卡显存：{:?} MiB",
            sizes.iter().map(|s| s / (1 << 20)).collect::<Vec<_>>()
        );
    }

    #[test]
    fn 本机采集值域合理() {
        let mut c = GpuCollector::with_intervals(Duration::from_secs(1), ENGINE_GAP);
        let out = c.collect(Instant::now()).expect("采集不应失败");
        for s in &out {
            assert!(s.value.is_finite() && s.value >= 0.0, "{} = {}", s.metric, s.value);
            if s.metric == cat::GPU_USAGE || s.metric == cat::GPU_ENGINE_USAGE {
                assert!(s.value <= 100.0, "{} = {}", s.metric, s.value);
            }
        }
        if out.is_empty() {
            eprintln!("本机没有 WDDM GPU 计数器（虚拟机 / 服务器），如实返回空");
        } else {
            eprintln!("本机 GPU：");
            for s in &out {
                eprintln!("    {}{{{}}} = {:.2}", s.metric, s.canonical_labels(), s.value);
            }
        }
    }
}
