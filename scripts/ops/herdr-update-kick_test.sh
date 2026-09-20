#!/bin/bash
# herdr-update-kick.sh 的隔離測試：自己的 AGM 目錄、假的 `herdr`／`gh`／`agents-managerd`／`bin/agm`，
# 完全不碰正式 AGM、daemon 或真的 herdr。測的是決策：什麼時候派、派幾次、派給誰、state 什麼時候才寫、
# launchd 最小 PATH 找不找得到依賴、缺依賴會不會喊人；
# 版本比較與 CHANGELOG 段落擷取本身的正確性由 `daemon/src/herdr_update.rs` 的 `cargo test` 釘住，
# 這裡的假 `agents-managerd` 只做最簡單的版本比較，不重現全部 Rust 邏輯。
#
#   bash scripts/ops/herdr-update-kick_test.sh
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
SCRIPT="$HERE/herdr-update-kick.sh"
PASS=0
FAIL=0

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
printf '%s' '{"id":"a-1"}'
STUB
  chmod +x "$AGM_DIR/bin/agm"

  # 假 herdr：印 `herdr <STUB_HERDR_VERSION>`（預設 0.8.2），跟真的 `herdr --version` 同形狀。
  cat > "$ROOT/stubbin/herdr" <<'STUB'
#!/bin/bash
echo "herdr ${STUB_HERDR_VERSION:-0.8.2}"
STUB
  chmod +x "$ROOT/stubbin/herdr"

  # 假 gh：只認 `release list -R <repo> --exclude-pre-releases -L 1 --json tagName`。
  # STUB_GH_TAG 沒設就印空陣列（模擬查不到）。
  cat > "$ROOT/stubbin/gh" <<'STUB'
#!/bin/bash
echo "$*" >> "$AGM_DIR/gh-calls.log"
if [ -n "${STUB_GH_TAG:-}" ]; then
  printf '[{"tagName":"%s"}]' "$STUB_GH_TAG"
else
  printf '[]'
fi
STUB
  chmod +x "$ROOT/stubbin/gh"
  # 腳本開頭會把 AGM_EXTRA_PATH（預設 Homebrew）接到 PATH 前面；不蓋掉的話真的
  # /opt/homebrew/bin/herdr、gh 會蓋過假的。env -i 模擬 launchd 時也靠這個找到 stub。
  export AGM_EXTRA_PATH="$ROOT/stubbin"
  export PATH="$ROOT/stubbin:$PATH"

  # 假 agents-managerd：只做最簡單的數值版本比較，跟真 CLI 同一份 JSON 形狀
  # （installed_version/latest_version/has_update/should_notify/brief），拿掉中括號/日期/v 前綴
  # 這種細節解析——那些由 daemon/src/changelog.rs 的 cargo test 釘住，不在這裡重測。
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
# launchd 的預設 PATH 不含 Homebrew。AGM_EXTRA_PATH 指向 stubbin＝「配置完成後仍找得到依賴」。
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

# 1. 有更新、第一次看到：派給 responder（runtime.json 這份沒有 release_bot_id），帶版本與 CHANGELOG 內容，state 寫入。
setup
bash "$SCRIPT"
check "派給協調者（沒有更專用的 bot id）" "\-\-bot bot-resp" "$AGM_DIR/calls.log"
check_no "不派給巡檢自己（daemon 會 400）" "\-\-bot bot-agm" "$AGM_DIR/calls.log"
check "旗標是 --request-id" "\-\-request-id agm-herdr-update-0.9.0" "$AGM_DIR/calls.log"
check "交辦給巡檢驗收" "\-\-review-by patrol" "$AGM_DIR/calls.log"
check "交辦帶版本差異" "0.8.2 → 0.9.0" "$AGM_DIR/assign-body.txt"
check "交辦帶 CHANGELOG 原文" "修了一堆繞路" "$AGM_DIR/assign-body.txt"
equals "state 寫入" "$(cat "$AGM_DIR/herdr-update.last")" "0.9.0"
teardown

# 2. 同一版再跑一次：should_notify 為 false（Rust 端去重），不重派，不留新 log 行。
setup
bash "$SCRIPT"
LINES_BEFORE=$(wc -l < "$AGM_DIR/herdr-update.log" | tr -d ' ')
: > "$AGM_DIR/calls.log"
bash "$SCRIPT"
check_no "同一版不重派" "assign" "$AGM_DIR/calls.log"
LINES_AFTER=$(wc -l < "$AGM_DIR/herdr-update.log" | tr -d ' ')
equals "同一版不留新 log" "$LINES_AFTER" "$LINES_BEFORE"
teardown

# 3. 沒有更新（本機已經是最新）：不派，不留有意義的 log（`2>>"$LOG"` 承接 gh／CLI 的 stderr
# 會讓檔案存在但是空的——那是無害的副作用，不是「有事要看」，不強求檔案完全不存在）。
setup
export STUB_HERDR_VERSION="0.9.0" STUB_GH_TAG="v0.9.0"
bash "$SCRIPT"
check_no "沒有更新不派" "assign" "$AGM_DIR/calls.log"
equals "沒有更新不留有意義的 log" "$(cat "$AGM_DIR/herdr-update.log" 2>/dev/null)" ""
teardown

# 4. 新版又出現：換版本再派一次，request-id 與 state 都跟著換。
setup
bash "$SCRIPT"                       # 派 0.9.0，state=0.9.0
export STUB_GH_TAG="v0.9.1"
bash "$SCRIPT"
check "新版換一筆 request-id" "\-\-request-id agm-herdr-update-0.9.1" "$AGM_DIR/calls.log"
equals "state 跟著換版" "$(cat "$AGM_DIR/herdr-update.last")" "0.9.1"
teardown

# 5. 派工失敗：state 不動，下一輪還會重派同一版。
setup
export STUB_ASSIGN_FAIL=1
bash "$SCRIPT"
[ ! -f "$AGM_DIR/herdr-update.last" ] && { echo "ok   - 失敗不寫 state"; PASS=$((PASS + 1)); } || { echo "FAIL - 失敗不寫 state"; FAIL=$((FAIL + 1)); }
check "失敗有記 log" "派工失敗" "$AGM_DIR/herdr-update.log"
export STUB_ASSIGN_FAIL=""
bash "$SCRIPT"
equals "下一輪重派後才寫 state" "$(cat "$AGM_DIR/herdr-update.last")" "0.9.0"
teardown

# 6. 找不到要派給誰：跳過，不亂派給別的 bot。
setup
rm -f "$AGM_DIR/runtime.json"
bash "$SCRIPT"
check "找不到對象就跳過" "找不到要派給誰" "$AGM_DIR/herdr-update.log"
check_no "不亂派" "assign" "$AGM_DIR/calls.log"
teardown

# 7. runtime.json 有專用的 herdr_update_bot_id：優先派給它，不是 release/responder。
setup
cp "$HERE/fixtures/herdr-update-runtime.json" "$AGM_DIR/runtime.json"
bash "$SCRIPT"
check "優先派給專用 bot" "\-\-bot bot-herdr" "$AGM_DIR/calls.log"
check_no "不派給 release_bot_id" "\-\-bot bot-release" "$AGM_DIR/calls.log"
teardown

# 8. AGM_HERDR_UPDATE_BOT 環境變數蓋過 runtime.json。
setup
cp "$HERE/fixtures/herdr-update-runtime.json" "$AGM_DIR/runtime.json"
export AGM_HERDR_UPDATE_BOT="bot-override"
bash "$SCRIPT"
check "env 覆寫優先" "\-\-bot bot-override" "$AGM_DIR/calls.log"
teardown

# 9. 抓不到 CHANGELOG（file:// 指到不存在的檔）：跳過，不派、不寫 state。
setup
export HERDR_CHANGELOG_URL="file://$ROOT/does-not-exist.md"
bash "$SCRIPT"
check "抓不到 CHANGELOG 有記 log" "抓不到 CHANGELOG" "$AGM_DIR/herdr-update.log"
check_no "抓不到 CHANGELOG 不派" "assign" "$AGM_DIR/calls.log"
[ ! -f "$AGM_DIR/herdr-update.last" ] && { echo "ok   - 抓不到 CHANGELOG 不寫 state"; PASS=$((PASS + 1)); } || { echo "FAIL - 抓不到 CHANGELOG 不寫 state"; FAIL=$((FAIL + 1)); }
teardown

# 10. 查不到最新穩定版（gh 沒回 tag）：跳過。
setup
export STUB_GH_TAG=""
bash "$SCRIPT"
check "查不到最新版有記 log" "查不到" "$AGM_DIR/herdr-update.log"
check_no "查不到最新版不派" "assign" "$AGM_DIR/calls.log"
teardown

# 11. 鎖（#66 review 留言：純 mkdir 鎖被 SIGKILL 就永久停擺）。鎖裡寫 pid＋時間，執行者不在就回收。
# seed_lock <owner 那行|空字串＝舊版腳本留下的、沒有 owner 檔的鎖> <鎖已經存在幾秒>
seed_lock() {
  mkdir "$AGM_DIR/herdr-update.lock"
  [ -n "$1" ] && echo "$1" > "$AGM_DIR/herdr-update.lock/owner"
  python3 -c 'import os,sys,time; t=time.time()-int(sys.argv[2]); os.utime(sys.argv[1],(t,t))' "$AGM_DIR/herdr-update.lock" "$2"
}
lock_gone() { [ ! -d "$AGM_DIR/herdr-update.lock" ]; }

# 11a. 昨天留下的鎖（執行者 pid 早就不在）：今天照常偵測、派工，並記一行回收 log。
setup
seed_lock "999999 $(( $(date +%s) - 90000 ))" 90000
bash "$SCRIPT"
equals "昨天留下的鎖：回收後照派" "$(assigns)" "1"
check "昨天留下的鎖：有記回收 log" "清掉殘留鎖" "$AGM_DIR/herdr-update.log"
equals "昨天留下的鎖：state 寫入" "$(cat "$AGM_DIR/herdr-update.last" 2>/dev/null)" "0.9.0"
lock_gone && { echo "ok   - 跑完鎖有釋放"; PASS=$((PASS + 1)); } || { echo "FAIL - 跑完鎖有釋放"; FAIL=$((FAIL + 1)); }
teardown

# 11b. 舊版腳本留下的鎖（沒有 owner 檔，就是原本那個純 mkdir）：昨天的回收；剛建立的先不動。
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

# 11c. 活鎖（執行者還在、指令列是這支腳本）：擋下、不搶；卡太久才推 ops-alert。
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

# 11d. pid 還活著但不是這支腳本（pid 被別的程序重用）：視為殘留，回收。
setup
bash -c 'sleep 30; :' & LIVE=$!
sleep 0.3
seed_lock "$LIVE $(( $(date +%s) - 90000 ))" 90000
bash "$SCRIPT"
equals "pid 被別的程序重用：回收後照派" "$(assigns)" "1"
kill "$LIVE" 2>/dev/null; wait "$LIVE" 2>/dev/null
teardown

# 12. 找不到 agents-managerd binary：跳過，不當機。
setup
export AM_BINARY="$ROOT/no-such-binary"
bash "$SCRIPT"
check "binary 不在就跳過" "找不到" "$AGM_DIR/herdr-update.log"
check_no "binary 不在不派" "assign" "$AGM_DIR/calls.log"
teardown

# 13. 第一次跑（沒有 $STATE，`LAST_ARGS` 是空陣列）要用系統的 `/bin/bash` 跑：launchd 就是這樣呼叫，
# macOS 內建那顆還是 3.2，`set -u` 對空陣列的 `"${arr[@]}"` 會直接 unbound variable 死掉（跟
# daemon-update-kick.sh 的 `${REVIEW[@]+"${REVIEW[@]}"}` 是同一個坑）；PATH 上的新版 bash 不會踩到，
# 所以其餘測試用 `bash "$SCRIPT"` 測不出這個回歸。
if [ -x /bin/bash ]; then
  setup
  /bin/bash "$SCRIPT"
  check "系統 /bin/bash 第一次跑不會 unbound variable 死掉" "已派 AGM 解析" "$AGM_DIR/herdr-update.log"
  equals "系統 /bin/bash 下 state 照樣寫入" "$(cat "$AGM_DIR/herdr-update.last")" "0.9.0"
  teardown
else
  echo "skip - 這台機器沒有 /bin/bash，略過舊版 bash 相容性測試"
fi

# 13b. state 原子寫：rename 失敗時不留暫存檔、不留半個 state（下一輪同 request-id 由 daemon 去重）。
setup
mkdir "$ROOT/failmv"; printf '#!/bin/sh\nexit 1\n' > "$ROOT/failmv/mv"; chmod +x "$ROOT/failmv/mv"
PATH="$ROOT/failmv:$PATH" bash "$SCRIPT"
[ ! -f "$AGM_DIR/herdr-update.last" ] && { echo "ok   - rename 失敗：不留 state"; PASS=$((PASS + 1)); } || { echo "FAIL - rename 失敗：留下 state"; FAIL=$((FAIL + 1)); }
[ -z "$(ls "$AGM_DIR" | grep 'last.tmp')" ] && { echo "ok   - rename 失敗：不留暫存檔"; PASS=$((PASS + 1)); } || { echo "FAIL - rename 失敗：留下暫存檔"; FAIL=$((FAIL + 1)); }
check "寫不了狀態檔有記 log" "寫不了狀態檔" "$AGM_DIR/herdr-update.log"
teardown

# 14. launchd 的最小環境：env -i、PATH 只有 /usr/bin:/bin:/usr/sbin:/sbin（不含 Homebrew），
# 假 herdr／gh 只在 AGM_EXTRA_PATH。腳本必須自己把 EXTRA_PATH 接到前面，系統 /bin/bash 也要跑得起來。
setup
run_launchd
equals "env -i 最小 PATH（系統 /bin/bash）：照派" "$(assigns)" "1"
equals "env -i 最小 PATH：state 照寫" "$(cat "$AGM_DIR/herdr-update.last")" "0.9.0"
teardown

# 14b. 找不到 herdr／gh／python3／curl：log＋ops_alert，不是靜默 exit 0（#66 留言：
# launchd 預設 PATH 找不到 Homebrew 的 herdr／gh 時，功能表面「已排程」但永遠不派）。
link_tools() { # link_tools <dir> <name…>
  local d="$1"; shift
  mkdir -p "$d"
  for t in "$@"; do ln -s "$(command -v "$t")" "$d/$t"; done
}
setup
mkdir "$ROOT/noherdr"
ln -s "$ROOT/stubbin/gh" "$ROOT/noherdr/gh"
link_tools "$ROOT/noherdr" date python3 curl
env -i PATH="$ROOT/noherdr:/usr/bin:/bin" AGM_EXTRA_PATH="" \
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
env -i PATH="$ROOT/nogh:/usr/bin:/bin" AGM_EXTRA_PATH="" \
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

# 14c. 預設補的目錄要含 ~/.local/bin（這台登入 shell 的 herdr 在那裡，launchd 預設 PATH 沒有）：AGM_EXTRA_PATH **不設**，
# 假 herdr／gh 只放在 $HOME/.local/bin（HOME 指到測試目錄）。用系統 /bin/bash（3.2）＋最小 PATH，stderr 收下來看有沒有 unbound variable。
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

# 15. 「連續一個排程週期不可運作」要被 AGM 看見（#66 review 留言）：查不到最新版連續兩輪才喊，一次抖動不吵人；成功一次就清零。
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

# 15b. 沒有新版（檢查完整跑完）也算成功，清零；之後再失敗一次不會直接喊人。
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

# 15c. 派工連續失敗也算「不可運作」：兩輪之後喊人；binary 連續不在也一樣（原本只有 local log）。
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

# 15d. 版本比較 CLI 結束碼 0、卻印出看不懂的東西（不是 JSON、或少了 should_notify）：這輪沒能完成檢查，不是「沒有新版」。
# 以前 `|| SHOULD=0` 把它當成沒有新版：安靜退出、還把連續失敗清零，CLI 換了輸出形狀就永遠沒人知道。
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

echo "$PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
