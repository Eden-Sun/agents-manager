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
#   AGM_LOCK_HUNG_SECS 可覆寫（測試用）。
set -u
PATH="${AGM_EXTRA_PATH-/opt/homebrew/bin:/usr/local/bin}:$PATH"; export PATH   # AGM_EXTRA_PATH 只給測試蓋掉

DIR="${AGM_DIR:-${HOME:-/nonexistent}/.config/agents-manager/supervisor/AGM}"
REPO="${AGM_REPO:-${HOME:-/nonexistent}/project/agents-manager}"
BIN="${AM_BINARY:-$REPO/target/release/agents-managerd}"
AGM="$DIR/bin/agm"
LOG="$DIR/release-triage.log"
TASK="$DIR/release-triage-task.md"
OWNER="${AM_AGENT_NAME:-release-triage-kick}"
QUOTA_MAX="${AGM_TRIAGE_QUOTA_MAX:-85}"
MAX_VERSIONS=5      # 一則交辦最多帶幾版
MAX_ENTRIES=80      # 一則交辦 kept+unmatched 合計上限；超過的留給下一輪

log() { echo "$(date '+%F %T') $*" >> "$LOG"; }

[ -x "$AGM" ] || exit 0

# 卡住了、自己解不開時喊人：一則 durable inbox 事件（同 source+reason 每小時一則，daemon 去重）。
alert() { # alert <reason> <detail>
  log "ALERT ${1}：${2}"
  "$AGM" --compact ops-alert --source "$OWNER" --reason "$1" --detail "$2" >> "$LOG" 2>&1 ||
    log "推 ops-alert 失敗（舊 CLI 或 daemon 不在），只留在這份 log"
}

# launchd 的預設 PATH 不含 Homebrew；缺依賴不能只靜默 exit 0，否則「已排程」但永遠不做（#66 留言）。
command -v python3 >/dev/null 2>&1 || { alert missing_dependency "找不到 python3（PATH=${PATH}），上游新版分診停住"; exit 0; }
[ -f "$TASK" ] || { log "找不到 ${TASK}，跳過"; exit 0; }
[ -x "$BIN" ] || { log "找不到 ${BIN}，跳過（需要先建好 release binary）"; exit 0; }

# 鎖：兩個執行者同時派會送出重複交辦。鎖裡寫 pid 與時間（抄 daemon-update-kick.sh 的格式）：
# SIGKILL／斷電那一輪 EXIT trap 沒跑，鎖會留在磁碟上——執行者不在就回收接手；還活著但卡太久才喊人。
LOCK="$DIR/release-triage.lock"
LOCK_STALE_SECS=${AGM_LOCK_STALE_SECS:-120}    # 沒有 pid 可查時，超過這麼久就算殘留
LOCK_HUNG_SECS=${AGM_LOCK_HUNG_SECS:-3600}     # 執行者還活著但卡了這麼久：喊人
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
TMPS=()
cleanup() { rm -rf "$LOCK" 2>/dev/null || true; [ ${#TMPS[@]} -gt 0 ] && rm -f "${TMPS[@]}" 2>/dev/null; true; }
take_lock() { mkdir "$LOCK" 2>/dev/null && { echo "$$ $(date +%s)" > "$LOCK/owner"; trap cleanup EXIT; return 0; }; return 1; }
if ! take_lock; then
  _pid=$(cut -d' ' -f1 "$LOCK/owner" 2>/dev/null)
  _age=$(lock_age)
  if [ -n "$_pid" ] && kill -0 "$_pid" 2>/dev/null && ps -o command= -p "$_pid" 2>/dev/null | grep -q 'release-triage-kick'; then
    if [ "$_age" -ge "$LOCK_HUNG_SECS" ]; then
      alert runner_hung "上一輪（pid ${_pid}）已經跑了 ${_age} 秒還沒結束，上游新版分診停住。請確認它在做什麼，必要時結束它並移除 ${LOCK}"
    else
      log "分診已有執行者（pid ${_pid}，${_age} 秒），這輪跳過"
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
  REPORT=$("$BIN" release-triage-check --kind "$KIND" --json 2>>"$LOG") || {
    log "release-triage-check --kind ${KIND} 失敗，這輪跳過"; continue
  }
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
    3) continue ;;      # pending 是空的：安靜，不寫 log
    *) log "release-triage-check --kind ${KIND} 的輸出看不懂（rc=${RC}），這輪跳過"; continue ;;
  esac
  TO=$(printf '%s\n' "$PICK" | head -1)
  PAYLOAD=$(printf '%s\n' "$PICK" | sed 1d)

  [ -n "$BOT" ] || { log "找不到要派給誰（AGM_RELEASE_BOT／runtime.json 的 release_bot_id 或 responder_bot_id），跳過"; exit 0; }
  quota_gate || exit 0

  BODY=$(mktemp -t agm-release-triage); TMPS+=("$BODY")
  {
    cat "$TASK"
    printf '\n\n---\n本次：kind=%s，%s 版待分診（新版 %s）。以下 JSON 是 `agents-managerd release-triage-check` 的輸出，逐條 verdict 用它的 `id`。\n\n```json\n%s\n```\n' \
      "$KIND" "$(printf '%s' "$PAYLOAD" | python3 -c 'import json,sys; print(len(json.load(sys.stdin)["pending"]))')" "$TO" "$PAYLOAD"
  } > "$BODY"

  # 旗標叫 `--request-id`（不是 --client-request-id）：拼錯的話 argparse 直接 exit 2，
  # 而 stub 吃掉未知旗標的測試看不出來（2026-09-16 的事故，見 claude-release-kick.sh）。
  if "$AGM" --compact assign --bot "$BOT" --review-by patrol --text-file "$BODY" \
       --request-id "release-triage-${KIND}-${TO}" >> "$LOG" 2>&1; then
    log "${KIND} → ${TO}：已派 ${BOT} 分診"
  else
    log "派工失敗（${KIND} → ${TO}），下一輪再試"
  fi
done
exit 0
