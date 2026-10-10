#!/usr/bin/env bash
# clippy／fmt 的範圍要涵蓋整個 workspace（issue #1077）：只帶 `-p agents-managerd` 時，拆出去的 am-base／am-config／
# am-core／am-ports 從此沒人 lint，報告數字也只算 daemon。desktop 不在範圍內（tauri build script 要 sidecar）。
#
#   bash scripts/lint_scope_test.sh
set -u
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
PASS=0
FAIL=0
ok() { echo "ok   - $1"; PASS=$((PASS + 1)); }
bad() { echo "FAIL - $1"; FAIL=$((FAIL + 1)); }

check_sh=$(grep -h '^ *cargo clippy' "$ROOT/scripts/check.sh" | head -1)
case "$check_sh" in
  *"--workspace --exclude agents-manager-desktop"*) ok "check.sh clippy 涵蓋 workspace（不含 desktop）" ;;
  *) bad "check.sh clippy 範圍不是 workspace：$check_sh" ;;
esac

fmt_sh=$(grep -h '^ *cargo fmt' "$ROOT/scripts/check.sh" | head -1)
case "$fmt_sh" in
  *"fmt --all"*) ok "check.sh fmt 涵蓋 workspace" ;;
  *) bad "check.sh fmt 範圍不是 workspace：$fmt_sh" ;;
esac

# CI（.github/workflows/ci.yml）的同一組範圍斷言先不在這裡：那個檔的改動需要有 workflow scope 的人套用，
# 套用之後再把 CI clippy／CI fmt 兩條加回來（見 #1077 的留言）。

if grep -q -- '--workspace --exclude agents-manager-desktop --lib --bins' "$ROOT/AGENTS.md"; then
  ok "AGENTS.md 的 clippy 指令涵蓋 workspace"
else
  bad "AGENTS.md 還只寫 cargo clippy -p agents-managerd（跟實際範圍不一致）"
fi

echo "lint_scope: ${PASS} passed, ${FAIL} failed"
[ "$FAIL" -eq 0 ]
