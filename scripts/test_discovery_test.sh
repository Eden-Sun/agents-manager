#!/usr/bin/env bash
# 每一支測試檔都要有人會跑它：check.sh ops 只撈 scripts/*_test.sh 與 scripts/ops/*_test.sh，另外直接點名 agm_test.py／ob_test.py／ob_browser_test.mjs；
# 其他測試檔（例如 .py）必須被某支會跑的腳本點名。放錯位置或命名不合（`foo-test.sh` 少了底線、放在子目錄）的測試檔
# 會變成「看起來有測試、其實從來沒跑過」——這支把它擋下來。
#
# 例外只有兩支需要真的在跑的 daemon 才能執行的手動端到端腳本，在下面明列。
set -euo pipefail
cd "$(dirname "$0")/.."

# 需要運作中的 daemon／sshd 的手動端到端腳本，不進 check.sh。
MANUAL="scripts/hook-timing-test.sh scripts/remote-loop-test.sh"

fail=0
runs_directly() {
    case "$1" in
        scripts/agm_test.py | scripts/ob_test.py | scripts/ob_browser_test.mjs) return 0 ;;
        scripts/*_test.sh) [ "$(dirname "$1")" = scripts ] && return 0 ;;
    esac
    case "$1" in scripts/ops/*_test.sh) [ "$(dirname "$1")" = scripts/ops ] && return 0 ;; esac
    return 1
}

while IFS= read -r t; do
    case " $MANUAL " in *" $t "*) continue ;; esac
    runs_directly "$t" && continue
    base="$(basename "$t")"
    # 被另一支「會跑的腳本」點名（wrapper 呼叫 .py 之類）。
    if grep -rlF --include='*.sh' --include='*.py' --include='*.mjs' --include='*.ts' -- "$base" scripts | grep -vFx -- "$t" | grep -q .; then
        continue
    fi
    printf 'FAIL: %s 沒有人會跑它（不在 scripts/*_test.sh、scripts/ops/*_test.sh，也沒有被任何腳本點名）\n' "$t" >&2
    fail=1
done < <(find scripts -type f \( -name '*_test.*' -o -name '*-test.*' -o -name 'test_*' \) ! -name '*.md' ! -path '*/fixtures/*' ! -path '*/__pycache__/*' | sort)

if [ "$fail" = 0 ]; then echo "test discovery: OK"; fi
exit "$fail"
