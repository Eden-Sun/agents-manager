#!/usr/bin/env bash
set -euo pipefail

: "${SCCACHE_DIR:="${HOME:-/tmp}/.cache/agents-manager-sccache"}"
: "${SCCACHE_CACHE_SIZE:=10G}"
export SCCACHE_DIR SCCACHE_CACHE_SIZE
incremental=0
prev=""
for a in "$@"; do
  case "$a" in
    -Cincremental=*) incremental=1 ;;
    incremental=*) [ "$prev" = "-C" ] && incremental=1 ;;
  esac
  prev="$a"
done
if [ "$incremental" = 0 ] && command -v sccache >/dev/null 2>&1 && sccache --show-stats >/dev/null 2>&1; then
  exec sccache "$@"
fi
exec "$@"
