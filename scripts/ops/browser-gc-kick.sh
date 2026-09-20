#!/bin/zsh
# 定期喚醒 agm-pxf2pv-browser-gc（sonnet）清理殭屍瀏覽器視窗。由 launchd 每 6 小時跑一次。
set -u
DIR="$(cd "$(dirname "$0")/.." && pwd)"
BOT=01M248GA4H1TAHJCZRKVR73S3C
LOG="$DIR/browser-gc.log"
cd "$DIR" || exit 1
echo "== $(date '+%F %T') kick" >> "$LOG"

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
      echo "  keep headless pid $pid（父程序 $ppid 還活著）dir=$dir" >> "$LOG"; kept=$((kept+1)); continue
    fi
    if [ -n "$port" ] && [ "$(lsof -nP -iTCP:"$port" -sTCP:ESTABLISHED 2>/dev/null | grep -c ESTABLISHED)" -gt 0 ]; then
      echo "  keep headless pid $pid（port $port 上有 CDP 連線，正在用）dir=$dir" >> "$LOG"; kept=$((kept+1)); continue
    fi
    # 算不出年齡就當成「剛起」保守保留，寧可下一輪再收。
    if [ -z "${secs:-}" ] || [ "$secs" -lt 120 ]; then
      echo "  keep headless pid $pid（剛起 ${secs}s，client 可能還沒連上）dir=$dir" >> "$LOG"; kept=$((kept+1)); continue
    fi
    kill -TERM "$pid" 2>/dev/null
    sleep 3
    kill -0 "$pid" 2>/dev/null && kill -KILL "$pid" 2>/dev/null
    echo "  reap headless pid $pid（孤兒、無 CDP 連線、活了 ${secs:-?}s）dir=$dir" >> "$LOG"
    # 帶 .. 的不刪：/tmp/am-x/../../foo 前綴上像 /tmp/am-*，實際指到別處（#373）。
    case "$dir" in *..*) ;; /tmp/am-*) rm -rf "$dir" && echo "    rm -rf $dir" >> "$LOG";; esac
    reaped=$((reaped+1))
  done <<EOF2
$(ps -axo pid,ppid,command | grep 'Google Chrome' | grep -- '--headless' | grep -v -- '--type=' | grep -v grep)
EOF2
  echo "headless Chrome：收掉 $reaped／保留 $kept" >> "$LOG"
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
    rm -rf "$d" && { echo "  rm -rf $d（${sz}KB，沒有 Chrome 在用）" >> "$LOG"; n=$((n+1)); }
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
bin/agm --compact assign --review-by patrol --bot "$BOT" --text-file browser-gc-task.md \
  --request-id "agm-browser-gc-$(date +%Y%m%d-%H%M)" >> "$LOG" 2>&1
