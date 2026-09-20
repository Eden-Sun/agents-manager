#!/bin/bash
# daemon-swap.sh 的隔離測試。
#
# 完全不碰正式 daemon／正式 DB／正式 AGM 目錄：每個 case 開一個暫存目錄，放一份假的 checkout
# （只有 daemon/src/db.rs 與一個假 binary）、假 repo、假 DB，再把 agm／sqlite3／curl／launchctl／
# pgrep 換成 stub，從 log 檢查腳本做了什麼決定。
#
# 測的是 2026-09-20 那次 33 秒停機的四個根因：
#   1. 預期 schema 版本要從 checkout 讀，不能寫死。
#   2. 回滾還原 DB 要連 -wal／-shm 一起處理，而且要自驗 user_version／integrity_check。
#   3. 升過 schema 的失敗預設往前修，不把舊 binary 放回去（會被版本閘擋下）。
#   4. 啟動要走 launchd（nice 0）＋ setsid，不是在 pane 裡直接背景起。
#
#   bash scripts/ops/daemon-swap_test.sh
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
SCRIPT="$HERE/daemon-swap.sh"
PASS=0
FAIL=0

check() { # check <描述> <要出現的字串> <檔案>
  if grep -q -- "$2" "$3" 2>/dev/null; then
    echo "ok   - $1"; PASS=$((PASS + 1))
  else
    echo "FAIL - $1"; echo "      找不到 '$2'，實際內容："; sed 's/^/      /' "$3"; FAIL=$((FAIL + 1))
  fi
}

check_no() {
  if grep -q -- "$2" "$3" 2>/dev/null; then
    echo "FAIL - $1"; echo "      不該出現 '$2'"; sed 's/^/      /' "$3"; FAIL=$((FAIL + 1))
  else
    echo "ok   - $1"; PASS=$((PASS + 1))
  fi
}

check_file() { # check_file <描述> <檔案應該存在?yes/no> <路徑>
  got=no
  [ -e "$3" ] && got=yes
  if [ "$got" = "$2" ]; then
    echo "ok   - $1"; PASS=$((PASS + 1))
  else
    echo "FAIL - $1（存在=$got，預期=$2：$3）"; FAIL=$((FAIL + 1))
  fi
}

check_eq() { # check_eq <描述> <期望> <實際>
  if [ "$2" = "$3" ]; then
    echo "ok   - $1"; PASS=$((PASS + 1))
  else
    echo "FAIL - $1（預期 '$2'，實際 '$3'）"; FAIL=$((FAIL + 1))
  fi
}

setup() { # setup <checkout 的 SCHEMA_VERSION> <DB 目前的 user_version>
  ROOT=$(mktemp -d); export ROOT
  export AGM_DIR="$ROOT/agm" AGM_REPO="$ROOT/repo" CHECKOUT="$ROOT/checkout"
  export DAEMON_DB="$ROOT/am.sqlite3" DAEMON_LOG="$ROOT/daemon.log" SWAP_LOG="$ROOT/swap.log"
  export SWAP_SETTLE_SECS=0 SWAP_WINDOW_TRIES=1
  export AGM_BIN="$ROOT/bin/agm" SQLITE_BIN="$ROOT/bin/sqlite3" CURL_BIN="$ROOT/bin/curl"
  export LAUNCHCTL_BIN="$ROOT/bin/launchctl" PGREP_BIN="$ROOT/bin/pgrep" HERDR_BIN="$ROOT/bin/herdr"
  export SWAP_PROBE_TRIES=2 SWAP_PROBE_BOT=bot-probe
  export HERDR_PANE_ID="w1:pA"          # 預設：pane 裡本來就有；測 current 的 case 會 unset
  export STUB_PANE_READ_OK=1 STUB_PANE_CURRENT="w1:pA" STUB_PROBE='200 {"delivery":"ok"}' 
  mkdir -p "$AGM_DIR" "$AGM_REPO/target/release" "$CHECKOUT/daemon/src" "$CHECKOUT/target/release" "$ROOT/bin"

  # 假 checkout：SCHEMA_HISTORY 的最後一項就是這顆 binary 認得的版本。
  { echo 'const SCHEMA_HISTORY: &[(i64, &str)] = &['
    echo '    (9, "aaa"),'
    echo '    (10, "bbb"),'
    [ "$1" -ge 11 ] && echo "    ($1, \"ccc\"),"
    echo '];'; } > "$CHECKOUT/daemon/src/db.rs"
  printf 'new-binary\n' > "$CHECKOUT/target/release/agents-managerd"; chmod +x "$CHECKOUT/target/release/agents-managerd"
  /usr/bin/git init -q "$CHECKOUT"
  ( cd "$CHECKOUT" && /usr/bin/git config user.email t@t && /usr/bin/git config user.name t \
      && /usr/bin/git commit -q --allow-empty -m deployed && /usr/bin/git add -A && /usr/bin/git commit -qm init ) >/dev/null 2>&1
  export SHA=$(/usr/bin/git -C "$CHECKOUT" rev-parse HEAD)
  # 線上那一版＝前一個 commit；CD 信任閘門要它在 checkout 的歷史裡。假 gh：沒有 fork PR。
  export OLD=$(/usr/bin/git -C "$CHECKOUT" rev-parse --short HEAD~1)
  cat > "$ROOT/gh" <<'GHSTUB'
#!/bin/bash
case "$1" in pr) printf '[]' ;; api) printf '%s' "${STUB_GH_PULLS:-[]}" ;; esac
GHSTUB
  chmod +x "$ROOT/gh"
  export AGM_GH_BIN="$ROOT/gh" CD_TRUST_SLUG="me/proj" STUB_GH_PULLS=""

  # 假 repo：線上那顆 binary（回滾點的內容）。
  printf 'old-binary\n' > "$AGM_REPO/target/release/agents-managerd"; chmod +x "$AGM_REPO/target/release/agents-managerd"
  export OLDHASH=$(shasum -a 256 "$AGM_REPO/target/release/agents-managerd" | cut -c1-16)

  # 假 DB 檔＋它的 -wal／-shm（回滾時這兩個一定要被清掉）。
  printf 'db@%s\n' "$2" > "$DAEMON_DB"
  printf 'stale-wal\n' > "$DAEMON_DB-wal"
  printf 'stale-shm\n' > "$DAEMON_DB-shm"
  echo "$2" > "$ROOT/uv"          # sqlite3 stub 回的 user_version（migration 會改它）
  echo ok > "$ROOT/integrity"
  : > "$DAEMON_LOG"
  export STUB_UV_AFTER_START="$1"  # 新 binary 起來之後 DB 會被 migrate 到這個版本
  export STUB_SESSION_OK=1 STUB_SESSION_OK_AFTER_FORWARD=1 STUB_HEALTH_OK=1
  export STUB_ACQUIRE_HELD=true STUB_SAFE=true STUB_SUPERVISOR=idle
  export STUB_NAMES_BEFORE='["a","b"]' STUB_NAMES_AFTER='["a","b"]'
  export STUB_RESTART_HELD=false

  cat > "$ROOT/bin/agm" <<'STUB'
#!/bin/bash
echo "$*" >> "$AGM_DIR/calls.log"
sub=""; op=""
for a in "$@"; do
  case "$a" in --*) continue ;; esac
  if [ -z "$sub" ]; then sub="$a"; elif [ -z "$op" ]; then op="$a"; fi
done
case "$sub:$op" in
  lease:acquire) printf '{"lease":{"held":%s,"fence":9},"lease_token":"tok-1"}' "$STUB_ACQUIRE_HELD" ;;
  lease:safety)  printf '{"safe":%s,"working":[],"delivering":[]}' "$STUB_SAFE" ;;
  lease:status)  printf '{"leases":[{"resource":"restart","held":%s}]}' "$STUB_RESTART_HELD" ;;
  lease:release) printf '{"released":true}' ;;
  health:*)      [ -n "$STUB_HEALTH_OK" ] || exit 1; printf '{"status":"healthy"}' ;;
  supervisor:*)  printf '{"status":"%s"}' "$STUB_SUPERVISOR" ;;
  state:*)       if [ -e "$AGM_DIR/started" ]; then names="$STUB_NAMES_AFTER"; else names="$STUB_NAMES_BEFORE"; fi
                 printf '{"bots":['; sep=""
                 for n in $(printf '%s' "$names" | tr -d '[]"' | tr ',' ' '); do printf '%s{"name":"%s"}' "$sep" "$n"; sep=","; done
                 printf ']}' ;;
  *)             printf '{}' ;;
esac
STUB

  cat > "$ROOT/bin/sqlite3" <<'STUB'
#!/bin/bash
db="$1"; q="$2"
case "$q" in
  "pragma user_version")
      if [ "$db" = "$DAEMON_DB" ]; then cat "$ROOT/uv"; else cat "$db.uv" 2>/dev/null || cat "$ROOT/uv"; fi ;;
  "pragma integrity_check") cat "$ROOT/integrity" ;;
  .backup*) dest=${q#.backup }; cp "$db" "$dest"; cp "$ROOT/uv" "$dest.uv" ;;
  *) : ;;
esac
STUB

  cat > "$ROOT/bin/curl" <<'STUB'
#!/bin/bash
# 第二次 submit 之後＝往前修那次重啟；它成不成功由 STUB_SESSION_OK_AFTER_FORWARD 決定。
starts=$(grep -c "^submit" "$AGM_DIR/launchctl.log" 2>/dev/null || echo 0)
if [ "$starts" -ge 2 ]; then [ -n "$STUB_SESSION_OK_AFTER_FORWARD" ] && exit 0 || exit 7; fi
[ -n "$STUB_SESSION_OK" ] && exit 0 || exit 7
STUB

  # launchctl stub：記錄呼叫，submit 時真的跑一次啟動器（它會 fork+setsid 再 exec 假 binary）。
  cat > "$ROOT/bin/launchctl" <<'STUB'
#!/bin/bash
echo "$*" >> "$AGM_DIR/launchctl.log"
case "$1" in
  submit) : > "$AGM_DIR/started"
          echo "$(cat "$AGM_REPO/target/release/agents-managerd")" >> "$AGM_DIR/started-binary.log"
          echo "$STUB_UV_AFTER_START" > "$ROOT/uv" ;;
esac
exit 0
STUB

  cat > "$ROOT/bin/pgrep" <<'STUB'
#!/bin/bash
echo 99999
STUB

  cat > "$ROOT/bin/probe" <<'STUB'
#!/bin/bash
echo "$*" >> "$AGM_DIR/probe.log"
n=$(wc -l < "$AGM_DIR/probe.log" | tr -d ' ')
if [ -n "$STUB_PROBE_FIRST_INFLIGHT" ] && [ "$n" = 1 ]; then
  printf '409 {"error":"conflict","reason":"a turn is already in flight"}'; exit 0
fi
printf '%s' "$STUB_PROBE"
STUB
  export SWAP_PROBE_CMD="$ROOT/bin/probe"

  cat > "$ROOT/bin/herdr" <<'STUB'
#!/bin/bash
echo "$*" >> "$AGM_DIR/herdr.log"
case "$1 $2" in
  "pane current") printf '{"result":{"pane":{"pane_id":"%s"}}}' "$STUB_PANE_CURRENT" ;;
  "pane read")
      # 只有「當下真的存在」的那個 pane 讀得到：預設是 HERDR_PANE_ID／current 回的那顆。
      want="$STUB_PANE_CURRENT"   # 當下真的存在的那顆；別的 id（例如被寫死的舊 id）一律 not found
      if [ -z "$STUB_PANE_READ_OK" ] || [ "$3" != "$want" ]; then
        printf '{"error":{"code":"pane_not_found","message":"pane %s not found"}}' "$3"; exit 1
      fi
      printf 'some pane text' ;;
esac
STUB
  chmod +x "$ROOT/bin/"*
  : > "$AGM_DIR/calls.log"; : > "$AGM_DIR/launchctl.log"; : > "$AGM_DIR/herdr.log"; : > "$AGM_DIR/probe.log"
}

teardown() { rm -rf "$ROOT"; }

run() { bash "$SCRIPT" --sha "$SHA" --old "$OLD" --old-hash "$OLDHASH" --approval ap-1 --owner bot-me --checkout "$CHECKOUT" >/dev/null 2>&1; echo $?; }

# 1. 一般情形（schema 沒升）：換 binary、重啟、驗證、寫 .built。
setup 10 10
rc=$(run)
check_eq "順利時 rc=0" "0" "$rc"
check "預期版本從 checkout 讀出來" "checkout SCHEMA_VERSION=10, db user_version=10, bumped=no" "$SWAP_LOG"
check "有換上新 binary" "new-binary" "$AGM_DIR/started-binary.log"
check_eq ".built 寫的是這次的 sha" "$(echo "$SHA" | cut -c1-8)" "$(cat "$AGM_DIR/daemon-update.built")"
check "啟動走 launchd（nice 0）" "submit -l am-daemon-swap" "$AGM_DIR/launchctl.log"
check_no "不是在 pane 裡直接背景起" "nohup" "$SCRIPT"
teardown

# 2. 這批升了 schema：預期版本要跟著 checkout 走（11），migrate 後的 11 不能被當成失敗。
setup 11 10
rc=$(run)
check_eq "升 schema 也是 rc=0" "0" "$rc"
check "認得出這批升了 schema" "SCHEMA_VERSION=11, db user_version=10, bumped=yes" "$SWAP_LOG"
check "用 checkout 的版本比對，不是寫死的數字" "user_version=11 (expected 11)" "$SWAP_LOG"
check_no "不會把 migrate 成功當成失敗回滾" "ROLLBACK" "$SWAP_LOG"
teardown

# 3. 升過 schema 又出事：預設往前修（新 binary 重起），不還原 DB、不放回舊 binary。
setup 11 10
export STUB_SUPERVISOR=stopped
rc=$(run)
check_eq "往前修之後以 rc=6 結束（不是回滾）" "6" "$rc"
check "講清楚往前修的理由" "先往前修（舊 binary 開不了這個 DB）" "$SWAP_LOG"
check "往前修成功就停在新 binary" "forward-fix ok" "$SWAP_LOG"
check_no "沒有還原 DB" "db restored" "$SWAP_LOG"
check_eq "線上留著新 binary" "new-binary" "$(tail -1 "$AGM_DIR/started-binary.log")"
teardown

# 4. 升過 schema、往前修也救不回來：才還原 binary 與 DB，且 -wal／-shm 一起清掉、還原後自驗。
setup 11 10
export STUB_SUPERVISOR=stopped STUB_SESSION_OK_AFTER_FORWARD=""
rc=$(run)
check_eq "真的救不回來才回滾（rc=7）" "7" "$rc"
check "往前修失敗有講" "forward-fix 失敗" "$SWAP_LOG"
check "有還原 DB 並自驗 user_version／integrity" "db restored from .*user_version=.*integrity=ok" "$SWAP_LOG"
check_file "新版留下的 -wal 要清掉" no "$DAEMON_DB-wal"
check_file "新版留下的 -shm 要清掉" no "$DAEMON_DB-shm"
check_eq "binary 換回舊的" "old-binary" "$(cat "$AGM_REPO/target/release/agents-managerd")"
teardown

# 5. 沒升 schema 時出事：照舊直接回滾，不繞往前修那條路。
setup 10 10
export STUB_SUPERVISOR=stopped
rc=$(run)
check_eq "沒升 schema 的失敗走回滾（rc=7）" "7" "$rc"
check_no "不會講往前修" "forward-fix" "$SWAP_LOG"
check_eq "binary 換回舊的" "old-binary" "$(cat "$AGM_REPO/target/release/agents-managerd")"
check_file "-wal 一樣要清掉" no "$DAEMON_DB-wal"
teardown

# 6. 讀不到 checkout 的 SCHEMA_VERSION：停手，不要拿上一輪的數字猜。
setup 10 10
echo 'no schema history here' > "$CHECKOUT/daemon/src/db.rs"
rc=$(run)
check_eq "讀不到版本就中止（rc=3）" "3" "$rc"
check "講清楚不要用上一輪的數字" "不要用上一輪的數字猜" "$SWAP_LOG"
check_no "沒有動到 binary" "submit" "$AGM_DIR/launchctl.log"
teardown

# 7. 回滾點的 binary 對不上 sha256：換版前就停手。
setup 10 10
printf 'someone-elses-binary\n' > "$AGM_REPO/target/release/agents-managerd.bak-$OLD"
rc=$(run)
check_eq "回滾點不對就中止（rc=3）" "3" "$rc"
check "講出實際的 sha256" "不是 $OLDHASH" "$SWAP_LOG"
teardown

# 8. 拿不到窗口：延後，不硬換。
setup 10 10
export STUB_ACQUIRE_HELD=false
rc=$(run)
check_eq "拿不到窗口以 rc=4 結束" "4" "$rc"
check "記成延後" "DEFER" "$SWAP_LOG"
check_no "沒有重啟" "submit" "$AGM_DIR/launchctl.log"
teardown

# 9. 換 binary 前一刻複查不安全：交還窗口、不換版。
setup 10 10
export STUB_SAFE=false
rc=$(run)
check_eq "複查不安全就中止（rc=4）" "4" "$rc"
check "窗口要交還" "lease release restart" "$AGM_DIR/calls.log"
check_no "沒有重啟" "submit" "$AGM_DIR/launchctl.log"
teardown

# 10. DB 備份讀不回來：不換版（不然回滾時沒有可用的備份）。
setup 10 10
echo "bad rows" > "$ROOT/integrity"
rc=$(run)
check_eq "備份壞掉就中止（rc=5）" "5" "$rc"
check "講出 integrity_check 的結果" "備份讀不回來" "$SWAP_LOG"
check_no "沒有重啟" "submit" "$AGM_DIR/launchctl.log"
teardown

# 11. 啟動器本身：fork + setsid，而且不繼承 pane 的 AM_* 環境。
check "啟動器用 setsid 脫離" "os.setsid()" "$HERE/daemon-start.py"
check "啟動器會 fork，父行程先結束（job 才不會被 remove 殺到）" "os.fork()" "$HERE/daemon-start.py"
check "啟動器丟掉 AM_DATA_DIR 等 pane 變數" "AM_DATA_DIR" "$HERE/daemon-start.py"


# CD 信任閘門：要換上去的範圍裡有 fork PR 的 commit → 拿窗口之前就停手，binary 不動。
setup 10 10
export STUB_GH_PULLS='[{"number":7,"user":{"login":"mallory"},"head":{"repo":{"full_name":"mallory/proj"}}}]'
rc=$(run)
check_eq "fork PR 的 commit 不換版（rc=3）" "3" "$rc"
check "講出是哪個 PR" "fork PR #7" "$SWAP_LOG"
check_eq "binary 沒動" "old-binary" "$(cat "$AGM_REPO/target/release/agents-managerd")"
check_no "連窗口都沒去拿" "lease acquire" "$AGM_DIR/calls.log"
teardown

# 線上那一版不在 checkout 的歷史裡：閘門無從比對，不換。
setup 10 10
OLD=notacommit
rc=$(run)
check_eq "基準不在歷史裡就中止（rc=3）" "3" "$rc"
check "講清楚為什麼" "信任閘門無從比對" "$SWAP_LOG"
teardown

# 12. 3b：herdr pane read 讀不到 pane（pane 換過、或被寫死舊 id）→ exit 3，binary 不能被動。
setup 10 10
export HERDR_PANE_ID="w9:pSTALE"      # 一個已經不存在的舊 id（模擬寫死）
rc=$(run)
check_eq "pane 讀不到就中止（rc=3）" "3" "$rc"
check "log 講出 pane_not_found" "pane_not_found" "$SWAP_LOG"
check_no "沒有動 binary" "submit" "$AGM_DIR/launchctl.log"
check_eq "線上 binary 沒被換掉" "old-binary" "$(cat "$AGM_REPO/target/release/agents-managerd")"
teardown

# 13. 3b：沒有 HERDR_PANE_ID 時，自己去問 `herdr pane current`，不是猜也不是用呼叫端的值。
setup 10 10
unset HERDR_PANE_ID
rc=$(run)
check_eq "問得到 current 就能往下走（rc=0）" "0" "$rc"
check "有去問 pane current" "pane current" "$AGM_DIR/herdr.log"
check "讀的是 current 回的那顆" "pane read w1:pA" "$AGM_DIR/herdr.log"
teardown

# 14. 3b：pane id 不接受呼叫端帶進來——腳本沒有這種參數，而且讀的一定是當下取到的那顆。
setup 10 10
rc=$(bash "$SCRIPT" --sha "$SHA" --old "$OLD" --old-hash "$OLDHASH" --approval ap-1 --owner bot-me --checkout "$CHECKOUT" --pane w9:pSTALE >/dev/null 2>&1; echo $?)
check_eq "帶 --pane 會被拒（rc=2）" "2" "$rc"
setup 10 10
rc=$(run)
check "讀的是環境變數那顆，不是舊的寫死值" "pane read w1:pA" "$AGM_DIR/herdr.log"
check_no "不會去讀寫死的舊 id" "w169:pN" "$AGM_DIR/herdr.log"
teardown

# 15. 3b 自測 prompt：非 200 也不是 in flight → exit 3，不換 binary。
setup 10 10
export STUB_PROBE='409 {"error":"conflict","reason":"composer_unreadable"}'
rc=$(run)
check_eq "送不出去就中止（rc=3）" "3" "$rc"
check "log 帶上回應" "composer_unreadable" "$SWAP_LOG"
check_no "沒有動 binary" "submit" "$AGM_DIR/launchctl.log"
teardown

# 16. 3b 自測 prompt：對方正在跑回合 → 等它結束重送，不算失敗。
setup 10 10
export STUB_PROBE_FIRST_INFLIGHT=1
rc=$(run)
check_eq "in flight 會重送並繼續（rc=0）" "0" "$rc"
check "重送過（送了兩次）" "bot-probe" "$AGM_DIR/probe.log"
check "自測最後是通的" "3b self probe ok" "$SWAP_LOG"
teardown

# 17. 腳本本身：`$VAR` 後面直接接全形標點會被 `set -u` 當成變數名的一部分（2026-09-16／09-20／09-20 踩過三次）。
if LC_ALL=C grep -nE '\$[A-Za-z_][A-Za-z0-9_]*[^ -~]' "$SCRIPT" > /tmp/daemon-swap-unbraced.$$ 2>/dev/null; then
  echo "FAIL - 變數後面接非 ASCII 字元要用 \${VAR}"; sed 's/^/      /' /tmp/daemon-swap-unbraced.$$; FAIL=$((FAIL + 1))
else
  echo "ok   - 變數後面接非 ASCII 字元都有大括號"; PASS=$((PASS + 1))
fi
rm -f /tmp/daemon-swap-unbraced.$$

echo "$PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
