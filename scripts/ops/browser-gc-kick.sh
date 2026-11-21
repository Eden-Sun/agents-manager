#!/bin/zsh
set -u
DIR="$(cd "$(dirname "$0")/.." && pwd)"
BOT=01M248GA4H1TAHJCZRKVR73S3C
LOG="$DIR/browser-gc.log"
cd "$DIR" || exit 1
log() { echo "$(date '+%F %T') $*" >> "$LOG"; }
. "${0%/*}/kick-common.sh" 2>/dev/null || {
  _m="找不到 ${0%/*}/kick-common.sh，${0##*/} 停擺（它要和 kick 一起裝進 bin/）"
  echo "$(date '+%F %T') ALERT kick_common_missing：$_m" >> "$LOG"; echo "$_m" >&2
  _a="${AGM:-$DIR/bin/agm}"
  [ -x "$_a" ] && "$_a" --compact ops-alert --source "${OWNER:-${0##*/}}" --reason kick_common_missing --detail "$_m" >> "$LOG" 2>&1
  exit 1
}
echo "== $(date '+%F %T') kick" >> "$LOG"

LOCK="$DIR/browser-gc.lock"
LOCK_STALE_SECS=${AGM_LOCK_STALE_SECS:-120}
LOCK_HUNG_SECS=${AGM_LOCK_HUNG_SECS:-3600}
cleanup() { rm -rf "$LOCK" 2>/dev/null || true; true; }
take_lock() { mkdir "$LOCK" 2>/dev/null && { echo "$$ $(date +%s)" > "$LOCK/owner"; trap cleanup EXIT; return 0; }; return 1; }
if ! take_lock; then
  _pid=$(cut -d' ' -f1 "$LOCK/owner" 2>/dev/null)
  _age=$(lock_age)
  if [ -n "$_pid" ] && kill -0 "$_pid" 2>/dev/null && ps -o command= -p "$_pid" 2>/dev/null | grep -q 'browser-gc-kick'; then
    if [ "$_age" -ge "$LOCK_HUNG_SECS" ]; then
      log "WARN: 上一輪（pid ${_pid}）已經跑了 ${_age} 秒還沒結束，瀏覽器清理停住；必要時結束它並移除 ${LOCK}"
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
    log "WARN: 殘留鎖 ${LOCK} 清不掉（執行者 ${_pid:-未知} 已不在），瀏覽器清理停住；請人工確認沒有執行者後移除它"
    exit 0
  fi
fi

cdp_state() {
  local out rc
  out=$(lsof -nP -iTCP:"$1" -sTCP:ESTABLISHED 2>&1); rc=$?
  if printf '%s\n' "$out" | grep -q ESTABLISHED; then echo connected
  elif [ "$rc" = 1 ] && [ -z "$out" ]; then echo none
  else echo unknown
  fi
}

proc_fingerprint() { ps -o lstart=,command= -p "$1" 2>/dev/null; }

dir_ambiguous() {
  local c
  for c in /tmp/am-*(N); do
    [ "$c" != "$1" ] && case "$c" in "$1 "*) return 0 ;; esac
  done
  return 1
}

reap_headless() {
  local reaped=0 kept=0
  while IFS= read -r line; do
    [ -z "$line" ] && continue
    local pid ppid secs dir port fp after
    pid=$(echo "$line" | awk '{print $1}')
    ppid=$(echo "$line" | awk '{print $2}')
    secs=$(ps -o etime= -p "$pid" 2>/dev/null | tr -d ' ' | awk -F'[-:]' '{
      if (NF==4) print (($1*24+$2)*60+$3)*60+$4;
      else if (NF==3) print (($1*60)+$2)*60+$3;
      else if (NF==2) print $1*60+$2;
    }')
    dir=$(echo "$line" | sed -n 's/.*--user-data-dir=\([^ ]*\).*/\1/p')
    port=$(echo "$line" | sed -n 's/.*--remote-debugging-port=\([0-9]*\).*/\1/p')
    if [ "$ppid" != "1" ]; then
      echo "  keep headless pid ${pid}（父程序 $ppid 還活著）dir=$dir" >> "$LOG"; kept=$((kept+1)); continue
    fi
    if [ -n "$port" ]; then
      case "$(cdp_state "$port")" in
        connected) echo "  keep headless pid ${pid}（port $port 上有 CDP 連線，正在用）dir=$dir" >> "$LOG"; kept=$((kept+1)); continue ;;
        unknown) echo "  keep headless pid ${pid}（port $port 的 CDP 連線狀態無法判定，保守保留）dir=$dir" >> "$LOG"; kept=$((kept+1)); continue ;;
      esac
    fi
    if [ -z "${secs:-}" ] || [ "$secs" -lt 120 ]; then
      echo "  keep headless pid ${pid}（剛起 ${secs}s，client 可能還沒連上）dir=$dir" >> "$LOG"; kept=$((kept+1)); continue
    fi
    fp=$(proc_fingerprint "$pid")
    case "$fp" in
      *--headless*) ;;
      *) echo "  keep headless pid ${pid}（讀不到行程身份，保守保留）dir=$dir" >> "$LOG"; kept=$((kept+1)); continue ;;
    esac
    kill -TERM "$pid" 2>/dev/null
    sleep 3
    after=$(proc_fingerprint "$pid")
    if [ -z "$after" ]; then
      :
    elif [ "$after" = "$fp" ]; then
      kill -0 "$pid" 2>/dev/null && kill -KILL "$pid" 2>/dev/null
    else
      echo "  pid ${pid} 已不是原來的行程（TERM 後身份改變，PID 可能被回收），不送 KILL" >> "$LOG"
    fi
    echo "  reap headless pid ${pid}（孤兒、無 CDP 連線、活了 ${secs:-?}s）dir=$dir" >> "$LOG"
    case "$dir" in *..*) ;; /tmp/am-*)
      if dir_ambiguous "$dir"; then echo "    保留 ${dir}（路徑可能含空白、無法確定，交給 profile 清理）" >> "$LOG"
      else rm -rf "$dir" && echo "    rm -rf $dir" >> "$LOG"
      fi ;;
    esac
    reaped=$((reaped+1))
  done <<EOF2
$(ps -axo pid,ppid,command | grep 'Google Chrome' | grep -- '--headless' | grep -v -- '--type=' | grep -v grep)
EOF2
  echo "headless Chrome：收掉 ${reaped}／保留 $kept" >> "$LOG"
}

profile_in_use() {
  D="$1" awk 'BEGIN { k = "--user-data-dir=" ENVIRON["D"] }
    { s = $0; while ((i = index(s, k)) > 0) { r = substr(s, i + length(k)); if (r == "" || substr(r, 1, 1) == " ") found = 1; s = substr(s, i + 1) } }
    END { exit found ? 0 : 1 }' <<< "$2"
}

reap_profiles() {
  local n=0 sz psout cmds d
  if ! psout=$(ps -axo command 2>/dev/null); then
    echo "profile 目錄：略過（無法列出行程）" >> "$LOG"; return
  fi
  cmds=$(printf '%s\n' "$psout" | grep 'Google Chrome' | grep -v grep)
  for d in /tmp/am-*(N); do
    [ -d "$d" ] || continue
    { [ -e "$d/Local State" ] || [ -d "$d/Default" ] || [ -e "$d/DevToolsActivePort" ]; } || continue
    profile_in_use "$d" "$cmds" && continue
    [ -n "$(find "$d" -maxdepth 0 -mmin -60 2>/dev/null)" ] && continue
    sz=$(du -sk "$d" 2>/dev/null | awk '{print $1}')
    rm -rf "$d" && { echo "  rm -rf ${d}（${sz}KB，沒有 Chrome 在用）" >> "$LOG"; n=$((n+1)); }
  done
  echo "profile 目錄：刪掉 $n 個" >> "$LOG"
}

reap_headless
reap_profiles
"$DIR/bin/pane-gc.sh"
if ! bin/agm --compact state | python3 -c "
import json,sys;d=json.load(sys.stdin)
b=[x for x in d['bots'] if x['id']=='$BOT'][0]
sys.exit(0 if (b.get('run') or {}).get('state')=='running' else 1)"; then
  bin/agm bot start "$BOT" >> "$LOG" 2>&1
  sleep 20
fi
bin/agm --compact assign --review-by patrol --bot "$BOT" --text-file browser-gc-task.md \
  --request-id "agm-browser-gc-$(date +%Y%m%d-%H%M)" >> "$LOG" 2>&1
