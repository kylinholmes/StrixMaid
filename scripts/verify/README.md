# 07 验证工装

`roadmap/07-verification.md` 的清单里，能自动化的部分做成了脚本；需要人眼、
真实浏览器、或长时间运行的仍有未覆盖项。Ubuntu / Rocky 的历史 root 验证结果见
`docs/roadmap/07-verification.md`，不能当作当前工作区已复跑的结果；当前 Linux
D-Bus 验证见 `docs/reviews/2026-09-29-linux-dbus.md`。

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
，CI 是唯一两样齐全的环境：`ci.yml` 的 `package` job
把 `build-musl` 与 `build-helper` 的产物组装成与 `scripts/package.sh` 同构的
`strixmaid-dist-x86_64` 产物，下载解压即可喂给 `--dist`。

```sh
gh run download <run-id> -n strixmaid-dist-x86_64
tar xzf strixmaid-0.1.0-x86_64.tar.gz
```

本机有 musl-tools 时仍可用 `scripts/package.sh x86_64` 自己出包，两者产物同构。

Apple Silicon 上还有第三条路：在下面那台验证虚拟机里跑
`scripts/package.sh aarch64`（原生构建，约 5 分钟），产出 aarch64 的发布物。

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

## CI 里也跑

`ci.yml` 的 `verify` job 对 ubuntu / rocky 两个发行版各跑一遍本目录的检查
（`needs: [package]`，直接吃 `package` 出的发布物）。GitHub 的 runner 本身就是
带 root 与 docker 的 Ubuntu VM，跑法与开发者本机一致。

这么做的理由是实打实的：07 第一次真跑就掉出一个**只在 RHEL 系上出现**的缺陷——
journalctl 的 `insufficient permissions` 被归成 `internal`，普通用户打开日志页得到
500，而 Ubuntu 上永远返回 200。跨发行版的矩阵是唯一防得住这类回归的办法。

`fail-fast: false`：一个发行版挂了要能看到另一个的结果，否则分不清是普遍问题还是
发行版相关。搬不进 CI 的仍见下表标「人工」的那几项。

## 本机虚拟机（Apple Silicon）

macOS 上没有「不开虚拟机」的选项——Docker Desktop、podman machine、Colima，
它们本身都是一台 Linux 虚拟机。既然一定要有一台，就让它是一台**看得见、进得去、
能当真机用**的，而不是一个只给容器用的黑盒。`vm/` 下就是这台机器的全部定义：

```
scripts/verify/vm/
  lima-strix.yaml     虚拟机定义（Lima + Apple 虚拟化框架）
  setup-build.sh      客机内：装成能出 Linux 发布物的构建机
  setup-desktop.sh    客机内：装一层桌面（可选，headless sway + VNC）
  setup-dbus.sh       客机内：真实总线验证依赖
  prepare-dbus-service.sh  专用测试账号与临时 server
  dbus-subscriber.py  活跃订阅下的总线压测
  session-lifecycle.py  真实 PAM 提权/会话超时、进程与数据库回收
```

### 起

```sh
brew install lima
limactl start --name=strix scripts/verify/vm/lima-strix.yaml     # 首次约 1 分钟
limactl shell strix
```

**不用时 `limactl stop strix`。** 内存是硬成本：Apple 虚拟化框架按需分配，
但客机一旦把页面用过（编译时的 page cache 就够了），宿主侧那个
`com.apple.Virtualization.VirtualMachine` 进程的 physical footprint 就等于配置的
`memory`，不会自己还回来（没开 balloon）。停掉就是 0，而且**回来只要 11 秒**：

| | 实测 |
|---|---|
| `limactl stop strix` | 2.6 s |
| `limactl start strix`（热） | 11.5 s，到 `systemctl is-system-running: running` |
| 首次开机（冷，含 cloud-init） | 约 35 s |
| 宿主内存占用（客机用过之后） | = 配置的 `memory`（4.0 GB） |
| 宿主内存占用（刚 stop/start 过） | 1.5 GB —— 嫌它涨得多就重启一次 |
| 宿主磁盘占用（含 rust 工具链 + 桌面 + 容器镜像 + 一次完整构建） | 5.6 GB |

只跑验收的话 `memory: 2GiB` 就够（客机实测 used 719 MB，其余是 page cache）；
要在里面编译再调回 `4GiB`。

### 架构：本机验逻辑，CI 验架构

CI 的发布物是 x86_64，Apple Silicon 上的客机是 aarch64。配置里开了 Rosetta，
它**能**跑 CI 的静态 `strixmaid`，也能跑整个 `--platform linux/amd64` 的容器
用户态（镜像构建 85 s，比 qemu-user 快一个数量级）。

但它**跑不了 07 要的那个容器**：x86_64 的 systemd 一旦当 PID 1 就 SIGSEGV（139），
换 qemu-user-static 同样崩——普通进程没事，只有 PID 1 会。原生 arm64 容器则
一切正常（实测 `is-system-running: running`）。

所以本机这条路走**原生 aarch64**：

```sh
limactl shell strix -- scripts/verify/vm/setup-build.sh    # rustup + musl + bun
limactl shell strix
  export PATH="$HOME/.cargo/bin:$HOME/.bun/bin:$PATH"
  export CARGO_TARGET_DIR="$HOME/target"     # 源码走 virtiofs，产物写本地盘
  cd ~/src/StrixMaid && scripts/package.sh aarch64
  tar xzf strixmaid-0.1.0-aarch64.tar.gz
  scripts/verify/run-in-podman.sh --dist strixmaid-0.1.0-aarch64 --distro ubuntu
  scripts/verify/run-in-podman.sh --dist strixmaid-0.1.0-aarch64 --distro rocky
```

**不要在挂载目录里构建**：`bun install` 会把宿主的 `web/node_modules` 换成
Linux 版。`setup-build.sh` 的用法里源码是复制进客机本地盘的。

架构那一维仍然由 CI 覆盖（`ci.yml` 的 `verify` job 在 x86_64 runner 上跑同一套
脚本）。本机负责的是**改完立刻能验**，以及容器覆盖不到的那几项。

实测（2026-09-25，M 系列 Mac，4 vCPU / 4 GiB 客机，aarch64 发布物）：

| 步骤 | 耗时 |
|---|---|
| 镜像下载（ftp.riken.jp，503 MB）+ 首次开机 | 约 1 分钟 |
| `setup-build.sh`（rustup + musl + bun） | 33 s |
| `package.sh aarch64`（含前端，冷缓存） | 5 分钟 |
| `run-in-podman.sh --distro ubuntu` | 约 9 分钟（其中 agent-checks 的 `sleep 150` 占一半） |
| `run-in-podman.sh --distro rocky` | 约 9 分钟 |

结果：**ubuntu 通过 32 / 失败 0 / 未测 7，rocky 通过 35 / 失败 0 / 未测 5**，
agent-checks 两边都是 5 / 0 / 1，退出码均为 0。

### 容器覆盖不到、只有这台 VM 能验的

| 项 | 容器里的结果 | 为什么 |
|---|---|---|
| 07 §1.2 #9 `/services?scope=user` | 503（`未测`） | 容器里没起 `user@.service`，拿不到用户总线 |
| 07 §1.2 #15/#16 空闲超时 | 未测 | 要等 300 s / 900 s，不适合塞进一次 CI；VM 可以挂着 |
| 07 §1.2 #17 faillock、#20 20 次登录 RSS | 未测 | 依赖发行版配置与长时间观测 |
| 07 §2 浏览器 | 未测 | 需要真实渲染 |
| `dbus-wedge-stress.sh` | 跑不了 | 要 systemd-logind + sshd 的 pam_systemd + 一个正订阅 `services.changed` 的客户端 |

### 桌面（可选）

```sh
limactl shell strix -- scripts/verify/vm/setup-desktop.sh
limactl shell strix -- systemctl --user start strix-desktop
open vnc://127.0.0.1:5900        # macOS 自带「屏幕共享」，无密码
```

headless sway + wayvnc，1920x1200。终端 foot、启动器 fuzzel、浏览器 Firefox；
字体默认退回 Noto CJK，要用宿主那套 Maple Mono NF CN 就在**宿主**上跑一次：

```sh
tar -C ~/Library/Fonts -cf - $(cd ~/Library/Fonts && ls MapleMono-NF-CN-*) \
  | limactl shell strix -- bash -lc \
    'mkdir -p ~/.local/share/fonts/MapleMono && tar -C ~/.local/share/fonts/MapleMono -xf - && fc-cache -f'
```

Lima 的 `video.display` 只对 QEMU 后端有效，
Apple 虚拟化框架这条路没有显示设备；即便有，它给 Linux 客机的 virtio-gpu 也只有
2D，一样是软件渲染。headless + VNC 反而更省：**不连就一个像素都不画**。

**说清楚：桌面不是任何一条验收项的必要条件。** Lima 已经把客机端口转到宿主
`127.0.0.1`，用 macOS 上的浏览器打开 `http://127.0.0.1:9700` 更快也更准——
07 §2 本来写的就是「任一现代浏览器，SSH 隧道到 9700」，`dbus-wedge-stress.sh`
要的那个「正订阅 services.changed 的客户端」，宿主浏览器一样算数。

（别把它说成 polkit：服务操作的提权走的是 strixmaid 自己的 PAM admin worker，
07 §1.2 #7 的预期答案就是 403 + `can_retry_elevated`，桌面的 polkit 认证代理
不在这条路上。）

装这层的实际价值只有两条，但都成立：**Linux 侧浏览器的真实渲染**
（字体回退、深浅色跟随，与 macOS 上的浏览器不是一回事），以及
**手上多一台随时能用的 Linux 图形机器**。不用时 stop 掉，代价为零。

### 真实 PAM 会话生命周期

已有 `strix-dbus` 实例可复用，先 `limactl start --yes strix-dbus`。
在专用 VM 中完成 `setup-dbus.sh`、workspace build 和 `prepare-dbus-service.sh`
后，停止该脚本启动的 `strixmaid`，再运行独立生命周期检查：

```sh
# 以下都在客机源码根目录执行。使用刚构建的二进制，不覆盖宿主 node_modules。
sudo systemctl stop strixmaid
sudo install -m 755 "$HOME/target/debug/strixmaid" "$HOME/target/debug/strixmaid-helper" /usr/local/bin/
sudo restorecon /usr/local/bin/strixmaid /usr/local/bin/strixmaid-helper
sudo env STRIXMAID_TEST_VM=1 python3 scripts/verify/vm/session-lifecycle.py \
  strix-dbus-test "$HOME/.local/state/strixmaid-dbus/password"
```

脚本创建独立临时 server、随机本地端口与数据库，复用已有测试账号，不修改账号的
组或密码。验证真实登录、提权、普通请求不延长管理访问、超时后管理请求 403、
用户终端仍可用、最终 token 401，以及 helper / worker / shell、库表和审计回收。
另检查 trace 日志及数据库/WAL 无明文密码或 token。退出时停止临时服务并删除
它的临时文件；VM 由调用方在检查结束后停止。

默认使用产品允许的短档：会话 60 秒、提权 30 秒。要测正式默认值，追加
`--idle-timeout 900 --elevated-timeout 300`，约需 20 分钟。
短档通过不能记成正式默认时长已验证。最新结果见
`docs/reviews/2026-09-29-session-lifecycle.md`。

## 覆盖矩阵

| 07 章节 | 项 | 状态 |
|---|---|---|
| §1.2 | #1 helper/polkit 探测 | 自动（root-checks） |
| §1.2 | #2–#10 alice 登录 / 会话 / 未提权写 403 / 提权被拒 / 登出回收 | 自动 |
| §1.2 | #11–#14 bob 登录 + 提权 + 管理操作 + 系统日志 | 自动 |
| §1.2 | #18 改主机名 | 自动 |
| §1.2 | #19 kill -9 主进程后恢复、旧 token 401、sessions 清空 | 自动（工装内） |
| §1.2 | #15/#16 空闲超时（300s/900s） | `vm/session-lifecycle.py` 可执行；默认跑 30s/60s 短档，正式默认值需显式传参 |
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

- **#15/#16 空闲超时**在 `root-checks.sh` 中仍标 skip；`LONG=1` 不会实际等待。
  使用上面的独立生命周期脚本验证，避免轮询受保护 API 无意间给会话续期。
- **§5 #1 密码不进日志**：严格检验要 `RUST_LOG=trace`；unit 默认 `info`。工装的
  unit 用 `Environment=RUST_LOG=info`，脚本据此在消息里注明「需 trace 才是严格检验」。
- **podman 驱动要 `--privileged`**（2026-09-25 补）。少了它 **Ubuntu 上
  `polkit.service` 起不来**（`Failed to keep CAP_SYS_ADMIN` → `status=217/USER`），
  于是 #7 / #13 / #18 三条全红，而报出来的是
  `Failed to activate service 'org.freedesktop.PolicyKit1': timed out` ——
  离原因很远。实测 `--cap-add=SYS_ADMIN` 单独不够（接着卡在 cgroup 委派，
  `memory.pressure Permission denied` / `226/NAMESPACE`），配 `--cgroupns=private`
  或 `unmask=/sys/fs/cgroup` 也不行，只有 `--privileged` 成。
  roadmap/07 §1.1 本来就写着「以 `--privileged` 或至少 `--cap-add SYS_ADMIN`」，
  docker 驱动一直照做，podman 驱动漏了——**而 CI 跑的是 docker 那条**，
  所以这个洞一直没被发现。Rocky 9 不受影响（它的 polkit.service 没有那几条
  hardening）：同一个缺陷在两个发行版上表现完全不同，这正是矩阵的意义。

- **agent-checks 的重连测试**把「停 3 分钟」压成一次 `systemctl restart`：补发逻辑
  与停机时长无关（水位由 Server 已有的最大 ts 决定），一次重启足以验证无空洞。
