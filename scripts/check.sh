#!/usr/bin/env bash
set -euo pipefail

export AGM_RUST_ENGINE=cargo

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

step() { printf '\n==> %s\n' "$*"; }

TEMPORARY_WEB_DIST_STUB=0
CANARY_DIR=""
cleanup_temporary_web_dist_stub() {
    if [ "$TEMPORARY_WEB_DIST_STUB" = 1 ]; then
        /bin/rm -f web/dist/index.html
        rmdir web/dist 2>/dev/null || true
    fi
}

DISK_LOW_KB=2097152
on_exit() {
    local rc=$? avail_kb
    cleanup_temporary_web_dist_stub
    if [ -n "${CANARY_DIR}" ]; then
        if [ -s "${CANARY_DIR}/hits.log" ]; then
            echo "!! 中斷前有測試真的叫到破壞性指令（已被擋下）：" >&2
            cat "${CANARY_DIR}/hits.log" >&2
        fi
        /bin/rm -rf "${CANARY_DIR}"
    fi
    if [ "$rc" != 0 ]; then
        avail_kb="$(df -Pk "$ROOT" 2>/dev/null | awk 'NR==2 {print $4}')"
        if [ -n "${avail_kb:-}" ] && [ "$avail_kb" -lt "$DISK_LOW_KB" ] 2>/dev/null; then
            printf '\n!! 失敗時磁碟只剩 %s MB（No space left on device）：這次紅很可能是磁碟滿了，不是程式碼；清出空間（例如不用的 worktree 的 target/）後重跑。\n' "$((avail_kb / 1024))" >&2
        fi
    fi
}
trap on_exit EXIT

load_gate() {
    [ -n "${CHECK_MAX_LOAD:-}" ] || return 0
    step "負載閘：1 分鐘負載 ≤ ${CHECK_MAX_LOAD} 才跑（最多等 ${CHECK_LOAD_TIMEOUT:-1800} 秒）"
    bash scripts/wait-load.sh --max "${CHECK_MAX_LOAD}" --timeout "${CHECK_LOAD_TIMEOUT:-1800}"
    unset CHECK_MAX_LOAD
}
if [ "${1:-default}" != changed ]; then load_gate; fi

web_install() {
    step "web: bun install --frozen-lockfile"
    (cd web && bun install --frozen-lockfile)
}
web_build() {
    step "web: vite build"
    (cd web && bunx vite build)
}
web_verify() {
    step "web: tsc -b（app 與 vite config，一次檢查）"
    (cd web && bunx tsc -b)
    step "web: oxlint src"
    (cd web && bunx oxlint src)
    step "web: bun test（scripts/web-test-shards.sh：N 個 shard 平行，CHECK_WEB_SHARDS=1 退回單程序）"
    bash scripts/web-test-shards.sh
}
check_web() { web_install; web_verify; web_build; }
check_web_bundle() { web_install; web_build; }
check_web_verify() { web_verify; }

run_daemon_tests() {
    local log rc=0 red
    log="$(mktemp "${TMPDIR:-/tmp}/am-check-daemon.XXXXXX")"
    env -u AM_MODEL -u AM_EFFORT -u AM_DATA_DIR cargo test -p agents-managerd --locked "$@" 2>&1 | tee "$log" || rc=$?
    if [ "$rc" = 0 ] && ! grep -Eq '^test result: ok\. [1-9][0-9]* passed;' "$log"; then
        echo "daemon: cargo test 沒有選到任何測試（檢查 CHECK_TESTS 或 scripts/ci-daemon-filters.sh 的過濾字串），拒絕放行" >&2
        rc=2
    fi
    if [ "$rc" != 0 ]; then
        red="$(grep -E '^test .* \.\.\. FAILED$' "$log" | awk '{print $2}' || true)"
        if [ -n "$red" ]; then
            printf '\n==> daemon: %s 條測試紅：\n' "$(printf '%s\n' "$red" | wc -l | tr -d ' ')" >&2
            printf '%s\n' "$red" | sed 's/^/    /' >&2
        fi
    fi
    /bin/rm -f "$log"
    return "$rc"
}

check_daemon() {
    if [ ! -f web/dist/index.html ]; then
        echo "web/dist/index.html 不存在：先跑 scripts/check.sh web（daemon 會把它嵌進去）" >&2
        exit 1
    fi
    step "daemon: ci-daemon-filters 的 fixture 測試"
    bash scripts/ci-daemon-filters_test.sh
    step "daemon: cargo test -p agents-managerd"
    run_daemon_tests
    step "workspace: cargo test -p xtask"
    env -u AM_MODEL -u AM_EFFORT -u AM_DATA_DIR cargo test -p xtask --locked
    local d
    for d in crates/*/; do
        d="${d%/}"
        step "crate: cargo test -p ${d##*/}"
        env -u AM_MODEL -u AM_EFFORT -u AM_DATA_DIR cargo test -p "${d##*/}" --locked
    done
}

mem_pressure_level() {
    if [ -n "${WAIT_MEM_FILE:-}" ]; then
        awk 'NR == 1 {print $2}' "${WAIT_MEM_FILE}" 2>/dev/null || true
    else
        sysctl -n kern.memorystatus_vm_pressure_level 2>/dev/null || true
    fi
}

run_with_pressure_watchdog() {
    local out="$1" tripped="$1.critical" cpid wpid rc=0
    shift
    set -m
    "$@" </dev/null >"$out" 2>&1 &
    cpid=$!
    (
        while :; do
            if [ "$(mem_pressure_level)" = 4 ]; then
                : >"$tripped"
                kill -KILL -- "-$cpid" 2>/dev/null || true
                exit 0
            fi
            sleep "${CHECK_MEM_WATCH_INTERVAL:-5}"
        done
    ) </dev/null >/dev/null 2>&1 &
    wpid=$!
    set +m
    trap 'kill -KILL -- "-$cpid" "-$wpid" 2>/dev/null; exit 130' INT TERM
    wait "$cpid" 2>/dev/null || rc=$?
    kill -KILL -- "-$wpid" 2>/dev/null || true
    wait "$wpid" 2>/dev/null || true
    trap - INT TERM
    if [ -e "$tripped" ]; then
        echo "記憶體壓力 critical，已中止 macos-local，避免整台機器卡死" >&2
        return 1
    fi
    return "$rc"
}

check_macos_local() {
    local local_cargo local_rustc local_rustdoc path_cargo tmp rc=0
    if [ "$(uname -s)" != Darwin ]; then
        echo "macos-local tests only run on macOS（這是 Linux／其他系統，什麼都沒驗）：請在 Mac 本機跑 scripts/check.sh macos-local" >&2
        return 2
    fi
    if ! command -v rustup >/dev/null 2>&1; then
        echo "macos-local tests need rustup to resolve the native macOS toolchain" >&2
        return 2
    fi

    local_cargo="$(rustup which cargo)"
    local_rustc="$(rustup which rustc)"
    local_rustdoc="$(rustup which rustdoc)"
    path_cargo="$(command -v cargo || true)"
    for tool in "$local_cargo" "$local_rustc" "$local_rustdoc"; do
        if [ ! -x "$tool" ]; then
            printf 'macos-local could not resolve an executable rustup tool: %s\n' "$tool" >&2
            return 2
        fi
    done
    if [ "$local_cargo" = "$path_cargo" ]; then
        echo "macos-local refused: rustup resolved the PATH cargo shim instead of native Cargo" >&2
        return 2
    fi
    step "macOS native Cargo: $local_cargo"

    step "記憶體閘：可用記憶體 ≥ ${CHECK_MIN_FREE_MEM:-30}% 且壓力不是 critical 才開編（最多等 ${CHECK_LOAD_TIMEOUT:-1800} 秒）"
    bash scripts/wait-load.sh --max 999999 --min-free-mem "${CHECK_MIN_FREE_MEM:-30}" --timeout "${CHECK_LOAD_TIMEOUT:-1800}" || return $?

    tmp="$(mktemp -d "${TMPDIR:-/tmp}/am-macos-local.XXXXXX")"
    macos_local_cohorts "$local_cargo" "$local_rustc" "$local_rustdoc" "$tmp/cargo.log" || rc=$?
    rm -rf "$tmp"
    return "$rc"
}

macos_local_cohorts() {
    local local_cargo="$1" local_rustc="$2" local_rustdoc="$3" log="$4" filter output

    local filters=("cli_update::tests" "cargo_shim::tests" "macos_local_")
    for filter in "${filters[@]}"; do
        step "macOS tests: $filter"
        if run_with_pressure_watchdog "$log" env -u AM_MODEL -u AM_EFFORT -u AM_DATA_DIR -u CARGO_BUILD_TARGET \
            RUSTC="$local_rustc" RUSTDOC="$local_rustdoc" \
            "$local_cargo" test --color never --locked -j 1 -p agents-managerd "$filter" -- --test-threads=1; then
            output="$(cat "$log")"
            printf '%s\n' "$output"
        else
            cat "$log" >&2
            return 1
        fi
        if ! grep -Eq '^test result: ok\. [1-9][0-9]* passed;' <<<"$output"; then
            printf 'macos-local required cohort %s selected zero tests\n' "$filter" >&2
            return 1
        fi
    done
}

check_ob() {
    step "repo: 沒有被追蹤的 Python bytecode"
    local tracked
    tracked="$(git ls-files -- '*.pyc' '*__pycache__*')"
    if [ -n "$tracked" ]; then
        printf '這些 bytecode 被 git 追蹤，請 git rm --cached：\n%s\n' "$tracked" >&2
        exit 1
    fi
    step "OB: queue/operator contracts"
    python3 -B scripts/ob_test.py
    step "OB: browser transport contracts"
    node --test scripts/ob_browser_test.mjs
    bash -n scripts/chatgpt-consult.sh
}

run_ops_test() {
    case "$1" in
        *.py) AM_CANARY_TEST="$1" python3 -B "$1" ;;
        *) AM_CANARY_TEST="$1" bash "$1" ;;
    esac
}
check_ops() {
    local t selected=__all__
    if [ -n "${1:-}" ] && [ -f scripts/ci-ops-tests.py ]; then
        selected="$(printf '%s\n' "$1" | python3 -B scripts/ci-ops-tests.py)" || selected=__all__
        [ -n "$selected" ] || selected=__all__
    fi
    step "ops: shell 變數後面緊接非 ASCII 字元"
    scripts/ops/lint-shell-vars.sh
    if command -v bun >/dev/null 2>&1; then
        for t in scripts/ops/*.ts; do
            step "ops: bun 載得進 $t"
            bun build --target=bun "$t" --outfile=/dev/null >/dev/null
        done
    else
        step "ops: 沒有 bun，跳過 scripts/ops/*.ts 的載入檢查"
    fi

    step "ops: canary lint（PATH 覆寫／env -i）"
    scripts/ops/destructive-canary.sh lint

    local canary
    canary="${TMPDIR:-/tmp}/am-canary-$$"
    CANARY_DIR="${canary}"
    eval "$(scripts/ops/destructive-canary.sh arm "${canary}")"
    if [ -z "${AM_CANARY_DIR:-}" ] || [ ! -x "${AM_CANARY_DIR}/am-canary-probe" ]; then
        echo "destructive-canary arm 失敗：護欄沒有裝起來，不能在沒有保護的情況下跑 ops 測試" >&2
        exit 1
    fi
    local tests=()
    for t in scripts/*_test.sh scripts/ops/*_test.sh scripts/agm_test.py; do
        [ -f "$t" ] || continue
        if [ "$selected" != __all__ ] && ! grep -qxF -- "$t" <<<"$selected"; then continue; fi
        tests+=("$t")
    done
    if [ "$selected" = __all__ ]; then
        echo "ops 測試：__all__（全部 ${#tests[@]} 支）"
    else
        echo "ops 測試：選中 ${#tests[@]} 支：${tests[*]:-}"
    fi
    local jobs="${CHECK_OPS_JOBS:-}"
    if [ -z "$jobs" ]; then
        jobs="$(bash scripts/ops/ops-jobs.sh)"
    fi
    if [ "$jobs" -le 1 ] || [ "${#tests[@]}" -le 1 ]; then
        for t in "${tests[@]}"; do
            step "ops: $t"
            run_ops_test "$t"
        done
    else
        local logdir
        logdir="$(mktemp -d "${TMPDIR:-/tmp}/am-ops-tests.XXXXXX")"
        local i=0 failed=() pids=()
        for t in "${tests[@]}"; do
            (run_ops_test "$t") > "$logdir/$i.log" 2>&1 &
            pids+=("$!")
            i=$((i + 1))
            while [ "$(jobs -rp | wc -l)" -ge "$jobs" ]; do sleep 0.2; done
        done
        i=0
        for t in "${tests[@]}"; do
            wait "${pids[$i]}" || failed+=("$t")
            step "ops: $t"
            cat "$logdir/$i.log"
            i=$((i + 1))
        done
        /bin/rm -rf "$logdir"
        printf '\n==> ops：%s 支測試、%s 個同時跑\n' "${#tests[@]}" "$jobs"
        if [ "${#failed[@]}" -gt 0 ]; then
            printf '!! 紅的 ops 測試：%s（往上找該支的輸出；要序列重跑：CHECK_OPS_JOBS=1 scripts/check.sh ops）\n' "${failed[*]}" >&2
            exit 1
        fi
    fi
    if ! scripts/ops/destructive-canary.sh hits "${canary}"; then
        echo "上面這些測試真的叫到了破壞性指令（已被擋下，但那條路要修）：見 scripts/ops/destructive-canary.sh" >&2
        /bin/rm -rf "${canary}"
        CANARY_DIR=""
        exit 1
    fi
    /bin/rm -rf "${canary}"
    CANARY_DIR=""
    echo "canary 乾淨：沒有任何測試叫到 $(scripts/ops/destructive-canary.sh cmds)"
}

check_fmt() {
    step "workspace: cargo fmt --check"
    cargo fmt --all -- --check
}

check_clippy() {
    step "workspace: cargo clippy（lib／bin）"
    cargo clippy --workspace --exclude agents-manager-desktop --exclude agents-managerd --lib --bins --locked
    step "daemon: cargo clippy --all-targets"
    cargo clippy -p agents-managerd --all-targets --locked -- -D warnings
}

macos_local_hint() {
    local f hit=""
    while IFS= read -r f; do
        case "$f" in
            crates/am-base/src/cargo_shim.rs | crates/am-base/src/cargo_shim.*.rs | daemon/src/runners/am_base_tests/cargo_shim.rs | daemon/src/cli_update.rs) hit="${hit} ${f}" ;;
            daemon/src/*.rs | daemon/src/*/*.rs | crates/*.rs) if [ -f "$f" ] && grep -q 'fn macos_local_' "$f" 2>/dev/null; then hit="${hit} ${f}"; fi ;;
        esac
    done <<EOF
$1
EOF
    if [ -n "$hit" ]; then
        printf '!! 改到平台敏感的模組（%s ）：請在 Mac 本機再跑 scripts/check.sh macos-local（Linux 上跑不了，這裡沒有驗）\n' "${hit# }" >&2
    fi
}

check_changed() {
    local base="${1:-origin/main}" committed dirty untracked files parts filters="" nfilters=0 f
    if ! committed="$(GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=diff.renames GIT_CONFIG_VALUE_0=false git diff --name-only "${base}...HEAD")"; then
        echo "changed: 無法比較 ${base}...HEAD（${base} 不存在、沒 fetch 或沒有共同祖先？）：看不出改了什麼，拒絕放行" >&2
        unset CHECK_MAX_LOAD
        return 2
    fi
    if ! dirty="$(GIT_CONFIG_COUNT=1 GIT_CONFIG_KEY_0=diff.renames GIT_CONFIG_VALUE_0=false git diff --name-only HEAD)"; then
        echo "changed: 無法列出工作樹的改動（git diff --name-only HEAD 失敗）：拒絕放行" >&2
        unset CHECK_MAX_LOAD
        return 2
    fi
    if ! untracked="$(git ls-files --others --exclude-standard)"; then
        echo "changed: 無法列出未追蹤的檔案（git ls-files 失敗）：拒絕放行" >&2
        unset CHECK_MAX_LOAD
        return 2
    fi
    files="$(printf '%s\n%s\n%s\n' "$committed" "$dirty" "$untracked" | sort -u)"
    parts="$(printf '%s\n' "$files" | bash scripts/ci-changed-parts.sh)"
    if [ -z "$parts" ]; then
        echo "changed: 只有文件類改動，不用跑"
        unset CHECK_MAX_LOAD
        return 0
    fi
    echo "changed（base ${base}）：$(echo "$parts" | tr '\n' ' ')"
    load_gate
    macos_local_hint "$files"
    if [ -n "${CHECK_TESTS:-}" ] && [ "${CHECK_TESTS}" != none ] && ! grep -qx -e daemon -e full <<<"$parts"; then
        echo "!! CHECK_TESTS=${CHECK_TESTS} 沒有用到：這次改動沒有 daemon 部分，不會跑任何 daemon 測試" >&2
    fi
    if grep -qx full <<<"$parts"; then
        check_ob; check_ops; check_web; check_daemon
        return
    fi
    if grep -qx ob <<<"$parts"; then check_ob; fi
    if grep -qx ops <<<"$parts"; then check_ops "$files"; fi
    if grep -qx web <<<"$parts"; then check_web; fi
    if grep -qx daemon <<<"$parts"; then
        local release_triage_submission_test=0 daemon_tests_will_run=0 am_base_crate_input=0
        filters="$(printf '%s\n' "$files" | bash scripts/ci-daemon-filters.sh)"
        echo "daemon 測試 filter：$(tr '\n' ' ' <<<"$filters")"
        if grep -qx '__am_base_crate__' <<<"$filters"; then
            am_base_crate_input=1
            filters="$(printf '%s\n' "$filters" | grep -vx '__am_base_crate__' || true)"
        fi
        if grep -Eq '^(__am_base|__daemon)_release_triage_submission__$' <<<"$filters"; then
            release_triage_submission_test=1
            filters="$(printf '%s\n' "$filters" | grep -Ev '^(__am_base|__daemon)_release_triage_submission__$' || true)"
        fi
        if [ -n "${CHECK_TESTS:-}" ]; then
            [ "${CHECK_TESTS}" = none ] || daemon_tests_will_run=1
        elif grep -qx '__all__' <<<"$filters" || [ -n "$filters" ]; then
            daemon_tests_will_run=1
        fi
        if [ "${CHECK_TESTS:-}" != none ] && [ -f scripts/rust_test_mount_test.sh ] && ! grep -qx ops <<<"$parts"; then
            step "daemon: Rust 測試檔掛載（scripts/rust_test_mount_test.sh）"
            bash scripts/rust_test_mount_test.sh
        fi
        if [ ! -f web/dist/index.html ]; then
            step "daemon: temporary web/dist stub (skip web build on the Rust fast path)"
            mkdir -p web/dist
            printf '<!doctype html>\n' > web/dist/index.html
            TEMPORARY_WEB_DIST_STUB=1
        fi
        if [ "$daemon_tests_will_run" = 1 ]; then
            echo "daemon: 略過 cargo check（下面的 cargo test 會編同一批 target）"
        else
            step "daemon: cargo check --all-targets"
            env -u AM_MODEL -u AM_EFFORT -u AM_DATA_DIR cargo check -p agents-managerd --all-targets --locked
        fi
        if [ "${CHECK_TESTS:-}" != none ]; then
            workspace_test_inputs=0
            while IFS= read -r f; do
                case "$f" in
                    Cargo.toml | Cargo.lock | rust-toolchain* | .cargo/*) workspace_test_inputs=1 ;;
                esac
            done <<< "$files"
            if [ "$workspace_test_inputs" = 1 ]; then
                for d in crates/*/; do
                    [ -f "${d}Cargo.toml" ] || continue
                    f="${d%/}"
                    f="${f##*/}"
                    step "crate: cargo test -p ${f}（workspace-wide input）"
                    env -u AM_MODEL -u AM_EFFORT -u AM_DATA_DIR cargo test -p "${f}" --locked
                done
            else
                has_am_base=0
                for f in $( { printf '%s\n' "$files" | sed -n 's#^crates/\([^/]*\)/.*#\1#p'; [ "$am_base_crate_input" = 0 ] || echo am-base; } | sort -u); do
                    if [ "$f" = "am-base" ]; then
                        has_am_base=1
                    fi
                    step "crate: cargo test -p ${f}"
                    env -u AM_MODEL -u AM_EFFORT -u AM_DATA_DIR cargo test -p "${f}" --locked
                done
                if [ "$has_am_base" = 0 ]; then
                    if printf '%s\n' "$files" | grep -E '^crates/[^/]+/src/.+\.rs$' >/dev/null; then
                        step "crate: cargo test -p am-base --locked -- scratch_guard::"
                        log="$(mktemp "${TMPDIR:-/tmp}/am-check-crate.XXXXXX")"
                        rc=0
                        env -u AM_MODEL -u AM_EFFORT -u AM_DATA_DIR cargo test -p am-base --locked -- scratch_guard:: 2>&1 | tee "$log" || rc=$?
                        if [ "$rc" = 0 ] && ! grep -Eq '^test result: ok\. [1-9][0-9]* passed;' "$log"; then
                            echo "crate: cargo test -p am-base 沒有選到任何測試（檢查 scratch_guard:: 過濾字串），拒絕放行" >&2
                            rc=2
                        fi
                        /bin/rm -f "$log"
                        if [ "$rc" != 0 ]; then
                            return "$rc"
                        fi
                    fi
                fi
            fi
        fi
        if [ "$release_triage_submission_test" = 1 ]; then
            if [ "${CHECK_TESTS:-}" = none ]; then
                echo "daemon: CHECK_TESTS=none：略過發行契約測試（the_task_template_teaches_a_submission_the_daemon_accepts）"
            else
                step "daemon: cargo test -p agents-managerd -- runners::am_base_tests::release_triage::tests::the_task_template_teaches_a_submission_the_daemon_accepts"
                run_daemon_tests -- runners::am_base_tests::release_triage::tests::the_task_template_teaches_a_submission_the_daemon_accepts
            fi
        fi
        if [ -n "${CHECK_TESTS:-}" ]; then
            if [ "${CHECK_TESTS}" = none ]; then
                echo "daemon: CHECK_TESTS=none：不跑測試"
            else
                step "daemon: cargo test ${CHECK_TESTS}"
                run_daemon_tests "${CHECK_TESTS}"
            fi
        else
            if grep -qx '__all__' <<<"$filters"; then
                step "daemon: cargo test（build／crate-wide test input）"
                run_daemon_tests
            else
                for f in $filters; do nfilters=$((nfilters + 1)); done
            fi
            if [ "$nfilters" -gt 0 ]; then
                step "daemon: cargo test（改到的模組：$(echo $filters)）"
                # shellcheck disable=SC2086  # 過濾字串是 [a-z_:]，不含空白與萬用字元
                run_daemon_tests -- $filters || return $?
            elif ! grep -qx '__all__' <<<"$filters" && [ "$release_triage_submission_test" = 0 ] && [ "$am_base_crate_input" = 0 ]; then
                echo "daemon: 沒有可挑的測試子集（只動了 main.rs 之類不屬於任何模組的檔案）；要跑測試用 CHECK_TESTS=<過濾字串>"
            fi
        fi
    fi
}

case "${1:-default}" in
    changed) check_changed "${2:-origin/main}" ;;
    web) check_web ;;
    web-bundle) check_web_bundle ;;
    web-verify) check_web_verify ;;
    daemon) check_daemon ;;
    macos-local) check_macos_local ;;
    ob) check_ob ;;
    ops)
        if [ -n "${2:-}" ] && [ -n "${3:-}" ]; then
            if ops_files="$(
                git -c core.quotePath=false diff --no-renames --name-only "$2" "$3" &&
                    if [ -n "${4:-}" ]; then git -c core.quotePath=false diff --no-renames --name-only "$2" "$4"; fi
            )"; then
                check_ops "$ops_files"
            else
                echo "ops: 無法比較基準，改跑完整測試" >&2
                check_ops
            fi
        else
            check_ops
        fi
        ;;
    fmt) check_fmt ;;
    clippy) check_clippy ;;
    default) check_ob; check_ops; check_web; check_daemon ;;
    all)
        check_ob
        check_ops
        check_web
        check_daemon
        check_fmt || echo "!! fmt 不乾淨（不擋）"
        check_clippy || echo "!! clippy 不乾淨（不擋）"
        ;;
    *) echo "用法：scripts/check.sh [changed [base]|web|daemon|macos-local|ob|ops|fmt|clippy|all]" >&2; exit 2 ;;
esac
