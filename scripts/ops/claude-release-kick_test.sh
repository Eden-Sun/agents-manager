#!/bin/bash
# claude-release-kick.sh 的隔離測試：自己的 AGM 目錄、假的版本目錄、假的 `bin/agm`，
# 完全不碰正式 AGM 或 daemon。測的是決策：什麼時候派、派幾次、派給誰、state 什麼時候才寫。
#
#   bash scripts/ops/claude-release-kick_test.sh
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
SCRIPT="$HERE/claude-release-kick.sh"
PASS=0
FAIL=0

setup() {
  ROOT=$(mktemp -d)
  export AGM_DIR="$ROOT/agm" CLAUDE_VERSIONS_DIR="$ROOT/versions"
  mkdir -p "$AGM_DIR/bin" "$CLAUDE_VERSIONS_DIR"
  cp "$HERE/claude-release-task.md" "$AGM_DIR/claude-release-task.md"
  printf '%s' '{"manager_bot_id":"bot-agm"}' > "$AGM_DIR/runtime.json"
  cat > "$AGM_DIR/bin/agm" <<'STUB'
#!/bin/bash
echo "$*" >> "$AGM_DIR/calls.log"
# 交辦內容也留一份，才驗得到有沒有把新舊版本帶進去。
for i in $(seq 1 $#); do
  eval "a=\${$i}"
  case "$a" in --text-file) eval "f=\${$((i+1))}"; cat "$f" >> "$AGM_DIR/assign-body.txt" ;; esac
done
[ -n "${STUB_ASSIGN_FAIL:-}" ] && exit 1
printf '%s' '{"id":"a-1"}'
STUB
  chmod +x "$AGM_DIR/bin/agm"
  : > "$AGM_DIR/calls.log"
  : > "$AGM_DIR/assign-body.txt"
  export STUB_ASSIGN_FAIL=""
}
teardown() { rm -rf "$ROOT"; unset AGM_DIR CLAUDE_VERSIONS_DIR AGM_RELEASE_BOT STUB_ASSIGN_FAIL; }

ver() { mkdir -p "$CLAUDE_VERSIONS_DIR/$1"; touch "$CLAUDE_VERSIONS_DIR/$1"; sleep 0.01; }
check() {
  if grep -q -- "$2" "$3" 2>/dev/null; then echo "ok   - $1"; PASS=$((PASS + 1))
  else echo "FAIL - $1"; echo "      找不到 '$2'，實際內容："; sed 's/^/      /' "$3"; FAIL=$((FAIL + 1)); fi
}
check_no() {
  if grep -q -- "$2" "$3" 2>/dev/null; then echo "FAIL - $1"; echo "      不該有 '$2'"; FAIL=$((FAIL + 1))
  else echo "ok   - $1"; PASS=$((PASS + 1)); fi
}
equals() {
  if [ "$2" = "$3" ]; then echo "ok   - $1"; PASS=$((PASS + 1))
  else echo "FAIL - $1（是 '$2'，預期 '$3'）"; FAIL=$((FAIL + 1)); fi
}

# 1. 第一次執行：只記下目前版本，不為「本來就在的版本」派工。
setup
ver 2.1.272; ver 2.1.273
bash "$SCRIPT"
equals "第一次只記版本" "$(cat "$AGM_DIR/claude-release.last")" "2.1.273"
check_no "第一次不派工" "assign" "$AGM_DIR/calls.log"
teardown

# 2. 換版：派給 runtime.json 的 AGM，交辦帶新舊版本與 binary 路徑，state 更新。
setup
ver 2.1.272; ver 2.1.273
bash "$SCRIPT"                      # 記下 2.1.273
ver 2.1.274
bash "$SCRIPT"
check "派給 AGM 自己" "--bot bot-agm" "$AGM_DIR/calls.log"
check "交辦給巡檢驗收" "--review-by patrol" "$AGM_DIR/calls.log"
check "request id 帶版本" "agm-claude-release-2.1.274" "$AGM_DIR/calls.log"
check "交辦寫出新舊版本" "本次：舊版 2.1.273 → 新版 2.1.274" "$AGM_DIR/assign-body.txt"
check "交辦帶兩顆 binary 路徑" "NEW=$CLAUDE_VERSIONS_DIR/2.1.274" "$AGM_DIR/assign-body.txt"
equals "state 更新" "$(cat "$AGM_DIR/claude-release.last")" "2.1.274"
teardown

# 3. 沒換版：安靜退出，不派也不寫 log。
setup
ver 2.1.273
bash "$SCRIPT"; : > "$AGM_DIR/calls.log"
bash "$SCRIPT"
check_no "沒換版不派" "assign" "$AGM_DIR/calls.log"
equals "沒換版不留 log" "$(wc -l < "$AGM_DIR/claude-release.log" | tr -d ' ')" "1"
teardown

# 4. 派工失敗：state 不動，下一輪還會再試。
setup
ver 2.1.273; bash "$SCRIPT"
ver 2.1.274
export STUB_ASSIGN_FAIL=1
bash "$SCRIPT"
equals "失敗不寫 state" "$(cat "$AGM_DIR/claude-release.last")" "2.1.273"
check "失敗有記 log" "派工失敗" "$AGM_DIR/claude-release.log"
export STUB_ASSIGN_FAIL=""
bash "$SCRIPT"
equals "下一輪重派後才寫 state" "$(cat "$AGM_DIR/claude-release.last")" "2.1.274"
teardown

# 5. 找不到要派給誰（沒有 runtime.json 也沒設 env）：跳過，不亂派給別的 bot。
setup
ver 2.1.273; bash "$SCRIPT"
rm -f "$AGM_DIR/runtime.json"
ver 2.1.274
bash "$SCRIPT"
check "找不到對象就跳過" "找不到要派給誰" "$AGM_DIR/claude-release.log"
check_no "不亂派" "assign" "$AGM_DIR/calls.log"
teardown

# 6. 殘留的鎖：不派，交 AGM 檢查。
setup
ver 2.1.273; bash "$SCRIPT"
ver 2.1.274
mkdir "$AGM_DIR/claude-release.lock"
bash "$SCRIPT"
check "有鎖就跳過" "已有執行者或殘留鎖" "$AGM_DIR/claude-release.log"
check_no "有鎖不派" "assign" "$AGM_DIR/calls.log"
teardown

echo "$PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
