#!/usr/bin/env bash
set -euo pipefail
repo="$(cd "$(dirname "$0")/.." && pwd)"
tmp=$(mktemp -d)
trap '/bin/rm -rf "$tmp"' EXIT
mkdir -p "$tmp/scripts" "$tmp/bin"
cp "$repo/scripts/ci-changed-parts.sh" "$tmp/scripts/"
cat > "$tmp/bin/git" <<'SH'
#!/bin/bash
case "$1" in
 merge-base) exit "${CI_TEST_ANCESTOR_RC:-0}" ;;
 -c) printf '%s\n' "$CI_TEST_PATHS" ;;
 *) exit 90 ;;
esac
SH
chmod +x "$tmp/bin/git"
export CI_BASE=1111111111111111111111111111111111111111 CI_HEAD=2222222222222222222222222222222222222222
export CI_EVENT=pull_request CI_BASE_STATUS="$tmp/status.json"
printf '{"sha":"%s","statuses":[{"context":"ubuntu-ci","state":"success","description":"ob+ops+web+daemon 全綠"}]}' "$CI_BASE" > "$CI_BASE_STATUS"
all=$'ob=true\nops=true\nweb=true\ndaemon=true\nlint=true'
run() { (cd "$tmp" && PATH="${AM_CANARY_DIR:+$AM_CANARY_DIR:}$tmp/bin:$PATH" python3 -B "$repo/scripts/ci-pr-parts.py"); }
check() { local want="$1" actual; export CI_TEST_PATHS="$2"; actual=$(run); [ "$actual" = "$want" ] || { echo "FAIL: $2 → $actual"; exit 1; }; }
check $'ob=false\nops=false\nweb=false\ndaemon=false\nlint=false' docs/readme.md
check $'ob=false\nops=false\nweb=true\ndaemon=false\nlint=false' web/src/x.ts
check $'ob=false\nops=false\nweb=false\ndaemon=true\nlint=true' crates/am-share/src/share/store.rs
check $'ob=false\nops=true\nweb=false\ndaemon=false\nlint=false' scripts/ops/outbox-gc.sh
check $'ob=true\nops=true\nweb=false\ndaemon=false\nlint=false' scripts/chatgpt-consult.sh
check $'ob=false\nops=false\nweb=true\ndaemon=true\nlint=true' $'web/src/x.ts\ncrates/am-share/src/share/store.rs'
check "$all" unknown-input
CI_EVENT=schedule check "$all" docs/readme.md
CI_TEST_ANCESTOR_RC=1 check "$all" docs/readme.md
CI_BASE=invalid check "$all" docs/readme.md
printf '{"sha":"%s","statuses":[{"context":"ubuntu-ci","state":"failure"}]}' "$CI_BASE" > "$CI_BASE_STATUS"
check "$all" docs/readme.md
printf '{}' > "$CI_BASE_STATUS"
check "$all" docs/readme.md
printf 'invalid' > "$CI_BASE_STATUS"
check "$all" docs/readme.md
# Resolve a docs-only main status back to the full ancestor, never inherit its skip as evidence.
printf '{"sha":"%s","statuses":[{"context":"ubuntu-ci","state":"success","description":"docs-only skip（沿用完整成功基準 33333333）"}]}' "$CI_BASE" > "$CI_BASE_STATUS"
[ "$(PATH="${AM_CANARY_DIR:+$AM_CANARY_DIR:}$PATH" python3 -B "$repo/scripts/ci-pr-parts.py" --baseline-ref "$CI_BASE_STATUS")" = 33333333 ]
check "$all" docs/readme.md
printf '{"sha":"3333333333333333333333333333333333333333","statuses":[{"context":"ubuntu-ci","state":"success","description":"ob+ops+web+daemon 全綠"}]}' > "$CI_BASE_STATUS"
check $'ob=false\nops=false\nweb=false\ndaemon=false\nlint=false' docs/readme.md
printf '#!/bin/bash\nexit 1\n' > "$tmp/scripts/ci-changed-parts.sh"
check "$all" docs/readme.md
# Required jobs remain present with stable names and an explicit classification failure gate.
python3 - "$repo/.github/workflows/ci.yml" <<'PY'
import sys
import re
from pathlib import Path
text = Path(sys.argv[1]).read_text()
for job in ('web', 'daemon'):
    block = re.split(r'\n  [a-z_]+:\n', text.split('\n  ' + job + ':\n', 1)[1], maxsplit=1)[0]
    assert 'if: ${{ always() }}' in block
    assert "needs.parts.result != 'success'" in block
assert 'workflow_dispatch:' in text and 'schedule:' in text
PY
echo 'ci-pr-parts: docs/scoped/full selection and conservative fallbacks passed'
