#!/usr/bin/env bash
set -euo pipefail

if [ -n "${AM_TEST_OPS_CORES:-}" ]; then
    cores="$AM_TEST_OPS_CORES"
else
    cores="$(nproc 2>/dev/null || sysctl -n hw.ncpu 2>/dev/null || echo 4)"
fi
case "$cores" in
    ''|*[!0-9]*|0) cores=1 ;;
esac

if [ -n "${AM_TEST_OPS_LOAD:-}" ]; then
    load="$AM_TEST_OPS_LOAD"
elif [ -r /proc/loadavg ]; then
    load="$(awk 'NR == 1 {print $1}' /proc/loadavg)"
else
    load="$(sysctl -n vm.loadavg 2>/dev/null | sed -E 's/^\{[[:space:]]*([^[:space:]]+).*/\1/' || true)"
fi
if ! awk -v value="${load:-}" 'BEGIN { exit !(value ~ /^[0-9]+([.][0-9]+)?$/) }'; then
    load=0
fi

awk -v cores="$cores" -v busy="$load" 'BEGIN {
    jobs = int(cores - busy)
    if (jobs < 1) jobs = 1
    if (jobs > 8) jobs = 8
    print jobs
}'
