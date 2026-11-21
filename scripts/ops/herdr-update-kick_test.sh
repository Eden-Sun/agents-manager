#!/bin/bash
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
SCRIPT="$HERE/herdr-update-kick.sh"
PASS=0
FAIL=0
. "$HERE/../test-assertions.sh" || exit 1
export AGM_OPS_PLATFORM=darwin

setup() {
  ROOT=$(mktemp -d)
  export AGM_DIR="$ROOT/agm" AGM_REPO="$ROOT/repo" HERDR_REPO="fake/herdr"
  mkdir -p "$AGM_DIR/bin" "$ROOT/stubbin"
  cp "$HERE/fixtures/patrol-runtime.json" "$AGM_DIR/runtime.json"

  cat > "$AGM_DIR/bin/agm" <<'STUB'
#!/bin/bash
echo "$*" >> "$AGM_DIR/calls.log"
for a in "$@"; do
  case "$a" in
    --compact|--bot|--review-by|--text-file|--request-id|--source|--reason|--detail|assign|ops-alert) ;;
    --*) echo "agm: error: unrecognized arguments: $a" >&2; exit 2 ;;
  esac
done
case "$*" in
  *ops-alert*) printf '%s' '{"ok":true}'; exit 0 ;;
esac
for i in $(seq 1 $#); do
  eval "a=\${$i}"
  case "$a" in --text-file) eval "f=\${$((i+1))}"; cat "$f" >> "$AGM_DIR/assign-body.txt" ;; esac
done
[ -n "${STUB_ASSIGN_FAIL:-}" ] && exit 1
# 同一個 request id 已經被更新框的「請 AGM 解析」用掉（正文不同）：daemon 回 409，agm 把錯誤 JSON 印到 stderr、exit 4。
if [ -n "${STUB_ASSIGN_409:-}" ]; then
  printf '%s\n' "{\"error\":\"http_error\",\"message\":\"POST /api/supervisor/assignments 回 409\",\"status\":409,\"detail\":${STUB_ASSIGN_409}}" >&2
  exit 4
fi
printf '%s' '{"id":"a-1"}'
STUB
  chmod +x "$AGM_DIR/bin/agm"

  cat > "$ROOT/stubbin/herdr" <<'STUB'
#!/bin/bash
echo "herdr ${STUB_HERDR_VERSION:-0.8.2}"
STUB
  chmod +x "$ROOT/stubbin/herdr"

  cat > "$ROOT/stubbin/gh" <<'STUB'
#!/bin/bash
echo "$*" >> "$AGM_DIR/gh-calls.log"
# 真的 gh 遇到不認得的呼叫方式是非零；少了 -R 就會去查錯的 repo，這裡要擋下來，不能照樣回 tag。
case "$*" in
  "release list -R ${HERDR_REPO} --exclude-pre-releases -L 1 --json tagName") ;;
  *) echo "gh: unexpected arguments: $*" >&2; exit 2 ;;
esac
if [ -n "${STUB_GH_TAG:-}" ]; then
  printf '[{"tagName":"%s"}]' "$STUB_GH_TAG"
else
  printf '[]'
fi
STUB
  chmod +x "$ROOT/stubbin/gh"
  export AGM_EXTRA_PATH="$ROOT/stubbin"
  export PATH="$ROOT/stubbin:$PATH"

  cat > "$ROOT/fake-agents-managerd" <<'PYEOF'
#!/usr/bin/env python3
import argparse, json, sys

def version_tuple(s):
    if not s:
        return None
    tok = s.split()[0]
    tok = tok.lstrip('[').rstrip(']').lstrip('v')
    try:
        nums = [int(p) for p in tok.split('.')]
    except ValueError:
        return None
    return nums or None

def version_string(s):
    v = version_tuple(s)
    return '.'.join(str(n) for n in v) if v is not None else None

p = argparse.ArgumentParser()
sub = p.add_subparsers(dest='cmd')
c = sub.add_parser('herdr-update-check')
c.add_argument('--installed', required=True)
c.add_argument('--latest', required=True)
c.add_argument('--changelog-file', required=True)
c.add_argument('--last-notified')
args = p.parse_args()
if args.cmd != 'herdr-update-check':
    sys.exit(2)

installed = version_string(args.installed)
latest = version_string(args.latest)
if installed is None or latest is None:
    print(f"看不懂版本號：installed=`{args.installed}` latest=`{args.latest}`", file=sys.stderr)
    sys.exit(2)
has_update = version_tuple(latest) > version_tuple(installed)
should_notify = has_update and args.last_notified != latest
brief = None
if should_notify:
    with open(args.changelog_file, encoding='utf-8') as f:
        changelog = f.read()
    brief = f"herdr 有新版：{installed} → {latest}（本機／最新穩定版）。\n\n{changelog}"
print(json.dumps({
    "installed_version": installed,
    "latest_version": latest,
    "has_update": has_update,
    "should_notify": should_notify,
    "brief": brief,
}))
PYEOF
  chmod +x "$ROOT/fake-agents-managerd"
  export AM_BINARY="$ROOT/fake-agents-managerd"

  echo "## 0.9.0

- 修了一堆繞路" > "$ROOT/changelog.md"
  export HERDR_CHANGELOG_URL="file://$ROOT/changelog.md"

  : > "$AGM_DIR/calls.log"
  : > "$AGM_DIR/gh-calls.log"
  : > "$AGM_DIR/assign-body.txt"
  export STUB_ASSIGN_FAIL="" STUB_HERDR_VERSION="0.8.2" STUB_GH_TAG="v0.9.0"
}
teardown() {
  rm -rf "$ROOT"
  unset AGM_DIR AGM_REPO HERDR_REPO AM_BINARY HERDR_CHANGELOG_URL AGM_HERDR_UPDATE_BOT AGM_EXTRA_PATH
  unset AGM_LOCK_STALE_SECS AGM_LOCK_HUNG_SECS AGM_FAIL_ALERT_AFTER
  unset STUB_ASSIGN_FAIL STUB_HERDR_VERSION STUB_GH_TAG
}
run_launchd() {
  env -i PATH=/usr/bin:/bin:/usr/sbin:/sbin \
    AGM_DIR="$AGM_DIR" AGM_REPO="$AGM_REPO" AM_BINARY="$AM_BINARY" \
    HERDR_REPO="$HERDR_REPO" HERDR_CHANGELOG_URL="$HERDR_CHANGELOG_URL" \
    AGM_EXTRA_PATH="${1-$ROOT/stubbin}" \
    STUB_HERDR_VERSION="${STUB_HERDR_VERSION:-0.8.2}" \
    STUB_GH_TAG="${STUB_GH_TAG:-v0.9.0}" \
    STUB_ASSIGN_FAIL="${STUB_ASSIGN_FAIL:-}" \
    /bin/bash "$SCRIPT"
}
assigns() { grep -c ' assign ' "$AGM_DIR/calls.log" 2>/dev/null | tr -d ' '; }

setup
bash "$SCRIPT"
check "派給協調者（沒有更專用的 bot id）" "\-\-bot bot-resp" "$AGM_DIR/calls.log"
check_no "不派給巡檢自己（daemon 會 400）" "\-\-bot bot-agm" "$AGM_DIR/calls.log"
check "旗標是 --request-id" "\-\-request-id agm-herdr-update-0.9.0" "$AGM_DIR/calls.log"
check "交辦給巡檢驗收" "\-\-review-by patrol" "$AGM_DIR/calls.log"
check "交辦帶版本差異" "0.8.2 → 0.9.0" "$AGM_DIR/assign-body.txt"
check "交辦帶 CHANGELOG 原文" "修了一堆繞路" "$AGM_DIR/assign-body.txt"
check "交辦帶升級後本機網路驗收" "herdr-lan-check.sh" "$AGM_DIR/assign-body.txt"
check "交辦禁止 Apple 工具代驗" "Apple 內建 nc" "$AGM_DIR/assign-body.txt"
equals "state 寫入" "$(cat "$AGM_DIR/herdr-update.last")" "0.9.0"
check "darwin 仍帶 codesign 驗收" "codesign" "$AGM_DIR/assign-body.txt"
check "darwin 仍帶 /opt/homebrew 路徑" "/opt/homebrew" "$AGM_DIR/assign-body.txt"
check "darwin 仍帶授權本機網路" "需要使用者授權本機網路" "$AGM_DIR/assign-body.txt"
check "darwin 升級驗收包含真 herdr argv 契約測試" "AM_TEST_REAL_HERDR=1 cargo test -p agents-managerd -- the_generated_argv_parses_with_the_real_herdr" "$AGM_DIR/assign-body.txt"
teardown

setup
AGM_OPS_PLATFORM=linux bash "$SCRIPT"
check "linux：照樣派工並帶版本差異" "0.8.2 → 0.9.0" "$AGM_DIR/assign-body.txt"
check "linux：驗收看 herdr --version 是新版" "herdr --version\` 是 0.9.0" "$AGM_DIR/assign-body.txt"
check "linux：驗收看 herdr pane list" "herdr pane list" "$AGM_DIR/assign-body.txt"
check "linux：驗收對 idle bot 送 prompt" "idle bot 送一句 prompt" "$AGM_DIR/assign-body.txt"
check_no "linux：正文不含 codesign" "codesign" "$AGM_DIR/assign-body.txt"
check_no "linux：正文不含 /opt/homebrew" "/opt/homebrew" "$AGM_DIR/assign-body.txt"
check_no "linux：正文不含授權本機網路" "授權本機網路" "$AGM_DIR/assign-body.txt"
check "linux：升級驗收包含真 herdr argv 契約測試" "AM_TEST_REAL_HERDR=1 cargo test -p agents-managerd -- the_generated_argv_parses_with_the_real_herdr" "$AGM_DIR/assign-body.txt"
equals "linux：state 照寫" "$(cat "$AGM_DIR/herdr-update.last")" "0.9.0"
teardown

setup
bash "$SCRIPT"
LINES_BEFORE=$(wc -l < "$AGM_DIR/herdr-update.log" | tr -d ' ')
: > "$AGM_DIR/calls.log"
bash "$SCRIPT"
check_no "同一版不重派" "assign" "$AGM_DIR/calls.log"
LINES_AFTER=$(wc -l < "$AGM_DIR/herdr-update.log" | tr -d ' ')
equals "同一版不留新 log" "$LINES_AFTER" "$LINES_BEFORE"
teardown

setup
export STUB_HERDR_VERSION="0.9.0" STUB_GH_TAG="v0.9.0"
bash "$SCRIPT"
check_no "沒有更新不派" "assign" "$AGM_DIR/calls.log"
equals "沒有更新不留有意義的 log" "$(cat "$AGM_DIR/herdr-update.log" 2>/dev/null)" ""
teardown

setup
bash "$SCRIPT"
export STUB_GH_TAG="v0.9.1"
bash "$SCRIPT"
check "新版換一筆 request-id" "\-\-request-id agm-herdr-update-0.9.1" "$AGM_DIR/calls.log"
equals "state 跟著換版" "$(cat "$AGM_DIR/herdr-update.last")" "0.9.1"
teardown

setup
export STUB_ASSIGN_FAIL=1
bash "$SCRIPT"
[ ! -f "$AGM_DIR/herdr-update.last" ] && { echo "ok   - 失敗不寫 state"; PASS=$((PASS + 1)); } || { echo "FAIL - 失敗不寫 state"; FAIL=$((FAIL + 1)); }
check "失敗有記 log" "派工失敗" "$AGM_DIR/herdr-update.log"
export STUB_ASSIGN_FAIL=""
bash "$SCRIPT"
equals "下一輪重派後才寫 state" "$(cat "$AGM_DIR/herdr-update.last")" "0.9.0"
teardown

for detail in '{"error":"conflict","reason":"text_mismatch","assignment_id":"a-ui"}' '{"error":"conflict","reason":"request_mismatch","inbox_event_id":"ev-1"}'; do
  setup
  export STUB_ASSIGN_409="$detail"
  bash "$SCRIPT"
  equals "409 已派過（${detail}）：寫 state" "$(cat "$AGM_DIR/herdr-update.last" 2>/dev/null)" "0.9.0"
  check "409 已派過：log 講明不重派" "不重派" "$AGM_DIR/herdr-update.log"
  check_no "409 已派過：不算失敗" "派工失敗" "$AGM_DIR/herdr-update.log"
  : > "$AGM_DIR/calls.log"
  bash "$SCRIPT"
  check_no "409 已派過：下一輪不再送" "assign" "$AGM_DIR/calls.log"
  unset STUB_ASSIGN_409
  teardown
done
setup
export STUB_ASSIGN_409='{"error":"conflict","reason":"not_configured"}'
bash "$SCRIPT"
[ ! -f "$AGM_DIR/herdr-update.last" ] && { echo "ok   - 別的 409：不寫 state"; PASS=$((PASS + 1)); } || { echo "FAIL - 別的 409：不寫 state"; FAIL=$((FAIL + 1)); }
check "別的 409：記成派工失敗" "派工失敗" "$AGM_DIR/herdr-update.log"
unset STUB_ASSIGN_409
teardown

setup
rm -f "$AGM_DIR/runtime.json"
bash "$SCRIPT"
check "找不到對象就跳過" "找不到要派給誰" "$AGM_DIR/herdr-update.log"
check_no "不亂派" "assign" "$AGM_DIR/calls.log"
teardown

setup
cp "$HERE/fixtures/herdr-update-runtime.json" "$AGM_DIR/runtime.json"
bash "$SCRIPT"
check "優先派給專用 bot" "\-\-bot bot-herdr" "$AGM_DIR/calls.log"
check_no "不派給 release_bot_id" "\-\-bot bot-release" "$AGM_DIR/calls.log"
teardown

setup
cp "$HERE/fixtures/herdr-update-runtime.json" "$AGM_DIR/runtime.json"
export AGM_HERDR_UPDATE_BOT="bot-override"
bash "$SCRIPT"
check "env 覆寫優先" "\-\-bot bot-override" "$AGM_DIR/calls.log"
teardown

setup
export HERDR_CHANGELOG_URL="file://$ROOT/does-not-exist.md"
bash "$SCRIPT"
check "抓不到 CHANGELOG 有記 log" "抓不到 CHANGELOG" "$AGM_DIR/herdr-update.log"
check_no "抓不到 CHANGELOG 不派" "assign" "$AGM_DIR/calls.log"
[ ! -f "$AGM_DIR/herdr-update.last" ] && { echo "ok   - 抓不到 CHANGELOG 不寫 state"; PASS=$((PASS + 1)); } || { echo "FAIL - 抓不到 CHANGELOG 不寫 state"; FAIL=$((FAIL + 1)); }
teardown

setup
export STUB_GH_TAG=""
bash "$SCRIPT"
check "查不到最新版有記 log" "查不到" "$AGM_DIR/herdr-update.log"
check_no "查不到最新版不派" "assign" "$AGM_DIR/calls.log"
teardown

setup
export STUB_GH_TAG=""
/bin/bash "$SCRIPT" >"$ROOT/early-failure.out" 2>"$ROOT/early-failure.err"
check_no "建立暫存檔前失敗時 cleanup 不展開空陣列" "unbound variable" "$ROOT/early-failure.err"
teardown

seed_lock() {
  mkdir "$AGM_DIR/herdr-update.lock"
  [ -n "$1" ] && echo "$1" > "$AGM_DIR/herdr-update.lock/owner"
  python3 -c 'import os,sys,time; t=time.time()-int(sys.argv[2]); os.utime(sys.argv[1],(t,t))' "$AGM_DIR/herdr-update.lock" "$2"
}
lock_gone() { [ ! -d "$AGM_DIR/herdr-update.lock" ]; }

setup
seed_lock "999999 $(( $(date +%s) - 90000 ))" 90000
bash "$SCRIPT"
equals "昨天留下的鎖：回收後照派" "$(assigns)" "1"
check "昨天留下的鎖：有記回收 log" "清掉殘留鎖" "$AGM_DIR/herdr-update.log"
equals "昨天留下的鎖：state 寫入" "$(cat "$AGM_DIR/herdr-update.last" 2>/dev/null)" "0.9.0"
lock_gone && { echo "ok   - 跑完鎖有釋放"; PASS=$((PASS + 1)); } || { echo "FAIL - 跑完鎖有釋放"; FAIL=$((FAIL + 1)); }
teardown

setup
seed_lock "" 90000
bash "$SCRIPT"
equals "沒有 owner 檔的昨天的舊鎖：回收後照派" "$(assigns)" "1"
teardown
setup
seed_lock "" 0
bash "$SCRIPT"
equals "剛建立、讀不到執行者的鎖：這輪不派" "$(assigns)" "0"
check "剛建立的鎖：有記跳過原因" "鎖剛建立" "$AGM_DIR/herdr-update.log"
[ -d "$AGM_DIR/herdr-update.lock" ] && { echo "ok   - 剛建立的鎖沒被動"; PASS=$((PASS + 1)); } || { echo "FAIL - 剛建立的鎖沒被動"; FAIL=$((FAIL + 1)); }
teardown

setup
bash -c 'sleep 30; : # herdr-update-kick' & LIVE=$!
sleep 0.3
seed_lock "$LIVE $(date +%s)" 0
bash "$SCRIPT"
equals "活鎖：不派" "$(assigns)" "0"
check "活鎖：有記 log" "已有執行者" "$AGM_DIR/herdr-update.log"
check_no "活鎖：沒到卡住門檻不喊人" "ops-alert" "$AGM_DIR/calls.log"
export AGM_LOCK_HUNG_SECS=0
bash "$SCRIPT"
check "活鎖卡太久：推 ops-alert" "ops-alert .*\-\-reason runner_hung" "$AGM_DIR/calls.log"
equals "活鎖卡太久：仍不搶鎖、不派" "$(assigns)" "0"
[ -d "$AGM_DIR/herdr-update.lock" ] && { echo "ok   - 活鎖沒被搶"; PASS=$((PASS + 1)); } || { echo "FAIL - 活鎖沒被搶"; FAIL=$((FAIL + 1)); }
kill "$LIVE" 2>/dev/null; wait "$LIVE" 2>/dev/null
teardown

setup
bash -c 'sleep 30; :' & LIVE=$!
sleep 0.3
seed_lock "$LIVE $(( $(date +%s) - 90000 ))" 90000
bash "$SCRIPT"
equals "pid 被別的程序重用：回收後照派" "$(assigns)" "1"
kill "$LIVE" 2>/dev/null; wait "$LIVE" 2>/dev/null
teardown

setup
export AM_BINARY="$ROOT/no-such-binary"
bash "$SCRIPT"
check "binary 不在就跳過" "找不到" "$AGM_DIR/herdr-update.log"
check_no "binary 不在不派" "assign" "$AGM_DIR/calls.log"
teardown

if [ -x /bin/bash ]; then
  setup
  /bin/bash "$SCRIPT"
  check "系統 /bin/bash 第一次跑不會 unbound variable 死掉" "已派 AGM 解析" "$AGM_DIR/herdr-update.log"
  equals "系統 /bin/bash 下 state 照樣寫入" "$(cat "$AGM_DIR/herdr-update.last")" "0.9.0"
  teardown
else
  echo "skip - 這台機器沒有 /bin/bash，略過舊版 bash 相容性測試"
fi

setup
mkdir "$ROOT/failmv"; printf '#!/bin/sh\nexit 1\n' > "$ROOT/failmv/mv"; chmod +x "$ROOT/failmv/mv"
PATH="$ROOT/failmv:$PATH" bash "$SCRIPT"
[ ! -f "$AGM_DIR/herdr-update.last" ] && { echo "ok   - rename 失敗：不留 state"; PASS=$((PASS + 1)); } || { echo "FAIL - rename 失敗：留下 state"; FAIL=$((FAIL + 1)); }
[ -z "$(ls "$AGM_DIR" | grep 'last.tmp')" ] && { echo "ok   - rename 失敗：不留暫存檔"; PASS=$((PASS + 1)); } || { echo "FAIL - rename 失敗：留下暫存檔"; FAIL=$((FAIL + 1)); }
check "寫不了狀態檔有記 log" "寫不了狀態檔" "$AGM_DIR/herdr-update.log"
teardown

setup
run_launchd
equals "env -i 最小 PATH（系統 /bin/bash）：照派" "$(assigns)" "1"
equals "env -i 最小 PATH：state 照寫" "$(cat "$AGM_DIR/herdr-update.last")" "0.9.0"
teardown

link_tools() {
  local d="$1"; shift
  mkdir -p "$d"
  for t in "$@"; do ln -s "$(command -v "$t")" "$d/$t"; done
}
setup
mkdir "$ROOT/noherdr"
ln -s "$ROOT/stubbin/gh" "$ROOT/noherdr/gh"
link_tools "$ROOT/noherdr" date python3 curl
env -i PATH="$ROOT/noherdr" AGM_EXTRA_PATH="" \
  AGM_DIR="$AGM_DIR" AGM_REPO="$AGM_REPO" AM_BINARY="$AM_BINARY" \
  HERDR_REPO="$HERDR_REPO" HERDR_CHANGELOG_URL="$HERDR_CHANGELOG_URL" \
  /bin/bash "$SCRIPT"
check "缺 herdr：有記 log" "找不到 herdr" "$AGM_DIR/herdr-update.log"
check "缺 herdr：推 ops-alert" "\-\-reason missing_dependency" "$AGM_DIR/calls.log"
equals "缺 herdr：不派" "$(assigns)" "0"
teardown
setup
mkdir "$ROOT/nogh"
ln -s "$ROOT/stubbin/herdr" "$ROOT/nogh/herdr"
link_tools "$ROOT/nogh" date python3 curl
env -i PATH="$ROOT/nogh" AGM_EXTRA_PATH="" \
  AGM_DIR="$AGM_DIR" AGM_REPO="$AGM_REPO" AM_BINARY="$AM_BINARY" \
  HERDR_REPO="$HERDR_REPO" HERDR_CHANGELOG_URL="$HERDR_CHANGELOG_URL" \
  STUB_HERDR_VERSION=0.8.2 /bin/bash "$SCRIPT"
check "缺 gh：有記 log" "找不到 gh" "$AGM_DIR/herdr-update.log"
check "缺 gh：推 ops-alert" "\-\-reason missing_dependency" "$AGM_DIR/calls.log"
equals "缺 gh：不派" "$(assigns)" "0"
teardown
setup
mkdir "$ROOT/nopy"
ln -s "$ROOT/stubbin/herdr" "$ROOT/nopy/herdr"
ln -s "$ROOT/stubbin/gh" "$ROOT/nopy/gh"
link_tools "$ROOT/nopy" date cat head sed mktemp tr curl mkdir rmdir rm
env -i PATH="$ROOT/nopy" AGM_EXTRA_PATH="" \
  AGM_DIR="$AGM_DIR" AGM_REPO="$AGM_REPO" AM_BINARY="$AM_BINARY" \
  HERDR_REPO="$HERDR_REPO" HERDR_CHANGELOG_URL="$HERDR_CHANGELOG_URL" \
  STUB_HERDR_VERSION=0.8.2 STUB_GH_TAG=v0.9.0 /bin/bash "$SCRIPT"
check "缺 python3：有記 log" "找不到 python3" "$AGM_DIR/herdr-update.log"
check "缺 python3：推 ops-alert" "\-\-reason missing_dependency" "$AGM_DIR/calls.log"
equals "缺 python3：不派" "$(assigns)" "0"
teardown
setup
mkdir "$ROOT/nocurl"
ln -s "$ROOT/stubbin/herdr" "$ROOT/nocurl/herdr"
ln -s "$ROOT/stubbin/gh" "$ROOT/nocurl/gh"
link_tools "$ROOT/nocurl" date python3
env -i PATH="$ROOT/nocurl" AGM_EXTRA_PATH="" \
  AGM_DIR="$AGM_DIR" AGM_REPO="$AGM_REPO" AM_BINARY="$AM_BINARY" \
  HERDR_REPO="$HERDR_REPO" HERDR_CHANGELOG_URL="$HERDR_CHANGELOG_URL" \
  STUB_HERDR_VERSION=0.8.2 STUB_GH_TAG=v0.9.0 /bin/bash "$SCRIPT"
check "缺 curl：有記 log" "找不到 curl" "$AGM_DIR/herdr-update.log"
check "缺 curl：推 ops-alert" "\-\-reason missing_dependency" "$AGM_DIR/calls.log"
equals "缺 curl：不派" "$(assigns)" "0"
teardown

setup
unset AGM_EXTRA_PATH
mkdir -p "$ROOT/home/.local/bin"
cp "$ROOT/stubbin/herdr" "$ROOT/stubbin/gh" "$ROOT/home/.local/bin/"
env -i PATH=/usr/bin:/bin:/usr/sbin:/sbin HOME="$ROOT/home" \
  AGM_DIR="$AGM_DIR" AGM_REPO="$AGM_REPO" AM_BINARY="$AM_BINARY" HERDR_REPO="$HERDR_REPO" HERDR_CHANGELOG_URL="$HERDR_CHANGELOG_URL" \
  STUB_HERDR_VERSION=0.8.2 STUB_GH_TAG=v0.9.0 /bin/bash "$SCRIPT" 2>"$ROOT/stderr.txt"
equals "預設補的目錄含 ~/.local/bin：照派" "$(assigns)" "1"
check_no "預設補 PATH 的那輪沒有 unbound variable" "unbound variable" "$ROOT/stderr.txt"
teardown

setup
export STUB_GH_TAG=""
bash "$SCRIPT"
check_no "第一次失敗：只留 log，不喊人" "ops-alert" "$AGM_DIR/calls.log"
equals "第一次失敗：連續次數記 1" "$(cat "$AGM_DIR/herdr-update.fails")" "1"
bash "$SCRIPT"
check "連續第二輪失敗：推 ops-alert" "ops-alert .*\-\-reason check_failing" "$AGM_DIR/calls.log"
equals "連續次數記 2" "$(cat "$AGM_DIR/herdr-update.fails")" "2"
export STUB_GH_TAG="v0.9.0"
bash "$SCRIPT"
[ ! -f "$AGM_DIR/herdr-update.fails" ] && { echo "ok   - 恢復之後連續次數清零"; PASS=$((PASS + 1)); } || { echo "FAIL - 恢復之後連續次數清零"; FAIL=$((FAIL + 1)); }
equals "恢復之後照派" "$(assigns)" "1"
teardown

setup
export STUB_GH_TAG=""
bash "$SCRIPT"
export STUB_HERDR_VERSION="0.9.0" STUB_GH_TAG="v0.9.0"
bash "$SCRIPT"
[ ! -f "$AGM_DIR/herdr-update.fails" ] && { echo "ok   - 沒有新版的一輪也清零"; PASS=$((PASS + 1)); } || { echo "FAIL - 沒有新版的一輪也清零"; FAIL=$((FAIL + 1)); }
export STUB_GH_TAG=""
bash "$SCRIPT"
check_no "清零之後再失敗一次不喊人" "ops-alert" "$AGM_DIR/calls.log"
teardown

setup
export STUB_ASSIGN_FAIL=1
bash "$SCRIPT"; bash "$SCRIPT"
check "派工連續失敗：推 ops-alert" "ops-alert .*\-\-reason check_failing" "$AGM_DIR/calls.log"
teardown
setup
export AM_BINARY="$ROOT/no-such-binary"
bash "$SCRIPT"
check_no "binary 第一次不在：只留 log" "ops-alert" "$AGM_DIR/calls.log"
bash "$SCRIPT"
check "binary 連續不在：推 ops-alert" "ops-alert .*\-\-reason check_failing" "$AGM_DIR/calls.log"
teardown

for bad in 'warning: config.toml 有不認得的 key' '{"installed_version":"0.8.2"}'; do
  setup
  printf '#!/bin/bash\necho %q\n' "$bad" > "$ROOT/odd-agents-managerd"
  chmod +x "$ROOT/odd-agents-managerd"
  export AM_BINARY="$ROOT/odd-agents-managerd"
  echo 1 > "$AGM_DIR/herdr-update.fails"
  bash "$SCRIPT"
  equals "報告看不懂（${bad}）：算一輪失敗，不清零" "$(cat "$AGM_DIR/herdr-update.fails" 2>/dev/null)" "2"
  check "報告看不懂（${bad}）：連續兩輪推 ops-alert" "ops-alert .*\-\-reason check_failing" "$AGM_DIR/calls.log"
  equals "報告看不懂（${bad}）：不派工" "$(assigns)" "0"
  teardown
done

setup
chmod -x "$AGM_DIR/bin/agm"
bash "$SCRIPT"; RC=$?
equals "agm 不在：exit 0" "$RC" "0"
check  "agm 不在：留一行 log" "agm CLI 不在" "$AGM_DIR/herdr-update.log"
teardown

echo "$PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
