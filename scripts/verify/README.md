# 07 验证工装

`roadmap/07-verification.md` 的清单里，能自动化的部分做成了脚本；需要人眼、
真实浏览器、或长时间运行的仍是人工项。全部**尚未在真实 root 环境跑通**
（开发机无 root），首次运行按报错微调即可——脚本里每条断言都打印具体消息。

## 一键跑（rootless podman + systemd 容器）

```sh
# 1. 先产出可安装的静态发布物（需 musl-tools / cargo-zigbuild，见 roadmap/06）
scripts/package.sh x86_64
tar xzf strixmaid-0.1.0-x86_64.tar.gz          # 解出 strixmaid-0.1.0-x86_64/

# 2. 起容器、装、跑全部自动检查、拆
scripts/verify/run-in-podman.sh --dist strixmaid-0.1.0-x86_64 --distro ubuntu
scripts/verify/run-in-podman.sh --dist strixmaid-0.1.0-x86_64 --distro rocky
```

驱动脚本做的事：`podman build` systemd 基础镜像（含 polkit / PAM / sudo /
alice / bob / 一个只睡觉的 `strixtest.service`）→ `--systemd=always` 起容器 →
设测试密码 → `install.sh` 装 → `systemctl start strixmaid` → 跑
`root-checks.sh` 与 `agent-checks.sh` → 打印 journald 尾部 → 拆。

**网络**：容器要能装包与访问软件源。国内可给基础镜像换镜像源
（apt 用 `mirrors.aliyun.com`，dnf 用对应 mirror），或预先 `podman build` 一个
带源的镜像。rootless `--systemd=always` 需要 cgroup v2（`podman info` 里
`cgroupVersion: v2`）。

## 只有 docker 的机器

`run-in-docker.sh` 与上面的 podman 版等价，参数相同：

```sh
scripts/verify/run-in-docker.sh --dist strixmaid-0.1.0-x86_64 --distro ubuntu
```

差别只在起容器那一步（docker 没有 `--systemd=always`，要自己给 `--privileged`、
tmpfs 的 `/run`、`SIGRTMIN+3`，并用 `--cgroupns=private` 而不是 host）。
**改了一边记得改另一边**：两个脚本的其余步骤是逐条对齐的。

## 发布物从哪来

被测机器上**不需要**编译。开发机通常既没有 musl-tools 也没有 zigbuild
（HANDOFF-2026-08-28 §3），CI 是唯一两样齐全的环境：`ci.yml` 的 `package` job
把 `build-musl` 与 `build-helper` 的产物组装成与 `scripts/package.sh` 同构的
`strixmaid-dist-x86_64` 产物，下载解压即可喂给 `--dist`。

```sh
gh run download <run-id> -n strixmaid-dist-x86_64
tar xzf strixmaid-0.1.0-x86_64.tar.gz
```

本机有 musl-tools 时仍可用 `scripts/package.sh x86_64` 自己出包，两者产物同构。

## 手工跑（已有 VM / 已在跑的 Server）

脚本不依赖工装，也能对着一个已经跑起来的 strixmaid 直接跑：

```sh
BASE=http://127.0.0.1:9700 ALICE_PW=... BOB_PW=... \
  DB=/var/lib/strixmaid/strixmaid.db TEST_UNIT=strixtest.service \
  scripts/verify/root-checks.sh

BASE=http://127.0.0.1:9700 BOB_PW=... \
  scripts/verify/agent-checks.sh
```

`login.sh` 从 `$STRIX_PASSWORD` 取密码以便无人值守——**只在一次性测试环境用**，
真实系统请用 `scripts/dev-login.sh`（`read -s` 读密码，明文不落地）。

## 覆盖矩阵

| 07 章节 | 项 | 状态 |
|---|---|---|
| §1.2 | #1 helper/polkit 探测 | 自动（root-checks） |
| §1.2 | #2–#10 alice 登录 / 会话 / 未提权写 403 / 提权被拒 / 登出回收 | 自动 |
| §1.2 | #11–#14 bob 登录 + 提权 + 管理操作 + 系统日志 | 自动 |
| §1.2 | #18 改主机名 | 自动 |
| §1.2 | #19 kill -9 主进程后恢复、旧 token 401、sessions 清空 | 自动（工装内） |
| §1.2 | #15/#16 空闲超时（300s/900s） | 人工 / 长测（脚本标 skip） |
| §1.2 | #17 faillock、#20 20 次登录 RSS | 人工（发行版相关 / 容量观测） |
| §1.3 | pam.d 模板通过各发行版 PAM 栈 | 由 install.sh 选模板 + #2/#11 登录成功间接证；warning 需人工看 journald |
| §2 | 浏览器（Scalar / /debug 各面板 / WS 握手 / 深浅色） | **人工**——无头脚本测不了渲染 |
| §3 | 长时间运行（7 天清理 / m_1d / RSS / 时钟回拨） | **人工 / 长测** |
| §4 | release 性能（进程列表 / 查询 / 空闲 CPU） | **人工**（对 release 构建计时） |
| §5 | #1 密码不进日志、#2 不入库、#3 token 只存 hash、#5 helper fd3、#6 WS 无 token、#7 XFF 伪造 | 自动（root-checks） |
| §5 | #4 worker uid 校验 | 代码层单测已覆盖，脚本标 skip |
| 05 §5.2 | Agent 双进程：登记 / 上线 / 补发 / 重连无空洞 | 自动（agent-checks） |
| 06 §5 | 干净机 install.sh + start + health/capabilities | 自动（工装即是干净机） |
| 06 §5.2 | 二进制体积上限 | CI 的 `size` job（ci.yml） |
| 06 §5.3 | Alpine 跑 agent | 人工（`--distro` 未含 alpine，可自行加 `alpine:3` 基础镜像试） |

## 已知偏离 / 注意

- **#5/#6 空闲超时**默认标 skip；`--long` 目前只是把它们说清而非真等（等 900s 不适合
  塞进一次 CI）。要真测就人工把 `session.idle_timeout_secs` 调小后观察，或在脚本里
  按需加 `sleep`。
- **§5 #1 密码不进日志**：严格检验要 `RUST_LOG=trace`；unit 默认 `info`。工装的
  unit 用 `Environment=RUST_LOG=info`，脚本据此在消息里注明「需 trace 才是严格检验」。
- **agent-checks 的重连测试**把「停 3 分钟」压成一次 `systemctl restart`：补发逻辑
  与停机时长无关（水位由 Server 已有的最大 ts 决定），一次重启足以验证无空洞。
