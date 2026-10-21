#!/usr/bin/env bash
# scripts/claude-hook-debug.py 的隔離測試（只吃自造的文字，不讀 ~/.claude/debug、不開 claude）。
# 本體在 claude-hook-debug_test.py；這支讓 `scripts/check.sh ops` 的 *_test.sh 迴圈撈得到。
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
if ! command -v python3 >/dev/null 2>&1; then
    echo "skip: 沒有 python3"
    exit 0
fi
# canary-gap: PATH 原樣繼承；被測腳本與測試只用 python 標準庫，測試只 spawn sys.executable。
exec python3 -B "${HERE}/claude-hook-debug_test.py"
