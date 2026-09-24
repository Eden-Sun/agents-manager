//! 忙到一半被重啟的 claude，接回來之後補一句續行提示（claude 2.1.281）。
//!
//! 2.1.281 的 changelog：「Fixed resuming a session that ended during a tool call: Claude now sees the call and is told its
//! outcome is unknown, and a manual resume no longer adds a hidden "Continue" message」。以前 `--resume` 一段停在工具中間的
//! 對話，CLI 自己補一句隱藏的 Continue、agent 接著做；現在不補了——接回來的 bot 停在輸入框，等一句永遠不會來的話。
//!
//! 所以由 daemon 補：`resume_native` 起的 claude，重啟前在忙（`working`／`blocked`／還有一回合沒收），接回之後排一則
//! 續行提示進佇列（`relay_from = daemon`，聊天室看得到是誰送的）。走一般的排隊送達：`resume_gate` 等 SessionStart 驗過、
//! 畫面回到閒置才送，跟 AGM 排著的派工同一條路。
//!
//! 「重啟前在忙」有兩種讀法：
//! - **重啟**（`restart_bot_with`：`?resume=native` restart、換身分）：停之前在鎖裡讀（[`busy_before_restart`]）。
//!   一鍵重啟要求閒置（`require_idle`），不會遇到。
//! - **啟動**（herdr 整個重啟後 `start?resume=native`）：舊 run 已經被 pane 消失收成 `exited`，看它最後記的
//!   `agent_status`（[`prior_run_ended_busy`]）。pane 是被外力收掉的，最後一次狀態就是當時的狀態。使用者自己停的
//!   （`stopped`）不算：停下來是他要的，接回來不替他續做。
//!
//! 不補的：codex／grok（沒有這個改變）；接不回、退回開新對話的（沒有「原本的工作」可續）；佇列已經有一則在排的
//! （每個對話最多一筆 queued；那一則送進去 bot 自然會動，也不能插在 AGM 的派工前面）。同一個 run 只補一次
//! （`client_request_id = resume-nudge:<run_id>`）。

use super::*;

/// 送進 bot 的續行提示。
pub(crate) const NUDGE_TEXT: &str = "[來自 AG Man daemon] 重啟前你正在進行的工作被中斷了（重啟當下可能有工具還在跑，結果不明）。\
請先確認上一個工具的實際結果，再接著把原本的工作做完。";

/// 純規則：這次啟動要不要補續行提示。`busy` 是重啟前讀到的忙碌理由（`None`＝閒著）。
pub(crate) fn wants_nudge(kind: &str, resume_native: bool, busy: Option<&str>) -> bool {
    kind == "claude" && resume_native && matches!(busy, Some("working" | "blocked" | "turn_in_flight"))
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
        "blocked" => Some("blocked"),
        _ => match db::in_flight_turn(&app.db, &run.id).await {
            Ok(t) => t.map(|_| "turn_in_flight"),
            Err(e) => {
                tracing::warn!(bot = %bot.name, error = %e, "could not read the in-flight turn before the restart; no resume nudge");
                None
            }
        },
    };
    busy.filter(|b| wants_nudge(&bot.kind, opts.resume_native, Some(b)))
}

/// 啟動的那一半：在新 run 寫進去之前讀這顆 bot 的上一個 run。只認被外力收掉的（`exited`），最後記的是 working／blocked。
pub(crate) async fn prior_run_ended_busy(app: &Arc<App>, bot: &db::Bot, opts: &StartOpts) -> Option<&'static str> {
    if !wants_nudge(&bot.kind, opts.resume_native, Some("working")) {
        return None;
    }
    let last: Option<(String, String)> =
        match sqlx::query_as("SELECT state, agent_status FROM runs WHERE bot_id = ? ORDER BY started_at DESC, id DESC LIMIT 1")
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
    match last {
        Some((state, status)) if state == "exited" => match status.as_str() {
            "working" => Some("working"),
            "blocked" => Some("blocked"),
            _ => None,
        },
        _ => None,
    }
}

/// 新 run 起來之後排續行提示。只在真的帶了 `--resume` 時排（接不回、開了新對話就沒有原本的工作）。
/// 回 `true`＝排進去了。排不進去不影響啟動結果，只記 log。
pub(crate) async fn queue(app: &Arc<App>, bot: &db::Bot, run_id: &str, busy: &str) -> bool {
    let resumed: Option<(Option<String>, Option<String>)> =
        sqlx::query_as("SELECT resume_session_id, resume_outcome FROM runs WHERE id = ?").bind(run_id).fetch_optional(&app.db).await.ok().flatten();
    let resumed = match resumed {
        Some((Some(_), _)) => true,
        Some((None, Some(o))) => o == "verified" || o == "unverified",
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
    let crid = format!("resume-nudge:{run_id}");
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
        let transcript = e.dir.join(format!("{bot_id}.jsonl"));
        std::fs::write(&transcript, "{}\n").unwrap();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, native_session_id, transcript_path, started_at, ended_at)
             VALUES (?,?,?,?,?,?,'2026-09-24T00:00:00Z','2026-09-24T00:01:00Z')",
        )
        .bind(db::ulid())
        .bind(bot_id)
        .bind(state)
        .bind(status)
        .bind(format!("sid-{bot_id}"))
        .bind(transcript.to_str().unwrap())
        .execute(&e.app.db)
        .await
        .unwrap();
    }

    #[test]
    fn only_a_busy_claude_resume_is_nudged() {
        assert!(wants_nudge("claude", true, Some("working")));
        assert!(wants_nudge("claude", true, Some("blocked")));
        assert!(wants_nudge("claude", true, Some("turn_in_flight")));
        assert!(!wants_nudge("claude", true, None), "閒著重啟的不補");
        assert!(!wants_nudge("claude", false, Some("working")), "開新對話的不補");
        assert!(!wants_nudge("codex", true, Some("working")), "codex 照舊");
        assert!(!wants_nudge("grok", true, Some("working")), "grok 照舊");
    }

    /// 重啟：忙碌中被 `?resume=native` 重啟的 claude，接回之後排一則續行提示；閒置中重啟的不排。
    #[tokio::test]
    async fn a_claude_restarted_while_busy_gets_a_nudge_and_an_idle_one_does_not() {
        let e = env().await;
        let strict = StartOpts { resume_native: true, resume_required: true, ..Default::default() };
        for (status, want) in [("working", true), ("blocked", true), ("idle", false)] {
            let bot = claude_bot(&e.app, &e.project_id, &format!("r-{status}")).await;
            resumable(&e, &bot.id, "stopped", "idle").await;
            let run = crate::lifecycle::start_bot_with(&e.app, &bot.id, strict.clone()).await.unwrap();
            sqlx::query("UPDATE runs SET agent_status=? WHERE id=?").bind(status).bind(&run).execute(&e.app.db).await.unwrap();
            crate::lifecycle::restart_bot_with(&e.app, &bot.id, strict.clone()).await.unwrap();
            let q = queued(&e, &bot.id).await;
            if want {
                assert_eq!(q, vec![(NUDGE_TEXT.to_string(), Some("daemon".to_string()))], "{status}");
            } else {
                assert!(q.is_empty(), "閒置中重啟的不補：{q:?}");
            }
            crate::lifecycle::stop_bot(&e.app, &bot.id).await.unwrap();
        }
    }

    /// 閒著但還有一回合沒收（hook 還沒到）也算忙。
    #[tokio::test]
    async fn an_open_turn_counts_as_busy() {
        let e = env().await;
        let strict = StartOpts { resume_native: true, resume_required: true, ..Default::default() };
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
    /// 最後是 idle 的、使用者自己停的（`stopped`）不補。
    #[tokio::test]
    async fn a_start_after_the_pane_died_mid_work_is_nudged() {
        let e = env().await;
        let strict = StartOpts { resume_native: true, resume_required: true, ..Default::default() };
        for (state, status, want) in [("exited", "working", 1), ("exited", "blocked", 1), ("exited", "idle", 0), ("stopped", "working", 0)] {
            let bot = claude_bot(&e.app, &e.project_id, &format!("s-{state}-{status}")).await;
            resumable(&e, &bot.id, state, status).await;
            crate::lifecycle::start_bot_with(&e.app, &bot.id, strict.clone()).await.unwrap();
            assert_eq!(queued(&e, &bot.id).await.len(), want, "{state}/{status}");
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
