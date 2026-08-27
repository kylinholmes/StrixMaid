# 08 指标裁剪与性能面板

## 1. 目标

两件事，必须一起做，分开做任何一半都会退化：

1. **把采集项从 58 种裁到 34 种**（含新增的 GPU 四条）。裁剪按三条可复述的规则执行，不是凭感觉挑冷门项删。
2. **按 Windows 10 任务管理器的密度重做性能面板**：少数几个入口，每个入口一张大图加六个数字；同类资源收进**资源组**，组页与详情页共用同一个骨架。

裁剪的动机不是采集成本（读几个 `/proc` 文件而已），是**面板上没有主次**——一次铺两百条曲线等于什么都没显示。

本文件供后续实施者（人或 AI）直接执行。配套的**可交互样稿**见 `08-metrics-and-panel.mockup.html`（自包含单文件，双击即可打开，内联了 uPlot v1.6.32）。样稿里的每一条曲线都是真实的 uPlot 实例，不是示意图；所有尺寸、阈值、配色都可以从中直接量取。

## 2. 现状

| 项 | 位置 | 状态 |
|---|---|---|
| 指标常量表 | `crates/strixmaid-core/src/metrics/catalog.rs` | `CATALOG: &[MetricDef]`，58 种；`MetricDef { name, unit, desc, labels }` |
| 采集器 | `crates/strixmaid-core/src/metrics/collect/linux/{cpu,mem,load,psi,disk,fs,net}.rs` | 按 `catalog` 常量产出 |
| macOS 采集器 | `.../collect/macos/{cpu,mem,load,fs,net}.rs` | 开发平台实现，无 `disk` / `psi` |
| 每核明细开关 | `config.rs:293` `per_core_detail: bool`（默认 `false`） | 关闭时每核只留 `cpu.core.usage` |
| 存储与聚合 | `metrics/{ring,engine,scheduler}.rs`、`store/` | 五层桶，`design.md` §7.2 |
| 静态信息 | `strixmaid-types/src/system.rs` | `SystemInfo{ cpu, memory, disks, filesystems, … }` |
| 健康聚合 | `HealthReport` / `HealthItem` | `HealthItem.id` 文档里**已预留** `"disk.inodes"`，尚未实现 |
| 前端 | `web/` | 只有 `dist/index.html` 占位，从零起 |

现状的三个具体问题：

- 每核 9 条 × N。128 核机器开了明细就是 1152 条 series，而面板上没人看第 97 核的 softirq 历史。
- 存了大量**派生量**：`fs.usage` = `used/total`，`mem.swap_free` = `swap_total − swap_used`，`cpu.idle` = 100 减其余。
- 存了**恒零列**：`psi.cpu.full` 在整机层面内核没有定义，只在 cgroup 层有意义。

## 3. 设计约束

- `design.md` §1：除 systemd 外默认假设系统里什么都没有，优先从 `/proc`、`/sys` 直读。
- `design.md` §2：**静态链接 musl 的二进制不能 `dlopen`**。任何需要动态库的采集必须放进 `strixmaid-helper`。这条直接决定 NVIDIA GPU 的接入方式（见 §12 未决事项 Q1）。
- `design.md` §6：能力探测而非硬依赖，探测不到的能力在 API 中标 unavailable，前端隐藏对应页面。
- `design.md` §7.2：五层桶、`cnt/min/max/sum/med`、查询自动选层。**本方案不触碰存储与聚合层**——它们对 series 的名字无感。
- `design.md` §7.5：展示形式是 uPlot band 系列（min–max 区间带 + avg 实线 + med 虚线）。本方案在样稿中已实测该形态。
- `design.md` §11：**「节点」一词已被占用**，指一台被纳管的机器。资源组内的成员在文档里称「成员」，界面上一律用资源自己的名词（逻辑处理器 / 块设备 / 显卡 / 接口），**不得叫「节点」**。

## 4. 采集项裁剪

### 4.1 三条规则

裁掉的每一条都能指回其中一条：

1. **派生量不存。** 存一条能算出来的曲线，等于白扔一整条 series 的五层桶。前端做一次除法的成本是零。
2. **同向计数器合成一条异常信号。** 四条平时全零、出事一起涨的计数器，回答的是同一个问题，合成一条即可。
3. **慢变量不进时序库。** 一天动不了一个百分点的量是**健康检查项**，不是曲线。

### 4.2 新 `CATALOG`（34 种）

`unit` 取值沿用 `catalog::unit`，`labels` 沿用 `catalog::label`。

| 组 | 指标名 | 单位 | 标签 | 说明 |
|---|---|---|---|---|
| CPU 总 | `cpu.usage` | percent | — | `100 − idle − iowait`，主曲线 |
| | `cpu.system` | percent | — | 内核态；面板上作为主曲线下方的深色填充 |
| | `cpu.iowait` | percent | — | 等待 IO 的空闲时间 |
| | `cpu.irq` | percent | — | **语义变更**：`irq + softirq` 之和 |
| | `cpu.steal` | percent | — | 探测到非虚拟化时整条隐藏 |
| CPU 每核 | `cpu.core.usage` | percent | `core` | 唯一保留的每核指标 |
| GPU | `gpu.usage` | percent | `gpu` | **新增** |
| | `gpu.mem_used` | bytes | `gpu` | **新增** |
| | `gpu.mem_total` | bytes | `gpu` | **新增** |
| | `gpu.temp` | celsius | `gpu` | **新增**；`unit` 需新增常量 `CELSIUS = "celsius"` |
| 内存 | `mem.total` | bytes | — | |
| | `mem.used` | bytes | — | |
| | `mem.available` | bytes | — | Linux 上唯一能回答「还能开多大进程」的数 |
| | `mem.cached` | bytes | — | **语义变更**：`cached + buffers` 之和 |
| | `mem.swap_total` | bytes | — | |
| | `mem.swap_used` | bytes | — | |
| 负载与进程 | `load.1m` | — | — | 只作数字展示，不画曲线 |
| | `procs.total` | count | — | |
| | `procs.running` | count | — | 运行队列长度 |
| PSI | `psi.cpu.some` | percent | — | |
| | `psi.memory.some` | percent | — | |
| | `psi.memory.full` | percent | — | |
| | `psi.io.some` | percent | — | |
| | `psi.io.full` | percent | — | |
| 磁盘 | `disk.util` | percent | `dev` | 「活动时间」 |
| | `disk.read_bytes` | bytes/s | `dev` | |
| | `disk.write_bytes` | bytes/s | `dev` | |
| | `disk.await` | ms | `dev` | 「平均响应时间」 |
| | `disk.iops` | iops | `dev` | **新增名**：`read_iops + write_iops` 之和 |
| 文件系统 | `fs.used` | bytes | `mount` | |
| | `fs.total` | bytes | `mount` | |
| 网络 | `net.rx_bytes` | bytes/s | `iface` | |
| | `net.tx_bytes` | bytes/s | `iface` | |
| | `net.errors` | count/s | `iface` | **新增名**：`rx_errors + tx_errors + rx_drops + tx_drops` 之和 |

### 4.3 删除清单（共 30 个名字）

`store` 的一次性迁移按这份名单执行，不要按前缀匹配。

```
cpu.user            cpu.nice            cpu.idle            cpu.softirq
cpu.core.user       cpu.core.nice       cpu.core.system     cpu.core.idle
cpu.core.iowait     cpu.core.irq        cpu.core.softirq    cpu.core.steal
mem.free            mem.buffers         mem.dirty           mem.swap_free
load.5m             load.15m
psi.cpu.full
disk.read_iops      disk.write_iops
fs.usage            fs.inodes_used      fs.inodes_total
net.rx_packets      net.tx_packets      net.rx_errors       net.tx_errors
net.rx_drops        net.tx_drops
```

逐条理由：

| 指标 | 规则 | 理由 |
|---|---|---|
| `cpu.user` | 一 | `user + nice = usage − system − irq − steal`，**恒等式而非近似**（各档时间占比之和恒为 100） |
| `cpu.nice` | 一 | 并入 user 一起显示；现代系统上几乎恒为 0 |
| `cpu.idle` | 一 | `100 − 其余` |
| `cpu.softirq` | 二 | 并入 `cpu.irq`。硬/软中断要分开看的场合只有网卡多队列调优，那时该上 `perf` |
| `cpu.core.*`（8 条） | — | 面板上没人看第 97 核的 softirq 历史。**连带删除 `metrics.per_core_detail` 配置项**——它存在的唯一理由就是开关这 8 条 |
| `mem.free` | 一 | 且它**对 Linux 是误导数字**：free 少不等于内存紧张，那是 page cache 在干活 |
| `mem.buffers` | 二 | 并入 `mem.cached`；现代内核里它是 page cache 的一小块，单列不产生决策差异 |
| `mem.dirty` | — | 写回排障的深水区；真出问题时 `psi.io` 叫得更早 |
| `mem.swap_free` | 一 | `swap_total − swap_used` |
| `load.5m` / `load.15m` | — | **它们本来就是 `load.1m` 的移动平均**。内核替我们平滑，是因为 `uptime` 没有历史；而我们存着五层完整曲线，趋势直接看图 |
| `psi.cpu.full` | — | 内核在整机层面对 cpu 的 `full` 没有定义，恒输出 0；只在 cgroup 层有意义 |
| `disk.{read,write}_iops` | 二 | 方向已由两条 bytes 给出；IOPS 在面板上只回答「打满的是大 IO 还是小 IO」，合计值足够 |
| `fs.usage` | 一 | `used / total` |
| `fs.inodes_*` | 三 | 见 §4.4 |
| `net.{rx,tx}_packets` | — | 唯一用途是除出平均包长判断小包攻击，那是抓包该干的事 |
| `net.{rx,tx}_{errors,drops}` | 二 | 合成 `net.errors`。非零即异常，面板打红角标；要分方向 `ip -s link` 一条命令 |

### 4.4 inode 移出时序库

**这一项几乎零成本，两侧都已就位：**

- `FilesystemInfo` 已经带 `inodes_total: Option<u64>` / `inodes_used: Option<u64>`，数据已在 `GET /api/v1/system/info` 里；
- `HealthItem.id` 的文档里**已经预留了 `"disk.inodes"`** 这个 id。

所以本项只需：从 `CATALOG` 删两条、在 health provider 里实现 `disk.inodes` 检查项。阈值与 `disk.usage` 对齐（建议 warning ≥ 80%，serious ≥ 90%）。

### 4.5 数量与存储

按 `design.md` §7.4 的样机（16 核 / 4 盘 / 2 网卡 / 3 挂载点，每核明细开启）另加 1 块 GPU：

| | 旧 | 新 |
|---|---|---|
| 指标种类 | 58 | 34（其中 4 条为新增的 GPU） |
| series 实例 | 9 + 144 + 10 + 5 + 6 + 24 + 15 + 16 = **229** | 5 + 16 + 4 + 6 + 3 + 5 + 20 + 6 + 6 = **71** |
| Normal 预设占用 | ≈100 MB | ≈35 MB |
| Less 预设占用 | ≈35 MB | ≈12 MB |

128 核机器的每核部分：1152 → 128。

`design.md` §7.4 的「约 200 个 series / Normal 约 100MB」需按上表改写。

## 5. 静态拓扑：资源组的全部依据

**资源组不新增任何 series。** 分组、排序、类型标记全部靠 `GET /api/v1/system/info` 的静态字段。需要新增四项：

### 5.1 `FilesystemInfo.backing_dev: Option<String>`

挂载点 → 块设备。取法：`/proc/self/mountinfo` 第三列是 `major:minor`，用它读 `/sys/dev/block/<maj>:<min>`，符号链接的**父目录**即整盘目录（分区在整盘目录之下）。这条路径能正确处理 dm-\*、md\*、loop；直接截字符串（`nvme0n1p2` → `nvme0n1`）不能，会在 `dm-0` 上失败。

伪文件系统（tmpfs / nfs / overlay）为 `None`，界面上显示「无块设备」，速率列显示 `—`。**这是事实不是缺陷。**

### 5.2 `CpuInfo.packages: Vec<CpuPackage>`

```rust
pub struct CpuPackage {
    /// 物理封装 id，来自 topology/physical_package_id。
    pub id: u32,
    /// 属于该封装的逻辑处理器编号，升序。
    pub logical_cores: Vec<u32>,
}
```

取自 `/sys/devices/system/cpu/cpu<N>/topology/physical_package_id`。读不到时退化为单个 package 包含全部逻辑核。

### 5.3 `SystemInfo.gpus: Vec<GpuInfo>`

```rust
pub struct GpuInfo {
    /// DRM 卡名，如 "card0"。
    pub card: String,
    pub model: Option<String>,
    pub driver: Option<String>,        // "amdgpu" / "i915" / "nvidia"
    pub vram_bytes: Option<u64>,
    pub bus: Option<String>,           // "PCIe 4.0 x16"
    /// 指标来自哪条路径，决定了哪些指标可用。
    pub source: GpuSource,             // Sysfs | Nvml | Unavailable
}
```

枚举 `/sys/class/drm/card*`，能读到 `device/gpu_busy_percent` 才算 `Sysfs` 可用。

### 5.4 `SystemInfo.networks: Vec<NetInfo>`

```rust
pub struct NetInfo {
    pub name: String,
    pub mac: Option<String>,
    /// /sys/class/net/<n>/speed，Mb/s。虚拟网卡、bond、无线常返回 -1 → None。
    pub speed_mbps: Option<u32>,
    pub duplex: Option<String>,
    pub mtu: u32,
    pub carrier: bool,
    pub driver: Option<String>,
    pub addrs: Vec<String>,
}
```

**目前 `SystemInfo` 里完全没有网卡列表**，这一整块是从零加。`speed_mbps` 的唯一用途是把吞吐归一成链路利用率，读不到时界面显示绝对吞吐（见 §6.5）——**不是阻塞项**。

### 5.5 不需要新增的

- **磁盘类型标记（NVMe / SSD / HDD）**：`DiskInfo.rotational` 已存在。`rotational == true` → HDD；否则名字以 `nvme` 开头 → NVMe，其余 → SSD。
- **SMART**：`DiskInfo.smart_healthy` 已存在（P0 通常为 `None`）。

## 6. 页面模型

### 6.1 两类页面与退化规则

```
组页 ── 聚合图 ── 成员区 ── [资源专有表] ── 数字 | 静态事实
                    └→ 详情页（仅当成员有内容可看）

详情页 ── 图 ── [资源专有表] ── 数字 | 静态事实 ── 表格视图
```

**组页与详情页是同一个骨架。** 成员区在组页里的位置，正对应磁盘详情页里挂载点表的位置——都是「这一页下面挂着什么」，都夹在图和文本块之间。

**唯一的例外是 CPU**：它用页内切换而不是把总体与成员同屏。理由是这两者在 CPU 上是**同一个指标的两个粒度**，同屏是重复；而在组页上，聚合与成员是不同的信息（组总吞吐 vs 谁在忙），必须同屏。

层数不是设计出来的，是**「成员有没有身份」**推出来的：

| 资源 | 成员 | 成员有身份？ | 详情页 | 成员视图 |
|---|---|---|---|---|
| CPU | 逻辑处理器 | 无（只有编号） | **无**，退化成一层 | 页内 `[总体｜逻辑处理器]` **切换**，不与总体同屏（见 §6.6）；≤32 名字角标网格，>32 热力图 |
| GPU | 显卡 | 有 | 有 | 成员格 |
| 磁盘 | 块设备 | 有 | 有 | 成员格 |
| 网络 | 接口 | 有 | 有 | 成员格 |
| 内存 | — | — | — | 无成员区 |

**同一个属性同时决定层数与成员视图形态**，所以 CPU 不是特例：`cpu137` 没有型号、没有 SMART、没有挂载点，压根没有一页内容可写，名字也不必印在格子上。

两条退化规则（样稿中已实现，可用参数面板验证）：

- **成员数 = 1 → 不是组**：左栏直接是那个成员的行，无计数徽标、无展开钮，点进去就是详情页。
- **成员数 = 0 → 整类隐藏**：对应能力探测不到的情形。

### 6.2 聚合语义

组头与聚合图的数字**必须分类算**，这是本方案里最容易实现错的一处：

| 类别 | 聚合 | 指标 |
|---|---|---|
| **速率** | **求和** | `disk.read_bytes`、`disk.write_bytes`、`disk.iops`、`net.rx_bytes`、`net.tx_bytes`、`net.errors` |
| **饱和度** | **取最大，并同时显示是哪个成员** | `disk.util`、`gpu.usage`、`cpu.core.usage` |
| **容量** | 求和 | `fs.used`、`fs.total`、`gpu.mem_used`、`gpu.mem_total` |
| **不可合计** | 留空，显示「不可合计」 | `disk.await` |

理由：

- 三块盘 25% / 73% / 1%，**平均 33% 会把一块打满的盘藏进两块闲盘里**。饱和度求平均或求和都是错的。
- `disk.await` 是每次 IO 的平均等待，把三块盘的平均值再平均一次没有物理意义。**宁可空着。**
- 异构不影响聚合合法性：NVMe 的 3 GB/s 加 HDD 的 120 MB/s，和就是总吞吐。异构的账记在**成员视图的可比性**上（见 §6.4），不在聚合上。

组页聚合图的标题必须说清是哪种聚合：写「**最忙设备 · disk.util（组内取最大）**」，不能写「`disk.util`」。注意 `cpu.usage` 是 `/proc/stat` 直接给的真值、不是各核平均，与组页的聚合图**看着一样但语义不同**——标签是唯一的区分手段。

### 6.3 挂载点表：默认折叠

收起时只有一行合计，点它（或 Enter / 空格）展开：

```
▸ 全部挂载点   7 个 · 3 块设备   [====      ]  66% 11.87 TB / 17.90 TB   331.1 MB/s   326.2 MB/s
```

**合计的读写按去重后的后端设备求和。** `/`、`/var`、`/boot/efi` 同在 `nvme0n1` 上，把三行的速率相加会把同一份 IO 数三遍。第二列写的是「7 个 · **3 块设备**」，分母是设备不是挂载点。

出现位置两处，同一个组件：

- **磁盘组页**：全部挂载点（含无块设备的 tmpfs / nfs，速率列 `—`）。跨设备找「哪个文件系统要满了」在这里一眼看完。
- **磁盘详情页**：只有这块盘上的挂载点。挂载点列在它所在的那块盘里，**「共享设备级 IO 计数器」这件事就不言自明，不需要在界面上解释**。

展开后每行整行可点，跳到承载它的那块盘；无块设备的行不可点。

### 6.4 成员格（组页的成员区）

格子规格（样稿实测值）：

| 项 | 值 |
|---|---|
| 高度 | 82px（含底部 3px 压力带） |
| 最小宽 | 132px，`grid-template-columns: repeat(auto-fill, minmax(132px, 1fr))` |
| 间距 | 4px |
| 底板 | `color-mix(in oklab, <资源色> 5~7%, <surface>)` |
| 内容 | 左上名字（等宽 11px 粗体）· 右上类型标记（9px 描边小签）· 左下大号当前值（20px）· 满格背景是一张 uPlot 小图 · 底部 3px PSI 压力带 |
| 交互 | 整格可点 → 详情页；`:hover` 边框转资源色；键盘可聚焦 |

**类型标记是必须的，不是装饰。** 异构机器里 `sda|HDD|87%` 和 `nvme1n1|NVMe|87%` 色深一样，但严重程度差一个量级；没有标记就分不出来。取值来自 `DiskInfo.rotational`（§5.5）、`NetInfo.speed_mbps`（`10G` / `1G`）、GPU 型号简称。

**成员区高度封顶 252px、内部自己滚**（`scrollbar-gutter: stable`）。这是为了让底下那块文本不会被顶到看不见的地方——24 块盘时成员区滚，页面本身不滚。样稿里 60 块盘也验证过。

### 6.5 各资源的成员格数值

| 资源 | 小图画什么 | 大号数字 |
|---|---|---|
| 磁盘 | `disk.util`，y 轴固定 0–100 | `util%` + 小字合计吞吐 |
| GPU | `gpu.usage`，y 轴固定 0–100 | `usage%` + 小字显存 `used/total` |
| 网络 | `net.tx_bytes`，**y 轴自动缩放**到本接口 `max(rx,tx)×1.15` | 合计吞吐；有异常包时追加红色小字 |

网络没有 0–100% 的有界指标，小图自动缩放即可——**这是去掉「热力条」形态后省下来的约束**。若 `NetInfo.speed_mbps` 可用，可额外在详情页显示链路利用率；不可用时不影响任何功能。

### 6.6 逻辑处理器视图（CPU 专有）

CPU 页保留 `[总体 | 逻辑处理器]` 切换（组页没有这个切换器，因为它把聚合与成员同屏放）。

| 逻辑核数 | 渲染 | 实例数 |
|---|---|---|
| ≤ 32 | 每核一张 uPlot 小图，54px 高，按 package 分块 | N |
| > 32 | **单张 canvas 热力图**，一格一个逻辑处理器，颜色 = 当前占用，仍按 package 分块 | **0**（与核数无关） |

阈值 32 的理由：32 格在 8 列下是 4 行，还读得动；再多就既画不下、也没人逐核看历史。而且**这时要回答的问题本来就变了**——不是「这个核过去怎样」，是「现在哪个核烫」。

热力图规格：列数从 `[8,16,32]` 里取满足 `宽度/列数 ≥ 22px` 的最大值；格高 `min(格宽, 30px)`；格间留 1–1.5px 空隙露出底板作为网格线；颜色为**单色相顺序色阶** `rgba(资源色, 0.10 + 0.90 × 占用)`；悬停出核编号与占用。256 核（2 package × 128）实测约 210px 高。

### 6.7 左栏

- 定长五行：CPU / GPU / 内存 / 磁盘 / 网络，每行 = 72×34 小图 + 名字 + 一行摘要 + **3px PSI 压力带**。
- 组行带成员数徽标与 `+` 展开钮。**组默认折叠**，展开把成员平铺为缩进子行。
- 展开钮与行本身是**两个平级按钮**，不能嵌套——`<button>` 套 `<button>` 是非法 HTML，浏览器会把内层拽出外层，布局直接崩。
- 选中行：`--sel` 底色 + 3px 资源色左边条。

**PSI 压力带是本产品相对任务管理器的唯一差异化元素。** 它画 `psi.{cpu,memory,io}.some`，「CPU 20% 但 `io.some` 60%」的机器体感是卡死的，而所有传统面板都显示「一切正常」——这道 3px 的条让这件事在用户点进任何一页之前就已经是红的。阈值：`≥ 12%` 转警告色，`≥ 40%` 转严重色（两者可配）。

**压力带只出现在三行：CPU、内存、磁盘。** 这是硬约束，不是取舍：

- **PSI 只有整机级**。`/proc/pressure/{cpu,memory,io}` 是全系统的，**内核没有按块设备的 pressure**；`io.pressure` 只在 cgroup v2 里按 cgroup 存在，那是按 cgroup 不是按设备，答非所问。
- 因此磁盘**组行**挂全局 `psi.io.some`，而**成员格不画压力带**——每块盘画同一个全局值是误导。
- **GPU 行与网络行不画压力带**，不是画一条恒绿的。恒绿意味着「测过了，正常」，而事实是根本没测。留空即可。

> ⚠️ 样稿在这一点上**故意偏离**：为了让压力带的视觉效果看得见，样稿给磁盘成员格与 GPU / 网络行都画了由 util 推导的假 PSI。**实现时按本节，不要照抄样稿。**

### 6.8 窗口与滚动

- 窗体高度**固定**（样稿 660px），不随内容伸缩。
- 左栏独立滚动。
- 右侧拆两区：`.d-top` 固定（标题 / 面包屑 / 时间范围与视图切换），`.d-body` 滚动。
- `.d-body` 必须设 **`scrollbar-gutter: stable`**。否则内容一超高滚动条弹出、容器宽度骤变，所有 uPlot 图会跟着跳一下宽度。

## 7. 视觉规范

### 7.1 语言

**Windows 10 的原生语言：一律直角、1px 描边、无阴影、Segoe UI。** 圆角是 Windows 11 才引入的（标准控件 4px），Win10 全部为 0。实现上用 `*{border-radius:0}` 兜底，并额外压掉 uPlot 自带的 `.u-cursor-pt{border-radius:50%}`。

等宽字**只给指标名、设备名、路径**——那些确实是代码；其余一律 Segoe UI，数字加 `font-variant-numeric: tabular-nums`。

```
--f-ui   : "Segoe UI","Segoe UI Variable","Microsoft YaHei UI","Microsoft YaHei",
           "PingFang SC","Hiragino Sans GB","Noto Sans CJK SC",system-ui,sans-serif
--f-mono : Consolas,"Cascadia Mono",ui-monospace,"SF Mono",Menlo,monospace
```

### 7.2 配色

中性色**一律偏青，不用纯灰**；图表底板与进度槽按所属资源的色相染 5–7%（`color-mix`），所以每一页的底色都轻微不同——不是装饰，是让用户切页时立刻知道自己在哪一类资源里。

| token | 亮色 | 暗色 |
|---|---|---|
| `--ground` | `#EBF0F0` | `#141A1B` |
| `--surface` | `#FFFFFF` | `#1E2526` |
| `--surface-2` | `#F2F6F6` | `#262F30` |
| `--surface-3` | `#DEE7E7` | `#323C3D` |
| `--line` | `#CBD7D7` | `#354040` |
| `--line-strong` | `#93A3A3` | `#586465` |
| `--ink` | `#0A1414` | `#EEF5F5` |
| `--ink-2` | `#3E4C4C` | `#BAC8C8` |
| `--ink-3` | `#677676` | `#889797` |
| `--accent` | `#0078D7` | `#4CC2FF` |
| `--sel` | `#CCE8FF` | `#0A3A56` |

资源色（**顺序即左栏相邻序，不可随意调换**）：

| 资源 | 亮色 | 暗色 |
|---|---|---|
| CPU | `#0C97AB` | `#12A6BA` |
| GPU | `#3F8632` | `#4E9A3D` |
| 内存 | `#7A5CD4` | `#8A6FE0` |
| 磁盘 · 存储 | `#B06A22` | `#C0762A` |
| 网络 | `#B44579` | `#C25086` |

状态色（保留，不参与类别配色）：`--ok` `#107C41`/`#4BB07A`、`--warn` `#8A6100`/`#D9A227`、`--crit` `#C42B1C`/`#E86B60`。

**顺序为什么不能改：** 绿（GPU）与琥珀（磁盘）是红绿色盲下的经典撞色对，相邻时 deutan ΔE 仅 3.7，远低于 8 的下限。穷举验证过：在固定亮度环上遍历全部五色组合，要让五色在 protan / deutan / tritan 下两两都安全，最好情况的最差 ΔE 也只有 **4.9**——**五个类别色两两全安全是不存在的**。解法是排序：GPU 挪到 CPU 后面（算力放一起也更合逻辑），只与青、紫相邻，避开琥珀。按此相邻序，暗色与亮色两套的五项检查（亮度带 / 彩度下限 / CVD 分离 / 常视觉下限 / 对比度）全部通过。

每一行还都带文字标签，**颜色从来不是唯一的身份编码**。

### 7.3 主题

三态：显式 `data-theme="dark"` / `data-theme="light"`，以及默认的「跟随系统」（不打戳，只由 `prefers-color-scheme` 决定）。CSS 必须写三段：裸 `:root` 定义完整亮色；`@media (prefers-color-scheme:dark)` 内以 `:root:not([data-theme="light"])` 覆盖；`:root[data-theme="dark"]` 再覆盖一次。**任何颜色都不能只定义在 media 或 `[data-theme]` 块里。**

canvas 里的颜色是取色后画进像素的，**主题切换必须重新读取 CSS 变量并重绘**，否则图表颜色不跟着变。

### 7.4 滚动条

方头、浅槽、灰拇指、12px 宽，`scrollbar-width: thin` + `::-webkit-scrollbar*`。出现在三处：左栏、`.d-body`、成员区。

## 8. 图表规格（uPlot）

样稿已验证 uPlot v1.6.32 可满足全部需求，**整页 5–37 个实例、1 Hz 全量 `setData` 无压力**。CSP 禁止外部请求，发布时需把 uPlot 内联或随资源打包，不能引 CDN。

### 8.1 通用配置

```js
{
  width, height, padding: [0,0,0,0],
  legend: { show: false },
  cursor: { x:true, y:false, points:{show:false}, drag:{setScale:false} },
  scales: { x:{time:false}, y:{ range: () => [0, ymax] } },
  axes: [{show:false},{show:false}],      // 轴标注用四角 HTML 覆盖层，不用库的轴
  hooks: {
    drawClear: [ 画 Win10 细网格 ],       // 固定像素网格，不随刻度走
    draw:      [ 画末点方块 ],
    setCursor: [ 定位 HTML tooltip ]
  }
}
```

- **四角标注**（Win10 任务管理器的做法）：左上 = 指标名，右上 = 上限，左下 = 时间跨度，右下 = `0`。绝对定位在 `.plot` 上，`z-index:2`，`pointer-events:none`。
- **网格**：`drawClear` 钩子里画，步长 `max(18, 宽度/18)` px 的固定像素网格，颜色 `rgba(资源色, 0.13~0.14)`。
- **末点**：直角语言里用 **6px 方块**加 1.5px `--surface` 描边，不用圆点。
- **游标线**：`.u-hz .u-cursor-x { border-right: 1px dashed var(--hue) }`，压掉库默认的蓝灰。

### 8.2 band 系列（`design.md` §7.5）

时间范围 ≥ 1 小时时切成聚合层渲染，uPlot 原生支持：

```js
data:   [xs, max, min, avg, med]     // series 索引 1=max 2=min 3=avg 4=med
series: [{}, {width:0, stroke:"rgba(0,0,0,0)"},   // 1 max，不描边
             {width:0, stroke:"rgba(0,0,0,0)"},   // 2 min，不描边
             {stroke:hue, width:2},               // 3 avg 实线
             {stroke:hue, width:1.5, dash:[4,3]}] // 4 med 虚线
bands:  [{ series:[1,2], fill: rgba(hue, 0.20~0.26) }]
```

自动选层与时间跨度的对应（`design.md` §7.2 的三条规则）：

| 范围 | 层 | 渲染 |
|---|---|---|
| 60 秒 | `live` | 原始点，实线 + 面积填充 |
| 1 小时 | `live` | band |
| 24 小时 | `m_1m` | band |
| 7 天 | `m_15m` | band |
| 30 天 | `m_1d` | band |

范围选择器上必须显示当前选中的层名与「区间带 = min–max，实线 = avg，虚线 = med」。

### 8.3 无障碍

- ≥2 条 series 必有图例；单条不需要（标题已经命名了它）。
- 每张图下提供 `<details>` 表格视图，列最近读数。
- 可点击的表格行 / 成员格：`role="button"` + `tabIndex=0` + Enter/空格。
- 尊重 `prefers-reduced-motion`：不跑动画，渲染静态快照。

## 9. 涉及文件

| 文件 | 改动 |
|---|---|
| `crates/strixmaid-core/src/metrics/catalog.rs` | 删 30 个名字常量，加 GPU 一组与 `unit::CELSIUS`；重写 `CATALOG`；给 `MetricDef` 加 `panel: Panel` 字段（`Cpu`/`Gpu`/`Memory`/`Disk`/`Filesystem`/`Network`），**前端按它分组，不再硬编码指标名前缀** |
| `.../collect/linux/{cpu,mem,load,psi,disk,fs,net}.rs` | 停止产出删除项；合并项（`cpu.irq` / `mem.cached` / `disk.iops` / `net.errors`）在采集器里做加法 |
| `.../collect/linux/gpu.rs` | **新文件**。`probe()` 枚举 `/sys/class/drm/card*`，能读到 `device/gpu_busy_percent` 才算可用；`mem_info_vram_{used,total}`；温度走 `hwmon` |
| `.../collect/linux/fs.rs` | 解析 `/proc/self/mountinfo` 的 `major:minor` → `/sys/dev/block/` → 整盘名，填 `backing_dev` |
| `.../collect/macos/{cpu,mem,load,fs,net}.rs` | 同步裁剪；`gpu.rs` 用 IOKit `PerformanceStatistics`，联调时不至于整页空白 |
| `.../collect/mod.rs` · `metrics/engine.rs` · `config.rs` | `default_collectors` 去掉 `per_core_detail` 形参；`MetricsConfig` 删该字段并从 TOML 样例（`config.rs:820`）移除 |
| `crates/strixmaid-types/src/system.rs` | 新增 `CpuPackage`、`GpuInfo`、`GpuSource`、`NetInfo`；`FilesystemInfo` 加 `backing_dev`；`CpuInfo` 加 `packages`；`SystemInfo` 加 `gpus` / `networks` |
| `crates/strixmaid-server/src/routes/system.rs` | 填充上述新字段 |
| `crates/strixmaid-core/src/providers/`（health） | 实现 `disk.inodes` 检查项 |
| `crates/strixmaid-core/src/store/` | 一次性迁移：按 §4.3 名单 `DELETE FROM series` + 五张桶表级联清理，避免老库留下永不更新的孤儿 series |
| `web/` | 前端从零起，按 §6–§8 实现 |
| `docs/design.md` | §6 能力探测清单加 GPU；§7.1 采集项表按 §4.2 改写；§7.2 删 `per_core_detail` 那段论证；§7.4 数量从 200/100MB 改到 71/35MB；新增一节资源组与聚合语义；§11 明确「节点」一词不得用于组内成员 |

## 10. 测试

- `catalog` 的 `CATALOG` 长度与名字集合快照测试，防止误删误增。
- 合并项的算术：给定 `/proc/stat` / `/proc/meminfo` / `/proc/diskstats` / `/proc/net/dev` 样本，断言 `cpu.irq == irq + softirq`、`mem.cached == cached + buffers`、`disk.iops == read + write`、`net.errors == 四项之和`。
- `backing_dev` 映射：对分区、dm-\*、md\*、tmpfs 四种情形断言结果（dm-\* 是最容易写错的一种）。
- GPU probe 在无 GPU 机器上返回空列表且不报错。
- 迁移幂等：连续执行两次，`series` 表行数不变。
- 前端：组页/详情页/CPU 三档核数（16 / 64 / 256）/五个时间档/主题三态，各渲染一遍断言无运行时错误；成员数取 0 / 1 / 2 / 24 验证两条退化规则。

## 11. 验收标准

1. `GET /api/v1/metrics/series` 在样机（§4.5 口径：16 核 / 4 盘 / 2 网卡 / 3 挂载点 / 1 GPU）上返回 71 条，名字集合与 §4.2 完全一致。
2. 老库升级后不再出现 §4.3 名单中的任何 series。
3. 128 核机器的每核 series 数为 128，且 `metrics.per_core_detail` 配置项已不存在。
4. 无 GPU 机器上 GPU 相关能力标为 unavailable，左栏不出现 GPU 行。
5. 面板：组页与详情页结构一致；成员区高度封顶且内部滚动；挂载点表默认折叠且合计的读写按去重设备求和；256 核走热力图且图表实例数与核数无关。
6. 亮暗两套主题下，五个资源色按左栏相邻序通过配色校验的五项检查。
7. `cargo clippy --workspace --all-targets` 零 warning，`cargo test --workspace` 全绿。

## 12. 未决事项

实施前需要决策，**Q1 是唯一会动进程拓扑的**：

| # | 问题 | 选项 | 倾向 |
|---|---|---|---|
| **Q1** | **NVIDIA 怎么接？** amdgpu 走 sysfs 是白捡的；但 NVIDIA 的利用率只有 NVML（`libnvidia-ml.so`）给得出来，而**静态链接 musl 的二进制不能 dlopen** | (a) 下放 `strixmaid-helper`——它本来就是动态 glibc，符合 `design.md` §1 原则四，但要给 IPC 加一组指标帧；(b) 每 2 秒 fork `nvidia-smi --query-gpu`；(c) P0 只支持 sysfs 能读的，NVIDIA 标 unavailable | (a) |
| Q2 | `load.5m` / `load.15m` 真的删？论证成立，但三元组是运维肌肉记忆 | 删 / 保留采集但只在 CPU 页角落显示一行文字 | 删 |
| Q3 | `disk.iops` 合并是否太狠？读写分离的 IOPS 在诊断写放大时有用 | 合并 / 保留两条 | 合并 |
| Q4 | 加不加 `cpu.freq`？任务管理器 CPU 页最显眼的第二个数字，取自 `scaling_cur_freq`，虚拟机上常读不到 | 加（+1 种）/ 不加 | 待定 |
| Q5 | 成员区的排序：24 块盘按什么排？ | 按名字（稳定）/ 按繁忙（有用但行会跳） | 按名字，不给排序开关——颜色已经让最烫的自己跳出来 |
| Q6 | 成员数极多（>32）时组页是否也要转热力图？现方案是 B 形态一路到底靠内滚 | 一路到底 / 加阈值 | 一路到底 |

## 13. 样稿

`08-metrics-and-panel.mockup.html`（自包含单文件，无外部请求，双击打开）。

包含：完整的窗口 mockup（左栏 + 组页 + 详情页 + CPU 逻辑处理器视图）、五个时间档的 band 渲染、亮暗主题切换，以及一个**浮在窗口外**的参数面板，可实时调：

- 资源数量（GPU 0–8 / 磁盘 1–24 / 网卡 1–8 / 挂载点 1–12）——用来验证 §6.1 的两条退化规则
- CPU 逻辑处理器 16 / 32 / 64 / 128 / 256——用来验证 §6.6 的阈值两侧渲染
- 五个资源色的取色器
- 窗口高度 / 成员区高度上限 / 成员格最小宽
- PSI 警告与严重阈值

样稿的数据是确定性伪随机（`mulberry32` 定种），不依赖网络也不依赖真实系统。**尺寸、阈值、配色可以直接从样稿量取，不必从本文档反推。**

内联的 uPlot v1.6.32 为 MIT 许可（<https://github.com/leeoniya/uPlot>），随样稿一并保留其版权头。

### 13.1 样稿与本文档的已知差异

样稿是设计验证工具，不是参考实现。以下几处**故意偏离**，实现时以本文档为准：

| 处 | 样稿 | 本文档 |
|---|---|---|
| PSI 压力带 | 磁盘成员格、GPU 行、网络行都画了 | **只有 CPU / 内存 / 磁盘三行画**，成员格不画（§6.7） |
| 数据来源 | `mulberry32` 定种伪随机，不读真实系统 | 真实采集 |
| 时间轴 | 1 Hz 推进、60 点窗口 | 采集间隔 2s（可配 1–60s），`live` 层保留 1 小时 |
| 参数面板 | 可实时改资源数量 / 核数 / 配色 / 布局 / 阈值 | 产品界面里不存在这个面板；PSI 阈值应进配置 |
| GPU 型号 | 固定为 AMD Radeon Pro W6800 | 由 `GpuInfo` 提供 |
