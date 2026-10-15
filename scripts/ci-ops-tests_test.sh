#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
selector="$PWD/scripts/ci-ops-tests.py"
guards=$'scripts/db_filename_test.sh\nscripts/rust_test_mount_test.sh\nscripts/test_discovery_test.sh'
check() {
    local expected="$1" actual
    shift
    actual=$(printf '%s\n' "$@" | python3 -B "$selector")
    [ "$actual" = "$expected" ] || { printf 'FAIL: %s → [%s], expected [%s]\n' "$*" "$actual" "$expected"; exit 1; }
}
check "$(printf '%s\nscripts/ops/outbox-gc_test.sh\n' "$guards" | sort)" scripts/ops/outbox-gc.sh
check "$(printf '%s\nscripts/ops/outbox-gc_test.sh\nscripts/ops/pane-gc_test.sh\n' "$guards" | sort)" scripts/ops/outbox-gc.sh scripts/ops/pane-gc.sh web/src/x.ts
check "$(printf '%s\nscripts/ops/browser-gc-linux_test.sh\n' "$guards" | sort)" scripts/ops/browser_gc_linux.py
check "$(printf '%s\nscripts/ops/project-transfer_test.sh\n' "$guards" | sort)" scripts/ops/project-transfer_test.py
for path in scripts/check.sh scripts/ops/destructive-canary.sh scripts/ops/install-manifest.tsv scripts/ops/cutover-helper.py scripts/ops/daemon-start.py scripts/wait-load.sh scripts/ops/unknown.sh scripts/ci-ops-tests.py AGENTS.md; do
    check __all__ "$path"
done
check __all__ scripts/ops/outbox-gc.sh scripts/ops/unknown.sh
check __all__ scripts/ops/outbox-gc.sh crates/am-base/src/db.rs
# A deleted entrypoint or missing suite must never silently skip the suite.
tmp=$(mktemp -d)
trap '/bin/rm -rf "$tmp"' EXIT
(cd "$tmp" && printf 'scripts/ops/outbox-gc.sh\n' | python3 -B "$selector") | grep -qx __all__
# Run check.sh's real dispatch against harmless suites, not just the selector in isolation.
fx="$tmp/repo"
mkdir -p "$fx/scripts/ops" "$fx/bin"
cp scripts/check.sh scripts/ci-ops-tests.py "$fx/scripts/"
printf '#!/bin/bash\necho ops\n' > "$fx/scripts/ci-changed-parts.sh"
printf '#!/bin/bash\ntrue\n' > "$fx/scripts/ops/lint-shell-vars.sh"
for name in rust_test_mount db_filename test_discovery; do
    printf '#!/bin/bash\necho guard-%s\n' "$name" > "$fx/scripts/${name}_test.sh"
done
printf '#!/bin/bash\necho selected-outbox\n' > "$fx/scripts/ops/outbox-gc_test.sh"
printf '#!/bin/bash\ntrue\n' > "$fx/scripts/ops/outbox-gc.sh"
printf '#!/bin/bash\necho unrelated-suite\n' > "$fx/scripts/ops/other_test.sh"
printf 'print("agm-suite")\n' > "$fx/scripts/agm_test.py"
cat > "$fx/scripts/ops/destructive-canary.sh" <<'SH'
#!/bin/bash
case "$1" in
 arm) mkdir -p "$2"; touch "$2/am-canary-probe"; chmod +x "$2/am-canary-probe"; printf 'export AM_CANARY_DIR=%s\n' "$2" ;;
esac
SH
cat > "$fx/bin/git" <<'SH'
#!/bin/bash
case "$*" in
 'diff --name-only HEAD') echo "$AM_OPS_TEST_PATH" ;;
 'diff --name-only origin/main...HEAD' | 'ls-files --others --exclude-standard') ;;
 *) exit 90 ;;
esac
SH
printf '#!/bin/bash\nexit 0\n' > "$fx/bin/bun"
chmod +x "$fx/bin/"* "$fx/scripts/ops/"*.sh
run_changed() { (cd "$fx" && PATH="$fx/bin:$PATH" AM_OPS_TEST_PATH="$1" bash scripts/check.sh changed); }
selected=$(run_changed scripts/ops/outbox-gc.sh)
printf '%s\n' "$selected" | grep -q selected-outbox
printf '%s\n' "$selected" | grep -q guard-test_discovery
if printf '%s\n' "$selected" | grep -Eq 'unrelated-suite|agm-suite'; then echo 'FAIL: unrelated suite ran'; exit 1; fi
full=$(run_changed scripts/check.sh)
printf '%s\n' "$full" | grep -q unrelated-suite
printf '%s\n' "$full" | grep -q agm-suite
echo 'ci-ops-tests: selection, aliases, fallbacks and real check.sh dispatch passed'
