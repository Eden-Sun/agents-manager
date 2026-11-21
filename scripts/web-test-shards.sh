#!/usr/bin/env bash
set -u
cd "$(dirname "$0")/../web" || exit 1

n="${CHECK_WEB_SHARDS:-}"
if [ -z "$n" ]; then
    cores="$(nproc 2>/dev/null || sysctl -n hw.ncpu 2>/dev/null || echo 4)"
    n=$(( cores < 8 ? cores : 8 ))
fi
if [ "$n" -le 1 ]; then
    exec bun test "$@"
fi

tmp="$(mktemp -d)"
trap 'rm -rf "$tmp"' EXIT
pids=()
for i in $(seq 1 "$n"); do
    bun test --shard="$i/$n" "$@" > "$tmp/$i.log" 2>&1 &
    pids+=($!)
done
rc=0
failed=()
retried=()
for i in $(seq 1 "$n"); do
    wait "${pids[$((i - 1))]}" || failed+=("$i")
done
for i in $(seq 1 "$n"); do
    printf '\n--- shard %s/%s ---\n' "$i" "$n"
    cat "$tmp/$i.log"
done
for i in ${failed[@]+"${failed[@]}"}; do
    printf '\n--- shard %s/%s 第一次紅，單獨重跑 ---\n' "$i" "$n"
    if bun test --shard="$i/$n" "$@" > "$tmp/$i.retry.log" 2>&1; then
        tail -4 "$tmp/$i.retry.log"
        retried+=("$i")
    else
        cat "$tmp/$i.retry.log"
        rc=1
    fi
done
total="$(grep -h '^Ran ' "$tmp"/*.log | awk '{t+=$2; f+=$5} END {printf "%d 條、%d 檔", t, f}')"
printf '\n==> bun test：%s 個 shard，共 %s\n' "$n" "$total"
if [ "${#retried[@]}" -gt 0 ]; then
    printf '!! shard %s 平行跑時紅、單獨重跑綠（多半是逾時或跨檔 timing）；連續出現就去找那個 shard 裡漏 async 的檔。\n' "${retried[*]}" >&2
fi
if [ "$rc" -ne 0 ]; then
    printf '!! 重跑仍紅的 shard：%s（往上找該段的 (fail)／error；要序列重跑同一組：bun test --shard=<i>/%s）\n' "${failed[*]}" "$n" >&2
fi
exit "$rc"
