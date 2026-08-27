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


---

## 8. 完成状态（2026-08-28）

§4.1–§4.5 全部实现。`cargo test --workspace` 333 通过（连续 3 轮），clippy 零 warning，
`scripts/acceptance.sh` 静态检查全过，本机实测登录失败已落审计且库里无任何凭据痕迹。

### 落地形态

- **写入点只有两处**：`auth/exec.rs` 的公开入口（`call_from` / `call_escalating_from`），
  以及认证路由与 sweeper。判据是 `should_audit = privilege == Admin || is_write(method)`
  ——两个条件取并集各堵一个洞：只看 `privilege` 会漏掉 `scope = user` 的
  `service.action`（走 user worker 却是货真价实的写），只看方法表会漏掉将来忘了登记的新 admin 方法。
- **一次用户操作只写一条记录**，靠结构保证：审计只出现在公开入口，
  `call_escalating_from` 内部那两次 worker 调用（先 user、被拒后 admin）是同一次操作的
  两次尝试，不是两件事。私有的 `call_inner` / `escalate` 一个字都不写。
- **来源地址**经 `exec::RequestOrigin` extractor 取得，它自己从请求里拿 `HeaderMap` 与
  `ConnectInfo`，不需要中间件。判断是否采信 `X-Forwarded-For` 只有一个实现
  （`auth::audit::remote_addr`），封在 `RequestOrigin` 里，调用点不可能绕过它。

### 实现时对本文档的补充

1. **认证失败的 result 映射不能复用 `outcome_of`。** PAM 拒绝在 API 层是 401
   `unauthenticated`，`outcome_of` 会算成 `error`，而 §4.1 要求 `denied`。
   `routes/auth.rs` 因此有一个本地的 `auth_outcome`。**反向也重要**：helper 起不来
   必须记成 `error`，否则运维分不清「有人在爆破」和「PAM 那天崩了」。
2. **`action` 是前缀匹配。** 三处文档打架：`types::AuditQuery::action` 说前缀、
   core 的 `AuditFilter::action` 是精确、`/debug` 的提示暗示精确。按 DTO 契约实现前缀
   （它是精确匹配的超集），做法是给 `AuditFilter` **新增** `action_prefix`，
   不改 `action` 的语义。SQL 用 `instr(action, ?) = 1` 而非 `LIKE ? || '%'`
   ——后者会把输入里的 `%` `_` 当通配符。
3. **时间区间是左闭右开 `[since, until)`**，与指标查询一致。`types::AuditQuery::until`
   的文档原写「含」，已改正；闭区间会让按整点分页的相邻两页重叠。
4. **`limit` 越界返回 400，不静默夹取**，与 `providers::log::normalize_limit` 一致。
   注意 core 的 `AuditFilter::effective_limit()` 是静默夹取的——存储层容错、API 层严格，
   两处策略不同是有意的。
5. **`/auth/respond` 拿不到用户名**，而登录失败那条审计需要它。为此在 core 加了
   `SessionManager::pending_username()`：`Pending` 现在记下这次对话认证的是谁。
   **必须在 `login_respond` 之前读**——终局时 core 会把 pending 条目摘掉。
6. **pending id 不认识时不写记录。** 伪造或过期的 id 意味着 PAM 根本没收到任何凭据，
   也没有用户名可记；写一条 username 为空的记录只会污染审计表。
7. **`/auth/start` 第一轮就失败也算一次登录失败**（用户不存在、账户锁定）；
   空用户名的 400 不记——那个请求连 PAM 都没碰到。
8. **`host.power` 没有 target**，动作留在 `params` 里。§4.1 只举了 unit / pid / 主机名。
9. **`trusted_proxies` 默认空，且只认精确 IP。** 默认空 = 谁都不信、一律用直连地址：
   来源地址若能被任意客户端用一个请求头伪造，那条记录会变成主动的误导，比没有更坏。
   写成 CIDR 会在**启动时报错**而不是静默永不匹配——后者会让部署者以为 XFF 已被采信。
10. **超时回收的执行者记作 `[system]`**（Unix 用户名不允许含空格与方括号，不会冲突），
    `target` 与 `uid` 仍记被影响的用户，事后才能按用户筛出「他的会话何时被回收过」。
11. **`call` 现在是读操作专用**，写操作必须用 `call_from`。有 `debug_assert` 加运行期
    warn 兜底：新增写端点时若忘了取 `RequestOrigin`，测试会立刻炸。
    不带 origin 的 `call_escalating` 已删除——走那条路的方法全是写操作。

### 尚未验证

- **§6.2 的完整集成链**（登录失败 → 成功 → 写操作 403 → 登出，共 4 条记录且顺序为
  id DESC）需要真实密码。已把可自动化的部分编码进 `scripts/acceptance.sh` 的审计段。
- **保留期清理的长期行为**（真的跑够一小时、跨天清理）——单测只验证了边界计算与
  「会再来一轮」，属 `07-verification.md` 的长时间运行范围。
