#!/bin/bash
# dev-sshd.sh ssh-opts 的隔離測試（#377）：金鑰產不出來時要回錯，不能印出指到不存在金鑰的 JSON。
# 資料目錄指到一個暫時目錄，不碰 ~/.config/agents-manager/dev-sshd。
#
#   bash scripts/dev-sshd_test.sh
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
PASS=0
FAIL=0
ok() { echo "ok   - $1"; PASS=$((PASS + 1)); }
bad() { echo "FAIL - $1"; FAIL=$((FAIL + 1)); }

# 1. 產不出金鑰（目錄建不起來）：exit 非 0、stdout 沒有 JSON、stderr 講原因。
ROOT=$(mktemp -d)
touch "$ROOT/notadir"
OUT=$(AM_DEV_SSHD_DIR="$ROOT/notadir/sub" sh "$HERE/dev-sshd.sh" ssh-opts 2>"$ROOT/err"); RC=$?
[ "$RC" -ne 0 ] && ok "產不出金鑰：exit 非 0" || bad "產不出金鑰卻 exit 0（輸出：${OUT}）"
[ -z "$OUT" ] && ok "產不出金鑰：stdout 不印 JSON" || bad "stdout 不該有東西：${OUT}"
grep -q 'ssh-opts' "$ROOT/err" && ok "產不出金鑰：stderr 說明原因" || bad "stderr 沒有說明"
rm -rf "$ROOT"

# 2. 金鑰產得出來：照常印 JSON、exit 0、金鑰檔真的存在。
ROOT=$(mktemp -d)
OUT=$(AM_DEV_SSHD_DIR="$ROOT/d" sh "$HERE/dev-sshd.sh" ssh-opts 2>/dev/null); RC=$?
[ "$RC" -eq 0 ] && ok "正常：exit 0" || bad "正常卻 exit ${RC}"
case "$OUT" in '["-i","'"$ROOT"'/d/clientkey"'*) ok "正常：JSON 指到 clientkey" ;; *) bad "JSON 不對：${OUT}" ;; esac
[ -f "$ROOT/d/clientkey" ] && ok "正常：clientkey 真的存在" || bad "clientkey 不存在"
rm -rf "$ROOT"

echo "$PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
