#!/bin/bash
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
SCRIPT="$HERE/release-triage-kick.sh"
PASS=0
FAIL=0
. "$HERE/../test-assertions.sh" || exit 1

setup() {
  ROOT=$(mktemp -d)
  export AGM_DIR="$ROOT/agm" AGM_REPO="$ROOT/repo" CLAUDE_VERSIONS_DIR="$ROOT/claude-versions"
  mkdir -p "$AGM_DIR/bin" "$ROOT/triage" "$CLAUDE_VERSIONS_DIR"
  cp "$HERE/fixtures/patrol-runtime.json" "$AGM_DIR/runtime.json"
  echo "TASK-BODY-MARKER 交辦正文" > "$AGM_DIR/release-triage-task.md"

  cat > "$AGM_DIR/bin/agm" <<'STUB'
#!/bin/bash
echo "$*" >> "$AGM_DIR/calls.log"
DEFAULT_QUOTA='{"kinds":{"claude":{"five_hour":{"used_pct":10.0},"limit_hit":null}}}'
for a in "$@"; do
  case "$a" in
    --compact|--bot|--review-by|--text-file|--request-id|--source|--reason|--detail|--kind|--version|--check|--binary-diff-file|assign|quota|state|ops-alert|release-triage|dispatched|publish|show|auto) ;;
    --*) echo "agm: error: unrecognized arguments: $a" >&2; exit 2 ;;
  esac
done
case "$*" in
  *"release-triage "*)
    [ -n "${STUB_OLD_AGM:-}" ] && { echo "agm: error: argument cmd: invalid choice: 'release-triage'" >&2; exit 2; }
    case "$*" in
      *" auto "*)
        case "$*" in *" --check"*) printf '{"enabled":%s}' "${STUB_AUTO_ENABLED:-false}"; exit 0 ;; esac
        [ -n "${STUB_AUTO_FAIL:-}" ] && exit 1
        if [ -f "$AGM_DIR/auto-published.json" ]; then
          k=$(printf '%s' "$*" | sed -n 's/.*--kind \([a-z]*\).*/\1/p')
          cp "$AGM_DIR/auto-published.json" "$AGM_DIR/show-$k.json"
        fi
        for i in $(seq 1 $#); do
          eval "a=\${$i}"
          case "$a" in --binary-diff-file) eval "f=\${$((i+1))}"; cat "$f" > "$AGM_DIR/auto-diff.txt" ;; esac
        done
        printf '%s' "${STUB_AUTO_JSON:-{\"results\":[]\}}" ;;
      *" dispatched "*) [ -n "${STUB_DISPATCHED_FAIL:-}" ] && exit 1; printf '%s' '{"dispatched":1}' ;;
      *" publish "*) [ -z "${STUB_PUBLISH_SLEEP:-}" ] || sleep "$STUB_PUBLISH_SLEEP"
        printf '%s' "${STUB_PUBLISH_JSON:-{\"publish_enabled\":false,\"results\":[]\}}" ;;
      *" show "*)
        k=$(printf '%s' "$*" | sed -n 's/.*--kind \([a-z]*\).*/\1/p')
        f="$AGM_DIR/show-$k.json"; [ -f "$f" ] && cat "$f" || printf '%s' '{"rows":[]}' ;;
    esac ;;
  *" quota"*|"--compact quota")
    [ -n "${STUB_QUOTA_FAIL:-}" ] && exit 1
    printf '%s' "${STUB_QUOTA_JSON:-$DEFAULT_QUOTA}" ;;
  *" state"*|"--compact state")
    printf '%s' '{"bots":[{"id":"bot-resp","kind":"claude","identity":"cc0"},{"id":"bot-release","kind":"claude","identity":"cc1"},{"id":"bot-override","kind":"claude","identity":""}]}' ;;
  *" assign "*)
    [ -n "${STUB_ASSIGN_FAIL:-}" ] && exit 1
    for i in $(seq 1 $#); do
      eval "a=\${$i}"
      case "$a" in --text-file) eval "f=\${$((i+1))}"; cat "$f" >> "$AGM_DIR/assign-body.txt" ;; esac
    done
    printf '%s' '{"id":"a-1"}' ;;
  *ops-alert*) printf '%s' '{"ok":true}' ;;
esac
STUB
  chmod +x "$AGM_DIR/bin/agm"
  cat > "$AGM_DIR/bin/gh" <<'GH'
#!/bin/bash
echo "$*" >> "$AGM_DIR/gh-calls.log"
[ "$1 $2" = "issue view" ] || exit 2
n=${3##*/}
[ ! -f "$AGM_DIR/gh-fail-$n" ] || exit 1
if [ -f "$AGM_DIR/gh-state-$n" ]; then cat "$AGM_DIR/gh-state-$n"; else echo OPEN; fi
GH
  chmod +x "$AGM_DIR/bin/gh"
  export GH_BIN="$AGM_DIR/bin/gh"

  cat > "$ROOT/fake-agents-managerd" <<'PYEOF'
#!/bin/bash
[ "${1:-}" = "release-triage-check" ] && [ "${2:-}" = "--kind" ] && [ "${4:-}" = "--json" ] || exit 2
echo "check $3" >> "$AGM_DIR/check.log"
f="$(dirname "$0")/triage/$3.json"
if [ ! -f "$f" ]; then
  # agy／grok（#1928）沒有各自的 fixture 時視為「沒有新版」，不讓舊測試被新 kind 的失敗洗掉。
  case "$3" in agy|grok) printf '{"kind":"%s","from":"1.0.0","to":"1.0.0","pending":[]}' "$3"; exit 0 ;; esac
  echo "boom" >&2; exit 1
fi
cat "$f"
PYEOF
  chmod +x "$ROOT/fake-agents-managerd"
  export AM_BINARY="$ROOT/fake-agents-managerd"
  : > "$AGM_DIR/calls.log"; : > "$AGM_DIR/assign-body.txt"
  export STUB_ASSIGN_FAIL="" STUB_QUOTA_FAIL="" STUB_QUOTA_JSON="" STUB_DISPATCHED_FAIL="" STUB_PUBLISH_JSON="" STUB_PUBLISH_SLEEP="" STUB_OLD_AGM="" STUB_AUTO_ENABLED="" STUB_AUTO_FAIL="" STUB_AUTO_JSON=""
  unset AGM_RELEASE_BOT
}
teardown() {
  rm -rf "$ROOT"
  unset GH_BIN
  unset AGM_DIR AGM_REPO AM_BINARY AGM_RELEASE_BOT AGM_TRIAGE_QUOTA_MAX AGM_LOCK_STALE_SECS AGM_LOCK_HUNG_SECS AGM_LOCK_QUIET_SECS AGM_EXTRA_PATH AGM_FAIL_ALERT_AFTER CLAUDE_VERSIONS_DIR
  unset STUB_ASSIGN_FAIL STUB_QUOTA_FAIL STUB_QUOTA_JSON STUB_DISPATCHED_FAIL STUB_PUBLISH_JSON STUB_PUBLISH_SLEEP STUB_OLD_AGM STUB_AUTO_ENABLED STUB_AUTO_FAIL STUB_AUTO_JSON
}

mk_pending() {
  local kind="$1" to="$2"; shift 2
  python3 - "$kind" "$to" "$ROOT/triage/$kind.json" "$@" <<'PY'
import json, sys
kind, to, path, *vers = sys.argv[1:]
pend = [{"version": v,
         "kept": [{"id": f"{v}-k{i}", "text": f"kept {v} {i}", "categories": ["hook"]} for i in range(2)],
         "unmatched": [{"id": f"{v}-u0", "text": f"unmatched {v}"}],
         "dropped_count": 4} for v in vers]
json.dump({"kind": kind, "from": "0.0.0", "to": to, "pending": pend}, open(path, "w"))
show_path = path.rsplit("/", 1)[0] + "/../agm/show-" + kind + ".json"
try:
    show = json.load(open(show_path))
except (OSError, ValueError):
    show = {"rows": []}
rows = show.setdefault("rows", [])
for version in vers:
    if not any(row.get("kind") == kind and row.get("version") == version for row in rows):
        rows.append({"kind": kind, "version": version, "status": "pending", "dispatch_gen": 0})
json.dump(show, open(show_path, "w"))
PY
}
mk_empty() { printf '{"kind":"%s","from":"1.0.0","to":"1.0.0","pending":[]}' "$1" > "$ROOT/triage/$1.json"; }
start_live_runner() {
  export STUB_PUBLISH_SLEEP=30
  bash "$SCRIPT" > "$ROOT/live-runner.log" 2>&1 &
  LIVE=$!
  _wait=0
  while [ ! -s "$AGM_DIR/release-triage.lock/owner" ] && [ "$_wait" -lt 100 ]; do
    sleep 0.05
    _wait=$((_wait + 1))
  done
  [ -s "$AGM_DIR/release-triage.lock/owner" ]
}

assigns() { grep -c ' assign ' "$AGM_DIR/calls.log" 2>/dev/null | tr -d ' '; }
assignment_ids() { sed -n '/ assign /s/.*--request-id \([^ ]*\).*/\1/p' "$AGM_DIR/calls.log"; }

setup
mk_empty claude; mk_empty codex
/bin/bash "$SCRIPT" >"$ROOT/empty.out" 2>"$ROOT/empty.err"
check_no "空 pending 的 exit cleanup 不因 Bash 3.2 空陣列報錯" "unbound variable" "$ROOT/empty.err"
equals "pending 空：不派" "$(assigns)" "0"
equals "pending 空：不寫 log" "$(cat "$AGM_DIR/release-triage.log" 2>/dev/null)" ""
equals "pending 空：四個 kind 都檢查過" "$(wc -l < "$AGM_DIR/check.log" | tr -d ' ')" "4"
check_no "pending 空：連額度都不查" "quota" "$AGM_DIR/calls.log"
teardown

setup
mk_pending claude 2.1.278 2.1.277 2.1.278; mk_empty codex
bash "$SCRIPT"
equals "派一次" "$(assigns)" "1"
check "旗標是 --request-id 且帶 kind＋to 與代號指紋" "\-\-request-id release-triage-claude-2.1.278-" "$AGM_DIR/calls.log"
check "派給協調者" "\-\-bot bot-resp" "$AGM_DIR/calls.log"
check_no "不派給巡檢自己（daemon 會 400）" "\-\-bot bot-agm" "$AGM_DIR/calls.log"
check "交辦給巡檢驗收" "\-\-review-by patrol" "$AGM_DIR/calls.log"
check "交辦帶正文" "TASK-BODY-MARKER" "$AGM_DIR/assign-body.txt"
check "交辦帶條目 id" "2.1.277-k0" "$AGM_DIR/assign-body.txt"
check "交辦帶 unmatched" "2.1.278-u0" "$AGM_DIR/assign-body.txt"
check "有記 log" "claude → 2.1.278：已派" "$AGM_DIR/release-triage.log"
teardown

setup
mk_pending claude 2.1.278 2.1.277 2.1.278; mk_empty codex
bash "$SCRIPT"
check "兩版的條目都在同一則正文" "2.1.277-k0" "$AGM_DIR/assign-body.txt"
check "第二版的條目也在同一則正文" "2.1.278-k0" "$AGM_DIR/assign-body.txt"
check "dispatched 一次帶兩版" "release-triage dispatched --kind claude --version 2.1.277 --version 2.1.278 --bot bot-resp" "$AGM_DIR/calls.log"
equals "dispatched 只打一次" "$(grep -c ' dispatched ' "$AGM_DIR/calls.log" | tr -d ' ')" "1"
teardown

setup
mk_pending claude 2.1.278 2.1.278; mk_pending codex 0.155.0 0.155.0
bash "$SCRIPT"
equals "兩個 kind 各派一則" "$(assigns)" "2"
check "claude 的 request-id" "\-\-request-id release-triage-claude-2.1.278-" "$AGM_DIR/calls.log"
check "codex 的 request-id" "\-\-request-id release-triage-codex-0.155.0-" "$AGM_DIR/calls.log"
equals "額度一輪只查一次" "$(grep -c ' quota' "$AGM_DIR/calls.log" | tr -d ' ')" "1"
teardown
setup
mk_pending agy 1.4.6 1.4.5 1.4.6; mk_pending grok 1.0.67 1.0.67
bash "$SCRIPT"
equals "agy、grok 各派一則" "$(assigns)" "2"
check "agy 的 request-id" "\-\-request-id release-triage-agy-1.4.6-" "$AGM_DIR/calls.log"
check "grok 的 request-id" "\-\-request-id release-triage-grok-1.0.67-" "$AGM_DIR/calls.log"
check "dispatched 帶 agy 兩版" "release-triage dispatched --kind agy --version 1.4.5 --version 1.4.6 --bot bot-resp" "$AGM_DIR/calls.log"
check "agy 通知 id" "通知 request-id（child 需原樣使用）：agm-release-triage-agy-1.4.6-notice" "$AGM_DIR/assign-body.txt"
check "grok 通知 id" "通知 request-id（child 需原樣使用）：agm-release-triage-grok-1.0.67-notice" "$AGM_DIR/assign-body.txt"
check_no "agy／grok 不碰 claude 專屬的 binary diff" "claude-release-diff" "$AGM_DIR/assign-body.txt"
teardown

setup
rm -f "$ROOT/triage/claude.json"; mk_pending codex 0.155.0 0.155.0
bash "$SCRIPT"
equals "claude 檢查失敗：codex 照派" "$(assigns)" "1"
check "codex 有派" "release-triage-codex-0.155.0" "$AGM_DIR/calls.log"
check "claude 失敗有記 log" "release-triage-check --kind claude 失敗" "$AGM_DIR/release-triage.log"
teardown

setup
mk_pending claude 2.1.290 2.1.281 2.1.282 2.1.283 2.1.284 2.1.285 2.1.290; mk_empty codex
bash "$SCRIPT"
equals "超過 5 版仍只派一則" "$(assigns)" "1"
check "request-id 用這批最後一版" "release-triage-claude-2.1.285" "$AGM_DIR/calls.log"
check "第 6 版不在正文的 pending" '"deferred_versions"' "$AGM_DIR/assign-body.txt"
check_no "第 6 版的條目不在這則" "2.1.290-k0" "$AGM_DIR/assign-body.txt"
teardown
setup
python3 - "$ROOT/triage/claude.json" <<'PY'
import json, sys
def ver(v, n):
    return {"version": v, "kept": [{"id": f"{v}-{i}", "text": "t", "categories": []} for i in range(n)], "unmatched": [], "dropped_count": 0}
json.dump({"kind": "claude", "from": "1", "to": "2.0.3", "pending": [ver("2.0.1", 50), ver("2.0.2", 40), ver("2.0.3", 5)]}, open(sys.argv[1], "w"))
show = sys.argv[1].rsplit("/", 1)[0] + "/../agm/show-claude.json"
json.dump({"rows": [{"kind": "claude", "version": v, "status": "pending", "dispatch_gen": 0}
                     for v in ("2.0.1", "2.0.2", "2.0.3")]}, open(show, "w"))
PY
mk_empty codex
bash "$SCRIPT"
check "50＋40 超過 80：第二版留下一輪" "release-triage-claude-2.0.1" "$AGM_DIR/calls.log"
check_no "第二版條目不在這則" "2.0.2-0" "$AGM_DIR/assign-body.txt"
teardown

setup
mk_pending claude 2.1.278 2.1.278; mk_empty codex
export STUB_QUOTA_JSON='{"kinds":{"claude":{"five_hour":{"used_pct":86.0},"limit_hit":null}}}'
bash "$SCRIPT"
equals "86%：不派" "$(assigns)" "0"
check "86%：有記 log" "額度閘門擋下" "$AGM_DIR/release-triage.log"
teardown
setup
mk_pending claude 2.1.278 2.1.278; mk_empty codex
export STUB_QUOTA_JSON='{"kinds":{"claude":{"five_hour":{"used_pct":84.0},"limit_hit":null}}}'
bash "$SCRIPT"
equals "84%：照派" "$(assigns)" "1"
teardown
setup
mk_pending claude 2.1.278 2.1.278; mk_empty codex
export STUB_QUOTA_JSON='{"kinds":{"claude":{"five_hour":{"used_pct":3.0},"limit_hit":{"kind":"five_hour"}}}}'
bash "$SCRIPT"
equals "limit_hit：不派" "$(assigns)" "0"
teardown
setup
mk_pending claude 2.1.278 2.1.278; mk_empty codex
export STUB_QUOTA_FAIL=1
bash "$SCRIPT"
equals "額度端點壞：照派" "$(assigns)" "1"
check "額度端點壞：有記 log" "查不到 bot bot-resp 的額度，照派" "$AGM_DIR/release-triage.log"
teardown
setup
mk_pending claude 2.1.278 2.1.278; mk_empty codex
export STUB_QUOTA_JSON='{"kinds":{"codex":{"five_hour":{"used_pct":99.0},"limit_hit":null}}}'
bash "$SCRIPT"
equals "看別的 kind 的額度不影響 claude bot（找不到格＝照派）" "$(assigns)" "1"
teardown
setup
cp "$HERE/fixtures/herdr-update-runtime.json" "$AGM_DIR/runtime.json"
mk_pending claude 2.1.278 2.1.278; mk_empty codex
export STUB_QUOTA_JSON='{"kinds":{"claude":{"five_hour":{"used_pct":5.0},"limit_hit":null},"claude:cc1":{"five_hour":{"used_pct":90.0},"limit_hit":null}}}'
bash "$SCRIPT"
equals "cc1 那格 90%：不派" "$(assigns)" "0"
teardown
setup
mk_pending claude 2.1.278 2.1.278; mk_pending codex 0.155.0 0.155.0
export STUB_QUOTA_JSON='{"kinds":{"claude":{"five_hour":{"used_pct":95.0},"limit_hit":null}}}'
bash "$SCRIPT"
equals "擋下時兩個 kind 都不派" "$(assigns)" "0"
teardown
setup
mk_pending claude 2.1.278 2.1.278; mk_empty codex
export AGM_TRIAGE_QUOTA_MAX=95 STUB_QUOTA_JSON='{"kinds":{"claude":{"five_hour":{"used_pct":90.0},"limit_hit":null}}}'
bash "$SCRIPT"
equals "門檻調到 95：90% 照派" "$(assigns)" "1"
teardown

setup
mk_pending claude 2.1.278 2.1.278; mk_empty codex
mkdir "$AGM_DIR/release-triage.lock"; echo "999999 1" > "$AGM_DIR/release-triage.lock/owner"
export AGM_LOCK_STALE_SECS=0
bash "$SCRIPT"
equals "殘留鎖：回收後照派" "$(assigns)" "1"
check "殘留鎖：有記回收 log" "清掉殘留鎖" "$AGM_DIR/release-triage.log"
[ ! -d "$AGM_DIR/release-triage.lock" ] && { echo "ok   - 跑完鎖有釋放"; PASS=$((PASS + 1)); } || { echo "FAIL - 跑完鎖有釋放"; FAIL=$((FAIL + 1)); }
teardown
setup
mk_pending claude 2.1.278 2.1.278; mk_empty codex
mkdir "$AGM_DIR/release-triage.lock"
bash "$SCRIPT"
equals "剛建立、讀不到執行者的鎖：這輪跳過" "$(assigns)" "0"
check "有記跳過原因" "鎖剛建立" "$AGM_DIR/release-triage.log"
export AGM_LOCK_STALE_SECS=0
bash "$SCRIPT"
equals "沒有 owner 檔的舊鎖：過了門檻就回收" "$(assigns)" "1"
teardown

setup
mk_pending claude 2.1.278 2.1.278; mk_empty codex
start_live_runner
bash "$SCRIPT"
equals "剛拿到的活鎖：不派" "$(assigns)" "0"
check_no "剛拿到的活鎖：不洗 log" "已有執行者" "$AGM_DIR/release-triage.log"
check_no "剛拿到的活鎖：不喊人" "ops-alert" "$AGM_DIR/calls.log"
kill "$LIVE" 2>/dev/null; wait "$LIVE" 2>/dev/null
teardown
setup
mk_pending claude 2.1.278 2.1.278; mk_empty codex
start_live_runner
export AGM_LOCK_QUIET_SECS=0
bash "$SCRIPT"
equals "活鎖：不派" "$(assigns)" "0"
check "活鎖：有記 log" "已有執行者" "$AGM_DIR/release-triage.log"
check_no "活鎖：沒到卡住門檻不喊人" "ops-alert" "$AGM_DIR/calls.log"
export AGM_LOCK_HUNG_SECS=0
bash "$SCRIPT"
check "活鎖卡太久：推 ops-alert" "ops-alert .*\-\-reason runner_hung" "$AGM_DIR/calls.log"
equals "活鎖卡太久：仍不搶鎖、不派" "$(assigns)" "0"
kill "$LIVE" 2>/dev/null; wait "$LIVE" 2>/dev/null
teardown
future_lock() { python3 -c 'import os,sys,time; t=time.time()+7200; os.utime(sys.argv[1],(t,t))' "$AGM_DIR/release-triage.lock"; }
setup
mk_pending claude 2.1.278 2.1.278; mk_empty codex
start_live_runner
future_lock
bash "$SCRIPT"
equals "未來時間的活鎖：不派" "$(assigns)" "0"
check "未來時間的活鎖：記 log 說年齡不可信" "鎖的時間在未來" "$AGM_DIR/release-triage.log"
kill "$LIVE" 2>/dev/null; wait "$LIVE" 2>/dev/null
teardown
setup
mk_pending claude 2.1.278 2.1.278; mk_empty codex
mkdir "$AGM_DIR/release-triage.lock"; echo "999999 $(date +%s)" > "$AGM_DIR/release-triage.lock/owner"; future_lock
bash "$SCRIPT"
equals "未來時間的死鎖：回收後照派" "$(assigns)" "1"
teardown
setup
mk_pending claude 2.1.278 2.1.278; mk_empty codex
bash -c 'sleep 30; :' & LIVE=$!
sleep 0.3
mkdir "$AGM_DIR/release-triage.lock"; echo "$LIVE $(date +%s)" > "$AGM_DIR/release-triage.lock/owner"
export AGM_LOCK_STALE_SECS=0
bash "$SCRIPT"
equals "pid 被別的程序重用：回收後照派" "$(assigns)" "1"
kill "$LIVE" 2>/dev/null; wait "$LIVE" 2>/dev/null
teardown

setup
mk_pending claude 2.1.278 2.1.278; mk_empty codex
printf 'preserve outside guard target\n' > "$ROOT/guard-target"
ln -s "$ROOT/guard-target" "$AGM_DIR/release-triage.lock.guard"
bash "$SCRIPT"
equals "guard symlink 目標原封不動" "$(cat "$ROOT/guard-target")" "preserve outside guard target"
check "guard symlink 推 lock_unavailable" "lock_unavailable" "$AGM_DIR/calls.log"
equals "guard symlink 下不派" "0" "$(assigns)"
teardown

setup
mk_pending claude 2.1.278 2.1.278; mk_empty codex
bash -c 'sleep 30; :' release-triage-kick & LIVE=$!
sleep 0.3
mkdir "$AGM_DIR/release-triage.lock"; echo "$LIVE $(date +%s)" > "$AGM_DIR/release-triage.lock/owner"
export AGM_LOCK_STALE_SECS=0
bash "$SCRIPT"
equals "重用 PID 的 command line 含名稱也要回收" "$(assigns)" "1"
check "重用 PID 不再被判成活 runner" "清掉殘留鎖" "$AGM_DIR/release-triage.log"
kill "$LIVE" 2>/dev/null; wait "$LIVE" 2>/dev/null
teardown

setup
mk_pending claude 2.1.278 2.1.278; mk_empty codex
env -i PATH=/usr/bin:/bin AGM_DIR="$AGM_DIR" AM_BINARY="$AM_BINARY" /bin/bash "$SCRIPT"
equals "env -i 最小 PATH：照派" "$(assigns)" "1"
teardown
setup
mk_pending claude 2.1.278 2.1.278; mk_empty codex
mkdir "$ROOT/nopy"; for t in date cat seq dirname; do ln -s "$(command -v $t)" "$ROOT/nopy/$t"; done
env -i PATH="$ROOT/nopy" AGM_EXTRA_PATH="" AGM_DIR="$AGM_DIR" AM_BINARY="$AM_BINARY" /bin/bash "$SCRIPT"
check "缺 python3：有記 log" "找不到 python3" "$AGM_DIR/release-triage.log"
check "缺 python3：推 ops-alert" "\-\-reason missing_dependency" "$AGM_DIR/calls.log"
equals "缺 python3：不派" "$(assigns)" "0"
teardown

setup
"$AGM_DIR/bin/agm" --compact assign --bot x --client-request-id y 2>/dev/null; equals "stub 對未知旗標 exit 2" "$?" "2"
teardown

setup
mk_pending claude 2.1.278 2.1.278; mk_empty codex
export STUB_ASSIGN_FAIL=1
bash "$SCRIPT"
check "失敗有記 log" "派工失敗" "$AGM_DIR/release-triage.log"
export STUB_ASSIGN_FAIL=""
bash "$SCRIPT"
equals "下一輪重派（同一個 request-id）" "$(grep -c 'release-triage-claude-2.1.278' "$AGM_DIR/calls.log" | tr -d ' ')" "2"
teardown

setup
mk_pending claude 2.1.278 2.1.278; mk_empty codex
cat > "$AGM_DIR/show-claude.json" <<'JSON'
{"rows":[{"kind":"claude","version":"2.1.278","status":"pending","dispatch_gen":0}]}
JSON
bash "$SCRIPT"
FIRST_ID=$(assignment_ids | tail -1)
cat > "$AGM_DIR/show-claude.json" <<'JSON'
{"rows":[{"kind":"claude","version":"2.1.278","status":"pending","dispatch_gen":1}]}
JSON
bash "$SCRIPT"
SECOND_ID=$(assignment_ids | tail -1)
[ -n "$FIRST_ID" ] && [ -n "$SECOND_ID" ] && [ "$FIRST_ID" != "$SECOND_ID" ] \
  && { echo "ok   - stale requeue 用新 request-id（${FIRST_ID} → ${SECOND_ID}）"; PASS=$((PASS + 1)); } \
  || { echo "FAIL - stale requeue 沿用舊 request-id（${FIRST_ID} → ${SECOND_ID}）"; FAIL=$((FAIL + 1)); }
equals "stale requeue 實際重新派一次" "$(assigns)" "2"
teardown

setup
mk_pending claude 2.1.290 2.1.281; mk_empty codex
bash "$SCRIPT"
FIRST_ID=$(assignment_ids | tail -1)
mk_pending claude 2.1.290 2.1.282
bash "$SCRIPT"
SECOND_ID=$(assignment_ids | tail -1)
if [ -n "$FIRST_ID" ] && [ -n "$SECOND_ID" ] && [ "$FIRST_ID" != "$SECOND_ID" ]; then
  echo "ok   - pending 子集不同就使用新 request-id"
  PASS=$((PASS + 1))
else
  echo "FAIL - pending 子集不同就使用新 request-id（${FIRST_ID} → ${SECOND_ID}）"
  FAIL=$((FAIL + 1))
fi
teardown

setup
mk_pending claude 2.1.278 2.1.278; mk_empty codex
export STUB_ASSIGN_FAIL=1 AGM_FAIL_ALERT_AFTER=3
bash "$SCRIPT"; bash "$SCRIPT"
check_no "連續 2 輪還不喊人" "ops-alert" "$AGM_DIR/calls.log"
bash "$SCRIPT"
check "連續 3 輪推 check_failing" "ops-alert.*check_failing" "$AGM_DIR/calls.log"
export STUB_ASSIGN_FAIL=""
bash "$SCRIPT"
[ ! -e "$AGM_DIR/release-triage.fails" ] && { echo "ok   - 完成一輪就清零"; PASS=$((PASS + 1)); } || { echo "FAIL - 完成一輪就清零"; FAIL=$((FAIL + 1)); }
unset AGM_FAIL_ALERT_AFTER
teardown

setup
cp "$HERE/fixtures/herdr-update-runtime.json" "$AGM_DIR/runtime.json"
mk_pending claude 2.1.278 2.1.278; mk_empty codex
bash "$SCRIPT"
check "優先派給 release_bot_id" "\-\-bot bot-release" "$AGM_DIR/calls.log"
teardown
setup
mk_pending claude 2.1.278 2.1.278; mk_empty codex
export AGM_RELEASE_BOT=bot-override
bash "$SCRIPT"
check "env 覆寫優先" "\-\-bot bot-override" "$AGM_DIR/calls.log"
teardown
setup
mk_pending claude 2.1.278 2.1.278; mk_empty codex
rm -f "$AGM_DIR/runtime.json"
bash "$SCRIPT"
check "找不到對象就跳過" "找不到要派給誰" "$AGM_DIR/release-triage.log"
equals "找不到對象不亂派" "$(assigns)" "0"
teardown

setup
mk_pending claude 2.1.278 2.1.278
export AM_BINARY="$ROOT/no-such-binary"
bash "$SCRIPT"
check "binary 不在就跳過" "找不到" "$AGM_DIR/release-triage.log"
equals "binary 不在不派" "$(assigns)" "0"
teardown

setup
mk_pending claude 2.1.278 2.1.277 2.1.278; mk_empty codex
bash "$SCRIPT"
check "dispatched 帶 kind 與兩版" "release-triage dispatched --kind claude --version 2.1.277 --version 2.1.278 --bot [^ ][^ ]*$" "$AGM_DIR/calls.log"
equals "dispatched 只呼叫一次" "$(grep -c 'release-triage dispatched' "$AGM_DIR/calls.log" | tr -d ' ')" "1"
teardown
setup
mk_pending claude 2.1.290 2.1.281 2.1.282 2.1.283 2.1.284 2.1.285 2.1.290; mk_empty codex
bash "$SCRIPT"
check "截斷：dispatched 只帶前 5 版" "dispatched --kind claude --version 2.1.281 --version 2.1.282 --version 2.1.283 --version 2.1.284 --version 2.1.285 --bot [^ ][^ ]*$" "$AGM_DIR/calls.log"
check_no "截斷：第 6 版不標" "version 2.1.290" "$AGM_DIR/calls.log"
teardown
setup
mk_pending claude 2.1.278 2.1.278; mk_pending codex 0.155.0 0.155.0
bash "$SCRIPT"
check "兩個 kind 各標自己的" "dispatched --kind codex --version 0.155.0 --bot [^ ][^ ]*$" "$AGM_DIR/calls.log"
teardown
setup
mk_pending claude 2.1.278 2.1.278; mk_empty codex
export STUB_ASSIGN_FAIL=1
bash "$SCRIPT"
check_no "assign 失敗：不標 dispatched" "dispatched" "$AGM_DIR/calls.log"
teardown
setup
mk_empty claude; mk_empty codex
bash "$SCRIPT"
check_no "沒 pending：不標 dispatched" "dispatched" "$AGM_DIR/calls.log"
teardown
setup
mk_pending claude 2.1.278 2.1.278; mk_empty codex
export STUB_DISPATCHED_FAIL=1
bash "$SCRIPT"; equals "dispatched 失敗：exit 0" "$?" "0"
equals "dispatched 失敗：不重派" "$(assigns)" "1"
check "dispatched 失敗有記 log" "標記 dispatched 失敗" "$AGM_DIR/release-triage.log"
teardown

setup
mk_empty claude; mk_empty codex
bash "$SCRIPT"
check "publish claude" "release-triage publish --kind claude" "$AGM_DIR/calls.log"
check "publish codex" "release-triage publish --kind codex" "$AGM_DIR/calls.log"
check "publish agy" "release-triage publish --kind agy" "$AGM_DIR/calls.log"
check "publish grok" "release-triage publish --kind grok" "$AGM_DIR/calls.log"
equals "publish 共四次" "$(grep -c 'release-triage publish' "$AGM_DIR/calls.log" | tr -d ' ')" "4"
equals "無事：不寫 log" "$(cat "$AGM_DIR/release-triage.log" 2>/dev/null)" ""
teardown
setup
mk_pending claude 2.1.278 2.1.278; mk_empty codex
export STUB_QUOTA_JSON='{"kinds":{"claude":{"five_hour":{"used_pct":95.0},"limit_hit":null}}}'
bash "$SCRIPT"
equals "額度擋下時 publish 仍照跑" "$(grep -c 'release-triage publish' "$AGM_DIR/calls.log" | tr -d ' ')" "4"
teardown
setup
mk_empty claude; mk_empty codex
export STUB_PUBLISH_JSON='{"publish_enabled":false,"results":[{"kind":"claude","version":"2.1.1","result":{"outcome":"disabled"}}]}'
bash "$SCRIPT"
equals "publish disabled：不寫 log" "$(cat "$AGM_DIR/release-triage.log" 2>/dev/null)" ""
teardown
setup
mk_empty claude; mk_empty codex
export STUB_PUBLISH_JSON='{"publish_enabled":true,"results":[{"kind":"claude","version":"2.1.1","result":{"outcome":"published"}}]}'
bash "$SCRIPT"
check "publish 成功有記 log" "2.1.1：publish published" "$AGM_DIR/release-triage.log"
teardown

setup
mk_pending claude 2.1.278 2.1.278; mk_empty codex
export STUB_OLD_AGM=1
bash "$SCRIPT"; equals "舊 agm：exit 0" "$?" "0"
equals "舊 agm：無法讀帳本時不派" "$(assigns)" "0"
check "舊 agm：記錄帳本讀取失敗" "查不到 API 模式" "$AGM_DIR/release-triage.log"
bash "$SCRIPT"; bash "$SCRIPT"
equals "舊 agm：說明只寫一次" "$(grep -c '不認得 release-triage' "$AGM_DIR/release-triage.log" | tr -d ' ')" "1"
teardown

setup
chmod -x "$AGM_DIR/bin/agm"
bash "$SCRIPT"; RC=$?
equals "agm 不在：exit 0" "$RC" "0"
check  "agm 不在：留一行 log" "agm CLI 不在" "$AGM_DIR/release-triage.log"
teardown

setup
mk_empty claude; mk_empty codex
mkdir -p "$ROOT/claude-versions/2.1.278"
bash "$SCRIPT"
equals "第一次只記下目前 Claude binary baseline" "$(cat "$AGM_DIR/claude-release.last")" "2.1.278"
equals "第一次不因現有 binary 派工" "$(assigns)" "0"
teardown

setup
mk_pending claude 2.1.279 2.1.279; mk_empty codex
mkdir -p "$ROOT/claude-versions/2.1.278"
sleep 1
mkdir -p "$ROOT/claude-versions/2.1.279"
echo 2.1.278 > "$AGM_DIR/claude-release.last"
echo 'CLAUDE_BINARY_DIFF_MARKER' > "$AGM_DIR/claude-release-diff-task.md"
export CLAUDE_VERSIONS_DIR="$ROOT/claude-versions"
bash "$SCRIPT"
equals "binary diff 與 changelog 合併成一則派工" "$(assigns)" "1"
check "合併交辦有 changelog 條目" "kept 2.1.279 0" "$AGM_DIR/assign-body.txt"
check "合併交辦有 binary diff 任務" "CLAUDE_BINARY_DIFF_MARKER" "$AGM_DIR/assign-body.txt"
check "合併交辦有 binary 舊版與新版路徑" "OLD=$ROOT/claude-versions/2.1.278" "$AGM_DIR/assign-body.txt"
check "合併交辦有 binary 新版路徑" "NEW=$ROOT/claude-versions/2.1.279" "$AGM_DIR/assign-body.txt"
check "合併交辦使用 changelog notice id" "通知 request-id（child 需原樣使用）：agm-release-triage-claude-2.1.279-notice" "$AGM_DIR/assign-body.txt"
check_no "合併後不再指示第二則 binary 公告" "agm-claude-release-<新版號>-notice" "$AGM_DIR/assign-body.txt"
teardown

setup
mk_empty claude; mk_empty codex
mkdir -p "$ROOT/claude-versions/2.1.278"
sleep 1
mkdir -p "$ROOT/claude-versions/2.1.279"
echo 2.1.278 > "$AGM_DIR/claude-release.last"
echo 'CLAUDE_BINARY_DIFF_MARKER' > "$AGM_DIR/claude-release-diff-task.md"
export CLAUDE_VERSIONS_DIR="$ROOT/claude-versions"
/bin/bash "$SCRIPT" 2>"$ROOT/binary-only.err"
check_no "binary-only 的空 VARGS 不因 Bash 3.2 報錯" "unbound variable" "$ROOT/binary-only.err"
equals "無 changelog pending 時仍派 binary-only" "$(assigns)" "1"
check "binary-only request id 有版本" "release-triage-claude-binary-2.1.279" "$AGM_DIR/calls.log"
check "binary-only body 說明沒有 changelog pending" "本輪沒有待分診的 changelog 版本" "$AGM_DIR/assign-body.txt"
check "binary-only 用專屬 notice id 避免撞上先前 changelog 通知" "通知 request-id（child 需原樣使用）：agm-claude-release-2.1.279-notice" "$AGM_DIR/assign-body.txt"
check_no "binary-only 不重用 changelog notice id" "通知 request-id（child 需原樣使用）：agm-release-triage-claude-2.1.279-notice" "$AGM_DIR/assign-body.txt"
teardown

setup
mk_pending claude 2.1.279 2.1.279; mk_empty codex
mkdir -p "$ROOT/claude-versions/2.1.278"
sleep 1
mkdir -p "$ROOT/claude-versions/2.1.279"
echo 2.1.278 > "$AGM_DIR/claude-release.last"
echo 'CLAUDE_BINARY_DIFF_MARKER' > "$AGM_DIR/claude-release-diff-task.md"
export STUB_DISPATCHED_FAIL=1
bash "$SCRIPT"
equals "標記 dispatched 失敗時不推進 binary state" "$(cat "$AGM_DIR/claude-release.last")" "2.1.278"
export STUB_DISPATCHED_FAIL=""
bash "$SCRIPT"
equals "重試仍用同一 request id" "$(grep -c 'release-triage-claude-2.1.279' "$AGM_DIR/calls.log" | tr -d ' ')" "2"
equals "同一份合併正文兩次都帶 binary diff" "$(grep -c 'CLAUDE_BINARY_DIFF_MARKER' "$AGM_DIR/assign-body.txt" | tr -d ' ')" "2"
equals "dispatched 成功後才推進 binary state" "$(cat "$AGM_DIR/claude-release.last")" "2.1.279"
teardown

setup
mk_empty claude; mk_empty codex
mkdir -p "$ROOT/claude-versions/2.1.278"
bash "$SCRIPT"
sleep 1
mkdir -p "$ROOT/claude-versions/2.1.279"
echo 'CLAUDE_BINARY_DIFF_MARKER' > "$AGM_DIR/claude-release-diff-task.md"
export STUB_ASSIGN_FAIL=1
bash "$SCRIPT"
equals "派工失敗時 binary state 不前進" "$(cat "$AGM_DIR/claude-release.last")" "2.1.278"
export STUB_ASSIGN_FAIL=""
bash "$SCRIPT"
equals "下一輪 binary diff 重試成功後才前進" "$(cat "$AGM_DIR/claude-release.last")" "2.1.279"
equals "binary diff assign 失敗後重試" "$(grep -c 'release-triage-claude-binary-2.1.279' "$AGM_DIR/calls.log" | tr -d ' ')" "2"
teardown

setup
mk_empty claude; mk_empty codex
cp "$HERE/release-issue-task.md" "$AGM_DIR/release-issue-task.md"
NOW=$(date -u +%Y-%m-%dT%H:%M:%SZ)
cat > "$AGM_DIR/show-claude.json" <<JSON
{"rows":[{"kind":"claude","version":"2.1.288","issues":[
  {"number":901,"url":"https://x/901","created_at":"$NOW","comment":false},
  {"number":902,"url":"https://x/902","created_at":"$NOW","comment":true},
  {"number":903,"url":"https://x/903","created_at":"2020-01-01T00:00:00Z","comment":false}]}]}
JSON
export STUB_ASSIGN_FAIL=1
bash "$SCRIPT"
equals "交辦失敗不記成已交" "$(cat "$AGM_DIR/release-issue-handed" 2>/dev/null)" ""
export STUB_ASSIGN_FAIL=""
bash "$SCRIPT"
equals "新開的 issue 交給分診 bot" "$(grep -c 'request-id release-issue-901' "$AGM_DIR/calls.log" | tr -d ' ')" "2"
equals "只在既有 issue 留言的不交" "$(grep -c 'release-issue-902' "$AGM_DIR/calls.log" | tr -d ' ')" "0"
equals "停機超過 14 天仍補交舊 issue" "$(grep -c 'release-issue-903' "$AGM_DIR/calls.log" | tr -d ' ')" "2"
equals "新舊 issue 都記在持久交接狀態" "$(paste -sd , "$AGM_DIR/release-issue-handed")" "901,903"
check "交辦正文帶 issue 編號" "issue #901 https://x/901" "$AGM_DIR/assign-body.txt"
check "交辦正文是接手 issue 的那份任務" "請接手把它做完" "$AGM_DIR/assign-body.txt"
ISSUE_TASK="$HERE/release-issue-task.md"
check "issue 的標題、內文與留言都明確視為外部資料" "GitHub issue 的標題、內文與留言（包括" "$AGM_DIR/assign-body.txt"
check "issue 需求要先對照派工與 repo 規範" "以本次派工、上游來源與 repo 現況核對需求" "$AGM_DIR/assign-body.txt"
check "交接要求 child 使用自己的分支" "推自己的分支" "$AGM_DIR/assign-body.txt"
check_no "交接不得指示 child 推 main" "push main" "$AGM_DIR/assign-body.txt"
grep -q -- "--bot bot-resp .*release-issue-901\|release-issue-901.*--bot bot-resp\|--bot bot-release .*release-issue-901" "$AGM_DIR/calls.log" && PASS=$((PASS+1)) || { FAIL=$((FAIL+1)); echo "FAIL: 交給分診 bot"; grep release-issue "$AGM_DIR/calls.log"; }
bash "$SCRIPT"
equals "交過的下一輪不再送" "$(grep -c 'release-issue-901' "$AGM_DIR/calls.log" | tr -d ' ')" "2"
equals "舊 issue 補交後也不再送" "$(grep -c 'release-issue-903' "$AGM_DIR/calls.log" | tr -d ' ')" "2"
rm "$AGM_DIR/release-issue-handed"
bash "$SCRIPT"
equals "交接檔遺失沿用相同 request-id 補送" "$(grep -c 'request-id release-issue-903$' "$AGM_DIR/calls.log" | tr -d ' ')" "3"
equals "補送成功恢復持久交接記錄" "$(paste -sd , "$AGM_DIR/release-issue-handed")" "901,903"
teardown

setup
mk_empty claude; mk_empty codex
cp "$HERE/release-issue-task.md" "$AGM_DIR/release-issue-task.md"
NOW=$(date -u +%Y-%m-%dT%H:%M:%SZ)
cat > "$AGM_DIR/show-claude.json" <<JSON
{"rows":[{"kind":"claude","version":"2.1.288","issues":[
  {"number":904,"url":"https://x/904","created_at":"2026-10-02T12:00:00","comment":false},
  {"number":905,"url":"https://x/905","created_at":"$NOW","comment":false},
  {"number":907,"url":"https://x/907","comment":false}]}]}
JSON
bash "$SCRIPT"
equals "無時區時間戳也不漏交" "$(grep -c 'request-id release-issue-904' "$AGM_DIR/calls.log" | tr -d ' ')" "1"
equals "naive 時間戳不阻斷後續 issue 補送" "$(grep -c 'request-id release-issue-905' "$AGM_DIR/calls.log" | tr -d ' ')" "1"
equals "缺建立時間也補送" "$(grep -c 'request-id release-issue-907' "$AGM_DIR/calls.log" | tr -d ' ')" "1"
equals "時間戳不再影響交接成敗" "$(cat "$AGM_DIR/release-triage.fails" 2>/dev/null)" ""
teardown

setup
mk_empty claude; mk_empty codex
cp "$HERE/release-issue-task.md" "$AGM_DIR/release-issue-task.md"
NOW=$(date -u +%Y-%m-%dT%H:%M:%SZ)
cat > "$AGM_DIR/show-claude.json" <<JSON
{"rows":[{"kind":"claude","version":"2.1.288","issues":[
  {"number":906,"url":"https://x/906","created_at":"$NOW","comment":false}]}]}
JSON
printf '{}\n' > "$AGM_DIR/runtime.json"
bash "$SCRIPT"
equals "收件 bot 遺失：不派也不記已交" "$(cat "$AGM_DIR/release-issue-handed" 2>/dev/null)" ""
equals "收件 bot 遺失：狀態記一輪失敗" "$(cat "$AGM_DIR/release-triage.fails" 2>/dev/null)" "1"
cp "$HERE/fixtures/patrol-runtime.json" "$AGM_DIR/runtime.json"
rm -f "$AGM_DIR/release-issue-task.md"
bash "$SCRIPT"
equals "交接任務檔遺失：狀態累計失敗" "$(cat "$AGM_DIR/release-triage.fails" 2>/dev/null)" "2"
cp "$HERE/release-issue-task.md" "$AGM_DIR/release-issue-task.md"
bash "$SCRIPT"
equals "補回前置條件後 issue 仍能交接" "$(grep -c 'request-id release-issue-906' "$AGM_DIR/calls.log" | tr -d ' ')" "1"
equals "交接成功清除失敗狀態" "$(cat "$AGM_DIR/release-triage.fails" 2>/dev/null)" ""
equals "交接成功記錄 issue" "$(cat "$AGM_DIR/release-issue-handed" 2>/dev/null)" "906"
teardown

setup
mk_empty claude; mk_empty codex
cp "$HERE/release-issue-task.md" "$AGM_DIR/release-issue-task.md"
NOW=$(date -u +%Y-%m-%dT%H:%M:%SZ)
cat > "$AGM_DIR/show-claude.json" <<JSON
{"rows":[{"kind":"claude","version":"2.1.288","issues":[
  {"number":907,"url":"https://x/907","created_at":"$NOW","comment":false}]}]}
JSON
mkdir "$AGM_DIR/release-issue-handed"
bash "$SCRIPT" 2>"$ROOT/state-write.err"
equals "狀態檔不可寫：assignment 只試一次" "$(grep -c 'request-id release-issue-907' "$AGM_DIR/calls.log" | tr -d ' ')" "1"
equals "狀態檔不可寫：記成失敗待重試" "$(cat "$AGM_DIR/release-triage.fails" 2>/dev/null)" "1"
check_no "狀態檔不可寫：不宣稱已交接" "issue #907 已交給" "$AGM_DIR/release-triage.log"
rmdir "$AGM_DIR/release-issue-handed"
bash "$SCRIPT"
equals "狀態檔恢復後沿用同 request id 重試" "$(grep -c 'request-id release-issue-907' "$AGM_DIR/calls.log" | tr -d ' ')" "2"
equals "狀態檔恢復後記錄 issue" "$(cat "$AGM_DIR/release-issue-handed")" "907"
equals "交接成功清掉狀態檔失敗" "$(cat "$AGM_DIR/release-triage.fails" 2>/dev/null)" ""
teardown

TRIAGE_TASK="$HERE/release-triage-task.md"
CLAUDE_TASK="$HERE/claude-release-diff-task.md"
check "一般／合併分診公告 id 帶管線與 kind" "agm-release-triage-<kind>-<新版號>-notice" "$TRIAGE_TASK"
check "binary-only 使用舊版獨立公告 id 命名空間" "agm-claude-release-<binary 版本>-notice" "$TRIAGE_TASK"
check "binary diff supplement 不另送通知" "不要另送通知" "$CLAUDE_TASK"
check "binary-only kick 輸出獨立公告 id" 'NOTICE_ID="agm-claude-release-${TO}-notice"' "$SCRIPT"
check "正常 kick 輸出分診公告 id" 'NOTICE_ID="agm-release-triage-${KIND}-${TO}-notice"' "$SCRIPT"
check "kick 的 changelog 派工 request-id 含 kind、to 與代號指紋" 'prefix = f"release-triage-{kind}-{to}"' "$SCRIPT"

setup
mk_pending claude 2.1.278 2.1.278; mk_empty codex
mkdir -p "$ROOT/bin" "$AGM_DIR/release-triage.lock"
echo '999999 1' > "$AGM_DIR/release-triage.lock/owner"
cat > "$ROOT/bin/rm" <<'STUB'
#!/bin/bash
is_lock=0
echo "rm role=${RACE_ROLE:-} agm_dir=${AGM_DIR:-} args=$*" >> "${AGM_DIR:-/tmp}/rm-stub.log" 2>/dev/null
for arg in "$@"; do [ "$arg" = "$AGM_DIR/release-triage.lock" ] && is_lock=1; done
if [ "$is_lock" = 1 ] && [ "${RACE_ROLE:-}" = a ] && [ ! -e "$AGM_DIR/race-a-entered" ]; then
  : > "$AGM_DIR/race-a-entered"
  _wait=0
  while [ ! -e "$AGM_DIR/race-b-entered" ] && [ "$_wait" -lt 250 ]; do sleep 0.02; _wait=$((_wait + 1)); done
elif [ "$is_lock" = 1 ] && [ "${RACE_ROLE:-}" = b ] && [ ! -e "$AGM_DIR/race-b-entered" ]; then
  : > "$AGM_DIR/race-b-entered"
  _wait=0
  while { [ ! -s "$AGM_DIR/release-triage.lock/owner" ] || grep -q '^999999 ' "$AGM_DIR/release-triage.lock/owner"; } && [ "$_wait" -lt 500 ]; do
    sleep 0.02; _wait=$((_wait + 1))
  done
fi
exec /bin/rm "$@"
STUB
chmod +x "$ROOT/bin/rm"
export AGM_EXTRA_PATH="$ROOT/bin" AGM_LOCK_STALE_SECS=0 STUB_PUBLISH_SLEEP=2
_saved_rm=$(declare -f rm 2>/dev/null || true)
export ROOT
rm() { "$ROOT/bin/rm" "$@"; }
export -f rm
RACE_ROLE=a bash "$SCRIPT" > "$ROOT/race-a.log" 2>&1 & RACE_A=$!
: > "$AGM_DIR/race-a-started"
_wait=0
while [ ! -e "$AGM_DIR/race-a-entered" ] && [ "$_wait" -lt 100 ]; do sleep 0.02; _wait=$((_wait + 1)); done
: > "$AGM_DIR/race-b-started"
RACE_ROLE=b bash "$SCRIPT" > "$ROOT/race-b.log" 2>&1 & RACE_B=$!
wait "$RACE_A"; wait "$RACE_B"
unset -f rm
if [ -n "$_saved_rm" ]; then eval "$_saved_rm"; export -f rm; fi
equals "兩個排程都啟動" "1" "$([ -e "$AGM_DIR/race-a-started" ] && [ -e "$AGM_DIR/race-b-started" ] && echo 1 || echo 0)"
equals "兩輪同時回收 stale lock 只派一次" "1" "$(assigns)"
if [ "$(assigns)" != "1" ]; then
  for f in "$ROOT/race-a.log" "$ROOT/race-b.log" "$AGM_DIR/calls.log" "$AGM_DIR/release-triage.log" "$AGM_DIR/rm-stub.log"; do echo "--- $f"; sed 's/^/    /' "$f" 2>/dev/null | head -20; done
  ls "$AGM_DIR" | sed 's/^/    ls: /'
  echo "--- probe: PATH 開頭 / type -a rm / bash 版本"
  ( export PATH="${AGM_EXTRA_PATH-}:$PATH"; echo "    PATH=${PATH%%:*}"; type -a rm 2>&1 | sed 's/^/    /'; bash --version | head -1 | sed 's/^/    /'; ls -l "$ROOT/bin/rm" 2>&1 | sed 's/^/    /'; "$ROOT/bin/rm" -f "$ROOT/nonexistent" 2>&1 | sed 's/^/    run: /'; ls "$AGM_DIR/rm-stub.log" 2>&1 | sed 's/^/    /' )
fi
teardown

setup
mk_pending claude 2.1.278 2.1.277 2.1.278; mk_empty codex
export STUB_AUTO_ENABLED=true STUB_AUTO_JSON='{"results":[{"version":"2.1.277","outcome":"judged","status":"judged","quiet":false},{"version":"2.1.278","outcome":"empty","status":"empty","quiet":false}],"binary_used":false}'
bash "$SCRIPT"
equals "API 全包：不派協調者" "$(assigns)" "0"
check_no "API 全包：不查額度" " quota" "$AGM_DIR/calls.log"
check_no "API 全包：不標 dispatched" " dispatched " "$AGM_DIR/calls.log"
check "API 分診有記 log" "API 分診完成 2.1.277" "$AGM_DIR/release-triage.log"
check "auto 帶兩個版本" "release-triage auto --kind claude --version 2.1.277 --version 2.1.278" "$AGM_DIR/calls.log"
teardown

setup
mk_pending claude 2.1.278 2.1.278; mk_empty codex
bash "$SCRIPT"
equals "沒開：照派協調者" "$(assigns)" "1"
check_no "沒開：不呼叫 auto 本體" "auto --kind claude --version" "$AGM_DIR/calls.log"
check_no "沒開：log 不吵" "API 分診" "$AGM_DIR/release-triage.log"
teardown

setup
mk_pending claude 2.1.278 2.1.277 2.1.278; mk_empty codex
export STUB_AUTO_ENABLED=true STUB_AUTO_JSON='{"results":[{"version":"2.1.277","outcome":"judged","status":"judged","quiet":false},{"version":"2.1.278","outcome":"failed","reason":"驗證不過：缺 verdict","quiet":false}]}'
bash "$SCRIPT"
equals "API 部分失敗：不派 bot" "$(assigns)" "0"
check_no "API 失敗不標 dispatched" "dispatched --kind" "$AGM_DIR/calls.log"
check "失敗原因進 log" "API 分診失敗 2.1.278：驗證不過：缺 verdict" "$AGM_DIR/release-triage.log"
teardown

setup
mk_pending claude 2.1.278 2.1.278; mk_empty codex
export STUB_AUTO_ENABLED=true STUB_AUTO_FAIL=1
bash "$SCRIPT"
equals "auto 失敗：不派協調者" "$(assigns)" "0"
check "auto 失敗有記 log" "release-triage auto --kind claude 失敗" "$AGM_DIR/release-triage.log"
teardown

setup
mk_pending claude 2.1.279 2.1.279; mk_empty codex
mkdir -p "$ROOT/claude-versions/2.1.278"; sleep 1; mkdir -p "$ROOT/claude-versions/2.1.279"
echo 2.1.278 > "$AGM_DIR/claude-release.last"
echo 'CLAUDE_BINARY_DIFF_MARKER' > "$AGM_DIR/claude-release-diff-task.md"
printf '#!/usr/bin/env python3\nprint("FAKE-BINARY-DIFF-REPORT", __import__("sys").argv[1:3])\n' > "$AGM_DIR/bin/claude_binary_diff.py"
export STUB_AUTO_ENABLED=true STUB_AUTO_JSON='{"results":[{"version":"2.1.279","outcome":"judged","status":"judged","quiet":false}],"binary_used":true}'
bash "$SCRIPT"
equals "API 併進 binary：不派協調者" "$(assigns)" "0"
check "報告檔交給 auto" "FAKE-BINARY-DIFF-REPORT" "$AGM_DIR/auto-diff.txt"
check "報告用 OLD／NEW 兩個路徑" "2.1.278" "$AGM_DIR/auto-diff.txt"
equals "binary_used 後推進 binary state" "$(cat "$AGM_DIR/claude-release.last")" "2.1.279"
teardown

setup
mk_pending claude 2.1.279 2.1.279; mk_empty codex
mkdir -p "$ROOT/claude-versions/2.1.278"; sleep 1; mkdir -p "$ROOT/claude-versions/2.1.279"
echo 2.1.278 > "$AGM_DIR/claude-release.last"
echo 'CLAUDE_BINARY_DIFF_MARKER' > "$AGM_DIR/claude-release-diff-task.md"
export STUB_AUTO_ENABLED=true STUB_AUTO_JSON='{"results":[{"version":"2.1.279","outcome":"judged","status":"judged","quiet":false}],"binary_used":false}'
bash "$SCRIPT"
equals "API 模式 binary 沒進分析：不派 bot" "$(assigns)" "0"
check_no "不標 dispatched" "dispatched --kind" "$AGM_DIR/calls.log"
check_no "不查 bot 額度" " quota" "$AGM_DIR/calls.log"
teardown

setup
cp "$HERE/fixtures/herdr-update-runtime.json" "$AGM_DIR/runtime.json"
rm "$AGM_DIR/release-triage-task.md"
mk_pending claude 2.1.278 2.1.278; mk_empty codex
cp "$HERE/release-issue-task.md" "$AGM_DIR/release-issue-task.md"
NOW=$(date -u +%Y-%m-%dT%H:%M:%SZ)
printf '{"rows":[{"kind":"claude","version":"2.1.278","issues":[{"number":991,"url":"https://x/991","created_at":"%s","comment":false}]}]}' "$NOW" > "$AGM_DIR/auto-published.json"
export STUB_AUTO_ENABLED=true STUB_AUTO_JSON='{"results":[{"version":"2.1.278","outcome":"judged","status":"published","quiet":false}]}'
bash "$SCRIPT"
equals "API 新票同一輪交辦實作" "$(assigns)" "1"
check "實作交給協調者而非分診 child" "assign --bot bot-resp .*request-id release-issue-991" "$AGM_DIR/calls.log"
check_no "實作不派給 release_bot_id child" "assign --bot bot-release" "$AGM_DIR/calls.log"
check "API 新票用實作 request-id" "request-id release-issue-991" "$AGM_DIR/calls.log"
check "交接正文要求 child 實作" "照既有派工流程交給 child" "$AGM_DIR/assign-body.txt"
check_no "API 模式不派 AGM 分析" "request-id release-triage-" "$AGM_DIR/calls.log"
check_no "API 模式不走 bot verdict" " dispatched " "$AGM_DIR/calls.log"
bash "$SCRIPT"
equals "已交接 issue 重跑不重派實作" "$(assigns)" "1"
teardown

setup
mk_pending claude 2.1.278 2.1.278; mk_empty codex
cp "$HERE/release-issue-task.md" "$AGM_DIR/release-issue-task.md"
python3 - "$AGM_DIR" <<'PYFIX'
import json,sys,pathlib
d=pathlib.Path(sys.argv[1])
def rows(kind, nums):
    return {"rows":[{"kind":kind,"version":"2.1.278","issues":[{"number":n,"url":"https://x/%s"%n,"comment":False} for n in nums]}]}
(d/"show-claude.json").write_text(json.dumps(rows("claude", [1000,1001,1002,1003,1004])))
(d/"show-codex.json").write_text(json.dumps(rows("codex", [1005])))
(d/"auto-published.json").write_text(json.dumps(rows("claude", [1000,1001,1002,1003,1004,1006,1007])))
PYFIX
echo CLOSED > "$AGM_DIR/gh-state-1000"
export STUB_AUTO_ENABLED=true STUB_AUTO_JSON='{"results":[{"version":"2.1.278","outcome":"judged","status":"published","quiet":false}]}'
bash "$SCRIPT"
equals "關閉票持久略過" "$(cat "$AGM_DIR/release-issue-skipped")" "1000"
equals "所有 kind 與同輪第二次掃描交接全部七張" "$(assigns)" "7"
check "同輪交接新 API issue" "request-id release-issue-1006" "$AGM_DIR/calls.log"
bash "$SCRIPT"
equals "下一輪不重送已交接的七張" "$(assigns)" "7"
equals "跨前後掃描與下輪仍不重查關閉票" "$(grep -c 'issue view https://x/1000 ' "$AGM_DIR/gh-calls.log")" "1"
check "已送 1006" "request-id release-issue-1006" "$AGM_DIR/calls.log"
teardown

setup
mk_empty claude; mk_empty codex
cp "$HERE/release-issue-task.md" "$AGM_DIR/release-issue-task.md"
python3 - "$AGM_DIR/show-claude.json" <<'PYFIX'
import json,sys
json.dump({"rows":[{"kind":"claude","version":"2.1.278","issues":[{"number":n,"url":"https://x/%s"%n} for n in range(1101,1108)]}]},open(sys.argv[1],"w"))
PYFIX
export STUB_ASSIGN_FAIL=1
bash "$SCRIPT"
equals "交辦失敗仍嘗試全部七張" "$(assigns)" "7"
equals "失敗的票不標記已交" "$(cat "$AGM_DIR/release-issue-handed" 2>/dev/null)" ""
teardown

setup
mk_empty claude; mk_empty codex
cp "$HERE/release-issue-task.md" "$AGM_DIR/release-issue-task.md"
printf '%s' '{"repo":"o/r","rows":[{"kind":"claude","version":"2.1.278","issues":[{"number":1201,"url":"https://x/1201"},{"number":1202,"url":"https://x/1202"},{"number":1203},{"number":1204,"url":"https://x/1204"}]}]}' > "$AGM_DIR/show-claude.json"
echo CLOSED > "$AGM_DIR/gh-state-1201"
touch "$AGM_DIR/gh-fail-1202"
echo UNKNOWN > "$AGM_DIR/gh-state-1204"
bash "$SCRIPT"
check_no "未知狀態不交接" "request-id release-issue-1204" "$AGM_DIR/calls.log"
equals "已關閉 issue 持久略過" "$(cat "$AGM_DIR/release-issue-skipped")" "1201"
check_no "已關閉 issue 不交接" "request-id release-issue-1201" "$AGM_DIR/calls.log"
check_no "查詢失敗不交接" "request-id release-issue-1202" "$AGM_DIR/calls.log"
check "缺 URL 使用 repo 建立查詢網址" "issue view https://github.com/o/r/issues/1203" "$AGM_DIR/gh-calls.log"
rm "$AGM_DIR/gh-fail-1202"
echo OPEN > "$AGM_DIR/gh-state-1204"
bash "$SCRIPT"
check "未知狀態恢復 OPEN 後補送" "request-id release-issue-1204" "$AGM_DIR/calls.log"
equals "已關閉 issue 下輪不再查詢" "$(grep -c 'issue view https://x/1201 ' "$AGM_DIR/gh-calls.log")" "1"
check "查詢恢復後補送" "request-id release-issue-1202" "$AGM_DIR/calls.log"
teardown

setup
mk_empty claude; mk_empty codex
cp "$HERE/release-issue-task.md" "$AGM_DIR/release-issue-task.md"
printf '%s' '{"rows":[{"kind":"claude","version":"2.1.278","issues":[{"number":1301,"url":"https://x/1301"}]}]}' > "$AGM_DIR/show-claude.json"
echo CLOSED > "$AGM_DIR/gh-state-1301"
mkdir "$AGM_DIR/release-issue-skipped"
bash "$SCRIPT"
equals "略過紀錄寫入失敗仍不交接" "$(assigns)" "0"
check "略過紀錄寫入失敗有原因" "略過紀錄寫入失敗" "$AGM_DIR/release-triage.log"
rmdir "$AGM_DIR/release-issue-skipped"
bash "$SCRIPT"
equals "恢復寫入後補上略過紀錄" "$(cat "$AGM_DIR/release-issue-skipped")" "1301"
teardown

echo "$PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
