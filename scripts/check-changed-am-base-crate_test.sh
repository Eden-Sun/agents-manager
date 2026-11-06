#!/usr/bin/env bash
# check.sh changed 對 `__am_base_crate__`（ci-daemon-filters.sh 印的）的編排（#2072）：只改了 am-base crate 自己的測試會讀的
# 非 Rust 檔（baseline.toml、host-baseline-sync-mac-verify.sh、codex-async fixtures）時，要補跑 `cargo test -p am-base`。
set -euo pipefail
unset CHECK_TESTS CHECK_MAX_LOAD
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
tmp="$(mktemp -d "${TMPDIR:-/tmp}/check-am-base-crate.XXXXXX")"
trap '/bin/rm -rf "$tmp"' EXIT
repo="$tmp/repo"
mkdir -p "$repo/scripts" "$repo/web" "$tmp/bin"
cp "$ROOT/scripts/check.sh" "$repo/scripts/check.sh"
# 假的選測腳本要讀完 stdin（#1986，同 check-changed-release-triage_test.sh）。
printf '#!/bin/bash\ncat >/dev/null\necho daemon\n' > "$repo/scripts/ci-changed-parts.sh"
printf '#!/bin/bash\ncat >/dev/null\nprintf "%%s\\n" $AM_TEST_FILTERS\n' > "$repo/scripts/ci-daemon-filters.sh"
cat > "$tmp/bin/git" <<'SH'
#!/bin/bash
case "$*" in
  'diff --name-only HEAD') echo ops/host-baseline/baseline.toml ;;
  'diff --name-only origin/main...HEAD' | 'ls-files --others --exclude-standard') ;;
  *) exit 90 ;;
esac
SH
cat > "$tmp/bin/cargo" <<'SH'
#!/bin/bash
printf '%s\n' "$*" >> "$AM_TEST_CARGO_LOG"
[ "$1" != test ] || echo 'test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s'
exit 0
SH
chmod +x "$tmp/bin/git" "$tmp/bin/cargo"
export AM_TEST_CARGO_LOG="$tmp/cargo.log"
run() { : > "$AM_TEST_CARGO_LOG"; (cd "$repo" && PATH="$tmp/bin:$PATH" bash scripts/check.sh changed) > "$tmp/output" 2>&1; }
fail() { echo "FAIL: $*" >&2; cat "$tmp/output" "$AM_TEST_CARGO_LOG" >&2; exit 1; }

AM_TEST_FILTERS=__am_base_crate__ run || fail 'marker alone failed'
[ "$(cat "$AM_TEST_CARGO_LOG")" = "check -p agents-managerd --all-targets --locked
test -p am-base --locked" ] || fail 'marker alone must run cargo check plus the am-base crate tests'
! grep -q '沒有可挑的測試子集' "$tmp/output" || fail 'marker alone must not claim there is nothing to test'

AM_TEST_FILTERS='__am_base_crate__ runners::am_base_tests::outbox::' run || fail 'marker with filters failed'
[ "$(cat "$AM_TEST_CARGO_LOG")" = "test -p am-base --locked
test -p agents-managerd --locked -- runners::am_base_tests::outbox::" ] || fail 'marker must not reach cargo test as a filter'

AM_TEST_FILTERS= run || fail 'no marker failed'
[ "$(cat "$AM_TEST_CARGO_LOG")" = 'check -p agents-managerd --all-targets --locked' ] || fail 'without the marker am-base must not run'

CHECK_TESTS=none AM_TEST_FILTERS=__am_base_crate__ run || fail 'none failed'
[ "$(cat "$AM_TEST_CARGO_LOG")" = 'check -p agents-managerd --all-targets --locked' ] || fail 'none must skip the am-base crate tests'
echo 'check-changed-am-base-crate: 4 orchestration cases passed'
