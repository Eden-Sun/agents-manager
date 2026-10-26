#!/bin/sh
# Install verified tools privately; never alter user Cargo configuration.
set -eu
blaze_home=${AGM_BLAZE_INSTALL_HOME:-${HOME}/.local/share/agents-manager/blaze}
revision=c8e5f6a88ae3eb61179d3fd20bd6afc871ca48e5
work=$(mktemp -d "${TMPDIR:-/tmp}/agm-blaze-install.XXXXXX")
trap 'rm -r "$work"' EXIT
trap 'exit 130' INT TERM
# Append rustup without displacing the managed Cargo shim.
PATH="${PATH}:${HOME}/.cargo/bin"
export PATH
if [ "${1:-}" = --source ]; then
    shift
    [ "$#" -eq 0 ] || { echo 'install-blaze: --source takes no additional arguments' >&2; exit 2; }
    # The published 2026-10-20 nightly predates the memory budget. Build the exact
    # reviewed source revision when no capability-checked archive is available.
    git init -q "$work/source"
    git -C "$work/source" -c http.lowSpeedLimit=1024 -c http.lowSpeedTime=60 \
        fetch -q --depth 1 https://github.com/indexable-inc/blaze.git "$revision"
    git -C "$work/source" checkout -q --detach FETCH_HEAD
    [ "$(git -C "$work/source" rev-parse HEAD)" = "$revision" ] || exit 1
    rustup toolchain install 1.99.0 --profile minimal --component rust-src
    project_root=$(CDPATH= cd -- "$(dirname "$0")/.." && pwd)
    bash "$project_root/scripts/wait-load.sh" --max 40 --min-free-mem 30
    (cd "$work/source" && cargo +1.99.0 install --path crates/ra-jit --locked \
        --root "$work/installed" \
        --target-dir "${HOME}/.cache/agents-manager/blaze-source-target")
    mkdir -p "$blaze_home/bin"
    for name in blaze cargo-blaze cargo-ra-jit ra-jit ra-jit-rustc ra-jit-rustc-first; do
        cp "$work/installed/bin/$name" "$blaze_home/bin/.$name.new.$$"
        chmod 755 "$blaze_home/bin/.$name.new.$$"
        mv "$blaze_home/bin/.$name.new.$$" "$blaze_home/bin/$name"
    done
else
    # The upstream installer verifies its archive against release SHA256SUMS.
    curl --proto '=https' --tlsv1.2 -fsSL --connect-timeout 10 --max-time 60 \
        "https://raw.githubusercontent.com/indexable-inc/blaze/$revision/install.sh" -o "$work/installer"
    CARGO_HOME="$blaze_home" sh "$work/installer" "$@"
fi
if ! strings "$blaze_home/bin/ra-jit" | grep 'RA_JIT_MEMORY_MAX' >/dev/null; then
    echo 'install-blaze: this archive lacks the required memory budget; build the pinned source with scripts/install-blaze.sh --source. Project acceleration remains disabled.' >&2
    exit 1
fi
printf 'memory-budget-v1; installer/source revision %s\n' "$revision" > "$blaze_home/bin/.agm-blaze-memory-v1"
printf '\nProject entrypoint: scripts/project-cargo.sh build -p agents-managerd\n'
