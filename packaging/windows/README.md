# StrixMaid — Windows 安装说明

本文件随发布包一起分发（`scripts/package-windows.ps1` 会把它放进 zip 的根），
因此不用相对链接引别的文件——在 zip 里那些链接都是死的。
平台本身的设计与能力边界见仓库里的 `docs/windows-platform.md`：
三进程结构、认证链、各 provider 的数据源、以及**Windows 上拿不到的能力清单**。

## 系统要求

| 项 | 要求 | 依据 |
|---|---|---|
| 操作系统 | Windows 10 1809（内部版本 17763）/ Windows Server 2019 及以上 | 终端走 ConPTY，`CreatePseudoConsole` 自 1809 才有（`crates/strixmaid-core/src/worker/terminal/windows.rs`） |
| 架构 | x86_64 | 发布包只构建 `x86_64-pc-windows-msvc` |
| 运行时 | 无 | 全部走系统 DLL；没有 PAM，认证用 `LogonUserW` |
| 权限 | 安装需要管理员 | 写 `%ProgramFiles%`、改 `%ProgramData%` 下的 ACL、向 SCM 注册服务 |

**更老的系统装不上也跑不起来**，不是没测过而是缺内核能力：ConPTY 之外，
进程信息走的 `NtQuerySystemInformation` 结构布局本项目按 Win7+ 写，
而 GPU 指标依赖的 WDDM 性能计数器要 Windows 10 1709 以后才有。

## 安装

解压后在**管理员** PowerShell 里：

```powershell
# 先干跑，看它会动哪些东西
powershell -NoProfile -ExecutionPolicy Bypass -File packaging\install.ps1 -WhatIf

# 真的装
powershell -NoProfile -ExecutionPolicy Bypass -File packaging\install.ps1
```

脚本只用 Windows 自带的 PowerShell 5.1 语法，不需要另装 PowerShell 7。

装完得到：

| 位置 | 内容 |
|---|---|
| `%ProgramFiles%\StrixMaid\` | `strixmaid.exe`、`strixmaid-helper.exe` |
| `C:\ProgramData\StrixMaid\config.toml` | 默认配置，由 `strixmaid.exe config example` 现场生成 |
| `C:\ProgramData\StrixMaid\data\` | SQLite（指标 / 会话 / 审计） |
| 服务 `StrixMaid` | 自动启动，登录账户 `LocalSystem` |

安装脚本**不会自动启动服务**（与 Linux 侧 `install.sh` 的「不自动 enable」一致）。
要装完就跑加 `-StartService`。

### 为什么配置是「生成」而不是「拷一份」

`Config::example_toml()` 会按平台替换路径、提权组与平台提示行，并且有单元测试
保证「示例里每一项都等于内置默认值」。包里另附的 `config.example.toml` 只是给人
在安装前先读一眼，安装脚本不用它——用装好的 exe 重新生成一次，快照就不会过期。

`config.toml` 已存在时**不覆盖**，重复执行安装脚本是安全的。

### 安装脚本改了配置里的哪一项

只有一项：`helper_path` 被写成绝对路径。

示例里的默认值是裸名 `strixmaid-helper`，那要靠 `PATH` 查找，而服务继承的是
**系统** `PATH`，`%ProgramFiles%\StrixMaid` 不在其中。不改的话表现是「装完能起、
但登录不了」，日志里只说 helper 不可用。改系统 `PATH` 是全局副作用，写绝对路径
只影响本实例。

## 服务的日常操作

```powershell
strixmaid.exe service start     # 启动
strixmaid.exe service stop      # 停止
strixmaid.exe service status    # 查询（只读，不需要管理员）
```

`services.msc` 与 `sc.exe` 同样可用——注册出来的就是一个普通的 Windows 服务。
只有注册与注销建议走 `strixmaid.exe service install` / `uninstall`：那条路还会
设置恢复策略与服务描述，`New-Service` 给不了。

日志写 stderr，由服务宿主收集；服务模式下另写 Windows 事件日志。

## 卸载

```powershell
# 停服务、注销、删二进制。配置与数据【保留】
powershell -NoProfile -ExecutionPolicy Bypass -File packaging\uninstall.ps1

# 连配置与全部历史数据一起删（不可撤销）
powershell -NoProfile -ExecutionPolicy Bypass -File packaging\uninstall.ps1 -Purge
```

**默认保留数据是有意的**：数据库里是几个月的指标历史与审计记录，配置里是运维
改过的监听地址、保留期与提权组。把「先卸掉再装新版本」这种最常见的操作变成一次
数据丢失，代价远大于目录残留。语义与 Linux 侧的 `apt remove` / `apt purge` 一致。

## 目录权限

安装脚本会显式设置两处 ACL，**不靠继承**：

| 目录 | SYSTEM | Administrators | Users |
|---|---|---|---|
| `%ProgramFiles%\StrixMaid` | 完全控制 | 完全控制 | 读取 + 执行 |
| `C:\ProgramData\StrixMaid` | 完全控制 | 完全控制 | 无 |

两条都不是洁癖：

* `%ProgramData%` 的默认 ACL 给 Users 一条「创建文件 / 写入数据」并带容器继承，
  新建的子目录会原样继承。若不断掉，**任何登录用户都能往配置目录里写文件**——
  而服务以 `LocalSystem` 读这里的 `config.toml`，配置里的 `helper_path` 是一条
  会被 `CreateProcess` 执行的路径。那是一条现成的提权链。
* 安装目录给 Users 的读 + 执行是**必需**的：worker 以登录用户的身份运行，
  而 worker 是 `strixmaid.exe` 的一个子命令（不是独立二进制）。Users 读不到
  这个文件，登录之后什么都干不了。写权限当然不给。

ACL 一律按 **SID** 授权而不是组名：内建组的显示名是本地化的（`S-1-5-32-544`
在德文 Windows 上叫 `Administratoren`），按名字授权在非英文系统上会直接失败。
这与 `platform/windows/token.rs` 给内建组补一个英文规范名是同一个理由。

## 两个编码上的坑

* **本目录下的 `.ps1` 文件是 UTF-8 with BOM**，不能改成无 BOM。Windows PowerShell
  5.1 读没有 BOM 的脚本时按系统 ANSI 代码页解码，中文注释会变成乱码，且乱码字节
  可能吞掉引号，直接导致语法错误。PowerShell 7 两种都认，5.1 只认 BOM。
* **`config.toml` 必须是无 BOM 的 UTF-8**。TOML 规范不允许文件以 BOM 开头，
  带 BOM 的配置解析器会直接拒绝。`Set-Content -Encoding UTF8` 在 5.1 上会写 BOM，
  安装脚本因此改用 `System.Text.UTF8Encoding($false)` 写回。

## 这个包里没有 `strixmaid-agent.exe`

Windows 侧的服务宿主（`service` 子命令族）只做在主二进制里，agent 没有被 SCM
托管的入口，装进来也只能手工前台运行。补上服务宿主之后再纳入发布包。
