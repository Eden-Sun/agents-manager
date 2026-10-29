//! #1944 A/C1: inert building blocks only. No kind, route, lifecycle entry, or paid call site.
//! The future host must persist checkpoints before acknowledging them and recover pending
//! attempts/tools as unknown, never by replaying their side effects.
#![allow(dead_code)]

pub(crate) mod adapter;
pub(crate) mod completion;
pub(crate) mod http;
pub(crate) mod protocol;
pub(crate) mod quote;
pub(crate) mod runner;
#[cfg(test)]
mod tests;
pub(crate) mod tools;

#[cfg(test)]
mod adapter_tests;
#[cfg(all(test, feature = "daemon-test-harness"))]
mod completion_tests;
pub(crate) mod registry;
pub(crate) mod runtime;
mod same_run;

mod receipt;
#[cfg(all(test, feature = "daemon-test-harness"))]
mod receipt_tests;

mod startup_recovery;
mod startup_retention;
mod startup_abandonment;
#[cfg(all(test, feature = "daemon-test-harness"))]
mod startup_recovery_tests;

pub(crate) mod startup_gate;
#[cfg(all(test, feature = "daemon-test-harness"))]
mod startup_gate_tests;

mod retention;
pub(crate) use retention::{spawn_retention_ticker, schedule_retention_after_delete};
#[cfg(all(test, feature = "daemon-test-harness"))]
mod retention_tests;
