#!/usr/bin/env bash
set -euo pipefail

rules="${CI_RULES_FILE:-$(dirname "${BASH_SOURCE[0]}")/ci-rules.tsv}"
rule_pats=() rule_daemon=()
while IFS='|' read -r pat _ dfilt _; do
    case "$pat" in '' | '#'*) continue ;; esac
    rule_pats+=("$pat") rule_daemon+=("$dfilt")
done <"$rules"

module_has_tests() {
    local root="$1" m="$2" file
    for file in "$root/$m.rs" "$root/$m/mod.rs"; do
        [ -f "$file" ] && grep -Eq '#\[(tokio::)?test|mod [a-z_0-9]*tests?[a-z_0-9]*[[:space:]]*;' "$file" && return 0
    done
    [ -d "$root/$m" ] && grep -rEq '#\[(tokio::)?test' "$root/$m" && return 0
    return 1
}

LIFECYCLE_MOVED=" agent_md agy_session agy_tests blocked_reason claude_resume_menu codex_history codex_model_migration daemon_notice dangerous_rm default_session events pane_text paste_check screen setup prompt delivery slash owed_delivery resume_nudge start_send fence busy_send send_now resume_gate composer_draft unsent_draft suggestion queue tui_prompts turn_error judge child_alerts handoff background_jobs claude_child_log grok_transcript grok_hook poller observed_status codex_session_refresh "
emit_lifecycle_moved() {
    case "$1" in
        crates/am-lifecycle/src/judge/*.rs) printf 'lifecycle_tests::judge::%s::\n' "$(basename "${1%.rs}")"; return 0 ;;
        crates/am-lifecycle/src/lifecycle/observed_status.rs | crates/am-lifecycle/src/lifecycle/codex_session_refresh.rs)
            printf 'lifecycle_tests::poller::%s::\n' "$(basename "${1%.rs}")"; return 0 ;;
    esac
    local n
    n="$(basename "${1%.rs}")"
    case "$LIFECYCLE_MOVED" in *" $n "*) printf 'lifecycle_tests::%s::\n' "$n"; return 0 ;; esac
    n="${n%_tests}"
    case "$LIFECYCLE_MOVED" in *" $n "*) printf 'lifecycle_tests::%s::\n' "$n" ;; esac
}

emit_mod() {
    local f="$1" mod="$2" base root
    case "$f" in crates/am-lifecycle/src/*.rs) emit_lifecycle_moved "$f" ;; esac
    case "$f" in
        crates/am-lifecycle/src/*.rs | crates/am-share/src/*.rs | crates/am-supervisor/src/*.rs)
            if [ -f scripts/ci-crate-only-tests.py ] && python3 -B scripts/ci-crate-only-tests.py "$f"; then return 0; fi
            ;;
    esac
    if [ -f "$f" ]; then
        base="${f%.rs}"; base="${base%/mod}"
        root="${base%/"$mod"}"
        while ! module_has_tests "$root" "$mod"; do
            case "$mod" in */*) mod="${mod%/*}" ;; *) return 0 ;; esac
        done
    fi
    printf '%s::\n' "${mod//\//::}"
}

GUARD_RULES=(
    "timestamp_compat_tests::|daemon/src/|timestamp_compat_tests.rs *_tests.rs"
    "testing::scratch_leak_guard::|daemon/src/|testing.rs"
)

emit_guard_rules() {
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
    dfilt='@rust'
    for i in "${!rule_pats[@]}"; do
        # shellcheck disable=SC2053
        if [[ "$f" == ${rule_pats[i]} ]]; then dfilt="${rule_daemon[i]}"; break; fi
    done
    case "$dfilt" in
        -) continue ;;
        @rust) ;;
        *) tr ',' '\n' <<<"$dfilt"; continue ;;
    esac
    case "$f" in
        Cargo.toml | Cargo.lock | rust-toolchain* | .cargo/* | daemon/Cargo.toml | daemon/build.rs \
            | daemon/src/main.rs | daemon/src/lib.rs | daemon/src/testing.rs | daemon/src/test_home.rs | daemon/tests/* \
            | crates/*/Cargo.toml | crates/*/build.rs \
            | crates/am-lifecycle/src/lib.rs)
            printf '__all__\n'
            ;;
        daemon/src/child_done_tests.rs | daemon/src/runners/child_done.rs)
            printf 'child_done::tests::\n'
            ;;
        daemon/src/shell.rs) printf 'api::shell::\n' ;;
        daemon/src/bot_scope_matrix.rs) printf 'api::bot_scope_matrix::\n' ;;
        daemon/src/shell_identity_delete.rs) printf 'api::shell_identity_delete::\n' ;;
        daemon/src/app_ports_p4sess.rs) printf 'runners::s6_l::app_ports_p4sess::\n' ;;
        daemon/src/reconcile_tests/*.rs) printf 'reconcile::%s::\n' "$(basename "$f" .rs)" ;;
        daemon/src/remote_cargo_tests/*.rs) printf 'remote_cargo::%s::\n' "$(basename "$f" .rs)" ;;
        daemon/src/remote_cargo_hosts.rs) printf 'remote_cargo::hosts::\n' ;;
        daemon/src/changelog_translate_stream.rs) printf 'changelog_translate::stream::\n' ;;
        daemon/src/state_limit_hit_tests.rs) printf 'state::limit_hit_tests::\n' ;;
        daemon/src/api_bot/runtime_tests.rs) printf 'api_bot::runtime::tests::\n' ;;
        daemon/src/api_bot/startup_abandonment_tests.rs) printf 'api_bot::startup_recovery_tests::startup_abandonment_tests::\n' ;;
        crates/am-lifecycle/src/hookrecv_tests/*.rs)
            mod="${f##*/}"
            printf 'hookrecv::%s::\n' "${mod%.rs}"
            ;;
        crates/am-lifecycle/src/lifecycle/suggestion_tests.rs | crates/am-lifecycle/src/lifecycle/composer_draft_tests.rs \
            | crates/am-lifecycle/src/lifecycle/claude_resume_menu_tests.rs | crates/am-lifecycle/src/lifecycle/unsent_draft_tests.rs)
            mod="${f#crates/am-lifecycle/src/}"
            mod="${mod%_tests.rs}"
            printf '%s::tests::\n' "${mod//\//::}"
            emit_lifecycle_moved "$f"
            ;;
        crates/am-lifecycle/src/lifecycle/poller_tests/*.rs)
            mod="${f##*/}"
            printf 'lifecycle_tests::poller::%s::\n' "${mod%.rs}"
            ;;
        crates/am-lifecycle/src/lifecycle/*_tests/*.rs)
            mod="${f##*/}"
            dir="${f%/*}"
            dir="${dir##*/}"
            printf 'lifecycle::%s::%s::\n' "${dir%_tests}" "${mod%.rs}"
            ;;
        crates/am-lifecycle/src/lifecycle/codex_rollout_turn.rs) printf 'lifecycle::codex_rollout::turn_boundary::\n' ;;
        crates/am-lifecycle/src/session_ports.rs) printf 'lifecycle::start::ports::\n' ;;
        crates/am-lifecycle/src/send_ports.rs) printf 'lifecycle::send_now::ports::\n' ;;
        crates/am-lifecycle/src/lifecycle/force_abort_tests.rs) printf 'lifecycle::stop::force_abort_tests::\n' ;;
        daemon/src/supervisor_tests/flow_fixture.rs)
            printf 'supervisor_tests::\n'
            printf 'api::relay_announce_tests::\napi::relay_from_auth_tests::\nrunners::supervisor::responder_api::\nrunners::supervisor::api::\n'
            printf 'testing::scratch_leak_guard::\ntimestamp_compat_tests::\n'
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
        crates/am-share/src/*)
            case "$f" in
                *_tests.rs | */test_env.rs) if [ -f "$f" ] && python3 -B scripts/ci-crate-only-tests.py "$f"; then continue; fi ;;
            esac
            case "$f" in
                *.rs) printf 'share_tests::\n' ;;
                *) printf '__all__\n' ;;
            esac
            ;;
        crates/am-lifecycle/src/*)
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
            case "$f" in
                */test_env.rs | */store_tests/*.rs) if [ -f "$f" ] && python3 -B scripts/ci-crate-only-tests.py "$f"; then continue; fi ;;
            esac
            case "$f" in
                *.rs) printf 'supervisor_tests::\nrunners::supervisor::\n' ;;
                *) printf '__all__\n' ;;
            esac
            ;;
    esac
done | LC_ALL=C sort -u
