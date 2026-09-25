#!/bin/bash
# StrixMaid 总线卡死 —— 修复后自检脚本
#
# 用法（在 <测试机> 上执行，建议 root，便于读取目标进程的 /proc）：
#   sudo bash dbus-wedge-check.sh            # 默认监控 60 秒
#   sudo MONITOR_SECS=300 bash dbus-wedge-check.sh
#
# 判据（全部满足才算通过）：
#   1. dbus-daemon 拒绝日志 0 行（本次样本：修复前 4 行/分钟，持续 25 小时）
#   2. strixmaid 的 bus socket Recv-Q 稳定（不单调增长、< 8 KB）
#   3. systemctl / Peer.Ping 延迟 < 100 ms
#   4. systemd-logind 无 "Connection timed out"，sshd 无 pam_systemd 失败
#   5. strixmaid 自身健康端点 200
set -uo pipefail

MONITOR_SECS=${MONITOR_SECS:-60}
SAMPLE_INTERVAL=5
RECVQ_LIMIT=8192          # 8 KB
LATENCY_LIMIT_MS=100
FAIL=0

# 读别的进程的 socket/fd 需要 root；能免密 sudo 就自动提权重跑。
if [[ "$(id -u)" -ne 0 ]] && command -v sudo >/dev/null 2>&1 && sudo -n true 2>/dev/null; then
  echo "（自动提权：sudo）"
  exec sudo -n env MONITOR_SECS="$MONITOR_SECS" bash "$0" "$@"
fi

say()  { printf '\n\033[1m== %s\033[0m\n' "$*"; }
ok()   { printf '  \033[32mPASS\033[0m %s\n' "$*"; }
bad()  { printf '  \033[31mFAIL\033[0m %s\n' "$*"; FAIL=1; }
warn() { printf '  \033[33mWARN\033[0m %s\n' "$*"; }
note() { printf '  ---- %s\n' "$*"; }

since_of() { date -d "$1" '+%Y-%m-%d %H:%M:%S'; }

# 取某进程所有 unix socket 里最大的 Recv-Q（字节）。
# 注意：`ss -xap` 的输出首列是 Netid（u_str），不能直接取 $2（那是 State）。
# 这里锚定 State 字段再取它后面那一个字段（兼容 Netid 列存在/不存在），并取所有 socket 的最大值。
recvq_of() {
  ss -xap 2>/dev/null | grep -F "pid=$1," \
    | awk '{ for (i=1;i<=NF;i++)
               if ($i ~ /^(ESTAB|UNCONN|LISTEN|CONN|DISCONN|CLOSE|SYN-SENT|SYN-RECV)$/) { print $(i+1); break } }' \
    | sort -rn | head -1
}

# 毫秒级计时（不依赖 GNU time 是否安装）
elapsed_ms() { echo $(( ($(date +%s%N) - $1) / 1000000 )); }

say "1. 服务状态"
WIN="-30 min"   # 未运行时的默认观察窗口
WIN_DESC="最近 30 分钟"
if systemctl is-active --quiet strixmaid; then
  PID=$(systemctl show strixmaid -p ExecMainPID --value)
  note "strixmaid active, PID=$PID"
  if [[ -n "$PID" && "$PID" != "0" ]]; then
    STARTED=$(ps -o lstart= -p "$PID" | xargs)
    note "启动时间: $STARTED"
    SINCE=$(since_of "$STARTED")
    WIN="$SINCE"
    WIN_DESC="本进程启动以来（$STARTED）"
  fi
else
  note "strixmaid 未运行：下面只能验证系统侧是否干净"
  PID=""
  # 以"服务停止时刻 +1s"作为观察窗口起点，这样历史事故行不会误判为当前问题。
  EXIT_TS=$(systemctl show strixmaid -p ExecMainExitTimestamp --value 2>/dev/null)
  if [[ -n "$EXIT_TS" && "$EXIT_TS" != "n/a" ]] && date -d "$EXIT_TS" +%s >/dev/null 2>&1; then
    WIN=$(date -d "@$(( $(date -d "$EXIT_TS" +%s) + 1 ))" '+%Y-%m-%d %H:%M:%S')
    WIN_DESC="strixmaid 停止以来（$EXIT_TS）"
  fi
fi

say "2. dbus-daemon 拒绝日志"
TOTAL=$(journalctl -u dbus --no-pager 2>/dev/null | grep -c 'full message queue')
note "本次开机累计: $TOTAL 行（历史事故为 7121 行 / 25 小时；历史行按时间戳区分）"
RECENT=$(journalctl -u dbus --since "$WIN" --no-pager 2>/dev/null | grep -c 'full message queue')
if [[ "$RECENT" -eq 0 ]]; then
  ok "$WIN_DESC 内 0 行拒绝日志"
else
  bad "$WIN_DESC 内出现 $RECENT 行拒绝日志（下面是最新 3 行）"
  journalctl -u dbus --since "$WIN" --no-pager 2>/dev/null | grep 'full message queue' | tail -3
fi
RECENT_T=$(journalctl -u dbus --since "$WIN" --no-pager 2>/dev/null | grep -c 'max_replies_per_connection')
[[ "$RECENT_T" -eq 0 ]] && ok "未触及 max_replies_per_connection 上限" \
                        || bad "触及 max_replies_per_connection 上限 $RECENT_T 次"

say "3. dbus-daemon 资源"
DBUS_PID=$(pgrep -x dbus-daemon | head -1)
if [[ -n "${DBUS_PID:-}" ]]; then
  RSS=$(ps -o rss= -p "$DBUS_PID" | tr -d ' ')
  FRESH=$(journalctl -u dbus --since '-2 min' --no-pager 2>/dev/null | grep -c 'full message queue')
  note "dbus-daemon PID=$DBUS_PID RSS=${RSS} KB（事故时 138340 KB）"
  if [[ "$RSS" -lt 61440 ]]; then
    ok "RSS 正常 (<60 MB)"
  elif [[ "$FRESH" -eq 0 && "$TOTAL" -gt 0 ]]; then
    warn "RSS 偏高 (${RSS} KB)：历史事故遗留（dbus-daemon 不归还内存），重启后应回落；近 2 分钟无拒绝日志，暂不影响功能"
  else
    bad "RSS 偏高 (${RSS} KB) 且近 2 分钟仍有 $FRESH 行拒绝日志，可能有连接在积压"
  fi
fi

say "4. strixmaid 总线 socket Recv-Q（监控 ${MONITOR_SECS}s）"
if [[ -n "$PID" ]]; then
  MAX_Q=0; LAST_Q=0; GROWTH=0; N=0
  DEADLINE=$(( $(date +%s) + MONITOR_SECS ))
  while [[ $(date +%s) -lt "$DEADLINE" ]]; do
    Q=$(recvq_of "$PID")
    [[ "${Q:-0}" =~ ^[0-9]+$ ]] && Q=$Q || Q=0
    N=$((N+1))
    printf '  [%02d] %s  Recv-Q=%s bytes\n' "$N" "$(date +%H:%M:%S)" "$Q"
    [[ "$Q" -gt "$MAX_Q" ]] && MAX_Q=$Q
    [[ "$Q" -gt "$LAST_Q" ]] && GROWTH=$((GROWTH+1))
    LAST_Q=$Q
    sleep "$SAMPLE_INTERVAL"
  done
  note "峰值 Recv-Q=${MAX_Q} bytes，${N} 次采样中 ${GROWTH} 次比上次大"
  if [[ "$MAX_Q" -lt "$RECVQ_LIMIT" && "$GROWTH" -le 2 ]]; then
    ok "Recv-Q 稳定（事故时固定 47182 bytes 且无读取者）"
  else
    bad "Recv-Q 异常增长（阈值 ${RECVQ_LIMIT} bytes），总线上很可能又堆积了未消费的消息"
    note "抓现场：sudo ls -l /proc/$PID/fd; sudo ss -xaep | grep -F pid=$PID; sudo sh -c 'for t in /proc/'\$PID'/task/*; do echo \$t; cat \$t/stack; done'"
  fi
else
  note "跳过（strixmaid 未运行）"
fi

say "5. 总线调用延迟"
T0=$(date +%s%N)
timeout 30 dbus-send --system --print-reply --dest=org.freedesktop.systemd1 \
  /org/freedesktop/systemd1 org.freedesktop.DBus.Peer.Ping >/dev/null 2>&1
PING_MS=$(elapsed_ms "$T0")
note "Peer.Ping → ${PING_MS} ms"
[[ "$PING_MS" -lt "$LATENCY_LIMIT_MS" ]] && ok "Peer.Ping < ${LATENCY_LIMIT_MS} ms" \
                                         || bad "Peer.Ping 过慢：${PING_MS} ms（30 s 超时上限）"

T0=$(date +%s%N)
timeout 30 systemctl is-system-running >/dev/null 2>&1
SYSD_MS=$(elapsed_ms "$T0")
note "systemctl is-system-running → ${SYSD_MS} ms"
[[ "$SYSD_MS" -lt "$LATENCY_LIMIT_MS" ]] && ok "systemctl < ${LATENCY_LIMIT_MS} ms" \
                                         || bad "systemctl 过慢：${SYSD_MS} ms（事故时 25 s 超时，无法完成）"

say "6. logind / sshd 关联症状"
T1=$(journalctl -u systemd-logind --since "$WIN" --no-pager 2>/dev/null | grep -c 'Connection timed out')
T2=$(journalctl -u ssh --since "$WIN" --no-pager 2>/dev/null | grep -c 'pam_systemd.*Failed')
note "窗口: $WIN_DESC"
[[ "$T1" -eq 0 ]] && ok "logind 无 Connection timed out" || bad "logind 出现 $T1 次 Connection timed out"
[[ "$T2" -eq 0 ]] && ok "sshd 无 pam_systemd 失败" || bad "sshd 出现 $T2 次 pam_systemd 失败（登录会卡 25 s）"

say "7. strixmaid 自身健康端点"
if [[ -n "$PID" ]]; then
  CODE=$(curl -sS -m 6 -o /dev/null -w '%{http_code}' http://127.0.0.1:9700/ 2>/dev/null)
  [[ "$CODE" == "200" ]] && ok "HTTP 200" || bad "HTTP 探活返回 '$CODE'"
fi

say "8. 进程/线程快照"
if [[ -n "$PID" ]]; then
  note "线程数: $(ls /proc/$PID/task | wc -l)"
  note "fd 数:   $(ls /proc/$PID/fd 2>/dev/null | wc -l)"
  note "线程状态:"
  for t in /proc/$PID/task/*; do
    printf '    %s state=%s wchan=%s\n' "$(basename "$t")" \
      "$(awk '{print $3}' "$t/stat" 2>/dev/null)" "$(cat "$t/wchan" 2>/dev/null)"
  done
  note "任何线程卡在 D 状态、或 Recv-Q 只增不减，都应立刻按 §3.2 抓现场"
fi

say "结论"
if [[ "$FAIL" -eq 0 ]]; then
  printf '  \033[32m全部通过\033[0m：system bus 健康，strixmaid 没有再把总线拖死。\n'
else
  printf '  \033[31m存在失败项\033[0m：见上面 FAIL 行；请按 docs/incidents/2026-09-25-dbus-wedge.md §3.2 抓取现场证据。\n'
fi
exit "$FAIL"
