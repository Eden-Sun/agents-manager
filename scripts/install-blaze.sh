#!/bin/sh
# Install into a project-specific tools directory, without altering Rust or Cargo configuration.
set -eu
blaze_home=${AGM_BLAZE_INSTALL_HOME:-${HOME}/.local/share/agents-manager/blaze}
installer=$(mktemp "${TMPDIR:-/tmp}/agm-blaze-install.XXXXXX")
trap 'rm -f "$installer"' EXIT
trap 'exit 130' INT TERM
# Pin the installer source; its release archive is checked against upstream SHA256SUMS.
curl --proto '=https' --tlsv1.2 -fsSL --connect-timeout 10 --max-time 60 \
    https://raw.githubusercontent.com/indexable-inc/blaze/c8e5f6a88ae3eb61179d3fd20bd6afc871ca48e5/install.sh \
    -o "$installer"
# The daemon's minimal PATH may omit rustup; append it without displacing the Cargo shim.
PATH="${PATH}:${HOME}/.cargo/bin"
export PATH
CARGO_HOME="$blaze_home" sh "$installer" "$@"
printf '\nProject entrypoint: scripts/project-cargo.sh build -p agents-managerd\n'
