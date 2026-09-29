//! 属性解包、对象路径解码与摘要/计时器转换（无总线请求）。
use super::super::{
    lookup_enable_state, opt_u64, parse_active_state, parse_load_state, summary_for_unloaded_file,
    unit_file_basename, unit_type_of,
};
use super::{UNIT_PATH_PREFIX, UnitListEntry};
use std::collections::{HashMap, HashSet};
use strixmaid_types::service::{CgroupUsage, UnitScope, UnitSummary};
use zbus::zvariant::OwnedValue;
/// `GetAll` 返回的属性包，按名字取值并做类型转换。
#[derive(Debug, Default)]
pub(super) struct Props(pub(super) HashMap<String, OwnedValue>);

impl Props {
    fn take<T: TryFrom<OwnedValue>>(&mut self, key: &str) -> Option<T> {
        self.0.remove(key).and_then(|v| T::try_from(v).ok())
    }
    /// 取原始值（复合类型自己拆）。
    pub(super) fn value(&mut self, key: &str) -> Option<OwnedValue> {
        self.0.remove(key)
    }
    pub(super) fn string(&mut self, key: &str) -> String {
        self.take::<String>(key).unwrap_or_default()
    }
    /// 空串视为「未设置」。
    pub(super) fn opt_string(&mut self, key: &str) -> Option<String> {
        self.take::<String>(key).filter(|s| !s.is_empty())
    }
    pub(super) fn strings(&mut self, key: &str) -> Vec<String> {
        self.take::<Vec<String>>(key).unwrap_or_default()
    }
    pub(super) fn u64(&mut self, key: &str) -> Option<u64> {
        self.take::<u64>(key)
    }
    pub(super) fn u32(&mut self, key: &str) -> Option<u32> {
        self.take::<u32>(key)
    }
    pub(super) fn i32(&mut self, key: &str) -> Option<i32> {
        self.take::<i32>(key)
    }
}

/// unit 类型 → 带 cgroup / 进程信息的类型接口。target / timer / path / device 没有。
pub(super) fn type_interface(unit_type: &str) -> Option<&'static str> {
    Some(match unit_type {
        "service" => "org.freedesktop.systemd1.Service",
        "socket" => "org.freedesktop.systemd1.Socket",
        "mount" => "org.freedesktop.systemd1.Mount",
        "swap" => "org.freedesktop.systemd1.Swap",
        "slice" => "org.freedesktop.systemd1.Slice",
        "scope" => "org.freedesktop.systemd1.Scope",
        _ => return None,
    })
}

/// unit 对象路径 → unit 名。systemd 把非 `[A-Za-z0-9]` 的字节转义成 `_XX`（小写 hex）。
pub fn unit_name_from_path(path: &str) -> Option<String> {
    let escaped = path.strip_prefix(UNIT_PATH_PREFIX)?;
    let bytes = escaped.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'_'
            && i + 2 < bytes.len()
            && let (Some(h), Some(l)) = (hex_val(bytes[i + 1]), hex_val(bytes[i + 2]))
        {
            out.push(h << 4 | l);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn hex_val(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

/// 合并已加载 unit 与 unit 文件列表。
pub(super) fn merge_lists(
    loaded: Vec<UnitListEntry>,
    files: Vec<(String, String)>,
    scope: UnitScope,
) -> Vec<UnitSummary> {
    let file_states: HashMap<String, String> = files
        .into_iter()
        .filter_map(|(path, state)| unit_file_basename(&path).map(|n| (n.to_owned(), state)))
        .collect();

    let mut seen = HashSet::with_capacity(loaded.len());
    let mut out = Vec::with_capacity(loaded.len() + file_states.len());
    for (name, description, load, active, sub, _following, _path, _job_id, _job_type, _job_path) in
        loaded
    {
        seen.insert(name.clone());
        out.push(UnitSummary {
            unit_type: unit_type_of(&name).to_owned(),
            description,
            load_state: parse_load_state(&load),
            active_state: parse_active_state(&active),
            sub_state: sub,
            enable_state: lookup_enable_state(&file_states, &name),
            scope,
            name,
        });
    }
    for (name, state) in &file_states {
        // alias 指向的 unit 已经以本名出现过了。
        if !seen.contains(name) && state != "alias" {
            out.push(summary_for_unloaded_file(name, state, scope));
        }
    }
    out
}

/// Timer 的 `TimersCalendar`（`a(sst)`）/ `TimersMonotonic`（`a(stt)`）→ `Base=spec` 行。
///
/// 第二个字段是日历表达式（字符串）或相对偏移（微秒）；第三个字段（下次触发）
/// 由 `NextElapseUSec*` 统一给出，不进调度行。手工拆 `Value` 而不是 `TryFrom` 成元组，
/// 属性形状对不上时得到的是空数组而不是错误——调度规则「拿不到」不该毁掉整条记录。
pub(super) fn timer_spec_lines(v: &zbus::zvariant::Value<'_>) -> Vec<String> {
    use zbus::zvariant::Value;
    let Value::Array(arr) = v else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|item| {
            let Value::Structure(s) = item else {
                return None;
            };
            let f = s.fields();
            let Value::Str(base) = f.first()? else {
                return None;
            };
            match f.get(1)? {
                Value::Str(spec) => Some(format!("{base}={spec}")),
                Value::U64(usec) => Some(format!("{base}={}s", usec / 1_000_000)),
                _ => None,
            }
        })
        .collect()
}

/// `NextElapseUSecMonotonic`（CLOCK_MONOTONIC 微秒）→ unix 秒。
///
/// monotonic 时钟不含 epoch 信息，换算靠「现在的 monotonic 读数」对齐：
/// `unix_now + (next_mono - now_mono)`。已过期（差为负）视为「马上」，报当前时刻。
pub(super) fn monotonic_usec_to_ts(next_mono_usec: u64) -> Option<i64> {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    // SAFETY: 只写入栈上的 timespec。
    if unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) } != 0 {
        return None;
    }
    let now_mono_usec = ts.tv_sec * 1_000_000 + ts.tv_nsec / 1_000;
    let delta_secs = (next_mono_usec as i64 - now_mono_usec) / 1_000_000;
    let unix_now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .ok()?
        .as_secs() as i64;
    Some(unix_now + delta_secs.max(0))
}

/// 直读缺失的字段用 systemd 属性补齐。
pub(super) fn fill_from_props(usage: &mut CgroupUsage, t: &mut Props) {
    if usage.cpu_usage_nsec.is_none() {
        usage.cpu_usage_nsec = t.u64("CPUUsageNSec").and_then(opt_u64);
    }
    if usage.memory_current_bytes.is_none() {
        usage.memory_current_bytes = t.u64("MemoryCurrent").and_then(opt_u64);
    }
    if usage.memory_peak_bytes.is_none() {
        usage.memory_peak_bytes = t.u64("MemoryPeak").and_then(opt_u64);
    }
    if usage.memory_limit_bytes.is_none() {
        usage.memory_limit_bytes = t.u64("MemoryMax").and_then(opt_u64);
    }
    if usage.tasks_current.is_none() {
        usage.tasks_current = t.u64("TasksCurrent").and_then(opt_u64);
    }
    if usage.tasks_limit.is_none() {
        usage.tasks_limit = t.u64("TasksMax").and_then(opt_u64);
    }
}
