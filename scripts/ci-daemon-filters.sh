#!/usr/bin/env bash
# 讀 stdin 的改動檔案清單（一行一個 repo 相對路徑），印出 `scripts/check.sh changed` 要跑的 daemon 測試過濾字串
# （cargo test 的 `--` 後面可以放好幾個，任一個符合就跑；一行一個、去重、排序）。
#
# 為什麼要有：`changed` 以前對 daemon 只做 `cargo check --all-targets`，測試要自己記得用 CHECK_TESTS 挑——一天內好幾次
# 靠另外跑測試才抓到壞掉的 commit。整包 daemon 測試要十幾分鐘不能放進收尾，所以挑「改到的模組自己的測試」：
#   - daemon/src/a/b.rs → `a::b::`；a/mod.rs → `a::`；頂層 x.rs → `x::`；測試檔 a/tests.rs → `a::tests::`。
#   - build scripts、crate wiring、共用測試 helper、被嵌入的非 Rust 資料與 daemon/tests 變更，回報 `__all__` 跑全套。
#   - daemon 以外、但 daemon 的測試會在執行期讀的檔案（跟 ci-changed-parts.sh 同一份清單，兩邊要一起改）：
#     scripts/check.sh → cargo_shim、lint-shell-vars.sh → herdr_shim、release-task.md → claude_review、
#     ops/fixtures → supervisor::setup、agm.py 與 persona 文件（被 include_str! 編進 setup/responder）。
# 這是「合理子集」不是全量：跨模組的連帶影響（改了被很多模組用的型別）抓不到，完整的交給 ubuntu-ci（scripts/ops/ubuntu-ci.sh）。
set -euo pipefail

while IFS= read -r f; do
    [ -n "$f" ] || continue
    case "$f" in
        # Build inputs, crate wiring, and shared test setup can invalidate tests across modules.
        # Return an all-suite sentinel: cargo check alone does not exercise those behaviors.
        Cargo.toml | Cargo.lock | rust-toolchain* | .cargo/* | daemon/Cargo.toml | daemon/build.rs \
            | daemon/src/main.rs | daemon/src/lib.rs | daemon/src/testing.rs | daemon/src/test_home.rs | daemon/tests/*)
            printf '__all__\n'
            ;;
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
        scripts/check.sh) echo 'cargo_shim::' ;;
        scripts/ops/lint-shell-vars.sh) echo 'herdr_shim::' ;;
        scripts/ops/claude-release-task.md | scripts/ops/codex-release-task.md) echo 'claude_review::' ;;
        scripts/ops/fixtures/*) echo 'supervisor::setup::' ;;
        scripts/agm.py) echo 'supervisor::setup::' ;;
        docs/goals/agm-supervisor-persona.md) echo 'supervisor::persona::'; echo 'supervisor::setup::' ;;
        docs/goals/agm-responder-persona.md) echo 'supervisor::persona::'; echo 'supervisor::responder::' ;;
    esac
done | sort -u
