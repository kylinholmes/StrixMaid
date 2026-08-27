//! 健康证据的 macOS 取证。
//!
//! 判定规则在 [`super::super::health`]，本文件只负责取证。macOS 上能取到的证据比
//! Linux 少一项——没有「需要重启」的标记，原因见 [`super::collect_health`] 里的注释。

/// 1 分钟负载均值，`getloadavg(3)`。
pub fn read_load1() -> Option<f64> {
    let mut buf = [0f64; 3];
    // SAFETY: buf 是 3 个 c_double 的可写数组，nelem 如实描述其长度。
    let n = unsafe { libc::getloadavg(buf.as_mut_ptr(), 3) };
    (n >= 1).then_some(buf[0]).filter(|v| v.is_finite())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn 本机负载() {
        let l = read_load1().expect("getloadavg");
        assert!(l >= 0.0 && l.is_finite(), "负载异常：{l}");
    }
}
