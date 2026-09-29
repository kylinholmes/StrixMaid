# 代码质量与架构审查及修复（2026-09-27）

后续修复与最新验证见 [09-29 记录](2026-09-29-follow-up.md)，本页保留当日结果。

初次审查基于 HEAD `5ef964e`，下述修复已落到当前工作区，尚未提交。
保留原有打包、容器验证与 VM 文件改动。未部署服务、修改系统配置或执行真实登录。

## 1. 结论

Rust crate 边界基本合理：types 定契约，core 提供系统能力，node 提供单机 API，
主程序负责传输与 UI，helper 负责系统认证和身份切换。保留现有架构。
本次修复集中于会话资源归属、流式连接的等待时限和昂贵工作的容量限制，
同时拆分前端性能面板、按页面加载，并清理无用依赖和失效文档引用。

## 2. 已修复的运行问题

### P1：会话切换复用上一用户的缓存和工作区

原问题已复现：Alice 锁定后 Bob 登录仍保留旧目录、终端标签；同路径缩略图
直接返回 Alice 的 Blob URL，没有再请求后端。

新增 `web/src/session/lifecycle.ts` 统一会话生命周期。切换凭据时取消在途请求，
清空 QueryClient、工作区、系统图标与缩略图缓存，撤销 Blob URL。
应用通过会话 epoch 重建页面；异步恢复、认证、终端创建、图片读取与指标轮询
检查原会话是否仍有效，防止旧响应重新写入新会话。

回归测试覆盖跨用户缓存、旧图片响应晚到、旧 restore 晚到和真实 API 中间件的取消信号。

### P2：锁定界面等待注销 HTTP 完成

原问题已复现：注销请求挂起时，页面保持 open 且旧 token 仍可使用。
现在同步清 token、身份和本地资源并锁定界面；随后用事先捕获的旧 token
撤销服务端会话，等待最多 5 秒。迟到的注销响应不会影响期间建立的新会话。
测试覆盖挂起注销和注销期间重新登录。

### P2：终端 WebSocket 慢发送无法及时回收

`crates/strixmaid-node/src/ws/terminal.rs` 对 WebSocket send、PTY write 和
控制操作设置 10 秒时限；结束握手最多 1 秒。先释放 attachment，发送停滞时
直接丢弃连接，避免再次等待已取消发送的缓冲刷新。

新增 Unix 集成测试使用真实 IPC、worker dispatcher、PTY、TCP 和 WebSocket。
客户端触发大量输出后停止读取，验证服务任务在时限内结束且终端解除附着。
测试注入 100 ms 写入时限以缩短运行时间；本机通过。尚未做真实登出配合慢连接的浏览器压测。

### P2：并行缩略图解码缺乏总量限制

每个 worker 在进入阻塞线程池前最多接纳两个缩略图任务，超额立即返回 503。
配额由实际阻塞任务持有，取消 HTTP/调用任务不会提前释放配额。
源文件上限从 512 MiB 收紧至 64 MiB，读取过程中也检查上限；Rust 解码器
设置 256 MiB 分配预算，并保留 8000 万像素检查。前端用两条请求队列加载缩略图。

测试覆盖并发饱和、取消调用后配额继续占用、超大源文件拒绝及任务结束后恢复。
这是解码并发与输入预算，不是 worker 总 RSS 的硬上限；系统 HEIC 解码器不受
Rust image 的分配预算控制。

### P2：release 的 panic 策略与 catch_unwind 冲突

release 从 `panic = "abort"` 改为 `panic = "unwind"`，使缩略图任务中的
`catch_unwind` 与发布配置一致。回归测试注入 Rust panic，验证转换为 API 错误、
释放配额，且后续任务可成功。OOM 和原生库终止进程不在恢复保证内。
没有构造恶意图片触发实际解码器 panic，也没有对发布二进制做故障注入。

### P2：日志暂停跟随后 pending 缓冲无限增长

展示列表和 pending 各保留最多 5,000 条。pending 溢出后显示提示，返回最新时
仅展示连续保留的最新记录，并从其末尾游标恢复向旧记录分页，避免拼接出隐形缺口。
历史分页同样限制展示容量。新增测试覆盖连续 100 批日志和单批超过容量的情况。

## 3. 架构与代码结构

| 项目 | 处理结果 |
|---|---|
| 前端资源没有统一会话生命周期 | 新增轻量生命周期模块，由各资源注册清理；API 无需反向依赖 Zustand store |
| 性能面板 `sections.tsx` 1,925 行 | 拆为共享图表、布局、资源列表、详情和 CPU/内存/磁盘/网络/GPU 模块，原文件仅保留导出 |
| App 静态加载所有页面 | 页面使用 lazy / Suspense；入口 JS 从 946.35 kB 降至 324.16 kB，gzip 从 284.75 kB 降至 103.44 kB；这是入口体积，不是总下载量 |
| 无引用的直接依赖 | 移除主程序 tower、utoipa-scalar，node 的 rand，helper 的 anyhow；同步 Cargo.lock，本机构建通过 |
| rustdoc 89 条 warning | 修正失效、平台条件及私有项链接，严格文档构建零警告，没有全局关闭 lint |
| core / node / helper 边界 | 保留现有依赖方向，不增加无必要的 crate |

仍可后续改善：terminal、config、D-Bus 大模块按职责拆分；跨平台 capability 命名
进行兼容迁移。总行数包含大量测试，不能仅据此判为架构缺陷。Agent 当前只读采集的
边界保持不变，完整远程管理恢复时再统一 node 生命周期。

## 4. 验证

环境：macOS，Rust 1.98.1，Bun 1.3.14。Cargo 使用 `--locked --offline`。
本机 sccache 无法启动，以 `RUSTC_WRAPPER=` 禁用缓存；真实 IPC/网络测试在获授权后
于沙箱外运行，沙箱权限错误不计为产品失败。

| 检查 | 结果 |
|---|---|
| Clippy，workspace / all-targets，-D warnings | 通过 |
| Clippy，无 UI 变体，-D warnings | 通过 |
| cargo build --workspace | 通过 |
| cargo test --workspace | 544 个单元/集成测试通过、2 个 ignored；另有 3 个 doctest 通过、4 个 ignored |
| 前端 Biome / TypeScript | 通过 |
| Vitest | 20 文件、250 测试通过 |
| 前端构建 | 通过，无大包或混合导入警告 |
| RUSTDOCFLAGS='-D warnings' cargo doc --workspace --no-deps | 通过，零警告 |
| release workspace 构建 | 通过；主程序 --version 启动检查通过 |

本机 release 主程序 13,243,744 字节，helper 453,568 字节。macOS 产物不用于替代 Linux musl 体积验收。

两个 ignored 用例分别为实际 PAM 错误密码检查与手工导出 OpenAPI。
部分系统能力测试在探测环境后提前返回，通过数字不代表所有平台能力均覆盖。

未执行：浏览器真实账户切换、Linux root / D-Bus 压测、Windows 测试、安装卸载、
长时 RSS / 保留期验证、release 崩溃与资源耗尽实验。跨平台打包与大小门槛仍需 CI 验证。

## 5. 文档清理范围

删除三个旧交接、过时 gap-analysis 和已完成的工作区 B 期逐步计划；
终端、打包、node 文档改为当前实现说明，清理工作区的旧现状和废弃 cwd 方案。
设计基线区分已实现 Agent 指标汇聚与暂停中的远程管理目标，移除独立 Agent
二进制、前端未选型、根页面跳转 debug 等过期描述。

保留平台设计、D-Bus 事故、指标语义、样稿及尚未定案的文件方案。
统一入口为 [项目现状](../README.md)，历史设计可从 git 查阅。
