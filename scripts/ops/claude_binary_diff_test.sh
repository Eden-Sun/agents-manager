#!/usr/bin/env bash
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
PATH="${AM_CANARY_DIR:+$AM_CANARY_DIR:}/usr/bin:/bin" PYTHONDONTWRITEBYTECODE=1 python3 -B "$HERE/claude_binary_diff_test.py"
