//! Lifecycle and adjacent daemon domain modules extracted from agents-managerd.

// Preserve the former crate-root paths for the stable lower-level modules.
pub use am_base::*;
pub use am_core;
pub use am_ports;

mod arc_ports;

pub mod ask_answers;
pub mod background_jobs;
pub mod blocked_reason;
pub mod child_alerts;
pub mod child_done;
pub mod codex_history;
pub mod codex_live;
pub mod codex_model_migration;
pub mod daemon_notice;
pub mod default_session;
pub mod dangerous_rm;
pub mod events;
pub mod handoff;
pub mod hookrecv;
pub mod judge;
pub mod lifecycle;
pub mod ports_impl;
pub mod projection;
pub mod tui_prompts;
pub mod turn_error;
