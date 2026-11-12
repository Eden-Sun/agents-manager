#!/usr/bin/env bash
# 讀 stdin 的改動檔案清單（一行一個 repo 相對路徑），印出 `scripts/check.sh changed` 要跑的部分
# （web／daemon／ops／ob，一行一個、去重、照這個順序）。認不得的路徑一律印 `full`，寧可多跑。
# issue #716：小修改不必每次跑完整 CI；完整驗證交給 ubuntu 的背景 CI（scripts/ops/ubuntu-ci.sh）。
# 路徑規則（含跨界讀取的緣由）全在 scripts/ci-rules.tsv 的第 2 欄；ci-daemon-filters.sh、ci-ops-tests.py 讀同一份。
set -euo pipefail

rules="${CI_RULES_FILE:-$(dirname "${BASH_SOURCE[0]}")/ci-rules.tsv}"
pats=() partsv=()
while IFS='|' read -r pat parts _; do
    case "$pat" in '' | '#'*) continue ;; esac
    pats+=("$pat") partsv+=("$parts")
done <"$rules"

web=0 daemon=0 ops=0 ob=0 full=0
while IFS= read -r f; do
    [ -n "$f" ] || continue
    parts=FULL
    for i in "${!pats[@]}"; do
        # shellcheck disable=SC2053
        if [[ "$f" == ${pats[i]} ]]; then parts="${partsv[i]}"; break; fi
    done
    case ",$parts," in
        *,FULL,*) full=1 ;;
        *)
            case ",$parts," in *,web,*) web=1 ;; esac
            case ",$parts," in *,daemon,*) daemon=1 ;; esac
            case ",$parts," in *,ops,*) ops=1 ;; esac
            case ",$parts," in *,ob,*) ob=1 ;; esac
            ;;
    esac
done

if [ "$full" = 1 ]; then
    echo full
    exit 0
fi
[ "$web" = 1 ] && echo web
[ "$daemon" = 1 ] && echo daemon
[ "$ops" = 1 ] && echo ops
[ "$ob" = 1 ] && echo ob
exit 0
