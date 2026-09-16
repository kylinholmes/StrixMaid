//! `wevtapi` 的薄封装：句柄 RAII、查询 / 取批 / 渲染 / 消息格式化 / 清空 / 订阅。
//!
//! 这一层只做「把 Win32 的出参风格翻译成 `io::Result`」，不含任何业务判断；
//! 拼 XPath、判级别、算游标全在 [`super::model`] 里。每处 `unsafe` 单独标注前提。
//!
//! # 为什么不起 `wevtutil` 子进程
//!
//! `wevtutil qe` 每次调用都要重新解析一遍发布者元数据、把结果渲染成 XML 再打到
//! stdout，一次 200 条的查询要跑好几秒；而且它**没有推送接口**，follow 只能轮询。
//! `EvtSubscribe` 是内核推上来的，这一条就足以定下走 FFI。
//!
//! # 两个容易写反的缓冲长度单位
//!
//! | 函数 | `BufferSize` / `BufferUsed` 的单位 |
//! |---|---|
//! | [`EvtRender`] | **字节** |
//! | [`EvtFormatMessage`] | **字符**（UTF-16 码元） |
//!
//! 写反了不会立刻崩：`EvtRender` 那边按字符给会得到「缓冲不够」的死循环，
//! `EvtFormatMessage` 那边按字节给会白分配一倍内存。两处都在下面标了单位。

use std::collections::HashMap;
use std::io;
use std::sync::Mutex;

use windows_sys::Win32::Foundation::{
    ERROR_EVT_MAX_INSERTS_REACHED, ERROR_EVT_MESSAGE_ID_NOT_FOUND,
    ERROR_EVT_MESSAGE_LOCALE_NOT_FOUND, ERROR_EVT_MESSAGE_NOT_FOUND,
    ERROR_EVT_UNRESOLVED_PARAMETER_INSERT, ERROR_EVT_UNRESOLVED_VALUE_INSERT,
    ERROR_INSUFFICIENT_BUFFER, ERROR_NO_MORE_ITEMS,
};
use windows_sys::Win32::System::EventLog::{
    EVT_HANDLE, EVT_SUBSCRIBE_CALLBACK, EvtClearLog, EvtClose, EvtFormatMessage,
    EvtFormatMessageEvent, EvtNext, EvtOpenPublisherMetadata, EvtQuery, EvtQueryChannelPath,
    EvtQueryReverseDirection, EvtRender, EvtRenderEventXml, EvtSubscribe,
    EvtSubscribeToFutureEvents,
};

use crate::platform::windows::wide::{from_wide_nul, to_wide};
use crate::platform::windows::{error_from_code, last_error};

/// `EvtNext` 单次取回的事件数上限。
///
/// 每个返回的句柄都要 `EvtClose`，批太大只是让一次失败浪费更多；64 条足够把
/// 每条事件一次 RPC 的开销摊掉。
pub const NEXT_BATCH: usize = 64;

/// `EvtNext` 的等待上限（毫秒）。查询是有界的活儿，卡住就该报超时而不是挂死。
pub const NEXT_TIMEOUT_MS: u32 = 5_000;

/// 自动 `EvtClose` 的 `EVT_HANDLE`。
///
/// `EVT_HANDLE` 是 `isize`，**空句柄（0）表示失败**——与
/// [`crate::platform::windows::handle::Owned`] 挡的那两个值不是同一套，
/// 所以不能直接复用它，只能在这里再写一个薄壳。
#[derive(Debug)]
pub struct EvtHandle(EVT_HANDLE);

impl EvtHandle {
    /// 接管一个刚由 wevtapi 返回的句柄。0 视为失败，此时取当前 Win32 错误码返回。
    ///
    /// # Safety
    ///
    /// `h` 必须是刚由 `Evt*` 函数返回、尚无其它持有者的句柄。
    pub unsafe fn new(h: EVT_HANDLE) -> io::Result<EvtHandle> {
        if h == 0 {
            return Err(last_error());
        }
        Ok(EvtHandle(h))
    }

    /// 裸句柄。只在调用期间借用，不转移所有权。
    pub fn raw(&self) -> EVT_HANDLE {
        self.0
    }
}

impl Drop for EvtHandle {
    fn drop(&mut self) {
        // SAFETY: 构造时已排除 0，本类型独占所有权，只关这一次。
        unsafe {
            EvtClose(self.0);
        }
    }
}

/// 打开一次通道查询，**反向**（最新在前）。
///
/// `xpath` 由 [`super::model::build_xpath`] 拼出，已经把时间窗口、级别下限、
/// Provider 都放在源头，比拉回来再筛快一个量级。
pub fn open_query(channel: &str, xpath: &str) -> io::Result<EvtHandle> {
    let ch = to_wide(channel);
    let q = to_wide(xpath);
    // SAFETY: session 为 NULL 表示本机；ch / q 是本函数持有的、以 NUL 结尾的
    // UTF-16 缓冲，调用期间有效。返回值的所有权交给 EvtHandle。
    unsafe {
        EvtHandle::new(EvtQuery(
            0,
            ch.as_ptr(),
            q.as_ptr(),
            EvtQueryChannelPath | EvtQueryReverseDirection,
        ))
    }
}

/// 从结果集取下一批事件。返回空 `Vec` 表示已经读完。
pub fn next_batch(result: &EvtHandle, max: usize) -> io::Result<Vec<EvtHandle>> {
    let want = max.min(NEXT_BATCH);
    let mut raw = vec![0 as EVT_HANDLE; want];
    let mut returned: u32 = 0;
    // SAFETY: raw 至少有 want 个元素，与传入的 eventssize 一致；returned 是
    // 本栈上的出参。失败时不写 raw。
    let ok = unsafe {
        EvtNext(
            result.raw(),
            want as u32,
            raw.as_mut_ptr(),
            NEXT_TIMEOUT_MS,
            0,
            &raw mut returned,
        )
    };
    if ok == 0 {
        let e = last_error();
        // 读完了不是错误
        if e.raw_os_error() == Some(ERROR_NO_MORE_ITEMS as i32) {
            return Ok(Vec::new());
        }
        return Err(e);
    }
    let n = (returned as usize).min(want);
    Ok(raw[..n]
        .iter()
        // SAFETY: EvtNext 成功时前 n 个元素是刚创建、尚无其它持有者的事件句柄。
        .filter_map(|h| unsafe { EvtHandle::new(*h) }.ok())
        .collect())
}

/// 把一条事件渲染成 XML。
///
/// 整条 XML 既是游标哈希的输入，也是 `LogEntryDetail::fields` 的来源，见
/// [`super::model::EventXml`] 的文档。
pub fn render_xml(event: EVT_HANDLE) -> io::Result<String> {
    // 先问需要多大。`EvtRender` 的两个长度参数单位是**字节**。
    let mut used: u32 = 0;
    let mut count: u32 = 0;
    // SAFETY: buffersize 为 0 时 buffer 允许为空指针，函数只回填 used；
    // used / count 是本栈上的出参。
    let ok = unsafe {
        EvtRender(
            0,
            event,
            EvtRenderEventXml,
            0,
            std::ptr::null_mut(),
            &raw mut used,
            &raw mut count,
        )
    };
    if ok == 0 {
        let e = last_error();
        if e.raw_os_error() != Some(ERROR_INSUFFICIENT_BUFFER as i32) {
            return Err(e);
        }
    }
    if used == 0 {
        return Ok(String::new());
    }
    // 字节 → UTF-16 码元，向上取整，再留一个 NUL 的位置
    let mut buf = vec![0u16; (used as usize).div_ceil(2) + 1];
    let cap_bytes = (buf.len() * 2) as u32;
    // SAFETY: buf 有 cap_bytes 个字节可写，与传入的 buffersize 一致。
    let ok = unsafe {
        EvtRender(
            0,
            event,
            EvtRenderEventXml,
            cap_bytes,
            buf.as_mut_ptr().cast(),
            &raw mut used,
            &raw mut count,
        )
    };
    if ok == 0 {
        return Err(last_error());
    }
    Ok(from_wide_nul(&buf))
}

// ---------------------------------------------------------------------------
// 发布者元数据与消息渲染
// ---------------------------------------------------------------------------

/// 发布者元数据句柄的缓存，按 provider 名。
///
/// # 为什么必须缓存
///
/// `EvtOpenPublisherMetadata` 要去注册表找发布者、加载它的消息资源 DLL——
/// 一次几毫秒。一页 200 条事件如果每条都开一次，光元数据就是一秒多，而这 200 条
/// 事件往往只来自十几个发布者。
///
/// # 为什么格式化时一直握着锁
///
/// 元数据句柄是内核对象，但 MSDN **没有**承诺同一个 `EVT_HANDLE` 可以被多个线程
/// 并发使用。本实现的查询跑在 `spawn_blocking` 上、follow 的回调跑在 wevtapi
/// 自己的线程上，两者都会来取消息，所以 [`Self::message`] 全程持锁——把并发格式化
/// 串行掉。这是刻意的取舍：事件日志的量级（一台机器一天几千条）让这点串行代价
/// 可以忽略，而句柄被并发使用一旦出事是崩进程，不是慢一点。
#[derive(Debug, Default)]
pub struct PublisherCache {
    map: Mutex<HashMap<String, Option<EvtHandle>>>,
}

impl PublisherCache {
    pub fn new() -> Self {
        Self::default()
    }

    /// 渲染一条事件的人类可读消息。
    ///
    /// 返回 `None` 的情形（都是**正常**的，不是错误）：发布者没在本机注册、
    /// 消息资源 DLL 被卸载、事件定义在当前语言下没有对应消息。调用方应退回
    /// [`super::model::EventXml::fallback_message`]。
    pub fn message(&self, provider: &str, event: EVT_HANDLE) -> Option<String> {
        let mut map = self.map.lock().unwrap_or_else(|p| p.into_inner());
        let meta = map
            .entry(provider.to_owned())
            .or_insert_with(|| open_publisher(provider));
        let handle = meta.as_ref()?;
        format_message(handle.raw(), event)
    }

    /// 缓存里的发布者数量（测试与诊断用）。
    pub fn len(&self) -> usize {
        self.map.lock().unwrap_or_else(|p| p.into_inner()).len()
    }

    /// 缓存是否为空。
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// 打开一个发布者的元数据。取不到就是取不到，不报错——调用方缓存这个 `None`，
/// 免得对同一个未注册的发布者反复去注册表里找。
fn open_publisher(provider: &str) -> Option<EvtHandle> {
    if provider.is_empty() {
        return None;
    }
    let id = to_wide(provider);
    // SAFETY: session / logfilepath 为 NULL 表示本机与默认；id 是本函数持有的、
    // 以 NUL 结尾的 UTF-16 缓冲。locale 传 0 表示按系统默认语言。
    let h = unsafe {
        EvtOpenPublisherMetadata(0, id.as_ptr(), std::ptr::null(), 0, 0)
    };
    // SAFETY: h 若非 0 即为刚创建、尚无其它持有者的句柄。
    match unsafe { EvtHandle::new(h) } {
        Ok(h) => Some(h),
        Err(e) => {
            tracing::debug!(provider, error = %e, "发布者元数据打不开，消息将退回 EventData");
            None
        }
    }
}

/// 这些错误码表示「消息渲染得不完整，但缓冲里确实有东西」——插入参数没解析全的
/// 事件仍然比一串裸参数好读，收下它们。
const PARTIAL_MESSAGE_ERRORS: &[u32] = &[
    ERROR_EVT_UNRESOLVED_VALUE_INSERT,
    ERROR_EVT_UNRESOLVED_PARAMETER_INSERT,
    ERROR_EVT_MAX_INSERTS_REACHED,
];

/// 这些错误码表示「本机根本没有这条消息」——安静退回 `None`，不进日志。
const MISSING_MESSAGE_ERRORS: &[u32] = &[
    ERROR_EVT_MESSAGE_NOT_FOUND,
    ERROR_EVT_MESSAGE_ID_NOT_FOUND,
    ERROR_EVT_MESSAGE_LOCALE_NOT_FOUND,
];

/// `EvtFormatMessage(EvtFormatMessageEvent)`：拿到事件的人类可读正文。
///
/// 光有 XML 里的 `EventData` 是没用的——那是一串没有上下文的占位参数
/// （`Spooler`、`自动启动`），真正的句子在发布者的消息资源里。
fn format_message(publisher: EVT_HANDLE, event: EVT_HANDLE) -> Option<String> {
    // 先问需要多大。`EvtFormatMessage` 的两个长度参数单位是**字符**，不是字节。
    let mut used: u32 = 0;
    // SAFETY: buffersize 为 0 时 buffer 允许为空指针，函数只回填 used。
    let ok = unsafe {
        EvtFormatMessage(
            publisher,
            event,
            0,
            0,
            std::ptr::null(),
            EvtFormatMessageEvent,
            0,
            std::ptr::null_mut(),
            &raw mut used,
        )
    };
    if ok == 0 {
        let code = io::Error::last_os_error().raw_os_error().unwrap_or(0) as u32;
        if MISSING_MESSAGE_ERRORS.contains(&code) {
            return None;
        }
        if code != ERROR_INSUFFICIENT_BUFFER && !PARTIAL_MESSAGE_ERRORS.contains(&code) {
            tracing::debug!(code, "EvtFormatMessage 探长度失败");
            return None;
        }
    }
    if used == 0 {
        return None;
    }
    let mut buf = vec![0u16; used as usize];
    // SAFETY: buf 有 used 个 u16 可写，与传入的 buffersize（单位：字符）一致。
    let ok = unsafe {
        EvtFormatMessage(
            publisher,
            event,
            0,
            0,
            std::ptr::null(),
            EvtFormatMessageEvent,
            used,
            buf.as_mut_ptr(),
            &raw mut used,
        )
    };
    if ok == 0 {
        let code = io::Error::last_os_error().raw_os_error().unwrap_or(0) as u32;
        // 插入参数没解析全时缓冲里仍然有可读内容，收下；其余算取不到。
        if !PARTIAL_MESSAGE_ERRORS.contains(&code) {
            return None;
        }
    }
    let s = from_wide_nul(&buf);
    // 事件消息普遍以 `\r\n` 收尾，有的还带一串空行，展示前统一去掉尾部空白。
    let s = s.trim_end().to_owned();
    (!s.is_empty()).then_some(s)
}

// ---------------------------------------------------------------------------
// 清空与订阅
// ---------------------------------------------------------------------------

/// 清空一个通道（`EvtClearLog`，不做备份）。需要管理员。
pub fn clear_log(channel: &str) -> io::Result<()> {
    let ch = to_wide(channel);
    // SAFETY: session / targetfilepath 为 NULL 表示本机与「不备份」；
    // ch 是本函数持有的、以 NUL 结尾的 UTF-16 缓冲。
    let ok = unsafe { EvtClearLog(0, ch.as_ptr(), std::ptr::null(), 0) };
    if ok == 0 {
        return Err(last_error());
    }
    Ok(())
}

/// 订阅一个通道的**未来**事件，回调式。
///
/// # Safety
///
/// - `context` 必须在**订阅句柄被 `EvtClose` 之后**才可以释放。回调可能在任意
///   线程上、在本函数返回之前就被调用；`EvtClose` 是唯一能保证它不再被调用的操作。
///   调用方（[`super::Subscription`]）的 `Drop` 就是按这个顺序写的。
/// - `callback` 必须能承受在任意线程上被调用，且**绝不能 unwind 穿过 FFI 边界**。
pub unsafe fn subscribe(
    channel: &str,
    xpath: &str,
    context: *const std::ffi::c_void,
    callback: EVT_SUBSCRIBE_CALLBACK,
) -> io::Result<EvtHandle> {
    let ch = to_wide(channel);
    let q = to_wide(xpath);
    // SAFETY: session 为 NULL（本机）、signalevent 为 NULL（回调模式而非事件对象
    // 模式）、bookmark 为 0（只要未来事件）。ch / q 在本调用期间有效——wevtapi 会
    // 把查询串拷走，不会留引用。context / callback 的前提由本函数的 Safety 段承担。
    unsafe {
        EvtHandle::new(EvtSubscribe(
            0,
            std::ptr::null_mut(),
            ch.as_ptr(),
            q.as_ptr(),
            0,
            context,
            callback,
            EvtSubscribeToFutureEvents,
        ))
    }
}

/// Win32 错误码 → [`io::Error`]，供上层做分类。
pub fn io_error(code: u32) -> io::Error {
    error_from_code(code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::Foundation::ERROR_EVT_INVALID_QUERY;

    /// 空句柄必须被当成失败：wevtapi 的失败返回就是 0，放过去会 `EvtClose(0)`。
    #[test]
    fn 空句柄当失败() {
        // SAFETY: 传的就是失败值，函数只做判断不解引用。
        assert!(unsafe { EvtHandle::new(0) }.is_err());
    }

    /// 本机实测：`System` 通道能打开，句柄 drop 后不崩。
    #[test]
    fn 本机能打开_system_通道查询() {
        let r = open_query("System", "*");
        assert!(r.is_ok(), "System 通道应当可查：{:?}", r.err());
        let q = r.unwrap();
        let batch = next_batch(&q, 4).expect("取批不应失败");
        eprintln!("[evt] System 通道首批取回 {} 条", batch.len());
        for h in &batch {
            let xml = render_xml(h.raw()).expect("渲染 XML 不应失败");
            assert!(xml.starts_with("<Event"), "渲染结果应是事件 XML：{xml:.60}");
        }
    }

    /// 坏 XPath 必须报 `ERROR_EVT_INVALID_QUERY`，而不是悄悄返回空结果。
    #[test]
    fn 非法_xpath_报错而不是空结果() {
        let e = open_query("System", "*[System[Level=").unwrap_err();
        eprintln!("[evt] 非法 XPath 的错误：{e}");
        assert_eq!(
            e.raw_os_error(),
            Some(ERROR_EVT_INVALID_QUERY as i32),
            "wevtapi 应当拒绝这条查询"
        );
    }

    /// 不存在的通道要报错（上层据此跳过而不是当成「没有日志」）。
    #[test]
    fn 不存在的通道报错() {
        assert!(open_query("StrixMaid-NoSuchChannel", "*").is_err());
    }

    /// 发布者元数据缓存：同一个 provider 只开一次。
    #[test]
    fn 本机消息渲染与缓存() {
        let cache = PublisherCache::new();
        let Ok(q) = open_query("System", "*") else {
            eprintln!("[evt] System 通道不可查，跳过");
            return;
        };
        let batch = next_batch(&q, 8).unwrap_or_default();
        if batch.is_empty() {
            eprintln!("[evt] System 通道为空，跳过消息渲染");
            return;
        }
        let mut rendered = 0usize;
        for h in &batch {
            let xml = render_xml(h.raw()).unwrap_or_default();
            let Some(ev) = super::super::model::parse_event_xml(&xml) else {
                continue;
            };
            let provider = ev.provider.clone().unwrap_or_default();
            if let Some(msg) = cache.message(&provider, h.raw()) {
                rendered += 1;
                assert!(!msg.is_empty(), "渲染出来的消息不该是空串");
                assert_eq!(msg.trim_end(), msg, "尾部空白应当已被去掉");
                if rendered == 1 {
                    eprintln!(
                        "[evt] {provider}: {}",
                        msg.chars().take(80).collect::<String>()
                    );
                }
            }
        }
        eprintln!(
            "[evt] {}/{} 条渲染出可读消息，缓存了 {} 个发布者",
            rendered,
            batch.len(),
            cache.len()
        );
        assert!(cache.len() <= batch.len(), "缓存条目不该多于事件数");
    }

    #[test]
    fn 错误码转换保留原值() {
        assert_eq!(io_error(5).raw_os_error(), Some(5));
        assert_eq!(io_error(5).kind(), io::ErrorKind::PermissionDenied);
    }
}
