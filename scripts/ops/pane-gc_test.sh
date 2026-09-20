#!/bin/bash
# pane-gc.sh 的隔離測試（issue #318）：假的 herdr、假的 ps，測試裡不會真的關任何 pane。
# 腳本開頭那行 export PATH（把 /opt/homebrew/bin、~/.local/bin 排到最前面）在測試用的副本裡拿掉，
# 否則真的 herdr 會蓋過假的；其餘一字不改，並先確認那行還在，腳本改了形狀測試要跟著改。
# 測的是破壞性那一條：只關「卡住太久的互動式登入」，選不到就什麼都不做。
#
#   bash scripts/ops/pane-gc_test.sh
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
  ROOT=$(mktemp -d); FIX="$ROOT/fix"
  mkdir -p "$ROOT/agm/bin" "$ROOT/fakebin" "$FIX"
  [ "$(grep -c '^export PATH="/opt/homebrew/bin:' "$HERE/pane-gc.sh")" = 1 ] || { echo "FAIL - pane-gc.sh 的 export PATH 那行不見了，測試的中和步驟要更新"; exit 1; }
  grep -v '^export PATH="/opt/homebrew/bin:' "$HERE/pane-gc.sh" > "$ROOT/agm/bin/pane-gc.sh"
  LOG="$ROOT/agm/browser-gc.log"; : > "$LOG"; : > "$FIX/calls.log"
  echo '{"result":{"panes":[]}}' > "$FIX/panes.json"
  export FIX
  # 假 herdr：只認 pane list／process-info／get／close；其餘 exit 2。close 只記錄。
  cat > "$ROOT/fakebin/herdr" <<'STUB'
#!/bin/bash
echo "$*" >> "$FIX/calls.log"
case "$1 $2" in
  "pane list") cat "$FIX/panes.json" ;;
  "pane process-info") cat "$FIX/info.$4" 2>/dev/null ;;
  "pane get") cat "$FIX/get.$3" 2>/dev/null ;;
  "pane close") echo "CLOSE $3" >> "$FIX/closed.log"; [ -f "$FIX/closefail" ] && exit 1; exit 0 ;;
  *) echo "herdr stub: unknown $*" >&2; exit 2 ;;
esac
STUB
  # 假 ps：只認 -o etime= -p PID，從 $FIX/etime.PID 讀；沒有檔＝ps 查不到（行程已死）。
  cat > "$ROOT/fakebin/ps" <<'STUB'
#!/bin/bash
[ "$1 $2" = "-o etime=" ] && [ "$3" = "-p" ] || { echo "ps stub: unknown $*" >&2; exit 2; }
cat "$FIX/etime.$4" 2>/dev/null
STUB
  chmod +x "$ROOT/fakebin/herdr" "$ROOT/fakebin/ps"
  : > "$FIX/closed.log"
  unset PANE_GC_MAX_AGE
}
teardown() { rm -rf "$ROOT"; unset FIX; }
run() { PATH="$ROOT/fakebin:$PATH" zsh "$ROOT/agm/bin/pane-gc.sh"; echo $?; }
# pane <id> <pid> <cmdline> <etime>：登記一個 pane、它的前景程式與年齡。
pane() {
  python3 - "$FIX/panes.json" "$1" <<'PY'
import json, sys
p = json.load(open(sys.argv[1])); p["result"]["panes"].append({"pane_id": sys.argv[2]}); json.dump(p, open(sys.argv[1], "w"))
PY
  printf '{"result":{"process_info":{"foreground_processes":[{"pid":%s,"cmdline":"%s"}]}}}' "$2" "$3" > "$FIX/info.$1"
  [ -n "${4:-}" ] && echo "$4" > "$FIX/etime.$2"
  return 0
}
closed() { grep -c '^CLOSE' "$FIX/closed.log" | tr -d ' '; }

# 1. 卡住超過 24 小時的登入：四種都關；只關它們，bot 與閒置 shell 不碰。
setup
pane w1:p1 101 "claude auth login" "2-03:00:00"
pane w1:p2 102 "/opt/x/gcloud.py auth login --no-launch-browser" "1-00:00:00"
pane w1:p3 103 "gcloud auth login" "5-00:00:00"
pane w1:p4 104 "codex login" "3-12:00:00"
pane w1:p5 105 "claude --resume abc" "9-00:00:00"
pane w1:p6 106 "codex --model gpt-5" "9-00:00:00"
pane w1:p7 107 "-zsh" "9-00:00:00"
pane w1:p8 108 "vim notes.md" "9-00:00:00"
equals "exit 0" "$(run)" "0"
equals "只關 4 個" "$(closed)" "4"
check  "關 claude auth login" "CLOSE w1:p1" "$FIX/closed.log"
check  "關 gcloud.py auth login" "CLOSE w1:p2" "$FIX/closed.log"
check  "關 gcloud auth login" "CLOSE w1:p3" "$FIX/closed.log"
check  "關 codex login" "CLOSE w1:p4" "$FIX/closed.log"
check_no "bot 的 claude 不碰" "w1:p5" "$FIX/closed.log"
check_no "bot 的 codex 不碰" "w1:p6" "$FIX/closed.log"
check_no "閒置 shell 不碰" "w1:p7" "$FIX/closed.log"
check_no "別的互動程式不碰" "w1:p8" "$FIX/closed.log"
check  "log 記關掉 4" "pane：關掉 4／幽靈 0" "$LOG"
teardown

# 2. 年齡門檻：剛好 24h 關、差一秒不關；ps 查不到年齡也不關；PANE_GC_MAX_AGE 可覆寫。
setup
pane w1:p1 101 "claude auth login" "1-00:00:00"
pane w1:p2 102 "codex login" "23:59:59"
pane w1:p3 103 "gcloud auth login" "07:00"
pane w1:p4 104 "claude auth login" ""
run >/dev/null
check    "剛好 86400 秒就關" "CLOSE w1:p1" "$FIX/closed.log"
check_no "差一秒不關" "w1:p2" "$FIX/closed.log"
check_no "才 7 分鐘不關" "w1:p3" "$FIX/closed.log"
check_no "查不到年齡（ps 沒回）不關" "w1:p4" "$FIX/closed.log"
: > "$FIX/closed.log"
PANE_GC_MAX_AGE=60 run >/dev/null
check    "PANE_GC_MAX_AGE=60 時 7 分鐘的登入被關" "CLOSE w1:p3" "$FIX/closed.log"
check    "PANE_GC_MAX_AGE=60 時 23:59:59 的也關" "CLOSE w1:p2" "$FIX/closed.log"
check_no "年齡查不到還是不關" "w1:p4" "$FIX/closed.log"
teardown

# 3. 選不到就什麼都不做：沒有 pane、全是 bot／shell、herdr 吐壞掉的 JSON、pane 資訊空白。
setup
equals "沒有 pane 也 exit 0" "$(run)" "0"
equals "沒有 pane：不關任何東西" "$(closed)" "0"
check  "沒有 pane：log 記 0／0" "pane：關掉 0／幽靈 0" "$LOG"
teardown
setup
pane w1:p1 101 "claude" "9-00:00:00"; pane w1:p2 102 "-zsh" "9-00:00:00"
run >/dev/null
equals "只有 bot 與 shell：不關" "$(closed)" "0"
teardown
setup
pane w1:p1 101 "claude auth login" "9-00:00:00"
echo '{"result":{"process_info":{"foreground_processes":[{"pid":101,"cmdline":"claude auth login "quoted""}]}}}' > "$FIX/info.w1:p1"
run >/dev/null
equals "herdr 吐不合法 JSON：跳過、不關" "$(closed)" "0"
teardown

# 4. 幽靈 pane：list 有、get 說 pane_not_found → 只記錄，不關；get 找得到的空資訊不算幽靈。
setup
pane w1:p1 101 "x" ""; : > "$FIX/info.w1:p1"; echo '{"error":{"code":"pane_not_found"}}' > "$FIX/get.w1:p1"
pane w1:p2 102 "x" ""; : > "$FIX/info.w1:p2"; echo '{"result":{"pane":{}}}' > "$FIX/get.w1:p2"
run >/dev/null
equals "幽靈 pane 不關" "$(closed)" "0"
check  "幽靈 pane 有記錄" "ghost pane w1:p1" "$LOG"
check_no "get 找得到的不算幽靈" "ghost pane w1:p2" "$LOG"
check  "計數：關 0／幽靈 1" "pane：關掉 0／幽靈 1" "$LOG"
teardown

# 5. pane close 失敗：不算進關掉的數字。
setup
pane w1:p1 101 "codex login" "9-00:00:00"; touch "$FIX/closefail"
run >/dev/null
check    "有嘗試關" "CLOSE w1:p1" "$FIX/closed.log"
check    "關失敗不算數" "pane：關掉 0／幽靈 0" "$LOG"
check_no "關失敗不寫 close pane 那行" "close pane" "$LOG"
teardown

# 6. PATH 上沒有 herdr：記 log、exit 0、什麼都不做。
setup
mkdir "$ROOT/nobin"; ln -s "$(command -v dirname)" "$ROOT/nobin/dirname"
env -i PATH="$ROOT/nobin" FIX="$FIX" /bin/zsh "$ROOT/agm/bin/pane-gc.sh"; equals "沒有 herdr exit 0" "$?" "0"
check  "沒有 herdr 有記 log" "herdr 不在 PATH" "$LOG"
equals "沒有 herdr 沒呼叫任何東西" "$(wc -l < "$FIX/calls.log" | tr -d ' ')" "0"
teardown

echo "$PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
