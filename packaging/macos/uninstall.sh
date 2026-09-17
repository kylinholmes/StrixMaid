#!/bin/sh
# StrixMaid macOS 卸载脚本。默认【保留】配置与数据。
#
# 默认行为对应 apt remove 而不是 apt purge：卸载只拿走程序与服务定义，
# /etc/strixmaid 下的 config.toml 与 /var/db/strixmaid 下的 SQLite 原样留着。
#
# 这是有意的，与 Windows 侧 uninstall.ps1 的 -Purge 语义一字不差：数据库里是几个月
# 的指标历史与审计记录，配置里是运维改过的监听地址、保留期与提权组。卸载一次就把
# 它们清空，等于把「先卸掉再装个新版本」这种最常见的操作变成一次事故。
#
# 真的要清干净时加 --purge。那会删掉配置、数据与日志，【不可撤销】。
#
# 兼容性：只用 POSIX sh 语法，能在 macOS 自带的 bash 3.2 下跑。
# 同 install.sh：凡是后面紧跟中文的变量展开一律写成 ${var}（3.2 按字节解析变量名）。
set -eu

bindir=/usr/local/bin
confdir=/etc/strixmaid
datadir=/var/db/strixmaid
rundir=/var/run/strixmaid
logdir=/var/log/strixmaid
daemondir=/Library/LaunchDaemons
server_label=io.strixmaid.server
agent_label=io.strixmaid.agent

purge=0
while [ $# -gt 0 ]; do
    case "$1" in
        --purge) purge=1 ;;
        -h|--help)
            cat <<'USAGE'
用法：sudo packaging/uninstall.sh [--purge]

  --purge   连同配置、数据与日志一并删除（不可撤销）：
            /etc/strixmaid、/etc/pam.d/strixmaid、/var/db/strixmaid、/var/log/strixmaid。
            不加本参数时这些目录原样保留，重装后继续沿用。
USAGE
            exit 0 ;;
        *) echo "未知参数 $1（--help 看用法）" >&2; exit 2 ;;
    esac
    shift
done

if [ "$(uname -s)" != "Darwin" ]; then
    echo "本脚本只适用于 macOS。" >&2
    exit 2
fi

if [ "$(id -u)" != "0" ]; then
    cat >&2 <<'NEEDROOT'
需要 root 权限，请用 sudo 重新运行：

    sudo packaging/uninstall.sh

原因：要从 /Library/LaunchDaemons 注销系统级服务，并删除 /usr/local/bin 下的文件。
NEEDROOT
    exit 1
fi

# --------------------------------------------------------------------------
# 1. 停掉并注销作业——必须在删二进制与 plist 之前
# --------------------------------------------------------------------------
#
# bootout 收的是「域/标签」，不是 plist 路径。作业本来就没装载时它返回非零
# （常见是 3，No such process），那不算错误，吞掉。
#
# 先 bootout 再删 plist 的顺序不能反：plist 没了之后 launchd 仍记着这个作业，
# 那时再 bootout 也能成功，但中间有个窗口期里「服务还在跑、定义已经没了」，
# 排查起来莫名其妙。
for label in "$agent_label" "$server_label"; do
    if launchctl print "system/$label" >/dev/null 2>&1; then
        echo "注销 ${label} ..."
        launchctl bootout "system/$label" || true
    else
        echo "${label} 未装载，跳过。"
    fi
done

for label in "$server_label" "$agent_label"; do
    if [ -e "$daemondir/$label.plist" ]; then
        rm -f "$daemondir/$label.plist"
        echo "已删除 ${daemondir}/${label}.plist"
    fi
done

# --------------------------------------------------------------------------
# 2. 二进制
# --------------------------------------------------------------------------
#
# 只删自己装的这三个文件，不动 /usr/local/bin 这个目录本身——那里面是别人的东西。
for f in strixmaid strixmaid-helper; do
    if [ -e "$bindir/$f" ]; then
        rm -f "$bindir/$f"
    fi
done
echo "已删除 ${bindir} 下的三个二进制"

# --------------------------------------------------------------------------
# 3. 运行目录
# --------------------------------------------------------------------------
#
# 无条件删：里面只有进程活着时才有意义的东西，而且 /var/run 的内容本来每次开机
# 就会被清掉，留着它没有任何价值。
rm -rf "$rundir"

# --------------------------------------------------------------------------
# 4. 配置、数据与日志
# --------------------------------------------------------------------------

if [ "$purge" = "1" ]; then
    rm -rf "$datadir" "$logdir"
    rm -f  "$confdir/config.toml" "$confdir/agent.toml" /etc/pam.d/strixmaid
    # 目录空了才删：管理员可能在 /etc/strixmaid 下放了别的东西（比如 agent.token）。
    rmdir "$confdir" 2>/dev/null || echo "${confdir} 下还有其它文件，目录保留。"
    cat <<TIP

已卸载，并删除了配置、数据与日志：
    $confdir  /etc/pam.d/strixmaid  $datadir  $logdir
TIP
else
    cat <<TIP

卸载完成。配置与数据【已保留】：
    配置    $confdir/config.toml
    PAM     /etc/pam.d/strixmaid
    数据    $datadir
    日志    $logdir
重装时会沿用它们；要一并删除请重新运行本脚本并加 --purge。
TIP
fi
