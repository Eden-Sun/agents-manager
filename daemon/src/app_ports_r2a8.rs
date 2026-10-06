//! R2A8 App adapters for lower-layer modules.
//!
//! Provides composition helpers for prompt_suggestion, primary_keep_warm,
//! and prompt_cache so they do not directly depend on lifecycle or its p4 app_ports.

#![allow(dead_code)]

use std::sync::Arc;
use crate::state::App;
use am_ports::DbContext;

pub async fn prompt_suggestion_observe(app: &Arc<App>, run: &crate::db::Run) -> Option<bool> {
    let db = DbContext::new(app.db.clone());
    let reader = crate::lifecycle::app_ports_p4::AppHerdrPort::new(app);
    let events = crate::lifecycle::app_ports_p4::AppEventSink::new(app);
    crate::prompt_suggestion::observe_with_ports(&db, &reader, &events, run).await
}

pub async fn primary_keep_warm_sweep(app: &Arc<App>) {
    let db = DbContext::new(app.db.clone());
    let turns = crate::lifecycle::app_ports_p4::AppTurnControl::new(app);
    let messages = crate::lifecycle::app_ports_p4::AppSystemMessageWriter::new(app);
    let events = crate::lifecycle::app_ports_p4::AppEventSink::new(app);
    let clock = crate::lifecycle::app_ports_p4::AppClock;
    crate::primary_keep_warm::sweep_with(&db, &turns, &messages, &events, &clock, &app.shutdown).await;
}

pub async fn refresh_codex(app: &Arc<App>, run: &crate::db::Run) {
    let database = DbContext::new(app.db.clone());
    let rollout = crate::lifecycle::app_ports_p4::AppCodexRolloutAccess::new(app);
    let events = crate::lifecycle::app_ports_p4::AppEventSink::new(app);
    crate::prompt_cache::refresh_codex_with_ports(&database, &rollout, &events, run).await;
}
