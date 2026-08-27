//! `/proc/<pid>/stat` 的 `tty_nr` → 设备名（`pts/0`、`tty1`、`ttyS0`）。

/// 解码 `tty_nr`。0 表示没有控制终端。
pub fn tty_name(tty_nr: i32) -> Option<String> {
    if tty_nr <= 0 {
        return None;
    }
    let nr = tty_nr as u32;
    // Linux dev_t 编码：major 在 bit 8–19，minor 在 bit 0–7 与 bit 20–31
    let major = (nr >> 8) & 0xfff;
    let minor = (nr & 0xff) | ((nr >> 12) & 0xfff00);
    Some(match major {
        4 if minor < 64 => format!("tty{minor}"),
        4 => format!("ttyS{}", minor - 64),
        136..=143 => format!("pts/{}", minor + (major - 136) * 256),
        188 => format!("ttyUSB{minor}"),
        204 if minor >= 64 => format!("ttyAMA{}", minor - 64),
        _ => format!("{major}:{minor}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 常见终端() {
        assert_eq!(tty_name(0), None);
        assert_eq!(tty_name(-1), None);
        assert_eq!(tty_name(34816).as_deref(), Some("pts/0")); // 136 << 8
        assert_eq!(tty_name(34819).as_deref(), Some("pts/3"));
        assert_eq!(tty_name(1025).as_deref(), Some("tty1")); // 4 << 8 | 1
        assert_eq!(tty_name(1088).as_deref(), Some("ttyS0")); // 4 << 8 | 64
        assert_eq!(tty_name((188 << 8) | 2).as_deref(), Some("ttyUSB2"));
        assert_eq!(tty_name((7 << 8) | 1).as_deref(), Some("7:1"));
    }
}
