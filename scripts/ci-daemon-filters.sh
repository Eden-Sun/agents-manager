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

# 模組自己（含子目錄）沒有任何測試時，照檔名推的過濾字串一定選到 0 條，check.sh changed 會以「沒有選到任何測試」拒絕放行（#1338）。
# 這時退到上一層模組（share/store.rs → share::，share 底下有測試）；一路退到頂層都沒有測試就不出過濾字串（check.sh 會印「沒有可挑的測試子集」）。
# 檔案不存在（被刪掉、或測試給的假路徑）照舊照檔名推。mod.rs 的子目錄就是它自己，子模組有測試也算有測試。
# 模組有沒有測試：模組檔（<m>.rs 或 <m>/mod.rs）裡有 #[test] 或 `mod …tests;` 宣告，或 <m>/ 底下任何檔有 #[test]。
module_has_tests() { # module_has_tests <src 根目錄> <用 / 分隔的模組路徑>
    local root="$1" m="$2" file
    for file in "$root/$m.rs" "$root/$m/mod.rs"; do
        [ -f "$file" ] && grep -Eq '#\[(tokio::)?test|mod [a-z_0-9]*tests?[a-z_0-9]*[[:space:]]*;' "$file" && return 0
    done
    [ -d "$root/$m" ] && grep -rEq '#\[(tokio::)?test' "$root/$m" && return 0
    return 1
}

emit_mod() { # emit_mod <repo 相對路徑> <用 / 分隔的模組路徑>
    local f="$1" mod="$2" base root
    case "$f" in
        crates/am-lifecycle/src/*.rs | crates/am-share/src/*.rs | crates/am-supervisor/src/*.rs)
            # These tests already run in the owning crate target. Keep mixed/App modules below.
            if [ -f scripts/ci-crate-only-tests.py ] && python3 -B scripts/ci-crate-only-tests.py "$f"; then return 0; fi
            ;;
    esac
    if [ -f "$f" ]; then
        # 模組路徑從 src 根目錄起算：去掉副檔名與 /mod，再去掉模組路徑那一段尾巴，剩下的就是 src 根目錄。
        base="${f%.rs}"; base="${base%/mod}"
        root="${base%/"$mod"}"
        while ! module_has_tests "$root" "$mod"; do
            case "$mod" in */*) mod="${mod%/*}" ;; *) return 0 ;; esac
        done
    fi
    printf '%s::\n' "${mod//\//::}"
}

# 跨檔原始碼掃描型契約測試對應表（#1983）。
# 格式：<過濾字串>|<包含路徑前綴（以空格分隔）>|<排除規則（以空格分隔）>
# 每當改動檔案落在包含前綴且不符合排除規則時，一律加選該守門測試。
# timestamp_compat_tests 條件與其 production_sources() 完全一致：
#   daemon/src/ 底下 *.rs，排除 timestamp_compat_tests.rs 本身與 *_tests.rs 結尾的檔（tests.rs 不排除）。
GUARD_RULES=(
    "timestamp_compat_tests::|daemon/src/|timestamp_compat_tests.rs *_tests.rs"
    "testing::scratch_leak_guard::|daemon/src/|testing.rs"
)

emit_guard_rules() { # emit_guard_rules <repo 相對路徑>
    local f="$1" rule guard_test prefix_pats exc_pats prefix exc matched
    case "$f" in
        *.rs) ;;
        *) return 0 ;;
    esac
    for rule in "${GUARD_RULES[@]}"; do
        IFS="|" read -r guard_test prefix_pats exc_pats <<<"$rule"
        matched=0
        for prefix in $prefix_pats; do
            case "$f" in
                "$prefix"*) matched=1; break ;;
            esac
        done
        [ "$matched" = 1 ] || continue
        for exc in $exc_pats; do
            case "${f##*/}" in
                $exc) matched=0; break ;;
            esac
            case "$f" in
                $exc) matched=0; break ;;
            esac
        done
        if [ "$matched" = 1 ]; then
            printf '%s\n' "$guard_test"
        fi
    done
}

while IFS= read -r f; do
    [ -n "$f" ] || continue
    emit_guard_rules "$f"
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
        # 用 #[path] 掛在別的模組底下的檔（掛載點：daemon/src/api.rs、daemon/src/runners/s6_l.rs、daemon/src/remote_cargo.rs、daemon/src/state.rs）；照檔名推會選到零個（#1338、#1983）。
        daemon/src/shell.rs) printf 'api::shell::\n' ;;
        daemon/src/bot_scope_matrix.rs) printf 'api::bot_scope_matrix::\n' ;;
        daemon/src/app_ports_p4sess.rs) printf 'runners::s6_l::app_ports_p4sess::\n' ;;
        daemon/src/remote_cargo_hosts.rs) printf 'remote_cargo::hosts::\n' ;;
        daemon/src/state_limit_hit_tests.rs) printf 'state::limit_hit_tests::\n' ;;
        crates/am-lifecycle/src/lifecycle/grok_transcript_tests.rs | crates/am-lifecycle/src/lifecycle/claude_child_log_tests.rs \
            | crates/am-lifecycle/src/lifecycle/suggestion_tests.rs | crates/am-lifecycle/src/lifecycle/composer_draft_tests.rs \
            | crates/am-lifecycle/src/lifecycle/claude_resume_menu_tests.rs | crates/am-lifecycle/src/lifecycle/unsent_draft_tests.rs)
            mod="${f#crates/am-lifecycle/src/}"
            mod="${mod%_tests.rs}"
            printf '%s::tests::\n' "${mod//\//::}"
            ;;
        crates/am-lifecycle/src/lifecycle/codex_rollout_turn.rs) printf 'lifecycle::codex_rollout::turn_boundary::\n' ;;
        crates/am-lifecycle/src/session_ports.rs) printf 'lifecycle::start::ports::\n' ;;
        crates/am-lifecycle/src/send_ports.rs) printf 'lifecycle::send_now::ports::\n' ;;
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
            emit_mod "$f" "$mod"
            ;;
        crates/am-base/src/*)
            rel="${f#crates/am-base/src/}"
            case "$rel" in
                *.rs)
                    if [ -f scripts/ci-am-base-filters.py ]; then
                        python3 -B scripts/ci-am-base-filters.py "$f" || printf 'runners::am_base_tests::\n'
                    else
                        printf 'runners::am_base_tests::\n'
                    fi
                    ;;
                *) printf '__all__\n' ;;
            esac
            ;;
        crates/am-lifecycle/src/* | crates/am-share/src/*)
            rel="${f#crates/*/src/}"
            case "$rel" in
                *.rs)
                    mod="${rel%.rs}"
                    mod="${mod%/mod}"
                    emit_mod "$f" "$mod"
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
            emit_mod "$f" "$mod"
            ;;
        scripts/check.sh) echo 'cargo_shim::' ;;
        scripts/ops/lint-shell-vars.sh) echo 'herdr_shim::' ;;
        scripts/ops/claude-release-task.md | scripts/ops/codex-release-task.md) echo 'claude_review::' ;;
        scripts/ops/fixtures/*) echo 'supervisor::setup::' ;;
        # The test is mounted under the daemon harness because it also uses daemon-only test support.
        scripts/ops/release-triage-task.md) echo '__daemon_release_triage_submission__' ;;
        scripts/agm.py) echo 'supervisor::setup::' ;;
        docs/goals/agm-supervisor-persona.md) echo 'supervisor::persona::'; echo 'supervisor::setup::' ;;
        docs/goals/agm-responder-persona.md) echo 'supervisor::persona::'; echo 'supervisor::responder::' ;;
        # docs/API.md：路由對照（daemon/tests/api_doc_parity.rs，整合測試沒有模組前綴）與 WS 事件表（ws_event_docs_tests，#907）。#1170。
        docs/API.md) echo 'api_route_methods_match_documented_inventory'; echo 'ws_event_docs_tests::' ;;
    esac
done | LC_ALL=C sort -u
