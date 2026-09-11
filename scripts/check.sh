#!/usr/bin/env bash
# 收尾前一鍵跑跟 CI（.github/workflows/ci.yml）同一組檢查。
#
#   scripts/check.sh            # web + daemon（會擋 merge 的那兩項）
#   scripts/check.sh web        # bun install --frozen-lockfile、tsc、oxlint、vite build
#   scripts/check.sh daemon     # cargo test -p agents-managerd（要先有 web/dist）
#   scripts/check.sh fmt        # cargo fmt --check（只報告，現況不乾淨）
#   scripts/check.sh clippy     # cargo clippy（只報告，現況不乾淨）
#   scripts/check.sh all        # 以上全部
#
# 注意：
# - web 的型別檢查一定要 `tsc -p tsconfig.app.json`；根目錄的 tsconfig.json 只有
#   references 沒有 files，`tsc --noEmit` 什麼都不檢查（假綠燈）。
# - daemon 用 rust-embed 把 web/dist 編進二進位，所以 web 要先 build。
# - daemon 的測試會讀 AM_MODEL / AM_EFFORT（herdr shim 的沿用邏輯），在 bot 的 pane
#   裡跑時這兩個有值會讓測試結果不同，這裡一律清掉。
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
    step "web: bun run build"
    (cd web && bun run build)
}

check_daemon() {
    if [ ! -f web/dist/index.html ]; then
        echo "web/dist/index.html 不存在：先跑 scripts/check.sh web（daemon 會把它嵌進去）" >&2
        exit 1
    fi
    step "daemon: cargo test -p agents-managerd"
    env -u AM_MODEL -u AM_EFFORT cargo test -p agents-managerd --locked
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
    fmt) check_fmt ;;
    clippy) check_clippy ;;
    default) check_web; check_daemon ;;
    all)
        check_web
        check_daemon
        # fmt / clippy 現況不乾淨，只報告不擋；見 ci.yml 的註解。
        check_fmt || echo "!! fmt 不乾淨（不擋）"
        check_clippy || echo "!! clippy 不乾淨（不擋）"
        ;;
    *) echo "用法：scripts/check.sh [web|daemon|fmt|clippy|all]" >&2; exit 2 ;;
esac
