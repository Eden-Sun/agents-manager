#!/usr/bin/env bash
set -euo pipefail

if ! command -v sccache >/dev/null 2>&1; then
  echo "sccache 沒裝：managed cargo 會直接用原生 rustc，不影響編譯，只是沒有跨 worktree 快取。"
  echo "要裝的話：brew install sccache（macOS）或見 https://github.com/mozilla/sccache#installation"
  exit 0
fi

: "${SCCACHE_DIR:="${HOME:-/tmp}/.cache/agents-manager-sccache"}"
export SCCACHE_DIR
echo "SCCACHE_DIR=$SCCACHE_DIR"
sccache --show-stats
