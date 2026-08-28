# 04 文件管理壳与两个 WS 频道

> **实施状态（2026-08-28）**：A、B 两项均已完成。
>
> 与方案的三处偏离，均已在代码注释中说明理由：
>
> - **A.3 的 `allowed_roots` 下发**：未加 `ToWorker::Configure` 帧，选择随调用经
>   `FsParams` 传入（方案允许的另一形态）。为一个策略值给 IPC 协议加一种帧、给
>   分发表加一份可变状态，代价大于每次多传几十字节——文件浏览是人手速驱动的
>   低频调用，且 `allowed_roots` 不是安全边界（裁决在文件权限）。
> - **A.3 的大小上限**：读全程带 `take(上限+1)` 双重兜底（procfs 文件 stat 报 0，
>   普通文件也可能在 stat 之后变大），`FileContent.truncated` 恒为 `false` 并在
>   DTO 文档里说明保留原因；新增 `lossy` 字段、`DirListing.skipped` 计数。
> - **B.3 的参数校验**：边界校验与缺省回填在主进程侧（`channels/processes_live.rs`）
>   ——只有那里能带着订阅方的 `id` 回 `err` 帧；worker 对收到的值只用不复核。
>
> B.2 的 `system.health` 由主进程每 30 秒重算，failed units 计数并入
> `unit.failed` 并把 `systemd` 从 `skipped` 摘除；只有条目集合
> （id + target + severity）变化才广播。`/debug` 页新增文件面板，
> WS 面板的频道列表已含两个新频道。集成级验收（A.4-4 的 `/etc/shadow` 403、
> 浏览器实测）与其余 root 项一并归入 `07-verification.md` 的环境验证。

本文件包含两项互不依赖的小工作。

## A. 文件管理壳

### A.1 目标

`design.md` §9.1 的两个只读端点，在 user worker 内执行，权限由文件系统裁决。完整的文件管理（上传、编辑、复制移动）由项目负责人另行打磨，本方案只建 provider 与只读路由，接口形态为后续扩展留位。

### A.2 现状

`crates/strixmaid-types/src/file.rs` 已有 `FileKind`、`FilePathQuery { path }`、`DirEntryInfo`、`DirListing`、`FileContent`。无 provider、无路由。`design.md` Q21 决定 `allowed_roots` 配置项默认 `["/"]`。

### A.3 方案

`crates/strixmaid-core/src/providers/fs/mod.rs`：`FsProvider`，实现 `Provider`（id `"fs"`，`probe` 恒 `Available`），方法：

| 方法 | 行为 |
|---|---|
| `list(path) -> DirListing` | 路径规范化后校验在 `allowed_roots` 内；`read_dir` 每项 `lstat`；符号链接填 `target`；`user` / `group` 由 uid / gid 查 `/etc/passwd`、`/etc/group`（缓存 60 s；NSS 用户查不到时为 `None`，见 `design.md` §10 helper 职责表）；按名称排序，目录在前 |
| `read(path) -> FileContent` | 大小上限 5 MiB（超出返回 400 `invalid_request`，detail 说明上限与实际大小；`ErrorCode` 无 413 对应项，不新增）；前 8 KiB 含 NUL 字节视为二进制，返回 400；内容按 UTF-8 解码，无效序列用 U+FFFD 替换并在响应中标 `lossy: true`（`FileContent` 需增此字段） |

路径规范化：拒绝空路径与相对路径；`..` 逐段解析但**不跟随符号链接**做规范化（`realpath` 会把 `/data/link` 解析到 `allowed_roots` 之外，而用户看到的路径是 `/data/link`）。`allowed_roots` 校验对规范化后的字面路径进行。

RPC：`fs.list { path }`、`fs.read { path }`，读操作，走 user worker。`allowed_roots` 从主进程配置经 `Call.params` 传入，或 worker 启动时由主进程下发一次（`ToWorker::Configure { fs_allowed_roots }`，推荐后者，避免每次调用重复传）。

路由 `crates/strixmaid-server/src/routes/files.rs`：`GET /files?path=`、`GET /files/content?path=`。

配置：`files.allowed_roots: Vec<PathBuf>`，默认 `["/"]`，校验每项为绝对路径。

### A.4 测试

1. 规范化：`/a/../b` → `/b`；`/a/./b` → `/a/b`；`a/b` 拒绝；`/` 保持；`/data/../..` → `/`。
2. `allowed_roots = ["/home", "/var/log"]` 时 `/etc` 拒绝、`/home/x` 通过、`/home` 本身通过。
3. 本机：`list("/proc/self")` 不 panic（`stat` 失败的项跳过并计数）；`read("/etc/hostname")` 正确；`read("/bin/ls")` 返回 400；`read` 一个 6 MiB 文件返回 400。
4. 集成：登录后 `GET /files/content?path=/etc/shadow` 返回 403 `permission_denied`（worker 为普通用户）。

### A.5 验收

上述测试通过；`/debug` 页新增文件面板可浏览目录树与查看文本文件。

## B. WS 频道 `system.health` 与 `processes.live`

### B.1 现状

`crates/strixmaid-server/src/ws/hub.rs` 的 `ChannelSource` 接口与 `Hub::register` 就绪；`metrics.live`、`services.changed`、`logs.follow` 已注册。`strixmaid_types::ws::WsChannel` 已含 `SystemHealth`、`ProcessesLive` 变体。

### B.2 `system.health`

- 主进程内每 30 秒调用 `HostProvider::health()`，与上一次结果按 `HealthItem.id + target` 做集合比较；有增删或 `severity` 变化时广播整份 `HealthReport`。
- 订阅时立即推送当前报告。
- 实现：`crates/strixmaid-server/src/ws/channels/system_health.rs`，内部 `tokio::sync::watch<Arc<HealthReport>>`；`subscribe` 返回 `WatchStream` 去重后的流。
- 后续 `services.changed` 中 `failed` 数变化也应触发一次健康重算（`HealthReport.skipped` 目前标注 systemd 项由 service provider 提供）。本方案内先把 failed units 数并入：`system_health` 源持有 `Arc<dyn ServiceProvider>`，健康检查时 `list_units { state: failed }` 计数，生成 `unit.failed` 条目。

### B.3 `processes.live`

- 依赖 `01-worker-execution.md` §4.4 的流式 RPC 与 `SubscribeContext`。
- 订阅参数 `{ sort, order, limit, interval_secs }`，`interval_secs` 允许 2–10，默认 3，`limit` 上限 500。
- worker 内 `register_stream("proc.live", ...)`：按间隔调用 `ProcProvider::list_blocking`（在 `spawn_blocking` 中），取前 `limit` 项产帧。
- 主进程 `channels/processes_live.rs`：`subscribe(params, ctx)` → `WorkerHandle::subscribe("proc.live", params)`。
- 帧 `d` 为 `Vec<ProcessSummary>`。

### B.4 测试

1. `system.health`：伪造两份 `HealthReport`，只有 `detail` 文本不同时不广播，`severity` 变化时广播。
2. `processes.live`：订阅后 `interval_secs` 内收到首帧且长度 ≤ `limit`；`unsub` 后 worker 侧任务在下一个周期前退出（观察 `Unsubscribe` 帧）。
3. 参数越界（`interval_secs = 1`、`limit = 10000`）返回 `err` 帧，`code = invalid_request`。

### B.5 涉及文件

`ws/channels/{system_health,processes_live}.rs`、`ws/channels/mod.rs`、`core/src/worker/providers.rs`（注册 `proc.live`）、`main.rs::serve`（注册两个源）。
