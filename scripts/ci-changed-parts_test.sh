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

check "" "docs/API.md" "README.md"
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

# 2026-10-02 閘門審查：不是 daemon/ 底下、但 daemon 的測試會在執行期讀的檔案，改了就要跑 daemon 的測試。
#   cargo_shim 的測試讀 scripts/check.sh、herdr_shim 的讀 scripts/ops/lint-shell-vars.sh、claude_review 的讀 scripts/ops/*-release-task.md、
#   supervisor::setup 的讀 scripts/ops/fixtures/patrol-runtime.json。
check "daemon ops" "scripts/check.sh"
check "daemon ops" "scripts/ops/lint-shell-vars.sh"
check "daemon ops" "scripts/ops/codex-release-task.md"
check "daemon ops" "scripts/ops/fixtures/patrol-runtime.json"
# scripts/ 底下的 .md 不是「只有文件」：任務檔被 install-manifest／ops 測試／daemon 讀（browser-gc-task.md 等），README 也由 ops 測試釘住內容。
check "ops" "scripts/ops/README.md"
check "ops" "scripts/ops/browser-gc-task.md"
# 反方向：web 的測試讀 daemon 的 fixtures；ops 的 project-transfer 測試從 daemon 的 schema 原始碼抽欄位。
check "web daemon" "daemon/src/lifecycle/fixtures/codex-0.155-draft.ansi"
check "web daemon" "crates/am-lifecycle/src/lifecycle/fixtures/codex-0.155-draft.ansi"
check "daemon" "daemon/src/release_triage/fixtures/claude_2.1.276-278.md"
for f in crates/am-base/src/db.rs daemon/src/supervisor/store.rs daemon/src/supervisor/roles.rs daemon/src/mission/store.rs; do
    check "daemon ops" "$f"
done
check "daemon" "daemon/src/api.rs"

# 有測試釘住內容的文件：docs/SPEC.md（scripts/jev-role_test.sh 要求裡面有 Jev 角色政策那幾句）。
# 只改這份文件不必跑整包 ops，單獨一個 `specs` 部分只跑那支契約測試。
check "specs" "docs/SPEC.md"
check "specs" "docs/SPEC.md" "docs/API.md"
check "daemon specs" "docs/SPEC.md" "daemon/src/api.rs"
check "ops specs" "docs/SPEC.md" "scripts/ops/README.md"

# web 的設定檔改了＝web 整套（build 與全部測試）：vite／tsconfig／package.json／鎖檔／bunfig／測試墊片／靜態資源。
for f in web/vite.config.ts web/tsconfig.app.json web/tsconfig.json web/tsconfig.node.json web/package.json web/bun.lock web/bunfig.toml web/test/node-test-shim.ts web/index.html web/public/x.svg; do
    check "web" "$f"
done

if [ "$fail" = 0 ]; then echo "ci-changed-parts: OK"; fi
exit "$fail"
