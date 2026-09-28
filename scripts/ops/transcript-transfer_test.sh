#!/usr/bin/env bash
# scripts/ops/transcript-transfer 的隔離測試（假的來源 HOME 與目標目錄，issue #717）。
# 本體在 transcript-transfer_test.py；這支只是讓 `scripts/check.sh ops` 的 *_test.sh 迴圈撈得到。
set -euo pipefail
HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
# canary-gap: PATH 原樣繼承（canary 目錄還在最前面）；測試只用 --target-dir（本機暫存目錄），
# 不 ssh、不碰真的 ~/.claude／~/.codex／~/.grok，只 spawn sys.executable 跑被測的 transcript-transfer。
exec python3 -B "${HERE}/transcript-transfer_test.py"
