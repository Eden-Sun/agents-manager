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
    echo "FAIL - $1（存在=${got}，預期=$2：$3）"; FAIL=$((FAIL + 1))
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
  export SWAP_SETTLE_SECS=0 SWAP_WINDOW_TRIES=3 SWAP_WINDOW_WAIT_SECS=0
  export STUB_ACQUIRE_FAIL_TIMES=0   # 前幾次 acquire 回 409（模擬瞬間有人在跑）
  export STUB_ACQUIRE_EMPTY_WORKING=""   # 設了：那幾次 409 的 working 名單是空的
  export STUB_PROBE_INFLIGHT_TIMES=0     # 前幾次 lease safety 裡自測對象還在 in_flight
  export SWAP_PROBE_SETTLE_TRIES=5 SWAP_PROBE_SETTLE_WAIT_SECS=0
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
      && /usr/bin/git add -A && /usr/bin/git commit -qm init ) >/dev/null 2>&1
  export SHA=$(/usr/bin/git -C "$CHECKOUT" rev-parse HEAD)
  export OLD=oldsha

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
  export STUB_RELEASE_FAIL=""       # 設了：lease release 回 409（模擬 fence 過期／token 對不上）
  export STUB_NAMES_BEFORE='["a","b"]' STUB_NAMES_AFTER='["a","b"]'
  export STUB_RESTART_HELD=false
  export REAL_SQLITE="${REAL_SQLITE:-$(command -v sqlite3)}"
  "$REAL_SQLITE" "$ROOT/audit.sqlite3" "CREATE TABLE bots (id TEXT PRIMARY KEY, name TEXT, project_id TEXT, deleted_at TEXT);
    CREATE TABLE intents (id TEXT PRIMARY KEY, kind TEXT, subject_id TEXT, payload_json TEXT NOT NULL DEFAULT '{}',
      status TEXT NOT NULL DEFAULT 'done', created_at TEXT NOT NULL);"

  cat > "$ROOT/bin/agm" <<'STUB'
#!/bin/bash
echo "$*" >> "$AGM_DIR/calls.log"
sub=""; op=""
for a in "$@"; do
  case "$a" in --*) continue ;; esac
  if [ -z "$sub" ]; then sub="$a"; elif [ -z "$op" ]; then op="$a"; fi
done
case "$sub:$op" in
  lease:acquire)
      # 自測回合還在飛就拿不到窗口（跟正式 daemon 一樣：in_flight 擋，working 名單是空的）。
      # 時間用「查過幾次 safety＋要過幾次窗口」來代表：每問一次，自測回合就往收尾推進一步。
      safeties=$(grep -cE "lease (safety|acquire restart)" "$AGM_DIR/calls.log" 2>/dev/null); safeties=${safeties:-0}
      if [ "${STUB_PROBE_INFLIGHT_TIMES:-0}" -ge "$safeties" ] && [ "${STUB_PROBE_INFLIGHT_TIMES:-0}" -gt 0 ]; then
        printf '{"error":"http_error","status":409,"detail":{"error":"conflict","reason":"not_idle","safety":{"working":[],"in_flight":[{"bot_id":"bot-probe"}]}}}'
        exit 1
      fi
      tries=$(grep -c "lease acquire restart" "$AGM_DIR/calls.log" 2>/dev/null); tries=${tries:-0}
      if [ "${STUB_ACQUIRE_FAIL_TIMES:-0}" -ge "$tries" ] && [ "${STUB_ACQUIRE_FAIL_TIMES:-0}" -gt 0 ]; then
        if [ -n "$STUB_ACQUIRE_EMPTY_WORKING" ]; then
          printf '{"error":"http_error","status":409,"detail":{"error":"conflict","reason":"not_idle","safety":{"working":[],"in_flight":[{"bot_id":"bot-probe"}]}}}'
        else
          printf '{"error":"http_error","status":409,"detail":{"error":"conflict","reason":"not_idle","escalates_at":"2026-09-21T01:31:00Z","safety":{"working":[{"name":"busy-bot"}]}}}'
        fi
        exit 1
      fi
      printf '{"lease":{"held":%s,"fence":9},"lease_token":"tok-1"}' "$STUB_ACQUIRE_HELD" ;;
  lease:safety)  n=$(grep -cE "lease (safety|acquire restart)" "$AGM_DIR/calls.log"); fl=""
                 [ "${STUB_PROBE_INFLIGHT_TIMES:-0}" -ge "$n" ] && fl='{"bot_id":"bot-probe","turn_id":"t-1"}'
                 printf '{"safe":%s,"working":[],"delivering":[],"in_flight":[%s]}' "$STUB_SAFE" "$fl" ;;
  lease:status)  printf '{"leases":[{"resource":"restart","held":%s}]}' "$STUB_RESTART_HELD" ;;
  lease:release)
      # token 是怎麼進來的：記下檔案路徑、權限與讀到的內容，測試才驗得到「agm 真的讀到了
      # 那顆 token，而且它只待在一個 600 的檔裡」。
      tf=""; nxt=0
      for a in "$@"; do
        [ "$nxt" = 1 ] && { tf="$a"; nxt=0; continue; }
        [ "$a" = "--lease-token-file" ] && nxt=1
      done
      if [ -n "$tf" ]; then
        printf '%s mode=%s token=%s\n' "$tf" "$(stat -f '%Lp' "$tf" 2>/dev/null || stat -c '%a' "$tf")" "$(cat "$tf")" \
          >> "$AGM_DIR/tokenfile.log"
      fi
      [ -n "${STUB_RELEASE_FAIL:-}" ] && { printf '{"error":"http_error","status":409,"detail":{"error":"conflict","reason":"fence_mismatch"}}'; exit 1; }
      printf '{"released":true}' ;;
  health:*)      [ -n "$STUB_HEALTH_OK" ] || exit 1; printf '{"status":"healthy"}' ;;
  supervisor:*)  printf '{"status":"%s"}' "$STUB_SUPERVISOR" ;;
  state:*)       if [ -e "$AGM_DIR/started" ]; then names="$STUB_NAMES_AFTER"; else names="$STUB_NAMES_BEFORE"; fi
                 printf '{"bots":['; sep=""
                 for n in $(printf '%s' "$names" | tr -d '[]"' | tr ',' ' '); do printf '%s{"id":"id-%s","name":"%s"}' "$sep" "$n" "$n"; sep=","; done
                 printf ']}' ;;
  *)             printf '{}' ;;
esac
STUB

  cat > "$ROOT/bin/sqlite3" <<'STUB'
#!/bin/bash
ro=""; [ "$1" = -readonly ] && { ro=-readonly; shift; }
db="$1"; q="$2"
case "$q" in
  "pragma user_version")
      if [ "$db" = "$DAEMON_DB" ]; then cat "$ROOT/uv"; else cat "$db.uv" 2>/dev/null || cat "$ROOT/uv"; fi ;;
  "pragma integrity_check") cat "$ROOT/integrity" ;;
  .backup*) dest=${q#.backup }; cp "$db" "$dest"; cp "$ROOT/uv" "$dest.uv" ;;
  *) # 其他查詢（換版後比對刪除紀錄）交給真的 sqlite3，查 seed_audit 種的那份；要求一定是唯讀開的。
     [ "$db" = "$DAEMON_DB" ] && [ -n "$ro" ] || { echo "non-readonly query: $q" >> "$AGM_DIR/sqlite-rw.log"; exit 1; }
     "$REAL_SQLITE" -readonly "$ROOT/audit.sqlite3" "$q" ;;
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
seed_audit() { "$REAL_SQLITE" "$ROOT/audit.sqlite3" "$1"; }   # 換版後腳本唯讀查的那份 DB（bots／intents）

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
# 真的跑一次啟動器：假 binary 把環境倒出來。launchd 給的最小 PATH 要被換成含 Homebrew 的那組，AM_* 要被丟掉。
STARTER_ROOT=$(mktemp -d)
mkdir -p "$STARTER_ROOT/target/release"
# 暫存檔 + `mv`（rename 是原子的）：等待那邊用的 `[ -s ]` 只證明「非空」、不證明「寫完了」，
# 直接寫目的檔的話，剛好在 write 中間被讀到就會拿到半份 dump——env 小於 stdio 的 4096 緩衝時
# 機率極低，但這一條斷言就是為了不要再有這類機率才改的（issue #433，i264 review）。
printf '%s\n' '#!/bin/sh' 'env > "$STARTER_ENV_DUMP.tmp" && mv "$STARTER_ENV_DUMP.tmp" "$STARTER_ENV_DUMP"' > "$STARTER_ROOT/target/release/agents-managerd"
chmod +x "$STARTER_ROOT/target/release/agents-managerd"
# canary-gap: 這一行就是在測「啟動器自己推出來的 PATH」，帶進 canary 目錄會讓下面那條
# 精確比對的斷言失敗。這段只跑 daemon-start.py 與一個印 env 的假 agents-managerd，
# 沒有任何破壞性指令；真 binary 就算可達也沒有東西會叫到它。
STARTER_ENV_DUMP="$STARTER_ROOT/env" AM_DATA_DIR=/should/be/dropped PATH=/usr/bin:/bin:/usr/sbin:/sbin HOME="$STARTER_ROOT/home" \
  /usr/bin/python3 "$HERE/daemon-start.py" "$STARTER_ROOT" "$STARTER_ROOT/daemon.log"
# `daemon-start.py` 刻意 fork + setsid 脫離、父行程先結束，所以上面那行 python3 回來**不代表**
# 孫行程已經跑完 `env > "$STARTER_ENV_DUMP"`。機器忙的時候它排不到 CPU：原本只等 10 × 0.2 ＝ 2 秒，
# 2026-09-24 本機 load ~70（好幾顆 agent 同時編譯）時就寫不出來，後面的斷言去 sed 一個不存在的檔，
# 訊息變成 `sed: …/env: No such file or directory`，要看兩層才知道其實是逾時（issue #433）。
STARTER_WAIT_SECS=${STARTER_WAIT_SECS:-30}
waited=0
while [ ! -s "$STARTER_ROOT/env" ] && [ "$waited" -lt "$((STARTER_WAIT_SECS * 5))" ]; do
  waited=$((waited + 1))
  sleep 0.2
done
if [ -s "$STARTER_ROOT/env" ]; then
  echo "ok   - 啟動器在 ${STARTER_WAIT_SECS} 秒內寫出 env dump"; PASS=$((PASS + 1))
  check "啟動器帶出的 PATH 含 /opt/homebrew/bin" "^PATH=$STARTER_ROOT/home/.local/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin$" "$STARTER_ROOT/env"
  # `check_no` 找不到字串就算過，所以**一定要先確定 dump 真的寫出來了**：檔案不存在時它照樣綠，
  # 逾時就變成一條看不出來的假綠（issue #433，只有上面那條 PATH 會露出來）。
  check_no "啟動器真的丟掉 AM_DATA_DIR" "^AM_DATA_DIR=" "$STARTER_ROOT/env"
else
  echo "FAIL - 啟動器沒有在 ${STARTER_WAIT_SECS} 秒內寫出 env dump（$STARTER_ROOT/env）"
  echo "      fork+setsid 的孫行程可能還沒排到 CPU（機器忙），或根本沒起來。daemon.log："
  if [ -s "$STARTER_ROOT/daemon.log" ]; then sed 's/^/      /' "$STARTER_ROOT/daemon.log"; else echo "      （空的或不存在）"; fi
  FAIL=$((FAIL + 1))
fi
rm -rf "$STARTER_ROOT"


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

# 18. 窗口：第一次 409（瞬間有人在跑）不該整趟白跑——腳本自己重試，第二次拿到就繼續。
setup 10 10
export STUB_ACQUIRE_FAIL_TIMES=1
rc=$(run)
check_eq "重試後成功（rc=0）" "0" "$rc"
check "被拒那次有記下 reason" "no window yet (try 1/3) reason=not_idle" "$SWAP_LOG"
check "reason 帶上是誰在跑" "working=busy-bot" "$SWAP_LOG"
check "第二次就拿到窗口" "restart lease fence=9" "$SWAP_LOG"
teardown

# 19. 窗口：一直 409 → 試滿次數才 DEFER，而且 DEFER 那行要看得出 reason（不是只有 escalate_after_secs）。
setup 10 10
export STUB_ACQUIRE_FAIL_TIMES=99
rc=$(run)
check_eq "一直拿不到就 DEFER（rc=4）" "4" "$rc"
check "DEFER 記下最後的 reason" "DEFER: 拿不到 restart 窗口（試了 3 次，最後 reason=not_idle" "$SWAP_LOG"
check "試滿設定的次數" "try 3/3" "$SWAP_LOG"
check_no "沒有動 binary" "submit" "$AGM_DIR/launchctl.log"
teardown

# 20. 預設值：腳本自己的預設是重試 12 次、間隔 15 秒，不是只試一次。
check "SWAP_WINDOW_TRIES 預設 12" 'SWAP_WINDOW_TRIES:-12' "$SCRIPT"
check "間隔預設 15 秒" 'SWAP_WINDOW_WAIT_SECS:-15' "$SCRIPT"

# 21. 3b 自測回合還在飛：先等它收尾再拿窗口，不應該自己擋自己吃一次 409。
setup 10 10
export STUB_PROBE_INFLIGHT_TIMES=2
rc=$(run)
check_eq "等自測回合收尾後成功（rc=0）" "0" "$rc"
check "有等自測回合" "等自測回合收尾（bot-probe 還在飛）" "$SWAP_LOG"
check "第 3 次查就收尾了" "3b self probe turn settled (check 3/5)" "$SWAP_LOG"
check_no "沒有吃 409" "no window yet" "$SWAP_LOG"
check_eq "窗口只要了一次" "1" "$(grep -c "lease acquire restart" "$AGM_DIR/calls.log")"
teardown

# 22. 409 但 working 名單是空的：log 要註明可能是自測回合，並帶上 in_flight 是誰。
setup 10 10
export STUB_ACQUIRE_FAIL_TIMES=1 STUB_ACQUIRE_EMPTY_WORKING=1
rc=$(run)
check_eq "重試後成功（rc=0）" "0" "$rc"
check "空名單註明可能是自測回合" "reason=not_idle working=(空，可能是自測回合 in_flight=bot-probe)" "$SWAP_LOG"
teardown

# 23. 自測回合一直不收尾：等到上限就照樣去拿窗口（交給窗口重試），不在這裡卡死或中止。
setup 10 10
export STUB_PROBE_INFLIGHT_TIMES=6
rc=$(run)
check_eq "上限用完仍繼續，窗口重試接手（rc=0）" "0" "$rc"
check "講清楚等滿了" "自測回合等了 5 次還在飛" "$SWAP_LOG"
check "那次 409 有註明可能是自測回合" "no window yet (try 1/3) reason=not_idle working=(空，可能是自測回合" "$SWAP_LOG"
check "預設等 12 次" 'SWAP_PROBE_SETTLE_TRIES:-12' "$SCRIPT"
teardown

# 24. lease_token 走檔案，不進 argv（issue #477）。
# STUB_RESTART_HELD=true：daemon 沒有自動放掉窗口，腳本要自己交還——這才會走到 release。
setup 10 10
export STUB_RESTART_HELD=true
rc=$(run)
check_eq "順利時 rc=0" "0" "$rc"
check "交還窗口走 --lease-token-file" "lease release restart .*--lease-token-file" "$AGM_DIR/calls.log"
check_no "token 本身不出現在 argv" "tok-1" "$AGM_DIR/calls.log"
check "agm 真的從檔案讀到那顆 token" "mode=600 token=tok-1" "$AGM_DIR/tokenfile.log"
check_eq "token 檔收尾要刪掉" "0" "$(ls "$AGM_DIR"/daemon-swap.lease-token.* 2>/dev/null | wc -l | tr -d ' ')"
check "token 檔用 mktemp，不是可預測路徑" 'mktemp "$AGM_DIR/daemon-swap.lease-token.XXXXXX"' "$SCRIPT"
teardown

# 25. 交還窗口失敗：log 不准說「已交還」，而且要用非零結束碼講出來（issue #477）。
setup 10 10
export STUB_RESTART_HELD=true STUB_RELEASE_FAIL=1
rc=$(run)
check_eq "換版成功但窗口沒還：rc=8" "8" "$rc"
check "log 說交還失敗" "交還 restart 窗口失敗 rc=" "$SWAP_LOG"
check "log 說窗口還被握著" "窗口仍被握著" "$SWAP_LOG"
check_no "不准謊報已交還" "restart 窗口已交還" "$SWAP_LOG"
check "換版本身有做完" "new-binary" "$AGM_DIR/started-binary.log"
teardown

# 26. 中途中止時交還失敗也一樣不謊報（這條路的結束碼仍是它自己的原因碼）。
setup 10 10
export STUB_SAFE=false STUB_RELEASE_FAIL=1
rc=$(run)
check_eq "複查不安全仍以 rc=4 結束" "4" "$rc"
check "log 說交還失敗" "交還 restart 窗口失敗 rc=" "$SWAP_LOG"
check_no "不准謊報已交還" "restart 窗口已交還" "$SWAP_LOG"
teardown

# 27. 換版窗口內父 bot 用刪除 API 收掉 child（2026-09-24 22:53 ca9a0330 的 i263）：有 delete intent、
#     deleted_at 在窗口內 → 不是重啟弄丟的，不回滾。時間用 2099 年代表「比換版起點晚」。
setup 10 10
export STUB_NAMES_BEFORE='["a","b","kid"]' STUB_NAMES_AFTER='["a","b"]'
seed_audit "INSERT INTO bots VALUES ('id-kid','kid','p1','2099-01-01T00:00:01.000Z');
  INSERT INTO intents VALUES ('it1','delete_bot','id-kid','{\"bots\":[],\"requested_by\":\"agm\"}','done','2099-01-01T00:00:00.000Z');"
rc=$(run)
check_eq "窗口內刻意刪掉的不回滾（rc=0）" "0" "$rc"
check "log 列出窗口內刪掉的" "missing=\[\] deleted_in_window=\[kid \]" "$SWAP_LOG"
check_no "沒有回滾" "ROLLBACK" "$SWAP_LOG"
check_file "不是唯讀開 DB 的查詢一筆都沒有" no "$AGM_DIR/sqlite-rw.log"
teardown

# 28. 母 bot 被刪、child 跟著走：child 的 id 在母 bot 那筆 intent 的 payload 快照裡 → 一樣不回滾。
setup 10 10
export STUB_NAMES_BEFORE='["a","mom","kid"]' STUB_NAMES_AFTER='["a"]'
seed_audit "INSERT INTO bots VALUES ('id-mom','mom','p1','2099-01-01T00:00:01.000Z'), ('id-kid','kid','p1','2099-01-01T00:00:02.000Z');
  INSERT INTO intents VALUES ('it1','delete_bot','id-mom','{\"bots\":[{\"id\":\"id-kid\",\"managed_by\":\"child\"}]}','done','2099-01-01T00:00:00.000Z');"
rc=$(run)
check_eq "連帶刪掉的 child 也不回滾（rc=0）" "0" "$rc"
check "兩顆都算刻意刪除" "deleted_in_window=\[kid mom \]" "$SWAP_LOG"
teardown

# 29. 不見了、只有 deleted_at 沒有刪除紀錄（重啟後 reconcile 找不到 pane 退役、或投影軟刪都是這個樣子）
#     → 照樣回滾。這條不能放寬。
setup 10 10
export STUB_NAMES_BEFORE='["a","b","kid"]' STUB_NAMES_AFTER='["a","b"]'
seed_audit "INSERT INTO bots VALUES ('id-kid','kid','p1','2099-01-01T00:00:01.000Z');"
rc=$(run)
check_eq "沒有刪除紀錄就回滾（rc=7）" "7" "$rc"
check "log 講是沒有刪除紀錄" "有 bot 不見了（沒有刪除紀錄）：kid" "$SWAP_LOG"
check_eq "binary 換回舊的" "old-binary" "$(cat "$AGM_REPO/target/release/agents-managerd")"
teardown

# 30. 刪除紀錄是換版之前的舊帳（例如之前刪過又還原），這次不見的原因不是它 → 照樣回滾。
setup 10 10
export STUB_NAMES_BEFORE='["a","kid"]' STUB_NAMES_AFTER='["a"]'
seed_audit "INSERT INTO bots VALUES ('id-kid','kid','p1','2099-01-01T00:00:01.000Z');
  INSERT INTO intents VALUES ('it0','delete_bot','id-kid','{}','done','2000-01-01T00:00:00.000Z');"
rc=$(run)
check_eq "窗口外的刪除紀錄不算（rc=7）" "7" "$rc"
check "log 講是沒有刪除紀錄" "有 bot 不見了（沒有刪除紀錄）：kid" "$SWAP_LOG"
teardown

# 31. 父 bot 用 `herdr pane close` 收掉 child（沒呼叫 DELETE，#554）：daemon 退役時寫了 retire_child 紀錄，
#     herdr 報過關閉事件、當下 pane 也不在（cause=pane_closed）→ 刻意收掉的，不回滾。promote 收掉的 child 一樣。
setup 10 10
export STUB_NAMES_BEFORE='["a","kid","pro"]' STUB_NAMES_AFTER='["a"]'
seed_audit "INSERT INTO bots VALUES ('id-kid','kid','p1','2099-01-01T00:00:01.000Z'), ('id-pro','pro','p1','2099-01-01T00:00:02.000Z');
  INSERT INTO intents VALUES ('it1','retire_child','id-kid','{\"why\":\"reconcile_run_already_ended\",\"cause\":\"pane_closed\"}','done','2099-01-01T00:00:01.000Z'),
    ('it2','retire_child','id-pro','{\"why\":\"promoted\",\"cause\":\"promoted\"}','done','2099-01-01T00:00:02.000Z');"
rc=$(run)
check_eq "pane 被關掉而退役的 child 不回滾（rc=0）" "0" "$rc"
check "兩顆都算刻意收掉" "missing=\[\] deleted_in_window=\[kid pro \]" "$SWAP_LOG"
check_no "沒有回滾" "ROLLBACK" "$SWAP_LOG"
check_file "不是唯讀開 DB 的查詢一筆都沒有" no "$AGM_DIR/sqlite-rw.log"
teardown

# 32. 換版弄丟的形狀（#554）：有 retire_child 紀錄，但 pane 還在、只是新 daemon 認不出 agent（agent_missing），
#     或 pane 不在卻沒有人看到它被關（unconfirmed）→ 照樣回滾。有紀錄不等於刻意。
for c in agent_missing unconfirmed herdr_restarted; do
  setup 10 10
  export STUB_NAMES_BEFORE='["a","kid"]' STUB_NAMES_AFTER='["a"]'
  seed_audit "INSERT INTO bots VALUES ('id-kid','kid','p1','2099-01-01T00:00:01.000Z');
    INSERT INTO intents VALUES ('it1','retire_child','id-kid','{\"why\":\"reconcile_agent_gone\",\"cause\":\"$c\"}','done','2099-01-01T00:00:01.000Z');"
  rc=$(run)
  check_eq "cause=${c} 的退役照樣回滾（rc=7）" "7" "$rc"
  check "cause=${c}：log 講是沒有刪除紀錄" "有 bot 不見了（沒有刪除紀錄）：kid" "$SWAP_LOG"
  teardown
done

# 33. retire_child 紀錄在換版窗口之前（舊帳），或 subject 是別顆（payload 裡提到它只是 parent_bot_id）→ 不算，回滾。
setup 10 10
export STUB_NAMES_BEFORE='["a","kid"]' STUB_NAMES_AFTER='["a"]'
seed_audit "INSERT INTO bots VALUES ('id-kid','kid','p1','2099-01-01T00:00:01.000Z');
  INSERT INTO intents VALUES ('it0','retire_child','id-kid','{\"cause\":\"pane_closed\"}','done','2000-01-01T00:00:00.000Z'),
    ('it1','retire_child','id-grandkid','{\"cause\":\"pane_closed\",\"parent_bot_id\":\"id-kid\"}','done','2099-01-01T00:00:01.000Z');"
rc=$(run)
check_eq "窗口外、或別顆的退役紀錄都不算（rc=7）" "7" "$rc"
check "log 講是沒有刪除紀錄" "有 bot 不見了（沒有刪除紀錄）：kid" "$SWAP_LOG"
teardown

echo "$PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
