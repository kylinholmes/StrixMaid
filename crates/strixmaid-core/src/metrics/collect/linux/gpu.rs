//! GPU：`/sys/class/drm/card*/device` 直读（roadmap/08 §4.2）。
//!
//! # 覆盖范围与 NVIDIA 的取舍
//!
//! 只支持在 sysfs 暴露利用率的驱动——判定标准是 `device/gpu_busy_percent`
//! 可读（amdgpu 有；多数 i915 与所有 NVIDIA 专有驱动没有）。NVIDIA 的利用率
//! 只有 NVML（`libnvidia-ml.so`）给得出来，而静态链接 musl 的主进程不能
//! `dlopen`（design.md §2）；把 NVML 下放给 PAM 会话 helper 又有生命周期冲突
//! （helper 每会话一个，指标引擎常驻，见 `docs/HANDOFF.md` §6）——正确做法是
//! 一个独立的长命采集进程，那是一项未决策的架构改动（roadmap/08 §12 Q1）。
//! 因此 P0 按选项 (c)：sysfs 读不到的卡不产出任何样本，如实缺席。
//!
//! # 指标与数据源
//!
//! | 指标 | 文件 | 说明 |
//! |---|---|---|
//! | `gpu.usage` | `device/gpu_busy_percent` | 0–100；它同时是「这块卡可采集」的判据 |
//! | `gpu.mem_used` | `device/mem_info_vram_used` | 字节 |
//! | `gpu.mem_total` | `device/mem_info_vram_total` | 字节 |
//! | `gpu.temp` | `device/hwmon/hwmon*/temp1_input` | 毫摄氏度 → 摄氏度 |
//!
//! `usage` 之外的文件缺哪个就少哪条曲线（能力探测而非硬依赖，design.md §6）。
//!
//! # 每轮重新枚举
//!
//! 不在构造时把卡列表定死：eGPU 热插拔、驱动重载都会改变 `/sys/class/drm`，
//! 每轮一次 `read_dir` 的成本可忽略。无 GPU 的机器（多数服务器只有 BMC 显示
//! 芯片，如 mgag200 / ast，它们没有 `gpu_busy_percent`）每轮如实返回空，
//! 不记日志——缺席是常态，不是异常。

use std::path::{Path, PathBuf};
use std::time::Instant;

use super::{CollectError, Collector, Sample, read_text, sanitize_label};
use crate::metrics::catalog::{self as cat, label};

const DRM_PATH: &str = "/sys/class/drm";

/// 是不是卡目录本身：`card` + 纯数字。
///
/// `/sys/class/drm` 下还有 `card0-VGA-1` 这类**连接器**目录与 `renderD128`、
/// `version`，都不是卡。
pub fn is_card_name(name: &str) -> bool {
    name.strip_prefix("card")
        .is_some_and(|rest| !rest.is_empty() && rest.chars().all(|c| c.is_ascii_digit()))
}

/// 读一个只含数字的 sysfs 文件。
fn read_u64(path: &Path) -> Option<u64> {
    read_text(path).ok()?.trim().parse().ok()
}

/// 第一个可读的 `hwmon*/temp1_input`，毫摄氏度。
///
/// amdgpu 在 `device/hwmon/` 下恰好挂一个 hwmon；扫目录而不是猜编号，
/// 是因为 hwmon 的编号由注册顺序决定，重启后会变。
fn read_temp_millic(device: &Path) -> Option<u64> {
    let rd = std::fs::read_dir(device.join("hwmon")).ok()?;
    for entry in rd.filter_map(|e| e.ok()) {
        if let Some(v) = read_u64(&entry.path().join("temp1_input")) {
            return Some(v);
        }
    }
    None
}

/// GPU 采集器（无状态——全是瞬时量）。
pub struct GpuCollector {
    dir: PathBuf,
}

impl Default for GpuCollector {
    fn default() -> Self {
        Self::new()
    }
}

impl GpuCollector {
    /// 读 `/sys/class/drm`。
    pub fn new() -> Self {
        Self::with_dir(DRM_PATH)
    }

    /// 改读别的目录（测试用）。
    pub fn with_dir(dir: impl Into<PathBuf>) -> Self {
        GpuCollector { dir: dir.into() }
    }

    /// 采一块卡；`gpu_busy_percent` 读不到即视为不可采集，返回空。
    fn card_samples(card: &str, device: &Path) -> Vec<Sample> {
        let Some(busy) = read_u64(&device.join("gpu_busy_percent")) else {
            return Vec::new();
        };
        let gpu = sanitize_label(card);
        let mk = |metric, v| Sample::labeled(metric, label::GPU, gpu.clone(), v);
        let mut out = vec![mk(cat::GPU_USAGE, (busy as f64).clamp(0.0, 100.0))];
        if let Some(used) = read_u64(&device.join("mem_info_vram_used")) {
            out.push(mk(cat::GPU_MEM_USED, used as f64));
        }
        if let Some(total) = read_u64(&device.join("mem_info_vram_total")) {
            out.push(mk(cat::GPU_MEM_TOTAL, total as f64));
        }
        if let Some(millic) = read_temp_millic(device) {
            out.push(mk(cat::GPU_TEMP, millic as f64 / 1000.0));
        }
        out
    }
}

impl Collector for GpuCollector {
    fn name(&self) -> &'static str {
        "gpu"
    }

    fn collect(&mut self, _now: Instant) -> Result<Vec<Sample>, CollectError> {
        // 目录不存在（无 DRM 的容器 / 内核）不是错误，与 PSI 缺席同理。
        let Ok(rd) = std::fs::read_dir(&self.dir) else {
            return Ok(Vec::new());
        };
        let mut cards: Vec<String> = rd
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| is_card_name(n))
            .collect();
        // read_dir 的顺序不保证，排序让 series 的出现顺序稳定。
        cards.sort();
        let mut out = Vec::new();
        for card in cards {
            out.extend(Self::card_samples(&card, &self.dir.join(&card).join("device")));
        }
        Ok(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 卡目录名的判定() {
        assert!(is_card_name("card0"));
        assert!(is_card_name("card12"));
        assert!(!is_card_name("card0-VGA-1"), "连接器不是卡");
        assert!(!is_card_name("card0-HDMI-A-1"));
        assert!(!is_card_name("renderD128"));
        assert!(!is_card_name("version"));
        assert!(!is_card_name("card"));
    }

    /// 假 sysfs 树。放在 target 同级的系统临时目录，测试结束即删。
    struct FakeSysfs {
        root: PathBuf,
    }

    impl FakeSysfs {
        fn new(tag: &str) -> FakeSysfs {
            let root = std::env::temp_dir().join(format!(
                "strixmaid-gpu-test-{}-{tag}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            FakeSysfs { root }
        }

        fn write(&self, rel: &str, content: &str) {
            let p = self.root.join(rel);
            std::fs::create_dir_all(p.parent().unwrap()).unwrap();
            std::fs::write(p, content).unwrap();
        }
    }

    impl Drop for FakeSysfs {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn 完整的卡产出四条样本() {
        let fs = FakeSysfs::new("full");
        fs.write("card0/device/gpu_busy_percent", "37\n");
        fs.write("card0/device/mem_info_vram_used", "1073741824\n");
        fs.write("card0/device/mem_info_vram_total", "8589934592\n");
        fs.write("card0/device/hwmon/hwmon3/temp1_input", "56000\n");
        // 连接器目录与不可采集的第二块卡都要被跳过。
        fs.write("card0-VGA-1/device/gpu_busy_percent", "99\n");
        fs.write("card1/device/mem_info_vram_used", "1\n");

        let out = GpuCollector::with_dir(&fs.root)
            .collect(Instant::now())
            .unwrap();
        assert_eq!(out.len(), 4, "{out:?}");
        assert!(
            out.iter()
                .all(|s| s.labels == vec![(label::GPU, "card0".to_string())]),
            "card1 没有 gpu_busy_percent，不可采集"
        );
        let get = |m: &str| out.iter().find(|s| s.metric == m).unwrap().value;
        assert_eq!(get(cat::GPU_USAGE), 37.0);
        assert_eq!(get(cat::GPU_MEM_USED), 1073741824.0);
        assert_eq!(get(cat::GPU_MEM_TOTAL), 8589934592.0);
        assert_eq!(get(cat::GPU_TEMP), 56.0, "毫摄氏度换算成摄氏度");
    }

    #[test]
    fn 只有利用率的卡也产出() {
        let fs = FakeSysfs::new("busy-only");
        fs.write("card2/device/gpu_busy_percent", "5");
        let out = GpuCollector::with_dir(&fs.root)
            .collect(Instant::now())
            .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].metric, cat::GPU_USAGE);
        assert_eq!(out[0].labels, vec![(label::GPU, "card2".to_string())]);
    }

    #[test]
    fn 无_gpu_机器返回空且不报错() {
        let mut c = GpuCollector::with_dir("/nonexistent/strixmaid-drm");
        assert!(c.collect(Instant::now()).unwrap().is_empty());

        // 目录存在但没有任何可采集的卡（本机若只有 BMC 显示芯片即真实此况）。
        let fs = FakeSysfs::new("empty");
        fs.write("version", "drm 1.1.0\n");
        assert!(
            GpuCollector::with_dir(&fs.root)
                .collect(Instant::now())
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    fn 本机采集不报错且值域合理() {
        let out = GpuCollector::new().collect(Instant::now()).unwrap();
        // 本机可能没有可采集的 GPU；有则逐项检查。
        for s in &out {
            assert!(s.value.is_finite() && s.value >= 0.0, "{} = {}", s.metric, s.value);
            assert!(s.metric.starts_with("gpu."));
            assert_eq!(s.labels[0].0, label::GPU);
        }
    }
}
