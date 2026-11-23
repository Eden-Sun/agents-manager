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
check "$(printf '%s\nscripts/ops/claude_binary_diff_test.sh\n' "$guards" | sort)" scripts/ops/claude_binary_diff.py
check "$(printf '%s\nscripts/ops/project-transfer_test.sh\n' "$guards" | sort)" scripts/ops/project-transfer_test.py
check "$(printf '%s\nscripts/lint_scope_test.sh\n' "$guards" | sort)" AGENTS.md
check "$(printf '%s\nscripts/ops/cutover-helper_test.sh\n' "$guards" | sort)" scripts/ops/cutover-helper.py
check "$(printf '%s\nscripts/wait-load_test.sh\n' "$guards" | sort)" scripts/wait-load.sh
check "$(printf '%s\nscripts/agm_test.py\n' "$guards" | sort)" scripts/agm.py
check "$(printf '%s\nscripts/ops/cutover-to-host_test.sh\n' "$guards" | sort)" scripts/ops/host-state-transfer.py scripts/ops/host-state-transfer_test.py
check "$(printf '%s\nscripts/ops/build-web-bundle_test.sh\n' "$guards" | sort)" scripts/ops/build-web-bundle.py
check "$(printf '%s\nscripts/ops/browser-gc-linux_test.sh\n' "$guards" | sort)" scripts/ops/browser_gc_linux_test.py
check "$(printf '%s\nscripts/ops/destructive-canary_test.sh\n' "$guards" | sort)" scripts/ops/canary-baseline.tsv
for path in scripts/check.sh scripts/ops/destructive-canary.sh scripts/ops/install-manifest.tsv scripts/ops/daemon-start.py scripts/ops/unknown.sh scripts/ci-ops-tests.py scripts/ci-changed-parts.sh scripts/ops/README.md scripts/ops/launchd/com.agm.outbox-gc.plist scripts/ops/systemd/com.agm.outbox-gc.service; do
    check __all__ "$path"
done
check __all__ scripts/ops/outbox-gc.sh scripts/ops/unknown.sh
check "$(printf '%s\nscripts/ops/project-transfer_test.sh\n' "$guards" | sort)" scripts/ops/project-transfer scripts/ops/project-transfer_test.py
check "$(printf '%s\nscripts/ops/daemon-swap_test.sh\nscripts/ops/project-transfer_test.sh\n' "$guards" | sort)" crates/am-base/src/db.rs
check "$(printf '%s\nscripts/ops/daemon-swap_test.sh\nscripts/ops/outbox-gc_test.sh\nscripts/ops/project-transfer_test.sh\n' "$guards" | sort)" scripts/ops/outbox-gc.sh crates/am-base/src/db.rs
check __all__ crates/am-base/src/db.rs scripts/ci-daemon-filters.sh
tmp=$(mktemp -d)
trap '/bin/rm -rf "$tmp"' EXIT
deleted_entry=$(cd "$tmp" && printf 'scripts/ops/outbox-gc.sh\n' | python3 -B "$selector")
grep -qx __all__ <<<"$deleted_entry"
mkdir -p "$tmp/crates/am-base/src" "$tmp/scripts/ops"
touch "$tmp/crates/am-base/src/db.rs" "$tmp/scripts/ops/daemon-swap_test.sh"
missing_suite=$(cd "$tmp" && printf 'crates/am-base/src/db.rs\n' | python3 -B "$selector")
grep -qx __all__ <<<"$missing_suite"
fx="$tmp/repo"
mkdir -p "$fx/scripts/ops" "$fx/bin"
cp scripts/check.sh scripts/ci-ops-tests.py scripts/ci-rules.tsv "$fx/scripts/"
cp scripts/ops/ops-jobs.sh "$fx/scripts/ops/"
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
grep -q selected-outbox <<<"$selected"
grep -q guard-test_discovery <<<"$selected"
if grep -Eq 'unrelated-suite|agm-suite' <<<"$selected"; then echo 'FAIL: unrelated suite ran'; exit 1; fi
full=$(run_changed scripts/check.sh)
grep -q unrelated-suite <<<"$full"
grep -q agm-suite <<<"$full"
grep -q 'ops 測試：選中 ' <<<"$selected"
grep -q 'ops 測試：__all__' <<<"$full"
agents_only=$(printf '%s\n' AGENTS.md | python3 -B "$selector")
grep -qx scripts/lint_scope_test.sh <<<"$agents_only"
if grep -qx __all__ <<<"$agents_only"; then echo 'FAIL: AGENTS.md alone ran everything'; exit 1; fi
mixed_agents=$(printf '%s\n' AGENTS.md scripts/ops/unknown.sh | python3 -B "$selector")
grep -qx __all__ <<<"$mixed_agents"
cat > "$fx/bin/git" <<'SH'
#!/bin/bash
case "$*" in
 '-c core.quotePath=false diff --no-renames --name-only full-success pr-head') echo scripts/ops/outbox-gc.sh ;;
 '-c core.quotePath=false diff --no-renames --name-only full-success mixed-head') printf '%s\n' scripts/ops/outbox-gc.sh scripts/ops/unknown.sh ;;
 '-c core.quotePath=false diff --no-renames --name-only full-success docs-main') echo docs/x.md ;;
 '-c core.quotePath=false diff --no-renames --name-only full-success shared-main') echo scripts/check.sh ;;
 *) exit 90 ;;
esac
SH
run_ops() { (cd "$fx" && PATH="$fx/bin:$PATH" bash scripts/check.sh ops "$@"); }
selected=$(run_ops full-success pr-head)
grep -q selected-outbox <<<"$selected"
for guard in rust_test_mount db_filename test_discovery; do
    grep -q "guard-$guard" <<<"$selected"
done
if grep -Eq 'unrelated-suite|agm-suite' <<<"$selected"; then echo 'FAIL: PR ran unrelated suite'; exit 1; fi
selected=$(run_ops full-success docs-main pr-head)
grep -q selected-outbox <<<"$selected"
if grep -Eq 'unrelated-suite|agm-suite' <<<"$selected"; then echo 'FAIL: docs-only main widened selection'; exit 1; fi
for args in 'full-success missing-head' 'full-success mixed-head' 'full-success shared-main pr-head' 'full-success docs-main missing-head' ''; do
    full=$(run_ops $args)
    grep -q unrelated-suite <<<"$full"
    grep -q agm-suite <<<"$full"
done
echo 'ci-ops-tests: selection, aliases, fallbacks and real check.sh dispatch passed'
