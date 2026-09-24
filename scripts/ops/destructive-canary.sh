#!/usr/bin/env bash
# 隔離測試的破壞性指令護欄（issue #422）。
#
#   eval "$(scripts/ops/destructive-canary.sh arm <暫存目錄>)"   # 產生 canary 目錄並印出要 export 的 PATH
#   scripts/ops/destructive-canary.sh hits <暫存目錄>            # 印出被叫到的紀錄（空＝乾淨）
#
# 為什麼：`*_test.sh` 會把被測腳本複製出來跑，被測腳本裡有 `launchctl bootout`、`pkill`、`ssh`
# 這種一叫下去就會影響整台機器的指令。測試靠「在 PATH 前面放替身」來擋，但只要有一個替身沒放到，
# 就會 fallthrough 打到真 binary——2026-09-24 就是這樣真的把 gui/501 的兩個 herdr job bootout 掉，
# 連自己的 pane 都被收掉（issue #418）。
#
# 這支的作法是在**所有替身的更前面**再放一層 canary：真的被叫到就記一筆並以 rc=99 失敗，
# 既擋下破壞，也讓 check.sh 事後看得出是哪一支測試、哪一個指令漏擋了。
#
# `am-canary-probe` 是**驗證這層機制專用的無害指令名**：要證明 canary 擋得住，就叫它，
# 絕對不要拿真的 `launchctl`／`pkill` 當白老鼠——2026-09-24 就是那樣真的殺掉了 herdr 的
# default server（護欄沒生效時，白老鼠就變成真的破壞）。
#
# 涵蓋的是「測試永遠不該真的執行」的那幾個。`kill` 不在內（shell 內建，攔不到，而且測試
# 收自己 spawn 的行程本來就要用它）；要擋的是通用比對的 `pkill -f`／`killall` 那類。
set -euo pipefail

# `bootout` 不在內：它是 `launchctl` 的子命令、不是可執行檔，放進來只會讓清單看起來涵蓋得比實際多。
CANARY_CMDS="launchctl pkill killall ssh scp shutdown reboot am-canary-probe"

# `rm` 不能整支做成 stub：測試本來就要清自己的暫存目錄，擋掉就全部壞掉。
# 要擋的只有「刪到暫存目錄以外」——#422 明列的那一條，2026-09-23 的事故類型之一。
# 判斷只看**絕對路徑**的參數：`rm -rf ~/foo` 在 shell 展開後就是絕對路徑，正好是危險的那種；
# 相對路徑在測試裡是相對於自己的暫存 cwd，擋它只會製造假警報。
canary_rm_guard_body() {
    cat <<'RMGUARD'
# 把路徑正規化成「絕對＋去掉 symlink」再比：純字串前綴比對擋不住
# `cd /tmp && rm ../Users/…`（相對路徑），也擋不住 /tmp 底下指到暫存外的 symlink（i407 審核）。
_am_canary_real() {
    local p="$1" d b
    case "$p" in /*) ;; *) p="${PWD}/${p}" ;; esac
    d="${p%/*}"; b="${p##*/}"
    [ -n "$d" ] || d=/
    if [ -d "$d" ]; then
        d="$(cd -P "$d" 2>/dev/null && pwd -P)" || d="${p%/*}"
    fi
    printf '%s/%s' "${d%/}" "$b"
}

rm() {  # 依賴 _am_canary_real：兩個函式必須一起匯出，少一個就會把所有 rm 都擋掉
    local a r
    for a in "$@"; do
        case "$a" in
            -*) continue ;;
        esac
        # 相對路徑也要看：測試只要 cd 過，「cwd 一定在暫存目錄」這個前提就不成立。
        r="$(_am_canary_real "$a")"
        case "$r" in
            /tmp/*|/private/tmp/*|/var/folders/*|/private/var/folders/*) continue ;;
        esac
        case "${TMPDIR:+x}" in x) case "$r" in "${TMPDIR%/}"/*) continue ;; esac ;; esac
        case "${AM_CANARY_ALLOW_RM:+x}" in x) case "$r" in "${AM_CANARY_ALLOW_RM%/}"/*) continue ;; esac ;; esac
        echo "${AM_CANARY_TEST:-<未知測試>}: rm $*" >> "${AM_CANARY_DIR}/hits.log"
        echo "destructive-canary: rm 的目標在暫存目錄之外（${a} → ${r}），已擋下" >&2
        return 99
    done
    # **不要**用 `command -v rm` 找真 rm：函式叫 rm 時它回傳的就是 "rm"（函式名），
    # 於是 "$real" "$@" 又叫回這個函式，無限遞迴到 stack 爆掉（實測 SIGSEGV／rc=139）。
    # 直接指絕對路徑。
    if [ -x /bin/rm ]; then /bin/rm "$@"; else /usr/bin/rm "$@"; fi
}
RMGUARD
}

case "${1:-}" in
  arm)
    dir="${2:?用法：destructive-canary.sh arm <暫存目錄>}"
    rm -rf "${dir}"
    mkdir -p "${dir}"
    : > "${dir}/hits.log"
    for c in ${CANARY_CMDS}; do
        cat > "${dir}/${c}" <<STUB
#!/bin/bash
echo "\${AM_CANARY_TEST:-<未知測試>}: ${c} \$*" >> "${dir}/hits.log"
echo "destructive-canary: 測試裡有東西真的叫到 ${c}，已擋下（見 ${dir}/hits.log）" >&2
exit 99
STUB
        chmod 755 "${dir}/${c}"
    done
    # 第一層：PATH。便宜、對「腳本照常查 PATH」的情況有效。
    printf 'export AM_CANARY_DIR=%s\n' "${dir}"
    printf 'export PATH=%s:"$PATH"\n' "${dir}"
    # 第二層：**匯出的 bash 函式**。測試常常整個覆寫 PATH（`PATH="$ROOT/bin:/usr/bin:/bin"`，
    # 不是前綴），那一刻 canary 目錄就被丟掉、`/usr/bin:/bin` 還在，真 binary 又變回可達——
    # 這正是 #418 出事的那一類 fallthrough，光靠 PATH 擋不住。匯出的函式會跟著環境進到子 bash，
    # 而且函式優先於 PATH 查找，覆寫 PATH 蓋不掉它。
    # 擋不到的殘留情況：`env -i`（連環境一起清掉）與非 bash 的子行程（zsh／Bun.spawn）；
    # 那兩種由 `lint` 靜態掃出來，見下。
    # `export -f` 是 bash 專有的。在 zsh 底下 `export -f foo` 會變成「印出 foo 的定義」，
    # 函式根本沒被匯出，卻看起來好像成功了——2026-09-24 就是這樣讓驗證用的 victim 腳本
    # 跑到真的 pkill。所以整段用 BASH_VERSION 包起來，不是 bash 就乾脆不裝這一層。
    echo 'if [ -n "${BASH_VERSION:-}" ]; then'
    for c in ${CANARY_CMDS}; do
        printf '%s() { echo "${AM_CANARY_TEST:-<未知測試>}: %s $*" >> %s/hits.log; echo "destructive-canary: 測試裡有東西真的叫到 %s，已擋下" >&2; return 99; }\n' "${c}" "${c}" "${dir}" "${c}"
        printf 'export -f %s\n' "${c}"
    done
    canary_rm_guard_body "${dir}"
    echo 'export -f _am_canary_real'
    echo 'export -f rm'
    echo 'else'
    echo '  echo "destructive-canary: 這個 shell 不是 bash，bash 函式層裝不了（export -f 是 bash 專有）；PATH 與 zsh 層仍然有效" >&2'
    echo 'fi'
    # 第三層：**zsh 的 ZDOTDIR/.zshenv**。匯出的 bash 函式進不了 zsh（`zsh -c` 拿不到
    # `BASH_FUNC_*`），所以 zsh 子行程在覆寫 PATH 之後就完全沒有保護——實測是 rc=127，
    # 而那個 127 只是因為 sentinel 本來就不是真 binary；換成真的 `launchctl` 就會真的執行。
    # zsh 對**每一個** shell（含 `zsh -c`、含巢狀）都會讀 `$ZDOTDIR/.zshenv`，而且 ZDOTDIR
    # 自己是匯出的環境變數，所以一路傳得下去。這一層跟 PATH 無關，覆寫 PATH 蓋不掉。
    zdot="${dir}/zdotdir"
    mkdir -p "${zdot}"
    : > "${zdot}/.zshenv"
    for c in ${CANARY_CMDS}; do
        printf '%s() { echo "${AM_CANARY_TEST:-<未知測試>}: %s $*" >> %s/hits.log; echo "destructive-canary: 測試裡有東西真的叫到 %s，已擋下" >&2; return 99; }\n' \
            "${c}" "${c}" "${dir}" "${c}" >> "${zdot}/.zshenv"
    done
    printf '%s\n' "$(canary_rm_guard_body "${dir}")" >> "${zdot}/.zshenv"
    printf 'export ZDOTDIR=%s\n' "${zdot}"
    ;;
  hits)
    dir="${2:?用法：destructive-canary.sh hits <暫存目錄>}"
    [ -s "${dir}/hits.log" ] && cat "${dir}/hits.log"
    [ ! -s "${dir}/hits.log" ]
    ;;
  cmds) echo "${CANARY_CMDS}" ;;
  lint)
    # 靜態掃出 canary 到不了的地方：整個覆寫 PATH（沒有帶 ${PATH}、也沒有帶 AM_CANARY_DIR）、
    # 以及 `env -i`（連匯出的函式一起清掉）。這兩種 runtime 擋不住，只能在這裡點名。
    # 有些是刻意的（例如用 env -i 模擬 launchd 的最小環境），那就在上一行寫
    # `# canary-gap: <理由與怎麼另外擋住>`，這支就放行——重點是有人寫下理由，不是默默漏掉。
    #
    # 現況是 7 支既有測試共 33 處蓋不到（見 baseline 與 issue #422）。一次全改要動到好幾顆
    # agent 的測試檔，所以用**棘輪**：既有的記在 baseline，只要不再變多就放行，新增一處就紅。
    shift
    update=0
    if [ "${1:-}" = "--update" ]; then update=1; shift; fi
    BASELINE="scripts/ops/canary-baseline.tsv"
    # baseline 壞掉時**一定要紅**。壞掉的最常見形式就是 rebase 留下的：同一個檔名兩行
    # （兩邊各自 --update 過），或整段衝突標記沒清。兩種原本都只會讓比較用的 `[` 噴
    # 「integer expected」到 stderr，然後整支照樣 rc=0 並印一個亂算的總數——
    # 棘輪等於被靜靜關掉，而且沒有人看得出來（i407 審核實測：重複行 129 處、衝突標記 42 處，都 rc=0）。
    if [ ! -f "${BASELINE}" ]; then
        echo "canary lint: 找不到 ${BASELINE}" >&2
        exit 1
    fi
    bad="$(awk -F'\t' '
        /^#/ || /^[[:space:]]*$/ { next }
        NF != 2 || $1 !~ /^scripts\// || $2 !~ /^[0-9]+$/ { print "  格式不對：" $0; next }
        { if (seen[$1]++) print "  檔名重複：" $1 }
    ' "${BASELINE}")"
    if [ -n "${bad}" ]; then
        echo "canary lint: ${BASELINE} 壞了，棘輪不能信：" >&2
        printf '%s\n' "${bad}" >&2
        echo "（rebase 之後最常見：同一個檔名兩行，或衝突標記沒清乾淨。修好再跑一次 lint --update。）" >&2
        exit 1
    fi
    files="$*"
    if [ -z "${files}" ]; then
        files="$(ls scripts/*_test.sh scripts/ops/*_test.sh 2>/dev/null || true)"
    fi
    gaps=""
    for f in ${files}; do
        [ -f "${f}" ] || continue
        n=0
        prev_ok=0
        cont=0
        while IFS= read -r line; do
            n=$((n + 1))
            case "${line}" in
                *'canary-gap:'*) prev_ok=1; continue ;;
                # 註解**不重設** prev_ok：理由常常寫不只一行，而「canary-gap 只認正上方那一行」
                # 會逼人把理由擠成一行或乾脆不寫——這支工具要的正好相反（有人寫下理由）。
                # 連續的註解區塊只要有一行是 canary-gap，整塊到下一行程式碼為止都算已說明。
                '#'*|[[:space:]]*'#'*) continue ;;
            esac
            hit=""
            case "${line}" in
                *'env -i '*) hit="env -i 清掉環境，匯出的函式跟著沒了" ;;
                # 非 shell 的子行程（python 的 subprocess、Bun.spawn）走 execvp，
                # **不經過 shell 的指令查找**，所以第 2、3 層（bash 函式／zsh ZDOTDIR）對它們完全無效，
                # 只剩 PATH 那一層。那支 .py／.ts 自己再 spawn 時若 PATH 被換掉就沒有保護了（i407 審核實測）。
                # 這裡只認「測試直接叫某支 .py／.ts」，要求那一行帶著 canary 的 PATH 前綴。
                # 只認「直譯器＋腳本在同一行」這種看得出來是在執行的：光比副檔名會把
                # 「skip - 沒有 bun，跳過 dev-server-kick.ts 的測試」這種說明文字、
                # 以及 fixture 裡的假 pane 標題（`/opt/x/gcloud.py auth login`）全部誤判（實測）。
                # 用變數叫的（`"${BUN}" "${SCRIPT}"`）比不出來，那類靠 README 的規則約束。
                *python3' '*.py*|*python' '*.py*|*bun' '*.ts*|*node' '*.ts*|*node' '*.mjs*)
                    case "${line}" in
                        *AM_CANARY_DIR*) : ;;
                        *) hit="直接用直譯器跑 .py／.ts：execvp 不看 shell 函式，只剩 PATH 層，要帶上 AM_CANARY_DIR" ;;
                    esac
                    ;;
                # `OUTPUT=$(PATH="..." …)` 一樣是整個覆寫，只是前面不是空白。
                # 用「PATH= 前面不是識別字字元」比對，才不會漏掉命令替換裡的那種，
                # 也不會誤中 `HERDR_PATH=`／`FAKEPATH=`。
                # 只認「真的在指派」的位置：行首、空白、`$(`、`;`、`&` 之後。
                # 不能用「前面不是識別字字元」一概而論——`check "^PATH=${X}…"` 這種斷言字串
                # 也會中，那是假警報（daemon-swap_test.sh:322 就是在斷言啟動器推出來的 PATH）。
                PATH=*|*' PATH='*|*'(PATH='*|*';PATH='*|*'&PATH='*)
                    case "${line}" in
                        *'$PATH'*|*'${PATH'*|*AM_CANARY_DIR*) : ;;
                        *) hit="整個覆寫 PATH，canary 目錄被丟掉" ;;
                    esac
                    ;;
            esac
            # 反斜線續行的下一行仍屬同一個指令：`canary-gap:` 寫在指令上方時，
            # 續行不該因為「上一個物理行是程式碼」就失去說明（daemon-swap_test.sh 的
            # `PATH=… \` 換行再 `/usr/bin/python3 …` 就是這種）。
            case "${line}" in *\\) cont=1 ;; *) cont=0 ;; esac
            if [ -z "${hit}" ]; then
                [ "${cont}" = "1" ] || prev_ok=0
                continue
            fi
            if [ "${prev_ok}" = "1" ]; then
                [ "${cont}" = "1" ] || prev_ok=0
                continue
            fi
            gaps="${gaps}${f}:${n}: ${hit}
"
        done < "${f}"
    done

    now="$(printf '%s' "${gaps}" | grep -E '^scripts' | awk -F: '{print $1}' | sort | uniq -c | awk '{print $2"\t"$1}' || true)"

    if [ "${update}" = "1" ]; then
        {
            echo "# canary lint 的棘輪基準（issue #422）：檔名<TAB>還蓋不到的處數。"
            echo "# 只能變小；修掉之後跑 scripts/ops/destructive-canary.sh lint --update 更新。"
            printf '%s\n' "${now}"
        } > "${BASELINE}"
        echo "已更新 ${BASELINE}"
        exit 0
    fi

    worse=""
    better=""
    # 要比的是 baseline ∪ now：只看 now 的話，**修到 0 的檔會整個消失**，
    # 於是「變好了」永遠偵測不到，baseline 一直停在舊數字——棘輪就等於沒有收緊，
    # 之後退回去也不會紅（i266 2026-09-24：herdr-lan-check_test.sh 修到 0 時踩到）。
    all_files="$(
        { printf '%s\n' "${now}" | awk -F'\t' '/^scripts/ {print $1}'
          awk -F'\t' '/^scripts/ {print $1}' "${BASELINE}" 2>/dev/null || true
        } | sort -u
    )"
    for f in ${all_files}; do
        [ -n "${f}" ] || continue
        cnt="$(printf '%s\n' "${now}" | awk -F'\t' -v f="${f}" '$1==f {print $2}')"
        cnt="${cnt:-0}"
        was="$(awk -F'\t' -v f="${f}" '$1==f {print $2}' "${BASELINE}" 2>/dev/null || true)"
        was="${was:-0}"
        if [ "${cnt}" -gt "${was}" ]; then worse="${worse}  ${f}：baseline ${was} → 現在 ${cnt}
"; fi
        if [ "${cnt}" -lt "${was}" ]; then better="${better}  ${f}：baseline ${was} → 現在 ${cnt}
"; fi
    done

    if [ -n "${worse}" ]; then
        printf '%s' "${gaps}" >&2
        printf '\n比 baseline 多出來的：\n%s' "${worse}" >&2
        cat >&2 <<'HINT'

新增的地方 canary 蓋不到（PATH 被整個換掉，或 env -i 把匯出的函式也清了）。兩條路擇一：
  1. 把 canary 帶上：PATH="$你的fakebin:${AM_CANARY_DIR:+$AM_CANARY_DIR:}/usr/bin:/bin"
  2. 真的需要最小環境（例如模擬 launchd）就在上一行寫
     `# canary-gap: <理由，以及那段裡的破壞性指令是怎麼另外擋住的>`
HINT
        exit 1
    fi
    if [ -n "${better}" ]; then
        printf '有地方修好了，請跑 `scripts/ops/destructive-canary.sh lint --update` 把 baseline 縮小：\n%s' "${better}" >&2
        exit 1
    fi
    total="$(awk -F'\t' '/^scripts/ {s+=$2} END {print s+0}' "${BASELINE}" 2>/dev/null || echo 0)"
    echo "canary lint: 沒有新增蓋不到的地方（既有 ${total} 處見 ${BASELINE}）"
    ;;
  *) echo "用法：destructive-canary.sh [arm|hits|cmds|lint] <暫存目錄>" >&2; exit 2 ;;
esac
