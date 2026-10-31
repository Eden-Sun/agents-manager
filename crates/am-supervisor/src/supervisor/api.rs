//! Supervisor domain rules shared by the daemon adapters.

#[cfg(all(test, feature = "daemon-test-harness"))]
pub use crate::runners::supervisor::api::*;
