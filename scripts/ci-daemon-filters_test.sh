#!/usr/bin/env bash
# scripts/ci-daemon-filters.sh 的路徑 → cargo test 過濾字串對應（2026-10-02 閘門審查）。純函式：stdin 檔案清單 → 過濾字串（一行一個）。
set -euo pipefail
cd "$(dirname "$0")/.."

fail=0
check() {
    local want="$1" got
    shift
    got="$(printf '%s\n' "$@" | bash scripts/ci-daemon-filters.sh | tr '\n' ' ' | sed 's/ $//')"
    if [ "$got" != "$want" ]; then
        printf 'FAIL: %s → got [%s] want [%s]\n' "$*" "$got" "$want" >&2
        fail=1
    fi
}

# 模組路徑：daemon/src/a/b.rs → a::b::；mod.rs 就是目錄本身；頂層 x.rs → x::。
check "lifecycle::queue::" "daemon/src/lifecycle/queue.rs"
check "api::" "daemon/src/api.rs"
check "supervisor::" "crates/am-supervisor/src/supervisor/mod.rs"
check "supervisor::controller::" "crates/am-supervisor/src/supervisor/controller.rs"
# 測試檔就在自己的模組底下（foo/tests.rs → foo::tests::，包含在 foo:: 裡）。
check "host_baseline::tests::" "daemon/src/host_baseline/tests.rs"
# Build inputs, crate wiring, shared test helpers, and include data can affect tests across modules.
check "__all__" "daemon/build.rs"
check "__all__" "Cargo.toml"
check "__all__" "Cargo.lock"
check "__all__" ".cargo/config.toml"
check "__all__" "rust-toolchain.toml"
check "__all__" "daemon/Cargo.toml"
check "__all__" "daemon/tests/fixtures/capture/claude/input.txt"
check "__all__" "daemon/src/testing.rs"
check "__all__" "daemon/src/test_home.rs"
check "__all__" "daemon/src/main.rs"
check "__all__" "daemon/src/lib.rs"
check "__all__" "crates/am-lifecycle/src/lifecycle/fixtures/claude-2.1.281-draft.ansi"
check "__all__" "crates/am-base/src/release_triage/rules.toml"
check "lifecycle::queue::" "crates/am-lifecycle/src/lifecycle/queue.rs"
check "codex_history::" "crates/am-lifecycle/src/codex_history.rs"
check "lifecycle::" "crates/am-lifecycle/src/lifecycle/mod.rs"
check "__all__" "crates/am-lifecycle/src/lifecycle/fixtures/capture.txt"
check "mission::ports_impl::" "crates/am-supervisor/src/mission/ports_impl.rs"
check "build_info::" "crates/am-supervisor/src/build_info.rs"
check "share::portal::" "crates/am-share/src/share/portal.rs"
check "share::" "crates/am-share/src/share/mod.rs"
check "__all__ lifecycle::queue::" "daemon/build.rs" "daemon/src/lifecycle/queue.rs"
# 多個檔案：去重、排序。
check "lifecycle::prompt:: lifecycle::queue::" "daemon/src/lifecycle/queue.rs" "daemon/src/lifecycle/prompt.rs" "daemon/src/lifecycle/queue.rs"
# daemon 以外、但 daemon 的測試會讀的檔案（跟 ci-changed-parts.sh 同一份清單）。
check "cargo_shim::" "scripts/check.sh"
check "herdr_shim::" "scripts/ops/lint-shell-vars.sh"
check "claude_review::" "scripts/ops/codex-release-task.md"
check "supervisor::setup::" "scripts/ops/fixtures/patrol-runtime.json"
check "supervisor::setup::" "scripts/agm.py"
check "supervisor::persona:: supervisor::responder:: supervisor::setup::" "docs/goals/agm-supervisor-persona.md" "docs/goals/agm-responder-persona.md"
# 跟 daemon 無關的檔案不產生過濾字串。
check "" "web/src/store/store.ts" "docs/SPEC.md" "scripts/ops/README.md"

if [ "$fail" = 0 ]; then echo "ci-daemon-filters: OK"; fi
exit "$fail"
