#!/usr/bin/env bash
# 非交互登录 / 提权助手 —— **仅供 07 验证工装在一次性测试容器里使用**。
#
# 与 scripts/dev-login.sh 的区别：dev-login 用 `read -s` 从终端读密码，
# 明文不进环境（design.md §5.3）。本脚本从 $STRIX_PASSWORD 取密码以便无人值守，
# 这只在**用完即弃的测试容器**里可接受——绝不要用它登录真实系统。
#
#   login.sh login  BASE USER            # 打印会话 token
#   login.sh elevate BASE TOKEN [USER]   # 用已有 token 提权，打印（不变的）token
#
# 密码来自 $STRIX_PASSWORD。需要 jq、curl。
# 兼容 bash 3.2；变量紧跟中文时一律 ${var}（3.2 按字节解析变量名）。
set -uo pipefail

MODE="${1:?login|elevate}"; BASE="${2:?BASE_URL}"
PW="${STRIX_PASSWORD:?需要环境变量 STRIX_PASSWORD}"
command -v jq >/dev/null || { echo "缺少 jq" >&2; exit 2; }

# start / respond 的端点随 MODE 变；elevate 需要带 Bearer。
if [ "$MODE" = login ]; then
  USER="${3:-alice}"; START=auth/start; RESPOND=auth/respond; AUTH=()
elif [ "$MODE" = elevate ]; then
  TOKEN="${3:?TOKEN}"; USER="${4:-}"; START=auth/elevate/start; RESPOND=auth/elevate/respond
  AUTH=(-H "Authorization: Bearer $TOKEN")
else
  echo "未知模式 $MODE" >&2; exit 2
fi

post() {  # post PATH JSON
  curl -sS "${AUTH[@]+"${AUTH[@]}"}" -X POST "$BASE/api/v1/$1" \
    -H 'Content-Type: application/json' -d "$2"
}

RESP="$(post "$START" "$(jq -nc --arg u "$USER" '{username:$u}')")"
SESSION="$(echo "$RESP" | jq -er '.session')" || { echo "start 失败：$RESP" >&2; exit 1; }

for _round in 1 2 3 4 5; do
  N="$(echo "$RESP" | jq '.prompts | length')"
  ANSWERS='[]'
  for ((i = 0; i < N; i++)); do
    ID="$(echo "$RESP" | jq ".prompts[$i].id")"
    STYLE="$(echo "$RESP" | jq -r ".prompts[$i].style")"
    # 只有隐藏输入（style=prompt）是密码；其余（echo / info / error）答空。
    if [ "$STYLE" = prompt ]; then VALUE="$PW"; else VALUE=''; fi
    ANSWERS="$(echo "$ANSWERS" | jq -c --argjson id "$ID" --arg v "$VALUE" '. + [{id:$id, value:$v}]')"
  done
  RESP="$(post "$RESPOND" "$(jq -nc --arg s "$SESSION" --argjson r "$ANSWERS" '{session:$s, responses:$r}')")"
  STATUS="$(echo "$RESP" | jq -r '.status // "error"')"
  case "$STATUS" in
    complete) echo "$RESP" | jq -r '.token'; exit 0 ;;
    more)     SESSION="$(echo "$RESP" | jq -r '.session')" ;;
    *)        echo "$MODE 失败：$RESP" >&2; exit 1 ;;
  esac
done
echo "对话轮次过多，放弃" >&2; exit 1
