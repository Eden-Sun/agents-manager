#!/usr/bin/env bash
# 讀 stdin 的改動檔案清單（一行一個 repo 相對路徑），印出 `scripts/check.sh changed` 要跑的 daemon 測試過濾字串
# （cargo test 的 `--` 後面可以放好幾個，任一個符合就跑；一行一個、去重、排序）。
#
# 為什麼要有：`changed` 以前對 daemon 只做 `cargo check --all-targets`，測試要自己記得用 CHECK_TESTS 挑——一天內好幾次
# 靠另外跑測試才抓到壞掉的 commit。整包 daemon 測試要十幾分鐘不能放進收尾，所以挑「改到的模組自己的測試」：
#   - daemon/src/a/b.rs → `a::b::`；a/mod.rs → `a::`；頂層 x.rs → `x::`；am-base Rust 模組 → daemon composition harness；am-lifecycle／am-share Rust 模組沿用原 daemon 路徑（share/…→`share::…::`）。
#   - build scripts、crate wiring、共用測試 helper、被嵌入的非 Rust 資料與 daemon/tests 變更，回報 `__all__` 跑全套。
#     crate 的 Cargo.toml／build.rs，與以 #[path] 編進 daemon 的 am-lifecycle／am-share／am-supervisor 的 lib.rs 也是 `__all__`（不能當成一般模組）。
#   - daemon 以外、但 daemon 的測試會在執行期讀的檔案（跟 ci-changed-parts.sh 同一份清單，兩邊要一起改）：
#     scripts/check.sh → cargo_shim、lint-shell-vars.sh → herdr_shim、release-task.md → claude_review、
#     ops/fixtures → supervisor::setup、agm.py 與 persona 文件（被 include_str! 編進 setup/responder）、docs/API.md（api_doc_parity、ws_event_docs_tests）。
# 這是「合理子集」不是全量：跨模組的連帶影響（改了被很多模組用的型別）抓不到，完整的交給 ubuntu-ci（scripts/ops/ubuntu-ci.sh）。
set -euo pipefail

while IFS= read -r f; do
    [ -n "$f" ] || continue
    case "$f" in
        # Build inputs, crate wiring, and shared test setup can invalidate tests across modules.
        # Return an all-suite sentinel: cargo check alone does not exercise those behaviors.
        # crate 的 Cargo.toml／build.rs，以及以 #[path] 編進 daemon 的三個 crate 的 lib.rs，也是 __all__（issue #1018）。
        Cargo.toml | Cargo.lock | rust-toolchain* | .cargo/* | daemon/Cargo.toml | daemon/build.rs \
            | daemon/src/main.rs | daemon/src/lib.rs | daemon/src/testing.rs | daemon/src/test_home.rs | daemon/tests/* \
            | crates/*/Cargo.toml | crates/*/build.rs \
            | crates/am-lifecycle/src/lib.rs | crates/am-share/src/lib.rs | crates/am-supervisor/src/lib.rs)
            printf '__all__\n'
            ;;
        # child_done 的測試由 am-lifecycle 的 child_done.rs 以 #[path] 編進 daemon（lib.rs），模組路徑是 child_done::tests::；
        # 按檔名選會選到零個（#1033 的驗證就是這樣被拒絕放行）。
        daemon/src/child_done_tests.rs | daemon/src/runners/child_done.rs)
            printf 'child_done::tests::\n'
            ;;
        # 其他用 #[path] 掛成別的模組名的測試檔（同上；掛載點見各 crate 的 #[path] 行，#1169）。照檔名推會選到零個，要明列。
        daemon/src/child_alerts_tests.rs) printf 'child_alerts::tests::\n' ;;
        daemon/src/judge_tests.rs) printf 'judge::tests::\n' ;;
        crates/am-lifecycle/src/lifecycle/grok_transcript_tests.rs | crates/am-lifecycle/src/lifecycle/claude_child_log_tests.rs \
            | crates/am-lifecycle/src/lifecycle/suggestion_tests.rs | crates/am-lifecycle/src/lifecycle/composer_draft_tests.rs)
            mod="${f#crates/am-lifecycle/src/}"
            mod="${mod%_tests.rs}"
            printf '%s::tests::\n' "${mod//\//::}"
            ;;
        crates/am-lifecycle/src/lifecycle/force_abort_tests.rs) printf 'lifecycle::stop::force_abort_tests::\n' ;;
        crates/am-share/src/share/trusted_tests.rs) printf 'share::tests::trusted::\n' ;;
        crates/am-share/src/share/portal_upload_tests.rs) printf 'share::portal::upload_tests::\n' ;;
        daemon/src/*)
            rel="${f#daemon/src/}"
            case "$rel" in
                */fixtures/* | fixtures/*)
                    printf '__all__\n'
                    continue
                    ;;
            esac
            case "$rel" in
                *.rs) ;;
                *) printf '__all__\n'; continue ;;
            esac
            mod="${rel%.rs}"
            mod="${mod%/mod}"
            printf '%s::\n' "${mod//\//::}"
            ;;
        crates/am-base/src/*)
            rel="${f#crates/am-base/src/}"
            case "$rel" in
                *.rs) printf 'runners::am_base_tests::\n' ;;
                *) printf '__all__\n' ;;
            esac
            ;;
        crates/am-lifecycle/src/* | crates/am-share/src/*)
            rel="${f#crates/*/src/}"
            case "$rel" in
                *.rs)
                    mod="${rel%.rs}"
                    mod="${mod%/mod}"
                    printf '%s::\n' "${mod//\//::}"
                    ;;
                *) printf '__all__\n' ;;
            esac
            ;;
        crates/am-supervisor/src/*)
            rel="${f#crates/am-supervisor/src/}"
            case "$rel" in
                *.rs) ;;
                *) printf '__all__\n'; continue ;;
            esac
            mod="${rel%.rs}"
            mod="${mod%/mod}"
            printf '%s::\n' "${mod//\//::}"
            ;;
        scripts/check.sh) echo 'cargo_shim::' ;;
        scripts/ops/lint-shell-vars.sh) echo 'herdr_shim::' ;;
        scripts/ops/claude-release-task.md | scripts/ops/codex-release-task.md) echo 'claude_review::' ;;
        scripts/ops/fixtures/*) echo 'supervisor::setup::' ;;
        # am-base owns this include_str! template-contract test, so it needs a crate-specific selector in check.sh.
        scripts/ops/release-triage-task.md) echo '__am_base_release_triage_submission__' ;;
        scripts/agm.py) echo 'supervisor::setup::' ;;
        docs/goals/agm-supervisor-persona.md) echo 'supervisor::persona::'; echo 'supervisor::setup::' ;;
        docs/goals/agm-responder-persona.md) echo 'supervisor::persona::'; echo 'supervisor::responder::' ;;
        # docs/API.md：路由對照（daemon/tests/api_doc_parity.rs，整合測試沒有模組前綴）與 WS 事件表（ws_event_docs_tests，#907）。#1170。
        docs/API.md) echo 'api_route_methods_match_documented_inventory'; echo 'ws_event_docs_tests::' ;;
    esac
done | sort -u
