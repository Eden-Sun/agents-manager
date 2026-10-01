#!/usr/bin/env bash
# 讀 stdin 的改動檔案清單（一行一個 repo 相對路徑），印出 `scripts/check.sh changed` 要跑的部分
# （web／daemon／ops／ob，一行一個、去重、照這個順序）。認不得的路徑一律印 `full`，寧可多跑。
# issue #716：小修改不必每次跑完整 CI；完整驗證交給 ubuntu 的背景 CI（scripts/ops/ubuntu-ci.sh）。
set -euo pipefail

web=0 daemon=0 ops=0 ob=0 full=0
while IFS= read -r f; do
    [ -n "$f" ] || continue
    case "$f" in
        # project skill（例如 verify）是 .md 但會被 Claude 當指令執行：要先於下面的文件規則，歸 ops（scripts/verify_skill_test.sh 驗契約）。
        .claude/skills/*) ops=1 ;;
        docs/* | *.md | LICENSE | .gitignore | .github/ISSUE_TEMPLATE/*) ;;
        web/*) web=1 ;;
        daemon/* | Cargo.toml | Cargo.lock | rust-toolchain* | .cargo/*) daemon=1 ;;
        # agm.py 與 herdr shim 等腳本由 daemon include_str! 編進去，改了兩邊都要看。
        scripts/agm.py | scripts/agm_test.py) ops=1; daemon=1 ;;
        scripts/ob* | scripts/chatgpt-consult*) ob=1 ;;
        scripts/* | bin/* | ops/*) ops=1 ;;
        .github/workflows/*) ops=1 ;;
        *) full=1 ;;
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
