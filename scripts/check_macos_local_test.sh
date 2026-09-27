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
if [ "${AM_TEST_ZERO_TESTS:-0}" = 1 ]; then
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
export PATH="$bin:$PATH"

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

if output="$(AM_TEST_UNAME=Darwin AM_TEST_ZERO_TESTS=1 "$ROOT/scripts/check.sh" macos-local 2>&1)"; then
    echo "macos-local must reject an empty test selection" >&2
    exit 1
fi
if [[ "$output" != *"selected no tests"* ]]; then
    printf 'unexpected empty-selection failure: %s\n' "$output" >&2
    exit 1
fi

echo "macos-local dispatch is native, scoped, and serial"
