#!/bin/sh
# 组装 Debian/Ubuntu 包(roadmap/06 §3.5 的 deb 形态;CI 的 deb job 调用)。
#
# 与 packaging/install.sh 对齐——那份脚本的头注释写明它是「deb/rpm postinst 的
# 逻辑来源」:二进制进 /usr/bin,pam.d 用 debian 模板且不覆盖(conffile 机制),
# 首次安装生成 /etc/strixmaid/config.toml,unit 装好但不自动 enable。
#
# 产出两个包:
#   strixmaid_<ver>_<arch>.deb        server(静态 musl)+ helper(glibc 2.28)+ unit + pam.d
#   strixmaid-agent_<ver>_<arch>.deb  agent(静态 musl)+ unit,无 pam/helper
#
# 用法: package-deb.sh --bins <目录:含三个二进制> --version <deb 版本> --out <目录>
set -eu

bins= version= out= arch=amd64
while [ $# -gt 0 ]; do
    case "$1" in
        --bins)    bins=$2;    shift 2 ;;
        --version) version=$2; shift 2 ;;
        --out)     out=$2;     shift 2 ;;
        --arch)    arch=$2;    shift 2 ;;
        *) echo "未知参数: $1" >&2; exit 2 ;;
    esac
done
[ -n "$bins" ] && [ -n "$version" ] && [ -n "$out" ] || {
    echo "用法: $0 --bins <dir> --version <ver> --out <dir> [--arch amd64]" >&2; exit 2; }

here=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
mkdir -p "$out"

# ---------------------------------------------------------------------------
# strixmaid(server + helper)
# ---------------------------------------------------------------------------
root=$(mktemp -d)
install -D -m 0755 "$bins/strixmaid"        "$root/usr/bin/strixmaid"
# helper 由 root 主进程 spawn,不需要 setuid 位(packaging/install.sh 同款)
install -D -m 0755 "$bins/strixmaid-helper" "$root/usr/bin/strixmaid-helper"
install -D -m 0644 "$here/packaging/strixmaid.service" "$root/lib/systemd/system/strixmaid.service"
install -D -m 0644 "$here/packaging/pam.d/strixmaid.debian" "$root/etc/pam.d/strixmaid"
install -D -m 0644 "$here/LICENSE" "$root/usr/share/doc/strixmaid/LICENSE"

mkdir -p "$root/DEBIAN"
# strixmaid 本体是静态 musl,libc6 的下限来自动态链接的 helper(glibc 2.28 基线,
# 见 ci.yml build-helper);libpam0g 提供 helper 链接的 libpam.so.0,
# libpam-modules 提供 pam.d 模板里引用的模块。
cat > "$root/DEBIAN/control" <<EOF
Package: strixmaid
Version: $version
Architecture: $arch
Maintainer: StrixMaid <noreply@github.com>
Section: admin
Priority: optional
Homepage: https://github.com/kylinholmes/StrixMaid
Depends: libc6 (>= 2.28), libpam0g, libpam-modules
Description: web-based host management panel
 StrixMaid server (static musl binary) with its PAM helper.
 Serves the management UI on 127.0.0.1:9700 by default; authentication
 is delegated to PAM, authorization to polkit/systemd.
EOF

# pam.d 是管理员可改的配置:声明成 conffile,升级不覆盖本地修改
printf '/etc/pam.d/strixmaid\n' > "$root/DEBIAN/conffiles"

cat > "$root/DEBIAN/postinst" <<'EOF'
#!/bin/sh
# 逻辑来源 packaging/install.sh:生成示例配置(不覆盖)、校验、daemon-reload。
set -e
if [ "$1" = "configure" ]; then
    mkdir -p /etc/strixmaid
    if [ ! -e /etc/strixmaid/config.toml ]; then
        /usr/bin/strixmaid config example > /etc/strixmaid/config.toml
        echo "已生成 /etc/strixmaid/config.toml"
    fi
    /usr/bin/strixmaid --check-config
    if command -v systemctl >/dev/null 2>&1; then
        systemctl daemon-reload || true
    fi
    echo "启动: systemctl enable --now strixmaid"
fi
EOF

cat > "$root/DEBIAN/prerm" <<'EOF'
#!/bin/sh
set -e
if [ "$1" = "remove" ] && command -v systemctl >/dev/null 2>&1; then
    systemctl stop strixmaid 2>/dev/null || true
    systemctl disable strixmaid 2>/dev/null || true
fi
EOF

cat > "$root/DEBIAN/postrm" <<'EOF'
#!/bin/sh
set -e
case "$1" in
    purge)
        # remove 保留配置(还会再装回来),purge 才清(Debian 约定)
        rm -f /etc/strixmaid/config.toml
        rmdir /etc/strixmaid 2>/dev/null || true
        ;;
esac
if command -v systemctl >/dev/null 2>&1; then
    systemctl daemon-reload || true
fi
EOF
chmod 0755 "$root/DEBIAN/postinst" "$root/DEBIAN/prerm" "$root/DEBIAN/postrm"

dpkg-deb --build --root-owner-group "$root" "$out/strixmaid_${version}_${arch}.deb"
rm -rf "$root"

# ---------------------------------------------------------------------------
# strixmaid-agent(静态 musl,无任何依赖)
# ---------------------------------------------------------------------------
root=$(mktemp -d)
install -D -m 0755 "$bins/strixmaid-agent" "$root/usr/bin/strixmaid-agent"
install -D -m 0644 "$here/packaging/strixmaid-agent.service" "$root/lib/systemd/system/strixmaid-agent.service"
install -D -m 0644 "$here/LICENSE" "$root/usr/share/doc/strixmaid-agent/LICENSE"

# 示例配置放 doc 而不是直接落 /etc:server_url/token 没有可猜的默认值,
# 生成一个残缺的 /etc 配置只会让 unit 的 ConditionPathExists 放行然后照样崩
mkdir -p "$root/usr/share/doc/strixmaid-agent"
cat > "$root/usr/share/doc/strixmaid-agent/agent.toml.example" <<'EOF'
# StrixMaid agent 配置。抄到 /etc/strixmaid/agent.toml 并填好两个必填项。
# 环境变量同名覆盖:STRIXMAID_AGENT_SERVER_URL 等(嵌套键用 __)。

# 指标推给哪台 StrixMaid server(仅 ws://;跨公网走服务端前的反向代理终结 TLS)
server_url = "ws://<server>:9700"

# 预共享 token:在服务端注册节点(POST /nodes)时返回。
# 不想写进本文件可改用 token_file = "/etc/strixmaid/agent.token"(读首行)。
token = "<node token>"
EOF

mkdir -p "$root/DEBIAN"
cat > "$root/DEBIAN/control" <<EOF
Package: strixmaid-agent
Version: $version
Architecture: $arch
Maintainer: StrixMaid <noreply@github.com>
Section: admin
Priority: optional
Homepage: https://github.com/kylinholmes/StrixMaid
Description: metrics agent for StrixMaid
 Static (musl) agent that pushes host metrics to a StrixMaid server
 over a single outbound WebSocket. No UI, no PAM.
EOF

cat > "$root/DEBIAN/postinst" <<'EOF'
#!/bin/sh
set -e
if [ "$1" = "configure" ]; then
    if command -v systemctl >/dev/null 2>&1; then
        systemctl daemon-reload || true
    fi
    if [ ! -e /etc/strixmaid/agent.toml ]; then
        cat <<'TIP'
strixmaid-agent 需要配置才会启动:
    mkdir -p /etc/strixmaid
    cp /usr/share/doc/strixmaid-agent/agent.toml.example /etc/strixmaid/agent.toml
    # 填好 server_url 与 token 后:
    systemctl enable --now strixmaid-agent
TIP
    fi
fi
EOF

cat > "$root/DEBIAN/prerm" <<'EOF'
#!/bin/sh
set -e
if [ "$1" = "remove" ] && command -v systemctl >/dev/null 2>&1; then
    systemctl stop strixmaid-agent 2>/dev/null || true
    systemctl disable strixmaid-agent 2>/dev/null || true
fi
EOF

cat > "$root/DEBIAN/postrm" <<'EOF'
#!/bin/sh
set -e
if command -v systemctl >/dev/null 2>&1; then
    systemctl daemon-reload || true
fi
EOF
chmod 0755 "$root/DEBIAN/postinst" "$root/DEBIAN/prerm" "$root/DEBIAN/postrm"

dpkg-deb --build --root-owner-group "$root" "$out/strixmaid-agent_${version}_${arch}.deb"
rm -rf "$root"

ls -l "$out"/*.deb
