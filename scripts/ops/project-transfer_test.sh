#!/usr/bin/env bash
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
if ! python3 -c 'import tomllib' 2>/dev/null; then
    echo "skip: python3 沒有 tomllib（需要 3.11 以上）"
    exit 0
fi
# canary-gap: PATH 原樣繼承（canary 目錄還在最前面）；被測的 project-transfer 只用 python 標準庫
exec python3 -B "${HERE}/project-transfer_test.py"
