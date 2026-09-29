//! 固定容量的回看缓冲。

use super::SCROLLBACK_CAP;

// ===========================================================================
// RingBuf
// ===========================================================================

/// 固定容量的字节环形缓冲：写满后覆盖最旧的字节。
///
/// 用它而不是 `VecDeque<u8>` + `truncate`：回看缓冲的写入是高频的（每次 PTY 输出一次），
/// 而读出只在附着时发生一次。环形缓冲让写入恒定是两次 `copy_from_slice`，不搬运已有数据，
/// 也不重新分配——代价只是读出时要拼两段。
#[derive(Debug)]
pub struct RingBuf {
    buf: Box<[u8]>,
    /// 最旧那个字节的下标。
    start: usize,
    /// 已用字节数，永远 `<= buf.len()`。
    len: usize,
}

impl Default for RingBuf {
    fn default() -> Self {
        Self::with_capacity(SCROLLBACK_CAP)
    }
}

impl RingBuf {
    /// 容量为 [`SCROLLBACK_CAP`] 的缓冲。
    pub fn new() -> Self {
        Self::default()
    }

    /// 指定容量。容量 0 表示「不保留任何回看」，写入被直接丢弃。
    pub fn with_capacity(cap: usize) -> Self {
        RingBuf {
            buf: vec![0u8; cap].into_boxed_slice(),
            start: 0,
            len: 0,
        }
    }

    /// 容量上限。
    pub fn capacity(&self) -> usize {
        self.buf.len()
    }

    /// 当前保留的字节数。
    pub fn len(&self) -> usize {
        self.len
    }

    /// 是否为空。
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// 追加一段字节，必要时覆盖最旧的。
    pub fn push(&mut self, data: &[u8]) {
        let cap = self.buf.len();
        if cap == 0 || data.is_empty() {
            return;
        }
        // 一次写入就超过容量：旧内容注定一个字节都留不下，直接取尾部重置。
        // 单独处理这一支不只是为了快——下面的两段拷贝假设 `data.len() < cap`，
        // 混在一起写会在「一次写入正好跨越自己」时出错。
        if data.len() >= cap {
            self.buf.copy_from_slice(&data[data.len() - cap..]);
            self.start = 0;
            self.len = cap;
            return;
        }

        let write_at = (self.start + self.len) % cap;
        let first = std::cmp::min(data.len(), cap - write_at);
        self.buf[write_at..write_at + first].copy_from_slice(&data[..first]);
        let rest = data.len() - first;
        if rest > 0 {
            self.buf[..rest].copy_from_slice(&data[first..]);
        }

        let filled = self.len + data.len();
        if filled > cap {
            // 溢出多少就丢掉多少个最旧的字节。
            self.start = (self.start + (filled - cap)) % cap;
            self.len = cap;
        } else {
            self.len = filled;
        }
    }

    /// 按写入顺序读出全部内容。
    pub fn to_vec(&self) -> Vec<u8> {
        let cap = self.buf.len();
        let mut out = Vec::with_capacity(self.len);
        if self.len == 0 {
            return out;
        }
        let first = std::cmp::min(self.len, cap - self.start);
        out.extend_from_slice(&self.buf[self.start..self.start + first]);
        out.extend_from_slice(&self.buf[..self.len - first]);
        out
    }

    /// 清空。
    pub fn clear(&mut self) {
        self.start = 0;
        self.len = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    // -------------------------------------------------------------- RingBuf

    #[test]
    fn 未写满时按写入顺序读出() {
        let mut r = RingBuf::with_capacity(8);
        r.push(b"ab");
        r.push(b"cd");
        assert_eq!(r.len(), 4);
        assert_eq!(r.to_vec(), b"abcd".to_vec());
    }

    #[test]
    fn 环绕后读出的是最后一段() {
        let mut r = RingBuf::with_capacity(8);
        r.push(b"abcde");
        r.push(b"fghij");
        // 一共写了 10 个字节，只该留下最后 8 个，且顺序不变。
        assert_eq!(r.len(), 8);
        assert_eq!(r.to_vec(), b"cdefghij".to_vec());

        // 再绕一圈：start 已经不在 0 上，这一步才真正考验下标计算。
        r.push(b"klm");
        assert_eq!(r.to_vec(), b"fghijklm".to_vec());
    }

    #[test]
    fn 一次写入超过容量只保留末尾() {
        let mut r = RingBuf::with_capacity(8);
        r.push(b"xxxx");
        r.push(b"0123456789abcdefghij");
        assert_eq!(r.len(), 8);
        assert_eq!(r.to_vec(), b"cdefghij".to_vec());
    }

    #[test]
    fn 任意分片写入都与朴素模型一致() {
        // 朴素模型：把所有字节接起来，只留最后 cap 个。环形缓冲的每一步都必须与它相同。
        // 只测「没写满」的用例抓不到环绕下标算错，这个逐步比对能。
        const CAP: usize = 64;
        let mut ring = RingBuf::with_capacity(CAP);
        let mut model: Vec<u8> = Vec::new();
        let mut byte: u8 = 0;
        // 固定种子的 LCG：分片长度覆盖「不跨界」「正好到界」「跨界」「超过容量」四种。
        let mut seed: u32 = 0x9E37_79B9;
        for _ in 0..500 {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            let n = (seed >> 16) as usize % (CAP * 2 + 3);
            let chunk: Vec<u8> = (0..n)
                .map(|_| {
                    byte = byte.wrapping_add(1);
                    byte
                })
                .collect();
            ring.push(&chunk);
            model.extend_from_slice(&chunk);
            if model.len() > CAP {
                model.drain(..model.len() - CAP);
            }
            assert_eq!(ring.to_vec(), model, "写入 {n} 字节后不一致");
            assert_eq!(ring.len(), model.len());
        }
    }

    #[test]
    fn 零容量的缓冲不保留任何内容() {
        let mut r = RingBuf::with_capacity(0);
        r.push(b"abc");
        assert!(r.is_empty());
        assert_eq!(r.to_vec(), Vec::<u8>::new());
    }
}
