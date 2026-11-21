#!/bin/bash
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
PASS=0
FAIL=0
. "$HERE/../test-assertions.sh" || exit 1
exists() { [ -e "$2" ] && { echo "ok   - $1"; PASS=$((PASS + 1)); } || { echo "FAIL - $1（$2 不見了）"; FAIL=$((FAIL + 1)); }; }
gone() { [ ! -e "$2" ] && { echo "ok   - $1"; PASS=$((PASS + 1)); } || { echo "FAIL - $1（$2 還在）"; FAIL=$((FAIL + 1)); }; }

setup() {
  ROOT=$(mktemp -d); FIX="$ROOT/fix"; TMPD="$ROOT/tmp"
  mkdir -p "$ROOT/agm/bin" "$ROOT/fakebin" "$FIX" "$TMPD"
  grep -q '/tmp/am-' "$HERE/browser-gc-kick.sh" || { echo "FAIL - 腳本裡沒有 /tmp/am-，測試的路徑替換要更新"; exit 1; }
  {
    echo '#!/bin/zsh'
    echo 'kill() { echo "kill $*" >> "$FIX/kills.log"; if [ "$1" = "-0" ]; then [ -e "$FIX/alive.$2" ]; return; fi; if [ "$1" = "-TERM" ] && [ -e "$FIX/reuse.$2" ]; then cp "$FIX/reuse.$2" "$FIX/fp.$2"; fi; if [ "$1" = "-TERM" ] && [ -e "$FIX/dies.$2" ]; then rm -f "$FIX/fp.$2"; fi; return 0; }'
    echo 'sleep() { :; }'
    tail -n +2 "$HERE/browser-gc-kick.sh" | sed "s#/tmp/am-#$TMPD/am-#g"
  } > "$ROOT/agm/bin/browser-gc-kick.sh"
  cp "$HERE/kick-common.sh" "$ROOT/agm/bin/kick-common.sh"
  echo '#!/bin/bash
echo "pane-gc called" >> "$FIX/kills.log"' > "$ROOT/agm/bin/pane-gc.sh"
  echo "TASK-BODY-MARKER" > "$ROOT/agm/browser-gc-task.md"
  cat > "$ROOT/agm/bin/agm" <<'STUB'
#!/bin/bash
echo "$*" >> "$FIX/agm.log"
case "$*" in
  *" state"*) printf '{"bots":[{"id":"01M248GA4H1TAHJCZRKVR73S3C","run":%s}]}' "$(cat "$FIX/agm-state" 2>/dev/null || echo '{"state":"running"}')" ;;
esac
STUB
  cat > "$ROOT/fakebin/ps" <<'STUB'
#!/bin/bash
case "$*" in
  "-axo pid,ppid,command") cat "$FIX/ps.txt" ;;
  "-axo command") awk '{ $1=""; $2=""; sub(/^ +/, ""); print }' "$FIX/ps.txt" ;;
  "-o etime= -p "*) cat "$FIX/etime.$4" 2>/dev/null ;;
  # 鎖的殘留判斷要比對指令名（issue #490）：$FIX/cmd.<pid> 有就印那一行。
  "-o command= -p "*) cat "$FIX/cmd.$4" 2>/dev/null ;;
  "-o lstart=,command= -p "*) cat "$FIX/fp.$4" 2>/dev/null ;;
  *) echo "ps stub: unknown $*" >&2; exit 2 ;;
esac
STUB
  cat > "$ROOT/fakebin/lsof" <<'STUB'
#!/bin/bash
p=${2#-iTCP:}
if [ -e "$FIX/estab.$p" ]; then echo "chrome 1 mac_builder 20u IPv4 TCP 127.0.0.1:$p->127.0.0.1:5555 (ESTABLISHED)"; exit 0; fi
case "$(cat "$FIX/lsof.mode" 2>/dev/null)" in
  missing) echo "lsof: command not found" >&2; exit 127 ;;
  fail) echo "lsof: can't open socket: Operation not permitted" >&2; exit 1 ;;
esac
exit 1
STUB
  chmod +x "$ROOT/agm/bin/"* "$ROOT/fakebin/"*
  LOG="$ROOT/agm/browser-gc.log"; : > "$LOG"; : > "$FIX/kills.log"; : > "$FIX/ps.txt"; : > "$FIX/agm.log"
  export FIX
}
teardown() { rm -rf "$ROOT"; unset FIX; }
run() { PATH="$ROOT/fakebin:$PATH" zsh "$ROOT/agm/bin/browser-gc-kick.sh" >/dev/null 2>&1; echo $?; }
chrome() {
  echo "$1 $2 /Applications/Google Chrome.app/Contents/MacOS/Google Chrome --headless=new --remote-debugging-port=$4 --user-data-dir=$5 ${6:-}" >> "$FIX/ps.txt"
  [ -n "$3" ] && echo "$3" > "$FIX/etime.$1"
  echo "Mon Oct 11 10:00:00 2026 /Applications/Google Chrome.app/Contents/MacOS/Google Chrome --headless=new --remote-debugging-port=$4 --user-data-dir=$5 ${6:-}" > "$FIX/fp.$1"
  return 0
}
kills() { grep -c '^kill -\(TERM\|KILL\)' "$FIX/kills.log" | tr -d ' '; }
mkprofile() { mkdir -p "$1"; touch "$1/Local State"; touch -t 202601010000 "$1" "$1/Local State"; }

setup
mkprofile "$TMPD/am-shot1"
chrome 501 1 "10:00" 9222 "$TMPD/am-shot1"
touch "$FIX/alive.501"
equals "exit 0" "$(run)" "0"
check  "先送 TERM" "kill -TERM 501" "$FIX/kills.log"
check  "TERM 後還活著就 KILL" "kill -KILL 501" "$FIX/kills.log"
check  "log 記 reap" "reap headless pid 501" "$LOG"
check  "log 計數 收 1／留 0" "headless Chrome：收掉 1／保留 0" "$LOG"
gone   "它的 /tmp/am-* profile 被刪" "$TMPD/am-shot1"
teardown
setup
chrome 502 1 "3:00:00" 9223 "$TMPD/am-shot2"; mkprofile "$TMPD/am-shot2"
run >/dev/null
check    "TERM 就死了就不 KILL：有 TERM" "kill -TERM 502" "$FIX/kills.log"
check_no "TERM 就死了就不 KILL：沒有 KILL" "kill -KILL" "$FIX/kills.log"
teardown

setup
mkprofile "$TMPD/am-clean"
chrome 503 1 "10:00" 9224 "$TMPD/am-clean"; touch "$FIX/dies.503"
run >/dev/null
check    "TERM 後正常結束：有送 TERM" "kill -TERM 503" "$FIX/kills.log"
check_no "TERM 後正常結束：不 KILL" "kill -KILL 503" "$FIX/kills.log"
check_no "TERM 後正常結束：log 不說 PID 被回收" "已不是原來的行程" "$LOG"
check    "TERM 後正常結束：照常記 reap" "reap headless pid 503" "$LOG"
teardown

setup
mkprofile "$TMPD/am-a"; mkprofile "$TMPD/am-b"; mkprofile "$TMPD/am-c"; mkprofile "$TMPD/am-d"; mkprofile "$TMPD/am-e"
chrome 601 4321 "10:00" 9301 "$TMPD/am-a"
chrome 602 1 "10:00" 9302 "$TMPD/am-b"; touch "$FIX/estab.9302"
chrome 603 1 "00:59" 9303 "$TMPD/am-c"
chrome 604 1 "" 9304 "$TMPD/am-d"
chrome 605 1 "10:00" 9305 "$TMPD/am-e" "--type=renderer"
echo "606 1 /Applications/Google Chrome.app/Contents/MacOS/Google Chrome --user-data-dir=/Users/me/Library/Application Support/Google/Chrome" >> "$FIX/ps.txt"
echo "607 1 /usr/bin/grep Google Chrome --headless" >> "$FIX/ps.txt"
run >/dev/null
equals "一個都不殺" "$(kills)" "0"
check  "父程序還活著：保留" "keep headless pid 601（父程序 4321 還活著）" "$LOG"
check  "有 CDP 連線：保留" "keep headless pid 602（port 9302 上有 CDP 連線" "$LOG"
check  "剛起：保留" "keep headless pid 603（剛起 59s" "$LOG"
check  "算不出年齡：保留" "keep headless pid 604（剛起 s" "$LOG"
check_no "renderer 子程序不進清單" "pid 605" "$LOG"
check_no "使用者自己的 Chrome（無 --headless）不進清單" "pid 606" "$LOG"
check_no "grep 自己不進清單" "pid 607" "$LOG"
check  "計數 收 0／保留 4" "headless Chrome：收掉 0／保留 4" "$LOG"
exists "被保留的 profile 都還在（父程序活著）" "$TMPD/am-a"
exists "被保留的 profile 都還在（有 CDP）" "$TMPD/am-b"
exists "被保留的 profile 都還在（剛起）" "$TMPD/am-c"
exists "被保留的 profile 都還在（沒年齡）" "$TMPD/am-d"
teardown

setup
run >/dev/null
equals "沒有 Chrome：不殺" "$(kills)" "0"
check  "沒有 Chrome：收 0／保留 0" "headless Chrome：收掉 0／保留 0" "$LOG"
teardown

setup
mkdir -p "$ROOT/keepme"; touch "$ROOT/keepme/Local State"
chrome 701 1 "10:00" 9401 "$ROOT/keepme"
run >/dev/null
check "收掉了" "reap headless pid 701" "$LOG"
exists "但不在 /tmp/am-* 的目錄不刪" "$ROOT/keepme/Local State"
teardown

setup
mkdir -p "$TMPD/am-x" "$TMPD/victim"; touch "$TMPD/victim/Local State"
chrome 702 1 "10:00" 9402 "$TMPD/am-x/../victim"
run >/dev/null
check  "帶 .. 的照樣收行程" "reap headless pid 702" "$LOG"
exists "帶 .. 指到 am-* 之外的目錄不刪" "$TMPD/victim/Local State"
teardown

setup
mkprofile "$TMPD/am-old-localstate"
mkdir -p "$TMPD/am-old-default/Default"; touch -t 202601010000 "$TMPD/am-old-default" "$TMPD/am-old-default/Default"
mkdir -p "$TMPD/am-old-devtools"; touch "$TMPD/am-old-devtools/DevToolsActivePort"; touch -t 202601010000 "$TMPD/am-old-devtools"
mkdir -p "$TMPD/am-recent"; touch "$TMPD/am-recent/Local State"
mkdir -p "$TMPD/am-target/debug"; touch -t 202601010000 "$TMPD/am-target" "$TMPD/am-target/debug"
mkdir -p "$TMPD/am-shots"; echo png > "$TMPD/am-shots/a.png"; touch -t 202601010000 "$TMPD/am-shots"
mkprofile "$TMPD/am-live"
chrome 801 4321 "10:00" 9501 "$TMPD/am-live"
mkdir -p "$TMPD/other-old"; touch "$TMPD/other-old/Local State"; touch -t 202601010000 "$TMPD/other-old"
mkprofile "$ROOT/notmp-old"
run >/dev/null
gone   "Local State 標記的舊 profile 刪" "$TMPD/am-old-localstate"
gone   "Default 標記的舊 profile 刪" "$TMPD/am-old-default"
gone   "DevToolsActivePort 標記的舊 profile 刪" "$TMPD/am-old-devtools"
exists "1 小時內動過的不刪" "$TMPD/am-recent"
exists "沒有 profile 標記的（build target）不刪" "$TMPD/am-target"
exists "沒有 profile 標記的（截圖目錄）不刪" "$TMPD/am-shots/a.png"
exists "有 Chrome 在用的不刪" "$TMPD/am-live"
exists "名字不是 am-* 的不刪" "$TMPD/other-old"
exists "根本不在 /tmp 的不刪" "$ROOT/notmp-old"
check  "log 計數 刪 3 個" "profile 目錄：刪掉 3 個" "$LOG"
teardown
setup
mkdir -p "$TMPD/am-x"; touch -t 202601010000 "$TMPD/am-x"
run >/dev/null
check "沒有像 profile 的：刪 0 個" "profile 目錄：刪掉 0 個" "$LOG"
exists "沒標記的空目錄不刪" "$TMPD/am-x"
teardown

setup
run >/dev/null
check    "有呼叫 pane-gc.sh" "pane-gc called" "$FIX/kills.log"
check_no "bot 在跑：不 start" "bot start" "$FIX/agm.log"
check    "派 assign 到正確的 bot" "assign --review-by patrol --bot 01M248GA4H1TAHJCZRKVR73S3C --text-file browser-gc-task.md --request-id agm-browser-gc-" "$FIX/agm.log"
teardown
setup
echo '{"state":"stopped"}' > "$FIX/agm-state"
run >/dev/null
check "bot 沒在跑：先 start" "bot start 01M248GA4H1TAHJCZRKVR73S3C" "$FIX/agm.log"
check "start 之後照派 assign" " assign " "$FIX/agm.log"
teardown
setup
echo 'null' > "$FIX/agm-state"
run >/dev/null
check "bot 沒有 run（null）：先 start" "bot start" "$FIX/agm.log"
teardown

setup
equals "沒有任何 am-*：exit 0" "$(run)" "0"
check  "沒有任何 am-*：profile 清理照跑、刪 0 個" "profile 目錄：刪掉 0 個" "$LOG"
check  "沒有任何 am-*：pane-gc 照跑" "pane-gc called" "$FIX/kills.log"
check  "沒有任何 am-*：照派 assign" " assign " "$FIX/agm.log"
teardown

setup
chrome 501 1 "05:00" 9222 "$TMPD/am-p1"
mkdir -p "$ROOT/agm/browser-gc.lock"
echo "4242 $(date +%s)" > "$ROOT/agm/browser-gc.lock/owner"
: > "$FIX/alive.4242"
echo "zsh $ROOT/agm/bin/browser-gc-kick.sh" > "$FIX/cmd.4242"
equals "有執行者時 exit 0" "$(run)" "0"
check    "log 說這輪跳過" "已有執行者（pid 4242" "$LOG"
check_no "沒有殺任何 Chrome" "kill -TERM" "$FIX/kills.log"
check_no "沒有呼叫 pane-gc" "pane-gc called" "$FIX/kills.log"
check_no "沒有派工" "assign" "$FIX/agm.log"
teardown

setup
mkdir -p "$ROOT/agm/browser-gc.lock"
echo "4242 1" > "$ROOT/agm/browser-gc.lock/owner"
AGM_LOCK_STALE_SECS=0 PATH="$ROOT/fakebin:$PATH" zsh "$ROOT/agm/bin/browser-gc-kick.sh" >/dev/null 2>&1
check "log 說清掉殘留鎖並接手" "清掉殘留鎖" "$LOG"
check "接手之後照常派工" "assign" "$FIX/agm.log"
teardown

setup
mkdir -p "$ROOT/agm/browser-gc.lock"
equals "讀不到執行者且鎖還新 → exit 0" "$(run)" "0"
check    "log 說鎖剛建立" "鎖剛建立" "$LOG"
check_no "沒有派工" "assign" "$FIX/agm.log"
teardown

setup
run >/dev/null
equals "跑完鎖不留下" "$([ -e "$ROOT/agm/browser-gc.lock" ] && echo yes || echo no)" "no"
teardown

equals "request-id 仍含 %H%M" "$(grep -c 'request-id "agm-browser-gc-$(date +%Y%m%d-%H%M)"' "$HERE/browser-gc-kick.sh")" "1"

setup
mkprofile "$TMPD/am-lsof-missing"
chrome 1101 1 "10:00" 9601 "$TMPD/am-lsof-missing"; echo missing > "$FIX/lsof.mode"
run >/dev/null
check_no "lsof 不存在：不殺" "kill -TERM 1101" "$FIX/kills.log"
check    "lsof 不存在：log 說無法判定、保留" "keep headless pid 1101（port 9601 的 CDP 連線狀態無法判定" "$LOG"
exists   "lsof 不存在：profile 還在" "$TMPD/am-lsof-missing"
teardown
setup
mkprofile "$TMPD/am-lsof-fail"
chrome 1102 1 "10:00" 9602 "$TMPD/am-lsof-fail"; echo fail > "$FIX/lsof.mode"
run >/dev/null
check_no "lsof 權限不足（exit 1＋錯誤輸出）：不殺" "kill -TERM 1102" "$FIX/kills.log"
check    "lsof 權限不足：log 說無法判定" "keep headless pid 1102（port 9602 的 CDP 連線狀態無法判定" "$LOG"
exists   "lsof 權限不足：profile 還在" "$TMPD/am-lsof-fail"
teardown

setup
mkprofile "$TMPD/am-reuse"
chrome 1201 1 "10:00" 9701 "$TMPD/am-reuse"; touch "$FIX/alive.1201"
echo "Mon Oct 11 10:05:00 2026 /usr/bin/sleep 999" > "$FIX/reuse.1201"
run >/dev/null
check    "有送 TERM" "kill -TERM 1201" "$FIX/kills.log"
check_no "身份變了（PID 被回收）：不送 KILL 給無關的行程" "kill -KILL 1201" "$FIX/kills.log"
check    "log 說身份改變、不送 KILL" "pid 1201 已不是原來的行程" "$LOG"
teardown
setup
mkprofile "$TMPD/am-noid"
chrome 1202 1 "10:00" 9702 "$TMPD/am-noid"; rm "$FIX/fp.1202"; touch "$FIX/alive.1202"
run >/dev/null
check_no "讀不到行程身份：不送任何訊號（fail closed）" "kill -" "$FIX/kills.log"
check    "讀不到身份：log 保留" "keep headless pid 1202（讀不到行程身份" "$LOG"
exists   "讀不到身份：profile 還在" "$TMPD/am-noid"
teardown

setup
gui="$TMPD/am-gui"; mkprofile "$gui"; touch -t 202601010000 "$gui" "$gui/Local State"
echo "608 1 /Applications/Google Chrome.app/Contents/MacOS/Google Chrome --user-data-dir=$gui" >> "$FIX/ps.txt"
run >/dev/null
exists "GUI Chrome 在用的舊 profile 不刪" "$gui"
check  "log 計數 刪 0 個" "profile 目錄：刪掉 0 個" "$LOG"
teardown

setup
spaced="$TMPD/am-my photos"; mkprofile "$spaced"; touch -t 202601010000 "$spaced" "$spaced/Local State"
chrome 1301 4321 "10:00" 9801 "$spaced"
run >/dev/null
exists "含空白的使用中 profile 不刪" "$spaced"
check  "log 計數 刪 0 個" "profile 目錄：刪掉 0 個" "$LOG"
teardown
setup
spaced="$TMPD/am-my photos"; mkprofile "$spaced"; touch -t 202601010000 "$spaced" "$spaced/Local State"
bare="$TMPD/am-my"; mkprofile "$bare"; touch -t 202601010000 "$bare" "$bare/Local State"
chrome 1302 1 "10:00" 9802 "$spaced"
run >/dev/null
check  "孤兒含空白 profile：行程照收" "reap headless pid 1302" "$LOG"
exists "截斷成 am-my 不能誤刪同前綴的目錄" "$bare"
teardown

echo "$PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
