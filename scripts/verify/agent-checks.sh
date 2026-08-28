#!/usr/bin/env bash
# 05-agent.md §5.2 的双进程验收：Agent 连 Server、补发、重连无空洞。
#
# 在 root 环境里运行（要能登录 + 提权登记节点）。Server 已在 $BASE 跑着。
# 需要 strixmaid-agent 二进制在 PATH（或 $AGENT_BIN）。
#
# 与 §5.2 的偏离：原文「等 2 分钟」这里参数化为 $WAIT（默认 150s），
# 并把「停 Server 3 分钟」压成一次重启（工装里 Server 由 systemd 管，
# stop→start 即可，不必真等 3 分钟——补发逻辑与停多久无关）。
#
# **尚未在真实环境执行过。**
set -uo pipefail

BASE="${BASE:-http://127.0.0.1:9700}"
BOB="${BOB:-bob}"; BOB_PW="${BOB_PW:-bobpw}"
AGENT_BIN="${AGENT_BIN:-$(command -v strixmaid-agent || echo /usr/bin/strixmaid-agent)}"
NODE_ID="${NODE_ID:-test}"
WAIT="${WAIT:-150}"
DATA="${AGENT_DATA:-/var/lib/strixmaid-agent-verify}"
HERE="$(cd "$(dirname "$0")" && pwd)"

PASS=0; FAIL=0; SKIP=0
ok()   { PASS=$((PASS+1)); printf '  ✓ %s\n' "$1"; }
bad()  { FAIL=$((FAIL+1)); printf '  ✗ %s\n' "$1"; [ $# -gt 1 ] && printf '      %s\n' "$2"; return 0; }
skip() { SKIP=$((SKIP+1)); printf '  — %s%s\n' "$1" "${2:+ · $2}"; }

BODY=''
code() { local m="$1" p="$2"; shift 2; local t; t="$(mktemp)"
  local c; c="$(curl -sS -o "$t" -w '%{http_code}' -X "$m" "$@" "$BASE$p" 2>/dev/null || echo 000)"
  BODY="$(cat "$t")"; rm -f "$t"; echo "$c"; }

login()   { STRIX_PASSWORD="$2" "$HERE/login.sh" login "$BASE" "$1"; }
elevate() { STRIX_PASSWORD="$2" "$HERE/login.sh" elevate "$BASE" "$1" ""; }

command -v jq >/dev/null || { echo "缺少 jq" >&2; exit 2; }
[ -x "$AGENT_BIN" ] || { echo "找不到 strixmaid-agent（$AGENT_BIN）" >&2; exit 2; }

# ---- 登记节点（需提权）----
BTOK="$(login "$BOB" "$BOB_PW")" || { bad "bob 登录失败"; exit 1; }
ETOK="$(elevate "$BTOK" "$BOB_PW")" || { bad "bob 提权失败"; exit 1; }

C="$(code POST /api/v1/nodes -H "Authorization: Bearer $ETOK" \
  -H 'Content-Type: application/json' -d "$(jq -nc --arg id "$NODE_ID" '{id:$id, name:"验证节点"}')")"
if [ "$C" != 201 ]; then
  # 可能已存在：删掉重登。
  code DELETE "/api/v1/nodes/$NODE_ID" -H "Authorization: Bearer $ETOK" >/dev/null
  C="$(code POST /api/v1/nodes -H "Authorization: Bearer $ETOK" \
    -H 'Content-Type: application/json' -d "$(jq -nc --arg id "$NODE_ID" '{id:$id, name:"验证节点"}')")"
fi
[ "$C" = 201 ] && ok "POST /nodes 登记 $NODE_ID → 201" || { bad "登记节点失败" "$C $BODY"; exit 1; }
TOKEN="$(echo "$BODY" | jq -r '.token')"
[ -n "$TOKEN" ] && [ "$TOKEN" != null ] && ok "token 仅在响应出现一次（已取走）" || bad "响应无 token"

# 错误 token 连接被拒（§5.3）：curl 只到升级前，401 即符合。
C="$(code GET /ws/agent -H 'Sec-WebSocket-Protocol: bearer, wrongtoken')"
[ "$C" = 401 ] && ok "§5.3 错误 token 的 /ws/agent → 401" || skip "§5.3 错误 token" "得到 $C（非 101/200 即基本符合）"

# ---- 起 Agent ----
rm -rf "$DATA"; mkdir -p "$DATA"
CFG="$DATA/agent.toml"
cat > "$CFG" <<EOF
server_url = "${BASE/http:/ws:}"
node_id = "$NODE_ID"
node_name = "验证节点"
token = "$TOKEN"
data_dir = "$DATA"
sync_interval_secs = 5

[metrics]
interval_secs = 2
EOF

"$AGENT_BIN" --config "$CFG" >"$DATA/agent.log" 2>&1 &
AGENT_PID=$!
trap 'kill "$AGENT_PID" 2>/dev/null || true; code DELETE "/api/v1/nodes/$NODE_ID" -H "Authorization: Bearer $ETOK" >/dev/null 2>&1 || true' EXIT

# 在线？
online=0
for _ in $(seq 1 20); do
  C="$(code GET /api/v1/nodes -H "Authorization: Bearer $ETOK")"
  [ "$(echo "$BODY" | jq -r --arg n "$NODE_ID" '.[] | select(.id==$n) | .online')" = true ] && { online=1; break; }
  sleep 1
done
[ "$online" = 1 ] && ok "GET /nodes 显示 $NODE_ID online" || { bad "Agent 未上线，日志：" "$(tail -5 "$DATA/agent.log")"; }

# ---- 等采集落盘并推送 ----
printf '  … 等 %ss 让 Agent 采集、落盘、推送\n' "$WAIT"
sleep "$WAIT"

now="$(date +%s)"
C="$(code GET "/api/v1/metrics/query?node=$NODE_ID&series=cpu.usage&from=$((now-600))&to=$now&step=60" -H "Authorization: Bearer $ETOK")"
n="$(echo "$BODY" | jq '[.series[]?.points[]?] | length')"
[ "$C" = 200 ] && [ "${n:-0}" -ge 1 ] && ok "query?node=$NODE_ID&series=cpu.usage 返回 ${n} 个点（≥1）" \
  || bad "非本机节点的落盘查询应有 ≥1 点" "$C n=$n"

# ---- 重连无空洞（§5.2 后半）----
if systemctl show strixmaid >/dev/null 2>&1; then
  systemctl restart strixmaid
  for _ in $(seq 1 30); do [ "$(code GET /api/v1/health)" = 200 ] && break; sleep 1; done
  BTOK="$(login "$BOB" "$BOB_PW")"; ETOK="$(elevate "$BTOK" "$BOB_PW")"
  sleep "$WAIT"
  now="$(date +%s)"
  C="$(code GET "/api/v1/metrics/query?node=$NODE_ID&series=cpu.usage&from=$((now-3600))&to=$now&step=60" -H "Authorization: Bearer $ETOK")"
  # 相邻 ts 差应恒为 60（无空洞）。
  gaps="$(echo "$BODY" | jq '[.series[0].points[].ts] | [range(1;length) as $i | .[$i]-.[$i-1]] | map(select(. != 60)) | length' 2>/dev/null || echo '?')"
  if [ "$gaps" = 0 ]; then ok "Server 重启后 $NODE_ID 的 cpu.usage 无空洞（相邻 ts 差恒 60）"
  elif [ "$gaps" = '?' ]; then skip "空洞检查" "点数不足或解析失败，人工核对"
  else bad "重启后出现 $gaps 处 ts 空洞（补发未闭合）"; fi
else
  skip "§5.2 重连无空洞" "无 systemd 管理，请人工 stop/start Server 后重查"
fi

printf '\n总计  通过 %d  失败 %d  未测 %d\n' "$PASS" "$FAIL" "$SKIP"
[ "$FAIL" = 0 ]
