#!/usr/bin/env bash
# 在专用 Fedora Lima VM 内执行；仅安装真实 D-Bus 验证的原生构建依赖。
# limactl shell strix-dbus -- bash scripts/verify/vm/setup-dbus.sh
set -euo pipefail
sudo dnf -y install gcc jq curl dbus-tools iproute procps-ng python3-websockets pam-devel man-db
if ! command -v rustup >/dev/null 2>&1; then
    curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs -o /tmp/strix-rustup.sh
    sh /tmp/strix-rustup.sh -y --no-modify-path --profile minimal --default-toolchain 1.98.0
fi
source "$HOME/.cargo/env"
rustup component add clippy rustfmt
printf '%s\n' '依赖就绪。构建产物请放客机本地盘：export CARGO_TARGET_DIR="$HOME/target" RUSTC_WRAPPER='
