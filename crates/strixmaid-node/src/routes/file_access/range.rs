//! 单段 bytes Range。畸形、多段或未知单位忽略，合法但不可满足的区间返回 416。
#[derive(Debug, PartialEq, Eq)]
pub enum Selection {
    Full,
    Partial { offset: u64, length: u64 },
    Unsatisfiable,
}
pub fn select(value: Option<&str>, size: u64) -> Selection {
    let Some(value) = value.and_then(|v| v.strip_prefix("bytes=")) else {
        return Selection::Full;
    };
    if value.contains(',') {
        return Selection::Full;
    }
    let Some((start, end)) = value.split_once('-') else {
        return Selection::Full;
    };
    fn number(s: &str) -> Option<u64> {
        if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
            None
        } else {
            s.parse().ok()
        }
    }
    if start.is_empty() {
        let Some(suffix) = number(end) else {
            return Selection::Full;
        };
        if suffix == 0 || size == 0 {
            return Selection::Unsatisfiable;
        }
        let length = suffix.min(size);
        return Selection::Partial {
            offset: size - length,
            length,
        };
    }
    let Some(start) = number(start) else {
        return Selection::Full;
    };
    let end = if end.is_empty() {
        None
    } else {
        let Some(end) = number(end) else {
            return Selection::Full;
        };
        if end < start {
            return Selection::Full;
        }
        Some(end)
    };
    if start >= size {
        return Selection::Unsatisfiable;
    }
    let end = end.unwrap_or(size - 1).min(size - 1);
    Selection::Partial {
        offset: start,
        length: end - start + 1,
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn ranges_and_large_offsets() {
        assert_eq!(
            select(Some("bytes=2-4"), 10),
            Selection::Partial {
                offset: 2,
                length: 3
            }
        );
        assert_eq!(
            select(Some("bytes=8-"), 10),
            Selection::Partial {
                offset: 8,
                length: 2
            }
        );
        assert_eq!(
            select(Some("bytes=-3"), 10),
            Selection::Partial {
                offset: 7,
                length: 3
            }
        );
        assert_eq!(
            select(Some("bytes=-30"), 10),
            Selection::Partial {
                offset: 0,
                length: 10
            }
        );
        assert_eq!(
            select(Some("bytes=4294967296-"), 4294967300),
            Selection::Partial {
                offset: 4294967296,
                length: 4
            }
        );
        for r in ["bytes=10-", "bytes=-0", "bytes=100-200"] {
            assert_eq!(select(Some(r), 10), Selection::Unsatisfiable);
        }
        assert_eq!(select(Some("bytes=0-"), 0), Selection::Unsatisfiable);
        for r in [
            "bytes=4-2",
            "items=1-2",
            "bytes=1-2,4-5",
            "bytes=+1-2",
            "bytes=1-x",
            "bytes=-",
            "bytes=18446744073709551616-",
        ] {
            assert_eq!(select(Some(r), 10), Selection::Full);
        }
    }
}
