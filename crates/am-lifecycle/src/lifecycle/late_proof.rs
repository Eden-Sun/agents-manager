//! 同步窗口判不出、但框已空的送達（`not_proven_submitted`，issue #1479）：背景繼續看證據。
//!
//! `confirm_submitted` 只看 `SUBMIT_CHECKS × SUBMIT_SETTLE_MS` 這一小段。字已經打進去、Enter 按了、框是空的，
//! 證據卻還沒長出來時，以前就此記成 `delivery=unknown`＋`in_flight`：網頁鎖住輸入框、叫使用者「放棄該回合」，
//! 而 bot 其實收到了、正在做——要等 Stop hook 才解開（遠端 claude 的回音列補白 bug 之外，transcript 晚一步 flush、
//! 回音被 spinner 擠出視窗也是同一種情形）。
//!
//! 這裡在寫完 `unknown` 之後接手，每隔 [`CHECK_EVERY`] 看一次，最多 [`CHECKS`] 次：
//! - 證據長出來（回音列／transcript 比基準多了這一則）→ `Submitted`：`ok`＋已驗證；
//! - 框仍是空的、而且 agent 明顯在做事（hook 有動靜、畫面有進行中的 spinner、herdr 說 working）→ `Unverified`：
//!   `ok`＋未驗證，跟「框空了但沒有無損證據」那條路同一個結果（不自動重送）；
//! - 兩種都沒有就維持 `unknown`：框是空的但 bot 沒動、也沒有證據，不放寬。
//!
//! 升級只用 CAS（`status='in_flight' AND delivery='unknown'`）：Stop hook、放棄、run 結束先到的，這裡什麼都不寫。
//! 基準（回音列數、transcript offset）只有打字的那一刻知道，所以 `confirm_submitted` 在回 `Unproven` 前記在
//! 這裡（以 run 為鍵、很快過期），呼叫端寫完 `unknown` 再用 [`arm`] 認領。

use super::delivery::{box_state_within, evidence, read_composer, same_session, typed_tail, BoxState, Delivered, Proof, SessionMatch};
use super::*;
use std::collections::{HashMap, HashSet};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// 每隔多久看一次。
pub const CHECK_EVERY: Duration = Duration::from_secs(3);
/// 最多看幾次（約三分鐘）：之後留給 Stop hook 與 stuck_turns。
pub const CHECKS: u32 = 60;
/// 記下來之後多久內要被認領；過了就當是別一則的殘影。
const CLAIM_WITHIN: Duration = Duration::from_secs(60);
/// Registry entries are deliberately bounded even if many runs fail their unknown write at once.
const MAX_PENDING: usize = 256;
/// A durable watcher survives a restart for a short window; expired evidence must not pin prompt text.
const DURABLE_TTL: Duration = Duration::from_secs(10 * 60);
/// How often the daemon prunes abandoned registry rows and in-memory prompts.
pub const SWEEP_EVERY: Duration = Duration::from_secs(15);

/// 打字那一刻的證據基準，加上之後讀畫面要用的東西。
#[derive(Debug, Clone)]
pub struct Pending {
    pub kind: String,
    pub pane: String,
    pub proof: Proof,
    pub offset: u64,
    pub baseline: usize,
    pub text: String,
    /// 送出鍵按下去的時刻（`db::now()` 格式）：hook 動靜從這之後算起，不算排隊期間別人的事件。
    pub since: String,
    noted: Instant,
    /// 這筆基準屬於哪個回合（`persist_pending_at` 查到並綁上）；欠帳結清只認領綁同一回合的項目。
    bound_turn: Option<String>,
}

impl Pending {
    pub fn new(kind: &str, pane: &str, proof: &Proof, offset: u64, baseline: usize, text: &str, since: String) -> Pending {
        Self::new_at(kind, pane, proof, offset, baseline, text, since, Instant::now())
    }

    pub(crate) fn new_at(kind: &str, pane: &str, proof: &Proof, offset: u64, baseline: usize, text: &str, since: String, noted: Instant) -> Pending {
        Pending {
            kind: kind.to_string(),
            pane: pane.to_string(),
            proof: proof.clone(),
            offset,
            baseline,
            text: text.to_string(),
            since,
            noted,
            bound_turn: None,
        }
    }
}

#[derive(Default)]
struct PendingRegistry {
    by_run: HashMap<String, Pending>,
}

impl PendingRegistry {
    fn insert_at(&mut self, run_id: String, pending: Pending, now: Instant) -> bool {
        self.prune_expired(now);
        let evicted = if !self.contains(&run_id) && self.by_run.len() >= MAX_PENDING {
            let oldest = self.by_run.iter().min_by_key(|(_, p)| p.noted).map(|(id, _)| id.clone());
            oldest.is_some_and(|id| self.by_run.remove(&id).is_some())
        } else {
            false
        };
        self.by_run.insert(run_id, pending);
        evicted
    }

    fn remove(&mut self, run_id: &str) -> Option<Pending> {
        self.by_run.remove(run_id)
    }

    fn sweep_at(&mut self, now: Instant, active_runs: &HashSet<String>) -> usize {
        let before = self.by_run.len();
        self.by_run.retain(|run_id, p| active_runs.contains(run_id) && now.saturating_duration_since(p.noted) <= CLAIM_WITHIN);
        before - self.by_run.len()
    }

    fn prune_expired(&mut self, now: Instant) -> usize {
        let before = self.by_run.len();
        self.by_run.retain(|_, p| now.saturating_duration_since(p.noted) <= CLAIM_WITHIN);
        before - self.by_run.len()
    }

    fn take(&mut self, run_id: &str, now: Instant) -> Option<Pending> {
        let p = self.remove(run_id)?;
        (now.saturating_duration_since(p.noted) <= CLAIM_WITHIN).then_some(p)
    }

    fn bind_turn(&mut self, run_id: &str, turn_id: &str) {
        if let Some(p) = self.by_run.get_mut(run_id) {
            p.bound_turn = Some(turn_id.to_string());
        }
    }

    /// 欠帳結清只認領綁到同一回合的項目；別的回合（或沒綁）的留著、不動，也不升級。
    fn take_for_turn(&mut self, run_id: &str, turn_id: &str) -> Option<Pending> {
        if self.by_run.get(run_id)?.bound_turn.as_deref() != Some(turn_id) {
            return None;
        }
        self.remove(run_id)
    }

    fn contains(&self, run_id: &str) -> bool {
        self.by_run.contains_key(run_id)
    }

    fn len(&self) -> usize {
        self.by_run.len()
    }
}

fn registry() -> &'static Mutex<PendingRegistry> {
    static M: OnceLock<Mutex<PendingRegistry>> = OnceLock::new();
    M.get_or_init(Default::default)
}

/// 這個 run 剛判不出來的那一則（`confirm_submitted` 呼叫）。同一個 run 只留最新的一筆。
pub fn note(run_id: &str, p: Pending) {
    let now = Instant::now();
    let mut registry = registry().lock().unwrap_or_else(|e| e.into_inner());
    let removed_expired = registry.prune_expired(now);
    let evicted_oldest = registry.insert_at(run_id.to_string(), p, now);
    if removed_expired > 0 || evicted_oldest {
        tracing::debug!(removed_expired, evicted_oldest, pending = registry.len(), "late proof memory registry pruned");
    }
}

/// 把登記的基準綁到它所屬的回合（持久化時查到的那一回合）。
pub fn bind_turn(run_id: &str, turn_id: &str) {
    registry().lock().unwrap_or_else(|e| e.into_inner()).bind_turn(run_id, turn_id);
}

/// Current number of not-yet-claimed in-memory prompts, exposed for diagnostics.
pub fn pending_count() -> usize {
    registry().lock().unwrap_or_else(|e| e.into_inner()).len()
}

/// 保存 watcher 的證據基準，讓 daemon 重啟後能接回尚未結束的 unknown turn。
pub async fn persist_pending(pool: &sqlx::SqlitePool, run_id: &str, p: &Pending) -> anyhow::Result<()> {
    persist_pending_at(pool, run_id, p, &db::now()).await
}

/// 用注入的時間保存資料，測試不需等待牆鐘。
pub async fn persist_pending_at(pool: &sqlx::SqlitePool, run_id: &str, p: &Pending, now: &str) -> anyhow::Result<()> {
    use anyhow::Context;

    let turn_id = sqlx::query_scalar::<_, String>(
        "SELECT id FROM turns WHERE run_id = ? AND status = 'in_flight' AND delivery IN ('pending','unknown','ok') ORDER BY created_at DESC LIMIT 1",
    )
    .bind(run_id)
    .fetch_optional(pool)
    .await?
    .ok_or_else(|| anyhow::anyhow!("late proof has no in-flight turn for run {run_id}"))?;
    let expires_at = chrono::DateTime::parse_from_rfc3339(now)
        .context("parse late proof created_at")?
        .with_timezone(&chrono::Utc)
        .checked_add_signed(chrono::Duration::from_std(DURABLE_TTL)?)
        .ok_or_else(|| anyhow::anyhow!("late proof expiry overflow"))?
        .to_rfc3339_opts(chrono::SecondsFormat::Millis, true);
    let (proof_kind, proof_format, proof_path, proof_session_id) = match &p.proof {
        Proof::EchoRow => ("echo_row", None, None, None),
        Proof::Unverified => ("unverified", None, None, None),
        Proof::Transcript { format, path, session_id } => (
            "transcript",
            Some(match format {
                super::delivery::LogFormat::Claude => "claude",
                super::delivery::LogFormat::Codex => "codex",
                super::delivery::LogFormat::Agy => "agy",
            }),
            Some(path.to_string_lossy().into_owned()),
            Some(session_id.as_str()),
        ),
    };
    prune_durable(pool, now).await?;
    sqlx::query(
        "INSERT INTO late_proof_pending
             (turn_id, run_id, kind, pane, proof_kind, proof_format, proof_path, proof_session_id,
              proof_offset, baseline, prompt_text, since, created_at, expires_at)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT(run_id) DO UPDATE SET
             turn_id=excluded.turn_id, kind=excluded.kind, pane=excluded.pane, proof_kind=excluded.proof_kind,
             proof_format=excluded.proof_format, proof_path=excluded.proof_path,
             proof_session_id=excluded.proof_session_id, proof_offset=excluded.proof_offset,
             baseline=excluded.baseline, prompt_text=excluded.prompt_text, since=excluded.since,
             created_at=excluded.created_at, expires_at=excluded.expires_at",
    )
    .bind(turn_id.clone())
    .bind(run_id)
    .bind(&p.kind)
    .bind(&p.pane)
    .bind(proof_kind)
    .bind(proof_format)
    .bind(proof_path)
    .bind(proof_session_id)
    .bind(p.offset.min(i64::MAX as u64) as i64)
    .bind(p.baseline.min(i64::MAX as usize) as i64)
    .bind(&p.text)
    .bind(&p.since)
    .bind(now)
    .bind(expires_at)
    .execute(pool)
    .await?;
    bind_turn(run_id, &turn_id);
    trim_durable(pool).await?;
    Ok(())
}

/// 用注入的時間做啟動恢復；正式啟動透過 [`recover_at_startup`] 使用目前時間。
pub async fn recover_at_startup_at(app: &(impl LcHost + Clone + 'static), now: &str) -> anyhow::Result<usize> {
    prune_durable(app.db(), now).await?;
    let rows: Vec<(
        String, String, String, String, String, String, Option<String>, Option<String>, Option<String>, i64, i64, String, String,
    )> = sqlx::query_as(
        "SELECT p.turn_id, p.run_id, r.bot_id, p.kind, p.pane, p.proof_kind,
                p.proof_format, p.proof_path, p.proof_session_id, p.proof_offset, p.baseline, p.prompt_text, p.since
           FROM late_proof_pending p
           JOIN runs r ON r.id = p.run_id
           JOIN turns t ON t.id = p.turn_id AND t.run_id = p.run_id
          WHERE p.expires_at > ? AND r.state = 'running' AND r.pane_id = p.pane
            AND t.status = 'in_flight' AND t.delivery = 'unknown'
          ORDER BY p.created_at DESC
          LIMIT ?",
    )
    .bind(now)
    .bind(MAX_PENDING as i64)
    .fetch_all(app.db())
    .await?;
    let mut restored = 0;
    for (turn_id, run_id, bot_id, kind, pane, proof_kind, proof_format, proof_path, proof_session_id, offset, baseline, text, since) in rows {
        let proof = match proof_kind.as_str() {
            "echo_row" => Proof::EchoRow,
            "unverified" => Proof::Unverified,
            "transcript" => {
                let Some((format, path, session_id)) = proof_format.zip(proof_path).zip(proof_session_id).map(|((f, p), s)| (f, p, s)) else {
                    tracing::warn!(run = %run_id, turn = %turn_id, "discarding incomplete durable late proof");
                    delete_durable_for_turn(app.db(), &turn_id).await?;
                    continue;
                };
                let format = match format.as_str() {
                    "claude" => super::delivery::LogFormat::Claude,
                    "codex" => super::delivery::LogFormat::Codex,
                    "agy" => super::delivery::LogFormat::Agy,
                    _ => {
                        tracing::warn!(run = %run_id, turn = %turn_id, "discarding durable late proof with unknown transcript format");
                        delete_durable_for_turn(app.db(), &turn_id).await?;
                        continue;
                    }
                };
                Proof::Transcript { format, path: std::path::PathBuf::from(path), session_id }
            }
            _ => {
                tracing::warn!(run = %run_id, turn = %turn_id, "discarding durable late proof with unknown proof kind");
                delete_durable_for_turn(app.db(), &turn_id).await?;
                continue;
            }
        };
        let pending = Pending::new(&kind, &pane, &proof, offset.max(0) as u64, baseline.max(0) as usize, &text, since);
        arm_pending(app, &run_id, &bot_id, &turn_id, pending);
        restored += 1;
    }
    Ok(restored)
}

/// 找回 DB 裡仍有效、且對應 `in_flight/unknown` turn 的補證工作。
pub async fn recover_at_startup(app: &(impl LcHost + Clone + 'static)) -> anyhow::Result<usize> {
    recover_at_startup_at(app, &db::now()).await
}

async fn prune_durable(pool: &sqlx::SqlitePool, now: &str) -> anyhow::Result<u64> {
    let removed = sqlx::query(
        "DELETE FROM late_proof_pending AS p
          WHERE p.expires_at <= ?
             OR NOT EXISTS (
                 SELECT 1 FROM turns t JOIN runs r ON r.id = p.run_id
                  WHERE t.id = p.turn_id AND t.run_id = p.run_id
                    AND t.status = 'in_flight' AND t.delivery IN ('pending','unknown','ok')
                    AND r.state = 'running' AND r.pane_id = p.pane
             )",
    )
    .bind(now)
    .execute(pool)
    .await?
    .rows_affected();
    Ok(removed)
}

async fn trim_durable(pool: &sqlx::SqlitePool) -> anyhow::Result<()> {
    sqlx::query(
        "DELETE FROM late_proof_pending WHERE turn_id IN (
             SELECT turn_id FROM late_proof_pending ORDER BY created_at DESC, turn_id DESC LIMIT -1 OFFSET ?
         )",
    )
    .bind(MAX_PENDING as i64)
    .execute(pool)
    .await?;
    Ok(())
}

async fn delete_durable_for_turn(pool: &sqlx::SqlitePool, turn_id: &str) -> anyhow::Result<()> {
    sqlx::query("DELETE FROM late_proof_pending WHERE turn_id = ?").bind(turn_id).execute(pool).await?;
    Ok(())
}

/// Deterministic registry and durable-row sweep. `wall_now` drives SQLite expiry; `monotonic_now`
/// drives in-memory entry TTLs, so both paths can be tested without sleeping.
pub async fn sweep_at(app: &impl LcHost, wall_now: &str, monotonic_now: Instant) -> anyhow::Result<(usize, u64)> {
    let durable_removed = prune_durable(app.db(), wall_now).await?;
    let active: Vec<String> = sqlx::query_scalar(
        "SELECT DISTINCT r.id FROM runs r JOIN turns t ON t.run_id = r.id
          WHERE r.state = 'running' AND t.status = 'in_flight' AND t.delivery IN ('pending','unknown','ok')",
    )
    .fetch_all(app.db())
    .await?;
    let active: HashSet<String> = active.into_iter().collect();
    let memory_removed = registry().lock().unwrap_or_else(|e| e.into_inner()).sweep_at(monotonic_now, &active);
    if memory_removed > 0 || durable_removed > 0 {
        tracing::debug!(memory_removed, durable_removed, pending = pending_count(), "late proof registry sweep");
    }
    Ok((memory_removed, durable_removed))
}

pub async fn sweep(app: &impl LcHost) -> anyhow::Result<(usize, u64)> {
    sweep_at(app, &db::now(), Instant::now()).await
}

/// 新的一輪送達開始：上一則沒被認領的殘影不能留給這一則。
pub fn clear(run_id: &str) {
    registry().lock().unwrap_or_else(|e| e.into_inner()).remove(run_id);
}

#[cfg(all(test, feature = "daemon-test-harness"))]
pub(crate) fn forget_pending_for_restart_test(run_id: &str) {
    registry().lock().unwrap_or_else(|e| e.into_inner()).remove(run_id);
}

fn take(run_id: &str) -> Option<Pending> {
    let now = Instant::now();
    registry().lock().unwrap_or_else(|e| e.into_inner()).take(run_id, now)
}

fn take_after_owed_write(run_id: &str, turn_id: &str) -> Option<Pending> {
    registry().lock().unwrap_or_else(|e| e.into_inner()).take_for_turn(run_id, turn_id)
}

#[cfg(all(test, feature = "daemon-test-harness"))]
pub(crate) fn expire_pending_for_test(run_id: &str) {
    if let Some(p) = registry().lock().unwrap_or_else(|e| e.into_inner()).by_run.get_mut(run_id) {
        p.noted = Instant::now() - CLAIM_WITHIN - Duration::from_secs(1);
    }
}

#[cfg(all(test, feature = "daemon-test-harness"))]
mod registry_tests {
    use super::*;

    fn pending(noted: Instant) -> Pending {
        Pending::new_at("claude", "pane-test", &Proof::EchoRow, 0, 0, "prompt", "2026-10-12T12:00:00.000Z".into(), noted)
    }

    #[test]
    fn issue_1494_registry_caps_1000_runs_and_expires_without_waiting() {
        let base = Instant::now();
        let mut registry = PendingRegistry::default();
        for n in 0..1000u64 {
            let noted = base + Duration::from_millis(n);
            registry.insert_at(format!("run-{n}"), pending(noted), noted);
        }

        assert_eq!(registry.len(), MAX_PENDING, "容量上限應淘汰最舊的 prompt");
        assert!(!registry.contains("run-0"));
        assert!(registry.contains("run-999"));
        let active_runs = (0..1000).map(|n| format!("run-{n}")).collect();
        let expired_at = base + Duration::from_millis(999) + CLAIM_WITHIN + Duration::from_millis(1);
        assert_eq!(registry.sweep_at(expired_at, &active_runs), MAX_PENDING, "TTL 到期應清掉所有 prompt");
        assert_eq!(registry.len(), 0);

        let mut completed = PendingRegistry::default();
        completed.insert_at("run-completed".into(), pending(base), base);
        assert_eq!(completed.sweep_at(base, &HashSet::new()), 1, "run 已完成／消失就立即清掉 prompt");
        assert!(!completed.contains("run-completed"));
    }

    #[test]
    fn owed_take_refuses_a_pending_bound_to_another_turn() {
        // 欠帳的 T1 結清時，run 的登記已被 T2 的 Pending 取代：不能把 T2 的基準認領到 T1 上（#1483 複查）。
        let base = Instant::now();
        let mut registry = PendingRegistry::default();
        registry.insert_at("run-1".into(), pending(base), base);
        registry.bind_turn("run-1", "turn-2");

        assert!(registry.take_for_turn("run-1", "turn-1").is_none(), "T2 的 Pending 不能升級 T1");
        assert!(registry.contains("run-1"), "屬於 T2 的項目留給 T2 自己認領");
        assert!(registry.take_for_turn("run-1", "turn-2").is_some(), "同一回合仍可認領");
    }

    #[test]
    fn owed_take_refuses_an_unbound_pending() {
        // 沒有綁到任何回合（持久化失敗）時寧可不認領：fail closed，回合留給 Stop hook／stuck_turns。
        let base = Instant::now();
        let mut registry = PendingRegistry::default();
        registry.insert_at("run-1".into(), pending(base), base);

        assert!(registry.take_for_turn("run-1", "turn-1").is_none());
    }
}

/// 一次檢查的結論。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Look {
    /// 證據長出來了：照 `Submitted` 記。
    Submitted,
    /// 框是空的、agent 在做事，但沒有無損證據：照 `Unverified` 記。
    Active,
    /// 還看不出來，下一輪再看。
    Wait,
    /// 不用再看了（回合已經被別的路收掉、run 換了、session 換了）。
    Stop,
}

/// 看一次。只讀，不寫任何東西、不按任何鍵。
pub async fn look(app: &impl LcHost, bot_id: &str, turn_id: &str, p: &Pending) -> Look {
    let row: Option<(String, String, Option<String>)> =
        match sqlx::query_as("SELECT status, delivery, run_id FROM turns WHERE id = ?").bind(turn_id).fetch_optional(app.db()).await {
            Ok(r) => r,
            Err(_) => return Look::Wait,
        };
    let Some((status, delivery, run_id)) = row else { return Look::Stop };
    let Some(run_id) = run_id else { return Look::Stop };
    if status != "in_flight" || delivery != "unknown" {
        return Look::Stop;
    }
    let run = match db::run(app.db(), &run_id).await {
        Ok(Some(r)) => r,
        Ok(None) => return Look::Stop,
        Err(_) => return Look::Wait,
    };
    if run.state != "running" || run.pane_id.as_deref() != Some(p.pane.as_str()) {
        return Look::Stop;
    }
    match same_session(app, &run_id, &p.proof).await {
        SessionMatch::Same => {}
        SessionMatch::Changed => return Look::Stop,
        SessionMatch::Unavailable => return Look::Wait,
    }
    let Ok(client) = client_for_run(app, &run).await else { return Look::Wait };
    let Ok(screen) = read_composer(&client, &p.pane).await else { return Look::Wait };
    match evidence(&p.kind, &p.proof, p.offset, &screen, &p.text) {
        Ok(n) if n > p.baseline => return Look::Submitted,
        Ok(_) => {}
        Err(_) => return Look::Wait,
    }
    // 沒有證據時只看「框空了＋agent 在忙」：框裡還有字（字還沒送出去）或畫面讀不準，都不算。
    if box_state_within(&p.kind, &screen, typed_tail(&p.text)) != BoxState::Empty {
        return Look::Wait;
    }
    let working = run.agent_status == "working";
    let hooks = super::poller::has_hook_activity(app.db(), bot_id, &p.since, turn_id).await;
    let spinner = !working
        && !hooks
        && client.pane_read(&p.pane, "visible", 60).await.is_ok_and(|read| super::poller::is_screen_busy(&read.text));
    if working || hooks || spinner {
        Look::Active
    } else {
        Look::Wait
    }
}

/// 升級：只有還在飛、還是 `unknown` 的那一筆。`Ok(false)`＝別的路先收掉了。
pub async fn promote(app: &impl LcHost, turn_id: &str, how: Delivered) -> anyhow::Result<bool> {
    let rec = how.record().expect("Submitted／Unverified 都有記錄");
    let moved = sqlx::query(
        "UPDATE turns SET delivery = ?, delivery_verified = ?, auto_resend = ?,
                resend_count = MAX(resend_count, ?)
          WHERE id = ? AND status = 'in_flight' AND delivery = 'unknown'",
    )
    .bind(rec.stored)
    .bind(i64::from(rec.verified))
    .bind(i64::from(rec.auto_resend))
    .bind(super::MAX_PROMPT_RESENDS)
    .bind(turn_id)
    .execute(app.db())
    .await?
    .rows_affected();
    Ok(moved > 0)
}

/// 一輪一輪看到有結論或看完為止。回升級成什麼（沒升級＝`None`）。`checks`／`every` 給測試用。
pub async fn watch(app: &impl LcHost, run_id: &str, bot_id: &str, turn_id: &str, p: &Pending, checks: u32, every: Duration) -> Option<Delivered> {
    let result = watch_loop(app, run_id, bot_id, turn_id, p, checks, every).await;
    // A successful proof needs no recovery row. If the bounded watch ends without a conclusion,
    // retain the baseline until its durable TTL so a later daemon restart can retry it.
    if result.is_some() {
        delete_durable_for_turn(app.db(), turn_id).await.ok();
    }
    result
}

async fn watch_loop(app: &impl LcHost, run_id: &str, bot_id: &str, turn_id: &str, p: &Pending, checks: u32, every: Duration) -> Option<Delivered> {
    for _ in 0..checks {
        tokio::time::sleep(every).await;
        let how = match look(app, bot_id, turn_id, p).await {
            Look::Submitted => Delivered::Submitted,
            Look::Active => Delivered::Unverified,
            Look::Wait => continue,
            Look::Stop => return None,
        };
        match promote(app, turn_id, how).await {
            Ok(true) => {
                tracing::info!(run = %run_id, turn = %turn_id, how = ?how, "送達狀態未知的回合背景補到了證據：升成 ok、解鎖");
                emit_turn(app, turn_id).await;
                // 跟一般 ok／unverified 的送達同一套看門狗（`prompt.rs` 寫完送達結果之後做的事）。
                super::poller::arm_stall(app, run_id, bot_id, turn_id).await;
                super::poller::arm_progress(app, run_id, bot_id, turn_id).await;
                return Some(how);
            }
            Ok(false) => return None,
            Err(e) => tracing::warn!(turn = %turn_id, error = %e, "送達狀態未知的回合升級寫不進去，下一輪再試"),
        }
    }
    None
}

#[cfg(test)]
pub(crate) fn armed_in_tests() -> &'static Mutex<Vec<(String, Pending)>> {
    static M: OnceLock<Mutex<Vec<(String, Pending)>>> = OnceLock::new();
    M.get_or_init(Default::default)
}

/// 呼叫端寫完 `delivery=unknown` 之後認領這一筆（沒有記著＝不是 `not_proven_submitted`，什麼都不做）。
/// 背景任務不擋呼叫端；測試裡不開背景任務（跟 `owed_delivery::schedule_retry` 一樣），只留下記號，行為由
/// [`watch`]／[`look`] 直接驗。
pub fn arm(app: &impl LcHost, run_id: &str, bot_id: &str, turn_id: &str) {
    let Some(p) = take(run_id) else { return };
    arm_pending(app, run_id, bot_id, turn_id, p);
}

/// 欠帳的 unknown 寫入結清時，Pending 對應的回合已明確，原本的認領期限不再適用。
pub fn arm_after_owed_write(app: &(impl LcHost + Clone + 'static), run_id: &str, bot_id: &str, turn_id: &str) {
    if let Some(p) = take_after_owed_write(run_id, turn_id) {
        arm_pending(app, run_id, bot_id, turn_id, p);
        return;
    }
    #[cfg(not(test))]
    {
        let app = (*app).clone();
        let (run_id, bot_id, turn_id) = (run_id.to_string(), bot_id.to_string(), turn_id.to_string());
        tokio::spawn(async move {
            match load_pending_for_owed(app.db(), &run_id, &turn_id).await {
                Ok(Some(p)) => arm_pending(&app, &run_id, &bot_id, &turn_id, p),
                Ok(None) => {}
                Err(e) => tracing::warn!(run = %run_id, turn = %turn_id, error = %e, "could not restore late proof after owed unknown write"),
            }
        });
    }
}

#[cfg(not(test))]
async fn load_pending_for_owed(pool: &sqlx::SqlitePool, run_id: &str, turn_id: &str) -> anyhow::Result<Option<Pending>> {
    let row: Option<(
        String, String, String, Option<String>, Option<String>, Option<String>, i64, i64, String, String,
    )> = sqlx::query_as(
        "SELECT p.kind, p.pane, p.proof_kind, p.proof_format, p.proof_path, p.proof_session_id,
                p.proof_offset, p.baseline, p.prompt_text, p.since
           FROM late_proof_pending p JOIN turns t ON t.id = p.turn_id
          WHERE p.run_id = ? AND p.turn_id = ? AND p.expires_at > ?
            AND t.status = 'in_flight' AND t.delivery = 'unknown'",
    )
    .bind(run_id)
    .bind(turn_id)
    .bind(db::now())
    .fetch_optional(pool)
    .await?;
    let Some((kind, pane, proof_kind, proof_format, proof_path, proof_session_id, offset, baseline, text, since)) = row else { return Ok(None) };
    let proof = match proof_kind.as_str() {
        "echo_row" => Proof::EchoRow,
        "unverified" => Proof::Unverified,
        "transcript" => {
            let (Some(format), Some(path), Some(session_id)) = (proof_format, proof_path, proof_session_id) else {
                anyhow::bail!("incomplete durable transcript proof for run {run_id} turn {turn_id}");
            };
            let format = match format.as_str() {
                "claude" => super::delivery::LogFormat::Claude,
                "codex" => super::delivery::LogFormat::Codex,
                "agy" => super::delivery::LogFormat::Agy,
                _ => anyhow::bail!("unknown durable transcript format {format}"),
            };
            Proof::Transcript { format, path: std::path::PathBuf::from(path), session_id }
        }
        _ => anyhow::bail!("unknown durable proof kind {proof_kind}"),
    };
    Ok(Some(Pending::new(&kind, &pane, &proof, offset.max(0) as u64, baseline.max(0) as usize, &text, since)))
}

fn arm_pending(app: &impl LcHost, run_id: &str, bot_id: &str, turn_id: &str, p: Pending) {
    #[cfg(test)]
    {
        let _ = (app, run_id, bot_id);
        armed_in_tests().lock().unwrap_or_else(|e| e.into_inner()).push((turn_id.to_string(), p));
    }
    #[cfg(not(test))]
    {
        let app = (*app).clone();
        let (run_id, bot_id, turn_id) = (run_id.to_string(), bot_id.to_string(), turn_id.to_string());
        tokio::spawn(async move {
            watch(&app, &run_id, &bot_id, &turn_id, &p, CHECKS, CHECK_EVERY).await;
        });
    }
}
