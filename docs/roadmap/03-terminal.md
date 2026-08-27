# 03 终端

## 1. 目标

浏览器内的 PTY 终端，按 `design.md` Q20 确认的功能表：多会话、刷新页面后自动重连（后端保留 PTY）、回看缓冲、尺寸同步、shell 与身份选择、空闲超时。终端在 worker 内 fork，运行身份即会话用户。

## 2. 现状

| 项 | 状态 |
|---|---|
| DTO | `crates/strixmaid-types/src/terminal.rs`：`CreateTerminalReq { shell, user }`、`CreateTerminalResp { id }`、`TerminalInfo { id, shell, user, uid, cols, rows, created_ts, last_active_ts, attached }`、`ResizeReq { cols, rows }` |
| 端点 | `design.md` §9.1 定义了 `POST /terminals`、`GET /terminals`、`DELETE /terminals/{id}`、`POST /terminals/{id}/resize`；§9.2 定义了 `WS /ws/terminal/{id}` |
| PTY 库 | `portable-pty 0.9`，未添加 |
| worker | 见 `01-worker-execution.md`，本方案依赖其 RPC 与 fd 传递 |
| 前端 | `/debug` 页无终端面板；xterm.js 未 vendor |

### 已落地的地基（commit `5b13c06`）

§4.1 要的帧格式改造与 fd 传递已经做完，后续工作可以直接用：

| 项 | 位置 |
|---|---|
| 帧头 `u32 len + u8 fd_count` | `types/src/ipc.rs`：`FRAME_HEADER_LEN=5`、`MAX_FRAME_FDS=4`、`encode_with_fds`、`parse_header -> (len, fd_count)` |
| 收 fd 的读端 | `core/src/session/framing.rs`：`FdFrameReader`（整条连接每帧都走 `recvmsg`） |
| 发 fd 的写端 | 同上：`write_msg_with_fds` |
| worker 侧交出 fd | `core/src/worker/mod.rs`：`FdHandler`、`Dispatcher::register_fd`、`dispatch_with_fds`；`serve` 已会把 fd 随 `Result` 帧发出 |
| 主进程侧取 fd | `core/src/session/worker_handle.rs`：`WorkerHandle::call_with_fds` |
| RPC 契约 | `types/src/rpc.rs`：`TERM_OPEN`/`TERM_RESIZE`/`TERM_CLOSE` 与 `TermOpenParams`/`TermOpenResult`/`TermResizeParams`/`TermCloseParams` |
| 配置 | `core/src/config.rs`：`TerminalConfig { idle_timeout_secs=1800, max_per_session=8 }` |
| xterm 资产 | `server/src/debug/vendor/`：`xterm.js.gz`(5.5.0)、`xterm.css.gz`、`addon-fit.js.gz`(0.10.0) |

一处与 §4.5 的偏差：`TermOpenParams` **不含身份判断**，只把 `user` 作为 admin worker 的
目标用户名透传。以谁的身份跑由「发给哪个 worker」决定，而不是由参数决定——把身份
放进参数就等于让 worker 自己判断该不该切身份，那正是 `design.md` §5.1 要避免的自建鉴权。
`TermOpenResult` 相应地回带 `shell`/`user`/`uid` 实际值，供主进程回填 `TerminalInfo`。

## 3. 设计约束

- `design.md` §2.2：PTY 在 worker 内。
- Q20 决定：终端是高危能力。在 PAM 模型下，**以会话用户自己的身份开 shell 等价于该用户 SSH 登录**，不需要提权；`user` 参数指定其他用户时才需要提权，由 admin worker（root）完成 setuid。
- Q13 决定：终端使用独立 WS 连接，不进多路复用控制面。
- 鉴权：`/ws/terminal/{id}` 同 `/ws`，token 走子协议。
- 文件传输不在本方案内。

## 4. 方案

### 4.1 数据通路

浏览器与 PTY 之间是字节流，不应经过 JSON 帧。采用 **fd 传递**：

```
浏览器 ⇄ WS /ws/terminal/{id} ⇄ 主进程（泵 + 回看缓冲）⇄ socketpair ⇄ worker（泵）⇄ PTY master ⇄ shell
```

1. 主进程经 `WorkerHandle::call("term.open", {shell, cols, rows})` 请求。
2. worker 内：`portable-pty` 打开 PTY、以 `cols × rows` 启动 shell；创建 `socketpair(AF_UNIX, SOCK_STREAM)`；起两个任务在 PTY master 与 socketpair 一端之间双向泵字节；把另一端随 `FromWorker::Result` 帧经 `SCM_RIGHTS` 发回主进程。
3. 主进程收到 fd 后包成 `tokio::net::UnixStream`，由 `TerminalRegistry` 持有。
4. WS 附着时：主进程把该 stream 与 WS 之间双向泵字节。

fd 传递需要扩展 `crates/strixmaid-core/src/session/framing.rs` 的读端：用 `nix::sys::socket::recvmsg` 读帧并带 `cmsg` 缓冲，帧头增加一个 `fd_count: u8` 字段（`types/src/ipc.rs` 的帧格式从 `u32 长度 + JSON` 改为 `u32 长度 + u8 fd_count + JSON`，helper 与 worker 侧同步修改）。helper 已有向主进程传 worker fd 的 `SCM_RIGHTS` 代码（`crates/strixmaid-helper/src/spawn.rs`），逻辑可移植到 core。

替代方案：worker 在 `/run/user/<uid>/strixmaid/term-<id>.sock` 建监听 socket（0600），主进程连接。不需要改帧格式，但多一次握手且依赖 `XDG_RUNTIME_DIR` 存在（`pam_open_session` 失败时没有）。首选 fd 传递。

### 4.2 身份

| `CreateTerminalReq.user` | 行为 |
|---|---|
| `None` 或等于会话用户 | user worker 内以自身身份启动 shell |
| 其他用户 | 要求 `session.elevated`，否则 403 `elevation_required`；admin worker 内 `fork` 后 `initgroups + setgid + setuid` 到目标用户再 exec shell（逻辑同 helper 的 `spawn.rs`，可抽到 core 的 `worker/spawn_as.rs` 供复用） |

shell 选择：`CreateTerminalReq.shell` 为 `None` 时用目标用户在 `/etc/passwd` 中的登录 shell；非空时必须在 `/etc/shells` 中。环境变量：`TERM=xterm-256color`、`HOME`、`USER`、`LOGNAME`、`SHELL`、`PATH`、以及 worker 自身继承的 PAM 环境（含 `XDG_RUNTIME_DIR`、`DBUS_SESSION_BUS_ADDRESS`，若 `pam_open_session` 成功）。

### 4.3 主进程的 `TerminalRegistry`

`crates/strixmaid-core/src/terminal/mod.rs`（core 而非 server，Agent 后续复用）：

```rust
pub struct Terminal {
    id: String,            // 16 字节随机 hex
    session_hash: String,  // 归属会话
    info: TerminalInfo,
    stream: UnixStream,    // 来自 worker 的 socketpair 一端
    scrollback: RingBuf,   // 256 KiB
    attached: Option<AttachHandle>,
}
```

- **附着**：一个终端同一时刻只允许一个 WS。新附着到来时先关闭旧的（发 close 帧，原因 `replaced`），再回放 `scrollback` 全部内容，然后开始实时泵。
- **断开**：WS 断开只是解除附着，PTY 与 shell 继续运行，输出持续写入 `scrollback`。
- **关闭**：`DELETE /terminals/{id}`、shell 退出（stream EOF）、空闲超时、会话登出四种情况。关闭时向 worker 发 `term.close {id}`，worker 侧 `SIGHUP` 进程组、回收 PTY。
- **空闲**：无附着且无输出持续 `terminal.idle_timeout_secs`（新增配置，默认 1800）即关闭。
- **上限**：`terminal.max_per_session`（默认 8）。

`GET /terminals` 只返回本会话的终端。会话登出或超时时 `SessionManager` 关闭其全部终端（`session/mod.rs` 的 `logout` 与 sweeper 调用 `TerminalRegistry::close_all_for(session_hash)`）。

### 4.4 WS 协议 `/ws/terminal/{id}`

不使用 `WsEnvelope`。二进制帧为终端字节；文本帧为控制消息，只有两种：

```jsonc
{ "t": "resize", "cols": 220, "rows": 50 }   // 客户端 → 服务端
{ "t": "exit",   "code": 0 }                 // 服务端 → 客户端，shell 退出后发送并关闭
```

`resize` 经 `WorkerHandle::call("term.resize", {id, cols, rows})`，worker 内 `ioctl(TIOCSWINSZ)`（`portable-pty` 的 `MasterPty::resize`）。`POST /terminals/{id}/resize` 与 WS 内 `resize` 等价，前者供无 WS 时使用。

心跳沿用 WS 协议层 ping/pong（axum 自动应答），无应用层保活。

### 4.5 RPC 方法

| 方法 | params | result | 执行者 |
|---|---|---|---|
| `term.open` | `{shell, user, cols, rows}` | `{pid}` + 1 个 fd | user 或 admin worker（4.2） |
| `term.resize` | `{pid, cols, rows}` | — | 同上 |
| `term.close` | `{pid}` | — | 同上 |

worker 内以 shell 的 pid 作为终端句柄；主进程的 `id` 到 `(worker, pid)` 的映射在 `TerminalRegistry`。

### 4.6 审计

`terminal.open`（target 为目标用户名，params 含 shell）与 `terminal.close`（detail 为关闭原因）经 `02-audit.md` 的写入点记录。其它用户身份的终端 `elevated = true`。

### 4.7 前端（`/debug` 页）

vendor `@xterm/xterm` 5.x 的 `xterm.js` 与 `xterm.css`，以及 `@xterm/addon-fit`，gzip 入库方式同 uPlot（`crates/strixmaid-server/src/debug/README.md`）。面板：终端列表（`GET /terminals`）、新建（shell / user 可选）、点击附着、关闭；附着时用 `FitAddon` 计算 cols/rows 并发 `resize`；窗口变化时重发。

## 5. 涉及文件

| 文件 | 改动 |
|---|---|
| `Cargo.toml`（core、server） | `cargo add -p strixmaid-core portable-pty`；server 无新依赖 |
| `crates/strixmaid-types/src/ipc.rs` | 帧头 `fd_count`；同步与 tokio 编解码 |
| `crates/strixmaid-core/src/session/framing.rs` | `recvmsg` 带 cmsg |
| `crates/strixmaid-helper/src/ipc.rs`、`spawn.rs` | 帧头同步；`SCM_RIGHTS` 发送逻辑抽到 types 或 core 共用 |
| `crates/strixmaid-core/src/worker/terminal.rs` | 新建：PTY、泵、`term.*` 方法 |
| `crates/strixmaid-core/src/worker/spawn_as.rs` | 新建：以指定用户 exec（从 helper 移植） |
| `crates/strixmaid-core/src/terminal/mod.rs` | 新建：`TerminalRegistry`、`RingBuf` |
| `crates/strixmaid-core/src/session/mod.rs` | 登出 / 超时时关闭终端 |
| `crates/strixmaid-core/src/config.rs` | `TerminalConfig { idle_timeout_secs, max_per_session }` |
| `crates/strixmaid-server/src/routes/terminals.rs` | 新建：4 个 REST 端点 |
| `crates/strixmaid-server/src/ws/terminal.rs` | 新建：`/ws/terminal/{id}` |
| `crates/strixmaid-server/src/debug/` | xterm.js 与面板 |

## 6. 测试

本机以当前用户走真实 helper 可完整测试（`setuid(自身)` 合法）：

1. **单元**：`RingBuf` 满环覆盖与顺序；帧编解码带 0 / 1 个 fd 的往返（socketpair 自测）。
2. **集成**：`POST /terminals {}` → 附着 WS → 发送 `echo $USER\n` → 收到含当前用户名的输出 → 发 `resize 100x30` → 发送 `stty size\n` → 收到 `30 100` → 断开 WS → 再附着 → 回放内容含之前全部输出 → `DELETE` → worker 内 shell 进程消失（`kill(pid, 0)` 返回 `ESRCH`）。
3. **集成**：`exit\n` 后收到 `{"t":"exit","code":0}` 且 WS 关闭，`GET /terminals` 不再列出。
4. **集成**：登出后该会话全部终端关闭，`pgrep -u $USER -f 'strixmaid worker'` 数量归零。
5. **集成**：`user` 指定其他用户且未提权 → 403。
6. **root 环境**：`user` 指定其他用户且已提权 → shell 内 `id -u` 为目标用户；`/proc/<pid>/status` 的 `Uid` 四列一致（无残留特权）。

## 7. 验收标准

> **实施状态（2026-08-28）**：主体已完成，但下列验收项**尚未满足**，详见
> [`docs/HANDOFF.md`](../HANDOFF.md)：
>
> - `{"t":"exit"}` 目前不带 `code`（退出码在 worker 里，没有通道送到主进程），故 6.3 未过；
> - 空闲 / shell 退出 / 登出这三种关闭**没有审计记录**（它们发生在 core 内部），故本节最后一条未过；
> - `/debug` 终端面板**从未在浏览器中运行过**；
> - `user` 指定其他用户的 setuid 路径**从未运行过**（开发机非 root），6.6 待 Linux+root 补测。


- 6.2–6.5 通过；
- 刷新页面后终端内容与光标位置恢复（xterm.js 回放后自动定位）；
- 一个会话开 8 个终端后第 9 个返回 `409 conflict`；
- 空闲 30 分钟的未附着终端被关闭，审计中有 `terminal.close` / `idle` 记录。

## 8. 未决问题

1. 帧头加 `fd_count` 是 IPC 协议的不兼容变更。helper、worker、主进程同版本发布，不存在跨版本兼容需求；但 `05-agent.md` 若复用帧格式，Agent 与 Server 之间应单独版本化。
2. 回看缓冲存的是原始字节，回放到新的 xterm.js 实例时终端状态（备用屏幕、光标形态）能否正确重建取决于 xterm.js 对转义序列回放的处理。`vim` 一类全屏程序在回放后可能需要 `Ctrl-L`。不在本方案内解决。
3. `terminal.max_per_session = 8` 与 `idle_timeout_secs = 1800` 为本方案提出的默认值。
