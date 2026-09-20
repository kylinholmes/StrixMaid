# 后续工作方案索引

本目录下每个文件描述一项独立的工作，供后续实施者（人或 AI）直接执行。阅读顺序与依赖关系见 `../gap-analysis.md` §6。

## 文件

| 文件 | 内容 | 前置 | 规模 |
|---|---|---|---|
| `03-terminal.md` | PTY 终端：创建、附着、会话保持、回看缓冲、调整尺寸、超时 | 无 | 大 |
| `06-packaging.md` | musl 静态构建、helper 的 glibc 构建、`strixmaid.service`、pam.d 安装、`ui` feature、发布产物；macOS 与 Windows 的补充 | 无 | 中 |
| `07-verification.md` | root 环境、浏览器、长时间运行、release 性能的验证清单与预期结果 | 06 | — |
| `08-metrics-and-panel.md` | 采集项裁剪与性能面板重做；配套可交互样稿 `08-metrics-and-panel.mockup.html` | 无 | 大 |
| `09-ci-verification.md` | 把 07 能自动化的部分搬进 CI 的分期方案 | 06、07 | 中 |
| `10-node-layer.md` | 抽出 `strixmaid-node`：把 server 里的业务逻辑变成可被两个宿主装载的 AgentCore。纯搬家，行为零变化 | 无 | 大 |
| `11-multi-host.md` | 多主机：yamux 管道、`/nodes/<id>` 路由、SSH 远程推装、B 侧会话与审计 | 10 | 大 |
| `12-workspace.md` | 工作区：终端与文件合成一块；含一个必须先修的终端退出信号 bug。分 A–D 四期 | 无 | 大 |

`12-workspace.md` 的 A–D 四期**已全部合并**（cwd 双向联动那条于 2026-09-20 整条移除，
见其 §4.4）。剩下的缺口与后续八个模块的路线见
[`../HANDOFF-2026-09-21.md`](../HANDOFF-2026-09-21.md)。

已完成并删除的方案（内容已落进代码与注释，需要考据走 git 历史）：`01-worker-execution`、`02-audit`、`04-files-and-ws-channels`、`05-agent`。其中 `05-agent` 描述的「Agent 只读」形态已被 `11-multi-host.md` 取代。

代码注释里仍有约一百处 `roadmap/01 §4.3` 这样的引用指向这四个文件——**那是刻意保留的**：它们标注的是某段代码当初为什么这么写，改写成别的出处只会把一条准确的线索换成一条含糊的。要查就按上面这句走 git 历史。

## 下一批工作（2026-09-21 定）

八个模块，按项目负责人给出的顺序：**文件写操作 + 预览 → VM 管理 → 容器管理 →
账户管理 → 网络 → 存储只读 → 定时任务编辑 → 一键诊断**。多主机（`11`）、
软件更新、Prometheus 导出**不在**这一轮。

排序上的硬依赖、"跨平台稀释"这条判据，以及第一项（文件模块）已定的四条决策与
**尚未获批的字节通道方案**，全部记在
[`../HANDOFF-2026-09-21.md`](../HANDOFF-2026-09-21.md) §4–§5。
接手时先读它再动手——那份设计停在「方案已提出、等负责人批准」，**批准前不要写实现代码**。

## 实施约定

以下约定在 Phase 0–3 中形成，后续工作沿用。

1. **先读 `../design.md`。** 各方案文件只引用其章节号，不重复其内容。方案与设计冲突时以设计为准，并在方案文件中记录冲突。
2. **依赖只用 `cargo add` 添加**，不手写 `Cargo.toml` 的 `[dependencies]`。多个并行任务需要新依赖时，由协调者统一添加后再分派，避免并发改写 `Cargo.lock`。
3. **共享接缝文件**由协调者统一修改：`crates/strixmaid-core/src/lib.rs`、`crates/strixmaid-node/src/{lib,node,app,state}.rs`、`crates/strixmaid-node/src/routes/mod.rs`、`crates/strixmaid/src/{main,app}.rs`、各 `Cargo.toml`。并行任务在各自目录内交付，并在报告中给出接线所需的代码行。
4. **路由模块的形态**：`pub fn router(state: Arc<XxxState>) -> OpenApiRouter<()>`，自带状态、返回无状态 router；处理器带 `#[utoipa::path]`。不为未实现的端点建空壳路由。
5. **DTO 归 `strixmaid-types`**，路由文件内临时定义的类型（如 `UnitDeps`、`PartialSystemInfo`）在稳定后迁入。
6. **凭据约束**（`design.md` §5.3）：明文密码只以 `Zeroizing<String>` 存在，不进日志（含 debug 级）、不入库、不实现 `Serialize` / `Clone`。
7. **质量门**：`cargo check --workspace --all-targets`、`cargo clippy --workspace --all-targets` 零 warning；`cargo test --workspace` 全绿；依赖真实系统服务的测试用运行期探测跳过而非 `#[ignore]`，但实施者须在本机实际运行过。
8. **报告格式**：交付文件清单、实测数据、接线代码行、对设计做的补充假设（逐条）。假设部分最重要，协调者据此更新 `design.md`。
9. 注释与文档用中文，书面语。
