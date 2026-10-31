#!/usr/bin/env bash
# scripts/ci-daemon-filters.sh 的路徑 → cargo test 過濾字串對應（2026-10-02 閘門審查）。純函式：stdin 檔案清單 → 過濾字串（一行一個）。
set -euo pipefail
cd "$(dirname "$0")/.."

fail=0
check() {
    local want="$1" got
    shift
    got="$(printf '%s\n' "$@" | bash scripts/ci-daemon-filters.sh | tr '\n' ' ' | sed 's/ $//')"
    if [ "$got" != "$want" ]; then
        printf 'FAIL: %s → got [%s] want [%s]\n' "$*" "$got" "$want" >&2
        fail=1
    fi
}

# 模組路徑：daemon/src/a/b.rs → a::b::；mod.rs 就是目錄本身；頂層 x.rs → x::。
# daemon/src/**/*.rs 內非 _tests.rs 的 production 檔同時會觸發跨檔守門 timestamp_compat_tests::（#1983）。
# daemon/src 下所有 .rs 檔（只排除 testing.rs 自己）改動時會觸發 testing::scratch_leak_guard::（#1983）。
check "lifecycle::queue:: testing::scratch_leak_guard:: timestamp_compat_tests::" "daemon/src/lifecycle/queue.rs"
check "api:: testing::scratch_leak_guard:: timestamp_compat_tests::" "daemon/src/api.rs"
check "supervisor::" "crates/am-supervisor/src/supervisor/mod.rs"
check "supervisor::controller::" "crates/am-supervisor/src/supervisor/controller.rs"
# 測試檔就在自己的模組底下（foo/tests.rs → foo::tests::，包含在 foo:: 裡）。
# 檔名不是 _tests.rs 結尾的測試檔（如 tests.rs）也屬於 production_sources 掃描範圍，因此帶有 timestamp_compat_tests::。
check "host_baseline::tests:: testing::scratch_leak_guard:: timestamp_compat_tests::" "daemon/src/host_baseline/tests.rs"
# 用 #[path] 掛成別的模組名的測試檔：照檔名推會選到零個（同 #1033 的 child_done）。#1169。
# 以 _tests.rs 結尾者不加選 timestamp_compat_tests::，但會加選 testing::scratch_leak_guard::。
check "child_alerts::tests:: testing::scratch_leak_guard::" "daemon/src/child_alerts_tests.rs"
check "judge::tests:: testing::scratch_leak_guard::" "daemon/src/judge_tests.rs"
check "lifecycle::grok_transcript::tests::" "crates/am-lifecycle/src/lifecycle/grok_transcript_tests.rs"
check "lifecycle::claude_child_log::tests::" "crates/am-lifecycle/src/lifecycle/claude_child_log_tests.rs"
check "lifecycle::suggestion::tests::" "crates/am-lifecycle/src/lifecycle/suggestion_tests.rs"
check "lifecycle::composer_draft::tests::" "crates/am-lifecycle/src/lifecycle/composer_draft_tests.rs"
check "lifecycle::stop::force_abort_tests::" "crates/am-lifecycle/src/lifecycle/force_abort_tests.rs"
check "lifecycle::claude_resume_menu::tests::" "crates/am-lifecycle/src/lifecycle/claude_resume_menu_tests.rs"
check "lifecycle::unsent_draft::tests::" "crates/am-lifecycle/src/lifecycle/unsent_draft_tests.rs"
check "lifecycle::codex_rollout::turn_boundary::" "crates/am-lifecycle/src/lifecycle/codex_rollout_turn.rs"
# 混合：兩支新路徑＋既有有效路徑（judge_tests.rs）同進來，各自的真實過濾字串都要在，且排序去重（#1961）。
check "judge::tests:: lifecycle::claude_resume_menu::tests:: lifecycle::unsent_draft::tests:: testing::scratch_leak_guard::" \
    "crates/am-lifecycle/src/lifecycle/unsent_draft_tests.rs" "daemon/src/judge_tests.rs" \
    "crates/am-lifecycle/src/lifecycle/claude_resume_menu_tests.rs" "crates/am-lifecycle/src/lifecycle/unsent_draft_tests.rs"
check "share::tests::trusted::" "crates/am-share/src/share/trusted_tests.rs"
check "share::portal::upload_tests::" "crates/am-share/src/share/portal_upload_tests.rs"
# 模組自己（含子目錄）沒有測試：照檔名推的字串會選到零個，退到上一層（#1338）。
check "share::" "crates/am-share/src/share/store.rs"
check "lifecycle::" "crates/am-lifecycle/src/lifecycle/codex_steer.rs"
check "runners::supervisor:: testing::scratch_leak_guard:: timestamp_compat_tests::" "daemon/src/runners/supervisor/health.rs"
# 檔案自己沒有測試、但同名子目錄有測試：不退，留在這一層（runners::supervisor:: 底下有測試）。
check "runners::supervisor:: testing::scratch_leak_guard:: timestamp_compat_tests::" "daemon/src/runners/supervisor.rs"
# 頂層檔自己沒有測試：不出自身的模組字串，但若是 production 檔仍會選入跨檔守門（#1983）。
check "cli:: testing::scratch_leak_guard:: timestamp_compat_tests::" "daemon/src/cli.rs"
# 用 #[path] 掛在別的模組底下的檔：照掛載點給字串（api.rs、runners/s6_l.rs、remote_cargo.rs、state.rs）；照檔名推會選到零個（#1338、#1983）。
check "api::shell:: testing::scratch_leak_guard:: timestamp_compat_tests::" "daemon/src/shell.rs"
check "api::bot_scope_matrix:: testing::scratch_leak_guard:: timestamp_compat_tests::" "daemon/src/bot_scope_matrix.rs"
check "runners::s6_l::app_ports_p4sess:: testing::scratch_leak_guard:: timestamp_compat_tests::" "daemon/src/app_ports_p4sess.rs"
check "remote_cargo::hosts:: testing::scratch_leak_guard:: timestamp_compat_tests::" "daemon/src/remote_cargo_hosts.rs"
check "state::limit_hit_tests:: testing::scratch_leak_guard::" "daemon/src/state_limit_hit_tests.rs"
check "api_bot::startup_recovery_tests::startup_abandonment_tests:: testing::scratch_leak_guard::" "daemon/src/api_bot/startup_abandonment_tests.rs"
# 一般 production 檔加選跨檔守門，_tests.rs 測試檔不加選跨檔守門（#1983）。
check "changelog_translate:: testing::scratch_leak_guard:: timestamp_compat_tests::" "daemon/src/changelog_translate.rs"
check "testing::scratch_leak_guard:: timestamp_compat_tests::" "daemon/src/timestamp_compat_tests.rs"
# 直接 `mod x_tests;` 宣告的不受影響。
check "lifecycle::agy_tests::" "crates/am-lifecycle/src/lifecycle/agy_tests.rs"
check "" "crates/am-share/src/share/remote_fs_tests.rs"
# Build inputs, crate wiring, shared test helpers, and include data can affect tests across modules.
check "__all__" "daemon/build.rs"
check "__all__" "Cargo.toml"
check "__all__" "Cargo.lock"
check "__all__" ".cargo/config.toml"
check "__all__" "rust-toolchain.toml"
check "__all__" "daemon/Cargo.toml"
check "__all__" "daemon/tests/fixtures/capture/claude/input.txt"
check "__all__ timestamp_compat_tests::" "daemon/src/testing.rs"
check "__all__ testing::scratch_leak_guard:: timestamp_compat_tests::" "daemon/src/test_home.rs"
check "__all__ testing::scratch_leak_guard:: timestamp_compat_tests::" "daemon/src/main.rs"
check "__all__ testing::scratch_leak_guard:: timestamp_compat_tests::" "daemon/src/lib.rs"
check "__all__" "crates/am-lifecycle/src/lifecycle/fixtures/claude-2.1.281-draft.ansi"
check "__all__" "crates/am-base/src/release_triage/rules.toml"
check "lifecycle::queue::" "crates/am-lifecycle/src/lifecycle/queue.rs"
check "codex_history::" "crates/am-lifecycle/src/codex_history.rs"
check "lifecycle::" "crates/am-lifecycle/src/lifecycle/mod.rs"
check "__all__" "crates/am-lifecycle/src/lifecycle/fixtures/capture.txt"
check "build_info::" "crates/am-supervisor/src/build_info.rs"
check "share::portal::" "crates/am-share/src/share/portal.rs"
check "share::" "crates/am-share/src/share/mod.rs"
check "__all__ lifecycle::queue:: testing::scratch_leak_guard:: timestamp_compat_tests::" "daemon/build.rs" "daemon/src/lifecycle/queue.rs"
# 多個檔案：去重、排序。
check "lifecycle::prompt:: lifecycle::queue:: testing::scratch_leak_guard:: timestamp_compat_tests::" "daemon/src/lifecycle/queue.rs" "daemon/src/lifecycle/prompt.rs" "daemon/src/lifecycle/queue.rs"
# daemon 以外、但 daemon 的測試會讀的檔案（跟 ci-changed-parts.sh 同一份清單）。
check "cargo_shim::" "scripts/check.sh"
check "herdr_shim::" "scripts/ops/lint-shell-vars.sh"
check "claude_review::" "scripts/ops/codex-release-task.md"
check "__daemon_release_triage_submission__" "scripts/ops/release-triage-task.md"
check "supervisor::setup::" "scripts/ops/fixtures/patrol-runtime.json"
check "supervisor::setup::" "scripts/agm.py"
check "supervisor::persona:: supervisor::responder:: supervisor::setup::" "docs/goals/agm-supervisor-persona.md" "docs/goals/agm-responder-persona.md"
# docs/API.md：路由對照（api_doc_parity，整合測試沒有模組前綴）與 WS 事件表（ws_event_docs_tests）。#1170。
check "api_route_methods_match_documented_inventory ws_event_docs_tests::" "docs/API.md"
# 跟 daemon 無關的檔案不產生過濾字串。
check "" "web/src/store/store.ts" "docs/SPEC.md" "scripts/ops/README.md"
# crate 的 lib.rs 是接線檔（#[path] 進 daemon），不能對成 lib:: 這種選不到測試的過濾字串（issue #1018）。
check "__all__" "crates/am-share/src/lib.rs"
check "__all__" "crates/am-lifecycle/src/lib.rs"
check "__all__" "crates/am-supervisor/src/lib.rs"
# crate 的 build.rs／Cargo.toml 改動要跑 daemon 全套，不能印「沒有可挑的測試子集」就綠燈。
check "__all__" "crates/am-supervisor/build.rs"
check "__all__" "crates/am-core/Cargo.toml"
check "__all__ share::portal::" "crates/am-share/src/lib.rs" "crates/am-share/src/share/portal.rs"
check "runners::am_base_tests::" "crates/am-base/src/lib.rs"

check "lifecycle::start::ports::" "crates/am-lifecycle/src/session_ports.rs"
check "lifecycle::send_now::ports::" "crates/am-lifecycle/src/send_ports.rs"
check "share::budget_remote_tests::" "crates/am-share/src/share/budget_remote_tests.rs"
# Crate-owned pure modules need no daemon subset; App/mixed modules still select their tests.
check "" "crates/am-lifecycle/src/ask_answers.rs"
check "" "crates/am-share/src/share/svg_check.rs"
check "supervisor::timing::" "crates/am-supervisor/src/supervisor/timing.rs"
check "build_info::" "crates/am-supervisor/src/build_info.rs"
# am-base uses the actual composition mount table, including aliases and shared fallbacks.
check "runners::am_base_tests::attach::" "crates/am-base/src/attach.rs"
check "runners::am_base_tests::capture_claude::" "crates/am-base/src/capture.rs"
check "runners::am_base_tests::capture_claude::" "crates/am-base/src/capture/claude.daemon-tests-reply_boundary_tests.rs"
check "runners::am_base_tests::rewind:: runners::am_base_tests::rewind_anchor::" "crates/am-base/src/rewind/anchor.rs"
check "runners::am_base_tests::login_assist::" "crates/am-base/src/login_assist/tests.rs"
check "runners::am_base_tests::release_triage::" "crates/am-base/src/release_triage/tests_publish.rs"
for f in lib.rs db.rs db/schema_guard.rs shared_host.rs race_point.rs unrecognised.rs; do
    check "runners::am_base_tests::" "crates/am-base/src/$f"
done
# A different alias is read from the table, and missing/broken mounts fall back broadly.
MOUNTS=$(mktemp -d)
mkdir -p "$MOUNTS/daemon/src/runners/am_base_tests" "$MOUNTS/crates/am-base/src"
printf '#[path = "am_base_tests/alias.rs"]\nmod renamed;\n' > "$MOUNTS/daemon/src/runners/am_base_tests.rs"
printf '#[path = "../../../../crates/am-base/src/attach.daemon-tests-tests.rs"]\nmod tests;\n' > "$MOUNTS/daemon/src/runners/am_base_tests/alias.rs"
am_filter() { (cd "$MOUNTS" && PATH="${AM_CANARY_DIR:+$AM_CANARY_DIR:}$PATH" python3 -B "$OLDPWD/scripts/ci-am-base-filters.py" crates/am-base/src/attach.rs); }
[ "$(am_filter)" = 'runners::am_base_tests::renamed::' ] || fail=1
rm "$MOUNTS/daemon/src/runners/am_base_tests/alias.rs"
[ "$(am_filter)" = 'runners::am_base_tests::' ] || fail=1
rm "$MOUNTS/daemon/src/runners/am_base_tests.rs"
[ "$(am_filter)" = 'runners::am_base_tests::' ] || fail=1
rm -rf "$MOUNTS"

# 巢狀、自己與上層都沒有測試的模組（zz/yy/xx.rs；zz 與 zz/yy 都沒有 #[test]）：只退一層還是 zz::yy::（選到零個，check.sh 拒絕放行，#1338 的同一件事）。
# 要一路退到真的有測試的那一層；一直退到頂層都沒有就不出過濾字串。用 mktemp 的假目錄跑，不碰真的 repo 檔案。
REPO_ROOT="$(pwd)"
NEST=$(mktemp -d)
mkdir -p "$NEST/daemon/src/zz/yy" "$NEST/daemon/src/ww"
printf 'pub fn f() {}\n' > "$NEST/daemon/src/zz/yy/xx.rs"
printf 'pub fn g() {}\n' > "$NEST/daemon/src/zz/mod.rs"
printf '#[test]\nfn t() {}\n' > "$NEST/daemon/src/ww/mod.rs"
printf 'pub fn h() {}\n' > "$NEST/daemon/src/ww/vv.rs"
nest_got() { (cd "$NEST" && printf '%s\n' "$1" | bash "$REPO_ROOT/scripts/ci-daemon-filters.sh" | tr '\n' ' ' | sed 's/ $//'); }
got="$(nest_got daemon/src/zz/yy/xx.rs)"
if [ "$got" != "testing::scratch_leak_guard:: timestamp_compat_tests::" ]; then
    printf 'FAIL: 巢狀沒有測試的模組 daemon/src/zz/yy/xx.rs → got [%s] want [testing::scratch_leak_guard:: timestamp_compat_tests::]\n' "$got" >&2
    fail=1
fi
got="$(nest_got daemon/src/ww/vv.rs)"
if [ "$got" != "testing::scratch_leak_guard:: timestamp_compat_tests:: ww::" ]; then
    printf 'FAIL: 同層檔沒有測試、上一層 ww/mod.rs 有測試 → got [%s] want [testing::scratch_leak_guard:: timestamp_compat_tests:: ww::]\n' "$got" >&2
    fail=1
fi
# #1517 的形狀：a/mod.rs 有測試，a/b/c.rs 與 a/b/ 都沒有：要退到 a::，不能停在 a::b::（選到零個）。
mkdir -p "$NEST/daemon/src/aa/bb"
printf '#[test]\nfn t() {}\n' > "$NEST/daemon/src/aa/mod.rs"
printf 'pub fn k() {}\n' > "$NEST/daemon/src/aa/bb/c.rs"
got="$(nest_got daemon/src/aa/bb/c.rs)"
if [ "$got" != "aa:: testing::scratch_leak_guard:: timestamp_compat_tests::" ]; then
    printf 'FAIL: a/b/c.rs（a 有測試、a/b 沒有）→ got [%s] want [aa:: testing::scratch_leak_guard:: timestamp_compat_tests::]\n' "$got" >&2
    fail=1
fi

# 自動掃描 daemon/src 下所有 #[path = ...] mod 宣告（#1983 條件 C）。
# 對每個被掛載的目標檔跑 selector，斷言輸出等於其真實模組路徑（不可退回或照檔名推錯）。
python3 -B - <<'EOF' || fail=1
import os, re, subprocess, sys

pattern = re.compile(r'#\[path\s*=\s*"([^"]+)"\]\s*(?:#\[[^\]]+\]\s*)*(?:pub(?:\([^\)]+\))?\s+)?mod\s+([a-zA-Z0-9_]+);')
alias_targets = {}
for root, dirs, files in os.walk("daemon/src"):
    for file in files:
        if file.endswith(".rs"):
            p = os.path.join(root, file)
            content = open(p).read()
            for m in pattern.finditer(content):
                target_rel = m.group(1)
                mod_name = m.group(2)
                target_full = os.path.normpath(os.path.join(root, target_rel))
                if target_full.startswith("daemon/src") and not target_full.startswith("daemon/src/runners/am_base_tests"):
                    parent_rel = os.path.relpath(p, "daemon/src")
                    if parent_rel.endswith("/mod.rs"):
                        parent_mod = parent_rel[:-7]
                    elif parent_rel.endswith(".rs"):
                        parent_mod = parent_rel[:-3]
                    else:
                        parent_mod = ""
                    p_prefix = parent_mod.replace("/", "::")
                    expected_mod = f"{p_prefix}::{mod_name}::" if p_prefix else f"{mod_name}::"
                    alias_targets[target_full] = expected_mod

assert alias_targets, "沒有掃描到任何 daemon/src 內的 #[path] 掛載檔"
for target_file, expected_mod in sorted(alias_targets.items()):
    out = subprocess.check_output(
        ["bash", "scripts/ci-daemon-filters.sh"],
        input=f"{target_file}\n",
        text=True
    )
    tokens = out.strip().split()
    if expected_mod not in tokens:
        print(f"FAIL: #[path] 掛載檔 {target_file} 期望包含真實模組 {expected_mod}，實際輸出為: {tokens}", file=sys.stderr)
        sys.exit(1)
EOF

if [ "$fail" = 0 ]; then echo "ci-daemon-filters: OK"; fi
exit "$fail"
