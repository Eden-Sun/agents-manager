//! P12 Tooling/external operations seam adapter.
//!
//! Provides narrow port helpers for tooling modules so they do not directly
//! depend on lifecycle, state, or api internals.

#![allow(dead_code)]

use std::sync::Arc;
use crate::state::App;

/// Constant-time string equality helper.
pub fn ct_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

/// Checks if a run is sitting in the user's default herdr session.
pub fn in_default_session(run: &crate::db::Run) -> bool {
    run.herdr_session.as_deref() == Some("default")
}

/// Restarts a bot with native resume enabled.
pub async fn restart_bot_resume_native(app: &Arc<App>, bot_id: &str) -> Result<String, String> {
    let opts = crate::lifecycle::StartOpts { resume_native: true, ..Default::default() };
    crate::lifecycle::restart_bot_with(app, bot_id, opts)
        .await
        .map_err(|e| format!("{e:?}"))
}

/// Relays a prompt to a parent bot with retries.
pub async fn notify_parent_relayed(
    app: &Arc<App>,
    parent_bot_id: &str,
    text: &str,
    request_id: &str,
) -> Result<(), String> {
    let mut last = String::new();
    for _ in 0..12 {
        match crate::lifecycle::prompt_relayed_queueable(
            app,
            parent_bot_id,
            text,
            request_id,
            Some(crate::agent_relay::DAEMON_SENDER),
        )
        .await
        {
            Ok(_) => return Ok(()),
            Err(e) => last = format!("{e:?}"),
        }
        tokio::time::sleep(std::time::Duration::from_secs(10)).await;
    }
    Err(last)
}

/// Inserts a system notification message into a conversation.
pub async fn insert_system_message(
    app: &Arc<App>,
    conv_id: &str,
    note: &str,
) -> anyhow::Result<crate::db::Message> {
    crate::lifecycle::insert_message(app, conv_id, None, "system", note, "system", false, None).await
}

/// Reads styled pane snapshot and converts it to plain text without hints.
pub async fn read_pane_plain_text(
    client: &crate::herdr::HerdrClient,
    pane_id: &str,
    kind: &str,
) -> anyhow::Result<String> {
    let styled = crate::lifecycle::read_styled(client, pane_id, "visible", 60).await?;
    Ok(crate::lifecycle::plain_without_hints(kind, &styled))
}

/// Resolves remote bot directory via herdr connection.
pub async fn remote_bot_dir_for(
    conn: &crate::hosts::HostConn,
    bot_id: &str,
    instance: Option<&str>,
) -> anyhow::Result<String> {
    Ok(crate::lifecycle::remote_bot_dir_for(conn, bot_id, instance).await?.dir)
}

/// Retains in-memory pane typed memo for active runs.
pub fn retain_pane_typed(active_runs: &[String]) {
    crate::lifecycle::retain_pane_typed(active_runs);
}

/// Retains in-memory bot state for live bots.
pub fn retain_bot_state(live_bots: &[String]) {
    #[cfg(not(test))]
    crate::lifecycle::retain_bot_state(live_bots);
    #[cfg(test)]
    let _ = live_bots;
}

/// Emits host changed event.
pub async fn emit_host_changed(app: &Arc<App>, fence: &crate::hosts::HostFence) {
    crate::state::emit_host_changed(app, fence).await;
}

#[cfg(test)]
pub mod race_point {
    use std::future::Future;

    pub fn arm<F, Fut>(point: &'static str, key: &str, f: F)
    where
        F: FnOnce() -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        crate::lifecycle::race_point::arm(point, key, f);
    }

    pub async fn hit(point: &'static str, key: &str) {
        crate::lifecycle::race_point::hit(point, key).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ct_eq_works() {
        assert!(ct_eq("secret123", "secret123"));
        assert!(!ct_eq("secret123", "secret456"));
        assert!(!ct_eq("secret123", "secret12"));
    }

    #[tokio::test]
    async fn in_default_session_works() {
        let env = crate::testing::env().await;
        let bot = crate::testing::claude_bot(&env.app, &env.project_id, "b1").await;
        let run_id = crate::testing::fake_run(&env.app, &bot.id).await;
        let mut run = crate::db::run(&env.app.db, &run_id).await.unwrap().unwrap();
        run.herdr_session = Some("default".into());
        assert!(in_default_session(&run));
        run.herdr_session = Some("custom".into());
        assert!(!in_default_session(&run));
        run.herdr_session = None;
        assert!(!in_default_session(&run));
    }
}
