#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
tmp="$(mktemp -d "${TMPDIR:-/tmp}/am-macos-local-check.XXXXXX")"
trap '/bin/rm -rf "$tmp"' EXIT

bin="$tmp/bin"
native="$tmp/native"
mkdir -p "$bin" "$native"

cat >"$bin/uname" <<'SH'
#!/bin/sh
printf '%s\n' "${AM_TEST_UNAME:-Linux}"
SH

cat >"$bin/rustup" <<'SH'
#!/bin/sh
case "$*" in
    'which cargo') printf '%s\n' "$AM_TEST_LOCAL_CARGO" ;;
    'which rustc') printf '%s\n' "$AM_TEST_LOCAL_RUSTC" ;;
    'which rustdoc') printf '%s\n' "$AM_TEST_LOCAL_RUSTDOC" ;;
    *) exit 2 ;;
esac
SH

cat >"$bin/cargo" <<'SH'
#!/bin/sh
printf 'remote cargo shim was invoked\n' >>"$AM_TEST_REMOTE_CARGO"
printf 'remote cargo shim was invoked\n' >&2
exit 91
SH

cat >"$native/cargo" <<'SH'
#!/bin/sh
if [ -n "${AM_TEST_LONG_CARGO:-}" ]; then
    # 假的長時間編譯：底下再掛一個孫行程，看門狗要連整個行程群組一起砍。
    "$AM_TEST_REAL_SLEEP" 300 &
    printf '%s\n' "$!" >"$AM_TEST_LONG_CARGO.grandchild"
    printf '%s\n' "$$" >"$AM_TEST_LONG_CARGO"
    exec "$AM_TEST_REAL_SLEEP" 300
fi
if [ -n "${AM_TEST_CARGO_WAITS_FOR:-}" ]; then
    # 等看門狗至少進到一次 sleep 再結束，才驗得到「正常結束會收掉看門狗」。
    while [ ! -s "$AM_TEST_CARGO_WAITS_FOR" ]; do "$AM_TEST_REAL_SLEEP" 0.05; done
fi
{
    printf 'CALL\n'
    for arg in "$@"; do printf 'ARG:%s\n' "$arg"; done
    printf 'RUSTC:%s\n' "${RUSTC:-}"
    printf 'RUSTDOC:%s\n' "${RUSTDOC:-}"
    printf 'CARGO_BUILD_TARGET:%s\n' "${CARGO_BUILD_TARGET:-}"
    printf 'AM_MODEL:%s\n' "${AM_MODEL:-}"
    printf 'AM_EFFORT:%s\n' "${AM_EFFORT:-}"
    printf 'AM_DATA_DIR:%s\n' "${AM_DATA_DIR:-}"
} >>"$AM_TEST_LOCAL_CARGO_LOG"
zero=0
if [ "$AM_TEST_ZERO_TESTS" = 1 ]; then
    zero=1
elif [ -n "$AM_TEST_ZERO_SELECTOR" ]; then
    for arg in "$@"; do
        if [ "$arg" = "$AM_TEST_ZERO_SELECTOR" ]; then
            zero=1
            break
        fi
    done
fi
if [ "$zero" = 1 ]; then
    printf 'test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 12 filtered out; finished in 0.00s\n'
else
    printf 'test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s\n'
fi
SH

cat >"$native/rustc" <<'SH'
#!/bin/sh
exit 0
SH

cat >"$native/rustdoc" <<'SH'
#!/bin/sh
exit 0
SH

chmod +x "$bin/uname" "$bin/rustup" "$bin/cargo" "$native/cargo" "$native/rustc" "$native/rustdoc"

export AM_TEST_LOCAL_CARGO="$native/cargo"
export AM_TEST_LOCAL_RUSTC="$native/rustc"
export AM_TEST_LOCAL_RUSTDOC="$native/rustdoc"
export AM_TEST_LOCAL_CARGO_LOG="$tmp/local-cargo.log"
export AM_TEST_REMOTE_CARGO="$tmp/remote-cargo.log"
export AM_TEST_REAL_SLEEP="$(command -v sleep)"
export PATH="$bin:$PATH"
# 記憶體閘與看門狗都讀這個假檔（第一欄 free%、第二欄壓力等級），不看這台機器的真記憶體。
export WAIT_MEM_FILE="$tmp/mem"
printf '80 1\n' >"$WAIT_MEM_FILE"

if output="$(AM_TEST_UNAME=Linux "$ROOT/scripts/check.sh" macos-local 2>&1)"; then
    echo "macos-local must reject a non-macOS host" >&2
    exit 1
fi
if [[ "$output" != *"only run on macOS"* ]]; then
    printf 'unexpected non-macOS refusal: %s\n' "$output" >&2
    exit 1
fi
if [[ -e "$AM_TEST_LOCAL_CARGO_LOG" || -e "$AM_TEST_REMOTE_CARGO" ]]; then
    echo "macos-local attempted cargo before checking the host OS" >&2
    exit 1
fi

AM_MODEL=sentinel AM_EFFORT=sentinel AM_DATA_DIR=/tmp/am-macos-local-test AM_TEST_UNAME=Darwin \
    "$ROOT/scripts/check.sh" macos-local

call_count="$(grep -c '^CALL$' "$AM_TEST_LOCAL_CARGO_LOG" || true)"
if [[ "$call_count" != 3 ]]; then
    printf 'expected three serial local Cargo filters, got %s\n' "$call_count" >&2
    exit 1
fi
for selector in 'cli_update::tests' 'cargo_shim::tests' 'macos_local_'; do
    if ! grep -Fq "ARG:$selector" "$AM_TEST_LOCAL_CARGO_LOG"; then
        printf 'missing macOS test selector: %s\n' "$selector" >&2
        exit 1
    fi
done
for required_arg in 'ARG:-j' 'ARG:1' 'ARG:--test-threads=1'; do
    if ! grep -Fq "$required_arg" "$AM_TEST_LOCAL_CARGO_LOG"; then
        printf 'missing serial Cargo setting: %s\n' "$required_arg" >&2
        exit 1
    fi
done
for required_env in \
    "RUSTC:$AM_TEST_LOCAL_RUSTC" \
    "RUSTDOC:$AM_TEST_LOCAL_RUSTDOC" \
    'CARGO_BUILD_TARGET:' \
    'AM_MODEL:' \
    'AM_EFFORT:' \
    'AM_DATA_DIR:'; do
    if ! grep -Fq "$required_env" "$AM_TEST_LOCAL_CARGO_LOG"; then
        printf 'missing native or isolated Cargo environment: %s\n' "$required_env" >&2
        exit 1
    fi
done
if [[ -e "$AM_TEST_REMOTE_CARGO" ]]; then
    echo "macos-local must bypass the Cargo PATH shim" >&2
    exit 1
fi

for selector in 'cli_update::tests' 'cargo_shim::tests' 'macos_local_'; do
    if output="$(AM_TEST_UNAME=Darwin AM_TEST_ZERO_SELECTOR="$selector" "$ROOT/scripts/check.sh" macos-local 2>&1)"; then
        printf 'macos-local must reject an empty required cohort: %s\n' "$selector" >&2
        exit 1
    fi
    if [[ "$output" != *"required cohort $selector selected zero tests"* ]]; then
        printf 'unexpected empty required-cohort failure for %s: %s\n' "$selector" "$output" >&2
        exit 1
    fi
done

if output="$(AM_TEST_UNAME=Darwin AM_TEST_ZERO_TESTS=1 "$ROOT/scripts/check.sh" macos-local 2>&1)"; then
    echo "macos-local must reject an empty test selection" >&2
    exit 1
fi
if [[ "$output" != *"required cohort cli_update::tests selected zero tests"* ]]; then
    printf 'unexpected empty-selection failure: %s\n' "$output" >&2
    exit 1
fi

# 記憶體閘（issue #1783）：可用記憶體不夠就不開編，逾時 124。
/bin/rm -f "$AM_TEST_LOCAL_CARGO_LOG"
printf '10 1\n' >"$WAIT_MEM_FILE"
rc=0
output="$(AM_TEST_UNAME=Darwin CHECK_LOAD_TIMEOUT=0 "$ROOT/scripts/check.sh" macos-local 2>&1)" || rc=$?
if [[ "$rc" != 124 ]]; then
    printf 'macos-local must refuse to compile when free memory is low (rc=%s): %s\n' "$rc" "$output" >&2
    exit 1
fi
if [[ "$output" != *"記憶體閘"* || "$output" != *"可用記憶體 10% < 30%"* ]]; then
    printf 'unexpected memory gate refusal: %s\n' "$output" >&2
    exit 1
fi
if [[ -e "$AM_TEST_LOCAL_CARGO_LOG" ]]; then
    echo "macos-local ran cargo although the memory gate refused" >&2
    exit 1
fi
AM_TEST_UNAME=Darwin CHECK_MIN_FREE_MEM=5 CHECK_LOAD_TIMEOUT=0 "$ROOT/scripts/check.sh" macos-local >/dev/null 2>&1 || {
    echo "CHECK_MIN_FREE_MEM must lower the memory threshold" >&2
    exit 1
}

# 看門狗：編譯中壓力升到 critical 就砍掉整個 cargo 行程群組。假的 sleep（看門狗每輪呼叫）等假 cargo 起來後把壓力改成 4。
watch_bin="$tmp/watch-bin"
mkdir -p "$watch_bin"
cat >"$watch_bin/sleep" <<'SH'
#!/bin/sh
if [ -n "${AM_TEST_LONG_CARGO:-}" ]; then
    while [ ! -s "$AM_TEST_LONG_CARGO" ]; do "$AM_TEST_REAL_SLEEP" 0.05; done
    printf '80 4\n' >"$WAIT_MEM_FILE"
    exit 0
fi
printf '%s\n' "$$" >"$AM_TEST_WATCH_SLEEP"
exec "$AM_TEST_REAL_SLEEP" 300
SH
chmod +x "$watch_bin/sleep"
gone() {
    local i
    for i in $(seq 1 100); do
        kill -0 "$1" 2>/dev/null || return 0
        "$AM_TEST_REAL_SLEEP" 0.05
    done
    return 1
}

/bin/rm -f "$AM_TEST_LOCAL_CARGO_LOG"
printf '80 1\n' >"$WAIT_MEM_FILE"
rc=0
output="$(PATH="$watch_bin:$PATH" AM_TEST_UNAME=Darwin AM_TEST_LONG_CARGO="$tmp/long-cargo.pid" \
    "$ROOT/scripts/check.sh" macos-local 2>&1)" || rc=$?
if [[ "$rc" == 0 ]]; then
    echo "macos-local must fail when memory pressure turns critical" >&2
    exit 1
fi
if [[ "$output" != *"記憶體壓力 critical，已中止 macos-local，避免整台機器卡死"* ]]; then
    printf 'missing critical-pressure explanation: %s\n' "$output" >&2
    exit 1
fi
if [[ "$output" == *"macOS tests: cargo_shim::tests"* ]]; then
    echo "macos-local kept compiling the next cohort after a critical abort" >&2
    exit 1
fi
for pid in "$(cat "$tmp/long-cargo.pid")" "$(cat "$tmp/long-cargo.pid.grandchild")"; do
    if ! gone "$pid"; then
        printf 'the watchdog left cargo process %s running\n' "$pid" >&2
        kill -KILL "$pid" 2>/dev/null || true
        exit 1
    fi
done

# 正常結束要收掉看門狗（連它正在睡的 sleep）。
printf '80 1\n' >"$WAIT_MEM_FILE"
export AM_TEST_WATCH_SLEEP="$tmp/watch-sleep.pid"
PATH="$watch_bin:$PATH" AM_TEST_UNAME=Darwin AM_TEST_CARGO_WAITS_FOR="$AM_TEST_WATCH_SLEEP" \
    "$ROOT/scripts/check.sh" macos-local >/dev/null 2>&1 || {
    echo "macos-local must pass when memory pressure stays normal" >&2
    exit 1
}
if ! gone "$(cat "$AM_TEST_WATCH_SLEEP")"; then
    echo "macos-local left its memory watchdog running after a normal finish" >&2
    kill -KILL "$(cat "$AM_TEST_WATCH_SLEEP")" 2>/dev/null || true
    exit 1
fi

echo "macos-local dispatch is native, scoped, and serial"
