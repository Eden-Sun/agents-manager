#!/bin/zsh
# 定期喚醒 agm-pxf2pv-browser-gc（sonnet）清理殭屍瀏覽器視窗。
# launchd `com.agm.browser-gc`，`StartInterval 1800`（30 分鐘，見 scripts/ops/launchd/）。
#
#   AGM_LOCK_STALE_SECS、AGM_LOCK_HUNG_SECS 可覆寫（測試用）。
set -u
DIR="$(cd "$(dirname "$0")/.." && pwd)"
BOT=01M248GA4H1TAHJCZRKVR73S3C
LOG="$DIR/browser-gc.log"
cd "$DIR" || exit 1
log() { echo "$(date '+%F %T') $*" >> "$LOG"; }
echo "== $(date '+%F %T') kick" >> "$LOG"

# 鎖（issue #490）：這一支會殺 Chrome、`rm -rf` profile、關 pane、派工，卻是 kick 家族裡唯一沒有鎖的。
# 一輪跑超過 `StartInterval` 下一輪就疊上去，最明確的後果是重複派工。形狀抄 release-triage-kick.sh：
# 鎖裡寫 pid 與時間；SIGKILL／斷電那一輪 EXIT trap 沒跑，鎖會留在磁碟上——執行者不在就回收接手，
# 還活著但卡太久只記一行（這支沒有 ops-alert 的管道，不在這張票的範圍裡加）。
LOCK="$DIR/browser-gc.lock"
LOCK_STALE_SECS=${AGM_LOCK_STALE_SECS:-120}    # 沒有 pid 可查時，超過這麼久就算殘留
LOCK_HUNG_SECS=${AGM_LOCK_HUNG_SECS:-3600}     # 執行者還活著但卡了這麼久：記一行醒目的
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
cleanup() { rm -rf "$LOCK" 2>/dev/null || true; true; }
take_lock() { mkdir "$LOCK" 2>/dev/null && { echo "$$ $(date +%s)" > "$LOCK/owner"; trap cleanup EXIT; return 0; }; return 1; }
if ! take_lock; then
  _pid=$(cut -d' ' -f1 "$LOCK/owner" 2>/dev/null)
  _age=$(lock_age)
  # `kill -0` 之外再比對指令名：pid 被回收之後，光看「還活著」會把別人的行程當成自己的執行者。
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

# ── bot 的 headless Chrome（CDP 截圖用）────────────────────────────────────────
# 這段用 shell 直接做，不花 LLM：判斷純機械（孤兒 + 沒有 CDP 連線 + 活超過 2 分鐘）。
# 使用者自己的 Chrome 沒有 --headless，永遠不會進這個清單。
# 注意 ppid=1 不等於「沒人在用」：bot 用 nohup/detached 起的實例，bot 還活著時 ppid 也是 1。
# 所以再要求「debug port 上沒有 ESTABLISHED 連線」——正在截圖的實例一定有一條。
reap_headless() {
  local reaped=0 kept=0
  while IFS= read -r line; do
    [ -z "$line" ] && continue
    local pid ppid secs dir port
    pid=$(echo "$line" | awk '{print $1}')
    ppid=$(echo "$line" | awk '{print $2}')
    # macOS 的 ps 沒有 etimes，只有 etime（[[D-]HH:]MM:SS），自己換算成秒。
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
    if [ -n "$port" ] && [ "$(lsof -nP -iTCP:"$port" -sTCP:ESTABLISHED 2>/dev/null | grep -c ESTABLISHED)" -gt 0 ]; then
      echo "  keep headless pid ${pid}（port $port 上有 CDP 連線，正在用）dir=$dir" >> "$LOG"; kept=$((kept+1)); continue
    fi
    # 算不出年齡就當成「剛起」保守保留，寧可下一輪再收。
    if [ -z "${secs:-}" ] || [ "$secs" -lt 120 ]; then
      echo "  keep headless pid ${pid}（剛起 ${secs}s，client 可能還沒連上）dir=$dir" >> "$LOG"; kept=$((kept+1)); continue
    fi
    kill -TERM "$pid" 2>/dev/null
    sleep 3
    kill -0 "$pid" 2>/dev/null && kill -KILL "$pid" 2>/dev/null
    echo "  reap headless pid ${pid}（孤兒、無 CDP 連線、活了 ${secs:-?}s）dir=$dir" >> "$LOG"
    # 帶 .. 的不刪：/tmp/am-x/../../foo 前綴上像 /tmp/am-*，實際指到別處（#373）。
    case "$dir" in *..*) ;; /tmp/am-*) rm -rf "$dir" && echo "    rm -rf $dir" >> "$LOG";; esac
    reaped=$((reaped+1))
  done <<EOF2
$(ps -axo pid,ppid,command | grep 'Google Chrome' | grep -- '--headless' | grep -v -- '--type=' | grep -v grep)
EOF2
  echo "headless Chrome：收掉 ${reaped}／保留 $kept" >> "$LOG"
}

# 沒有 Chrome 在用的 /tmp/am-* profile 目錄（上次跑完沒刪的殘留）。1 小時內動過的不碰，
# 免得殺到正在起的實例。
reap_profiles() {
  local live n=0 sz
  live=$(ps -axo command | grep 'Google Chrome' | grep -- '--headless' | sed -n 's/.*--user-data-dir=\([^ ]*\).*/\1/p')
  # 任何 /tmp/am-* 只要長得像 Chrome profile（有 Local State / Default / DevToolsActivePort）就算；
  # 截圖、build target 之類的目錄沒有這些標記，不會被碰。
  # (N)＝沒符合就展開成空；只放寬這一個 glob，不對整支 setopt nonomatch（#371）。
  for d in /tmp/am-*(N); do
    [ -d "$d" ] || continue
    { [ -e "$d/Local State" ] || [ -d "$d/Default" ] || [ -e "$d/DevToolsActivePort" ]; } || continue
    echo "$live" | grep -qx "$d" && continue
    [ -n "$(find "$d" -maxdepth 0 -mmin -60 2>/dev/null)" ] && continue
    sz=$(du -sk "$d" 2>/dev/null | awk '{print $1}')
    rm -rf "$d" && { echo "  rm -rf ${d}（${sz}KB，沒有 Chrome 在用）" >> "$LOG"; n=$((n+1)); }
  done
  echo "profile 目錄：刪掉 $n 個" >> "$LOG"
}

reap_headless
reap_profiles
"$DIR/bin/pane-gc.sh"
# 若尚未 running 就啟動（逾時不算失敗，daemon 會繼續起）
if ! bin/agm --compact state | python3 -c "
import json,sys;d=json.load(sys.stdin)
b=[x for x in d['bots'] if x['id']=='$BOT'][0]
sys.exit(0 if (b.get('run') or {}).get('state')=='running' else 1)"; then
  bin/agm bot start "$BOT" >> "$LOG" 2>&1
  sleep 20
fi
# `--request-id` **刻意保持分鐘級**（issue #490）：`turns_client_req` 是
# `(conversation_id, client_request_id)` 上**沒有時間範圍**的唯一索引（`daemon/src/db.rs:105`），
# 所以換成日期級的「穩定 key」會讓一天 48 輪只有第一輪派得出去，其餘全被當成重試擋掉。
# 防重複派工靠的是上面那把鎖——鎖擋住重疊之後，每一輪本來就只會有一個 key。
bin/agm --compact assign --review-by patrol --bot "$BOT" --text-file browser-gc-task.md \
  --request-id "agm-browser-gc-$(date +%Y%m%d-%H%M)" >> "$LOG" 2>&1
