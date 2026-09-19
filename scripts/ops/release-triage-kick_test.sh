#!/bin/bash
# release-triage-kick.sh 的隔離測試：自己的 AGM 目錄、假的 `bin/agm`／`agents-managerd`，完全不碰正式 AGM、
# daemon 或 gh。測的是決策：什麼時候派、派幾次、派給誰、額度閘門、鎖回收、最小 PATH。
# `release-triage-check` 本身（版本比較、切條、分桶）由 daemon 的 cargo test 釘住，這裡的假二進位只吐固定 JSON。
#
#   bash scripts/ops/release-triage-kick_test.sh
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
SCRIPT="$HERE/release-triage-kick.sh"
PASS=0
FAIL=0

setup() {
  ROOT=$(mktemp -d)
  export AGM_DIR="$ROOT/agm" AGM_REPO="$ROOT/repo"
  mkdir -p "$AGM_DIR/bin" "$ROOT/triage"
  cp "$HERE/fixtures/patrol-runtime.json" "$AGM_DIR/runtime.json"
  echo "TASK-BODY-MARKER 交辦正文" > "$AGM_DIR/release-triage-task.md"

  # 假 agm：assign／quota／state／ops-alert；未知旗標一律 exit 2（2026-09-16 `--request-id` 拼錯事故的教訓）。
  cat > "$AGM_DIR/bin/agm" <<'STUB'
#!/bin/bash
echo "$*" >> "$AGM_DIR/calls.log"
DEFAULT_QUOTA='{"kinds":{"claude":{"five_hour":{"used_pct":10.0},"limit_hit":null}}}'
for a in "$@"; do
  case "$a" in
    --compact|--bot|--review-by|--text-file|--request-id|--source|--reason|--detail|--kind|--version|assign|quota|state|ops-alert|release-triage|dispatched|publish) ;;
    --*) echo "agm: error: unrecognized arguments: $a" >&2; exit 2 ;;
  esac
done
case "$*" in
  *"release-triage "*)
    [ -n "${STUB_OLD_AGM:-}" ] && { echo "agm: error: argument cmd: invalid choice: 'release-triage'" >&2; exit 2; }
    case "$*" in
      *" dispatched "*) [ -n "${STUB_DISPATCHED_FAIL:-}" ] && exit 1; printf '%s' '{"dispatched":1}' ;;
      *" publish "*) printf '%s' "${STUB_PUBLISH_JSON:-{\"publish_enabled\":false,\"results\":[]\}}" ;;
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

  # 假 agents-managerd：只認 `release-triage-check --kind <k> --json`，吐 $ROOT/triage/<k>.json；
  # 該檔不存在＝這個 kind 檢查失敗（exit 1）。
  cat > "$ROOT/fake-agents-managerd" <<'PYEOF'
#!/bin/bash
[ "${1:-}" = "release-triage-check" ] && [ "${2:-}" = "--kind" ] && [ "${4:-}" = "--json" ] || exit 2
echo "check $3" >> "$AGM_DIR/check.log"
f="$(dirname "$0")/triage/$3.json"
[ -f "$f" ] || { echo "boom" >&2; exit 1; }
cat "$f"
PYEOF
  chmod +x "$ROOT/fake-agents-managerd"
  export AM_BINARY="$ROOT/fake-agents-managerd"
  : > "$AGM_DIR/calls.log"; : > "$AGM_DIR/assign-body.txt"
  export STUB_ASSIGN_FAIL="" STUB_QUOTA_FAIL="" STUB_QUOTA_JSON="" STUB_DISPATCHED_FAIL="" STUB_PUBLISH_JSON="" STUB_OLD_AGM=""
  unset AGM_RELEASE_BOT
}
teardown() {
  rm -rf "$ROOT"
  unset AGM_DIR AGM_REPO AM_BINARY AGM_RELEASE_BOT AGM_TRIAGE_QUOTA_MAX AGM_LOCK_STALE_SECS AGM_LOCK_HUNG_SECS AGM_EXTRA_PATH
  unset STUB_ASSIGN_FAIL STUB_QUOTA_FAIL STUB_QUOTA_JSON STUB_DISPATCHED_FAIL STUB_PUBLISH_JSON STUB_OLD_AGM
}

# mk_pending <kind> <to> <version…>：每版 2 條 kept、1 條 unmatched。
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
PY
}
mk_empty() { printf '{"kind":"%s","from":"1.0.0","to":"1.0.0","pending":[]}' "$1" > "$ROOT/triage/$1.json"; }

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
assigns() { grep -c ' assign ' "$AGM_DIR/calls.log" 2>/dev/null | tr -d ' '; }

# 1. 兩個 kind 的 pending 都是空的：不派、不寫 log。
setup
mk_empty claude; mk_empty codex
bash "$SCRIPT"
equals "pending 空：不派" "$(assigns)" "0"
equals "pending 空：不寫 log" "$(cat "$AGM_DIR/release-triage.log" 2>/dev/null)" ""
equals "pending 空：兩個 kind 都檢查過" "$(wc -l < "$AGM_DIR/check.log" | tr -d ' ')" "2"
check_no "pending 空：連額度都不查" "quota" "$AGM_DIR/calls.log"
teardown

# 2. 有 pending：派一次，request-id 正確，交辦帶正文與 JSON，派給協調者、不派給巡檢自己。
setup
mk_pending claude 2.1.278 2.1.277 2.1.278; mk_empty codex
bash "$SCRIPT"
equals "派一次" "$(assigns)" "1"
check "旗標是 --request-id 且帶 kind＋to" "\-\-request-id release-triage-claude-2.1.278" "$AGM_DIR/calls.log"
check "派給協調者" "\-\-bot bot-resp" "$AGM_DIR/calls.log"
check_no "不派給巡檢自己（daemon 會 400）" "\-\-bot bot-agm" "$AGM_DIR/calls.log"
check "交辦給巡檢驗收" "\-\-review-by patrol" "$AGM_DIR/calls.log"
check "交辦帶正文" "TASK-BODY-MARKER" "$AGM_DIR/assign-body.txt"
check "交辦帶條目 id" "2.1.277-k0" "$AGM_DIR/assign-body.txt"
check "交辦帶 unmatched" "2.1.278-u0" "$AGM_DIR/assign-body.txt"
check "有記 log" "claude → 2.1.278：已派" "$AGM_DIR/release-triage.log"
teardown

# 3. 兩個 kind 各自獨立：各派一則、request-id 各自帶自己的 to；一個 kind 檢查失敗不影響另一個。
setup
mk_pending claude 2.1.278 2.1.278; mk_pending codex 0.155.0 0.155.0
bash "$SCRIPT"
equals "兩個 kind 各派一則" "$(assigns)" "2"
check "claude 的 request-id" "\-\-request-id release-triage-claude-2.1.278" "$AGM_DIR/calls.log"
check "codex 的 request-id" "\-\-request-id release-triage-codex-0.155.0" "$AGM_DIR/calls.log"
equals "額度一輪只查一次" "$(grep -c ' quota' "$AGM_DIR/calls.log" | tr -d ' ')" "1"
teardown
setup
rm -f "$ROOT/triage/claude.json"; mk_pending codex 0.155.0 0.155.0
bash "$SCRIPT"
equals "claude 檢查失敗：codex 照派" "$(assigns)" "1"
check "codex 有派" "release-triage-codex-0.155.0" "$AGM_DIR/calls.log"
check "claude 失敗有記 log" "release-triage-check --kind claude 失敗" "$AGM_DIR/release-triage.log"
teardown

# 4. 一則交辦最多 5 版：多的留給下一輪，request-id 用這批最後一版（不會跟下一批撞同一個 id）。
setup
mk_pending claude 2.1.290 2.1.281 2.1.282 2.1.283 2.1.284 2.1.285 2.1.290; mk_empty codex
bash "$SCRIPT"
equals "超過 5 版仍只派一則" "$(assigns)" "1"
check "request-id 用這批最後一版" "release-triage-claude-2.1.285" "$AGM_DIR/calls.log"
check "第 6 版不在正文的 pending" '"deferred_versions"' "$AGM_DIR/assign-body.txt"
check_no "第 6 版的條目不在這則" "2.1.290-k0" "$AGM_DIR/assign-body.txt"
teardown
# 4b. kept+unmatched 合計 80 條：每版 3 條，27 版＝81 條，第 27 版留給下一輪（先受 5 版上限，改用大版本測 80 條）。
setup
python3 - "$ROOT/triage/claude.json" <<'PY'
import json, sys
def ver(v, n):
    return {"version": v, "kept": [{"id": f"{v}-{i}", "text": "t", "categories": []} for i in range(n)], "unmatched": [], "dropped_count": 0}
json.dump({"kind": "claude", "from": "1", "to": "2.0.3", "pending": [ver("2.0.1", 50), ver("2.0.2", 40), ver("2.0.3", 5)]}, open(sys.argv[1], "w"))
PY
mk_empty codex
bash "$SCRIPT"
check "50＋40 超過 80：第二版留下一輪" "release-triage-claude-2.0.1" "$AGM_DIR/calls.log"
check_no "第二版條目不在這則" "2.0.2-0" "$AGM_DIR/assign-body.txt"
teardown

# 5. 額度閘門：5h 86% → 不派；84% → 派；limit_hit → 不派；端點壞或找不到額度格 → 照派並記 log。
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
# 5b. 額度認的是「被派的那顆 bot 的身分」：release_bot_id 的 identity 是 cc1 → 看 claude:cc1 這一格。
setup
cp "$HERE/fixtures/herdr-update-runtime.json" "$AGM_DIR/runtime.json"
mk_pending claude 2.1.278 2.1.278; mk_empty codex
export STUB_QUOTA_JSON='{"kinds":{"claude":{"five_hour":{"used_pct":5.0},"limit_hit":null},"claude:cc1":{"five_hour":{"used_pct":90.0},"limit_hit":null}}}'
bash "$SCRIPT"
equals "cc1 那格 90%：不派" "$(assigns)" "0"
teardown
# 5c. 閘門擋下後兩個 kind 都不派，而且只查一次額度。
setup
mk_pending claude 2.1.278 2.1.278; mk_pending codex 0.155.0 0.155.0
export STUB_QUOTA_JSON='{"kinds":{"claude":{"five_hour":{"used_pct":95.0},"limit_hit":null}}}'
bash "$SCRIPT"
equals "擋下時兩個 kind 都不派" "$(assigns)" "0"
teardown
# 5d. 門檻可調。
setup
mk_pending claude 2.1.278 2.1.278; mk_empty codex
export AGM_TRIAGE_QUOTA_MAX=95 STUB_QUOTA_JSON='{"kinds":{"claude":{"five_hour":{"used_pct":90.0},"limit_hit":null}}}'
bash "$SCRIPT"
equals "門檻調到 95：90% 照派" "$(assigns)" "1"
teardown

# 6. 殘留鎖（pid 不存在）：回收並接手這一輪。
setup
mk_pending claude 2.1.278 2.1.278; mk_empty codex
mkdir "$AGM_DIR/release-triage.lock"; echo "999999 1" > "$AGM_DIR/release-triage.lock/owner"
export AGM_LOCK_STALE_SECS=0
bash "$SCRIPT"
equals "殘留鎖：回收後照派" "$(assigns)" "1"
check "殘留鎖：有記回收 log" "清掉殘留鎖" "$AGM_DIR/release-triage.log"
[ ! -d "$AGM_DIR/release-triage.lock" ] && { echo "ok   - 跑完鎖有釋放"; PASS=$((PASS + 1)); } || { echo "FAIL - 跑完鎖有釋放"; FAIL=$((FAIL + 1)); }
teardown
# 6b. 舊格式的鎖（沒有 owner 檔）也回收；剛建立的鎖（< STALE_SECS）先不動。
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

# 7. 活鎖（執行者還在、指令列是這支腳本）：擋下；卡太久推 ops-alert，仍不搶鎖。
setup
mk_pending claude 2.1.278 2.1.278; mk_empty codex
bash -c 'sleep 30; : # release-triage-kick' & LIVE=$!
sleep 0.3
mkdir "$AGM_DIR/release-triage.lock"; echo "$LIVE $(date +%s)" > "$AGM_DIR/release-triage.lock/owner"
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
# 7b. pid 還活著但不是這支腳本（pid 被別的程序重用）：視為殘留，回收。
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

# 8. launchd 的最小環境：env -i、PATH 只有 /usr/bin:/bin，用系統 /bin/bash 跑（macOS 內建 3.2）也要跑得起來。
setup
mk_pending claude 2.1.278 2.1.278; mk_empty codex
env -i PATH=/usr/bin:/bin AGM_DIR="$AGM_DIR" AM_BINARY="$AM_BINARY" /bin/bash "$SCRIPT"
equals "env -i 最小 PATH：照派" "$(assigns)" "1"
teardown
# 8b. 找不到 python3：log＋ops_alert，不是靜默 exit 0（PATH 只放少數幾個工具，AGM_EXTRA_PATH 清空）。
setup
mk_pending claude 2.1.278 2.1.278; mk_empty codex
mkdir "$ROOT/nopy"; for t in date cat seq dirname; do ln -s "$(command -v $t)" "$ROOT/nopy/$t"; done
env -i PATH="$ROOT/nopy" AGM_EXTRA_PATH="" AGM_DIR="$AGM_DIR" AM_BINARY="$AM_BINARY" /bin/bash "$SCRIPT"
check "缺 python3：有記 log" "找不到 python3" "$AGM_DIR/release-triage.log"
check "缺 python3：推 ops-alert" "\-\-reason missing_dependency" "$AGM_DIR/calls.log"
equals "缺 python3：不派" "$(assigns)" "0"
teardown

# 9. 假 agm 對未知旗標要 exit 2（守住 stub 本身，`--client-request-id` 拼錯就會被抓到）。
setup
"$AGM_DIR/bin/agm" --compact assign --bot x --client-request-id y 2>/dev/null; equals "stub 對未知旗標 exit 2" "$?" "2"
teardown

# 10. 派工失敗：記 log，下一輪再試（沒有 state 檔要寫——同一批由 daemon 的 request-id 去重）。
setup
mk_pending claude 2.1.278 2.1.278; mk_empty codex
export STUB_ASSIGN_FAIL=1
bash "$SCRIPT"
check "失敗有記 log" "派工失敗" "$AGM_DIR/release-triage.log"
export STUB_ASSIGN_FAIL=""
bash "$SCRIPT"
equals "下一輪重派（同一個 request-id）" "$(grep -c 'release-triage-claude-2.1.278' "$AGM_DIR/calls.log" | tr -d ' ')" "2"
teardown

# 11. 派給誰：release_bot_id ＞ responder；AGM_RELEASE_BOT 蓋過；都沒有就跳過。
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

# 12. binary 或 task 不在：記 log 跳過，不當機。
setup
mk_pending claude 2.1.278 2.1.278
export AM_BINARY="$ROOT/no-such-binary"
bash "$SCRIPT"
check "binary 不在就跳過" "找不到" "$AGM_DIR/release-triage.log"
equals "binary 不在不派" "$(assigns)" "0"
teardown

# 13. 派成功 → 對這一則實際帶出去的版本標 dispatched（截斷時只標截斷後那批）；派失敗不標。
setup
mk_pending claude 2.1.278 2.1.277 2.1.278; mk_empty codex
bash "$SCRIPT"
check "dispatched 帶 kind 與兩版" "release-triage dispatched --kind claude --version 2.1.277 --version 2.1.278$" "$AGM_DIR/calls.log"
equals "dispatched 只呼叫一次" "$(grep -c 'release-triage dispatched' "$AGM_DIR/calls.log" | tr -d ' ')" "1"
teardown
setup
mk_pending claude 2.1.290 2.1.281 2.1.282 2.1.283 2.1.284 2.1.285 2.1.290; mk_empty codex
bash "$SCRIPT"
check "截斷：dispatched 只帶前 5 版" "dispatched --kind claude --version 2.1.281 --version 2.1.282 --version 2.1.283 --version 2.1.284 --version 2.1.285$" "$AGM_DIR/calls.log"
check_no "截斷：第 6 版不標" "version 2.1.290" "$AGM_DIR/calls.log"
teardown
setup
mk_pending claude 2.1.278 2.1.278; mk_pending codex 0.155.0 0.155.0
bash "$SCRIPT"
check "兩個 kind 各標自己的" "dispatched --kind codex --version 0.155.0$" "$AGM_DIR/calls.log"
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
# 13b. dispatched 失敗：記 log、不重派、exit 0。
setup
mk_pending claude 2.1.278 2.1.278; mk_empty codex
export STUB_DISPATCHED_FAIL=1
bash "$SCRIPT"; equals "dispatched 失敗：exit 0" "$?" "0"
equals "dispatched 失敗：不重派" "$(assigns)" "1"
check "dispatched 失敗有記 log" "標記 dispatched 失敗" "$AGM_DIR/release-triage.log"
teardown

# 14. publish：每輪每個 kind 各一次（不論有沒有 pending）；沒東西要重試時安靜。
setup
mk_empty claude; mk_empty codex
bash "$SCRIPT"
check "publish claude" "release-triage publish --kind claude" "$AGM_DIR/calls.log"
check "publish codex" "release-triage publish --kind codex" "$AGM_DIR/calls.log"
equals "publish 共兩次" "$(grep -c 'release-triage publish' "$AGM_DIR/calls.log" | tr -d ' ')" "2"
equals "無事：不寫 log" "$(cat "$AGM_DIR/release-triage.log" 2>/dev/null)" ""
teardown
setup
mk_pending claude 2.1.278 2.1.278; mk_empty codex
export STUB_QUOTA_JSON='{"kinds":{"claude":{"five_hour":{"used_pct":95.0},"limit_hit":null}}}'
bash "$SCRIPT"
equals "額度擋下時 publish 仍照跑" "$(grep -c 'release-triage publish' "$AGM_DIR/calls.log" | tr -d ' ')" "2"
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

# 15. 舊 agm 不認得 release-triage（exit 2）：只講一次；派工照舊。
setup
mk_pending claude 2.1.278 2.1.278; mk_empty codex
export STUB_OLD_AGM=1
bash "$SCRIPT"; equals "舊 agm：exit 0" "$?" "0"
equals "舊 agm：assign 照派" "$(assigns)" "1"
bash "$SCRIPT"; bash "$SCRIPT"
equals "舊 agm：說明只寫一次" "$(grep -c '不認得 release-triage' "$AGM_DIR/release-triage.log" | tr -d ' ')" "1"
teardown

echo "$PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
