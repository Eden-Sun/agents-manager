#!/usr/bin/env bash
# #1987：check.sh 跑在 `set -euo pipefail` 下，`printf … | grep -q` 的 grep 一命中就退，輸入夠大時 printf 會 EPIPE、
# 整條 pipeline 非零（#1986）。判斷一律走 here-string；這支守住兩件事：check.sh 不再有 `| grep -q` 管線，
# 以及上萬行變更檔下 check.sh changed 照樣跑完、part 判斷不變。假 git／cargo 放在暫存 fixture 裡。
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
fail=0
bad() { printf 'FAIL: %s\n' "$*" >&2; fail=1; }

# 1. 靜態：判斷式不經 pipe（註解行不算）。不管來源是 echo、printf 還是別的指令，也不管 q 跟哪些旗標黏在一起
# （-q、-Eq、-qx、-qxF、-qE、-E -q、--quiet／--silent），一律抓。
pipe_q='\|[[:space:]]*grep([[:space:]]+-[^[:space:]]+)*[[:space:]]+(-[A-Za-z]*q[A-Za-z]*|--quiet|--silent)([[:space:]]|$)'
for sample in "echo \"\$x\" | grep -q a" "printf '%s\\n' \"\$x\" | grep -Eq a" "echo \"\$x\" | grep -qx a" \
    "printf '%s\\n' \"\$x\" | grep -qxF -- a" "echo \"\$x\" |grep -qE a" "echo \"\$x\" | grep -E -q a" "echo \"\$x\" | grep --quiet a"; do
    grep -Eq "$pipe_q" <<<"$sample" || bad "靜態檢查漏抓：$sample"
done
for sample in "grep -qx a <<<\"\$x\"" "printf '%s\\n' \"\$x\" | grep -E a >/dev/null" "printf '%s\\n' \"\$x\" | grep -Ev a"; do
    if grep -Eq "$pipe_q" <<<"$sample"; then bad "靜態檢查誤抓：$sample"; fi
done
hits="$(grep -nE "$pipe_q" "$ROOT/scripts/check.sh" | grep -vE '^[0-9]+:[[:space:]]*#' || true)"
[ -z "$hits" ] || bad "check.sh 仍有 '| grep -q…' 管線，pipefail 下大量輸入會 EPIPE：
$hits"

# 2. 行為：兩萬行變更檔下，check.sh changed 要跑完且只選 daemon。
tmp="$(mktemp -d "${TMPDIR:-/tmp}/am-check-pipe-grep.XXXXXX")"
trap '/bin/rm -rf "$tmp"' EXIT
fx="$tmp/repo"
mkdir -p "$fx/scripts" "$fx/web" "$fx/bin"
cp "$ROOT/scripts/check.sh" "$ROOT/scripts/ci-changed-parts.sh" "$ROOT/scripts/ci-daemon-filters.sh" "$fx/scripts/"
seq -f 'daemon/src/gen/m%05g.rs' 1 20000 >"$tmp/changed"

cat >"$fx/bin/git" <<'SH'
#!/usr/bin/env bash
case "$*" in
    "diff --name-only origin/main...HEAD") ;;
    "diff --name-only HEAD") cat "$AM_TEST_CHANGED_FILE" ;;
    "ls-files --others --exclude-standard") ;;
    *) printf 'unexpected git invocation: %s\n' "$*" >&2; exit 90 ;;
esac
SH
cat >"$fx/bin/cargo" <<'SH'
#!/usr/bin/env bash
case "$1" in
    check) exit 0 ;;
    test)
        printf 'running 1 test\ntest a::tests::fine ... ok\n\ntest result: ok. 1 passed; 0 failed; 0 ignored\n'
        exit 0 ;;
esac
SH
chmod +x "$fx/bin/git" "$fx/bin/cargo"

rc=0
out="$(cd "$fx" && AM_TEST_CHANGED_FILE="$tmp/changed" CHECK_TESTS=x PATH="$fx/bin:$PATH" bash scripts/check.sh changed origin/main 2>&1)" || rc=$?
[ "$rc" = 0 ] || bad "兩萬行變更檔下 check.sh changed 中途退出（rc=${rc}）：$(printf '%s\n' "$out" | tail -20)"
case "$out" in *"daemon: cargo test"*) ;; *) bad "兩萬行 daemon 變更卻沒跑 daemon 測試：$(printf '%s\n' "$out" | tail -20)" ;; esac
case "$out" in *"web"*"bun"*) bad "只改 daemon 卻跑了 web：$(printf '%s\n' "$out" | tail -20)" ;; esac

[ "$fail" = 0 ] && echo "check.sh pipe-grep guard: OK"
exit "$fail"
