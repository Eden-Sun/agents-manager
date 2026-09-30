#!/usr/bin/env bash
# Clean-checkout daemon fast path: rust-embed needs an index.html, but should not trigger web CI.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
tmp="$(mktemp -d "${TMPDIR:-/tmp}/am-check-changed-daemon.XXXXXX")"
trap '/bin/rm -rf "$tmp"' EXIT

fixture="$tmp/repo"
mkdir -p "$fixture/scripts" "$fixture/web" "$fixture/bin"
cp "$ROOT/scripts/check.sh" "$fixture/scripts/check.sh"
cp "$ROOT/scripts/ci-changed-parts.sh" "$fixture/scripts/ci-changed-parts.sh"

cat >"$fixture/bin/git" <<'SH'
#!/usr/bin/env bash
case "$*" in
    "diff --name-only origin/main...HEAD") ;;
    "diff --name-only HEAD") printf 'daemon/src/main.rs\n' ;;
    "ls-files --others --exclude-standard") ;;
    *) printf 'unexpected git invocation: %s\n' "$*" >&2; exit 90 ;;
esac
SH

cat >"$fixture/bin/bun" <<'SH'
#!/usr/bin/env bash
printf '%s\n' "$*" >>"$AM_TEST_BUN_LOG"
SH

cat >"$fixture/bin/bunx" <<'SH'
#!/usr/bin/env bash
printf 'bunx %s\n' "$*" >>"$AM_TEST_BUN_LOG"
SH

cat >"$fixture/bin/cargo" <<'SH'
#!/usr/bin/env bash
printf '%s\n' "$*" >>"$AM_TEST_CARGO_LOG"
if [ -f web/dist/index.html ]; then
    printf 'web-dist-stub-present\n' >>"$AM_TEST_CARGO_LOG"
else
    printf 'web-dist-stub-missing\n' >>"$AM_TEST_CARGO_LOG"
fi
SH

chmod +x "$fixture/bin/git" "$fixture/bin/bun" "$fixture/bin/bunx" "$fixture/bin/cargo"
export AM_TEST_BUN_LOG="$tmp/bun.log"
export AM_TEST_CARGO_LOG="$tmp/cargo.log"

if ! output="$(cd "$fixture" && PATH="$fixture/bin:$PATH" bash scripts/check.sh changed origin/main 2>&1)"; then
    printf 'changed daemon fast path failed:\n%s\n' "$output" >&2
    exit 1
fi
if [ -s "$AM_TEST_BUN_LOG" ]; then
    printf 'daemon changed path invoked web checks:\n%s\n' "$(cat "$AM_TEST_BUN_LOG")" >&2
    printf '%s\n' "$output" >&2
    exit 1
fi
if ! grep -q '^web-dist-stub-present$' "$AM_TEST_CARGO_LOG"; then
    printf 'cargo check did not receive a temporary rust-embed stub:\n%s\n' "$(cat "$AM_TEST_CARGO_LOG" 2>/dev/null || true)" >&2
    exit 1
fi
if [ -e "$fixture/web/dist/index.html" ]; then
    echo 'changed daemon fast path left its temporary web/dist stub behind' >&2
    exit 1
fi

echo 'changed daemon fast path uses and cleans a temporary web/dist stub'
