# 交接：roadmap/03 终端

> 写于 2026-08-28，同日第二轮更新（补退出码与关闭审计后重写 §1 §3 §7，新增 §2.5 §2.6）。
> 给下一个接手的人（或 AI）。
> 只记**从代码里读不出来的东西**：为什么这么做、哪些没做完、哪些地方会咬人。
> 代码结构、API 签名请直接看源码，不在这里复述。

## 1. 一句话状态

`roadmap/03-terminal.md` **代码侧的验收项已全部实现**：第二轮补上了退出码（§2.5）与
非 REST 关闭的审计（§2.6）。工作区 clippy 零警告、`cargo test --workspace` 414 通过
（Ubuntu 开发机；此前 388 是 macOS 上的数字，Linux 会多编译出一批测试）。
剩下 **2 项只能换环境做**（§3）：浏览器实测与 Linux root 下的 setuid 路径。

## 2. 已完成的部分与关键决策

### 2.1 IPC 帧带 fd（地基，已提交）

帧头从 `u32 长度` 改成 `u32 长度 + u8 fd_count`。

**这条必须先理解，否则改动这块必然出事**：`SOCK_STREAM` 上用普通 `read()` 读过附着了
`SCM_RIGHTS` 的那些字节，**内核会把 fd 直接丢掉**——不报错、无痕迹，只是拿到的终端永远连不上。
所以只要一条连接上**可能**出现带 fd 的帧，它的读端就必须**每一帧**都走 `recvmsg`。

由此产生的连锁改动：

- `WorkerHandle` 不能再 `into_split()`（`OwnedReadHalf` 给不了裸 fd），改成读写共享
  一个 `Arc<UnixStream>`，写侧用 `Mutex` 串行化保证一帧原子写完；
- worker 侧同理，`FrameWriter` 取代了原来的 `Arc<Mutex<OwnedWriteHalf>>`。

fd 在类型上一路显形，不给「顺手丢掉」留位置：`Dispatcher` 有独立的 `fd_handlers` 表
（`register_fd` / `dispatch_with_fds`），`WorkerHandle::call_with_fds` 取走 fd，
普通 `call` 收到没人接的 fd 会**告警并关闭**。收到个数与帧头 `fd_count` 不符即协议错——
少收一个 fd 意味着后面拿着半个终端往下走，早炸比晚炸便宜。

### 2.2 身份判断只有一处

「该不该以别人的身份开终端」**只在 `server/src/routes/terminals.rs::privilege_for` 判断**，
结果只体现为「投给 user worker 还是 admin worker」。worker 拿到 `TermOpenParams.user`
之后照做，**不再判第二次**。

不要「为了保险」在 worker 里再加一道检查：两套鉴权迟早不一致，而不一致的那一侧就是提权漏洞
（`design.md` §5.1）。`types/src/rpc.rs` 里 `TermOpenParams` 的文档专门讲了这点。

注意一个容易搞反的细节：**已提权的会话开自己的终端仍然走 user worker**。
走 admin 会让 shell 以 root 起来再 setuid 回去，多一次不必要的特权经过。

### 2.3 会话销毁只有一个汇聚点

`session/mod.rs::teardown` 是登出、空闲超时、进程退出三条路的**共同出口**，
所以终端回收的钩子只挂在那里一处。

顺序很重要：**先关终端，再关 worker**。终端的 PTY 跑在 worker 里，关闭要靠一次
`term.close` RPC 送进去；worker 先没了，那个 RPC 就发不出去，可能留下游离的 shell。

### 2.4 worker 侧 PTY 的两个非显然之处

- **不能用 `portable-pty` 的 `SlavePty::spawn_command`**：它内部写死了一段 `pre_exec`，
  塞不进 setuid。因此只用它 `openpty`，fork/exec 自己写（`worker/spawn_as.rs`）。
- **fork 之后不能调 `initgroups`**：helper 是单线程可以那么干，worker 跑在多线程 tokio 上，
  fork 后走 NSS 有 malloc/NSS 锁死锁风险。现在的做法是 fork **之前** `getgrouplist` 算好，
  fork 后只做纯系统调用。
- 放弃特权的次序 `setgroups → setgid → setuid`，之后有一步 `setuid(0)` **必须失败**的自检。
- 子进程会关掉 3 号及以上全部 fd：helper `dup2` 上来的 IPC fd 3 天生没有 CLOEXEC，
  不关就等于把主进程的控制通道交给终端用户。**这条有测试守着**
  （`shell_不继承_worker_不带_cloexec_的_fd`），改动 exec 路径时不要绕过它。

### 2.5 退出码走 `term.close` 的返回值（第二轮新增）

`{"t":"exit"}` 的 `code` / `signal` 来自 `term.close` 的结果 `TermCloseResult { exit }`
（`types/src/rpc.rs`）。通路上有三处决定，别拆：

- worker 的收尸任务（`worker/terminal.rs::reap`）把 `waitpid` 的结果折成
  `TermExit { code, signal }`，经收尸任务的 `JoinHandle` 交给 `close`；
- shell **自行退出**时条目不再立即出表：标记 `closed` 后保留 30 秒
  （`REAPED_LINGER`，测试用 `with_linger` 缩短），等主进程随 EOF 补发的
  `term.close` 来取。保留期内 `closed = true`，`close` 对这种条目**跳过 killpg**
  ——pid 可能已被系统重用，这条不能省（有测试 `自行退出后_close_取回真实退出码`）；
- 主进程的 `shutdown`（`terminal/mod.rs`）改为**先发 `term.close` 取回状态、再通知
  附着方**——反过来 exit 帧永远带不上退出码。附着方因此多等一次 RPC 往返；
  EOF 路径上 shell 已死、状态现成，只有几毫秒。

取不到状态就是取不到（`exit: None`，帧里不带字段），没有任何一处编造 `code: 0`。

### 2.6 非 REST 关闭的审计走观察者（第二轮新增）

`TerminalRegistry` 增加 `TerminalObserver` 钩子（`set_observer`），每次**真正执行**的
关闭恰好回调一次（幂等裁决输掉的一方不回调；有测试守着）。server 侧实现是
`auth::audit::TerminalAudit`，`main.rs` 启动时装入。三条容易被「顺手改坏」的决定：

- actor 身份（用户名 / uid / 是否提权）在**开终端那一刻**定格进 `TerminalOwner`
  随终端保存——登出路径上会话正在拆，关闭时再反查会查无此人；
- `deleted` 在观察者里**跳过**：`DELETE /terminals/{id}` 的处理器已经记了（那条带
  来源地址），观察者再记就违反 roadmap/02 §7「一次用户操作恰好一条记录」；
- 回调是同步的、在关闭路径上，写库 `spawn` 出去，失败只记日志不阻断关闭。

## 3. 没做完的（按重要性排序）

> 上一轮记在这里的「exit 帧无退出码」「非 REST 关闭无审计」已按原建议实现，
> 机制见 §2.5 与 §2.6。剩下两项都是环境所限，不是代码没写。

### 3.1 `/debug` 终端面板从未在浏览器里跑过

面板代码是完整的（xterm.js 5.5.0 + addon-fit 0.10.0 已 vendor 进 `server/src/debug/vendor/`），
但**从头到尾没有在真浏览器里打开过**——写它的时候后端路由还不存在，本机也没有无头浏览器。
只验证过内联 JS 能被解析。

未经验证的具体点：`FitAddon` 在面板的滚动容器里算出的 cols/rows 是否合理；
`term.reset()` 之后服务端回看缓冲的回放是否正确落位；面板重绘（`replaceChildren`）后
xterm 实例是否还活着；`exit` 后面板是否显示出真实的 code（后端已带，见 §2.5）。

**下一步第一件事就该是**：起服务、登录、开一个终端、在浏览器里真的敲几下。

### 3.2 setuid 到**其他**用户的路径从未运行过

写它的开发机是 macOS 且非 root；当前的 Ubuntu 开发机（Linux 全量测试在此通过）
同样无 root。所以 `spawn_as.rs` 里 `identity` 那个分支（`setgroups/setgid/setuid` +
tty chown）只有编译与单侧测试保证，**没有任何运行时验证**。

`roadmap/03` §6.6 那条 root 环境的验收必须在 Linux + root 上补。

## 4. 会咬人的地方

| 坑 | 说明 |
|---|---|
| **helper 的 spawn 测试用的是磁盘上的旧二进制** | `crates/strixmaid-helper` 的 `spawn_真实_worker_并完成_whoami` 会 spawn `target/` 下已有的 `strixmaid`。**改了 IPC 帧格式后若不先 `cargo build -p strixmaid-server`，它会用旧二进制而失败**，且错误看起来像帧解析 bug（`TooManyFds { count: 123 }`，123 是 `{` 的 ASCII）。 |
| **不要另建 target 目录** | 两个 agent 都干过把 target 指到 `/tmp/cargo_target*` 的事，一次留下 13 GB。而且并发 cargo 抢同一个锁会互相堵死。用仓库默认的 `target/`。 |
| **测试里的 PTY 会 fork 真进程** | `worker::terminal` 的测试真的起 shell。测试挂住时先 `ps` 看有没有留下孤儿进程和挂死的测试二进制——它们会占着 cargo 的构建锁，让后续每条 cargo 命令永远排队。 |
| **仓库不是 rustfmt-clean** | `cargo fmt --check` 在**未改动的** HEAD 上就有 diff（`capability/mod.rs`）。所以 fmt 不在质量门槛里，别顺手全量格式化，会制造巨大噪声 diff。 |
| **`metrics::engine` 曾有 1/60 概率的 flaky** | 已修：测试依赖墙上时刻对齐，60 个偏移里恰有一个（`start ≡ 1 mod 60`）少一个桶。现在把 `now` 对齐到整分。**别把这个对齐删掉**。 |

## 5. 质量门槛

```sh
cargo clippy --workspace --all-targets   # 必须零警告
cargo test --workspace                   # 必须全绿
```

注意事项见 §4 第一行（先 build server 再跑 helper 的测试）。

**测试要能真的发现问题**：这个仓库里已经用变异测试验证过几条关键测试确实会失败——
关掉子进程关 fd 的循环，`shell_不继承...` 会报 `LEAK`；改错 vendor 路由名，
`页面引用的_vendor_文件都能取到` 会报 404。新增测试请保持这个标准，
只断言「没报错」的测试等于没测。

## 6. 另一条并行的线

`docs/roadmap/08-metrics-and-panel.md` + 同名 `.mockup.html` 是**另一个会话**做的
指标裁剪与面板重做提案（58 项裁到 34 项 + 任务管理器式面板），已在 `design.md` §7 前
加了指向它的提案说明。它与 03 无重叠，但**尚未实施**。

我对它的一条意见（尚未写进那份文档）：§12 的 **Q1 倾向 (a)「把 NVML 下放给 `strixmaid-helper`」有生命周期冲突**——
`design.md` 第 457 行写明 helper 是**每会话一个**、持有 PAM 句柄，而指标引擎是**常驻、与登录无关**。
照 (a) 字面实现，**没人登录时就没有 GPU 指标**，而且空洞会一路带进五层桶的 rollup。
可行的修法是由 daemon 启动时拉起一个**独立的长命采集进程**（复用那个动态链接的二进制可以，
但它不能是 PAM 会话 helper）。

## 7. 下一步建议顺序

1. 起服务，在浏览器里把终端**真的用一遍**（§3.1）——这是唯一能暴露一整类前端问题的
   办法；顺带确认 `exit` 后面板显示出真实的 code（调试页已改为显示 code / signal）；
2. Linux + root 环境补 §3.2 的验收；
3. 之后进 `roadmap/04-files-and-ws-channels.md`。
