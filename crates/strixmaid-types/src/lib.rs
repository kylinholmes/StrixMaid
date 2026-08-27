//! StrixMaid 共享类型：DTO / WS envelope / 错误。
//!
//! 本 crate 只依赖 `serde` / `serde_json` / `thiserror` / `utoipa`，不含任何业务逻辑，
//! 也不依赖 tokio、axum、sqlx。参见 `docs/design.md` §3、§9。
//!
//! # 全局约定
//!
//! 这些约定对本 crate 内**所有**类型生效，各字段文档不再重复说明：
//!
//! - **时间戳**一律为 `i64` 的 unix 秒（UTC）。单一主时间戳的字段名为 `ts`；同一结构里的
//!   其它绝对时刻以 `_ts` 结尾（如 `authed_ts`）；区间端点用 `from` / `to` / `since` /
//!   `until`。本 crate **不引入 chrono / jiff**，格式化交给前端。
//!   唯一的例外是 [`log::LogEntry`]：它额外带一个 `us` 字段表示秒内微秒偏移，
//!   因为同一秒内几十条日志是常态，只有秒精度时间列会糊成一片。
//! - **字节数**为 `u64`，字段名以 `_bytes` 结尾。
//! - **时长**为秒（`_secs`，`u64`）或纳秒（`_nsec`，`u64`），单位写进字段名。
//! - **百分比**为 `f64`，取值范围 `0.0..=100.0`（多核 CPU 占用可超过 100.0，见具体字段），
//!   字段名以 `_percent` 结尾。
//! - **所有枚举以 `snake_case` 序列化**（少数字段为兼容 design.md 里写死的大写字面量而额外
//!   接受 `alias`，见 [`process::SignalName`]）。
//! - `Option<T>` 对应 JSON 中可为 `null` 或缺省的字段，「为什么可能没有」写在字段文档里。
//! - 每个对外类型都实现 [`utoipa::ToSchema`]，查询参数类型额外实现 [`utoipa::IntoParams`]，
//!   保证 OpenAPI 能完整导出——这是 P0 硬要求。
//!
//! # 安全约定
//!
//! 明文凭据只出现在 [`auth::AuthRespondReq`] / [`auth::PromptResponse`] 中。三重保护全部由
//! **类型系统**强制，而不是靠 code review 自觉：字段类型是 `zeroize::Zeroizing<String>`
//! （drop 时自动擦除）、**故意不实现 `Serialize` / `Clone`**、`Debug` 手写脱敏。
//! 详见 `docs/design.md` §5.3 与 [`auth`] 模块文档。

pub mod audit;
pub mod auth;
pub mod capability;
pub mod error;
pub mod file;
pub mod ipc;
pub mod log;
pub mod metrics;
pub mod process;
pub mod rpc;
pub mod service;
pub mod system;
pub mod terminal;
pub mod ws;

#[cfg(test)]
mod openapi_smoke_test;

// 错误类型在每一层都要用，直接提到 crate 根。
pub use error::{ApiError, ApiResult, ErrorCode};
