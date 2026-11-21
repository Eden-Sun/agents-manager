
alert() {
  log "ALERT ${1}：${2}"
  if [ -x "$AGM" ]; then
    "$AGM" --compact ops-alert --source "$OWNER" --reason "$1" --detail "$2" >> "$LOG" 2>&1 ||
      log "推 ops-alert 失敗（舊 CLI 或 daemon 不在），只留在這份 log"
  else
    log "agm CLI 不在 ${AGM}，ops-alert 只留在這份 log"
  fi
}

lock_age() {
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

note_fail() { ROUND_FAIL="${ROUND_FAIL:-$1}"; log "$1"; }
