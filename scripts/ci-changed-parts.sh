#!/usr/bin/env bash
# 讀 stdin 的改動檔案清單（一行一個 repo 相對路徑），印出 `scripts/check.sh changed` 要跑的部分
# （web／daemon／ops／ob／specs，一行一個、去重、照這個順序）。認不得的路徑一律印 `full`，寧可多跑。
# issue #716：小修改不必每次跑完整 CI；完整驗證交給 ubuntu 的背景 CI（scripts/ops/ubuntu-ci.sh）。
set -euo pipefail

web=0 daemon=0 ops=0 ob=0 specs=0 full=0
while IFS= read -r f; do
    [ -n "$f" ] || continue
    case "$f" in
        # project skill（例如 verify）是 .md 但會被 Claude 當指令執行：要先於下面的文件規則，歸 ops（scripts/verify_skill_test.sh 驗契約）。
        .claude/skills/*) ops=1 ;;
        # 跨界的讀取（2026-10-02 閘門審查）：下面這些不在自己那一塊的檔案，另一塊的測試會在執行期讀它，改了兩邊都要跑。
        #   - web 的測試讀 am-lifecycle crate 的 fixtures（codexUpdatePrompt／blockedKeys／tuiChoices；#863）。
        #   - ops 的 project-transfer 測試從 am-base schema 原始碼（db.rs）與 supervisor／mission store 抽欄位。
        #   - daemon 的測試讀 scripts/check.sh（cargo_shim）、scripts/ops/lint-shell-vars.sh（herdr_shim）、
        #     scripts/ops/fixtures/*（supervisor::setup）、scripts/ops/{claude,codex}-release-task.md（claude_review）、
        #     scripts/ops/release-triage-task.md（am-base 的提交模板契約測試；#1185）。
        #   - daemon 的測試讀 docs/API.md（api_doc_parity、ws_event_docs_tests；#1170）。
        # 要先於下面的 daemon/*、scripts/*、*.md 規則。對應的 daemon 測試子集見 scripts/ci-daemon-filters.sh（兩邊要一起改）。
        crates/am-lifecycle/src/lifecycle/fixtures/*) daemon=1; web=1 ;;
        crates/am-base/src/db.rs | crates/am-supervisor/src/supervisor/store.rs | crates/am-supervisor/src/supervisor/roles.rs | crates/am-supervisor/src/mission/store.rs) daemon=1; ops=1 ;;
        scripts/check.sh | scripts/ops/lint-shell-vars.sh | scripts/ops/fixtures/* | scripts/ops/claude-release-task.md | scripts/ops/codex-release-task.md) ops=1; daemon=1 ;;
        scripts/ops/release-triage-task.md) ops=1; daemon=1 ;;
        # scripts/ 底下的 .md 不是「只有文件」：任務檔被 install-manifest／ops 測試／daemon 讀，README 內容也有測試釘住。
        scripts/*.md) ops=1 ;;
        # daemon 用 include_str! 編進去的文件（`supervisor::persona::BUILD_INPUTS`，那邊的測試擋「又多一個沒列」）與 daemon 底下的
        # .md fixture 不是「只有文件」：改名、刪掉會編不過，也要先於下面的文件規則。
        docs/goals/agm-supervisor-persona.md | docs/goals/agm-responder-persona.md | docs/API.md | daemon/*) daemon=1 ;;
        # 有測試釘住內容的文件：docs/SPEC.md（scripts/jev-role_test.sh 要求裡面有 Jev 角色政策那幾句）。
        # 只改它不必跑整包 ops，獨立一個 `specs` 部分只跑那支契約測試（check.sh changed 處理）。
        docs/SPEC.md) specs=1 ;;
        docs/* | *.md | LICENSE | .gitignore | .github/ISSUE_TEMPLATE/*) ;;
        web/*) web=1 ;;
        daemon/* | crates/* | Cargo.toml | Cargo.lock | rust-toolchain* | .cargo/*) daemon=1 ;;
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
[ "$specs" = 1 ] && echo specs
exit 0
