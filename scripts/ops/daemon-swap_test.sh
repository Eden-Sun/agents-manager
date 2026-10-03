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

db_backup_count() {
  n=0
  for f in "$DAEMON_DB".bak-*; do
    [ -f "$f" ] || continue
    case "$f" in *.uv) continue ;; esac
    n=$((n + 1))
  done
  echo "$n"
}

logged_db_backup() {
  sed -n 's/.*db backup \(.*\) integrity=.*/\1/p' "$SWAP_LOG" | tail -1
}

setup() { # setup <checkout 的 SCHEMA_VERSION> <DB 目前的 user_version>
  ROOT=$(mktemp -d); export ROOT
  export AGM_DIR="$ROOT/agm" AGM_REPO="$ROOT/repo" CHECKOUT="$ROOT/checkout"
  export AM_DATA="$ROOT/data" DAEMON_DB="$ROOT/am.sqlite3" DAEMON_LOG="$ROOT/daemon.log" SWAP_LOG="$ROOT/swap.log"
  export SWAP_SETTLE_SECS=0 SWAP_WINDOW_TRIES=3 SWAP_WINDOW_WAIT_SECS=0
  # 假 daemon 的埠：沒人聽的 1 號。腳本自己用 python 打 daemon（service 能力探測），不設就會打到本機正式的 7788。
  export AM_PORT_SWAP=1
  export STUB_ACQUIRE_FAIL_TIMES=0   # 前幾次 acquire 回 409（模擬瞬間有人在跑）
  export STUB_ACQUIRE_EMPTY_WORKING=""   # 設了：那幾次 409 的 working 名單是空的
  export STUB_PROBE_INFLIGHT_TIMES=0     # 前幾次 lease safety 裡自測對象還在 in_flight
  export STUB_SWAP_BINARY_ON_ACQUIRE="" # 測試窗口等待期間正式 binary 被另一趟換掉
  export SWAP_PROBE_SETTLE_TRIES=5 SWAP_PROBE_SETTLE_WAIT_SECS=0
  export STUB_AGM_STATE_FAIL="" STUB_AGM_SUPERVISOR_FAIL=""
  export STUB_AGM_SUPERVISOR_FAIL_AT=0
  export AGM_BIN="$ROOT/bin/agm" SQLITE_BIN="$ROOT/bin/sqlite3" CURL_BIN="$ROOT/bin/curl"
  export LAUNCHCTL_BIN="$ROOT/bin/launchctl" PGREP_BIN="$ROOT/bin/pgrep" HERDR_BIN="$ROOT/bin/herdr"
  # 預設照 macOS 走 launchd；Linux 那幾個 case 自己改（issue #677）。不設的話在 Linux 上跑這支，
  # 前面整批 launchd 的斷言會因為 uname 換了路徑而紅。
  export SYSTEMD_RUN_BIN="$ROOT/bin/systemd-run" AGM_OPS_PLATFORM=darwin
  export SWAP_PROBE_TRIES=2 SWAP_PROBE_BOT=bot-probe
  export HERDR_PANE_ID="w1:pA"          # 預設：pane 裡本來就有；測 current 的 case 會 unset
  export STUB_CAP=service STUB_PANE_LIST_FAIL=""
  export STUB_PANE_READ_OK=1 STUB_PANE_CURRENT="w1:pA" STUB_PROBE='200 {"delivery":"ok"}' 
  mkdir -p "$AGM_DIR" "$AM_DATA/service-tokens" "$AGM_REPO/target/release" "$CHECKOUT/daemon/src" "$CHECKOUT/target/release" "$ROOT/bin"
  printf 'test-daemon-swap-service-token\n' > "$AM_DATA/service-tokens/daemon-swap.token"
  chmod 600 "$AM_DATA/service-tokens/daemon-swap.token"

  # 假 checkout：SCHEMA_HISTORY 的最後一項就是這顆 binary 認得的版本。
  { echo 'const SCHEMA_HISTORY: &[(i64, &str)] = &['
    echo '    (9, "aaa"),'
    echo '    (10, "bbb"),'
    [ "$1" -ge 11 ] && echo "    ($1, \"ccc\"),"
    echo '];'; } > "$CHECKOUT/daemon/src/db.rs"
  # 假 binary 是會答 `--version` 的 shell 腳本（真 binary 的 `--version` 印 `<name> <版本> <完整 sha>[-dirty]`）；
  # 最後一行 `new-binary` 永遠跑不到（上一行 exit），只是讓測試認得「換上去的是新的那顆」。
  { echo '#!/bin/sh'
    echo '[ "$1" = --version ] && printf "agents-managerd 0.1.0 %s\n" "$STUB_BIN_VERSION_SHA"; exit 0'
    echo 'new-binary'; } > "$CHECKOUT/target/release/agents-managerd"; chmod +x "$CHECKOUT/target/release/agents-managerd"
  /usr/bin/git init -q "$CHECKOUT"
  ( cd "$CHECKOUT" && /usr/bin/git config user.email t@t && /usr/bin/git config user.name t \
      && /usr/bin/git add -A && /usr/bin/git commit -qm live \
      && /usr/bin/git commit -q --allow-empty -m target ) >/dev/null 2>&1
  export SHA=$(/usr/bin/git -C "$CHECKOUT" rev-parse HEAD)
  export STUB_BIN_VERSION_SHA="$SHA"      # 新 binary `--version` 印的 sha（預設＝要換上的那顆）
  export STUB_DAEMON_SHA="$SHA"           # 新 daemon 起來後 `agm supervisor` 回報的 last_deploy.sha_full
  export OLD=$(/usr/bin/git -C "$CHECKOUT" rev-parse HEAD~1)

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
  unset STUB_SUPERVISOR_BEFORE
  export STUB_RELEASE_FAIL=""       # 設了：lease release 回 409（模擬 fence 過期／token 對不上）
  export STUB_NAMES_BEFORE='["a","b"]' STUB_NAMES_AFTER='["a","b"]'
  export STUB_RESTART_HELD=false
  export REAL_SQLITE="${REAL_SQLITE:-$(command -v sqlite3)}"
  "$REAL_SQLITE" "$ROOT/audit.sqlite3" "CREATE TABLE bots (id TEXT PRIMARY KEY, name TEXT, project_id TEXT, deleted_at TEXT);
    ALTER TABLE bots ADD COLUMN managed_by TEXT;
    ALTER TABLE bots ADD COLUMN parent_bot_id TEXT;
    ALTER TABLE bots ADD COLUMN created_at TEXT;
    CREATE TABLE intents (id TEXT PRIMARY KEY, kind TEXT, subject_id TEXT, payload_json TEXT NOT NULL DEFAULT '{}',
      status TEXT NOT NULL DEFAULT 'done', created_at TEXT NOT NULL);
    CREATE TABLE runs (bot_id TEXT NOT NULL, state TEXT NOT NULL);"

  cat > "$ROOT/bin/agm" <<'STUB'
#!/bin/bash
echo "$*" >> "$AGM_DIR/calls.log"
echo "identity service=${AM_SERVICE_ID:-} bot=${AM_BOT_ID:-}" >> "$AGM_DIR/identity.log"
sub=""; op=""
for a in "$@"; do
  case "$a" in --*) continue ;; esac
  if [ -z "$sub" ]; then sub="$a"; elif [ -z "$op" ]; then op="$a"; fi
done
case "$sub:$op" in
  lease:safety)  n=$(grep -cE "lease (safety|acquire restart)" "$AGM_DIR/calls.log"); fl=""
                 [ "${STUB_PROBE_INFLIGHT_TIMES:-0}" -ge "$n" ] && fl='{"bot_id":"bot-probe","turn_id":"t-1"}'
                 printf '{"safe":%s,"working":[],"delivering":[],"in_flight":[%s]}' "$STUB_SAFE" "$fl" ;;
  lease:acquire) printf '{"lease":{"held":true,"fence":9},"lease_token":"tok-legacy"}' ;;
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
        printf '%s mode=%s token=%s\n' "$tf" "$(stat -c '%a' "$tf" 2>/dev/null || stat -f '%Lp' "$tf")" "$(cat "$tf")" \
          >> "$AGM_DIR/tokenfile.log"
      fi
      [ -n "${STUB_RELEASE_FAIL:-}" ] && { printf '{"error":"http_error","status":409,"detail":{"error":"conflict","reason":"fence_mismatch"}}'; exit 1; }
      printf '{"released":true}' ;;
  health:*)      [ -n "$STUB_HEALTH_OK" ] || exit 1; printf '{"status":"healthy"}' ;;
  supervisor:*)  sup_calls=$(grep -c ' supervisor' "$AGM_DIR/calls.log" 2>/dev/null || true); sup_calls=${sup_calls:-0}
                 if [ -n "$STUB_AGM_SUPERVISOR_FAIL" ] || [ "${STUB_AGM_SUPERVISOR_FAIL_AT:-0}" = "$sup_calls" ]; then printf '{"status":"idle"}'; exit 1; fi
                 # 換版前（started 還沒出現）可用 STUB_SUPERVISOR_BEFORE 單獨指定；沒設就跟換版後同一個值。
                 if [ ! -e "$AGM_DIR/started" ] && [ -n "${STUB_SUPERVISOR_BEFORE+x}" ]; then printf '{"status":"%s"}' "$STUB_SUPERVISOR_BEFORE"; exit 0; fi
                 printf '{"status":"%s","last_deploy":{"sha":"x","sha_full":"%s","dirty":%s}}' "$STUB_SUPERVISOR" "$STUB_DAEMON_SHA" "${STUB_DAEMON_DIRTY:-false}" ;;
  state:*)       if [ -e "$AGM_DIR/started" ]; then phase=after; names="$STUB_NAMES_AFTER"; else phase=before; names="$STUB_NAMES_BEFORE"; fi
                 [ "$STUB_AGM_STATE_FAIL" != "$phase" ] || { printf '{}'; exit 1; }
                 printf '{"bots":['; sep=""
                 for n in $(printf '%s' "$names" | tr -d '[]"' | tr ',' ' '); do printf '%s{"id":"id-%s","name":"%s"}' "$sep" "$n" "$n"; sep=","; done
                 printf ']}' ;;
  *)             printf '{}' ;;
esac
STUB

  # 假的 restart-window 回應（SWAP_WINDOW_CMD）：daemon 的 restart-window 路由印出的本文；被拒時是 409 的 JSON。
  cat > "$ROOT/bin/window" <<'STUB'
#!/bin/bash
echo "lease acquire restart --owner $1 --commit $2" >> "$AGM_DIR/calls.log"
# 自測回合還在飛就拿不到窗口（跟正式 daemon 一樣：in_flight 擋，working 名單是空的）。
# 時間用「查過幾次 safety＋要過幾次窗口」來代表：每問一次，自測回合就往收尾推進一步。
safeties=$(grep -cE "lease (safety|acquire restart)" "$AGM_DIR/calls.log" 2>/dev/null); safeties=${safeties:-0}
if [ "${STUB_PROBE_INFLIGHT_TIMES:-0}" -ge "$safeties" ] && [ "${STUB_PROBE_INFLIGHT_TIMES:-0}" -gt 0 ]; then
  printf '{"error":"http_error","status":409,"detail":{"error":"conflict","reason":"not_idle","safety":{"working":[],"in_flight":[{"bot_id":"bot-probe"}]}}}'
  exit 0
fi
tries=$(grep -c "lease acquire restart" "$AGM_DIR/calls.log" 2>/dev/null); tries=${tries:-0}
if [ "${STUB_ACQUIRE_FAIL_TIMES:-0}" -ge "$tries" ] && [ "${STUB_ACQUIRE_FAIL_TIMES:-0}" -gt 0 ]; then
  if [ -n "$STUB_ACQUIRE_EMPTY_WORKING" ]; then
    printf '{"error":"http_error","status":409,"detail":{"error":"conflict","reason":"not_idle","safety":{"working":[],"in_flight":[{"bot_id":"bot-probe"}]}}}'
  else
    printf '{"error":"http_error","status":409,"detail":{"error":"conflict","reason":"not_idle","escalates_at":"2026-09-21T01:31:00Z","safety":{"working":[{"name":"busy-bot"}]}}}'
  fi
  exit 0
fi
if [ -n "${STUB_SWAP_BINARY_ON_ACQUIRE:-}" ] && [ ! -e "$AGM_DIR/changed-live" ]; then
  printf 'newer-live-binary\n' > "$AGM_REPO/target/release/agents-managerd"
  : > "$AGM_DIR/changed-live"
fi
printf '{"lease":{"held":%s,"fence":9},"lease_token":"tok-1"}' "$STUB_ACQUIRE_HELD"
STUB
  cat > "$ROOT/bin/cap" <<'STUB'
#!/bin/bash
printf '%s' "${STUB_CAP:-service}"
STUB
  export SWAP_WINDOW_CMD="$ROOT/bin/window" SWAP_CAP_CMD="$ROOT/bin/cap"

  cat > "$ROOT/bin/sqlite3" <<'STUB'
#!/bin/bash
ro=""; [ "$1" = -readonly ] && { ro=-readonly; shift; }
db="$1"; q="$2"
case "$q" in
  "pragma user_version")
      if [ "$db" = "$DAEMON_DB" ]; then cat "$ROOT/uv"; else cat "$db.uv" 2>/dev/null || cat "$ROOT/uv"; fi ;;
  "pragma integrity_check") cat "$ROOT/integrity" ;;
  .backup*) [ -z "${STUB_BACKUP_FAIL:-}" ] || exit 1; dest=${q#.backup }; cp "$db" "$dest"; cp "$ROOT/uv" "$dest.uv" ;;
  *) # 其他查詢（換版後比對刪除紀錄）交給真的 sqlite3，查 seed_audit 種的那份；要求一定是唯讀開的。
     [ "$db" = "$DAEMON_DB" ] && [ -n "$ro" ] || { echo "non-readonly query: $q" >> "$AGM_DIR/sqlite-rw.log"; exit 1; }
     "$REAL_SQLITE" -readonly "$ROOT/audit.sqlite3" "$q" ;;
esac
STUB

  cat > "$ROOT/bin/curl" <<'STUB'
#!/bin/bash
# 第二次啟動（launchctl submit 或 systemd-run）之後＝往前修那次重啟；成不成功由 STUB_SESSION_OK_AFTER_FORWARD 決定。
starts=$(wc -l < "$AGM_DIR/starts.log" 2>/dev/null | tr -d ' '); starts=${starts:-0}
if [ "$starts" -ge 2 ]; then [ -n "$STUB_SESSION_OK_AFTER_FORWARD" ] && exit 0 || exit 7; fi
[ -n "$STUB_SESSION_OK" ] && exit 0 || exit 7
STUB

  # launchctl stub：記錄呼叫，submit 時真的跑一次啟動器（它會 fork+setsid 再 exec 假 binary）。
  cat > "$ROOT/bin/launchctl" <<'STUB'
#!/bin/bash
echo "$*" >> "$AGM_DIR/launchctl.log"
case "$1" in
  submit) : > "$AGM_DIR/started"; echo launchctl >> "$AGM_DIR/starts.log"
          [ -z "${STUB_MUTATE_DB:-}" ] || [ -e "$AGM_DIR/mutated" ] || { printf 'mutated-by-new-binary\n' > "$DAEMON_DB"; : > "$AGM_DIR/mutated"; }
          echo "$(cat "$AGM_REPO/target/release/agents-managerd")" >> "$AGM_DIR/started-binary.log"
          echo "$STUB_UV_AFTER_START" > "$ROOT/uv" ;;
esac
exit 0
STUB

  # systemd-run stub（Linux，issue #677）：記下 argv 與 XDG_RUNTIME_DIR，行為同上面的 submit。
  cat > "$ROOT/bin/systemd-run" <<'STUB'
#!/bin/bash
echo "$*" >> "$AGM_DIR/systemd-run.log"
echo "XDG_RUNTIME_DIR=${XDG_RUNTIME_DIR:-}" >> "$AGM_DIR/systemd-run.env"
echo systemd-run >> "$AGM_DIR/starts.log"
echo "$(cat "$AGM_REPO/target/release/agents-managerd")" >> "$AGM_DIR/started-binary.log"
echo "$STUB_UV_AFTER_START" > "$ROOT/uv"
exit "${STUB_SYSTEMD_RUN_RC:-0}"
STUB

  cat > "$ROOT/bin/pgrep" <<'STUB'
#!/bin/bash
echo "${STUB_PID:-99999}"
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
  "pane list") [ -z "${STUB_PANE_LIST_FAIL:-}" ] || { printf '{"error":"socket down"}'; exit 1; }; printf '{"result":{"panes":[]}}' ;;
  "pane current") [ -n "$STUB_PANE_CURRENT" ] || exit 1; printf '{"result":{"pane":{"pane_id":"%s"}}}' "$STUB_PANE_CURRENT" ;;
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
  : > "$AGM_DIR/systemd-run.log"; : > "$AGM_DIR/starts.log"
}

teardown() { rm -rf "$ROOT"; }
seed_audit() { "$REAL_SQLITE" "$ROOT/audit.sqlite3" "$1"; }   # 換版後腳本唯讀查的那份 DB（bots／intents）

run() {
  local approval="${1:-}"
  local sha="${2:-$SHA}"
  # These tests assert decisions, not wall-clock waits; keep recovery cases quick without changing
  # the separate daemon-start process test below.
  (
    sleep() { :; }
    export -f sleep
    if [ -n "$approval" ]; then
      bash "$SCRIPT" --sha "$sha" --old "$OLD" --old-hash "$OLDHASH" --approval "$approval" --owner bot-me --checkout "$CHECKOUT" >/dev/null 2>&1
    else
      bash "$SCRIPT" --sha "$sha" --old "$OLD" --old-hash "$OLDHASH" --owner bot-me --checkout "$CHECKOUT" >/dev/null 2>&1
    fi
  )
  echo $?
}

run_capture() {
  (
    sleep() { :; }
    export -f sleep
    bash "$SCRIPT" --sha "$SHA" --old "$OLD" --old-hash "$OLDHASH" --owner bot-me --checkout "$CHECKOUT" >"$ROOT/swap.out" 2>&1
  )
  echo $?
}

# 1. 一般情形（schema 沒升）：換 binary、重啟、驗證、寫 .built。
setup 10 10
printf 'old-backup-1\n' > "$DAEMON_DB.bak-20000101-0000"
printf 'old-backup-2\n' > "$DAEMON_DB.bak-20010101-0000"
rc=$(run)
check_eq "順利時 rc=0" "0" "$rc"
check "預期版本從 checkout 讀出來" "checkout SCHEMA_VERSION=10, db user_version=10, bumped=no" "$SWAP_LOG"
check "有換上新 binary" "new-binary" "$AGM_DIR/started-binary.log"
check_eq ".built 寫的是這次的 sha" "$(echo "$SHA" | cut -c1-8)" "$(cat "$AGM_DIR/daemon-update.built")"
check "啟動走 launchd（nice 0）" "submit -l am-daemon-swap" "$AGM_DIR/launchctl.log"
check_no "不是在 pane 裡直接背景起" "nohup" "$SCRIPT"
check_file "成功後保留這趟 DB 備份" yes "$(logged_db_backup)"
check_file "成功後刪除較舊 DB 備份" no "$DAEMON_DB.bak-20000101-0000"
check_file "成功後刪除另一份較舊 DB 備份" no "$DAEMON_DB.bak-20010101-0000"
check_eq "成功後只保留一份 DB 備份" "1" "$(db_backup_count)"
teardown

# 1b. DB 備份檔權限：DB 裡有 bot 的 hook token 等憑證，備份不能是預設 umask 的 644（別的使用者讀得到）。
#     備份是腳本自己建的，不論呼叫端的 umask 是什麼都要 600。
setup 10 10
( umask 022; rc=$(run); check_eq "備份權限：順利時 rc=0" "0" "$rc" )
BK=$(logged_db_backup)
MODE=$(stat -c '%a' "$BK" 2>/dev/null || stat -f '%Lp' "$BK")
check_eq "DB 備份檔權限是 600（呼叫端 umask 022 也一樣）" "600" "$MODE"
teardown

# 1b-2. 同一秒／同名的備份已經存在：不能覆蓋它（那可能是上一趟換版前唯一的一份好備份），拒絕換版、什麼都沒動。
setup 11 10
printf 'precious-pre-migration-backup\n' > "$DAEMON_DB.bak-samestamp"
rc=$(SWAP_BACKUP_STAMP=samestamp run)
check_eq "備份檔已存在：rc=5" "5" "$rc"
check_eq "既有的備份一個字都沒動" "precious-pre-migration-backup" "$(cat "$DAEMON_DB.bak-samestamp")"
check_eq "沒有重啟 daemon" "0" "$(wc -l < "$AGM_DIR/starts.log" | tr -d ' ')"
teardown

# 1b-2b. 備份拍不出來（磁碟滿、DB 鎖著）：已經拿著 restart 窗口，中止前要還——不然窗口一直握到 TTL 到期，別人都拿不到。
setup 11 10
rc=$(STUB_BACKUP_FAIL=1 run)
check_eq "備份失敗：rc=5" "5" "$rc"
check "備份失敗要交還窗口" "lease release restart" "$AGM_DIR/calls.log"
check_eq "沒有重啟 daemon" "0" "$(wc -l < "$AGM_DIR/starts.log" | tr -d ' ')"
teardown

# 1b-3. DB 的 schema 比要換上的 binary 還新（例如部署了比線上舊的 commit）：換上去的 binary 一定會被版本閘擋下，
#       停機之後才發現就晚了——停 daemon 之前就拒絕。
setup 10 11
rc=$(run)
check_eq "DB 比目標 binary 新：rc=3" "3" "$rc"
check "log 講明是 schema 比 binary 新" "比要換上的" "$SWAP_LOG"
check_eq "沒有重啟 daemon" "0" "$(wc -l < "$AGM_DIR/starts.log" | tr -d ' ')"
check_eq "線上 binary 沒被動" "old-binary" "$(cat "$AGM_REPO/target/release/agents-managerd")"
check_eq "沒有拍備份" "0" "$(db_backup_count)"
teardown

# 1b-4. 讀不到 DB 的 user_version（sqlite3 壞了、DB 開不了）：不能當成「升過 schema」往下跑。
setup 10 10
: > "$ROOT/uv"
rc=$(run)
check_eq "讀不到 user_version：rc=3" "3" "$rc"
check_eq "沒有重啟 daemon" "0" "$(wc -l < "$AGM_DIR/starts.log" | tr -d ' ')"
teardown

# 1c. 線上版本比目標新：在任何備份或重啟前拒絕（#638）。
setup 10 10
NEWER=$(/usr/bin/git -C "$CHECKOUT" commit -q --allow-empty -m newer && /usr/bin/git -C "$CHECKOUT" rev-parse HEAD)
/usr/bin/git -C "$CHECKOUT" checkout -q "$SHA"
export OLD="$NEWER"
rc=$(run)
check_eq "目標較舊時 swap rc=3" "3" "$rc"
check "說明不會降版" "不是要換上的 $SHA 的祖先或同一顆，無法確認不是降版" "$SWAP_LOG"
check_no "沒有切換 binary" "new-binary" "$AGM_DIR/started-binary.log"
check_file "沒有備份正式 binary" no "$AGM_REPO/target/release/agents-managerd.bak-$OLD"
teardown

# 1d. restart 窗口前另一趟已換過 live：拿窗口後用 hash 重驗。
setup 10 10
export STUB_SWAP_BINARY_ON_ACQUIRE=1
rc=$(run)
check_eq "窗口前 live 改變時 swap rc=3" "3" "$rc"
check "說明 live 已改變並重讀" "取得 restart 窗口後線上 binary 已改變" "$SWAP_LOG"
check "歸還 restart 窗口" "restart 窗口已交還（線上 binary 已改變）" "$SWAP_LOG"
check_no "沒有啟動較舊 binary" "new-binary" "$AGM_DIR/started-binary.log"
check_eq "保留取得窗口時的 live" "newer-live-binary" "$(cat "$AGM_REPO/target/release/agents-managerd")"
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
printf 'older-backup\n' > "$DAEMON_DB.bak-20000101-0000"
export STUB_SUPERVISOR=stopped
rc=$(run)
check_eq "沒升 schema 的失敗走回滾（rc=7）" "7" "$rc"
check_no "不會講往前修" "forward-fix" "$SWAP_LOG"
check_eq "binary 換回舊的" "old-binary" "$(cat "$AGM_REPO/target/release/agents-managerd")"
check_file "回滾後保留這趟 DB 備份" yes "$(logged_db_backup)"
check_file "回滾後保留舊 DB 備份" yes "$DAEMON_DB.bak-20000101-0000"
check_eq "回滾後保留新舊兩份 DB 備份" "2" "$(db_backup_count)"
check_file "-wal 一樣要清掉" no "$DAEMON_DB-wal"
teardown

# 5b. 回滾還原 DB 要原子：先寫同目錄暫存檔、fsync，再暫存 sidecar，最後用 rename 覆蓋主檔。
#     以前 `rm -f -wal -shm; cp backup DB`：cp 中途失敗（磁碟滿、被殺）就是「原 DB 被截斷＋它的 WAL 已經刪了」。
setup 10 10
export STUB_SUPERVISOR=stopped STUB_MUTATE_DB=1
rc=$(run)
check_eq "原子還原：回滾 rc=7" "7" "$rc"
check_eq "DB 內容是備份的（新版寫的被還原掉）" "db@10" "$(cat "$DAEMON_DB")"
check_file "還原後 -wal 清掉" no "$DAEMON_DB-wal"
check_file "還原後 -shm 清掉" no "$DAEMON_DB-shm"
check_eq "還原後的 DB 權限 600" "600" "$(stat -c '%a' "$DAEMON_DB" 2>/dev/null || stat -f '%Lp' "$DAEMON_DB")"
LEFT=$(ls "$ROOT" | grep -c 'restore' || true)
check_eq "沒有留下暫存檔" "0" "$LEFT"
teardown

# 5c. 還原的 cp 中途失敗：原 DB 與它的 -wal／-shm 一個位元組都不能動（WAL 裡還有 commit 過、沒 checkpoint 的資料），
#     不留暫存檔，log 講清楚還原失敗。
setup 10 10
export STUB_SUPERVISOR=stopped STUB_MUTATE_DB=1
mkdir -p "$ROOT/fakecp"
cat > "$ROOT/fakecp/cp" <<'STUB'
#!/bin/bash
for a in "$@"; do dest=$a; done
case "$dest" in
  *restore*) printf 'half-written' > "$dest"; exit 1 ;;   # 還原用的暫存檔：寫一半就失敗
esac
exec /bin/cp "$@"
STUB
chmod +x "$ROOT/fakecp/cp"
rc=$(PATH="$ROOT/fakecp:$PATH" run)
check_eq "還原失敗仍走回滾結束（rc=7）" "7" "$rc"
check_eq "原 DB 沒被動（新版寫的內容還在）" "mutated-by-new-binary" "$(cat "$DAEMON_DB")"
check_eq "原 -wal 沒被刪" "stale-wal" "$(cat "$DAEMON_DB-wal" 2>/dev/null)"
check_eq "原 -shm 沒被刪" "stale-shm" "$(cat "$DAEMON_DB-shm" 2>/dev/null)"
LEFT=$(ls "$ROOT" | grep -c 'restore' || true)
check_eq "失敗後不留暫存檔" "0" "$LEFT"
check "log 講清楚 DB 還原失敗" "DB 還原失敗" "$SWAP_LOG"
check_no "失敗時不能說還原成功" "db restored from" "$SWAP_LOG"
teardown
unset STUB_MUTATE_DB

# 5d. 暫存檔已備好，但替換主 DB 的 rename 失敗：原 DB 與 sidecar 都必須保留。
setup 10 10
export STUB_SUPERVISOR=stopped STUB_MUTATE_DB=1
mkdir -p "$ROOT/fakemv"
cat > "$ROOT/fakemv/mv" <<'STUB'
#!/bin/bash
for a in "$@"; do dest=$a; done
if [ "$dest" = "$DAEMON_DB" ]; then exit 1; fi
exec /bin/mv "$@"
STUB
chmod +x "$ROOT/fakemv/mv"
rc=$(PATH="$ROOT/fakemv:$PATH" run)
check_eq "rename 還原失敗仍走回滾結束（rc=7）" "7" "$rc"
check_eq "rename 失敗保留新版 DB" "mutated-by-new-binary" "$(cat "$DAEMON_DB")"
check_eq "rename 失敗保留 WAL" "stale-wal" "$(cat "$DAEMON_DB-wal" 2>/dev/null)"
check_eq "rename 失敗保留 SHM" "stale-shm" "$(cat "$DAEMON_DB-shm" 2>/dev/null)"
check "rename 失敗有講清楚 DB 還原失敗" "DB 還原失敗" "$SWAP_LOG"
teardown
unset STUB_MUTATE_DB

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

# 9a. 換版前的 bot 名單讀取失敗或空名單不能被當成「沒有 bot 需要保護」。
setup 10 10
export STUB_AGM_STATE_FAIL=before
rc=$(run)
check_eq "換版前 agm state 失敗要中止（rc=3）" "3" "$rc"
check "明講換版前名單不可用" "換版前 agm state 讀取失敗或 bot 名單為空" "$SWAP_LOG"
check "中止前歸還 restart lease" "lease release restart" "$AGM_DIR/calls.log"
check_no "名單讀不到時不能換 binary" "submit" "$AGM_DIR/launchctl.log"
teardown

setup 10 10
export STUB_NAMES_BEFORE='[]'
rc=$(run)
check_eq "換版前空名單要中止（rc=3）" "3" "$rc"
check_no "空名單不能放行換版" "submit" "$AGM_DIR/launchctl.log"
teardown

# 9b. 換版後 state 失敗與 supervisor 非健康／未知狀態都必須 rollback。
setup 10 10
export STUB_AGM_STATE_FAIL=after
rc=$(run)
check_eq "換版後 agm state 失敗要 rollback（rc=7）" "7" "$rc"
check "明講新版後名單不可用" "新版後 agm state 讀取失敗或 bot 名單為空" "$SWAP_LOG"
check_eq "失敗後還原舊 binary" "old-binary" "$(cat "$AGM_REPO/target/release/agents-managerd")"
teardown

setup 10 10
export STUB_AGM_SUPERVISOR_FAIL=1
rc=$(run)
check_eq "agm supervisor 失敗要 rollback（rc=7）" "7" "$rc"
check "命令回錯時不能接受它輸出的 idle" "agm supervisor 讀取失敗" "$SWAP_LOG"
check_eq "supervisor 讀取失敗後還原舊 binary" "old-binary" "$(cat "$AGM_REPO/target/release/agents-managerd")"
teardown

# 檔案 --version 已驗過，但新 daemon 的 provenance API 第一次讀失敗；後續一般狀態恢復正常也不能跳過 sha 複核。
setup 10 10
export STUB_AGM_SUPERVISOR_FAIL_AT=2
rc=$(run)
check_eq "新 daemon provenance 讀取失敗即回滾（rc=7）" "7" "$rc"
check "provenance 讀取失敗原因清楚" "新 daemon 的 agm supervisor 讀取失敗" "$SWAP_LOG"
check_eq "provenance 讀取失敗後還原舊 binary" "old-binary" "$(cat "$AGM_REPO/target/release/agents-managerd")"
teardown

setup 10 10
export STUB_SUPERVISOR=""
rc=$(run)
check_eq "空 supervisor status 要 rollback（rc=7）" "7" "$rc"
check "空 status 要當成讀取失敗" "agm supervisor 讀取失敗" "$SWAP_LOG"
check_eq "空 status 後還原舊 binary" "old-binary" "$(cat "$AGM_REPO/target/release/agents-managerd")"
teardown

# 換版前就健康、換版後才 waiting_quota／unknown：是新版造成的，照樣 rollback。
for status in waiting_quota unknown; do
  setup 10 10
  export STUB_SUPERVISOR_BEFORE="idle" STUB_SUPERVISOR="$status"
  rc=$(run)
  check_eq "換版前 idle、換版後 ${status} 要 rollback（rc=7）" "7" "$rc"
  check "拒絕 supervisor 非健康／未知狀態" "supervisor status 不健康或未知" "$SWAP_LOG"
  teardown
done

# 換版前就是 unknown（不是 waiting_quota）：沒有「額度等待」這個前提，換版後仍 unknown 也照樣 rollback。
setup 10 10
export STUB_SUPERVISOR_BEFORE="unknown" STUB_SUPERVISOR="unknown"
rc=$(run)
check_eq "換版前後都 unknown 仍要 rollback（rc=7）" "7" "$rc"
teardown

# 換版前讀不到 supervisor：不能當成 waiting_quota，換版後 waiting_quota 照樣 rollback。
setup 10 10
export STUB_SUPERVISOR_BEFORE="" STUB_SUPERVISOR="waiting_quota"
rc=$(run)
check_eq "換版前 status 讀不到、換版後 waiting_quota 要 rollback（rc=7）" "7" "$rc"
teardown

# issue #771：額度等待是換版前就存在的狀態（跟 binary 無關），換版前後都 waiting_quota 不回滾。
setup 10 10
export STUB_SUPERVISOR_BEFORE="waiting_quota" STUB_SUPERVISOR="waiting_quota"
rc=$(run)
check_eq "換版前後都 waiting_quota 不回滾（rc=0）" "0" "$rc"
check "log 講明是換版前就存在的額度等待" "換版前就是 waiting_quota" "$SWAP_LOG"
check_no "沒有 ROLLBACK" "ROLLBACK requested" "$SWAP_LOG"
check_eq "新 binary 留在線上" "new-binary" "$(tail -1 "$AGM_REPO/target/release/agents-managerd")"
teardown

# issue #771 對抗式審查：換版前是 waiting_quota，只放過「換版後仍是 waiting_quota」這一種；
# 額度等待期間真壞掉的版本（supervisor 變 stopped／unknown、daemon 沒起來、bot 不見）照樣回滾。
for status in stopped failed unknown; do
  setup 10 10
  export STUB_SUPERVISOR_BEFORE="waiting_quota" STUB_SUPERVISOR="$status"
  rc=$(run)
  check_eq "換版前 waiting_quota、換版後 ${status}：照樣 rollback（rc=7）" "7" "$rc"
  check_eq "額度等待中壞掉的版本（${status}）還原舊 binary" "old-binary" "$(cat "$AGM_REPO/target/release/agents-managerd")"
  teardown
done

setup 10 10
export STUB_SUPERVISOR_BEFORE="waiting_quota" STUB_SUPERVISOR="idle"
rc=$(run)
check_eq "換版前 waiting_quota、換版後已恢復 idle：放行（rc=0）" "0" "$rc"
check_no "額度恢復不走 waiting_quota 放行那條" "額度等待與這顆 binary 無關" "$SWAP_LOG"
teardown

setup 10 10
export STUB_SUPERVISOR_BEFORE="waiting_quota" STUB_SUPERVISOR="waiting_quota" STUB_SESSION_OK=""
rc=$(run)
check_eq "waiting_quota 放行不蓋過 /api/session 起不來（rc=7）" "7" "$rc"
check "講的是 session 沒起來" "/api/session 30 秒內沒起來" "$SWAP_LOG"
check_eq "session 起不來還原舊 binary" "old-binary" "$(cat "$AGM_REPO/target/release/agents-managerd")"
teardown

setup 10 10
export STUB_SUPERVISOR_BEFORE="waiting_quota" STUB_SUPERVISOR="waiting_quota" STUB_HEALTH_OK=""
rc=$(run)
check_eq "waiting_quota 放行不蓋過 health 失敗（rc=7）" "7" "$rc"
teardown

setup 10 10
export STUB_SUPERVISOR_BEFORE="waiting_quota" STUB_SUPERVISOR="waiting_quota" STUB_NAMES_AFTER='["a"]'
rc=$(run)
check_eq "waiting_quota 放行不蓋過 bot 不見（rc=7）" "7" "$rc"
check "講的是 bot 不見了" "有 bot 不見了" "$SWAP_LOG"
teardown

# 升過 schema 又在額度等待：waiting_quota 不是失敗，不該被轉成「往前修」（rc=6）。
setup 11 10
export STUB_SUPERVISOR_BEFORE="waiting_quota" STUB_SUPERVISOR="waiting_quota"
rc=$(run)
check_eq "升 schema、前後都 waiting_quota：rc=0，不走往前修" "0" "$rc"
check_no "不講往前修" "forward-fix" "$SWAP_LOG"
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
rc=$(bash "$SCRIPT" --sha "$SHA" --old "$OLD" --old-hash "$OLDHASH" --owner bot-me --checkout "$CHECKOUT" --pane w9:pSTALE >/dev/null 2>&1; echo $?)
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

# 16a. launchd maintenance requests use the scoped service principal and fixed daemon-owned probe.
check "agm 維運請求用 daemon-swap service principal" "AM_SERVICE_ID=daemon-swap" "$SCRIPT"
check "自測送到固定 service route" "/api/services/daemon-swap/probe/" "$SCRIPT"
check_no "自測不再自行宣告 relay_from" "relay_from" "$SCRIPT"
check "舊 daemon bootstrap 自測使用 User session token" 'with urllib.request.urlopen(base + "/api/session")' "$SCRIPT"

# 16b. If the service credential disappeared, do not silently turn a newer daemon client back into User.
setup 10 10
rm -rf "$AM_DATA/service-tokens"
rc=$(run)
check_eq "service 能力無法判定時 fail closed（rc=4）" "4" "$rc"
check_no "能力不明時不取得維運租約" "lease acquire restart" "$AGM_DIR/calls.log"
check_no "能力不明時不替換 binary" "submit" "$AGM_DIR/launchctl.log"
teardown

# 16c. 新 daemon 走 restart-window，不帶 approval；舊 task 多傳的 --approval 要相容忽略。
setup 10 10
rc=$(run)
check_eq "沒有核准單也能換版（rc=0）" "0" "$rc"
check "窗口帶的是 owner 與 commit" "lease acquire restart --owner bot-me --commit $SHA" "$AGM_DIR/calls.log"
check_no "safety 查詢不帶 --approval" "lease safety.*--approval" "$AGM_DIR/calls.log"
teardown

setup 10 10
rc=$(run ap-1)
check_eq "新 daemon 接受舊 task 傳入的 --approval（rc=0）" "0" "$rc"
check_no "restart-window 模式忽略舊 approval" "lease acquire restart.*--approval" "$AGM_DIR/calls.log"
teardown

# 16e. 沒有 restart-window 的舊 daemon 只在明確給 approval 時走舊 User lease acquire。
setup 10 10
export STUB_CAP=service_old
rc=$(run ap-1)
check_eq "舊 daemon 用 approval bootstrap 完成 swap（rc=0）" "0" "$rc"
check "舊路徑以 approval 取得 restart lease" "lease acquire restart --owner bot-me --approval ap-1 --commit $SHA --ttl 900 --exclude-bot bot-me" "$AGM_DIR/calls.log"
check "舊路徑 safety 綁定同一 approval" "lease safety --approval ap-1 --owner bot-me --exclude-bot bot-me" "$AGM_DIR/calls.log"
check "舊路徑清掉 service/bot 身分，使用本機 User token" "identity service= bot=" "$AGM_DIR/identity.log"
teardown

setup 10 10
export STUB_CAP=bootstrap
rm -rf "$AM_DATA/service-tokens"
rc=$(run ap-1)
check_eq "不支援 service principal 的舊 daemon 可用核准 bootstrap（rc=0）" "0" "$rc"
check "bootstrap 使用舊式核准 acquire" "lease acquire restart --owner bot-me --approval ap-1" "$AGM_DIR/calls.log"
teardown

setup 10 10
export STUB_CAP=service_old
rm -rf "$AM_DATA/service-tokens"
rc=$(run ap-1)
check_eq "daemon 宣告 service principal 卻缺 token 時不降級（rc=4）" "4" "$rc"
check_no "缺 service token 時不取得 User 租約" "lease acquire restart" "$AGM_DIR/calls.log"
teardown

# 16g. 找不到與 swap script 同版的 daemon-start.py 時，必須在停舊 daemon 前中止。
setup 10 10
ORIGINAL_SCRIPT="$SCRIPT"
mkdir -p "$ROOT/without-starter"
cp "$SCRIPT" "$ROOT/without-starter/daemon-swap.sh"
SCRIPT="$ROOT/without-starter/daemon-swap.sh"
sleep 60 &
OLD_PID=$!
export STUB_PID="$OLD_PID"
rc=$(run)
if kill -0 "$OLD_PID" 2>/dev/null; then old_alive=yes; else old_alive=no; fi
kill "$OLD_PID" 2>/dev/null || true
wait "$OLD_PID" 2>/dev/null || true
SCRIPT="$ORIGINAL_SCRIPT"
check_eq "缺 daemon-start.py 時 swap rc=3" "3" "$rc"
check_eq "缺啟動器時舊 daemon 未被停止" "yes" "$old_alive"
check_no "缺啟動器時沒有送出重新啟動" "submit" "$AGM_DIR/launchctl.log"
teardown

# 16f. 自測對象沒有在跑（no active run，例如 Linux 上 offline 的 browser-gc child）：略過自測、照樣換版，不卡死每趟自動部署。
setup 10 10
export STUB_PROBE='409 {"error":"conflict","reason":"bot has no active run"}'
rc=$(run)
check_eq "自測對象沒在跑就略過（rc=0）" "0" "$rc"
check "log 講清楚略過的原因" "3b 自測略過" "$SWAP_LOG"
check "照樣換了 binary" "new-binary" "$AGM_DIR/started-binary.log"
teardown

# 16d. 排程（timer／launchd）跑的沒有 pane：改用 herdr pane list 確認 socket 通，不是中止。
setup 10 10
unset HERDR_PANE_ID
export STUB_PANE_CURRENT=""
rc=$(run)
check_eq "沒有自己的 pane 也能往下走（rc=0）" "0" "$rc"
check "改問 pane list" "pane list" "$AGM_DIR/herdr.log"
check "log 講明是排程執行" "3b herdr pane list ok" "$SWAP_LOG"
teardown
setup 10 10
unset HERDR_PANE_ID
export STUB_PANE_CURRENT="" STUB_PANE_LIST_FAIL=1
rc=$(run)
check_eq "herdr 不通就中止（rc=3）" "3" "$rc"
check_no "沒有動 binary" "submit" "$AGM_DIR/launchctl.log"
teardown

# 16h. 舊 daemon 沒有 restart-window 且呼叫端也沒給 approval：要求明確 bootstrap，不拿 User 權限猜。
setup 10 10
export STUB_CAP=service_old
rc=$(run)
check_eq "缺 approval 的舊 daemon 安全中止（rc=9）" "9" "$rc"
check_no "沒有動 binary" "submit" "$AGM_DIR/launchctl.log"
check_no "沒有要窗口" "lease acquire restart" "$AGM_DIR/calls.log"
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
seed_audit "INSERT INTO bots (id,name,project_id,deleted_at) VALUES ('id-kid','kid','p1','2099-01-01T00:00:01.000Z');
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
seed_audit "INSERT INTO bots (id,name,project_id,deleted_at) VALUES ('id-mom','mom','p1','2099-01-01T00:00:01.000Z'), ('id-kid','kid','p1','2099-01-01T00:00:02.000Z');
  INSERT INTO intents VALUES ('it1','delete_bot','id-mom','{\"bots\":[{\"id\":\"id-kid\",\"managed_by\":\"child\"}]}','done','2099-01-01T00:00:00.000Z');"
rc=$(run)
check_eq "連帶刪掉的 child 也不回滾（rc=0）" "0" "$rc"
check "兩顆都算刻意刪除" "deleted_in_window=\[kid mom \]" "$SWAP_LOG"
teardown

# 29. 不見了、只有 deleted_at 沒有刪除紀錄（重啟後 reconcile 找不到 pane 退役、或投影軟刪都是這個樣子）
#     → 照樣回滾。這條不能放寬。
setup 10 10
export STUB_NAMES_BEFORE='["a","b","kid"]' STUB_NAMES_AFTER='["a","b"]'
seed_audit "INSERT INTO bots (id,name,project_id,deleted_at) VALUES ('id-kid','kid','p1','2099-01-01T00:00:01.000Z');"
rc=$(run)
check_eq "沒有刪除紀錄就回滾（rc=7）" "7" "$rc"
check "log 講是沒有刪除紀錄" "有 bot 不見了（沒有刪除紀錄）：kid" "$SWAP_LOG"
check_eq "binary 換回舊的" "old-binary" "$(cat "$AGM_REPO/target/release/agents-managerd")"
teardown

# 30. 刪除紀錄是換版之前的舊帳（例如之前刪過又還原），這次不見的原因不是它 → 照樣回滾。
setup 10 10
export STUB_NAMES_BEFORE='["a","kid"]' STUB_NAMES_AFTER='["a"]'
seed_audit "INSERT INTO bots (id,name,project_id,deleted_at) VALUES ('id-kid','kid','p1','2099-01-01T00:00:01.000Z');
  INSERT INTO intents VALUES ('it0','delete_bot','id-kid','{}','done','2000-01-01T00:00:00.000Z');"
rc=$(run)
check_eq "窗口外的刪除紀錄不算（rc=7）" "7" "$rc"
check "log 講是沒有刪除紀錄" "有 bot 不見了（沒有刪除紀錄）：kid" "$SWAP_LOG"
teardown

# 31. 父 bot 用 `herdr pane close` 收掉 child（沒呼叫 DELETE，#554）：daemon 退役時寫了 retire_child 紀錄，
#     herdr 報過關閉事件、當下 pane 也不在（cause=pane_closed）→ 刻意收掉的，不回滾。promote 收掉的 child 一樣。
setup 10 10
export STUB_NAMES_BEFORE='["a","kid","pro"]' STUB_NAMES_AFTER='["a"]'
seed_audit "INSERT INTO bots (id,name,project_id,deleted_at) VALUES ('id-kid','kid','p1','2099-01-01T00:00:01.000Z'), ('id-pro','pro','p1','2099-01-01T00:00:02.000Z');
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
  seed_audit "INSERT INTO bots (id,name,project_id,deleted_at) VALUES ('id-kid','kid','p1','2099-01-01T00:00:01.000Z');
    INSERT INTO intents VALUES ('it1','retire_child','id-kid','{\"why\":\"reconcile_agent_gone\",\"cause\":\"$c\"}','done','2099-01-01T00:00:01.000Z');"
  rc=$(run)
  check_eq "cause=${c} 的退役照樣回滾（rc=7）" "7" "$rc"
  check "cause=${c}：log 講是沒有刪除紀錄" "有 bot 不見了（沒有刪除紀錄）：kid" "$SWAP_LOG"
  teardown
done

# 33. retire_child 紀錄在換版窗口之前（舊帳），或 subject 是別顆（payload 裡提到它只是 parent_bot_id）→ 不算，回滾。
setup 10 10
export STUB_NAMES_BEFORE='["a","kid"]' STUB_NAMES_AFTER='["a"]'
seed_audit "INSERT INTO bots (id,name,project_id,deleted_at) VALUES ('id-kid','kid','p1','2099-01-01T00:00:01.000Z');
  INSERT INTO intents VALUES ('it0','retire_child','id-kid','{\"cause\":\"pane_closed\"}','done','2000-01-01T00:00:00.000Z'),
    ('it1','retire_child','id-grandkid','{\"cause\":\"pane_closed\",\"parent_bot_id\":\"id-kid\"}','done','2099-01-01T00:00:01.000Z');"
rc=$(run)
check_eq "窗口外、或別顆的退役紀錄都不算（rc=7）" "7" "$rc"
check "log 講是沒有刪除紀錄" "有 bot 不見了（沒有刪除紀錄）：kid" "$SWAP_LOG"
teardown

# 34. child 雖是 unconfirmed，但父 bot 仍有 active run、且窗口內已收編同一父 bot 的新 child：不是遺失。
setup 10 10
export STUB_NAMES_BEFORE='["mom","kid"]' STUB_NAMES_AFTER='["mom","new-kid"]'
seed_audit "INSERT INTO bots (id,name,project_id,deleted_at) VALUES ('id-mom','mom','p1',NULL), ('id-kid','kid','p1','2099-01-01T00:00:01.000Z');
  UPDATE bots SET managed_by='child', parent_bot_id='id-mom' WHERE id='id-kid';
  INSERT INTO bots (id,name,project_id,deleted_at,managed_by,parent_bot_id,created_at)
    VALUES ('id-new-kid','new-kid','p1',NULL,'child','id-mom','2099-01-01T00:00:02.000Z');
  INSERT INTO runs VALUES ('id-mom','running');
  INSERT INTO intents VALUES ('it1','retire_child','id-kid','{\"why\":\"reconcile_agent_gone\",\"cause\":\"unconfirmed\",\"parent_bot_id\":\"id-mom\"}','done','2099-01-01T00:00:01.000Z');"
rc=$(run)
check_eq "父 bot 活著且有窗口內 successor 的 unconfirmed child 不回滾（rc=0）" "0" "$rc"
check_no "有 successor 時不回滾" "ROLLBACK" "$SWAP_LOG"
teardown

# 35. rollback 還原 DB 前要辨認窗口中新 daemon 收編的 child，並在還原後明確警示它仍有 live pane。
setup 10 10
export STUB_NAMES_BEFORE='["mom","kid"]' STUB_NAMES_AFTER='["mom","new-kid"]'
seed_audit "INSERT INTO bots (id,name,project_id,deleted_at) VALUES ('id-mom','mom','p1',NULL), ('id-kid','kid','p1',NULL);
  INSERT INTO bots (id,name,project_id,deleted_at,managed_by,parent_bot_id,created_at)
    VALUES ('id-new-kid','new-kid','p1',NULL,'child','id-mom','2099-01-01T00:00:02.000Z');"
rc=$(run)
check_eq "舊 child 無明確 successor 退役證據仍回滾（rc=7）" "7" "$rc"
check "DB 還原前記下的新收編 child 在回滾後有警示" "WARN: rollback restored the DB but these newly adopted children may still have live panes: new-kid (id-new-kid)" "$SWAP_LOG"
check_eq "binary 照常還原" "old-binary" "$(cat "$AGM_REPO/target/release/agents-managerd")"
teardown

# 39. #726：rollback 首次執行時，必須已定義用來記錄新收編 child 的 helper。
setup 10 10
export STUB_SUPERVISOR=stopped
rc=$(run_capture)
check_eq "forced rollback 完成（rc=7）" "7" "$rc"
check_no "rollback 時 helper 已定義" "rollback_new_children: command not found" "$ROOT/swap.out"
check_no "rollback 不會因缺 helper 遺漏 child 對帳" "could not determine whether newly adopted children" "$SWAP_LOG"
teardown

# 39b. rollback 停新 daemon 不能只 TERM＋固定 sleep 5：新 daemon 還沒退就覆蓋 binary、刪 WAL、蓋掉 DB，會踩在活著的 daemon 底下。
#      跟換版主線一樣要等它退出、等不到才 KILL。假 kill：第 2 次 TERM（rollback 那次）之後 daemon 裝死，直到收到 -KILL。
setup 10 10
export STUB_SUPERVISOR=stopped
: > "$AGM_DIR/kills.log"
kill() {
  echo "kill $*" >> "$AGM_DIR/kills.log"
  if [ "$1" = -0 ]; then
    [ "$(grep -c '^kill -TERM' "$AGM_DIR/kills.log")" -ge 2 ] && ! grep -q '^kill -KILL' "$AGM_DIR/kills.log" && return 0
    return 1
  fi
  return 0
}
export -f kill
rc=$(run_capture)
unset -f kill
check_eq "daemon 裝死的 rollback 仍完成（rc=7）" "7" "$rc"
check "rollback 停不掉新 daemon 時補 KILL" "^kill -KILL" "$AGM_DIR/kills.log"
check_eq "只補一次 KILL（換版主線那次 daemon 正常退出）" "1" "$(grep -c '^kill -KILL' "$AGM_DIR/kills.log")"
check_eq "KILL 之後 binary 才還原" "old-binary" "$(cat "$AGM_REPO/target/release/agents-managerd")"
teardown

# 36. Linux（issue #677）：重啟改走 `systemd-run --user` 的 transient unit，不叫 launchctl。
#     Type=forking（啟動器 fork 後父行程結束，daemon 才是 main PID）與 KillMode=process
#     （systemd 看 cgroup，setsid 脫離不了；只殺 main PID 才跟 macOS 一樣）缺一不可。
setup 10 10
export AGM_OPS_PLATFORM=linux
rc=$(run)
check_eq "Linux 上順利換版 rc=0" "0" "$rc"
check "Linux 走 systemd-run --user 的 transient unit" "^--user --collect --unit=am-daemon-swap-[0-9]*-1 -p Type=forking -p KillMode=process -- /" "$AGM_DIR/systemd-run.log"
check "啟動器用絕對路徑（transient unit 的 cwd 是 HOME）" " $HERE/daemon-start.py $AGM_REPO $DAEMON_LOG\$" "$AGM_DIR/systemd-run.log"
check_eq "Linux 上完全沒叫 launchctl" "" "$(cat "$AGM_DIR/launchctl.log")"
check "有換上新 binary" "new-binary" "$AGM_DIR/started-binary.log"
teardown

# 37. Linux 往前修：第二次啟動用新的 unit 名（上一顆可能還沒被 --collect 收掉，同名會撞）。
setup 11 10
export AGM_OPS_PLATFORM=linux STUB_SUPERVISOR=stopped
rc=$(run)
check_eq "Linux 往前修 rc=6" "6" "$rc"
check "第一次啟動 unit -1" "--unit=am-daemon-swap-[0-9]*-1 " "$AGM_DIR/systemd-run.log"
check "往前修那次換成 unit -2" "--unit=am-daemon-swap-[0-9]*-2 " "$AGM_DIR/systemd-run.log"
teardown

# 38. Linux 沒有 XDG_RUNTIME_DIR（不是從登入 session 叫起來）：補成 /run/user/<uid>，systemd-run 才連得到 user bus；
#     systemd-run 失敗要留 log，交給後面的 /api/session 驗證判斷（這裡驗證也失敗 → 回滾）。
setup 10 10
export AGM_OPS_PLATFORM=linux STUB_SYSTEMD_RUN_RC=1 STUB_SESSION_OK=""
rc=$( unset XDG_RUNTIME_DIR; run )
check "沒有 XDG_RUNTIME_DIR 時補成 /run/user/<uid>" "^XDG_RUNTIME_DIR=/run/user/$(id -u)\$" "$AGM_DIR/systemd-run.env"
check "systemd-run 失敗有留 log" "WARN: systemd-run rc=1" "$SWAP_LOG"
check_eq "起不來就照舊回滾（rc=7）" "7" "$rc"
teardown

# 39. 換 binary 之前先問新 binary 自己是哪個 commit（`--version`），完整比對核准的 --sha：不符就整趟中止（rc=10），
#     窗口都不拿、舊 daemon 不停、binary 不換。daemon 沒辦法驗自己換上去的是不是核准的那顆，只有這支腳本摸得到檔案。
for case_ in "other:0000000000000000000000000000000000000000" "dirty:DIRTY" "unknown:unknown" "old:NONE"; do
  name=${case_%%:*}; val=${case_#*:}
  setup 10 10
  case "$val" in
    DIRTY) export STUB_BIN_VERSION_SHA="${SHA}-dirty" ;;
    NONE)  export STUB_BIN_VERSION_SHA="" ;;   # 舊 binary：`--version` 只印名字與版本，沒有 sha
    *)     export STUB_BIN_VERSION_SHA="$val" ;;
  esac
  rc=$(run)
  check_eq "新 binary 的 sha 不對（${name}）→ rc=10" "10" "$rc"
  check "log 講明 binary 內嵌的 sha 對不上" "內嵌" "$SWAP_LOG"
  check_no "沒有去拿窗口" "lease acquire restart" "$AGM_DIR/calls.log"
  check_file "沒有啟動任何東西" no "$AGM_DIR/started"
  check_eq "線上 binary 沒動" "old-binary" "$(cat "$AGM_REPO/target/release/agents-managerd")"
  check_file "沒有寫 .built" no "$AGM_DIR/daemon-update.built"
  teardown
done

# 40. 核准 sha 必須是完整 commit id：短前綴不足以證明 checkout 和 binary 是同一顆。
setup 10 10
PREFIX_SHA=$(printf '%s' "$SHA" | cut -c1-7)
rc=$(run "" "$PREFIX_SHA")
check_eq "只給 7 碼前綴就拒絕（rc=10）" "10" "$rc"
check_no "短前綴沒有去拿窗口" "lease acquire restart" "$AGM_DIR/calls.log"
check_file "短前綴沒有啟動任何東西" no "$AGM_DIR/started"
check_eq "短前綴沒有換正式 binary" "old-binary" "$(cat "$AGM_REPO/target/release/agents-managerd")"
teardown

# 完整 sha 對得上，換版照常完成。
setup 10 10
export STUB_BIN_VERSION_SHA="$SHA"
rc=$(run)
check_eq "binary 的 sha 對上 → 照常換版（rc=0）" "0" "$rc"
check "log 記下驗過的 sha" "binary sha ok" "$SWAP_LOG"
teardown

# binary 回報「完整核准 sha + 其他字元」不算相符；不允許前綴比對放過錯誤／偽造回報。
setup 10 10
export STUB_BIN_VERSION_SHA="${SHA}f"
rc=$(run)
check_eq "binary sha 後面多一碼要拒絕（rc=10）" "10" "$rc"
check_no "binary sha 多一碼沒有去拿窗口" "lease acquire restart" "$AGM_DIR/calls.log"
check_eq "binary sha 多一碼沒有換正式 binary" "old-binary" "$(cat "$AGM_REPO/target/release/agents-managerd")"
teardown

# 41. 新 daemon 起來後再用 API 回報的 sha 複核一次：binary 驗過了，但起來的不是它（例如啟動器拉起別顆）→ 回滾（rc=7）。
setup 10 10
export STUB_DAEMON_SHA="1111111111111111111111111111111111111111"
rc=$(run)
check_eq "新 daemon 回報的 sha 不是核准的 → 回滾（rc=7）" "7" "$rc"
check "log 講明是 sha 複核失敗" "回報的 sha" "$SWAP_LOG"
teardown

setup 10 10
export STUB_DAEMON_SHA="${SHA}f"
rc=$(run)
check_eq "daemon 回報的 sha 多一碼要 rollback（rc=7）" "7" "$rc"
check_eq "daemon 回報多一碼後還原舊 binary" "old-binary" "$(cat "$AGM_REPO/target/release/agents-managerd")"
check "daemon 回報多一碼講明 sha 不符" "回報的 sha" "$SWAP_LOG"
teardown

# 42. 新 daemon 是髒樹建的（dirty:true）→ 回滾；沒回報 sha_full 的 daemon（讀不到）也不放行。
setup 10 10
export STUB_DAEMON_DIRTY=true
rc=$(run)
check_eq "新 daemon 說自己是髒樹建的 → 回滾（rc=7）" "7" "$rc"
teardown

echo "$PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
