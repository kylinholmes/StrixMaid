# Cockpit 功能全景梳理与 StrixMaid 对标

> 目的：在设计 StrixMaid 之前，穷尽 Cockpit 的功能面与架构，逐项决定「抄 / 改 / 不做」。
> 数据来源：Cockpit 官方 applications 页、`cockpit-project/cockpit` 仓库 `pkg/` 目录树、官方 Guide。
> 整理日期：2026-08-27。对应 Cockpit 主线版本约 v351+。

---

## 0. 结论速览

Cockpit 共约 **110 个可辨识功能点**，分布在 13 个域。其中：

| 判定 | 数量 | 说明 |
|---|---|---|
| **P0** MVP 必做 | ~28 | 概览、服务、日志、指标、进程、终端、文件、底座 |
| **P1** 第二阶段 | ~22 | 网络、基础存储、账户、容器、软件更新 |
| **P2** 后续 | ~18 | 高级存储、防火墙、多节点、插件体系 |
| **不做** | ~42 | 虚拟机、SELinux、kdump、sosreport、全部发行版特有插件 |

Cockpit 投入产出比最差的部分（存储 13 项 + 虚拟化 + 30 个发行版插件）恰好是它最重、最难移植的部分，也正是 StrixMaid 应当放弃的部分。

---

## 1. 平台底座（架构层）

| # | 功能点 | Cockpit 实现 | StrixMaid 对标 |
|---|---|---|---|
| A1 | Web 服务 | `cockpit-ws`，systemd socket 激活，不登录零常驻 | **改**：单二进制常驻（需要常驻采集），目标空闲 RSS < 20MB |
| A2 | 会话后端 | `cockpit-bridge`，每登录会话 fork 一进程，以该用户身份运行 | **改**：单进程 + AgentCore，权限模型见 §14 |
| A3 | 通道协议 | WebSocket 多路复用，通道类型：`dbus` `fsread1` `fswrite1` `fswatch1` `spawn` `stream` `http-stream` `metrics1` `null` `echo` | **改**：REST + SSE 为主，WS 仅用于终端与文件传输 |
| A4 | 认证 | PAM 密码 / Kerberos GSSAPI SSO / 客户端证书 / Bearer token / PAM 2FA | **P0 子集**：见 §14 |
| A5 | 提权 | sudo / polkit，UI 顶部「管理访问」开关 | **P0** 概念保留，实现方式待定 |
| A6 | 多主机 | 通过 SSH 连到目标机再起一个 bridge；新版默认关闭（`AllowMultiHost`） | **重做**：Server + Agent，共享 AgentCore，HTTP API 聚合，不用 SSH 套娃 |
| A7 | 扩展模型 | 静态前端目录 + `manifest.json` 放入 `/usr/share/cockpit/` 即成为页面 | **P2** |
| A8 | 分发形态 | 发行版包 rpm/deb、`cockpit/ws` 容器（跳板机）、`cockpit-client` 桌面 Flatpak | **改**：单静态二进制（musl），x86_64 + aarch64 |
| A9 | 定制 | branding、`cockpit.conf`、Origins/CORS、TLS、空闲超时 | **P1** |

## 2. 概览页（`pkg/systemd/overview-cards`）

| # | 卡片 | 具体内容 | StrixMaid |
|---|---|---|---|
| B1 | 健康 | 失败服务数、待重启、SMART 异常、非正常关机、SELinux 告警聚合 | **P0**（去掉 SELinux 项） |
| B2 | 系统信息 | 主机名、发行版、硬件型号、资产标签、machine-id、运行时长、SSH 主机指纹 | **P0** |
| B3 | 用量 | CPU / 内存 实时曲线 | **P0** |
| B4 | 配置 | 主机名、域加入（realmd）、时间与时区/NTP、性能配置（tuned）、加密策略、安全启动 | **P0** 仅主机名/时间时区；realmd/tuned/crypto-policies **不做** |
| B5 | 杂项 | MOTD、上次登录、关机重启状态、Red Hat Insights 注册 | **P0** MOTD/上次登录/关机重启；Insights **不做** |

## 3. 系统与服务（`pkg/systemd`）

| # | 功能点 | StrixMaid |
|---|---|---|
| C1 | systemd units 列表：service / target / socket / timer / path，启停、启用、禁用、屏蔽 | **P0** |
| C2 | unit 详情：状态、依赖关系图、unit 文件原文 | **P0** |
| C3 | 图形化创建 systemd timer | **P1** |
| C4 | journald 日志：优先级 / 时间范围 / 服务 / boot 过滤，日志详情，abrt 崩溃上报 | **P0**（abrt 不做） |
| C5 | 浏览器内终端（xterm.js，可选 shell） | **P0** |
| C6 | 硬件信息：PCI 设备、内存 DIMM 插槽（lshw / dmidecode） | **P1**，改为直读 `/sys/bus/pci` + DMI，不依赖外部命令 |
| C7 | *(Cockpit 缺失)* 独立进程管理页 | **P0 新增** — Cockpit 只在指标页给 Top N，无进程树、无 kill/renice |

## 4. 性能指标（`pkg/metrics`）

| # | 功能点 | Cockpit 实现 | StrixMaid |
|---|---|---|---|
| D1 | 历史指标曲线（CPU/内存/磁盘/网络） | **依赖 PCP + pmlogger 另装**，未装则只有实时值 | **P0**，AgentCore 内建常驻采集，无外部依赖 |
| D2 | 异常事件高亮（CPU 尖峰、内存压力、磁盘饱和） | PCP 派生指标 | **P1** |
| D3 | Top 资源消耗进程 / cgroup | PCP | **P0**，直读 `/proc` |
| D4 | 导出到 Grafana / Redis | pmproxy | **P2**，改为暴露 Prometheus `/metrics` 端点 |

## 5. 存储（`pkg/storaged`）

> Cockpit 全域走 **udisks2 / DBus**，是其最重、最难移植的模块。

| # | 功能点 | StrixMaid |
|---|---|---|
| E1 | 磁盘 / 分区 / 分区表 | **P1** 只读 |
| E2 | 文件系统、挂载点、fstab | **P1** 只读 + 挂载/卸载 |
| E3 | LUKS 加密、密钥槽、Tang/Clevis 网络解锁 | **不做** |
| E4 | LVM2（PV / VG / LV / 精简池） | **P2** |
| E5 | mdraid 软 RAID | **P2** |
| E6 | Stratis 池（v2） | **不做** |
| E7 | btrfs 子卷 / 快照 | **P2** |
| E8 | swap | **P1** 只读 |
| E9 | NFS 挂载客户端 | **P2** |
| E10 | iSCSI 发起端 | **不做** |
| E11 | multipath 多路径 | **不做** |
| E12 | SMART 健康 | **P1**，直读 `/dev` SMART，不依赖 smartmontools |
| E13 | 存储任务队列、存储日志、legacy VDO、Anaconda 嵌入模式 | **不做** |

## 6. 网络（`pkg/networkmanager`）

> Cockpit 全域走 **NetworkManager / DBus**，在服务器上 NM 常常并未安装。

| # | 功能点 | StrixMaid |
|---|---|---|
| F1 | 接口列表、IPv4/IPv6 设置（DHCP/静态）、MAC、MTU | **P1**，netlink 直连 |
| F2 | bond / bridge / team / VLAN | **P2** |
| F3 | WireGuard | **P2** |
| F4 | firewalld 区域 / 服务 / 端口 | **P2**，改为直接操作 nftables |
| F5 | 端口转发（v351 新增） | **P2** |
| F6 | 流量曲线、网络相关日志 | **P0** 流量曲线（AgentCore 采集） |
| F7 | **检查点回滚**：改网络配置失联后自动回滚 | **P1 必抄** — 这是 Cockpit 最值得借鉴的设计 |

## 7. 账户（`pkg/users`）

| # | 功能点 | StrixMaid |
|---|---|---|
| G1 | 用户增删改、密码设置、密码过期策略 | **P1** |
| G2 | 组管理、管理员角色 | **P1** |
| G3 | SSH `authorized_keys` 管理 | **P1** |
| G4 | 账户锁定、活动会话查看与踢出 | **P1** |

## 8. 软件与更新

| # | 功能点 | Cockpit 实现 | StrixMaid |
|---|---|---|---|
| H1 | 更新检查 / 应用、安全更新分类、更新历史、需重启提示 | PackageKit / DBus | **P1**，直调 apt / dnf / apk 的命令行 wrapper |
| H2 | 自动更新配置 | dnf-automatic | **P2** |
| H3 | Applications 页：AppStream 安装 Cockpit 插件 | AppStream | **不做** |

## 9. 安全与合规

| # | 功能点 | StrixMaid |
|---|---|---|
| I1 | SELinux 告警、修复建议、enforcing 切换、autorelabel | **不做** |
| I2 | 系统加密策略（crypto-policies） | **不做** |
| I3 | 证书管理（cockpit-certificates） | **不做** |
| I4 | 会话录制与回放（tlog） | **不做** |
| I5 | SCAP 合规扫描（第三方） | **不做** |

## 10. 诊断

| # | 功能点 | StrixMaid |
|---|---|---|
| J1 | kdump 配置与验证 | **不做** |
| J2 | sosreport 诊断报告生成与下载 | **不做**（可考虑自研极简「一键诊断包」） |

## 11. 虚拟化与容器

| # | 功能点 | StrixMaid |
|---|---|---|
| K1 | `cockpit-machines`：KVM/libvirt 虚拟机 — 创建、快照、存储池、虚拟网络、VNC/SPICE 控制台、迁移、内核启动参数 | **不做** |
| K2 | `cockpit-podman`：镜像、容器、Pod、卷、容器日志、容器内终端 | **P1**，抽象成统一 Container Provider，同时覆盖 podman / docker |
| K3 | 第三方：Docker、Docker Compose、systemd-nspawn | **P2** compose |

## 12. 文件（`cockpit-files`）

| # | 功能点 | StrixMaid |
|---|---|---|
| L1 | 浏览、上传、下载、重命名、删除、权限、隐藏文件、搜索、在线编辑 | **P0** |

## 13. 发行版 / 厂商特有插件（约 30 个）

ostree、tukit、repos、subscriptions、packages、bootloader、389-DS、Pacemaker HA、Image Builder、ZFS、Samba/NFS 共享、sensors、Tailscale、Headscale、Cloudflared、BIND、sudo 策略、Backblaze B2 备份、下载管理、IPTV、ROS2、pacman…

**StrixMaid 全部不做。** 这类需求应由 P2 的插件机制承接。

---

## 14. Cockpit 的架构弱点（StrixMaid 的机会）

### 14.1 重度依赖 DBus 系统服务

Cockpit 的功能几乎全部是「给某个 DBus 系统服务做 UI」：

| 模块 | 依赖的外部服务 |
|---|---|
| 存储 | `udisks2`（+ `stratisd`、`lvm2`、`mdadm`） |
| 网络 | `NetworkManager`、`firewalld` |
| 软件更新 | `PackageKit` |
| 虚拟机 | `libvirtd` |
| 域加入 | `realmd` |
| 性能配置 | `tuned` |
| 历史指标 | `PCP` / `pmlogger` |
| 加密策略 | `crypto-policies` |

后果：安装 Cockpit 会拖入一大堆依赖；在 Alpine、Debian minimal、容器、嵌入式环境中大面积失效。**这是 Cockpit「不通用」的根本原因。**

> StrixMaid 原则：默认假设系统中除 systemd（或 initd）之外**什么都没有**。所有信息从 `/proc`、`/sys`、netlink、`/etc` 直读。

### 14.2 没有常驻采集 → 没有历史数据

优点是不登录时零开销；代价是**打开面板只能看到实时值**，要历史曲线必须另装 PCP（又一个重依赖）。这是 Cockpit 在现代监控体验上最大的短板。

> StrixMaid：AgentCore 常驻采集，内存环形缓冲 + SQLite 降采样落盘。

### 14.3 多机是 SSH 隧道套娃

Cockpit 连接其他主机的方式是通过 SSH 登录目标机、在目标机上再启动一个 `cockpit-bridge`。慢、安全模型有缺陷（凭据在中间机停留），新版本已默认禁用。Cockpit 实质上是纯单机工具。

> StrixMaid：Server / Agent 双形态共享 AgentCore，走 HTTP API，天然支持聚合。

---

## 15. StrixMaid 已确定的设计基线

| 维度 | 决策 | 状态 |
|---|---|---|
| **产品名** | StrixMaid。crate 前缀 `strixmaid-`，二进制 `strixmaid` / `strixmaid-agent` | 已定 |
| **拓扑** | 单节点优先。`StrixMaid Server = UI + AgentCore + APIServer`，`StrixMaid Agent = Client + AgentCore`。**Server 自身也包含完整 Agent 能力**（可直接管理本机） | 已定 |
| **构建产物** | Server 与 Agent 分开编译，`ui` feature 控制是否嵌入前端；Server 默认嵌入 | 已定 |
| **目标环境** | 第一版 Linux + systemd。服务/网络/包管理等抽象为 provider trait，预留 OpenRC / initd wrapper | 已定 |
| **外部依赖** | 默认假设不存在。仅允许依赖「几乎必然存在」的基础设施（systemd / journalctl）。信息优先从 `/proc`、`/sys`、netlink 获取 | 已定 |
| **能力探测** | 启动期 `probe()` 每个 provider，结果经 API 暴露，前端隐藏不可用页面（优雅降级，而非 Cockpit 的报错） | 已定 |
| **systemd 接入** | zbus 走 `org.freedesktop.systemd1` 为主（有属性信号与 job 事件），连不上 bus 时降级 `systemctl` 子进程 | 已定 |
| **journald 接入** | `journalctl -o json` 子进程流式读。libsystemd FFI 会毁掉静态构建，纯 Rust 解析 `.journal` 格式不成熟 | 已定 |
| **轻量定义** | Rust 实现、逻辑自包含、无外部服务依赖、单二进制 | 已定 |
| **实时通道** | WebSocket（协议自行定义，不参考 Cockpit 的多路复用设计） | 已定 |
| **MVP 范围** | P0：概览、服务、日志、指标历史、进程、终端、文件、底座与认证 | 已定 |
| **认证模型** | 待定 — PAM 与静态 musl 冲突，见 §16 | **未定** |
| **持久化选型** | 待定 — sea-orm / sqlx / rusqlite | **未定** |

## 16. 认证方案的硬约束（调研结论）

1. **不存在纯 Rust 的 PAM 实现。** `pam` / `pam-sys` / `pam-client` / `pamsm` 全部是 libpam 的 FFI binding。原理上也不可能有：PAM 的核心行为就是 dlopen `/etc/pam.d/` 中配置的 `pam_*.so`（pam_unix、pam_sssd、pam_google_authenticator 等 C 动态库），重写 libpam 核心并不能摆脱对这些模块的动态加载。
2. **静态链接的 musl 不支持 dlopen** —— 其 `dlopen` 是永久返回 NULL 的 stub，属于 musl 的设计决定。因此「静态 musl 二进制 + 运行时 dlopen libpam」不可行。
3. 直读 `/etc/shadow` 自行校验密码可以纯 Rust 实现，但会绕过 PAM 的全部策略（账户锁定、密码有效期、2FA、LDAP/SSSD 集成），且 Ubuntu 24.04 起默认 yescrypt 的纯 Rust 实现并不成熟。

**候选落地方案：** (a) 放弃静态 musl 改用低版本 glibc 动态链接；(b) 独立的动态链接 PAM helper 子进程 + Unix socket，主二进制保持静态；(c) 直读 shadow；(d) 仅自建用户库。

---

> 本文档的对标结论已落实为设计基线，详见 [`design.md`](./design.md)。
