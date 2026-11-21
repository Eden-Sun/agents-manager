#!/usr/bin/env bash
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
CANON="agents-manager.sqlite3"
OTHER_DBS=(
    ob.sqlite3
)

check_source() {
    local root=$1 bad=0
    if ! grep -qF "prepared.dir.join(\"${CANON}\")" "${root}/daemon/src/startup.rs" 2>/dev/null; then
        echo "FAIL - daemon/src/startup.rs 找不到 prepared.dir.join(\"${CANON}\")：daemon 的 DB 檔名改了，底下每支腳本都要跟著改"
        bad=1
    fi
    if ! grep -qF "dir.join(\"${CANON}\")" "${root}/crates/am-base/src/release_triage/mod.rs" 2>/dev/null; then
        echo "FAIL - crates/am-base/src/release_triage/mod.rs 找不到 dir.join(\"${CANON}\")：daemon 的 DB 檔名改了，底下每支腳本都要跟著改"
        bad=1
    fi
    return "${bad}"
}

is_other_db() {
    local o
    for o in "${OTHER_DBS[@]}"; do
        [ "$1" = "${o}" ] && return 0
    done
    return 1
}

check_scripts() {
    local root=$1 bad=0 f hit lineno line stripped m
    while IFS= read -r f; do
        case "${f}" in *_test.sh|*_test.py) continue ;; esac
        [ -f "${root}/${f}" ] || continue
        while IFS= read -r hit; do
            lineno=${hit%%:*}
            line=${hit#*:}
            stripped="${line#"${line%%[![:space:]]*}"}"
            case "${stripped}" in '#'*) continue ;; esac
            while IFS= read -r m; do
                case "${m}" in "${CANON}"*) continue ;; esac
                is_other_db "${m}" && continue
                echo "FAIL - ${f}:${lineno}: ${m}（不是 ${CANON}；daemon 的 DB 檔名只有一個）"
                bad=1
            done < <(printf '%s\n' "${line}" | grep -oE '[A-Za-z0-9_.-]+\.sqlite3?' || true)
        done < <(grep -nE '[A-Za-z0-9_.-]+\.sqlite3?' "${root}/${f}" || true)
    done < <(git -C "${root}" ls-files 'scripts/*.sh' 'scripts/ops/*.sh' 'scripts/ops/*.py' 'scripts/ops/*.ts' 'scripts/*.py')
    return "${bad}"
}

check_fixtures() {
    local root=$1 bad=0 hit
    while IFS= read -r hit; do
        [ -n "${hit}" ] || continue
        local body=${hit#*:*:}
        local stripped="${body#"${body%%[![:space:]]*}"}"
        case "${stripped}" in '#'*) continue ;; esac
        echo "FAIL - ${hit}（測試用的假檔名 db.sqlite3 不該出現在非測試的腳本）"
        bad=1
    done < <(git -C "${root}" grep -n 'db\.sqlite3' -- scripts ':!scripts/*_test.sh' ':!scripts/ops/*_test.sh' ':!scripts/ops/*_test.py' || true)
    return "${bad}"
}

check_tree() {
    local root=$1 bad=0
    check_source "${root}" || bad=1
    check_scripts "${root}" || bad=1
    check_fixtures "${root}" || bad=1
    return "${bad}"
}

selfcheck() {
    local tmp fails=0 out rc
    tmp=$(mktemp -d)
    SELF_TMP=${tmp}
    trap 'rm -rf "${SELF_TMP:-}"' EXIT
    mkdir -p "${tmp}/daemon/src" "${tmp}/crates/am-base/src/release_triage" "${tmp}/scripts/ops"
    printf 'let db = prepared.dir.join("agents-manager.sqlite3");\n' > "${tmp}/daemon/src/startup.rs"
    printf 'Ok(dir.join("agents-manager.sqlite3"))\n' > "${tmp}/crates/am-base/src/release_triage/mod.rs"
    git -C "${tmp}" init -q
    printf '#!/bin/bash\n# 註解裡提 db.sqlite3 不算\nDB="$D/db.sqlite3"\n' > "${tmp}/scripts/ops/x.sh"
    git -C "${tmp}" add -A
    out=$(check_tree "${tmp}" 2>&1) && rc=0 || rc=$?
    if [ "${rc}" = 1 ] && grep -qF 'scripts/ops/x.sh:3: db.sqlite3' <<<"${out}"; then
        echo "ok   - 自我檢查：寫 db.sqlite3 的腳本會 FAIL 並點名那一行"
    else
        echo "FAIL - 自我檢查：寫 db.sqlite3 的腳本沒有被抓到（rc=${rc}）"; echo "${out}" | sed 's/^/      /'; fails=$((fails + 1))
    fi
    printf '#!/bin/bash\nDB="$D/agents-manager.sqlite3-wal"\nDB2="$D/agents-manager.sqlite3.bak-20261011"\n' > "${tmp}/scripts/ops/x.sh"
    git -C "${tmp}" add -A
    out=$(check_tree "${tmp}" 2>&1) && rc=0 || rc=$?
    if [ "${rc}" = 0 ]; then
        echo "ok   - 自我檢查：正式檔名加後綴（-wal、.bak-…）放行"
    else
        echo "FAIL - 自我檢查：正式檔名加後綴被誤判（rc=${rc}）"; echo "${out}" | sed 's/^/      /'; fails=$((fails + 1))
    fi
    printf 'let db = prepared.dir.join("ledger.sqlite3");\n' > "${tmp}/daemon/src/startup.rs"
    git -C "${tmp}" add -A
    out=$(check_tree "${tmp}" 2>&1) && rc=0 || rc=$?
    if [ "${rc}" = 1 ] && grep -qF 'daemon/src/startup.rs 找不到' <<<"${out}"; then
        echo "ok   - 自我檢查：daemon 的檔名改了，檢查 1 會 FAIL"
    else
        echo "FAIL - 自我檢查：daemon 的檔名改了沒有被抓到（rc=${rc}）"; echo "${out}" | sed 's/^/      /'; fails=$((fails + 1))
    fi
    printf 'let db = prepared.dir.join("agents-manager.sqlite3");\n' > "${tmp}/daemon/src/startup.rs"
    printf '#!/bin/bash\nDB="$D/db.sqlite3"\n' > "${tmp}/scripts/ops/x_test.sh"
    printf '#!/bin/bash\nDB="$D/db.sqlite3"\n' > "${tmp}/scripts/ops/x.sh"
    rm -f "${tmp}/scripts/ops/x.sh"
    git -C "${tmp}" add -A
    out=$(check_tree "${tmp}" 2>&1) && rc=0 || rc=$?
    if [ "${rc}" = 0 ]; then
        echo "ok   - 自我檢查：測試檔裡的 db.sqlite3 放行"
    else
        echo "FAIL - 自我檢查：測試檔的 db.sqlite3 被誤判（rc=${rc}）"; echo "${out}" | sed 's/^/      /'; fails=$((fails + 1))
    fi
    printf '#!/bin/bash\nDB="$D/db.sqlite3"\n' > "${tmp}/scripts/ops/y.sh"
    git -C "${tmp}" add -A
    out=$(check_tree "${tmp}" 2>&1) && rc=0 || rc=$?
    if [ "${rc}" = 1 ] && grep -qF 'scripts/ops/y.sh:2:' <<<"${out}"; then
        echo "ok   - 自我檢查：非測試腳本的 db.sqlite3 會 FAIL"
    else
        echo "FAIL - 自我檢查：非測試腳本的 db.sqlite3 沒被抓到（rc=${rc}）"; echo "${out}" | sed 's/^/      /'; fails=$((fails + 1))
    fi
    return "${fails}"
}

main() {
    local root=${DB_FILENAME_ROOT:-${REPO}} rc=0 out
    out=$(check_tree "${root}" 2>&1) || rc=1
    if [ "${rc}" = 0 ]; then
        echo "ok   - daemon 的 DB 檔名（${CANON}）在來源與所有腳本一致，沒有測試假檔名外流"
    else
        printf '%s\n' "${out}"
    fi
    local self_fails=0
    selfcheck || self_fails=$?
    if [ "${rc}" != 0 ] || [ "${self_fails}" != 0 ]; then
        echo "db_filename_test: FAILED"
        exit 1
    fi
    echo "db_filename_test: all ok"
}

main "$@"
