#!/bin/sh
# 组装发布 tar.gz（roadmap/06 §3.5）。
#
# 用法：scripts/package.sh [x86_64|aarch64]
#
# 依赖：
#   - musl-tools（x86_64-linux-musl-gcc）：strixmaid 的静态构建；
#     aarch64 需要 cargo-zigbuild（cargo install cargo-zigbuild && zig 在 PATH）。
#   - helper 构建为动态 glibc；发布机 glibc 应不高于目标基线（2.28，Debian 10 /
#     RHEL 8），或改用 `cargo zigbuild --target x86_64-unknown-linux-gnu.2.28`。
# 缺工具链时报错退出并说明装法，不产出「看起来是静态其实不是」的包。
set -eu

arch="${1:-x86_64}"
case "$arch" in
    x86_64)  musl_target=x86_64-unknown-linux-musl ;;
    aarch64) musl_target=aarch64-unknown-linux-musl ;;
    *) echo "未知架构 $arch（支持 x86_64 / aarch64）" >&2; exit 2 ;;
esac
# helper 动态链接 glibc，必须与 strixmaid 同架构。
# 原先这里写死 x86_64：`package.sh aarch64` 会把一个 x86_64 的 helper
# 装进 aarch64 的包里，装上去之后 PAM 认证在第一步就失败。
gnu_target="$arch-unknown-linux-gnu"

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"
version=$(grep -m1 '^version' Cargo.toml | sed 's/.*"\(.*\)".*/\1/')
# cargo 的产物目录可被 CARGO_TARGET_DIR 改写。虚拟机里源码走 virtiofs、
# 产物写客机本地盘会快很多，下面一律用这个变量而不是字面量 target/。
td="${CARGO_TARGET_DIR:-target}"

# 前端产物。web/dist 不在 git 里（它是 web/src 的派生物，跟踪必然漂移），
# 所以每次出包都在这里重建一次——本地与 CI 因此走同一条路，
# 不会出现「CI 的包是新的、本地打的包是旧的」。
command -v bun >/dev/null 2>&1 || {
    echo "缺 bun：前端产物 web/dist 由 bun 构建，见 https://bun.sh" >&2
    exit 3
}
( cd web && bun install --frozen-lockfile && bun run build )
[ -f web/dist/index.html ] || {
    echo "前端构建未产出 web/dist/index.html" >&2
    exit 3
}

if [ "$arch" = "x86_64" ]; then
    command -v x86_64-linux-musl-gcc >/dev/null 2>&1 || {
        echo "缺 x86_64-linux-musl-gcc：apt install musl-tools（libsqlite3-sys 要编 C 源）" >&2
        exit 3
    }
    cargo build --release --target "$musl_target" -p strixmaid
elif [ "$(uname -s)-$(uname -m)" = "Linux-$arch" ]; then
    # 同架构的 Linux 上这是**原生构建**，不是交叉编译，不需要 zigbuild。
    # （典型场景：Apple Silicon 上的 aarch64 Linux 虚拟机，见
    # scripts/verify/vm/lima-strix.yaml。）
    command -v "$arch-linux-musl-gcc" >/dev/null 2>&1 || {
        echo "缺 $arch-linux-musl-gcc（libsqlite3-sys 要编 C 源）。" >&2
        echo "  Debian/Ubuntu: apt install musl-tools" >&2
        echo "  Fedora:        dnf install musl-gcc &&" >&2
        echo "                 ln -s /usr/bin/musl-gcc /usr/local/bin/$arch-linux-musl-gcc" >&2
        exit 3
    }
    cargo build --release --target "$musl_target" -p strixmaid
else
    command -v cargo-zigbuild >/dev/null 2>&1 || {
        echo "缺 cargo-zigbuild：cargo install cargo-zigbuild（并安装 zig）" >&2
        exit 3
    }
    cargo zigbuild --release --target "$musl_target" -p strixmaid
fi
cargo build --release --target "$gnu_target" -p strixmaid-helper

# 静态性断言（§3.1）：不产出动态链接的「静态包」。
for bin in strixmaid; do
    f="$td/$musl_target/release/$bin"
    if ldd "$f" 2>&1 | grep -qv 'not a dynamic executable\|statically linked'; then
        echo "$f 不是静态链接：" >&2; ldd "$f" >&2; exit 4
    fi
done

out="strixmaid-$version-$arch"
stage=$(mktemp -d)
mkdir -p "$stage/$out/packaging/pam.d"
cp "$td/$musl_target/release/strixmaid"       "$stage/$out/"
cp "$td/$gnu_target/release/strixmaid-helper" "$stage/$out/"
cp packaging/strixmaid.service packaging/strixmaid-agent.service "$stage/$out/packaging/"
cp packaging/install.sh "$stage/$out/packaging/"
cp packaging/pam.d/strixmaid.debian packaging/pam.d/strixmaid.rhel "$stage/$out/packaging/pam.d/"
cp LICENSE "$stage/$out/"

tar -C "$stage" -czf "$out.tar.gz" "$out"
rm -rf "$stage"
ls -l "$out.tar.gz"
echo "体积（验收 §5.2：strixmaid ≤ 18MiB、helper ≤ 1MiB；agent 已并入 strixmaid）："
ls -l "$td/$musl_target/release/strixmaid" \
      "$td/$gnu_target/release/strixmaid-helper" | awk '{print $5, $NF}'
