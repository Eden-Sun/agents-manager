use std::collections::BTreeMap;
use std::sync::Arc;
use anyhow::{anyhow, Result};
use chrono::Local;
use serde_json::{json, Value};
use crate::config::LOCAL_HOST;
use crate::quota::Quota;
use crate::quota_claude::{
    client_for_fence, cooling_down, failure_backoff, forced_target, mark_usage_seen,
    park, parse_claude_usage, parse_claude_usage_report, plan_targets, probe_command,
    probe_label, record_probe_identity, run_probe_pane, skip_usage_probe, sweep_stale,
    unnamed_claude_running, unpark, usage_fresh, PaneRun, PlanInput,
    ProbeEvidence, ProbeOutcome, CLAUDE_POLL, PROBE_TIMEOUT,
};
pub use crate::quota_claude::ForceProbeError;
use crate::state::App;

/// `Ok(None)` = claude not installed. Caller must hold [`crate::quota::probe_lock`] for that host.
pub(crate) async fn refresh_claude_account(
    app: &Arc<App>,
    host: &str,
    home: &str,
    base_key: &str,
    account: Option<&str>,
    env: BTreeMap<String, String>,
    with_usage: bool,
    fence: &crate::hosts::HostFence,
) -> Result<Option<ProbeOutcome>> {
    if !app.hosts.is_current(fence).await {
        anyhow::bail!("host `{host}` was superseded before the Claude probe started");
    }
    if !app.tools.lock().await.contains_key(host) {
        crate::tools::detect(app, host).await?;
    }
    if !app.hosts.is_current(fence).await {
        anyhow::bail!("host `{host}` was superseded during Claude tool detection");
    }
    let Some(bin) = crate::tools::cached_path(app, host, "claude").await else {
        return Ok(None);
    };

    let client = client_for_fence(host, fence).await?;
    if !app.hosts.is_current(fence).await {
        anyhow::bail!("host `{host}` was superseded before the Claude probe started");
    }
    // A sane cwd keeps the session's project files out of `/`.
    let cwd = if host == LOCAL_HOST {
        std::env::current_dir().ok().map(|p| p.display().to_string()).unwrap_or_else(|| home.to_string())
    } else {
        home.to_string()
    };
    let label = probe_label(app.as_ref(), host, account).await;
    let env_json = Value::Object(env.iter().map(|(k, v)| (k.clone(), json!(v))).collect());
    let cmd = probe_command(&bin, &env, with_usage);
    let (auth, usage) = match run_probe_pane(&client, &cwd, &label, env_json, &cmd, PROBE_TIMEOUT).await? {
        PaneRun::Done(auth, usage) => (auth, usage),
        PaneRun::TimedOut(last) => {
            return Err(anyhow!("claude probe on {host} did not finish within {PROBE_TIMEOUT:?}; screen:\n{}", last.trim()))
        }
    };
    // 探測幾十秒，期間同名主機可能已換成另一台（#347）：整個結果作廢，不回登入狀態也不寫額度。
    if !app.hosts.is_current(fence).await {
        return Err(anyhow!("host `{host}` was reconnected/reconfigured during the claude probe; stale result discarded"));
    }
    let (logged_in, email, plan) = crate::tools::read_login_answer("claude", &auth);
    let mut out = ProbeOutcome { logged_in, email, plan: plan.clone(), quota: None };
    if !with_usage {
        return Ok(Some(out));
    }
    let parsed = parse_claude_usage_report(&usage, account).or_else(|| parse_claude_usage(&usage, Local::now(), account));
    if let Some(mut q) = parsed {
        q.plan = plan;
        crate::quota::set_fenced(app, host, base_key, q.clone(), fence).await?;
        out.quota = Some(q);
    } else {
        tracing::debug!(host, account = ?account, logged_in = ?out.logged_in, usage = %usage.trim(), "claude `/usage` reported no plan lines");
    }
    Ok(Some(out))
}

pub async fn refresh_claude(app: &Arc<App>, host: &str) -> Result<bool> {
    let _guard = crate::quota::probe_lock(host).await;
    let fence = app.hosts.fence(host).await.ok_or_else(|| anyhow!("unknown host `{host}`"))?;
    // `~` expands against the *probed* host's home, not the daemon's.
    let home = crate::hosts::home_for_fence(&fence).await.map_err(|e| {
        tracing::warn!(host, error = %e, "skipping Claude quota probe because the host HOME is unreadable");
        e
    })?;
    // `ccN` are per host (SPEC §16): cc1 on m4p is a different account than here.
    let identities = crate::tools::identities_for_host(app, host).await;
    let logins = app.tools.lock().await.get(host).map(|t| t.identities.clone()).unwrap_or_default();
    // DB-backed so a daemon restart doesn't lose the proof.
    let live = crate::db::live_identities_on_host(&app.db, host).await.unwrap_or_default();
    // 停用的身份不上額度條（使用者 2026-09-16），所以也不必再花探測去問它。
    let off = crate::db::disabled_identities(&app.db, host, "claude").await.unwrap_or_default();
    let unnamed_running = unnamed_claude_running(app.as_ref(), host).await;
    let statusline_keys: std::collections::BTreeSet<String> =
        app.quotas.lock().await.iter().filter(|(_, q)| q.source == "statusline").map(|(k, _)| k.clone()).collect();
    if !app.hosts.is_current(&fence).await {
        anyhow::bail!("host `{host}` was superseded while preparing the Claude quota probe");
    }
    let targets = plan_targets(
        &PlanInput { host, home: &home, identities: &identities, logins: &logins, live: &live, off: &off, unnamed_running, statusline_keys: &statusline_keys },
        cooling_down,
    );

    let mut any = false;
    let mut saw_missing = false;
    let mut touched = false;
    for t in targets {
        if !app.hosts.is_current(&fence).await {
            anyhow::bail!("host `{host}` was superseded during the Claude quota refresh");
        }
        let full = crate::quota::quota_key(host, &t.key);
        // `/usage` answered within USAGE_REFRESH = skip (many `ccN` per host, SPEC §16, would queue
        // probes), unless we've never had a login answer for it — that rides on the probe.
        let login_known = t.names.iter().all(|n| logins.get(n).is_some_and(|i| i.logged_in.is_some()));
        if t.login_only && login_known {
            continue;
        }
        if skip_usage_probe(login_known, usage_fresh(&full)) {
            any = true;
            continue;
        }
        let probe = refresh_claude_account(app, host, &home, &t.key, t.account.as_deref(), t.env, !t.login_only, &fence).await;
        if !app.hosts.is_current(&fence).await {
            anyhow::bail!("host `{host}` was superseded before the Claude quota result could be applied");
        }
        match probe {
            Ok(None) => saw_missing = true,
            Ok(Some(o)) => {
                for name in &t.names {
                    touched |= record_probe_identity(app, host, name, &o, &fence).await;
                }
                if !app.hosts.is_current(&fence).await {
                    anyhow::bail!("host `{host}` was superseded while recording Claude identity state");
                }
                if t.login_only {
                    if o.logged_in.is_some() {
                        unpark(&full);
                    } else {
                        park(&full, false);
                    }
                } else if o.quota.is_some() {
                    any = true;
                    unpark(&full);
                    // 這把 key 的 `/usage` 剛答過：接下來十分鐘不用再開 pane（有 bot 講話時狀態列補 5h/7d）。
                    mark_usage_seen(&full);
                } else {
                    // No plan lines: park it using the login answer we just got.
                    let logged_out = o.logged_in == Some(false);
                    park(&full, logged_out);
                    let retry = failure_backoff(ProbeEvidence { cli_says_logged_out: logged_out, ..t.evidence });
                    tracing::info!(host, key = %t.key, logged_in = ?o.logged_in, retry_in_s = retry.as_secs(), "claude reported no plan lines; parking this account");
                }
            }
            Err(e) => {
                park(&full, t.evidence.cli_says_logged_out);
                let retry = failure_backoff(t.evidence);
                tracing::warn!(host, key = %t.key, error = %e, retry_in_s = retry.as_secs(), "claude quota probe failed; parking this account");
            }
        }
    }
    if !app.hosts.is_current(&fence).await {
        anyhow::bail!("host `{host}` was superseded before the Claude quota refresh completed");
    }
    if touched {
        crate::state::emit_host_changed(app, &fence).await;
    }
    if saw_missing && !any {
        return Ok(false);
    }
    Ok(any || !saw_missing)
}

pub async fn force_probe(app: &Arc<App>, host: &str, account: Option<&str>) -> Result<(String, Quota), ForceProbeError> {
    let _guard = crate::quota::probe_lock(host).await;
    let fence = app.hosts.fence(host).await.ok_or_else(|| ForceProbeError::Failed(format!("unknown host `{host}`")))?;
    let home = crate::hosts::home_for_fence(&fence).await.map_err(|e| {
        tracing::warn!(host, error = %e, "skipping forced Claude quota probe because the host HOME is unreadable");
        ForceProbeError::Failed(e.to_string())
    })?;
    let identities = crate::tools::identities_for_host(app, host).await;
    let (logins, live, statusline_keys) = (BTreeMap::new(), Default::default(), Default::default());
    let input = PlanInput { host, home: &home, identities: &identities, logins: &logins, live: &live, off: &[], unnamed_running: true, statusline_keys: &statusline_keys };
    let t = forced_target(plan_targets(&input, |_, _, _| false), account).ok_or(ForceProbeError::UnknownAccount)?;
    let full = crate::quota::quota_key(host, &t.key);
    if !app.hosts.is_current(&fence).await {
        return Err(ForceProbeError::Failed(format!("host `{host}` was superseded while preparing the Claude quota probe")));
    }
    let o = match refresh_claude_account(app, host, &home, &t.key, t.account.as_deref(), t.env, true, &fence).await {
        Ok(Some(o)) => o,
        Ok(None) => return Err(ForceProbeError::NotInstalled),
        Err(e) => return Err(ForceProbeError::Failed(e.to_string())),
    };
    let mut touched = false;
    for name in &t.names {
        touched |= record_probe_identity(app, host, name, &o, &fence).await;
    }
    if !app.hosts.is_current(&fence).await {
        return Err(ForceProbeError::Failed(format!("host `{host}` was superseded before the Claude quota result could be applied")));
    }
    if touched {
        crate::state::emit_host_changed(app, &fence).await;
    }
    let Some(q) = o.quota else {
        return Err(ForceProbeError::Failed(format!("claude `/usage` reported no plan lines (logged_in = {:?})", o.logged_in)));
    };
    unpark(&full);
    mark_usage_seen(&full);
    let stored = app.quotas.lock().await.get(&full).cloned().unwrap_or(q);
    Ok((full, stored))
}

pub fn spawn_claude_poller(app: Arc<App>) {
    crate::background_loop::spawn_restartable(&app, "claude quota poller", {
        let app = app.clone();
        move || {
            let app = app.clone();
            async move {
        let shutdown = app.shutdown.clone();
        sweep_stale(app.as_ref()).await;
        loop {
            crate::quota::for_each_host(crate::quota::pollable_hosts(&app).await, |host| {
                let app = app.clone();
                async move {
                    match refresh_claude(&app, &host).await {
                        Ok(true) => {}
                        Ok(false) => tracing::info!(host = %host, "claude not installed; claude quota stays null"),
                        Err(e) => tracing::warn!(host = %host, error = %e, "claude quota refresh failed"),
                    }
                }
            })
            .await;
            tokio::select! {
                _ = shutdown.cancelled() => return,
                _ = tokio::time::sleep(CLAUDE_POLL) => {}
            }
        }
            }
        }
    });
}
