#!/usr/bin/env bash
# daemon 的帳本檔名（agents-manager.sqlite3）寫死在好幾處：daemon 正式開 DB（startup.rs）、release-triage CLI 自己開帳本
# （release_triage::default_db_path），腳本裡 daemon-swap／herdr-upgrade／cutover-to-host／hook-timing-test 各寫一次。
# 只有 outbox-gc 有守門（#1391）；這支把其餘的釘在一起（#1457）：正式檔名的來源、腳本裡每一個 sqlite 檔名、測試用的假檔名不外流。
# 純文字比對（git 追蹤的檔），不編譯、不需要 sqlite3。真樹檢查與假樹自我檢查都在這支裡：check.sh ops 的 scripts/*_test.sh 迴圈會撈到它。
#
#   scripts/db_filename_test.sh          # 檢查 repo（或 DB_FILENAME_ROOT 指的樹）＋自我檢查
set -euo pipefail

REPO="$(cd "$(dirname "$0")/.." && pwd)"
CANON="agents-manager.sqlite3"
# 不是 daemon 的帳本、但腳本裡也有 sqlite 檔名的：各自有自己的守門或本來就不是 daemon 的 DB。新增要在這裡寫原因。
#   ob.sqlite3：OB 外腦的帳本（scripts/ob_store.py，ob_store 自己開，跟 daemon 無關）。
OTHER_DBS=(
    ob.sqlite3
)

# 檢查 1：正式檔名的來源。daemon 的 startup.rs 與 release_triage 都要用同一個字面值開帳本。
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

# 檢查 2：腳本裡每一個 sqlite 檔名都以 agents-manager.sqlite3 開頭（允許 -wal、-shm、.pre-transfer-…、.bak-… 後綴）。
# 註解行不算（`outbox-gc.sh` 的註解刻意提到 db.sqlite3）。
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

# 檢查 3：測試用的假檔名不外流到非測試的腳本（db.sqlite3 只准出現在測試檔或註解裡）。
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

# 整棵樹跑三項檢查；有任何一項紅就回 1。
check_tree() {
    local root=$1 bad=0
    check_source "${root}" || bad=1
    check_scripts "${root}" || bad=1
    check_fixtures "${root}" || bad=1
    return "${bad}"
}

# 假樹自我檢查：守門本身要會紅、會放行。
selfcheck() {
    local tmp fails=0 out rc
    tmp=$(mktemp -d)
    SELF_TMP=${tmp}
    trap 'rm -rf "${SELF_TMP:-}"' EXIT
    mkdir -p "${tmp}/daemon/src" "${tmp}/crates/am-base/src/release_triage" "${tmp}/scripts/ops"
    printf 'let db = prepared.dir.join("agents-manager.sqlite3");\n' > "${tmp}/daemon/src/startup.rs"
    printf 'Ok(dir.join("agents-manager.sqlite3"))\n' > "${tmp}/crates/am-base/src/release_triage/mod.rs"
    git -C "${tmp}" init -q
    # 假樹一：腳本寫 db.sqlite3 → 檢查 2 必須 FAIL 並點名那一行。
    printf '#!/bin/bash\n# 註解裡提 db.sqlite3 不算\nDB="$D/db.sqlite3"\n' > "${tmp}/scripts/ops/x.sh"
    git -C "${tmp}" add -A
    out=$(check_tree "${tmp}" 2>&1) && rc=0 || rc=$?
    if [ "${rc}" = 1 ] && grep -qF 'scripts/ops/x.sh:3: db.sqlite3' <<<"${out}"; then
        echo "ok   - 自我檢查：寫 db.sqlite3 的腳本會 FAIL 並點名那一行"
    else
        echo "FAIL - 自我檢查：寫 db.sqlite3 的腳本沒有被抓到（rc=${rc}）"; echo "${out}" | sed 's/^/      /'; fails=$((fails + 1))
    fi
    # 假樹二：改成 -wal 後綴 → 放行（只有後綴，檔名仍是正式的）。
    printf '#!/bin/bash\nDB="$D/agents-manager.sqlite3-wal"\nDB2="$D/agents-manager.sqlite3.bak-20261011"\n' > "${tmp}/scripts/ops/x.sh"
    git -C "${tmp}" add -A
    out=$(check_tree "${tmp}" 2>&1) && rc=0 || rc=$?
    if [ "${rc}" = 0 ]; then
        echo "ok   - 自我檢查：正式檔名加後綴（-wal、.bak-…）放行"
    else
        echo "FAIL - 自我檢查：正式檔名加後綴被誤判（rc=${rc}）"; echo "${out}" | sed 's/^/      /'; fails=$((fails + 1))
    fi
    # 假樹三：startup.rs 沒有那個字串 → 檢查 1 FAIL。
    printf 'let db = prepared.dir.join("ledger.sqlite3");\n' > "${tmp}/daemon/src/startup.rs"
    git -C "${tmp}" add -A
    out=$(check_tree "${tmp}" 2>&1) && rc=0 || rc=$?
    if [ "${rc}" = 1 ] && grep -qF 'daemon/src/startup.rs 找不到' <<<"${out}"; then
        echo "ok   - 自我檢查：daemon 的檔名改了，檢查 1 會 FAIL"
    else
        echo "FAIL - 自我檢查：daemon 的檔名改了沒有被抓到（rc=${rc}）"; echo "${out}" | sed 's/^/      /'; fails=$((fails + 1))
    fi
    # 假樹四：測試檔裡的 db.sqlite3 不算外流。
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
    # 假樹五：非測試腳本裡的 db.sqlite3 → 檢查 3 FAIL（只有非註解的）。
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
