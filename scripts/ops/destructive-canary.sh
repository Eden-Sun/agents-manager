#!/usr/bin/env bash
set -euo pipefail

CANARY_CMDS="launchctl systemctl systemd-run pkill killall ssh scp shutdown reboot am-canary-probe"

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
    printf 'export AM_CANARY_DIR=%s\n' "${dir}"
    printf 'export PATH=%s:"$PATH"\n' "${dir}"
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
    shift
    update=0
    if [ "${1:-}" = "--update" ]; then update=1; shift; fi
    BASELINE="scripts/ops/canary-baseline.tsv"
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
                '#'*|[[:space:]]*'#'*) continue ;;
            esac
            hit=""
            case "${line}" in
                *'env -i '*) hit="env -i 清掉環境，匯出的函式跟著沒了" ;;
                *python3' '*.py*|*python' '*.py*|*bun' '*.ts*|*node' '*.ts*|*node' '*.mjs*)
                    case "${line}" in
                        *AM_CANARY_DIR*) : ;;
                        *) hit="直接用直譯器跑 .py／.ts：execvp 不看 shell 函式，只剩 PATH 層，要帶上 AM_CANARY_DIR" ;;
                    esac
                    ;;
                PATH=*|*' PATH='*|*'(PATH='*|*';PATH='*|*'&PATH='*)
                    case "${line}" in
                        *'$PATH'*|*'${PATH'*|*AM_CANARY_DIR*) : ;;
                        *) hit="整個覆寫 PATH，canary 目錄被丟掉" ;;
                    esac
                    ;;
            esac
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
