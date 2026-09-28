#!/usr/bin/env bash
# scripts/ops/project-transfer 的隔離測試（假 DB／假 config／暫存目錄，issue #710）。
# 本體在 project-transfer_test.py；這支只是讓 `scripts/check.sh ops` 的 *_test.sh 迴圈撈得到。
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
if ! python3 -c 'import tomllib' 2>/dev/null; then
    echo "skip: python3 沒有 tomllib（需要 3.11 以上）"
    exit 0
fi
# canary-gap: PATH 原樣繼承（canary 目錄還在最前面）；被測的 project-transfer 只用 python 標準庫
# （sqlite3／fcntl／gzip），不起任何子行程，測試本身也只 spawn sys.executable 跑它。
exec python3 -B "${HERE}/project-transfer_test.py"
