//! Background sweeper for claude-api bots.
//!
//! Periodically detects and settles:
//! 1. Abandoned pending checkpoints after owner turn has ended/failed.
//! 2. Reserved attempts exceeding the turn hard deadline.
//! 3. Stale unknown attempts exceeding N minutes (clearing checkpoint pending to let bot take next turn).
//!
//! Safety invariants:
//! - Never replays requests.
//! - Never replays tools.
//! - Preserves conservative unknown accounting.
//! - Emits warn logs on every settlement.

use std::{sync::Arc, time::Duration};
use am_base::db::api_bot::Checkpoint;
use serde_json::{json, Value};

pub(crate) const SWEEP_INTERVAL: Duration = Duration::from_secs(60);
pub(crate) const UNKNOWN_STALE_SECS: i64 = 300; // 5 minutes

#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct SweepReport {
    pub(crate) abandoned_checkpoints: usize,
    pub(crate) reserved_timeouts: usize,
    pub(crate) stale_unknowns: usize,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub(crate) struct StuckInfo {
    pub(crate) reason: &'static str,
    pub(crate) message: &'static str,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub(crate) struct StuckBotEntry {
    pub(crate) bot_id: String,
    pub(crate) name: String,
    pub(crate) reason: String,
    pub(crate) message: String,
}

pub(crate) fn spawn_sweeper(app: Arc<crate::state::App>) {
    crate::background_loop::spawn_periodic(
        &app,
        "API bot sweeper",
        SWEEP_INTERVAL,
        SWEEP_INTERVAL,
        |app| async move {
            if let Err(e) = sweep_once(&app).await {
                tracing::warn!(error = ?e, "API bot sweeper iteration failed");
            }
        },
    );
}

pub(crate) async fn sweep_once(app: &crate::state::App) -> anyhow::Result<SweepReport> {
    if app.shutdown.is_cancelled() {
        return Ok(SweepReport::default());
    }
    let mut report = SweepReport::default();

    // 1. Abandoned checkpoints after owner turn failed / ended
    sweep_abandoned_pending_checkpoints(app, &mut report).await?;

    // 2. Reserved attempts exceeding hard deadline
    sweep_reserved_attempts_exceeding_hard_deadline(app, &mut report).await?;

    // 3. Stale unknown attempts older than N minutes
    sweep_stale_unknown_attempts(app, &mut report).await?;

    Ok(report)
}

async fn sweep_abandoned_pending_checkpoints(
    app: &crate::state::App,
    report: &mut SweepReport,
) -> anyhow::Result<()> {
    let sessions: Vec<(String, String, Option<String>, Option<String>, i64, i64, String)> =
        sqlx::query_as(
            "SELECT s.id, s.bot_id, s.owner_run_id, s.owner_turn_id, s.generation, s.revision, s.checkpoint_json \
             FROM api_bot_sessions s \
             JOIN bots b ON b.id = s.bot_id \
             WHERE b.kind = 'claude-api' AND b.deleted_at IS NULL AND s.retired_at IS NULL",
        )
        .fetch_all(&app.db)
        .await?;

    for (session_id, bot_id, owner_run_id, owner_turn_id, _generation, revision, checkpoint_json) in sessions {
        let Ok(val) = serde_json::from_str::<Value>(&checkpoint_json) else { continue };
        let api_pending = val["api_pending"] == true;
        let tool_pending = !val["tool_pending"].is_null();
        if !api_pending && !tool_pending {
            continue;
        }

        // Check if owner turn is still actively running:
        let is_active = if let Some(turn_id) = &owner_turn_id {
            let turn_row: Option<(String, Option<String>, Option<String>)> = sqlx::query_as(
                "SELECT t.status, t.run_id, r.state FROM turns t LEFT JOIN runs r ON r.id = t.run_id WHERE t.id = ?",
            )
            .bind(turn_id)
            .fetch_optional(&app.db)
            .await?;
            match turn_row {
                Some((status, run_id, run_state)) => {
                    let turn_active = status == "queued" || status == "in_flight";
                    let run_active = run_state.as_deref().map_or(false, |s| s == "starting" || s == "running");
                    let same_run = run_id.as_deref() == owner_run_id.as_deref();
                    turn_active && run_active && same_run
                }
                None => false,
            }
        } else {
            false
        };

        if is_active || app.api_bot_runtime.has_live_turn(&bot_id) {
            continue;
        }

        // Owner turn is ended or failed, but checkpoint is still pending. Settle safely:
        let mut tx = crate::db::begin_write(&app.db).await?;

        // 1. Close reserved attempts on this session:
        let attempts: Vec<(String, Option<String>)> = sqlx::query_as(
            "SELECT id, turn_id FROM api_bot_attempts WHERE session_id = ? AND state = 'reserved'",
        )
        .bind(&session_id)
        .fetch_all(&mut *tx)
        .await?;

        let now = crate::db::now();
        for (att_id, att_turn) in attempts {
            let has_response = if let Some(t_id) = &att_turn {
                sqlx::query_scalar::<_, bool>(
                    "SELECT EXISTS(SELECT 1 FROM messages WHERE turn_id = ? AND role = 'assistant' AND source = 'transcript')",
                )
                .bind(t_id)
                .fetch_one(&mut *tx)
                .await
                .unwrap_or(false)
            } else {
                false
            };
            let new_state = if has_response { "recorded" } else { "unknown" };
            sqlx::query(
                "UPDATE api_bot_attempts SET state = ?, finished_at = COALESCE(finished_at, ?) WHERE id = ? AND state = 'reserved'",
            )
            .bind(new_state)
            .bind(&now)
            .bind(&att_id)
            .execute(&mut *tx)
            .await?;
            tracing::warn!(attempt_id = %att_id, bot_id = %bot_id, state = new_state, "API bot sweeper closed reserved attempt on ended owner turn");
        }

        // 2. Apply any pending join receipts:
        let receipts: Vec<(String, String)> = sqlx::query_as(
            "SELECT turn_id, state FROM api_bot_join_receipts WHERE session_id = ? AND state = 'pending'",
        )
        .bind(&session_id)
        .fetch_all(&mut *tx)
        .await?;

        for (rec_turn, _) in receipts {
            let turn_status: Option<String> = sqlx::query_scalar(
                "SELECT status FROM turns WHERE id = ?",
            )
            .bind(&rec_turn)
            .fetch_optional(&mut *tx)
            .await?;
            let outcome = if turn_status.as_deref() == Some("completed") { "completed" } else { "failed" };
            sqlx::query(
                "UPDATE api_bot_join_receipts SET state = 'applied', outcome = ?, processed_at = ? WHERE session_id = ? AND turn_id = ? AND state = 'pending'",
            )
            .bind(outcome)
            .bind(&now)
            .bind(&session_id)
            .bind(&rec_turn)
            .execute(&mut *tx)
            .await?;
        }

        // 3. Clear api_pending and synthesize tool_result for tool_pending:
        let mut new_val = val.clone();
        new_val["api_pending"] = json!(false);
        if let Some(tool_pending_id) = new_val["tool_pending"].as_str().map(|s| s.to_string()) {
            new_val["tool_pending"] = json!(null);
            let mut completed: Vec<String> = new_val["completed_tool_ids"]
                .as_array()
                .map(|arr| arr.iter().filter_map(|v| v.as_str().map(String::from)).collect())
                .unwrap_or_default();
            if !completed.contains(&tool_pending_id) {
                completed.push(tool_pending_id.clone());
            }
            new_val["completed_tool_ids"] = json!(completed);

            let mut messages = new_val["messages"].as_array().cloned().unwrap_or_default();
            let already_returned = messages.iter().any(|m| {
                m.get("content").and_then(|c| c.as_array()).map_or(false, |blocks| {
                    blocks.iter().any(|b| b.get("tool_use_id").and_then(|v| v.as_str()) == Some(&tool_pending_id))
                })
            });
            if !already_returned {
                let tool_result_block = json!({
                    "type": "tool_result",
                    "tool_use_id": tool_pending_id,
                    "content": "not executed: turn ended",
                    "is_error": true
                });
                messages.push(json!({
                    "role": "user",
                    "content": [tool_result_block]
                }));
                new_val["messages"] = json!(messages);
            }
        }

        let Ok(chk) = Checkpoint::new(&new_val) else {
            tx.rollback().await?;
            continue;
        };
        let encoded = serde_json::to_string(&chk.value())?;
        let next_rev = revision + 1;
        let changed = sqlx::query(
            "UPDATE api_bot_sessions SET checkpoint_json = ?, revision = ?, updated_at = ? WHERE id = ? AND revision = ?",
        )
        .bind(&encoded)
        .bind(next_rev)
        .bind(&now)
        .bind(&session_id)
        .bind(revision)
        .execute(&mut *tx)
        .await?
        .rows_affected();

        if changed == 1 {
            tx.commit().await?;
            tracing::warn!(
                bot_id = %bot_id,
                session_id = %session_id,
                owner_turn_id = ?owner_turn_id,
                "API bot sweeper cleared abandoned pending checkpoint"
            );
            app.api_bot_runtime.remove_session_cursor(&bot_id, &session_id);
            report.abandoned_checkpoints += 1;
        } else {
            tx.rollback().await?;
        }
    }

    Ok(())
}

async fn sweep_reserved_attempts_exceeding_hard_deadline(
    app: &crate::state::App,
    report: &mut SweepReport,
) -> anyhow::Result<()> {
    let hard_deadline_secs = app.cfg.get().await.claude_api.api_bot.turn_timeout_secs + 120;
    let cutoff = crate::db::iso_in(-(hard_deadline_secs as i64));
    let stale_reserved: Vec<(String, Option<String>, Option<String>, Option<String>, String)> =
        sqlx::query_as(
            "SELECT id, bot_id, session_id, turn_id, created_at FROM api_bot_attempts WHERE state = 'reserved' AND created_at < ?",
        )
        .bind(&cutoff)
        .fetch_all(&app.db)
        .await?;

    for (att_id, bot_id, session_id, turn_id, att_created_at) in stale_reserved {
        let mut tx = crate::db::begin_write(&app.db).await?;
        let has_response = if let Some(t_id) = &turn_id {
            sqlx::query_scalar::<_, bool>(
                "SELECT EXISTS(SELECT 1 FROM messages WHERE turn_id = ? AND role = 'assistant' AND source = 'transcript')",
            )
            .bind(t_id)
            .fetch_one(&mut *tx)
            .await
            .unwrap_or(false)
        } else {
            false
        };
        let new_state = if has_response { "recorded" } else { "unknown" };
        let now = crate::db::now();
        let changed = sqlx::query(
            "UPDATE api_bot_attempts SET state = ?, finished_at = COALESCE(finished_at, ?) WHERE id = ? AND state = 'reserved'",
        )
        .bind(new_state)
        .bind(&now)
        .bind(&att_id)
        .execute(&mut *tx)
        .await?
        .rows_affected();

        if changed == 1 {
            if let Some(s_id) = &session_id {
                let session_row: Option<(i64, i64, String, String)> = sqlx::query_as(
                    "SELECT revision, generation, checkpoint_json, updated_at FROM api_bot_sessions WHERE id = ? AND retired_at IS NULL",
                )
                .bind(s_id)
                .fetch_optional(&mut *tx)
                .await?;

                if let Some((rev, generation, chk_raw, updated_at)) = session_row {
                    // 過期的 reserved 只結清帳（上面已做）；session 現在有人在用（回合在跑、更新的 attempt）就不動 checkpoint。
                    let busy = session_busy(app, s_id, bot_id.as_deref(), Some(&att_id), &att_created_at, &updated_at).await?;
                    if let (false, Ok(mut val)) = (busy, serde_json::from_str::<Value>(&chk_raw)) {
                        if val["api_pending"] == true {
                            val["api_pending"] = json!(false);
                            if let Ok(chk) = Checkpoint::new(&val) {
                                let next_rev = rev + 1;
                                let encoded = serde_json::to_string(&chk.value())?;
                                let wrote = sqlx::query(
                                    "UPDATE api_bot_sessions SET checkpoint_json = ?, revision = ?, updated_at = ? \
                                     WHERE id = ? AND revision = ? AND generation = ? AND updated_at = ?",
                                )
                                .bind(&encoded)
                                .bind(next_rev)
                                .bind(&now)
                                .bind(s_id)
                                .bind(rev)
                                .bind(generation)
                                .bind(&updated_at)
                                .execute(&mut *tx)
                                .await?
                                .rows_affected();

                                if wrote == 1 {
                                    if let Some(b_id) = &bot_id {
                                        app.api_bot_runtime.remove_session_cursor(b_id, s_id);
                                    }
                                }
                            }
                        }
                    }
                }
            }
            tx.commit().await?;
            tracing::warn!(
                attempt_id = %att_id,
                bot_id = ?bot_id,
                state = new_state,
                "API bot sweeper settled reserved attempt exceeding hard deadline"
            );
            report.reserved_timeouts += 1;
        } else {
            tx.rollback().await?;
        }
    }

    Ok(())
}

async fn sweep_stale_unknown_attempts(
    app: &crate::state::App,
    report: &mut SweepReport,
) -> anyhow::Result<()> {
    let cutoff = crate::db::iso_in(-UNKNOWN_STALE_SECS);
    let stale_unknowns: Vec<(String, String, Option<String>, i64, i64, String, String, String)> = sqlx::query_as(
        "SELECT DISTINCT a.id, s.id, s.bot_id, s.revision, s.generation, s.checkpoint_json, s.updated_at, a.created_at \
         FROM api_bot_attempts a \
         JOIN api_bot_sessions s ON s.id = a.session_id \
         WHERE a.state = 'unknown' \
           AND COALESCE(a.finished_at, a.created_at) < ? \
           AND s.retired_at IS NULL",
    )
    .bind(&cutoff)
    .fetch_all(&app.db)
    .await?;

    for (att_id, session_id, bot_id, revision, generation, checkpoint_json, session_updated_at, att_created_at) in stale_unknowns {
        // 一筆舊的 unknown 只是帳務未知（保守計入花費），跟**目前**的 checkpoint 無關：只有整個 session 真的靜止才碰 checkpoint。
        // 2026-11-15 事故：舊回合的 ordinal 55 unknown 讓 sweeper 在新回合的請求進行中把 api_pending 清掉，回合 journal 因此寫失敗。
        // 判斷一律看**目前**狀態：這顆 bot 有沒有回合在跑、session 有沒有還在進行的 reserved、有沒有比這筆更新的 attempt、
        // session 最近有沒有被寫過。任何一個成立就放著不動。
        if session_busy(app, &session_id, bot_id.as_deref(), Some(&att_id), &att_created_at, &session_updated_at).await? {
            continue;
        }
        let Ok(mut val) = serde_json::from_str::<Value>(&checkpoint_json) else { continue };
        if val["api_pending"] != true {
            continue;
        }
        val["api_pending"] = json!(false);
        let Ok(chk) = Checkpoint::new(&val) else { continue };
        let mut tx = crate::db::begin_write(&app.db).await?;
        let next_rev = revision + 1;
        let now = crate::db::now();
        let encoded = serde_json::to_string(&chk.value())?;
        // 與 runtime 同一道圍欄：revision＋generation 都要對得上，而且 session 還沒被別人寫過（updated_at 不變）；對不上就放棄。
        let changed = sqlx::query(
            "UPDATE api_bot_sessions SET checkpoint_json = ?, revision = ?, updated_at = ? \
             WHERE id = ? AND revision = ? AND generation = ? AND updated_at = ? AND retired_at IS NULL",
        )
        .bind(&encoded)
        .bind(next_rev)
        .bind(&now)
        .bind(&session_id)
        .bind(revision)
        .bind(generation)
        .bind(&session_updated_at)
        .execute(&mut *tx)
        .await?
        .rows_affected();

        if changed == 1 {
            tx.commit().await?;
            tracing::warn!(
                attempt_id = %att_id,
                bot_id = ?bot_id,
                session_id = %session_id,
                "API bot sweeper cleared api_pending for stale unknown attempt"
            );
            if let Some(b_id) = &bot_id {
                app.api_bot_runtime.remove_session_cursor(b_id, &session_id);
            }
            report.stale_unknowns += 1;
        } else {
            tx.rollback().await?;
        }
    }

    Ok(())
}

/// session 現在是不是「有人在用」：只要其中一個成立，sweeper 就不准改它的 checkpoint。
/// - 這顆 bot 的 runtime 有回合在跑；
/// - session 上有還沒過硬期限的 reserved attempt（請求進行中）；
/// - 有比 `unknown_att` 更新的 attempt（那筆 unknown 已經不是這個 session 的現況）；
/// - session 最近 [`UNKNOWN_STALE_SECS`] 內被寫過（回合在推進 checkpoint）。
async fn session_busy(
    app: &crate::state::App,
    session_id: &str,
    bot_id: Option<&str>,
    unknown_att: Option<&str>,
    unknown_created_at: &str,
    session_updated_at: &str,
) -> anyhow::Result<bool> {
    if bot_id.is_some_and(|b| app.api_bot_runtime.has_live_turn(b)) {
        return Ok(true);
    }
    if crate::db::cmp_ts(session_updated_at, &crate::db::iso_in(-UNKNOWN_STALE_SECS)).is_ge() {
        return Ok(true);
    }
    let hard = app.cfg.get().await.claude_api.api_bot.turn_timeout_secs as i64 + 120;
    let fresh_reserved: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM api_bot_attempts WHERE session_id = ? AND state = 'reserved' AND created_at >= ?)",
    )
    .bind(session_id)
    .bind(crate::db::iso_in(-hard))
    .fetch_one(&app.db)
    .await?;
    if fresh_reserved {
        return Ok(true);
    }
    let newer: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM api_bot_attempts WHERE session_id = ? AND id != ? AND created_at > ?)",
    )
    .bind(session_id)
    .bind(unknown_att.unwrap_or(""))
    .bind(unknown_created_at)
    .fetch_one(&app.db)
    .await?;
    Ok(newer)
}

pub(crate) async fn detect_stuck(app: &crate::state::App, bot_id: &str) -> Option<StuckInfo> {
    if !app.api_startup_gate.ready() {
        return Some(StuckInfo {
            reason: "startup_gate_quarantined",
            message: "API 啟動閘已隔離",
        });
    }
    if app.api_bot_runtime.explicit_retire_quarantined(bot_id) {
        return Some(StuckInfo {
            reason: "bot_quarantined",
            message: "Bot 已被安全隔離",
        });
    }

    let hard_deadline_secs = app.cfg.get().await.claude_api.api_bot.turn_timeout_secs + 120;
    let cutoff = crate::db::iso_in(-(hard_deadline_secs as i64));
    let has_stalled_reserved: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM api_bot_attempts WHERE bot_id = ? AND state = 'reserved' AND created_at < ?)",
    )
    .bind(bot_id)
    .bind(&cutoff)
    .fetch_one(&app.db)
    .await
    .unwrap_or(false);

    if has_stalled_reserved {
        return Some(StuckInfo {
            reason: "reserved_attempt_timeout",
            message: "請求預留逾時未有結果",
        });
    }

    let cutoff_5m = crate::db::iso_in(-UNKNOWN_STALE_SECS);
    let session: Option<(String, Option<String>, String)> = sqlx::query_as(
        "SELECT s.checkpoint_json, s.owner_turn_id, s.updated_at \
         FROM api_bot_sessions s \
         WHERE s.bot_id = ? AND s.retired_at IS NULL \
         ORDER BY s.updated_at DESC LIMIT 1",
    )
    .bind(bot_id)
    .fetch_optional(&app.db)
    .await
    .unwrap_or(None);

    if let Some((raw_chk, owner_turn_id, session_updated_at)) = session {
        if let Ok(val) = serde_json::from_str::<Value>(&raw_chk) {
            let api_pending = val["api_pending"] == true;
            let tool_pending = !val["tool_pending"].is_null();
            if api_pending || tool_pending {
                let owner_ended = if let Some(t_id) = &owner_turn_id {
                    let status: Option<String> = sqlx::query_scalar(
                        "SELECT status FROM turns WHERE id = ?",
                    )
                    .bind(t_id)
                    .fetch_optional(&app.db)
                    .await
                    .unwrap_or(None);
                    status.map_or(true, |s| s != "queued" && s != "in_flight")
                } else {
                    true
                };
                if owner_ended {
                    return Some(StuckInfo {
                        reason: "checkpoint_pending_after_owner_exit",
                        message: "回合結束但仍有未結算標記",
                    });
                }
                let has_stale_unknown: bool = sqlx::query_scalar(
                    "SELECT EXISTS(SELECT 1 FROM api_bot_attempts WHERE bot_id = ? AND state = 'unknown' AND COALESCE(finished_at, created_at) < ?)",
                )
                .bind(bot_id)
                .bind(&cutoff_5m)
                .fetch_one(&app.db)
                .await
                .unwrap_or(false);
                // 回合在跑、或 session 最近才被寫過＝不是卡住（跟 sweeper 的 `session_busy` 同一條）：舊的 unknown 只是帳務未知。
                let moving = app.api_bot_runtime.has_live_turn(bot_id) || crate::db::cmp_ts(&session_updated_at, &cutoff_5m).is_ge();
                if has_stale_unknown && !moving {
                    return Some(StuckInfo {
                        reason: "unknown_attempt_stuck",
                        message: "未知結果請求卡住未結清",
                    });
                }
            }
        }
    }
    None
}

pub(crate) async fn stuck_api_bots(app: &crate::state::App) -> Vec<StuckBotEntry> {
    let bots: Vec<(String, String)> = sqlx::query_as(
        "SELECT id, name FROM bots WHERE kind = 'claude-api' AND deleted_at IS NULL",
    )
    .fetch_all(&app.db)
    .await
    .unwrap_or_default();

    let mut stuck_list = Vec::new();
    for (bot_id, name) in bots {
        if let Some(stuck) = detect_stuck(app, &bot_id).await {
            stuck_list.push(StuckBotEntry {
                bot_id,
                name,
                reason: stuck.reason.to_string(),
                message: stuck.message.to_string(),
            });
        }
    }
    stuck_list
}
