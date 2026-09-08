#!/usr/bin/env bash
# Run the same checks locally and in CI.
#
# The web build runs before the daemon checks because the daemon's default `embed-ui` feature
# embeds web/dist at compile time. `--daemon-only` therefore expects web/dist/index.html to
# already exist (the CI daemon job downloads it from the web job).
#
# The current daemon baseline is 32 clippy warnings, so this invokes clippy without `-D
# warnings` for now. Reduce that baseline before making warnings fatal in CI.

set -u -o pipefail

ROOT=$(CDPATH= cd -- "$(dirname -- "${BASH_SOURCE[0]}")/.." && pwd)

usage() {
    echo "usage: $0 [--web-only|--daemon-only]" >&2
}

run_step() {
    local name=$1
    shift
    printf '\n==> %s\n' "$name"
    if "$@"; then
        printf 'PASS: %s\n' "$name"
    else
        local status=$?
        printf 'FAIL: %s (exit %s)\n' "$name" "$status" >&2
        exit "$status"
    fi
}

web_checks() {
    local -a webx web_run install
    if command -v bun >/dev/null 2>&1; then
        webx=(bunx)
        web_run=(bun run)
        install=(bun install --frozen-lockfile)
    else
        echo "bun not found; using npx/npm fallback" >&2
        webx=(npx --no-install)
        web_run=(npm run)
        install=(npm install --no-package-lock)
    fi

    cd "$ROOT/web" || exit 1
    run_step "web dependencies" "${install[@]}"
    run_step "web TypeScript" "${webx[@]}" tsc -p tsconfig.app.json --noEmit
    run_step "web oxlint" "${webx[@]}" oxlint src
    run_step "web build" "${web_run[@]}" build
}

daemon_checks() {
    if [ ! -f "$ROOT/web/dist/index.html" ]; then
        printf 'FAIL: daemon checks require web/dist/index.html; run the web checks first\n' >&2
        exit 1
    fi
    cd "$ROOT" || exit 1
    run_step "daemon build" cargo build --release -p agents-managerd
    run_step "daemon tests" cargo test -p agents-managerd
    run_step "daemon clippy" cargo clippy -p agents-managerd
}

mode=all
case "${1:-}" in
    "") ;;
    --web-only) mode=web ;;
    --daemon-only) mode=daemon ;;
    *) usage; exit 2 ;;
esac

case "$mode" in
    web) web_checks ;;
    daemon) daemon_checks ;;
    all) web_checks ; daemon_checks ;;
esac

printf '\nAll requested checks passed.\n'
