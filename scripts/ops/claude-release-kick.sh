#!/bin/bash
# Claude Code 換版就派 AGM 解析新版有什麼用得上的，結論當通知回給使用者（使用者 2026-09-16）。
# launchd `com.agm.claude-release` 每 30 分鐘跑一次；唯讀，只派工，不 build 不重啟。
#
#   AGM_DIR、CLAUDE_VERSIONS_DIR、AGM_RELEASE_BOT、AGM_FAIL_ALERT_AFTER 可覆寫（測試用）。
set -u
DIR="${AGM_DIR:-$HOME/.config/agents-manager/supervisor/AGM}"
VERSIONS="${CLAUDE_VERSIONS_DIR:-$HOME/.local/share/claude/versions}"
AGM="$DIR/bin/agm"
LOG="$DIR/claude-release.log"
STATE="$DIR/claude-release.last"     # 已經解析過的版本
TASK="$DIR/claude-release-task.md"

log() { echo "$(date '+%F %T') $*" >> "$LOG"; }

[ -x "$AGM" ] || exit 0
[ -f "$TASK" ] || { log "找不到 ${TASK}，跳過"; exit 0; }
[ -d "$VERSIONS" ] || { log "找不到版本目錄 ${VERSIONS}，跳過"; exit 0; }

# 兩個執行者同時派會送出兩筆一樣的交辦。鎖裡寫 pid 與時間（同 herdr-update-kick.sh／release-triage-kick.sh）：
# SIGKILL／斷電那一輪 EXIT trap 沒跑，鎖會留在磁碟上——以前這裡只 `mkdir` 失敗就跳過，殘留的鎖讓 Claude 換版通知
# 永久、安靜地停住（#66 同一類）。現在執行者不在就回收接手；還活著但卡太久、或回收不了才推 ops_alert。
OWNER="${AM_AGENT_NAME:-claude-release-kick}"
alert() { # alert <reason> <detail>：一則 durable inbox 事件（同 source+reason 每小時一則，daemon 去重）
  log "ALERT ${1}：${2}"
  "$AGM" --compact ops-alert --source "$OWNER" --reason "$1" --detail "$2" >> "$LOG" 2>&1 ||
    log "推 ops-alert 失敗（舊 CLI 或 daemon 不在），只留在這份 log"
}
# 連續派不出去（assign 失敗、找不到派給誰）不能永遠只有 local log：連續 FAIL_ALERT_AFTER 輪推 ops_alert，派成功清零。
FAILS="$DIR/claude-release.fails"
FAIL_ALERT_AFTER=${AGM_FAIL_ALERT_AFTER:-4}   # 每 30 分鐘一輪＝連續約 2 小時
ROUND_FAIL=""
note_fail() { ROUND_FAIL="${ROUND_FAIL:-$1}"; log "$1"; }
settle() { # 收尾結算：這輪有失敗就累計，否則（有換版且派成功，或沒換版）清零
  if [ -n "$ROUND_FAIL" ]; then
    _n=$(cat "$FAILS" 2>/dev/null)
    case "$_n" in ''|*[!0-9]*) _n=0 ;; esac
    _n=$((_n + 1)); echo "$_n" > "$FAILS"
    [ "$_n" -ge "$FAIL_ALERT_AFTER" ] && alert dispatch_failing "Claude 換版通知連續 ${_n} 輪派不出去：${ROUND_FAIL}"
  else
    rm -f "$FAILS" 2>/dev/null
  fi
  return 0
}
LOCK="$DIR/claude-release.lock"
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
BODY=""
cleanup() { settle; rm -rf "$LOCK" 2>/dev/null || true; [ -n "$BODY" ] && rm -f "$BODY" 2>/dev/null; true; }
take_lock() { mkdir "$LOCK" 2>/dev/null && { echo "$$ $(date +%s)" > "$LOCK/owner"; trap cleanup EXIT; return 0; }; return 1; }
if ! take_lock; then
  _pid=$(cut -d' ' -f1 "$LOCK/owner" 2>/dev/null)
  _age=$(lock_age)
  if [ -n "$_pid" ] && kill -0 "$_pid" 2>/dev/null && ps -o command= -p "$_pid" 2>/dev/null | grep -q 'claude-release-kick'; then
    if [ "$_age" -ge "$LOCK_HUNG_SECS" ]; then
      alert runner_hung "上一輪（pid ${_pid}）已經跑了 ${_age} 秒還沒結束，Claude 換版通知停住。請確認它在做什麼，必要時結束它並移除 ${LOCK}"
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
    alert stale_lock "殘留鎖 ${LOCK} 清不掉（執行者 ${_pid:-未知} 已不在），Claude 換版通知停住。請人工確認沒有執行者後移除它"
    exit 0
  fi
fi

# 版本目錄名就是版本號；最新的那個是現在會跑的（claude 自己更新時寫進去）。
NEW=$(ls -t "$VERSIONS" 2>/dev/null | head -1)
[ -n "$NEW" ] || { log "版本目錄是空的，跳過"; exit 0; }
DONE_VER=""
[ -f "$STATE" ] && DONE_VER=$(tr -d '[:space:]' < "$STATE")
if [ "$NEW" = "$DONE_VER" ]; then
  exit 0      # 沒換版：安靜退出，不寫 log（每 30 分鐘一次，不值得留一行）
fi
# 第一次跑（還沒有 state）只記下現在的版本，不為「安裝當下已經在的版本」派一次工。
if [ -z "$DONE_VER" ]; then
  echo "$NEW" > "$STATE"
  log "第一次執行，記下目前版本 ${NEW}，不派工"
  exit 0
fi
OLD=$DONE_VER

# 派給誰：**不能是巡檢自己**——daemon 擋掉「總管對自己下交辦」（supervisor/mod.rs 的
# `the supervisor cannot assign work to itself`），原本填 manager_bot_id 的版本每一輪都 400。
# 順序：明指的 AGM_RELEASE_BOT ＞ runtime.json 的 release_bot_id（專用 child）＞ 協調者。
# 協調者是合法目標（走交接佇列），它解析完照 task 裡的指示把通知交給巡檢，使用者才看得到。
BOT="${AGM_RELEASE_BOT:-}"
if [ -z "$BOT" ] && [ -f "$DIR/runtime.json" ]; then
  BOT=$(python3 -c '
import json,sys
d = json.load(open(sys.argv[1]))
print(d.get("release_bot_id") or d.get("responder_bot_id") or "")
' "$DIR/runtime.json" 2>/dev/null)
fi
[ -n "$BOT" ] || { note_fail "找不到要派給誰（AGM_RELEASE_BOT／runtime.json 的 release_bot_id 或 responder_bot_id），跳過"; exit 0; }

BODY=$(mktemp -t agm-claude-release)
{
  cat "$TASK"
  printf '\n\n---\n本次：舊版 %s → 新版 %s\n' "$OLD" "$NEW"
  printf 'OLD=%s/%s\nNEW=%s/%s\n' "$VERSIONS" "$OLD" "$VERSIONS" "$NEW"
} > "$BODY"

# 旗標叫 `--request-id`（不是 --client-request-id）：拼錯的話 argparse 直接 exit 2，
# 而 stub 吃掉未知旗標的測試看不出來（2026-09-16 的事故）。
if "$AGM" --compact assign --bot "$BOT" --review-by patrol --text-file "$BODY" \
     --request-id "agm-claude-release-$NEW" >> "$LOG" 2>&1; then
  echo "$NEW" > "$STATE"
  log "Claude Code ${OLD} → ${NEW}：已派 AGM 解析"
else
  note_fail "派工失敗（${OLD} → ${NEW}），下一輪再試"
fi
