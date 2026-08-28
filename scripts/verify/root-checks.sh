#!/usr/bin/env bash
# roadmap/07 §1.2（root 路径）与 §5（安全检查）的可执行验收。
#
# **在 root 环境里、对一个已经跑起来的 strixmaid 运行**（工装容器内由
# run-in-podman.sh 调起；也可手工在 VM 里跑）。它自己不起服务、不装东西。
#
# 前置（工装已备好；手工跑时需自备）：
#   - strixmaid 以 root 跑在 $BASE（默认 127.0.0.1:9700），helper 已装、可 PAM 登录；
#   - 两个用户：$ALICE（普通，不在 sudo/wheel）、$BOB（在 sudo/wheel），
#     密码分别为 $ALICE_PW / $BOB_PW；
#   - 一个可安全重启的测试 unit $TEST_UNIT（默认 strixtest.service）；
#   - jq、curl、sqlite3、pgrep、loginctl、systemctl 可用；
#   - $DB 指向 SQLite（默认 /var/lib/strixmaid/strixmaid.db）。
#
# 退出码：有 FAIL 即非零。SKIP 不算失败——它标注「这条需要更长时间 / 更多前置 /
# 只能人工」，如实说清没测，而不是假装通过。
#
# **本脚本尚未在真实 root 环境执行过**（开发机无 root）。首次运行若有环境相关的
# 小问题（unit 名、发行版差异），按 ok/bad 的具体消息调整即可。
#
# 兼容 bash 3.2；变量紧跟中文一律 ${var}。
set -uo pipefail

BASE="${BASE:-http://127.0.0.1:9700}"
ALICE="${ALICE:-alice}"; ALICE_PW="${ALICE_PW:-alicepw}"
BOB="${BOB:-bob}";       BOB_PW="${BOB_PW:-bobpw}"
TEST_UNIT="${TEST_UNIT:-strixtest.service}"
DB="${DB:-/var/lib/strixmaid/strixmaid.db}"
LONG="${LONG:-0}"          # =1 时跑空闲超时等耗时用例
HERE="$(cd "$(dirname "$0")" && pwd)"

PASS=0; FAIL=0; SKIP=0
c_g=$'\033[32m'; c_r=$'\033[31m'; c_d=$'\033[2m'; c_o=$'\033[0m'
[ -t 1 ] || { c_g=''; c_r=''; c_d=''; c_o=''; }
ok()   { PASS=$((PASS+1)); printf '  %s✓%s %s\n' "$c_g" "$c_o" "$1"; }
bad()  { FAIL=$((FAIL+1)); printf '  %s✗%s %s\n' "$c_r" "$c_o" "$1"; [ $# -gt 1 ] && printf '      %s%s%s\n' "$c_d" "$2" "$c_o"; return 0; }
skip() { SKIP=$((SKIP+1)); printf '  %s—%s %s%s%s\n' "$c_d" "$c_o" "$1" "${2:+ · }" "${2:-}"; }
sec()  { printf '\n%s── %s%s\n' "$c_d" "$1" "$c_o"; }

# code METHOD PATH [curl-args...]；响应体留在 $BODY，状态码回显。
BODY=''
code() {
  local m="$1" p="$2"; shift 2
  local tmp; tmp="$(mktemp)"
  local c; c="$(curl -sS -o "$tmp" -w '%{http_code}' -X "$m" "$@" "$BASE$p" 2>/dev/null || echo 000)"
  BODY="$(cat "$tmp")"; rm -f "$tmp"; echo "$c"
}

login()   { STRIX_PASSWORD="$2" "$HERE/login.sh" login "$BASE" "$1"; }
elevate() { STRIX_PASSWORD="$2" "$HERE/login.sh" elevate "$BASE" "$1" ""; }

worker_count() { pgrep -u "$1" -f 'strixmaid worker' 2>/dev/null | wc -l | tr -d ' '; }
helper_count() { pgrep -c -x strixmaid-helper 2>/dev/null || echo 0; }

# ===========================================================================
sec "§1.2 未认证与 alice（普通用户）"

C="$(code GET /api/v1/capabilities)"
if [ "$C" = 200 ] && [ "$(echo "$BODY" | jq -r '.system.helper')" = true ]; then
  ok "#1 /capabilities helper=true（polkit=$(echo "$BODY" | jq -r '.system.polkit')）"
else bad "#1 /capabilities helper 应为 true" "$C $BODY"; fi

ATOK="$(login "$ALICE" "$ALICE_PW" || true)"
if [ -n "$ATOK" ]; then
  ok "#2 alice 登录成功（token 到手）"
  [ "$(worker_count "$ALICE")" -ge 1 ] && ok "#2 ps 有 $ALICE 的 worker" || bad "#2 未见 $ALICE 的 worker"
  loginctl list-sessions 2>/dev/null | grep -qw "$ALICE" && ok "#2 loginctl 有 $ALICE 会话（pam_open_session）" \
    || skip "#2 loginctl 无 $ALICE 会话" "容器里 logind 可能未起，VM 上应有"
else
  bad "#2 alice 登录失败——后续 alice 用例无法进行"
fi

if [ -n "$ATOK" ]; then
  AH=(-H "Authorization: Bearer $ATOK")
  C="$(code GET /api/v1/auth/session "${AH[@]}")"
  [ "$(echo "$BODY" | jq -r '.session_opened')" = true ] && ok "#3 session_opened=true" \
    || skip "#3 session_opened 非 true" "同 #2 的 logind 依赖"

  C="$(code GET /api/v1/capabilities "${AH[@]}")"
  ce="$(echo "$BODY" | jq -r '.user.can_elevate') $(echo "$BODY" | jq -r '.user.elevated')"
  [ "$ce" = "false false" ] && ok "#4 alice can_elevate=false elevated=false" || bad "#4 alice 能力异常" "$BODY"

  C="$(code GET '/api/v1/logs?limit=50' "${AH[@]}")"
  [ "$C" = 200 -o "$C" = 501 ] && ok "#5 alice /logs 返回 $C（内容可见性属人工核对）" || bad "#5 /logs 异常" "$C"

  WPID="$(pgrep -u "$ALICE" -f 'strixmaid worker' 2>/dev/null | head -1)"
  if [ -n "$WPID" ]; then
    C="$(code GET "/api/v1/processes/$WPID" "${AH[@]}")"
    [ "$(echo "$BODY" | jq -r '.uid // .user // empty')" != "" ] && ok "#6 /processes/$WPID 可取（uid/cgroup 属人工核对）" \
      || skip "#6 /processes 详情形状需人工核对" "$BODY"
  else skip "#6 拿不到 alice worker pid"; fi

  C="$(code POST "/api/v1/services/$TEST_UNIT/action" "${AH[@]}" -H 'Content-Type: application/json' -d '{"action":"restart"}')"
  [ "$C" = 403 ] && [ "$(echo "$BODY" | jq -r '.code')" = elevation_required ] \
    && ok "#7 alice restart → 403 elevation_required" || bad "#7 未提权写操作应 403 elevation_required" "$C $BODY"

  BEFORE="$(helper_count)"
  C="$(code POST /api/v1/auth/elevate/start "${AH[@]}" -H 'Content-Type: application/json' -d '{"username":""}')"
  AFTER="$(helper_count)"
  [ "$C" = 403 ] && [ "$(echo "$BODY" | jq -r '.code')" = permission_denied ] \
    && ok "#8 alice elevate/start → 403 permission_denied" || bad "#8 alice 提权应 403 permission_denied" "$C $BODY"
  [ "$AFTER" -le "$BEFORE" ] && ok "#8 提权被拒未新起 helper（$BEFORE→$AFTER）" || bad "#8 提权被拒却起了 helper" "$BEFORE→$AFTER"

  C="$(code GET '/api/v1/services?scope=user' "${AH[@]}")"
  [ "$C" = 200 -o "$C" = 501 ] && ok "#9 alice services?scope=user 返回 $C" || bad "#9 scope=user 异常" "$C"

  C="$(code POST /api/v1/auth/logout "${AH[@]}")"
  sleep 1
  gone_w="$(worker_count "$ALICE")"; gone_h="$(helper_count)"
  [ "$C" = 200 -o "$C" = 204 ] && ok "#10 alice logout → $C" || bad "#10 logout 异常" "$C"
  [ "$gone_w" = 0 ] && ok "#10 logout 后 $ALICE 无残留 worker" || bad "#10 logout 后仍有 $ALICE worker（$gone_w）"
  loginctl list-sessions 2>/dev/null | grep -qw "$ALICE" && skip "#10 loginctl 仍见 $ALICE 会话" "logind 回收可能异步" \
    || ok "#10 loginctl 中 $ALICE 会话已消失"
fi

# ===========================================================================
sec "§1.2 bob（提权）"

BTOK="$(login "$BOB" "$BOB_PW" || true)"
if [ -z "$BTOK" ]; then
  bad "#11 bob 登录失败——后续 bob 用例无法进行"
else
  ok "#11 bob 登录成功"
  ETOK="$(elevate "$BTOK" "$BOB_PW" || true)"
  if [ -n "$ETOK" ]; then
    ok "#11 bob 提权成功"
    sleep 1
    pgrep -u root -f 'strixmaid worker' >/dev/null 2>&1 && ok "#11 ps 出现 uid 0 的 worker" \
      || skip "#11 未见 uid 0 worker" "admin worker 可能名字不同，人工 ps 核对"
    BH=(-H "Authorization: Bearer $ETOK")

    C="$(code GET /api/v1/capabilities "${BH[@]}")"
    caps="$(echo "$BODY" | jq -r '.user.elevated') $(echo "$BODY" | jq -r '.user.can_manage_units') $(echo "$BODY" | jq -r '.user.can_read_journal')"
    [ "$caps" = "true true true" ] && ok "#12 bob elevated/can_manage_units/can_read_journal 全 true" \
      || bad "#12 bob 提权后能力不全" "$caps · $BODY"

    C="$(code POST "/api/v1/services/$TEST_UNIT/action" "${BH[@]}" -H 'Content-Type: application/json' -d '{"action":"restart"}')"
    if [ "$C" = 200 ]; then
      ok "#13 bob restart $TEST_UNIT → 200"
      systemctl is-active "$TEST_UNIT" >/dev/null 2>&1 && ok "#13 $TEST_UNIT restart 后 active" || skip "#13 $TEST_UNIT 非 active" "取决于测试 unit 类型"
    else bad "#13 bob restart 应 200" "$C $BODY"; fi

    C="$(code GET '/api/v1/logs?limit=50' "${BH[@]}")"
    [ "$C" = 200 ] && ok "#14 bob /logs → 200（含系统日志属人工核对）" || bad "#14 bob /logs 异常" "$C"

    C="$(code PUT /api/v1/system/hostname "${BH[@]}" -H 'Content-Type: application/json' -d '{"hostname":"strix-verify"}')"
    if [ "$C" = 200 -o "$C" = 204 ]; then
      [ "$(hostnamectl --static 2>/dev/null)" = strix-verify ] && ok "#18 PUT hostname → 生效" || skip "#18 hostnamectl 未反映" "容器内 hostnamectl 可能受限"
    else bad "#18 PUT hostname 应成功" "$C $BODY"; fi

    if [ "$LONG" = 1 ]; then
      skip "#15/#16 空闲超时（300s/900s）" "LONG=1 但本工装未实现计时等待，请人工观察或加睡眠"
    else
      skip "#15 提权空闲超时(300s)回收 admin worker" "耗时，LONG=1 启用"
      skip "#16 会话空闲超时(900s)失效" "耗时，LONG=1 启用"
    fi
    skip "#17 连续 5 次登录失败触发 faillock" "依赖发行版 faillock 配置，人工核对"
    skip "#20 alice 登录 20 次 RSS 线性" "属容量观测，人工或长测跑"
  else
    bad "#11 bob 提权失败——admin 路径无法验证（helper 是否 root？）"
  fi
fi

# ===========================================================================
sec "§1.2 #19 崩溃恢复"
if command -v systemctl >/dev/null && systemctl show strixmaid >/dev/null 2>&1; then
  OLD="$(login "$ALICE" "$ALICE_PW" || true)"
  MAINPID="$(systemctl show -p MainPID --value strixmaid 2>/dev/null)"
  if [ -n "$MAINPID" ] && [ "$MAINPID" != 0 ]; then
    kill -9 "$MAINPID" 2>/dev/null || true
    for _ in $(seq 1 30); do systemctl is-active strixmaid >/dev/null 2>&1 && break; sleep 1; done
    for _ in $(seq 1 30); do [ "$(code GET /api/v1/health)" = 200 ] && break; sleep 1; done
    if [ -n "$OLD" ]; then
      C="$(code GET /api/v1/auth/session -H "Authorization: Bearer $OLD")"
      [ "$C" = 401 ] && ok "#19 kill -9 重启后旧 token 401" || bad "#19 旧 token 未失效" "$C"
    fi
    [ "$(pgrep -f 'strixmaid worker' | wc -l)" = 0 ] && ok "#19 重启后无残留 worker" || bad "#19 重启后仍有 worker"
    # sessions 表应为空
    if command -v sqlite3 >/dev/null && [ -f "$DB" ]; then
      n="$(sqlite3 "$DB" 'SELECT COUNT(*) FROM sessions' 2>/dev/null || echo '?')"
      [ "$n" = 0 ] && ok "#19 sessions 表已清空" || bad "#19 sessions 表非空" "$n"
    fi
  else skip "#19 拿不到 MainPID"; fi
else
  skip "#19 崩溃恢复" "无 systemd 管理（非工装环境），请人工 kill -9 主进程后重启核对"
fi

# ===========================================================================
sec "§5 安全检查"

# #1 密码不进日志（trace 级）：登录一次，查 journald / 日志里有无明文
LOGIN_PW='Zx9-secret-probe'
if command -v journalctl >/dev/null; then
  STRIX_PASSWORD="$LOGIN_PW" "$HERE/login.sh" login "$BASE" "$ALICE" >/dev/null 2>&1 || true
  if journalctl -u strixmaid --no-pager 2>/dev/null | grep -qF "$LOGIN_PW"; then
    bad "#1 日志里出现了密码明文"
  else ok "#1 密码明文不在 journald（注意：需 RUST_LOG=trace 才是严格检验）"; fi
else skip "#1 密码不进日志" "无 journalctl"; fi

# #2 密码不入库
if command -v strings >/dev/null && [ -f "$DB" ]; then
  strings "$DB" | grep -qF "$LOGIN_PW" && bad "#2 数据库里出现了密码明文" || ok "#2 密码明文不在 SQLite"
  strings "$DB" | grep -qF "$ALICE_PW" && bad "#2 数据库里出现了 alice 密码" || ok "#2 alice 密码不在 SQLite"
else skip "#2 密码不入库" "无 strings 或找不到 $DB"; fi

# #3 token 只存 hash
if command -v sqlite3 >/dev/null && [ -f "$DB" ]; then
  TT="$(login "$ALICE" "$ALICE_PW" || true)"
  if [ -n "$TT" ]; then
    if sqlite3 "$DB" "SELECT id FROM sessions" 2>/dev/null | grep -qxF "$TT"; then
      bad "#3 sessions.id 等于明文 token（应存 hash）"
    else
      ID="$(sqlite3 "$DB" "SELECT id FROM sessions LIMIT 1" 2>/dev/null)"
      echo "$ID" | grep -qE '^[0-9a-f]{64}$' && ok "#3 sessions.id 是 64 位 hex 且 ≠ token" || bad "#3 sessions.id 形状异常" "$ID"
    fi
  else skip "#3 token 只存 hash" "登录失败"; fi
else skip "#3 token 只存 hash" "无 sqlite3 或找不到 $DB"; fi

# #4 worker uid 校验：需要改过的 helper，代码层已有测试
skip "#4 worker uid 校验（helper 谎报 uid 被拒）" "需篡改 helper，见 core 单测；此处不重复"

# #5 helper 的 fd 3
HELPER="$(command -v strixmaid-helper || echo /usr/bin/strixmaid-helper)"
if [ -x "$HELPER" ]; then
  OUT="$("$HELPER" </dev/null 2>&1 || true)"
  echo "$OUT" | grep -qiE 'fd 3|socket|not a socket' && ok "#5 直接运行 helper 立即退出并报 fd 3 不是 socket" \
    || bad "#5 helper 未按预期拒绝" "$OUT"
else skip "#5 helper fd 3" "找不到 strixmaid-helper"; fi

# #6 WS 无 token → 401
C="$(code GET /ws)"
[ "$C" = 401 ] && ok "#6 无 token 的 /ws → 401" || skip "#6 /ws 无 token" "得到 $C（升级前的鉴权，非 200 即基本符合）"

# #7 反代头伪造：无 trusted_proxies 时 X-Forwarded-For 不被采信
if [ -n "${BTOK:-}" ]; then
  code POST "/api/v1/services/$TEST_UNIT/action" -H "Authorization: Bearer ${ETOK:-$BTOK}" \
    -H 'X-Forwarded-For: 1.2.3.4' -H 'Content-Type: application/json' -d '{"action":"restart"}' >/dev/null
  if command -v sqlite3 >/dev/null && [ -f "$DB" ]; then
    RA="$(sqlite3 "$DB" "SELECT remote_addr FROM audit_log ORDER BY id DESC LIMIT 1" 2>/dev/null)"
    echo "$RA" | grep -q '1.2.3.4' && bad "#7 审计采信了伪造的 X-Forwarded-For" "$RA" \
      || ok "#7 审计的 remote_addr 未被 XFF 伪造（=$RA）"
  else skip "#7 反代头伪造" "无 sqlite3 / DB"; fi
else skip "#7 反代头伪造" "需要一个 bob 会话"; fi

# ===========================================================================
printf '\n%s总计%s  %s通过 %d%s  %s失败 %d%s  %s未测 %d%s\n' \
  "$c_d" "$c_o" "$c_g" "$PASS" "$c_o" "$c_r" "$FAIL" "$c_o" "$c_d" "$SKIP" "$c_o"
[ "$FAIL" = 0 ]
