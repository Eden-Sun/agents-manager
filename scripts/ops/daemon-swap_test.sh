#!/bin/bash
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
SCRIPT="$HERE/daemon-swap.sh"
PASS=0
FAIL=0

check() {
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

check_file() {
  got=no
  [ -e "$3" ] && got=yes
  if [ "$got" = "$2" ]; then
    echo "ok   - $1"; PASS=$((PASS + 1))
  else
    echo "FAIL - $1（存在=${got}，預期=$2：$3）"; FAIL=$((FAIL + 1))
  fi
}

check_eq() {
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

setup() {
  if [ -n "${ROOT:-}" ] && [ -d "$ROOT" ]; then rm -rf "$ROOT"; fi
  ROOT=$(mktemp -d); export ROOT
  export AGM_DIR="$ROOT/agm" AGM_REPO="$ROOT/repo" CHECKOUT="$ROOT/checkout"
  export AM_DATA="$ROOT/data" DAEMON_DB="$ROOT/am.sqlite3" DAEMON_LOG="$ROOT/daemon.log" SWAP_LOG="$ROOT/swap.log"
  export SWAP_SETTLE_SECS=0 SWAP_WINDOW_TRIES=3 SWAP_WINDOW_WAIT_SECS=0
  export AM_PORT_SWAP=1
  export STUB_ACQUIRE_FAIL_TIMES=0
  export STUB_ACQUIRE_EMPTY_WORKING=""
  export STUB_PROBE_INFLIGHT_TIMES=0
  export STUB_SWAP_BINARY_ON_ACQUIRE=""
  export SWAP_PROBE_SETTLE_TRIES=5 SWAP_PROBE_SETTLE_WAIT_SECS=0
  export STUB_AGM_STATE_FAIL="" STUB_AGM_SUPERVISOR_FAIL=""
  export STUB_AGM_SUPERVISOR_FAIL_AT=0
  export STUB_DELAY_RETIRE_INTENT_FOR=""
  export STUB_PID=99999
  export STUB_PID_AFTER_START=""
  export STUB_NO_PID=""
  export AGM_BIN="$ROOT/bin/agm" SQLITE_BIN="$ROOT/bin/sqlite3" CURL_BIN="$ROOT/bin/curl"
  export LAUNCHCTL_BIN="$ROOT/bin/launchctl" PGREP_BIN="$ROOT/bin/pgrep" HERDR_BIN="$ROOT/bin/herdr"
  export SYSTEMD_RUN_BIN="$ROOT/bin/systemd-run" AGM_OPS_PLATFORM=darwin
  export SWAP_PROBE_TRIES=2 SWAP_PROBE_BOT=bot-probe
  export HERDR_PANE_ID="w1:pA"
  export STUB_CAP=service STUB_PANE_LIST_FAIL=""
  export STUB_PANE_READ_OK=1 STUB_PANE_CURRENT="w1:pA" STUB_PROBE='200 {"delivery":"ok"}' 
  mkdir -p "$AGM_DIR" "$AM_DATA/service-tokens" "$AGM_REPO/target/release" "$CHECKOUT/crates/am-base/src" "$CHECKOUT/target/release" "$ROOT/bin"
  printf 'test-daemon-swap-service-token\n' > "$AM_DATA/service-tokens/daemon-swap.token"
  chmod 600 "$AM_DATA/service-tokens/daemon-swap.token"

  { echo 'const SCHEMA_HISTORY: &[(i64, &str)] = &['
    echo '    (9, "aaa"),'
    echo '    (10, "bbb"),'
    [ "$1" -ge 11 ] && echo "    ($1, \"ccc\"),"
    echo '];'; } > "$CHECKOUT/crates/am-base/src/db.rs"
  { echo '#!/bin/sh'
    echo '[ "$1" = --version ] && printf "agents-managerd 0.1.0 %s\n" "$STUB_BIN_VERSION_SHA"; exit 0'
    echo 'new-binary'; } > "$CHECKOUT/target/release/agents-managerd"; chmod +x "$CHECKOUT/target/release/agents-managerd"
  /usr/bin/git init -q "$CHECKOUT"
  ( cd "$CHECKOUT" && /usr/bin/git config user.email t@t && /usr/bin/git config user.name t \
      && /usr/bin/git add -A && /usr/bin/git commit -qm live \
      && /usr/bin/git commit -q --allow-empty -m target ) >/dev/null 2>&1
  export SHA=$(/usr/bin/git -C "$CHECKOUT" rev-parse HEAD)
  export STUB_BIN_VERSION_SHA="$SHA"
  export STUB_DAEMON_SHA="$SHA"
  export OLD=$(/usr/bin/git -C "$CHECKOUT" rev-parse HEAD~1)

  printf 'old-binary\n' > "$AGM_REPO/target/release/agents-managerd"; chmod +x "$AGM_REPO/target/release/agents-managerd"
  export OLDHASH=$(shasum -a 256 "$AGM_REPO/target/release/agents-managerd" | cut -c1-16)

  printf 'db@%s\n' "$2" > "$DAEMON_DB"
  printf 'stale-wal\n' > "$DAEMON_DB-wal"
  printf 'stale-shm\n' > "$DAEMON_DB-shm"
  echo "$2" > "$ROOT/uv"
  echo ok > "$ROOT/integrity"
  : > "$DAEMON_LOG"
  export STUB_UV_AFTER_START="$1"
  export STUB_SESSION_OK=1 STUB_SESSION_OK_AFTER_FORWARD=1 STUB_SESSION_OK_AFTER_ROLLBACK=1 STUB_HEALTH_OK=1
  export STUB_ACQUIRE_HELD=true STUB_SAFE=true STUB_SUPERVISOR=idle
  export STUB_WINDOW_APPROVAL=ap-auto-1
  export STUB_RECHECK_WORKING="" STUB_RECHECK_DELIVERING="" STUB_ESCALATED_APPROVAL=""
  export STUB_WINDOW_ESCALATED_BY="" STUB_RECHECK_UNREADABLE="" STUB_RECHECK_OTHER_LEASE="" STUB_RECHECK_NO_FIELDS=""
  unset STUB_SUPERVISOR_BEFORE STUB_QUOTA_RESET_AT
  export STUB_RELEASE_FAIL=""
  export STUB_RENEW_FAIL=""
  export STUB_NAMES_BEFORE='["a","b"]' STUB_NAMES_AFTER='["a","b"]'
  export STUB_RESTART_HELD=false
  export REAL_SQLITE="${REAL_SQLITE:-$(command -v sqlite3)}"
  "$REAL_SQLITE" "$ROOT/audit.sqlite3" "CREATE TABLE bots (id TEXT PRIMARY KEY, name TEXT, project_id TEXT, deleted_at TEXT);
    ALTER TABLE bots ADD COLUMN managed_by TEXT;
    ALTER TABLE bots ADD COLUMN parent_bot_id TEXT;
    ALTER TABLE bots ADD COLUMN created_at TEXT;
    CREATE TABLE intents (id TEXT PRIMARY KEY, kind TEXT, subject_id TEXT, payload_json TEXT NOT NULL DEFAULT '{}',
      status TEXT NOT NULL DEFAULT 'done', created_at TEXT NOT NULL,
      updated_at TEXT NOT NULL DEFAULT '2000-01-01T00:00:00.000Z');
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
                 if [ -n "${STUB_RECHECK_WORKING:-}${STUB_RECHECK_DELIVERING:-}" ]; then
                   # 照 daemon `safety_as` 的判斷：全靜止 || （放寬 && 沒人在送達臨界區）。放寬綁 `--approval` 那一張。
                   ap=""; nxt=0
                   for a in "$@"; do
                     [ "$nxt" = 1 ] && { ap="$a"; nxt=0; continue; }
                     [ "$a" = "--approval" ] && nxt=1
                   done
                   esc=false; [ -n "$ap" ] && [ "$ap" = "${STUB_ESCALATED_APPROVAL:-}" ] && esc=true
                   wk=""; [ -n "${STUB_RECHECK_WORKING:-}" ] && wk="{\"bot_id\":\"id-$STUB_RECHECK_WORKING\",\"name\":\"$STUB_RECHECK_WORKING\"}"
                   dl=""; [ -n "${STUB_RECHECK_DELIVERING:-}" ] && dl="{\"bot_id\":\"id-$STUB_RECHECK_DELIVERING\",\"name\":\"$STUB_RECHECK_DELIVERING\",\"turn_id\":\"t-9\"}"
                   ur=""; [ -n "${STUB_RECHECK_UNREADABLE:-}" ] && ur="{\"bot_id\":\"id-$STUB_RECHECK_UNREADABLE\",\"name\":\"$STUB_RECHECK_UNREADABLE\"}"
                   hl='{"resource":"restart","owner":"bot-me","own":true,"fence":9}'
                   [ -n "${STUB_RECHECK_OTHER_LEASE:-}" ] && hl="$hl,{\"resource\":\"rebuild\",\"owner\":\"$STUB_RECHECK_OTHER_LEASE\",\"own\":false,\"fence\":3}"
                   safe=false
                   if [ -z "$dl" ] && [ -z "$ur" ] && [ -z "${STUB_RECHECK_OTHER_LEASE:-}" ] && { [ -z "$wk" ] || [ "$esc" = true ]; }; then safe=true; fi
                   if [ -n "${STUB_RECHECK_NO_FIELDS:-}" ]; then
                     printf '{"safe":%s,"escalated":%s,"working":[%s],"delivering":[%s],"in_flight":[%s]}' "$safe" "$esc" "$wk" "$dl" "$fl"
                   else
                     printf '{"safe":%s,"escalated":%s,"working":[%s],"delivering":[%s],"unreadable":[%s],"held_leases":[%s],"in_flight":[%s]}' "$safe" "$esc" "$wk" "$dl" "$ur" "$hl" "$fl"
                   fi
                 else
                   printf '{"safe":%s,"working":[],"delivering":[],"in_flight":[%s]}' "$STUB_SAFE" "$fl"
                 fi ;;
  lease:acquire) printf '{"lease":{"held":true,"fence":9},"lease_token":"tok-legacy"}' ;;
  lease:renew)
      echo "lease renew restart --owner bot-me --fence 9 --ttl 900" >> "$AGM_DIR/calls.log"
      [ -z "$STUB_RENEW_FAIL" ] || { printf '{"error":"lease_lost"}'; exit 1; }
      printf '{"renewed":true}' ;;
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
                 printf '{"status":"%s","quota_reset_at":%s,"last_deploy":{"sha":"x","sha_full":"%s","dirty":%s}}' "$STUB_SUPERVISOR" "$( [ -n "${STUB_QUOTA_RESET_AT:-}" ] && printf '"%s"' "$STUB_QUOTA_RESET_AT" || printf null )" "$STUB_DAEMON_SHA" "${STUB_DAEMON_DIRTY:-false}" ;;
  state:*)       if [ -e "$AGM_DIR/started" ]; then
                   phase=after
                   n=$(cat "$ROOT/after-state-reads" 2>/dev/null || echo 0); n=$((n + 1)); echo "$n" > "$ROOT/after-state-reads"
                   if [ -n "${STUB_AFTER_READS_BEFORE_DROP:-}" ] && [ "$n" -gt "$STUB_AFTER_READS_BEFORE_DROP" ]; then
                     names="$STUB_NAMES_AFTER_LATER"
                   else
                     names="$STUB_NAMES_AFTER"
                   fi
                 else phase=before; names="$STUB_NAMES_BEFORE"; fi
                 [ "$STUB_AGM_STATE_FAIL" != "$phase" ] || { printf '{}'; exit 1; }
                 printf '{"bots":['; sep=""
                 for n in $(printf '%s' "$names" | tr -d '[]"' | tr ',' ' '); do printf '%s{"id":"id-%s","name":"%s"}' "$sep" "$n" "$n"; sep=","; done
                 printf ']}' ;;
  *)             printf '{}' ;;
esac
STUB

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
if [ -n "${STUB_WINDOW_ESCALATED_BY:-}" ]; then
  # acquire 在鎖內判定放寬時，回應的 safety 帶 escalated 與計時用的那張核准（舊 daemon 97803818 起就有）。
  printf '{"lease":{"held":%s,"fence":9},"lease_token":"tok-1","approval":{"id":"%s"},"safety":{"safe":true,"escalated":true,"escalation_approval_id":"%s"}}' \
    "$STUB_ACQUIRE_HELD" "$STUB_WINDOW_APPROVAL" "$STUB_WINDOW_ESCALATED_BY"
elif [ -n "${STUB_WINDOW_APPROVAL:-}" ]; then
  printf '{"lease":{"held":%s,"fence":9},"lease_token":"tok-1","approval":{"id":"%s"}}' "$STUB_ACQUIRE_HELD" "$STUB_WINDOW_APPROVAL"
else
  printf '{"lease":{"held":%s,"fence":9},"lease_token":"tok-1"}' "$STUB_ACQUIRE_HELD"
fi
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
     # Regression fixture: a reconcile retire intent becomes visible after the first
     # deleted_on_purpose lookup, matching the boot-time race from issue #834.
     case "$q" in
       "SELECT count(*) FROM bots b WHERE b.id = '$STUB_DELAY_RETIRE_INTENT_FOR'"*)
         if [ -n "$STUB_DELAY_RETIRE_INTENT_FOR" ]; then
           n=$(cat "$ROOT/delayed-retire-lookups" 2>/dev/null || echo 0); n=$((n + 1))
           echo "$n" > "$ROOT/delayed-retire-lookups"
           if [ "$n" -ge 2 ] && [ ! -e "$ROOT/delayed-retire-inserted" ]; then
             "$REAL_SQLITE" "$ROOT/audit.sqlite3" "INSERT INTO intents (id,kind,subject_id,payload_json,status,created_at) VALUES ('it-delayed','retire_child','id-kid','{\"why\":\"reconcile_agent_gone\",\"cause\":\"unconfirmed\",\"mode\":\"implicit\",\"pane\":\"gone\",\"parent_bot_id\":\"id-mom\"}','done','2099-01-01T00:00:01.000Z');"
             : > "$ROOT/delayed-retire-inserted"
           fi
         fi ;;
     esac
     "$REAL_SQLITE" -readonly "$ROOT/audit.sqlite3" "$q" ;;
esac
STUB

  cat > "$ROOT/bin/curl" <<'STUB'
#!/bin/bash
# 回滾起回的舊 binary（內容是 old-binary）：成不成功由 STUB_SESSION_OK_AFTER_ROLLBACK 決定（issue #1102：回滾後也要驗它起得來）。
if [ "$(cat "$AGM_REPO/target/release/agents-managerd" 2>/dev/null)" = old-binary ]; then [ -n "$STUB_SESSION_OK_AFTER_ROLLBACK" ] && exit 0 || exit 7; fi
# 第二次啟動（launchctl submit 或 systemd-run）之後＝往前修那次重啟；成不成功由 STUB_SESSION_OK_AFTER_FORWARD 決定。
starts=$(wc -l < "$AGM_DIR/starts.log" 2>/dev/null | tr -d ' '); starts=${starts:-0}
if [ "$starts" -ge 2 ]; then [ -n "$STUB_SESSION_OK_AFTER_FORWARD" ] && exit 0 || exit 7; fi
[ -n "$STUB_SESSION_OK" ] && exit 0 || exit 7
STUB

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
[ -z "${STUB_NO_PID:-}" ] || exit 1
if [ -n "${STUB_PID_AFTER_START:-}" ] && [ -e "$AGM_DIR/started" ]; then
  printf '%s\n' "$STUB_PID_AFTER_START"
else
  printf '%s\n' "${STUB_PID:-99999}"
fi
STUB

  cat > "$ROOT/bin/probe" <<'STUB'
#!/bin/bash
echo "$*" >> "$AGM_DIR/probe.log"
n=$(wc -l < "$AGM_DIR/probe.log" | tr -d ' ')
if [ -n "$STUB_PROBE_FIRST_INFLIGHT" ] && [ "$n" = 1 ]; then
  printf '409 {"error":"conflict","reason":"a turn is already in flight"}'; exit 0
fi
case " $STUB_PROBE_DRAFT_BOTS " in *" $1 "*) printf '409 {"draft":"0","draft_actions":["submit","clear"],"draft_token":"t"}'; exit 0 ;; esac
case " $STUB_PROBE_BLOCKED_BOTS " in *" $1 "*) printf '409 {"error":"conflict","reason":"agent is blocked; answer the prompt first","run_id":"r1"}'; exit 0 ;; esac
case " $STUB_PROBE_NOTRUNNING_BOTS " in *" $1 "*) printf '409 {"error":"conflict","reason":"run is not running","state":"starting"}'; exit 0 ;; esac
case " $STUB_PROBE_OTHER409_BOTS " in *" $1 "*) printf '409 {"error":"conflict","reason":"something else"}'; exit 0 ;; esac
if [ -n "$STUB_PROBE_NO_RUN_BOT" ] && [ "$1" = "$STUB_PROBE_NO_RUN_BOT" ]; then
  printf '404 {"error":"not_found","reason":"no active run"}'; exit 0
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
seed_audit() { "$REAL_SQLITE" "$ROOT/audit.sqlite3" "$1"; }

run() {
  local approval="${1:-}"
  local sha="${2:-$SHA}"
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

setup 10 10
printf 'old-backup-1\n' > "$DAEMON_DB.bak-20000101-0000"
printf 'old-backup-2\n' > "$DAEMON_DB.bak-20010101-0000"
printf 'stale-bak\n' > "$AGM_REPO/target/release/agents-managerd.bak-0000dead"
printf 'stale-prev\n' > "$AGM_REPO/target/release/agents-managerd.prev-0000dead-20000101-000000-1"
ln -s "$SCRIPT" "$AGM_REPO/target/release/agents-managerd.bak-0000link"
rc=$(run)
check_eq "順利時 rc=0" "0" "$rc"
check_file "成功後清掉舊的 .bak- binary 備份" no "$AGM_REPO/target/release/agents-managerd.bak-0000dead"
check_file "成功後清掉舊的 .prev- binary 備份" no "$AGM_REPO/target/release/agents-managerd.prev-0000dead-20000101-000000-1"
check_file "binary 備份只清一般檔，符號連結不碰" yes "$AGM_REPO/target/release/agents-managerd.bak-0000link"
check_file "成功後保留這趟的 .bak- 備份" yes "$AGM_REPO/target/release/agents-managerd.bak-$OLD"
check_eq "成功後只剩這趟的 .prev- 備份" "1" "$(ls "$AGM_REPO"/target/release/agents-managerd.prev-* 2>/dev/null | wc -l | tr -d ' ')"
check "預期版本從 checkout 讀出來" "checkout SCHEMA_VERSION=10, db user_version=10, bumped=no" "$SWAP_LOG"
check "有換上新 binary" "new-binary" "$AGM_DIR/started-binary.log"
check_eq ".built 寫的是這次的 sha" "$(echo "$SHA" | cut -c1-8)" "$(cat "$AGM_DIR/daemon-update.built")"
check "啟動走 launchd（nice 0）" "submit -l am-daemon-swap" "$AGM_DIR/launchctl.log"
check "停機前用原持有權續約 restart lease" "lease renew restart --owner bot-me --fence 9 --ttl 900" "$AGM_DIR/calls.log"
check_no "不是在 pane 裡直接背景起" "nohup" "$SCRIPT"
check_file "成功後保留這趟 DB 備份" yes "$(logged_db_backup)"
check_file "成功後刪除較舊 DB 備份" no "$DAEMON_DB.bak-20000101-0000"
check_file "成功後刪除另一份較舊 DB 備份" no "$DAEMON_DB.bak-20010101-0000"
check_eq "成功後只保留一份 DB 備份" "1" "$(db_backup_count)"
teardown

setup 10 10
export STUB_RENEW_FAIL=1
rc=$(run)
check_eq "停機前 lease renew 失敗就 defer（rc=4）" "4" "$rc"
check "失敗的續約已留下錯誤" "換版前 restart lease 續約失敗" "$SWAP_LOG"
check "續約失敗後仍嘗試歸還窗口" "lease release restart" "$AGM_DIR/calls.log"
check_eq "續約失敗沒有重啟 daemon" "0" "$(wc -l < "$AGM_DIR/starts.log" | tr -d ' ')"
check_eq "續約失敗保留舊 binary" "old-binary" "$(cat "$AGM_REPO/target/release/agents-managerd")"
teardown

setup 10 10
mkdir "$AGM_DIR/daemon-update.built"
rc=$(run)
check_eq "built marker 寫入失敗回報 rc=11" "11" "$rc"
check "built marker 寫入失敗明確記錄" "部署成功但寫入 daemon-update.built 失敗" "$SWAP_LOG"
check "健康的新 binary 保持在線" "new-binary" "$AGM_DIR/started-binary.log"
check_no "marker 失敗不回滾健康 binary" "ROLLBACK requested" "$SWAP_LOG"
teardown

setup 10 10
( umask 022; rc=$(run); check_eq "備份權限：順利時 rc=0" "0" "$rc" )
BK=$(logged_db_backup)
MODE=$(stat -c '%a' "$BK" 2>/dev/null || stat -f '%Lp' "$BK")
check_eq "DB 備份檔權限是 600（呼叫端 umask 022 也一樣）" "600" "$MODE"
teardown

setup 11 10
printf 'precious-pre-migration-backup\n' > "$DAEMON_DB.bak-samestamp"
rc=$(SWAP_BACKUP_STAMP=samestamp run)
check_eq "備份檔已存在：rc=5" "5" "$rc"
check_eq "既有的備份一個字都沒動" "precious-pre-migration-backup" "$(cat "$DAEMON_DB.bak-samestamp")"
check_eq "沒有重啟 daemon" "0" "$(wc -l < "$AGM_DIR/starts.log" | tr -d ' ')"
teardown

setup 11 10
rc=$(STUB_BACKUP_FAIL=1 run)
check_eq "備份失敗：rc=5" "5" "$rc"
check "備份失敗要交還窗口" "lease release restart" "$AGM_DIR/calls.log"
check_eq "沒有重啟 daemon" "0" "$(wc -l < "$AGM_DIR/starts.log" | tr -d ' ')"
teardown

setup 10 11
rc=$(run)
check_eq "DB 比目標 binary 新：rc=3" "3" "$rc"
check "log 講明是 schema 比 binary 新" "比要換上的" "$SWAP_LOG"
check_eq "沒有重啟 daemon" "0" "$(wc -l < "$AGM_DIR/starts.log" | tr -d ' ')"
check_eq "線上 binary 沒被動" "old-binary" "$(cat "$AGM_REPO/target/release/agents-managerd")"
check_eq "沒有拍備份" "0" "$(db_backup_count)"
teardown

setup 10 10
: > "$ROOT/uv"
rc=$(run)
check_eq "讀不到 user_version：rc=3" "3" "$rc"
check_eq "沒有重啟 daemon" "0" "$(wc -l < "$AGM_DIR/starts.log" | tr -d ' ')"
teardown

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

setup 10 10
export STUB_SWAP_BINARY_ON_ACQUIRE=1
rc=$(run)
check_eq "窗口前 live 改變時 swap rc=3" "3" "$rc"
check "說明 live 已改變並重讀" "取得 restart 窗口後線上 binary 已改變" "$SWAP_LOG"
check "歸還 restart 窗口" "restart 窗口已交還（線上 binary 已改變）" "$SWAP_LOG"
check_no "沒有啟動較舊 binary" "new-binary" "$AGM_DIR/started-binary.log"
check_eq "保留取得窗口時的 live" "newer-live-binary" "$(cat "$AGM_REPO/target/release/agents-managerd")"
teardown

setup 11 10
rc=$(run)
check_eq "升 schema 也是 rc=0" "0" "$rc"
check "認得出這批升了 schema" "SCHEMA_VERSION=11, db user_version=10, bumped=yes" "$SWAP_LOG"
check "用 checkout 的版本比對，不是寫死的數字" "user_version=11 (expected 11)" "$SWAP_LOG"
check_no "不會把 migrate 成功當成失敗回滾" "ROLLBACK" "$SWAP_LOG"
teardown

setup 11 10
export STUB_SUPERVISOR=stopped
rc=$(run)
check_eq "往前修之後以 rc=6 結束（不是回滾）" "6" "$rc"
check "講清楚往前修的理由" "先往前修（舊 binary 開不了這個 DB）" "$SWAP_LOG"
check "往前修成功就停在新 binary" "forward-fix ok" "$SWAP_LOG"
check_no "沒有還原 DB" "db restored" "$SWAP_LOG"
check_eq "線上留著新 binary" "new-binary" "$(tail -1 "$AGM_DIR/started-binary.log")"
teardown

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

setup 10 10
printf 'older-backup\n' > "$DAEMON_DB.bak-20000101-0000"
printf 'stale-bak\n' > "$AGM_REPO/target/release/agents-managerd.bak-0000dead"
printf 'stale-prev\n' > "$AGM_REPO/target/release/agents-managerd.prev-0000dead-20000101-000000-1"
export STUB_SUPERVISOR=stopped
rc=$(run)
check_eq "沒升 schema 的失敗走回滾（rc=7）" "7" "$rc"
check_file "回滾不清舊的 .bak- binary 備份" yes "$AGM_REPO/target/release/agents-managerd.bak-0000dead"
check_file "回滾不清舊的 .prev- binary 備份" yes "$AGM_REPO/target/release/agents-managerd.prev-0000dead-20000101-000000-1"
check_no "不會講往前修" "forward-fix" "$SWAP_LOG"
check_no "沒升 schema 不還原 DB" "db restored" "$SWAP_LOG"
check "說明 DB 原樣保留" "db left as is" "$SWAP_LOG"
check_eq "binary 換回舊的" "old-binary" "$(cat "$AGM_REPO/target/release/agents-managerd")"
check "swap 回滾時先持久化 rejected sha" "$SHA" "$AGM_DIR/daemon-update.rejected"
check_eq "DB 沒被動" "db@10" "$(cat "$DAEMON_DB")"
check_eq "-wal 原樣保留" "stale-wal" "$(cat "$DAEMON_DB-wal" 2>/dev/null)"
check_eq "-shm 原樣保留" "stale-shm" "$(cat "$DAEMON_DB-shm" 2>/dev/null)"
check_file "回滾後保留這趟 DB 備份" yes "$(logged_db_backup)"
check_file "回滾後保留舊 DB 備份" yes "$DAEMON_DB.bak-20000101-0000"
check_eq "回滾後保留新舊兩份 DB 備份" "2" "$(db_backup_count)"
teardown

setup 10 10
export STUB_SUPERVISOR=stopped STUB_SESSION_OK_AFTER_ROLLBACK=""
rc=$(run)
check_eq "回滾後舊 daemon 沒起來：rc=12（不是已回滾的 7）" "12" "$rc"
check "log 講明回滾後 daemon 沒有在服務" "回滾後 daemon 沒有在服務" "$SWAP_LOG"
check_no "沒起來時不說已回滾" "rolled back pid" "$SWAP_LOG"
check_eq "舊 binary 確實被起過" "old-binary" "$(tail -1 "$AGM_DIR/started-binary.log")"
teardown

setup 10 10
export STUB_SUPERVISOR=stopped STUB_MUTATE_DB=1
rc=$(run)
check_eq "沒升 schema 的回滾 rc=7" "7" "$rc"
check_eq "新 daemon 寫過的 DB 內容保留" "mutated-by-new-binary" "$(cat "$DAEMON_DB")"
check_eq "舊 binary 被起回來" "old-binary" "$(tail -1 "$AGM_DIR/started-binary.log")"
teardown
unset STUB_MUTATE_DB

setup 11 10
export STUB_SUPERVISOR=stopped STUB_SESSION_OK_AFTER_FORWARD="" STUB_MUTATE_DB=1
rc=$(run)
check_eq "原子還原：回滾 rc=7" "7" "$rc"
check_eq "DB 內容是備份的（新版寫的被還原掉）" "db@10" "$(cat "$DAEMON_DB")"
check_file "還原後 -wal 清掉" no "$DAEMON_DB-wal"
check_file "還原後 -shm 清掉" no "$DAEMON_DB-shm"
check_eq "還原後的 DB 權限 600" "600" "$(stat -c '%a' "$DAEMON_DB" 2>/dev/null || stat -f '%Lp' "$DAEMON_DB")"
LEFT=$(ls "$ROOT" | grep -c 'restore' || true)
check_eq "沒有留下暫存檔" "0" "$LEFT"
teardown

setup 11 10
export STUB_SUPERVISOR=stopped STUB_SESSION_OK_AFTER_FORWARD="" STUB_MUTATE_DB=1
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
check_eq "DB 還原失敗不起任何 daemon，以 rc=11 留給人處理（不是已回滾的 7）" "11" "$rc"
check_eq "DB 還原失敗時沒起舊 binary：最後一次還是換版那次" "new-binary" "$(tail -1 "$AGM_DIR/started-binary.log")"
check_eq "原 DB 沒被動（新版寫的內容還在）" "mutated-by-new-binary" "$(cat "$DAEMON_DB")"
check_eq "原 -wal 沒被刪" "stale-wal" "$(cat "$DAEMON_DB-wal" 2>/dev/null)"
check_eq "原 -shm 沒被刪" "stale-shm" "$(cat "$DAEMON_DB-shm" 2>/dev/null)"
LEFT=$(ls "$ROOT" | grep -c 'restore' || true)
check_eq "失敗後不留暫存檔" "0" "$LEFT"
check "log 講清楚 DB 還原失敗" "DB 還原失敗" "$SWAP_LOG"
check_no "失敗時不能說還原成功" "db restored from" "$SWAP_LOG"
teardown
unset STUB_MUTATE_DB

setup 10 10
mkdir -p "$ROOT/fakecp"
cat > "$ROOT/fakecp/cp" <<'STUB'
#!/bin/bash
for a in "$@"; do dest=$a; done
case "$dest" in
  *agents-managerd.new.*) printf 'half-written' > "$dest"; exit 1 ;;   # 換版用的暫存檔：寫一半就失敗
esac
exec /bin/cp "$@"
STUB
chmod +x "$ROOT/fakecp/cp"
rc=$(SWAP_BACKUP_STAMP=first PATH="$ROOT/fakecp:$PATH" run)
check_eq "換版 cp 失敗 rc=5" "5" "$rc"
check_eq "正式 binary 還是舊的" "old-binary" "$(cat "$AGM_REPO/target/release/agents-managerd")"
check_eq "舊 daemon 被起回來" "old-binary" "$(tail -1 "$AGM_DIR/started-binary.log")"
check_no "不能說已回滾" "rolled back" "$SWAP_LOG"
check "log 講清楚複製失敗" "新 binary 複製到 target/release 失敗" "$SWAP_LOG"
check_file "不留截斷的暫存檔" no "$AGM_REPO/target/release/agents-managerd.new.*"
check_file "沒有寫 .rejected" no "$AGM_DIR/daemon-update.rejected"
rc=$(SWAP_BACKUP_STAMP=second run)
check_eq "cp 恢復後再跑就成功（不是卡在 rc=3）" "0" "$rc"
check_eq "第二次換上新 binary" "new-binary" "$(tail -1 "$AGM_DIR/started-binary.log")"
teardown

setup 10 10
mkdir -p "$ROOT/fakecp"
cat > "$ROOT/fakecp/cp" <<'STUB'
#!/bin/bash
for a in "$@"; do dest=$a; done
case "$dest" in
  *agents-managerd.bak-*) printf 'half-written' > "$dest"; exit 1 ;;
esac
exec /bin/cp "$@"
STUB
chmod +x "$ROOT/fakecp/cp"
rc=$(SWAP_BACKUP_STAMP=first PATH="$ROOT/fakecp:$PATH" run)
check_eq "備份 cp 失敗 rc=3（什麼都沒動）" "3" "$rc"
check_file "沒有留下半截的 .bak-<old>" no "$AGM_REPO/target/release/agents-managerd.bak-$OLD"
check_eq "正式 binary 沒動" "old-binary" "$(cat "$AGM_REPO/target/release/agents-managerd")"
check_file "沒有啟動任何東西" no "$AGM_DIR/started-binary.log"
rc=$(SWAP_BACKUP_STAMP=second run)
check_eq "備份恢復正常後再跑就成功" "0" "$rc"
teardown

setup 10 10
mkdir -p "$ROOT/fakedf"
cat > "$ROOT/fakedf/df" <<'STUB'
#!/bin/sh
printf 'Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/x 100 99 0 100%% /\n'
STUB
chmod +x "$ROOT/fakedf/df"
rc=$(PATH="$ROOT/fakedf:$PATH" run)
check_eq "空間不足 rc=5" "5" "$rc"
check "log 講明空間不足" "不到新 binary 的兩倍" "$SWAP_LOG"
check_file "沒有啟動任何東西（daemon 沒被停）" no "$AGM_DIR/started-binary.log"
check_eq "正式 binary 沒動" "old-binary" "$(cat "$AGM_REPO/target/release/agents-managerd")"
teardown

setup 10 10
export STUB_SUPERVISOR=stopped
mkdir -p "$ROOT/fakecp"
cat > "$ROOT/fakecp/cp" <<'STUB'
#!/bin/bash
for a in "$@"; do dest=$a; done
case "$dest" in
  *agents-managerd.restore.*|*/agents-managerd) printf 'half-written' > "$dest"; exit 1 ;;
esac
exec /bin/cp "$@"
STUB
chmod +x "$ROOT/fakecp/cp"
rc=$(PATH="$ROOT/fakecp:$PATH" run)
check_eq "cp 寫一半也能靠 .prev- 回滾（rc=7）" "7" "$rc"
check_eq "舊 binary 完整" "old-binary" "$(cat "$AGM_REPO/target/release/agents-managerd")"
check "有說回滾完成" "rolled back" "$SWAP_LOG"
teardown

setup 10 10
export STUB_SUPERVISOR=stopped
mkdir -p "$ROOT/fakecp" "$ROOT/fakemv"
cat > "$ROOT/fakecp/cp" <<'STUB'
#!/bin/bash
for a in "$@"; do dest=$a; done
case "$dest" in
  *agents-managerd.restore.*) printf 'half-written' > "$dest"; exit 1 ;;
esac
exec /bin/cp "$@"
STUB
cat > "$ROOT/fakemv/mv" <<'STUB'
#!/bin/bash
if [ "$1" = "-f" ] && [[ "$2" == *agents-managerd.prev-* ]]; then exit 1; fi
exec /bin/mv "$@"
STUB
chmod +x "$ROOT/fakecp/cp" "$ROOT/fakemv/mv"
rc=$(PATH="$ROOT/fakecp:$ROOT/fakemv:$PATH" run)
check_eq "舊 binary 還原不了 rc=11（不是 7）" "11" "$rc"
check_no "不能說已回滾" "rolled back" "$SWAP_LOG"
check "log 講明還原失敗" "舊 binary 還原失敗" "$SWAP_LOG"
check_eq "沒再啟動任何 daemon（最後一次還是換版那次）" "new-binary" "$(tail -1 "$AGM_DIR/started-binary.log")"
check_file "留著備份可手動還原" yes "$AGM_REPO/target/release/agents-managerd.bak-$OLD"
teardown
unset STUB_SUPERVISOR

setup 11 10
export STUB_SUPERVISOR=stopped STUB_SESSION_OK_AFTER_FORWARD="" STUB_MUTATE_DB=1
mkdir -p "$ROOT/fakemv"
cat > "$ROOT/fakemv/mv" <<'STUB'
#!/bin/bash
for a in "$@"; do dest=$a; done
if [ "$dest" = "$DAEMON_DB" ]; then exit 1; fi
exec /bin/mv "$@"
STUB
chmod +x "$ROOT/fakemv/mv"
rc=$(PATH="$ROOT/fakemv:$PATH" run)
check_eq "rename 還原失敗不起任何 daemon，以 rc=11 留給人處理（不是已回滾的 7）" "11" "$rc"
check_eq "rename 還原失敗時沒起舊 binary：最後一次還是換版那次" "new-binary" "$(tail -1 "$AGM_DIR/started-binary.log")"
check_eq "rename 失敗保留新版 DB" "mutated-by-new-binary" "$(cat "$DAEMON_DB")"
check_eq "rename 失敗保留 WAL" "stale-wal" "$(cat "$DAEMON_DB-wal" 2>/dev/null)"
check_eq "rename 失敗保留 SHM" "stale-shm" "$(cat "$DAEMON_DB-shm" 2>/dev/null)"
check "rename 失敗有講清楚 DB 還原失敗" "DB 還原失敗" "$SWAP_LOG"
teardown
unset STUB_MUTATE_DB

setup 10 10
mkdir -p "$ROOT/fakedate"
cat > "$ROOT/fakedate/date" <<'STUB'
#!/bin/bash
case "$*" in
  *%Y%m%d-%H%M%S*) echo 20000101-000000 ;;
  *) exec /bin/date "$@" ;;
esac
STUB
chmod +x "$ROOT/fakedate/date"
export SWAP_PREV_NONCE=fixed
rc=$(SWAP_BACKUP_STAMP=first PATH="$ROOT/fakedate:$PATH" run)
check_eq "同一秒第一趟換版 rc=0" "0" "$rc"
check_eq "第一趟換上新 binary" "new-binary" "$(tail -1 "$AGM_DIR/started-binary.log")"
STARTS1=$(wc -l < "$AGM_DIR/started-binary.log")
cp "$AGM_REPO/target/release/agents-managerd.bak-$OLD" "$AGM_REPO/target/release/agents-managerd"
rc=$(SWAP_BACKUP_STAMP=second PATH="$ROOT/fakedate:$PATH" run)
unset SWAP_PREV_NONCE
check_eq "同一秒第二趟照樣換版 rc=0（不撞名）" "0" "$rc"
check_eq "第二趟真的又啟動了 daemon" "yes" "$([ "$(wc -l < "$AGM_DIR/started-binary.log")" -gt "$STARTS1" ] && echo yes || echo no)"
check_eq "第二趟換上新 binary" "new-binary" "$(tail -1 "$AGM_DIR/started-binary.log")"
check "正式 binary 是新的" "new-binary" "$AGM_REPO/target/release/agents-managerd"
teardown

setup 10 10
echo 'no schema history here' > "$CHECKOUT/crates/am-base/src/db.rs"
rc=$(run)
check_eq "讀不到版本就中止（rc=3）" "3" "$rc"
check "講清楚不要用上一輪的數字" "不要用上一輪的數字猜" "$SWAP_LOG"
check_no "沒有動到 binary" "submit" "$AGM_DIR/launchctl.log"
teardown

REAL_DB_RS="$HERE/../../crates/am-base/src/db.rs"
REAL_UV=$(awk '/^pub const SCHEMA_HISTORY/ {on=1; next} on && /^\];/ {exit} on && match($0, /^[[:space:]]*\([0-9]+,/) {v=$0; sub(/^[[:space:]]*\(/, "", v); sub(/,.*/, "", v); last=v} END {print last}' "$REAL_DB_RS")
case "$REAL_UV" in
  ''|*[!0-9]*) echo "FAIL - 從真的 db.rs 算不出版本（拿到 '$REAL_UV'）"; FAIL=$((FAIL + 1)) ;;
  *) echo "ok   - 從真的 db.rs 算出版本 $REAL_UV" ; PASS=$((PASS + 1)) ;;
esac
setup "$REAL_UV" "$REAL_UV"
cp "$REAL_DB_RS" "$CHECKOUT/crates/am-base/src/db.rs"
rc=$(run)
check "真的 db.rs：腳本讀到的版本跟獨立算法一樣" "checkout SCHEMA_VERSION=$REAL_UV, db user_version=$REAL_UV, bumped=no" "$SWAP_LOG"
check_no "真的 db.rs：沒有走到讀不到的 ABORT" "讀不到 checkout 的 SCHEMA_VERSION" "$SWAP_LOG"
teardown

setup 12 12
cat > "$CHECKOUT/crates/am-base/src/db.rs" <<'RS'
pub const SCHEMA_HISTORY: &[(i64, &str)] = &[
    (9, "aaa"),
    // 舊寫法是 vec![(1, "x")]; 之後改成常數
    (10, "bbb"),
    (12, "ccc"),
];
RS
rc=$(run)
check "註解裡的 ]; 不會讓版本變小" "checkout SCHEMA_VERSION=12, db user_version=12, bumped=no" "$SWAP_LOG"
check_no "註解裡的 ]; 不會被當成「DB 比 binary 還新」" "比要換上的 binary 認得的" "$SWAP_LOG"
teardown

setup 10 10
printf 'someone-elses-binary\n' > "$AGM_REPO/target/release/agents-managerd.bak-$OLD"
rc=$(run)
check_eq "回滾點不對就中止（rc=3）" "3" "$rc"
check "講出實際的 sha256" "不是 $OLDHASH" "$SWAP_LOG"
teardown

setup 10 10
export STUB_ACQUIRE_HELD=false
rc=$(run)
check_eq "拿不到窗口以 rc=4 結束" "4" "$rc"
check "記成延後" "DEFER" "$SWAP_LOG"
check_no "沒有重啟" "submit" "$AGM_DIR/launchctl.log"
teardown

setup 10 10
export STUB_SAFE=false
rc=$(run)
check_eq "複查不安全就中止（rc=4）" "4" "$rc"
check "窗口要交還" "lease release restart" "$AGM_DIR/calls.log"
check_no "沒有重啟" "submit" "$AGM_DIR/launchctl.log"
teardown

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

for status in waiting_quota unknown; do
  setup 10 10
  export STUB_SUPERVISOR_BEFORE="idle" STUB_SUPERVISOR="$status"
  rc=$(run)
  check_eq "換版前 idle、換版後 ${status} 要 rollback（rc=7）" "7" "$rc"
  check "拒絕 supervisor 非健康／未知狀態" "supervisor status 不健康或未知" "$SWAP_LOG"
  teardown
done

setup 10 10
export STUB_SUPERVISOR_BEFORE="idle" STUB_SUPERVISOR="waiting_quota" STUB_QUOTA_RESET_AT="2999-01-01T00:00:00Z"
rc=$(run)
check_eq "換版後 waiting_quota 帶未來重置時間不回滾（rc=0）" "0" "$rc"
check "log 講明是額度閘門" "額度閘門擋著（重置 2999-01-01T00:00:00Z）" "$SWAP_LOG"
check_no "沒有 ROLLBACK" "ROLLBACK requested" "$SWAP_LOG"
teardown

setup 10 10
export STUB_SUPERVISOR_BEFORE="idle" STUB_SUPERVISOR="waiting_quota" STUB_QUOTA_RESET_AT="2000-01-01T00:00:00Z"
rc=$(run)
check_eq "換版後 waiting_quota、重置時間已過要 rollback（rc=7）" "7" "$rc"
teardown

setup 10 10
export STUB_SUPERVISOR_BEFORE="unknown" STUB_SUPERVISOR="unknown"
rc=$(run)
check_eq "換版前後都 unknown 仍要 rollback（rc=7）" "7" "$rc"
teardown

setup 10 10
export STUB_SUPERVISOR_BEFORE="" STUB_SUPERVISOR="waiting_quota"
rc=$(run)
check_eq "換版前 status 讀不到、換版後 waiting_quota 要 rollback（rc=7）" "7" "$rc"
teardown

setup 10 10
export STUB_SUPERVISOR_BEFORE="waiting_quota" STUB_SUPERVISOR="waiting_quota"
rc=$(run)
check_eq "換版前後都 waiting_quota 不回滾（rc=0）" "0" "$rc"
check "log 講明是換版前就存在的額度等待" "換版前就是 waiting_quota" "$SWAP_LOG"
check_no "沒有 ROLLBACK" "ROLLBACK requested" "$SWAP_LOG"
check_eq "新 binary 留在線上" "new-binary" "$(tail -1 "$AGM_REPO/target/release/agents-managerd")"
teardown

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

setup 11 10
export STUB_SUPERVISOR_BEFORE="waiting_quota" STUB_SUPERVISOR="waiting_quota"
rc=$(run)
check_eq "升 schema、前後都 waiting_quota：rc=0，不走往前修" "0" "$rc"
check_no "不講往前修" "forward-fix" "$SWAP_LOG"
teardown

setup 10 10
echo "bad rows" > "$ROOT/integrity"
rc=$(run)
check_eq "備份壞掉就中止（rc=5）" "5" "$rc"
check "講出 integrity_check 的結果" "備份讀不回來" "$SWAP_LOG"
check_no "沒有重啟" "submit" "$AGM_DIR/launchctl.log"
teardown

check "啟動器用 setsid 脫離" "os.setsid()" "$HERE/daemon-start.py"
check "啟動器會 fork，父行程先結束（job 才不會被 remove 殺到）" "os.fork()" "$HERE/daemon-start.py"
check "啟動器丟掉 AM_DATA_DIR 等 pane 變數" "AM_DATA_DIR" "$HERE/daemon-start.py"
STARTER_ROOT=$(mktemp -d)
mkdir -p "$STARTER_ROOT/target/release"
printf '%s\n' '#!/bin/sh' 'env > "$STARTER_ENV_DUMP.tmp" && mv "$STARTER_ENV_DUMP.tmp" "$STARTER_ENV_DUMP"' > "$STARTER_ROOT/target/release/agents-managerd"
chmod +x "$STARTER_ROOT/target/release/agents-managerd"
# canary-gap: 這一行就是在測「啟動器自己推出來的 PATH」，帶進 canary 目錄會讓下面那條
STARTER_ENV_DUMP="$STARTER_ROOT/env" AM_DATA_DIR=/should/be/dropped AM_BOT_TOKEN=private-bot-token \
  AM_HOOK_TOKEN=private-hook-token AM_CHILD_OF=parent-agent AM_INSTANCE=instance-7 \
  AM_INSTRUCTIONS_FILE=/private/instructions.md CLAUDE_CONFIG_DIR=/private/claude-home \
  CODEX_HOME=/private/codex-home GROK_HOME=/private/grok-home CLAUDE_CODE_CHILD_SESSION=child-session \
  CLAUDE_CODE_ENABLE_PROMPT_SUGGESTION=1 CLAUDE_CODE_DISABLE_CLAUDE_MDS=1 \
  PATH=/usr/bin:/bin:/usr/sbin:/sbin HOME="$STARTER_ROOT/home" \
  /usr/bin/python3 "$HERE/daemon-start.py" "$STARTER_ROOT" "$STARTER_ROOT/daemon.log"
STARTER_WAIT_SECS=${STARTER_WAIT_SECS:-30}
waited=0
while [ ! -s "$STARTER_ROOT/env" ] && [ "$waited" -lt "$((STARTER_WAIT_SECS * 5))" ]; do
  waited=$((waited + 1))
  sleep 0.2
done
if [ -s "$STARTER_ROOT/env" ]; then
  echo "ok   - 啟動器在 ${STARTER_WAIT_SECS} 秒內寫出 env dump"; PASS=$((PASS + 1))
  check "啟動器帶出的 PATH 含 /opt/homebrew/bin" "^PATH=$STARTER_ROOT/home/.local/bin:/opt/homebrew/bin:/usr/local/bin:/usr/bin:/bin:/usr/sbin:/sbin$" "$STARTER_ROOT/env"
  for key in AM_DATA_DIR AM_BOT_TOKEN AM_HOOK_TOKEN AM_CHILD_OF AM_INSTANCE AM_INSTRUCTIONS_FILE \
      CLAUDE_CONFIG_DIR CODEX_HOME GROK_HOME CLAUDE_CODE_CHILD_SESSION \
      CLAUDE_CODE_ENABLE_PROMPT_SUGGESTION CLAUDE_CODE_DISABLE_CLAUDE_MDS; do
    check_no "啟動器真的丟掉 ${key}" "^${key}=" "$STARTER_ROOT/env"
  done
else
  echo "FAIL - 啟動器沒有在 ${STARTER_WAIT_SECS} 秒內寫出 env dump（$STARTER_ROOT/env）"
  echo "      fork+setsid 的孫行程可能還沒排到 CPU（機器忙），或根本沒起來。daemon.log："
  if [ -s "$STARTER_ROOT/daemon.log" ]; then sed 's/^/      /' "$STARTER_ROOT/daemon.log"; else echo "      （空的或不存在）"; fi
  FAIL=$((FAIL + 1))
fi
rm -rf "$STARTER_ROOT"

setup 10 10
export HERDR_PANE_ID="w9:pSTALE"
rc=$(run)
check_eq "pane 讀不到就中止（rc=3）" "3" "$rc"
check "log 講出 pane_not_found" "pane_not_found" "$SWAP_LOG"
check_no "沒有動 binary" "submit" "$AGM_DIR/launchctl.log"
check_eq "線上 binary 沒被換掉" "old-binary" "$(cat "$AGM_REPO/target/release/agents-managerd")"
teardown

setup 10 10
unset HERDR_PANE_ID
rc=$(run)
check_eq "問得到 current 就能往下走（rc=0）" "0" "$rc"
check "有去問 pane current" "pane current" "$AGM_DIR/herdr.log"
check "讀的是 current 回的那顆" "pane read w1:pA" "$AGM_DIR/herdr.log"
teardown

setup 10 10
rc=$(bash "$SCRIPT" --sha "$SHA" --old "$OLD" --old-hash "$OLDHASH" --owner bot-me --checkout "$CHECKOUT" --pane w9:pSTALE >/dev/null 2>&1; echo $?)
check_eq "帶 --pane 會被拒（rc=2）" "2" "$rc"
teardown
setup 10 10
rc=$(run)
check "讀的是環境變數那顆，不是舊的寫死值" "pane read w1:pA" "$AGM_DIR/herdr.log"
check_no "不會去讀寫死的舊 id" "w169:pN" "$AGM_DIR/herdr.log"
teardown

setup 10 10
export STUB_PROBE='409 {"error":"conflict","reason":"composer_unreadable"}'
rc=$(run)
check_eq "送不出去就中止（rc=3）" "3" "$rc"
check "log 帶上回應" "composer_unreadable" "$SWAP_LOG"
check_no "沒有動 binary" "submit" "$AGM_DIR/launchctl.log"
teardown

setup 10 10
export STUB_PROBE_FIRST_INFLIGHT=1
rc=$(run)
check_eq "in flight 會重送並繼續（rc=0）" "0" "$rc"
check "重送過（送了兩次）" "bot-probe" "$AGM_DIR/probe.log"
check "自測最後是通的" "3b self probe ok" "$SWAP_LOG"
teardown

check "agm 維運請求用 daemon-swap service principal" "AM_SERVICE_ID=daemon-swap" "$SCRIPT"
check "自測送到固定 service route" "/api/services/daemon-swap/probe/" "$SCRIPT"
check_no "自測不再自行宣告 relay_from" "relay_from" "$SCRIPT"
check "舊 daemon bootstrap 自測使用 User session token" 'with opener.open(base + "/api/session")' "$SCRIPT"

setup 10 10
rm -rf "$AM_DATA/service-tokens"
rc=$(run)
check_eq "service 能力無法判定時 fail closed，而且是設定錯誤不是「有人在忙」的 4（rc=3）" "3" "$rc"
check_no "能力不明時不取得維運租約" "lease acquire restart" "$AGM_DIR/calls.log"
check_no "能力不明時不替換 binary" "submit" "$AGM_DIR/launchctl.log"
teardown

setup 10 10
rc=$(run)
check_eq "沒有核准單也能換版（rc=0）" "0" "$rc"
check "窗口帶的是 owner 與 commit" "lease acquire restart --owner bot-me --commit $SHA" "$AGM_DIR/calls.log"
check "3a 複查綁 restart-window 開窗口的那張核准" "lease safety --approval ap-auto-1 --owner bot-me" "$AGM_DIR/calls.log"
teardown

setup 10 10
rc=$(run ap-1)
check_eq "新 daemon 接受舊 task 傳入的 --approval（rc=0）" "0" "$rc"
check_no "restart-window 模式忽略舊 approval" "lease acquire restart.*--approval" "$AGM_DIR/calls.log"
check_no "複查也不拿舊 task 的 approval" "lease safety.*--approval ap-1" "$AGM_DIR/calls.log"
teardown

setup 10 10
export STUB_RECHECK_WORKING=busy-bot STUB_ESCALATED_APPROVAL=ap-auto-1
rc=$(run)
check_eq "放寬生效時有 bot working 仍能換版（rc=0）" "0" "$rc"
check "複查 log 看得出是放寬後的判斷" "3a recheck: True \['busy-bot'\] \[\] escalated=True" "$SWAP_LOG"
check "放寬後真的換了 binary" "submit" "$AGM_DIR/launchctl.log"
teardown

setup 10 10
export STUB_RECHECK_WORKING=busy-bot STUB_RECHECK_DELIVERING=typing-bot STUB_ESCALATED_APPROVAL=ap-auto-1
rc=$(run)
check_eq "放寬了但有人在送達臨界區：照擋（rc=4）" "4" "$rc"
check "送達中擋下時 log 寫明複查不安全" "ABORT: 換 binary 前複查不安全" "$SWAP_LOG"
check "送達中擋下時交還窗口" "lease release restart" "$AGM_DIR/calls.log"
check_no "送達中擋下時不換 binary" "submit" "$AGM_DIR/launchctl.log"
teardown

setup 10 10
export STUB_RECHECK_WORKING=busy-bot
rc=$(run)
check_eq "沒放寬時 working 照樣擋（rc=4）" "4" "$rc"
check_no "沒放寬時不換 binary" "submit" "$AGM_DIR/launchctl.log"
teardown

setup 10 10
export STUB_RECHECK_WORKING=busy-bot STUB_ESCALATED_APPROVAL=ap-other
rc=$(run)
check_eq "放寬的是別張核准：不替這張開門（rc=4）" "4" "$rc"
teardown

setup 10 10
export STUB_RECHECK_WORKING=busy-bot STUB_WINDOW_ESCALATED_BY=ap-auto-1
rc=$(run)
check_eq "舊 daemon：acquire 已放寬、複查說沒放寬，有 working 仍能換版（rc=0）" "0" "$rc"
check "log 寫明沿用 acquire 的放寬" "3a recheck: True \['busy-bot'\] \[\] escalated=False (沿用 acquire 的放寬)" "$SWAP_LOG"
check "沿用放寬後真的換了 binary" "submit" "$AGM_DIR/launchctl.log"
teardown

setup 10 10
export STUB_RECHECK_WORKING=busy-bot STUB_WINDOW_ESCALATED_BY=ap-auto-1 STUB_RECHECK_DELIVERING=typing-bot
rc=$(run)
check_eq "沿用放寬時送達臨界區照擋（rc=4）" "4" "$rc"
check_no "送達中不換 binary" "submit" "$AGM_DIR/launchctl.log"
teardown

setup 10 10
export STUB_RECHECK_WORKING=busy-bot STUB_WINDOW_ESCALATED_BY=ap-auto-1 STUB_RECHECK_OTHER_LEASE=someone-else
rc=$(run)
check_eq "沿用放寬時別人的租約照擋（rc=4）" "4" "$rc"
teardown

setup 10 10
export STUB_RECHECK_WORKING=busy-bot STUB_WINDOW_ESCALATED_BY=ap-auto-1 STUB_RECHECK_UNREADABLE=ghost
rc=$(run)
check_eq "沿用放寬時讀不到狀態照擋（rc=4）" "4" "$rc"
teardown

setup 10 10
export STUB_RECHECK_WORKING=busy-bot STUB_WINDOW_ESCALATED_BY=ap-other
rc=$(run)
check_eq "acquire 的放寬計時綁的不是開窗口那張：不沿用（rc=4）" "4" "$rc"
teardown

setup 10 10
export STUB_RECHECK_WORKING=busy-bot STUB_WINDOW_ESCALATED_BY=ap-auto-1 STUB_RECHECK_NO_FIELDS=1
rc=$(run)
check_eq "複查缺 unreadable／held_leases 欄位：不沿用（fail closed，rc=4）" "4" "$rc"
teardown

setup 10 10
export STUB_WINDOW_APPROVAL=""
rc=$(run)
check_eq "舊 daemon 窗口回應沒有 approval 也照常換版（rc=0）" "0" "$rc"
check_no "沒有 approval 時複查不帶 --approval" "lease safety.*--approval" "$AGM_DIR/calls.log"
teardown

setup 10 10
export STUB_CAP=service_old
rc=$(run ap-1)
check_eq "舊 daemon 用 approval bootstrap 完成 swap（rc=0）" "0" "$rc"
check "舊路徑以 approval 取得 restart lease" "lease acquire restart --owner bot-me --approval ap-1 --commit $SHA --ttl 900 --exclude-bot bot-me" "$AGM_DIR/calls.log"
check "舊路徑 safety 綁定同一 approval" "lease safety --approval ap-1 --owner bot-me --exclude-bot bot-me" "$AGM_DIR/calls.log"
check "舊路徑清掉 service/bot 身份，使用本機 User token" "identity service= bot=" "$AGM_DIR/identity.log"
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
check_eq "daemon 宣告 service principal 卻缺 token 時不降級，回設定錯誤（rc=3）" "3" "$rc"
check_no "缺 service token 時不取得 User 租約" "lease acquire restart" "$AGM_DIR/calls.log"
teardown

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

setup 10 10
export STUB_PROBE='409 {"error":"conflict","reason":"bot has no active run"}'
rc=$(run)
check_eq "自測對象沒在跑就略過（rc=0）" "0" "$rc"
check "log 講清楚略過的原因" "3b 自測略過" "$SWAP_LOG"
check "照樣換了 binary" "new-binary" "$AGM_DIR/started-binary.log"
teardown

setup 10 10
export STUB_PROBE='404 {"error":"not_found","reason":"no active run"}'
rc=$(run)
check_eq "略過計數：第一趟 rc=0" "0" "$rc"
check_eq "略過計數：第一趟是 1" "1" "$(cat "$AGM_DIR/daemon-swap.probe-skips" 2>/dev/null)"
SKIPS=$(cat "$AGM_DIR/daemon-swap.probe-skips")
teardown
setup 10 10
echo "$SKIPS" > "$AGM_DIR/daemon-swap.probe-skips"
export STUB_PROBE='404 {"error":"not_found","reason":"no active run"}'
rc=$(run)
check_eq "略過計數：第二趟 rc=0" "0" "$rc"
check_eq "略過計數：第二趟是 2" "2" "$(cat "$AGM_DIR/daemon-swap.probe-skips" 2>/dev/null)"
SKIPS=$(cat "$AGM_DIR/daemon-swap.probe-skips")
teardown
setup 10 10
echo "$SKIPS" > "$AGM_DIR/daemon-swap.probe-skips"
export STUB_PROBE='200 {"delivery":"ok"}'
rc=$(run)
check_eq "略過計數：自測成功 rc=0" "0" "$rc"
check_file "自測 200 成功就清掉計數檔" no "$AGM_DIR/daemon-swap.probe-skips"
teardown

setup 10 10
unset SWAP_PROBE_BOT
printf '{"swap_probe_bot_id": "bot-from-runtime"}\n' > "$AGM_DIR/runtime.json"
rc=$(run)
check_eq "runtime.json 指定自測對象：rc=0" "0" "$rc"
check "自測送給 runtime.json 的 bot" "bot-from-runtime" "$AGM_DIR/probe.log"
teardown
setup 10 10
unset SWAP_PROBE_BOT
printf '{"swap_probe_bot_id": "", "manager_bot_id": "bot-agm-self"}\n' > "$AGM_DIR/runtime.json"
rc=$(run)
check_eq "runtime.json 的值是空：rc=0" "0" "$rc"
check "空值退回 manager_bot_id（AGM 本人）" "bot-agm-self" "$AGM_DIR/probe.log"
check_no "不再用寫死的 browser-gc id" "01M248GA4H1TAHJCZRKVR73S3C" "$AGM_DIR/probe.log"
teardown
setup 10 10
unset SWAP_PROBE_BOT
printf '{"manager_bot_id": "bot-agm-self"}\n' > "$AGM_DIR/runtime.json"
rc=$(run)
check_eq "沒有 swap_probe_bot_id：rc=0" "0" "$rc"
check "沒設就用 manager_bot_id" "bot-agm-self" "$AGM_DIR/probe.log"
teardown
setup 10 10
unset SWAP_PROBE_BOT
printf '{"responder_bot_id": "bot-responder", "manager_bot_id": "bot-agm-self"}\n' > "$AGM_DIR/runtime.json"
rc=$(run)
check_eq "responder 與 manager 並存：rc=0" "0" "$rc"
check "自測送給協調者" "bot-responder" "$AGM_DIR/probe.log"
check_no "不叫醒巡檢" "bot-agm-self" "$AGM_DIR/probe.log"
teardown
setup 10 10
unset SWAP_PROBE_BOT
printf '{"swap_probe_bot_id": "bot-from-runtime", "responder_bot_id": "bot-responder", "manager_bot_id": "bot-agm-self"}\n' > "$AGM_DIR/runtime.json"
rc=$(run)
check "swap_probe_bot_id 仍最優先" "bot-from-runtime" "$AGM_DIR/probe.log"
check_no "有指定就不碰協調者" "bot-responder" "$AGM_DIR/probe.log"
teardown
setup 10 10
unset SWAP_PROBE_BOT
printf '{"responder_bot_id": "bot-responder", "manager_bot_id": "bot-agm-self"}\n' > "$AGM_DIR/runtime.json"
export STUB_PROBE_NO_RUN_BOT=bot-responder
rc=$(run)
unset STUB_PROBE_NO_RUN_BOT
check_eq "協調者沒在跑：退回巡檢、照樣換版（rc=0）" "0" "$rc"
check "先試協調者" "bot-responder" "$AGM_DIR/probe.log"
check "再退回 manager_bot_id" "bot-agm-self" "$AGM_DIR/probe.log"
check "log 講明改送誰" "改送 bot-agm-self" "$SWAP_LOG"
check_file "退回後自測成功，不算略過" no "$AGM_DIR/daemon-swap.probe-skips"
teardown
setup 10 10
unset SWAP_PROBE_BOT
printf '{"responder_bot_id": "bot-responder", "manager_bot_id": "bot-agm-self"}\n' > "$AGM_DIR/runtime.json"
export STUB_PROBE_DRAFT_BOTS="bot-responder"
rc=$(run)
unset STUB_PROBE_DRAFT_BOTS
check_eq "主對象輸入框有字：改送 fallback、照樣換版（rc=0）" "0" "$rc"
check "先試協調者" "bot-responder" "$AGM_DIR/probe.log"
check "再改送 manager_bot_id" "bot-agm-self" "$AGM_DIR/probe.log"
check "log 講明是輸入框有字" "輸入框有未送出的字，改送 fallback bot-agm-self" "$SWAP_LOG"
check_file "fallback 成功不算略過" no "$AGM_DIR/daemon-swap.probe-skips"
teardown
setup 10 10
unset SWAP_PROBE_BOT
printf '{"responder_bot_id": "bot-responder", "manager_bot_id": "bot-agm-self"}\n' > "$AGM_DIR/runtime.json"
export STUB_PROBE_DRAFT_BOTS="bot-responder bot-agm-self"
rc=$(run)
unset STUB_PROBE_DRAFT_BOTS
check_eq "兩顆輸入框都有字：略過、不是 ABORT（rc=0）" "0" "$rc"
check "log 寫輸入框有字，不是 no active run" "輸入框有未送出的字（draft）" "$SWAP_LOG"
check_no "不誤報 no active run" "沒有在跑（no active run）" "$SWAP_LOG"
check_no "不 ABORT" "ABORT" "$SWAP_LOG"
check_file "略過要計數" yes "$AGM_DIR/daemon-swap.probe-skips"
check "原因檔記輸入框有字" "輸入框有字" "$AGM_DIR/daemon-swap.probe-skips.why"
teardown
setup 10 10
unset SWAP_PROBE_BOT
printf '{"swap_probe_bot_id": "bot-solo"}\n' > "$AGM_DIR/runtime.json"
export STUB_PROBE_DRAFT_BOTS="bot-solo"
rc=$(run)
unset STUB_PROBE_DRAFT_BOTS
check_eq "沒有 fallback 且有字：略過（rc=0）" "0" "$rc"
check_file "有計數" yes "$AGM_DIR/daemon-swap.probe-skips"
teardown
setup 10 10
unset SWAP_PROBE_BOT
printf '{"responder_bot_id": "bot-responder", "manager_bot_id": "bot-agm-self"}\n' > "$AGM_DIR/runtime.json"
export STUB_PROBE_BLOCKED_BOTS="bot-responder"
rc=$(run)
unset STUB_PROBE_BLOCKED_BOTS
check_eq "協調者 blocked：退回巡檢、照樣換版（rc=0）" "0" "$rc"
check "再退回 manager_bot_id" "bot-agm-self" "$AGM_DIR/probe.log"
check "log 講明暫時不能收 prompt、改送誰" "暫時不能收 prompt" "$SWAP_LOG"
check "log 改送 bot-agm-self" "改送 fallback bot-agm-self" "$SWAP_LOG"
check_no "不 ABORT" "ABORT" "$SWAP_LOG"
check_file "fallback 成功不算略過" no "$AGM_DIR/daemon-swap.probe-skips"
teardown
setup 10 10
unset SWAP_PROBE_BOT
printf '{"responder_bot_id": "bot-responder", "manager_bot_id": "bot-agm-self"}\n' > "$AGM_DIR/runtime.json"
export STUB_PROBE_BLOCKED_BOTS="bot-responder bot-agm-self"
rc=$(run)
unset STUB_PROBE_BLOCKED_BOTS
check_eq "兩顆都 blocked：略過、不是 ABORT（rc=0）" "0" "$rc"
check "log 寫暫時不能收 prompt" "3b 自測略過：bot-agm-self 暫時不能收 prompt" "$SWAP_LOG"
check_no "不 ABORT" "ABORT" "$SWAP_LOG"
check_file "略過要計數" yes "$AGM_DIR/daemon-swap.probe-skips"
check "原因檔記目標不能收 prompt" "目標不能收 prompt" "$AGM_DIR/daemon-swap.probe-skips.why"
teardown
setup 10 10
unset SWAP_PROBE_BOT
printf '{"swap_probe_bot_id": "bot-solo"}\n' > "$AGM_DIR/runtime.json"
export STUB_PROBE_BLOCKED_BOTS="bot-solo"
rc=$(run)
unset STUB_PROBE_BLOCKED_BOTS
check_eq "沒有 fallback 且 blocked：略過（rc=0）" "0" "$rc"
check_file "有計數" yes "$AGM_DIR/daemon-swap.probe-skips"
teardown
setup 10 10
unset SWAP_PROBE_BOT
printf '{"responder_bot_id": "bot-responder", "manager_bot_id": "bot-agm-self"}\n' > "$AGM_DIR/runtime.json"
export STUB_PROBE_NOTRUNNING_BOTS="bot-responder"
rc=$(run)
unset STUB_PROBE_NOTRUNNING_BOTS
check_eq "協調者 run 還沒進 running：退回巡檢（rc=0）" "0" "$rc"
check "改送 bot-agm-self" "改送 fallback bot-agm-self" "$SWAP_LOG"
teardown
setup 10 10
unset SWAP_PROBE_BOT
printf '{"responder_bot_id": "bot-responder", "manager_bot_id": "bot-agm-self"}\n' > "$AGM_DIR/runtime.json"
export STUB_PROBE_OTHER409_BOTS="bot-responder"
rc=$(run)
unset STUB_PROBE_OTHER409_BOTS
check_eq "非 draft 的 409 仍 ABORT（rc=3）" "3" "$rc"
check "log 有 ABORT" "ABORT: 自測 prompt 回" "$SWAP_LOG"
teardown
setup 10 10
unset SWAP_PROBE_BOT
rm -f "$AGM_DIR/runtime.json" "$AGM_DIR/probe.log"
rc=$(run)
check_eq "完全沒有自測對象：略過、照樣換版（rc=0）" "0" "$rc"
check "log 講明是略過" "3b 自測略過" "$SWAP_LOG"
check_file "沒有對象就不送自測" no "$AGM_DIR/probe.log"
check_file "略過要計數" yes "$AGM_DIR/daemon-swap.probe-skips"
teardown
setup 10 10
printf '{"swap_probe_bot_id": "bot-from-runtime"}\n' > "$AGM_DIR/runtime.json"
rc=$(run)
check_eq "環境變數與 runtime.json 並存：rc=0" "0" "$rc"
check "環境變數 SWAP_PROBE_BOT 優先" "bot-probe" "$AGM_DIR/probe.log"
check_no "不用 runtime.json 的值" "bot-from-runtime" "$AGM_DIR/probe.log"
teardown

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

setup 10 10
export STUB_CAP=service_old
rc=$(run)
check_eq "缺 approval 的舊 daemon 安全中止（rc=9）" "9" "$rc"
check_no "沒有動 binary" "submit" "$AGM_DIR/launchctl.log"
check_no "沒有要窗口" "lease acquire restart" "$AGM_DIR/calls.log"
teardown

if LC_ALL=C grep -nE '\$[A-Za-z_][A-Za-z0-9_]*[^ -~]' "$SCRIPT" > /tmp/daemon-swap-unbraced.$$ 2>/dev/null; then
  echo "FAIL - 變數後面接非 ASCII 字元要用 \${VAR}"; sed 's/^/      /' /tmp/daemon-swap-unbraced.$$; FAIL=$((FAIL + 1))
else
  echo "ok   - 變數後面接非 ASCII 字元都有大括號"; PASS=$((PASS + 1))
fi
rm -f /tmp/daemon-swap-unbraced.$$

setup 10 10
export STUB_ACQUIRE_FAIL_TIMES=1
rc=$(run)
check_eq "重試後成功（rc=0）" "0" "$rc"
check "被拒那次有記下 reason" "no window yet (try 1/3) reason=not_idle" "$SWAP_LOG"
check "reason 帶上是誰在跑" "working=busy-bot" "$SWAP_LOG"
check "第二次就拿到窗口" "restart lease fence=9" "$SWAP_LOG"
teardown

setup 10 10
export STUB_ACQUIRE_FAIL_TIMES=99
rc=$(run)
check_eq "一直拿不到就 DEFER（rc=4）" "4" "$rc"
check "DEFER 記下最後的 reason" "DEFER: 拿不到 restart 窗口（試了 3 次，最後 reason=not_idle" "$SWAP_LOG"
check "試滿設定的次數" "try 3/3" "$SWAP_LOG"
check_no "沒有動 binary" "submit" "$AGM_DIR/launchctl.log"
teardown

check "SWAP_WINDOW_TRIES 預設 12" 'SWAP_WINDOW_TRIES:-12' "$SCRIPT"
check "間隔預設 15 秒" 'SWAP_WINDOW_WAIT_SECS:-15' "$SCRIPT"

setup 10 10
export STUB_PROBE_INFLIGHT_TIMES=2
rc=$(run)
check_eq "等自測回合收尾後成功（rc=0）" "0" "$rc"
check "有等自測回合" "等自測回合收尾（bot-probe 還在飛）" "$SWAP_LOG"
check "第 3 次查就收尾了" "3b self probe turn settled (check 3/5)" "$SWAP_LOG"
check_no "沒有吃 409" "no window yet" "$SWAP_LOG"
check_eq "窗口只要了一次" "1" "$(grep -c "lease acquire restart" "$AGM_DIR/calls.log")"
teardown

setup 10 10
export STUB_ACQUIRE_FAIL_TIMES=1 STUB_ACQUIRE_EMPTY_WORKING=1
rc=$(run)
check_eq "重試後成功（rc=0）" "0" "$rc"
check "空名單註明可能是自測回合" "reason=not_idle working=(空，可能是自測回合 in_flight=bot-probe)" "$SWAP_LOG"
teardown

setup 10 10
export STUB_PROBE_INFLIGHT_TIMES=6
rc=$(run)
check_eq "上限用完仍繼續，窗口重試接手（rc=0）" "0" "$rc"
check "講清楚等滿了" "自測回合等了 5 次還在飛" "$SWAP_LOG"
check "那次 409 有註明可能是自測回合" "no window yet (try 1/3) reason=not_idle working=(空，可能是自測回合" "$SWAP_LOG"
check "預設等 12 次" 'SWAP_PROBE_SETTLE_TRIES:-12' "$SCRIPT"
teardown

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

setup 10 10
export STUB_RESTART_HELD=true STUB_RELEASE_FAIL=1
rc=$(run)
check_eq "換版成功但窗口沒還：rc=8" "8" "$rc"
check "log 說交還失敗" "交還 restart 窗口失敗 rc=" "$SWAP_LOG"
check "log 說窗口還被握著" "窗口仍被握著" "$SWAP_LOG"
check_no "不准謊報已交還" "restart 窗口已交還" "$SWAP_LOG"
check "換版本身有做完" "new-binary" "$AGM_DIR/started-binary.log"
teardown

setup 10 10
export STUB_SAFE=false STUB_RELEASE_FAIL=1
rc=$(run)
check_eq "複查不安全仍以 rc=4 結束" "4" "$rc"
check "log 說交還失敗" "交還 restart 窗口失敗 rc=" "$SWAP_LOG"
check_no "不准謊報已交還" "restart 窗口已交還" "$SWAP_LOG"
teardown

setup 10 10
export STUB_NAMES_BEFORE='["a","b","kid"]' STUB_NAMES_AFTER='["a","b"]'
seed_audit "INSERT INTO bots (id,name,project_id,deleted_at) VALUES ('id-kid','kid','p1','2099-01-01T00:00:01.000Z');
  INSERT INTO intents (id,kind,subject_id,payload_json,status,created_at) VALUES ('it1','delete_bot','id-kid','{\"bots\":[],\"requested_by\":\"agm\"}','done','2099-01-01T00:00:00.000Z');"
rc=$(run)
check_eq "窗口內刻意刪掉的不回滾（rc=0）" "0" "$rc"
check "log 列出窗口內刪掉的" "missing=\[\] deleted_in_window=\[kid \]" "$SWAP_LOG"
check_no "沒有回滾" "ROLLBACK" "$SWAP_LOG"
check_file "不是唯讀開 DB 的查詢一筆都沒有" no "$AGM_DIR/sqlite-rw.log"
teardown

setup 10 10
export STUB_NAMES_BEFORE='["a","mom","kid"]' STUB_NAMES_AFTER='["a"]'
seed_audit "INSERT INTO bots (id,name,project_id,deleted_at) VALUES ('id-mom','mom','p1','2099-01-01T00:00:01.000Z'), ('id-kid','kid','p1','2099-01-01T00:00:02.000Z');
  INSERT INTO intents (id,kind,subject_id,payload_json,status,created_at) VALUES ('it1','delete_bot','id-mom','{\"bots\":[{\"id\":\"id-kid\",\"managed_by\":\"child\"}]}','done','2099-01-01T00:00:00.000Z');"
rc=$(run)
check_eq "連帶刪掉的 child 也不回滾（rc=0）" "0" "$rc"
check "兩顆都算刻意刪除" "deleted_in_window=\[kid mom \]" "$SWAP_LOG"
teardown

setup 10 10
export STUB_NAMES_BEFORE='["a","b","kid"]' STUB_NAMES_AFTER='["a","b"]'
seed_audit "INSERT INTO bots (id,name,project_id,deleted_at) VALUES ('id-kid','kid','p1','2099-01-01T00:00:01.000Z');"
rc=$(run)
check_eq "沒有刪除紀錄就回滾（rc=7）" "7" "$rc"
check "log 講是沒有刪除紀錄" "有 bot 不見了（沒有刪除紀錄）：kid" "$SWAP_LOG"
check_eq "binary 換回舊的" "old-binary" "$(cat "$AGM_REPO/target/release/agents-managerd")"
teardown

setup 10 10
export STUB_NAMES_BEFORE='["a","kid"]' STUB_NAMES_AFTER='["a"]'
seed_audit "INSERT INTO bots (id,name,project_id,deleted_at) VALUES ('id-kid','kid','p1','2099-01-01T00:00:01.000Z');
  INSERT INTO intents (id,kind,subject_id,payload_json,status,created_at) VALUES ('it0','delete_bot','id-kid','{}','done','2000-01-01T00:00:00.000Z');"
rc=$(run)
check_eq "窗口外的刪除紀錄不算（rc=7）" "7" "$rc"
check "log 講是沒有刪除紀錄" "有 bot 不見了（沒有刪除紀錄）：kid" "$SWAP_LOG"
teardown

setup 10 10
export STUB_NAMES_BEFORE='["a","kid"]' STUB_NAMES_AFTER='["a"]'
seed_audit "INSERT INTO bots (id,name,project_id,deleted_at) VALUES ('id-kid','kid','p1','2099-01-01T00:00:01.000Z');
  INSERT INTO intents (id,kind,subject_id,payload_json,status,created_at) VALUES ('it-in-flight','delete_bot','id-kid','{}','running','2000-01-01T00:00:00.000Z');"
rc=$(run)
check_eq "換版前開始、窗口內定案的刪除不回滾（rc=0）" "0" "$rc"
check "窗口內定案由 open delete intent 證明" "deleted_in_window=\[kid \]" "$SWAP_LOG"
check_no "跨窗口 delete intent 不誤回滾" "ROLLBACK" "$SWAP_LOG"
teardown

setup 10 10
export STUB_NAMES_BEFORE='["a","kid"]' STUB_NAMES_AFTER='["a"]'
seed_audit "INSERT INTO bots (id,name,project_id,deleted_at) VALUES ('id-kid','kid','p1','2099-01-01T00:00:01.000Z');
  INSERT INTO intents (id,kind,subject_id,payload_json,status,created_at) VALUES ('it-finished','delete_bot','id-kid','{}','done','2000-01-01T00:00:00.000Z');
  UPDATE intents SET updated_at='2099-01-01T00:00:01.000Z' WHERE id='it-finished';"
rc=$(run)
check_eq "換版前開始、窗口內完成的刪除不回滾（rc=0）" "0" "$rc"
check "窗口內完成時間由 updated_at 證明" "deleted_in_window=\[kid \]" "$SWAP_LOG"
check_no "完成時間在窗口內不誤回滾" "ROLLBACK" "$SWAP_LOG"
teardown

setup 10 10
export STUB_NAMES_BEFORE='["a","kid","pro"]' STUB_NAMES_AFTER='["a"]'
seed_audit "INSERT INTO bots (id,name,project_id,deleted_at) VALUES ('id-kid','kid','p1','2099-01-01T00:00:01.000Z'), ('id-pro','pro','p1','2099-01-01T00:00:02.000Z');
  INSERT INTO intents (id,kind,subject_id,payload_json,status,created_at) VALUES ('it1','retire_child','id-kid','{\"why\":\"reconcile_run_already_ended\",\"cause\":\"pane_closed\"}','done','2099-01-01T00:00:01.000Z'),
    ('it2','retire_child','id-pro','{\"why\":\"promoted\",\"cause\":\"promoted\"}','done','2099-01-01T00:00:02.000Z');"
rc=$(run)
check_eq "pane 被關掉而退役的 child 不回滾（rc=0）" "0" "$rc"
check "兩顆都算刻意收掉" "missing=\[\] deleted_in_window=\[kid pro \]" "$SWAP_LOG"
check_no "沒有回滾" "ROLLBACK" "$SWAP_LOG"
check_file "不是唯讀開 DB 的查詢一筆都沒有" no "$AGM_DIR/sqlite-rw.log"
teardown

for c in agent_missing unconfirmed herdr_restarted; do
  setup 10 10
  export STUB_NAMES_BEFORE='["a","kid"]' STUB_NAMES_AFTER='["a"]'
  seed_audit "INSERT INTO bots (id,name,project_id,deleted_at) VALUES ('id-kid','kid','p1','2099-01-01T00:00:01.000Z');
    INSERT INTO intents (id,kind,subject_id,payload_json,status,created_at) VALUES ('it1','retire_child','id-kid','{\"why\":\"reconcile_agent_gone\",\"cause\":\"$c\"}','done','2099-01-01T00:00:01.000Z');"
  rc=$(run)
  check_eq "cause=${c} 的退役照樣回滾（rc=7）" "7" "$rc"
  check "cause=${c}：log 講是沒有刪除紀錄" "有 bot 不見了（沒有刪除紀錄）：kid" "$SWAP_LOG"
  teardown
done

setup 10 10
export STUB_NAMES_BEFORE='["a","kid"]' STUB_NAMES_AFTER='["a"]'
seed_audit "INSERT INTO bots (id,name,project_id,deleted_at) VALUES ('id-kid','kid','p1','2099-01-01T00:00:01.000Z');
  INSERT INTO intents (id,kind,subject_id,payload_json,status,created_at) VALUES ('it0','retire_child','id-kid','{\"cause\":\"pane_closed\"}','done','2000-01-01T00:00:00.000Z'),
    ('it1','retire_child','id-grandkid','{\"cause\":\"pane_closed\",\"parent_bot_id\":\"id-kid\"}','done','2099-01-01T00:00:01.000Z');"
rc=$(run)
check_eq "窗口外、或別顆的退役紀錄都不算（rc=7）" "7" "$rc"
check "log 講是沒有刪除紀錄" "有 bot 不見了（沒有刪除紀錄）：kid" "$SWAP_LOG"
teardown

setup 10 10
export STUB_NAMES_BEFORE='["mom","kid"]' STUB_NAMES_AFTER='["mom","new-kid"]'
seed_audit "INSERT INTO bots (id,name,project_id,deleted_at) VALUES ('id-mom','mom','p1',NULL), ('id-kid','kid','p1','2099-01-01T00:00:01.000Z');
  UPDATE bots SET managed_by='child', parent_bot_id='id-mom' WHERE id='id-kid';
  INSERT INTO bots (id,name,project_id,deleted_at,managed_by,parent_bot_id,created_at)
    VALUES ('id-new-kid','new-kid','p1',NULL,'child','id-mom','2099-01-01T00:00:02.000Z');
  INSERT INTO runs VALUES ('id-mom','running');
  INSERT INTO intents (id,kind,subject_id,payload_json,status,created_at) VALUES ('it1','retire_child','id-kid','{\"why\":\"reconcile_agent_gone\",\"cause\":\"unconfirmed\",\"parent_bot_id\":\"id-mom\"}','done','2099-01-01T00:00:01.000Z');"
rc=$(run)
check_eq "父 bot 活著且有窗口內 successor 的 unconfirmed child 不回滾（rc=0）" "0" "$rc"
check_no "有 successor 時不回滾" "ROLLBACK" "$SWAP_LOG"
teardown

setup 10 10
export STUB_NAMES_BEFORE='["a","mom","kid"]' STUB_NAMES_AFTER='["a","mom"]'
export STUB_DELAY_RETIRE_INTENT_FOR=id-kid
seed_audit "INSERT INTO bots (id,name,project_id,deleted_at,managed_by,parent_bot_id,created_at)
    VALUES ('id-mom','mom','p1',NULL,'user',NULL,'2000-01-01T00:00:00.000Z'),
           ('id-kid','kid','p1','2099-01-01T00:00:02.000Z','child','id-mom','2000-01-01T00:00:00.000Z');
  INSERT INTO runs VALUES ('id-mom','running');"
rc=$(run)
check_eq "reconcile intent 晚到但父仍活著：不回滾（rc=0）" "0" "$rc"
check "延後出現的 intent 被認成 expected retire" 'missing=\[\] deleted_in_window=\[kid \]' "$SWAP_LOG"
check_no "intent 競態不誤回滾" "ROLLBACK" "$SWAP_LOG"
teardown

setup 10 10
export STUB_NAMES_BEFORE='["a","mom","kid"]' STUB_NAMES_AFTER='["a"]'
seed_audit "INSERT INTO bots (id,name,project_id,deleted_at,managed_by,parent_bot_id,created_at)
    VALUES ('id-mom','mom','p1',NULL,'user',NULL,'2000-01-01T00:00:00.000Z'),
           ('id-kid','kid','p1','2099-01-01T00:00:02.000Z','child','id-mom','2000-01-01T00:00:00.000Z');
  INSERT INTO runs VALUES ('id-mom','running');
  INSERT INTO intents (id,kind,subject_id,payload_json,status,created_at) VALUES ('it1','retire_child','id-kid','{\"why\":\"reconcile_agent_gone\",\"cause\":\"unconfirmed\",\"mode\":\"implicit\",\"pane\":\"gone\",\"parent_bot_id\":\"id-mom\"}','done','2099-01-01T00:00:01.000Z');"
rc=$(run)
check_eq "母 bot 消失仍回滾（rc=7）" "7" "$rc"
check "母 bot 仍列在 missing" 'missing=\[kid mom \]' "$SWAP_LOG"
teardown

setup 10 10
export STUB_NAMES_BEFORE='["a","mom","kid"]' STUB_NAMES_AFTER='["a","mom"]'
seed_audit "INSERT INTO bots (id,name,project_id,deleted_at,managed_by,parent_bot_id,created_at)
    VALUES ('id-mom','mom','p1',NULL,'user',NULL,'2000-01-01T00:00:00.000Z'),
           ('id-kid','kid','p1',NULL,'child','id-mom','2000-01-01T00:00:00.000Z');
  INSERT INTO runs VALUES ('id-mom','running');"
rc=$(run)
check_eq "intent 永遠不來：滿 5 次重查後回滾（rc=7）" "7" "$rc"
check "重查停在 5/5" "rechecking state and intent (5/5)" "$SWAP_LOG"
check_no "不會有第 6 次重查" "rechecking state and intent (6/5)" "$SWAP_LOG"
teardown

setup 10 10
export STUB_NAMES_BEFORE='["a","mom","kid"]' STUB_NAMES_AFTER='["a","mom"]'
seed_audit "INSERT INTO bots (id,name,project_id,deleted_at,managed_by,parent_bot_id,created_at)
    VALUES ('id-mom','mom','p1',NULL,'user',NULL,'2000-01-01T00:00:00.000Z'),
           ('id-kid','kid','p1','2099-01-01T00:00:02.000Z','child','id-mom','2000-01-01T00:00:00.000Z');
  INSERT INTO runs VALUES ('id-mom','running');
  INSERT INTO intents (id,kind,subject_id,payload_json,status,created_at) VALUES ('it1','retire_child','id-kid','{\"why\":\"reconcile_agent_gone\",\"cause\":\"agent_missing\",\"mode\":\"implicit\",\"pane\":\"present\",\"parent_bot_id\":\"id-mom\"}','done','2099-01-01T00:00:01.000Z');"
rc=$(run)
check_eq "已看見 agent_missing：立刻回滾（rc=7）" "7" "$rc"
check_no "不合格的 intent 不該進入重查" "rechecking state and intent" "$SWAP_LOG"
teardown

setup 10 10
export STUB_NAMES_BEFORE='["a","mom","kid"]' STUB_NAMES_AFTER='["a","mom"]' STUB_NAMES_AFTER_LATER='["a"]' STUB_AFTER_READS_BEFORE_DROP=1
seed_audit "INSERT INTO bots (id,name,project_id,deleted_at,managed_by,parent_bot_id,created_at)
    VALUES ('id-mom','mom','p1',NULL,'user',NULL,'2000-01-01T00:00:00.000Z'),
           ('id-kid','kid','p1',NULL,'child','id-mom','2000-01-01T00:00:00.000Z');
  INSERT INTO runs VALUES ('id-mom','running');"
rc=$(run)
check_eq "重查中母子一起消失：回滾（rc=7）" "7" "$rc"
check "母與 child 都在 missing" 'missing=\[kid mom \]' "$SWAP_LOG"
teardown
unset STUB_NAMES_AFTER_LATER STUB_AFTER_READS_BEFORE_DROP

setup 10 10
export STUB_NAMES_BEFORE='["a","mom","kid"]' STUB_NAMES_AFTER='["a","mom"]'
seed_audit "INSERT INTO bots (id,name,project_id,deleted_at,managed_by,parent_bot_id,created_at)
    VALUES ('id-mom','mom','p1',NULL,'user',NULL,'2000-01-01T00:00:00.000Z'),
           ('id-kid','kid','p1','2099-01-01T00:00:02.000Z','child','id-mom','2000-01-01T00:00:00.000Z');
  INSERT INTO intents (id,kind,subject_id,payload_json,status,created_at) VALUES ('it1','retire_child','id-kid','{\"why\":\"reconcile_agent_gone\",\"cause\":\"unconfirmed\",\"mode\":\"implicit\",\"pane\":\"gone\",\"parent_bot_id\":\"id-mom\"}','done','2099-01-01T00:00:01.000Z');"
rc=$(run)
check_eq "父 row 存在但沒有 active run：child 仍回滾（rc=7）" "7" "$rc"
check "沒有 active parent run 時 child 保持 missing" 'missing=\[kid \]' "$SWAP_LOG"
teardown

setup 10 10
export STUB_NAMES_BEFORE='["a","mom","kid"]' STUB_NAMES_AFTER='["a","mom"]'
seed_audit "INSERT INTO bots (id,name,project_id,deleted_at,managed_by,parent_bot_id,created_at)
    VALUES ('id-mom','mom','p1',NULL,'user',NULL,'2000-01-01T00:00:00.000Z'),
           ('id-kid','kid','p1','2099-01-01T00:00:02.000Z','child','id-mom','2000-01-01T00:00:00.000Z');
  INSERT INTO runs VALUES ('id-mom','running');
  INSERT INTO intents (id,kind,subject_id,payload_json,status,created_at) VALUES ('it1','retire_child','id-kid','{\"why\":\"reconcile_run_already_ended\",\"cause\":\"agent_missing\",\"mode\":\"implicit\",\"pane\":\"present\",\"parent_bot_id\":\"id-mom\",\"last_run\":{\"state\":\"exited\",\"ended_at\":\"2000-01-01T00:00:01.000Z\",\"exit_reason\":\"pane gone\"}}','done','2099-01-01T00:00:01.000Z');"
rc=$(run)
check_eq "換版前就結束的 child 被退役：不回滾（rc=0）" "0" "$rc"
check "被認成預期退役" 'missing=\[\] deleted_in_window=\[kid \]' "$SWAP_LOG"
teardown

for c in ended_late no_last_run agent_gone live_run; do
  setup 10 10
  export STUB_NAMES_BEFORE='["a","mom","kid"]' STUB_NAMES_AFTER='["a","mom"]'
  why=reconcile_run_already_ended; last=',\"last_run\":{\"state\":\"exited\",\"ended_at\":\"2000-01-01T00:00:01.000Z\"}'; extra=""
  case "$c" in
    ended_late) last=',\"last_run\":{\"state\":\"exited\",\"ended_at\":\"2099-01-01T00:00:00.500Z\"}' ;;
    no_last_run) last="" ;;
    agent_gone) why=reconcile_agent_gone ;;
    live_run) extra="INSERT INTO runs VALUES ('id-kid','running');" ;;
  esac
  seed_audit "INSERT INTO bots (id,name,project_id,deleted_at,managed_by,parent_bot_id,created_at)
      VALUES ('id-mom','mom','p1',NULL,'user',NULL,'2000-01-01T00:00:00.000Z'),
             ('id-kid','kid','p1','2099-01-01T00:00:02.000Z','child','id-mom','2000-01-01T00:00:00.000Z');
    INSERT INTO runs VALUES ('id-mom','running'); $extra
    INSERT INTO intents (id,kind,subject_id,payload_json,status,created_at) VALUES ('it1','retire_child','id-kid','{\"why\":\"$why\",\"cause\":\"agent_missing\",\"mode\":\"implicit\",\"pane\":\"present\",\"parent_bot_id\":\"id-mom\"$last}','done','2099-01-01T00:00:01.000Z');"
  rc=$(run)
  check_eq "34d 對照 ${c}：照樣回滾（rc=7）" "7" "$rc"
  check "34d 對照 ${c}：child 列在 missing" 'missing=\[kid \]' "$SWAP_LOG"
  teardown
done

setup 10 10
export STUB_NAMES_BEFORE='["a","mom","kid"]' STUB_NAMES_AFTER='["a","mom"]'
seed_audit "INSERT INTO bots (id,name,project_id,deleted_at,managed_by,parent_bot_id,created_at)
    VALUES ('id-mom','mom','p1',NULL,'user',NULL,'2000-01-01T00:00:00.000Z'),
           ('id-kid','kid','p1','2099-01-01T00:00:02.000Z','child','id-mom','2000-01-01T00:00:00.000Z');
  INSERT INTO runs VALUES ('id-mom','running');
  INSERT INTO intents (id,kind,subject_id,payload_json,status,created_at) VALUES ('it1','retire_child','id-kid','{\"why\":\"parent_replaced_child\",\"cause\":\"parent_replaced_child\",\"mode\":\"implicit\",\"pane\":\"gone\",\"parent_bot_id\":\"id-mom\",\"last_run\":{\"state\":\"exited\",\"ended_at\":\"2099-01-01T00:00:00.900Z\",\"exit_reason\":\"pane exited\"}}','done','2099-01-01T00:00:01.000Z');"
rc=$(run)
check_eq "父 bot 收掉 child（pane exited）：不回滾（rc=0）" "0" "$rc"
check "被認成預期退役" 'missing=\[\] deleted_in_window=\[kid \]' "$SWAP_LOG"
teardown

for c in still_running not_observed pane_present parent_idle; do
  setup 10 10
  export STUB_NAMES_BEFORE='["a","mom","kid"]' STUB_NAMES_AFTER='["a","mom"]'
  state=exited; reason='pane exited'; pane=gone; momrun="INSERT INTO runs VALUES ('id-mom','running');"
  case "$c" in
    still_running) state=running ;;
    not_observed) reason='pane gone' ;;
    pane_present) pane=present ;;
    parent_idle) momrun="" ;;
  esac
  seed_audit "INSERT INTO bots (id,name,project_id,deleted_at,managed_by,parent_bot_id,created_at)
      VALUES ('id-mom','mom','p1',NULL,'user',NULL,'2000-01-01T00:00:00.000Z'),
             ('id-kid','kid','p1','2099-01-01T00:00:02.000Z','child','id-mom','2000-01-01T00:00:00.000Z');
    $momrun
    INSERT INTO intents (id,kind,subject_id,payload_json,status,created_at) VALUES ('it1','retire_child','id-kid','{\"why\":\"parent_replaced_child\",\"cause\":\"parent_replaced_child\",\"mode\":\"implicit\",\"pane\":\"$pane\",\"parent_bot_id\":\"id-mom\",\"last_run\":{\"state\":\"$state\",\"ended_at\":\"2099-01-01T00:00:00.900Z\",\"exit_reason\":\"$reason\"}}','done','2099-01-01T00:00:01.000Z');"
  rc=$(run)
  check_eq "34e 對照 ${c}：照樣回滾（rc=7）" "7" "$rc"
  check "34e 對照 ${c}：child 列在 missing" 'missing=\[kid \]' "$SWAP_LOG"
  teardown
done

for c in pane_closed unconfirmed agent_missing parent_idle no_successor old_retire pending_retire; do
  setup 10 10
  export STUB_NAMES_BEFORE='["mom","kid"]' STUB_NAMES_AFTER='["mom"]'
  old_reason='crashed'; [ "$c" = pane_closed ] && old_reason='agent not found during reconcile'
  successor_cause=pane_closed; successor_status=done; successor_retired_at='2099-01-01T12:27:27.000Z'
  parent_run_sql="INSERT INTO runs VALUES ('id-mom','running');"
  case "$c" in
    unconfirmed|agent_missing) successor_cause="$c" ;;
    parent_idle) parent_run_sql="" ;;
    old_retire) successor_retired_at='2000-01-01T00:00:00.000Z' ;;
    pending_retire) successor_status=pending ;;
  esac
  successor_sql="INSERT INTO bots (id,name,project_id,deleted_at,managed_by,parent_bot_id,created_at)
    VALUES ('id-new-kid','new-kid','p1','2099-01-01T12:27:27.000Z','child','id-mom','2099-01-01T12:27:20.000Z');
    INSERT INTO intents (id,kind,subject_id,payload_json,status,created_at) VALUES ('it-new','retire_child','id-new-kid','{\"why\":\"reconcile_run_already_ended\",\"cause\":\"$successor_cause\",\"parent_bot_id\":\"id-mom\",\"last_run\":{\"pane_id\":\"wBVC:pUX\",\"state\":\"exited\",\"exit_reason\":\"pane exited\"}}','$successor_status','$successor_retired_at');"
  [ "$c" = no_successor ] && successor_sql=""
  seed_audit "INSERT INTO bots (id,name,project_id,deleted_at,managed_by,parent_bot_id,created_at)
      VALUES ('id-mom','mom','p1',NULL,'user',NULL,'2000-01-01T00:00:00.000Z'),
             ('id-kid','kid','p1','2099-01-01T12:27:17.000Z','child','id-mom','2000-01-01T00:00:00.000Z');
    $parent_run_sql
    $successor_sql
    INSERT INTO intents (id,kind,subject_id,payload_json,status,created_at) VALUES ('it-old','retire_child','id-kid','{\"why\":\"parent_replaced_child\",\"cause\":\"parent_replaced_child\",\"mode\":\"implicit\",\"pane\":\"gone\",\"parent_bot_id\":\"id-mom\",\"last_run\":{\"pane_id\":\"wBVC:pUX\",\"state\":\"exited\",\"ended_at\":\"2099-01-01T12:27:17.000Z\",\"exit_reason\":\"$old_reason\"}}','done','2099-01-01T12:27:17.000Z');"
  rc=$(run)
  if [ "$c" = pane_closed ]; then
    check_eq "1895 同 pane successor 正常退役且父 active：不回滾（rc=0）" "0" "$rc"
    check "1895 舊 child 被認成預期退役" 'missing=\[\] deleted_in_window=\[kid \]' "$SWAP_LOG"
    check_no "1895 正常替換與收尾不回滾" "ROLLBACK" "$SWAP_LOG"
  else
    check_eq "1895 對照 ${c}：照樣回滾（rc=7）" "7" "$rc"
    check "1895 對照 ${c}：舊 child 保持 missing" 'missing=\[kid \]' "$SWAP_LOG"
  fi
  teardown
done

setup 10 10
export STUB_NAMES_BEFORE='["a","mom","kid"]' STUB_NAMES_AFTER='["a","mom"]'
seed_audit "INSERT INTO bots (id,name,project_id,deleted_at,managed_by,parent_bot_id,created_at)
    VALUES ('id-mom','mom','p1',NULL,'user',NULL,'2000-01-01T00:00:00.000Z'),
           ('id-kid','kid','p1','2099-01-01T00:00:02.000Z','child','id-mom','2000-01-01T00:00:00.000Z');
  INSERT INTO runs VALUES ('id-mom','running');
  INSERT INTO intents (id,kind,subject_id,payload_json,status,created_at) VALUES ('it1','retire_child','id-kid','{\"why\":\"parent_replaced_child\",\"cause\":\"parent_replaced_child\",\"mode\":\"implicit\",\"pane\":\"gone\",\"parent_bot_id\":\"id-mom\",\"last_run\":{\"state\":\"exited\",\"ended_at\":\"2099-01-01T00:00:00.900Z\",\"exit_reason\":\"agent not found during reconcile\"}}','done','2099-01-01T00:00:01.000Z');"
rc=$(run)
check_eq "34g a 本次型（agent not found during reconcile，父 active）：不回滾（rc=0）" "0" "$rc"
check "34g a 被認成預期退役" 'missing=\[\] deleted_in_window=\[kid \]' "$SWAP_LOG"
teardown

setup 10 10
export STUB_NAMES_BEFORE='["a","mom","kid"]' STUB_NAMES_AFTER='["a","mom"]'
seed_audit "INSERT INTO bots (id,name,project_id,deleted_at,managed_by,parent_bot_id,created_at)
    VALUES ('id-mom','mom','p1',NULL,'user',NULL,'2000-01-01T00:00:00.000Z'),
           ('id-kid','kid','p1','2099-01-01T00:00:02.000Z','user',NULL,'2000-01-01T00:00:00.000Z');
  INSERT INTO runs VALUES ('id-mom','running');
  INSERT INTO intents (id,kind,subject_id,payload_json,status,created_at) VALUES ('it1','retire_child','id-kid','{\"why\":\"parent_replaced_child\",\"cause\":\"parent_replaced_child\",\"mode\":\"implicit\",\"pane\":\"gone\",\"parent_bot_id\":\"id-mom\",\"last_run\":{\"state\":\"exited\",\"ended_at\":\"2099-01-01T00:00:00.900Z\",\"exit_reason\":\"agent not found during reconcile\"}}','done','2099-01-01T00:00:01.000Z');"
rc=$(run)
check_eq "34g c 頂層 bot 不見：回滾（rc=7）" "7" "$rc"
check "34g c 頂層 bot 列在 missing" 'missing=\[kid \]' "$SWAP_LOG"
teardown

setup 10 10
export STUB_NAMES_BEFORE='["a","mom","kid"]' STUB_NAMES_AFTER='["a"]'
seed_audit "INSERT INTO bots (id,name,project_id,deleted_at,managed_by,parent_bot_id,created_at)
    VALUES ('id-mom','mom','p1','2099-01-01T00:00:01.000Z','user',NULL,'2000-01-01T00:00:00.000Z'),
           ('id-kid','kid','p1','2099-01-01T00:00:02.000Z','child','id-mom','2000-01-01T00:00:00.000Z');
  INSERT INTO runs VALUES ('id-mom','running');
  INSERT INTO intents (id,kind,subject_id,payload_json,status,created_at) VALUES ('it1','retire_child','id-kid','{\"why\":\"parent_replaced_child\",\"cause\":\"parent_replaced_child\",\"mode\":\"implicit\",\"pane\":\"gone\",\"parent_bot_id\":\"id-mom\",\"last_run\":{\"state\":\"exited\",\"ended_at\":\"2099-01-01T00:00:00.900Z\",\"exit_reason\":\"agent not found during reconcile\"}}','done','2099-01-01T00:00:01.000Z');"
rc=$(run)
check_eq "34g d 父 bot 已刪：回滾（rc=7）" "7" "$rc"
check "34g d child 列在 missing" 'missing=\[.*kid' "$SWAP_LOG"
teardown

for c in parent_idle pane_present; do
  setup 10 10
  export STUB_NAMES_BEFORE='["a","mom","kid"]' STUB_NAMES_AFTER='["a","mom"]'
  pane=gone; momrun="INSERT INTO runs VALUES ('id-mom','running');"
  case "$c" in
    parent_idle) momrun="" ;;
    pane_present) pane=present ;;
  esac
  seed_audit "INSERT INTO bots (id,name,project_id,deleted_at,managed_by,parent_bot_id,created_at)
      VALUES ('id-mom','mom','p1',NULL,'user',NULL,'2000-01-01T00:00:00.000Z'),
             ('id-kid','kid','p1','2099-01-01T00:00:02.000Z','child','id-mom','2000-01-01T00:00:00.000Z');
    $momrun
    INSERT INTO intents (id,kind,subject_id,payload_json,status,created_at) VALUES ('it1','retire_child','id-kid','{\"why\":\"parent_replaced_child\",\"cause\":\"parent_replaced_child\",\"mode\":\"implicit\",\"pane\":\"$pane\",\"parent_bot_id\":\"id-mom\",\"last_run\":{\"state\":\"exited\",\"ended_at\":\"2099-01-01T00:00:00.900Z\",\"exit_reason\":\"agent not found during reconcile\"}}','done','2099-01-01T00:00:01.000Z');"
  rc=$(run)
  check_eq "34g 對照 ${c}：照樣回滾（rc=7）" "7" "$rc"
  check "34g 對照 ${c}：child 列在 missing" 'missing=\[kid \]' "$SWAP_LOG"
  teardown
done

setup 10 10
export STUB_NAMES_BEFORE='["a","mom","kid"]' STUB_NAMES_AFTER='["a","mom"]'
seed_audit "INSERT INTO bots (id,name,project_id,deleted_at,managed_by,parent_bot_id,created_at)
    VALUES ('id-mom','mom','p1',NULL,'user',NULL,'2000-01-01T00:00:00.000Z'),
           ('id-kid','kid','p1','2099-01-01T00:00:02.000Z','child','id-mom','2000-01-01T00:00:00.000Z');
  INSERT INTO runs VALUES ('id-mom','running');
  INSERT INTO intents (id,kind,subject_id,payload_json,status,created_at) VALUES ('it1','retire_child','id-kid','{\"boot_id\":\"boot-test\",\"call_site\":\"daemon/src/reconcile.rs:423:12\",\"cause\":\"parent_replaced_child\",\"last_run\":{\"ended_at\":\"2099-01-01T00:00:00.900Z\",\"exit_reason\":\"agent not found during reconcile\",\"id\":\"run-test\",\"pane_id\":\"w0:pRG\",\"state\":\"exited\"},\"mode\":\"implicit\",\"name\":\"kid\",\"pane\":\"gone\",\"parent_bot_id\":\"id-mom\",\"project_id\":\"p1\",\"requested_by\":\"-\",\"why\":\"parent_replaced_child\"}','done','2099-01-01T00:00:01.000Z');"
rc=$(run)
check_eq "34h wait1937 真實形狀：不回滾（rc=0）" "0" "$rc"
check "34h 被認成預期退役" 'missing=\[\] deleted_in_window=\[kid \]' "$SWAP_LOG"
teardown

setup 10 10
export STUB_NAMES_BEFORE='["a","mom","kid"]' STUB_NAMES_AFTER='["a","mom"]'
seed_audit "INSERT INTO bots (id,name,project_id,deleted_at,managed_by,parent_bot_id,created_at)
    VALUES ('id-mom','mom','p1',NULL,'user',NULL,'2000-01-01T00:00:00.000Z'),
           ('id-kid','kid','p1','2099-01-01T00:00:02.000Z','child','id-mom','2000-01-01T00:00:00.000Z');
  INSERT INTO runs VALUES ('id-mom','running');
  INSERT INTO intents (id,kind,subject_id,payload_json,status,created_at) VALUES ('it1','retire_child','id-kid','{\"why\":\"parent_replaced_child\",\"cause\":\"parent_replaced_child\",\"mode\":\"implicit\",\"pane\":\"gone\",\"parent_bot_id\":\"id-mom\",\"last_run\":{\"state\":\"exited\",\"ended_at\":\"2099-01-01T00:00:00.900Z\",\"exit_reason\":\"crashed\"}}','done','2099-01-01T00:00:01.000Z');"
rc=$(run)
check_eq "34i exit_reason=crashed：回滾（rc=7）" "7" "$rc"
check "34i child 列在 missing" 'missing=\[kid \]' "$SWAP_LOG"
teardown

setup 11 10
export STUB_SESSION_OK_AFTER_FORWARD=""
export STUB_NAMES_BEFORE='["mom","kid"]' STUB_NAMES_AFTER='["mom","new-kid"]'
seed_audit "INSERT INTO bots (id,name,project_id,deleted_at) VALUES ('id-mom','mom','p1',NULL), ('id-kid','kid','p1',NULL);
  INSERT INTO bots (id,name,project_id,deleted_at,managed_by,parent_bot_id,created_at)
    VALUES ('id-new-kid','new-kid','p1',NULL,'child','id-mom','2099-01-01T00:00:02.000Z');"
rc=$(run)
check_eq "舊 child 無明確 successor 退役證據仍回滾（rc=7）" "7" "$rc"
check "DB 還原前記下的新收編 child 在回滾後有警示" "WARN: rollback restored the DB but these newly adopted children may still have live panes: new-kid (id-new-kid)" "$SWAP_LOG"
check_eq "binary 照常還原" "old-binary" "$(cat "$AGM_REPO/target/release/agents-managerd")"
teardown

setup 11 10
export STUB_SUPERVISOR=stopped STUB_SESSION_OK_AFTER_FORWARD=""
rc=$(run_capture)
check_eq "forced rollback 完成（rc=7）" "7" "$rc"
check_no "rollback 時 helper 已定義" "rollback_new_children: command not found" "$ROOT/swap.out"
check_no "rollback 不會因缺 helper 遺漏 child 對帳" "could not determine whether newly adopted children" "$SWAP_LOG"
teardown

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

setup 10 10
: > "$AGM_DIR/kills.log"
kill() {
  echo "kill $*" >> "$AGM_DIR/kills.log"
  [ "$1" = -0 ] && return 0
  return 0
}
export -f kill
rc=$(run_capture)
unset -f kill
check_eq "KILL 後仍存活就以 rc=11 中止" "11" "$rc"
check_eq "KILL 後仍存活時正式 binary 不動" "old-binary" "$(cat "$AGM_REPO/target/release/agents-managerd")"
check_eq "KILL 後仍存活時不啟動另一顆 daemon" "0" "$(wc -l < "$AGM_DIR/starts.log" | tr -d ' ')"
check "KILL 後仍存活時明確記錄拒絕換檔" "daemon is still alive after SIGKILL" "$SWAP_LOG"
teardown

setup 10 10
export STUB_NO_PID=1
rc=$(run_capture)
check_eq "舊 daemon pid 找不到就以 rc=11 中止" "11" "$rc"
check_eq "舊 daemon pid 找不到時正式 binary 不動" "old-binary" "$(cat "$AGM_REPO/target/release/agents-managerd")"
check_eq "舊 daemon pid 找不到時不啟動另一顆 daemon" "0" "$(wc -l < "$AGM_DIR/starts.log" | tr -d ' ')"
check "舊 daemon pid 找不到時記錄拒絕換檔" "could not identify the running daemon pid" "$SWAP_LOG"
teardown

setup 10 10
export STUB_PID=$'99999\n99998'
rc=$(run_capture)
check_eq "有多個 matching daemon pid 就以 rc=11 中止" "11" "$rc"
check_eq "有多個 daemon pid 時正式 binary 不動" "old-binary" "$(cat "$AGM_REPO/target/release/agents-managerd")"
check_eq "有多個 daemon pid 時不啟動另一顆 daemon" "0" "$(wc -l < "$AGM_DIR/starts.log" | tr -d ' ')"
check "多個 daemon pid 時記錄拒絕換檔" "found 2 matching daemon processes" "$SWAP_LOG"
teardown

setup 10 10
export STUB_SUPERVISOR=stopped STUB_MUTATE_DB=1
: > "$AGM_DIR/kills.log"
kill() {
  echo "kill $*" >> "$AGM_DIR/kills.log"
  if [ "$1" = -0 ]; then
    [ "$(grep -c '^kill -TERM' "$AGM_DIR/kills.log")" -ge 2 ] && return 0
    return 1
  fi
  return 0
}
export -f kill
rc=$(run_capture)
unset -f kill
check_eq "rollback KILL 後仍存活就以 rc=11 停手" "11" "$rc"
check "rollback KILL 後仍存活時新版 binary 留在原位" "new-binary" "$AGM_REPO/target/release/agents-managerd"
check_eq "rollback KILL 後仍存活時新版 DB 不被還原" "mutated-by-new-binary" "$(cat "$DAEMON_DB")"
check_no "rollback KILL 後仍存活時不進行 DB restore" "db restored from" "$SWAP_LOG"
teardown

setup 10 10
export STUB_SUPERVISOR=stopped STUB_MUTATE_DB=1 STUB_PID_AFTER_START=$'99998\n99997'
rc=$(run_capture)
check_eq "rollback 發現多顆 daemon 就以 rc=11 停手" "11" "$rc"
check_eq "rollback 多顆 daemon 時正式 binary 不還原" "$(shasum -a 256 "$CHECKOUT/target/release/agents-managerd" | awk '{print $1}')" "$(shasum -a 256 "$AGM_REPO/target/release/agents-managerd" | awk '{print $1}')"
check_eq "rollback 多顆 daemon 時新版 DB 不被還原" "mutated-by-new-binary" "$(cat "$DAEMON_DB")"
check_no "rollback 多顆 daemon 時不進行 DB restore" "db restored from" "$SWAP_LOG"
teardown

setup 10 10
export AGM_OPS_PLATFORM=linux
rc=$(run)
check_eq "Linux 上順利換版 rc=0" "0" "$rc"
check "Linux 走 systemd-run --user 的 transient unit" "^--user --collect --unit=am-daemon-swap-[0-9]*-1 -p Type=forking -p KillMode=process -- /" "$AGM_DIR/systemd-run.log"
check "啟動器用絕對路徑（transient unit 的 cwd 是 HOME）" " $HERE/daemon-start.py $AGM_REPO $DAEMON_LOG\$" "$AGM_DIR/systemd-run.log"
check_eq "Linux 上完全沒叫 launchctl" "" "$(cat "$AGM_DIR/launchctl.log")"
check "有換上新 binary" "new-binary" "$AGM_DIR/started-binary.log"
teardown

setup 11 10
export AGM_OPS_PLATFORM=linux STUB_SUPERVISOR=stopped
rc=$(run)
check_eq "Linux 往前修 rc=6" "6" "$rc"
check "第一次啟動 unit -1" "--unit=am-daemon-swap-[0-9]*-1 " "$AGM_DIR/systemd-run.log"
check "往前修那次換成 unit -2" "--unit=am-daemon-swap-[0-9]*-2 " "$AGM_DIR/systemd-run.log"
teardown

setup 10 10
export AGM_OPS_PLATFORM=linux STUB_SYSTEMD_RUN_RC=1 STUB_SESSION_OK=""
rc=$( unset XDG_RUNTIME_DIR; run )
check "沒有 XDG_RUNTIME_DIR 時補成 /run/user/<uid>" "^XDG_RUNTIME_DIR=/run/user/$(id -u)\$" "$AGM_DIR/systemd-run.env"
check "systemd-run 失敗有留 log" "WARN: systemd-run rc=1" "$SWAP_LOG"
check_eq "起不來就照舊回滾（rc=7）" "7" "$rc"
teardown

for case_ in "other:0000000000000000000000000000000000000000" "dirty:DIRTY" "unknown:unknown" "old:NONE"; do
  name=${case_%%:*}; val=${case_#*:}
  setup 10 10
  case "$val" in
    DIRTY) export STUB_BIN_VERSION_SHA="${SHA}-dirty" ;;
    NONE)  export STUB_BIN_VERSION_SHA="" ;;
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

setup 10 10
PREFIX_SHA=$(printf '%s' "$SHA" | cut -c1-7)
rc=$(run "" "$PREFIX_SHA")
check_eq "只給 7 碼前綴就拒絕（rc=10）" "10" "$rc"
check_no "短前綴沒有去拿窗口" "lease acquire restart" "$AGM_DIR/calls.log"
check_file "短前綴沒有啟動任何東西" no "$AGM_DIR/started"
check_eq "短前綴沒有換正式 binary" "old-binary" "$(cat "$AGM_REPO/target/release/agents-managerd")"
teardown

setup 10 10
export STUB_BIN_VERSION_SHA="$SHA"
rc=$(run)
check_eq "binary 的 sha 對上 → 照常換版（rc=0）" "0" "$rc"
check "log 記下驗過的 sha" "binary sha ok" "$SWAP_LOG"
teardown

setup 10 10
export STUB_BIN_VERSION_SHA="${SHA}f"
rc=$(run)
check_eq "binary sha 後面多一碼要拒絕（rc=10）" "10" "$rc"
check_no "binary sha 多一碼沒有去拿窗口" "lease acquire restart" "$AGM_DIR/calls.log"
check_eq "binary sha 多一碼沒有換正式 binary" "old-binary" "$(cat "$AGM_REPO/target/release/agents-managerd")"
teardown

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

setup 10 10
export STUB_DAEMON_DIRTY=true
rc=$(run)
check_eq "新 daemon 說自己是髒樹建的 → 回滾（rc=7）" "7" "$rc"
teardown

unbalanced=$(awk '/^ *setup( |$|;)/ { if (open) print last; open = 1; last = NR }
                  /^ *teardown( *$|;| *#)|; *teardown *$/ { open = 0 }' "$0" | tr '\n' ' ')
check_eq "每個 setup 都有 teardown（沒收的 setup 行號）" "" "$unbalanced"

echo "$PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
