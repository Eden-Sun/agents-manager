#!/bin/bash
set -u
set -o pipefail

SELF="$(cd "$(dirname "$0")" && pwd)/${0##*/}"
WEB_BUILDER="${SELF%/*}/build-web-bundle.py"

DIR="${AGM_DIR:-$HOME/.config/agents-manager/supervisor/AGM}"
REPO="${AGM_REPO:-$HOME/project/agents-manager}"
DEPLOY="${AGM_DEPLOY_CHECKOUT:-$HOME/.cache/agents-manager/deploy-checkout}"
CLONE_STAGE=""
GH_REPO="${AGM_GH_REPO:-Eden-Sun/agents-manager}"
CI_CONTEXT="${AGM_CI_CONTEXT:-ubuntu-ci}"
CI_LOOKBACK="${AGM_CI_LOOKBACK:-30}"
CI_ROOT="${AGM_CI_ROOT:-${HOME}/.cache/agents-manager/ci}"
AGM="$DIR/bin/agm"
LOG="$DIR/daemon-update.log"
BUILT="$DIR/daemon-update.built"
REJECTED="$DIR/daemon-update.rejected"
NOW_FILE="$DIR/daemon-update.now.json"
OWNER="${AM_AGENT_NAME:-daemon-update-kick}"
GIT="${GIT_BIN:-/usr/bin/git}"
GH="${GH_BIN:-gh}"
BUN="${BUN_BIN:-bun}"
CARGO="${CARGO_BIN:-$HOME/.cargo/bin/cargo}"
SWAP="${AGM_SWAP_SCRIPT:-$DEPLOY/scripts/ops/daemon-swap.sh}"
NICE="${NICE_BIN:-nice}"

log() { echo "$(date '+%F %T') $*" >> "$LOG"; }
. "${0%/*}/kick-common.sh" 2>/dev/null || {
  _m="找不到 ${0%/*}/kick-common.sh，${0##*/} 停擺（它要和 kick 一起裝進 bin/）"
  echo "$(date '+%F %T') ALERT kick_common_missing：$_m" >> "$LOG"; echo "$_m" >&2
  _a="${AGM:-$DIR/bin/agm}"
  [ -x "$_a" ] && "$_a" --compact ops-alert --source "${OWNER:-${0##*/}}" --reason kick_common_missing --detail "$_m" >> "$LOG" 2>&1
  exit 1
}

OPS_AUTO_FLAG="$DIR/ops-auto-install.enabled"
ops_auto_on() { [ "${AGM_OPS_AUTO_INSTALL:-0}" = 1 ] || [ -f "$OPS_AUTO_FLAG" ]; }
ops_install_step() {
  ops_auto_on || return 0
  _osha="$1"; _ogreen="$2"
  _oscript=$("$GIT" -C "$DEPLOY" show "${_osha}:scripts/ops/ops-install.sh" 2>/dev/null) || { log "ops 自動安裝：${_osha} 沒有 ops-install.sh，跳過"; return 0; }
  _oplan=$(printf '%s\n' "$_oscript" | bash -s -- --repo "$DEPLOY" --ref "$_osha" --dir "$DIR" --dry-run 2>>"$LOG")
  _on=$(printf '%s\n' "$_oplan" | sed -n 's/.*changes=\([0-9][0-9]*\) failed=.*/\1/p' | tail -1)
  case "$_on" in ''|*[!0-9]*) log "ops 自動安裝：看不懂 dry-run 的結果，這輪不裝"; return 0 ;; esac
  if printf '%s\n' "$_oplan" | grep -q '^drifted '; then
    alert ops_install_drift "已安裝的 ops 腳本被手改過（不是 repo 任何一版），自動換新不會覆蓋它：$(printf '%s\n' "$_oplan" | grep '^drifted ' | cut -d' ' -f2 | head -5 | tr '\n' ' ')。先看差在哪（agm ops-sync --check），要換就手動 ops-install.sh --force（舊檔會備份）"
  fi
  [ "$_on" -gt 0 ] || return 0
  if [ "$_ogreen" = 1 ]; then
    _ostate=$("$GH" api "repos/${GH_REPO}/commits/${_osha}/status" -q ".statuses[]|select(.context==\"${CI_CONTEXT}\")|.state" 2>>"$LOG" | head -1)
    if [ "$_ostate" != success ]; then
      log "ops 自動安裝：有 ${_on} 支待更新，但 $(printf '%s' "$_osha" | cut -c1-8) 的 ${CI_CONTEXT} 是「${_ostate:-沒有狀態}」，等綠燈"
      return 0
    fi
  fi
  _orc=0
  _oout=$(printf '%s\n' "$_oscript" | bash -s -- --repo "$DEPLOY" --ref "$_osha" --dir "$DIR" 2>>"$LOG") || _orc=$?
  printf '%s\n' "$_oout" | sed 's/^/ops-install: /' >> "$LOG"
  if [ "$_orc" -ne 0 ]; then
    alert ops_install_failed "自動換新已安裝的 ops 腳本有失敗（沒換成的舊版原封不動）：$(printf '%s\n' "$_oout" | grep '^failed ' | head -3 | tr '\n' ';')。細節見 ${LOG}，備份在 ${DIR}/ops-install-backups"
  else
    log "ops 自動安裝：已把 ${_on} 支換成 $(printf '%s' "$_osha" | cut -c1-8) 的版本"
  fi
  return 0
}

sync_main_tree() {
  _br=$("$GIT" -C "$REPO" symbolic-ref -q --short HEAD 2>/dev/null) || _br=""
  [ "$_br" = main ] || { log "主樹 ${REPO} 在「${_br:-分離的 HEAD}」，不是 main，不同步"; return 0; }
  "$GIT" -C "$REPO" fetch -q origin main >> "$LOG" 2>&1 || { log "主樹 fetch 失敗，這次不同步"; return 0; }
  _up=$("$GIT" -C "$REPO" rev-parse --verify -q origin/main 2>/dev/null) || { log "讀不到主樹的 origin/main，不同步"; return 0; }
  [ "$("$GIT" -C "$REPO" rev-parse HEAD 2>/dev/null)" = "$_up" ] && return 0
  if "$GIT" -C "$REPO" merge-base --is-ancestor HEAD "$_up" 2>/dev/null &&
     "$GIT" -C "$REPO" merge -q --ff-only "$_up" >> "$LOG" 2>&1; then
    log "主樹快轉到 $(printf '%s' "$_up" | cut -c1-8)"; return 0
  fi
  _cherry=$("$GIT" -C "$REPO" cherry "$_up" HEAD 2>>"$LOG") || { alert main_tree_sync_failed "讀不出主樹 ${REPO} 與 origin/main 的差異，不同步"; return 0; }
  if printf '%s\n' "$_cherry" | grep -q '^+'; then
    alert main_tree_diverged "主樹 ${REPO} 有 origin/main 沒有的本地 commit，自動同步不動它；請人工確認（git -C ${REPO} log origin/main..HEAD）後對齊"
    return 0
  fi
  if ! "$GIT" -C "$REPO" diff --quiet HEAD -- 2>/dev/null; then
    alert main_tree_diverged "主樹 ${REPO} 有已追蹤檔的未提交改動，自動同步不動它；請人工確認後對齊 origin/main"
    return 0
  fi
  if "$GIT" -C "$REPO" reset -q --keep "$_up" >> "$LOG" 2>&1; then
    log "主樹的本地 commit 都已在 origin/main，對齊到 $(printf '%s' "$_up" | cut -c1-8)"
  else
    alert main_tree_sync_failed "主樹 ${REPO} 對齊 origin/main 失敗（細節見 ${LOG}），主樹未動"
  fi
  return 0
}

LOCK="$DIR/daemon-update.lock"
LOCK_GUARD="$DIR/daemon-update.lock.guard"
LOCK_STALE_SECS=${AGM_LOCK_STALE_SECS:-120}
LOCK_HUNG_SECS=${AGM_LOCK_HUNG_SECS:-7200}
acquire_guard() {
  [ ! -L "$LOCK_GUARD" ] || return 2
  exec 9>>"$LOCK_GUARD" || return 2
  python3 -c '
import errno, fcntl, sys
try:
    fcntl.flock(9, fcntl.LOCK_EX | fcntl.LOCK_NB)
except OSError as e:
    sys.exit(1 if e.errno in (errno.EACCES, errno.EAGAIN) else 2)
' >/dev/null 2>&1
}
is_self_runner() {
  _cmd=$(ps -o command= -p "$1" 2>/dev/null) || return 1
  RUNNER_COMMAND="$_cmd" RUNNER_SCRIPT="$SELF" python3 -c '
import os, shlex, sys
try:
    argv = shlex.split(os.environ["RUNNER_COMMAND"])
except ValueError:
    sys.exit(1)
if len(argv) < 2 or os.path.basename(argv[0]).lstrip("-") not in ("bash", "sh"):
    sys.exit(1)
sys.exit(0 if os.path.normpath(argv[1]) == os.environ["RUNNER_SCRIPT"] else 1)
' >/dev/null 2>&1
}
FAILS="$DIR/daemon-update.fails"
FAIL_ALERT_AFTER=${AGM_FAIL_ALERT_AFTER:-6}
ROUND_FAIL=""
note_fail() { ROUND_FAIL="${ROUND_FAIL:-$1}"; log "$1"; }
record_rejected() {
  local target="$1" tmp
  [ ! -L "$REJECTED" ] || return 1
  if [ -e "$REJECTED" ] && [ ! -f "$REJECTED" ]; then return 1; fi
  tmp=$(mktemp "$DIR/daemon-update.rejected.tmp.XXXXXX") || return 1
  if [ -f "$REJECTED" ] && ! cat "$REJECTED" > "$tmp"; then rm -f "$tmp"; return 1; fi
  if ! grep -qxF -- "$target" "$tmp"; then
    { printf '\n'; printf '%s\n' "$target"; } >> "$tmp" || { rm -f "$tmp"; return 1; }
  fi
  if ! mv -f "$tmp" "$REJECTED"; then rm -f "$tmp"; return 1; fi
  return 0
}
write_built_marker() {
  local sha="$1" tmp
  [ ! -L "$BUILT" ] || return 1
  if [ -e "$BUILT" ] && [ ! -f "$BUILT" ]; then return 1; fi
  tmp=$(mktemp "$DIR/daemon-update.built.tmp.XXXXXX") || return 1
  if ! printf '%s\n' "$sha" > "$tmp"; then rm -f "$tmp"; return 1; fi
  if ! mv -f "$tmp" "$BUILT"; then rm -f "$tmp"; return 1; fi
  return 0
}
settle() {
  if [ -n "$ROUND_FAIL" ]; then
    _n=$(cat "$FAILS" 2>/dev/null)
    case "$_n" in ''|*[!0-9]*) _n=0 ;; esac
    _n=$((_n + 1)); echo "$_n" > "$FAILS"
    [ "$_n" -ge "$FAIL_ALERT_AFTER" ] && alert check_failing "自動部署連續 ${_n} 輪沒能完成：${ROUND_FAIL}"
  else
    rm -f "$FAILS" 2>/dev/null
  fi
  return 0
}
cleanup() {
  settle
  [ -z "$CLONE_STAGE" ] || rm -rf "$CLONE_STAGE" 2>/dev/null || true
  rm -rf "$LOCK" 2>/dev/null || true
}
take_lock() {
  mkdir "$LOCK" 2>/dev/null || return 1
  if ! echo "$$ $(date +%s)" > "$LOCK/owner"; then rm -rf "$LOCK" 2>/dev/null; return 1; fi
  trap cleanup EXIT
  return 0
}
mkdir -p "$DIR" 2>/dev/null
_guard_rc=0
acquire_guard || _guard_rc=$?
if [ "$_guard_rc" -eq 1 ]; then
  _pid=$(cut -d' ' -f1 "$LOCK/owner" 2>/dev/null)
  _age=$(lock_age)
  if [ "$_age" -lt 0 ]; then
    log "鎖的時間在未來（${_age} 秒），時鐘倒退或鎖是搬來的，年齡不可信"
    alert runner_clock_skew "部署鎖 ${LOCK} 的時間戳在未來，無法安全判斷持有者是否逾時；本輪不回收鎖，請檢查系統時鐘與鎖狀態"
  elif [ -n "$_pid" ] && kill -0 "$_pid" 2>/dev/null && is_self_runner "$_pid"; then
    if [ "$_age" -ge "$LOCK_HUNG_SECS" ]; then
      alert runner_hung "上一輪（pid ${_pid}）已經跑了 ${_age} 秒還沒結束，自動部署停住。請確認它在做什麼，必要時結束它並移除 ${LOCK}"
    fi
  elif [ "$_age" -ge "$LOCK_HUNG_SECS" ]; then
    alert runner_hung "自動部署 runner 持有 OS lock 已 ${_age} 秒，但鎖的 owner（${_pid:-未知}）無法驗證；部署停住。請確認後必要時結束行程並移除 ${LOCK}"
  fi
  exit 0
elif [ "$_guard_rc" -ne 0 ]; then
  alert lock_unavailable "無法建立或取得部署鎖（${LOCK_GUARD}），本輪跳過"
  exit 0
fi
if ! take_lock; then
  _pid=$(cut -d' ' -f1 "$LOCK/owner" 2>/dev/null)
  _age=$(lock_age)
  if [ "$_age" -lt 0 ]; then
    log "鎖的時間在未來（${_age} 秒），時鐘倒退或鎖是搬來的，年齡不可信"
    alert runner_clock_skew "部署鎖 ${LOCK} 的時間戳在未來，無法安全判斷持有者是否逾時；本輪不回收鎖，請檢查系統時鐘與鎖狀態"
    exit 0
  fi
  if [ -n "$_pid" ] && kill -0 "$_pid" 2>/dev/null && is_self_runner "$_pid"; then
    if [ "$_age" -ge "$LOCK_HUNG_SECS" ]; then
      alert runner_hung "上一輪（pid ${_pid}）已經跑了 ${_age} 秒還沒結束，自動部署停住。請確認它在做什麼，必要時結束它並移除 ${LOCK}"
    fi
    exit 0
  fi
  if [ "$_age" -lt "$LOCK_STALE_SECS" ]; then
    log "鎖剛建立（${_age} 秒）但讀不到執行者，這輪跳過"
    exit 0
  fi
  rm -rf "$LOCK" 2>/dev/null
  if take_lock; then
    log "清掉殘留鎖（執行者 ${_pid:-未知} 已不在，鎖存在 ${_age} 秒）並接手這一輪"
  else
    alert stale_lock "殘留鎖 ${LOCK} 清不掉（執行者 ${_pid:-未知} 已不在），自動部署停住。請人工確認沒有執行者後移除它"
    exit 0
  fi
fi
log "== check"

NOW=0; NOW_SHA=""
NOW_FAILS="$DIR/daemon-update.now.fails"
NOW_FAIL_MAX=${AGM_NOW_FAIL_MAX:-3}
drop_now() { rm -f "$NOW_FILE" "$NOW_FAILS"; log "收掉立即部署請求（$1）"; }
now_fail() {
  [ "$NOW" = 1 ] || return 0
  _nsha=""; _nn=0
  [ -e "$NOW_FAILS" ] && read -r _nsha _nn < "$NOW_FAILS" || true
  case "$_nn" in ''|*[!0-9]*) _nn=0 ;; esac
  if [ "$_nsha" = "$NOW_SHA" ]; then _nn=$((_nn + 1)); else _nn=1; fi
  echo "$NOW_SHA $_nn" > "$NOW_FAILS.tmp" && mv -f "$NOW_FAILS.tmp" "$NOW_FAILS"
  if [ "$_nn" -ge "$NOW_FAIL_MAX" ]; then
    alert now_failed "立即部署 ${NOW_SHA} 連續 ${_nn} 次失敗（${1}），已收掉請求；細節見 ${DIR}/daemon-swap.log"
    drop_now "連續 ${_nn} 次失敗"
  fi
  return 0
}
NOW_TTL=${AGM_NOW_TTL_SECS:-7200}
if [ -f "$NOW_FILE" ]; then
  NOW_RC=0
  NOW_SHA=$(python3 -c '
import json,sys
from datetime import datetime,timezone
with open(sys.argv[1]) as f: d=json.load(f)
c=d.get("sha")
if not isinstance(c,str) or not c.strip() or " " in c.strip(): sys.exit(1)
print(c.strip())
at=d.get("requested_at")
if isinstance(at,str) and at.strip():
    try: t=datetime.fromisoformat(at.strip().replace("Z","+00:00"))
    except ValueError: sys.exit(0)
    if t.tzinfo is None: t=t.replace(tzinfo=timezone.utc)
    if (datetime.now(timezone.utc)-t).total_seconds()>int(sys.argv[2]): sys.exit(3)
' "$NOW_FILE" "$NOW_TTL" 2>/dev/null) || NOW_RC=$?
  if [ "$NOW_RC" = 3 ] && [ -n "$NOW_SHA" ]; then
    alert now_request_expired "立即部署請求 ${NOW_SHA} 已超過 ${NOW_TTL} 秒沒做，已收掉；要部署請使用者重按一次"
    drop_now "請求過期"
    NOW_SHA=""
  elif [ "$NOW_RC" != 0 ]; then
    NOW_SHA=""
    alert now_request_corrupt "立即部署請求 ${NOW_FILE} 讀不出 sha，已收掉；請使用者重按一次"
    drop_now "檔案壞掉"
  elif [ -n "$NOW_SHA" ]; then
    NOW=1
    log "立即部署請求：commit ${NOW_SHA}（使用者按下，不等 ubuntu-ci）"
  else
    alert now_request_corrupt "立即部署請求 ${NOW_FILE} 讀不出 sha，已收掉；請使用者重按一次"
    drop_now "檔案壞掉"
  fi
fi

ORIGIN_URL=$("$GIT" -C "$REPO" remote get-url origin 2>/dev/null) || { note_fail "讀不到 ${REPO} 的 origin URL，建不出專用 checkout"; exit 0; }
if [ -e "$DEPLOY" ] && [ ! -d "$DEPLOY/.git" ]; then
  alert deploy_checkout_invalid "專用 checkout 路徑 ${DEPLOY} 已存在但不是 git clone，自動部署停住；請確認該路徑內容後移開它"
  note_fail "專用 checkout 路徑存在但不是 git clone"
  exit 0
fi
if [ ! -d "$DEPLOY/.git" ]; then
  mkdir -p "$(dirname "$DEPLOY")" 2>/dev/null
  CLONE_STAGE=$(mktemp -d "${DEPLOY}.clone.XXXXXX") || { note_fail "無法建立專用 checkout 的暫存 clone 目錄"; exit 0; }
  "$GIT" clone -q --no-checkout "$ORIGIN_URL" "$CLONE_STAGE/repo" >> "$LOG" 2>&1 || { note_fail "clone 專用 checkout ${DEPLOY} 失敗"; exit 0; }
  if [ -e "$DEPLOY" ] || [ -L "$DEPLOY" ]; then
    alert deploy_checkout_invalid "clone 完成時專用 checkout 路徑 ${DEPLOY} 已被建立；保留既有路徑並丟棄暫存 clone"
    note_fail "clone 完成時專用 checkout 路徑已被建立"
    exit 0
  fi
  mv "$CLONE_STAGE/repo" "$DEPLOY" >> "$LOG" 2>&1 || { note_fail "無法將暫存 clone 移到專用 checkout ${DEPLOY}"; exit 0; }
  log "建好專用 checkout ${DEPLOY}"
fi
CHECKOUT_ORIGIN_URL=$("$GIT" -C "$DEPLOY" remote get-url origin 2>/dev/null) || CHECKOUT_ORIGIN_URL=""
if [ -z "$CHECKOUT_ORIGIN_URL" ] || [ "$CHECKOUT_ORIGIN_URL" != "$ORIGIN_URL" ]; then
  alert deploy_checkout_origin_mismatch "專用 checkout ${DEPLOY} 的 origin 與正式 repo 不同；預期「${ORIGIN_URL}」、實際「${CHECKOUT_ORIGIN_URL:-讀取失敗}」，停止 fetch、建置與部署"
  note_fail "專用 checkout origin 不符合正式 repo，停止部署"
  exit 0
fi
"$GIT" -C "$DEPLOY" fetch -q origin main >> "$LOG" 2>&1 || { note_fail "fetch origin/main 失敗，這輪不動"; exit 0; }
HEAD_SHA=$("$GIT" -C "$DEPLOY" rev-parse origin/main) || { note_fail "無法讀取 origin/main，跳過"; exit 0; }

PATHS=""
[ -x "$AGM" ] && PATHS=$("$AGM" --compact build-inputs 2>/dev/null | python3 -c '
import json,sys
try:
    d = json.load(sys.stdin)
except Exception:
    sys.exit(1)
print(" ".join(d.get("paths") or []))
' 2>/dev/null)
[ -n "$PATHS" ] || PATHS="daemon crates web Cargo.toml Cargo.lock docs/goals/agm-supervisor-persona.md docs/goals/agm-responder-persona.md scripts/agm.py"
# shellcheck disable=SC2086  # PATHS 是刻意要拆成多個參數的
binary_same() { "$GIT" -C "$DEPLOY" diff --quiet "$1" "$2" -- $PATHS; }

LIVE_BIN="$REPO/target/release/agents-managerd"
embedded_live_sha() {
  local line name pkg sha extra
  line=$("$LIVE_BIN" --version 2>/dev/null | head -n 1) || return 1
  read -r name pkg sha extra <<< "$line"
  [ -n "$sha" ] && [ -z "$extra" ] || return 1
  printf "%s" "$sha"
}
BUILT_SHA=""
[ -f "$BUILT" ] && BUILT_SHA=$(tr -d '[:space:]' < "$BUILT")
BUILT_FULL=""
[ -n "$BUILT_SHA" ] && BUILT_FULL=$("$GIT" -C "$DEPLOY" rev-parse --verify --quiet "${BUILT_SHA}^{commit}" 2>/dev/null) || BUILT_FULL=""
LIVE_SHA=$(embedded_live_sha) || LIVE_SHA=""
if [ -n "$LIVE_SHA" ]; then
  LIVE_FULL=$("$GIT" -C "$DEPLOY" rev-parse --verify --quiet "${LIVE_SHA}^{commit}" 2>/dev/null) || LIVE_FULL=""
  if [ -z "$LIVE_FULL" ]; then
    log "線上 binary 內嵌的 sha「${LIVE_SHA}」不是專用 checkout 裡的 commit（髒樹或沒抓到），沿用 ${BUILT} 的「${BUILT_SHA}」"
  elif [ "$LIVE_FULL" != "$BUILT_FULL" ]; then
    LIVE_SHORT=$(printf '%s' "$LIVE_FULL" | cut -c1-8)
    if write_built_marker "$LIVE_SHORT"; then
      alert built_reconciled "線上 binary 內嵌的 ${LIVE_SHORT} 與 ${BUILT} 記的「${BUILT_SHA:-空}」不符（像是管線外換過 binary），已以線上為準更新 ${BUILT}"
    else
      note_fail "線上 binary 是 ${LIVE_SHORT}，但 ${BUILT} 寫入失敗（記的是「${BUILT_SHA:-空}」）；這輪照線上版本往上換"
    fi
    BUILT_SHA="$LIVE_SHORT"
    BUILT_FULL="$LIVE_FULL"
  fi
fi
if [ -z "$BUILT_SHA" ] || [ -z "$BUILT_FULL" ]; then
  alert built_unknown "讀不到線上是哪一版（${BUILT} 是「${BUILT_SHA}」，線上 binary 也說不出 commit），不知道從哪裡往上換，自動部署停住。請確認線上 binary 的 short sha 後寫進那個檔"
  exit 0
fi

TARGET=""
if [ "$NOW" = 1 ]; then
  TARGET=$("$GIT" -C "$DEPLOY" rev-parse --verify --quiet "${NOW_SHA}^{commit}") || TARGET=""
  if [ -z "$TARGET" ] || ! "$GIT" -C "$DEPLOY" merge-base --is-ancestor "$TARGET" origin/main 2>/dev/null; then
    alert now_target_invalid "立即部署的 commit ${NOW_SHA} 不在 origin/main 上（或讀不到），這趟不做"
    drop_now "commit 不在 origin/main"; exit 0
  fi
  if ! "$GIT" -C "$DEPLOY" merge-base --is-ancestor "$BUILT_FULL" "$TARGET" 2>/dev/null; then
    alert now_target_older "線上版本 ${BUILT_SHA} 不是立即部署目標 ${NOW_SHA} 的祖先或同一顆，這趟拒絕降版"
    drop_now "target older than live"; exit 0
  fi
  if binary_same "$BUILT_FULL" "$TARGET"; then
    drop_now "${BUILT_SHA} 到 ${NOW_SHA} 只動到不進 binary 的檔，已經是最新"; exit 0
  fi
else
  if binary_same "$BUILT_FULL" "$HEAD_SHA"; then
    log "${BUILT_SHA} 之後沒有會進 binary 的差異，跳過（origin/main ${HEAD_SHA}）"
    sync_main_tree
    ops_install_step "$HEAD_SHA" 1
    exit 0
  fi
  LAST_SHA=$(tr -d '[:space:]' < "${CI_ROOT}/last-sha" 2>/dev/null) || LAST_SHA=""
  LAST_FULL=""
  if printf '%s' "$LAST_SHA" | grep -Eq '^[0-9a-f]{40}$'; then
    LAST_FULL=$("$GIT" -C "$DEPLOY" rev-parse --verify --quiet "${LAST_SHA}^{commit}" 2>/dev/null) || LAST_FULL=""
  fi
  if [ -n "$LAST_FULL" ] \
     && "$GIT" -C "$DEPLOY" merge-base --is-ancestor "$LAST_FULL" origin/main 2>/dev/null \
     && ! "$GIT" -C "$DEPLOY" merge-base --is-ancestor "$LAST_FULL" "$BUILT_FULL" 2>/dev/null \
     && ! binary_same "$BUILT_FULL" "$LAST_FULL" \
     && ! { [ -f "$REJECTED" ] && grep -qx "$LAST_FULL" "$REJECTED"; }; then
    GH_RC=0
    GH_OUT=$("$GH" api "repos/${GH_REPO}/commits/${LAST_FULL}/status" -q ".statuses[]|select(.context==\"${CI_CONTEXT}\")|.state" 2>>"$LOG") || GH_RC=$?
    if [ "$GH_RC" -eq 0 ] && [ "$(printf '%s\n' "$GH_OUT" | head -1)" = success ]; then
      TARGET=$LAST_FULL
      log "挑到 ${TARGET}（${CI_CONTEXT} success，來自 ${CI_CONTEXT} 的 last-sha；origin/main ${HEAD_SHA}，線上 ${BUILT_SHA}）"
    fi
  fi
  REACHED=0
  SEEN=0
  if [ -z "$TARGET" ]; then
    for CAND in $("$GIT" -C "$DEPLOY" rev-list --first-parent -n "$CI_LOOKBACK" origin/main); do
      SEEN=$((SEEN + 1))
      [ "$CAND" = "$BUILT_FULL" ] && { REACHED=1; break; }
      "$GIT" -C "$DEPLOY" merge-base --is-ancestor "$CAND" "$BUILT_FULL" 2>/dev/null && { REACHED=1; break; }
      binary_same "$BUILT_FULL" "$CAND" && { REACHED=1; break; }
      if [ -f "$REJECTED" ] && grep -qx "$CAND" "$REJECTED"; then
        log "${CAND} 之前換上去被回滾過，不再挑它"; continue
      fi
      GH_RC=0
      GH_OUT=$("$GH" api "repos/${GH_REPO}/commits/${CAND}/status" -q ".statuses[]|select(.context==\"${CI_CONTEXT}\")|.state" 2>>"$LOG") || GH_RC=$?
      STATE=$(printf '%s\n' "$GH_OUT" | head -1)
      if [ "$GH_RC" -ne 0 ]; then
        note_fail "問不到 ${CAND} 的 ${CI_CONTEXT} 狀態（gh rc=${GH_RC}），這輪不動"; exit 0
      fi
      if [ "$STATE" = success ]; then
        TARGET=$CAND
        log "挑到 ${TARGET}（${CI_CONTEXT} success；origin/main ${HEAD_SHA}，線上 ${BUILT_SHA}）"
        break
      fi
      log "${CAND} 的 ${CI_CONTEXT} 是「${STATE:-沒有狀態}」，往前一顆找"
    done
  fi
  if [ -z "$TARGET" ]; then
    if [ "$REACHED" = 0 ] && [ "$SEEN" -ge "$CI_LOOKBACK" ]; then
      log "回看上限 ${CI_LOOKBACK} 顆用完，還沒走到線上 ${BUILT_SHA}（${CI_CONTEXT} 綠燈的 commit 可能落在更早的地方）；等下一輪"
    else
      log "往回 ${SEEN} 顆沒有比線上 ${BUILT_SHA} 新、又有會進 binary 的差異、${CI_CONTEXT} 綠燈的 commit，等下一輪"
    fi
    exit 0
  fi
fi
SHORT=$(printf '%s' "$TARGET" | cut -c1-8)

BUILD_MARK="$DEPLOY/target/release/.built-for"
if [ "$(cat "$BUILD_MARK" 2>/dev/null)" = "$TARGET" ] && [ -x "$DEPLOY/target/release/agents-managerd" ]; then
  log "${SHORT} 已經建好，直接換版"
else
  rm -f "$BUILD_MARK"
  "$GIT" -C "$DEPLOY" checkout -q --force --detach "$TARGET" >> "$LOG" 2>&1 || { note_fail "checkout ${SHORT} 失敗"; exit 0; }
  "$GIT" -C "$DEPLOY" clean -fdq >> "$LOG" 2>&1 || true
  log "建置 ${SHORT}：web"
  [ -r "$WEB_BUILDER" ] || { note_fail "找不到配對的 web helper：${WEB_BUILDER}；請同時安裝 kick 與 helper"; now_fail "缺少 web helper"; exit 0; }
  python3 "$WEB_BUILDER" --web "$DEPLOY/web" --bun "$BUN" >> "$LOG" 2>&1 || { note_fail "web 建置失敗（${SHORT}）"; now_fail "web 建置失敗"; exit 0; }
  log "建置 ${SHORT}：daemon（cargo build --release）"
  FAST_ENV=()
  [ "${AGM_DEPLOY_FAST_BUILD:-1}" = 0 ] || FAST_ENV=(CARGO_PROFILE_RELEASE_LTO=false CARGO_PROFILE_RELEASE_INCREMENTAL=true)
  ( cd "$DEPLOY" && AM_REAL_CARGO="$CARGO" PATH="$(dirname "$CARGO"):$PATH" "$NICE" -n 10 env ${FAST_ENV[@]+"${FAST_ENV[@]}"} "$CARGO" build --release -p agents-managerd ) >> "$LOG" 2>&1 \
    || { note_fail "cargo build 失敗（${SHORT}）"; now_fail "cargo build 失敗"; exit 0; }
  [ -x "$DEPLOY/target/release/agents-managerd" ] || { note_fail "cargo build 沒產出 binary（${SHORT}）"; exit 0; }
  echo "$TARGET" > "$BUILD_MARK"
  log "建好 ${SHORT}"
fi

OLDHASH=$(shasum -a 256 "$LIVE_BIN" 2>/dev/null | cut -c1-16)
[ -n "$OLDHASH" ] || { note_fail "讀不到正式 binary ${LIVE_BIN}，不換版"; exit 0; }
[ -f "$SWAP" ] || { note_fail "找不到換版腳本 ${SWAP}"; exit 0; }
log "換版 ${BUILT_SHA} → ${SHORT}"
SWAP_RC=0
AGM_DIR="$DIR" AGM_REPO="$REPO" bash "$SWAP" --sha "$TARGET" --old "$BUILT_SHA" --old-hash "$OLDHASH" --owner "$OWNER" --checkout "$DEPLOY" >> "$LOG" 2>&1 || SWAP_RC=$?
case "$SWAP_RC" in
  0)
    log "已換上 ${SHORT}"
    [ "$NOW" = 1 ] && drop_now "已部署 ${SHORT}"
    sync_main_tree
    ops_install_step "$TARGET" 0
    ;;
  4)
    log "還有人在忙（或複查不安全），這輪沒換版（daemon-swap rc=4）；下一輪再試"
    rm -f "$NOW_FAILS"
    ;;
  6)
    if write_built_marker "$SHORT"; then
      alert swap_forward_fixed "換上 ${SHORT} 後驗證沒過，但升過 schema 不能放回舊 binary，已往前修：新 binary 服務中。請看 ${DIR}/daemon-swap.log 確認"
    else
      note_fail "daemon-update.built 寫入失敗：前向修復的 ${SHORT} 已在線，下一輪可能重試部署"
      alert swap_built_marker_failed "前向修復的 ${SHORT} 已在線，但 daemon-update.built 寫入失敗、下一輪可能重試部署。請修復 ${BUILT} 並確認部署狀態"
    fi
    [ "$NOW" = 1 ] && drop_now "往前修"
    ;;
  7)
    if record_rejected "$TARGET"; then
      alert swap_rolled_back "換上 ${SHORT} 失敗、已回滾到 ${BUILT_SHA}；之後不再自動挑這顆。請看 ${DIR}/daemon-swap.log"
    else
      note_fail "rejected 清單寫入失敗：${SHORT} 已回滾但下一輪可能重新挑選"
      alert swap_rolled_back "換上 ${SHORT} 失敗、已回滾到 ${BUILT_SHA}，但 rejected 清單寫入失敗、下一輪可能重新挑選。請修復 ${REJECTED} 後確認部署狀態"
    fi
    [ "$NOW" = 1 ] && drop_now "已回滾"
    ;;
  8)
    alert swap_lease_not_released "換上 ${SHORT} 成功，但 restart 窗口沒交還成功，下一次換版可能拿不到窗口。請看 ${DIR}/daemon-swap.log"
    [ "$NOW" = 1 ] && drop_now "已部署 ${SHORT}（窗口沒交還）"
    ;;
  10)
    alert swap_binary_sha_mismatch "要換上 ${SHORT} 的 binary 內嵌的 sha 不是它（或髒樹建的、或舊 binary 沒內嵌 sha），換版中止、窗口沒拿、線上沒動。請看 ${DIR}/daemon-swap.log"
    ROUND_FAIL="${ROUND_FAIL:-新 binary 的內嵌 sha 對不上 ${SHORT}}"
    rm -f "$BUILD_MARK"
    now_fail "rc=10"
    ;;
  11)
    if write_built_marker "$SHORT"; then
      log "daemon-swap 的部署 sha 標記失敗，kick 已修復為 ${SHORT}"
      alert swap_built_marker_repaired "新 binary ${SHORT} 已驗證並在線；daemon-swap 寫入部署 sha 標記失敗，但 kick 已修復 ${BUILT}"
    else
      note_fail "daemon-update.built 寫入失敗：已驗證並在線的 ${SHORT} 無法記成部署版本，下一輪可能重試"
      alert swap_built_marker_failed "新 binary ${SHORT} 已驗證並在線，但 daemon-update.built 寫入失敗、下一輪可能重試部署。請修復 ${BUILT} 並確認部署狀態"
    fi
    [ "$NOW" = 1 ] && drop_now "已部署但 marker 曾失敗"
    ;;
  9)
    alert swap_daemon_too_old "線上 daemon 太舊，沒有 restart-window 路由，自動換版做不了。要先手動換過一次含這條路由的 binary（scripts/ops/README.md）"
    ROUND_FAIL="${ROUND_FAIL:-線上 daemon 太舊，沒有 restart-window 路由}"
    [ "$NOW" = 1 ] && drop_now "daemon 太舊，立即部署做不了"
    ;;
  *)
    note_fail "daemon-swap 失敗 rc=${SWAP_RC}（${SHORT}），細節見 ${DIR}/daemon-swap.log"
    now_fail "rc=${SWAP_RC}"
    ;;
esac
_skips=$(cat "$DIR/daemon-swap.probe-skips" 2>/dev/null)
case "$_skips" in ''|*[!0-9]*) _skips=0 ;; esac
if [ "$_skips" -ge 3 ]; then
  _why=$(cat "$DIR/daemon-swap.probe-skips.why" 2>/dev/null); case "$_why" in "輸入框有字"|"目標不能收 prompt") ;; *) _why="對象沒有 run" ;; esac
  if [ "$_why" = "目標不能收 prompt" ]; then
    alert swap_probe_skipped "換版前的送達自測已連續 ${_skips} 次略過（自測對象卡在 blocked／busy／等權限回應，退回的對象也不能收）：到自測對象的 pane 回應它卡住的提示，或在 ${DIR}/runtime.json 設 swap_probe_bot_id 改指一顆常駐、會回 ok 的 bot"
  elif [ "$_why" = "輸入框有字" ]; then
    alert swap_probe_skipped "換版前的送達自測已連續 ${_skips} 次略過（自測對象的輸入框有未送出的字，腳本不會替人清）：到那顆 bot 的 pane 清掉輸入框殘字（例如 Claude 滿意度問卷落進去的「0」），或在 ${DIR}/runtime.json 設 swap_probe_bot_id 改指別顆"
  else
    alert swap_probe_skipped "換版前的送達自測已連續 ${_skips} 次略過（${_why}）：在 ${DIR}/runtime.json 設 swap_probe_bot_id 指到一顆常駐、會回 ok 的 bot（沒設時依序用 responder_bot_id、manager_bot_id）"
  fi
fi
exit 0
