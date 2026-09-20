#!/usr/bin/env bash
# 收尾前一鍵跑跟 CI（.github/workflows/ci.yml）同一組檢查。
#
#   scripts/check.sh            # OB + ops + web + daemon
#   scripts/check.sh web        # bun install --frozen-lockfile、tsc、oxlint、bun test、vite build
#   scripts/check.sh daemon     # cargo test -p agents-managerd（要先有 web/dist）
#   scripts/check.sh ops        # scripts/*_test.sh 與 scripts/ops/*_test.sh（shell 腳本的隔離測試，假 agm／假 gh／假 sccache，不碰正式環境）
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
#   都被注入正式資料目錄，測試（hook spool 的預設目錄等）不該吃到它。
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
    # 新的 *_test.sh 放在 scripts/ 或 scripts/ops/ 就會被撈到；需要外部工具的測試自己 skip 並印原因。
    for t in scripts/*_test.sh scripts/ops/*_test.sh; do
        step "ops: $t"
        bash "$t"
    done
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
