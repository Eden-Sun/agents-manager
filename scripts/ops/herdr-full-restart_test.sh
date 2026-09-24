#!/bin/bash
# herdr-full-restart.sh 的隔離測試（issue #418）：測試裡不會真的 bootout 任何 launchd job、
# 不碰真的 herdr socket、不殺任何行程。
#
# **危險指令一律用注入的 shell 函式攔截，不靠 PATH 上的替身**：原腳本自己 `export PATH=…`，
# 用 PATH 擋的話只要某個替身沒放好就會 fallthrough 打到 /bin/launchctl 真貨。
#（2026-09-24 就是這樣真的把 gui/501 的兩個 herdr job bootout 掉，連自己的 pane 都被收掉。）
# 函式優先於 PATH 查找，而且 `export PATH=` 蓋不掉函式，所以 launchctl／pkill／pgrep／herdr／sleep
# 一律在副本開頭定義成函式；「缺依賴」的情境也是讓函式回 127，不是把替身拿掉。
#
# 測試用的副本另外只換路徑（原腳本的判斷一字不改）：LOG、herdr 設定目錄、herdr binary、
# plist 目錄、cd 的目標、/tmp/herdr-default.log。
#
# 測的是主要決策路徑：先 bootout 再殺 server（順序反了 launchd 會把 server 拉回來）、
# TERM 之後補 KILL、只刪清單上的 unix socket、缺依賴時 rc 有進 log。
#
#   bash scripts/ops/herdr-full-restart_test.sh
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
SRC="${HERE}/herdr-full-restart.sh"
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

# 路徑替換的前提：原腳本裡真的有這些字串。沒有就是腳本改過，測試要跟著更新，不要默默跑一個空殼。
for pat in '/Users/m4p/.config/agents-manager/supervisor/AGM/herdr-full-restart.log' \
           '/Users/m4p/.config/herdr' '/opt/homebrew/bin/herdr' \
           '/Users/m4p/Library/LaunchAgents' '/tmp/herdr-default.log' 'cd /Users/m4p'; do
  grep -q -- "${pat}" "${SRC}" || { echo "FAIL - 原腳本找不到 '${pat}'，測試的路徑替換要更新"; exit 1; }
done

mksock() { python3 -c 'import socket,sys
s=socket.socket(socket.AF_UNIX); s.bind(sys.argv[1])' "$1"; }

setup() {
  ROOT="$(mktemp -d)"
  FIX="$ROOT/fix"; HERDR="$ROOT/herdr"
  BIN="$ROOT/fakebin"
  mkdir -p "$FIX" "$HERDR/sessions/a" "$HERDR/sessions/b" "$ROOT/LaunchAgents" "$ROOT/agm/bin" "$BIN"
  # nohup／setsid 是外部指令，看不到下面注入的 shell 函式，所以 herdr 另外放一個檔案樁；
  # PATH 也換成只有這個樁＋系統工具，真的 herdr／launchctl 不在裡面。
  printf '#!/bin/bash\necho "herdr $*" >> "%s/calls.log"\n' "$FIX" > "$BIN/herdr"
  chmod 755 "$BIN/herdr"
  LOG="$ROOT/agm/herdr-full-restart.log"
  SCRIPT="$ROOT/agm/bin/herdr-full-restart.sh"
  echo "7422
7459" > "$FIX/servers"
  echo 0 > "$FIX/launchctl.rc"
  echo running > "$FIX/default.status"
  {
    echo '#!/bin/bash'
    # 危險指令的攔截器。全部只記錄，rc 由 fixture 決定；真的 binary 一次都碰不到。
    echo "FIX='${FIX}'"
    echo 'launchctl() { echo "launchctl $*" >> "$FIX/calls.log"; return "$(cat "$FIX/launchctl.rc")"; }'
    echo 'pkill() { echo "pkill $*" >> "$FIX/calls.log"; case "$1" in -TERM) : > "$FIX/servers" ;; esac; return 0; }'
    echo 'pgrep() { case "$*" in *-fl*) awk "{print \$1\" /opt/homebrew/bin/herdr server\"}" "$FIX/servers" ;; *) cat "$FIX/servers" ;; esac; }'
    echo 'sleep() { :; }'
    # `session list` 的狀態由 fixture 決定：#455 之後腳本要靠它判斷 default 到底起來了沒有，
    # 兩個方向（running／stopped）都要測得到。
    echo 'herdr() { echo "herdr $*" >> "$FIX/calls.log"; case "$*" in "session list") echo "default              $(cat "$FIX/default.status")" ;; esac; return 0; }'
    tail -n +2 "$SRC" \
      | sed -e "s#/Users/m4p/.config/agents-manager/supervisor/AGM/herdr-full-restart.log#${LOG}#g" \
            -e "s#/Users/m4p/.config/herdr#${HERDR}#g" \
            -e "s#^export PATH=.*#export PATH=${BIN}:/usr/bin:/bin#" \
            -e "s#/opt/homebrew/bin/herdr#${BIN}/herdr#g" \
            -e "s#/Users/m4p/Library/LaunchAgents#${ROOT}/LaunchAgents#g" \
            -e "s#/tmp/herdr-default.log#${ROOT}/herdr-default.log#g" \
            -e "s#cd /Users/m4p #cd ${ROOT} #"
  } > "$SCRIPT"
  chmod 755 "$SCRIPT"
  : > "$LOG"
}
teardown() { rm -rf "$ROOT"; }
run() { bash "$SCRIPT"; echo $?; }

# 副本真的攔住了嗎：跑之前先確認沒有任何一行會叫到 PATH 上的 launchctl／pkill。
setup
equals "副本裡 launchctl 已被函式攔截" "$(grep -c '^launchctl() {' "$SCRIPT")" "1"
equals "副本裡 pkill 已被函式攔截" "$(grep -c '^pkill() {' "$SCRIPT")" "1"
# canary-gap: 這一行是**斷言字串**，不是指派——它在檢查副本裡被改寫成的 PATH 長什麼樣。
# 這支測試本來就不靠 PATH 擋（檔頭寫了理由）：launchctl／pkill／pgrep／herdr 都用注入的
# shell 函式攔，函式優先於 PATH 查找，被測腳本自己 `export PATH=` 也蓋不掉。
equals "副本的 PATH 只指到測試用的 fakebin" "$(grep '^export PATH=' "$SCRIPT")" "export PATH=${BIN}:/usr/bin:/bin"
# nohup 看不到函式，所以那一行必須指到檔案樁，不能留真 binary 的路徑。
equals "nohup 起 server 那行指到測試樁" "$(grep -c "^nohup ${BIN}/herdr server" "$SCRIPT")" "1"
# issue #455：macOS 沒有 setsid，`nohup setsid …` 是 nohup 找不到 setsid 直接失敗、herdr 從沒被執行。
equals "不再用 macOS 沒有的 setsid（只看會被執行的行，註解照樣可以解釋原因）" \
  "$(grep -cE '^[^#]*[^[:alnum:]_]setsid|^setsid' "$SCRIPT")" "0"
# `&` 綁整個 `cd … && nohup … &` 清單時，`$!` 是 subshell 的 pid，起不起得來都有值。
# 對**原始檔**斷言，不是對副本：副本的路徑被 sed 改寫過，`cd /Users/m4p` 已經不在了。
equals "起 server 那行沒有跟 cd 串成 && 清單" "$(grep -cE '^[^#]*cd .* && nohup' "$SRC")" "0" 
# 副本裡還提到 /opt/homebrew 的，只准是假 pgrep 印出來的那行字串（不是會被執行的指令）。
equals "副本裡提到真 binary 路徑的只剩假 pgrep 的輸出字串" \
  "$(grep -n '/opt/homebrew' "$SCRIPT" | grep -vc '^[0-9]*:pgrep() {')" "0"
teardown

# 1. 正常路徑：兩個 job 都 bootout、TERM 之後補 KILL、兩個 plist 都 bootstrap、log 有完整段落。
setup
equals "正常跑 exit 0" "$(run)" "0"
check "log 有開頭" "== full restart start" "$LOG"
check "記下重啟前的 server pid" "servers before: 7422 7459" "$LOG"
check "bootout agents-manager" "bootout gui/501/dev.agents-manager.herdr-agents-manager" "$FIX/calls.log"
check "bootout am-attach-remote" "bootout gui/501/dev.agents-manager.herdr-am-attach-remote" "$FIX/calls.log"
check "bootout 的 rc 有進 log" "bootout agents-manager rc=0" "$LOG"
check "先送 TERM" "pkill -TERM -f herdr.\*server" "$FIX/calls.log"
check "再補 KILL" "pkill -KILL -f herdr.\*server" "$FIX/calls.log"
check "TERM 之後 server 清空（log 的 after kill 是空的）" "servers after kill: $" "$LOG"
check "bootstrap agents-manager" "bootstrap gui/501 $ROOT/LaunchAgents/dev.agents-manager.herdr-agents-manager.plist" "$FIX/calls.log"
check "bootstrap am-attach-remote" "bootstrap gui/501 $ROOT/LaunchAgents/dev.agents-manager.herdr-am-attach-remote.plist" "$FIX/calls.log"
check "收尾問 session list" "herdr session list" "$FIX/calls.log"
check "log 有結尾" "== done" "$LOG"
# 順序：bootout 一定要在 pkill 之前，否則 launchd 會馬上把 server 拉回來又被殺。
equals "bootout 排在 pkill 前面" \
  "$(awk '{print $1}' "$FIX/calls.log" | head -3 | tr '\n' ',')" \
  "launchctl,launchctl,pkill,"
teardown

# 2. socket 只刪清單上的那四種，而且只刪真的 unix socket。
setup
mksock "$HERDR/herdr.sock"
mksock "$HERDR/herdr-client.sock"
mksock "$HERDR/sessions/a/herdr.sock"
echo "不是 socket" > "$HERDR/sessions/a/herdr-client.sock"
mksock "$HERDR/sessions/b/other.sock"
mksock "$HERDR/keep.sock"
run >/dev/null
gone   "根目錄的 herdr.sock 被清掉" "$HERDR/herdr.sock"
gone   "根目錄的 herdr-client.sock 被清掉" "$HERDR/herdr-client.sock"
gone   "session 底下的 herdr.sock 被清掉" "$HERDR/sessions/a/herdr.sock"
exists "同名但不是 socket 的普通檔不刪（-S 守衛）" "$HERDR/sessions/a/herdr-client.sock"
exists "清單外的 socket 不刪" "$HERDR/sessions/b/other.sock"
exists "根目錄其他 socket 不刪" "$HERDR/keep.sock"
teardown

# 3. 缺依賴：launchctl 不在（回 127）時不會中止（原腳本沒有 set -e），但 rc 會進 log——
#    出事看得出來是哪一步。注意這裡是讓攔截函式回 127，不是把替身拿掉：拿掉會打到真 binary。
setup
echo 127 > "$FIX/launchctl.rc"
equals "launchctl 回 127 仍 exit 0（現況）" "$(run)" "0"
check "bootout 的 rc=127 有記下來" "bootout agents-manager rc=127" "$LOG"
check "bootstrap 的 rc=127 也記下來" "bootstrap agents-manager rc=127" "$LOG"
check "缺依賴不影響後面的收尾" "== done" "$LOG"
teardown

# 4. issue #455：以前這裡釘的是 bug 本身（macOS 沒有 setsid，`nohup setsid …` 起不來，
#    下一行卻照樣寫「default server started」）。現在釘的是修好之後的行為——**不依賴這台機器
#    有沒有 setsid**：腳本裡已經沒有 setsid 了，所以兩種機器上結果都一樣。
setup
run >/dev/null
# 那一行是真的背景行程（nohup … &），腳本結束時它不一定已經被排到——這裡等它，
# 不是放寬斷言：等不到就照樣 FAIL。
for _ in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20; do
  grep -q 'herdr server' "$FIX/calls.log" && break
  sleep 0.1
done
check "herdr server 真的被執行了" "herdr server" "$FIX/calls.log"
check_no "log 不再用「started」宣稱成功" "default server started pid=" "$LOG"
check "改成確認過 default 真的 running 才記 up" "default server up pid=" "$LOG"
teardown

# 5. issue #455：default 起不來時要非零退出並留下可以查的 log，不能像以前那樣靜默宣告成功。
#    daemon 的 `ensure_session` 明文拒絕代起 `default`，所以這是唯一沒有自癒路徑的 session。
setup
echo stopped > "$FIX/default.status"
equals "default 起不來要非零退出" "$(run)" "1"
check "log 寫明是 FAIL" "FAIL: default server 沒起來" "$LOG"
check "log 說明 pid 不代表起來了" "只代表 fork 成功" "$LOG"
check_no "不能同時又說 up" "default server up pid=" "$LOG"
check "收尾仍然標明結束" "== done (failed)" "$LOG"
teardown

echo "$PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
