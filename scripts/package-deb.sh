#!/bin/sh
# 组装 Debian/Ubuntu 包(roadmap/06 §3.5 的 deb 形态;CI 的 deb job 调用)。
#
# 与 packaging/install.sh 对齐——那份脚本的头注释写明它是「deb/rpm postinst 的
# 逻辑来源」:二进制进 /usr/bin,pam.d 用 debian 模板且不覆盖(conffile 机制),
# 首次安装生成 /etc/strixmaid/config.toml,unit 装好但不自动 enable。
#
# 产出两个包:
#   strixmaid_<ver>_<arch>.deb        server(静态 musl)+ helper(glibc 2.28)+ unit + pam.d
#   strixmaid-agent_<ver>_<arch>.deb  只装 unit 与示例配置;二进制来自 strixmaid
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
# strixmaid-agent(只有 unit 与示例配置,二进制由 strixmaid 提供)
# ---------------------------------------------------------------------------
# 2026-09-17 起 Agent 与 Server 是同一个二进制的两种模式(design.md §11),
# 所以这个包里【没有二进制】,只有 unit 与示例配置,靠 Depends 把 strixmaid 拉进来。
# 保留成独立包是为了 `apt install strixmaid-agent` 这个用法仍然成立——
# 装的是「这台机器当 agent」这件事,不是一个可执行文件。
root=$(mktemp -d)
install -D -m 0644 "$here/packaging/strixmaid-agent.service" "$root/lib/systemd/system/strixmaid-agent.service"
install -D -m 0644 "$here/LICENSE" "$root/usr/share/doc/strixmaid-agent/LICENSE"

# 示例配置放 doc 而不是直接落 /etc:server_url/token 没有可猜的默认值,
# 生成一个残缺的 /etc 配置只会让 unit 的 ConditionPathExists 放行然后照样崩
# 示例配置由【刚构建出来的】二进制生成,不是手抄一份快照:
# data_dir 的缺省在三个平台上是三个值,手抄必然会漂。
mkdir -p "$root/usr/share/doc/strixmaid-agent"
"$bins/strixmaid" config example --agent > "$root/usr/share/doc/strixmaid-agent/agent.toml.example"

mkdir -p "$root/DEBIAN"
cat > "$root/DEBIAN/control" <<EOF
Package: strixmaid-agent
Version: $version
Architecture: $arch
Depends: strixmaid (= $version)
Maintainer: StrixMaid <noreply@github.com>
Section: admin
Priority: optional
Homepage: https://github.com/kylinholmes/StrixMaid
Description: agent-mode unit for StrixMaid
 Systemd unit and example config that run the strixmaid binary in agent mode:
 collect host metrics locally and push them to an upstream StrixMaid server
 over a single outbound WebSocket. The binary itself ships in the strixmaid
 package - agent and server are two modes of one executable.
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
