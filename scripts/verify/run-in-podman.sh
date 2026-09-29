#!/usr/bin/env bash
# 07 验证工装的一键驱动：起一个 systemd 容器，装 strixmaid，跑 root-checks +
# agent-checks，然后拆掉。**在 root 那一侧全在容器里，宿主用 rootless podman 即可。**
#
#   scripts/verify/run-in-podman.sh --dist <解压后的发布目录> [--distro ubuntu|rocky] [--arch amd64|arm64] [--long]
#
# <发布目录> 是 scripts/package.sh 产出的 tar.gz 解压后的那层，含：
#   strixmaid  strixmaid-helper  packaging/{*.service,install.sh,pam.d/*}
# 静态 musl 二进制在任何发行版容器里都能跑，这正是它的用处。
#
# 前置：rootless podman、cgroup v2、能访问镜像与软件源。
# 已在 Fedora 44 aarch64 客机上跑通（2026-09-25，rootless、原生 arm64 容器）。
set -euo pipefail

DIST=''; DISTRO=ubuntu; ARCH=''; LONG=0
while [ $# -gt 0 ]; do case "$1" in
  --dist) DIST="$2"; shift 2 ;;
  --distro) DISTRO="$2"; shift 2 ;;
  --arch) ARCH="$2"; shift 2 ;;
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

# 发布物的架构与宿主不同时（典型：Apple Silicon 上跑 CI 的 x86_64 产物），
# 必须让容器的整个用户态也是那个架构——`strixmaid` 是静态的无所谓，但
# `strixmaid-helper` 动态链接 glibc，还要 dlopen 发行版的 PAM 模块，
# 它们只在同架构的镜像里存在。不指定就会拉到与宿主同架构的镜像，helper
# 起不来，而报错（"failed to open elf at /lib64/ld-linux-x86-64.so.2"）离原因很远。
#
# 宿主侧要有对应的 binfmt 处理器（Rosetta 或 qemu-user-static）。
# **注意**：翻译只对普通进程成立。x86_64 的 systemd 一旦当 PID 1，
# Rosetta 与 qemu-user 下都会 SIGSEGV（实测 Apple Silicon + Fedora 44 客机），
# 所以 `--arch amd64` 目前只在 x86_64 宿主上真正可用；在 Apple Silicon 上
# 请用原生 aarch64 发布物（scripts/package.sh aarch64），架构那一维交给 CI。
if [ -z "$ARCH" ] && command -v file >/dev/null 2>&1; then
  case "$(file -b "$DIST/strixmaid")" in
    *x86-64*)  ARCH=amd64 ;;
    *aarch64*) ARCH=arm64 ;;
  esac
fi
# 下面对 PLATFORM 一律写成 ${PLATFORM[@]+"${PLATFORM[@]}"}：macOS 自带的
# bash 3.2 在 set -u 下展开空数组会当成未绑定变量直接退出。
PLATFORM=()
if [ -n "$ARCH" ]; then
  case "$ARCH" in
    amd64|arm64) ;;
    x86_64)  ARCH=amd64 ;;
    aarch64) ARCH=arm64 ;;
    *) echo "--arch 支持 amd64 / arm64" >&2; exit 2 ;;
  esac
  PLATFORM=(--platform "linux/$ARCH")
  # 把上面那条限制在运行时也说一遍——只写在注释里没人会看见。
  host="$(uname -m)"
  case "$host-$ARCH" in
    x86_64-amd64|aarch64-arm64|arm64-arm64) ;;
    *) echo "注意：宿主是 $host，要起的是 linux/$ARCH 容器。" >&2
       echo "      容器里的 systemd 作为 PID 1 极可能 SIGSEGV——翻译层支持不到那一层。" >&2
       echo "      本机验收请改用与宿主同架构的发布物（scripts/package.sh $host），" >&2
       echo "      架构那一维交给 CI。见 README.md 的「本机虚拟机」一节。" >&2 ;;
  esac
fi

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
IMG="strix-verify:$DISTRO${ARCH:+-$ARCH}"
NAME="strix-verify-$$"
ALICE_PW='alice-verify-pw'; BOB_PW='bob-verify-pw'

echo "== 构建镜像 $IMG（BASE=$BASE）=="
podman build ${PLATFORM[@]+"${PLATFORM[@]}"} --build-arg "BASE=$BASE" -t "$IMG" -f "$ROOT/scripts/verify/Containerfile" "$ROOT"

echo "== 起 systemd 容器 $NAME =="
# --privileged 不是偷懒：roadmap/07 §1.1 就写着「以 --privileged 或至少
# --cap-add SYS_ADMIN」，docker 驱动一直是这么起的，podman 驱动漏了。
# 少了它，**Ubuntu 上 polkit.service 根本起不来**——
#   polkit.service: Failed to keep CAP_SYS_ADMIN: Operation not permitted
#   ... status=217/USER
# 于是 #7 / #13 / #18 三条全红，报的却是
#   "Failed to activate service 'org.freedesktop.PolicyKit1': timed out"
# 这么一句离原因很远的话。实测 --cap-add=SYS_ADMIN 单独不够（接着卡在
# cgroup 委派：memory.pressure Permission denied，226/NAMESPACE），
# 配 --cgroupns=private 或 unmask=/sys/fs/cgroup 也不行，只有 --privileged 成。
# Rocky 9 不受影响（它的 polkit.service 没有那几条 hardening），
# 这正是跨发行版矩阵的意义。
podman run -d ${PLATFORM[@]+"${PLATFORM[@]}"} --name "$NAME" --systemd=always --hostname strix-verify \
  --privileged --cgroupns=host "$IMG" >/dev/null
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
# podman 5.x 与 docker 一样不会替你建目标目录（报 "could not be found on
# container ...: no such file or directory"）。docker 版一直有这一步，
# podman 版漏了——两个脚本本该逐条对齐。
podman exec "$NAME" mkdir -p /opt/strixmaid-dist
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
  -e BOB_PW="$BOB_PW" -e AGENT_BIN=/usr/bin/strixmaid \
  "$NAME" bash /opt/verify/agent-checks.sh
RC2=$?
set -e

echo; echo "== journalctl -u strixmaid（尾部，供排查）=="
podman exec "$NAME" journalctl -u strixmaid --no-pager -n 20 2>/dev/null || true

echo
echo "root-checks 退出码 $RC1；agent-checks 退出码 $RC2"
[ "$RC1" = 0 ] && [ "$RC2" = 0 ]
