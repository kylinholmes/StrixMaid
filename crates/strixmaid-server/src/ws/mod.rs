//! WebSocket 控制面（design.md §9.2）：`GET /ws` 升级后走 envelope 协议，多频道复用。
//!
//! - [`hub::Hub`]：连接管理、每连接的订阅集合、`sub` / `unsub` / `req` / `ping` 分发；
//! - [`hub::ChannelSource`]：频道源抽象，其它模块（`logs.follow` / `services.changed`）
//!   实现它并 [`hub::Hub::register`] 进来即可；
//! - [`channels`]：本模块自带的频道实现，目前只有 `metrics.live`；
//! - [`handler`]：axum 升级处理器与 [`router`]。
//!
//! 认证中间件由其它模块统一接线，本模块不做鉴权。

pub mod agent;
pub mod channels;
pub mod handler;
pub mod hub;
pub mod terminal;

pub use handler::router;
pub use hub::Hub;

#[cfg(test)]
mod tests;
