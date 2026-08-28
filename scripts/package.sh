#!/bin/sh
# 组装发布 tar.gz（roadmap/06 §3.5）。
#
# 用法：scripts/package.sh [x86_64|aarch64]
#
# 依赖：
#   - musl-tools（x86_64-linux-musl-gcc）：strixmaid / strixmaid-agent 的静态构建；
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
gnu_target=x86_64-unknown-linux-gnu

root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$root"
version=$(grep -m1 '^version' Cargo.toml | sed 's/.*"\(.*\)".*/\1/')

if [ "$arch" = "x86_64" ]; then
    command -v x86_64-linux-musl-gcc >/dev/null 2>&1 || {
        echo "缺 x86_64-linux-musl-gcc：apt install musl-tools（libsqlite3-sys 要编 C 源）" >&2
        exit 3
    }
    cargo build --release --target "$musl_target" -p strixmaid-server -p strixmaid-agent
else
    command -v cargo-zigbuild >/dev/null 2>&1 || {
        echo "缺 cargo-zigbuild：cargo install cargo-zigbuild（并安装 zig）" >&2
        exit 3
    }
    cargo zigbuild --release --target "$musl_target" -p strixmaid-server -p strixmaid-agent
fi
cargo build --release --target "$gnu_target" -p strixmaid-helper

# 静态性断言（§3.1）：不产出动态链接的「静态包」。
for bin in strixmaid strixmaid-agent; do
    f="target/$musl_target/release/$bin"
    if ldd "$f" 2>&1 | grep -qv 'not a dynamic executable\|statically linked'; then
        echo "$f 不是静态链接：" >&2; ldd "$f" >&2; exit 4
    fi
done

out="strixmaid-$version-$arch"
stage=$(mktemp -d)
mkdir -p "$stage/$out/packaging/pam.d"
cp "target/$musl_target/release/strixmaid"       "$stage/$out/"
cp "target/$musl_target/release/strixmaid-agent" "$stage/$out/"
cp "target/$gnu_target/release/strixmaid-helper" "$stage/$out/"
cp packaging/strixmaid.service packaging/strixmaid-agent.service "$stage/$out/packaging/"
cp packaging/install.sh "$stage/$out/packaging/"
cp packaging/pam.d/strixmaid.debian packaging/pam.d/strixmaid.rhel "$stage/$out/packaging/pam.d/"
cp LICENSE "$stage/$out/"

tar -C "$stage" -czf "$out.tar.gz" "$out"
rm -rf "$stage"
ls -l "$out.tar.gz"
echo "体积（验收 §5.2：strixmaid ≤ 15MiB、agent ≤ 8MiB、helper ≤ 1MiB）："
ls -l "target/$musl_target/release/strixmaid" "target/$musl_target/release/strixmaid-agent" \
      "target/$gnu_target/release/strixmaid-helper" | awk '{print $5, $NF}'
