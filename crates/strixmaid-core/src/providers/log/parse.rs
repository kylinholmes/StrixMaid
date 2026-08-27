//! `journalctl -o json` 输出解析，以及两个纯函数工具（UTC 时间戳、boot 列表文本格式）。
//!
//! journalctl 的 JSON 每行一个对象，值的形状有四种（`journalctl(1)` OUTPUT OPTIONS）：
//!
//! - 字符串：常规；
//! - `null`：字段超过 4096 字节被截断（我们传 `--all` 关掉这个行为，但仍要容忍）；
//! - 数字数组：非 UTF-8 / 含不可打印字节的值，逐字节编码；
//! - 字符串数组：同名字段出现多次。
//!
//! 全部收敛成 `String`（有损），解析失败的行整行跳过而不是让整页失败。

use std::collections::BTreeMap;

use serde_json::{Map, Value};
use strixmaid_types::log::{BootInfo, LogEntry, LogEntryDetail, LogPriority};

/// 一行 JSON 对象。
pub type RawEntry = Map<String, Value>;

/// 解析一行。不是 JSON 对象（空行、`-- No entries --`、被截断的行）返回 `None`。
pub fn parse_line(line: &str) -> Option<RawEntry> {
    match serde_json::from_str::<Value>(line.trim()) {
        Ok(Value::Object(m)) => Some(m),
        _ => None,
    }
}

/// 把一个字段值转成字符串，见模块文档。`null` 返回 `None`。
pub fn field_string(v: &Value) -> Option<String> {
    match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Null => None,
        Value::Array(items) => {
            // 字节数组：每个元素都是 0..=255 的整数。
            if !items.is_empty() && items.iter().all(|i| i.as_u64().is_some_and(|b| b <= 255)) {
                let bytes: Vec<u8> = items
                    .iter()
                    .filter_map(|i| i.as_u64())
                    .map(|b| b as u8)
                    .collect();
                return Some(String::from_utf8_lossy(&bytes).into_owned());
            }
            // 多值字段：逐个转换，换行拼接。
            let parts: Vec<String> = items.iter().filter_map(field_string).collect();
            (!parts.is_empty()).then(|| parts.join("\n"))
        }
        Value::Object(_) => Some(v.to_string()),
    }
}

/// 取字符串字段。
pub fn get_str(m: &RawEntry, key: &str) -> Option<String> {
    m.get(key).and_then(field_string)
}

/// 取整数字段。journalctl 把数字也输出成字符串（`"PRIORITY":"6"`），两种都收。
pub fn get_u64(m: &RawEntry, key: &str) -> Option<u64> {
    match m.get(key)? {
        Value::Number(n) => n.as_u64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// 原始行 → 列表条目。没有 `__CURSOR` 或 `__REALTIME_TIMESTAMP` 的行无法定位，返回 `None`。
pub fn entry_from_raw(m: &RawEntry) -> Option<LogEntry> {
    let cursor = get_str(m, "__CURSOR")?;
    let usec = get_u64(m, "__REALTIME_TIMESTAMP")?;
    let priority = get_u64(m, "PRIORITY")
        .and_then(|p| u8::try_from(p).ok())
        .and_then(LogPriority::from_u8)
        .unwrap_or(LogPriority::Info);
    Some(LogEntry {
        cursor,
        ts: (usec / 1_000_000) as i64,
        us: (usec % 1_000_000) as u32,
        priority,
        message: get_str(m, "MESSAGE").unwrap_or_default(),
        // 系统 unit 优先；用户 unit 的消息只有 _SYSTEMD_USER_UNIT；内核消息回落到 UNIT。
        unit: get_str(m, "_SYSTEMD_UNIT")
            .or_else(|| get_str(m, "_SYSTEMD_USER_UNIT"))
            .or_else(|| get_str(m, "UNIT")),
        identifier: get_str(m, "SYSLOG_IDENTIFIER"),
        pid: get_u64(m, "_PID").and_then(|p| u32::try_from(p).ok()),
        uid: get_u64(m, "_UID").and_then(|u| u32::try_from(u).ok()),
        hostname: get_str(m, "_HOSTNAME"),
        boot_id: get_str(m, "_BOOT_ID"),
        transport: get_str(m, "_TRANSPORT"),
    })
}

/// 原始行 → 全字段详情。`null`（截断）字段跳过。
pub fn detail_from_raw(m: &RawEntry) -> Option<LogEntryDetail> {
    let entry = entry_from_raw(m)?;
    let fields: BTreeMap<String, String> = m
        .iter()
        .filter_map(|(k, v)| field_string(v).map(|s| (k.clone(), s)))
        .collect();
    Some(LogEntryDetail { entry, fields })
}

/// `journalctl --list-boots -o json` 的一行。
#[derive(Debug, serde::Deserialize)]
struct BootRow {
    index: i32,
    boot_id: String,
    /// 微秒 epoch。
    first_entry: u64,
    last_entry: u64,
}

/// 解析 `--list-boots -o json`。
pub fn boots_from_json(s: &str) -> Option<Vec<BootInfo>> {
    let rows: Vec<BootRow> = serde_json::from_str(s).ok()?;
    Some(
        rows.into_iter()
            .map(|r| BootInfo {
                index: r.index,
                boot_id: r.boot_id,
                first_ts: (r.first_entry / 1_000_000) as i64,
                last_ts: (r.last_entry / 1_000_000) as i64,
            })
            .collect(),
    )
}

/// 解析旧版 `--list-boots` 的文本格式（子进程需设 `TZ=UTC`）：
///
/// ```text
/// -1 3c4d… Tue 2026-08-26 10:02:17 UTC—Wed 2026-08-27 09:00:00 UTC
///  0 5d6e… Wed 2026-08-27 09:01:00 UTC Thu 2026-08-27 17:41:52 UTC
/// ```
///
/// 新旧版本在两个时刻之间用 `—` 或空格分隔，这里不依赖分隔符，只按 token 找日期 + 时间对。
pub fn boots_from_text(s: &str) -> Vec<BootInfo> {
    s.lines()
        .filter_map(|line| {
            let line = line.replace('—', " ");
            let mut tokens = line.split_whitespace();
            let index: i32 = tokens.next()?.parse().ok()?;
            let boot_id = tokens.next()?;
            if boot_id.len() != 32 || !boot_id.bytes().all(|b| b.is_ascii_hexdigit()) {
                return None;
            }
            let rest: Vec<&str> = tokens.collect();
            let mut times = Vec::with_capacity(2);
            for w in rest.windows(2) {
                if let Some(ts) = civil_to_epoch(w[0], w[1]) {
                    times.push(ts);
                }
            }
            Some(BootInfo {
                index,
                boot_id: boot_id.to_owned(),
                first_ts: *times.first()?,
                last_ts: *times.get(1).unwrap_or(times.first()?),
            })
        })
        .collect()
}

/// 解析 systemd 的 UTC 时间戳文本（`Thu 2026-08-27 06:54:25 UTC`），返回 unix 秒。
///
/// 只认 UTC：调用方必须给子进程设 `TZ=UTC`。`n/a` / 空串 / 其它时区返回 `None`。
pub fn parse_utc_timestamp(s: &str) -> Option<i64> {
    let tokens: Vec<&str> = s.split_whitespace().collect();
    if tokens.iter().any(|t| {
        *t != "UTC" && t.len() == 3 && t.chars().all(|c| c.is_ascii_uppercase()) && !is_weekday(t)
    }) {
        // 出现了别的时区缩写（CST / PDT …），按错误处理而不是算出一个偏 8 小时的值。
        return None;
    }
    tokens.windows(2).find_map(|w| civil_to_epoch(w[0], w[1]))
}

fn is_weekday(t: &str) -> bool {
    matches!(t, "Mon" | "Tue" | "Wed" | "Thu" | "Fri" | "Sat" | "Sun")
}

/// `YYYY-MM-DD` + `HH:MM:SS` → unix 秒（按 UTC）。
fn civil_to_epoch(date: &str, time: &str) -> Option<i64> {
    let mut d = date.splitn(3, '-');
    let y: i64 = d.next()?.parse().ok()?;
    let m: u32 = d.next()?.parse().ok()?;
    let day: u32 = d.next()?.parse().ok()?;
    let mut t = time.splitn(3, ':');
    let h: i64 = t.next()?.parse().ok()?;
    let mi: i64 = t.next()?.parse().ok()?;
    let s: i64 = t.next()?.parse().ok()?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&day) || h > 23 || mi > 59 || s > 60 {
        return None;
    }
    Some(days_from_civil(y, m, day) * 86_400 + h * 3600 + mi * 60 + s)
}

/// 公历日期 → 距 1970-01-01 的天数（Howard Hinnant 的算法）。
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m as i64 + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    const LINE_FULL: &str = r#"{"__CURSOR":"s=f978c38c82b9455fa2ad7e5edd8f0a7e;i=452ef3a;b=5d6eb52cf50c4b1cb5950accac688272;m=14480bd4b04b;t=65a0426b2f897;x=84acd4b9a874bd7e","__REALTIME_TIMESTAMP":"1787823668792735","__MONOTONIC_TIMESTAMP":"22299668688971","_BOOT_ID":"5d6eb52cf50c4b1cb5950accac688272","PRIORITY":"4","SYSLOG_IDENTIFIER":"nginx","_PID":"1234","_UID":"33","_HOSTNAME":"web-01","_TRANSPORT":"stdout","_SYSTEMD_UNIT":"nginx.service","MESSAGE":"upstream timed out"}"#;
    const LINE_BYTES: &str = r#"{"__CURSOR":"c2","__REALTIME_TIMESTAMP":"1787823668000001","MESSAGE":[104,105,32,255,32,228,184,173],"PRIORITY":"6","_SYSTEMD_USER_UNIT":"foo.service"}"#;
    const LINE_MINIMAL: &str = r#"{"__CURSOR":"c3","__REALTIME_TIMESTAMP":1787823669000000,"UNIT":"kernel.thing","MESSAGE":null,"MULTI":["a","b"]}"#;

    #[test]
    fn parses_full_line() {
        let m = parse_line(LINE_FULL).unwrap();
        let e = entry_from_raw(&m).unwrap();
        assert_eq!(e.ts, 1_787_823_668);
        assert_eq!(e.us, 792_735);
        assert_eq!(e.priority, LogPriority::Warning);
        assert_eq!(e.unit.as_deref(), Some("nginx.service"));
        assert_eq!(e.identifier.as_deref(), Some("nginx"));
        assert_eq!(e.pid, Some(1234));
        assert_eq!(e.uid, Some(33));
        assert_eq!(e.hostname.as_deref(), Some("web-01"));
        assert_eq!(e.transport.as_deref(), Some("stdout"));
        assert_eq!(
            e.boot_id.as_deref(),
            Some("5d6eb52cf50c4b1cb5950accac688272")
        );
        assert_eq!(e.message, "upstream timed out");
        let d = detail_from_raw(&m).unwrap();
        assert_eq!(d.fields["__MONOTONIC_TIMESTAMP"], "22299668688971");
        assert_eq!(d.entry, e);
    }

    #[test]
    fn byte_array_message_is_lossy_decoded() {
        let m = parse_line(LINE_BYTES).unwrap();
        let e = entry_from_raw(&m).unwrap();
        assert_eq!(e.message, "hi \u{FFFD} 中");
        assert_eq!(
            e.unit.as_deref(),
            Some("foo.service"),
            "用户 unit 回落到 _SYSTEMD_USER_UNIT"
        );
        assert_eq!(e.priority, LogPriority::Info);
        assert_eq!(e.pid, None);
    }

    #[test]
    fn missing_and_null_fields() {
        let m = parse_line(LINE_MINIMAL).unwrap();
        let e = entry_from_raw(&m).unwrap();
        assert_eq!(e.ts, 1_787_823_669);
        assert_eq!(e.us, 0);
        assert_eq!(e.message, "", "null（截断）消息归成空串");
        assert_eq!(
            e.unit.as_deref(),
            Some("kernel.thing"),
            "内核消息回落到 UNIT"
        );
        assert_eq!(e.priority, LogPriority::Info, "缺 PRIORITY 归一成 info");
        let d = detail_from_raw(&m).unwrap();
        assert!(!d.fields.contains_key("MESSAGE"), "null 字段不进 fields");
        assert_eq!(d.fields["MULTI"], "a\nb");

        assert!(parse_line("").is_none());
        assert!(parse_line("-- No entries --").is_none());
        assert!(parse_line("[1,2]").is_none());
        assert!(entry_from_raw(&parse_line(r#"{"MESSAGE":"no cursor"}"#).unwrap()).is_none());
    }

    #[test]
    fn boots_json_and_text() {
        let json = r#"[{"index":-1,"boot_id":"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","first_entry":1787709737833843,"last_entry":1787723420781826},{"index":0,"boot_id":"5d6eb52cf50c4b1cb5950accac688272","first_entry":1787809737833843,"last_entry":1787823420781826}]"#;
        let b = boots_from_json(json).unwrap();
        assert_eq!(b.len(), 2);
        assert_eq!(b[1].index, 0);
        assert_eq!(b[1].first_ts, 1_787_809_737);
        assert_eq!(b[1].last_ts, 1_787_823_420);
        assert!(boots_from_json("not json").is_none());

        let text = "-1 aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa Tue 2026-08-26 02:02:17 UTC—Wed 2026-08-27 09:00:00 UTC\n\
                     0 5d6eb52cf50c4b1cb5950accac688272 Wed 2026-08-27 09:01:00 UTC Thu 2026-08-27 09:41:52 UTC\n\
                    Hint: You are currently not seeing messages from other users and the system.\n";
        let b = boots_from_text(text);
        assert_eq!(b.len(), 2, "提示行被跳过");
        assert_eq!(b[0].index, -1);
        assert_eq!(
            b[0].first_ts,
            civil_to_epoch("2026-08-26", "02:02:17").unwrap()
        );
        assert_eq!(
            b[0].last_ts,
            civil_to_epoch("2026-08-27", "09:00:00").unwrap()
        );
        assert_eq!(b[1].boot_id, "5d6eb52cf50c4b1cb5950accac688272");
        assert_eq!(b[1].last_ts - b[1].first_ts, 40 * 60 + 52);
    }

    #[test]
    fn utc_timestamps() {
        assert_eq!(days_from_civil(1970, 1, 1), 0);
        assert_eq!(days_from_civil(2000, 3, 1), 11_017);
        assert_eq!(civil_to_epoch("1970-01-02", "00:00:00"), Some(86_400));
        // 与 `date -u -d '2026-08-27 06:54:25' +%s` 一致。
        assert_eq!(
            parse_utc_timestamp("Thu 2026-08-27 06:54:25 UTC"),
            Some(1_787_813_665)
        );
        assert_eq!(parse_utc_timestamp("n/a"), None);
        assert_eq!(parse_utc_timestamp(""), None);
        assert_eq!(
            parse_utc_timestamp("Thu 2026-08-27 14:54:25 CST"),
            None,
            "非 UTC 拒绝"
        );
        assert_eq!(civil_to_epoch("2026-13-01", "00:00:00"), None);
    }
}
