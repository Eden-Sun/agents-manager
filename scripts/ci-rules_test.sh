#!/usr/bin/env bash
set -euo pipefail
cd "$(dirname "$0")/.."
rules=scripts/ci-rules.tsv
golden=scripts/ci-rules-golden.tsv
fail() { printf 'FAIL: %s\n' "$*" >&2; exit 1; }

last=
while IFS='|' read -r pat parts daemon ops extra; do
    case "$pat" in '' | '#'*) continue ;; esac
    [ -z "${extra:-}" ] && [ -n "$parts" ] && [ -n "$daemon" ] && [ -n "$ops" ] || fail "規則列欄位不對：$pat|$parts|$daemon|$ops${extra:+|$extra}"
    [[ "$parts" == - || "$parts" == FULL || "$parts" =~ ^(web|daemon|ops|ob)(,(web|daemon|ops|ob))*$ ]] || fail "parts 值不合法：$pat → $parts"
    case "$ops" in
        @default | __all__) ;;
        *) [ -f "$ops" ] || fail "ops 指定的測試檔不存在：$pat → $ops" ;;
    esac
    last="$pat|$parts"
done <"$rules"
[ "$last" = '*|FULL' ] || fail "最後一列必須是 *|FULL，現在是 $last"

n=0
while IFS='|' read -r f want_parts want_daemon; do
    case "$f" in '' | '#'*) continue ;; esac
    got_parts=$(printf '%s\n' "$f" | bash scripts/ci-changed-parts.sh | paste -sd, -)
    [ "${got_parts:--}" = "$want_parts" ] || fail "parts：$f → [${got_parts:--}]，預期 [$want_parts]"
    if [ "$want_daemon" != '@rust' ]; then
        got_daemon=$(printf '%s\n' "$f" | bash scripts/ci-daemon-filters.sh | paste -sd, -)
        [ "${got_daemon:--}" = "$want_daemon" ] || fail "daemon：$f → [${got_daemon:--}]，預期 [$want_daemon]"
    fi
    n=$((n + 1))
done <"$golden"
[ "$n" -gt 50 ] || fail "快照只讀到 $n 列"
grep -q 'bash scripts/ci-daemon-filters_test.sh' scripts/check.sh || fail "check.sh 的 daemon 部分沒有跑 ci-daemon-filters_test.sh"
[ "$(printf '%s\n' daemon/src/cli.rs | bash scripts/ci-changed-parts.sh | paste -sd, -)" = daemon ] || fail "daemon/src/cli.rs 沒有選到 daemon 部分"
echo "ci-rules_test: ok（$n 個對拍路徑）"
