#!/usr/bin/env bash
# check.sh 失敗時磁碟幾乎滿了：要明講是磁碟（No space left on device），不是叫人去看程式碼；磁碟夠的時候不吵；
# 成功時不吵。假 git／cargo／df 放在暫存 fixture 裡。
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
tmp="$(mktemp -d "${TMPDIR:-/tmp}/am-check-disk-hint.XXXXXX")"
trap '/bin/rm -rf "$tmp"' EXIT
fx="$tmp/repo"
mkdir -p "$fx/scripts" "$fx/web" "$fx/bin"
cp "$ROOT/scripts/check.sh" "$ROOT/scripts/ci-changed-parts.sh" "$ROOT/scripts/ci-daemon-filters.sh" "$fx/scripts/"
fail=0
bad() { printf 'FAIL: %s\n' "$*" >&2; fail=1; }

cat >"$fx/bin/git" <<'SH'
#!/usr/bin/env bash
case "$*" in
    "diff --name-only origin/main...HEAD") ;;
    "diff --name-only HEAD") printf 'daemon/src/main.rs\n' ;;
    "ls-files --others --exclude-standard") ;;
    *) printf 'unexpected git invocation: %s\n' "$*" >&2; exit 90 ;;
esac
SH
cat >"$fx/bin/cargo" <<'SH'
#!/usr/bin/env bash
rc="${AM_TEST_CARGO_RC:-0}"
if [ "${1:-}" = test ] && [ "$rc" = 0 ]; then
    echo 'test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s'
fi
exit "$rc"
SH
cat >"$fx/bin/df" <<'SH'
#!/usr/bin/env bash
printf 'Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/x 100000000 99500000 %s 99%% /\n' "$AM_TEST_DF_AVAIL_KB"
SH
chmod +x "$fx/bin/git" "$fx/bin/cargo" "$fx/bin/df"

run() { (cd "$fx" && AM_TEST_DF_AVAIL_KB="$1" AM_TEST_CARGO_RC="$2" PATH="$fx/bin:$PATH" bash scripts/check.sh changed origin/main 2>&1); }

rc=0; out="$(run 300000 1)" || rc=$?
[ "$rc" != 0 ] || bad "cargo 失敗時 check.sh 要非零"
case "$out" in *"磁碟"*"No space left on device"*) ;; *) bad "磁碟快滿時失敗沒有磁碟提示：$out" ;; esac

rc=0; out="$(run 90000000 1)" || rc=$?
case "$out" in *"No space left on device"*) bad "磁碟夠的時候不該出現磁碟提示：$out" ;; esac

rc=0; out="$(run 300000 0)" || rc=$?
[ "$rc" = 0 ] || bad "成功時要 exit 0：$out"
case "$out" in *"No space left on device"*) bad "成功時不該出現磁碟提示：$out" ;; esac

[ "$fail" = 0 ] && echo "check.sh disk hint: OK"
exit "$fail"
