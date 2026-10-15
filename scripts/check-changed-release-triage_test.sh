#!/usr/bin/env bash
# Only orchestration lives here: the daemon contract owns Submission/template validation.
set -euo pipefail
unset CHECK_TESTS CHECK_MAX_LOAD
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
tmp="$(mktemp -d "${TMPDIR:-/tmp}/check-release-triage.XXXXXX")"
trap '/bin/rm -rf "$tmp"' EXIT
repo="$tmp/repo"
mkdir -p "$repo/scripts" "$repo/web" "$tmp/bin"
cp "$ROOT/scripts/check.sh" "$repo/scripts/check.sh"
printf '#!/bin/bash\necho daemon\n' > "$repo/scripts/ci-changed-parts.sh"
printf '#!/bin/bash\necho __daemon_release_triage_submission__\n' > "$repo/scripts/ci-daemon-filters.sh"
cat > "$tmp/bin/git" <<'SH'
#!/bin/bash
case "$*" in
  'diff --name-only HEAD') echo scripts/ops/release-triage-task.md ;;
  'diff --name-only origin/main...HEAD' | 'ls-files --others --exclude-standard') ;;
  *) exit 90 ;;
esac
SH
cat > "$tmp/bin/cargo" <<'SH'
#!/bin/bash
printf '%s\n' "$*" >> "$AM_TEST_CARGO_LOG"
if [[ " $* " == *" $AM_TEST_CONTRACT"* ]] && [ "${AM_TEST_CONTRACT_FAIL:-0}" = 1 ]; then exit 101; fi
[ "$1" != test ] || echo 'test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s'
exit 0
SH
chmod +x "$tmp/bin/git" "$tmp/bin/cargo"
export AM_TEST_CARGO_LOG="$tmp/cargo.log"
export AM_TEST_CONTRACT=runners::am_base_tests::release_triage::tests::the_task_template_teaches_a_submission_the_daemon_accepts
contract="test -p agents-managerd --locked -- $AM_TEST_CONTRACT"
run() { : > "$AM_TEST_CARGO_LOG"; (cd "$repo" && PATH="$tmp/bin:$PATH" bash scripts/check.sh changed) > "$tmp/output" 2>&1; }
fail() { echo "FAIL: $*" >&2; cat "$tmp/output" "$AM_TEST_CARGO_LOG" >&2; exit 1; }

run || fail 'contract selection failed'
[ "$(cat "$AM_TEST_CARGO_LOG")" = "check -p agents-managerd --all-targets --locked
$contract" ] || fail 'marker did not select the daemon contract'
CHECK_TESTS=cargo_shim:: run || fail 'override failed'
[ "$(cat "$AM_TEST_CARGO_LOG")" = "$contract
test -p agents-managerd --locked cargo_shim::" ] || fail 'override must follow, not replace, the contract'
if AM_TEST_CONTRACT_FAIL=1 CHECK_TESTS=cargo_shim:: run; then fail 'contract failure was swallowed'; fi
[ "$(cat "$AM_TEST_CARGO_LOG")" = "$contract" ] || fail 'override ran after a failed contract'
CHECK_TESTS=none run || fail 'none failed'
[ "$(cat "$AM_TEST_CARGO_LOG")" = 'check -p agents-managerd --all-targets --locked' ] || fail 'none must skip both tests'
echo 'check-changed-release-triage: 4 orchestration cases passed'
