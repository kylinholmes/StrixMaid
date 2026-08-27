#!/usr/bin/env bash
# StrixMaid API 冒烟测试：对照 OpenAPI 规范逐端点跑一遍，结果可复现、可进 CI。
#
# 用法：
#   scripts/api-smoke.sh [BASE_URL]          # 默认 http://127.0.0.1:9700
#
# 环境变量：
#   STRIX_TOKEN   已有的会话 token；给了就跑需要认证的端点，不给就只跑公开端点
#                 并断言受保护端点确实返回 401
#
# 退出码：0 全部通过；1 有断言失败。
#
# 设计要点：
#   * **端点清单从 /api/v1/openapi.json 现取**，不在脚本里硬编码。规范里新增了端点
#     而这里没覆盖时，末尾的「未覆盖端点」一节会列出来——脚本因此不会悄悄过时。
#   * 每条断言只检查**状态码**与**少量关键字段**，不比对整个响应体：这是冒烟，
#     不是快照测试，机器之间的实际数据本来就不同。
#   * 能力不可用（`capability_unavailable`）与未实现一样是**可接受**的结果，
#     只要状态码和错误码自洽——这正是 design.md §1「优雅降级」要验证的东西。

set -uo pipefail

# macOS 自带的是 bash 3.2（Apple 停在 GPLv2 那一版），因此全脚本必须兼容 3.2：
#
#   * 不能用 mapfile / declare -A / ${var^^}；
#   * `set -u` 下展开可能为空的数组要写成 ${arr[@]+"${arr[@]}"}，否则报
#     "unbound variable"；
#   * **变量引用紧跟中文时必须写成 ${var}**。bash 3.2 按字节找变量名的结尾，
#     不认多字节字符，`"$code（期望 $want）"` 会被解析成一个名叫
#     `code\xef\xbc\x88期望` 的变量。这个坑只在带中文的脚本里出现，
#     且报错信息里的变量名是乱码，极难对症。

BASE="${1:-http://127.0.0.1:9700}"
TOKEN="${STRIX_TOKEN:-}"

PASS=0
FAIL=0
declare -a FAILURES=()
# 已断言过的端点，形如 "GET /api/v1/health"
declare -a COVERED=()

c_green=$'\033[32m'; c_red=$'\033[31m'; c_dim=$'\033[2m'; c_off=$'\033[0m'
[[ -t 1 ]] || { c_green=''; c_red=''; c_dim=''; c_off=''; }

# ---------------------------------------------------------------------------
# 断言原语
# ---------------------------------------------------------------------------

# check <期望状态码(可用 | 分隔多个)> <方法> <路径> [curl 额外参数...]
#   把响应体留在 $BODY 里供后续 jq 断言。
check() {
  local want="$1" method="$2" path="$3"; shift 3
  local url="$BASE$path"
  local tmp; tmp="$(mktemp)"
  local code
  if [[ -n "$TOKEN" ]]; then
    code="$(curl -sS -o "$tmp" -w '%{http_code}' -X "$method" \
      -H "Authorization: Bearer $TOKEN" "$@" "$url" 2>/dev/null)"
  else
    code="$(curl -sS -o "$tmp" -w '%{http_code}' -X "$method" "$@" "$url" 2>/dev/null)"
  fi
  BODY="$(cat "$tmp")"; rm -f "$tmp"

  COVERED+=("$method ${path%%\?*}")

  if [[ "|$want|" == *"|$code|"* ]]; then
    PASS=$((PASS+1))
    printf '  %s✓%s %-6s %-46s %s\n' "$c_green" "$c_off" "$method" "${path:0:46}" "$code"
    return 0
  fi
  FAIL=$((FAIL+1))
  FAILURES+=("$method $path → ${code}（期望 ${want}）")
  printf '  %s✗%s %-6s %-46s %s%s（期望 %s）%s\n' \
    "$c_red" "$c_off" "$method" "${path:0:46}" "$c_red" "$code" "$want" "$c_off"
  printf '      %s%s%s\n' "$c_dim" "$(echo "$BODY" | head -c 200)" "$c_off"
  return 1
}

# expect_json <jq 表达式> <说明>
#   对上一次 check 的响应体求值，结果必须是 true。
expect_json() {
  local expr="$1" what="$2"
  if echo "$BODY" | jq -e "$expr" >/dev/null 2>&1; then
    PASS=$((PASS+1))
    printf '      %s✓%s %s\n' "$c_green" "$c_off" "$what"
  else
    FAIL=$((FAIL+1))
    FAILURES+=("字段断言失败：${what}（${expr}）")
    printf '      %s✗%s %s  %s%s%s\n' "$c_red" "$c_off" "$what" "$c_dim" "$expr" "$c_off"
  fi
}

section() { printf '\n%s── %s%s\n' "$c_dim" "$1" "$c_off"; }

need() { command -v "$1" >/dev/null 2>&1 || { echo "缺少依赖：$1" >&2; exit 2; }; }
need curl
need jq

# ---------------------------------------------------------------------------
printf '目标：%s\n' "$BASE"
if [[ -n "$TOKEN" ]]; then
  printf '模式：%s已认证%s（跑全部端点）\n' "$c_green" "$c_off"
else
  printf '模式：%s未认证%s（只跑公开端点，并断言受保护端点返回 401）\n' "$c_dim" "$c_off"
  printf '      要跑全部端点：STRIX_TOKEN=<token> %s\n' "$0"
fi

# ---------------------------------------------------------------------------
section "公开端点（无需认证）"

check 200 GET /api/v1/health
expect_json '.status == "ok"' 'status 为 ok'

check 200 GET /api/v1/capabilities
expect_json 'has("system")' '含 system 层'
expect_json '.system | has("systemd") and has("journal") and has("helper")' 'system 层字段齐全'
if [[ -z "$TOKEN" ]]; then
  expect_json '(.user // null) == null' '未认证时 user 层为 null（design.md §6）'
fi

# 文档端点只在 debug 构建 / apidoc feature 下存在
check '200|404' GET /api/v1/openapi.json
SPEC="$BODY"
check '200|404' GET /api/docs

# ---------------------------------------------------------------------------
section "认证协议"

# 用一个几乎不可能存在的用户名，只验证协议形状，不真的去撞任何账户。
#   200 → PAM 起了对话（PAM 通常不泄露「用户不存在」，照样问密码）
#   401 → 直接拒绝
#   501 → helper 没装（capability_unavailable，是**永久**缺失，故不是 503）
check '200|401|501' POST /api/v1/auth/start \
  -H 'Content-Type: application/json' \
  -d '{"username":"strixmaid-smoke-nonexistent"}'
expect_json 'has("session") or has("code")' '要么给出会话与 prompts，要么是结构化错误'

# 参数缺失必须是 400/422，不能是 500
check '400|422' POST /api/v1/auth/start -H 'Content-Type: application/json' -d '{}'

if [[ -z "$TOKEN" ]]; then
  check 401 GET /api/v1/auth/session
fi

# ---------------------------------------------------------------------------
section "受保护端点"

# 未认证时期望 401；已认证时期望 200（或能力不可用的 503）
# 501 = 该能力在本机不存在（例如没有 systemd 的机器上问服务），
# 这是 design.md §1「优雅降级」的正常结果，不算失败。
WANT='401'
[[ -n "$TOKEN" ]] && WANT='200|501'

check "$WANT" GET /api/v1/system/info
if [[ -n "$TOKEN" ]]; then
  expect_json '.hostname | length > 0' '主机名非空'
  expect_json '.os.id | length > 0' 'OS 标识非空'
  expect_json '.memory.total_bytes > 0' '物理内存 > 0'
  expect_json '.memory.available_bytes <= .memory.total_bytes' '可用内存不超过总量'
  expect_json '[.filesystems[].mount_point] | index("/") != null' '文件系统列表含根'
  expect_json '.boot_ts > 0 and .boot_ts <= .ts' '开机时刻早于当前时刻'
fi

check "$WANT" GET /api/v1/system/health
[[ -n "$TOKEN" ]] && expect_json '.status | test("ok|warning|critical")' '健康状态是已知取值'

check "$WANT" GET /api/v1/system/time
[[ -n "$TOKEN" ]] && expect_json '.timezone | length > 0' '时区非空'
[[ -n "$TOKEN" ]] && expect_json '.utc_offset_secs >= -50400 and .utc_offset_secs <= 50400' 'UTC 偏移在 ±14 小时内'

check "$WANT" GET '/api/v1/processes?limit=5'
[[ -n "$TOKEN" ]] && expect_json 'length > 0' '进程列表非空'
[[ -n "$TOKEN" ]] && expect_json 'all(.[]; .pid > 0)' 'pid 全部为正'

check "$WANT" GET "/api/v1/processes/$$"
check "$WANT" GET '/api/v1/services?limit=5'
[[ -n "$TOKEN" ]] && expect_json 'all(.[]; .unit_type | length > 0)' '每个 unit 都有类型后缀'

check "$WANT" GET '/api/v1/logs?limit=5'
[[ -n "$TOKEN" ]] && expect_json '.entries | length >= 0' '有 entries 数组'
[[ -n "$TOKEN" ]] && expect_json 'all(.entries[]; .cursor | length > 0)' '每条都有游标'

check "$WANT" GET /api/v1/logs/boots
[[ -n "$TOKEN" ]] && expect_json 'length >= 1' '至少有当前这次启动'

check "$WANT" GET /api/v1/metrics/series
[[ -n "$TOKEN" ]] && expect_json 'length > 0' '已登记 series'
check "$WANT" GET /api/v1/metrics/current

# ---------------------------------------------------------------------------
section "错误契约"

check 404 GET /api/v1/no-such-endpoint
check "$([[ -n "$TOKEN" ]] && echo '400|404' || echo 401)" GET '/api/v1/processes/0'
check "$([[ -n "$TOKEN" ]] && echo '400' || echo 401)" GET '/api/v1/logs?limit=999999'
if [[ -n "$TOKEN" ]]; then
  expect_json '.code == "invalid_request"' '超限的 limit 报 invalid_request'
fi

# WS 升级必须在握手前就拒绝未认证的连接
check 401 GET /ws -H 'Connection: Upgrade' -H 'Upgrade: websocket' \
  -H 'Sec-WebSocket-Version: 13' -H 'Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ=='

# ---------------------------------------------------------------------------
section "规范覆盖率"

if [[ -n "$SPEC" ]] && echo "$SPEC" | jq -e '.paths' >/dev/null 2>&1; then
  # 规范里声明的所有 "METHOD /path"（bash 3.2 没有 mapfile，用 while-read）
  SPEC_OPS=()
  while IFS= read -r line; do
    [[ -n "$line" ]] && SPEC_OPS+=("$line")
  done < <(
    echo "$SPEC" | jq -r '.paths | to_entries[] | .key as $p | .value
      | to_entries[] | select(.key | test("^(get|post|put|delete|patch)$"))
      | "\(.key | ascii_upcase) \($p)"' | sort -u
  )
  # 本脚本覆盖到的（把 /api/v1/processes/123 归一成规范里的 {pid} 形式做不到完全精确，
  # 因此这里只做「前缀能对上」的宽松匹配，宁可漏报也不误报）
  UNCOVERED=()
  for op in ${SPEC_OPS[@]+"${SPEC_OPS[@]}"}; do
    method="${op%% *}"; path="${op#* }"
    # 把 {param} 换成通配再匹配已覆盖列表
    pattern="^${method} $(echo "$path" | sed 's/{[^}]*}/[^\/]*/g')$"
    hit=0
    for cov in ${COVERED[@]+"${COVERED[@]}"}; do
      [[ "$cov" =~ $pattern ]] && { hit=1; break; }
    done
    [[ $hit -eq 0 ]] && UNCOVERED+=("$op")
  done

  printf '  规范声明 %d 个操作，本脚本覆盖 %d 个\n' \
    "${#SPEC_OPS[@]}" "$(( ${#SPEC_OPS[@]} - ${#UNCOVERED[@]} ))"
  if [[ ${#UNCOVERED[@]} -gt 0 ]]; then
    printf '  %s未覆盖：%s\n' "$c_dim" "$c_off"
    printf '    %s\n' ${UNCOVERED[@]+"${UNCOVERED[@]}"}
    printf '  %s（写操作与需要提权的端点刻意不测——冒烟脚本不应改变系统状态）%s\n' "$c_dim" "$c_off"
  fi
else
  printf '  %s拿不到 openapi.json（release 构建会 gate 掉它），跳过覆盖率检查%s\n' "$c_dim" "$c_off"
fi

# ---------------------------------------------------------------------------
printf '\n'
if [[ $FAIL -eq 0 ]]; then
  printf '%s全部通过%s：%d 项断言\n' "$c_green" "$c_off" "$PASS"
  exit 0
fi
printf '%s失败 %d 项%s（通过 %d 项）：\n' "$c_red" "$FAIL" "$c_off" "$PASS"
printf '  - %s\n' ${FAILURES[@]+"${FAILURES[@]}"}
exit 1
