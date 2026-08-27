# 05 Agent 与多节点汇聚（只读）

## 1. 目标

`design.md` §11 的 MVP 形态：`strixmaid-agent` 在远程节点常驻采集并本地存储，主动连接 Server 推送指标；Server 汇聚各节点数据；断连后按时间戳补发。Agent 不接受管理操作。

本项不阻塞其它工作，优先级最低。

## 2. 现状

- `crates/strixmaid-agent/src/main.rs` 为 3 行占位；依赖已加（tokio、clap、tracing、strixmaid-core、strixmaid-types）。
- `MetricsEngine`、`Store`（含 `insert_tier`、`query`）、分层聚合均在 core，Agent 可直接复用。
- `nodes` 表已有（`id, name, kind, token_hash, last_seen, created_at`），`SessionManager::new` 启动时 upsert `local`。
- `series.node` 列存在，主进程写入时恒为 `local`。
- HTTP 路径不带节点标识（`design.md` §11），多节点 UI 属 P2。

## 3. 方案

### 3.1 Agent 进程

```
strixmaid-agent [--config /etc/strixmaid/agent.toml]
```

配置（独立于 Server 的 `Config`，新建 `AgentConfig`）：

| 字段 | 说明 |
|---|---|
| `server_url` | `wss://host:9700`，必填 |
| `node_id` | 稳定标识；缺省取 `/etc/machine-id` |
| `node_name` | 显示名，缺省主机名 |
| `token` | 预共享 token；也可 `token_file` |
| `data_dir` | 本地 SQLite，默认 `/var/lib/strixmaid-agent` |
| `metrics.*` | 同 Server 的 `MetricsConfig` |
| `tls.insecure` | 开发用，默认 false |

启动：`Store::open_with` → `MetricsEngine::start(cfg, Some(store))`（与 Server 完全相同的采集、环形缓冲与分层聚合）→ 连接循环。

### 3.2 连接与协议

Agent 主动连 `WS /ws/agent`。鉴权：`Sec-WebSocket-Protocol: bearer, <token>`，Server 端用独立中间件按 `nodes.token_hash` 校验（不是 PAM 会话）。协议复用 `WsEnvelope`，频道：

| 方向 | `ch` | `d` | 说明 |
|---|---|---|---|
| Agent → Server | `agent.hello` | `{ node_id, node_name, version, caps: SystemCapabilities }` | 连接后首帧 |
| Server → Agent | `agent.resume` | `{ since_ts }` | Server 端该节点 `m_1m` 的最大 `ts`；无数据时 0 |
| Agent → Server | `agent.rows` | `{ layer: "m_1m", rows: [MetricRow + series 元数据] }` | 补发与常规推送共用；每帧 ≤ 1000 行 |
| Agent → Server | `agent.snapshot` | `MetricSnapshot` | 每个采集周期一帧，供 Server 端 `metrics.live` 转发 |
| Server → Agent | `agent.request` | `{ id, method, params }` | 预留，MVP 只允许 `caps.probe` 与 `host.info` |

补发：Agent 收到 `agent.resume` 后从本地 `m_1m` 表 `WHERE ts > since_ts` 分批发送，发完后进入常规模式（每分钟落盘后把新行推一帧）。Server 用 `insert_tier(M1m, rows)`（幂等 UPSERT）写入，`series.node` 为该节点 id；Server 自身的每分钟 `maintain` 会把这些行聚合到粗层。

`series` 映射：Agent 发送的行携带 `{metric, labels, unit}`，Server 端 `get_or_create_series(node, metric, labels, unit)` 得到本地 id。每帧行数多、series 数少，因此帧内先列 series 表再列行，行引用序号。

### 3.3 Server 端

- `crates/strixmaid-server/src/ws/agent.rs`：`/ws/agent` 端点与 `AgentRegistry`（在线节点、最近心跳、最近快照）。
- `routes/nodes.rs`：`GET /api/v1/nodes` 列出节点与在线状态；`POST /api/v1/nodes` 登记节点（生成 token，仅返回一次，存 hash；需提权）；`DELETE`。
- `routes/metrics.rs` 的三个端点增加可选 `?node=` 参数，缺省 `local`。`MetricsEngine::query` 与 `series_list` 增加 node 参数；`local` 以外的节点没有环形缓冲，`live` 层不可用，选层时跳过。
- `metrics.live` 频道订阅参数增加 `node`；非 local 时转发该节点最近一帧 `agent.snapshot`。

### 3.4 不做

远程管理操作、Agent 上的 PAM、mTLS（P1）、Agent 自动注册。

## 4. 涉及文件

`crates/strixmaid-agent/src/{main,config,client}.rs`；`crates/strixmaid-types/src/agent.rs`（帧 DTO）；`crates/strixmaid-core/src/metrics/engine.rs`（node 参数）；`crates/strixmaid-server/src/ws/agent.rs`、`routes/nodes.rs`、`routes/metrics.rs`。

## 5. 测试

1. 单元：补发分批逻辑——1 万行按 1000 切帧，最后一帧不足 1000；`since_ts` 边界为开区间。
2. 集成（本机两个进程）：Server 起在 9700；Agent 以 `node_id = test` 连接；等 2 分钟；Server 的 `GET /metrics/query?node=test&series=cpu.usage` 返回 ≥ 1 行；停 Server 3 分钟再起；Agent 重连后 Server 端该 series 无空洞（相邻 `ts` 差恒为 60）。
3. 错误 token 连接被 401 拒绝；`nodes` 表无对应行时同样拒绝。

## 6. 验收

5.2 通过；Agent 静态二进制体积 < 8 MiB（release，见 `06-packaging.md`）。

## 7. 未决问题

1. 帧格式与 `03-terminal.md` §8.1 的帧头变更无关（Agent 走 WS，不走 IPC 帧）。
2. Server 端存储量随节点线性增长（`design.md` §7.4）；Agent 数量上限与 `Less` 预设的联动策略未定。
