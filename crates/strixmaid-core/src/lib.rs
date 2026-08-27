//! StrixMaid AgentCore —— 全部业务逻辑所在地。
//!
//! Server 与 Agent 都只是本 crate 的宿主。参见 `docs/design.md` §3。
//!
//! 模块分层：
//! - [`providers`]：对系统能力的封装（服务 / 日志 / 进程 / 主机信息），每个都能 `probe()` 自报可用性
//! - [`metrics`]：常驻采集、内存环形缓冲、分层聚合调度
//! - [`store`]：SQLite 持久化（时序 / 会话 / 审计 / 设置）
//! - [`session`]：浏览器会话与 node_session、worker 生命周期、提权状态
//! - [`worker`]：`strixmaid worker` 模式——以登录用户身份运行、经 socketpair 接受 RPC
//! - [`capability`]：两层能力探测（system / user）
//! - [`platform`]：平台原语——某平台上多个模块共用的底层系统调用封装（目前只有 macOS 需要）
//! - [`config`]：四层优先级配置

pub mod capability;
pub mod config;
pub mod metrics;
pub mod platform;
pub mod providers;
pub mod session;
pub mod store;
pub mod terminal;
pub mod worker;
