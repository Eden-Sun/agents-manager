#!/usr/bin/env bash
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CANARY="${HERE}/destructive-canary.sh"
ROOT="$(mktemp -d)"
trap '/bin/rm -rf "${ROOT}"' EXIT

pass=0
fail=0
check() {
    if [ "$2" = "$3" ]; then
        echo "ok   - $1"
        pass=$((pass + 1))
    else
        echo "FAIL - $1（期望 rc=$2，實得 rc=$3）"
        fail=$((fail + 1))
    fi
}

D="${ROOT}/canary"

rc=$(bash -c 'eval "$('"${CANARY}"' arm "'"${D}"'")"; am-canary-probe x >/dev/null 2>&1; echo $?')
check "bash：直接叫 sentinel 被擋" 99 "${rc}"

# canary-gap: 這裡的覆寫 PATH 是**被測對象本身**，不是漏擋——整支測試就是在證明覆寫之後
rc=$(bash -c 'eval "$('"${CANARY}"' arm "'"${D}"'")"; PATH=/usr/bin:/bin; am-canary-probe x >/dev/null 2>&1; echo $?')
check "bash：覆寫 PATH 後仍被擋（匯出的函式層）" 99 "${rc}"

rc=$(bash -c 'eval "$('"${CANARY}"' arm "'"${D}"'")"; bash -c "PATH=/usr/bin:/bin; am-canary-probe x >/dev/null 2>&1; echo \$?"')
check "子 bash：覆寫 PATH 後仍被擋" 99 "${rc}"

if command -v zsh >/dev/null 2>&1; then
    rc=$(bash -c 'eval "$('"${CANARY}"' arm "'"${D}"'")"; zsh -c "PATH=/usr/bin:/bin; am-canary-probe x >/dev/null 2>&1; echo \$?"')
    check "子 zsh：覆寫 PATH 後仍被擋（ZDOTDIR 層）" 99 "${rc}"
    rc=$(bash -c 'eval "$('"${CANARY}"' arm "'"${D}"'")"; zsh -c "zsh -c \"PATH=/usr/bin:/bin; am-canary-probe x >/dev/null 2>&1; echo \\\$?\""')
    check "巢狀 zsh：仍被擋" 99 "${rc}"
else
    echo "skip - 沒有 zsh，跳過 ZDOTDIR 那層"
fi

rc=$(bash -c 'eval "$('"${CANARY}"' arm "'"${D}"'")"; rm /nonexistent-am-canary-sentinel >/dev/null 2>&1; echo $?')
check "rm：目標在暫存目錄之外被擋" 99 "${rc}"

legit="${ROOT}/legit"
rc=$(bash -c 'eval "$('"${CANARY}"' arm "'"${D}"'")"; mkdir -p "'"${legit}"'/sub"; rm -rf "'"${legit}"'"; echo $?')
check "rm：暫存目錄之內照常刪得掉" 0 "${rc}"
if [ -d "${legit}" ]; then
    echo "FAIL - rm 暫存目錄之內應該真的刪掉，但目錄還在"
    fail=$((fail + 1))
else
    echo "ok   - rm 暫存目錄之內真的刪掉了"
    pass=$((pass + 1))
fi

CLEAN="${ROOT}/clean"
bash -c 'eval "$('"${CANARY}"' arm "'"${CLEAN}"'")" >/dev/null'
"${CANARY}" hits "${CLEAN}" >/dev/null 2>&1
check "hits：沒人叫到時回 0" 0 "$?"
DIRTY="${ROOT}/dirty"
bash -c 'eval "$('"${CANARY}"' arm "'"${DIRTY}"'")" >/dev/null; am-canary-probe x >/dev/null 2>&1'
"${CANARY}" hits "${DIRTY}" >/dev/null 2>&1
check "hits：有人叫到時回非 0" 1 "$?"

if "${CANARY}" cmds | grep -qw bootout; then
    echo "FAIL - cmds 仍列著 bootout（不是可執行檔，那個 stub 永遠不會被叫到）"
    fail=$((fail + 1))
else
    echo "ok   - cmds 沒有列 bootout"
    pass=$((pass + 1))
fi

legit2="${ROOT}/legit-child"
rc=$(bash -c 'eval "$('"${CANARY}"' arm "'"${D}"'")"; bash -c "mkdir -p '"${legit2}"'/sub && rm -rf '"${legit2}"' && echo \$?"')
check "子 bash：暫存目錄內的 rm 照常能刪（助手函式也要匯出）" 0 "${rc}"
if [ -d "${legit2}" ]; then
    echo "FAIL - 子 bash 的 rm 應該真的刪掉，但目錄還在"
    fail=$((fail + 1))
else
    echo "ok   - 子 bash 的 rm 真的刪掉了"
    pass=$((pass + 1))
fi

BL="scripts/ops/canary-baseline.tsv"
if [ -f "${BL}" ]; then
    WORK="${ROOT}/repo"
    mkdir -p "${WORK}/scripts/ops"
    cp "${BL}" "${WORK}/${BL}"
    dup="${WORK}/dup.tsv"
    { cat "${BL}"; printf 'scripts/ops/pane-gc_test.sh\t99\n'; } > "${WORK}/${BL}"
    ( cd "${WORK}" && "${CANARY}" lint >/dev/null 2>&1 ); rc=$?
    check "baseline 有重複檔名時整支要紅" 1 "${rc}"
    { cat "${BL}"; printf '<<<<<<< HEAD\n'; } > "${WORK}/${BL}"
    ( cd "${WORK}" && "${CANARY}" lint >/dev/null 2>&1 ); rc=$?
    check "baseline 留著衝突標記時整支要紅" 1 "${rc}"
    : "${dup}"
fi

rc=$(bash -c 'eval "$('"${CANARY}"' arm "'"${D}"'")"; cd /tmp && rm ../nonexistent-am-canary-sentinel >/dev/null 2>&1; echo $?')
check "rm：cd 之後用 ../ 指到暫存外也要擋" 99 "${rc}"
link="${ROOT}/linkfarm"
mkdir -p "${link}"
ln -sfn / "${link}/toroot" 2>/dev/null || true
if [ -L "${link}/toroot" ]; then
    rc=$(bash -c 'eval "$('"${CANARY}"' arm "'"${D}"'")"; rm "'"${link}"'/toroot/nonexistent-am-canary-sentinel" >/dev/null 2>&1; echo $?')
    check "rm：經 symlink 指到暫存外也要擋" 99 "${rc}"
fi

echo "${pass} passed, ${fail} failed"
[ "${fail}" -eq 0 ]
