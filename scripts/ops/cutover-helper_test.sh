#!/bin/bash
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
TEST_HOME="$(mktemp -d "${TMPDIR:-/tmp}/cutover-helper-test.XXXXXX")"
trap '/bin/rm -rf "$TEST_HOME"' EXIT
HOME="$TEST_HOME" PYTHONDONTWRITEBYTECODE=1 python3 -B "$HERE/cutover-helper_test.py"
