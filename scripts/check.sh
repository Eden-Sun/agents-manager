#!/usr/bin/env bash
# 收尾前一鍵跑跟 CI（.github/workflows/ci.yml）同一組檢查。
#
#   scripts/check.sh            # OB + ops + web + daemon
#   scripts/check.sh web        # bun install --frozen-lockfile、tsc、oxlint、bun test、vite build
#   scripts/check.sh daemon     # cargo test -p agents-managerd（要先有 web/dist）
#   scripts/check.sh ops        # shell 變數寫法的 lint，加上 scripts/*_test.sh 與 scripts/ops/*_test.sh（shell 腳本的隔離測試，假 agm／假 gh／假 sccache，不碰正式環境）
#   scripts/check.sh ob         # 沒有被追蹤的 bytecode；OB queue/operator 與瀏覽器契約（隔離，不用登入）
#   scripts/check.sh macos-local # shell／行程／signal 的原生 macOS 測試（不走 cargo shim）；開編前等可用記憶體 ≥ CHECK_MIN_FREE_MEM%（預設 30），編譯中壓力 critical 就中止（issue #1783）
#   scripts/check.sh fmt        # cargo fmt --check（只報告，現況不乾淨）
#   scripts/check.sh clippy     # cargo clippy（只報告，現況不乾淨）
#   scripts/check.sh all        # 以上全部
#   CHECK_MAX_LOAD=40 scripts/check.sh changed # 有檢查要跑時才先等 1 分鐘負載 ≤ 40（逾時 CHECK_LOAD_TIMEOUT 秒、預設 1800，失敗不照跑；issue #813）
#   scripts/check.sh changed [base] # 只跑改到的部分（跟 base，預設 origin/main 比），daemon 有測試時由 cargo test 編 targets，否則 cargo check（CHECK_TESTS=none 關掉測試）；issue #716
#
# 注意：
# - web 用 `tsc -b` 一次檢查 references 裡的 app 與 vite config，再單獨 vite build；
#   不帶 -b 的根目錄 `tsc --noEmit` 沒有 files，什麼都不檢查（假綠燈）。
# - web 的單元測試用 `bun test`（不是 `node --test`）：測試檔是 `node:test` 寫的，bun 直接吃，
#   而且會解析 `.tsx` 與沒副檔名的 import；node 的 --experimental-strip-types 對那兩種都會
#   ERR_MODULE_NOT_FOUND。
# - daemon 用 rust-embed 把 web/dist 編進二進位；`changed` 的 Rust fast path 用臨時 stub，完整 web bundle 由 web/full CI 驗。
# - daemon 的測試會讀 AM_MODEL / AM_EFFORT（herdr shim 的沿用邏輯），在 bot 的 pane
#   裡跑時這兩個有值會讓測試結果不同，這裡一律清掉。AM_DATA_DIR 也一樣：本機 bot 的 pane
#   都被注入正式資料目錄，測試（hook spool 的預設目錄等）不該吃到它。清掉它不影響外部編譯
#   （issue #417）：cargo shim 只要 AM_DAEMON_EXE／AM_CONFIG_PATH，資料目錄由 helper 從設定檔推。
#   AM_DAEMON_EXE／AM_CONFIG_PATH 不能清，清了就退回本機排那 2 個名額。
# - 只做檢查，不改任何檔案（fmt 用 --check）。
set -euo pipefail

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

# 失敗的當下磁碟幾乎滿了：多半是 `No space left on device`（同機各 worktree 的 target/ 吃掉的），不是程式碼的問題；
# 錯誤訊息散在 cargo／bun／sqlite 各自的輸出裡不好認（c3914e46 的 ops 假紅），所以在最後明講。
DISK_LOW_KB=2097152
on_exit() {
    local rc=$? avail_kb
    cleanup_temporary_web_dist_stub
    if [ -n "${CANARY_DIR}" ]; then
        # 提早結束（測試紅、被訊號收掉）時護欄目錄還在：先把被擋下的破壞性指令印出來，再清掉。
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

# 負載閘（issue #813）：`CHECK_MAX_LOAD=<1 分鐘負載門檻>` 有設就等負載降到門檻以下再跑（scripts/wait-load.sh），
# 最多等 `CHECK_LOAD_TIMEOUT` 秒（預設 1800）；逾時整輪失敗（exit 124），不照跑。`changed` 延後到確認有工作才等。
load_gate() {
    [ -n "${CHECK_MAX_LOAD:-}" ] || return 0
    step "負載閘：1 分鐘負載 ≤ ${CHECK_MAX_LOAD} 才跑（最多等 ${CHECK_LOAD_TIMEOUT:-1800} 秒）"
    bash scripts/wait-load.sh --max "${CHECK_MAX_LOAD}" --timeout "${CHECK_LOAD_TIMEOUT:-1800}"
    # 只在最外層等一次：ops 測試會在沙盒裡再叫 check.sh（那裡沒有 wait-load.sh），不能讓這個開關漏進去。
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
    step "web: bun test"
    (cd web && bun test)
}
check_web() { web_install; web_verify; web_build; }
# CI 的 web job 拆兩段：先出 bundle 並上傳（daemon 只依賴它），再跑 tsc／oxlint／bun test。
check_web_bundle() { web_install; web_build; }
check_web_verify() { web_verify; }

# cargo test 一次吐幾千行：紅了就把紅的測試名單獨列在最後（高負載偶發紅時，一眼看出是哪一條、有幾條）。
# 輸出照常印到終端；編譯錯誤沒有 `... FAILED` 行，那種紅看 cargo 自己的訊息。
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
    step "daemon: cargo test -p agents-managerd"
    run_daemon_tests
    # 從 daemon 抽出去的 crate（crates/*）各自的測試：-p agents-managerd 不含它們。
    local d
    for d in crates/*/; do
        d="${d%/}"
        step "crate: cargo test -p ${d##*/}"
        env -u AM_MODEL -u AM_EFFORT -u AM_DATA_DIR cargo test -p "${d##*/}" --locked
    done
}

# 記憶體壓力等級（macOS：1=normal、2=warn、4=critical）；讀不到印空字串。WAIT_MEM_FILE 的第二欄可蓋過（測試用，跟 wait-load.sh 同一個來源）。
mem_pressure_level() {
    if [ -n "${WAIT_MEM_FILE:-}" ]; then
        awk 'NR == 1 {print $2}' "${WAIT_MEM_FILE}" 2>/dev/null || true
    else
        sysctl -n kern.memorystatus_vm_pressure_level 2>/dev/null || true
    fi
}

# 跑一條指令（輸出寫進 $1），旁邊起看門狗（issue #1783）：每 CHECK_MEM_WATCH_INTERVAL 秒（預設 5）看一次記憶體壓力，
# 到 critical（4）就砍掉這條指令的整個行程群組（cargo 連同底下的 rustc／測試），失敗並說明原因。16 GB 的 Mac 編到記憶體耗盡
# 會整台卡死到 kernel watchdog panic，寧可這輪紅。`set -m` 讓背景工作各自成一個行程群組，才砍得到整棵樹。
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
    # 背景群組收不到終端的 Ctrl-C：自己轉送，不留下孤兒 cargo。
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

    # Bypass the managed PATH cargo shim. Resolve every compiler tool from rustup so both Cargo and
    # the test binary are built for this Mac instead of silently offloading to the Linux worker.
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

    # 記憶體閘（issue #1783）：不管有沒有設 CHECK_MAX_LOAD 都先等記憶體夠才開編（CPU 負載由上面的 CHECK_MAX_LOAD 閘管，這裡不看）。
    step "記憶體閘：可用記憶體 ≥ ${CHECK_MIN_FREE_MEM:-30}% 且壓力 normal 才開編（最多等 ${CHECK_LOAD_TIMEOUT:-1800} 秒）"
    bash scripts/wait-load.sh --max 999999 --min-free-mem "${CHECK_MIN_FREE_MEM:-30}" --timeout "${CHECK_LOAD_TIMEOUT:-1800}" || return $?

    tmp="$(mktemp -d "${TMPDIR:-/tmp}/am-macos-local.XXXXXX")"
    macos_local_cohorts "$local_cargo" "$local_rustc" "$local_rustdoc" "$tmp/cargo.log" || rc=$?
    rm -rf "$tmp"
    return "$rc"
}

macos_local_cohorts() {
    local local_cargo="$1" local_rustc="$2" local_rustdoc="$3" log="$4" filter output

    # cli_update::tests is the existing process-lock suite (#455/#591); cargo_shim::tests covers
    # process trees and signals. shell::tests has a real ps/lsof smoke test under the macos_local_
    # prefix; future platform-sensitive tests in other modules use that prefix too. Every cohort
    # is required and checked independently so a stale selector cannot hide behind another's tests.
    # These selectors are serial and disjoint: no repeated tests or long per-test allowlist.
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
        if ! printf '%s\n' "$output" | grep -Eq '^test result: ok\. [1-9][0-9]* passed;'; then
            printf 'macos-local required cohort %s selected zero tests\n' "$filter" >&2
            return 1
        fi
    done
}

check_ob() {
    step "repo: 沒有被追蹤的 Python bytecode"
    # 各 bot 在自己的 worktree 跑 python 測試就會改寫 __pycache__；被追蹤的話，git status 會出現
    # 一筆「別人的改動」，還會被誤帶進 commit。.gitignore 已經擋，這裡擋已經被 add 進去的。
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

check_ops() {
    local t selected=__all__
    if [ -n "${1:-}" ] && [ -f scripts/ci-ops-tests.py ]; then
        selected="$(printf '%s\n' "$1" | python3 -B scripts/ci-ops-tests.py)" || selected=__all__
        [ -n "$selected" ] || selected=__all__
    fi
    # macOS 的 bash 3.2 會把變數名後面緊接的全形標點併進變數名，set -u 下直接 unbound
    # variable 而中止（issue #412，三天內踩到三次）。修法一律是 ${VAR}。
    step "ops: shell 變數後面緊接非 ASCII 字元"
    scripts/ops/lint-shell-vars.sh
    # scripts/ops/*.ts 不在 web 的 tsconfig／oxlint 範圍裡（那兩個只看 web/src），
    # 至少確認 bun 載得進來（語法／import 沒壞）。沒有 bun 就跳過並說原因。
    if command -v bun >/dev/null 2>&1; then
        for t in scripts/ops/*.ts; do
            step "ops: bun 載得進 $t"
            bun build --target=bun "$t" --outfile=/dev/null >/dev/null
        done
    else
        step "ops: 沒有 bun，跳過 scripts/ops/*.ts 的載入檢查"
    fi

    # canary 到不了的地方（整個覆寫 PATH、env -i）靜態掃出來，用棘輪擋「又多一處」。
    step "ops: canary lint（PATH 覆寫／env -i）"
    scripts/ops/destructive-canary.sh lint

    # 每一支測試都在護欄底下跑（issue #422）：`launchctl`／`pkill`／`ssh` 這種
    # 「測試永遠不該真的執行」的指令被三層攔下（PATH、匯出的 bash 函式、zsh 的 ZDOTDIR），
    # 真的被叫到就記一筆並讓整輪失敗，看得出是哪一支測試漏擋了。
    # 2026-09-23 就是一支測試的 PATH 沒擋住，真的把 herdr 的 launchd job bootout 掉，全機 pane 消失。
    local canary
    canary="${TMPDIR:-/tmp}/am-canary-$$"
    CANARY_DIR="${canary}"
    eval "$(scripts/ops/destructive-canary.sh arm "${canary}")"
    # `arm` 萬一失敗，命令替換是空字串、`eval` 什麼都不做，而且 `set -e` 在這個位置抓不到——
    # 整輪就會在**毫無保護**的情況下跑完，還不會有任何徵兆（i407 審核指出）。
    # 護欄沒裝起來時，寧可整輪紅在這裡。
    if [ -z "${AM_CANARY_DIR:-}" ] || [ ! -x "${AM_CANARY_DIR}/am-canary-probe" ]; then
        echo "destructive-canary arm 失敗：護欄沒有裝起來，不能在沒有保護的情況下跑 ops 測試" >&2
        exit 1
    fi
    # 新的 *_test.sh 放在 scripts/ 或 scripts/ops/ 就會被撈到；需要外部工具的測試自己 skip 並印原因。
    for t in scripts/*_test.sh scripts/ops/*_test.sh; do
        if [ "$selected" != __all__ ] && ! printf '%s\n' "$selected" | grep -qxF -- "$t"; then continue; fi
        step "ops: $t"
        AM_CANARY_TEST="$t" bash "$t"
    done
    # `bin/agm` 的 CLI 契約測試。以前沒有任何自動化路徑會跑它：上面那個迴圈只撈 `*_test.sh`，
    # `check_ob` 跑的是 OB 那幾支，而 CI 的四個 job 都是呼叫這支 check.sh（issue #538）。
    # `scripts/agm.py` 是 include_str! 編進 daemon 二進位、setup 時寫成 `<cwd>/bin/agm` 的，
    # AGM 的每一條指令都走它——它的測試不該只靠「改到它的人自己記得跑」。
    # 放在護欄裡面跑：它自己起一個 loopback 的假 daemon 與假 gh，不連正式 daemon、不碰正式環境。
    # `-B` 跟 `ob_test.py` 一致：不要在工作樹留 __pycache__（`check_ob` 有一條守衛在擋）。
    if [ "$selected" = __all__ ] || printf '%s\n' "$selected" | grep -qxF scripts/agm_test.py; then
        step "ops: scripts/agm_test.py"
        AM_CANARY_TEST="scripts/agm_test.py" python3 -B scripts/agm_test.py
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
    # 範圍跟 clippy 一樣是整個 workspace（issue #1077）：只帶 -p agents-managerd 時拆出去的 crate 沒人看。
    step "workspace: cargo fmt --check"
    cargo fmt --all -- --check
}

check_clippy() {
    # workspace 的 lib／bin（desktop 要 sidecar；daemon 由下一條 all-targets 驗，避免重跑）。crate 的測試 target 在 daemon 的 #[path] 裡
    # 已由下面那條跑過；直接 `--all-targets` 會讓 am-supervisor 的 lib test 編不過（issue #1077）。範圍擴大後的既有 warning 不在這裡修、不加 -D warnings。
    step "workspace: cargo clippy（lib／bin）"
    cargo clippy --workspace --exclude agents-manager-desktop --exclude agents-managerd --lib --bins --locked
    step "daemon: cargo clippy --all-targets"
    cargo clippy -p agents-managerd --all-targets --locked -- -D warnings
}

# 平台敏感的 daemon 與 crates/ 模組（macos-local 的測試組：cli_update::tests、cargo_shim::tests，以及函式名帶 macos_local_ 的）改了：
# 非 macOS 上跑不了 macos-local（check.sh macos-local 會明確拒絕），至少要明講，不能安靜綠燈讓人以為都驗過了（AGENTS.md：
# 改動 shell／行程／signal 或 BSD 與 GNU 工具差異時要在 Mac 本機跑）。只提醒不擋：Mac 上同樣只提醒，因為它要幾分鐘。
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

# 只跑改到的部分（issue #716）：跟 base（預設 origin/main）比的 commit 差異＋工作樹還沒提交的改動。
# daemon 做 `cargo check --all-targets`（`#[cfg(test)]` 被非測試路徑用到也抓得到），再跑改到的模組測試；build inputs、crate wiring、共用 helper、embedded data 會跑整套。
# `scripts/ci-daemon-filters.sh` 由路徑挑測試過濾字串；選到零個測試會失敗。`CHECK_TESTS=<過濾字串>` 蓋過自動挑的、`CHECK_TESTS=none` 明確略過。全量 CI 仍交給 ubuntu 背景跑。
check_changed() {
    local base="${1:-origin/main}" committed dirty untracked files parts filters="" nfilters=0 f
    # 三個 git 指令各自檢查：`{ a; b; c; } | …` 的結束碼只看最後一個，base 不存在（沒 fetch、淺 clone、沒有共同祖先）時
    # 第一個失敗被吞掉，工作樹乾淨時就印「只有文件類改動，不用跑」然後綠燈——壞 commit 過閘。看不出改了什麼就拒絕放行。
    # 改名只列新路徑的話，把會進 binary 的檔搬到 docs/ 會被當成「只有文件」；關掉改名偵測，舊路徑以刪除出現（issue #1173）。
    # 用環境變數而不加 --no-renames：其他假 git 測試用整串 argv 比對。
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
    if [ -n "${CHECK_TESTS:-}" ] && [ "${CHECK_TESTS}" != none ] && ! echo "$parts" | grep -qx -e daemon -e full; then
        echo "!! CHECK_TESTS=${CHECK_TESTS} 沒有用到：這次改動沒有 daemon 部分，不會跑任何 daemon 測試" >&2
    fi
    if echo "$parts" | grep -qx full; then
        check_ob; check_ops; check_web; check_daemon
        return
    fi
    if echo "$parts" | grep -qx ob; then check_ob; fi
    if echo "$parts" | grep -qx ops; then check_ops "$files"; fi
    if echo "$parts" | grep -qx web; then check_web; fi
    if echo "$parts" | grep -qx daemon; then
        local release_triage_submission_test=0 daemon_tests_will_run=0
        filters="$(printf '%s\n' "$files" | bash scripts/ci-daemon-filters.sh)"
        if printf '%s\n' "$filters" | grep -Eq '^(__am_base|__daemon)_release_triage_submission__$'; then
            release_triage_submission_test=1
            filters="$(printf '%s\n' "$filters" | grep -Ev '^(__am_base|__daemon)_release_triage_submission__$' || true)"
        fi
        if [ -n "${CHECK_TESTS:-}" ]; then
            [ "${CHECK_TESTS}" = none ] || daemon_tests_will_run=1
        elif printf '%s\n' "$filters" | grep -qx '__all__' || [ -n "$filters" ]; then
            daemon_tests_will_run=1
        fi
        # Rust 測試檔有沒有被掛進去（#1336）是純文字檢查，不編譯：改到 daemon／crates 就先跑，不等 ubuntu-ci 的 ops 才紅。
        # CHECK_TESTS=none＝不跑測試，一併略過；ops 部分已經會跑它時不重跑。
        # 假 repo（其他 *_test.sh 只複製 check.sh 幾支腳本進去）沒有這支就不跑；真的 repo 一定有。
        if [ "${CHECK_TESTS:-}" != none ] && [ -f scripts/rust_test_mount_test.sh ] && ! echo "$parts" | grep -qx ops; then
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
        # 從 daemon 抽出去的 crate（crates/am-*）自己的測試：改到哪個就跑哪個；workspace-wide 輸入則全部跑。
        # CHECK_TESTS=none 連這段也不跑（#1251）：none 的意思是「不跑測試」，不是只關 daemon 那一段。
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
                for f in $(printf '%s\n' "$files" | sed -n 's#^crates/\([^/]*\)/.*#\1#p' | sort -u); do
                    step "crate: cargo test -p ${f}"
                    env -u AM_MODEL -u AM_EFFORT -u AM_DATA_DIR cargo test -p "${f}" --locked
                done
            fi
        fi
        # 發行契約（#1185）不歸 CHECK_TESTS 管（#1258）：指定 daemon 過濾字串只換掉 daemon 那一段，契約照跑；
        # CHECK_TESTS=none 才連它一起略過（none 就是不跑測試，略過時明講）。
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
            if printf '%s\n' "$filters" | grep -qx '__all__'; then
                step "daemon: cargo test（build／crate-wide test input）"
                run_daemon_tests
            else
                for f in $filters; do nfilters=$((nfilters + 1)); done
            fi
            if [ "$nfilters" -gt 0 ]; then
                step "daemon: cargo test（改到的模組：$(echo $filters)）"
                # libtest 收多個位置過濾字串（[FILTERS...]），任一個符合就跑；一次跑完，只有全部加起來選到 0 條才拒絕（#1332）。
                # shellcheck disable=SC2086  # 過濾字串是 [a-z_:]，不含空白與萬用字元
                run_daemon_tests -- $filters || return $?
            elif ! printf '%s\n' "$filters" | grep -qx '__all__' && [ "$release_triage_submission_test" = 0 ]; then
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
        # PR：ops <full> <head> 或 ops <full> <base> <head>。差異取「完整成功基準→main tip」與「基準→PR head」
        # 的聯集（merge 結果變動檔案的超集），否則會漏掉基準之後尚未驗過的 main 改動。
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
        # fmt / clippy 現況不乾淨，只報告不擋；見 ci.yml 的註解。
        check_fmt || echo "!! fmt 不乾淨（不擋）"
        check_clippy || echo "!! clippy 不乾淨（不擋）"
        ;;
    *) echo "用法：scripts/check.sh [changed [base]|web|daemon|macos-local|ob|ops|fmt|clippy|all]" >&2; exit 2 ;;
esac
