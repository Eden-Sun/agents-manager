//! Supervisor composition adapters: HTTP handlers and App-backed middleware.

use crate::lifecycle::LcError;
use crate::state::App;
use axum::extract::State;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use std::sync::Arc;

pub(crate) use crate::supervisor::{
    assignment_state, bot_requests, controller, failover, health as health_core, idle_sleep, maintenance, persona, ports, remote, responder,
    roles, setup, store, timing, watchdog,
};
pub(crate) use crate::supervisor::{assign, check_assignable, lock, manager_liveness, sanitized_state, start_manager, start_requested, status_json, stop_requested};

pub mod api;
pub mod cli;
pub mod health;
pub mod responder_api;
pub mod runtime;

/// Route middleware stays at the App boundary; the lower role checks only need the DB port.
pub async fn gate_plain_bots(State(app): State<Arc<App>>, req: axum::extract::Request, next: Next) -> Response {
    match bot_requests::forbid_plain_bot(&app, req.headers()).await {
        Ok(()) => next.run(req).await,
        Err(error) => error.into_response(),
    }
}

#[allow(dead_code)]
fn _error_type(_: LcError) {}
