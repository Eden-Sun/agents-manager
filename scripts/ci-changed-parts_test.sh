#!/usr/bin/env bash
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

check "" "docs/FRONTEND.md" "README.md"
check "daemon" "docs/API.md"
check "web" "web/src/store/store.ts"
check "daemon" "daemon/src/api.rs"
check "daemon" "Cargo.lock"
check "ops" "scripts/ops/daemon-swap.sh"
check "daemon ops" "scripts/agm.py"
check "ob" "scripts/ob.py"
check "ops" ".claude/skills/verify/SKILL.md"
check "web daemon ops ob" "web/src/a.ts" "daemon/src/b.rs" "scripts/check.sh"
check "full" "web/src/a.ts" "somewhere/unknown.txt"
check "full" "xtask/src/main.rs"
check "" ""
check "daemon" "docs/goals/agm-supervisor-persona.md"
check "daemon" "docs/goals/agm-responder-persona.md"
check "daemon" "daemon/src/release_triage/fixtures/claude_2.1.276-278.md"
check "daemon" "crates/am-base/src/release_triage/fixtures/claude_2.1.276-278.md"
check "" "docs/goals/other-goal.md"
check "ops ob" "scripts/chatgpt-consult.sh"
check "ob" "scripts/chatgpt-consult.mjs"

check "web daemon ops ob" "scripts/check.sh"
check "daemon ops" "scripts/ops/lint-shell-vars.sh"
check "daemon ops" "scripts/ops/codex-release-task.md"
check "daemon ops" "scripts/ops/release-triage-task.md"
check "daemon ops" "scripts/ops/fixtures/patrol-runtime.json"
check "ops" "scripts/ops/README.md"
check "ops" "scripts/ops/browser-gc-task.md"
check "web daemon" "crates/am-lifecycle/src/lifecycle/fixtures/codex-0.155-draft.ansi"
check "daemon" "daemon/src/release_triage/fixtures/claude_2.1.276-278.md"
for f in crates/am-base/src/db.rs crates/am-supervisor/src/supervisor/store.rs crates/am-supervisor/src/supervisor/roles.rs crates/am-supervisor/src/mission/store.rs; do
    check "daemon ops" "$f"
done
check "daemon" "daemon/src/api.rs"

check "daemon ops" "ops/host-baseline/baseline.toml"
check "daemon ops" "scripts/project-cargo.sh"
check "daemon ops" "scripts/ops/host-baseline-sync-mac-verify.sh"
check "web daemon" "web/src/lib/outboxList.ts"
check "web daemon" "web/src/lib/__fixtures__/codex-async-screen-cases.json"
check "web daemon" "web/src/lib/__fixtures__/codex-async-ordinary-menu.txt"
check "web" "web/src/lib/__fixtures__/other.json"

scanned=0
while IFS=$'\t' read -r file p; do
    scanned=$((scanned + 1))
    got="$(printf '%s\n' "$p" | bash scripts/ci-changed-parts.sh)"
    if ! grep -qx -e daemon -e full <<<"$got"; then
        printf 'FAIL: %s 讀 %s，但只分到 [%s]（要含 daemon；補進 ci-changed-parts.sh 的跨界清單）\n' "$file" "$p" "$(echo $got)" >&2
        fail=1
    fi
done < <(python3 -B - <<'PY'
import os, re
pat = re.compile(r'include_str!\("([^"]+)"\)|CARGO_MANIFEST_DIR"\)\)\.join\("([^"]+)"\)')
for top in ("daemon", "crates"):
    for d, _, names in os.walk(top):
        for n in names:
            if not n.endswith(".rs"):
                continue
            f = os.path.join(d, n)
            crate = d
            while not os.path.isfile(os.path.join(crate, "Cargo.toml")):
                crate = os.path.dirname(crate)
            for inc, joined in pat.findall(open(f, encoding="utf-8").read()):
                for base in ([d] if inc else [crate, "daemon"]):
                    p = os.path.relpath(os.path.normpath(os.path.join(base, inc or joined)))
                    if os.path.isfile(p) and p.split(os.sep)[0] in ("web", "ops", "scripts", "docs"):
                        print(f"{f}\t{p}")
PY
)
if [ "$scanned" -lt 5 ]; then
    echo "FAIL: 只掃到 ${scanned} 個跨界讀取，掃描規則大概壞了" >&2
    fail=1
fi

check "" "docs/SPEC.md"
check "daemon" "docs/SPEC.md" "docs/API.md"
check "daemon" "docs/SPEC.md" "daemon/src/api.rs"
check "ops" "docs/SPEC.md" "scripts/ops/README.md"
check "ops" "AGENTS.md"
check "ops" "AGENTS.md" "docs/SPEC.md"
check "" "README.md"

for f in web/vite.config.ts web/tsconfig.app.json web/tsconfig.json web/tsconfig.node.json web/package.json web/bun.lock web/bunfig.toml web/test/node-test-shim.ts web/index.html web/public/x.svg; do
    check "web" "$f"
done

if [ "$fail" = 0 ]; then echo "ci-changed-parts: OK"; fi
exit "$fail"
