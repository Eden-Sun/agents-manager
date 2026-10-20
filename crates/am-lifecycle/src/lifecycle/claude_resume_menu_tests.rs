use super::*;
use crate::testing as tt;
use crate::testing::LivePane;
use crate::tui_prompts::{cold_resume_choice, ResumeChoice};

// 真實 2.1.304 PTY 畫面，2026-10-20 重新擷取；獨立 binary 副本、拋棄式 cwd/session、safe-mode。
// Max 5x / Opus 4.6[1m]；只修改自己的 transcript timestamp（2h）與 usage/iterations（1M）。
// 沒有自然等待 2h，也沒有真的發送 1M tokens。畫面由 @xterm/headless 80x24 從 script(1) raw stream 還原。
const ACTUAL: &str = include_str!("fixtures/claude-2.1.304-cold-resume.txt");
// 尚未實機重現：依 2.1.304 binary 的固定字串與無編號選項合成；帳號 idle 功能旗標未開。
const SYNTHETIC: &str = include_str!("fixtures/claude-2.1.304-cold-resume-unsent-synthetic.txt");

#[test]
fn cold_resume_actual_is_not_ready_an_error_a_reply_or_a_human_question() {
    assert_eq!(cold_resume_choice(ACTUAL), Some(ResumeChoice::Resume));
    for reported in [AgentStatus::Idle, AgentStatus::Blocked] {
        assert_eq!(
            startup_status("claude", ACTUAL, reported),
            AgentStatus::Unknown
        );
    }
    assert_eq!(
        super::super::delivery::box_state("claude", ACTUAL),
        crate::composer_parse::BoxState::Unready
    );
    assert!(super::super::screen::clean_screen("claude", ACTUAL).is_none());
    assert!(super::super::screen::extract_reply("claude", ACTUAL).is_none());
    assert!(crate::turn_error::api_error_line(ACTUAL).is_none());
    assert!(!crate::tui_prompts::awaits_menu_choice(ACTUAL));
    assert!(crate::tui_prompts::permission_prompt(ACTUAL).is_none());
    assert!(crate::child_alerts::alertable_question(ACTUAL).is_none());
}

#[test]
fn cold_resume_synthetic_unsent_is_recognized_without_numbered_options() {
    assert_eq!(
        cold_resume_choice(SYNTHETIC),
        Some(ResumeChoice::NewConversation)
    );
    assert!(SYNTHETIC.contains("Your unsent message will still be there after you answer"));
    assert_eq!(
        startup_status("claude", SYNTHETIC, AgentStatus::Idle),
        AgentStatus::Unknown
    );
    assert_eq!(
        super::super::delivery::box_state("claude", SYNTHETIC),
        crate::composer_parse::BoxState::Unready
    );
    assert!(crate::turn_error::api_error_line(SYNTHETIC).is_none());
    assert!(!crate::tui_prompts::awaits_menu_choice(SYNTHETIC));
    assert!(crate::child_alerts::alertable_question(SYNTHETIC).is_none());
}

#[test]
fn cold_resume_other_resume_screens_and_incomplete_dialogs_never_authorize_keys() {
    for screen in [
        "Resume",
        "❯ Resume\n  Start a new conversation\nEnter to confirm",
        "Resume this conversation?\n❯ Resume\nStart a new conversation",
    ] {
        assert_eq!(cold_resume_choice(screen), None, "{screen}");
    }
    for screen in [
        ACTUAL.replace("of your 5-hour usage limit", "of the cost"),
        ACTUAL.replace("1. Resume", "1. Retry"),
        ACTUAL.replace("Start a new conversation", "Cancel"),
        format!("{ACTUAL}\n────────────────\n❯\n────────────────\n"),
        format!("{ACTUAL}\n● This is just a quoted example.\n"),
        format!("{ACTUAL}\n────────────────\n❯ a user's draft\n────────────────\n"),
    ] {
        assert_eq!(cold_resume_choice(&screen), None, "{screen}");
    }
}

#[test]
fn cold_resume_synthetic_all_of_limit_and_wrapped_warning_are_supported() {
    let all = SYNTHETIC.replace("about 10%", "all");
    assert_eq!(
        cold_resume_choice(&all),
        Some(ResumeChoice::NewConversation)
    );
    let wrapped = SYNTHETIC.replace(
        "of your 5-hour usage limit",
        "of your\n  5-hour usage limit",
    );
    assert_eq!(
        cold_resume_choice(&wrapped),
        Some(ResumeChoice::NewConversation)
    );
    assert_eq!(
        startup_status("codex", ACTUAL, AgentStatus::Idle),
        AgentStatus::Idle
    );
}

async fn fixture(menu: Option<&str>) -> (tt::Env, db::Bot, db::Run, std::path::PathBuf) {
    let env = tt::env().await;
    let bot = tt::claude_bot(&env.app, &env.project_id, "resume-menu").await;
    let id = tt::fake_run(&env.app, &bot.id).await;
    let log = tt::trusted_transcript(&env.app, &bot.id, &env.dir, "resume-menu.jsonl").await;
    std::fs::write(&log, "").unwrap();
    sqlx::query("UPDATE runs SET native_session_id='sess-1', resume_session_id='sess-1', transcript_path=? WHERE id=?")
        .bind(log.to_str().unwrap()).bind(&id).execute(&env.app.db).await.unwrap();
    let bot = db::bot(&env.app.db, &bot.id).await.unwrap().unwrap();
    let run = db::run(&env.app.db, &id).await.unwrap().unwrap();
    let pane = run.pane_id.as_deref().unwrap();
    env.herdr.set_agent("agent", pane, true);
    env.herdr.live_pane(
        pane,
        LivePane {
            cold_resume_menu: menu.map(str::to_string),
            transcript_file: Some(log.clone()),
            no_spinner: true,
            width: Some(120),
            ..Default::default()
        },
    );
    (env, bot, run, log)
}

#[tokio::test]
async fn cold_resume_start_handles_actual_menu_before_ready_and_keeps_sessionstart_identity() {
    let (env, bot, run, _) = fixture(Some(ACTUAL)).await;
    sqlx::query("UPDATE runs SET state='starting' WHERE id=?")
        .bind(&run.id)
        .execute(&env.app.db)
        .await
        .unwrap();
    assert_eq!(
        after_start(&env.app, "claude", &run.id, AgentStatus::Idle, None).await,
        AgentStatus::Idle
    );
    let keys = env.herdr.calls_to("pane.send_keys");
    assert_eq!(keys.len(), 1);
    assert_eq!(keys[0]["keys"], json!(["enter"]));
    assert!(
        env.herdr.calls_to("pane.send_text").is_empty(),
        "警告關閉前不能打字"
    );
    let current = db::run(&env.app.db, &run.id).await.unwrap().unwrap();
    assert!(
        current.resume_outcome.is_none(),
        "按 Resume 不等於 SessionStart 已驗證"
    );
    crate::hookrecv::consume_resume_session(&env.app, &bot, &current, Some("sess-1"))
        .await
        .unwrap();
    let verified = db::run(&env.app.db, &run.id).await.unwrap().unwrap();
    assert_eq!(verified.native_session_id.as_deref(), Some("sess-1"));
    assert_eq!(verified.resume_outcome.as_deref(), Some("verified"));
    assert!(
        !super::super::resume_nudge::wants_nudge("claude", true, None),
        "閒置收起不補續行提示"
    );
}

#[tokio::test]
async fn cold_resume_start_does_not_trust_the_empty_frame_before_the_warning() {
    let (env, _, run, _) = fixture(None).await;
    let live = env.herdr.live.clone();
    let pane = run.pane_id.clone().unwrap();
    // 決定性的 race barrier：第一幀已讀到空框，才把下一幀換成真實警告。
    let changed = pane.clone();
    super::super::race_point::arm(
        "cold_resume_start_empty_frame",
        &run.id,
        move || async move {
            live.lock()
                .unwrap()
                .get_mut(&changed)
                .unwrap()
                .cold_resume_menu = Some(ACTUAL.to_string());
        },
    );
    assert_eq!(
        after_start(&env.app, "claude", &run.id, AgentStatus::Idle, None).await,
        AgentStatus::Idle
    );
    assert!(env.herdr.pane(&pane).unwrap().cold_resume_menu.is_none());
    assert_eq!(env.herdr.calls_to("pane.send_keys").len(), 1);
}

#[tokio::test]
async fn cold_resume_synthetic_delivery_preserves_and_proves_the_unsent_message_once() {
    let (env, bot, run, log) = fixture(None).await;
    let pane = run.pane_id.as_deref().unwrap();
    env.herdr
        .live
        .lock()
        .unwrap()
        .get_mut(pane)
        .unwrap()
        .cold_resume_on_submit = Some(SYNTHETIC.to_string());
    let client = super::super::client_for_run(&env.app, &run).await.unwrap();
    let out = super::super::delivery::deliver_prompt(
        &env.app,
        &client,
        &run,
        &bot,
        "keep this exact message",
        false,
        false,
    )
    .await
    .unwrap();
    assert_eq!(out, super::super::delivery::Delivered::Submitted);
    let content = std::fs::read_to_string(log).unwrap();
    assert_eq!(content.lines().count(), 1, "原訊息只送一次");
    assert!(content.contains("keep this exact message"));
    assert_eq!(
        env.herdr.calls_to("pane.send_text").len(),
        1,
        "不能把保留在框裡的訊息重貼一次"
    );
    let keys: Vec<_> = env
        .herdr
        .calls_to("pane.send_keys")
        .into_iter()
        .map(|p| {
            serde_json::Value::Array(
                p["keys"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|k| json!(k.as_str().unwrap().to_ascii_lowercase()))
                    .collect(),
            )
        })
        .collect();
    assert_eq!(
        keys,
        vec![
            json!(["enter"]),
            json!(["up"]),
            json!(["enter"]),
            json!(["enter"])
        ]
    );
    assert_eq!(
        db::run(&env.app.db, &run.id)
            .await
            .unwrap()
            .unwrap()
            .native_session_id
            .as_deref(),
        Some("sess-1")
    );
}

#[tokio::test]
async fn cold_resume_actual_preflight_accepts_before_typing_and_respects_same_session() {
    let (env, bot, run, _) = fixture(Some(ACTUAL)).await;
    let client = super::super::client_for_run(&env.app, &run).await.unwrap();
    let out = super::super::delivery::deliver_prompt(
        &env.app,
        &client,
        &run,
        &bot,
        "after resuming",
        true,
        false,
    )
    .await
    .unwrap();
    assert_eq!(out, super::super::delivery::Delivered::Submitted);
    let calls = env.herdr.calls.lock().unwrap();
    let accept = calls
        .iter()
        .position(|(m, _)| m == "pane.send_keys")
        .unwrap();
    let paste = calls
        .iter()
        .position(|(m, _)| m == "pane.send_text")
        .unwrap();
    assert!(accept < paste);
}

#[tokio::test]
async fn cold_resume_stale_run_never_sends_a_key() {
    let (env, _, run, _) = fixture(Some(ACTUAL)).await;
    let app = env.app.clone();
    let id = run.id.clone();
    super::super::race_point::arm("cold_resume_before_key", &run.id, move || async move {
        sqlx::query("UPDATE runs SET state='exited' WHERE id=?")
            .bind(id)
            .execute(&app.db)
            .await
            .unwrap();
    });
    assert!(accept_if_shown(&env.app, &run).await.is_err());
    assert!(env.herdr.calls_to("pane.send_keys").is_empty());
}

#[tokio::test]
async fn cold_resume_start_checks_the_menu_even_after_sessionstart_cleared_the_marker() {
    let (env, bot, run, _) = fixture(Some(ACTUAL)).await;
    crate::hookrecv::consume_resume_session(&env.app, &bot, &run, Some("sess-1"))
        .await
        .unwrap();
    assert_eq!(
        after_start(&env.app, "claude", &run.id, AgentStatus::Blocked, None).await,
        AgentStatus::Idle
    );
    assert_eq!(env.herdr.calls_to("pane.send_keys").len(), 1);
    let after = db::run(&env.app.db, &run.id).await.unwrap().unwrap();
    assert_eq!(after.native_session_id.as_deref(), Some("sess-1"));
    assert_eq!(after.resume_outcome.as_deref(), Some("verified"));
}

#[tokio::test]
async fn cold_resume_synthetic_already_submitted_message_is_never_submitted_again() {
    let (env, bot, run, log) = fixture(None).await;
    let pane = run.pane_id.as_deref().unwrap();
    {
        let mut live = env.herdr.live.lock().unwrap();
        let p = live.get_mut(pane).unwrap();
        p.cold_resume_on_submit = Some(SYNTHETIC.to_string());
        p.cold_resume_submit_on_accept = true;
    }
    let client = super::super::client_for_run(&env.app, &run).await.unwrap();
    assert_eq!(
        super::super::delivery::deliver_prompt(
            &env.app,
            &client,
            &run,
            &bot,
            "exactly once",
            false,
            false
        )
        .await
        .unwrap(),
        super::super::delivery::Delivered::Submitted
    );
    assert_eq!(std::fs::read_to_string(log).unwrap().lines().count(), 1);
    assert_eq!(
        env.herdr.calls_to("pane.send_keys").len(),
        3,
        "送出、移焦、Resume；不能補第四顆送出鍵"
    );
}

#[tokio::test]
async fn cold_resume_changed_host_generation_never_sends_a_key() {
    let (env, _, run, _) = fixture(Some(ACTUAL)).await;
    let conn = env.app.hosts.get(crate::config::LOCAL_HOST).await.unwrap();
    super::super::race_point::arm("cold_resume_before_key", &run.id, move || async move {
        conn.bump_generation_for_test();
    });
    assert!(accept_if_shown(&env.app, &run).await.is_err());
    assert!(env.herdr.calls_to("pane.send_keys").is_empty());
}

#[tokio::test]
async fn cold_resume_changed_screen_never_sends_a_key() {
    let (env, _, run, _) = fixture(Some(ACTUAL)).await;
    let live = env.herdr.live.clone();
    let pane = run.pane_id.clone().unwrap();
    super::super::race_point::arm("cold_resume_before_key", &run.id, move || async move {
        let mut panes = live.lock().unwrap();
        let p = panes.get_mut(&pane).unwrap();
        p.cold_resume_menu = None;
        p.composer = vec!["a user's draft".to_string()];
    });
    assert!(accept_if_shown(&env.app, &run).await.is_err());
    assert!(env.herdr.calls_to("pane.send_keys").is_empty());
}

#[tokio::test]
async fn cold_resume_late_actual_menu_is_resolved_before_the_blocked_prompt_admission() {
    let (env, bot, run, log) = fixture(Some(ACTUAL)).await;
    crate::hookrecv::consume_resume_session(&env.app, &bot, &run, Some("sess-1"))
        .await
        .unwrap();
    sqlx::query("UPDATE runs SET agent_status='blocked' WHERE id=?")
        .bind(&run.id)
        .execute(&env.app.db)
        .await
        .unwrap();
    let out = super::super::prompt::prompt_with(
        &env.app,
        &bot.id,
        "wake and continue",
        "cold-resume-web",
        &[],
    )
    .await
    .unwrap();
    assert_eq!(out.delivery, "ok");
    assert_eq!(std::fs::read_to_string(log).unwrap().lines().count(), 1);
    assert!(
        env.herdr.calls_to("agent.prompt").is_empty(),
        "驗證過的 resumed run 仍須確認送達"
    );
}

#[tokio::test]
async fn cold_resume_synthetic_menu_during_paste_preserves_the_draft_without_repasting() {
    let (env, bot, run, log) = fixture(None).await;
    let pane = run.pane_id.as_deref().unwrap();
    env.herdr
        .live
        .lock()
        .unwrap()
        .get_mut(pane)
        .unwrap()
        .cold_resume_on_type = Some(SYNTHETIC.to_string());
    let client = super::super::client_for_run(&env.app, &run).await.unwrap();
    assert_eq!(
        super::super::delivery::deliver_prompt(
            &env.app,
            &client,
            &run,
            &bot,
            "preserve the pasted draft",
            false,
            false
        )
        .await
        .unwrap(),
        super::super::delivery::Delivered::Submitted
    );
    assert_eq!(std::fs::read_to_string(log).unwrap().lines().count(), 1);
    assert_eq!(env.herdr.calls_to("pane.send_text").len(), 1);
    assert_eq!(
        env.herdr.calls_to("pane.send_keys").len(),
        3,
        "移焦、Resume、送原訊息"
    );
}

#[tokio::test]
async fn cold_resume_blocked_menu_race_is_retryable_not_a_question_for_the_user() {
    let (env, bot, run, _) = fixture(Some(ACTUAL)).await;
    crate::hookrecv::consume_resume_session(&env.app, &bot, &run, Some("sess-1"))
        .await
        .unwrap();
    sqlx::query("UPDATE runs SET agent_status='blocked' WHERE id=?")
        .bind(&run.id)
        .execute(&env.app.db)
        .await
        .unwrap();
    let live = env.herdr.live.clone();
    let pane = run.pane_id.clone().unwrap();
    super::super::race_point::arm("cold_resume_before_key", &run.id, move || async move {
        // 選項剛重畫；不能依舊的一幀按鍵，也不能回「answer the prompt first」。
        live.lock().unwrap().get_mut(&pane).unwrap().revision += 2;
    });
    match super::super::prompt::prompt_with(
        &env.app,
        &bot.id,
        "retry safely",
        "cold-resume-race",
        &[],
    )
    .await
    .unwrap_err()
    {
        LcError::Conflict(body) => {
            assert_eq!(body["reason"], PENDING);
            assert_eq!(body["retryable"], true);
        }
        other => panic!("{other:?}"),
    }
    assert!(env.herdr.calls_to("pane.send_keys").is_empty());
    assert!(env.herdr.calls_to("pane.send_text").is_empty());
}
