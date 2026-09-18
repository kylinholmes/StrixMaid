# 10 抽出 node 层：让 AgentCore 成为可被两个宿主装载的东西

## 1. 目标

`design.md` 开篇第 5 条写的是「**AgentCore 是唯一的业务逻辑所在地。Server 与 Agent 都只是它的宿主**」，§11 写的是「Server 内含一个 AgentCore 实例，即 `local` 节点，与远程节点走完全相同的代码路径」。

实现漂了。今天 `strixmaid-server` 有 12,809 行，其中 10,076 行是业务逻辑——API 处理器、认证与会话、审计、WS 频道、SCM 托管——而 `strixmaid-agent` 只有 894 行，是一个只会采集和推送的壳。两个宿主装载同一个 AgentCore 这件事，没有发生。

本方案把这 10,076 行抽进新 crate `strixmaid-node`，使：

- `strixmaid`（server）= node + UI + 节点目录 + 转发；
- `strixmaid-agent` = node + 向 Server 拨号；
- 两者提供**同一个 `axum::Router`**，逐字节相同的 API。

**本方案不改变任何外部行为。** 端点、响应、鉴权、审计、CI 全部照旧；唯一的证明标准就是这一条（见 §6）。远程管理、管道转发、SSH 推装属于 `11-multi-host.md`，本方案只为它们把地基铺平。

## 2. 现状

`strixmaid-server/src` 全部 42 个文件共 12,809 行，按去向分（三段加总即 12,809）：

| 去向 | 模块 | 行数 | 说明 |
|---|---|---|---|
| **→ node** | `routes/`（除 `nodes.rs`） | 4,228 | API 本体。11 个路由模块 + `mod.rs` |
| | `auth/` | 1,622 | `exec` 选 worker 与审计写入点、`audit` 落库、`middleware`/`extract` 鉴权 |
| | `ws/`（除 `agent.rs`、`tests.rs`） | 1,670 | `hub` 订阅、5 个频道、`terminal` 流 |
| | `service/` | 1,733 | SCM 托管（`host`/`scm`/`logging`/`mod`） |
| | `main.rs` 的 `serve_with` 一段 | ~300 | 启动编排、`ShutdownKind`、`StartupReporter` |
| | `apidoc.rs` `app.rs` `error.rs` `state.rs` `debug/` | 523 | 描述与装配 |
| **留 server** | `embed.rs` `assets.rs` | 358 | 前端资源与 SPA 回退 |
| | `ws/agent.rs` | 594 | B 拨进来的那条 WS（11 号方案里长成管道） |
| | `routes/nodes.rs` | 295 | 节点目录 |
| | `cli.rs` `main.rs` 其余 | ~845 | 子命令与进程入口 |
| **两边各有** | `ws/tests.rs` | 641 | 按被测对象拆 |

crate 依赖现状，两处与直觉不同，记下来免得重复踩：

- **`strixmaid-helper` 只依赖 `strixmaid-types`**，不依赖 core。所以「把 API 层放进 core 会拖累 helper」这个担心不成立——它独立成 crate 的理由是另一条，见 §3.1。
- **worker 的 RPC 契约已经在 `strixmaid-types/src/rpc.rs`**：30 个方法名是常量，worker 侧按名字注册处理器、主进程侧按同名发起调用。这条缝已经在正确的位置，本方案不动它。

## 3. 设计

### 3.1 为什么是新 crate 而不是并进 core

`design.md` 的目录树把 core 标成「★ AgentCore —— 全部业务逻辑」，按字面读，API 层并进 core 也说得通。不这么做的理由只有一条，但足够：

core 已经 58,137 行。它现在的角色是「能力库」——providers、worker、session、metrics、store、platform，每一项都不知道 HTTP 的存在。把 axum、utoipa、tower 引进来之后，这条边界就只剩约定，而约定会漂（本方案要修的正是一次漂移）。分成独立 crate 之后，**编译器替人守这条线**：core 的 `Cargo.toml` 里没有 axum，想 `use` 也 use 不到。

代价是多一层 crate。可以接受。

### 3.2 API 的单位是 `Router`

抽出来的东西不是「一堆 handler」，是一个函数：

```rust
// strixmaid-node
pub fn router(node: Arc<Node>) -> axum::Router;
```

`Node` 持有这台主机的全部运行时状态（会话管理器、provider 注册表、metrics engine、store、终端注册表、审计写入器）。`router()` 返回的是**完整的 `/api/v1` 与 `/ws`**，不带任何节点前缀、不含前端资源。

这样一来：

| 宿主 | 怎么用 |
|---|---|
| server，本机 | `router(local_node)` 挂在 `/`（今天的行为）与 `/nodes/local`（11 号方案） |
| server，远程节点 | 不用。远程走管道，B 那边自己 serve（11 号方案） |
| agent | `router(node)` serve 在管道的一条流上；`hyper` 可以在任意 `AsyncRead + AsyncWrite` 上 `serve_connection`，不需要 TCP |

将来若要「浏览器直连 B」，也只是把同一个 `Router` 绑到 B 的 `TcpListener` 上，业务代码一行不改。这是选 `Router` 作为单位的主要收益。

### 3.3 `Node` 的构造与生命周期（**本次未做**，见 §7 未决 4）

> **实施偏离（2026-09-18）**：这一节没有做。`serve_with` 的启动编排仍留在 `strixmaid`。
> 它与监听器、agent 注册表绑得紧，抽它要连关停路径一起动，与本方案「纯搬家、
> 行为零变化」的性质不符——那会让 OpenAPI diff 这个硬门槛失去意义（门槛证明的是
> 「搬对了」，一旦同时改了编排，diff 为空就只能证明「端点没变」）。
> 移入 node 的只有 `ShutdownKind` / `StartupReporter` 两个接口，SCM 托管要用。
> 下面这段是原计划，留作后续的依据。

今天 `serve_with` 里按顺序做的事——开库、起 metrics engine、探测能力、起 helper 会话管理器、装终端注册表、接审计观察者——应当全部搬进：

```rust
impl Node {
    pub async fn start(cfg: &NodeConfig, reporter: Arc<dyn StartupReporter>) -> Result<Arc<Node>>;
    pub async fn shutdown(&self, kind: ShutdownKind);
}
```

`StartupReporter` 与 `ShutdownKind` 一并从 `strixmaid-server/src/main.rs`（现为 `pub(crate)`）移入 node 并转为 `pub`。它们本来就是为 SCM 的递增 checkpoint 设计的接口，而 SCM 托管也要搬到 node——两者必须在同一个 crate 里才不用互相 re-export。

`NodeConfig` 是今天 `Config` 中与 HTTP 无关的那部分。`listen`、`tls`、前端相关项留在 server 的 `Config` 里。**配置文件格式不变**：拆的是 Rust 结构，不是 TOML 的形状。

（实际实现里连 `NodeConfig` 都没拆——`Config` 本来就在 `strixmaid-core`，node 直接用它。拆分留到真正做 `Node::start` 时再评估是否必要。）

### 3.4 SCM 托管归 node

`service/` 四个文件搬进 `node/src/winsvc/`，并参数化掉三处 server 专属耦合：

| 耦合点 | 现在 | 之后 |
|---|---|---|
| `SERVICE_NAME` / `DISPLAY_NAME` / `DESCRIPTION` | 三个常量 | `&'static ServiceIdentity` 入参 |
| `crate::load_config` + `crate::serve_with` | 直接调 server 的函数 | 一个回调：给你 reporter 与关停接收端，你把主循环跑完 |
| `image_path(&exe, global)` | 按 server 的全局参数拼 | 由宿主提供 argv |

还有一处**搬家时必然踩**的坑，先写下来：`host.rs` 里有 `env!("CARGO_PKG_VERSION")`，搬进 node 之后它取的是 node 的版本而不是宿主二进制的版本。版本号要进 `ServiceIdentity`。

agent 的服务身份用 `StrixMaidAgent`，默认账户 `NT AUTHORITY\LocalService`（server 是 `LocalSystem`）——agent 是只读采集器，最小权限起步。实测哪些指标在 LocalService 下取不到，结论写进 `docs/windows-platform.md`；缺得太多再退回 `LocalSystem`。

### 3.5 server 剩下什么

```
strixmaid-server  2,092 行
├─ embed.rs / assets.rs     358   前端资源与 SPA 回退
├─ routes/nodes.rs          295   节点目录
├─ ws/agent.rs              594   B 拨进来的那条 WS
└─ main.rs / cli.rs         845   子命令、进程入口、装配
```

外加 `ws/tests.rs` 里留下的那部分（见 §7 未决 2），所以验收定在 2,500 行。

`app.rs::build` 从「组装全部东西」缩成「把 node 的 `Router` 与前端、节点目录 merge 起来」。

### 3.6 不做

- 不加任何端点，不改任何响应形状。
- 不动 `/nodes/<id>` 路径前缀（11 号方案）。
- 不动 agent 的能力范围——本方案结束时 agent 仍然只采集和推送，只是**代码结构上已经能装载完整 node**。
- 不动 worker、helper、IPC 帧格式。
- 不改前端一行。

## 4. 涉及文件

**新增**：`crates/strixmaid-node/`（`Cargo.toml` + 上表「→ node」的全部模块）。

**修改**：`crates/strixmaid-server/src/{main,cli,app,embed,assets}.rs`、`crates/strixmaid-server/src/routes/nodes.rs`、`crates/strixmaid-server/src/ws/agent.rs`、`crates/strixmaid-server/Cargo.toml`、`crates/strixmaid-agent/Cargo.toml`、workspace `Cargo.toml`。

**删除**：`crates/strixmaid-server/src/` 下已搬走的模块。

**文档**：`docs/design.md` §11 与目录树；`docs/roadmap/README.md` 索引。

## 5. 测试

本方案是搬家，测试的全部意义在于证明**什么都没变**：

1. 现有测试**一个不改地全部通过**。测试文件跟着被测对象走，但断言内容不许动——改断言就等于放弃了这个证明。
2. `scripts/api-smoke.sh` 与 `scripts/acceptance.sh` 在搬家前后输出一致。
3. OpenAPI 文档逐字节相同：搬家前后各导出一次 `/api/v1/openapi.json`，`diff` 为空。这一条比任何单测都硬——它覆盖全部端点的路径、方法、请求与响应 schema。
4. 三平台 CI 全绿（`quality` 矩阵 + `build-*` + `package-*`）。

## 6. 验收

1. §5 四项全部满足，其中第 3 项（OpenAPI diff 为空）是**硬门槛**。
2. `strixmaid-node` 不依赖 `strixmaid-server`；`strixmaid-core` 的依赖表里没有 axum / utoipa / tower。
3. `strixmaid-server/src` 行数 ≤ 2,500。
4. `cargo clippy --workspace --all-targets` 零 warning；`cargo test --workspace` 全绿。
5. 二进制体积：`strixmaid` 不增长超过 2%（搬家不该改变产物）；agent 本方案内不变。

## 7. 未决问题

1. **agent 体积。** 11 号方案让 agent 装载完整 node 之后，原 `05-agent.md`（已删，见 git 历史）定的「Agent 静态二进制 < 8 MiB」必然破。新的上限在 11 号方案里定，本方案不涉及。
2. **`ws/tests.rs` 怎么拆。** 641 行里既有 hub 的单测也有 agent 协议的单测，实施时按被测对象分，拆不开的留在 server 并在报告里说明。
3. **`Node::start` / `Node::shutdown` 未抽取**（§3.3 的实施偏离）。这是 Agent 装载完整 node 的前提，也是 `11-multi-host.md` 的地基之一，得在 11 号动工前补上。

4. **`ServiceApp::prepare` 的返回类型绑死在 `Config` 上。** Agent 模式读的是 `agent.toml`，形状与 `Config` 不同，现在的做法是构造一个**只填了 `data_dir`、仅供定位日志落点**的 `Config`，真实配置在 `serve` 里再读一次（见 `crates/strixmaid/src/winsvc_app.rs`）。能跑但别扭，干净的做法是让 `prepare` 返回一个宿主自定义的关联类型。

5. **`debug/` 的归属。** 它渲染的是 node 的内部状态，按理归 node；但它同时依赖前端资源的存在与否。倾向归 node，实施时若发现耦合到 `embed` 则留在 server。
