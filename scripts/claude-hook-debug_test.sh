#!/usr/bin/env bash
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
if ! command -v python3 >/dev/null 2>&1; then
    echo "skip: 沒有 python3"
    exit 0
fi
# canary-gap: PATH 原樣繼承；被測腳本與測試只用 python 標準庫，測試只 spawn sys.executable。
exec python3 -B "${HERE}/claude-hook-debug_test.py"
