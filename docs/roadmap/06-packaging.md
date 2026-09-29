# 06 打包与交付

打包已落地。当前命令与产物以 `scripts/package*`、`packaging/` 和
`.github/workflows/ci.yml` 为准，不再保留创建这些文件的旧实施步骤。

## 1. 产物

只有两个可执行文件：`strixmaid` 与 `strixmaid-helper`。
`serve`、`agent`、`worker` 是主程序的子命令；`strixmaid-agent` 仍可作为
服务名或 deb 包名，但不是单独的可执行文件。

## 2. 平台

Linux 主程序静态 musl，helper 动态链接 glibc / PAM；macOS 发布 Apple Silicon
包，使用系统 dylib / OpenPAM / launchd；Windows 使用系统 DLL、LogonUserW 与 SCM。
安装说明见 `packaging/macos/README.md`、`packaging/windows/README.md`。

## 3. 构建与安装

### 3.1 Linux 构建

`package.sh` 构建主程序与 helper；CI 对 helper 使用 glibc 2.28 基线。
跨架构工具链与静态性检查保留在脚本中，不复制第二份命令。

### 3.2 UI feature

默认 `ui` 嵌入 `web/dist`。先运行 `bun run build`，再构建 Rust。
`--no-default-features` 只去掉前端资源，API 保留；非 API / WS 路径返回 404。
`apidoc` 可显式在 release 中保留 API 文档与调试页。

### 3.3 服务

Linux 的两个 unit 分别执行 `strixmaid serve` 与 `strixmaid agent`。
macOS 由 launchd 托管，Windows 的 `service` 子命令区分两种模式。

### 3.4 配置与权限

helper 为 root:root、0755，无 setuid 位，由服务主进程启动。
`config example` 输出 Server 模板，`config example --agent` 输出 Agent 模板。
`--check-config` 在两种模式下都只校验合并配置并退出，不创建数据库或启动采集。
Server 缺失配置仍使用默认值；Agent 显式给出的配置必须存在。两者都拒绝目录、
设备等非普通文件，且读取错误不能被命令行覆盖掩盖。
PAM 模板按发行版安装，不能把一个平台的认证栈直接用于另一个平台。

### 3.5 发布布局

发布包包含主程序、helper、安装脚本、服务定义、配置示例与许可证。
Linux 另有 deb 包。精确清单由打包脚本与 CI 校验，避免文档重复维护。

### 3.6 CI

frontend job 检查并构建前端，quality 三平台矩阵跑 clippy / build / test；
发布与 Linux 容器验收消费构建产物。具体覆盖见 [09](09-ci-verification.md)。

## 4. 运行边界

TLS 由反向代理终结；当前 Agent 传输没有启用原生 wss。
Alpine 没有 glibc 时 helper 不能直接使用，Agent 的只读采集不依赖它。

## 5. 验收

### 5.1 功能

按 [07](07-verification.md) 在目标环境验证安装、认证、提权、服务与卸载。

### 5.2 体积

Linux musl 主程序门槛为 18 MiB，helper 为 1 MiB。
主程序门槛在加入缩略图解码器后由 15 调到 18 MiB；macOS 动态链接体积不能替代此检查。

### 5.3 Alpine

验证 `strixmaid agent` 的采集能力；没有 glibc / PAM helper 不等于可提供登录管理。

## 6. 未完成范围

原生 wss、Alpine 登录支持与 rpm 交付不因现有脚本而自动成立。
