//! Unit tests for am-base modules that require daemon composition/test helpers.

use std::path::Path;
use sqlx::SqlitePool;

pub(crate) fn composition_features() -> crate::db::FeatureMigrations { crate::app_ports_p1::FEATURE_MIGRATIONS }

pub(crate) async fn check_drift(pool: &SqlitePool) -> anyhow::Result<()> {
    crate::db::schema_guard::check_drift_with(pool, composition_features()).await
}

pub(crate) async fn open_test_db(path: &Path) -> anyhow::Result<SqlitePool> {
    crate::db::open_with(path, composition_features()).await
}

pub(crate) async fn apply_migrations(pool: &SqlitePool) -> anyhow::Result<()> {
    crate::db::apply_migrations_for_test(pool, composition_features()).await
}

pub(crate) async fn migrate(pool: &SqlitePool) -> anyhow::Result<()> {
    crate::db::migrate_for_test(pool, composition_features()).await
}

pub(crate) async fn apply_migrations_failing_after_spawn_hints_drop(pool: &SqlitePool) -> anyhow::Result<()> {
    crate::db::apply_migrations_failing_after_spawn_hints_drop_for_test(pool, composition_features()).await
}

#[path = "am_base_tests/agy_support.rs"]
mod agy_support;

#[path = "am_base_tests/attach.rs"]
mod attach;

#[path = "am_base_tests/background_hook.rs"]
mod background_hook;

#[path = "am_base_tests/background_loop.rs"]
mod background_loop;

#[path = "am_base_tests/bot_trash.rs"]
mod bot_trash;

#[path = "am_base_tests/build_scheduler.rs"]
mod build_scheduler;

#[path = "am_base_tests/capabilities.rs"]
mod capabilities;

#[path = "am_base_tests/capture_claude.rs"]
mod capture_claude;

#[path = "am_base_tests/cargo_shim.rs"]
mod cargo_shim;

#[path = "am_base_tests/changelog.rs"]
mod changelog;

#[path = "am_base_tests/claude_live.rs"]
mod claude_live;

#[path = "am_base_tests/claude_mode.rs"]
mod claude_mode;

#[path = "am_base_tests/credential_spawn.rs"]
mod credential_spawn;

#[path = "am_base_tests/db.rs"]
mod db;

#[path = "am_base_tests/db_schema_guard.rs"]
mod db_schema_guard;

#[path = "am_base_tests/exec_retry.rs"]
mod exec_retry;

#[path = "am_base_tests/git_sh.rs"]
mod git_sh;

#[path = "am_base_tests/github.rs"]
mod github;

#[path = "am_base_tests/herdr.rs"]
mod herdr;

#[path = "am_base_tests/herdr_maintenance.rs"]
mod herdr_maintenance;

#[path = "am_base_tests/herdr_shim.rs"]
mod herdr_shim;

#[path = "am_base_tests/herdr_unit.rs"]
mod herdr_unit;

#[path = "am_base_tests/herdr_version.rs"]
mod herdr_version;

#[path = "am_base_tests/hook_cmd.rs"]
mod hook_cmd;

#[path = "am_base_tests/hosts.rs"]
mod hosts;

#[path = "am_base_tests/kind_probe.rs"]
mod kind_probe;

#[path = "am_base_tests/launch_rev.rs"]
mod launch_rev;

#[path = "am_base_tests/linux_proc.rs"]
mod linux_proc;

#[path = "am_base_tests/local_image.rs"]
mod local_image;

#[path = "am_base_tests/local_sh.rs"]
mod local_sh;

#[path = "am_base_tests/models.rs"]
mod models;

#[path = "am_base_tests/outbox.rs"]
mod outbox;

#[path = "am_base_tests/outbox_remote.rs"]
mod outbox_remote;

#[path = "am_base_tests/pane_identity.rs"]
mod pane_identity;

#[path = "am_base_tests/pasted_content.rs"]
mod pasted_content;

#[path = "am_base_tests/primary_keep_warm.rs"]
mod primary_keep_warm;

#[path = "am_base_tests/private_files.rs"]
mod private_files;

#[path = "am_base_tests/prompt_cache.rs"]
mod prompt_cache;

#[path = "am_base_tests/quota.rs"]
mod quota;

#[path = "am_base_tests/quota_agy.rs"]
mod quota_agy;

#[path = "am_base_tests/quota_claude.rs"]
mod quota_claude;

#[path = "am_base_tests/quota_grok.rs"]
mod quota_grok;

#[path = "am_base_tests/remote_health.rs"]
mod remote_health;

#[path = "am_base_tests/remote_purge.rs"]
mod remote_purge;

#[path = "am_base_tests/remote_trash.rs"]
mod remote_trash;

#[path = "am_base_tests/rewind_anchor.rs"]
mod rewind_anchor;

#[path = "am_base_tests/shared_host.rs"]
pub(crate) mod shared_host;

#[path = "am_base_tests/shim_refresh.rs"]
mod shim_refresh;

#[path = "am_base_tests/spawn_hints.rs"]
mod spawn_hints;

#[path = "am_base_tests/tools.rs"]
mod tools;

#[path = "am_base_tests/transcript_read.rs"]
mod transcript_read;

#[path = "am_base_tests/trust.rs"]
mod trust;

#[path = "am_base_tests/trusted_open.rs"]
mod trusted_open;

#[path = "am_base_tests/update_watch.rs"]
mod update_watch;

#[path = "am_base_tests/upstream_update.rs"]
mod upstream_update;
