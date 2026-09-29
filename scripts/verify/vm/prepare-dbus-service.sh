#!/usr/bin/env bash
# 只在专用验证 VM 内运行。创建独立测试账号、临时单元和 systemd 托管的 server。
# STRIXMAID_DBUS_TEST_VM=1 bash scripts/verify/vm/prepare-dbus-service.sh
# 源码根目录执行；先 cargo build --workspace，CARGO_TARGET_DIR 默认 $HOME/target。
set -euo pipefail
: "${STRIXMAID_DBUS_TEST_VM:?仅供专用测试 VM；确认后设置 STRIXMAID_DBUS_TEST_VM=1}"
test "$STRIXMAID_DBUS_TEST_VM" = 1
build_dir=${CARGO_TARGET_DIR:-$HOME/target}
export STRIX_DBUS_PASSWORD_FILE="$HOME/.local/state/strixmaid-dbus/password"
test -x "$build_dir/debug/strixmaid"
test -x "$build_dir/debug/strixmaid-helper"
if systemctl is-active --quiet strixmaid; then
    echo 'strixmaid 已运行；请先确认并停止该测试服务' >&2
    exit 1
fi
if ! id strix-dbus-test >/dev/null 2>&1; then
    # Lima 默认用户可能占满 subordinate UID 区间；此账号不运行 rootless 容器。
    sudo useradd -K SUB_UID_COUNT=0 -K SUB_GID_COUNT=0 -m strix-dbus-test
    python3 - <<'PY'
import os
import secrets
import subprocess
password = secrets.token_urlsafe(24)
os.makedirs(os.path.dirname(os.environ['STRIX_DBUS_PASSWORD_FILE']), mode=0o700, exist_ok=True)
fd = os.open(os.environ['STRIX_DBUS_PASSWORD_FILE'], os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
with os.fdopen(fd, 'w') as f:
    f.write(password)
subprocess.run(['sudo', 'chpasswd'], input='strix-dbus-test:' + password + '\n', text=True, check=True)
PY
fi
# 已有账号必须同时有本脚本生成的密码文件；不擅自重设其他账号。
test -r "$STRIX_DBUS_PASSWORD_FILE"
sudo install -m 644 packaging/pam.d/strixmaid.rhel /etc/pam.d/strixmaid
sudo install -m 755 "$build_dir/debug/strixmaid" "$build_dir/debug/strixmaid-helper" /usr/local/bin/
# systemd 无法执行 home 标签的文件；保留 SELinux，使用标准可执行文件标签。
sudo restorecon /usr/local/bin/strixmaid /usr/local/bin/strixmaid-helper
cat > /tmp/strix-dbus-config.toml <<'CONF'
listen = "127.0.0.1:9700"
data_dir = "/var/lib/strixmaid-dbus-test"
helper_path = "/usr/local/bin/strixmaid-helper"
CONF
cat > /tmp/strixmaid-dbus-probe@.service <<'UNIT'
[Unit]
Description=StrixMaid D-Bus verification probe %i
[Service]
Type=simple
ExecStart=/usr/bin/sleep 600
UNIT
sudo install -m 644 /tmp/strixmaid-dbus-probe@.service /run/systemd/system/strixmaid-dbus-probe@.service
sudo systemctl daemon-reload
sudo systemctl reset-failed strixmaid 2>/dev/null || true
sudo systemd-run --unit=strixmaid --service-type=exec \
    --setenv=RUST_LOG=info,strixmaid_core::providers::service::bus=debug \
    /usr/local/bin/strixmaid --config /tmp/strix-dbus-config.toml serve
