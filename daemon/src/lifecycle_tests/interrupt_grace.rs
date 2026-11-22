#![allow(unused_imports)]

use crate::lifecycle_tests::prelude::*;
use crate::lifecycle::*;
use crate::lifecycle::interrupt_grace::*;
use crate::lifecycle::interrupt_grace::test_seams::*;
use chrono::{DateTime, Utc};

#[cfg(test)]
mod tests {
    use super::*;

    /// Interrupt evidence must come from this local bot's own transcript, not a legacy run path
    /// that happens to point at another local JSONL file.
    #[tokio::test]
    async fn interrupt_readers_ignore_a_transcript_outside_the_bots_projects_dir() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let bot = crate::testing::claude_bot(&app, &env.project_id, "interrupt-path-guard").await;
        let run_id = crate::testing::fake_run(&app, &bot.id).await;
        let scratch = crate::testing::track(std::env::temp_dir().join(format!("am-interrupt-path-{}", db::ulid())));
        std::fs::create_dir_all(&scratch).unwrap();
        let outside = scratch.join("foreign.jsonl");
        std::fs::write(&outside, json!({"type":"user","promptId":"FOREIGN-PROMPT-6091","timestamp":"2026-09-16T12:01:00.000Z","message":{"content":"[Request interrupted by user]"}}).to_string()).unwrap();
        sqlx::query("UPDATE runs SET transcript_path=? WHERE id=?")
            .bind(outside.to_string_lossy())
            .bind(&run_id)
            .execute(&app.db)
            .await
            .unwrap();
        let run = db::run(&app.db, &run_id).await.unwrap().unwrap();

        assert_eq!(session_log(&app, &bot, &run).await, None);
        assert_eq!(claude_prompt_id(&app, &bot, &run).await, None);
    }

    /// 接管標記是「剛按了停」，不是「這顆 bot 按過停」。以前標記只有在排隊的派工問 `hold()` 時才清：
    /// 使用者按一次 Esc、之後自己繼續用這顆（沒有派工），標記就一直留著，之後真的 `StopFailure`（撞額度、
    /// API 錯誤）全被當成那次 Esc 的回聲丟掉——回合不收、撞限也不記（#79／#108 的收尾都失效）。
    #[tokio::test]
    async fn a_stop_failure_long_after_an_interrupt_is_a_real_failure() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let bot = crate::testing::claude_bot(&app, &env.project_id, "esc-then-work").await;
        let run = crate::testing::fake_run(&app, &bot.id).await;
        let conv = db::conversation_id(&app.db, &bot.id).await.unwrap();
        let at = Utc::now() - chrono::Duration::minutes(10);
        note_user_interrupt_at(&bot.id, at);
        expect_interrupt_echo(&bot.id, InterruptedTurn { run_id: run.clone(), turn_id: None, session_id: None, prompt_id: None, at });
        let turn = open_turn(&app, &conv, &run).await;

        let mut ev = stop_failure(&bot.id, "p-esc", Some(QUOTA), Utc::now());
        ev.received_at = None;
        crate::hookrecv::process(&app, &ev).await.unwrap();

        assert_eq!(status_of(&app, &turn).await, "failed", "十分鐘前的 Esc 不是這則 StopFailure 的來源");
    }

    async fn open_turn(app: &Arc<App>, conv: &str, run: &str) -> String {
        open_turn_at(app, conv, run, Utc::now()).await
    }

    async fn open_turn_at(app: &Arc<App>, conv: &str, run: &str, at: DateTime<Utc>) -> String {
        let id = db::ulid();
        sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at) VALUES (?,?,?,'web','in_flight','ok',?)")
            .bind(&id)
            .bind(conv)
            .bind(run)
            .bind(at.to_rfc3339_opts(chrono::SecondsFormat::Millis, true))
            .execute(&app.db)
            .await
            .unwrap();
        id
    }

    async fn status_of(app: &Arc<App>, turn: &str) -> String {
        sqlx::query_scalar("SELECT status FROM turns WHERE id=?").bind(turn).fetch_one(&app.db).await.unwrap()
    }

    /// 一則 claude `StopFailure`，`stamped` 是送端蓋的時間（`hook_cmd`／`hook.sh` 在事件發生當下蓋）。
    fn stop_failure(bot_id: &str, prompt_id: &str, error: Option<&str>, stamped: DateTime<Utc>) -> crate::hookrecv::HookBody {
        let mut payload = json!({"hook_event_name": "StopFailure", "session_id": "s-esc", "prompt_id": prompt_id});
        if let Some(e) = error {
            payload["error"] = json!(e);
        }
        crate::hookrecv::HookBody {
            bot_id: bot_id.to_string(),
            provider: "claude".into(),
            payload,
            received_at: Some(stamped.to_rfc3339_opts(chrono::SecondsFormat::Millis, true)),
            truncated: false,
            run_id: None,
        }
    }

    const QUOTA: &str = "You've hit your session limit · resets 3pm (Asia/Taipei)";

    /// #117 重開留言的時序：A 被網頁 Esc 中斷 → A 的回聲到了 → 馬上開 B → B 在 120 秒內真的撞額度。
    /// Esc 的標記只屬於 A：B 必須收成 failed，撞限也要記下來（#108 的閘靠它擋住下一則派工）。
    #[tokio::test]
    async fn a_new_turn_that_hits_the_limit_right_after_an_escaped_one_is_a_real_failure() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let bot = crate::testing::claude_bot(&app, &env.project_id, "esc-then-limit").await;
        let run = crate::testing::fake_run(&app, &bot.id).await;
        env.herdr.set_agent("agent", &format!("pane-{}", bot.id), true);
        let conv = db::conversation_id(&app.db, &bot.id).await.unwrap();
        let a = open_turn(&app, &conv, &run).await;
        let t0 = Utc::now();

        interrupt_bot(&app, &bot.id).await.unwrap();
        assert_eq!(status_of(&app, &a).await, "failed", "A 由 Esc 收掉");
        crate::hookrecv::process(&app, &stop_failure(&bot.id, "p-a", None, t0)).await.unwrap();

        let b = open_turn(&app, &conv, &run).await;
        crate::hookrecv::process(&app, &stop_failure(&bot.id, "p-b", Some(QUOTA), t0 + chrono::Duration::seconds(40))).await.unwrap();

        assert_eq!(status_of(&app, &b).await, "failed", "A 的 Esc 不是 B 這則 StopFailure 的來源");
        assert!(crate::runners::quota::limit_hit_for_bot(&app, &bot).await.is_some(), "B 撞的額度要記下來");
    }

    /// 遠端 hook 走 spool，回聲可能在 B 已經開始之後才到（30 秒以上）。認它靠的是它**在 Esc 當下被擷取**，
    /// 不是放大收到時間的窗口：B 不被它收掉，之後 B 自己的撞額度照樣收。回聲結清的是回聲那筆，
    /// 排隊寬限的接管標記不受影響（那是讓使用者拿回輸入框，兩件事）。
    #[tokio::test]
    async fn an_echo_delayed_past_the_start_of_the_next_turn_is_still_recognised_by_when_it_was_captured() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let bot = crate::testing::claude_bot(&app, &env.project_id, "esc-remote-echo").await;
        let run = crate::testing::fake_run(&app, &bot.id).await;
        env.herdr.set_agent("agent", &format!("pane-{}", bot.id), true);
        let conv = db::conversation_id(&app.db, &bot.id).await.unwrap();
        let a = open_turn(&app, &conv, &run).await;

        interrupt_bot(&app, &bot.id).await.unwrap();
        let pending = pending_echo(&bot.id).expect("記下了被中斷的那一回合");
        assert_eq!((pending.run_id.as_str(), pending.turn_id.as_deref()), (run.as_str(), Some(a.as_str())));
        let t0 = pending.at;
        let b = open_turn_at(&app, &conv, &run, t0 + chrono::Duration::seconds(20)).await;

        crate::hookrecv::process(&app, &stop_failure(&bot.id, "p-a", None, t0 + chrono::Duration::milliseconds(300))).await.unwrap();
        assert_eq!(status_of(&app, &b).await, "in_flight", "A 的回聲不收 B");
        assert_eq!(pending_echo(&bot.id), None, "回聲到了就結清");
        assert!(hold_of(&bot.id).is_some(), "排隊寬限的標記另外算，不被回聲收掉");

        crate::hookrecv::process(&app, &stop_failure(&bot.id, "p-b", Some(QUOTA), t0 + chrono::Duration::seconds(40))).await.unwrap();
        assert_eq!(status_of(&app, &b).await, "failed");
        assert!(crate::runners::quota::limit_hit_for_bot(&app, &bot).await.is_some());
    }

    /// 本機 claude 讀得到 transcript：記下被中斷那一則 prompt 的 id（hook 的 `prompt_id` 就是它），
    /// 之後認回聲只看它——擷取時間再晚也是回聲，擷取時間再早、prompt 不同就不是。
    #[tokio::test]
    async fn the_interrupted_prompt_id_decides_what_is_an_echo_when_the_transcript_is_readable() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let bot = crate::testing::claude_bot(&app, &env.project_id, "esc-prompt-id").await;
        let run = crate::testing::fake_run(&app, &bot.id).await;
        env.herdr.set_agent("agent", &format!("pane-{}", bot.id), true);
        let conv = db::conversation_id(&app.db, &bot.id).await.unwrap();
        let config_dir = env.dir.join("claude-config");
        sqlx::query("UPDATE bots SET env_json=? WHERE id=?")
            .bind(json!({"CLAUDE_CONFIG_DIR": config_dir.to_string_lossy()}).to_string())
            .bind(&bot.id)
            .execute(&app.db)
            .await
            .unwrap();
        let transcript_dir = config_dir.join("projects/-test");
        std::fs::create_dir_all(&transcript_dir).unwrap();
        let transcript = transcript_dir.join(format!("{}.jsonl", bot.id));
        let lines = [
            json!({"type": "user", "promptId": "p-old", "message": {"role": "user", "content": "上一則"}}),
            json!({"type": "user", "promptId": "p-a", "message": {"role": "user", "content": "跑測試"}}),
            json!({"type": "assistant", "promptId": "p-a", "message": {"content": [{"type": "text", "text": "跑…"}]}}),
            json!({"type": "user", "isSidechain": true, "promptId": "p-sub", "message": {"role": "user", "content": "subagent"}}),
        ];
        std::fs::write(&transcript, lines.iter().map(|l| l.to_string()).collect::<Vec<_>>().join("\n")).unwrap();
        sqlx::query("UPDATE runs SET transcript_path=? WHERE id=?").bind(transcript.to_string_lossy().to_string()).bind(&run).execute(&app.db).await.unwrap();
        open_turn(&app, &conv, &run).await;

        interrupt_bot(&app, &bot.id).await.unwrap();
        let t0 = pending_echo(&bot.id).expect("記下了").at;
        assert_eq!(pending_echo(&bot.id).unwrap().prompt_id.as_deref(), Some("p-a"), "subagent 的 sidechain 不算");
        let b = open_turn_at(&app, &conv, &run, t0 + chrono::Duration::seconds(20)).await;

        crate::hookrecv::process(&app, &stop_failure(&bot.id, "p-b", Some("API Error: 500"), t0 + chrono::Duration::seconds(1))).await.unwrap();
        assert_eq!(status_of(&app, &b).await, "failed", "prompt id 對不上就不是回聲");
        assert!(pending_echo(&bot.id).is_some(), "不是回聲的不結清：A 的回聲可能還在路上");

        let c = open_turn_at(&app, &conv, &run, t0 + chrono::Duration::seconds(60)).await;
        crate::hookrecv::process(&app, &stop_failure(&bot.id, "p-a", None, t0 + chrono::Duration::seconds(600))).await.unwrap();
        assert_eq!(status_of(&app, &c).await, "in_flight", "同一則 prompt 的回聲，擷取再晚也不收新回合");
        assert_eq!(pending_echo(&bot.id), None);
    }

    /// 回聲一直沒來（claude 對 Esc 不一定送 `StopFailure`），使用者馬上開了 B、B 立刻失敗：
    /// 就算落在擷取時間寬容的那幾秒內，擷取在 B 開始之後就是 B 的。強制中止走的是同一條。
    #[tokio::test]
    async fn an_old_interrupt_never_swallows_the_failure_of_a_turn_started_after_it() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let bot = crate::testing::claude_bot(&app, &env.project_id, "abort-then-b").await;
        let run = crate::testing::fake_run(&app, &bot.id).await;
        env.herdr.set_agent("agent", &format!("pane-{}", bot.id), true);
        let conv = db::conversation_id(&app.db, &bot.id).await.unwrap();
        let a = open_turn(&app, &conv, &run).await;

        abort_turns(&app, &bot.id).await.unwrap();
        assert_eq!(status_of(&app, &a).await, "failed");
        let t0 = pending_echo(&bot.id).expect("強制中止也記下被中斷的那一回合").at;
        let b = open_turn_at(&app, &conv, &run, t0 + chrono::Duration::milliseconds(800)).await;
        crate::hookrecv::process(&app, &stop_failure(&bot.id, "p-b", Some(QUOTA), t0 + chrono::Duration::milliseconds(1500))).await.unwrap();

        assert_eq!(status_of(&app, &b).await, "failed");
        assert!(crate::runners::quota::limit_hit_for_bot(&app, &bot).await.is_some());
    }
}

mod retain_tests {
    use super::*;

    #[test]
    fn a_deleted_bots_interrupt_marks_are_dropped() {
        for bot in ["intr-gone", "intr-kept"] {
            note_user_interrupt(bot);
            expect_interrupt_echo(bot, InterruptedTurn { run_id: "r".into(), turn_id: None, session_id: None, prompt_id: None, at: Utc::now() });
        }
        retain_bots(&["intr-kept".to_string()]);
        assert!(hold_of("intr-gone").is_none() && pending_echo("intr-gone").is_none(), "沒了的 bot 不留記錄");
        assert!(hold_of("intr-kept").is_some() && pending_echo("intr-kept").is_some());
        retain_bots(&[]);
    }
}
