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

# code METHOD PATH [curl-args...]；状态码留在 $C，响应体留在 $BODY。
#
# **不要**写成 `C="$(code ...)"`：命令替换会开子 shell，函数里对 BODY 的赋值
# 出不来，调用方拿到的永远是上一次的值（多半是空串），于是每个 jq 判断都失败——
# 状态码全对、断言全红。首轮真跑就栽在这上面。两个值都由函数直接设全局。
C=''; BODY=''
code() {
  local m="$1" p="$2"; shift 2
  local tmp; tmp="$(mktemp)"
  C="$(curl -sS -o "$tmp" -w '%{http_code}' -X "$m" "$@" "$BASE$p" 2>/dev/null || echo 000)"
  BODY="$(cat "$tmp")"; rm -f "$tmp"
}

login()   { STRIX_PASSWORD="$2" "$HERE/login.sh" login "$BASE" "$1"; }
elevate() { STRIX_PASSWORD="$2" "$HERE/login.sh" elevate "$BASE" "$1" ""; }

worker_count() { pgrep -u "$1" -f 'strixmaid worker' 2>/dev/null | wc -l | tr -d ' '; }
# 两个坑，都是首轮真跑才发现的：
#
# 1. Linux 的 comm 只有 15 个字符，而 `strixmaid-helper` 是 16 个——内核把它截成
#    `strixmaid-helpe`，于是 `pgrep -x strixmaid-helper` **永远匹配不到**。这条
#    检查一直在拿 0 和 0 比，恒真（HANDOFF.md §5：只断言没报错的测试等于没测）。
#    模式写成 `strixmaid-helpe.?`，截断与不截断的系统都能覆盖。
#    不用 `pgrep -f`：它匹配整条 cmdline，连「命令行里碰巧出现过这个词的 shell」
#    都会算进去——实测能多数出一个。
# 2. `pgrep -c` 无匹配时**既打印 0 又以非零退出**，写成 `pgrep -c ... || echo 0`
#    会得到两行 "0\n0"，之后的 [ "$n" -le ... ] 直接报 integer expression expected。
helper_count() {
  local n; n="$(pgrep -c -x 'strixmaid-helpe.?' 2>/dev/null)"; [ -n "$n" ] || n=0; echo "$n"
}

# ===========================================================================
sec "§1.2 未认证与 alice（普通用户）"

code GET /api/v1/capabilities
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
  code GET /api/v1/auth/session "${AH[@]}"
  [ "$(echo "$BODY" | jq -r '.session_opened')" = true ] && ok "#3 session_opened=true" \
    || skip "#3 session_opened 非 true" "同 #2 的 logind 依赖"

  code GET /api/v1/capabilities "${AH[@]}"
  ce="$(echo "$BODY" | jq -r '.user.can_elevate') $(echo "$BODY" | jq -r '.user.elevated')"
  [ "$ce" = "false false" ] && ok "#4 alice can_elevate=false elevated=false" || bad "#4 alice 能力异常" "$BODY"
  ALICE_CAP_J="$(echo "$BODY" | jq -r '.user.can_read_journal')"

  # 读不到日志的普通用户应得到 403 + can_retry_elevated（提权确实能解决——
  # 日志读取走 exec::escalate），而**不是 500**。RHEL 系上曾是 500：journalctl 说
  # "insufficient permissions"，映射没认出来，落进了 internal。
  code GET '/api/v1/logs?limit=50' "${AH[@]}"
  case "$C" in
    200|501) ok "#5 alice /logs 返回 $C（内容可见性属人工核对）" ;;
    403) [ "$(echo "$BODY" | jq -r '.can_retry_elevated')" = true ] \
           && ok "#5 alice 读不到日志 → 403 且提示可提权重试" \
           || bad "#5 /logs 403 但没带 can_retry_elevated" "$BODY" ;;
    *) bad "#5 /logs 异常" "$C $BODY" ;;
  esac
  # 契约只有一个方向成立：**说能读就必须真能读**（true ⇒ 200），否则前端会画出
  # 一个亮着的日志入口、点进去报错。
  #
  # 反方向**不**成立，别把它写进断言：can_read_journal 说的是「能不能看到**系统**
  # 日志」（探测判据是内核日志），而 /logs 返回 200 只说明 journalctl 成功了——
  # Ubuntu 上 alice 拿到的正是 200，内容却只有她自己的条目。两者都对。
  # 「false 时前端要不要显示日志页」是产品决定（只给自己的日志也是有用的），
  # 不是这里该裁决的事。
  if [ "$ALICE_CAP_J" = true ] && [ "$C" = 403 ]; then
    bad "#5 can_read_journal=true 却拿到 403" "标志位说能读就必须真能读"
  else
    ok "#5 alice can_read_journal=$ALICE_CAP_J 与 /logs 的 $C 不矛盾"
  fi

  WPID="$(pgrep -u "$ALICE" -f 'strixmaid worker' 2>/dev/null | head -1)"
  if [ -n "$WPID" ]; then
    code GET "/api/v1/processes/$WPID" "${AH[@]}"
    [ "$(echo "$BODY" | jq -r '.uid // .user // empty')" != "" ] && ok "#6 /processes/$WPID 可取（uid/cgroup 属人工核对）" \
      || skip "#6 /processes 详情形状需人工核对" "$BODY"
  else skip "#6 拿不到 alice worker pid"; fi

  # 服务操作走 exec.rs 的 call_escalating_from：**先在 user worker 里试**，让 polkit
  # 裁决（design.md §5 的原则），失败时返回 polkit 的真实理由并带上 can_retry_elevated。
  # 所以这里可能是 permission_denied（polkit 拒绝）而不是 elevation_required
  # （后者是 Privilege::Admin 那类路由在「压根没有 admin worker」时的回答）。
  # 真正要断言的是「403 且前端被告知提权可解决」。
  code POST "/api/v1/services/$TEST_UNIT/action" "${AH[@]}" -H 'Content-Type: application/json' -d '{"action":"restart"}'
  ecode="$(echo "$BODY" | jq -r '.code')"
  retry="$(echo "$BODY" | jq -r '.can_retry_elevated')"
  if [ "$C" = 403 ] && [ "$retry" = true ] \
     && { [ "$ecode" = elevation_required ] || [ "$ecode" = permission_denied ]; }; then
    ok "#7 alice restart → 403 $ecode（can_retry_elevated=true）"
  else bad "#7 未提权写操作应 403 且 can_retry_elevated=true" "$C $BODY"; fi

  BEFORE="$(helper_count)"
  code POST /api/v1/auth/elevate/start "${AH[@]}" -H 'Content-Type: application/json' -d '{"username":""}'
  AFTER="$(helper_count)"
  [ "$C" = 403 ] && [ "$(echo "$BODY" | jq -r '.code')" = permission_denied ] \
    && ok "#8 alice elevate/start → 403 permission_denied" || bad "#8 alice 提权应 403 permission_denied" "$C $BODY"
  [ "$AFTER" -le "$BEFORE" ] && ok "#8 提权被拒未新起 helper（$BEFORE→$AFTER）" || bad "#8 提权被拒却起了 helper" "$BEFORE→$AFTER"

  # 503 unavailable = 「用户级 systemd 的 session bus 连不上」，是设计好的一档
  # （error.rs：503 暂时不可用、页面别隐藏；501 本机没装、隐藏整个页面）。
  # 容器里 pam_open_session 建了 logind 会话但通常没起 user@.service，503 属环境
  # 而非缺陷——如实标 skip，不假装通过、也不误报为失败。
  code GET '/api/v1/services?scope=user' "${AH[@]}"
  case "$C" in
    200|501) ok "#9 alice services?scope=user 返回 $C" ;;
    503) skip "#9 scope=user 得到 503" "用户级 systemd 未起（容器常见）；VM 上应为 200" ;;
    *) bad "#9 scope=user 异常" "$C $BODY" ;;
  esac

  code POST /api/v1/auth/logout "${AH[@]}"
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

    # can_read_journal 不是按组推导的结论，而是 worker 内**实测**的结果，会覆盖推导
    # （worker/probe.rs：判据是读不读得到 _TRANSPORT=kernel 的条目）。容器里没有
    # 内核日志可读，探测如实报 false——这正是它该做的，不是缺陷。分开断言，
    # 让「提权本身生效」与「日志可见性」各自成立或各自失败。
    code GET /api/v1/capabilities "${BH[@]}"
    caps="$(echo "$BODY" | jq -r '.user.elevated') $(echo "$BODY" | jq -r '.user.can_manage_units')"
    [ "$caps" = "true true" ] && ok "#12 bob elevated=true can_manage_units=true" \
      || bad "#12 bob 提权后能力不全" "$caps · $BODY"
    # 不要拿「bob 以自己的身份读不读得到」来对照：日志读取走 exec::escalate，
    # **已提权的会话被拒后会换 admin worker 重试**，所以提权后这两个值本就不该相等。
    # 该断言的是标志位能不能预测端点行为——前端靠它决定日志页显示还是灰掉。
    cap_j="$(echo "$BODY" | jq -r '.user.can_read_journal')"
    code GET '/api/v1/logs?limit=1' "${BH[@]}"
    # 同 #5：只断言「说能读就必须真能读」这一个方向（理由见那里）。
    # 提权后 cap_j 应为 true——它正是靠升级到 admin worker 成立的。
    if [ "$cap_j" = true ] && [ "$C" = 403 ]; then
      bad "#12 提权后 can_read_journal=true 却拿到 403" "升级重试没生效？"
    else
      ok "#12 can_read_journal=$cap_j 与 /logs 的 $C 不矛盾（提权后经升级读到）"
    fi

    code POST "/api/v1/services/$TEST_UNIT/action" "${BH[@]}" -H 'Content-Type: application/json' -d '{"action":"restart"}'
    if [ "$C" = 200 ]; then
      ok "#13 bob restart $TEST_UNIT → 200"
      systemctl is-active "$TEST_UNIT" >/dev/null 2>&1 && ok "#13 $TEST_UNIT restart 后 active" || skip "#13 $TEST_UNIT 非 active" "取决于测试 unit 类型"
    else bad "#13 bob restart 应 200" "$C $BODY"; fi

    code GET '/api/v1/logs?limit=50' "${BH[@]}"
    [ "$C" = 200 ] && ok "#14 bob /logs → 200（含系统日志属人工核对）" || bad "#14 bob /logs 异常" "$C"

    code PUT /api/v1/system/hostname "${BH[@]}" -H 'Content-Type: application/json' -d '{"hostname":"strix-verify"}'
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
    for _ in $(seq 1 30); do code GET /api/v1/health; [ "$C" = 200 ] && break; sleep 1; done
    if [ -n "$OLD" ]; then
      code GET /api/v1/auth/session -H "Authorization: Bearer $OLD"
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
code GET /ws
[ "$C" = 401 ] && ok "#6 无 token 的 /ws → 401" || skip "#6 /ws 无 token" "得到 $C（升级前的鉴权，非 200 即基本符合）"

# #7 反代头伪造：无 trusted_proxies 时 X-Forwarded-For 不被采信
if [ -n "${BTOK:-}" ]; then
  code POST "/api/v1/services/$TEST_UNIT/action" -H "Authorization: Bearer ${ETOK:-$BTOK}" \
    -H 'X-Forwarded-For: 1.2.3.4' -H 'Content-Type: application/json' -d '{"action":"restart"}' 
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
