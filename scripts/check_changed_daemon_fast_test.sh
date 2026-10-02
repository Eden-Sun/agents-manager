#!/usr/bin/env bash
# Clean-checkout daemon fast path: rust-embed needs an index.html, but should not trigger web CI.
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
tmp="$(mktemp -d "${TMPDIR:-/tmp}/am-check-changed-daemon.XXXXXX")"
trap '/bin/rm -rf "$tmp"' EXIT

fixture="$tmp/repo"
mkdir -p "$fixture/scripts" "$fixture/web" "$fixture/bin"
cp "$ROOT/scripts/check.sh" "$fixture/scripts/check.sh"
cp "$ROOT/scripts/ci-changed-parts.sh" "$fixture/scripts/ci-changed-parts.sh"
cp "$ROOT/scripts/ci-daemon-filters.sh" "$fixture/scripts/ci-daemon-filters.sh"

cat >"$fixture/bin/git" <<'SH'
#!/usr/bin/env bash
# 改動清單由 $AM_TEST_DIFF_HEAD 給（一行一個）；$AM_TEST_GIT_FAIL 設成某個子指令的整串參數，那一條就失敗（exit 128）。
if [ -n "${AM_TEST_GIT_FAIL:-}" ] && [ "$*" = "$AM_TEST_GIT_FAIL" ]; then
    echo "fatal: bad revision" >&2
    exit 128
fi
case "$*" in
    "diff --name-only origin/main...HEAD") ;;
    "diff --name-only HEAD") printf '%s\n' "${AM_TEST_DIFF_HEAD:-daemon/src/main.rs}" ;;
    "ls-files --others --exclude-standard") ;;
    *) printf 'unexpected git invocation: %s\n' "$*" >&2; exit 90 ;;
esac
SH

cat >"$fixture/bin/bun" <<'SH'
#!/usr/bin/env bash
printf '%s\n' "$*" >>"$AM_TEST_BUN_LOG"
SH

cat >"$fixture/bin/bunx" <<'SH'
#!/usr/bin/env bash
printf 'bunx %s\n' "$*" >>"$AM_TEST_BUN_LOG"
SH

cat >"$fixture/bin/cargo" <<'SH'
#!/usr/bin/env bash
printf '%s\n' "$*" >>"$AM_TEST_CARGO_LOG"
if [ "${1:-}" = test ] && [ -n "${AM_TEST_CARGO_FAIL_ON_TEST:-}" ]; then
    echo "test foo::bar ... FAILED" >&2
    exit 101
fi
if [ -f web/dist/index.html ]; then
    printf 'web-dist-stub-present\n' >>"$AM_TEST_CARGO_LOG"
else
    printf 'web-dist-stub-missing\n' >>"$AM_TEST_CARGO_LOG"
fi
SH

chmod +x "$fixture/bin/git" "$fixture/bin/bun" "$fixture/bin/bunx" "$fixture/bin/cargo"
export AM_TEST_BUN_LOG="$tmp/bun.log"
export AM_TEST_CARGO_LOG="$tmp/cargo.log"

changed() { (cd "$fixture" && PATH="$fixture/bin:$PATH" bash scripts/check.sh changed origin/main 2>&1); }
if ! output="$(changed)"; then
    printf 'changed daemon fast path failed:\n%s\n' "$output" >&2
    exit 1
fi
if [ -s "$AM_TEST_BUN_LOG" ]; then
    printf 'daemon changed path invoked web checks:\n%s\n' "$(cat "$AM_TEST_BUN_LOG")" >&2
    printf '%s\n' "$output" >&2
    exit 1
fi
if ! grep -q '^web-dist-stub-present$' "$AM_TEST_CARGO_LOG"; then
    printf 'cargo check did not receive a temporary rust-embed stub:\n%s\n' "$(cat "$AM_TEST_CARGO_LOG" 2>/dev/null || true)" >&2
    exit 1
fi
if [ -e "$fixture/web/dist/index.html" ]; then
    echo 'changed daemon fast path left its temporary web/dist stub behind' >&2
    exit 1
fi

echo 'changed daemon fast path uses and cleans a temporary web/dist stub'

fail() { printf '%s\n' "$1" >&2; exit 1; }
cargo_tests() { grep -E '^test ' "$AM_TEST_CARGO_LOG" || true; }

# main.rs 不是模組：沒有可挑的測試子集，只有 cargo check。
[ -z "$(cargo_tests)" ] || fail "main.rs 改動不該跑 cargo test：$(cargo_tests)"

# 改了某個模組：除了 cargo check，還要跑那個模組自己的測試（以前只有 check，壞掉的行為過閘）。
: >"$AM_TEST_CARGO_LOG"
export AM_TEST_DIFF_HEAD=daemon/src/lifecycle/queue.rs
output="$(changed)" || fail "改 queue.rs 的 changed 失敗：$output"
grep -qx 'test -p agents-managerd --locked -- lifecycle::queue::' "$AM_TEST_CARGO_LOG" \
    || fail "改 queue.rs 沒有跑 lifecycle::queue:: 的測試：$(cat "$AM_TEST_CARGO_LOG")"
[ -e "$fixture/web/dist/index.html" ] && fail 'stub 沒清掉'

# 兩個模組：兩個過濾字串都帶上。
: >"$AM_TEST_CARGO_LOG"
export AM_TEST_DIFF_HEAD=$'daemon/src/lifecycle/queue.rs\ndaemon/src/hookrecv.rs'
changed >/dev/null || fail "兩個模組的 changed 失敗"
grep -qx 'test -p agents-managerd --locked -- hookrecv:: lifecycle::queue::' "$AM_TEST_CARGO_LOG" \
    || fail "多模組的過濾字串不對：$(cat "$AM_TEST_CARGO_LOG")"

# 明講不要測試：只 check。明講要哪個：照它。
: >"$AM_TEST_CARGO_LOG"
export AM_TEST_DIFF_HEAD=daemon/src/lifecycle/queue.rs
CHECK_TESTS=none changed >/dev/null || fail "CHECK_TESTS=none 失敗"
[ -z "$(cargo_tests)" ] || fail "CHECK_TESTS=none 還是跑了測試：$(cargo_tests)"
CHECK_TESTS=prompt:: changed >/dev/null || fail "CHECK_TESTS=prompt:: 失敗"
grep -qx 'test -p agents-managerd --locked prompt::' "$AM_TEST_CARGO_LOG" || fail "CHECK_TESTS 沒照給的跑：$(cat "$AM_TEST_CARGO_LOG")"

# 測試紅要讓 changed 紅（結束碼不能被 pipe 吞掉）。
export AM_TEST_CARGO_FAIL_ON_TEST=1
if changed >/dev/null; then fail "cargo test 失敗但 changed 回了 0"; fi
unset AM_TEST_CARGO_FAIL_ON_TEST

# base 比不出來（沒 fetch、淺 clone、沒有共同祖先）：不能當成「沒有改動」放行——以前 `{ git diff A; git diff B; } | …` 的結束碼只看最後一個，
# 第一個壞了會被吞掉，工作樹乾淨時印「只有文件類改動，不用跑」然後綠燈。
export AM_TEST_DIFF_HEAD=''
export AM_TEST_GIT_FAIL='diff --name-only origin/main...HEAD'
if out="$(changed)"; then fail "base 比不出來卻放行了：$out"; fi
printf '%s' "$out" | grep -q 'origin/main' || fail "沒有說是 base 的問題：$out"
unset AM_TEST_GIT_FAIL
export AM_TEST_GIT_FAIL='ls-files --others --exclude-standard'
if out="$(changed)"; then fail "ls-files 失敗卻放行了：$out"; fi
unset AM_TEST_GIT_FAIL

echo 'changed: 受影響模組的測試、CHECK_TESTS、測試紅與 git 失敗都不會被吞掉'
