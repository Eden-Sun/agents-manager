# kick-common.sh：各 *-kick.sh 共用的函式（用 source 載入，不單獨執行）。
# 呼叫端要先設好 AGM、LOG、OWNER（alert）、LOCK（lock_age）、log()（alert／note_fail）；函式在呼叫時才讀這些變數。

# 卡住了、自己解不開時喊人：一則 durable inbox 事件（同 source+reason 每小時一則，daemon 去重）。
alert() { # alert <reason> <detail>
  log "ALERT ${1}：${2}"
  if [ -x "$AGM" ]; then
    "$AGM" --compact ops-alert --source "$OWNER" --reason "$1" --detail "$2" >> "$LOG" 2>&1 ||
      log "推 ops-alert 失敗（舊 CLI 或 daemon 不在），只留在這份 log"
  else
    log "agm CLI 不在 ${AGM}，ops-alert 只留在這份 log"
  fi
}

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

# 記 log，並讓收尾把這一輪算成失敗（呼叫端要先設 ROUND_FAIL=""）。
note_fail() { ROUND_FAIL="${ROUND_FAIL:-$1}"; log "$1"; }
