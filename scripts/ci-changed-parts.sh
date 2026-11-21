#!/usr/bin/env bash
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
