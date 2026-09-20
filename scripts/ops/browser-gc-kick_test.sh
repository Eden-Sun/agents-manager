#!/bin/bash
# browser-gc-kick.sh 的隔離測試（issue #318）：假的 ps／lsof／bin/agm，測試裡不會真的殺行程或刪真的 /tmp/am-*。
# 測試用的副本做三件事（原腳本一字不改的部分照跑）：開頭補上 kill／sleep 的替身函式（kill 是 shell 內建，
# 只能用函式蓋掉）、把 /tmp/am- 換成暫存目錄底下的 tmp/am-、把 pane-gc.sh 換成只記錄的樁（它自己另有測試）。
# 測的是破壞性兩條：headless Chrome 只在「孤兒＋沒有 CDP 連線＋活超過 2 分鐘」才收；profile 目錄只刪
# 「長得像 Chrome profile、沒人在用、1 小時沒動過」的，選不到就什麼都不做。
#
#   bash scripts/ops/browser-gc-kick_test.sh
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
PASS=0
FAIL=0
check() {
  if grep -q -- "$2" "$3" 2>/dev/null; then echo "ok   - $1"; PASS=$((PASS + 1))
  else echo "FAIL - $1"; echo "      找不到 '$2'，實際內容："; sed 's/^/      /' "$3" 2>/dev/null; FAIL=$((FAIL + 1)); fi
}
check_no() {
  if grep -q -- "$2" "$3" 2>/dev/null; then echo "FAIL - $1"; echo "      不該有 '$2'"; FAIL=$((FAIL + 1))
  else echo "ok   - $1"; PASS=$((PASS + 1)); fi
}
equals() {
  if [ "$2" = "$3" ]; then echo "ok   - $1"; PASS=$((PASS + 1))
  else echo "FAIL - $1（是 '$2'，預期 '$3'）"; FAIL=$((FAIL + 1)); fi
}
exists() { [ -e "$2" ] && { echo "ok   - $1"; PASS=$((PASS + 1)); } || { echo "FAIL - $1（$2 不見了）"; FAIL=$((FAIL + 1)); }; }
gone() { [ ! -e "$2" ] && { echo "ok   - $1"; PASS=$((PASS + 1)); } || { echo "FAIL - $1（$2 還在）"; FAIL=$((FAIL + 1)); }; }

setup() {
  ROOT=$(mktemp -d); FIX="$ROOT/fix"; TMPD="$ROOT/tmp"
  mkdir -p "$ROOT/agm/bin" "$ROOT/fakebin" "$FIX" "$TMPD"
  grep -q '/tmp/am-' "$HERE/browser-gc-kick.sh" || { echo "FAIL - 腳本裡沒有 /tmp/am-，測試的路徑替換要更新"; exit 1; }
  {
    echo '#!/bin/zsh'
    echo 'kill() { echo "kill $*" >> "$FIX/kills.log"; if [ "$1" = "-0" ]; then [ -e "$FIX/alive.$2" ]; return; fi; return 0; }'
    echo 'sleep() { :; }'
    tail -n +2 "$HERE/browser-gc-kick.sh" | sed "s#/tmp/am-#$TMPD/am-#g"
  } > "$ROOT/agm/bin/browser-gc-kick.sh"
  echo '#!/bin/bash
echo "pane-gc called" >> "$FIX/kills.log"' > "$ROOT/agm/bin/pane-gc.sh"
  echo "TASK-BODY-MARKER" > "$ROOT/agm/browser-gc-task.md"
  # 假 agm：state 依 $FIX/agm-state 回 running／stopped；其餘只記錄。
  cat > "$ROOT/agm/bin/agm" <<'STUB'
#!/bin/bash
echo "$*" >> "$FIX/agm.log"
case "$*" in
  *" state"*) printf '{"bots":[{"id":"01M248GA4H1TAHJCZRKVR73S3C","run":%s}]}' "$(cat "$FIX/agm-state" 2>/dev/null || echo '{"state":"running"}')" ;;
esac
STUB
  # 假 ps：-axo pid,ppid,command 與 -axo command 讀 $FIX/ps.txt（每行 "pid ppid command…"）；-o etime= -p PID 讀 $FIX/etime.PID。
  cat > "$ROOT/fakebin/ps" <<'STUB'
#!/bin/bash
case "$*" in
  "-axo pid,ppid,command") cat "$FIX/ps.txt" ;;
  "-axo command") awk '{ $1=""; $2=""; sub(/^ +/, ""); print }' "$FIX/ps.txt" ;;
  "-o etime= -p "*) cat "$FIX/etime.$4" 2>/dev/null ;;
  *) echo "ps stub: unknown $*" >&2; exit 2 ;;
esac
STUB
  # 假 lsof：-iTCP:PORT 有 $FIX/estab.PORT 就印一行 ESTABLISHED。
  cat > "$ROOT/fakebin/lsof" <<'STUB'
#!/bin/bash
p=${2#-iTCP:}; [ -e "$FIX/estab.$p" ] && echo "chrome 1 m4p 20u IPv4 TCP 127.0.0.1:$p->127.0.0.1:5555 (ESTABLISHED)"; exit 0
STUB
  chmod +x "$ROOT/agm/bin/"* "$ROOT/fakebin/"*
  LOG="$ROOT/agm/browser-gc.log"; : > "$LOG"; : > "$FIX/kills.log"; : > "$FIX/ps.txt"; : > "$FIX/agm.log"
  export FIX
}
teardown() { rm -rf "$ROOT"; unset FIX; }
run() { PATH="$ROOT/fakebin:$PATH" zsh "$ROOT/agm/bin/browser-gc-kick.sh" >/dev/null 2>&1; echo $?; }
# chrome <pid> <ppid> <etime> <port> <dir> [extra flag]：登記一個 headless Chrome。
chrome() {
  echo "$1 $2 /Applications/Google Chrome.app/Contents/MacOS/Google Chrome --headless=new --remote-debugging-port=$4 --user-data-dir=$5 ${6:-}" >> "$FIX/ps.txt"
  [ -n "$3" ] && echo "$3" > "$FIX/etime.$1"
  return 0
}
kills() { grep -c '^kill -\(TERM\|KILL\)' "$FIX/kills.log" | tr -d ' '; }
mkprofile() { mkdir -p "$1"; touch "$1/Local State"; touch -t 202601010000 "$1" "$1/Local State"; }

# 1. 該收的：孤兒（ppid 1）＋沒 CDP 連線＋活超過 2 分鐘 → TERM、還活著再 KILL、只刪 /tmp/am-* 的 profile。
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

# 2. 不該收的（每一種都要有 keep 的理由）：什麼都不殺、profile 不刪。
setup
mkprofile "$TMPD/am-a"; mkprofile "$TMPD/am-b"; mkprofile "$TMPD/am-c"; mkprofile "$TMPD/am-d"; mkprofile "$TMPD/am-e"
chrome 601 4321 "10:00" 9301 "$TMPD/am-a"            # 父程序還活著
chrome 602 1 "10:00" 9302 "$TMPD/am-b"; touch "$FIX/estab.9302"   # 有 CDP 連線
chrome 603 1 "00:59" 9303 "$TMPD/am-c"               # 剛起
chrome 604 1 "" 9304 "$TMPD/am-d"                    # 算不出年齡
chrome 605 1 "10:00" 9305 "$TMPD/am-e" "--type=renderer"   # 子程序（renderer）不在清單
echo "606 1 /Applications/Google Chrome.app/Contents/MacOS/Google Chrome --user-data-dir=/Users/m4p/Library/Application Support/Google/Chrome" >> "$FIX/ps.txt"   # 使用者自己的 Chrome：沒有 --headless
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

# 3. 沒有任何 headless Chrome：什麼都不殺。
setup
run >/dev/null
equals "沒有 Chrome：不殺" "$(kills)" "0"
check  "沒有 Chrome：收 0／保留 0" "headless Chrome：收掉 0／保留 0" "$LOG"
teardown

# 4. 收掉的 headless 若 profile 不在 /tmp/am-* 底下（使用者的目錄）：殺行程但不刪目錄。
setup
mkdir -p "$ROOT/keepme"; touch "$ROOT/keepme/Local State"
chrome 701 1 "10:00" 9401 "$ROOT/keepme"
run >/dev/null
check "收掉了" "reap headless pid 701" "$LOG"
exists "但不在 /tmp/am-* 的目錄不刪" "$ROOT/keepme/Local State"
teardown

# 5. profile 目錄清理：只刪「像 profile＋沒人用＋1 小時沒動」的 /tmp/am-*。
setup
mkprofile "$TMPD/am-old-localstate"
mkdir -p "$TMPD/am-old-default/Default"; touch -t 202601010000 "$TMPD/am-old-default" "$TMPD/am-old-default/Default"
mkdir -p "$TMPD/am-old-devtools"; touch "$TMPD/am-old-devtools/DevToolsActivePort"; touch -t 202601010000 "$TMPD/am-old-devtools"
mkdir -p "$TMPD/am-recent"; touch "$TMPD/am-recent/Local State"                          # 剛動過
mkdir -p "$TMPD/am-target/debug"; touch -t 202601010000 "$TMPD/am-target" "$TMPD/am-target/debug"   # build target：沒有 profile 標記
mkdir -p "$TMPD/am-shots"; echo png > "$TMPD/am-shots/a.png"; touch -t 202601010000 "$TMPD/am-shots"
mkprofile "$TMPD/am-live"                                                                   # 有 headless Chrome 正在用（但被保留）
chrome 801 4321 "10:00" 9501 "$TMPD/am-live"
mkdir -p "$TMPD/other-old"; touch "$TMPD/other-old/Local State"; touch -t 202601010000 "$TMPD/other-old"   # 名字不是 am-*
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

# 6. 收尾流程：呼叫 pane-gc、bot 已在跑就不 start、沒在跑先 start，最後都派 assign（帶 request-id 與交辦檔）。
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

# 7. /tmp 底下一個 am-* 都沒有（issue #371）：zsh 的 glob 沒符合會 "no matches found" 中止整支，
#    後面的 pane-gc 與派工整輪都不跑。setup 刻意不留任何 am-*。
setup
equals "沒有任何 am-*：exit 0" "$(run)" "0"
check  "沒有任何 am-*：profile 清理照跑、刪 0 個" "profile 目錄：刪掉 0 個" "$LOG"
check  "沒有任何 am-*：pane-gc 照跑" "pane-gc called" "$FIX/kills.log"
check  "沒有任何 am-*：照派 assign" " assign " "$FIX/agm.log"
teardown

echo "$PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
