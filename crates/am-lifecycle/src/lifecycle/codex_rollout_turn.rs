//! Native Codex task boundaries for reply recovery (#1994). No file or DB access.
use chrono::{DateTime, FixedOffset};
use serde_json::Value;

pub(super) enum Proof {
    /// Historical logs without task_started retain the existing strict parser.
    Legacy,
    Native(Option<NativeOutcome>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::lifecycle) enum NativeOutcome { Reply(String), Error { native_id: String, message: String, reply: Option<String> } }

fn row(line: &str) -> Option<Value> {
    serde_json::from_str(line).ok()
}
fn event(v: &Value) -> Option<&str> {
    (v.get("type")?.as_str()? == "event_msg")
        .then(|| v.pointer("/payload/type")?.as_str())
        .flatten()
}
fn id(v: &Value) -> Option<&str> {
    v.pointer("/payload/turn_id")?
        .as_str()
        .filter(|s| !s.trim().is_empty())
}
fn at(v: &Value) -> Option<DateTime<FixedOffset>> {
    DateTime::parse_from_rfc3339(v.get("timestamp")?.as_str()?).ok()
}
fn abort(e: Option<&str>) -> bool {
    matches!(e, Some("task_aborted" | "turn_aborted" | "turn_cancelled"))
}

fn native_boundary(v: &Value) -> bool {
    event(v) == Some("task_started")
        || abort(event(v))
        || (event(v) == Some("task_complete") && v.pointer("/payload/turn_id").is_some())
}

/// File append order is not sufficient: a replacement can be appended late.
fn ordered_after(lines: &[&str], completed: DateTime<FixedOffset>, native: &str) -> Option<()> {
    let mut clock = completed;
    for line in lines {
        let v = row(line)?;
        let e = event(&v);
        if matches!(e, Some("task_started" | "task_complete"))
            || abort(e)
            || crate::lifecycle::delivery::codex_user_text(line).is_some()
        {
            let t = at(&v)?;
            if t < clock {
                return None;
            }
            clock = t;
            if e == Some("task_started") && (id(&v).is_none() || id(&v) == Some(native)) {
                return None;
            }
            if abort(e) && id(&v) == Some(native) {
                return None;
            }
        }
    }
    Some(())
}

/// The prompt search remains in codex_rollout.rs. Only relax its user-input guard
/// when a complete, unbroken native task proves that the extra input is steering.
pub(super) fn for_prompt(
    lines: &[&str],
    prompt: usize,
    sent: &[String],
    since: &DateTime<FixedOffset>,
) -> Proof {
    let Some(begin) = (0..prompt)
        .rev()
        .find(|&i| row(lines[i]).as_ref().and_then(event) == Some("task_started"))
    else {
        return if lines[prompt + 1..]
            .iter()
            .any(|line| row(line).as_ref().is_some_and(|v| native_boundary(v)))
        {
            Proof::Native(None)
        } else {
            Proof::Legacy
        };
    };
    let prove = || -> Option<NativeOutcome> {
        // The unchanged caller searches the last matching prompt. A second matching
        // submission in a different native task must not steal an older AGM turn.
        let mut matched_texts = Vec::new();
        for (index, line) in lines[..=prompt].iter().enumerate() {
            let Some(text) = crate::lifecycle::delivery::codex_user_text(line) else {
                continue;
            };
            if !sent.contains(&text)
                || !row(line).as_ref().and_then(at).is_some_and(|t| t >= *since)
            {
                continue;
            }
            if index < begin || matched_texts.contains(&text) {
                return None;
            }
            matched_texts.push(text);
        }
        if matched_texts.is_empty() {
            return None;
        }
        let start = row(lines[begin])?;
        let native = id(&start)?;
        let mut clock = at(&start)?;
        let mut matched = 0;
        // Repeated identical submissions inside the recovery window cannot prove
        // which AGM turn owns the response. A later turn's since can exclude the old one.
        for line in &lines[begin + 1..=prompt] {
            let v = row(line)?;
            if event(&v) == Some("task_started")
                || event(&v) == Some("task_complete")
                || abort(event(&v))
            {
                return None;
            }
            if let Some(text) = crate::lifecycle::delivery::codex_user_text(line) {
                let t = at(&v)?;
                if t < clock {
                    return None;
                }
                clock = t;
                if t >= *since && sent.contains(&text) {
                    matched += 1;
                }
            }
        }
        if matched == 0 {
            return None;
        }
        for (offset, line) in lines[prompt + 1..].iter().enumerate() {
            let Some(v) = row(line) else { continue };
            let e = event(&v);
            if e == Some("task_started") || abort(e) {
                return None;
            }
            if crate::lifecycle::delivery::codex_user_text(line).is_some() {
                let t = at(&v)?;
                if t < clock {
                    return None;
                }
                clock = t;
            }
            if e == Some("task_complete") {
                if id(&v) != Some(native) {
                    return None;
                }
                let t = at(&v)?;
                if t < clock || t < *since {
                    return None;
                }
                if let Some(done) = v.pointer("/payload/completed_at").and_then(Value::as_i64) {
                    let done = chrono::DateTime::from_timestamp(done, 0)?;
                    if done < clock || done < *since {
                        return None;
                    }
                }
                ordered_after(&lines[prompt + 1 + offset + 1..], t, native)?;
                // A later *different* native task does not invalidate this proven
                // completion. The caller already selected the last matching prompt.
                let reply = v.pointer("/payload/last_agent_message").and_then(Value::as_str).filter(|s| !s.trim().is_empty());
                let error = v.pointer("/payload/error").filter(|e| !e.is_null());
                return match (reply, error) {
                    (Some(text), None) => Some(NativeOutcome::Reply(text.to_owned())),
                    (reply, Some(error)) => error.get("message").and_then(Value::as_str)
                        .filter(|s| !s.trim().is_empty()).map(|s| NativeOutcome::Error { native_id: native.to_owned(), message: s.to_owned(), reply: reply.map(str::to_owned) }),
                    _ => None,
                };
            }
        }
        None
    };
    Proof::Native(prove())
}

/// Hookless recovery has no prompt owner. Never consume an earlier completion
/// after a later native task/input has started; preserve its stricter contract.
pub(super) fn for_latest(lines: &[&str], complete: usize, since: &DateTime<FixedOffset>) -> Proof {
    let Some(begin) = (0..complete)
        .rev()
        .find(|&i| row(lines[i]).as_ref().and_then(event) == Some("task_started"))
    else {
        return if lines[complete..]
            .iter()
            .any(|line| row(line).as_ref().is_some_and(|v| native_boundary(v)))
        {
            Proof::Native(None)
        } else {
            Proof::Legacy
        };
    };
    let prove = || -> Option<String> {
        let start = row(lines[begin])?;
        let native = id(&start)?;
        let mut clock = at(&start)?;
        for (offset, line) in lines[begin + 1..=complete].iter().enumerate() {
            let v = row(line)?;
            let e = event(&v);
            if e == Some("task_started") || abort(e) {
                return None;
            }
            if crate::lifecycle::delivery::codex_user_text(line).is_some() {
                let t = at(&v)?;
                if t < clock {
                    return None;
                }
                clock = t;
            }
            if e == Some("task_complete") {
                if begin + 1 + offset != complete {
                    return None;
                }
                if id(&v) != Some(native) || at(&v)? < clock || at(&v)? <= *since {
                    return None;
                }
                if let Some(done) = v.pointer("/payload/completed_at").and_then(Value::as_i64) {
                    let done = chrono::DateTime::from_timestamp(done, 0)?;
                    if done < clock || done <= *since {
                        return None;
                    }
                }
            }
        }
        for line in &lines[complete + 1..] {
            let Some(v) = row(line) else { continue };
            if event(&v) == Some("task_started")
                || abort(event(&v))
                || crate::lifecycle::delivery::codex_user_text(line).is_some()
            {
                return None;
            }
        }
        let v = row(lines[complete])?;
        v.pointer("/payload/last_agent_message")?
            .as_str()
            .filter(|s| !s.trim().is_empty())
            .map(str::to_owned)
    };
    Proof::Native(prove().map(NativeOutcome::Reply))
}

#[cfg(test)]
mod tests {
    use super::super::{codex_latest_reply_since, codex_reply_for_turn};
    use serde_json::json;
    fn event(t: &str, kind: &str, id: Option<&str>) -> String {
        json!({"type":"event_msg","timestamp":format!("2026-10-25T10:{t}Z"),"payload":{"type":kind,"turn_id":id,"last_agent_message":"完整答案"}}).to_string()
    }
    fn user(t: &str, text: &str) -> String {
        json!({"type":"response_item","timestamp":format!("2026-10-25T10:{t}Z"),"payload":{"type":"message","role":"user","content":[{"type":"input_text","text":text}]}}).to_string()
    }
    fn log(extra: Vec<String>) -> String {
        extra.join("\n")
    }
    fn recover(log: &str, prompt: &str) -> Option<String> {
        codex_reply_for_turn(log, &[prompt.into()], "2026-10-25T10:00:00Z")
    }
    #[test]
    fn steering_inside_one_native_task_keeps_the_complete_reply() {
        let l = log(vec![
            event("01:00", "task_started", Some("a")),
            user("01:01", "原問題"),
            user("01:02", "追加指示"),
            event("01:03", "task_complete", Some("a")),
        ]);
        assert_eq!(recover(&l, "原問題"), Some("完整答案".into()));
    }
    #[test]
    fn next_native_task_does_not_invalidate_an_already_proven_reply() {
        let l = log(vec![
            event("01:00", "task_started", Some("a")),
            user("01:01", "原問題"),
            event("01:03", "task_complete", Some("a")),
            event("01:04", "task_started", Some("b")),
            user("01:05", "下一題"),
        ]);
        assert_eq!(recover(&l, "原問題"), Some("完整答案".into()));
        assert_eq!(recover(&l, "下一題"), None);
        assert_eq!(codex_latest_reply_since(&l, "2026-10-25T10:00:00Z"), None);
    }
    #[test]
    fn a_new_task_or_abort_before_completion_rejects_the_old_reply() {
        for boundary in [
            event("01:02", "task_started", Some("b")),
            event("01:02", "task_aborted", Some("a")),
        ] {
            let l = log(vec![
                event("01:00", "task_started", Some("a")),
                user("01:01", "原問題"),
                boundary,
                event("01:03", "task_complete", Some("a")),
            ]);
            assert_eq!(recover(&l, "原問題"), None);
        }
    }
    #[test]
    fn missing_mismatched_or_blank_native_ids_do_not_relax_the_guard() {
        for (start, end) in [
            (None, Some("a")),
            (Some("a"), None),
            (Some("a"), Some("b")),
            (Some(""), Some("")),
        ] {
            let l = log(vec![
                event("01:00", "task_started", start),
                user("01:01", "原問題"),
                user("01:02", "追加"),
                event("01:03", "task_complete", end),
            ]);
            assert_eq!(recover(&l, "原問題"), None);
        }
    }
    #[test]
    fn repeated_prompt_is_ambiguous_until_the_new_turns_time_window() {
        let l = log(vec![
            event("01:00", "task_started", Some("a")),
            user("01:01", "同文"),
            user("01:02", "同文"),
            event("01:03", "task_complete", Some("a")),
        ]);
        assert_eq!(recover(&l, "同文"), None);
        assert_eq!(
            codex_reply_for_turn(&l, &["同文".into()], "2026-10-25T10:01:01.500Z"),
            Some("完整答案".into())
        );
    }
    #[test]
    fn reversed_and_missing_timestamps_are_not_native_proof() {
        let l = log(vec![
            event("01:00", "task_started", Some("a")),
            user("01:02", "原問題"),
            event("01:01", "task_complete", Some("a")),
        ]);
        assert_eq!(recover(&l, "原問題"), None);
        let mut missing: serde_json::Value =
            serde_json::from_str(&event("01:03", "task_complete", Some("a"))).unwrap();
        missing.as_object_mut().unwrap().remove("timestamp");
        let l = log(vec![
            event("01:00", "task_started", Some("a")),
            user("01:01", "原問題"),
            missing.to_string(),
        ]);
        assert_eq!(recover(&l, "原問題"), None);
    }
    #[test]
    fn native_hookless_completion_requires_the_same_task_id() {
        let l = log(vec![
            event("01:00", "task_started", Some("a")),
            user("01:01", "原問題"),
            user("01:02", "追加"),
            event("01:03", "task_complete", Some("a")),
        ]);
        assert_eq!(
            codex_latest_reply_since(&l, "2026-10-25T10:00:00Z"),
            Some("完整答案".into())
        );
        let l = log(vec![
            event("01:00", "task_started", Some("a")),
            user("01:01", "原問題"),
            event("01:03", "task_complete", Some("b")),
        ]);
        assert_eq!(codex_latest_reply_since(&l, "2026-10-25T10:00:00Z"), None);
    }
    #[test]
    fn later_repeated_prompt_in_a_different_task_stays_incomplete() {
        let l = log(vec![
            event("01:00", "task_started", Some("a")),
            user("01:01", "同文"),
            event("01:03", "task_complete", Some("a")),
            event("01:04", "task_started", Some("b")),
            user("01:05", "同文"),
        ]);
        assert_eq!(recover(&l, "同文"), None);
    }
    #[test]
    fn a_later_completed_same_prompt_cannot_steal_an_older_agm_turn() {
        let l = log(vec![
            event("01:00", "task_started", Some("a")),
            user("01:01", "同文"),
            event("01:03", "task_complete", Some("a")),
            event("01:04", "task_started", Some("b")),
            user("01:05", "同文"),
            event("01:06", "task_complete", Some("b")),
        ]);
        assert_eq!(recover(&l, "同文"), None);
        assert_eq!(
            codex_reply_for_turn(&l, &["同文".into()], "2026-10-25T10:01:04Z"),
            Some("完整答案".into())
        );
    }
    #[test]
    fn different_supplements_owned_by_one_agm_turn_are_not_duplicate_submissions() {
        let l = log(vec![
            event("01:00", "task_started", Some("a")),
            user("01:01", "原問題"),
            user("01:02", "追加"),
            event("01:03", "task_complete", Some("a")),
        ]);
        assert_eq!(
            codex_reply_for_turn(
                &l,
                &["原問題".into(), "追加".into()],
                "2026-10-25T10:00:00Z"
            ),
            Some("完整答案".into())
        );
    }
    #[test]
    fn missing_start_does_not_hide_a_later_native_replacement() {
        let l = log(vec![
            user("01:01", "原問題"),
            event("01:02", "task_started", Some("b")),
            event("01:03", "task_complete", Some("b")),
        ]);
        assert_eq!(recover(&l, "原問題"), None);
    }
    #[test]
    fn native_complete_without_its_start_is_not_legacy_evidence() {
        let l = log(vec![
            user("01:01", "原問題"),
            event("01:03", "task_complete", Some("old")),
        ]);
        assert_eq!(recover(&l, "原問題"), None);
        assert_eq!(codex_latest_reply_since(&l, "2026-10-25T10:00:00Z"), None);
    }
    #[test]
    fn later_appended_start_with_an_earlier_time_cannot_validate_a_completion() {
        let l = log(vec![
            event("01:00", "task_started", Some("a")),
            user("01:01", "原問題"),
            event("01:03", "task_complete", Some("a")),
            event("01:02", "task_started", Some("b")),
            user("01:04", "別題"),
        ]);
        assert_eq!(recover(&l, "原問題"), None);
    }
    #[test]
    fn a_later_native_boundary_requires_a_valid_timestamp() {
        let mut b: serde_json::Value =
            serde_json::from_str(&event("01:04", "task_started", Some("b"))).unwrap();
        b.as_object_mut().unwrap().remove("timestamp");
        let l = log(vec![
            event("01:00", "task_started", Some("a")),
            user("01:01", "原問題"),
            event("01:03", "task_complete", Some("a")),
            b.to_string(),
        ]);
        assert_eq!(recover(&l, "原問題"), None);
    }
    #[test]
    fn unix_completion_seconds_do_not_erase_millisecond_boundaries() {
        let since = "2026-10-25T10:01:01.900Z";
        let mut complete: serde_json::Value =
            serde_json::from_str(&event("01:02.000", "task_complete", Some("a"))).unwrap();
        complete["payload"]["completed_at"] =
            serde_json::json!(chrono::DateTime::parse_from_rfc3339("2026-10-25T10:01:01Z")
                .unwrap()
                .timestamp());
        let l = log(vec![
            event("01:01.800", "task_started", Some("a")),
            user("01:01.950", "原問題"),
            complete.to_string(),
        ]);
        assert_eq!(codex_reply_for_turn(&l, &["原問題".into()], since), None);
        assert_eq!(codex_latest_reply_since(&l, since), None);
        complete["payload"]["completed_at"] =
            serde_json::json!(chrono::DateTime::parse_from_rfc3339("2026-10-25T10:01:02Z")
                .unwrap()
                .timestamp());
        let l = log(vec![
            event("01:01.800", "task_started", Some("a")),
            user("01:01.950", "原問題"),
            complete.to_string(),
        ]);
        assert_eq!(
            codex_reply_for_turn(&l, &["原問題".into()], since),
            Some("完整答案".into())
        );
        assert_eq!(codex_latest_reply_since(&l, since), Some("完整答案".into()));
    }
}

#[cfg(all(test, feature = "daemon-test-harness"))]
mod ownership_tests {
    use crate::{db, testing as tt};
    use serde_json::json;

    /// Turn replacement uses the existing expected_turn/CAS contract: an old
    /// armed callback cannot assign a reply to a superseded AGM turn.
    #[tokio::test]
    async fn a_stale_fallback_callback_cannot_duplicate_a_reply_after_turn_replacement() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "native-owner-1994").await;
        sqlx::query("UPDATE bots SET kind='codex' WHERE id=?")
            .bind(&bot.id)
            .execute(&app.db)
            .await
            .unwrap();
        let run = tt::fake_run(&app, &bot.id).await;
        let session = "01a18888-1994-7000-8000-000000000001";
        let home = env.dir.join("codex");
        let logs = home.join("sessions/2026/10/25");
        std::fs::create_dir_all(&logs).unwrap();
        sqlx::query("UPDATE bots SET env_json=? WHERE id=?")
            .bind(json!({"CODEX_HOME":home}).to_string())
            .bind(&bot.id)
            .execute(&app.db)
            .await
            .unwrap();
        sqlx::query("UPDATE runs SET native_session_id=? WHERE id=?")
            .bind(session)
            .bind(&run)
            .execute(&app.db)
            .await
            .unwrap();
        let old = crate::lifecycle::run_state::a_turn(&app, &bot.id, Some(&run), "in_flight").await;
        sqlx::query("UPDATE turns SET prompt_text='original',created_at='2026-10-25T10:00:00Z',delivered_at='2026-10-25T10:01:01Z' WHERE id=?").bind(&old).execute(&app.db).await.unwrap();
        let new = crate::lifecycle::run_state::a_turn(&app, &bot.id, None, "in_flight").await;
        crate::lifecycle::interruption::send_now_interrupted(&app, &bot.id, &run, &old, &new)
            .await
            .unwrap();
        sqlx::query("UPDATE turns SET prompt_text='steered',delivery='ok',created_at='2026-10-25T10:01:02Z',delivered_at='2026-10-25T10:01:02Z' WHERE id=?").bind(&new).execute(&app.db).await.unwrap();
        let inflight: Vec<String> =
            sqlx::query_scalar("SELECT id FROM turns WHERE run_id=? AND status='in_flight'")
                .bind(&run)
                .fetch_all(&app.db)
                .await
                .unwrap();
        assert_eq!(
            inflight,
            vec![new.clone()],
            "new turn must be the sole in-flight owner"
        );
        let event = |t: &str, kind: &str| {
            json!({"type":"event_msg","timestamp":format!("2026-10-25T10:{t}Z"),"payload":{"type":kind,"turn_id":"native-one","last_agent_message":"one complete final reply"}}).to_string()
        };
        let user = |t: &str, text: &str| {
            json!({"type":"response_item","timestamp":format!("2026-10-25T10:{t}Z"),"payload":{"type":"message","role":"user","content":[{"type":"input_text","text":text}]}}).to_string()
        };
        std::fs::write(
            logs.join(format!("rollout-2026-10-25T10-01-00-{session}.jsonl")),
            [
                event("01:00", "task_started"),
                user("01:01", "original"),
                user("01:02", "steered"),
                event("01:03", "task_complete"),
            ]
            .join("\n"),
        )
        .unwrap();
        env.herdr
            .set_screen(&format!("pane-{}", bot.id), "› steered\n• idle\n");
        assert!(
            !crate::lifecycle::poller::try_fallback(&app, &run, Some(&old))
                .await
                .unwrap()
        );
        assert!(
            crate::lifecycle::poller::try_fallback(&app, &run, Some(&new))
                .await
                .unwrap()
        );
        assert!(
            !crate::lifecycle::poller::try_fallback(&app, &run, Some(&old))
                .await
                .unwrap()
        );
        let rows: Vec<(String,String,String,i64)> = sqlx::query_as("SELECT turn_id,content,source,incomplete FROM messages WHERE turn_id IN (?,?) AND role='assistant'").bind(&old).bind(&new).fetch_all(&app.db).await.unwrap();
        assert_eq!(
            rows,
            vec![(
                new,
                "one complete final reply".into(),
                "transcript".into(),
                0
            )]
        );
        let old_state: String = sqlx::query_scalar("SELECT status FROM turns WHERE id=?")
            .bind(&old)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(old_state, "failed");
    }
}

#[cfg(all(test, feature = "daemon-test-harness"))]
mod codex_steer_tests {
    use crate::{db, testing as tt};
    use serde_json::json;

    #[tokio::test]
    async fn real_codex_steer_keeps_one_agm_turn_and_one_complete_reply() {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "codex-steer-owner-1994").await;
        sqlx::query("UPDATE bots SET kind='codex' WHERE id=?")
            .bind(&bot.id)
            .execute(&app.db)
            .await
            .unwrap();
        let run = tt::fake_run(&app, &bot.id).await;
        sqlx::query("UPDATE runs SET agent_status='working',codex_instant_interrupt=1,native_session_id=NULL,transcript_path=NULL WHERE id=?").bind(&run).execute(&app.db).await.unwrap();
        crate::codex_update::remember_running(&run, "│ >_ OpenAI Codex (v0.164.0)   │", None);
        app.cfg
            .update(|c| {
                c.codex.instant_interrupt = true;
                Ok(())
            })
            .await
            .unwrap();
        env.herdr.live_pane(
            &format!("pane-{}", bot.id),
            tt::LivePane {
                width: Some(120),
                codex: true,
                ..Default::default()
            },
        );
        let turn =
            crate::lifecycle::run_state::a_turn(&app, &bot.id, Some(&run), "in_flight").await;
        sqlx::query("UPDATE turns SET prompt_text='original',created_at='2026-10-25T10:00:00Z',delivered_at='2026-10-25T10:01:01Z' WHERE id=?").bind(&turn).execute(&app.db).await.unwrap();
        let steered = crate::lifecycle::prompt::prompt_send_now(
            &app,
            &bot.id,
            "supplement",
            "native-owner-1994",
            &[],
            None,
        )
        .await
        .unwrap();
        assert_eq!(steered.turn_id, turn);
        assert_eq!(steered.send_now, Some("steered"));
        let supplement: (String, String) =
            sqlx::query_as("SELECT turn_id,sent_via FROM messages WHERE id=?")
                .bind(&steered.message_id)
                .fetch_one(&app.db)
                .await
                .unwrap();
        assert_eq!(supplement, (turn.clone(), "supplement".into()));
        let session = "01a18888-1994-7000-8000-000000000002";
        let home = env.dir.join("codex");
        let logs = home.join("sessions/2026/10/25");
        std::fs::create_dir_all(&logs).unwrap();
        sqlx::query("UPDATE bots SET env_json=? WHERE id=?")
            .bind(json!({"CODEX_HOME":home}).to_string())
            .bind(&bot.id)
            .execute(&app.db)
            .await
            .unwrap();
        sqlx::query("UPDATE runs SET native_session_id=? WHERE id=?")
            .bind(session)
            .bind(&run)
            .execute(&app.db)
            .await
            .unwrap();
        let event = |t: &str, kind: &str| {
            json!({"type":"event_msg","timestamp":format!("2026-10-25T10:{t}Z"),"payload":{"type":kind,"turn_id":"native-steer","last_agent_message":"one final after steering"}}).to_string()
        };
        let user = |t: &str, text: &str| {
            json!({"type":"response_item","timestamp":format!("2026-10-25T10:{t}Z"),"payload":{"type":"message","role":"user","content":[{"type":"input_text","text":text}]}}).to_string()
        };
        std::fs::write(
            logs.join(format!("rollout-2026-10-25T10-01-00-{session}.jsonl")),
            [
                event("01:00", "task_started"),
                user("01:01", "original"),
                user("01:02", "supplement"),
                event("01:03", "task_complete"),
            ]
            .join("\n"),
        )
        .unwrap();
        env.herdr
            .set_screen(&format!("pane-{}", bot.id), "› supplement\n• idle\n");
        assert!(
            crate::lifecycle::poller::try_fallback(&app, &run, Some(&turn))
                .await
                .unwrap()
        );
        assert!(
            !crate::lifecycle::poller::try_fallback(&app, &run, Some(&turn))
                .await
                .unwrap()
        );
        let replies: Vec<(String, String, i64)> = sqlx::query_as(
            "SELECT content,source,incomplete FROM messages WHERE turn_id=? AND role='assistant'",
        )
        .bind(&turn)
        .fetch_all(&app.db)
        .await
        .unwrap();
        assert_eq!(
            replies,
            vec![("one final after steering".into(), "transcript".into(), 0)]
        );
        let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE run_id=?")
            .bind(&run)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(count, 1, "Codex steer must not create a second AGM turn");
    }
}
