#!/bin/bash
# herdr-update-kick.sh 的隔離測試：自己的 AGM 目錄、假的 `herdr`／`gh`／`agents-managerd`／`bin/agm`，
# 完全不碰正式 AGM、daemon 或真的 herdr。測的是決策：什麼時候派、派幾次、派給誰、state 什麼時候才寫；
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
    --compact|--bot|--review-by|--text-file|--request-id|assign) ;;
    --*) echo "agm: error: unrecognized arguments: $a" >&2; exit 2 ;;
  esac
done
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
  unset AGM_DIR AGM_REPO HERDR_REPO AM_BINARY HERDR_CHANGELOG_URL AGM_HERDR_UPDATE_BOT
  unset STUB_ASSIGN_FAIL STUB_HERDR_VERSION STUB_GH_TAG PATH_ADDED
}

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

# 11. 殘留的鎖：不派，交 AGM 檢查。
setup
mkdir "$AGM_DIR/herdr-update.lock"
bash "$SCRIPT"
check "有鎖就跳過" "已有執行者或殘留鎖" "$AGM_DIR/herdr-update.log"
check_no "有鎖不派" "assign" "$AGM_DIR/calls.log"
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

echo "$PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
