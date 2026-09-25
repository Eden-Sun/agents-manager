//! 忙到一半被重啟的 claude，接回來之後補一句續行提示（claude 2.1.281）。
//!
//! 2.1.281 的 changelog：「Fixed resuming a session that ended during a tool call: Claude now sees the call and is told its
//! outcome is unknown, and a manual resume no longer adds a hidden "Continue" message」。以前 `--resume` 一段停在工具中間的
//! 對話，CLI 自己補一句隱藏的 Continue、agent 接著做；現在不補了——接回來的 bot 停在輸入框，等一句永遠不會來的話。
//!
//! 所以由 daemon 補：`resume_native` 起的 claude，重啟前在做事（`working`／還有一回合沒收），接回之後排一則
//! 續行提示進佇列（`relay_from = daemon`，聊天室看得到是誰送的）。走一般的排隊送達：`resume_gate` 等 SessionStart 驗過、
//! 畫面回到閒置才送，跟 AGM 排著的派工同一條路。條件照 #424 的提案收（#430）：
//! - **停在等人的畫面不催**：`blocked`＝確認框、問卷、額度選單。那一步只有人能決定（#423：不准自動按），叫模型
//!   「接著把原本的工作做完」等於替人做了那個決定。
//! - **不是外力收掉的不催**：上一回合是被 API 錯誤收掉的（撞額度等，`runs.turn_error`）、或使用者剛從網頁中斷過
//!   （`interrupt_grace` 記的接管時刻落在這個 run 裡）——那是有人或有原因叫它停的，不替它續做。
//! - **只送進驗證過的接回**：排進佇列時 `SessionStart` 通常還沒到，接回成不成還不知道；flush 放行前
//!   （[`withdraw_unless_resumed`]）只有 `resume_outcome = verified` 才送，`mismatch`（CLI 默默開了新對話）、
//!   `unverified`（等滿窗口沒回報）、沒有 hook 可驗的，都把這一則撤掉並在聊天室說明——新對話裡沒有「原本的工作」。
//!
//! 「重啟前在忙」有兩種讀法：
//! - **重啟**（`restart_bot_with`：`?resume=native` restart、換身分）：停之前在鎖裡讀（[`busy_before_restart`]）。
//!   一鍵重啟要求閒置（`require_idle`），不會遇到。
//! - **啟動**（herdr 整個重啟後 `start?resume=native`）：舊 run 已經被 pane 消失收成 `exited`，看它最後記的
//!   `agent_status`（[`prior_run_ended_busy`]）。pane 是被外力收掉的，最後一次狀態就是當時的狀態。使用者自己停的
//!   （`stopped`）不算：停下來是他要的，接回來不替他續做。「上一個 run」用 `rowid` 挑：`started_at` 字串精度不一、
//!   ulid 同毫秒不單調，都排不出真正最後那個（#101、#100）。
//!
//! 不補的：codex／grok（沒有這個改變）；接不回、退回開新對話的（沒有「原本的工作」可續）；佇列已經有一則在排的
//! （每個對話最多一筆 queued；那一則送進去 bot 自然會動，也不能插在 AGM 的派工前面）。同一個 run 只補一次
//! （`client_request_id = resume-nudge:<run_id>`）。

use super::*;

/// 送進 bot 的續行提示。
pub(crate) const NUDGE_TEXT: &str = "[來自 AG Man daemon] 重啟前你正在進行的工作被中斷了（重啟當下可能有工具還在跑，結果不明）。\
請先確認上一個工具的實際結果，再接著把原本的工作做完。";

/// 續行提示的 `client_request_id` 前綴，後面接排它的那個 run。
const CRID_PREFIX: &str = "resume-nudge:";

/// 純規則：這次啟動要不要補續行提示。`busy` 是重啟前讀到的忙碌理由（`None`＝閒著）。
/// `blocked`（在等人按確認框／答問卷）不算：那一步只有人能決定（#423、#430）。
pub(crate) fn wants_nudge(kind: &str, resume_native: bool, busy: Option<&str>) -> bool {
    kind == "claude" && resume_native && matches!(busy, Some("working" | "turn_in_flight"))
}

/// 上一個 run 是**有人或有原因**叫它停的：回原因（只進 log），不補。`started_at` 是那個 run 的開始時間。
fn stopped_on_purpose(bot_id: &str, started_at: &str, turn_error: Option<&str>) -> Option<&'static str> {
    if turn_error.is_some_and(|e| !e.trim().is_empty()) {
        return Some("the last turn was cut short by an API error (quota or otherwise)");
    }
    let started = chrono::DateTime::parse_from_rfc3339(started_at).ok()?.with_timezone(&chrono::Utc);
    super::interrupt_grace::hold_of(bot_id).filter(|at| *at >= started).map(|_| "the user interrupted this run")
}

/// 重啟的那一半：停之前讀，呼叫端持 bot 鎖。讀不到就不補（寧可少補一句，也不在不知道的時候對 bot 說它被中斷了）。
pub(crate) async fn busy_before_restart(app: &Arc<App>, bot: &db::Bot, opts: &StartOpts) -> Option<&'static str> {
    if !wants_nudge(&bot.kind, opts.resume_native, Some("working")) {
        return None;
    }
    let run = match db::active_run(&app.db, &bot.id).await {
        Ok(Some(run)) => run,
        Ok(None) => return None,
        Err(e) => {
            tracing::warn!(bot = %bot.name, error = %e, "could not read the run before the restart; no resume nudge");
            return None;
        }
    };
    let busy = match run.agent_status.as_str() {
        "working" => Some("working"),
        // 在等人：不催，也不再看有沒有回合沒收（那一回合就是卡在等人）。
        "blocked" => None,
        _ => match db::in_flight_turn(&app.db, &run.id).await {
            Ok(t) => t.map(|_| "turn_in_flight"),
            Err(e) => {
                tracing::warn!(bot = %bot.name, error = %e, "could not read the in-flight turn before the restart; no resume nudge");
                None
            }
        },
    };
    let busy = busy.filter(|b| wants_nudge(&bot.kind, opts.resume_native, Some(b)))?;
    if let Some(why) = stopped_on_purpose(&bot.id, &run.started_at, run.turn_error.as_deref()) {
        tracing::info!(bot = %bot.name, run = %run.id, busy, why, "busy before the restart, but it was stopped on purpose; no resume nudge");
        return None;
    }
    Some(busy)
}

/// 啟動的那一半：在新 run 寫進去之前讀這顆 bot 的上一個 run。只認被外力收掉的（`exited`），最後記的是 working、
/// 而且不是有人或有原因叫它停的（[`stopped_on_purpose`]）。
pub(crate) async fn prior_run_ended_busy(app: &Arc<App>, bot: &db::Bot, opts: &StartOpts) -> Option<&'static str> {
    if !wants_nudge(&bot.kind, opts.resume_native, Some("working")) {
        return None;
    }
    // 最後寫進去的那一列＝最後一個 run：`rowid` 單調，`started_at`（精度不一）與 ulid（同毫秒不單調）都不是（#430）。
    let last: Option<(String, String, String, Option<String>)> =
        match sqlx::query_as("SELECT state, agent_status, started_at, turn_error FROM runs WHERE bot_id = ? ORDER BY rowid DESC LIMIT 1")
            .bind(&bot.id)
            .fetch_optional(&app.db)
            .await
        {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(bot = %bot.name, error = %e, "could not read the previous run; no resume nudge");
                return None;
            }
        };
    let (state, status, started_at, turn_error) = last?;
    if state != "exited" || status != "working" {
        return None;
    }
    if let Some(why) = stopped_on_purpose(&bot.id, &started_at, turn_error.as_deref()) {
        tracing::info!(bot = %bot.name, why, "the previous run died mid-work, but it was stopped on purpose; no resume nudge");
        return None;
    }
    Some("working")
}

/// 新 run 起來之後排續行提示。只在真的帶了 `--resume` 時排（接不回、開了新對話就沒有原本的工作）。
/// 這時多半還不知道接回成不成（`SessionStart` 還沒到）；送不送最後由 [`withdraw_unless_resumed`] 在 flush 放行前決定。
/// 回 `true`＝排進去了。排不進去不影響啟動結果，只記 log。
pub(crate) async fn queue(app: &Arc<App>, bot: &db::Bot, run_id: &str, busy: &str) -> bool {
    let resumed: Option<(Option<String>, Option<String>)> =
        sqlx::query_as("SELECT resume_session_id, resume_outcome FROM runs WHERE id = ?").bind(run_id).fetch_optional(&app.db).await.ok().flatten();
    let resumed = match resumed {
        Some((Some(_), _)) => true,
        Some((None, Some(o))) => o == "verified",
        _ => false,
    };
    if !resumed {
        tracing::info!(bot = %bot.name, run = run_id, busy, "busy before the restart, but this start did not resume the old session; no resume nudge");
        return false;
    }
    let conv = match db::conversation_id(&app.db, &bot.id).await {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(bot = %bot.name, run = run_id, error = %e, "no conversation for the resume nudge");
            return false;
        }
    };
    let relay = super::prompt::RelaySrc::trusted(Some(crate::agent_relay::DAEMON_SENDER));
    let crid = format!("{CRID_PREFIX}{run_id}");
    match super::prompt::queue_for_next_turn(app, &conv, &bot.id, NUDGE_TEXT, NUDGE_TEXT, &crid, None, relay).await {
        Ok(out) => {
            tracing::info!(bot = %bot.name, run = run_id, busy, turn = %out.turn_id,
                           "resumed a claude that was busy before the restart; queued a nudge to continue (2.1.281 no longer adds a hidden Continue)");
            true
        }
        Err(e) => {
            tracing::info!(bot = %bot.name, run = run_id, busy, error = ?e, "resume nudge not queued (another prompt is already queued, or the write failed)");
            false
        }
    }
}

/// flush 放行前的最後一關（`resume_gate` 開了之後）：這一筆是續行提示的話，只有排它的那個 run、而且
/// `resume_outcome = verified` 才送；其他一律撤掉並在聊天室說明。回 `true`＝這一筆不送了（撤掉了，或已經不在佇列）。
/// 不是續行提示的一律 `false`。呼叫端持 bot 鎖。
pub(crate) async fn withdraw_unless_resumed(app: &Arc<App>, turn: &db::Turn, run: &db::Run) -> anyhow::Result<bool> {
    let Some(for_run) = turn.client_request_id.as_deref().and_then(|c| c.strip_prefix(CRID_PREFIX)) else { return Ok(false) };
    let why = if for_run != run.id {
        "續行提示沒有送出：排它的那一次啟動已經不在了（之後又重啟過），接著做什麼交給使用者或 AGM。"
    } else {
        match run.resume_outcome.as_deref() {
            Some("verified") => return Ok(false),
            Some("mismatch") => "續行提示沒有送出：這次接回的不是原本那段對話（CLI 開了新對話），新對話裡沒有「原本的工作」可以接著做。",
            Some("unverified") => "續行提示沒有送出：確認不了接回的是不是原本那段對話，不在不確定的對話裡叫它接著做。",
            _ => "續行提示沒有送出：這顆 bot 沒有 hook 可以確認接回的是原本那段對話，不在不確定的對話裡叫它接著做。",
        }
    };
    super::queue::revoke_queued_turn(app, &turn.id, why).await?;
    tracing::info!(bot = %run.bot_id, run = %run.id, turn = %turn.id, outcome = ?run.resume_outcome, "withdrew the resume nudge: not a verified resume of the old session");
    Ok(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::{claude_bot, env, Env};

    async fn queued(e: &Env, bot_id: &str) -> Vec<(String, Option<String>)> {
        let conv = db::conversation_id(&e.app.db, bot_id).await.unwrap();
        sqlx::query_as(
            "SELECT m.content, m.relay_from FROM turns t JOIN messages m ON m.turn_id = t.id
             WHERE t.conversation_id = ? AND t.status = 'queued'",
        )
        .bind(&conv)
        .fetch_all(&e.app.db)
        .await
        .unwrap()
    }

    /// 上一段對話有 transcript，`--resume` 接得回。
    async fn resumable(e: &Env, bot_id: &str, state: &str, status: &str) {
        resumable_at(e, bot_id, state, status, "2026-09-24T00:00:00Z", None).await;
    }

    async fn resumable_at(e: &Env, bot_id: &str, state: &str, status: &str, started_at: &str, turn_error: Option<&str>) {
        let transcript = e.dir.join(format!("{bot_id}.jsonl"));
        std::fs::write(&transcript, "{}\n").unwrap();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, native_session_id, transcript_path, started_at, ended_at, turn_error)
             VALUES (?,?,?,?,?,?,?,'2026-09-24T00:01:00Z',?)",
        )
        .bind(db::ulid())
        .bind(bot_id)
        .bind(state)
        .bind(status)
        .bind(format!("sid-{bot_id}"))
        .bind(transcript.to_str().unwrap())
        .bind(started_at)
        .bind(turn_error)
        .execute(&e.app.db)
        .await
        .unwrap();
    }

    fn strict() -> StartOpts {
        StartOpts { resume_native: true, resume_required: true, ..Default::default() }
    }

    #[test]
    fn only_a_busy_claude_resume_is_nudged() {
        assert!(wants_nudge("claude", true, Some("working")));
        assert!(!wants_nudge("claude", true, Some("blocked")), "停在確認框／問卷等人的不催（#430）");
        assert!(wants_nudge("claude", true, Some("turn_in_flight")));
        assert!(!wants_nudge("claude", true, None), "閒著重啟的不補");
        assert!(!wants_nudge("claude", false, Some("working")), "開新對話的不補");
        assert!(!wants_nudge("codex", true, Some("working")), "codex 照舊");
        assert!(!wants_nudge("grok", true, Some("working")), "grok 照舊");
    }

    /// 重啟：做事中被 `?resume=native` 重啟的 claude，接回之後排一則續行提示；閒置中、停在等人畫面上（`blocked`）重啟的不排。
    #[tokio::test]
    async fn a_claude_restarted_while_busy_gets_a_nudge_and_an_idle_one_does_not() {
        let e = env().await;
        let strict = strict();
        for (status, want) in [("working", true), ("blocked", false), ("idle", false)] {
            let bot = claude_bot(&e.app, &e.project_id, &format!("r-{status}")).await;
            resumable(&e, &bot.id, "stopped", "idle").await;
            let run = crate::lifecycle::start_bot_with(&e.app, &bot.id, strict.clone()).await.unwrap();
            sqlx::query("UPDATE runs SET agent_status=? WHERE id=?").bind(status).bind(&run).execute(&e.app.db).await.unwrap();
            crate::lifecycle::restart_bot_with(&e.app, &bot.id, strict.clone()).await.unwrap();
            let q = queued(&e, &bot.id).await;
            if want {
                assert_eq!(q, vec![(NUDGE_TEXT.to_string(), Some("daemon".to_string()))], "{status}");
            } else {
                assert!(q.is_empty(), "{status} 中重啟的不補：{q:?}");
            }
            crate::lifecycle::stop_bot(&e.app, &bot.id).await.unwrap();
        }
    }

    /// 閒著但還有一回合沒收（hook 還沒到）也算忙。
    #[tokio::test]
    async fn an_open_turn_counts_as_busy() {
        let e = env().await;
        let strict = strict();
        let bot = claude_bot(&e.app, &e.project_id, "open-turn").await;
        resumable(&e, &bot.id, "stopped", "idle").await;
        let run = crate::lifecycle::start_bot_with(&e.app, &bot.id, strict.clone()).await.unwrap();
        sqlx::query("UPDATE runs SET agent_status='idle' WHERE id=?").bind(&run).execute(&e.app.db).await.unwrap();
        let conv = db::conversation_id(&e.app.db, &bot.id).await.unwrap();
        sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at) VALUES (?,?,?,'web','in_flight','ok',?)")
            .bind(db::ulid())
            .bind(&conv)
            .bind(&run)
            .bind(db::now())
            .execute(&e.app.db)
            .await
            .unwrap();
        crate::lifecycle::restart_bot_with(&e.app, &bot.id, strict).await.unwrap();
        assert_eq!(queued(&e, &bot.id).await.len(), 1);
    }

    /// herdr 整個重啟：舊 run 被 pane 消失收成 `exited`、最後記著 working，`start?resume=native` 接回之後補；
    /// 最後是 idle 的、停在等人畫面上的（`blocked`）、使用者自己停的（`stopped`）不補。
    #[tokio::test]
    async fn a_start_after_the_pane_died_mid_work_is_nudged() {
        let e = env().await;
        let strict = strict();
        for (state, status, want) in [("exited", "working", 1), ("exited", "blocked", 0), ("exited", "idle", 0), ("stopped", "working", 0)] {
            let bot = claude_bot(&e.app, &e.project_id, &format!("s-{state}-{status}")).await;
            resumable(&e, &bot.id, state, status).await;
            crate::lifecycle::start_bot_with(&e.app, &bot.id, strict.clone()).await.unwrap();
            assert_eq!(queued(&e, &bot.id).await.len(), want, "{state}/{status}");
            crate::lifecycle::stop_bot(&e.app, &bot.id).await.unwrap();
        }
    }

    /// 上一回合是被 API 錯誤收掉的（撞額度等，`runs.turn_error`）：不是外力砍在半路，不補——重啟與啟動兩條都一樣。
    #[tokio::test]
    async fn a_run_cut_short_by_an_api_error_is_not_nudged() {
        let e = env().await;
        let bot = claude_bot(&e.app, &e.project_id, "quota-start").await;
        resumable_at(&e, &bot.id, "exited", "working", "2026-09-24T00:00:00Z", Some("API Error: 429 rate limit")).await;
        crate::lifecycle::start_bot_with(&e.app, &bot.id, strict()).await.unwrap();
        assert!(queued(&e, &bot.id).await.is_empty(), "啟動：上一個 run 撞額度收掉的不補");
        crate::lifecycle::stop_bot(&e.app, &bot.id).await.unwrap();

        let bot = claude_bot(&e.app, &e.project_id, "quota-restart").await;
        resumable(&e, &bot.id, "stopped", "idle").await;
        let run = crate::lifecycle::start_bot_with(&e.app, &bot.id, strict()).await.unwrap();
        sqlx::query("UPDATE runs SET agent_status='working', turn_error='API Error: 429 rate limit' WHERE id=?")
            .bind(&run)
            .execute(&e.app.db)
            .await
            .unwrap();
        crate::lifecycle::restart_bot_with(&e.app, &bot.id, strict()).await.unwrap();
        assert!(queued(&e, &bot.id).await.is_empty(), "重啟：這一回合撞額度收掉的不補");
    }

    /// 使用者剛從網頁中斷過這個 run：是人叫它停的，重啟接回之後不替他續做。
    #[tokio::test]
    async fn a_run_the_user_interrupted_is_not_nudged() {
        let e = env().await;
        let bot = claude_bot(&e.app, &e.project_id, "interrupted").await;
        resumable(&e, &bot.id, "stopped", "idle").await;
        let run = crate::lifecycle::start_bot_with(&e.app, &bot.id, strict()).await.unwrap();
        sqlx::query("UPDATE runs SET agent_status='working' WHERE id=?").bind(&run).execute(&e.app.db).await.unwrap();
        crate::lifecycle::interrupt_grace::note_user_interrupt(&bot.id);
        crate::lifecycle::restart_bot_with(&e.app, &bot.id, strict()).await.unwrap();
        assert!(queued(&e, &bot.id).await.is_empty());
    }

    /// 「上一個 run」是最後寫進去的那一列，不是 `started_at` 字串排最大的那列：秒精度的 `…00Z` 字串上排在
    /// 毫秒精度的 `…00.500Z` 後面（`Z` > `.`，#101），用字串排會拿舊的那個 run 的 `exited`／working 來補。
    #[tokio::test]
    async fn the_previous_run_is_the_last_one_written_not_the_largest_timestamp_string() {
        let e = env().await;
        let bot = claude_bot(&e.app, &e.project_id, "mixed-precision").await;
        resumable_at(&e, &bot.id, "exited", "working", "2026-09-24T00:00:00Z", None).await;
        resumable_at(&e, &bot.id, "stopped", "idle", "2026-09-24T00:00:00.500Z", None).await;
        crate::lifecycle::start_bot_with(&e.app, &bot.id, strict()).await.unwrap();
        assert!(queued(&e, &bot.id).await.is_empty(), "真正最後那個 run 是使用者停的，不補");
    }

    /// 續行提示排進去的時候還不知道接回成不成；flush 放行前只有 `verified` 才送，`mismatch`／`unverified`／
    /// 沒有 hook 可驗的都撤掉、留一則說明——新對話裡沒有「原本的工作」（#430）。
    #[tokio::test]
    async fn the_nudge_only_goes_into_a_verified_resume() {
        let e = env().await;
        for (case, outcome, hooks, withdrawn) in [
            ("verified", Some("verified"), 1, false),
            ("mismatch", Some("mismatch"), 1, true),
            ("unverified", Some("unverified"), 1, true),
            ("no-hooks", None, 0, true),
        ] {
            let bot = claude_bot(&e.app, &e.project_id, &format!("v-{case}")).await;
            resumable(&e, &bot.id, "exited", "working").await;
            let run = crate::lifecycle::start_bot_with(&e.app, &bot.id, strict()).await.unwrap();
            assert_eq!(queued(&e, &bot.id).await.len(), 1, "{case}：先排進去，等結論");
            sqlx::query("UPDATE bots SET inject_hooks=? WHERE id=?").bind(hooks).bind(&bot.id).execute(&e.app.db).await.unwrap();
            if outcome.is_some() {
                sqlx::query("UPDATE runs SET resume_session_id=NULL, resume_outcome=? WHERE id=?")
                    .bind(outcome)
                    .bind(&run)
                    .execute(&e.app.db)
                    .await
                    .unwrap();
            }
            sqlx::query("UPDATE runs SET state='running', agent_status='idle' WHERE id=?").bind(&run).execute(&e.app.db).await.unwrap();
            let crid = format!("{CRID_PREFIX}{run}");
            forget_queue_retry_timer(&bot.id);
            flush_queued_locked(&e.app, &bot.id).await.unwrap();
            forget_queue_retry_timer(&bot.id);
            let conv = db::conversation_id(&e.app.db, &bot.id).await.unwrap();
            let t: db::Turn = sqlx::query_as("SELECT * FROM turns WHERE conversation_id=? AND client_request_id=?")
                .bind(&conv)
                .bind(&crid)
                .fetch_one(&e.app.db)
                .await
                .unwrap();
            let notes: Vec<String> = sqlx::query_scalar("SELECT content FROM messages WHERE turn_id=? AND role='system'")
                .bind(&t.id)
                .fetch_all(&e.app.db)
                .await
                .unwrap();
            let said = notes.iter().any(|n| n.contains("續行提示沒有送出"));
            if withdrawn {
                assert_eq!((t.status.as_str(), t.flush_retries, said), ("failed", 0, true), "{case}：撤掉、一個字都沒打、有說明：{notes:?}");
            } else {
                assert!(!said && (t.status != "queued" || t.flush_retries > 0), "{case}：過了這一關往下送：{t:?} {notes:?}");
            }
            crate::lifecycle::stop_bot(&e.app, &bot.id).await.unwrap();
        }
    }

    /// 接不回、退回開新對話：沒有原本的工作可續，不補。
    #[tokio::test]
    async fn a_fallback_to_a_new_conversation_is_not_nudged() {
        let e = env().await;
        let bot = claude_bot(&e.app, &e.project_id, "fresh").await;
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, started_at, ended_at)
             VALUES (?,?,'exited','working','2026-09-24T00:00:00Z','2026-09-24T00:01:00Z')",
        )
        .bind(db::ulid())
        .bind(&bot.id)
        .execute(&e.app.db)
        .await
        .unwrap();
        crate::lifecycle::start_bot_with(&e.app, &bot.id, StartOpts { resume_native: true, ..Default::default() }).await.unwrap();
        assert!(queued(&e, &bot.id).await.is_empty());
    }

    /// 佇列裡已經有一則（AGM 的派工）：不擠掉它、也不疊第二則。
    #[tokio::test]
    async fn an_already_queued_prompt_is_left_alone() {
        let e = env().await;
        let bot = claude_bot(&e.app, &e.project_id, "has-queue").await;
        resumable(&e, &bot.id, "exited", "working").await;
        let conv = db::conversation_id(&e.app.db, &bot.id).await.unwrap();
        let relay = super::super::prompt::RelaySrc::trusted(Some("agm"));
        super::super::prompt::queue_for_next_turn(&e.app, &conv, &bot.id, "派工", "派工", "agm-1", None, relay).await.unwrap();
        crate::lifecycle::start_bot_with(&e.app, &bot.id, StartOpts { resume_native: true, resume_required: true, ..Default::default() }).await.unwrap();
        assert_eq!(queued(&e, &bot.id).await, vec![("派工".to_string(), Some("agm".to_string()))]);
    }
}
