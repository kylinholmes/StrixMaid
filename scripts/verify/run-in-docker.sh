#!/usr/bin/env bash
# 07 验证工装的 docker 驱动。与 run-in-podman.sh 等价，用于只有 docker 的机器。
#
#   scripts/verify/run-in-docker.sh --dist <解压后的发布目录> [--distro ubuntu|rocky] [--long]
#
# 与 podman 版的差别只在起容器那一步（其余步骤逐条对齐，改一边记得改另一边）：
#
#   - 没有 `--systemd=always`：docker 不会替你把 systemd 接管好，要自己给
#     `--privileged`、tmpfs 的 /run 与 /run/lock、以及 SIGRTMIN+3 的停止信号。
#   - `--cgroupns=private` 而非 podman 版的 host：cgroup v2 上让容器里的 systemd
#     看见自己的 cgroup 根，是 systemd-in-docker 的推荐做法；用 host 会让它看见
#     宿主整棵树，在跑着别的服务的机器上不合适。
#   - `docker cp` 不会替你建目标目录，先 exec mkdir。
#
# 前置：docker、cgroup v2、能访问镜像与软件源。
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

# 动态链接的二进制换发行版就跑不起来（glibc 版本不同），提前说清楚而不是
# 让它在容器里报一句难懂的 "No such file or directory"。
if [ "$DISTRO" != ubuntu ] && command -v file >/dev/null 2>&1; then
  file "$DIST/strixmaid" | grep -q 'statically linked' || {
    echo "警告：$DIST/strixmaid 不是静态链接，在 $BASE 里很可能起不来。" >&2
    echo "      用 CI 的 strixmaid-dist-x86_64 产物（musl 静态），或只测 --distro ubuntu。" >&2
  }
fi

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
IMG="strix-verify:$DISTRO"
NAME="strix-verify-$$"
ALICE_PW='alice-verify-pw'; BOB_PW='bob-verify-pw'

echo "== 构建镜像 $IMG（BASE=$BASE）=="
docker build --build-arg "BASE=$BASE" -t "$IMG" -f "$ROOT/scripts/verify/Containerfile" "$ROOT"

echo "== 起 systemd 容器 $NAME =="
docker run -d --name "$NAME" --hostname strix-verify \
  --privileged --cgroupns=private \
  -e container=docker \
  --tmpfs /run --tmpfs /run/lock \
  --stop-signal SIGRTMIN+3 \
  "$IMG" /sbin/init >/dev/null
trap 'docker rm -f "$NAME" >/dev/null 2>&1 || true' EXIT

echo "== 等 systemd 就绪 =="
st=''
for _ in $(seq 1 30); do
  st="$(docker exec "$NAME" systemctl is-system-running 2>/dev/null || true)"
  case "$st" in running|degraded) break ;; esac
  sleep 1
done
echo "   systemctl is-system-running: ${st:-未知}"

echo "== 设置测试用户密码 =="
docker exec "$NAME" bash -c "echo 'alice:$ALICE_PW' | chpasswd; echo 'bob:$BOB_PW' | chpasswd"

echo "== 拷入发布物与验证脚本 =="
docker exec "$NAME" mkdir -p /opt/strixmaid-dist
docker cp "$DIST/." "$NAME:/opt/strixmaid-dist/"
docker cp "$ROOT/scripts/verify" "$NAME:/opt/verify"

echo "== 安装（install.sh）=="
docker exec "$NAME" sh -c 'cd /opt/strixmaid-dist && ./packaging/install.sh'

echo "== 启动 strixmaid =="
docker exec "$NAME" systemctl start strixmaid
for _ in $(seq 1 30); do
  docker exec "$NAME" curl -sf http://127.0.0.1:9700/api/v1/health >/dev/null 2>&1 && break
  sleep 1
done

echo; echo "======== root-checks ========"
set +e
docker exec \
  -e ALICE_PW="$ALICE_PW" -e BOB_PW="$BOB_PW" -e LONG="$LONG" \
  "$NAME" bash /opt/verify/root-checks.sh
RC1=$?

echo; echo "======== agent-checks ========"
docker exec \
  -e BOB_PW="$BOB_PW" -e AGENT_BIN=/usr/bin/strixmaid-agent \
  "$NAME" bash /opt/verify/agent-checks.sh
RC2=$?
set -e

echo; echo "== journalctl -u strixmaid（尾部，供排查）=="
docker exec "$NAME" journalctl -u strixmaid --no-pager -n 20 2>/dev/null || true

echo
echo "root-checks 退出码 $RC1；agent-checks 退出码 $RC2"
[ "$RC1" = 0 ] && [ "$RC2" = 0 ]
