#!/usr/bin/env bash
# 把验证虚拟机装成一台能出 Linux 发布物的构建机（在**客机内**执行）。
#
#   limactl shell strix -- scripts/verify/vm/setup-build.sh
#
# 为什么本机要能自己出包：CI 只出 x86_64，而 Apple Silicon 上的客机是 aarch64。
# 「把 CI 的 x86_64 包搬进来跑」这条路走不通——x86_64 的 **systemd 当 PID 1 时**
# 在 Rosetta 与 qemu-user 下都会 SIGSEGV（普通程序没事，只有 PID 1 会崩），
# 而 07 的验收正需要容器里那个 systemd。所以本机验收一律走原生 aarch64，
# 架构差异那一维交给 CI。
set -euo pipefail

sudo dnf -y install \
    gcc git jq curl unzip \
    musl-gcc musl-libc-static

# cc-rs 给 musl 目标找的编译器名字是 <arch>-linux-musl-gcc；
# Fedora 的 musl-gcc 包只给 /usr/bin/musl-gcc 这一个名字。
arch=$(uname -m)
[ -e "/usr/local/bin/$arch-linux-musl-gcc" ] || \
    sudo ln -s /usr/bin/musl-gcc "/usr/local/bin/$arch-linux-musl-gcc"

# Fedora 没有 musl 的 rust std（repo 里只有 uefi / wasm / windows 那几个交叉目标），
# 所以用 rustup 而不是 dnf 的 rust。
if ! command -v rustup >/dev/null 2>&1; then
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y --no-modify-path
fi
. "$HOME/.cargo/env"
rustup target add "$arch-unknown-linux-musl"

# 前端产物 web/dist 不在 git 里，而 rust-embed 的 folder 在 debug 下同样要求
# 目录存在，所以构建机必须有 bun。
if ! command -v bun >/dev/null 2>&1; then
    curl -fsSL https://bun.sh/install | bash
fi

cat <<'EOF'

装好了。出包（源码走 virtiofs，产物写客机本地盘）：

    export PATH="$HOME/.cargo/bin:$HOME/.bun/bin:$PATH"
    export CARGO_TARGET_DIR="$HOME/target"
    cd /Users/kylin/orca/StrixMaid
    scripts/package.sh aarch64

然后：

    tar xzf strixmaid-0.1.0-aarch64.tar.gz
    scripts/verify/run-in-podman.sh --dist strixmaid-0.1.0-aarch64 --distro ubuntu
    scripts/verify/run-in-podman.sh --dist strixmaid-0.1.0-aarch64 --distro rocky
EOF
