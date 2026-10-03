#!/usr/bin/env bash
# 等主機負載降下來再往下跑（issue #813）。取代各 agent 自己手寫的「等負載」迴圈——#789 的子 agent 寫的那個
# 逾時卻回成功，在 load≈152 時照跑。這支的規矩：**只有負載真的降到門檻以下才 exit 0**，其他一律非 0。
#
#   scripts/wait-load.sh [--max N] [--timeout 秒] [--interval 秒]
#
#   --max       1 分鐘負載 ≤ N 就放行（可帶小數）。預設＝核心數 × 1.25（32 核＝40）。
#   --timeout   最多等幾秒（預設 1800）；0＝只看一次。
#   --interval  多久看一次（預設 15 秒）。
#
# 結束碼：0＝負載已在門檻以下；124＝等到逾時負載還是太高（**不准照跑**）；3＝讀不到負載；2＝參數錯。
# 典型用法：`scripts/wait-load.sh --max 40 && cargo test …`（逾時 `&&` 就不會跑），或 `CHECK_MAX_LOAD=40 scripts/check.sh changed`。
#
# 負載來源：Linux 的 /proc/loadavg，macOS 的 `sysctl -n vm.loadavg`。`WAIT_LOAD_FILE` 指到一個檔案時改讀它的第一欄（測試用）。
set -euo pipefail

usage() {
    sed -n '2,15p' "$0" | sed 's/^# \{0,1\}//' >&2
    exit 2
}

cores() {
    local n
    n="$(getconf _NPROCESSORS_ONLN 2>/dev/null || sysctl -n hw.ncpu 2>/dev/null || echo 4)"
    case "${n}" in '' | *[!0-9]*) n=4 ;; esac
    printf '%s' "${n}"
}

# 現在的 1 分鐘負載（字串，可能帶小數）；讀不到回非 0。
load1() {
    local v=""
    if [ -n "${WAIT_LOAD_FILE:-}" ]; then
        v="$(awk 'NR == 1 {print $1}' "${WAIT_LOAD_FILE}" 2>/dev/null || true)"
    elif [ -r /proc/loadavg ]; then
        v="$(awk '{print $1}' /proc/loadavg 2>/dev/null || true)"
    else
        v="$(sysctl -n vm.loadavg 2>/dev/null | tr -d '{}' | awk '{print $1}' || true)"
    fi
    case "${v}" in '' | *[!0-9.]* | *.*.*) return 1 ;; esac
    printf '%s' "${v}"
}

is_number() {
    case "$1" in '' | . | *[!0-9.]* | *.*.*) return 1 ;; esac
    return 0
}

is_uint() {
    case "$1" in '' | *[!0-9]*) return 1 ;; esac
    return 0
}

max=""
timeout=1800
interval=15
while [ "$#" -gt 0 ]; do
    case "$1" in
        --max) [ "$#" -ge 2 ] || usage; max="$2"; shift 2 ;;
        --timeout) [ "$#" -ge 2 ] || usage; timeout="$2"; shift 2 ;;
        --interval) [ "$#" -ge 2 ] || usage; interval="$2"; shift 2 ;;
        -h | --help) usage ;;
        *) echo "wait-load: 看不懂的參數 $1" >&2; usage ;;
    esac
done
[ -n "${max}" ] || max="$(awk -v c="$(cores)" 'BEGIN {printf "%d", c * 1.25}')"
if ! is_number "${max}" || ! is_uint "${timeout}" || ! is_uint "${interval}" || [ "${interval}" -lt 1 ]; then
    echo "wait-load: --max 要是數字、--timeout 要是非負整數、--interval 要是正整數（拿到 max=${max} timeout=${timeout} interval=${interval}）" >&2
    exit 2
fi

start="$(date +%s)"
announced=""
while :; do
    if ! now_load="$(load1)"; then
        echo "wait-load: 讀不到主機負載，不放行" >&2
        exit 3
    fi
    if awk -v l="${now_load}" -v m="${max}" 'BEGIN {exit !(l <= m)}'; then
        [ -z "${announced}" ] || echo "wait-load: 負載 ${now_load} ≤ ${max}，放行" >&2
        exit 0
    fi
    elapsed=$(($(date +%s) - start))
    if [ "${elapsed}" -ge "${timeout}" ]; then
        echo "wait-load: 等了 ${elapsed} 秒負載還是 ${now_load}（門檻 ${max}），逾時——不放行（exit 124）" >&2
        exit 124
    fi
    if [ -z "${announced}" ]; then
        echo "wait-load: 負載 ${now_load} > ${max}，每 ${interval} 秒再看一次，最多等 ${timeout} 秒……" >&2
        announced=1
    fi
    left=$((timeout - elapsed))
    if [ "${left}" -lt "${interval}" ]; then sleep "${left}"; else sleep "${interval}"; fi
done
