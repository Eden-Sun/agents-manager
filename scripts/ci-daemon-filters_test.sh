#!/usr/bin/env bash
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

check "lifecycle::queue:: testing::scratch_leak_guard:: timestamp_compat_tests::" "daemon/src/lifecycle/queue.rs"
check "api:: testing::scratch_leak_guard:: timestamp_compat_tests::" "daemon/src/api.rs"
check "host_baseline::tests:: testing::scratch_leak_guard:: timestamp_compat_tests::" "daemon/src/host_baseline/tests.rs"
check "lifecycle_tests::child_alerts:: testing::scratch_leak_guard:: timestamp_compat_tests::" "daemon/src/lifecycle_tests/child_alerts.rs"
check "lifecycle_tests::judge:: testing::scratch_leak_guard:: timestamp_compat_tests::" "daemon/src/lifecycle_tests/judge.rs"
check "judge::collision:: lifecycle_tests::judge::collision::" "crates/am-lifecycle/src/judge/collision.rs"
check "lifecycle_tests::handoff::" "crates/am-lifecycle/src/handoff.rs"
check "background_jobs:: lifecycle_tests::background_jobs::" "crates/am-lifecycle/src/background_jobs.rs"
check "lifecycle_tests::grok_transcript:: testing::scratch_leak_guard:: timestamp_compat_tests::" "daemon/src/lifecycle_tests/grok_transcript.rs"
check "lifecycle_tests::claude_child_log:: testing::scratch_leak_guard:: timestamp_compat_tests::" "daemon/src/lifecycle_tests/claude_child_log.rs"
check "lifecycle_tests::grok_hook:: testing::scratch_leak_guard:: timestamp_compat_tests::" "daemon/src/lifecycle_tests/grok_hook.rs"
check "lifecycle:: lifecycle_tests::claude_child_log::" "crates/am-lifecycle/src/lifecycle/claude_child_log.rs"
check "lifecycle::suggestion::tests:: lifecycle_tests::suggestion::" "crates/am-lifecycle/src/lifecycle/suggestion_tests.rs"
check "lifecycle::composer_draft::tests:: lifecycle_tests::composer_draft::" "crates/am-lifecycle/src/lifecycle/composer_draft_tests.rs"
check "lifecycle::stop::force_abort_tests::" "crates/am-lifecycle/src/lifecycle/force_abort_tests.rs"
check "lifecycle_tests::poller::issue_17_tests::" "crates/am-lifecycle/src/lifecycle/poller_tests/issue_17_tests.rs"
check "lifecycle_tests::poller::folded_paste_tests::" "crates/am-lifecycle/src/lifecycle/poller_tests/folded_paste_tests.rs"
check "lifecycle::poller:: lifecycle_tests::poller::" "crates/am-lifecycle/src/lifecycle/poller.rs"
check "lifecycle::claude_resume_menu::tests:: lifecycle_tests::claude_resume_menu::" "crates/am-lifecycle/src/lifecycle/claude_resume_menu_tests.rs"
check "lifecycle::unsent_draft::tests:: lifecycle_tests::unsent_draft::" "crates/am-lifecycle/src/lifecycle/unsent_draft_tests.rs"
check "lifecycle::codex_rollout::turn_boundary::" "crates/am-lifecycle/src/lifecycle/codex_rollout_turn.rs"
check "lifecycle::claude_resume_menu::tests:: lifecycle::unsent_draft::tests:: lifecycle_tests::claude_resume_menu:: lifecycle_tests::judge:: lifecycle_tests::unsent_draft:: testing::scratch_leak_guard:: timestamp_compat_tests::" \
    "crates/am-lifecycle/src/lifecycle/unsent_draft_tests.rs" "daemon/src/lifecycle_tests/judge.rs" \
    "crates/am-lifecycle/src/lifecycle/claude_resume_menu_tests.rs" "crates/am-lifecycle/src/lifecycle/unsent_draft_tests.rs"
check "share_tests:: testing::scratch_leak_guard:: timestamp_compat_tests::" "daemon/src/share_tests/mod.rs"
check "share_tests::trusted:: testing::scratch_leak_guard:: timestamp_compat_tests::" "daemon/src/share_tests/trusted.rs"
check "share_tests::remote_cage:: testing::scratch_leak_guard:: timestamp_compat_tests::" "daemon/src/share_tests/remote_cage.rs"
check "share_tests::remote_svg:: testing::scratch_leak_guard:: timestamp_compat_tests::" "daemon/src/share_tests/remote_svg.rs"
check "" "crates/am-share/src/share/portal_remote_tests.rs"
check "" "crates/am-share/src/share/portal_upload_tests.rs"
check "" "crates/am-share/src/share/test_env.rs"
check "share_tests::" "crates/am-share/src/share/budget.rs" "crates/am-share/src/share/remote_fs.rs"
check "__all__" "crates/am-share/Cargo.toml"
check "share_tests::" "crates/am-share/src/share/remote_cage_tests.rs"
check "share_tests::" "crates/am-share/src/share/store.rs"
check "lifecycle::" "crates/am-lifecycle/src/lifecycle/codex_steer.rs"
check "lifecycle_tests::screen::" "crates/am-lifecycle/src/lifecycle/screen.rs"
check "lifecycle::setup:: lifecycle_tests::setup::" "crates/am-lifecycle/src/lifecycle/setup.rs"
check "" "crates/am-lifecycle/src/lifecycle/resume_context.rs"
check "codex_history:: lifecycle_tests::codex_history::" "crates/am-lifecycle/src/codex_history.rs"
check "runners::am_base_tests::" "crates/am-base/src/test_dirs.rs"
check "lifecycle::setup:: lifecycle_tests::screen:: lifecycle_tests::setup::" "crates/am-lifecycle/src/lifecycle/setup.rs" "crates/am-lifecycle/src/lifecycle/screen.rs"
check "runners::supervisor:: testing::scratch_leak_guard:: timestamp_compat_tests::" "daemon/src/runners/supervisor/health.rs"
check "runners::supervisor:: testing::scratch_leak_guard:: timestamp_compat_tests::" "daemon/src/runners/supervisor.rs"
check "cli:: testing::scratch_leak_guard:: timestamp_compat_tests::" "daemon/src/cli.rs"
check "api::shell:: testing::scratch_leak_guard:: timestamp_compat_tests::" "daemon/src/shell.rs"
check "api::bot_scope_matrix:: testing::scratch_leak_guard:: timestamp_compat_tests::" "daemon/src/bot_scope_matrix.rs"
check "api::shell_identity_delete:: testing::scratch_leak_guard:: timestamp_compat_tests::" "daemon/src/shell_identity_delete.rs"
check "runners::s6_l::app_ports_p4sess:: testing::scratch_leak_guard:: timestamp_compat_tests::" "daemon/src/app_ports_p4sess.rs"
check "reconcile::compat_tests:: testing::scratch_leak_guard::" "daemon/src/reconcile_tests/compat_tests.rs"
check "remote_cargo::tests:: testing::scratch_leak_guard:: timestamp_compat_tests::" "daemon/src/remote_cargo_tests/tests.rs"
check "remote_cargo::guard_tests:: testing::scratch_leak_guard::" "daemon/src/remote_cargo_tests/guard_tests.rs"
check "remote_cargo::hosts:: testing::scratch_leak_guard:: timestamp_compat_tests::" "daemon/src/remote_cargo_hosts.rs"
check "state::limit_hit_tests:: testing::scratch_leak_guard::" "daemon/src/state_limit_hit_tests.rs"
check "api_bot::startup_recovery_tests::startup_abandonment_tests:: testing::scratch_leak_guard::" "daemon/src/api_bot/startup_abandonment_tests.rs"
check "api_bot::runtime::tests:: testing::scratch_leak_guard::" "daemon/src/api_bot/runtime_tests.rs"
check "api_bot::runtime:: testing::scratch_leak_guard:: timestamp_compat_tests::" "daemon/src/api_bot/runtime.rs"
check "hookrecv::classify_tests::" "crates/am-lifecycle/src/hookrecv_tests/classify_tests.rs"
check "hookrecv::agy_tests::" "crates/am-lifecycle/src/hookrecv_tests/agy_tests.rs"
check "api::prompt_route_tests:: testing::scratch_leak_guard::" "daemon/src/api/prompt_route_tests.rs"
check "api:: testing::scratch_leak_guard:: timestamp_compat_tests::" "daemon/src/api.rs"
check "changelog_translate:: testing::scratch_leak_guard:: timestamp_compat_tests::" "daemon/src/changelog_translate.rs"
check "changelog_translate::stream:: testing::scratch_leak_guard:: timestamp_compat_tests::" "daemon/src/changelog_translate_stream.rs"
check "testing::scratch_leak_guard:: timestamp_compat_tests::" "daemon/src/timestamp_compat_tests.rs"
check "lifecycle_tests::agy_tests::" "crates/am-lifecycle/src/lifecycle/agy_tests.rs"
check "" "crates/am-share/src/share/remote_fs_tests.rs"
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
check "lifecycle_tests::queue::" "crates/am-lifecycle/src/lifecycle/queue.rs"
check "lifecycle_tests::tui_prompts:: tui_prompts::" "crates/am-lifecycle/src/tui_prompts.rs"
check "lifecycle_tests::turn_error:: turn_error::" "crates/am-lifecycle/src/turn_error.rs"
check "codex_history:: lifecycle_tests::codex_history::" "crates/am-lifecycle/src/codex_history.rs"
check "lifecycle::" "crates/am-lifecycle/src/lifecycle/mod.rs"
check "__all__" "crates/am-lifecycle/src/lifecycle/fixtures/capture.txt"
check "share_tests::" "crates/am-share/src/share/portal.rs"
check "share_tests::" "crates/am-share/src/share/mod.rs"
check "__all__ lifecycle::queue:: testing::scratch_leak_guard:: timestamp_compat_tests::" "daemon/build.rs" "daemon/src/lifecycle/queue.rs"
check "lifecycle::prompt:: lifecycle::queue:: testing::scratch_leak_guard:: timestamp_compat_tests::" "daemon/src/lifecycle/queue.rs" "daemon/src/lifecycle/prompt.rs" "daemon/src/lifecycle/queue.rs"
check "cargo_shim::" "scripts/check.sh"
check "herdr_shim::" "scripts/ops/lint-shell-vars.sh"
check "claude_review::" "scripts/ops/codex-release-task.md"
check "__daemon_release_triage_submission__" "scripts/ops/release-triage-task.md"
check "supervisor_tests::setup::" "scripts/ops/fixtures/patrol-runtime.json"
check "supervisor_tests::setup::" "scripts/agm.py"
check "runners::am_base_tests::outbox::" "web/src/lib/outboxList.ts"
check "runners::am_base_tests::cargo_shim::" "scripts/project-cargo.sh"
check "__am_base_crate__" "ops/host-baseline/baseline.toml"
check "__am_base_crate__" "scripts/ops/host-baseline-sync-mac-verify.sh"
check "__am_base_crate__" "web/src/lib/__fixtures__/codex-async-screen-cases.json"
check "supervisor_tests::responder:: supervisor_tests::setup::" "docs/goals/agm-supervisor-persona.md" "docs/goals/agm-responder-persona.md"
check "api_route_methods_match_documented_inventory ws_event_docs_tests::" "docs/API.md"
check "" "web/src/store/store.ts" "docs/SPEC.md" "scripts/ops/README.md"
check "share_tests::" "crates/am-share/src/lib.rs"
check "__all__" "crates/am-lifecycle/src/lib.rs"
check "runners::supervisor:: supervisor_tests::" "crates/am-supervisor/src/lib.rs"
check "__all__" "crates/am-supervisor/build.rs"
check "__all__" "crates/am-core/Cargo.toml"
check "share_tests::" "crates/am-share/src/lib.rs" "crates/am-share/src/share/portal.rs"
check "runners::am_base_tests::" "crates/am-base/src/lib.rs"

check "lifecycle::start::ports::" "crates/am-lifecycle/src/session_ports.rs"
check "" "crates/am-supervisor/src/supervisor/store_tests/tests_crate_only.rs"
check "lifecycle::start::tab_tests::" "crates/am-lifecycle/src/lifecycle/start_tests/tab_tests.rs"
check "lifecycle::start::resume_args_tests::" "crates/am-lifecycle/src/lifecycle/start_tests/resume_args_tests.rs"
check "lifecycle::send_now::ports::" "crates/am-lifecycle/src/send_ports.rs"
check "" "crates/am-share/src/share/budget_remote_tests.rs"
check "" "crates/am-lifecycle/src/ask_answers.rs"
check "share_tests::" "crates/am-share/src/share/svg_check.rs"
for m in health setup watchdog cli_refresh role_faults failover store responder controller idle_sleep maintenance incidents bot_requests timing; do
    check "runners::supervisor:: supervisor_tests::" "crates/am-supervisor/src/supervisor/$m.rs"
    check "supervisor_tests::$m:: testing::scratch_leak_guard:: timestamp_compat_tests::" "daemon/src/supervisor_tests/$m.rs"
done
check "runners::supervisor:: supervisor_tests::" "crates/am-supervisor/src/supervisor/mod.rs"
check "runners::supervisor:: supervisor_tests::" "crates/am-supervisor/src/supervisor/roles.rs"
check "runners::supervisor:: supervisor_tests::" "crates/am-supervisor/src/supervisor/controller.rs"
check "runners::supervisor:: supervisor_tests::" "crates/am-supervisor/src/build_info.rs"
check "supervisor_tests::build_info:: testing::scratch_leak_guard:: timestamp_compat_tests::" "daemon/src/supervisor_tests/build_info.rs"
check "api::relay_announce_tests:: api::relay_from_auth_tests:: runners::supervisor::api:: runners::supervisor::responder_api:: supervisor_tests:: testing::scratch_leak_guard:: timestamp_compat_tests::" "daemon/src/supervisor_tests/flow_fixture.rs"
check "supervisor_tests:: testing::scratch_leak_guard:: timestamp_compat_tests::" "daemon/src/supervisor_tests/mod.rs"
check "runners::supervisor:: supervisor_tests::" "crates/am-supervisor/src/supervisor/assignment_state.rs"
check "" "crates/am-supervisor/src/supervisor/test_env.rs"
check "runners::am_base_tests::attach::" "crates/am-base/src/attach.rs"
check "runners::am_base_tests::capture_claude::" "crates/am-base/src/capture.rs"
check "runners::am_base_tests::capture_claude::" "crates/am-base/src/capture/claude.daemon-tests-reply_boundary_tests.rs"
check "runners::am_base_tests::rewind:: runners::am_base_tests::rewind_anchor::" "crates/am-base/src/rewind/anchor.rs"
check "runners::am_base_tests::login_assist::" "crates/am-base/src/login_assist/tests.rs"
check "runners::am_base_tests::release_triage::" "crates/am-base/src/release_triage/tests_publish.rs"
for f in lib.rs db.rs db/schema_guard.rs shared_host.rs race_point.rs unrecognised.rs; do
    check "runners::am_base_tests::" "crates/am-base/src/$f"
done
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
mkdir -p "$NEST/daemon/src/aa/bb"
printf '#[test]\nfn t() {}\n' > "$NEST/daemon/src/aa/mod.rs"
printf 'pub fn k() {}\n' > "$NEST/daemon/src/aa/bb/c.rs"
got="$(nest_got daemon/src/aa/bb/c.rs)"
if [ "$got" != "aa:: testing::scratch_leak_guard:: timestamp_compat_tests::" ]; then
    printf 'FAIL: a/b/c.rs（a 有測試、a/b 沒有）→ got [%s] want [aa:: testing::scratch_leak_guard:: timestamp_compat_tests::]\n' "$got" >&2
    fail=1
fi

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
