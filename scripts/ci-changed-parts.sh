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
        # daemon 用 include_str! 編進去的文件（`supervisor::persona::BUILD_INPUTS`，那邊的測試擋「又多一個沒列」）與 daemon 底下的
        # .md fixture 不是「只有文件」：改名、刪掉會編不過，也要先於下面的文件規則。
        docs/goals/agm-supervisor-persona.md | docs/goals/agm-responder-persona.md | daemon/*) daemon=1 ;;
        docs/* | *.md | LICENSE | .gitignore | .github/ISSUE_TEMPLATE/*) ;;
        web/*) web=1 ;;
        daemon/* | Cargo.toml | Cargo.lock | rust-toolchain* | .cargo/*) daemon=1 ;;
        # agm.py 與 herdr shim 等腳本由 daemon include_str! 編進去，改了兩邊都要看。
        scripts/agm.py | scripts/agm_test.py) ops=1; daemon=1 ;;
        # chatgpt-consult.sh 同時是 scripts/ 底下的 shell：變數寫法 lint 與 canary 在 ops。
        scripts/chatgpt-consult*.sh) ob=1; ops=1 ;;
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
