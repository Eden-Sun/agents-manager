#!/usr/bin/env bash
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# canary-gap: PATH 原樣繼承（canary 目錄還在最前面）；測試只用 --target-dir（本機暫存目錄），
exec python3 -B "${HERE}/transcript-transfer_test.py"
