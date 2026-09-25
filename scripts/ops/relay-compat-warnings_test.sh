#!/bin/bash
# Check that the #339 compatibility warning report counts matching lines and exposes the latest timestamp.
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
HELPER="${RELAY_WARN_COUNTER_SCRIPT:-$HERE/relay-compat-warnings.sh}"
ROOT=$(mktemp -d)
trap 'rm -rf "$ROOT"' EXIT

if [ ! -f "$HELPER" ]; then
  echo "FAIL - warn counter helper exists: $HELPER"
  exit 1
fi

WARN='relay_from without X-AM-Bot-Token: accepted as unverified (issue #339 compat)'
printf '2026-09-24T02:39:45.271947Z %s\n\033[34m2026-09-25T08:30:42.301657Z\033[0m \033[33mWARN\033[0m %s\n' \
  "$WARN" "$WARN" > "$ROOT/daemon.log"
expected=$(printf 'warnings=2\nlatest=2026-09-25T08:30:42.301657Z')
actual=$(bash "$HELPER" "$ROOT/daemon.log")
if [ "$actual" = "$expected" ]; then
  echo "ok   - counts warnings and reports the newest timestamp"
else
  echo "FAIL - warning report differs (expected '$expected', got '$actual')"
  exit 1
fi

: > "$ROOT/empty.log"
expected=$(printf 'warnings=0\nlatest=none')
actual=$(bash "$HELPER" "$ROOT/empty.log")
if [ "$actual" = "$expected" ]; then
  echo "ok   - reports zero warnings without inventing a date"
else
  echo "FAIL - empty log report differs (expected '$expected', got '$actual')"
  exit 1
fi

if bash "$HELPER" "$ROOT/missing.log" >/dev/null 2>&1; then
  echo "FAIL - unreadable log must not look like zero warnings"
  exit 1
else
  rc=$?
  if [ "$rc" = 2 ]; then
    echo "ok   - missing log is an error, not a false zero"
  else
    echo "FAIL - missing log returned rc=$rc, expected 2"
    exit 1
  fi
fi

printf 'WARN %s\n' "$WARN" > "$ROOT/malformed.log"
if bash "$HELPER" "$ROOT/malformed.log" >/dev/null 2>&1; then
  echo "FAIL - warning without a timestamp must not produce a complete report"
  exit 1
else
  rc=$?
  if [ "$rc" = 3 ]; then
    echo "ok   - malformed warning is reported as incomplete"
  else
    echo "FAIL - malformed warning returned rc=$rc, expected 3"
    exit 1
  fi
fi
