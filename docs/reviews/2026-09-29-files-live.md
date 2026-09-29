# A4 Linux 真 worker 文件访问验收（2026-09-29）

状态：**Linux 真 worker 五轮验收通过，ATTEMPT 7 退出 0**。范围仅限本文列出的
Linux HTTP/worker 检查，不代表 Edge、Windows 或发布产物等完整 A4 均已通过。

实际运行二进制来自 **`bd50129` 单持续 blocking reader 构建**。后续 `d7cac6a`
仅补 EINTR 重试及 cancel 的 unit 边界（阻塞系统调用取消仍持 permit、真实 File
阻塞发送超时回收），主任务另负责 latest Linux workspace、live D-Bus、
Clippy 和文档检查；本次五轮不记作 `d7cac6a` 构建的实测。安装后二进制 SHA256：

```text
strixmaid        e32f943affe7209fbae0fa0689f288cfa81995e8ae506457117df78f7d258162
strixmaid-helper d28298ac04f4d9e21bc7b8700d4222ee6b389ecfdeab8187219ade19ee2bda8b
```

## 工装与隔离

[`scripts/verify/vm/file-access.py`](../../scripts/verify/vm/file-access.py) 仅使用
Python 标准库，复用 `session-lifecycle.py` 的 Client/PAM conversation、进程查询和
transient service 模式。必需 Linux、root、`STRIXMAID_TEST_VM=1`，密码文件须是
mode 0600 的普通文件，不接受符号链接；日志不输出密码、Bearer 或 Cookie。

使用已运行的 `strix-dbus`、已有 `strix-dbus-test` 账号及其密码文件。安装已构建的
二进制并 `restorecon` 是调用方在专用 VM 内执行的步骤，脚本本身不安装文件。
脚本不修改账号、PAM 配置或其他服务；随机端口、独立 systemd unit、数据库、配置
和文件都属于本次验收。临时目录固定在客机 `/tmp`，源目录通过 virtiofs 共享。
退出时只停止自己的 unit 并删除自己的目录，不停止 VM，不读取或修改用户下载镜像。

客机运行方式（源码根目录）：

```sh
sudo install -m755 "$HOME/target/debug/strixmaid" "$HOME/target/debug/strixmaid-helper" /usr/local/bin/
sudo restorecon /usr/local/bin/strixmaid /usr/local/bin/strixmaid-helper
sudo env STRIXMAID_TEST_VM=1 python3 -B scripts/verify/vm/file-access.py \
  strix-dbus-test "$HOME/.local/state/strixmaid-dbus/password" \
  --node-cycles 5
```

宿主完整输出追加保存在 `/tmp/strixmaid-files-live.log`，包括失败尝试，不能把某轮
部分 PASS 当作完整通过。脚本默认三轮节点压力；本轮最终复验实际执行五轮。

## 覆盖与判据

- 九次真实 PAM 登录，同一个 Unix 账号产生九个不同 session；检查真实 worker 的
  real/effective/saved/fs UID。这里验证的是 session 所有权隔离，不冒称两个 Unix 账号。
- 3,145,865 字节文件完整 SHA256、重复单段/开放结尾/suffix Range、206/416、
  无法验证的 If-Range 回完整 200、多段 Range 按当前契约回完整 200。
- 4,295,032,969 字节稀疏文件的尾部、跨 4 GiB 偏移和 HEAD；初始磁盘占用仅
  135,168 字节。空文件 GET/HEAD/416，HEAD 检查实际线上无 body。
- root-owned 0600 文件位于允许根目录，GET/HEAD 都须返回 403。
- 无 Cookie、单独 Bearer 不能读取；普通多 Cookie 可读取，重复文件 Cookie 拒绝；
  同账号第二 session 不可跨读/续期/删除；Cookie 不能调用管理 API。
- Bearer-only renew 后使用返回的 Set-Cookie 继续读取；Bearer-only DELETE 撤销记录。
  Cookie 的 HttpOnly、SameSite、Path、Max-Age、无 Domain，以及响应缓存、压缩和
  内容安全标头均检查；检查本轮秘密未出现在本服务 journal 和数据库/WAL。
- 每 session 第五条流、节点第三十三条流须返回 409；节点测试使用八个 session
  各四条流，第九个 session 单独验证全局上限。取消一个后必须可重新占用名额。
  取消传播过程中 core 的独立名额可能短暂返回 503，只允许有界重试，不能永久繁忙。
- 暂停 HTTP 客户端读取大文件制造背压，检查同时存在 32 个活动文件 fd；负载中
  测量同 session 的 `/auth/session` 及真实 worker `/files` 延迟，默认最大 1000 ms。
- 注销必须中断已开始的 HTTP 流，拒绝旧 token/Cookie，回收 helper/worker，同时
  第二 session 继续可读。socket timeout 不被当成取消成功。

资源采样每 100 ms 一次，统计独立服务及所有后代的 RSS、匿名/文件映射 RSS、fd、
测试文件 fd、进程数、内核线程数。各阶段还按 PID 和 server/helper/user worker
分组采集 `/proc/PID/smaps`，聚合总 Private_Dirty、Anonymous 和匿名 VMA 的
Private_Dirty，以及 AnonHugePages，并区分 heap、stack、无名映射。smaps 解析在延迟计时区间外进行，
不读取进程内存内容或认证日志作诊断。这些指标定位保留发生在哪类进程，不直接
证明某个 Rust 对象仍持有内存。RSS 是进程 RSS 之和，共享页可能重复计入，不是
VM 物理使用量；“线程数”不是 Tokio task 数。采样峰值不是数学意义的瞬时峰值。

每轮关闭 32 条流后记录 0/10/30/60 秒快照；文件 fd 必须归零，总 fd 默认可比基线
多 8 个。首次宽并发的冷启动 RSS 残留如实报告，以第一轮 60 秒空闲 RSS 为热基线：
后续峰值默认不超过热基线 +128 MiB、空闲 +64 MiB，多轮最大空闲增长不超过 32 MiB。
这些是工装预算，不能把首次 +64 MiB 直接解读为方案要求或产品泄漏。没有设置
`MALLOC_ARENA_MAX`、调用 malloc_trim 或调整服务分配器来使验收通过。

指定 `--diagnostic-hold-secs` 时，预算或其他检查失败会在清理前保留自己的服务供
诊断，并输出 unit、server/verifier PID；向 verifier 发送 SIGUSR1 可结束暂停，
保留时长到期也会继续清理。暂停期间只为已有会话保活，不另建文件流。

## 修复前诊断

首次运行发现工装误把 `HTTPConnection` 当 context manager，已改为复用 Client。
第二次单轮压力在冷基线 +64 MiB 的自定预算处退出；随后延长至 30 秒，观察到
文件 fd 从 32 回到 0、总 fd 从 315 回到 187、线程从 123 降到 62，而 RSS 基本
不变。这支持进一步检查分配器/线程缓存保留，不能单凭此认定泄漏或已通过。

取消传播窗口中的 core 独立名额可返回 503，已改为有界重试。

ATTEMPT 5 在旧 build 上完成三轮开/关，保留了当时的冷基线预算失败信息，结果不是
完整通过。60 秒观测如下（单位 KiB；这是进程 RSS 之和）：

| 轮次 | 采样峰值 RSS | 60 秒 RSS | 60 秒总 fd / 文件 fd | 60 秒线程 |
|---|---:|---:|---:|---:|
| 宽并发前基线 | — | 410852 | 187 / 0 | 73 |
| 1 | 529360 | 529360 | 189 / 0 | 63 |
| 2 | 558556 | 556848 | 189 / 0 | 64 |
| 3 | 597892 | 594496 | 189 / 0 | 64 |

第三轮比第一轮冷却后的 RSS 多 65136 KiB，尚未观察到稳定平台。控制 API 最大
13.34 ms；注销后已排队 327680 字节被读出，随后大文件流中断，旧凭证拒绝、另一
session 可读、helper/worker 全回收。旧版 100 ms 总峰值可能漏过极短载流窗口，
最终脚本已将显式 32 流快照合并进峰值统计，上表取各轮显式快照及采样的较大值。

ATTEMPT 6 使用逐进程 smaps 定位到 user worker 的无名匿名 VMA；第二轮冷却时
worker 合计匿名 Private_Dirty 242460 KiB，其中 AnonHugePages 235520 KiB。
heap 项变化很小，线程和活动文件 fd 均已回收。主任务另观察到 64 MiB 对齐 arena
中的 2 MiB RW dirty THP 映射，进而将 core 改为每流一个 blocking reader、单元素
队列背压，阻塞任务持有 permit 直至真正退出。上述证据支持 THP 放大和分配器保留
的解释，不等于从 smaps 直接证明具体对象。按主任务指示，在第 4 轮 30 秒后停止
旧 build（退出 130，已清理），保留全部诊断输出，不计完整验收通过。

## 最终结果：ATTEMPT 7

使用原参数和预算，在 `bd50129` 构建上完成协议检查、三轮 session 取消、五轮
节点 32 并发、每轮 60 秒冷却、注销、凭据检查及清理。没有修改 THP、分配器或
RSS 门槛来使验收通过。3,145,865 字节全量响应 SHA256 为：

```text
40739221d077f143d0cb9df5bf3c07f16bae9e20e70927290b3f579207dc7ed8
```

五轮实测（单位 KiB，RSS 为整棵服务进程树之和）：

| 轮次 | 总 RSS 峰值 | 60 秒总 RSS | server RSS | user worker RSS | worker 匿名 Private_Dirty |
|---|---:|---:|---:|---:|---:|
| 1 | 478880 | 478204 | 63900 | 333044 | 149440 |
| 2 | 483128 | 479700 | 65120 | 333320 | 149716 |
| 3 | 483688 | 481836 | 67168 | 333408 | 149804 |
| 4 | 486416 | 479840 | 65120 | 333460 | 149856 |
| 5 | 484148 | 479988 | 65236 | 333492 | 149888 |

首次宽并发前总 RSS 为 411520 KiB，首轮 60 秒空闲仍保留 **66684 KiB** 冷启动
增量，未把它写成“回到冷基线”。首轮以后，空闲 RSS 最大增长 **3632 KiB**，末轮
增长 **1784 KiB**；worker 自身五轮仅增长 **448 KiB**。worker AnonHugePages 五轮
始终 **143360 KiB（140 MiB）**；helpers RSS 五轮始终 81260 KiB。说明本次有界
重复压力下已形成稳定平台，不是长期无泄漏证明。

末轮 server 的匿名 Private_Dirty / AnonHugePages 为 34804 / 20480 KiB，helpers
为 11628 / 6144 KiB。变化较大的旧 worker 无名匿名映射在修复后不再按轮累积。

- 活动文件 fd 每轮实测 32，取消后均回到 **0**；总 fd 基线 187、峰值 317，
  冷却后稳定 189，处于原有 +8 预算。冷却后内核线程数稳定 64。
- 每 session 第五条流与节点第三十三条流返回 409；中断一条后能复用名额。
  阻塞 reader 的取消/permit 路径通过真实 HTTP 重复验证，未靠增加并发预算规避。
- 32 条暂停流下，所有轮次控制请求最大延迟：`/auth/session` **1.46 ms**，
  经真实 worker 的 `/files` **2.63 ms**，均低于原 1000 ms 预算。
- root 0600 的 GET/HEAD 403，字节 hash、重复 Range、4 GiB 边界、空文件、
  多 Cookie、跨 session、Bearer-only renew/delete 等本文覆盖项全部通过。
- 注销后排队的 **393216 字节**被读出，随后 4295032969 字节响应提前结束；旧
  token/Cookie 被拒绝，另一 session 继续可读，被注销 session 的 helper/worker 退出。
- 剩余会话全部注销，确认 helper/worker 无残留；密码、Bearer、文件 Cookie 未在
  本轮服务 trace journal 和数据库/WAL 中检出明文。
- 脚本 AST、`--help`、宿主非 VM 安全拒绝、Linux smaps 元数据解析器及
  `git diff --check` 已检查。未增加第三方 Python 依赖或生成源码目录 `__pycache__`。

最后确认没有 `strixmaid-file-check-*.service` 残留或对应 `/tmp` 目录。主任务随后对
最终代码（含 EINTR 重试与确定性取消回归测试）运行 Linux workspace：**594 项
单元/集成测试与 3 项 doctest 通过**，另有 2 项单元测试、4 项 doctest 按既有配置
ignored。启用 `STRIXMAID_REQUIRE_LIVE_DBUS=1`，真实总线测试没有作为缺环境跳过。
workspace 全目标 Clippy `-D warnings` 与严格 rustdoc 均通过。

主任务已复核本报告和完整日志，回收全部子任务；确认客机无 strixmaid 进程或本次
transient unit 后停止 `strix-dbus`。虚拟机磁盘、已有测试账号和用户 Downloads
镜像均保留。最终 Linux 质量日志为 `/tmp/strixmaid-files-linux-quality.log`。

不包含 Edge/UI、自然等待 600 秒的 Cookie/记录到期、两种 Unix 账号的完整交叉矩阵、
Windows 或发布产物体积验收。这些不因本脚本通过而自动获得通过结论。
