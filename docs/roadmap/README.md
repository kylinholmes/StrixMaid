# 后续工作方案索引

本目录同时保存已落地的设计与尚未实现的方案，不能把每个文件都当成待执行任务。
当前实现、检查命令与工程约束见 [项目入口](../README.md)。

| 文件 | 当前用途 |
|---|---|
| `03-terminal.md` | 已落地的终端协议与生命周期约束 |
| `06-packaging.md` | 当前打包边界、平台差异与体积要求 |
| `07-verification.md` | 目标环境验收清单与历史结果；不是本次检查结果 |
| `08-metrics-and-panel.md` | 已落地的指标语义与面板设计，附设计样稿 |
| `09-ci-verification.md` | 自动化覆盖与仍待补齐的验证 |
| `10-node-layer.md` | 已完成的模块边界拆分 |
| `11-multi-host.md` | 暂停中的远程管理方案，只有任意流服务的地基已实现 |
| `12-workspace.md` | 已完成的工作区设计，cwd 联动已移除 |
| `13-files.md` | A 预览/下载已实现，Edge 与 Linux 真 worker 验收通过；后续 B–D 为写操作 |

已删除的 01/02/04/05 以及旧交接、旧实施清单从 git 历史查阅；代码里的历史编号
只是设计出处，不表示仍有待办。保留的 09-21 交接记录文件传输方案的历史决策过程。

## 下一批工作（2026-09-21 定）

八个模块，按项目负责人给出的顺序：**文件写操作 + 预览 → VM 管理 → 容器管理 →
账户管理 → 网络 → 存储只读 → 定时任务编辑 → 一键诊断**。多主机（`11`）、
软件更新、Prometheus 导出**不在**这一轮。

排序上的硬依赖、"跨平台稀释"这条判据，以及第一项（文件模块）的历史决策，
记在 [`../HANDOFF-2026-09-21.md`](../HANDOFF-2026-09-21.md) §4–§5。

2026-09-29 已对照当前源码补成 [13 文件模块方案](13-files.md)，包含 A–D 边界、
专用字节流、浏览器文件 Cookie、Range 与 A1–A4 验收顺序。负责人已明确批准该方案
及并行子任务分派；A1–A3 已实现，A4 的 Edge 与 Linux 真 worker 验收结果已回写。
后续从 B 的改名、删除、新建目录、移动继续，具体退出条件见 13 号方案。

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
