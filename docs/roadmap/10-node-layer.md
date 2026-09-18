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

### 3.3 `Node` 的构造与生命周期

> **分两次做（2026-09-18）**：第一次只搬路由与状态，`serve_with` 的启动编排留在
> `strixmaid`——它与监听器、agent 注册表绑得紧，抽它要连关停路径一起动，而那会让
> OpenAPI diff 这个硬门槛失去意义（门槛证明的是「搬对了」，一旦同时改了编排，
> diff 为空就只能证明「端点没变」）。第二次（本节）在搬家已被证明正确之后才动编排，
> 门槛因此仍然有效：改完 OpenAPI 依旧逐字节相同。

原先 `serve_with` 里按顺序做的事——开库、起 helper 会话管理器、装终端注册表、
接审计观察者、起 metrics engine、探测能力、注册 WS 频道——全部搬进：

```rust
impl Node {
    pub async fn start(
        config: Config,
        reporter: &dyn StartupReporter,
        remotes: Option<Arc<dyn RemoteSnapshots>>,
    ) -> Result<Node>;
    pub fn router(&self, extra_protected: Option<OpenApiRouter<()>>) -> axum::Router;
    pub async fn shutdown(&self, kind: ShutdownKind);
}
```

三个签名上的决定，都是被实现逼出来的：

- **`router` 与 `start` 分开。** `/nodes` 的状态要用 node 自己的 store 与 auth，
  而那两样在 `start` 里才诞生。宿主追加的路由因此只能在 `start` 之后给。
- **`router` 取 `&self`。** Router 里的状态全是句柄的克隆，同一个 `Node` 要能给出
  多份 Router——`11-multi-host.md` 的 `/` 与 `/nodes/local` 就是两份。
- **`start` 不调 `reporter.ready()`。** 「就绪」的判据是传输能收请求了，而传输是
  宿主的事。node 自己报就是撒谎，SCM 会据此认为服务已经可用。

`serve_with` 因此从 290 行缩到 110 行，剩下的全是 Server 独有的：绑端口、挂
`/ws/agent` 与前端、axum 的两段式关停。

`StartupReporter` 与 `ShutdownKind` 一并从 `strixmaid-server/src/main.rs`（现为 `pub(crate)`）移入 node 并转为 `pub`。它们本来就是为 SCM 的递增 checkpoint 设计的接口，而 SCM 托管也要搬到 node——两者必须在同一个 crate 里才不用互相 re-export。

原计划里还要拆一个 `NodeConfig`（`Config` 中与 HTTP 无关的那部分，`listen`/`tls`/前端项留给 server）。**没拆，也不打算拆**：`Config` 本来就在 `strixmaid-core`，node 直接用它；`Node` 只读自己关心的字段，多出来的 `listen` 它根本不看。拆开只是多一个结构、多一次转换，换不来编译器守住任何东西——真正守住边界的是依赖表（core 里没有 axum）。

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

原计划（含 `ws/tests.rs` 留下的部分）把验收定在 2,500 行。实际（2026-09-18，两次提交之后）：

```
crates/strixmaid/src   3,492 行
├─ main.rs              453   进程入口、绑端口、axum 两段式关停
├─ cli.rs               498   子命令与全局参数
├─ embed.rs             238   前端资源与 SPA 回退
├─ app.rs                40   把 node 的 Router 与前端、/ws/agent merge 起来
├─ routes_nodes.rs      295   节点目录
├─ ws_agent.rs          616   Agent 拨进来的那条 WS
├─ winsvc_app.rs        258   两种服务身份（Windows）
└─ agent/             1,094   Agent 模式：配置、拨号客户端、主循环
```

比 2,500 多出来的近一千行是 `agent/`——本方案立项时它还是**另一个 crate**（`strixmaid-agent`），2026-09-17 的二进制合并把它并了进来。除去这部分是 2,398 行，在线内。

`app.rs::build` 从「组装全部东西」缩成 40 行：node 的 `Router` 进来，merge 上 `/ws/agent` 与前端回退，套两层。

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
3. ~~**`Node::start` / `Node::shutdown` 未抽取**（§3.3 的实施偏离）。~~ **已做（2026-09-18，第二次提交）**，见 §3.3。`11-multi-host.md` 的这块地基铺好了。

4. ~~**监听失败时不走 `node.shutdown()`。**~~ **已修（2026-09-18，第三次提交）。** `bind` 失败时先 `node.shutdown(Graceful)` 再返回。回归测试断言的是 `-wal` 不残留——那是「有没有干净关库」唯一在外部看得见的痕迹。

5. ~~**`ServiceApp::prepare` 的返回类型绑死在 `Config` 上。**~~ **已修（同上）。** 根因不在 `prepare`，在 `winsvc::logging` 上：它要一整个 `Config`，却只用 `data_dir` 与日志级别两样。把 `logging::init` 改成收 `(&Path, EnvFilter)` 之后，`prepare` 只需返回日志路径，`serve` 各读各的配置，伪造的 `Config` 随之消失。

   顺带查出一个**真 bug**：原先 `log_target()` 不分模式，一律去读 server 的配置。于是 `service install --mode agent --config agent.toml` 打印的「日志去哪了」是 `C:\ProgramData\StrixMaid\logs`——把 agent.toml 当 server 配置解析失败后静默回落的默认值，连命令行上的 `--data-dir` 都一并吞掉。照着那个路径去看，看到的是空目录。已用变异验证：把 `log_target` 换回旧写法，新加的两条测试立刻转红。

6. ~~**`debug/` 的归属。**~~ **已定**：归 node（`crates/strixmaid-node/src/debug/`）。担心的「耦合到 `embed`」没有发生——它渲染的全是 node 自己的状态。
