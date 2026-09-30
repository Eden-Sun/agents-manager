#!/usr/bin/env bash
set -euo pipefail
HERE="$(cd "$(dirname "$0")" && pwd)"
PATH="${AM_CANARY_DIR:+$AM_CANARY_DIR:}/usr/bin:/bin" python3 -B "$HERE/browser_gc_linux_test.py"
