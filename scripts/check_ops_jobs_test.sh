#!/usr/bin/env bash
set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"

expect_jobs() {
    local cores="$1" load="$2" expected="$3" actual
    actual="$(AM_TEST_OPS_CORES="$cores" AM_TEST_OPS_LOAD="$load" bash "$ROOT/scripts/ops/ops-jobs.sh")"
    [ "$actual" = "$expected" ] || {
        printf 'FAIL: %s cores and load %s selected %s workers, expected %s\n' "$cores" "$load" "$actual" "$expected" >&2
        exit 1
    }
}

expect_jobs 16 0 8
expect_jobs 16 7.9 8
expect_jobs 16 9 7
expect_jobs 16 15.9 1
expect_jobs 16 24 1
expect_jobs 1 0 1
expect_jobs 64 0 8
echo 'check-ops-jobs: 7 load-aware concurrency cases passed'
