//! 自写的 PAM 应用侧 FFI（design.md §10「PAM 接入方式」）。
//!
//! 只声明应用需要的十来个函数与常量，全部来自 Linux-PAM 的 `_pam_types.h` /
//! `pam_appl.h`，二十年未变。不用 `pam-client` / `pam` crate（均已停更且要求
//! `libpam0g-dev`）；链接方式见 `build.rs`。
//!
//! # conversation 回调就是 challenge-response 的落地点
//!
//! PAM 在 `pam_authenticate` / `pam_acct_mgmt` / `pam_chauthtok` 内部**同步**回调
//! [`conversation`]，把一批 `pam_message` 交给应用、要一批 `pam_response` 回去。
//! 本模块把这批消息转成 IPC 帧发给主进程（→ 浏览器），阻塞等主进程回传答案，再
//! 填成 `pam_response` 交还 PAM。一次 PAM 回调 = HTTP 协议里的一「轮」。
//!
//! # 凭据处理（§5.3）
//!
//! 主进程回传的答案落在 `Zeroizing<String>` 里；复制给 PAM 的那份用 `malloc`
//! 分配（PAM 约定用 `free` 回收，libpam 的 `_pam_drop_reply` 会先覆写再释放）。
//! 我们自己持有的副本在离开回调时 drop 并擦除。本模块**不打印任何消息内容**。

use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::ptr;

use strixmaid_types::auth::{Prompt, PromptStyle};
use strixmaid_types::ipc::{FromHelper, IpcError, IpcPromptResponse, ToHelper};

use crate::ipc::Ipc;

// ===========================================================================
// FFI 声明
// ===========================================================================

/// 不透明的 `pam_handle_t`。
#[repr(C)]
pub struct PamHandleRaw {
    _private: [u8; 0],
}

/// `struct pam_message`。
#[repr(C)]
struct PamMessage {
    msg_style: c_int,
    msg: *const c_char,
}

/// `struct pam_response`。
#[repr(C)]
struct PamResponse {
    resp: *mut c_char,
    resp_retcode: c_int,
}

/// conversation 函数指针类型（Linux-PAM 的 `msg` 是「指针数组的指针」）。
type ConvFn = unsafe extern "C" fn(
    num_msg: c_int,
    msg: *const *const PamMessage,
    resp: *mut *mut PamResponse,
    appdata_ptr: *mut c_void,
) -> c_int;

/// `struct pam_conv`。
#[repr(C)]
struct PamConv {
    conv: Option<ConvFn>,
    appdata_ptr: *mut c_void,
}

unsafe extern "C" {
    fn pam_start(
        service_name: *const c_char,
        user: *const c_char,
        pam_conversation: *const PamConv,
        pamh: *mut *mut PamHandleRaw,
    ) -> c_int;
    fn pam_end(pamh: *mut PamHandleRaw, pam_status: c_int) -> c_int;
    fn pam_authenticate(pamh: *mut PamHandleRaw, flags: c_int) -> c_int;
    fn pam_acct_mgmt(pamh: *mut PamHandleRaw, flags: c_int) -> c_int;
    fn pam_chauthtok(pamh: *mut PamHandleRaw, flags: c_int) -> c_int;
    fn pam_setcred(pamh: *mut PamHandleRaw, flags: c_int) -> c_int;
    fn pam_open_session(pamh: *mut PamHandleRaw, flags: c_int) -> c_int;
    fn pam_close_session(pamh: *mut PamHandleRaw, flags: c_int) -> c_int;
    fn pam_get_item(pamh: *const PamHandleRaw, item_type: c_int, item: *mut *const c_void)
    -> c_int;
    fn pam_set_item(pamh: *mut PamHandleRaw, item_type: c_int, item: *const c_void) -> c_int;
    fn pam_strerror(pamh: *const PamHandleRaw, errnum: c_int) -> *const c_char;
    fn pam_getenvlist(pamh: *mut PamHandleRaw) -> *mut *mut c_char;
}

// ---- 返回码（`_pam_types.h`）----
pub const PAM_SUCCESS: c_int = 0;
pub const PAM_BUF_ERR: c_int = 5;
pub const PAM_NEW_AUTHTOK_REQD: c_int = 12;
pub const PAM_CONV_ERR: c_int = 19;

// ---- item 类型 ----
const PAM_USER: c_int = 2;
const PAM_RHOST: c_int = 4;

// ---- 消息风格 ----
const PAM_PROMPT_ECHO_OFF: c_int = 1;
const PAM_PROMPT_ECHO_ON: c_int = 2;
const PAM_ERROR_MSG: c_int = 3;
// PAM_TEXT_INFO(4) 与任何未知风格都映射为 Info，见 `style_of`。

// ---- 标志 ----
const PAM_ESTABLISH_CRED: c_int = 0x2;
const PAM_DELETE_CRED: c_int = 0x4;
const PAM_CHANGE_EXPIRED_AUTHTOK: c_int = 0x20;

// ===========================================================================
// 错误
// ===========================================================================

/// PAM 调用失败：带返回码与 `pam_strerror` 文本，不含任何凭据。
#[derive(Debug)]
pub struct PamError {
    /// 失败的 PAM 函数名。
    pub func: &'static str,
    /// PAM 返回码。
    pub code: c_int,
    /// `pam_strerror` 的文本。
    pub message: String,
}

impl std::fmt::Display for PamError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {} (code {})", self.func, self.message, self.code)
    }
}

impl std::error::Error for PamError {}

// ===========================================================================
// conversation 状态
// ===========================================================================

/// 回调期间 PAM 通过 `appdata_ptr` 拿到的状态。
///
/// `ipc` 是「作用域指针」：只在 `pam_authenticate` 等调用期间由 [`Pam::with_ipc`]
/// 设置，调用结束即清空，保证不会悬垂。
struct ConvState {
    ipc: *mut Ipc,
    /// PAM 只发了信息、没要输入的那些消息，攒着放进下一轮 `Prompts` 一起送去浏览器。
    stashed: Vec<Prompt>,
    /// 回调期间遇到的 IPC 错误（主进程断开等），留给调用方决定如何退出。
    ipc_error: Option<IpcError>,
}

/// PAM 消息风格 → 协议里的 [`PromptStyle`]。未知风格按信息处理。
fn style_of(msg_style: c_int) -> PromptStyle {
    match msg_style {
        PAM_PROMPT_ECHO_OFF => PromptStyle::Prompt,
        PAM_PROMPT_ECHO_ON => PromptStyle::PromptEcho,
        PAM_ERROR_MSG => PromptStyle::Error,
        _ => PromptStyle::Info,
    }
}

/// 把 UTF-8 文本复制成 PAM 可 `free()` 的 C 字符串。含 NUL 则返回空指针。
///
/// 不经过 `CString`：`CString` drop 时不会擦除缓冲，会多留一份明文。
unsafe fn malloc_cstr(s: &str) -> *mut c_char {
    let bytes = s.as_bytes();
    if bytes.contains(&0) {
        return ptr::null_mut();
    }
    // SAFETY: malloc 返回的缓冲至少 len+1 字节；复制后补 NUL。
    unsafe {
        let p = libc::malloc(bytes.len() + 1) as *mut c_char;
        if p.is_null() {
            return p;
        }
        ptr::copy_nonoverlapping(bytes.as_ptr(), p as *mut u8, bytes.len());
        *p.add(bytes.len()) = 0;
        p
    }
}

/// conversation 回调本体。
///
/// # Safety
///
/// 由 libpam 在 `pam_*` 调用内部同步调用；`appdata_ptr` 是 [`Pam`] 里的 `Box<ConvState>`。
unsafe extern "C" fn conversation(
    num_msg: c_int,
    msg: *const *const PamMessage,
    resp: *mut *mut PamResponse,
    appdata_ptr: *mut c_void,
) -> c_int {
    // 任何 panic 都不能跨过 FFI 边界，统一折成 PAM_CONV_ERR。
    let result = std::panic::catch_unwind(|| {
        // SAFETY: 参数全部由 libpam 按约定提供；appdata_ptr 在 Pam 存活期间有效。
        unsafe { conversation_impl(num_msg, msg, resp, appdata_ptr) }
    });
    match result {
        Ok(code) => code,
        Err(_) => {
            crate::log::event("conversation 回调 panic，按 PAM_CONV_ERR 处理");
            PAM_CONV_ERR
        }
    }
}

unsafe fn conversation_impl(
    num_msg: c_int,
    msg: *const *const PamMessage,
    resp: *mut *mut PamResponse,
    appdata_ptr: *mut c_void,
) -> c_int {
    if num_msg <= 0 || msg.is_null() || resp.is_null() || appdata_ptr.is_null() {
        return PAM_CONV_ERR;
    }
    let num = num_msg as usize;
    // SAFETY: appdata_ptr 来自 Box::into_raw(Box<ConvState>)，Pam 持有它直到 pam_end 之后。
    let state = unsafe { &mut *(appdata_ptr as *mut ConvState) };
    if state.ipc.is_null() {
        crate::log::event("conversation 在没有 IPC 通道的情况下被调用");
        return PAM_CONV_ERR;
    }
    // SAFETY: with_ipc 保证在 PAM 调用期间该指针指向存活的 Ipc。
    let ipc = unsafe { &mut *state.ipc };

    // ---- 1. 收集本次回调的消息 ----
    let mut prompts: Vec<Prompt> = std::mem::take(&mut state.stashed);
    // 攒下来的信息占用了前 base 个 id；PAM 消息 i 的 id = base + i。
    let base = prompts.len() as u32;
    let mut needs_input = false;
    for i in 0..num {
        // SAFETY: Linux-PAM 传的是 num_msg 个指针组成的数组。
        let m = unsafe { &**msg.add(i) };
        let text = if m.msg.is_null() {
            String::new()
        } else {
            // SAFETY: PAM 保证 msg 是 NUL 结尾字符串。
            unsafe { CStr::from_ptr(m.msg) }
                .to_string_lossy()
                .into_owned()
        };
        let style = style_of(m.msg_style);
        needs_input |= style.needs_input();
        prompts.push(Prompt {
            id: base + i as u32,
            style,
            text,
        });
    }

    // ---- 2. 分配响应数组（PAM 用 free() 回收，必须 calloc）----
    // SAFETY: calloc 归零，resp 字段默认为 NULL。
    let responses =
        unsafe { libc::calloc(num, std::mem::size_of::<PamResponse>()) as *mut PamResponse };
    if responses.is_null() {
        return PAM_BUF_ERR;
    }

    // 纯信息轮：不必打扰主进程，攒起来下轮一起送。
    if !needs_input {
        crate::log::event(&format!("PAM 发来 {num} 条纯信息消息，暂存"));
        state.stashed = prompts;
        // SAFETY: resp 由 PAM 提供，是合法的输出指针。
        unsafe { *resp = responses };
        return PAM_SUCCESS;
    }

    // ---- 3. 往返主进程 ----
    let input_count = prompts.iter().filter(|p| p.style.needs_input()).count();
    crate::log::event(&format!(
        "PAM 需要 {input_count} 项输入（本轮共 {} 条消息），转发主进程",
        prompts.len()
    ));
    let answers: Vec<IpcPromptResponse> = match ipc.send_and_wait(FromHelper::Prompts { prompts }) {
        Ok(Some(ToHelper::AuthRespond { responses })) => responses,
        Ok(Some(_)) => {
            crate::log::event("等待 AuthRespond 时收到其它消息，认证中止");
            state.ipc_error = Some(IpcError::Protocol("等待 AuthRespond 时收到其它消息".into()));
            // SAFETY: 尚未填入任何 resp 字符串，直接释放数组。
            unsafe { libc::free(responses as *mut c_void) };
            return PAM_CONV_ERR;
        }
        Ok(None) => {
            crate::log::event("等待 AuthRespond 时主进程关闭了通道，认证中止");
            state.ipc_error = Some(IpcError::Protocol("主进程已断开".into()));
            unsafe { libc::free(responses as *mut c_void) };
            return PAM_CONV_ERR;
        }
        Err(e) => {
            crate::log::event(&format!("等待 AuthRespond 时 IPC 出错: {e}"));
            state.ipc_error = Some(e);
            unsafe { libc::free(responses as *mut c_void) };
            return PAM_CONV_ERR;
        }
    };

    // ---- 4. 填响应：按 id 对上 PAM 消息下标 ----
    let mut ok = true;
    for i in 0..num {
        // SAFETY: 同上。
        let m = unsafe { &**msg.add(i) };
        if !style_of(m.msg_style).needs_input() {
            continue;
        }
        let id = base + i as u32;
        match answers.iter().find(|a| a.id == id) {
            Some(a) => {
                // SAFETY: 明文只从 Zeroizing<String> 复制进 malloc 缓冲，交由 PAM 覆写并 free。
                let p = unsafe { malloc_cstr(a.value.as_str()) };
                if p.is_null() {
                    ok = false;
                    break;
                }
                // SAFETY: i < num，responses 有 num 个元素。
                unsafe { (*responses.add(i)).resp = p };
            }
            None => {
                crate::log::event(&format!("主进程未对提示 #{id} 作答，认证中止"));
                ok = false;
                break;
            }
        }
    }
    // `answers` 在此 drop：每个 Zeroizing<String> 自动擦除。
    drop(answers);

    if !ok {
        // 已分配的字符串先覆写再释放，与 libpam 的 _pam_drop_reply 行为一致。
        for i in 0..num {
            // SAFETY: 同上；未填的 resp 为 NULL。
            unsafe {
                let r = (*responses.add(i)).resp;
                if !r.is_null() {
                    let len = libc::strlen(r);
                    ptr::write_bytes(r, 0, len);
                    libc::free(r as *mut c_void);
                }
            }
        }
        unsafe { libc::free(responses as *mut c_void) };
        return PAM_CONV_ERR;
    }

    // SAFETY: 交给 PAM 接管。
    unsafe { *resp = responses };
    PAM_SUCCESS
}

// ===========================================================================
// 安全封装
// ===========================================================================

/// 一个 PAM 句柄，生命周期 = 一次认证对话 + （可选的）一个会话。
///
/// `pam_open_session` / `pam_close_session` 必须在同一进程、同一句柄上成对调用，
/// 这就是 helper 必须活到登出的原因（§10）。
pub struct Pam {
    handle: *mut PamHandleRaw,
    /// `pam_start` 会拷贝 `pam_conv` 结构，但 `appdata_ptr` 指向的状态必须活到 `pam_end`。
    state: *mut ConvState,
    /// 传给 `pam_end` 的最后状态码（PAM 模块据此决定是否清理）。
    last_status: c_int,
    /// `pam_setcred(ESTABLISH)` 是否成功，决定关闭时是否 `DELETE_CRED`。
    cred_established: bool,
    /// `pam_open_session` 是否成功，决定关闭时是否 `pam_close_session`。
    session_opened: bool,
}

impl Pam {
    /// `pam_start`。不发生任何 IPC；对话在 [`Pam::authenticate`] 里才开始。
    pub fn start(service: &str, username: &str, rhost: Option<&str>) -> Result<Pam, PamError> {
        let c_service = CString::new(service).map_err(|_| PamError {
            func: "pam_start",
            code: -1,
            message: "服务名含 NUL".into(),
        })?;
        let c_user = CString::new(username).map_err(|_| PamError {
            func: "pam_start",
            code: -1,
            message: "用户名含 NUL".into(),
        })?;

        let state = Box::into_raw(Box::new(ConvState {
            ipc: ptr::null_mut(),
            stashed: Vec::new(),
            ipc_error: None,
        }));
        let conv = PamConv {
            conv: Some(conversation),
            appdata_ptr: state as *mut c_void,
        };

        let mut handle: *mut PamHandleRaw = ptr::null_mut();
        // SAFETY: 全部指针指向本函数栈上 / 堆上的有效数据；libpam 会拷贝 conv 结构。
        let code = unsafe { pam_start(c_service.as_ptr(), c_user.as_ptr(), &conv, &mut handle) };
        if code != PAM_SUCCESS || handle.is_null() {
            // SAFETY: state 尚未交给任何人。
            drop(unsafe { Box::from_raw(state) });
            return Err(PamError {
                func: "pam_start",
                code,
                // 没有句柄时 pam_strerror 也能工作（传 NULL）。
                message: strerror(ptr::null(), code),
            });
        }

        let pam = Pam {
            handle,
            state,
            last_status: PAM_SUCCESS,
            cred_established: false,
            session_opened: false,
        };

        if let Some(rhost) = rhost
            && let Ok(c_rhost) = CString::new(rhost)
        {
            // 失败不致命：只是少了一条给 PAM 模块记日志的信息。
            // SAFETY: 句柄有效；PAM 会拷贝字符串。
            let rc = unsafe { pam_set_item(handle, PAM_RHOST, c_rhost.as_ptr() as *const c_void) };
            if rc != PAM_SUCCESS {
                crate::log::event(&format!(
                    "pam_set_item(PAM_RHOST) 失败: {}",
                    pam.strerror(rc)
                ));
            }
        }
        Ok(pam)
    }

    fn strerror(&self, code: c_int) -> String {
        strerror(self.handle, code)
    }

    fn check(&mut self, func: &'static str, code: c_int) -> Result<(), PamError> {
        self.last_status = code;
        if code == PAM_SUCCESS {
            Ok(())
        } else {
            Err(PamError {
                func,
                code,
                message: self.strerror(code),
            })
        }
    }

    /// 在 `ipc` 可用的作用域内执行 `f`（期间 conversation 回调可以往返主进程）。
    fn with_ipc<R>(&mut self, ipc: &mut Ipc, f: impl FnOnce(&mut Pam) -> R) -> R {
        // SAFETY: state 在 pam_end 之前一直有效。
        unsafe { (*self.state).ipc = ipc as *mut Ipc };
        let r = f(self);
        unsafe { (*self.state).ipc = ptr::null_mut() };
        r
    }

    /// 取出回调期间记录的 IPC 错误（若有）。
    pub fn take_ipc_error(&mut self) -> Option<IpcError> {
        // SAFETY: 同上。
        unsafe { (*self.state).ipc_error.take() }
    }

    /// 回调攒下的、尚未送出的纯信息消息数。
    pub fn stashed_info_count(&self) -> usize {
        // SAFETY: 同上。
        unsafe { (*self.state).stashed.len() }
    }

    /// `pam_authenticate` + `pam_acct_mgmt`（+ 密码过期时的 `pam_chauthtok`）。
    ///
    /// 期间的所有 PAM 提示都经 `ipc` 往返主进程。
    pub fn authenticate(&mut self, ipc: &mut Ipc) -> Result<(), PamError> {
        self.with_ipc(ipc, |pam| {
            // SAFETY: 句柄有效。
            let code = unsafe { pam_authenticate(pam.handle, 0) };
            pam.check("pam_authenticate", code)?;

            let code = unsafe { pam_acct_mgmt(pam.handle, 0) };
            if code == PAM_NEW_AUTHTOK_REQD {
                // 密码过期：继续用同一套对话协议走改密流程。
                crate::log::event("账户密码已过期，进入 pam_chauthtok");
                let code = unsafe { pam_chauthtok(pam.handle, PAM_CHANGE_EXPIRED_AUTHTOK) };
                pam.check("pam_chauthtok", code)?;
                return Ok(());
            }
            pam.check("pam_acct_mgmt", code)
        })
    }

    /// 认证后 PAM 眼中的用户名（模块可能改写 `PAM_USER`，如大小写规范化）。
    pub fn user(&self) -> Option<String> {
        let mut item: *const c_void = ptr::null();
        // SAFETY: 句柄有效；item 是输出参数。
        let code = unsafe { pam_get_item(self.handle, PAM_USER, &mut item) };
        if code != PAM_SUCCESS || item.is_null() {
            return None;
        }
        // SAFETY: PAM_USER 是 NUL 结尾字符串，归 PAM 所有。
        Some(
            unsafe { CStr::from_ptr(item as *const c_char) }
                .to_string_lossy()
                .into_owned(),
        )
    }

    /// `pam_setcred(PAM_ESTABLISH_CRED)` + `pam_open_session`。
    ///
    /// 两者任一失败都**不致命**（非 root 下 pam_systemd / pam_loginuid 必然失败），
    /// 返回 `Err` 供上层降级并记录原因。成功与否会记在句柄里，决定关闭时的清理步骤。
    pub fn open_session(&mut self, ipc: &mut Ipc) -> Result<(), PamError> {
        self.with_ipc(ipc, |pam| {
            // SAFETY: 句柄有效。
            let code = unsafe { pam_setcred(pam.handle, PAM_ESTABLISH_CRED) };
            if code == PAM_SUCCESS {
                pam.cred_established = true;
            } else {
                // setcred 失败不阻止开会话（login(1) 也是先 setcred 再 open_session，但二者独立）。
                crate::log::event(&format!(
                    "pam_setcred(ESTABLISH) 失败: {}",
                    pam.strerror(code)
                ));
            }
            let code = unsafe { pam_open_session(pam.handle, 0) };
            if code == PAM_SUCCESS {
                pam.session_opened = true;
            }
            pam.check("pam_open_session", code)
        })
    }

    /// 会话是否已成功打开。
    pub fn session_opened(&self) -> bool {
        self.session_opened
    }

    /// `pam_getenvlist`：PAM 模块（pam_env / pam_systemd）为会话设置的环境变量，
    /// 形如 `KEY=VALUE`。要传给 worker，否则 `XDG_RUNTIME_DIR` 等不会到位。
    pub fn envlist(&self) -> Vec<(String, String)> {
        // SAFETY: 句柄有效。
        let list = unsafe { pam_getenvlist(self.handle) };
        if list.is_null() {
            return Vec::new();
        }
        let mut out = Vec::new();
        let mut i = 0;
        // SAFETY: 返回值是 NULL 结尾的字符串指针数组，全部由 malloc 分配、归调用方释放。
        unsafe {
            loop {
                let p = *list.add(i);
                if p.is_null() {
                    break;
                }
                let s = CStr::from_ptr(p).to_string_lossy().into_owned();
                if let Some((k, v)) = s.split_once('=') {
                    out.push((k.to_string(), v.to_string()));
                }
                libc::free(p as *mut c_void);
                i += 1;
            }
            libc::free(list as *mut c_void);
        }
        out
    }

    /// 关闭会话并结束句柄：`pam_close_session` → `pam_setcred(DELETE)` → `pam_end`。
    /// 每一步只在对应的打开步骤成功过时才执行。消费 `self`。
    pub fn close(mut self) {
        // SAFETY: 句柄有效；每个函数只调用一次。
        unsafe {
            if self.session_opened {
                let code = pam_close_session(self.handle, 0);
                if code != PAM_SUCCESS {
                    crate::log::event(&format!("pam_close_session 失败: {}", self.strerror(code)));
                }
                self.session_opened = false;
            }
            if self.cred_established {
                let code = pam_setcred(self.handle, PAM_DELETE_CRED);
                if code != PAM_SUCCESS {
                    crate::log::event(&format!(
                        "pam_setcred(DELETE) 失败: {}",
                        self.strerror(code)
                    ));
                }
                self.cred_established = false;
            }
        }
        // Drop 里做 pam_end。
    }
}

impl Drop for Pam {
    fn drop(&mut self) {
        // SAFETY: 句柄与 state 都只释放一次；pam_end 之后 PAM 不再回调。
        unsafe {
            pam_end(self.handle, self.last_status);
            drop(Box::from_raw(self.state));
        }
        self.handle = ptr::null_mut();
        self.state = ptr::null_mut();
    }
}

/// `pam_strerror` 的安全包装。`handle` 可为空。
fn strerror(handle: *const PamHandleRaw, code: c_int) -> String {
    // SAFETY: pam_strerror 对任何 code 都返回静态字符串。
    let p = unsafe { pam_strerror(handle, code) };
    if p.is_null() {
        return format!("PAM 错误 {code}");
    }
    // SAFETY: 静态 NUL 结尾字符串。
    unsafe { CStr::from_ptr(p) }.to_string_lossy().into_owned()
}
