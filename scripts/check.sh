#!/usr/bin/env bash
# 收尾前一鍵跑跟 CI（.github/workflows/ci.yml）同一組檢查。
#
#   scripts/check.sh            # OB + ops + web + daemon
#   scripts/check.sh web        # bun install --frozen-lockfile、tsc、oxlint、bun test、vite build
#   scripts/check.sh daemon     # cargo test -p agents-managerd（要先有 web/dist）
#   scripts/check.sh ops        # shell 變數寫法的 lint，加上 scripts/*_test.sh 與 scripts/ops/*_test.sh（shell 腳本的隔離測試，假 agm／假 gh／假 sccache，不碰正式環境）
#   scripts/check.sh ob         # 沒有被追蹤的 bytecode；OB queue/operator 與瀏覽器契約（隔離，不用登入）
#   scripts/check.sh fmt        # cargo fmt --check（只報告，現況不乾淨）
#   scripts/check.sh clippy     # cargo clippy（只報告，現況不乾淨）
#   scripts/check.sh all        # 以上全部
#
# 注意：
# - web 的型別檢查一定要 `tsc -p tsconfig.app.json`；根目錄的 tsconfig.json 只有
#   references 沒有 files，`tsc --noEmit` 什麼都不檢查（假綠燈）。
# - web 的單元測試用 `bun test`（不是 `node --test`）：測試檔是 `node:test` 寫的，bun 直接吃，
#   而且會解析 `.tsx` 與沒副檔名的 import；node 的 --experimental-strip-types 對那兩種都會
#   ERR_MODULE_NOT_FOUND。
# - daemon 用 rust-embed 把 web/dist 編進二進位，所以 web 要先 build。
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

check_web() {
    step "web: bun install --frozen-lockfile"
    (cd web && bun install --frozen-lockfile)
    step "web: tsc -p tsconfig.app.json --noEmit"
    (cd web && bunx tsc -p tsconfig.app.json --noEmit)
    step "web: oxlint src"
    (cd web && bunx oxlint src)
    step "web: bun test"
    (cd web && bun test)
    step "web: bun run build"
    (cd web && bun run build)
}

check_daemon() {
    if [ ! -f web/dist/index.html ]; then
        echo "web/dist/index.html 不存在：先跑 scripts/check.sh web（daemon 會把它嵌進去）" >&2
        exit 1
    fi
    step "daemon: cargo test -p agents-managerd"
    env -u AM_MODEL -u AM_EFFORT -u AM_DATA_DIR cargo test -p agents-managerd --locked
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
    local t
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
        step "ops: $t"
        AM_CANARY_TEST="$t" bash "$t"
    done
    if ! scripts/ops/destructive-canary.sh hits "${canary}"; then
        echo "上面這些測試真的叫到了破壞性指令（已被擋下，但那條路要修）：見 scripts/ops/destructive-canary.sh" >&2
        /bin/rm -rf "${canary}"
        exit 1
    fi
    /bin/rm -rf "${canary}"
    echo "canary 乾淨：沒有任何測試叫到 $(scripts/ops/destructive-canary.sh cmds)"
}

check_fmt() {
    step "daemon: cargo fmt --check"
    cargo fmt -p agents-managerd -- --check
}

check_clippy() {
    step "daemon: cargo clippy"
    cargo clippy -p agents-managerd --all-targets --locked -- -D warnings
}

case "${1:-default}" in
    web) check_web ;;
    daemon) check_daemon ;;
    ob) check_ob ;;
    ops) check_ops ;;
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
    *) echo "用法：scripts/check.sh [web|daemon|ob|ops|fmt|clippy|all]" >&2; exit 2 ;;
esac
