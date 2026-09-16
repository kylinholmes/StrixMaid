//! PDH（性能数据助手）的最小封装——**只为 GPU**。
//!
//! # 为什么其余指标都不用它
//!
//! PDH 是 Windows 上「什么都能查」的通用计数器接口，但它有两个代价：
//! 速率型计数器必须采两轮才有值，且每次查询都要走一遍计数器名解析。
//! 本项目的其余指标都有更直接、更便宜的来源：
//!
//! | 指标 | 本项目的来源 | 为什么不用 PDH |
//! |---|---|---|
//! | CPU | `NtQuerySystemInformation` | 一次调用拿全逐核 tick，且与进程表同源 |
//! | 内存 | `GlobalMemoryStatusEx` + 页面文件信息 | 直接是字节数，无需差分 |
//! | 磁盘 | `IOCTL_DISK_PERFORMANCE` | 与 `iostat` 同源，能分到物理盘 |
//! | 网络 | `GetIfTable2` | 64 位计数不回绕 |
//! | 运行队列 | 进程表里的线程状态 | 同一次调用顺手算出来 |
//!
//! **只有 GPU 没有别的路**：`\GPU Engine(*)\Utilization Percentage` 与
//! `\GPU Adapter Memory(*)\Dedicated Usage` 是 WDDM 在 Win10 1709+ 暴露的
//! 唯一非 COM 接口。替代方案 DXGI 的 `QueryVideoMemoryInfo` 要起 COM、
//! 还只能看到本进程视角的显存预算，给不出「整机 GPU 利用率」。
//!
//! # 用 `PdhAddEnglishCounterW` 而不是 `PdhAddCounterW`
//!
//! 计数器路径里的对象名与计数器名在本地化的 Windows 上是**翻译过的**
//! （德文机器上 `\Prozessor(_Total)\Prozessorzeit (%)`）。`*EnglishCounter*`
//! 那一族固定按英文名解析，是唯一能把计数器路径写死在代码里的方式。
//! 这与 [`super::token`] 里给内建组补英文规范名是同一类问题。

use std::io;

use windows_sys::Win32::System::Performance::{
    PDH_FMT_COUNTERVALUE_ITEM_W, PDH_FMT_DOUBLE, PDH_HCOUNTER, PDH_HQUERY, PdhAddEnglishCounterW,
    PdhCloseQuery,
    PdhCollectQueryData, PdhGetFormattedCounterArrayW, PdhOpenQueryW,
};

use super::wide::{from_wide_ptr, to_wide};

/// `PDH_FMT_NOCAP100`（pdh.h）。windows-sys 只生成了 `PDH_FMT_*` 里的格式位，
/// 没生成这个修饰位，所以手写。
///
/// 不加它的话，PDH 会把百分比型计数器**截断到 100**。多 GPU 引擎合计的
/// 「GPU 使用率」本来就可以超过 100%（每个引擎各算 100%），截断会让繁忙的
/// 多引擎显卡恒读 100，看不出到底多忙。
const PDH_FMT_NOCAP100: u32 = 0x0000_8000;

/// `PDH_CSTATUS_VALID_DATA` / `ERROR_SUCCESS`
const PDH_SUCCESS: u32 = 0;
/// `PDH_MORE_DATA`
const PDH_MORE_DATA: u32 = 0x800007D2;

/// 一条通配计数器的一项取值。
#[derive(Debug, Clone, PartialEq)]
pub struct CounterItem {
    /// 实例名，如 `pid_1234_luid_0x00000000_0x0000C3F5_phys_0_eng_0_engtype_3D`。
    pub instance: String,
    /// 取值。
    pub value: f64,
}

/// 一次性的 PDH 查询：打开 → 加计数器 → 采两轮 → 取值 → 关。
///
/// # 为什么每次都重新打开
///
/// GPU 引擎实例随进程增减而变（每个用 GPU 的进程一组实例），长命查询要不断
/// 重新展开通配符才能看到新实例。既然每轮都得重新解析，不如每轮重开——
/// 代价是一次 `PdhOpenQueryW`（微秒级），换来的是不必维护实例表的生命周期。
///
/// # 两轮采集之间的间隔
///
/// 利用率类计数器是「两次采样之间的占比」，PDH 要求两轮之间有真实的时间流逝。
/// 间隔太短会得到抖动很大的值，太长会拖慢整轮采集。100ms 是 Windows 任务管理器
/// 自己用的量级。
pub fn query_wildcard(path: &str, gap: std::time::Duration) -> io::Result<Vec<CounterItem>> {
    let mut query: PDH_HQUERY = std::ptr::null_mut();
    // SAFETY: 数据源为空表示实时数据；query 是输出参数。
    let rc = unsafe { PdhOpenQueryW(std::ptr::null(), 0, &raw mut query) };
    if rc != PDH_SUCCESS {
        return Err(io::Error::other(format!("PdhOpenQueryW 失败: {rc:#x}")));
    }
    let _guard = QueryGuard(query);

    let wide_path = to_wide(path);
    let mut counter: PDH_HCOUNTER = std::ptr::null_mut();
    // SAFETY: query 有效；wide_path 以 NUL 结尾；counter 是输出参数。
    let rc = unsafe { PdhAddEnglishCounterW(query, wide_path.as_ptr(), 0, &raw mut counter) };
    if rc != PDH_SUCCESS {
        // 计数器不存在（没有 WDDM GPU、或系统太老）是**正常情况**，
        // 交给调用方按「本机没有这项能力」处理。
        return Err(io::Error::other(format!(
            "PdhAddEnglishCounterW({path}) 失败: {rc:#x}"
        )));
    }

    // SAFETY: query 有效。
    let rc = unsafe { PdhCollectQueryData(query) };
    if rc != PDH_SUCCESS {
        return Err(io::Error::other(format!("首轮采集失败: {rc:#x}")));
    }
    std::thread::sleep(gap);
    // SAFETY: 同上。
    let rc = unsafe { PdhCollectQueryData(query) };
    if rc != PDH_SUCCESS {
        return Err(io::Error::other(format!("次轮采集失败: {rc:#x}")));
    }

    // 先问缓冲大小。
    let mut size: u32 = 0;
    let mut count: u32 = 0;
    // SAFETY: 缓冲为空时只回填大小与条数。
    let rc = unsafe {
        PdhGetFormattedCounterArrayW(
            counter,
            PDH_FMT_DOUBLE | PDH_FMT_NOCAP100,
            &raw mut size,
            &raw mut count,
            std::ptr::null_mut(),
        )
    };
    if rc != PDH_MORE_DATA || size == 0 || count == 0 {
        // 没有任何实例：GPU 空闲时确实可能一个引擎实例都没有。
        return Ok(Vec::new());
    }

    // 缓冲里既有 PDH_FMT_COUNTERVALUE_ITEM_W 数组，也有它们指向的实例名字符串，
    // 所以按字节分配、按结构体读取。
    let mut buf = vec![0u8; size as usize];
    // SAFETY: buf 有 size 字节可写，size 如实描述其大小。
    let rc = unsafe {
        PdhGetFormattedCounterArrayW(
            counter,
            PDH_FMT_DOUBLE | PDH_FMT_NOCAP100,
            &raw mut size,
            &raw mut count,
            buf.as_mut_ptr().cast::<PDH_FMT_COUNTERVALUE_ITEM_W>(),
        )
    };
    if rc != PDH_SUCCESS {
        return Err(io::Error::other(format!(
            "PdhGetFormattedCounterArrayW 失败: {rc:#x}"
        )));
    }

    let each = std::mem::size_of::<PDH_FMT_COUNTERVALUE_ITEM_W>();
    let mut out = Vec::with_capacity(count as usize);
    for i in 0..count as usize {
        if (i + 1) * each > buf.len() {
            break;
        }
        // SAFETY: 刚确认过这一项完整落在缓冲内。
        let item = unsafe {
            std::ptr::read_unaligned(
                buf.as_ptr()
                    .add(i * each)
                    .cast::<PDH_FMT_COUNTERVALUE_ITEM_W>(),
            )
        };
        // 单项状态非 0 表示这个实例本轮没有有效值（进程刚退出等），跳过。
        if item.FmtValue.CStatus != PDH_SUCCESS {
            continue;
        }
        // SAFETY: szName 指向同一缓冲内部，buf 在本作用域内有效。
        let instance = unsafe { from_wide_ptr(item.szName) };
        // SAFETY: 上面确认了 CStatus 有效，按 PDH_FMT_DOUBLE 取 doubleValue。
        let value = unsafe { item.FmtValue.Anonymous.doubleValue };
        if value.is_finite() {
            out.push(CounterItem { instance, value });
        }
    }
    Ok(out)
}

/// `PdhCloseQuery` 的 RAII。
struct QueryGuard(PDH_HQUERY);

impl Drop for QueryGuard {
    fn drop(&mut self) {
        // SAFETY: 句柄由 PdhOpenQueryW 成功返回，只关一次。
        unsafe {
            PdhCloseQuery(self.0);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// 不存在的计数器要报错，而不是给一个空结果冒充「本机没有实例」——
    /// 两者对调用方的含义不同：前者是「这项能力不存在」，后者是「有能力但此刻没数据」。
    #[test]
    fn 不存在的计数器报错() {
        let r = query_wildcard(r"\没有这个对象(*)\没有这个计数器", Duration::from_millis(1));
        assert!(r.is_err());
    }

    /// 处理器计数器在任何 Windows 上都存在，用它验证整条管线（打开→加→两轮→取值）。
    /// 不拿它做指标（CPU 走 NtQuerySystemInformation，见模块文档），只做自检。
    #[test]
    fn 处理器计数器可取值() {
        let items = query_wildcard(
            r"\Processor(*)\% Processor Time",
            Duration::from_millis(120),
        )
        .expect("处理器计数器在任何 Windows 上都该有");
        assert!(!items.is_empty(), "至少有 _Total 一个实例");
        assert!(
            items.iter().any(|i| i.instance.eq_ignore_ascii_case("_Total")),
            "实例名里应当有 _Total：{:?}",
            items.iter().map(|i| &i.instance).collect::<Vec<_>>()
        );
        for i in &items {
            assert!(i.value.is_finite(), "{} 的值不是有限数", i.instance);
            assert!(i.value >= 0.0, "{} = {}", i.instance, i.value);
        }
    }

    /// GPU 计数器是本模块存在的理由；没有 WDDM GPU 的机器（服务器、虚拟机）
    /// 上它不存在，那不是失败——如实跳过。
    #[test]
    fn gpu_计数器要么可用要么明确缺席() {
        match query_wildcard(
            r"\GPU Engine(*)\Utilization Percentage",
            Duration::from_millis(120),
        ) {
            Ok(items) => {
                eprintln!("GPU 引擎实例 {} 个", items.len());
                for i in items.iter().take(3) {
                    eprintln!("  {} = {:.2}%", i.instance, i.value);
                    assert!(i.value >= 0.0);
                }
            }
            Err(e) => eprintln!("本机没有 GPU Engine 计数器（虚拟机 / 无 WDDM 驱动）：{e}"),
        }
    }
}
