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
 -c)
   # diff <full> <tip>: the main-tip side and the PR-head side are scripted separately.
   if [ "${*: -1}" = "$CI_BASE" ]; then
     [ -z "${CI_TEST_DIFF_FAIL_BASE:-}" ] || exit 1
     printf '%s\n' "${CI_TEST_PATHS_BASE:-}"
   else
     printf '%s\n' "$CI_TEST_PATHS"
   fi ;;
 *) exit 90 ;;
esac
SH
chmod +x "$tmp/bin/git"
export CI_BASE=1111111111111111111111111111111111111111 CI_HEAD=2222222222222222222222222222222222222222
export CI_EVENT=pull_request CI_BASE_STATUS="$tmp/status.json"
printf '{"sha":"%s","statuses":[{"context":"ubuntu-ci","state":"success","description":"ob+ops+web+daemon 全綠"}]}' "$CI_BASE" > "$CI_BASE_STATUS"
all=$'ob=true\nops=true\nweb=true\ndaemon=true\nlint=true'
run() { (cd "$tmp" && PATH="${AM_CANARY_DIR:+$AM_CANARY_DIR:}$tmp/bin:$PATH" python3 -B "$repo/scripts/ci-pr-parts.py"); }
baseline_ref() { PATH="${AM_CANARY_DIR:+$AM_CANARY_DIR:}$PATH" python3 -B "$repo/scripts/ci-pr-parts.py" --baseline-ref "$CI_BASE_STATUS"; }
check() { local want="$1" actual; want="$want"$'\nops_base='"${3:-}"; export CI_TEST_PATHS="$2"; actual=$(run); [ "$actual" = "$want" ] || { echo "FAIL: $2 → $actual"; exit 1; }; }
check $'ob=false\nops=false\nweb=false\ndaemon=false\nlint=false' docs/readme.md
check $'ob=false\nops=false\nweb=true\ndaemon=false\nlint=false' web/src/x.ts
check $'ob=false\nops=false\nweb=false\ndaemon=true\nlint=true' crates/am-share/src/share/store.rs
check $'ob=false\nops=true\nweb=false\ndaemon=false\nlint=false' scripts/ops/outbox-gc.sh "$CI_BASE"
check $'ob=true\nops=true\nweb=false\ndaemon=false\nlint=false' scripts/chatgpt-consult.sh "$CI_BASE"
check $'ob=false\nops=false\nweb=true\ndaemon=true\nlint=true' $'web/src/x.ts\ncrates/am-share/src/share/store.rs'
check "$all" unknown-input
CI_EVENT=schedule check "$all" scripts/ops/outbox-gc.sh
CI_EVENT=workflow_dispatch check "$all" scripts/ops/outbox-gc.sh
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
[ "$(baseline_ref)" = 33333333 ]
check "$all" docs/readme.md
printf '{"sha":"3333333333333333333333333333333333333333","statuses":[{"context":"ubuntu-ci","state":"success","description":"ob+ops+web+daemon 全綠"}]}' > "$CI_BASE_STATUS"
check $'ob=false\nops=false\nweb=false\ndaemon=false\nlint=false' docs/readme.md
check $'ob=false\nops=true\nweb=false\ndaemon=false\nlint=false' scripts/ops/outbox-gc.sh 3333333333333333333333333333333333333333
# Main moved on after the verified baseline: PR head does not contain base (diverged), yet docs-only stays docs-only.
check $'ob=false\nops=false\nweb=false\ndaemon=false\nlint=false' docs/readme.md
CI_TEST_PATHS_BASE=docs/other.md check $'ob=false\nops=false\nweb=false\ndaemon=false\nlint=false' docs/readme.md
# ...but unverified main code changes between baseline and base must still run, even when the PR itself is docs-only.
CI_TEST_PATHS_BASE=daemon/src/x.rs check $'ob=false\nops=false\nweb=false\ndaemon=true\nlint=true' docs/readme.md
CI_TEST_PATHS_BASE=web/src/x.ts check $'ob=false\nops=true\nweb=true\ndaemon=false\nlint=false' scripts/ops/outbox-gc.sh 3333333333333333333333333333333333333333
CI_TEST_PATHS_BASE=unknown-input check "$all" docs/readme.md
# Failing to compare either side, or a baseline that is no longer a main ancestor, fails open.
CI_TEST_DIFF_FAIL_BASE=1 check "$all" docs/readme.md
CI_TEST_ANCESTOR_RC=1 CI_TEST_PATHS_BASE=docs/other.md check "$all" docs/readme.md
# A scoped (partial) main success is never full evidence: resolve it to the cited full ancestor instead.
printf '{"sha":"%s","statuses":[{"context":"ubuntu-ci","state":"success","description":"scoped web+daemon（沿用完整成功基準 33333333）"}]}' "$CI_BASE" > "$CI_BASE_STATUS"
check "$all" docs/readme.md
[ "$(baseline_ref)" = 33333333 ]
printf '{"sha":"%s","statuses":[{"context":"ubuntu-ci","state":"success","description":"scoped ops（沿用完整成功基準 44444444）"}]}' "$CI_BASE" > "$CI_BASE_STATUS"
[ "$(baseline_ref)" = 44444444 ]
for bad in 'web+daemon（沿用完整成功基準 33333333）' 'scoped foo+bar（沿用完整成功基準 33333333）' 'scoped none+web（沿用完整成功基準 33333333）' 'scoped（沿用完整成功基準 33333333）' 'scoped web+daemon（沿用完整成功基準 3333）' 'foo+bar（沿用完整成功基準 33333333）' 'web+（沿用完整成功基準 33333333）' 'web（沿用完整成功基準 3333）'; do
    printf '{"sha":"%s","statuses":[{"context":"ubuntu-ci","state":"success","description":"%s"}]}' "$CI_BASE" "$bad" > "$CI_BASE_STATUS"
    [ -z "$(baseline_ref)" ] || { echo "FAIL: baseline from $bad"; exit 1; }
done
printf '{"sha":"%s","statuses":[{"context":"ubuntu-ci","state":"failure","description":"scoped web+daemon（沿用完整成功基準 33333333）"}]}' "$CI_BASE" > "$CI_BASE_STATUS"
[ -z "$(baseline_ref)" ]
printf '{"sha":"3333333333333333333333333333333333333333","statuses":[{"context":"ubuntu-ci","state":"success","description":"ob+ops+web+daemon 全綠"}]}' > "$CI_BASE_STATUS"
printf '#!/bin/bash\nexit 1\n' > "$tmp/scripts/ci-changed-parts.sh"
check "$all" docs/readme.md
# Required jobs remain present with stable names and an explicit classification failure gate.
python3 - "$repo/.github/workflows/ci.yml" <<'PY'
import sys
import re
from pathlib import Path
text = Path(sys.argv[1]).read_text()
def job(name):
    return re.split(r'\n  [a-z_]+:\n', text.split('\n  ' + name + ':\n', 1)[1], maxsplit=1)[0]
for name in ('web', 'daemon'):
    block = job(name)
    assert 'if: ${{ always() }}' in block
    assert "needs.parts.result != 'success'" in block
ops = job('ob')
assert 'fetch-depth: 0' in ops
assert 'scripts/check.sh ops "$CI_OPS_BASE" "$CI_OPS_MAIN" "$CI_OPS_HEAD"' in ops
assert 'CI_OPS_BASE: ${{ needs.parts.outputs.ops_base }}' in ops
assert 'CI_OPS_MAIN: ${{ github.event.pull_request.base.sha }}' in ops
assert 'CI_OPS_HEAD: ${{ github.event.pull_request.head.sha }}' in ops
# web publishes a valid bundle before running unit tests; its red must not hide or replace daemon's own result.
web, daemon = job('web'), job('daemon')
assert 'bundle: ${{ steps.bundle.outputs.ok }}' in web
order = [web.index(m) for m in ('scripts/check.sh web-bundle', 'actions/upload-artifact', 'id: bundle', 'scripts/check.sh web-verify')]
assert order == sorted(order), order
assert 'if-no-files-found: error' in web
assert "needs.web.outputs.bundle != 'true'" in daemon
assert 'needs.web.result' not in daemon
assert 'scripts/check.sh daemon' in daemon
assert text.count('scripts/check.sh web-bundle') == 1  # one bundle build per run
assert 'workflow_dispatch:' in text and 'schedule:' in text
PY
# check.sh splits web into bundle (install + build) and verify (tsc, oxlint, test): a red unit test must come
# after the bundle, and verify must not rebuild it.
fx="$tmp/webfx"
mkdir -p "$fx/scripts" "$fx/web" "$fx/bin"
cp "$repo/scripts/check.sh" "$fx/scripts/"
cat > "$fx/bin/bun" <<'SH'
#!/bin/bash
echo "bun $*" >> "$FX_LOG"
[ "$1" = test ] && [ -n "${FX_TEST_FAIL:-}" ] && exit 1
exit 0
SH
cat > "$fx/bin/bunx" <<'SH'
#!/bin/bash
echo "bunx $*" >> "$FX_LOG"
exit 0
SH
chmod +x "$fx/bin/bun" "$fx/bin/bunx"
export FX_LOG="$fx/log"
: > "$FX_LOG"
(cd "$fx" && PATH="$fx/bin:$PATH" bash scripts/check.sh web-bundle >/dev/null)
[ "$(cat "$FX_LOG")" = $'bun install --frozen-lockfile\nbunx vite build' ] || { echo 'FAIL: web-bundle'; cat "$FX_LOG"; exit 1; }
: > "$FX_LOG"
if (cd "$fx" && FX_TEST_FAIL=1 PATH="$fx/bin:$PATH" bash scripts/check.sh web-verify >/dev/null 2>&1); then echo 'FAIL: web-verify swallowed a red unit test'; exit 1; fi
grep -q '^bun test' "$FX_LOG"
if grep -q 'vite build\|install' "$FX_LOG"; then echo 'FAIL: web-verify rebuilt bundle'; exit 1; fi
echo 'ci-pr-parts: docs/scoped/full selection, diverged main, scoped baselines and web/daemon bundle gating passed'
