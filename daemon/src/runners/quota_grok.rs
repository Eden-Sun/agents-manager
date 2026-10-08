//! `quota_grok` runner 與背景額度輪詢。

use crate::quota_grok::{
    client_for_fence, cooling_down, name_lost, parse_probe_screen, park, probe_label,
    should_probe_grok, start_when_shell_ready, sweep_stale, wait_for_grok_pane, Probe,
    DIALOG_TIMEOUT, GROK_POLL, RETRY_AFTER_FAILURE,
};
use crate::state::App;
use anyhow::{anyhow, bail, Result};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

/// `Ok(false)` = grok is not installed there (quota stays null).
pub async fn refresh_grok(app: &Arc<App>, host: &str) -> Result<bool> {
    let _guard = crate::quota::probe_lock(host).await;
    let fence = app.hosts.fence(host).await.ok_or_else(|| anyhow!("unknown host `{host}`"))?;
    let cwd = crate::hosts::home_for_fence(&fence).await?;
    if !app.hosts.is_current(&fence).await {
        bail!("host `{host}` changed while resolving HOME");
    }
    // The start-up poller can beat detection; empty cache = unknown, not missing.
    if !app.tools.lock().await.contains_key(host) {
        crate::tools::detect(app, host).await?;
    }
    if !app.hosts.is_current(&fence).await {
        bail!("host `{host}` changed before its Grok quota probe");
    }
    if crate::tools::cached_path(app, host, "grok").await.is_none() {
        return Ok(false);
    }
    let client = client_for_fence(app, &fence).await?;
    let (ws, pane) = client.workspace_create(&cwd, &probe_label(app, host).await, json!({})).await?;
    let probe = Probe::new(client.clone(), ws.workspace_id.clone());
    let pane_id = pane.pane_id.clone();

    // A distinct name so reconcile and the bot list can never confuse it with a real bot.
    let name = format!("amquota{}", ulid::Ulid::new().to_string()[20..].to_ascii_lowercase());
    // grok 1.0.41 起 herdr 常在啟動當下就把名字從 pane 上拿掉（`agent_name_not_found`：named agent … no longer owns
    // the target terminal），之後用名字 `agent_wait` 回 `agent_not_running`——grok 其實好好開著，額度卻從 09-23 起
    // 大多數輪都讀不到（2026-09-28 使用者：「grok children 跑了一陣子，usage 沒更新」）。探測只需要 pane：
    // 名字掉了不算失敗，改看 pane 自己的 agent／狀態。
    if let Err(e) = start_when_shell_ready(&client, &name, &pane_id).await {
        if !name_lost(&e) {
            return Err(e);
        }
        tracing::debug!(host, pane = %pane_id, "grok probe: herdr dropped the agent name at start; waiting on the pane instead");
    }
    wait_for_grok_pane(&client, &pane_id, Duration::from_secs(60)).await?;
    // The TUI accepts a slash command only once its input line is drawn.
    tokio::time::sleep(Duration::from_secs(3)).await;
    client.pane_send_text(&pane_id, "/usage").await?;
    tokio::time::sleep(Duration::from_millis(800)).await;
    client.pane_send_keys(&pane_id, &["Enter"]).await?;

    let deadline = tokio::time::Instant::now() + DIALOG_TIMEOUT;
    let mut last = String::new();
    while tokio::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(900)).await;
        last = client.pane_read(&pane_id, "visible", 120).await.map(|r| r.text).unwrap_or_default();
        if let Some(q) = parse_probe_screen(app, host, &last, chrono::Utc::now()).await {
            let published = crate::quota::set_fenced(app, host, "grok", q, &fence).await;
            drop(probe);
            published?;
            return Ok(true);
        }
    }
    drop(probe);
    tracing::debug!(host, screen = %last, "grok /usage did not render a limit row");
    Err(anyhow!("grok `/usage` on {host} did not report a limit within {DIALOG_TIMEOUT:?}"))
}

/// `None` = skipped this cycle. `GET /api/quota?refresh=1` bypasses this and always really probes.
pub async fn refresh_grok_if_due(app: &Arc<App>, host: &str) -> Result<Option<bool>> {
    let key = crate::quota::quota_key(host, "grok");
    let logged_in = app.tools.lock().await.get(host).and_then(|t| t.tools.get("grok")).and_then(|t| t.logged_in);
    if !should_probe_grok(logged_in, cooling_down(&key)) {
        return Ok(None);
    }
    match refresh_grok(app, host).await {
        Ok(v) => Ok(Some(v)),
        Err(e) => {
            park(&key, RETRY_AFTER_FAILURE);
            Err(e)
        }
    }
}

pub fn spawn_grok_poller(app: Arc<App>) {
    crate::background_loop::spawn_restartable(&app, "grok quota poller", {
        let app = app.clone();
        move || {
            let app = app.clone();
            async move {
        let shutdown = app.shutdown.clone();
        sweep_stale(&app).await;
        loop {
            crate::quota::for_each_host(crate::quota::pollable_hosts(&app).await, |host| {
                let app = app.clone();
                async move {
                    match refresh_grok_if_due(&app, &host).await {
                        Ok(Some(true)) => {}
                        Ok(Some(false)) => tracing::info!(host = %host, "grok not installed; grok quota stays null"),
                        Ok(None) => tracing::debug!(host = %host, "grok probe skipped (logged out or cooling down)"),
                        Err(e) => tracing::warn!(host = %host, error = %e, retry_in_s = RETRY_AFTER_FAILURE.as_secs(), "grok quota refresh failed; parking this host"),
                    }
                }
            })
            .await;
            tokio::select! {
                _ = shutdown.cancelled() => return,
                _ = tokio::time::sleep(GROK_POLL) => {}
            }
        }
            }
        }
    });
}
