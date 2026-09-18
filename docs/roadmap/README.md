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

已完成并删除的方案（内容已落进代码与注释，需要考据走 git 历史）：`01-worker-execution`、`02-audit`、`04-files-and-ws-channels`、`05-agent`。其中 `05-agent` 描述的「Agent 只读」形态已被 `11-multi-host.md` 取代。

## 实施约定

以下约定在 Phase 0–3 中形成，后续工作沿用。

1. **先读 `../design.md`。** 各方案文件只引用其章节号，不重复其内容。方案与设计冲突时以设计为准，并在方案文件中记录冲突。
2. **依赖只用 `cargo add` 添加**，不手写 `Cargo.toml` 的 `[dependencies]`。多个并行任务需要新依赖时，由协调者统一添加后再分派，避免并发改写 `Cargo.lock`。
3. **共享接缝文件**由协调者统一修改：`crates/strixmaid-core/src/lib.rs`、`crates/strixmaid-server/src/{main,app,state}.rs`、`crates/strixmaid-server/src/routes/mod.rs`、各 `Cargo.toml`。并行任务在各自目录内交付，并在报告中给出接线所需的代码行。
4. **路由模块的形态**：`pub fn router(state: Arc<XxxState>) -> OpenApiRouter<()>`，自带状态、返回无状态 router；处理器带 `#[utoipa::path]`。不为未实现的端点建空壳路由。
5. **DTO 归 `strixmaid-types`**，路由文件内临时定义的类型（如 `UnitDeps`、`PartialSystemInfo`）在稳定后迁入。
6. **凭据约束**（`design.md` §5.3）：明文密码只以 `Zeroizing<String>` 存在，不进日志（含 debug 级）、不入库、不实现 `Serialize` / `Clone`。
7. **质量门**：`cargo check --workspace --all-targets`、`cargo clippy --workspace --all-targets` 零 warning；`cargo test --workspace` 全绿；依赖真实系统服务的测试用运行期探测跳过而非 `#[ignore]`，但实施者须在本机实际运行过。
8. **报告格式**：交付文件清单、实测数据、接线代码行、对设计做的补充假设（逐条）。假设部分最重要，协调者据此更新 `design.md`。
9. 注释与文档用中文，书面语。
