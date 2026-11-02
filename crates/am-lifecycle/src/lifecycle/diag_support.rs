//! 遠端診斷測試共用的紀錄器與插樁工具（grok 讀取／安裝、hook 種檔的 shell 診斷）。
//!
//! 被測腳本的每個判斷點前面會插一行 `printf 'AM_DIAG:…' >&9`，stderr 上的標記由 `code_from_marker` 還原成
//! 各自的 `Code`；這裡只負責有上限的紀錄、插樁數量檢查與 `replace_count`。只在測試建置存在。
use std::fmt::Debug;
use std::sync::{Arc, Mutex};

pub(crate) const RECORD_LIMIT: usize = 128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Record<C, S> {
    pub(crate) code: Option<C>,
    pub(crate) exit_code: Option<i32>,
    pub(crate) stdout: S,
}

#[derive(Debug)]
pub(crate) struct RecordSet<C, S> {
    pub(crate) records: Vec<Record<C, S>>,
    pub(crate) overflow: usize,
}

impl<C, S> Default for RecordSet<C, S> {
    fn default() -> Self {
        Self { records: Vec::new(), overflow: 0 }
    }
}

impl<C, S> RecordSet<C, S> {
    fn push(&mut self, record: Record<C, S>) {
        if self.records.len() < RECORD_LIMIT {
            self.records.push(record);
        } else {
            self.overflow = self.overflow.saturating_add(1);
        }
    }
}

/// 測試端的紀錄器（各測試模組的 `Handle` 就是 `Recorder<Code, StdoutKind>`）。
#[derive(Debug, Clone)]
pub(crate) struct Recorder<C, S>(Arc<Mutex<RecordSet<C, S>>>);

impl<C, S> Default for Recorder<C, S> {
    fn default() -> Self {
        Self(Arc::default())
    }
}

impl<C, S> Recorder<C, S>
where
    C: Copy + PartialEq,
    S: Copy,
{
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// 一次執行的結果：stderr 每一行若是標記就記一筆；一筆標記都沒有就記一筆 `code: None`。
    pub(crate) fn record_output(&self, status: Option<i32>, stdout: S, stderr: &[u8], code_from_marker: fn(&[u8]) -> Option<C>) {
        let mut found = false;
        let mut records = self.0.lock().expect("diagnostic record lock");
        for line in stderr.split(|byte| *byte == b'\n') {
            if let Some(code) = code_from_marker(line) {
                records.push(Record { code: Some(code), exit_code: status, stdout });
                found = true;
            }
        }
        if !found {
            records.push(Record { code: None, exit_code: status, stdout });
        }
    }

    pub(crate) fn record_code(&self, code: C, stdout: S) {
        self.0.lock().expect("diagnostic record lock").push(Record { code: Some(code), exit_code: Some(0), stdout });
    }

    pub(crate) fn contains(&self, code: C) -> bool {
        self.0.lock().expect("diagnostic record lock").records.iter().any(|record| record.code == Some(code))
    }

    pub(crate) fn summary(&self) -> String
    where
        C: Debug,
        S: Debug,
    {
        let records = self.0.lock().expect("diagnostic record lock");
        format!("records={:?}; overflow={}", records.records, records.overflow)
    }

    pub(crate) fn length_and_overflow(&self) -> (usize, usize) {
        let records = self.0.lock().expect("diagnostic record lock");
        (records.records.len(), records.overflow)
    }
}

/// 插樁失敗：哪個判斷點、預期幾處、實際找到幾處（插樁字串對不上就明確失敗，不要悄悄跑未插樁的腳本）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct InstrumentError<S> {
    pub(crate) site: S,
    pub(crate) expected: usize,
    pub(crate) found: usize,
}

impl<S: Debug> InstrumentError<S> {
    pub(crate) fn summary(self) -> String {
        format!("site={:?}; expected={}; found={}", self.site, self.expected, self.found)
    }
}

pub(crate) fn replace_count<S>(
    source: &str,
    needle: &str,
    replacement: &str,
    expected: usize,
    site: S,
) -> Result<String, InstrumentError<S>> {
    let found = source.matches(needle).count();
    if found != expected {
        return Err(InstrumentError { site, expected, found });
    }
    Ok(source.replace(needle, replacement))
}
