#!/usr/bin/env bash
# web 的 bun test 切成 N 個 shard 平行跑（2026-10-23 診斷：472 檔單程序序列跑 190 秒，8 個 shard 平行 38 秒）。
#
#   scripts/web-test-shards.sh            # N = min(8, nproc)，由 check.sh 叫
#   CHECK_WEB_SHARDS=1 scripts/web-test-shards.sh   # 退回單程序（除錯跨檔順序問題時用）
#
# 為什麼不用 bun 自己的 --parallel：它隱含 --isolate，domHarness 的 react-dom 載入順序在 isolate 下
# 炸（createRoot TDZ），748 條紅。--shard=i/N 是各自獨立的完整程序，跟序列跑同一套環境。
# 每個 shard 的輸出各寫一個暫存檔，跑完依序印出，紅的 shard 最後再點名一次；任何 shard 非 0 整輪失敗。
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
# 紅的 shard 單獨再跑一次（機器空著、沒有別的 shard 搶 CPU）：平行跑時的逾時或「前一檔 async 落在 teardown 之後」
# 這類跨檔 timing 紅，單跑一次就綠；真的紅會再紅一次。重跑過的 shard 一定點名，不當成沒事。
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
