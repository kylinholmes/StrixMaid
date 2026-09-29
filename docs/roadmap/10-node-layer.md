# 10 node 层：已完成的架构拆分

## 1. 目标

把单机 API 与宿主传输分离，业务能力不绑定 HTTP 监听端口。

## 2. 当前边界

`strixmaid-core` 是能力库；`strixmaid-node` 组装认证、API、审计、WebSocket 和运行时；
`strixmaid` 持有 UI、进程入口、节点目录与 Agent 指标传输。
helper 只依赖 types，不依赖 core 或 node。

## 3. 设计

### 3.1 crate 边界

core 不依赖 axum / tower；HTTP 错误转换和路由归 node，DTO 归 types。

### 3.2 Router 与运行时

`Node::start` 构造资源，`Node::router` 返回 Router，`Node::shutdown` 负责清理。
宿主注入远程快照和额外受保护路由；TCP、任意字节流、前端嵌入归宿主。

## 4. 完成状态

node 抽取、启动与关停编排、监听失败清理和 Windows 日志参数解耦均已实现。
Agent 当前仍自行启动采集与存储；装载完整 node 属于暂停中的
[多主机方案](11-multi-host.md)，不能据此宣称已支持远程管理。

## 5. 关键约束

额外路由必须与单机路由一起套鉴权层。Windows 服务日志配置必须区分 serve 与 agent。
默认 Rust 测试应先 build，以便真实 helper / worker 测试使用当前二进制。

## 6. 验证

API 契约与行为均需验证；OpenAPI 未变化不能证明鉴权没被绕过。
任意流上的 HTTP / WS 验证在 `strixmaid-node/src/stream.rs`。
