#!/bin/sh
# Development Cargo entrypoint. Re-enter the managed Cargo shim so Blaze keeps its build slot.
set -eu
engine=${AGM_RUST_ENGINE:-auto}
case "$engine" in auto|blaze|cargo) ;; *) echo "project-cargo: AGM_RUST_ENGINE must be auto, blaze or cargo" >&2; exit 2 ;; esac
AGM_RUST_DISPATCHED=$$
export AGM_RUST_DISPATCHED
sub=${1:-}
use_blaze=0
case "$sub" in build|run) use_blaze=1 ;; esac
# Only the default development profiles use the JIT. Compiler tools, explicit
# toolchains, cross targets and release/custom profiles retain their original Cargo semantics.
for arg in "$@"; do
    [ "$arg" != -- ] || break
    case "$arg" in --release|-r|--profile|--profile=*|--target|--target=*|--message-format|--message-format=*|-Z*) use_blaze=0 ;; esac
done
[ "$engine" != cargo ] || use_blaze=0
if [ "$use_blaze" = 0 ]; then exec cargo "$@"; fi
host_os=$(uname -s)
case "$host_os:$(uname -m)" in Linux:x86_64|Linux:aarch64|Darwin:arm64) ;; *)
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
# The pre-rename nightly ignores MEMORY_MAX. Only a capability-checked installation
# may start a daemon; unsupported tools must never silently use an unbounded budget.
plugin_path=$(command -v "cargo-$plugin")
engine_path="$(dirname "$plugin_path")/ra-jit"
capability="$(dirname "$plugin_path")/.agm-blaze-memory-v1"
if [ ! -f "$capability" ] || [ ! -x "$engine_path" ] || [ "$engine_path" -nt "$capability" ]; then
    if [ "$engine" = blaze ]; then echo "project-cargo: a memory-budget-capable Blaze is required; run scripts/install-blaze.sh --source" >&2; exit 127; fi
    echo "project-cargo: Blaze memory-budget support is unverified; using Cargo (install: scripts/install-blaze.sh --source)" >&2
    exec cargo "$@"
fi
RUSTUP_TOOLCHAIN=1.99.0
export RUSTUP_TOOLCHAIN
# A project-owned namespace prevents an existing, unbounded user daemon from
# silently ignoring these startup settings. All project worktrees share it.
RA_JIT_HOME=${AGM_BLAZE_RUNTIME_HOME:-${HOME}/.cache/agents-manager/blaze}
RA_JIT_MEMORY_MAX=${AGM_BLAZE_MEMORY_MAX:-8G}
RA_JIT_IDLE_SECS=${AGM_BLAZE_IDLE_SECS:-600}
BLAZE_MEMORY_MAX=$RA_JIT_MEMORY_MAX
BLAZE_IDLE_SECS=$RA_JIT_IDLE_SECS
export RA_JIT_HOME RA_JIT_MEMORY_MAX RA_JIT_IDLE_SECS BLAZE_MEMORY_MAX BLAZE_IDLE_SECS
# No retry on failure: a compiler error or failed test must remain a failure.
exec cargo "$plugin" "$@"
