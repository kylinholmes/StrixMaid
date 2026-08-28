#!/usr/bin/env bash
# 07 验证工装的一键驱动：起一个 systemd 容器，装 strixmaid，跑 root-checks +
# agent-checks，然后拆掉。**在 root 那一侧全在容器里，宿主用 rootless podman 即可。**
#
#   scripts/verify/run-in-podman.sh --dist <解压后的发布目录> [--distro ubuntu|rocky] [--long]
#
# <发布目录> 是 scripts/package.sh 产出的 tar.gz 解压后的那层，含：
#   strixmaid  strixmaid-agent  strixmaid-helper  packaging/{*.service,install.sh,pam.d/*}
# 静态 musl 二进制在任何发行版容器里都能跑，这正是它的用处。
#
# 前置：rootless podman、cgroup v2、能访问镜像与软件源。
# **本脚本尚未实际跑通**（开发机无法验证 rootless systemd 容器），首次运行按报错微调。
set -euo pipefail

DIST=''; DISTRO=ubuntu; LONG=0
while [ $# -gt 0 ]; do case "$1" in
  --dist) DIST="$2"; shift 2 ;;
  --distro) DISTRO="$2"; shift 2 ;;
  --long) LONG=1; shift ;;
  *) echo "未知参数 $1" >&2; exit 2 ;;
esac; done
[ -n "$DIST" ] && [ -d "$DIST" ] || { echo "需要 --dist <发布目录>（含 strixmaid 等二进制）" >&2; exit 2; }
[ -x "$DIST/strixmaid" ] || { echo "$DIST 里没有可执行的 strixmaid" >&2; exit 2; }

case "$DISTRO" in
  ubuntu) BASE=ubuntu:24.04 ;;
  rocky)  BASE=rockylinux:9 ;;
  *) echo "--distro 支持 ubuntu / rocky" >&2; exit 2 ;;
esac

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
IMG="strix-verify:$DISTRO"
NAME="strix-verify-$$"
ALICE_PW='alice-verify-pw'; BOB_PW='bob-verify-pw'

echo "== 构建镜像 $IMG（BASE=$BASE）=="
podman build --build-arg "BASE=$BASE" -t "$IMG" -f "$ROOT/scripts/verify/Containerfile" "$ROOT"

echo "== 起 systemd 容器 $NAME =="
podman run -d --name "$NAME" --systemd=always --hostname strix-verify \
  --cgroupns=host "$IMG" >/dev/null
trap 'podman rm -f "$NAME" >/dev/null 2>&1 || true' EXIT

echo "== 等 systemd 就绪 =="
for _ in $(seq 1 30); do
  st="$(podman exec "$NAME" systemctl is-system-running 2>/dev/null || true)"
  case "$st" in running|degraded) break ;; esac
  sleep 1
done
echo "   systemctl is-system-running: ${st:-未知}"

echo "== 设置测试用户密码 =="
podman exec "$NAME" bash -c "echo 'alice:$ALICE_PW' | chpasswd; echo 'bob:$BOB_PW' | chpasswd"

echo "== 拷入发布物与验证脚本 =="
podman cp "$DIST/." "$NAME:/opt/strixmaid-dist/"
podman cp "$ROOT/scripts/verify" "$NAME:/opt/verify"

echo "== 安装（install.sh）=="
podman exec "$NAME" sh -c 'cd /opt/strixmaid-dist && ./packaging/install.sh'

echo "== 启动 strixmaid =="
podman exec "$NAME" systemctl start strixmaid
for _ in $(seq 1 30); do
  podman exec "$NAME" curl -sf http://127.0.0.1:9700/api/v1/health >/dev/null 2>&1 && break
  sleep 1
done

echo; echo "======== root-checks ========"
set +e
podman exec \
  -e ALICE_PW="$ALICE_PW" -e BOB_PW="$BOB_PW" -e LONG="$LONG" \
  "$NAME" bash /opt/verify/root-checks.sh
RC1=$?

echo; echo "======== agent-checks ========"
podman exec \
  -e BOB_PW="$BOB_PW" -e AGENT_BIN=/usr/bin/strixmaid-agent \
  "$NAME" bash /opt/verify/agent-checks.sh
RC2=$?
set -e

echo; echo "== journalctl -u strixmaid（尾部，供排查）=="
podman exec "$NAME" journalctl -u strixmaid --no-pager -n 20 2>/dev/null || true

echo
echo "root-checks 退出码 $RC1；agent-checks 退出码 $RC2"
[ "$RC1" = 0 ] && [ "$RC2" = 0 ]
