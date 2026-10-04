#!/bin/bash
# daemon-swap embedded API callers must ignore environment proxies and reject redirects.
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
TEST_HOME="$(mktemp -d "${TMPDIR:-/tmp}/daemon-swap-network-test.XXXXXX")"
trap '/bin/rm -rf "$TEST_HOME"' EXIT
HOME="$TEST_HOME" PYTHONDONTWRITEBYTECODE=1 python3 -B "$HERE/daemon-swap-network_test.py"
