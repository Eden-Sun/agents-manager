//! API bot（claude-api）後端。`runtime.rs` 是已接線的入口（lifecycle 與 HTTP 路由由它進入），
//! 其餘模組是它依賴的協議、HTTP、報價、註冊表、完成儲存與啟動復原。
//! 持久化規則：checkpoint 必須先落盤才回應；復原時進行中的 attempt／tool 標為 unknown，不重放副作用。
#![allow(dead_code)]

pub(crate) mod adapter;
pub(crate) mod anthropic;
pub(crate) mod provider;
pub(crate) mod completion;
pub(crate) mod http;
pub(crate) mod openai;
pub(crate) mod protocol;
pub(crate) mod quote;
mod pricing;
pub(crate) mod runner;
pub(crate) mod sse;
pub(crate) mod timeline;
#[cfg(test)]
mod tests;
pub(crate) mod tools;

#[cfg(test)]
mod adapter_tests;
#[cfg(test)]
mod journal_failure_tests;
#[cfg(test)]
mod wrap_up_tests;
#[cfg(test)]
mod debug_redaction_tests;
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

pub(crate) mod sweeper;
pub(crate) use sweeper::spawn_sweeper;
#[cfg(all(test, feature = "daemon-test-harness"))]
mod sweeper_tests;

mod explicit_retirement;
#[cfg(all(test, feature = "daemon-test-harness"))]
mod explicit_retirement_tests;
#[cfg(test)]
mod provider_seam_tests;

/// 測試用 tracing writer：把 log 收進記憶體，讓測試斷言「敏感值不出現在 log」（adapter／openai 的 key、leak 測試共用）。
#[cfg(test)]
#[derive(Clone, Default)]
pub(crate) struct LogSink(pub(crate) std::sync::Arc<std::sync::Mutex<Vec<u8>>>);
#[cfg(test)]
impl std::io::Write for LogSink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
#[cfg(test)]
impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogSink {
    type Writer = LogSink;
    fn make_writer(&'a self) -> LogSink {
        self.clone()
    }
}

