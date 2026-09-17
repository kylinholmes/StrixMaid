#!/bin/sh
# StrixMaid macOS 安装脚本。
#
# 对应 Linux 侧的 packaging/install.sh 与 Windows 侧的 packaging/windows/install.ps1，
# 做同样的几件事：放二进制、装 PAM 模板、写默认配置、建目录、装服务定义、给出提示。
# 幂等：重复执行安全；已存在的 /etc/pam.d/strixmaid 与 config.toml 不覆盖。
#
# 与 Linux 版的实质差别有四处：
#
#   1. 二进制装到 /usr/local/bin 而不是 /usr/bin。macOS 的 /usr 在只读的系统卷上
#      并受 SIP 保护，根本写不进去；/usr/local 是 SIP 明确放行给第三方的目录，
#      hier(7) 对它的定义也正是「executables, libraries, etc. not included by the
#      basic operating system」。它默认就在 /etc/paths 里，命令行直接能调。
#   2. 服务定义是 launchd 的 plist 而不是 systemd unit，装到 /Library/LaunchDaemons。
#      装载用新式的 `launchctl bootstrap`，不用已过时的 `launchctl load`——后者在
#      plist 有问题时经常退出 0 却什么也没做，bootstrap 会把失败报出来。
#   3. PAM 模板**必须**装（Linux 上也装，但 macOS 上不装的后果更隐蔽），理由见第 3 节。
#   4. 多一步清除隔离标记（com.apple.quarantine），理由见第 2 节。
#
# 兼容性：只用 POSIX sh 语法，能在 macOS 自带的 bash 3.2 下跑。
# 注意 3.2 的一个坑：双引号里变量名按字节解析，"$var（中文）" 会把中文吃进变量名，
# 因此本文件里凡是后面紧跟中文的展开一律写成 ${var}。
set -eu

# --------------------------------------------------------------------------
# 可调参数（默认值与 crates/strixmaid-core/src/config.rs 里 macOS 那组常量一致，
# 改这里就必须同时用 --config / STRIXMAID_* 告诉服务新路径）
# --------------------------------------------------------------------------

bindir=/usr/local/bin
confdir=/etc/strixmaid
datadir=/var/db/strixmaid
rundir=/var/run/strixmaid
logdir=/var/log/strixmaid
daemondir=/Library/LaunchDaemons
server_label=io.strixmaid.server
agent_label=io.strixmaid.agent

start_now=0
while [ $# -gt 0 ]; do
    case "$1" in
        --start) start_now=1 ;;
        -h|--help)
            cat <<'USAGE'
用法：sudo packaging/install.sh [--start]

  --start   装完立即启动主进程（launchctl bootstrap）。
            默认不启动，与 Linux 侧「不自动 enable」、Windows 侧的 -StartService 一致。
            注意：plist 一旦放进 /Library/LaunchDaemons，下次开机 launchd 就会装载它；
            要装好却连开机也不起，执行 launchctl disable system/io.strixmaid.server。
USAGE
            exit 0 ;;
        *) echo "未知参数 $1（--help 看用法）" >&2; exit 2 ;;
    esac
    shift
done

# --------------------------------------------------------------------------
# 0. 前置检查
# --------------------------------------------------------------------------

if [ "$(uname -s)" != "Darwin" ]; then
    echo "本脚本只适用于 macOS。Linux 请用 packaging/install.sh，Windows 请用 packaging\\install.ps1。" >&2
    exit 2
fi

if [ "$(id -u)" != "0" ]; then
    cat >&2 <<'NEEDROOT'
需要 root 权限，请用 sudo 重新运行：

    sudo packaging/install.sh

原因：要写 /usr/local/bin 与 /etc，要在 /var 下建 root 独占的数据目录，
还要往 /Library/LaunchDaemons 注册系统级服务。这三件事都只有 root 能做。
NEEDROOT
    exit 1
fi

# 本包只构建 Apple Silicon（aarch64-apple-darwin），不做 Intel、不做 universal。
# Rosetta 只能把 x86_64 翻译到 arm64，反方向不存在，所以 Intel 机器上这些二进制
# 是根本跑不起来的——与其装完在日志里看「Bad CPU type」，不如这里就说清楚。
arch=$(uname -m)
if [ "$arch" != "arm64" ]; then
    echo "本发布包只含 Apple Silicon（arm64）二进制，当前机器是 ${arch}。" >&2
    echo "Intel Mac 请自行构建：cargo build --release --target x86_64-apple-darwin" >&2
    exit 2
fi

# 脚本位于发布包的 packaging/ 下，二进制在它的上一级。
here=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)

for f in strixmaid strixmaid-agent strixmaid-helper; do
    [ -f "$here/$f" ] || {
        echo "发布包不完整，缺 ${here}/${f}。请在解压后的目录里运行 packaging/install.sh。" >&2
        exit 2
    }
done

# --------------------------------------------------------------------------
# 1. 已在运行就先停下来
# --------------------------------------------------------------------------
#
# macOS 对正在执行的可执行文件会返回 ETXTBSY，直接覆盖会失败。
# bootout 的参数是「域/标签」，不是 plist 路径（bootstrap 才收路径）。
for label in "$server_label" "$agent_label"; do
    if launchctl print "system/$label" >/dev/null 2>&1; then
        echo "停止正在运行的 ${label} ..."
        launchctl bootout "system/$label" || true
    fi
done

# --------------------------------------------------------------------------
# 2. 二进制
# --------------------------------------------------------------------------

install -d -o root -g wheel -m 0755 "$bindir"
# helper 由主进程（root）spawn，不需要 setuid 位；0755 root:wheel 即可。
# 注意 macOS 的 root 主组是 wheel 而不是 root。
for f in strixmaid strixmaid-agent strixmaid-helper; do
    install -m 0755 -o root -g wheel "$here/$f" "$bindir/$f"
done

# 清除隔离标记。**这一步是「暂不做签名与公证」的直接后果，不是可选的洁癖**：
# 用浏览器下载的压缩包会被打上 com.apple.quarantine，展开出来的文件继承它，
# Gatekeeper 在 exec 时看到这个属性且找不到可信签名，就直接拒绝执行。
# 用 curl 下载则不会被打标记（标记是下载它的 App 打的），所以现象因人而异。
# 装好之后清掉，服务与命令行都不再受影响。
#
# 清的是安装后的副本，不是发布包里的原件：install(1) 是否连扩展属性一起复制
# 不同系统版本上表现不一，对目标文件清一次最稳妥。属性本来就不存在时
# xattr 会报错，这里吞掉。
for f in strixmaid strixmaid-agent strixmaid-helper; do
    xattr -d com.apple.quarantine "$bindir/$f" 2>/dev/null || true
done

# --------------------------------------------------------------------------
# 3. PAM 服务配置
# --------------------------------------------------------------------------
#
# macOS 自带 OpenPAM，这份文件**必须装**，不是可选项：
# OpenPAM 在服务文件不存在时整体回退到 /etc/pam.d/other，而 macOS 自带的 other
# 是四行 pam_deny，全拒。表现为「密码明明正确却认证失败」，helper 日志里只有一句
# 「认证失败: pam_authenticate」，极难往这个方向想（docs/macos-platform.md 有实测记录）。
#
# Linux 侧要按发行版在 debian / rhel 两份模板之间选，靠读 /etc/os-release；
# macOS 只有一份模板，也没有 /etc/os-release，所以这里不做判断。
# 与 Linux 一致：已存在就不覆盖，管理员改过的栈不能被安装脚本冲掉。
if [ -e /etc/pam.d/strixmaid ]; then
    echo "保留已存在的 /etc/pam.d/strixmaid"
else
    install -m 0644 -o root -g wheel "$here/packaging/pam.d/strixmaid.macos" /etc/pam.d/strixmaid
    echo "已安装 /etc/pam.d/strixmaid"
fi

# --------------------------------------------------------------------------
# 4. 目录
# --------------------------------------------------------------------------
#
# launchd 没有 systemd 的 StateDirectory / RuntimeDirectory，目录一律自己建。
#
#   数据目录 0700：里面是指标、会话与审计的 SQLite 库，只有主进程（root）该碰。
#   日志目录 0750 root:admin：文件由 launchd 以 0644 建出来，靠目录权限把非管理员
#     挡在外面；管理员（admin 组）不必 sudo 就能 tail 日志，与 Linux 上 journald
#     把日志开放给 adm 组是同一个取舍。
#   运行目录 0700：/var/run 的内容每次开机都会被清掉，这里建出来只保证「刚装完
#     就能用」。当前代码其实还用不到它（helper 走 socketpair，不落文件系统 socket）。
install -d -o root -g wheel  -m 0755 "$confdir"
install -d -o root -g wheel  -m 0700 "$datadir"
install -d -o root -g wheel  -m 0700 "$rundir"
install -d -o root -g admin  -m 0750 "$logdir"

# --------------------------------------------------------------------------
# 5. 默认配置（已存在时不覆盖）
# --------------------------------------------------------------------------
#
# 由刚装好的二进制现场生成，包里不带硬编的副本：Config::example_toml() 会按平台
# 替换路径、提权组与平台提示行（macOS 上是 /var/db 与 /var/run），
# 而「示例里每一项都等于内置默认值」是有单元测试保证的性质。
if [ -e "$confdir/config.toml" ]; then
    echo "保留已存在的 ${confdir}/config.toml"
else
    "$bindir/strixmaid" config example > "$confdir/config.toml"
    chmod 0644 "$confdir/config.toml"
    echo "已生成 ${confdir}/config.toml"
fi

# launchd 没有 ExecStartPre 的对应物（plist 里写了为什么不用 sh -c 绕），
# 所以启动前的配置校验放在这里跑一次。校验的是【合并后】的最终结果。
"$bindir/strixmaid" --check-config

# --------------------------------------------------------------------------
# 6. launchd 作业定义
# --------------------------------------------------------------------------
#
# 属主必须是 root 且 group / other 不可写，否则 launchd 拒绝装载。0644 满足这条，
# 也是各家分发的 daemon plist 的通行取值。
install -m 0644 -o root -g wheel "$here/packaging/$server_label.plist" "$daemondir/$server_label.plist"
install -m 0644 -o root -g wheel "$here/packaging/$agent_label.plist"  "$daemondir/$agent_label.plist"

# enable 写的是 launchd 的覆盖数据库，与「有没有装载」无关：上一次卸载前如果有人
# 执行过 launchctl disable，那条记录会留下来，下次 bootstrap 装上了也不会运行。
# 这里无条件清掉，保证「重装一遍」能修好这种状态。没被 disable 过时它什么也不做。
for label in "$server_label" "$agent_label"; do
    launchctl enable "system/$label" 2>/dev/null || true
done

# 默认只注册不启动，与 Linux 侧「不自动 enable」、Windows 侧默认不 StartService 一致。
# 区别要说清楚：plist 已经在 /Library/LaunchDaemons 里，**下次开机 launchd 会装载它**
# （这一点更接近 Windows 服务的「自动启动」而不是 Linux 的「装了但没 enable」）。
if [ "$start_now" = "1" ]; then
    launchctl bootstrap system "$daemondir/$server_label.plist"
    echo "已启动 ${server_label}"
fi

# --------------------------------------------------------------------------
# 7. 提示
# --------------------------------------------------------------------------

listen=$(grep -E '^listen *= *' "$confdir/config.toml" | head -1 | sed 's/.*= *//; s/"//g')

cat <<TIP

安装完成。
    二进制    $bindir/{strixmaid,strixmaid-agent,strixmaid-helper}
    配置      $confdir/config.toml
    数据      $datadir
    日志      $logdir/server.log
    服务定义  $daemondir/$server_label.plist

启动 / 停止 / 查看：
    sudo launchctl bootstrap system $daemondir/$server_label.plist
    sudo launchctl bootout   system/$server_label
    sudo launchctl print     system/$server_label
    sudo launchctl kickstart -k system/$server_label     # 改完配置重启

改过 config.toml 之后先校验再重启：
    sudo $bindir/strixmaid --check-config

监听地址：${listen:-127.0.0.1:9700}
默认只监听 127.0.0.1；对外访问请在前面配置反向代理（TLS 在反代终结）。

Agent 节点：填好 $confdir/agent.toml（server_url 与 token）之后
    sudo launchctl bootstrap system $daemondir/$agent_label.plist
token 由服务端 POST /api/v1/nodes 登记获得。没有这份配置时 agent 不会被保活。

卸载：packaging/uninstall.sh（默认保留配置与数据，加 --purge 才删）
TIP
