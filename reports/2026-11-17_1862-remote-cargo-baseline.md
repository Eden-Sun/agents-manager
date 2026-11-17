# #1862 第一階段：am-remote-cargo 拆分基線量測與測試清單

日期 2026-11-17。origin/main（detached worktree `../am-wt-1862`）。**只量測，未搬任何程式。**

## 環境與命令

- 本機（ubuntu，16 核）。`AGM_RUST_ENGINE=cargo` 經 bot 的 cargo shim 仍走「remote Cargo → ubuntu@127.0.0.1:22」：實際執行主機＝同一台，但在共用樹 `~/.cache/agents-manager/remote-cargo/b7b82bfc9522c5b7/shared`（與其他 worktree／bot 共用，不是 worktree 內的 target）。每次量測前 `scripts/wait-load.sh --max 40 --min-free-mem 30`。
- 量測：預熱（先 test --no-run、再 check 各一次）後，每格 3 次，append 一行 `// bench …` 註解到目標檔（每次內容不同），量牆鐘（含 shim 同步與排隊）。
- check：`cargo check -p agents-managerd --locked -v`；test：`cargo test -p agents-managerd --locked --no-run -v`。

## 牆鐘（秒）

| 改動 | check（3 次） | 中位 | test --no-run（3 次） | 中位 |
|---|---|---|---|---|
| no-op | 3.6／3.7／4.0 | 3.7 | 68.7*／4.4／4.0 | 4.0 |
| daemon/src/api.rs 一行 | 15.7／13.2／14.2 | 14.2 | 81.5／96.7／91.9 | 91.9 |
| daemon/src/api_bot/runtime.rs 一行 | 13.4／12.9／12.9 | 12.9 | 74.1／66.7／73.6 | 73.6 |

\*第一次 no-op 的 Dirty 原因：`daemon/src` mtime 在上次 build 之後被動過（共用樹同步），不是 no-op 本身的成本；穩態 no-op 約 4 秒（中位數 4.4 與 4.0 取該格中位 4.4，表中 4.0 為最小值）。

編譯單元：所有有改動的格子都**只重編 `agents-managerd`（lib）一個單元**（Compiling／Checking 行數＝1），其餘約 300 個依賴與 am-* crate 皆 Fresh；no-op 為 0 個單元。`-v` 的 Dirty 行只給 mtime 原因，不給細項。

## remote_cargo 相關程式量

`remote_cargo.rs` 3469＋`remote_cargo_hosts.rs` 641＋`tests.rs` 1368＋`guard_tests.rs` 3080＝8558 行；daemon/src 合計 162,378 行 → 約 5.3%。am-lifecycle 98,687、am-share 14,757、am-supervisor 27,259 行在 daemon test 單元仍透過 `#[path]` 再編（計畫表所述）。

## 判斷

- 熱區一行改動：check 約 13–14 秒、test 編譯約 74–92 秒，**全部花在 agents-managerd 單一單元**。remote_cargo 約佔該單元來源行數 5%，估計佔其編譯時間 ≤5–8%（check 約 1 秒、test 約 4–7 秒；這是依行數推估，非實測），且大量是測試與腳本字串，單位行數成本偏低。
- 拆出 am-remote-cargo 只讓 daemon 單元縮小這個比例，**不會讓 api.rs／runtime.rs 的改動少編任何東西**（仍重編 daemon 單元並重新連結）；收益落在量測波動（api test 81–97 秒）之內的機率高。單為效能拆它不值得；計畫自己的停止條件（拆後沒有超過波動的收益就不以效能名義合流）很可能成立。
- 若目標是降低 daemon 熱區編譯時間，較大的槓桿是 test 單元用 `#[path]` 再編進來的 am-lifecycle／am-share／am-supervisor（合計約 14 萬行）。remote-cargo 適合當低風險試點，不適合當效能理由。
- 量測限制：共用樹被其他 bot 的同步與編譯共用，波動可能偏大；只做了本機一份（未量 r7）。

## 三條 fits_in_a_one_mib_stack 測試

- `api::per_principal_auth_tests::a_controller_tick_fits_in_a_one_mib_stack`
- `api::per_principal_auth_tests::a_restart_fits_in_a_one_mib_stack`
- `api::per_principal_auth_tests::the_plain_bot_gate_path_fits_in_a_one_mib_stack`

## remote_cargo 測試完整名單（122 條，其中 guard_tests 60 條）

```
cli::tests::remote_cargo_engine_defaults_to_cargo_and_parses_blaze_explicitly: test
remote_cargo::guard_tests::a_busy_shared_target_hands_the_next_call_a_seeded_copy_instead_of_a_cold_dir: test
remote_cargo::guard_tests::a_busy_shared_target_is_waited_for_and_only_given_up_after_a_limit: test
remote_cargo::guard_tests::added_non_rust_input_rebuilds_a_compile_time_directory_listing: test
remote_cargo::guard_tests::a_dead_helper_leaves_no_running_remote_process_and_releases_the_shared_lock: test
remote_cargo::guard_tests::a_finished_call_removes_its_private_dir_and_keeps_the_shared_target: test
remote_cargo::guard_tests::a_full_pool_hands_out_whichever_copy_frees_up_first: test
remote_cargo::guard_tests::a_full_remote_overflows_to_local_without_running_cargo: test
remote_cargo::guard_tests::a_late_run_after_the_lease_ended_does_not_start_cargo: test
remote_cargo::guard_tests::an_interrupted_call_stops_the_remote_cargo_and_removes_its_dir: test
remote_cargo::guard_tests::an_interrupted_helper_reaps_the_remote_group_before_it_exits: test
remote_cargo::guard_tests::a_process_that_left_the_process_group_is_still_reaped_when_the_lease_ends: test
remote_cargo::guard_tests::a_queued_guard_whose_helper_went_away_leaves_the_queue: test
remote_cargo::guard_tests::a_queued_job_that_gives_up_leaves_no_lock_file_behind: test
remote_cargo::guard_tests::a_queue_that_never_moves_gives_up_with_exit_75: test
remote_cargo::guard_tests::a_reachable_remote_whose_cargo_fails_is_not_a_fallback: test
remote_cargo::guard_tests::a_remote_build_over_the_limit_is_aborted_and_its_remote_processes_are_reaped: test
remote_cargo::guard_tests::a_remote_build_within_the_limit_returns_its_own_result: test
remote_cargo::guard_tests::a_remote_that_refuses_the_login_falls_back_to_the_local_build_and_says_why: test
remote_cargo::guard_tests::a_shared_wait_is_capped_under_test: test
remote_cargo::guard_tests::a_stale_run_is_refused_after_the_next_lease_takes_over_the_same_dir: test
remote_cargo::guard_tests::a_zero_limit_means_no_limit: test
remote_cargo::guard_tests::cargo_target_symlinks_fail_closed_for_file_and_workspace_invalidation: test
remote_cargo::guard_tests::changing_one_crate_rechecks_only_it_and_its_dependent_crate: test
remote_cargo::guard_tests::chmod_only_rsync_change_reruns_a_cargo_build_script: test
remote_cargo::guard_tests::concurrent_calls_from_different_worktrees_never_exceed_the_remote_cap: test
remote_cargo::guard_tests::concurrent_worktrees_never_write_the_same_shared_target: test
remote_cargo::guard_tests::corrupt_source_manifest_falls_back_to_workspace_touch: test
remote_cargo::guard_tests::cross_package_directory_symlink_rebuilds_the_matching_real_cargo_artifacts: test
remote_cargo::guard_tests::different_worktrees_share_one_remote_cap: test
remote_cargo::guard_tests::directory_and_external_symlinks_force_workspace_invalidation: test
remote_cargo::guard_tests::event_write_failure_or_lock_contention_does_not_change_build_result: test
remote_cargo::guard_tests::file_touch_moves_past_equal_and_future_cargo_mtimes: test
remote_cargo::guard_tests::gc_removes_orphans_but_never_a_dir_in_use_or_outside_its_layout: test
remote_cargo::guard_tests::path_dependency_worktrees_rebuild_the_matching_real_cargo_artifacts: test
remote_cargo::guard_tests::pool_copies_are_shared_targets_for_gc: test
remote_cargo::guard_tests::remote_cargo_exclusions_are_opt_in_and_prune_old_remote_inputs: test
remote_cargo::guard_tests::remote_exclusion_pruning_refuses_symlink_ancestors: test
remote_cargo::guard_tests::rsync_retries_only_when_the_source_changes_under_it: test
remote_cargo::guard_tests::run_keeps_project_cache_overrides_inside_the_lease: test
remote_cargo::guard_tests::source_manifest_handles_newline_names_without_forging_touch_paths: test
remote_cargo::guard_tests::source_manifest_only_narrows_safe_regular_file_changes: test
remote_cargo::guard_tests::source_manifest_report_contains_only_categories_and_counts: test
remote_cargo::guard_tests::sync_touches_only_a_changed_regular_file: test
remote_cargo::guard_tests::the_guards_automatic_cap_matches_the_documented_formula: test
remote_cargo::guard_tests::the_lease_identity_is_the_callers_token_and_cleanup_only_reaps_its_own_generation: test
remote_cargo::guard_tests::the_note_names_the_host_and_which_credentials_were_used: test
remote_cargo::guard_tests::the_remote_run_starts_in_the_same_subdirectory_as_the_local_call: test
remote_cargo::guard_tests::the_remote_touch_skips_a_leftover_root_claude_tree: test
remote_cargo::guard_tests::time_spent_queueing_does_not_count_toward_the_overall_limit: test
remote_cargo::guard_tests::unchanged_sync_refuses_lease_and_target_symlinks_before_rsync: test
remote_cargo::guard_tests::web_dist_changes_do_not_mask_other_structural_control_symlink_or_unsafe_changes: test
remote_cargo::guard_tests::web_dist_fake_comment_and_raw_string_anchor_blocks_fall_back: test
remote_cargo::guard_tests::web_dist_structural_changes_require_a_regular_verified_anchor: test
remote_cargo::guard_tests::web_dist_structural_changes_use_verified_embed_anchor_and_union_safe_files: test
remote_cargo::guard_tests::web_dist_structural_sync_rebuilds_embed_contents_without_unrelated_build_script: test
remote_cargo::guard_tests::web_dist_structure_mixed_with_nonweb_regular_file_removal_is_workspace: test
remote_cargo::guard_tests::workspace_invalidation_ignores_removed_paths_and_prunes_target: test
remote_cargo::guard_tests::workspace_root_permission_changes_are_manifest_inputs: test
remote_cargo::guard_tests::workspace_touch_moves_past_equal_and_future_cargo_mtimes: test
remote_cargo::guard_tests::worktrees_share_cache_and_older_changed_sources_rebuild: test
remote_cargo::hosts::guard_backed_tests::an_old_single_host_config_takes_exactly_the_old_path: test
remote_cargo::hosts::guard_backed_tests::an_overloaded_local_host_has_no_free_slot_on_trial: test
remote_cargo::hosts::guard_backed_tests::an_unreachable_first_host_hands_the_call_to_the_next_one: test
remote_cargo::hosts::guard_backed_tests::a_queue_full_fallback_marks_the_queued_host_not_the_first_one: test
remote_cargo::hosts::guard_backed_tests::a_trial_guard_cleans_stale_unlocked_tmp_tickets_but_preserves_locked_ones: test
remote_cargo::hosts::guard_backed_tests::a_trial_guard_reports_busy_with_the_number_of_waiters: test
remote_cargo::hosts::guard_backed_tests::a_trial_on_a_full_host_does_not_seed_a_new_pool_copy_first: test
remote_cargo::hosts::guard_backed_tests::a_trial_whose_lock_file_was_recycled_reports_busy: test
remote_cargo::hosts::guard_backed_tests::the_local_executor_runs_cargo_in_the_shared_target_without_ssh: test
remote_cargo::hosts::guard_backed_tests::with_several_hosts_a_busy_first_host_hands_the_call_to_a_free_second: test
remote_cargo::hosts::tests::several_build_hosts_are_read_in_order_and_an_old_single_host_config_is_unchanged: test
remote_cargo::hosts::tests::the_local_load_cap_prefers_config_then_check_max_load_then_cores: test
remote_cargo::hosts::tests::when_every_host_is_busy_the_shortest_queue_wins_and_ties_keep_config_order: test
remote_cargo::tests::a_blank_password_uses_key_auth_and_identity_file_reaches_ssh_and_rsync: test
remote_cargo::tests::a_failure_or_crash_at_any_step_leaves_the_old_or_the_new_pair_never_a_mix: test
remote_cargo::tests::a_grandchild_holding_stderr_does_not_stall_a_finished_child: test
remote_cargo::tests::a_guard_that_answers_with_another_token_is_not_a_lease: test
remote_cargo::tests::a_hand_edited_config_with_option_or_shell_metacharacters_never_reaches_ssh: test
remote_cargo::tests::a_huge_timeout_does_not_panic_and_a_hung_ssh_is_bounded: test
remote_cargo::tests::a_lease_token_is_fixed_format_random_and_never_repeats: test
remote_cargo::tests::an_empty_argument_survives_quoting: test
remote_cargo::tests::an_orphaned_helper_stops_when_its_parent_shim_is_gone: test
remote_cargo::tests::a_probe_without_cargo_is_reported_as_a_missing_toolchain: test
remote_cargo::tests::a_queued_handshake_still_honours_a_termination_signal: test
remote_cargo::tests::a_shared_target_is_kept_as_cache_unless_the_disk_is_running_out: test
remote_cargo::tests::a_subdirectory_call_syncs_the_workspace_root_and_runs_in_the_same_subdirectory: test
remote_cargo::tests::blaze_accepts_only_safe_test_check_and_fails_closed_before_remote_work: test
remote_cargo::tests::blaze_script_uses_host_local_guarded_tools_and_propagates_fake_cargo_failure_once: test
remote_cargo::tests::cargo_command_handler_forwards_list_hosts_and_native_decline: test
remote_cargo::tests::cargo_engine_uses_the_legacy_remote_script_unchanged: test
remote_cargo::tests::every_remote_command_puts_cargo_bin_on_the_path: test
remote_cargo::tests::every_ssh_of_the_offload_gives_up_on_a_silent_peer: test
remote_cargo::tests::gc_only_ever_names_directories_in_its_own_layout: test
remote_cargo::tests::gc_picks_idle_orphans_and_never_touches_a_directory_in_use: test
remote_cargo::tests::installing_on_a_host_that_already_has_cargo_adds_clippy: test
remote_cargo::tests::macos_local_remote_command_is_parsed_by_sh_not_the_login_shell: test
remote_cargo::tests::only_cross_platform_verification_commands_are_offloaded: test
remote_cargo::tests::password_file_is_private_and_metadata_does_not_echo_the_secret: test
remote_cargo::tests::pre_1960_parser_equivalence_on_new_outcome_fixtures: test
remote_cargo::tests::read_handshake_backward_compatibility_without_outcome: test
remote_cargo::tests::read_handshake_observed_captures_queue_full_outcome_before_eof: test
remote_cargo::tests::read_handshake_with_outcome_control_line: test
remote_cargo::tests::rsync_destination_brackets_ipv6_hosts: test
remote_cargo::tests::saving_settings_keeps_clears_or_replaces_the_password_as_asked: test
remote_cargo::tests::shared_targets_are_reclaimed_by_a_configurable_age_and_by_a_count_cap_oldest_first: test
remote_cargo::tests::shell_quoting_does_not_allow_argument_injection: test
remote_cargo::tests::the_askpass_helper_is_written_once_and_is_not_world_readable: test
remote_cargo::tests::the_gc_policy_comes_from_the_remote_settings: test
remote_cargo::tests::the_handshake_skips_login_noise_and_fails_if_the_guard_dies_first: test
remote_cargo::tests::the_offload_decision_never_falls_back_silently_when_something_is_wrong: test
remote_cargo::tests::the_openssh_version_gate_compares_numbers_not_strings: test
remote_cargo::tests::the_overall_limit_defaults_to_twelve_minutes_and_is_configurable: test
remote_cargo::tests::the_probe_tells_a_missing_clippy_apart_from_a_missing_cargo: test
remote_cargo::tests::the_production_guard_script_is_byte_identical_to_before_the_loadavg_placeholder: test
remote_cargo::tests::the_remote_cap_defaults_to_what_the_hosts_cores_and_ram_can_take: test
remote_cargo::tests::the_remote_run_records_its_group_before_checking_the_lease: test
remote_cargo::tests::the_remote_test_thread_cap_defaults_to_eight_and_yields_to_the_caller: test
remote_cargo::tests::the_rsync_excludes_keep_source_dirs_named_target: test
remote_cargo::tests::without_a_data_dir_the_helper_resolves_it_from_the_config_like_the_daemon: test
remote_cargo::tests::without_sshpass_the_password_goes_through_ssh_askpass: test
runners::am_base_tests::cargo_shim::tests::macos_local_project_tests_keep_the_remote_cargo_route_even_with_blaze_requested: test
```
