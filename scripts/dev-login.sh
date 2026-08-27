#!/usr/bin/env bash
# 开发期登录：走完整的 challenge-response 协议，把 token 打到 stdout。
#
# 用法：
#   eval "$(scripts/dev-login.sh)"        # 直接把 STRIX_TOKEN 导进当前 shell
#   STRIX_TOKEN=$(scripts/dev-login.sh --raw) scripts/api-smoke.sh
#
#   scripts/dev-login.sh [--raw] [BASE_URL] [USERNAME]
#     --raw   只打印 token 本身，不打印 export 语句
#
# 密码用 `read -s` 从终端读，**不进命令行参数、不进环境变量、不写任何文件**
# （design.md §5.3：明文密码只在认证那一瞬间存在）。因此本脚本必须在真正的
# 终端里交互运行，不能在管道或 CI 里用。
#
# 兼容 bash 3.2（macOS 自带版本）。

set -uo pipefail

RAW=0
if [[ "${1:-}" == "--raw" ]]; then RAW=1; shift; fi
BASE="${1:-http://127.0.0.1:9700}"
USERNAME="${2:-$(id -un)}"

command -v jq >/dev/null || { echo "缺少 jq" >&2; exit 2; }

api() {
  curl -sS -X POST "$BASE/api/v1/$1" -H 'Content-Type: application/json' -d "$2"
}

# ---- 第一轮：要 prompts ----
RESP="$(api auth/start "$(jq -nc --arg u "$USERNAME" '{username:$u}')")"

if ! SESSION="$(echo "$RESP" | jq -er '.session')" 2>/dev/null; then
  echo "auth/start 失败：$RESP" >&2
  exit 1
fi

# ---- 逐轮应答，直到 complete ----
# PAM 是对话式的：2FA 会追问验证码、密码过期会要求改密，因此这里是个循环，
# 而不是「问一次密码就完事」（design.md §5.2）。
for _round in 1 2 3 4 5; do
  N="$(echo "$RESP" | jq '.prompts | length')"
  ANSWERS='[]'
  for ((i = 0; i < N; i++)); do
    ID="$(echo "$RESP" | jq ".prompts[$i].id")"
    STYLE="$(echo "$RESP" | jq -r ".prompts[$i].style")"
    TEXT="$(echo "$RESP" | jq -r ".prompts[$i].text")"
    case "$STYLE" in
      prompt)       # 不回显（密码）
        printf '%s ' "$TEXT" >&2
        IFS= read -rs VALUE
        printf '\n' >&2
        ;;
      prompt_echo)  # 回显（用户名、OTP 之类）
        printf '%s ' "$TEXT" >&2
        IFS= read -r VALUE
        ;;
      info|error)   # 只是给人看的消息，无需作答
        printf '[%s] %s\n' "$STYLE" "$TEXT" >&2
        VALUE=''
        ;;
      *)
        printf '未知的 prompt 风格 %s：%s\n' "$STYLE" "$TEXT" >&2
        VALUE=''
        ;;
    esac
    ANSWERS="$(echo "$ANSWERS" | jq -c --argjson id "$ID" --arg v "$VALUE" '. + [{id:$id, value:$v}]')"
    unset VALUE
  done

  RESP="$(api auth/respond "$(jq -nc --arg s "$SESSION" --argjson r "$ANSWERS" \
    '{session:$s, responses:$r}')")"
  unset ANSWERS

  STATUS="$(echo "$RESP" | jq -r '.status // "error"')"
  case "$STATUS" in
    complete)
      TOKEN="$(echo "$RESP" | jq -r '.token')"
      USER="$(echo "$RESP" | jq -r '.user.username')"
      UID_="$(echo "$RESP" | jq -r '.user.uid')"
      printf '登录成功：%s (uid=%s)\n' "$USER" "$UID_" >&2
      if [[ $RAW -eq 1 ]]; then
        printf '%s\n' "$TOKEN"
      else
        printf 'export STRIX_TOKEN=%s\n' "$TOKEN"
      fi
      exit 0
      ;;
    more)
      SESSION="$(echo "$RESP" | jq -r '.session')"
      ;;
    *)
      echo "认证失败：$RESP" >&2
      exit 1
      ;;
  esac
done

echo "对话轮次过多，放弃" >&2
exit 1
