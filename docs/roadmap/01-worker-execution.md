# 01 请求经 worker 执行

## 1. 目标

使 `design.md` §2.2 与 §5 描述的授权模型实际生效：

- 读操作在**该会话的 user worker**（uid = 登录用户）内执行；
- 写操作在**该会话的 admin worker**（uid = 0，提权后才存在）内执行，未提权时返回 `403 elevation_required`；
- 主进程只保留与登录用户无关的工作：全局指标采集、存储、会话路由、静态资源。

完成后，polkit、journald ACL、文件权限对每个请求的裁决对象都是真实的登录用户，服务端不再包含任何授权判断代码。

## 2. 现状

| 项 | 位置 | 状态 |
|---|---|---|
| worker 进程与 RPC 骨架 | `crates/strixmaid-core/src/worker/mod.rs` | `Dispatcher::register_fn(method, f)`、`dispatch(method, params)`、`run_from_fd(fd, dispatcher)`；已注册 `ping`、`whoami` |
| 主进程侧句柄 | `crates/strixmaid-core/src/session/worker_handle.rs` | `WorkerHandle::call(method: &str, params: Value) -> Result<Value, ApiError>`、`pid()`、`uid()`、`shutdown()`；`Hello` 帧校验 uid |
| 会话到 worker | `crates/strixmaid-core/src/session/mod.rs` | `SessionManager::user_worker(token_hash) -> Option<Arc<WorkerHandle>>`、`admin_worker(token_hash)` |
| 请求中的会话 | `crates/strixmaid-server/src/auth/middleware.rs` | `require_auth` 把 `Session` 与 `UserIdentity` 放进 request extensions |
| 路由处理器 | `crates/strixmaid-server/src/routes/{system,processes,services,logs}.rs` | 直接持有 provider 实例并在主进程内调用 |
| worker 子命令 | `crates/strixmaid-server/src/main.rs::run_worker` | 构造空 `Dispatcher` 后调用 `run_from_fd` |
| IPC 消息 | `crates/strixmaid-types/src/ipc.rs` | `ToWorker::{Call{id, method, params}, Shutdown}`、`FromWorker::{Hello, Result, Error}`；无流式消息 |

## 3. 设计约束

- `design.md` §2.2：全局指标采集留在主进程；一个会话最多两个 worker。
- `design.md` §5.1：不自建 RBAC。服务端只做「选哪个 worker」这一件事，不判断用户能否执行某操作。
- `design.md` §6：capability `user` 层应实测（试读 journal、查 sudo 组），不只按组名推导。
- `design.md` §11：`strixmaid-core` 的接口接受 node 上下文；MVP 中固定为 `local`。
- worker 是 `strixmaid` 主二进制的子命令，静态链接；provider 代码在 worker 内运行，其依赖（zbus、procfs）都已是 core 的依赖。

## 4. 方案

### 4.1 RPC 方法表

方法名以 provider id 为前缀。`params` 为对应请求 DTO 的 JSON，`result` 为响应 DTO 的 JSON。错误以 `ApiError` 原样回传（`FromWorker::Error`）。

| 方法 | params | result | 读/写 |
|---|---|---|---|
| `host.info` | — | `SystemInfo` | 读 |
| `host.health` | — | `HealthReport` | 读 |
| `host.time` | — | `TimeInfo` | 读 |
| `host.set_hostname` | `SetHostnameReq` | — | 写 |
| `host.set_timezone` | `SetTimezoneReq` | — | 写 |
| `host.power` | `PowerReq` | — | 写 |
| `proc.list` | `ProcessListQuery` | `Vec<ProcessSummary>` | 读 |
| `proc.detail` | `{pid}` | `ProcessDetail` | 读 |
| `proc.signal` | `{pid, signal}` | — | 写* |
| `proc.renice` | `{pid, nice}` | — | 写* |
| `service.list` | `UnitListQuery` | `Vec<UnitSummary>` | 读 |
| `service.detail` | `{scope, unit}` | `UnitDetail` | 读 |
| `service.file` | `{scope, unit}` | `UnitFile` | 读 |
| `service.deps` | `{scope, unit}` | `UnitDeps` | 读 |
| `service.action` | `{scope, unit, action}` | `UnitActionResp` | 写 |
| `log.query` | `LogQuery` | `LogPage` | 读 |
| `log.entry` | `{cursor}` | `LogEntryDetail` | 读 |
| `log.boots` | — | `Vec<BootInfo>` | 读 |
| `caps.probe_user` | — | `UserProbe`（见 4.6） | 读 |

带 `*` 的两项特殊：向自己的进程发信号是普通用户的正当操作，不应要求提权。规则：`proc.signal` / `proc.renice` 先走 user worker；worker 内 `kill()` 返回 `EPERM` 时，若会话已提权则改走 admin worker 重试，否则返回 `403 permission_denied` 并带 `can_retry_elevated = true`。`service.action` 在 `scope = user` 时同理（用户操作自己的 user unit 不需要 root）。

### 4.2 worker 侧

在 `crates/strixmaid-core/src/worker/` 下新增 `providers.rs`：

```rust
pub async fn default_dispatcher() -> Dispatcher
```

构造 `HostProvider`、`ProcProvider`、`pick_service_provider().await`、`pick_log_provider().await`，按 4.1 的表注册。provider 为 `None` 时对应方法返回 `ApiError::capability_unavailable`。`run_worker` 改为调用它。

worker 以登录用户身份运行，因此：

- `pick_service_provider` 在 worker 内连 system bus 时，zbus 的 EXTERNAL 认证携带的是该用户的 uid，polkit 据此裁决；
- `scope = user` 时 `SystemdBus::with_user_uid(uid)` 连的是 `/run/user/<uid>/bus`，此时 uid 即 worker 自身，认证天然通过。这直接解决 `gap-analysis.md` §3 第 8 项，不需要额外机制；
- journalctl 子进程继承 worker 的 uid，可见范围由 journald ACL 决定。

`ProcProvider` 的 CPU% 差分快照是实例状态。每个 worker 一个实例，快照随 worker 生命周期存在，与主进程共用一个实例的现状等价。

### 4.3 主进程侧：调用汇聚点

在 `crates/strixmaid-server/src/auth/` 下新增 `exec.rs`：

```rust
pub enum Privilege { User, Admin }

pub async fn call<P: Serialize, R: DeserializeOwned>(
    auth: &AuthState,
    session: &Session,
    privilege: Privilege,
    method: &str,
    params: P,
) -> Result<R, ApiError>
```

行为：

1. `Privilege::User` 取 `sessions.user_worker(&session.token_hash)`；`Admin` 取 `admin_worker`，为 `None` 时返回 `ApiError::elevation_required`（`ErrorCode::ElevationRequired`，HTTP 403）。
2. worker 不存在（进程已死）时返回 `401 unauthenticated`，并调用 `sessions.logout` 使会话失效；客户端重新登录。
3. 调用 `WorkerHandle::call`，结果反序列化为 `R`。
4. 这是全部写操作的唯一出口，`02-audit.md` 在此处写审计。

路由处理器改为：

```rust
async fn info(State(auth): State<Arc<AuthState>>, Extension(session): Extension<Session>) -> Result<Json<SystemInfo>, ApiErr> {
    Ok(Json(exec::call(&auth, &session, Privilege::User, "host.info", ()).await?))
}
```

各路由模块的 `XxxState` 不再持有 provider；`ApiStates` 中对应字段改为 `Arc<AuthState>`，或统一从 `AuthState` 取。`routes/mod.rs` 相应调整。

### 4.4 流式 RPC

`logs.follow` 的可见范围必须随用户，因此 follow 子进程要在 user worker 内运行。现有 IPC 只有请求—响应，需增加订阅语义。

`crates/strixmaid-types/src/ipc.rs` 增加：

```rust
ToWorker::Subscribe   { id: u64, channel: String, params: Value }
ToWorker::Unsubscribe { id: u64 }
FromWorker::Event     { id: u64, data: Value }
FromWorker::End       { id: u64, error: Option<ApiError> }
```

`WorkerHandle` 增加：

```rust
pub async fn subscribe(&self, channel: &str, params: Value) -> Result<mpsc::Receiver<Value>, ApiError>
```

返回端 drop 时发送 `Unsubscribe`。worker 侧 `Dispatcher` 增加 `register_stream(channel, f)`，`f` 返回 `BoxStream<Value>`；worker 为每个订阅起一个任务，把流转成 `Event` 帧，流结束发 `End`。

背压：`mpsc` 容量 64；主进程消费慢时 worker 侧 `send().await` 阻塞该订阅任务，不影响其它 RPC。日志 follow 的批次已在 provider 内合并。

WS hub 的 `ChannelSource::subscribe(&self, params)` 需要知道是哪个会话在订阅。修改 `crates/strixmaid-server/src/ws/hub.rs`：

```rust
pub struct SubscribeContext { pub session: Session }
fn subscribe(&self, params: Option<Value>, ctx: &SubscribeContext) -> Result<ChannelStream, ApiError>;
```

`ws/handler.rs::upgrade` 已在 `require_auth` 之后执行，从 `Extension<Session>` 取会话传入 `hub.serve`。`metrics.live` 忽略 ctx；`logs.follow` 改为经 `WorkerHandle::subscribe("log.follow", params)`；`services.changed` 的内容（unit 状态）对所有本地用户可见，保留主进程实现。

### 4.5 主进程不再需要的东西

完成后主进程不再构造 `HostProvider`、`ProcProvider`、service / log provider 用于请求处理。保留的用途：

- `CapabilityRegistry` 的 `probe_all()`（启动期 system 层探测）；
- `services.changed` 频道的事件源；
- `system.health` 频道（见 `04`）的定时健康检查。

`main.rs::serve` 相应精简。

### 4.6 capability user 层实测

`caps.probe_user` 在 user worker 内执行：

| 字段 | 探测方式 |
|---|---|
| `can_read_journal` | `journalctl -n 1 -q --system` 退出码为 0 且有输出。非 root 且不在 ACL 组时 journalctl 只返回用户自己的日志，用 `_TRANSPORT=kernel` 过滤可区分：能读到内核日志即有系统日志权限 |
| `can_manage_units` | `getuid() == 0`。polkit 的裁决无法离线探测，按 design §6 已提权即 true |
| `can_elevate` | 保持按组推导（`sudo` / `wheel` / `admin`），另检查 `/etc/sudoers.d` 不可读时不降级 |
| `user_units` | `/run/user/<uid>/bus` 存在且可连 |

结果与 `derive_user_caps` 合并：实测值覆盖推导值。`GET /capabilities` 在有会话时经 `exec::call` 取 `caps.probe_user`，结果按会话缓存 60 秒。

### 4.7 性能

进程列表 3000 个进程约 1 MB JSON，经 socketpair 一次往返。debug 构建实测主进程内 156 ms，IPC 序列化预计增加 10–20 ms。可接受；不做二进制编码。

`service.list` 的 610 ms 瓶颈在 systemd 的 `ListUnitFiles`（`busctl` 单独调用即 0.6 s），与 IPC 无关。

### 4.8 提权授权检查

现状：`SessionManager::elevate_start` 不检查资格，helper 的 `as_root` 分支只判断 helper 自身是否为 root。root 部署下任何用户重输密码即可获得 root worker。

规则：仅当用户属于 `session.elevate_groups`（新增配置，默认 `["sudo", "wheel", "admin"]`）之一时允许提权。检查在两处执行：

1. **helper**（权威）：处理 `SpawnWorker { as_root: true }` 时，用 `AuthOk` 阶段已取得的 `groups` 判断；不满足则返回 `Error`，不 fork。helper 是持有 root 的组件，检查必须在它内部完成，主进程的判断只能视为 UX。
2. **`elevate_start`**（提前拒绝）：不满足时直接返回 `403 permission_denied`，不 spawn 第二个 helper、不进入 PAM 对话。错误信息说明所需的组。

`elevate_groups` 经 `AuthStart` 帧传给 helper（`ToHelper::AuthStart` 增加字段），避免 helper 读配置文件。

与 `design.md` §5.1「不自建 RBAC」的关系：这不是应用层权限矩阵，而是把「谁能成为 root」交给系统的组策略——与 `sudo` 默认配置一致，也与 Cockpit 的管理访问模型一致。更严格的做法是在 user worker 内执行 `sudo -n -v` 询问 sudoers（覆盖非基于组的规则），依赖 `sudo` 存在，留作后续选项。


## 5. 涉及文件

| 文件 | 改动 |
|---|---|
| `crates/strixmaid-types/src/ipc.rs` | 4.4 的四个消息 |
| `crates/strixmaid-types/src/capability.rs` | `UserProbe` |
| `crates/strixmaid-core/src/worker/mod.rs` | `register_stream`、订阅任务管理 |
| `crates/strixmaid-core/src/worker/providers.rs` | 新建，`default_dispatcher` |
| `crates/strixmaid-core/src/session/worker_handle.rs` | `subscribe`、`Event` / `End` 分发、订阅表 |
| `crates/strixmaid-core/src/session/framing.rs` | 无变化（帧格式不变） |
| `crates/strixmaid-core/src/capability/mod.rs` | 合并实测结果 |
| `crates/strixmaid-server/src/auth/exec.rs` | 新建，4.3 |
| `crates/strixmaid-server/src/routes/{system,processes,services,logs,capabilities}.rs` | 改为经 `exec::call` |
| `crates/strixmaid-server/src/routes/mod.rs` | `ApiStates` 精简 |
| `crates/strixmaid-server/src/ws/hub.rs`、`handler.rs`、`channels/logs_follow.rs` | `SubscribeContext` |
| `crates/strixmaid-server/src/main.rs` | `run_worker` 用 `default_dispatcher`；`serve` 精简 |
| `crates/strixmaid-core/src/config.rs` | `SessionConfig.elevate_groups` |
| `crates/strixmaid-types/src/ipc.rs` | `AuthStart.elevate_groups` |
| `crates/strixmaid-helper/src/main.rs`、`spawn.rs` | `as_root` 前的组检查 |
| `crates/strixmaid-core/src/session/mod.rs` | `elevate_start` 提前拒绝 |

## 6. 测试

1. **单元**：`Dispatcher` 的流式订阅——注册一个每 10 ms 产一帧的流，订阅后收到帧、`Unsubscribe` 后任务退出、流自然结束时收到 `End`。
2. **单元**：`WorkerHandle::subscribe` 对进程内 worker（`session/tests.rs` 已有的 mock 形态）的往返；接收端 drop 后 worker 收到 `Unsubscribe`。
3. **集成（本机可跑，非 root）**：以当前用户走真实 helper 登录（需要当前用户密码，由实施者在本机交互式提供，不写入任何文件），然后：
   - `GET /processes/{worker_pid}`：返回的 `uid` 等于当前用户，证明请求确实在 worker 内执行；
   - `GET /logs?boot=0&limit=1`：结果集仅含当前用户的日志（本机用户不在 `adm` 组）；
   - `POST /services/ssh.service/action {restart}`：`403 elevation_required`（未提权）；
   - `POST /auth/elevate/start`：`403 permission_denied`（helper 非 root，`as_root` 被拒），错误信息说明原因；
   - `GET /capabilities`：`user.can_read_journal = false`（实测），`user.can_elevate` 按组。
4. **单元**：helper 的组检查——`groups` 不含 `elevate_groups` 任一项时 `SpawnWorker { as_root: true }` 返回 `Error`，进程表中无新 worker；含时（helper 为 root 才可测，见 07）成功。
5. **集成**：`elevate_start` 对不在 `elevate_groups` 的用户返回 403，且 `pgrep -c strixmaid-helper` 不增加（未 spawn 第二个 helper）。
6. **集成**：登录后 `kill -9` 该会话的 worker，下一次请求返回 401，`sessions` 表中该行已删除。
7. **root 环境**（见 `07-verification.md`）：上述第 3 项在 root 部署下以非 admin 用户与 admin 用户各跑一遍。

## 7. 验收标准

- `grep -rn 'HostProvider\|ProcProvider\|pick_service_provider\|pick_log_provider' crates/strixmaid-server/src/routes/` 为 0 处；
- 所有写端点在未提权会话下**被挡住且提示提权可解**。注意错误码分两种，
  这是 §4.1 决定的，不是实现走样：
  - 纯管理操作（`host.set_hostname` / `set_timezone` / `power`）→ 403 `elevation_required`；
  - 走「先以用户身份试」规则的（`proc.signal` / `proc.renice` / `service.action`）
    → 403 `permission_denied` + `can_retry_elevated = true`，因为拒绝来自内核 / polkit
    而不是「没有 admin worker」。
  （本条原文写的是「一律 `elevation_required`」，与 §4.1 冲突，已按 §4.1 更正。）
- 不在 `elevate_groups` 的用户提权返回 403，helper 内无对应 fork（6.4、6.5）；
- 6.3 全部通过；
- 质量门通过。

## 8. 未决问题

1. `proc.signal` 的「先 user 后 admin」重试规则（4.1）是本方案新增的，`design.md` 未定义。若不接受，改为一律走 admin worker，代价是用户杀自己的进程也要提权。
2. `services.changed` 保留在主进程意味着事件流不受用户身份限制。unit 状态本就对所有本地用户可见（`systemctl list-units` 无需权限），因此判断为可接受；若要求严格一致，改为 `WorkerHandle::subscribe("service.changed")`。
3. worker 内 `ProcProvider` 的首次 CPU% 为 0 的问题（无基线）会在每个新会话出现一次。可在 worker 启动时预采一次基线。


---

## 9. 完成状态（2026-08-28）

§4.1–§4.8 全部实现，`cargo test --workspace` 299 通过（连续 3 轮），clippy 零 warning，
`scripts/acceptance.sh` 的静态检查全过。

### 实现时对本文档的补充与更正

按实施约定第 8 条逐条记录。**协调者据此更新 `design.md` 的部分已在下面标出。**

#### 更正

1. **§7 的验收标准与 §4.1 自相矛盾**（已在 §7 就地更正）。写端点的 403 有两种错误码，
   取决于它走的是「必须 admin worker」还是「先以用户身份试」。
2. **§4.4 说 `ToWorker` / `FromWorker` 有手写 `Debug`** —— 没有，两者都是 derive；
   只有含明文密码的 `ToHelper` 是手写脱敏的。新增变体不含凭据，继续 derive。
3. **§4.4 说 `register_stream` 的 `f` 返回 `BoxStream<Value>`** —— 实际签名放宽成
   **async 且可失败**。`LogProvider::follow` 本身是 async 且返回 `ApiResult`
   （它要起 `journalctl -f` 子进程），不放宽就得在闭包里 block。

#### 新增的设计决定

4. **背压有 10 秒时限。** §4.4 说「主进程消费慢时不影响其它 RPC」——这在 **CPU 层面**
   成立（每订阅 / 每调用一个 task），但主进程与 worker 之间**只有一条 socket**：
   读循环若被慢订阅者无限期堵住，该 worker 的所有 RPC 响应都读不出来。
   因此超过 `SUBSCRIBER_STALL_LIMIT`（10s）即判定订阅者已死，摘登记并发 `Unsubscribe`。
5. **订阅的取消不用 `abort`。** abort 可能落在 `write_msg` 写了一半的位置，
   半截帧会毁掉整条连接的协议。改用 `oneshot` + `select!{biased}`，
   只在「等下一项」这个安全点响应取消，写帧永远原子。
6. **`subscribe()` 不等订阅确认。** 返回 `Ok` 只表示 `Subscribe` 帧已发出；
   频道不存在 / provider 不可用表现为「流立刻结束」。后果：无 journald 的机器上
   `logs.follow` 的客户端看到的是「频道已结束」而不是 `capability_unavailable`。
   要同步拿到失败原因就得给协议加一帧订阅确认，为一个几乎只在配置错误时发生的
   分支不值得。
7. **`Unsubscribe` 之后 worker 不再发 `End`**；`Unsubscribe` 对未知 id 是无操作、可重复发送。
8. **订阅 id 与调用 id 共用序号空间**（一条连接上一个 id 只对应一件事）。
9. **订阅参数 `null` 等同缺省**，客户端不必为订阅全部日志而专门送一个 `{}`。
10. **WS 的 `Session` 在升级时取快照**，连接期间不刷新。`logs.follow` 始终用 user worker，
    快照过期不会造成越权；将来若有频道要看 `elevated`，需要重新考虑。
11. **`service.action` 对两种 scope 都用升级重试**，而不是「`scope=system` 就要 admin」。
    后者等于在服务端重新实现一份权限矩阵，正是 `design.md` §5.1 禁止的自建 RBAC——
    某个发行版的 polkit 规则、某个 unit 自己的策略、`wheel` 组的默认配置，
    都可能已经允许登录用户直接操作。让 polkit 去判断。
12. **`log.entry` 的 404 同时覆盖「不存在」与「该用户看不到」。** 刻意不区分：
    区分就会泄露条目的存在性。
13. **`/capabilities` 不加 `security(("bearer" = []))`。** 它有意允许未认证访问
    （`design.md` §6：登录页要靠它判断 helper 可不可用），标上安全方案会让
    代码生成器以为必须带 token。其余受保护端点已全部补上 `security` 与 401。
14. **user 层实测结果按「会话 + 提权状态」缓存 60 秒。** §4.6 只说「按会话缓存」，
    但提权会改变结果（admin worker 起来后 `can_manage_units` 从「测不出」变成真），
    共用一条缓存会让刚提完权的用户在 60 秒内看到旧能力。
15. **实测失败不算错误**：`/capabilities` 沿用推导值并记 warn。能力探测本身失败
    而让这个端点 500，是最不该发生的事——前端正是靠它决定显示什么。

### 本机实测（2026-08-28，macOS，非 root，登录用户 uid 501）

`scripts/acceptance.sh` 带真实 token 跑完：

| 项 | 结果 |
|---|---|
| routes/ 下无 provider 的代码引用 | ✓ |
| 五个路由模块经 `exec::call` 执行 | ✓ |
| RPC 方法名全部引用常量，无字面量 | ✓ |
| `PUT /system/hostname` 未提权 | ✓ 403 `elevation_required` |
| `PUT /system/timezone` 未提权 | ✓ 403 `elevation_required` |
| `/capabilities` 报告 `user.uid` | ✓ 501 |
| **进程列表里 worker 的 uid** | ✓ **501，等于登录用户** |

最后一条是本次改造的核心证据：请求确实在**以登录用户身份运行的 worker** 里执行，
而不是在主进程里。§2 的缺口到此可以判定为闭合。

实测顺带发现并修掉一个缺陷：`launchd` 的 `action_error` 把「找不到该服务」
归进了 `internal`（500）。那是**调用方给错了名字**，应当是 404——
详情路径的 `not_found_or_denied` 一直是对的，操作路径漏了这一档。已补，并加用例。

`service.action` 的门禁**默认不自动测**：它走升级重试规则，请求会真的到达
launchctl / systemctl，用户若有权限那个服务就真被重启了。验收脚本把这一条
放在 `STRIX_ALLOW_MUTATING=1` 之后，默认报「未测」并说明原因——
验收脚本不该在别人机器上改变系统状态。

### 尚未验证

以下需要真实密码或 root 环境，属 `07-verification.md` 的范围：

- **§6.3 的集成验证**（登录后确认请求确实在 worker 内执行、日志可见范围随用户、
  未提权写操作 403）。`scripts/acceptance.sh` 已把这些编码成可执行断言，
  给它 `STRIX_TOKEN` 即可跑。
- **§6.6**：`kill -9` worker 后下一次请求返回 401 且 `sessions` 表中该行已删除。
- **§4.7 的性能数字**（进程列表经 IPC 的额外开销）未实测。
- **root 环境下的全部路径**（§6.7）。
