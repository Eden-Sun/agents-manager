#!/usr/bin/env bash
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
tmp="$(mktemp -d "${TMPDIR:-/tmp}/am-check-failed-tests.XXXXXX")"
trap '/bin/rm -rf "$tmp"' EXIT
fx="$tmp/repo"
mkdir -p "$fx/scripts" "$fx/web" "$fx/bin"
cp "$ROOT/scripts/check.sh" "$ROOT/scripts/ci-changed-parts.sh" "$ROOT/scripts/ci-daemon-filters.sh" "$ROOT/scripts/ci-rules.tsv" "$fx/scripts/"
fail=0
bad() { printf 'FAIL: %s\n' "$*" >&2; fail=1; }

cat >"$fx/bin/git" <<'SH'
#!/usr/bin/env bash
case "$*" in
    "diff --name-only origin/main...HEAD") ;;
    "diff --name-only HEAD") printf '%s\n' "$AM_TEST_CHANGED" ;;
    "ls-files --others --exclude-standard") ;;
    *) printf 'unexpected git invocation: %s\n' "$*" >&2; exit 90 ;;
esac
SH
cat >"$fx/bin/cargo" <<'SH'
#!/usr/bin/env bash
case "$1" in
    check) exit 0 ;;
    test)
        printf 'running 3 tests\ntest a::tests::fine ... ok\ntest cli_update::tests::flaky_one ... FAILED\ntest b::tests::flaky_two ... FAILED\n'
        printf '\nfailures:\n\ntest result: FAILED. 1 passed; 2 failed; 0 ignored\n'
        exit 101 ;;
esac
SH
chmod +x "$fx/bin/git" "$fx/bin/cargo"

run() { (cd "$fx" && AM_TEST_CHANGED="$1" CHECK_TESTS="${2:-}" PATH="$fx/bin:$PATH" bash scripts/check.sh changed origin/main 2>&1) ; }

rc=0; out="$(run daemon/src/main.rs x)" || rc=$?
[ "$rc" != 0 ] || bad "daemon 測試紅時 check.sh 要非零"
case "$out" in *"2 條測試紅"*) ;; *) bad "沒有列出紅的測試數：$out" ;; esac
case "$out" in *"cli_update::tests::flaky_one"*"b::tests::flaky_two"*) ;; *) bad "沒有列出紅的測試名：$out" ;; esac
tail_part="$(printf '%s\n' "$out" | tail -5)"
case "$tail_part" in *"cli_update::tests::flaky_one"*) ;; *) bad "紅的測試要在輸出最後面，不是埋在中間：$tail_part" ;; esac

out="$(run scripts/ops/foo_test.sh x 2>&1 || true)"
case "$out" in *"CHECK_TESTS"*"沒有"*) ;; *) bad "ops 單獨改動帶 CHECK_TESTS 時沒有警告：$out" ;; esac

[ "$fail" = 0 ] && echo "check.sh failed-test summary: OK"
exit "$fail"
