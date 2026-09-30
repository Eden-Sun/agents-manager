#!/bin/bash
# Compatibility entry point must hand the existing schedule to the unified release-triage kick.
set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT=$(mktemp -d)
trap 'rm -rf "$ROOT"' EXIT
mkdir -p "$ROOT/bin"
cp "$HERE/claude-release-kick.sh" "$ROOT/bin/claude-release-kick.sh"
cat > "$ROOT/bin/release-triage-kick.sh" <<'STUB'
#!/bin/bash
printf 'unified kick called: %s\n' "$*"
STUB
chmod +x "$ROOT/bin/claude-release-kick.sh" "$ROOT/bin/release-triage-kick.sh"

OUT=$(bash "$ROOT/bin/claude-release-kick.sh" scheduled)
if [ "$OUT" = 'unified kick called: scheduled' ]; then
  echo 'ok   - legacy Claude schedule delegates to the unified kick'
  echo '1 passed, 0 failed'
else
  echo "FAIL - legacy schedule did not delegate: $OUT"
  echo '0 passed, 1 failed'
  exit 1
fi
