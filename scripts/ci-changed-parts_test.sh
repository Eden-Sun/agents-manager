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
# 被 daemon include_str! 編進去的 .md／docs（supervisor::persona::BUILD_INPUTS）不是「只有文件」：改名、刪掉＝編不過。
check "daemon" "docs/goals/agm-supervisor-persona.md"
check "daemon" "docs/goals/agm-responder-persona.md"
check "daemon" "daemon/src/release_triage/fixtures/claude_2.1.276-278.md"
check "" "docs/goals/other-goal.md"
# chatgpt-consult.sh 是 scripts/ 底下的 shell：lint-shell-vars 與 canary 在 ops，不能只跑 ob。
check "ops ob" "scripts/chatgpt-consult.sh"
check "ob" "scripts/chatgpt-consult.mjs"

if [ "$fail" = 0 ]; then echo "ci-changed-parts: OK"; fi
exit "$fail"
