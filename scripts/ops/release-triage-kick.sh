#!/bin/bash
set -u

SELF="$(cd "$(dirname "$0")" && pwd)/${0##*/}"
PATH="${AGM_EXTRA_PATH-/opt/homebrew/bin:/usr/local/bin}:$PATH"; export PATH

DIR="${AGM_DIR:-${HOME:-/nonexistent}/.config/agents-manager/supervisor/AGM}"
REPO="${AGM_REPO:-${HOME:-/nonexistent}/project/agents-manager}"
BIN="${AM_BINARY:-$REPO/target/release/agents-managerd}"
AGM="$DIR/bin/agm"
LOG="$DIR/release-triage.log"
TASK="$DIR/release-triage-task.md"
CLAUDE_VERSIONS="${CLAUDE_VERSIONS_DIR:-${HOME:-/nonexistent}/.local/share/claude/versions}"
CLAUDE_STATE="$DIR/claude-release.last"
CLAUDE_DIFF_TASK="$DIR/claude-release-diff-task.md"
CLAUDE_DIFF_OLD=""
CLAUDE_DIFF_NEW=""
BINDIFF="$DIR/bin/claude_binary_diff.py"
OWNER="${AM_AGENT_NAME:-release-triage-kick}"
QUOTA_MAX="${AGM_TRIAGE_QUOTA_MAX:-85}"
MAX_VERSIONS=5
MAX_ENTRIES=80
FAILS="$DIR/release-triage.fails"
FAIL_ALERT_AFTER=${AGM_FAIL_ALERT_AFTER:-4}
ROUND_FAIL=""

log() { echo "$(date '+%F %T') $*" >> "$LOG"; }
. "${0%/*}/kick-common.sh" 2>/dev/null || {
  _m="找不到 ${0%/*}/kick-common.sh，${0##*/} 停擺（它要和 kick 一起裝進 bin/）"
  echo "$(date '+%F %T') ALERT kick_common_missing：$_m" >> "$LOG"; echo "$_m" >&2
  _a="${AGM:-$DIR/bin/agm}"
  [ -x "$_a" ] && "$_a" --compact ops-alert --source "${OWNER:-${0##*/}}" --reason kick_common_missing --detail "$_m" >> "$LOG" 2>&1
  exit 1
}

[ -x "$AGM" ] || { log "agm CLI 不在 ${AGM}，跳過"; exit 0; }

write_claude_state() {
  _t="$CLAUDE_STATE.tmp.$$"
  if printf '%s\n' "$1" > "$_t" 2>/dev/null && mv -f "$_t" "$CLAUDE_STATE" 2>/dev/null; then return 0; fi
  rm -f "$_t" 2>/dev/null
  return 1
}
prepare_claude_diff() {
  [ -d "$CLAUDE_VERSIONS" ] || return 0
  _new=$(ls -t "$CLAUDE_VERSIONS" 2>/dev/null | head -1)
  [ -n "$_new" ] || return 0
  _done=""
  [ -f "$CLAUDE_STATE" ] && _done=$(tr -d '[:space:]' < "$CLAUDE_STATE")
  if [ -z "$_done" ]; then
    write_claude_state "$_new" || note_fail "寫不了狀態檔 ${CLAUDE_STATE}，Claude binary diff 基準仍未記錄"
    return 0
  fi
  [ "$_new" != "$_done" ] || return 0
  if [ ! -e "$CLAUDE_VERSIONS/$_done" ] || [ ! -e "$CLAUDE_VERSIONS/$_new" ]; then
    note_fail "Claude binary diff 的舊版或新版不存在（${_done} → ${_new}），保留狀態等下一輪"
    return 0
  fi
  if [ ! -f "$CLAUDE_DIFF_TASK" ]; then
    note_fail "找不到 ${CLAUDE_DIFF_TASK}，Claude binary diff 保留到下一輪"
    return 0
  fi
  CLAUDE_DIFF_OLD="$_done"
  CLAUDE_DIFF_NEW="$_new"
}

bump_fail() {
  _n=$(cat "$FAILS" 2>/dev/null)
  case "$_n" in ''|*[!0-9]*) _n=0 ;; esac
  _n=$((_n + 1))
  echo "$_n" > "$FAILS"
  [ "$_n" -ge "$FAIL_ALERT_AFTER" ] && alert check_failing "上游新版分診連續 ${_n} 輪沒能完成：${1}"
  return 0
}
settle() { if [ -n "$ROUND_FAIL" ]; then bump_fail "$ROUND_FAIL"; else rm -f "$FAILS" 2>/dev/null; fi; }

command -v python3 >/dev/null 2>&1 || { alert missing_dependency "找不到 python3（PATH=${PATH}），上游新版分診停住"; exit 0; }
[ -x "$BIN" ] || { log "找不到 ${BIN}，跳過（需要先建好 release binary）"; bump_fail "找不到 ${BIN}（需要先建好 release binary）"; exit 0; }

LOCK="$DIR/release-triage.lock"
LOCK_GUARD="$DIR/release-triage.lock.guard"
LOCK_STALE_SECS=${AGM_LOCK_STALE_SECS:-120}
LOCK_HUNG_SECS=${AGM_LOCK_HUNG_SECS:-3600}
LOCK_QUIET_SECS=${AGM_LOCK_QUIET_SECS:-60}
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
TMPS=()
TMPS_COUNT=0
cleanup() {
  settle
  rm -rf "$LOCK" 2>/dev/null || true
  if [ "$TMPS_COUNT" -gt 0 ]; then rm -f "${TMPS[@]}" 2>/dev/null || true; fi
  true
}
take_lock() { mkdir "$LOCK" 2>/dev/null && { echo "$$ $(date +%s)" > "$LOCK/owner"; trap cleanup EXIT; return 0; }; return 1; }
mkdir -p "$DIR" 2>/dev/null
_guard_rc=0
acquire_guard || _guard_rc=$?
if [ "$_guard_rc" -eq 1 ]; then
  _pid=$(cut -d' ' -f1 "$LOCK/owner" 2>/dev/null)
  _age=$(lock_age)
  if [ "$_age" -lt 0 ]; then
    log "鎖的時間在未來（${_age} 秒），時鐘倒退或鎖是搬來的，年齡不可信"
  elif [ -n "$_pid" ] && kill -0 "$_pid" 2>/dev/null && is_self_runner "$_pid"; then
    if [ "$_age" -ge "$LOCK_HUNG_SECS" ]; then
      alert runner_hung "上一輪（pid ${_pid}）已經跑了 ${_age} 秒還沒結束，上游新版分診停住。請確認它在做什麼，必要時結束它並移除 ${LOCK}"
    elif [ "$_age" -ge "$LOCK_QUIET_SECS" ]; then
      log "分診已有執行者（pid ${_pid}，${_age} 秒），這輪跳過"
    fi
  elif [ "$_age" -ge "$LOCK_HUNG_SECS" ]; then
    alert runner_hung "release runner 持有 OS lock 已 ${_age} 秒，但鎖的 owner（${_pid:-未知}）無法驗證；分診停住。請確認後必要時結束行程並移除 ${LOCK}"
  elif [ "$_age" -ge "$LOCK_QUIET_SECS" ]; then
    log "另一個 release runner 持有 OS lock（owner 尚未可驗，${_age} 秒），這輪跳過"
  fi
  exit 0
elif [ "$_guard_rc" -ne 0 ]; then
  alert lock_unavailable "無法建立或取得分診鎖（${LOCK_GUARD}），本輪跳過"
  exit 0
fi
if ! take_lock; then
  _pid=$(cut -d' ' -f1 "$LOCK/owner" 2>/dev/null)
  _age=$(lock_age)
  _age_bad=0
  if [ "$_age" -lt 0 ]; then
    _age_bad=1
    log "鎖的時間在未來（${_age} 秒），時鐘倒退或鎖是搬來的，年齡不可信"
  fi
  if [ -n "$_pid" ] && kill -0 "$_pid" 2>/dev/null && is_self_runner "$_pid"; then
    if [ "$_age_bad" = 1 ]; then
      :
    elif [ "$_age" -ge "$LOCK_HUNG_SECS" ]; then
      alert runner_hung "上一輪（pid ${_pid}）已經跑了 ${_age} 秒還沒結束，上游新版分診停住。請確認它在做什麼，必要時結束它並移除 ${LOCK}"
    elif [ "$_age" -ge "$LOCK_QUIET_SECS" ]; then
      log "分診已有執行者（pid ${_pid}，${_age} 秒），這輪跳過"
    fi
    exit 0
  fi
  if [ "$_age_bad" = 0 ] && [ "$_age" -lt "$LOCK_STALE_SECS" ]; then
    log "鎖剛建立（${_age} 秒）但讀不到執行者，這輪跳過"
    exit 0
  fi
  rm -rf "$LOCK" 2>/dev/null
  if take_lock; then
    log "清掉殘留鎖（執行者 ${_pid:-未知} 已不在，鎖存在 ${_age} 秒）並接手這一輪"
  else
    alert stale_lock "殘留鎖 ${LOCK} 清不掉（執行者 ${_pid:-未知} 已不在），上游新版分診停住。請人工確認沒有執行者後移除它"
    exit 0
  fi
fi

BOT="${AGM_RELEASE_BOT:-}"
if [ -z "$BOT" ] && [ -f "$DIR/runtime.json" ]; then
  BOT=$(python3 -c '
import json,sys
d = json.load(open(sys.argv[1]))
print(d.get("release_bot_id") or d.get("responder_bot_id") or "")
' "$DIR/runtime.json" 2>/dev/null)
fi

ISSUE_BOT="${AGM_RELEASE_BOT:-}"
if [ -z "$ISSUE_BOT" ] && [ -f "$DIR/runtime.json" ]; then
  ISSUE_BOT=$(python3 -c 'import json,sys; print(json.load(open(sys.argv[1])).get("responder_bot_id") or "")' "$DIR/runtime.json" 2>/dev/null)
fi

prepare_claude_diff

NO_PUBLISH_NOTE="$DIR/release-triage-publish-unsupported"
for KIND in claude codex agy grok; do
  OUT=$("$AGM" --compact release-triage publish --kind "$KIND" 2>&1); RC=$?
  if [ "$RC" -eq 2 ]; then
    [ -e "$NO_PUBLISH_NOTE" ] || { log "bin/agm 不認得 release-triage（舊版，exit 2），略過 publish 重試；換新 agm 後才會有效"; : > "$NO_PUBLISH_NOTE"; }
    break
  elif [ "$RC" -ne 0 ]; then
    note_fail "release-triage publish --kind ${KIND} 失敗（rc=${RC}）：$(printf '%s' "$OUT" | head -c 200)"
    continue
  fi
  rm -f "$NO_PUBLISH_NOTE" 2>/dev/null
  printf '%s' "$OUT" | KIND="$KIND" python3 -c '
import json, os, sys
try:
    res = json.load(sys.stdin).get("results") or []
except ValueError:
    sys.exit(0)
for r in res:
    x = r.get("result") or {}
    if x.get("outcome") in ("published", "failed"):
        print("%s %s：publish %s%s" % (r.get("kind") or os.environ["KIND"], r.get("version"), x["outcome"],
                                       "（" + str(x["error"])[:200] + "）" if x.get("error") else ""))
' 2>/dev/null | while IFS= read -r _l; do log "$_l"; done
done

ISSUE_TASK="$DIR/release-issue-task.md"
HANDED="$DIR/release-issue-handed"
SKIPPED="$DIR/release-issue-skipped"
GH="${GH_BIN:-gh}"
handoff_issues() {
  local KIND
for KIND in "$@"; do
  if ! SHOW=$("$AGM" --compact release-triage show --kind "$KIND" 2>/dev/null); then
    note_fail "release-triage show --kind ${KIND} 失敗，尚未交接的 issue 留待下一輪"
    continue
  fi
  TODO=$(printf '%s' "$SHOW" | HANDED="$HANDED" SKIPPED="$SKIPPED" python3 -c '
import json, os, sys
try:
    data = json.load(sys.stdin)
except (ValueError, TypeError):
    sys.exit(1)
if not isinstance(data, dict) or not isinstance(data.get("rows"), list):
    sys.exit(1)
rows = data["rows"]
done = set()
for key in ("HANDED", "SKIPPED"):
    try:
        done.update(open(os.environ[key]).read().split())
    except OSError:
        pass
for r in rows:
    if not isinstance(r, dict) or not isinstance(r.get("issues", []), list):
        sys.exit(1)
    for i in r.get("issues") or []:
        if not isinstance(i, dict):
            sys.exit(1)
        if i.get("comment") or not i.get("number") or str(i["number"]) in done:
            continue
        ref = i.get("url") or ("https://github.com/%s/issues/%s" % (data["repo"], i["number"]) if data.get("repo") else "-")
        print("ready\t%s\t%s\t%s\t%s" % (i["number"], ref, r.get("kind") or "", r.get("version") or ""))
' 2>/dev/null)
  if [ "$?" -ne 0 ]; then
    note_fail "release-triage show --kind ${KIND} 回傳資料無法解析，尚未交接的 issue 留待下一輪"
    continue
  fi
  [ -n "$TODO" ] || continue
  if [ -z "$ISSUE_BOT" ]; then
    note_fail "release-triage ${KIND} 有尚未交接的 issue，但找不到收件 bot（AGM_RELEASE_BOT／runtime.json 的 responder_bot_id）"
    continue
  fi
  if [ ! -f "$ISSUE_TASK" ]; then
    note_fail "release-triage ${KIND} 有尚未交接的 issue，但找不到 ${ISSUE_TASK}"
    continue
  fi
  while IFS=$'\t' read -r _state _num _url _kind _ver; do
    if [ "$_state" != ready ]; then
      note_fail "release-triage show --kind ${KIND} 產生無法識別的交接列，留待下一輪"
      continue
    fi
    [ -n "$_num" ] || continue
    if [ "$_url" = "-" ]; then
      note_fail "issue #${_num} 缺網址與 repo，保留待重試"
      continue
    fi
    if ! _issue_state=$(python3 -c 'import subprocess,sys; subprocess.run(sys.argv[1:], check=True, timeout=40)' "$GH" issue view "$_url" --json state --jq .state 2>>"$LOG"); then
      note_fail "查詢 issue #${_num} 狀態失敗，保留待重試"
      continue
    fi
    case "$_issue_state" in
      CLOSED)
        if printf '%s\n' "$_num" >> "$SKIPPED"; then
          log "issue #${_num} 已關閉，持久略過"
        else
          note_fail "issue #${_num} 已關閉但略過紀錄寫入失敗，保留待重試"
        fi
        continue ;;
      OPEN) ;;
      *) note_fail "issue #${_num} 狀態不明，保留待重試"; continue ;;
    esac
    _body=$(mktemp "${TMPDIR:-/tmp}/agm-release-issue.XXXXXX")
    { cat "$ISSUE_TASK"; printf '\n\n---\n本次：%s %s 分診開的 issue #%s %s\n' "$_kind" "$_ver" "$_num" "$_url"; } > "$_body"
    if "$AGM" --compact assign --bot "$ISSUE_BOT" --review-by patrol --text-file "$_body" \
         --request-id "release-issue-${_num}" >> "$LOG" 2>&1; then
      if printf '%s\n' "$_num" >> "$HANDED"; then
        log "${_kind} ${_ver}：issue #${_num} 已交給 ${ISSUE_BOT} 接手"
      else
        note_fail "issue #${_num} 已送出但寫不了交接狀態 ${HANDED}，下一輪沿用同 request-id 對帳重試"
      fi
    else
      note_fail "issue #${_num} 交辦失敗（${_kind} ${_ver}），下一輪再試"
    fi
    rm -f "$_body"
  done <<< "$TODO"
done
}
handoff_issues claude codex agy grok

QUOTA_STATE=""
quota_gate() {
  if [ -z "$QUOTA_STATE" ]; then
    _q=$("$AGM" --compact quota 2>/dev/null) || _q=""
    _s=$("$AGM" --compact state 2>/dev/null) || _s=""
    _r=$(QUOTA_JSON="$_q" STATE_JSON="$_s" TARGET_BOT="$BOT" QUOTA_MAX="$QUOTA_MAX" python3 -c '
import json, os
try:
    quota = json.loads(os.environ["QUOTA_JSON"]).get("kinds") or {}
    bots = json.loads(os.environ["STATE_JSON"]).get("bots") or []
    bot = next(b for b in bots if b.get("id") == os.environ["TARGET_BOT"])
    kind, ident = bot.get("kind") or "", bot.get("identity") or ""
    cell = quota.get(f"{kind}:{ident}") if ident else None
    if cell is None:
        cell = quota.get(kind)
    if not isinstance(cell, dict):
        raise LookupError("no cell")
    if cell.get("limit_hit"):
        print("blocked limit_hit")
    else:
        used = float((cell.get("five_hour") or {})["used_pct"])
        print(("blocked" if used >= float(os.environ["QUOTA_MAX"]) else "ok") + f" 5h={used:g}%")
except Exception:
    print("unknown")
' 2>/dev/null) || _r="unknown"
    case "$_r" in
      blocked*) QUOTA_STATE=blocked; log "額度閘門擋下（bot ${BOT}：${_r#blocked }，門檻 ${QUOTA_MAX}%），pending 留到下一輪" ;;
      ok*)      QUOTA_STATE=ok ;;
      *)        QUOTA_STATE=ok; log "查不到 bot ${BOT} 的額度，照派" ;;
    esac
  fi
  [ "$QUOTA_STATE" = ok ]
}

api_triage() {
  _en=$("$AGM" --compact release-triage auto --kind "$KIND" --check 2>>"$LOG") || { log "查不到 API 模式，這輪不派協調者"; return 3; }
  _enabled=$(printf '%s' "$_en" | python3 -c 'import json,sys; v=json.load(sys.stdin)["enabled"]; assert isinstance(v,bool); print(1 if v else 0)' 2>/dev/null) || return 3
  [ "$_enabled" = 1 ] || return 0
  _vargs=(); _vcount=0
  while IFS= read -r _v; do
    [ -n "$_v" ] || continue
    _vargs+=(--version "$_v"); _vcount=$((_vcount + 1))
  done <<< "$(printf '%s' "$PAYLOAD" | python3 -c 'import json,sys
for p in json.load(sys.stdin)["pending"]: print(p["version"])' 2>/dev/null)"
  [ "$_vcount" -gt 0 ] || return 3
  _dargs=()
  if [ "$KIND" = claude ] && [ -n "$CLAUDE_DIFF_NEW" ] && [ -f "$BINDIFF" ]; then
    _dfile=$(mktemp "${TMPDIR:-/tmp}/agm-binary-diff.XXXXXX"); TMPS+=("$_dfile"); TMPS_COUNT=$((TMPS_COUNT + 1))
    if python3 "$BINDIFF" "$CLAUDE_VERSIONS/$CLAUDE_DIFF_OLD" "$CLAUDE_VERSIONS/$CLAUDE_DIFF_NEW" --timeout 120 > "$_dfile" 2>>"$LOG" && [ -s "$_dfile" ]; then
      _dargs=(--binary-diff-file "$_dfile")
    else
      log "binary diff 腳本沒產出報告（${CLAUDE_DIFF_OLD} → ${CLAUDE_DIFF_NEW}），binary 留給派協調者的路徑"
    fi
  fi
  _out=$("$AGM" --compact release-triage auto --kind "$KIND" "${_vargs[@]}" ${_dargs[@]+"${_dargs[@]}"} 2>>"$LOG") || {
    log "release-triage auto --kind ${KIND} 失敗，API 模式不派協調者"
    return 3
  }
  _rest=$(printf '%s' "$_out" | AUTO_OUT="$_out" PAYLOAD="$PAYLOAD" TO="$TO" python3 -c '
import json, os, sys
try:
    res = json.load(sys.stdin)["results"]
    payload = json.loads(os.environ["PAYLOAD"])
except Exception as exc:
    print("cannot read auto result: %s" % exc, file=sys.stderr)
    sys.exit(1)
done = {r["version"] for r in res if r.get("outcome") in ("judged", "empty")}
for r in res:
    if r.get("outcome") in ("fallback", "failed") and not r.get("quiet"):
        print("API 分診失敗 %s：%s" % (r.get("version"), str(r.get("reason"))[:200]), file=sys.stderr)
    elif r.get("outcome") in ("judged", "empty"):
        print("API 分診完成 %s（%s）%s" % (r.get("version"), r.get("status"), "：" + str(r["reason"])[:200] if r.get("reason") else ""), file=sys.stderr)
pend = [p for p in payload["pending"] if p["version"] not in done]
to = os.environ["TO"]
if to in done:
    to = pend[-1]["version"] if pend else ""
payload["pending"] = pend
payload["to"] = to or payload.get("to")
print("%d %d" % (len(pend), 1 if json.loads(os.environ["AUTO_OUT"]).get("binary_used") else 0))
print(to)
print(json.dumps(payload, ensure_ascii=False, indent=2))
' 2>>"$LOG") || return 3
  _head=$(printf '%s\n' "$_rest" | sed -n 1p)
  _left=${_head%% *}; _bin=${_head##* }
  if [ "$_bin" = 1 ]; then
    if write_claude_state "$CLAUDE_DIFF_NEW"; then
      log "claude → ${CLAUDE_DIFF_NEW}：binary diff 已併進 API 分診的通知"
      CLAUDE_DIFF_NEW=""; CLAUDE_DIFF_OLD=""
    else
      log "寫不了狀態檔 ${CLAUDE_STATE}（binary diff 已併進 API 通知；下一輪會再比一次）"
    fi
  fi
  if [ "$_left" = 0 ]; then return 3; fi
  TO=$(printf '%s\n' "$_rest" | sed -n 2p)
  PAYLOAD=$(printf '%s\n' "$_rest" | sed 1,2d)
  return 3
}

for KIND in claude codex agy grok; do
  BINARY_ONLY=0
  REPORT=$("$BIN" release-triage-check --kind "$KIND" --json 2>>"$LOG") || {
    note_fail "release-triage-check --kind ${KIND} 失敗，這輪跳過 changelog 派工"
    if [ "$KIND" != claude ] || [ -z "$CLAUDE_DIFF_NEW" ]; then continue; fi
    BINARY_ONLY=1
    TO="$CLAUDE_DIFF_NEW"
    PAYLOAD=$(printf '{"kind":"claude","from":"%s","to":"%s","pending":[]}' "$CLAUDE_DIFF_OLD" "$CLAUDE_DIFF_NEW")
  }
  if [ "$BINARY_ONLY" -eq 0 ]; then
  PICK=$(printf '%s' "$REPORT" | MAXV="$MAX_VERSIONS" MAXE="$MAX_ENTRIES" python3 -c '
import json, os, sys
d = json.load(sys.stdin)
pend = [p for p in (d.get("pending") or []) if isinstance(p, dict)]
if not pend:
    sys.exit(3)
maxv, maxe = int(os.environ["MAXV"]), int(os.environ["MAXE"])
picked, total = [], 0
for p in pend:
    n = len(p.get("kept") or []) + len(p.get("unmatched") or [])
    if picked and (len(picked) >= maxv or total + n > maxe):
        break
    picked.append(p); total += n
to = d.get("to") if len(picked) == len(pend) else picked[-1].get("version")
if not to:
    sys.exit(4)
out = {k: d.get(k) for k in ("kind", "from", "to")}
out["pending"] = picked
out["deferred_versions"] = [p.get("version") for p in pend[len(picked):]]
print(to)
print(json.dumps(out, ensure_ascii=False, indent=2))
' 2>>"$LOG"); RC=$?
  case $RC in
    0) ;;
    3)
      if [ "$KIND" = claude ] && [ -n "$CLAUDE_DIFF_NEW" ]; then
        BINARY_ONLY=1
        TO="$CLAUDE_DIFF_NEW"
        PAYLOAD=$(printf '{"kind":"claude","from":"%s","to":"%s","pending":[]}' "$CLAUDE_DIFF_OLD" "$CLAUDE_DIFF_NEW")
      else
        continue
      fi
      ;;
    *)
      note_fail "release-triage-check --kind ${KIND} 的輸出看不懂（rc=${RC}），這輪跳過"
      if [ "$KIND" = claude ] && [ -n "$CLAUDE_DIFF_NEW" ]; then
        BINARY_ONLY=1
        TO="$CLAUDE_DIFF_NEW"
        PAYLOAD=$(printf '{"kind":"claude","from":"%s","to":"%s","pending":[]}' "$CLAUDE_DIFF_OLD" "$CLAUDE_DIFF_NEW")
      else
        continue
      fi
      ;;
  esac
  if [ "$BINARY_ONLY" -eq 0 ]; then
    TO=$(printf '%s\n' "$PICK" | head -1)
    PAYLOAD=$(printf '%s\n' "$PICK" | sed 1d)
  fi
  fi

  if [ "$BINARY_ONLY" -eq 0 ]; then
    api_triage; _api_rc=$?
    if [ "$_api_rc" -eq 3 ]; then
      handoff_issues "$KIND"
      continue
    fi
  else
    _mode=$("$AGM" --compact release-triage auto --kind "$KIND" --check 2>>"$LOG") || continue
    _enabled=$(printf '%s' "$_mode" | python3 -c 'import json,sys; print(1 if json.load(sys.stdin).get("enabled") else 0)' 2>/dev/null) || continue
    if [ "$_enabled" = 1 ]; then
      log "API 模式缺 changelog，跳過 binary-only 協調者派工"
      continue
    fi
  fi

  [ -n "$BOT" ] || { note_fail "找不到要派給誰（AGM_RELEASE_BOT／runtime.json 的 release_bot_id 或 responder_bot_id），跳過"; exit 0; }
  [ -f "$TASK" ] || { note_fail "找不到 ${TASK}，跳過 AGM 分診"; continue; }
  quota_gate || exit 0

  BODY=$(mktemp "${TMPDIR:-/tmp}/agm-release-triage.XXXXXX"); TMPS+=("$BODY"); TMPS_COUNT=$((TMPS_COUNT + 1))
  NOTICE_ID="agm-release-triage-${KIND}-${TO}-notice"
  if [ "$BINARY_ONLY" -eq 1 ]; then
    NOTICE_ID="agm-claude-release-${TO}-notice"
  fi
  {
    cat "$TASK"
    if [ "$BINARY_ONLY" -eq 1 ]; then
      printf '\n\n---\n本輪沒有待分診的 changelog 版本；以下 JSON 的 pending 為空，只需完成 binary diff 並在同一則通知回報。\n\n```json\n%s\n```\n' "$PAYLOAD"
    else
      printf '\n\n---\n本次：kind=%s，%s 版待分診（新版 %s）。以下 JSON 是 `agents-managerd release-triage-check` 的輸出，逐條 verdict 用它的 `id`。\n\n```json\n%s\n```\n' \
        "$KIND" "$(printf '%s' "$PAYLOAD" | python3 -c 'import json,sys; print(len(json.load(sys.stdin)["pending"]))')" "$TO" "$PAYLOAD"
    fi
    if [ "$KIND" = claude ] && [ -n "$CLAUDE_DIFF_NEW" ]; then
      cat "$CLAUDE_DIFF_TASK"
      printf '\n\n---\nClaude binary diff：OLD=%s/%s\nNEW=%s/%s\n' "$CLAUDE_VERSIONS" "$CLAUDE_DIFF_OLD" "$CLAUDE_VERSIONS" "$CLAUDE_DIFF_NEW"
    fi
    printf '\n\n通知 request-id（child 需原樣使用）：%s\n' "$NOTICE_ID"
  } > "$BODY"

  SHOW_ASSIGN='{"rows":[]}'
  if [ "$BINARY_ONLY" -eq 0 ]; then
    SHOW_ASSIGN=$("$AGM" --compact release-triage show --kind "$KIND" 2>>"$LOG") || {
      note_fail "讀不到 ${KIND} 帳本目前代號，這輪不派工"
      continue
    }
  fi
  ASSIGN_ID=$(printf '%s' "$SHOW_ASSIGN" | PAYLOAD="$PAYLOAD" KIND="$KIND" TO="$TO" BINARY_ONLY="$BINARY_ONLY" \
    BINARY_OLD="$CLAUDE_DIFF_OLD" BINARY_NEW="$CLAUDE_DIFF_NEW" python3 -c '
import hashlib, json, os, sys
try:
    payload = json.loads(os.environ["PAYLOAD"])
    kind, to = os.environ["KIND"], os.environ["TO"]
    if os.environ["BINARY_ONLY"] == "1":
        identity = {"kind": kind, "to": to, "binary_old": os.environ.get("BINARY_OLD", ""),
                    "binary_new": os.environ.get("BINARY_NEW", "")}
        prefix = f"release-triage-claude-binary-{to}"
    else:
        rows = json.load(sys.stdin).get("rows")
        if not isinstance(rows, list):
            raise ValueError("show rows missing")
        generations = []
        for pending in payload["pending"]:
            version = pending["version"]
            matches = [row for row in rows if row.get("kind") == kind and row.get("version") == version]
            if len(matches) != 1 or matches[0].get("status") != "pending":
                raise ValueError(f"{version} is not exactly one pending row")
            generation = matches[0].get("dispatch_gen")
            if isinstance(generation, bool) or not isinstance(generation, int) or generation < 0:
                raise ValueError(f"{version} has invalid dispatch_gen")
            generations.append({"version": version, "dispatch_gen": generation + 1})
        identity = {"kind": kind, "to": to, "versions": generations}
        prefix = f"release-triage-{kind}-{to}"
    digest = hashlib.sha256(json.dumps(identity, sort_keys=True, separators=(",", ":"),
                              ensure_ascii=False).encode()).hexdigest()[:20]
    print(f"{prefix}-{digest}")
except Exception as exc:
    print(f"cannot build assignment request-id: {exc}", file=sys.stderr)
    sys.exit(1)
  ' 2>>"$LOG") || {
    note_fail "讀不到 ${KIND} 帳本目前代號，這輪不派工"
    continue
  }
  if "$AGM" --compact assign --bot "$BOT" --review-by patrol --text-file "$BODY" \
       --request-id "$ASSIGN_ID" >> "$LOG" 2>&1; then
    log "${KIND} → ${TO}：已派 ${BOT} 分診"
    VERS=$(printf '%s' "$PAYLOAD" | python3 -c '
import json, sys
for p in json.load(sys.stdin)["pending"]:
    print(p["version"])
' 2>>"$LOG") || VERS=""
    VARGS=(); VARGS_COUNT=0
    while IFS= read -r _v; do
      [ -n "$_v" ] || continue
      VARGS+=(--version "$_v")
      VARGS_COUNT=$((VARGS_COUNT + 1))
    done <<< "$VERS"
    DISPATCHED_OK=1
    if [ "$VARGS_COUNT" -gt 0 ]; then
      if "$AGM" --compact release-triage dispatched --kind "$KIND" "${VARGS[@]}" --bot "$BOT" >> "$LOG" 2>&1; then
        :
      else
        DISPATCHED_OK=0
        log "標記 dispatched 失敗（${KIND} → ${TO}），帳本仍是 pending；不重派"
      fi
    fi
    if [ "$KIND" = claude ] && [ -n "$CLAUDE_DIFF_NEW" ] && [ "$DISPATCHED_OK" -eq 1 ]; then
      write_claude_state "$CLAUDE_DIFF_NEW" || log "寫不了狀態檔 ${CLAUDE_STATE}（binary diff 已派成功；下一輪 request-id 會去重）"
    fi
  else
    note_fail "派工失敗（${KIND} → ${TO}），下一輪再試"
  fi
done
exit 0
