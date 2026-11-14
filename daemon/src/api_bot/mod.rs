//! API bot（claude-api）後端。`runtime.rs` 是已接線的入口（lifecycle 與 HTTP 路由由它進入），
//! 其餘模組是它依賴的協議、HTTP、報價、註冊表、完成儲存與啟動復原。
//! 持久化規則：checkpoint 必須先落盤才回應；復原時進行中的 attempt／tool 標為 unknown，不重放副作用。
#![allow(dead_code)]

pub(crate) mod adapter;
pub(crate) mod completion;
pub(crate) mod http;
pub(crate) mod protocol;
pub(crate) mod quote;
mod pricing;
pub(crate) mod runner;
pub(crate) mod sse;
#[cfg(test)]
mod tests;
pub(crate) mod tools;

#[cfg(test)]
mod adapter_tests;
#[cfg(all(test, feature = "daemon-test-harness"))]
mod completion_tests;
pub(crate) mod prompt;
pub(crate) mod registry;
pub(crate) mod runtime;
mod same_run;
mod native_resume;
mod native_start;
mod native_prompt;

mod receipt;
#[cfg(all(test, feature = "daemon-test-harness"))]
mod receipt_tests;

mod startup_recovery;
mod startup_history;
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

mod explicit_retirement;
#[cfg(all(test, feature = "daemon-test-harness"))]
mod explicit_retirement_tests;
