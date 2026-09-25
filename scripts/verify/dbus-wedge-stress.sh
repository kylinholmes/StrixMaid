#!/bin/bash
# StrixMaid 总线死锁 —— 压力复现脚本（与 verify-fix.sh 配套）
#
# verify-fix.sh 验的是「没卡住」，但它不制造触发条件。这个脚本负责制造：
# 旧代码的死锁需要**一次事件刷新期间涌入 ≥64 条 systemd 信号**
# （zbus proxy 信号流的默认容量 DEFAULT_MAX_QUEUED=64），所以这里用
# daemon-reload 与批量 unit 启停连续灌信号，同时按秒采样那条 bus socket
# 的 Recv-Q。
#
# 用法（<测试机> 上，需 root 读别的进程的 socket）：
#   sudo bash stress-bus.sh              # 默认 6 轮
#   sudo ROUNDS=20 bash stress-bus.sh
#
# **前提**：必须有一个客户端正订阅着 services.changed（浏览器打开服务页）。
# 修复后监听是按需存在的——没有订阅者时监听根本不启动，那样跑出来的
# 「全绿」什么也证明不了。脚本会先检查这一点。
set -uo pipefail

ROUNDS=${ROUNDS:-6}
FAIL=0

if [[ "$(id -u)" -ne 0 ]] && command -v sudo >/dev/null 2>&1 && sudo -n true 2>/dev/null; then
  exec sudo -n env ROUNDS="$ROUNDS" bash "$0" "$@"
fi

say()  { printf '\n\033[1m== %s\033[0m\n' "$*"; }
ok()   { printf '  \033[32mPASS\033[0m %s\n' "$*"; }
bad()  { printf '  \033[31mFAIL\033[0m %s\n' "$*"; FAIL=1; }
note() { printf '  ---- %s\n' "$*"; }

# 与 verify-fix.sh 同一个取法：锚定 State 字段再取它后面那个。
recvq_of() {
  ss -xap 2>/dev/null | grep -F "pid=$1," \
    | awk '{ for (i=1;i<=NF;i++)
               if ($i ~ /^(ESTAB|UNCONN|LISTEN|CONN|DISCONN|CLOSE|SYN-SENT|SYN-RECV)$/) { print $(i+1); break } }' \
    | sort -rn | head -1
}

PID=$(systemctl show strixmaid -p ExecMainPID --value 2>/dev/null)
[[ -z "$PID" || "$PID" == "0" ]] && { echo "strixmaid 没在跑"; exit 2; }

say "0. 前提检查：有没有人正订阅 services.changed"
# 监听任务起来时会打 debug 日志「systemd 事件监听已启动」。
# 需要服务以 RUST_LOG=debug 运行才看得到。
STARTED=$(journalctl -u strixmaid --no-pager 2>/dev/null | grep -c '事件监听已启动')
RETIRED=$(journalctl -u strixmaid --no-pager 2>/dev/null | grep -c '事件监听退场')
note "监听启动 $STARTED 次、退场 $RETIRED 次"
if [[ "$STARTED" -le "$RETIRED" ]]; then
  bad "当前没有活着的监听——请先在浏览器里打开服务页并保持，否则这轮压力测不到东西"
  note "（日志看不到？服务要以 RUST_LOG=debug 跑：systemctl edit strixmaid 加 Environment=RUST_LOG=debug）"
  exit 2
fi
ok "有活着的监听，可以施压"

BASE_Q=$(recvq_of "$PID"); BASE_Q=${BASE_Q:-0}
BASE_REJ=$(journalctl -u dbus --since '-1 min' --no-pager 2>/dev/null | grep -c 'full message queue')
note "起点：Recv-Q=${BASE_Q} bytes，近 1 分钟拒绝日志 ${BASE_REJ} 行"

say "1. 灌信号（${ROUNDS} 轮）"
# 后台按秒采样 Recv-Q，与施压并行——死锁发生在施压**过程中**，事后采样看不到爬升。
SAMPLES=$(mktemp)
( while :; do echo "$(date +%H:%M:%S) $(recvq_of "$PID")"; sleep 1; done ) > "$SAMPLES" &
SAMPLER=$!
trap 'kill $SAMPLER 2>/dev/null' EXIT

for i in $(seq 1 "$ROUNDS"); do
  printf '  [%d/%d] ' "$i" "$ROUNDS"
  # daemon-reload：一次就会对**所有** unit 发 PropertiesChanged，是最猛的信号源。
  systemctl daemon-reload
  # 批量启停：造 JobRemoved / UnitNew / UnitRemoved。挑无害的 oneshot 目标。
  for u in systemd-tmpfiles-clean.service man-db.service systemd-journal-flush.service; do
    systemctl start "$u" >/dev/null 2>&1 &
  done
  wait
  echo "reload + 启停完成"
done

sleep 3
kill $SAMPLER 2>/dev/null; trap - EXIT

say "2. 结果"
MAX_Q=$(awk '{print $2+0}' "$SAMPLES" | sort -rn | head -1)
END_Q=$(recvq_of "$PID"); END_Q=${END_Q:-0}
note "采样 $(wc -l < "$SAMPLES") 次，峰值 Recv-Q=${MAX_Q} bytes，结束时 ${END_Q} bytes"
note "采样明细：$SAMPLES"

# 判据：施压期间允许瞬时积压，但必须能排空。事故时是固定 47182 且从不下降。
if [[ "${MAX_Q:-0}" -lt 65536 ]]; then
  ok "峰值 Recv-Q < 64 KB"
else
  note "峰值偏高（${MAX_Q}），看是否排空"
fi
if [[ "${END_Q:-0}" -lt 8192 ]]; then
  ok "施压结束后 Recv-Q 已排空（<8 KB）——读取任务始终在读"
else
  bad "施压结束后 Recv-Q 仍有 ${END_Q} bytes 未读，疑似又卡住了"
fi

REJ=$(journalctl -u dbus --since '-3 min' --no-pager 2>/dev/null | grep -c 'full message queue')
[[ "$REJ" -eq 0 ]] && ok "施压期间 dbus 0 行拒绝日志" || bad "施压期间出现 $REJ 行拒绝日志"

T0=$(date +%s%N)
timeout 30 systemctl is-system-running >/dev/null 2>&1
MS=$(( ($(date +%s%N) - T0) / 1000000 ))
[[ "$MS" -lt 200 ]] && ok "systemctl 仍然秒回（${MS} ms）" || bad "systemctl 变慢：${MS} ms"

say "结论"
[[ "$FAIL" -eq 0 ]] && printf '  \033[32m压力下未复现\033[0m\n' || printf '  \033[31m疑似复现\033[0m：按 README §3.2 抓现场\n'
exit "$FAIL"
