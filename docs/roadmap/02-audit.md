# 02 审计日志

## 1. 目标

每一次写操作与认证事件都落入 `audit_log` 表，记录谁、何时、对什么、以何种权限、结果如何、来自哪里；提供 `GET /api/v1/audit` 分页查询；按保留期清理。

## 2. 现状

| 项 | 位置 | 状态 |
|---|---|---|
| 表 | `crates/strixmaid-core/migrations/0001_init.sql` | `audit_log`，字段见 `design.md` §8 |
| 存储 API | `crates/strixmaid-core/src/store/audit.rs` | `NewAuditEntry::new(node, user, action, outcome)` 加链式 `.target() .actor() .params() .detail() .remote_addr()`；`Store::audit_write(&NewAuditEntry) -> i64`；`audit_query(&AuditFilter) -> AuditPage`；`audit_get(id)`；`audit_prune(before_ts)` |
| DTO | `crates/strixmaid-types/src/audit.rs` | `AuditEntry`、`AuditResult`、`AuditQuery`（`before_id` 游标）、`AuditPage` |
| 写入 | 服务端 | 0 处 |
| 路由 | 服务端 | 无。`/debug` 页的审计面板已按 `GET /api/v1/audit?before_id=` 调用 |
| 客户端地址 | `main.rs::serve` | 已用 `into_make_service_with_connect_info::<SocketAddr>()`，处理器可取 `ConnectInfo` |

## 3. 设计约束

- `design.md` §8：`audit_log.action` 为 `service.start` 这类点分标识；`params` 为 JSON；`result` 为 `ok | denied | error`。
- `design.md` §5.3：审计记录不得含密码。认证事件只记用户名与结果。
- `design.md` §9.1：`GET /audit` 需管理访问。

## 4. 方案

### 4.1 写入点

写入集中在两处，不散落到各路由：

1. **`auth/exec.rs` 的 `call`**（`01-worker-execution.md` §4.3）：`privilege == Admin` 的调用，以及 4.1 表中标 `*` 的两项，无论结果如何都写一条。`action` 取 RPC 方法名（`service.action` 时展开为 `service.<action>`，如 `service.restart`）；`target` 从 params 提取（unit 名、pid、主机名等）；`params` 为去除 target 后的剩余 JSON；`result` 按 `Ok` / `ErrorCode::PermissionDenied | ElevationRequired` / 其它错误映射为 `ok` / `denied` / `error`；`detail` 为错误消息。
2. **认证路由**（`routes/auth.rs`）：

| 事件 | action | result | 说明 |
|---|---|---|---|
| 登录成功 | `auth.login` | `ok` | `username` 为 PAM 返回的规范用户名 |
| 登录失败 | `auth.login` | `denied` | `username` 为请求中的用户名；`detail` 为 PAM 的错误文本 |
| 提权成功 / 失败 | `auth.elevate` | `ok` / `denied` | |
| 登出 | `auth.logout` | `ok` | |
| 会话超时回收 | `session.expire` | `ok` | 由 sweeper 写，`actor` 为 `system` |
| 提权超时回收 | `session.drop_elevation` | `ok` | 同上 |

读操作不审计。

### 4.2 `NewAuditEntry` 的填充

| 字段 | 来源 |
|---|---|
| `node` | `session.node`（MVP 恒为 `local`） |
| `username` / `uid` | `session.user` |
| `elevated` | `session.elevated` |
| `remote_addr` | `ConnectInfo<SocketAddr>`；经反代时取 `X-Forwarded-For` 首项——仅当配置 `trusted_proxies` 包含直连地址时才信任该头（新增配置项，默认空） |
| `ts` | `now_unix()` |

写入失败只记 `tracing::error`，不阻断请求。

### 4.3 路由

`crates/strixmaid-server/src/routes/audit.rs`：

```
GET /api/v1/audit?before_id=&limit=&username=&action=&result=&since=&until=
```

要求会话已提权（`session.elevated`），否则 403 `elevation_required`。这是服务端唯一一处基于会话状态而非 worker 的判断，理由：审计表在主进程的 SQLite 中，没有对应的 OS 权限可外包。

### 4.4 保留期

新增配置 `audit.retention_days`，默认 90，允许 7–3650。`SessionManager::spawn_sweeper` 已有每 5 秒一次的周期任务；审计清理另起每小时一次的任务，调用 `audit_prune(now - retention)`。

### 4.5 导出

不做。`AuditPage` 已可分页拉取。

## 5. 涉及文件

| 文件 | 改动 |
|---|---|
| `crates/strixmaid-core/src/config.rs` | `AuditConfig { retention_days }`、`trusted_proxies: Vec<IpNet 或 String>`，校验与示例 |
| `crates/strixmaid-server/src/auth/exec.rs` | 写入点 1 |
| `crates/strixmaid-server/src/auth/audit.rs` | 新建：`record(store, &Session, action, target, params, result, detail, addr)` 与 `remote_addr(headers, connect_info, &config)` |
| `crates/strixmaid-server/src/routes/auth.rs` | 写入点 2 |
| `crates/strixmaid-server/src/routes/audit.rs` | 新建 |
| `crates/strixmaid-server/src/routes/mod.rs` | 注册 `audit` 模块与 tag |
| `crates/strixmaid-core/src/session/mod.rs` | sweeper 回收时写 `session.expire` / `session.drop_elevation`（需持有 store，已持有） |
| `crates/strixmaid-server/src/main.rs` | 起清理任务 |

## 6. 测试

1. `remote_addr` 解析：无代理头、有头但直连地址不在 `trusted_proxies`、在列表中三种情况。
2. 集成：登录失败一次、成功一次、调用一个写端点（未提权，得 403）、登出；`GET /audit` 应有 4 条，顺序为 `id DESC`，`result` 分别为 `ok`(logout)、`denied`、`ok`(login)、`denied`(login)。
3. `grep` 全部审计写入路径，确认 `params` 不可能含 `PromptResponse`。
4. `audit_prune` 已有单测；补一条清理任务的调度测试。

## 7. 验收标准

- 每个写端点的一次调用对应恰好一条审计记录；
- 未提权会话访问 `GET /audit` 返回 403；
- `/debug` 页审计面板可翻页。
