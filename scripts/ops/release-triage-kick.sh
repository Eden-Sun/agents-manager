#!/bin/bash
# 上游新版分診（issue #204）：claude／codex 每出一版，把「該採用或該提防」的 changelog 條目派給專責 bot 判斷。
# launchd `com.agm.release-triage` 每 30 分鐘跑一次；唯讀，只派工，**不升級、不改設定、不重啟**。
#
# 版本比較、切 changelog、分桶（dropped／kept／unmatched）全部交給
# `agents-managerd release-triage-check --kind <k> --json`；這支腳本不做版本比較也不切段
# （同 herdr-update-kick.sh 的原則），只照回來的 JSON 決定要不要派、派給誰、額度夠不夠。
# 輸出契約：{"kind","from","to","pending":[{"version","kept":[{"id","text","categories":[]}],
#            "unmatched":[{"id","text"}],"dropped_count"}]}；pending 是空的就安靜結束。
#
# 補的兩個 #66 留言的洞：殘留鎖永久停擺（鎖裡寫 pid＋時間，執行者不在就回收）、launchd PATH 找不到依賴
# （開頭自補 PATH；缺依賴不靜默，log＋ops_alert）。
#
#   AGM_DIR、AGM_REPO、AM_BINARY、AGM_RELEASE_BOT、AGM_TRIAGE_QUOTA_MAX、AGM_LOCK_STALE_SECS、
#   AGM_LOCK_HUNG_SECS、AGM_LOCK_QUIET_SECS、AGM_FAIL_ALERT_AFTER 可覆寫（測試用）。
set -u

SELF="$(cd "$(dirname "$0")" && pwd)/${0##*/}"
PATH="${AGM_EXTRA_PATH-/opt/homebrew/bin:/usr/local/bin}:$PATH"; export PATH   # AGM_EXTRA_PATH 只給測試蓋掉

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
OWNER="${AM_AGENT_NAME:-release-triage-kick}"
QUOTA_MAX="${AGM_TRIAGE_QUOTA_MAX:-85}"
MAX_VERSIONS=5      # 一則交辦最多帶幾版
MAX_ENTRIES=80      # 一則交辦 kept+unmatched 合計上限；超過的留給下一輪
FAILS="$DIR/release-triage.fails"             # 連續幾輪沒能完成（完成一輪就清掉）
FAIL_ALERT_AFTER=${AGM_FAIL_ALERT_AFTER:-4}   # 每 30 分鐘一輪＝連續約 2 小時
ROUND_FAIL=""                                 # 這一輪有沒有出過「沒能完成」的事（收尾時統一結算）

log() { echo "$(date '+%F %T') $*" >> "$LOG"; }

[ -x "$AGM" ] || { log "agm CLI 不在 ${AGM}，跳過"; exit 0; }

# 卡住了、自己解不開時喊人：一則 durable inbox 事件（同 source+reason 每小時一則，daemon 去重）。
alert() { # alert <reason> <detail>
  log "ALERT ${1}：${2}"
  "$AGM" --compact ops-alert --source "$OWNER" --reason "$1" --detail "$2" >> "$LOG" 2>&1 ||
    log "推 ops-alert 失敗（舊 CLI 或 daemon 不在），只留在這份 log"
}

# Claude binary diff 曾由 claude-release-kick 獨立派工。現在只由這支 kick 把它附進 changelog
# 分診交辦；版本狀態仍沿用 claude-release.last，派工成功後才推進。
write_claude_state() { # write_claude_state <version>
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

# 連續沒能完成（check／assign／publish 失敗、找不到派給誰、binary 不在）不能永遠只有 local log：
# 一次抖動不吵人，連續 FAIL_ALERT_AFTER 輪推 ops_alert，完成一輪清零（同 herdr-update-kick.sh）。
bump_fail() { # bump_fail <訊息>
  _n=$(cat "$FAILS" 2>/dev/null)
  case "$_n" in ''|*[!0-9]*) _n=0 ;; esac
  _n=$((_n + 1))
  echo "$_n" > "$FAILS"
  [ "$_n" -ge "$FAIL_ALERT_AFTER" ] && alert check_failing "上游新版分診連續 ${_n} 輪沒能完成：${1}"
  return 0
}
note_fail() { ROUND_FAIL="${ROUND_FAIL:-$1}"; log "$1"; }   # 記 log，並讓收尾把這一輪算成失敗
settle() { if [ -n "$ROUND_FAIL" ]; then bump_fail "$ROUND_FAIL"; else rm -f "$FAILS" 2>/dev/null; fi; }

# launchd 的預設 PATH 不含 Homebrew；缺依賴不能只靜默 exit 0，否則「已排程」但永遠不做（#66 留言）。
command -v python3 >/dev/null 2>&1 || { alert missing_dependency "找不到 python3（PATH=${PATH}），上游新版分診停住"; exit 0; }
[ -f "$TASK" ] || { log "找不到 ${TASK}，跳過"; exit 0; }
[ -x "$BIN" ] || { log "找不到 ${BIN}，跳過（需要先建好 release binary）"; bump_fail "找不到 ${BIN}（需要先建好 release binary）"; exit 0; }

# 鎖：兩個執行者同時派會送出重複交辦。鎖裡寫 pid 與時間（抄 daemon-update-kick.sh 的格式）：
# SIGKILL／斷電那一輪 EXIT trap 沒跑，鎖會留在磁碟上——執行者不在就回收接手；還活著但卡太久才喊人。
LOCK="$DIR/release-triage.lock"
LOCK_GUARD="$DIR/release-triage.lock.guard"
LOCK_STALE_SECS=${AGM_LOCK_STALE_SECS:-120}    # 沒有 pid 可查時，超過這麼久就算殘留
LOCK_HUNG_SECS=${AGM_LOCK_HUNG_SECS:-3600}     # 執行者還活著但卡了這麼久：喊人
# 執行者剛拿到鎖不到這麼久就撞上＝同一秒內兩個排程一起到點（com.agm.release-triage 與相容入口
# claude-release-kick.sh 的 com.agm.claude-release 兩個 timer，exec 進同一支腳本）：正常重疊，照擋不派，不寫 log
# （原本每個 tick 都洗一行「已有執行者（0 秒）」）。超過這個時間還在跑才記，卡太久仍照舊喊人。
LOCK_QUIET_SECS=${AGM_LOCK_QUIET_SECS:-60}
lock_age() { # lock_age → 鎖建立到現在幾秒（讀不到就當 0）
  _born=$(python3 -c '
import os,sys
try:
    print(int(os.path.getmtime(sys.argv[1])))
except OSError:
    print(0)
' "$LOCK" 2>/dev/null) || _born=0
  case "$_born" in ''|*[!0-9]*) _born=0 ;; esac
  [ "$_born" = 0 ] && { echo 0; return; }
  echo $(( $(date +%s) - _born ))
}
is_self_runner() { # is_self_runner <pid>：只認真正以這支腳本為 bash/sh 入口的行程
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
  exec 9>"$LOCK_GUARD" 2>/dev/null || return 2
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
  # 鎖的 mtime 在未來（時鐘倒退、鎖是搬來的）：年齡是負的，不可信。不能掉進「小於安靜門檻」而永遠安靜跳過
  # （runner_hung 要等時間追上才推），執行者已死的鎖也不能卡到那時才回收：記一行，死鎖照回收。
  _age_bad=0
  if [ "$_age" -lt 0 ]; then
    _age_bad=1
    log "鎖的時間在未來（${_age} 秒），時鐘倒退或鎖是搬來的，年齡不可信"
  fi
  if [ -n "$_pid" ] && kill -0 "$_pid" 2>/dev/null && is_self_runner "$_pid"; then
    if [ "$_age_bad" = 1 ]; then
      :   # 上面已記 log；執行者還活著，不搶、不派
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

# 派給誰：明指的 AGM_RELEASE_BOT ＞ runtime.json 的 release_bot_id（專用 child）＞ responder_bot_id（協調者）。
# **不能是巡檢自己**——daemon 擋「總管對自己下交辦」（同 claude-release-kick.sh）；查不到就跳過，不亂派。
BOT="${AGM_RELEASE_BOT:-}"
if [ -z "$BOT" ] && [ -f "$DIR/runtime.json" ]; then
  BOT=$(python3 -c '
import json,sys
d = json.load(open(sys.argv[1]))
print(d.get("release_bot_id") or d.get("responder_bot_id") or "")
' "$DIR/runtime.json" 2>/dev/null)
fi

prepare_claude_diff

# 重試 gh 失敗、停在 judged 的版本（daemon 端刻意不做定時器，靠這裡每輪叫一次）。與有沒有 pending、額度夠不夠無關，
# 所以放在派工迴圈之前；沒東西要重試（results 空、或只有 disabled／deferred）時安靜，不寫 log。
# 舊的 bin/agm 沒有 release-triage 子命令（argparse exit 2）：只講一次，用 state 檔記「已經講過」，不每 30 分鐘洗 log。
NO_PUBLISH_NOTE="$DIR/release-triage-publish-unsupported"
for KIND in claude codex; do
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

# 開出去的 issue 交給 AGM 接手實作（2026-10-03 使用者：「changelog 會自動解析，應該直接開對應 issue 來接」）。
# 帳本裡每張**新開**的 issue（`comment`＝只在既有 issue 留言的不算）交辦一次：request-id `release-issue-<編號>` 由 daemon 去重，
# 交辦成功才記進狀態檔，下一輪不再送；只看 14 天內開的，舊帳不翻。派給同一顆分診 bot（協調者／巡檢），由它照平常流程派 child。
ISSUE_TASK="$DIR/release-issue-task.md"
HANDED="$DIR/release-issue-handed"
for KIND in claude codex; do
  if ! SHOW=$("$AGM" --compact release-triage show --kind "$KIND" 2>/dev/null); then
    note_fail "release-triage show --kind ${KIND} 失敗，尚未交接的 issue 留待下一輪"
    continue
  fi
  TODO=$(printf '%s' "$SHOW" | HANDED="$HANDED" python3 -c '
import json, os, sys, datetime
try:
    data = json.load(sys.stdin)
except (ValueError, TypeError):
    sys.exit(1)
if not isinstance(data, dict) or not isinstance(data.get("rows"), list):
    sys.exit(1)
rows = data["rows"]
try:
    done = set(open(os.environ["HANDED"]).read().split())
except OSError:
    done = set()
cutoff = datetime.datetime.now(datetime.timezone.utc) - datetime.timedelta(days=14)
for r in rows:
    if not isinstance(r, dict) or not isinstance(r.get("issues", []), list):
        sys.exit(1)
    for i in r.get("issues") or []:
        if not isinstance(i, dict):
            sys.exit(1)
        if i.get("comment") or not i.get("number") or str(i["number"]) in done:
            continue
        try:
            at = datetime.datetime.fromisoformat(str(i.get("created_at", "")).replace("Z", "+00:00"))
        except ValueError:
            print("invalid_timestamp\t%s\t%s\t%s\t%s" % (i["number"], i.get("url") or "", r.get("kind") or "", r.get("version") or ""))
            continue
        if at.tzinfo is None or at.utcoffset() is None:
            print("invalid_timestamp\t%s\t%s\t%s\t%s" % (i["number"], i.get("url") or "", r.get("kind") or "", r.get("version") or ""))
            continue
        if at < cutoff:
            continue
        print("ready\t%s\t%s\t%s\t%s" % (i["number"], i.get("url") or "", r.get("kind") or "", r.get("version") or ""))
' 2>/dev/null)
  if [ "$?" -ne 0 ]; then
    note_fail "release-triage show --kind ${KIND} 回傳資料無法解析，尚未交接的 issue 留待下一輪"
    continue
  fi
  [ -n "$TODO" ] || continue
  if [ -z "$BOT" ]; then
    note_fail "release-triage ${KIND} 有尚未交接的 issue，但找不到收件 bot（AGM_RELEASE_BOT／runtime.json）"
    continue
  fi
  if [ ! -f "$ISSUE_TASK" ]; then
    note_fail "release-triage ${KIND} 有尚未交接的 issue，但找不到 ${ISSUE_TASK}"
    continue
  fi
  # 不接在管線後面：管線裡的 while 是 subshell，`note_fail` 設的 ROUND_FAIL 會丟掉。
  while IFS=$'\t' read -r _state _num _url _kind _ver; do
    if [ "$_state" = invalid_timestamp ]; then
      note_fail "issue #${_num} 的建立時間格式錯誤或沒有時區（${_kind} ${_ver}），留待修正後重試"
      continue
    fi
    if [ "$_state" != ready ]; then
      note_fail "release-triage show --kind ${KIND} 產生無法識別的交接列，留待下一輪"
      continue
    fi
    [ -n "$_num" ] || continue
    _body=$(mktemp "${TMPDIR:-/tmp}/agm-release-issue.XXXXXX")
    { cat "$ISSUE_TASK"; printf '\n\n---\n本次：%s %s 分診開的 issue #%s %s\n' "$_kind" "$_ver" "$_num" "$_url"; } > "$_body"
    if "$AGM" --compact assign --bot "$BOT" --review-by patrol --text-file "$_body" \
         --request-id "release-issue-${_num}" >> "$LOG" 2>&1; then
      if printf '%s\n' "$_num" >> "$HANDED"; then
        log "${_kind} ${_ver}：issue #${_num} 已交給 ${BOT} 接手"
      else
        note_fail "issue #${_num} 已送出但寫不了交接狀態 ${HANDED}，下一輪沿用同 request-id 對帳重試"
      fi
    else
      note_fail "issue #${_num} 交辦失敗（${_kind} ${_ver}），下一輪再試"
    fi
    rm -f "$_body"
  done <<< "$TODO"
done

# 額度閘門（issue #204 §3）：該 bot 身分的 5h ≥ 門檻或有 limit_hit → 不派，列維持 pending，下一輪再看。
# 查不到（端點壞、找不到這顆 bot 的額度格）照派並記 log，不要因為端點壞了就永遠不做。
# 只在真的有 pending 時才查（沒事的輪次不打 API），且一輪只查一次（兩個 kind 派給同一顆 bot）。
QUOTA_STATE=""   # ""＝還沒查；ok／blocked
quota_gate() { # → 0 可派、1 擋下
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

for KIND in claude codex; do
  # 抓不到／CLI 壞掉就是這輪不做這個 kind，不當成「沒有新版」；另一個 kind 照跑。
  BINARY_ONLY=0
  REPORT=$("$BIN" release-triage-check --kind "$KIND" --json 2>>"$LOG") || {
    note_fail "release-triage-check --kind ${KIND} 失敗，這輪跳過 changelog 派工"
    if [ "$KIND" != claude ] || [ -z "$CLAUDE_DIFF_NEW" ]; then continue; fi
    BINARY_ONLY=1
    TO="$CLAUDE_DIFF_NEW"
    PAYLOAD=$(printf '{"kind":"claude","from":"%s","to":"%s","pending":[]}' "$CLAUDE_DIFF_OLD" "$CLAUDE_DIFF_NEW")
  }
  if [ "$BINARY_ONLY" -eq 0 ]; then
  # 只挑這一則交辦要帶的版本（最多 MAX_VERSIONS 版、kept+unmatched 合計 MAX_ENTRIES 條，順序照 JSON，
  # 第一版一定帶——單版超量也不能永遠卡住）。輸出第一行是這一批的 "to"，其後是精簡 JSON。
  # 沒被截斷時 "to" 就是 JSON 的 to；截斷時用這批最後一版，避免下一輪同 request-id 被 daemon 去重吞掉。
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
        continue   # pending 是空的：安靜，不寫 log
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

  [ -n "$BOT" ] || { note_fail "找不到要派給誰（AGM_RELEASE_BOT／runtime.json 的 release_bot_id 或 responder_bot_id），跳過"; exit 0; }
  quota_gate || exit 0

  BODY=$(mktemp "${TMPDIR:-/tmp}/agm-release-triage.XXXXXX"); TMPS+=("$BODY"); TMPS_COUNT=$((TMPS_COUNT + 1))
  NOTICE_ID="agm-release-triage-${KIND}-${TO}-notice"
  if [ "$BINARY_ONLY" -eq 1 ]; then
    # A changelog notice for this version may already have been sent before the installed binary changed.
    # Keep the legacy binary namespace in that case so daemon notice idempotency does not reject different text.
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

  # 旗標叫 `--request-id`（不是 --client-request-id）：拼錯的話 argparse 直接 exit 2，
  # 而 stub 吃掉未知旗標的測試看不出來（2026-09-16 的事故，見 claude-release-kick.sh）。
  ASSIGN_ID="release-triage-${KIND}-${TO}"
  [ "$BINARY_ONLY" -eq 0 ] || ASSIGN_ID="release-triage-claude-binary-${TO}"
  if "$AGM" --compact assign --bot "$BOT" --review-by patrol --text-file "$BODY" \
       --request-id "$ASSIGN_ID" >> "$LOG" 2>&1; then
    log "${KIND} → ${TO}：已派 ${BOT} 分診"
    # 只標這一則實際帶出去的版本（截斷後那批）。標失敗不重派：request-id 會擋住重複交辦；
    # 帳本這輪停在 pending 只代表下一輪 check 又回同一批，下一輪同 request-id 由 daemon 去重。
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
      if "$AGM" --compact release-triage dispatched --kind "$KIND" "${VARGS[@]}" >> "$LOG" 2>&1; then
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
