#!/usr/bin/env bash
# scripts/ci-rules.tsv 的對拍與結構測試：
#   1. 每列四欄、不留空，欄位值合法，ops 指定的測試檔存在；最後一列是 `*`→FULL（沒有路徑掉出表外）。
#   2. scripts/ci-rules-golden.tsv：改成單一對照表之前，三支腳本對近 300 個 commit 的改動檔輸出的快照；
#      現在的 ci-changed-parts.sh／ci-daemon-filters.sh（非 @rust 的列）要逐例相同。
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
echo "ci-rules_test: ok（$n 個對拍路徑）"
