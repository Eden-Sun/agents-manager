#!/bin/bash
set -u
_extra="${AGM_EXTRA_PATH-${HOME:-/nonexistent}/.local/bin:/opt/homebrew/bin:/usr/local/bin}"
[ -n "$_extra" ] && PATH="$_extra:$PATH"
export PATH

DIR="${AGM_DIR:-${HOME:-/nonexistent}/.config/agents-manager/supervisor/AGM}"
REPO="${AGM_REPO:-${HOME:-/nonexistent}/project/agents-manager}"
HERDR_REPO="${HERDR_REPO:-herdrdev/herdr}"
AGM="$DIR/bin/agm"
BIN="${AM_BINARY:-$REPO/target/release/agents-managerd}"
CHANGELOG_URL="${HERDR_CHANGELOG_URL:-https://raw.githubusercontent.com/${HERDR_REPO}/master/CHANGELOG.md}"
LOG="$DIR/herdr-update.log"
STATE="$DIR/herdr-update.last"
FAILS="$DIR/herdr-update.fails"
OWNER="${AM_AGENT_NAME:-herdr-update-kick}"
platform() {
  if [ -n "${AGM_OPS_PLATFORM:-}" ]; then printf '%s' "$AGM_OPS_PLATFORM"; return; fi
  case "$(uname -s 2>/dev/null)" in Darwin) printf darwin ;; Linux) printf linux ;; *) printf '%s' "$(uname -s 2>/dev/null)" ;; esac
}
FAIL_ALERT_AFTER=${AGM_FAIL_ALERT_AFTER:-2}

log() { echo "$(date '+%F %T') $*" >> "$LOG"; }
. "${0%/*}/kick-common.sh" 2>/dev/null || {
  _m="找不到 ${0%/*}/kick-common.sh，${0##*/} 停擺（它要和 kick 一起裝進 bin/）"
  echo "$(date '+%F %T') ALERT kick_common_missing：$_m" >> "$LOG"; echo "$_m" >&2
  _a="${AGM:-$DIR/bin/agm}"
  [ -x "$_a" ] && "$_a" --compact ops-alert --source "${OWNER:-${0##*/}}" --reason kick_common_missing --detail "$_m" >> "$LOG" 2>&1
  exit 1
}

write_state() {
  _t="$STATE.tmp.$$"
  if printf '%s\n' "$1" > "$_t" 2>/dev/null && mv -f "$_t" "$STATE" 2>/dev/null; then return 0; fi
  rm -rf "$_t" 2>/dev/null
  return 1
}

[ -x "$AGM" ] || { log "agm CLI 不在 ${AGM}，跳過"; exit 0; }

fail_run() {
  log "$1"
  _n=$(cat "$FAILS" 2>/dev/null)
  case "$_n" in ''|*[!0-9]*) _n=0 ;; esac
  _n=$((_n + 1))
  echo "$_n" > "$FAILS"
  [ "$_n" -ge "$FAIL_ALERT_AFTER" ] && alert check_failing "herdr 新版偵測連續 ${_n} 輪沒能完成：${1}"
  exit 0
}
ran_ok() { rm -f "$FAILS" 2>/dev/null; return 0; }

[ -x "$BIN" ] || fail_run "找不到 ${BIN}，跳過（需要先建好 release binary）"
command -v herdr >/dev/null 2>&1 || { alert missing_dependency "找不到 herdr（PATH=${PATH}），herdr 新版偵測停住"; exit 0; }
command -v gh >/dev/null 2>&1 || { alert missing_dependency "找不到 gh（PATH=${PATH}），herdr 新版偵測停住"; exit 0; }
command -v python3 >/dev/null 2>&1 || { alert missing_dependency "找不到 python3（PATH=${PATH}），herdr 新版偵測停住"; exit 0; }
command -v curl >/dev/null 2>&1 || { alert missing_dependency "找不到 curl（PATH=${PATH}），herdr 新版偵測停住"; exit 0; }

LOCK="$DIR/herdr-update.lock"
LOCK_STALE_SECS=${AGM_LOCK_STALE_SECS:-120}
LOCK_HUNG_SECS=${AGM_LOCK_HUNG_SECS:-3600}
TMPS=()
TMPS_COUNT=0
cleanup() {
  rm -rf "$LOCK" 2>/dev/null || true
  if [ "$TMPS_COUNT" -gt 0 ]; then rm -f "${TMPS[@]}" 2>/dev/null || true; fi
  true
}
take_lock() { mkdir "$LOCK" 2>/dev/null && { echo "$$ $(date +%s)" > "$LOCK/owner"; trap cleanup EXIT; return 0; }; return 1; }
if ! take_lock; then
  _pid=$(cut -d' ' -f1 "$LOCK/owner" 2>/dev/null)
  _age=$(lock_age)
  if [ -n "$_pid" ] && kill -0 "$_pid" 2>/dev/null && ps -o command= -p "$_pid" 2>/dev/null | grep -q 'herdr-update-kick'; then
    if [ "$_age" -ge "$LOCK_HUNG_SECS" ]; then
      alert runner_hung "上一輪（pid ${_pid}）已經跑了 ${_age} 秒還沒結束，herdr 新版偵測停住。請確認它在做什麼，必要時結束它並移除 ${LOCK}"
    else
      log "已有執行者（pid ${_pid}，${_age} 秒），這輪跳過"
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
    alert stale_lock "殘留鎖 ${LOCK} 清不掉（執行者 ${_pid:-未知} 已不在），herdr 新版偵測停住。請人工確認沒有執行者後移除它"
    exit 0
  fi
fi

INSTALLED_LINE=$(herdr --version 2>/dev/null | head -1)
INSTALLED=$(printf '%s' "$INSTALLED_LINE" | sed -E 's/^[Hh]erdr[[:space:]]*//')
[ -n "$INSTALLED" ] || fail_run "讀不到本機 herdr 版本（herdr --version 沒輸出）"

LATEST_JSON=$(gh release list -R "$HERDR_REPO" --exclude-pre-releases -L 1 --json tagName 2>>"$LOG")
LATEST_TAG=$(printf '%s' "$LATEST_JSON" | python3 -c '
import json,sys
try:
    rows = json.load(sys.stdin)
except Exception:
    sys.exit(1)
if not isinstance(rows, list) or not rows or not isinstance(rows[0], dict):
    sys.exit(1)
tag = rows[0].get("tagName")
if not isinstance(tag, str) or not tag:
    sys.exit(1)
print(tag)
' 2>/dev/null) || LATEST_TAG=""
[ -n "$LATEST_TAG" ] || fail_run "查不到 ${HERDR_REPO} 的最新穩定版（gh release list）"

CHANGELOG=$(mktemp "${TMPDIR:-/tmp}/herdr-changelog.XXXXXX"); TMPS+=("$CHANGELOG"); TMPS_COUNT=$((TMPS_COUNT + 1))
if ! curl -fsSL --max-time 20 "$CHANGELOG_URL" -o "$CHANGELOG" || [ ! -s "$CHANGELOG" ]; then
  fail_run "抓不到 CHANGELOG（${CHANGELOG_URL}）"
fi

LAST_ARGS=()
if [ -f "$STATE" ]; then
  LAST=$(tr -d '[:space:]' < "$STATE")
  [ -n "$LAST" ] && LAST_ARGS=(--last-notified "$LAST")
fi

REPORT=$("$BIN" herdr-update-check --installed "$INSTALLED" --latest "$LATEST_TAG" \
  --changelog-file "$CHANGELOG" ${LAST_ARGS[@]+"${LAST_ARGS[@]}"} 2>>"$LOG") ||
  fail_run "版本比較失敗（installed='${INSTALLED}' latest='${LATEST_TAG}'）"

SHOULD=$(printf '%s' "$REPORT" | python3 -c '
import json,sys
v = json.load(sys.stdin).get("should_notify")
if not isinstance(v, bool):
    sys.exit(1)
print("1" if v else "0")
' 2>/dev/null) || fail_run "看不懂版本比較的報告（${REPORT}）"
[ "$SHOULD" = "1" ] || { ran_ok; exit 0; }

LATEST_VERSION=$(printf '%s' "$REPORT" | python3 -c 'import json,sys; print(json.load(sys.stdin)["latest_version"])')
BRIEF=$(printf '%s' "$REPORT" | python3 -c 'import json,sys; sys.stdout.write(json.load(sys.stdin)["brief"])')
[ -n "$LATEST_VERSION" ] && [ -n "$BRIEF" ] || fail_run "報告缺 latest_version 或 brief，跳過（${REPORT}）"

BOT="${AGM_HERDR_UPDATE_BOT:-}"
if [ -z "$BOT" ] && [ -f "$DIR/runtime.json" ]; then
  BOT=$(python3 -c '
import json,sys
d = json.load(open(sys.argv[1]))
print(d.get("herdr_update_bot_id") or d.get("release_bot_id") or d.get("responder_bot_id") or "")
' "$DIR/runtime.json" 2>/dev/null)
fi
[ -n "$BOT" ] || fail_run "找不到要派給誰（AGM_HERDR_UPDATE_BOT／runtime.json 的 herdr_update_bot_id、release_bot_id 或 responder_bot_id），跳過"

BODY=$(mktemp "${TMPDIR:-/tmp}/agm-herdr-update.XXXXXX"); TMPS+=("$BODY"); TMPS_COUNT=$((TMPS_COUNT + 1))
{
  printf '%s' "$BRIEF"
  printf '\n---\n本機 `herdr --version`：%s ｜ 最新穩定版（gh release list -R %s）：%s\nCHANGELOG：%s\n' \
    "$INSTALLED_LINE" "$HERDR_REPO" "$LATEST_TAG" "$CHANGELOG_URL"
  PLATFORM=$(platform)
  if [ "$PLATFORM" = darwin ]; then
    printf '\n--- 升級後強制驗收 ---\n核准並完成 herdr 維護窗口後，依 scripts/ops/herdr-upgrade-runbook.md：從 herdr pane 跑 `bash scripts/ops/herdr-lan-check.sh /opt/homebrew/bin/herdr`（第一個參數一定要寫真 binary 的路徑；不帶參數在 bot pane 裡會抓到沒簽章的 per-bot shim）。必須用新 binary 的 codesign -dv identifier 查本機網路授權表，再用 node 連區網；Apple 內建 nc、python3、curl 不可代驗。任何失敗或 node 缺少都停下，回報「需要使用者授權本機網路」，不要靠重啟硬試。另在有新 herdr binary 的環境跑 `AM_TEST_REAL_HERDR=1 cargo test -p agents-managerd -- the_generated_argv_parses_with_the_real_herdr`，確認 shim argv 通過新 parser（測試使用暫存 HOME／socket，不連正式 pane）。\n'
  else
    printf '\n--- 升級後驗收 ---\n核准並完成 herdr 維護窗口後，在 herdr pane 確認：`herdr --version` 是 %s、`herdr pane list` 正常、對一顆 idle bot 送一句 prompt 能回。另在有新 herdr binary 的環境跑 `AM_TEST_REAL_HERDR=1 cargo test -p agents-managerd -- the_generated_argv_parses_with_the_real_herdr`，確認 shim argv 通過新 parser（測試使用暫存 HOME／socket，不連正式 pane）。這台是 %s：沒有 macOS 的本機網路授權，不要跑 herdr-lan-check.sh（herdr-upgrade-runbook.md 只適用 macOS 主機）。任何一項失敗就停下回報，不要靠重啟硬試。\n' "$LATEST_VERSION" "$PLATFORM"
  fi
} > "$BODY"

ASSIGN_ERR=$(mktemp "${TMPDIR:-/tmp}/agm-herdr-assign.XXXXXX"); TMPS+=("$ASSIGN_ERR"); TMPS_COUNT=$((TMPS_COUNT + 1))
if "$AGM" --compact assign --bot "$BOT" --review-by patrol --text-file "$BODY" \
     --request-id "agm-herdr-update-$LATEST_VERSION" >> "$LOG" 2>"$ASSIGN_ERR"; then
  cat "$ASSIGN_ERR" >> "$LOG"
  write_state "$LATEST_VERSION" || log "寫不了狀態檔 ${STATE}（已派成功；下一輪同 request-id 由 daemon 去重）"
  ran_ok
  log "herdr ${INSTALLED} → ${LATEST_VERSION}：已派 AGM 解析"
elif cat "$ASSIGN_ERR" >> "$LOG"; python3 -c '
import json,sys
try:
    e = json.load(open(sys.argv[1]))
except Exception:
    sys.exit(1)
d = e.get("detail") if isinstance(e, dict) else None
ok = e.get("status") == 409 and isinstance(d, dict) and bool(d.get("assignment_id") or d.get("inbox_event_id"))
sys.exit(0 if ok else 1)
' "$ASSIGN_ERR" 2>/dev/null; then
  write_state "$LATEST_VERSION" || log "寫不了狀態檔 ${STATE}（這一版已派過）"
  ran_ok
  log "herdr ${INSTALLED} → ${LATEST_VERSION}：同一個 request id 已經派過（更新框的「請 AGM 解析」），不重派"
else
  fail_run "派工失敗（${INSTALLED} → ${LATEST_VERSION}），下一輪再試"
fi
