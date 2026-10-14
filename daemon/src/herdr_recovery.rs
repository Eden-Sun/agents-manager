//! Reattach top-level bots after an unexpected restart of the local herdr server.
//!
//! The event stream is the restart signal. Before the ordinary reconcile pass can end its runs,
//! verify that the server came back with no panes and reuse the native-resume path from
//! `herdr_upgrade`. A rolling circuit breaker limits repeated recovery attempts in an OOM loop.

use crate::{herdr::HerdrClient, state::App};
use futures::future::BoxFuture;
use serde::Serialize;
use std::{
    collections::VecDeque,
    sync::Arc,
    time::{Duration, Instant},
};

const ATTEMPT_WINDOW: Duration = Duration::from_secs(30 * 60);
const MAX_ATTEMPTS: usize = 3;
const BACKOFF: [Duration; MAX_ATTEMPTS] = [
    Duration::from_secs(2),
    Duration::from_secs(10),
    Duration::from_secs(30),
];

/// Lives for the lifetime of the global event watcher, so restarting herdr cannot reset its fuse.
#[derive(Default)]
pub(crate) struct RecoveryGuard {
    attempts: VecDeque<Instant>,
}

impl RecoveryGuard {
    fn delay(&mut self, now: Instant) -> Option<Duration> {
        while self
            .attempts
            .front()
            .is_some_and(|at| now.duration_since(*at) >= ATTEMPT_WINDOW)
        {
            self.attempts.pop_front();
        }
        BACKOFF.get(self.attempts.len()).copied()
    }

    fn record(&mut self, now: Instant) {
        self.attempts.push_back(now);
    }
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Resumed {
    pub bot_id: String,
    pub name: String,
    pub run_id: String,
}

#[derive(Debug, Clone, Serialize)]
pub(crate) struct Failed {
    pub bot_id: String,
    pub name: String,
    pub error: String,
}

/// Result of a confirmed full pane loss. `limited` means the fuse is open and no bot was started.
#[derive(Debug, Clone, Serialize)]
pub(crate) struct RecoveryReport {
    pub resumed: Vec<Resumed>,
    pub failed: Vec<Failed>,
    pub children_lost: Vec<crate::herdr_upgrade::LostChild>,
    pub limited: bool,
    pub skipped: Option<String>,
}

impl RecoveryReport {
    fn empty(children_lost: Vec<crate::herdr_upgrade::LostChild>) -> Self {
        Self {
            resumed: Vec::new(),
            failed: Vec::new(),
            children_lost,
            limited: false,
            skipped: None,
        }
    }
}

/// Called only after the local main event stream was established, dropped, then re-established.
/// Returning `None` means there was no evidence of a full server restart or maintenance owns it.
pub(crate) fn recover_after_reconnect<'a>(
    app: &'a Arc<App>,
    client: &'a HerdrClient,
    guard: &'a mut RecoveryGuard,
) -> BoxFuture<'a, Option<RecoveryReport>> {
    Box::pin(recover_after_reconnect_inner(app, client, guard))
}

async fn recover_after_reconnect_inner(
    app: &Arc<App>,
    client: &HerdrClient,
    guard: &mut RecoveryGuard,
) -> Option<RecoveryReport> {
    if maintenance_owns_reconnect(app).await {
        tracing::info!(
            "herdr recovery skipped: a herdr update or maintenance window owns this reconnect"
        );
        return None;
    }

    let first_plan = match crate::herdr_upgrade::affected(app, crate::config::LOCAL_HOST).await {
        Ok(plan) if !plan.resume.is_empty() || !plan.children.is_empty() => plan,
        Ok(_) => return None,
        Err(error) => {
            tracing::warn!(error = ?error, "herdr recovery could not read active bot runs");
            return None;
        }
    };
    match client.pane_list(None).await {
        Ok(panes) if panes.is_empty() => {}
        Ok(_) => return None,
        Err(error) => {
            tracing::warn!(error = ?error, "herdr recovery could not inspect panes after reconnect");
            return None;
        }
    }

    let mut report = RecoveryReport::empty(first_plan.children);
    if first_plan.resume.is_empty() {
        // There are no top-level bots to recreate; the normal reconcile pass retires the children.
        return Some(report);
    }

    let now = Instant::now();
    let Some(delay) = guard.delay(now) else {
        report.limited = true;
        tracing::error!("herdr recovery circuit open: already attempted {MAX_ATTEMPTS} recoveries in 30 minutes");
        return Some(report);
    };
    let delay = if cfg!(test) {
        Duration::from_millis(10)
    } else {
        delay
    };
    tokio::time::sleep(delay).await;

    // Recheck after backoff: an update may have opened its maintenance window or another actor
    // may have restored panes while we waited. Never race that work with a native resume.
    if maintenance_owns_reconnect(app).await {
        report.skipped = Some("maintenance_started_during_backoff".into());
        return Some(report);
    }
    match client.pane_list(None).await {
        Ok(panes) if panes.is_empty() => {}
        Ok(_) => {
            report.skipped = Some("panes_returned_during_backoff".into());
            return Some(report);
        }
        Err(error) => {
            report.skipped = Some(format!("pane_list_failed:{error}"));
            return Some(report);
        }
    }
    let plan = match crate::herdr_upgrade::affected(app, crate::config::LOCAL_HOST).await {
        Ok(plan) => plan,
        Err(error) => {
            report.skipped = Some(format!("active_runs_unavailable:{error}"));
            return Some(report);
        }
    };
    report.children_lost = plan.children;
    if plan.resume.is_empty() {
        report.skipped = Some("no_top_level_runs_remain".into());
        return Some(report);
    }

    // Count the episode before attempting the first bot; a partial batch followed by another OOM
    // must consume a strike as well.
    guard.record(Instant::now());
    tracing::warn!(
        bots = plan.resume.len(),
        children = report.children_lost.len(),
        delay_secs = delay.as_secs(),
        "herdr server restarted unexpectedly; reattaching top-level bots"
    );
    for bot in plan.resume {
        match crate::app_ports_p12::restart_bot_resume_native(app, &bot.bot_id).await {
            Ok(run_id) => report.resumed.push(Resumed {
                bot_id: bot.bot_id,
                name: bot.name,
                run_id,
            }),
            Err(error) => report.failed.push(Failed {
                bot_id: bot.bot_id,
                name: bot.name,
                error,
            }),
        }
    }
    Some(report)
}

async fn maintenance_owns_reconnect(app: &Arc<App>) -> bool {
    if crate::herdr_upgrade::is_running(app) {
        return true;
    }
    match crate::runners::herdr_maintenance::active(app).await {
        Ok(Some(_)) => true,
        Ok(None) => false,
        Err(error) => {
            tracing::warn!(error = ?error, "herdr recovery skipped: could not read maintenance state");
            true
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recovery_backoff_is_bounded_and_resets_after_the_window() {
        let start = Instant::now();
        let mut guard = RecoveryGuard::default();
        assert_eq!(guard.delay(start), Some(BACKOFF[0]));
        guard.record(start);
        assert_eq!(
            guard.delay(start + Duration::from_secs(1)),
            Some(BACKOFF[1])
        );
        guard.record(start + Duration::from_secs(1));
        assert_eq!(
            guard.delay(start + Duration::from_secs(2)),
            Some(BACKOFF[2])
        );
        guard.record(start + Duration::from_secs(2));
        assert_eq!(guard.delay(start + Duration::from_secs(3)), None);
        assert_eq!(
            guard.delay(start + ATTEMPT_WINDOW + Duration::from_secs(2)),
            Some(BACKOFF[0])
        );
    }
}
