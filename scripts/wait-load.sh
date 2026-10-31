#!/usr/bin/env bash
# 等主機負載降下來再往下跑（issue #813）。取代各 agent 自己手寫的「等負載」迴圈——#789 的子 agent 寫的那個
# 逾時卻回成功，在 load≈152 時照跑。這支的規矩：**只有負載真的降到門檻以下才 exit 0**，其他一律非 0。
#
#   scripts/wait-load.sh [--max N] [--min-free-mem 百分比] [--timeout 秒] [--interval 秒]
#
#   --max       1 分鐘負載 ≤ N 就放行（可帶小數）。預設＝核心數 × 1.25（32 核＝40）。
#   --min-free-mem  可用記憶體 ≥ 這個百分比、而且（macOS）記憶體壓力是 normal 才放行（issue #1783）。預設 0＝不看記憶體。
#               兩個條件都過才 exit 0。
#   --timeout   最多等幾秒（預設 1800）；0＝只看一次。
#   --interval  多久看一次（預設 15 秒）。
#
# 結束碼：0＝負載已在門檻以下；124＝等到逾時負載還是太高（**不准照跑**）；3＝讀不到負載；2＝參數錯。
# 典型用法：`scripts/wait-load.sh --max 40 && cargo test …`（逾時 `&&` 就不會跑），或 `CHECK_MAX_LOAD=40 scripts/check.sh changed`。
#
# 負載來源：Linux 的 /proc/loadavg，macOS 的 `sysctl -n vm.loadavg`。`WAIT_LOAD_FILE` 指到一個檔案時改讀它的第一欄（測試用）。
# 記憶體來源：Linux 的 /proc/meminfo（MemAvailable/MemTotal），macOS 的 `memory_pressure -Q`（free percentage）加
# `sysctl -n kern.memorystatus_vm_pressure_level`（1=normal、2=warn、4=critical；critical 不放行。warn 在 m4p 是常態
# （可用 39% 仍長駐 2，2026-10-31 使用者同意放寬），只要 free% 過門檻就放行；編譯中升到 critical 由 check.sh 看門狗中止）。
# `WAIT_MEM_FILE` 指到一個檔案時改讀它第一行：第一欄 free%、第二欄壓力等級（可省）（測試用）。
set -euo pipefail

usage() {
    # 檔頭就是說明書：印整段註解，不寫死行號（行號會隨著補充說明失準，印到程式碼）。
    awk 'NR > 1 && /^#/ { sub(/^# ?/, ""); print; next } NR > 1 { exit }' "$0" >&2
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

# 現在可用記憶體的百分比（整數）；讀不到回非 0。
mem_free_pct() {
    local v=""
    if [ -n "${WAIT_MEM_FILE:-}" ]; then
        v="$(awk 'NR == 1 {print $1}' "${WAIT_MEM_FILE}" 2>/dev/null || true)"
    elif [ -r /proc/meminfo ]; then
        v="$(awk '/^MemTotal:/ {t = $2} /^MemAvailable:/ {a = $2} END {if (t > 0 && a != "") printf "%d", a * 100 / t}' /proc/meminfo 2>/dev/null || true)"
    else
        v="$(memory_pressure -Q 2>/dev/null | sed -n 's/.*System-wide memory free percentage: *\([0-9][0-9]*\)%.*/\1/p' | head -1 || true)"
    fi
    is_uint "${v}" || return 1
    printf '%s' "${v}"
}

# 記憶體壓力等級（macOS：1=normal、2=warn、4=critical）；沒有這個來源（Linux）印空字串，看不懂回非 0。
mem_pressure_level() {
    local v=""
    if [ -n "${WAIT_MEM_FILE:-}" ]; then
        v="$(awk 'NR == 1 {print $2}' "${WAIT_MEM_FILE}" 2>/dev/null || true)"
    elif [ ! -r /proc/meminfo ]; then
        v="$(sysctl -n kern.memorystatus_vm_pressure_level 2>/dev/null || true)"
    fi
    [ -z "${v}" ] || is_uint "${v}" || return 1
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
min_free_mem=0
timeout=1800
interval=15
while [ "$#" -gt 0 ]; do
    case "$1" in
        --max) [ "$#" -ge 2 ] || usage; max="$2"; shift 2 ;;
        --min-free-mem) [ "$#" -ge 2 ] || usage; min_free_mem="$2"; shift 2 ;;
        --timeout) [ "$#" -ge 2 ] || usage; timeout="$2"; shift 2 ;;
        --interval) [ "$#" -ge 2 ] || usage; interval="$2"; shift 2 ;;
        -h | --help) usage ;;
        *) echo "wait-load: 看不懂的參數 $1" >&2; usage ;;
    esac
done
[ -n "${max}" ] || max="$(awk -v c="$(cores)" 'BEGIN {printf "%d", c * 1.25}')"
if ! is_number "${max}" || ! is_uint "${min_free_mem}" || [ "${min_free_mem}" -gt 100 ] || ! is_uint "${timeout}" || ! is_uint "${interval}" || [ "${interval}" -lt 1 ]; then
    echo "wait-load: --max 要是數字、--min-free-mem 要是 0～100 的整數、--timeout 要是非負整數、--interval 要是正整數（拿到 max=${max} min-free-mem=${min_free_mem} timeout=${timeout} interval=${interval}）" >&2
    exit 2
fi

start="$(date +%s)"
announced=""
while :; do
    if ! now_load="$(load1)"; then
        echo "wait-load: 讀不到主機負載，不放行" >&2
        exit 3
    fi
    blocked=""
    awk -v l="${now_load}" -v m="${max}" 'BEGIN {exit !(l <= m)}' || blocked="負載 ${now_load} > ${max}"
    status="負載 ${now_load}（門檻 ${max}）"
    if [ "${min_free_mem}" -gt 0 ]; then
        if ! free="$(mem_free_pct)" || ! pressure="$(mem_pressure_level)"; then
            echo "wait-load: 讀不到可用記憶體或記憶體壓力，不放行" >&2
            exit 3
        fi
        status="${status}、可用記憶體 ${free}%（門檻 ${min_free_mem}%）${pressure:+、壓力等級 ${pressure}}"
        if [ "${free}" -lt "${min_free_mem}" ]; then
            blocked="${blocked:+${blocked}、}可用記憶體 ${free}% < ${min_free_mem}%"
        fi
        if [ -n "${pressure}" ] && [ "${pressure}" != 1 ] && [ "${pressure}" != 2 ]; then
            blocked="${blocked:+${blocked}、}記憶體壓力等級 ${pressure}（critical）"
        fi
    fi
    if [ -z "${blocked}" ]; then
        [ -z "${announced}" ] || echo "wait-load: ${status}，放行" >&2
        exit 0
    fi
    elapsed=$(($(date +%s) - start))
    if [ "${elapsed}" -ge "${timeout}" ]; then
        echo "wait-load: 等了 ${elapsed} 秒還是 ${blocked}（${status}），逾時——不放行（exit 124）" >&2
        exit 124
    fi
    if [ -z "${announced}" ]; then
        echo "wait-load: ${blocked}，每 ${interval} 秒再看一次，最多等 ${timeout} 秒……" >&2
        announced=1
    fi
    left=$((timeout - elapsed))
    if [ "${left}" -lt "${interval}" ]; then sleep "${left}"; else sleep "${interval}"; fi
done
