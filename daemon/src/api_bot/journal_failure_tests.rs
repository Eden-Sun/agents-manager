//! Journal 失敗的具名原因（2026-11-19）：每個原因都有人話訊息；只有真正的寫入失敗才說「對話狀態無法寫入」；
//! 注入任一種失敗，turn 的原因都走對應的代碼（`checkpoint` 不吞掉原因）。

use super::adapter::{db_failure, SaveDiag};
use super::completion::failure_explanation;
use super::runner::{checkpoint, Error, Journal, JournalFailure, State};
use am_base::db::api_bot as db_api;
use futures::future::BoxFuture;
use std::collections::BTreeSet;

const ALL: [JournalFailure; 12] = [
    JournalFailure::CheckpointProviderMismatch,
    JournalFailure::CheckpointSerialize,
    JournalFailure::CheckpointTooLarge,
    JournalFailure::PendingStateChanged,
    JournalFailure::EncodeFailed,
    JournalFailure::InvalidQuote,
    JournalFailure::AttemptExists,
    JournalFailure::ResponseTransitionMismatch,
    JournalFailure::FenceMismatch,
    JournalFailure::StaleRevision,
    JournalFailure::Refused,
    JournalFailure::DbWriteFailed,
];

#[test]
fn every_failure_has_a_human_message_and_only_a_write_failure_says_state_unwritable() {
    for f in ALL {
        let msg = failure_explanation(f.reason());
        assert_ne!(msg.as_ref(), "原因未知。", "{f:?} 沒有對應的訊息");
        assert_eq!(msg.contains("對話狀態無法寫入"), f == JournalFailure::DbWriteFailed, "{f:?}：{msg}");
    }
    assert_eq!(JournalFailure::DbWriteFailed.reason(), "journal");
}

#[test]
fn codes_are_stable_and_distinct() {
    let codes: BTreeSet<&str> = ALL.iter().map(|f| f.code()).collect();
    assert_eq!(codes.len(), ALL.len());
    assert!(codes.contains("checkpoint_too_large") && codes.contains("db_write_failed") && codes.contains("fence_mismatch"));
}

#[test]
fn db_errors_are_classified_so_only_real_write_failures_are_db_write_failed() {
    let mut diag = SaveDiag::default();
    assert_eq!(db_failure(&mut diag, &anyhow::Error::new(db_api::StaleRevision)), JournalFailure::StaleRevision);
    assert_eq!(db_failure(&mut diag, &anyhow::Error::new(db_api::StaleFence("stale API bot owner"))), JournalFailure::FenceMismatch);
    // 外層包了 context 仍能認出來。
    let wrapped = anyhow::Error::new(db_api::StaleRevision).context("checkpoint cas");
    assert_eq!(db_failure(&mut diag, &wrapped), JournalFailure::StaleRevision);
    assert_eq!(db_failure(&mut diag, &anyhow::anyhow!("disk I/O error")), JournalFailure::DbWriteFailed);
    assert_eq!(diag.sqlite_code, None, "非 SQLite 錯誤不附代碼");
}

/// 每次 save 都回指定的失敗（模擬編碼失敗、stale revision、寫入失敗等）。
struct Failing(JournalFailure);
impl Journal for Failing {
    fn save<'a>(&'a self, _: &'a State) -> BoxFuture<'a, Result<(), JournalFailure>> {
        let f = self.0;
        Box::pin(async move { Err(f) })
    }
}

#[tokio::test]
async fn injected_failures_keep_their_reason_through_checkpoint_and_poison_the_state() {
    for f in ALL {
        let mut state = State::default();
        assert_eq!(checkpoint(&Failing(f), &mut state).await, Err(Error::Journal(f)), "{f:?}");
        assert!(state.api_pending, "{f:?}：失敗後不能再使用這份記憶體中的 transcript");
    }
}
