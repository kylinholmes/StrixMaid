#!/bin/sh
# StrixMaid 安装脚本（roadmap/06 §3.4）。也是将来 deb/rpm postinst 的逻辑来源。
# 幂等：重复执行安全；已存在的 pam.d 与 config.toml 不覆盖。
set -eu

here=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)

if [ "$(id -u)" != "0" ]; then
    echo "需要 root（安装 setuid helper 与 systemd unit）" >&2
    exit 1
fi

# 1. 二进制
install -m 0755 -o root -g root "$here/strixmaid"        /usr/bin/strixmaid
install -m 0755 -o root -g root "$here/strixmaid-agent"  /usr/bin/strixmaid-agent
# helper 由主进程（root）spawn，不需要 setuid 位；0755 root:root 即可。
install -m 0755 -o root -g root "$here/strixmaid-helper" /usr/bin/strixmaid-helper

# 2. pam.d：按发行版选模板，不覆盖已存在文件
if [ -e /etc/pam.d/strixmaid ]; then
    echo "保留已存在的 /etc/pam.d/strixmaid"
else
    . /etc/os-release
    like="${ID:-} ${ID_LIKE:-}"
    case " $like " in
        *debian*|*ubuntu*)
            install -m 0644 "$here/packaging/pam.d/strixmaid.debian" /etc/pam.d/strixmaid ;;
        *rhel*|*fedora*|*centos*|*suse*)
            install -m 0644 "$here/packaging/pam.d/strixmaid.rhel" /etc/pam.d/strixmaid ;;
        *)
            echo "无法识别的发行版（ID=$ID ID_LIKE=${ID_LIKE:-}）：" >&2
            echo "请手工从 packaging/pam.d/ 选择模板安装到 /etc/pam.d/strixmaid" >&2
            exit 2 ;;
    esac
    echo "已安装 /etc/pam.d/strixmaid"
fi

# 3. 示例配置，不覆盖
mkdir -p /etc/strixmaid
if [ ! -e /etc/strixmaid/config.toml ]; then
    /usr/bin/strixmaid config example > /etc/strixmaid/config.toml
    echo "已生成 /etc/strixmaid/config.toml"
fi
/usr/bin/strixmaid --check-config

# 4. systemd unit；不自动 enable
install -m 0644 "$here/packaging/strixmaid.service"       /etc/systemd/system/strixmaid.service
install -m 0644 "$here/packaging/strixmaid-agent.service" /etc/systemd/system/strixmaid-agent.service
systemctl daemon-reload

# 5. 提示
listen=$(grep -E '^listen *= *' /etc/strixmaid/config.toml | head -1 | sed 's/.*= *//; s/"//g')
cat <<TIP

安装完成。启动：
    systemctl enable --now strixmaid
监听地址：${listen:-127.0.0.1:9700}
默认只监听 127.0.0.1；对外访问请在前面配置反向代理（TLS 在反代终结）。
Agent 节点另装 strixmaid-agent（systemctl enable --now strixmaid-agent），
其 token 由服务端 POST /api/v1/nodes 登记获得。
TIP
