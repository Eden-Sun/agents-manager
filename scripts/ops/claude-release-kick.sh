#!/bin/bash
# Compatibility entry point for the existing com.agm.claude-release schedule.
# Binary diff and changelog triage now share release-triage-kick's lock and assignment.
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
KICK="$HERE/release-triage-kick.sh"
[ -f "$KICK" ] || { echo "找不到合流 kick：${KICK}" >&2; exit 0; }
exec /bin/bash "$KICK" "$@"
