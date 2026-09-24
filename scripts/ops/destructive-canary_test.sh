#!/usr/bin/env bash
# destructive-canary.sh 自己的隔離測試（issue #422）。
#
# **全程只用 `am-canary-probe` 這個無害的 sentinel 指令名**：驗證護欄時絕對不能拿真的
# `launchctl`／`pkill` 當白老鼠——護欄沒生效時白老鼠就變成真的破壞，#422 就是這樣來的。
# `rm` 的案例用「絕對路徑、暫存目錄之外、而且不存在」的 sentinel 路徑，而且不加 `-rf`：
# 護欄萬一失效，真 `rm` 也只會回「No such file」，不可能刪到任何東西。
set -uo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
CANARY="${HERE}/destructive-canary.sh"
ROOT="$(mktemp -d)"
trap '/bin/rm -rf "${ROOT}"' EXIT

pass=0
fail=0
check() { # check <說明> <期望 rc> <實際 rc>
    if [ "$2" = "$3" ]; then
        echo "ok   - $1"
        pass=$((pass + 1))
    else
        echo "FAIL - $1（期望 rc=$2，實得 rc=$3）"
        fail=$((fail + 1))
    fi
}

D="${ROOT}/canary"

# 1. bash：PATH 層。
rc=$(bash -c 'eval "$('"${CANARY}"' arm "'"${D}"'")"; am-canary-probe x >/dev/null 2>&1; echo $?')
check "bash：直接叫 sentinel 被擋" 99 "${rc}"

# 2. bash：**整個覆寫 PATH** 之後還是要被擋（這是 #418 出事的那一類；只有 PATH 層時這裡會是 127）。
# canary-gap: 這裡的覆寫 PATH 是**被測對象本身**，不是漏擋——整支測試就是在證明覆寫之後
# 函式層仍然攔得住（斷言 rc=99）。用的又只有無害的 am-canary-probe，不會碰到任何真指令。
rc=$(bash -c 'eval "$('"${CANARY}"' arm "'"${D}"'")"; PATH=/usr/bin:/bin; am-canary-probe x >/dev/null 2>&1; echo $?')
check "bash：覆寫 PATH 後仍被擋（匯出的函式層）" 99 "${rc}"

# 3. 子 bash（測試都是 `bash "$t"` 這樣跑的）覆寫 PATH。
rc=$(bash -c 'eval "$('"${CANARY}"' arm "'"${D}"'")"; bash -c "PATH=/usr/bin:/bin; am-canary-probe x >/dev/null 2>&1; echo \$?"')
check "子 bash：覆寫 PATH 後仍被擋" 99 "${rc}"

# 4. 子 zsh：bash 的匯出函式進不了 zsh，靠 ZDOTDIR/.zshenv 那層。
if command -v zsh >/dev/null 2>&1; then
    rc=$(bash -c 'eval "$('"${CANARY}"' arm "'"${D}"'")"; zsh -c "PATH=/usr/bin:/bin; am-canary-probe x >/dev/null 2>&1; echo \$?"')
    check "子 zsh：覆寫 PATH 後仍被擋（ZDOTDIR 層）" 99 "${rc}"
    rc=$(bash -c 'eval "$('"${CANARY}"' arm "'"${D}"'")"; zsh -c "zsh -c \"PATH=/usr/bin:/bin; am-canary-probe x >/dev/null 2>&1; echo \\\$?\""')
    check "巢狀 zsh：仍被擋" 99 "${rc}"
else
    echo "skip - 沒有 zsh，跳過 ZDOTDIR 那層"
fi

# 5. rm：暫存目錄之外要擋。
rc=$(bash -c 'eval "$('"${CANARY}"' arm "'"${D}"'")"; rm /nonexistent-am-canary-sentinel >/dev/null 2>&1; echo $?')
check "rm：目標在暫存目錄之外被擋" 99 "${rc}"

# 6. rm：暫存目錄之內要照常能刪（第一版用 command -v 找真 rm，函式名把自己叫回去，
#    無限遞迴到 SIGSEGV，而且踩的正是這條正當路徑）。
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

# 7. hits：乾淨要回 0、髒要回非 0（check.sh 靠這個判斷整輪紅不紅）。
CLEAN="${ROOT}/clean"
bash -c 'eval "$('"${CANARY}"' arm "'"${CLEAN}"'")" >/dev/null'
"${CANARY}" hits "${CLEAN}" >/dev/null 2>&1
check "hits：沒人叫到時回 0" 0 "$?"
# 注意：`arm` 會 `rm -rf` 掉整個 canary 目錄，等於把 hits.log 清空。所以要驗「髒」必須
# 用一個**armed 之後就沒有再 arm 過**的目錄，不能沿用上面被重複 arm 的那個（第一版就是這樣
# 誤判成乾淨的）。
DIRTY="${ROOT}/dirty"
bash -c 'eval "$('"${CANARY}"' arm "'"${DIRTY}"'")" >/dev/null; am-canary-probe x >/dev/null 2>&1'
"${CANARY}" hits "${DIRTY}" >/dev/null 2>&1
check "hits：有人叫到時回非 0" 1 "$?"

# 8. bootout 不該在清單裡（它是 launchctl 的子命令，不是可執行檔）。
if "${CANARY}" cmds | grep -qw bootout; then
    echo "FAIL - cmds 仍列著 bootout（不是可執行檔，那個 stub 永遠不會被叫到）"
    fail=$((fail + 1))
else
    echo "ok   - cmds 沒有列 bootout"
    pass=$((pass + 1))
fi

echo "${pass} passed, ${fail} failed"
[ "${fail}" -eq 0 ]
