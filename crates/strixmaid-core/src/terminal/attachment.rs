//! 附着的输入输出与 Drop 解除语义。

use super::{AttachEvent, Terminal};
use std::{io, sync::Arc};
use tokio::sync::mpsc;

/// 一次附着。`Drop` 即解除附着（终端继续跑）。
///
/// 把解除绑在 `Drop` 上而不是某个 `detach()` 方法：WS 可能因为任务被 abort、panic 展开、
/// 运行时关停而消失，只有 `Drop` 在这些路径上都会执行。漏掉一次解除，终端就会永远
/// 显示 `attached = true`，从而躲过空闲回收——一个不会自愈的泄漏。
pub struct Attachment {
    pub(super) terminal: Arc<Terminal>,
    pub(super) seq: u64,
    pub(super) rx: mpsc::Receiver<AttachEvent>,
}

impl std::fmt::Debug for Attachment {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Attachment")
            .field("terminal", &self.terminal.id)
            .field("seq", &self.seq)
            .finish()
    }
}

impl Attachment {
    /// 取下一个事件；`None` 表示这次附着彻底结束（通道被关闭）。
    ///
    /// 注意 [`AttachEvent::Closed`] 是**尽力而为**的：队列满时投不进去，此时附着方
    /// 只会看到 `None`。因此宿主判断「结束」必须以 `None` 为准，`Closed` 只用来
    /// 决定 close 帧里写什么原因。
    pub async fn next(&mut self) -> Option<AttachEvent> {
        self.rx.recv().await
    }

    /// 把浏览器发来的字节写进 PTY。
    ///
    /// 不加写锁：同一时刻只有一个附着，而键盘输入只从附着方来。
    ///
    /// 走 `writable` + `try_write` 而不是 `AsyncWriteExt::write_all`：后者要 `&mut`，
    /// 而这条 stream 是与泵任务共享的 `Arc`（tokio 的就绪 API 都取 `&self`，
    /// 这正是能一边读一边写而不 split 的原因）。
    pub async fn write(&self, data: &[u8]) -> io::Result<()> {
        let stream = &*self.terminal.stream;
        let mut sent = 0;
        while sent < data.len() {
            stream.writable().await?;
            match stream.try_write(&data[sent..]) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "终端 socket 拒绝写入",
                    ));
                }
                Ok(n) => sent += n,
                // 就绪是一个提示而不是保证：另一个写者可能抢先填满了缓冲。
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => continue,
                Err(e) => return Err(e),
            }
        }
        Ok(())
    }

    /// 所附着终端的 id。
    pub fn terminal_id(&self) -> &str {
        &self.terminal.id
    }

    /// 所附着的终端。
    pub fn terminal(&self) -> &Arc<Terminal> {
        &self.terminal
    }
}

impl Drop for Attachment {
    fn drop(&mut self) {
        self.terminal.detach_if(self.seq);
    }
}
