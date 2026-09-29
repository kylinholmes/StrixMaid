# Linux / D-Bus 验证与模块拆分（2026-09-29）

## 结果

真实 Fedora 44 ARM VM 上完成 D-Bus 拆分后的构建、测试和活跃订阅压测。
20 轮 reload 压测未复现卡死，Recv-Q 瞬时峰值 **13457 bytes**，结束时 **0**；
随后 60 秒 12 次采样均为 0。断开订阅后日志确认监听退役，重新订阅能继续收到事件。
这不是无订阅者的健康检查。

宿主原有未提交工作全部保留；没有提交或推送。另按用户指示，在
`/Users/kylin/orca/LimaGUI` 创建了独立空 Git 仓库及 README/HANDOFF，供另一个 AI
实现 macOS Lima GUI；该项目没有混入 StrixMaid。

## 环境与镜像选择

- 宿主 Apple Silicon arm64；Lima 2.2.0（`/opt/homebrew/bin/limactl`）。
- 开始时 Lima / Podman 都没有实例。Downloads 的 Kubuntu 26.04.1 desktop amd64
  和 Debian 13.7.0 amd64 netinst ISO 均为 x86_64 安装介质，未改动。
- 缓存中有 Ubuntu ARM cloud 镜像、nerdctl 包和一份未完成下载；本次使用仓库已有
  Fedora 配置固定的镜像。Fedora 下载约 10 秒，并由 Lima 验证固定 SHA256；
  没有使用 x86_64 整机模拟，也没有重下用户的 ISO。
- 镜像：`https://ftp.riken.jp/Linux/fedora/releases/44/Cloud/aarch64/images/Fedora-Cloud-Base-Generic-44-1.7.aarch64.qcow2`
- SHA256：`55c60a3b80d3616a08705afd0459e75fe9f03c54aba7a46e4002a41a72fa0d5b`。
- 实例名 **strix-dbus**，VZ / aarch64，4 vCPU、4 GiB RAM、60 GiB 稀疏磁盘，SSH 60022。
  禁用本次不需要的 containerd、Rosetta；源码通过原配置 virtiofs 挂载。
- 客机：kernel `6.19.10-300.fc44.aarch64`，systemd `259.5-1.fc44`，
  **dbus-broker**，SELinux **Enforcing**，system 与 user systemd 均正常运行。
- Rust `1.98.0 (88d9e12ae 2026-08-18)`，原生 `aarch64-unknown-linux-gnu`；
  编译产物在客机 `/home/kylin.guest/target`，没有在宿主反复折腾交叉编译。

创建命令（本次已执行；已有实例不要再次创建）：

```sh
limactl start --name=strix-dbus --containerd=none \
  --set '.vmOpts.vz.rosetta.enabled = false | .vmOpts.vz.rosetta.binfmt = false' \
  --yes scripts/verify/vm/lima-strix.yaml
```

日常操作：

```sh
limactl start --yes strix-dbus
limactl shell strix-dbus
limactl list
limactl stop strix-dbus
```

任务结束时停止 VM，保留磁盘、工具链和测试账号供复跑。宿主实例目录实际占用约
6.4 GiB；客机根盘 `df` 使用约 5.4 GiB / 60 GiB（btrfs 与文件逻辑大小统计不同）。
无运行中的本次 VM 消耗 4 GiB 配额。镜像缓存也保留；未删除其他缓存。

## 代码边界

原 `providers/service/bus.rs` 1492 行拆为 190 行连接/构造入口，以及：

| 文件 | 职责 |
| --- | --- |
| `bus/proxy.rs` | Manager 协议与 zbus 生成的 proxy / signal API |
| `bus/error.rs` | D-Bus 错误分类 |
| `bus/properties.rs` | 属性解包、对象路径、列表合并和 timer/cgroup 转换 |
| `bus/units.rs` | 单元查询、动作与 ServiceProvider 实现 |
| `bus/listener.rs` | 信号资源、独立 flusher、监听生命周期和退役协议 |
| `bus/tests.rs` | 原有解析/真实总线测试及严格模式 |

公开 API 保持原路径，含 Manager 的生成项，通过 facade 重新导出。
信号循环只 poll 流、入队和 `try_send`，不 await 总线请求；`SignalStreams`
按值交给循环，返回后先销毁流，再等 flusher / Unsubscribe。保留容量 1 队列、
满时合并名字、15 秒调用超时、30 秒无订阅者检查。退订完成前不释放监听标志，
`finish_retire` 在同一锁内释放标志并检查新订阅者。

新增两个回归：退订期间新订阅者到达时的标志所有权与重启决策；有界刷新队列满时
不等待、将旧名字放回并和新名字去重。`STRIXMAID_REQUIRE_LIVE_DBUS=1` 会把真实
system/user bus 缺失或缺少 systemd-run 变成失败，避免把跳过当通过。

额外的最小修正均来自实际 Linux 验证阻碍：

- `strixmaid-helper/src/auth/unix.rs`：`p as *mut u8` 改为 `p.cast::<u8>()`。
  Linux ARM 的 c_char 已经是 u8，旧写法触发严格 Clippy；cast 方法也兼容 macOS。
- `metrics/collect/mod.rs`、`providers/{fs/icon,log/mod,process/mod,process/icon/mod,service/mod,system/mod}.rs`：
  条件编译掉的 macOS 项改用代码字体，避免 Linux rustdoc 无法解析链接。
- `providers/system/linux/actions.rs`：去掉冗余显式链接目标。
- 两个 wedge 脚本补查 Fedora `dbus-broker` / `sshd` 单元；健康端点从 `/` 改为
  `/api/v1/health`。debug 首页的正常 307 曾使首轮健康检查失败，首轮结果没有计为通过。
  dbus-daemon RSS 项在 broker 环境明确标记跳过。
- `scripts/verify/vm/` 只新增 `setup-dbus.sh`、`prepare-dbus-service.sh`、
  `dbus-subscriber.py`，保留原有三个文件。

## 构建与测试

客机源码目录为 `/Users/kylin/orca/StrixMaid`：

```sh
bash scripts/verify/vm/setup-dbus.sh
source "$HOME/.cargo/env"
export CARGO_TARGET_DIR="$HOME/target" RUSTC_WRAPPER=
export STRIXMAID_REQUIRE_LIVE_DBUS=1
cargo build --workspace --locked
cargo test -p strixmaid-core providers::service:: --locked --offline -- --nocapture
cargo test --workspace --locked --offline
cargo clippy --workspace --all-targets --locked --offline -- -D warnings
RUSTDOCFLAGS='-D warnings' cargo doc --workspace --no-deps --locked --offline
```

首次构建需要网络取 Cargo 依赖，缓存完成后离线复跑。结果：

- Linux 原生全工作区 debug build 通过。
- service 专项 **28 通过，0 失败，0 忽略**；真实查询 458 个单元，CLI / bus 列表一致；
  user transient unit 收到 `(Active, Loaded)` 与 `(Inactive, NotFound)`。
- Linux 全工作区单元/集成 **563 通过，2 忽略**；doctest **3 通过，4 忽略**。
  原有忽略项保持忽略，没有计入通过。真实总线严格模式生效。
- Linux 全工作区严格 Clippy、rustdoc 通过。最小 helper 修正后另重跑 helper：5 通过。
- macOS service 专项 34 通过；真实 launchd 测试需沙箱外执行。
- 未进行本次 Linux release/musl 打包和 x86_64 验证；这不属于本次总线验收结果。

## 真实活跃订阅压测

专用测试 VM 内执行；不要在业务主机运行账号/测试单元准备脚本：

```sh
# 先完成上面的 debug build。
STRIXMAID_DBUS_TEST_VM=1 bash scripts/verify/vm/prepare-dbus-service.sh
python3 scripts/verify/vm/dbus-subscriber.py \
  strix-dbus-test "$HOME/.local/state/strixmaid-dbus/password"
```

准备脚本会安装标准 Fedora PAM 配置，生成独立测试账号及 0600 随机密码文件，
安装二进制到 `/usr/local/bin` 并 restorecon，然后通过 systemd-run 启动名为
`strixmaid` 的临时服务。监听仅在 127.0.0.1:9700；bus 日志级别 debug。
测试账号无需 subordinate UID 区间，规避 Lima 默认账号耗尽区间的问题。
脚本已在本次环境成功复跑，健康端点返回 `status=ok`。

Python 工装真实 PAM 登录，经 bearer 子协议连接 `/ws`，发送 `services.changed`
订阅并持续读帧，同时完成：

1. 三轮各 96 个无害 `sleep` 单元启动/停止；每轮断言全部 96 active 与 96 inactive
   事件到达（不是只看 HTTP 正常）。
2. 原 `dbus-wedge-stress.sh`，**20 轮** daemon-reload 与三个实际存在的服务启动；
   日志前提检查显示监听存活。
3. 原 `dbus-wedge-check.sh`，**60 秒**健康采样。
4. 断开订阅，等 35 秒，重新连接并断言一个新测试单元的 active/inactive 事件。

最终一轮实测：

| 观察项 | 结果 |
| --- | --- |
| 主订阅推送 | 40 批，534 个不同单元 |
| 突发单元事件 | 3 × (96 active + 96 inactive)，全部收到 |
| 压测并行 Recv-Q | 9 次采样，峰值 13457 bytes，结束 0 |
| 后续 60s Recv-Q | 12 次采样，全部 0 |
| 压测后 systemctl | 8 ms |
| 健康检查 Peer.Ping / systemctl | 10 ms / 12 ms |
| 正式健康端点 | HTTP 200 |
| broker / sshd / logind warning 及以上日志 | 本次窗口 0 条 |
| 刷新超时、退订失败、监听重建失败 | 未出现 |
| 退役后重订阅 | 2 批、6 个不同单元，目标 active/inactive 都收到 |

JST 生命周期日志：12:21:27 监听启动 → 12:22:57 无订阅者退场 →
12:23:17 重订阅启动 → 12:23:47 再次退场。符合 30 秒检查周期与按需监听语义。

这次验证的是 Fedora 的 **dbus-broker**，不能写成已验证 dbus-daemon 的 RSS /
max_replies_per_connection 特定实现行为。zbus 接收侧在真实 systemd 信号洪流下
能排空、事件持续到达、退役/重订阅可用，已实测。长达数日 soak、dbus-daemon
发行版及 x86_64 架构矩阵仍未覆盖。

## 证据与结束状态

宿主 `/tmp/strix-dbus-artifacts/stress.log` 保存完整最终压测输出；
`/tmp/strix-dbus-artifacts/strix-dbus-evidence.tar` 保存构建、全量测试、Clippy、
rustdoc、首轮误判、最终压测、Recv-Q 明细和生命周期日志，不含密码或 token。
这些是临时证据文件；本报告保留可复现命令与关键结果。

测试结束停止 `strixmaid` 和 VM，源码改动、客机磁盘和工具链保留。
重新启动 VM 后，runtime 测试模板与 transient server 需要重新执行准备脚本；
测试凭据已迁到客机 home 的私有 state 目录，可跨重启复用。

## 主任务复核与回收

主任务已独立阅读拆分后的连接入口、监听生命周期、单元操作与严格模式测试，
核对证据归档中的构建、563 项测试、Clippy、rustdoc、生命周期和压测原始日志。
信号循环不等待总线请求、释放信号流后再退订、持有标志直到退订结束的约束仍在；
压测确实包含活跃订阅与逐个单元的启停事件断言。此次复核未重新启动 VM 或重跑压测。
`git diff --check` 通过，`limactl list --json` 确认实例为 `Stopped`。

已将完成的 Codex 子任务 `01a0eb20-001b-77e0-9add-224f94404df5` 归档，
归档工具返回 `archived: true`。代码、报告、证据归档与 VM 磁盘保留；
这次创建的是 Codex 子任务，没有额外的 Orca worker terminal 需要释放。
