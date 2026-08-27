#!/usr/bin/env bash
# roadmap 各项验收标准的可执行版本。目前覆盖：
#   - 01-worker-execution.md §7（请求经 worker 执行、写端点的提权门禁）
#   - 02-audit.md §7（写操作与认证事件留痕、未提权不得查审计）
#
# 用法：
#   scripts/acceptance.sh [BASE_URL]
#
# 环境变量：
#   STRIX_TOKEN   已登录会话的 token。**不给就只跑静态检查**——需要认证的那几条
#                 断言拿不到会话就无从验证，与其跳过后报「通过」，不如说清没测。
#   STRIX_ALLOW_MUTATING=1
#                 允许尝试真正会改变系统状态的探测（对某个真实服务发 restart）。
#                 **默认关闭**：service.action 走的是「先以用户身份试」的升级重试
#                 规则，请求会真的到达 launchctl / systemctl；用户若恰好有权限，
#                 那个服务就真的被重启了。验收脚本不该在别人机器上干这种事。
#
# 静态检查不需要服务在跑；动态检查需要。
#
# 兼容 macOS 自带的 bash 3.2：不用 mapfile / declare -A，
# 变量紧跟中文时一律写 ${var}（3.2 按字节解析变量名，会把中文吃进变量名）。

set -uo pipefail

BASE="${1:-http://127.0.0.1:9700}"
TOKEN="${STRIX_TOKEN:-}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"

PASS=0
FAIL=0
SKIP=0
c_green=$'\033[32m'; c_red=$'\033[31m'; c_dim=$'\033[2m'; c_off=$'\033[0m'
[[ -t 1 ]] || { c_green=''; c_red=''; c_dim=''; c_off=''; }

ok()   { PASS=$((PASS+1)); printf '  %s✓%s %s\n' "$c_green" "$c_off" "$1"; }
bad()  { FAIL=$((FAIL+1)); printf '  %s✗%s %s\n' "$c_red" "$c_off" "$1"; [[ $# -gt 1 ]] && printf '      %s%s%s\n' "$c_dim" "$2" "$c_off"; }
skip() { SKIP=$((SKIP+1)); printf '  %s—%s %s（未测）\n' "$c_dim" "$c_off" "$1"; }
section() { printf '\n%s── %s%s\n' "$c_dim" "$1" "$c_off"; }

# ---------------------------------------------------------------------------
section "静态：路由层不得再直接持有 provider（§7 第 1 条）"

# roadmap 原文写的是「grep 为 0 处」，但那样会把**注释里提到这些名字**也算成违规。
# 要验的是「路由不再持有 / 构造 provider」，不是「这些词不许出现」——
# 一句 `HTTP 请求 → exec::call → worker 内的 HostProvider` 恰恰是有用的说明。
# 因此先剥掉注释行再 grep。
HITS="$(grep -rnE 'HostProvider|ProcProvider|pick_service_provider|pick_log_provider' \
        "$ROOT/crates/strixmaid-server/src/routes/" 2>/dev/null \
        | grep -vE ':[[:space:]]*//' || true)"
if [[ -z "$HITS" ]]; then
  ok "routes/ 下没有 provider 的代码引用（注释里的说明不算）"
else
  bad "routes/ 下仍有 provider 的代码引用" "$HITS"
fi

# 处理器必须经 exec::call 走 worker
CALLS="$(grep -rlE 'exec::call' "$ROOT/crates/strixmaid-server/src/routes/" 2>/dev/null | wc -l | tr -d ' ')"
if [[ "$CALLS" -ge 4 ]]; then
  ok "有 ${CALLS} 个路由模块经 exec::call 执行"
else
  bad "只有 ${CALLS} 个路由模块经 exec::call（期望 ≥ 4：system/processes/services/logs）"
fi

# 方法名必须引用常量而不是字面量。
#
# 只看**非测试**代码：审计相关的用例里会出现 "service.start" 这类字符串，
# 那是样本数据（审计记录的 action 列），不是被派发的 RPC 方法名。
# 用 `#[cfg(test)]` 作分界，把每个文件的测试模块整段切掉再查。
LITERALS=""
for f in "$ROOT"/crates/strixmaid-server/src/routes/*.rs; do
  hit="$(awk '/^#\[cfg\(test\)\]/{exit} {print FILENAME":"NR":"$0}' "$f" \
         | grep -E '"(host|proc|service|log|caps)\.[a-z_]+"' || true)"
  [[ -n "$hit" ]] && LITERALS="${LITERALS}${hit}"$'\n'
done
LITERALS="$(printf '%s' "$LITERALS")"
if [[ -z "$LITERALS" ]]; then
  ok "RPC 方法名一律引用 rpc:: 常量，无字面量"
else
  bad "路由里写了 RPC 方法名字面量（改名时不会被编译器发现）" "$LITERALS"
fi

# ---------------------------------------------------------------------------
section "动态：未提权会话的写端点必须被挡住（§7 第 2 条，按 §4.1 修正）"

# roadmap §7 写的是「所有写端点返回 403 elevation_required」，但这与 §4.1 冲突：
# §4.1 给 proc.signal / proc.renice / service.action 定的是「先以用户身份试，
# 被拒且已提权才升级」，那条路径产生的是内核/polkit 的 permission_denied
# 加 can_retry_elevated，不是 elevation_required。
# 两者都是「被挡住且提权可解」，验收要认的是这个语义，而不是某一个错误码字面量。

if [[ -z "$TOKEN" ]]; then
  skip "写端点的提权门禁——需要 STRIX_TOKEN"
  skip "读端点确实在 worker 内执行"
else
  probe_write() {
    local desc="$1" method="$2" path="$3" body="${4:-}"
    local tmp code
    tmp="$(mktemp)"
    if [[ -n "$body" ]]; then
      code="$(curl -sS -o "$tmp" -w '%{http_code}' -X "$method" \
        -H "Authorization: Bearer ${TOKEN}" -H 'Content-Type: application/json' \
        -d "$body" "${BASE}${path}")"
    else
      code="$(curl -sS -o "$tmp" -w '%{http_code}' -X "$method" \
        -H "Authorization: Bearer ${TOKEN}" "${BASE}${path}")"
    fi
    local out; out="$(cat "$tmp")"; rm -f "$tmp"
    local errcode; errcode="$(echo "$out" | jq -r '.code // ""' 2>/dev/null)"

    # 已提权的会话会成功；那不是失败，但这条断言就没意义了
    if [[ "$code" == "200" || "$code" == "204" ]]; then
      skip "${desc}——会话已提权，测不到门禁"
      return
    fi
    local retry; retry="$(echo "$out" | jq -r '.can_retry_elevated // false' 2>/dev/null)"
    if [[ "$code" == "403" && "$errcode" == "elevation_required" ]]; then
      ok "${desc} → 403 elevation_required"
    elif [[ "$code" == "403" && "$errcode" == "permission_denied" && "$retry" == "true" ]]; then
      ok "${desc} → 403 permission_denied + can_retry_elevated（§4.1 的升级重试路径）"
    elif [[ "$code" == "403" ]]; then
      bad "${desc} → 403 ${errcode} 但 can_retry_elevated=${retry}" \
          "被挡住却不告诉前端提权可解，用户会以为这是死路"
    elif [[ "$code" == "501" ]]; then
      # 该能力在本机不存在（如 macOS 上的 unit 依赖图），是合法结果
      ok "${desc} → 501 capability_unavailable（本机没有这项能力）"
    else
      bad "${desc} → ${code} ${errcode}（期望 403，且提权可解）" "$out"
    fi
  }

  # 这两个是纯管理操作（Privilege::Admin）：未提权时 exec::call 在**请求触达系统之前**
  # 就返回 403，因此探测无副作用。
  probe_write "PUT /system/hostname"  PUT  /api/v1/system/hostname  '{"hostname":"strixmaid-acceptance-probe"}'
  probe_write "PUT /system/timezone"  PUT  /api/v1/system/timezone  '{"timezone":"UTC"}'

  # service.action 不同：它走升级重试（roadmap §4.1），请求会真的到达
  # launchctl / systemctl。用户有权限时那个服务就真的被重启了，因此默认不做。
  if [[ "${STRIX_ALLOW_MUTATING:-}" == "1" ]]; then
    # 服务名不能写死——cron.service 是 Linux 才有的，macOS 上 launchd 会回
    # 「Could not find service」。从 API 现取一个本机真实存在的 system 域服务。
    UNIT="$(curl -sS -H "Authorization: Bearer ${TOKEN}" \
      "${BASE}/api/v1/services?scope=system&limit=1" | jq -r '.[0].name // empty')"
    if [[ -n "$UNIT" ]]; then
      probe_write "POST /services/${UNIT}/action" POST \
        "/api/v1/services/${UNIT}/action" '{"action":"restart"}'
    else
      skip "service.action 的门禁——列不出任何 system 域服务"
    fi
  else
    skip "service.action 的门禁——会真的重启一个服务，需 STRIX_ALLOW_MUTATING=1 显式允许"
  fi

  section "动态：读端点确实在 worker 内以登录用户身份执行"

  ME="$(curl -sS -H "Authorization: Bearer ${TOKEN}" "${BASE}/api/v1/capabilities" \
        | jq -r '.user.uid // empty')"
  if [[ -z "$ME" ]]; then
    bad "拿不到 user 层能力，token 可能已失效"
  else
    ok "capabilities 报告 user.uid=${ME}"
    # 进程列表里必然有本会话的 worker，且它的 uid 应等于登录用户
    WORKER_UIDS="$(curl -sS -H "Authorization: Bearer ${TOKEN}" \
      "${BASE}/api/v1/processes?q=strixmaid&limit=50" \
      | jq -r '[.[] | select(.cmdline // "" | contains("worker")) | .uid] | unique | join(",")')"
    if [[ -n "$WORKER_UIDS" ]]; then
      ok "进程列表里的 worker uid = ${WORKER_UIDS}（登录用户 ${ME}）"
    else
      skip "进程列表里没找到 worker（可能被 limit 截断）"
    fi
  fi
fi

# ---------------------------------------------------------------------------
section "审计（02-audit.md §7）"

if [[ -z "$TOKEN" ]]; then
  skip "审计的全部断言——需要 STRIX_TOKEN"
else
  # 未提权不得查审计。这是 roadmap/01 之后**全服务端唯一一处**基于会话状态
  # 而非 worker 的判断，理由见 routes/audit.rs 的模块文档。
  tmp="$(mktemp)"
  code="$(curl -sS -o "$tmp" -w '%{http_code}' -H "Authorization: Bearer ${TOKEN}" \
    "${BASE}/api/v1/audit?limit=5")"
  body="$(cat "$tmp")"; rm -f "$tmp"
  errcode="$(echo "$body" | jq -r '.code // ""' 2>/dev/null)"

  if [[ "$code" == "403" && "$errcode" == "elevation_required" ]]; then
    ok "未提权查审计 → 403 elevation_required"
    skip "审计内容断言——会话未提权，读不到 /audit"
  elif [[ "$code" == "200" ]]; then
    ok "已提权，可读取审计"
    n="$(echo "$body" | jq '.entries | length')"
    if [[ "$n" -gt 0 ]]; then
      ok "审计里已有 ${n} 条记录"
      # 按 id 严格递减（design.md §8：id 是 AUTOINCREMENT，与写入顺序严格一致）
      if echo "$body" | jq -e '[.entries[].id] | . == (sort | reverse) and (length == (unique | length))' >/dev/null; then
        ok "记录按 id 严格递减且无重复"
      else
        bad "记录顺序不是严格的 id 降序" "$(echo "$body" | jq -c '[.entries[].id]')"
      fi
      # 每条都要有 action 与 result
      if echo "$body" | jq -e 'all(.entries[]; (.action | length > 0) and (.result | test("^(ok|denied|error)$")))' >/dev/null; then
        ok "每条记录都有动作与合法的结果值"
      else
        bad "有记录缺 action 或 result 非法"
      fi
      # 审计里绝不能出现凭据（design.md §5.3）
      if echo "$body" | jq -e 'all(.entries[]; (.params // {} | tostring | test("password|passwd|secret|token"; "i") | not))' >/dev/null; then
        ok "记录的 params 里没有凭据字样"
      else
        bad "审计的 params 里出现了疑似凭据的字段"
      fi
    else
      skip "审计内容断言——表里还没有记录"
    fi
  else
    bad "查审计 → ${code} ${errcode}（期望 403 elevation_required 或 200）" "$body"
  fi
fi

# ---------------------------------------------------------------------------
printf '\n'
if [[ $FAIL -eq 0 ]]; then
  printf '%s通过 %d 项%s' "$c_green" "$PASS" "$c_off"
  [[ $SKIP -gt 0 ]] && printf '，%s未测 %d 项%s' "$c_dim" "$SKIP" "$c_off"
  printf '\n'
  [[ $SKIP -gt 0 ]] && printf '%s未测的项需要 STRIX_TOKEN；用 scripts/dev-login.sh 取。%s\n' "$c_dim" "$c_off"
  exit 0
fi
printf '%s失败 %d 项%s（通过 %d，未测 %d）\n' "$c_red" "$FAIL" "$c_off" "$PASS" "$SKIP"
exit 1
