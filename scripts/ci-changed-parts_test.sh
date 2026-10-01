#!/usr/bin/env bash
# scripts/ci-changed-parts.sh 的路徑對應（issue #716）。純函式：stdin 檔案清單 → 要跑的部分。
set -euo pipefail
cd "$(dirname "$0")/.."

fail=0
check() {
    local want="$1" got
    shift
    got="$(printf '%s\n' "$@" | bash scripts/ci-changed-parts.sh | tr '\n' ' ' | sed 's/ $//')"
    if [ "$got" != "$want" ]; then
        printf 'FAIL: %s → got [%s] want [%s]\n' "$*" "$got" "$want" >&2
        fail=1
    fi
}

check "" "docs/SPEC.md" "README.md"
check "web" "web/src/store/store.ts"
check "daemon" "daemon/src/api.rs"
check "daemon" "Cargo.lock"
check "ops" "scripts/ops/daemon-swap.sh"
check "daemon ops" "scripts/agm.py"
check "ob" "scripts/ob.py"
check "ops" ".claude/skills/verify/SKILL.md"
check "web daemon ops" "web/src/a.ts" "daemon/src/b.rs" "scripts/check.sh"
check "full" "web/src/a.ts" "somewhere/unknown.txt"
check "" ""

if [ "$fail" = 0 ]; then echo "ci-changed-parts: OK"; fi
exit "$fail"
