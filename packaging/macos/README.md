# StrixMaid — macOS 安装说明

本文件随发布包一起分发（`scripts/package-macos.sh` 会把它放进 tar.gz 的根），
因此不用相对链接引别的文件——在压缩包里那些链接都是死的。
平台本身的设计与能力边界见仓库里的 `docs/macos-platform.md`：三进程结构、认证链、
各 provider 的数据源、**macOS 上拿不到的能力清单**，以及交付形态的完整说明。

## 系统要求

| 项 | 要求 | 依据 |
|---|---|---|
| 架构 | **Apple Silicon（arm64）** | 发布包只构建 `aarch64-apple-darwin`。Rosetta 只能把 x86_64 翻译到 arm64，反方向不存在，所以这些二进制在 Intel Mac 上跑不起来 |
| 操作系统 | macOS 11 及以上 | Apple Silicon 机型的最低系统版本即 11 |
| 运行时 | 无 | 只链系统自带的 dylib；PAM 用系统的 OpenPAM |
| 权限 | 安装需要 root（`sudo`） | 要写 `/usr/local/bin` 与 `/etc`、在 `/var` 下建 root 独占目录、往 `/Library/LaunchDaemons` 注册系统级服务 |

## 先读这一条：隔离标记

**本版本不做代码签名与公证。** 因此用浏览器（Safari / Chrome）下载的这个包会被
macOS 打上 `com.apple.quarantine` 扩展属性，解压出来的文件继承它；Gatekeeper 在执行时
看到这个属性又找不到可信签名，就**直接拒绝运行**，提示大意是「无法验证开发者」。

**这是已知取舍，不是包坏了。** 三种解法，任选其一：

```sh
# 1. 解压后清掉整个目录的隔离标记（推荐）
xattr -dr com.apple.quarantine strixmaid-<版本>-aarch64
```

2. **用 `curl` 下载**。标记是下载它的那个 App 打的，命令行工具不打，这条路根本不会
   产生隔离标记——「别人能跑我不能跑」多半就是下载方式不同。

3. **在系统设置里放行一次**：系统设置 → 隐私与安全性 → 下方「安全性」一栏点「仍要打开」。
   注意 macOS 15 (Sequoia) 起 Apple 去掉了「在访达里按住 Control 点按再打开」那条老捷径，
   只能走系统设置。

安装脚本会对**装到 `/usr/local/bin` 的那三个副本**清一次标记，所以装完之后的服务与
命令行都不受影响。上面说的是「还没装、想先跑一下解压出来的二进制」那一步。

## 安装

```sh
tar xzf strixmaid-<版本>-aarch64-macos.tar.gz
cd strixmaid-<版本>-aarch64
xattr -dr com.apple.quarantine .          # 见上；用 curl 下载的可跳过

sudo packaging/install.sh                 # 只注册，不启动
sudo packaging/install.sh --start         # 装完立即启动
```

脚本是 POSIX sh，兼容 macOS 自带的 bash 3.2，不需要另装任何东西。
重复执行是安全的：已存在的 `/etc/pam.d/strixmaid` 与 `config.toml` 不会被覆盖。

装完得到：

| 位置 | 内容 |
|---|---|
| `/usr/local/bin/` | `strixmaid`、`strixmaid-agent`、`strixmaid-helper` |
| `/etc/strixmaid/config.toml` | 默认配置，由 `strixmaid config example` 现场生成 |
| `/etc/pam.d/strixmaid` | PAM 服务配置（**必须有**，见下） |
| `/var/db/strixmaid/` | SQLite（指标 / 会话 / 审计） |
| `/var/log/strixmaid/` | `server.log`、`agent.log` |
| `/Library/LaunchDaemons/io.strixmaid.server.plist` | launchd 服务定义 |
| `/Library/LaunchDaemons/io.strixmaid.agent.plist` | 同上，agent 用 |

### 为什么路径与 Linux 版不一样

macOS 是 BSD 的 hier(7) 布局，**没有 `/run`**，也**没有 `/var/lib`**：

* 数据目录用 `/var/db/strixmaid`（hier(7)：自动生成的系统数据库文件）；
* 运行目录用 `/var/run/strixmaid`（其内容每次开机会被清空）；
* 二进制装在 `/usr/local/bin` 而不是 `/usr/bin`——后者在只读的系统卷上并受 SIP 保护，
  根本写不进去。

### PAM 配置为什么是必须的

macOS 自带的是 OpenPAM，它在**服务文件不存在**时整体回退到 `/etc/pam.d/other`，
而系统自带的 `other` 是四行 `pam_deny`，全拒。症状是「密码明明正确却认证失败」，
日志里只有一句 `认证失败: pam_authenticate`，极难往这个方向想。
安装脚本因此必定装这一份；已存在时不覆盖（管理员改过的栈不能被冲掉）。

### 安装脚本不会自动启动服务

与 Linux 侧「不自动 `systemctl enable`」、Windows 侧默认不 `-StartService` 一致。
加 `--start` 就装完即启。

有一处与 Linux 不同要知道：**macOS 没有「装好了但开机不自启」这一档**。plist 一旦
放进 `/Library/LaunchDaemons`，下次开机 launchd 就会装载它——这更接近 Windows 服务的
「自动启动」。真要装而不启用：

```sh
sudo launchctl disable system/io.strixmaid.server
```

## 服务的日常操作

```sh
sudo launchctl bootstrap system /Library/LaunchDaemons/io.strixmaid.server.plist   # 启动
sudo launchctl bootout   system/io.strixmaid.server                                # 停止
sudo launchctl print     system/io.strixmaid.server                                # 查状态
sudo launchctl kickstart -k system/io.strixmaid.server                             # 重启
```

注意两条命令的参数形状不同：`bootstrap` 收「域 + plist 路径」，`bootout` 收「域/标签」。
一律用这套新式命令，不要用已过时的 `launchctl load` / `unload`——后者在 plist 有问题时
经常退出 0 却什么也没做。

改过配置之后先校验再重启：

```sh
sudo /usr/local/bin/strixmaid --check-config
sudo launchctl kickstart -k system/io.strixmaid.server
```

日志不进统一日志（launchd 不会把作业的 stdout / stderr 送进去），而是写在
`/var/log/strixmaid/` 下。该目录是 `0750 root:admin`，管理员不用 sudo 就能看：

```sh
tail -f /var/log/strixmaid/server.log
```

**没有配日志轮转**：launchd 在作业启动时打开日志文件并一直持有 fd，用 newsyslog 那种
「改名 + 新建」的方式轮转，进程会继续往改了名的旧文件里写。真要轮转，轮转之后跟一句
`launchctl kickstart -k system/io.strixmaid.server`。

## Agent 节点

包里带了 `strixmaid-agent` 与它的 plist，但**不会自动启动**：它的 `server_url` 与
`token` 没有可猜的默认值，没写配置起了也只会立即退出。plist 里用
`KeepAlive` 的 `PathState` 盯着 `/etc/strixmaid/agent.toml`，文件不存在就不保活。

填好那份配置（token 由服务端 `POST /api/v1/nodes` 登记获得）之后：

```sh
sudo launchctl bootstrap system /Library/LaunchDaemons/io.strixmaid.agent.plist
```

## 卸载

```sh
# 停服务、注销、删二进制与 plist。配置与数据【保留】
sudo packaging/uninstall.sh

# 连配置、数据与日志一起删（不可撤销）
sudo packaging/uninstall.sh --purge
```

**默认保留数据是有意的**：数据库里是几个月的指标历史与审计记录，配置里是运维改过的
监听地址、保留期与提权组。把「先卸掉再装新版本」这种最常见的操作变成一次数据丢失，
代价远大于目录残留。语义与 Linux 侧的 `apt remove` / `apt purge`、Windows 侧的
`-Purge` 完全一致。

## 目录权限

| 目录 | 权限 | 为什么 |
|---|---|---|
| `/usr/local/bin/strixmaid*` | `0755 root:wheel` | helper 由主进程（root）spawn，不需要 setuid 位。macOS 上 root 的主组是 `wheel` |
| `/etc/strixmaid/` | `0755 root:wheel` | 配置本身 `0644` |
| `/var/db/strixmaid/` | `0700 root:wheel` | 指标、会话与审计的 SQLite，只有主进程该碰 |
| `/var/log/strixmaid/` | `0750 root:admin` | 管理员不必 sudo 就能 tail；非管理员进不去 |
| `/var/run/strixmaid/` | `0700 root:wheel` | 开机即清空 |

## 这个包里没有 Intel 版

只构建 Apple Silicon，不做 Intel，也不做 universal——universal 要把两份代码都塞进去，
体积翻倍，而 Intel Mac 不在交付范围内。需要的话自行构建：

```sh
cargo build --release --target x86_64-apple-darwin
```

## 默认只监听 127.0.0.1

对外访问请在前面配置反向代理（nginx / Caddy），TLS 在反代终结。本版本不内置 TLS。
