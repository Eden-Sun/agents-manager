#!/bin/bash
# Report the #339 relay compatibility warning count and newest timestamp.
set -u

if [ "$#" -gt 1 ]; then
  echo "usage: $0 [daemon.log]" >&2
  exit 2
fi

if [ "$#" -eq 1 ]; then
  LOG_FILE="$1"
elif [ -n "${DAEMON_LOG:-}" ]; then
  LOG_FILE="$DAEMON_LOG"
else
  DATA_DIR="${AM_DATA_DIR:-${AM_DATA:-$HOME/.config/agents-manager}}"
  LOG_FILE="$DATA_DIR/daemon.log"
fi

if [ ! -f "$LOG_FILE" ] || [ ! -r "$LOG_FILE" ]; then
  echo "cannot read daemon log: $LOG_FILE" >&2
  exit 2
fi

# Strip terminal color escapes before searching. Emit only aggregate information;
# relay_from values and the rest of each log line may identify bots.
LC_ALL=C awk -v needle='relay_from without X-AM-Bot-Token: accepted as unverified (issue #339 compat)' '
  {
    gsub(/\033\[[0-9;]*[[:alpha:]]/, "", $0)
    if (index($0, needle)) {
      count++
      timestamp = substr($0, 1, 27)
      if (timestamp !~ /^[0-9][0-9][0-9][0-9]-[0-9][0-9]-[0-9][0-9]T[0-9][0-9]:[0-9][0-9]:[0-9][0-9]\.[0-9][0-9][0-9][0-9][0-9][0-9]Z$/) {
        malformed = 1
        next
      }
      if (timestamp > latest) latest = timestamp
    }
  }
  END {
    if (malformed) exit 3
    printf "warnings=%d\nlatest=%s\n", count, count ? latest : "none"
  }
' "$LOG_FILE"
