#!/bin/bash
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
SRC="${HERE}/herdr-full-restart.sh"
PASS=0
FAIL=0
. "$HERE/../test-assertions.sh" || exit 1
exists() { [ -e "$2" ] && { echo "ok   - $1"; PASS=$((PASS + 1)); } || { echo "FAIL - $1（$2 不見了）"; FAIL=$((FAIL + 1)); }; }
gone() { [ ! -e "$2" ] && { echo "ok   - $1"; PASS=$((PASS + 1)); } || { echo "FAIL - $1（$2 還在）"; FAIL=$((FAIL + 1)); }; }

for pat in '/Users/me/.config/agents-manager/supervisor/AGM/herdr-full-restart.log' \
           '/Users/me/.config/herdr' '/opt/homebrew/bin/herdr' \
           '/Users/me/Library/LaunchAgents' '/tmp/herdr-default.log' 'cd /Users/me'; do
  grep -q -- "${pat}" "${SRC}" || { echo "FAIL - 原腳本找不到 '${pat}'，測試的路徑替換要更新"; exit 1; }
done

mksock() { python3 -c 'import socket,sys
s=socket.socket(socket.AF_UNIX); s.bind(sys.argv[1])' "$1"; }

setup() {
  ROOT="$(mktemp -d)"
  FIX="$ROOT/fix"; HERDR="$ROOT/herdr"
  BIN="$ROOT/fakebin"
  mkdir -p "$FIX" "$HERDR/sessions/a" "$HERDR/sessions/b" "$ROOT/LaunchAgents" "$ROOT/agm/bin" "$BIN"
  printf '#!/bin/bash\necho "herdr $*" >> "%s/calls.log"\n' "$FIX" > "$BIN/herdr"
  chmod 755 "$BIN/herdr"
  LOG="$ROOT/agm/herdr-full-restart.log"
  SCRIPT="$ROOT/agm/bin/herdr-full-restart.sh"
  echo "7422
7459" > "$FIX/servers"
  echo 0 > "$FIX/launchctl.rc"
  echo running > "$FIX/default.status"
  echo Darwin > "$FIX/uname"
  : > "$FIX/units"; echo 0 > "$FIX/active.rc"
  {
    echo '#!/bin/bash'
    echo "FIX='${FIX}'"
    echo 'launchctl() { echo "launchctl $*" >> "$FIX/calls.log"; return "$(cat "$FIX/launchctl.rc")"; }'
    echo 'kill() { echo "kill $*" >> "$FIX/calls.log"; case "$1" in -TERM) [ -n "${STUB_TERM_IGNORED:-}" ] || : > "$FIX/servers" ;; esac; return 0; }'
    echo 'ps() { while read -r p; do [ -n "$p" ] && echo "$p herdr --session s$p server"; done < "$FIX/servers"
      echo "9001 claude --dangerously-skip-permissions --append-system-prompt 你是 bot，要用 herdr 開 pane、不要碰 server"
      echo "9002 codex -c developer_instructions=herdr rules for the server"
      echo "9003 /usr/bin/ssh agm-host herdr --session x server-log"
      echo "9004 herdr pane list server"; }'
    echo 'sleep() { :; }'
    echo 'uname() { cat "$FIX/uname"; }'
    echo 'systemctl() { echo "systemctl $* XDG_RUNTIME_DIR=${XDG_RUNTIME_DIR:-}" >> "$FIX/calls.log"; case "$2" in list-units) cat "$FIX/units" ;; is-active) return "$(cat "$FIX/active.rc")" ;; esac; return 0; }'
    echo 'herdr() { echo "herdr $*" >> "$FIX/calls.log"; case "$*" in "session list") echo "default              $(cat "$FIX/default.status")" ;; esac; return 0; }'
    tail -n +2 "$SRC" \
      | sed -e "s#/Users/me/.config/agents-manager/supervisor/AGM/herdr-full-restart.log#${LOG}#g" \
            -e "s#/Users/me/.config/herdr#${HERDR}#g" \
            -e "s#^export PATH=.*#export PATH=${BIN}:/usr/bin:/bin#" \
            -e "s#/opt/homebrew/bin/herdr#${BIN}/herdr#g" \
            -e "s#/Users/me/Library/LaunchAgents#${ROOT}/LaunchAgents#g" \
            -e "s#/tmp/herdr-default.log#${ROOT}/herdr-default.log#g" \
            -e "s#cd /Users/me #cd ${ROOT} #"
  } > "$SCRIPT"
  chmod 755 "$SCRIPT"
  : > "$LOG"
}
teardown() { rm -rf "$ROOT"; }
run() { bash "$SCRIPT"; echo $?; }

setup
equals "副本裡 launchctl 已被函式攔截" "$(grep -c '^launchctl() {' "$SCRIPT")" "1"
equals "副本裡 kill／ps 已被函式攔截" "$(grep -cE '^(kill|ps)\(\) \{' "$SCRIPT")" "2"
# canary-gap: 這一行是**斷言字串**，不是指派——它在檢查副本裡被改寫成的 PATH 長什麼樣。
equals "副本的 PATH（macOS 與 Linux 兩段）都只指到測試用的 fakebin" "$(grep '^export PATH=' "$SCRIPT" | sort -u)" "export PATH=${BIN}:/usr/bin:/bin"
equals "副本裡 systemctl 已被函式攔截" "$(grep -c '^systemctl() {' "$SCRIPT")" "1"
equals "nohup 起 server 那行指到測試樁" "$(grep -c "^nohup ${BIN}/herdr server" "$SCRIPT")" "1"
equals "不再用 macOS 沒有的 setsid（只看會被執行的行，註解照樣可以解釋原因）" \
  "$(grep -cE '^[^#]*[^[:alnum:]_]setsid|^setsid' "$SCRIPT")" "0"
equals "起 server 那行沒有跟 cd 串成 && 清單" "$(grep -cE '^[^#]*cd .* && nohup' "$SRC")" "0" 
equals "副本裡提到真 binary 路徑的只剩假 pgrep 的輸出字串" \
  "$(grep -c '/opt/homebrew' "$SCRIPT")" "0"
teardown

setup
equals "正常跑 exit 0" "$(run)" "0"
check "log 有開頭" "== full restart start" "$LOG"
check "記下重啟前的 server pid" "servers before: 7422 7459" "$LOG"
check "bootout agents-manager" "bootout gui/501/dev.agents-manager.herdr-agents-manager" "$FIX/calls.log"
check "bootout am-attach-remote" "bootout gui/501/dev.agents-manager.herdr-am-attach-remote" "$FIX/calls.log"
check "bootout 的 rc 有進 log" "bootout agents-manager rc=0" "$LOG"
check "先對 server 的 pid 送 TERM" "kill -TERM 7422" "$FIX/calls.log"
check "兩顆 server 都收到 TERM" "kill -TERM 7459" "$FIX/calls.log"
check_no "不再用 pkill -f 的 pattern 比對" "pkill" "$SCRIPT"
check_no "別人的行程（argv 恰好有 herdr 與 server）一顆都沒被碰" "kill .* 900[1-4]" "$FIX/calls.log"
check "TERM 之後 server 清空（log 的 after kill 是空的）" "servers after kill: $" "$LOG"
check "bootstrap agents-manager" "bootstrap gui/501 $ROOT/LaunchAgents/dev.agents-manager.herdr-agents-manager.plist" "$FIX/calls.log"
check "bootstrap am-attach-remote" "bootstrap gui/501 $ROOT/LaunchAgents/dev.agents-manager.herdr-am-attach-remote.plist" "$FIX/calls.log"
check "收尾問 session list" "herdr session list" "$FIX/calls.log"
check "log 有結尾" "== done" "$LOG"
equals "bootout 排在 kill 前面" \
  "$(awk '{print $1}' "$FIX/calls.log" | head -3 | tr '\n' ',')" \
  "launchctl,launchctl,kill,"
teardown

setup
export STUB_TERM_IGNORED=1
run >/dev/null
unset STUB_TERM_IGNORED
check "TERM 沒效時對 server pid 補 KILL" "kill -KILL 7422" "$FIX/calls.log"
check "另一顆 server 也補 KILL" "kill -KILL 7459" "$FIX/calls.log"
check_no "KILL 沒有波及 argv 恰好有 herdr／server 的 claude／codex／ssh／client" "kill .* 900[1-4]" "$FIX/calls.log"
teardown

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

setup
echo 127 > "$FIX/launchctl.rc"
equals "launchctl 回 127 仍 exit 0（現況）" "$(run)" "0"
check "bootout 的 rc=127 有記下來" "bootout agents-manager rc=127" "$LOG"
check "bootstrap 的 rc=127 也記下來" "bootstrap agents-manager rc=127" "$LOG"
check "缺依賴不影響後面的收尾" "== done" "$LOG"
teardown

setup
run >/dev/null
for _ in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20; do
  grep -q 'herdr server' "$FIX/calls.log" && break
  sleep 0.1
done
check "herdr server 真的被執行了" "herdr server" "$FIX/calls.log"
check_no "log 不再用「started」宣稱成功" "default server started pid=" "$LOG"
check "改成確認過 default 真的 running 才記 up" "default server up pid=" "$LOG"
teardown

setup
echo stopped > "$FIX/default.status"
equals "default 起不來要非零退出" "$(run)" "1"
check "log 寫明是 FAIL" "FAIL: default server 沒起來" "$LOG"
check "log 說明 pid 不代表起來了" "只代表 fork 成功" "$LOG"
check_no "不能同時又說 up" "default server up pid=" "$LOG"
check "收尾仍然標明結束" "== done (failed)" "$LOG"
teardown

LHOME=""
setup_linux() {
  setup
  echo Linux > "$FIX/uname"
  printf 'herdr@agents-manager.service loaded active running herdr server\nherdr@am-attach-remote.service loaded active running herdr server\n' > "$FIX/units"
  LHOME="$ROOT/home"
  mkdir -p "$LHOME/.config/herdr/sessions/a"
  LLOG="$LHOME/.config/agents-manager/supervisor/AGM/herdr-full-restart.log"
}
run_linux() { ( unset XDG_RUNTIME_DIR; HOME="$LHOME" bash "$SCRIPT"; echo $? ) | tail -1; }

setup_linux
mksock "$LHOME/.config/herdr/sessions/a/herdr.sock"
mksock "$LHOME/.config/herdr/keep.sock"
equals "Linux 正常跑 exit 0" "$(run_linux)" "0"
check "log 在 HOME 底下、標明 linux" "== full restart start (linux)" "$LLOG"
check "記下原本 active 的 unit" "units before: herdr@agents-manager.service herdr@am-attach-remote.service" "$LLOG"
check "停 unit" "systemctl --user stop herdr@agents-manager.service" "$FIX/calls.log"
check "起回 unit" "systemctl --user start herdr@am-attach-remote.service" "$FIX/calls.log"
check "沒有登入 session 也補上 runtime dir" "XDG_RUNTIME_DIR=/run/user/" "$FIX/calls.log"
check_no "Linux 不碰 launchctl" "launchctl" "$FIX/calls.log"
equals "stop 排在 kill 前面、start 排在 kill 後面" \
  "$(grep -E '^(systemctl --user (stop|start)|kill)' "$FIX/calls.log" | awk '{print $1 $3}' | tr '\n' ',')" \
  "systemctlstop,systemctlstop,kill7422,kill7459,systemctlstart,systemctlstart,"
gone   "HOME 底下 session 的 herdr.sock 被清掉" "$LHOME/.config/herdr/sessions/a/herdr.sock"
exists "清單外的 socket 不刪" "$LHOME/.config/herdr/keep.sock"
for _ in 1 2 3 4 5 6 7 8 9 10 11 12 13 14 15 16 17 18 19 20; do
  grep -q 'herdr server' "$FIX/calls.log" && break
  sleep 0.1
done
check "default 原本在跑就補起" "default server up pid=" "$LLOG"
check "log 有結尾" "== done" "$LLOG"
teardown

setup_linux
echo stopped > "$FIX/default.status"
equals "default 沒在跑也 exit 0" "$(run_linux)" "0"
check "log 說明不補起" "default 原本沒在跑（stopped），不補起" "$LLOG"
sleep 0.3
check_no "沒有起 default" "herdr server" "$FIX/calls.log"
teardown

setup_linux
echo 3 > "$FIX/active.rc"
equals "unit 起不回來要非零退出" "$(run_linux)" "1"
check "log 點名沒回來的 unit" "FAIL: herdr@agents-manager.service 沒回到 active" "$LLOG"
check "收尾標明失敗" "== done (failed)" "$LLOG"
teardown

echo "$PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
