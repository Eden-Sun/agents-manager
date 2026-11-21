#!/bin/bash
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
PASS=0
FAIL=0
ok() { echo "ok   - $1"; PASS=$((PASS + 1)); }
bad() { echo "FAIL - $1"; FAIL=$((FAIL + 1)); }

ROOT=$(mktemp -d)
touch "$ROOT/notadir"
OUT=$(AM_DEV_SSHD_DIR="$ROOT/notadir/sub" sh "$HERE/dev-sshd.sh" ssh-opts 2>"$ROOT/err"); RC=$?
[ "$RC" -ne 0 ] && ok "產不出金鑰：exit 非 0" || bad "產不出金鑰卻 exit 0（輸出：${OUT}）"
[ -z "$OUT" ] && ok "產不出金鑰：stdout 不印 JSON" || bad "stdout 不該有東西：${OUT}"
grep -q 'ssh-opts' "$ROOT/err" && ok "產不出金鑰：stderr 說明原因" || bad "stderr 沒有說明"
rm -rf "$ROOT"

ROOT=$(mktemp -d)
touch "$ROOT/notadir"
OUT=$(AM_DEV_SSHD_DIR="$ROOT/notadir/sub" sh "$HERE/dev-sshd.sh" start 2>"$ROOT/err"); RC=$?
[ "$RC" -ne 0 ] && ok "start 產不出金鑰：exit 非 0" || bad "start 產不出金鑰卻 exit 0（輸出：${OUT}）"
[ -z "$OUT" ] && ok "start 產不出金鑰：stdout 不印 JSON" || bad "start stdout 不該有東西：${OUT}"
grep -q 'could not create keys' "$ROOT/err" && ok "start 產不出金鑰：stderr 說明原因" || bad "start stderr 沒有說明：$(cat "$ROOT/err")"
rm -rf "$ROOT"

ROOT=$(mktemp -d); mkdir -p "$ROOT/d"
sleep 300 & VICTIM=$!
echo "$VICTIM" > "$ROOT/d/sshd.pid"
OUT=$(AM_DEV_SSHD_DIR="$ROOT/d" sh "$HERE/dev-sshd.sh" status 2>&1); RC=$?
[ "$RC" -eq 1 ] && ok "pid 被別的行程占用：status exit 1" || bad "status 卻 exit ${RC}（${OUT}）"
[ "$OUT" = "stopped" ] && ok "pid 被別的行程占用：status 說 stopped" || bad "status 輸出不對：${OUT}"
OUT=$(AM_DEV_SSHD_DIR="$ROOT/d" sh "$HERE/dev-sshd.sh" stop 2>&1)
[ "$OUT" = "dev sshd is not running" ] && ok "stop 不把別人的行程當 sshd" || bad "stop 輸出不對：${OUT}"
kill -0 "$VICTIM" 2>/dev/null && ok "那個不相干的行程還活著" || bad "stop 把不相干的行程殺掉了"
kill "$VICTIM" 2>/dev/null; wait "$VICTIM" 2>/dev/null
echo "abc" > "$ROOT/d/sshd.pid"
OUT=$(AM_DEV_SSHD_DIR="$ROOT/d" sh "$HERE/dev-sshd.sh" status 2>&1); RC=$?
[ "$RC" -eq 1 ] && [ "$OUT" = "stopped" ] && ok "pid 檔不是數字：status exit 1、說 stopped" || bad "pid 檔是 abc：exit ${RC}，輸出 ${OUT}"
rm -rf "$ROOT"

if ! command -v ssh-keygen >/dev/null 2>&1; then
  echo "skip - 金鑰產得出來：這台沒有 ssh-keygen"
  echo "$PASS passed, $FAIL failed（1 組 skip）"
  [ "$FAIL" -eq 0 ]; exit
fi
ROOT=$(mktemp -d)
OUT=$(AM_DEV_SSHD_DIR="$ROOT/d" sh "$HERE/dev-sshd.sh" ssh-opts 2>/dev/null); RC=$?
[ "$RC" -eq 0 ] && ok "正常：exit 0" || bad "正常卻 exit ${RC}"
case "$OUT" in '["-i","'"$ROOT"'/d/clientkey"'*) ok "正常：JSON 指到 clientkey" ;; *) bad "JSON 不對：${OUT}" ;; esac
[ -f "$ROOT/d/clientkey" ] && ok "正常：clientkey 真的存在" || bad "clientkey 不存在"
rm -rf "$ROOT"

echo "$PASS passed, $FAIL failed"
[ "$FAIL" -eq 0 ]
