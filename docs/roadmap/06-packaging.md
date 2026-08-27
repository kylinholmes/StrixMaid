# 06 构建与打包

## 1. 目标

产出 `design.md` §2.1 的三个产物并可安装运行：

| 产物 | 链接 | 目标 |
|---|---|---|
| `strixmaid` | 静态 musl | x86_64、aarch64；含 UI、AgentCore、Server、worker 模式 |
| `strixmaid-agent` | 静态 musl | 同上，无 UI |
| `strixmaid-helper` | 动态 glibc | 按目标发行版的 glibc 基线构建 |

附带：`strixmaid.service`、`strixmaid-agent.service`、`/etc/pam.d/strixmaid`、`/etc/strixmaid/config.toml` 示例、安装脚本。

## 2. 现状

- `rustup target list --installed` 含 `x86_64-unknown-linux-musl`；无 `.cargo/config.toml`；当前全部产物为 gnu 动态链接。
- `Cargo.toml` release profile：`lto = "fat"`、`codegen-units = 1`、`strip = true`、`panic = "abort"`。
- `crates/strixmaid-server/Cargo.toml` 已声明 `[[bin]] name = "strixmaid"`；`apidoc` feature 存在。`ui` feature（§2.1）未实现，`web/dist` 始终嵌入。
- helper 的 `build.rs` 用 `-l:libpam.so.0` 链接，无需 `libpam0g-dev`。
- pam.d 模板：`crates/strixmaid-helper/pam.d/strixmaid.{debian,rhel}`。
- 无 systemd unit、无安装脚本、无 CI。

## 3. 方案

### 3.1 静态构建

`.cargo/config.toml`：

```toml
[target.x86_64-unknown-linux-musl]
rustflags = ["-C", "target-feature=+crt-static"]

[target.aarch64-unknown-linux-musl]
rustflags = ["-C", "target-feature=+crt-static"]
linker = "aarch64-linux-musl-gcc"   # 或改用 cargo-zigbuild
```

构建命令：

```
cargo build --release --target x86_64-unknown-linux-musl -p strixmaid-server -p strixmaid-agent
cargo build --release --target x86_64-unknown-linux-gnu  -p strixmaid-helper
```

需要确认的点：

1. `sqlx` 的 `sqlite` feature 经 `libsqlite3-sys` 的 `bundled` 编译 C 源，musl 目标需要 `musl-gcc`（Debian 包 `musl-tools`）。不装的话 `cc` crate 会用默认 `gcc` 生成 glibc 目标文件，链接时报错。
2. `zbus`、`procfs`、`nix` 均为纯 Rust 或 libc 调用，无 C 依赖。
3. 验证：`ldd target/x86_64-unknown-linux-musl/release/strixmaid` 输出 `not a dynamic executable`；`file` 输出含 `statically linked`。
4. aarch64：首选 `cargo-zigbuild`（`cargo zigbuild --target aarch64-unknown-linux-musl`），避免维护交叉工具链。

helper 的 glibc 基线：用 `cargo-zigbuild --target x86_64-unknown-linux-gnu.2.28`（Debian 10 / RHEL 8 的 glibc 2.28）。`libpam.so.0` 的 ABI 自 PAM 1.1 起稳定，运行时链接到目标机自带的库。

### 3.2 `ui` feature

`crates/strixmaid-server/Cargo.toml`：

```toml
[features]
default = ["ui"]
ui = []
apidoc = []
```

`embed.rs` 中 `#[cfg(feature = "ui")]` 包住 `WebAssets` 与 SPA 回退；关闭时 `/` 返回 404 JSON。`strixmaid-agent` 不依赖 server crate，本身不含 UI，此 feature 只用于产出无 UI 的 `strixmaid` 变体（`design.md` §2.1 提到的精简版），非必需。

`web/dist` 不存在时 `rust-embed` 编译失败。仓库内保留占位 `index.html`，正式前端构建产物由前端仓库或 CI 放入。

### 3.3 systemd unit

`packaging/strixmaid.service`：

```ini
[Unit]
Description=StrixMaid server
Documentation=https://github.com/<org>/strixmaid
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
ExecStart=/usr/bin/strixmaid serve
Restart=on-failure
RestartSec=2
StateDirectory=strixmaid
RuntimeDirectory=strixmaid
Environment=RUST_LOG=info
# worker 与 helper 是本进程的子进程，随主进程停止。
KillMode=control-group
TimeoutStopSec=20
# 主进程需要 root：spawn helper、读全量 /proc、改主机名与时区。不加沙箱限制。

[Install]
WantedBy=multi-user.target
```

`strixmaid-agent.service` 同形，`ExecStart=/usr/bin/strixmaid-agent`，`StateDirectory=strixmaid-agent`。

停止顺序：主进程收到 SIGTERM 后 `sessions.shutdown()` 依次 `Shutdown` worker、`CloseSession` helper（`main.rs::serve` 已实现）；`control-group` 兜底杀残留。

### 3.4 pam.d 与配置安装

`packaging/install.sh`（也是 deb / rpm 的 postinst 逻辑来源）：

1. 复制二进制到 `/usr/bin/`（helper 0755，root:root）。
2. 按 `/etc/os-release` 的 `ID_LIKE` 选择 pam.d 模板：含 `debian` → `strixmaid.debian`；含 `rhel` / `fedora` / `suse` → `strixmaid.rhel`；其它发行版打印提示并退出非零。安装到 `/etc/pam.d/strixmaid`，不覆盖已存在文件。
3. `/etc/strixmaid/config.toml` 不存在时由 `strixmaid config example > /etc/strixmaid/config.toml` 生成（新增 `config example` 子命令，输出 `Config::example_toml()`）。
4. 安装 unit 文件，`systemctl daemon-reload`；不自动 enable。
5. 打印监听地址与「默认只监听 127.0.0.1，对外访问请配置反向代理」的提示。

`strixmaid --check-config` 子命令：加载并校验配置后退出，供 `ExecStartPre` 与安装脚本使用。

### 3.5 发布产物

```
strixmaid-<version>-x86_64.tar.gz
├── strixmaid
├── strixmaid-agent
├── strixmaid-helper
├── packaging/strixmaid.service
├── packaging/strixmaid-agent.service
├── packaging/pam.d/strixmaid.debian
├── packaging/pam.d/strixmaid.rhel
├── packaging/install.sh
└── LICENSE
```

`strixmaid --version` 输出 `strixmaid 0.1.0 (<git sha>, <target>)`，git sha 由 `build.rs` 从 `git rev-parse --short HEAD` 注入，无 git 时为 `unknown`。

### 3.6 CI

GitHub Actions，`.github/workflows/ci.yml`：

| Job | 内容 |
|---|---|
| `check` | `cargo fmt --check`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo test --workspace`（gnu，Ubuntu runner） |
| `build-musl` | 安装 `musl-tools`，构建 `strixmaid` 与 `strixmaid-agent`，断言 `ldd` 为静态；上传 artifact |
| `build-helper` | `cargo-zigbuild --target x86_64-unknown-linux-gnu.2.28`，`ldd` 断言只链 `libpam.so.0`、`libc.so.6` 及其依赖 |
| `size` | 记录三个二进制的字节数并与上次比较，增长超过 10% 时在 PR 中标注 |

## 4. 涉及文件

`.cargo/config.toml`、`crates/strixmaid-server/Cargo.toml`（features）、`crates/strixmaid-server/src/{embed,cli,main}.rs`（`ui` cfg、`config example`、`--check-config`、`--version`）、`crates/strixmaid-server/build.rs`（新建，git sha）、`packaging/*`、`.github/workflows/ci.yml`。

## 5. 验收

1. 在一台干净的 Ubuntu 24.04 与一台 Rocky 9 上，仅解压 tar.gz 并运行 `install.sh`，`systemctl start strixmaid` 后 `curl 127.0.0.1:9700/api/v1/health` 返回 200；`/api/v1/capabilities` 的 `helper = true`。
2. `strixmaid` 静态二进制 ≤ 15 MiB，`strixmaid-agent` ≤ 8 MiB，`strixmaid-helper` ≤ 1 MiB（release，`design.md` Q3）。
3. Alpine 容器内（无 glibc）`strixmaid-agent` 可运行并采集（helper 不可用属预期，Agent 不需要它）。

## 6. 未决问题

1. deb / rpm 包本身不在本方案内，`install.sh` 先覆盖 tar.gz 分发；包的 postinst 复用其逻辑。
2. Alpine 上主进程 `strixmaid serve` 的登录不可用（helper 为 glibc 动态链接）。若要支持，helper 需另出 musl 动态链接版本，Alpine 的 `linux-pam` 提供 `libpam.so.0`。属 P1。
