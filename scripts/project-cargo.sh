#!/bin/sh
# Development Cargo entrypoint. Re-enter the managed Cargo shim so Blaze keeps its build slot.
set -eu
engine=${AGM_RUST_ENGINE:-auto}
case "$engine" in auto|blaze|cargo) ;; *) echo "project-cargo: AGM_RUST_ENGINE must be auto, blaze or cargo" >&2; exit 2 ;; esac
AGM_RUST_DISPATCHED=$$
export AGM_RUST_DISPATCHED
sub=${1:-}
use_blaze=0
case "$sub" in build|run|test) use_blaze=1 ;; esac
# Only the default development/test profiles use the JIT. Compiler tools, explicit
# toolchains, cross targets and release/custom profiles retain their original Cargo semantics.
for arg in "$@"; do
    [ "$arg" != -- ] || break
    case "$arg" in --release|-r|--profile|--profile=*|--target|--target=*|--message-format|--message-format=*|-Z*) use_blaze=0 ;; esac
done
[ "$engine" != cargo ] || use_blaze=0
if [ "$use_blaze" = 0 ]; then exec cargo "$@"; fi
case "$(uname -s):$(uname -m)" in Linux:x86_64|Linux:aarch64|Darwin:arm64) ;; *)
    if [ "$engine" = blaze ]; then echo "project-cargo: Blaze does not support this host" >&2; exit 2; fi
    echo "project-cargo: unsupported Blaze host; using Cargo" >&2
    exec cargo "$@"
    ;;
esac
# Append the private installation: never put a second Cargo ahead of the managed shim.
blaze_bin=${AGM_BLAZE_BIN:-${HOME}/.local/share/agents-manager/blaze/bin}
PATH="${PATH}:${blaze_bin}"
export PATH
# The published engine uses Rust 1.99.0's ABI. Never splice its code into another rustc's output.
if ! command -v rustup >/dev/null 2>&1; then
    PATH="${PATH}:${HOME}/.cargo/bin"
    export PATH
fi
if ! rustup run 1.99.0 rustc --version >/dev/null 2>&1; then
    if [ "$engine" = blaze ]; then echo "project-cargo: Rust 1.99.0 is required; run scripts/install-blaze.sh" >&2; exit 127; fi
    echo "project-cargo: Blaze needs Rust 1.99.0; using Cargo (install: scripts/install-blaze.sh)" >&2
    exec cargo "$@"
fi
if command -v cargo-blaze >/dev/null 2>&1; then plugin=blaze
elif command -v cargo-ra-jit >/dev/null 2>&1; then plugin=ra-jit
else
    if [ "$engine" = blaze ]; then echo "project-cargo: install Blaze with scripts/install-blaze.sh" >&2; exit 127; fi
    echo "project-cargo: Blaze is not installed; using Cargo (install: scripts/install-blaze.sh)" >&2
    exec cargo "$@"
fi
RUSTUP_TOOLCHAIN=1.99.0
export RUSTUP_TOOLCHAIN
# Always run the complete selected test suite; do not inherit affected-tests-only settings.
if [ "$sub" = test ]; then
    RA_JIT_TESTS=all
    BLAZE_TESTS=all
    export RA_JIT_TESTS BLAZE_TESTS
fi
RUST_TEST_THREADS=${RUST_TEST_THREADS:-8}
export RUST_TEST_THREADS
# No retry on failure: a compiler error or failed test must remain a failure.
exec cargo "$plugin" "$@"
