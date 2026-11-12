use super::{resume_args_by_kind, restart_bot_with, start_bot, start_bot_with, stop_bot, LcError, StartOpts, CONTEXT_LOST_PREFIX};
use crate::capabilities::Db;
use crate::db;
use crate::testing::{claude_bot, env, scratch_dir, Env};
use serde_json::Value;

fn started_args(e: &Env) -> Vec<Vec<String>> {
    e.herdr
        .calls
        .lock()
        .unwrap()
        .iter()
        .filter(|(method, _)| method == "agent.start")
        .filter_map(|(_, params)| {
            params
                .get("args")
                .and_then(Value::as_array)
                .map(|args| args.iter().filter_map(Value::as_str).map(String::from).collect())
        })
        .collect()
}

#[test]
fn provider_resume_arguments_are_exact() {
    assert_eq!(resume_args_by_kind("claude", "sid-1").unwrap(), vec!["--resume", "sid-1"]);
    assert_eq!(resume_args_by_kind("codex", "sid-1").unwrap(), vec!["resume", "sid-1"]);
    assert_eq!(resume_args_by_kind("grok", "sid-1").unwrap(), vec!["--resume", "sid-1"]);
    assert_eq!(resume_args_by_kind("gemini", "sid-1"), Err("unsupported_kind"));
    assert_eq!(resume_args_by_kind("claude", ""), Err("no_session_id"));
}

/// 指名的 session 優先於 DB 記的那段（DB 記錯時的救援路徑，2026-09-22）：`--resume <指名的>`，
/// `resume_session_id` 也是它，之後 SessionStart 回報的就拿它來對。
#[tokio::test]
async fn an_explicit_session_overrides_the_recorded_one() {
    let e = env().await;
    let bot = claude_bot(&e.app, &e.project_id, "rescued").await;
    let plan = super::native_resume_plan(&e.app, &bot, crate::config::LOCAL_HOST, false, Some("246fcf93-real")).await.unwrap().unwrap();
    assert_eq!(plan.0, "246fcf93-real");
    assert!(plan.1.iter().any(|a| a == "246fcf93-real") && plan.1.iter().any(|a| a == "--resume"), "{:?}", plan.1);
    // DB 記了另一段（錯的）也不影響。
    sqlx::query("INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, started_at, ended_at, native_session_id)
                 VALUES (?,?,'exited','idle','ws-1','p-old',?,?, '01a0b8c6-wrong')")
        .bind(db::ulid()).bind(&bot.id).bind(db::now()).bind(db::now()).execute(e.app.db()).await.unwrap();
    assert_eq!(super::native_resume_plan(&e.app, &bot, crate::config::LOCAL_HOST, false, Some("246fcf93-real")).await.unwrap().unwrap().0, "246fcf93-real");
    assert_eq!(super::native_resume_plan(&e.app, &bot, crate::config::LOCAL_HOST, false, None).await.unwrap().unwrap().0, "01a0b8c6-wrong");
}

/// Continuation is opt-in: only `resume_native` gets `--resume <id>`.
#[tokio::test]
async fn native_resume_is_opt_in() {
    let e = env().await;
    let pm = claude_bot(&e.app, &e.project_id, "pm").await;
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, native_session_id, started_at, ended_at)
         VALUES (?,?,'stopped','idle',?,?,?)",
    )
    .bind(db::ulid())
    .bind(&pm.id)
    .bind("native-previous")
    .bind("2026-09-07T00:00:00Z")
    .bind("2026-09-07T00:01:00Z")
    .execute(e.app.db())
    .await
    .unwrap();

    start_bot_with(&e.app, &pm.id, StartOpts { resume_native: true, ..Default::default() }).await.unwrap();
    let args = started_args(&e).pop().unwrap();
    assert!(args.windows(2).any(|w| w == ["--resume", "native-previous"]));
    stop_bot(&e.app, &pm.id).await.unwrap();

    start_bot(&e.app, &pm.id).await.unwrap();
    let args = started_args(&e).pop().unwrap();
    assert!(!args.contains(&"--resume".into()));
    stop_bot(&e.app, &pm.id).await.unwrap();
}

/// #1869: sleeping runs keep their status, but a resumed run needs its own projection
/// before the next API turn. Empty startup redraws must not erase that projection.
#[tokio::test]
async fn resume_restores_context_before_any_new_turn_and_fresh_start_does_not() {
    let e = env().await;
    let bot = claude_bot(&e.app, &e.project_id, "resume-context").await;
    sqlx::query("UPDATE bots SET model='opus' WHERE id=?").bind(&bot.id).execute(e.app.db()).await.unwrap();
    let old_status = serde_json::json!({
        "session_id": "native-context", "model": {"id": "opus"},
        "context_window": {"used_percentage": 42, "context_window_size": 1000000},
        "prompt_cache": {"warm": true}, "account_email": "previous@example.com"
    });
    sqlx::query("INSERT INTO runs (id, bot_id, state, agent_status, native_session_id, status_json, started_at, ended_at)
        VALUES (?,?,'stopped','idle','native-context',?,'2026-09-07T00:00:00Z','2026-09-07T00:01:00Z')")
        .bind(db::ulid()).bind(&bot.id).bind(old_status.to_string()).execute(e.app.db()).await.unwrap();
    start_bot_with(&e.app, &bot.id, StartOpts { resume_native: true, ..Default::default() }).await.unwrap();
    let run = db::active_run(e.app.db(), &bot.id).await.unwrap().unwrap();
    let status: Value = serde_json::from_str(run.status_json.as_deref().unwrap()).unwrap();
    assert_eq!(status["context_window"]["used_percentage"], 42);
    assert!(status.get("prompt_cache").is_none() && status.get("account_email").is_none());

    for payload in [
        serde_json::json!({"hook_event_name": "SessionStart", "session_id": "native-context"}),
        serde_json::json!({"hook_event_name": "StatusLine", "session_id": "native-context",
            "model": {"id": "opus"}, "cost": {"total_api_duration_ms": 0},
            "context_window": {"used_percentage": null, "total_input_tokens": 0, "total_output_tokens": 0}}),
    ] {
        let body = crate::hookrecv::HookBody {
            bot_id: bot.id.clone(), provider: "claude".into(), payload,
            ..serde_json::from_value(serde_json::json!({"bot_id": ""})).unwrap()
        };
        crate::hookrecv::process(&e.app, &body).await.unwrap();
    }
    let run = db::active_run(e.app.db(), &bot.id).await.unwrap().unwrap();
    let status: Value = serde_json::from_str(run.status_json.as_deref().unwrap()).unwrap();
    assert_eq!(status["context_window"]["used_percentage"], 42, "SessionStart + empty redraw must keep context");
    assert!(crate::cache_clock::statusline_at(&run.id).is_none(), "restoring context must not restart cache warmth");
    let compact = crate::hookrecv::HookBody {
        bot_id: bot.id.clone(), provider: "claude".into(),
        payload: serde_json::json!({"hook_event_name": "SessionStart", "source": "compact", "session_id": "native-context"}),
        ..serde_json::from_value(serde_json::json!({"bot_id": ""})).unwrap()
    };
    crate::hookrecv::process(&e.app, &compact).await.unwrap();
    let run = db::active_run(e.app.db(), &bot.id).await.unwrap().unwrap();
    let status: Value = serde_json::from_str(run.status_json.as_deref().unwrap()).unwrap();
    assert!(status.get("context_window").is_none(), "compaction invalidates the provisional measurement");
    let body = crate::hookrecv::HookBody {
        bot_id: bot.id.clone(), provider: "claude".into(),
        payload: serde_json::json!({"hook_event_name": "StatusLine", "session_id": "native-context",
            "context_window": {"used_percentage": 0, "total_input_tokens": 300, "context_window_size": 1000000}}),
        ..serde_json::from_value(serde_json::json!({"bot_id": ""})).unwrap()
    };
    crate::hookrecv::process(&e.app, &body).await.unwrap();
    let run = db::active_run(e.app.db(), &bot.id).await.unwrap().unwrap();
    let status: Value = serde_json::from_str(run.status_json.as_deref().unwrap()).unwrap();
    assert_eq!(status["context_window"]["used_percentage"], 0);
    assert!(status.get("_agm_resumed_context").is_none());
    stop_bot(&e.app, &bot.id).await.unwrap();

    start_bot(&e.app, &bot.id).await.unwrap();
    let run = db::active_run(e.app.db(), &bot.id).await.unwrap().unwrap();
    assert!(run.status_json.is_none(), "a fresh session must not inherit old context");
    stop_bot(&e.app, &bot.id).await.unwrap();
}

async fn context_hook(e: &Env, bot: &str, payload: Value) {
    crate::hookrecv::process(&e.app, &crate::hookrecv::HookBody {
        bot_id: bot.into(), provider: "claude".into(), payload,
        ..serde_json::from_value(serde_json::json!({"bot_id": ""})).unwrap()
    }).await.unwrap();
}

async fn seeded_context_bot(e: &Env, name: &str) -> db::Bot {
    let bot = claude_bot(&e.app, &e.project_id, name).await;
    sqlx::query("UPDATE bots SET model='opus' WHERE id=?").bind(&bot.id).execute(e.app.db()).await.unwrap();
    sqlx::query("INSERT INTO runs (id,bot_id,state,native_session_id,status_json,started_at,ended_at)
        VALUES (?,?,'stopped','context-A',?,'2026-09-01','2026-09-02')")
        .bind(db::ulid()).bind(&bot.id).bind(serde_json::json!({
            "session_id": "context-A", "model": {"id": "opus"},
            "context_window": {"used_percentage": 42, "context_window_size": 1000000}
        }).to_string()).execute(e.app.db()).await.unwrap();
    db::bot(e.app.db(), &bot.id).await.unwrap().unwrap()
}

async fn active_context(e: &Env, bot: &str) -> Value {
    let run = db::active_run(e.app.db(), bot).await.unwrap().unwrap();
    run.status_json.map(|s| serde_json::from_str(&s).unwrap()).unwrap_or(Value::Null)
}

#[tokio::test]
async fn resume_after_measured_session_change_without_a_statusline_never_borrows_the_old_context() {
    let e = env().await;
    let bot = seeded_context_bot(&e, "context-session-change").await;
    start_bot_with(&e.app, &bot.id, StartOpts { resume_native: true, ..Default::default() }).await.unwrap();
    assert_eq!(active_context(&e, &bot.id).await["context_window"]["used_percentage"], 42);
    context_hook(&e, &bot.id, serde_json::json!({"hook_event_name": "SessionStart", "session_id": "context-A"})).await;
    context_hook(&e, &bot.id, serde_json::json!({"hook_event_name": "StatusLine", "session_id": "context-A",
        "model": {"id": "opus"}, "context_window": {"used_percentage": 77,
            "total_input_tokens": 770000, "context_window_size": 1000000}})).await;
    assert_eq!(active_context(&e, &bot.id).await["context_window"]["used_percentage"], 77);
    // /clear moves the same run to B, with no B StatusLine before it sleeps.
    context_hook(&e, &bot.id, serde_json::json!({"hook_event_name": "SessionStart", "session_id": "context-B"})).await;
    assert!(active_context(&e, &bot.id).await.get("context_window").is_none());
    stop_bot(&e.app, &bot.id).await.unwrap();
    start_bot_with(&e.app, &bot.id, StartOpts { resume_native: true, ..Default::default() }).await.unwrap();
    let run = db::active_run(e.app.db(), &bot.id).await.unwrap().unwrap();
    assert_eq!(run.resume_session_id.as_deref(), Some("context-B"));
    assert!(active_context(&e, &bot.id).await.get("context_window").is_none());
    context_hook(&e, &bot.id, serde_json::json!({"hook_event_name": "StatusLine", "session_id": "context-B",
        "model": {"id": "opus"}, "context_window": {"used_percentage": 0, "context_window_size": 1000000}})).await;
    assert_ne!(active_context(&e, &bot.id).await["context_window"]["used_percentage"], 77);
    stop_bot(&e.app, &bot.id).await.unwrap();
}

#[tokio::test]
async fn resume_does_not_restore_when_the_launch_model_changed() {
    let e = env().await;
    for model in ["sonnet", "opus[1m]"] {
        let bot = seeded_context_bot(&e, &format!("context-{model}")).await;
        sqlx::query("UPDATE bots SET model=? WHERE id=?").bind(model).bind(&bot.id).execute(e.app.db()).await.unwrap();
        start_bot_with(&e.app, &bot.id, StartOpts { resume_native: true, ..Default::default() }).await.unwrap();
        assert!(active_context(&e, &bot.id).await.is_null(), "changed model must not inherit old percentage: {model}");
        stop_bot(&e.app, &bot.id).await.unwrap();
    }
}

#[tokio::test]
async fn resume_with_a_new_window_and_compaction_boundaries_survives_another_stop() {
    let e = env().await;
    for compact in [false, true] {
        let bot = seeded_context_bot(&e, &format!("context-boundary-{compact}")).await;
        let opts = StartOpts { resume_native: true, ..Default::default() };
        start_bot_with(&e.app, &bot.id, opts.clone()).await.unwrap();
        assert_eq!(active_context(&e, &bot.id).await["context_window"]["context_window_size"], 1000000);
        context_hook(&e, &bot.id, if compact {
            serde_json::json!({"hook_event_name": "SessionStart", "session_id": "context-A", "source": "compact"})
        } else {
            serde_json::json!({"hook_event_name": "StatusLine", "session_id": "context-A",
                "model": {"id": "opus"}, "context_window": {"used_percentage": 0,
                    "total_input_tokens": 0, "context_window_size": 200000}})
        }).await;
        let status = active_context(&e, &bot.id).await;
        assert_eq!(status["_agm_context_invalidated"], "context-A");
        if !compact {
            assert_eq!(status["context_window"]["context_window_size"], 200000);
        }
        stop_bot(&e.app, &bot.id).await.unwrap();
        start_bot_with(&e.app, &bot.id, opts).await.unwrap();
        let status = active_context(&e, &bot.id).await;
        assert!(status.get("context_window").is_none(), "no resurrection across compact/window boundary");
        assert_eq!(status["_agm_context_invalidated"], "context-A");
        stop_bot(&e.app, &bot.id).await.unwrap();
    }
}

/// issue #748：`[codex] instant_interrupt` 開著才在 argv 帶 `-c features.instant_interrupt=true`，`runs.codex_instant_interrupt`
/// 照「argv 有沒有帶」記 1／0（同一個判斷）；預設關＝不帶、記 0。claude 不帶也不記（NULL）。
#[tokio::test]
async fn codex_start_carries_instant_interrupt_only_with_the_canary_on_and_records_it() {
    use crate::capabilities::Cfg;
    let flag = |a: &Vec<String>| a.windows(2).any(|w| w == ["-c", "features.instant_interrupt=true"]);
    let e = env().await;
    let bot = claude_bot(&e.app, &e.project_id, "codex-ii").await;
    sqlx::query("UPDATE bots SET kind='codex', identity=NULL WHERE id=?").bind(&bot.id).execute(e.app.db()).await.unwrap();
    for on in [false, true] {
        e.app.cfg().update(move |c| { c.codex.instant_interrupt = on; Ok(()) }).await.unwrap();
        start_bot(&e.app, &bot.id).await.unwrap();
        let args = started_args(&e).pop().unwrap();
        let run = db::active_run(e.app.db(), &bot.id).await.unwrap().unwrap();
        assert_eq!(flag(&args), on, "旗標 {on}：{args:?}");
        assert_eq!(run.codex_instant_interrupt, Some(i64::from(on)), "DB 跟 argv 同一個判斷");
        stop_bot(&e.app, &bot.id).await.unwrap();
    }
    let claude = claude_bot(&e.app, &e.project_id, "claude-ii").await;
    start_bot(&e.app, &claude.id).await.unwrap();
    assert!(!flag(&started_args(&e).pop().unwrap()), "只有 codex 帶");
    assert_eq!(db::active_run(e.app.db(), &claude.id).await.unwrap().unwrap().codex_instant_interrupt, None);
    stop_bot(&e.app, &claude.id).await.unwrap();
}

/// 受限的分享 bot 不帶這個參數（參數由籠子決定），記 0；旗標關也是空。
#[test]
fn the_instant_interrupt_args_follow_the_flag_and_skip_restricted_bots() {
    assert_eq!(super::codex_instant_interrupt_args(true, false), vec!["-c", "features.instant_interrupt=true"]);
    assert!(super::codex_instant_interrupt_args(true, true).is_empty());
    assert!(super::codex_instant_interrupt_args(false, false).is_empty());
    assert!(super::codex_instant_interrupt_args(false, true).is_empty());
}

#[tokio::test]
async fn codex_start_uses_an_inline_tui_and_its_own_app_server() {
    let e = env().await;
    let bot = claude_bot(&e.app, &e.project_id, "codex").await;
    sqlx::query("UPDATE bots SET kind='codex', identity=NULL, model='gpt-5.6-sol', auto_approve=1 WHERE id=?")
        .bind(&bot.id)
        .execute(e.app.db())
        .await
        .unwrap();

    start_bot(&e.app, &bot.id).await.unwrap();

    let args = started_args(&e).pop().unwrap();
    assert!(args.contains(&"--no-daemon".into()), "Codex must not reuse a shared app server: {args:?}");
    assert!(args.contains(&"--no-alt-screen".into()), "Codex must keep its screen readable: {args:?}");
    assert!(args.contains(&"--yolo".into()), "existing auto-approve args must be preserved: {args:?}");
    assert!(args.windows(2).any(|w| w == ["-c", "tui.show_tooltips=false"]), "Codex turn tips must stay off: {args:?}");
    assert!(
        args.windows(2).any(|w| w == ["-c", "allow_login_shell=false"]),
        "Codex tool shells must not be login shells (they rebuild PATH and push the herdr shim behind the real herdr, #1675): {args:?}"
    );
    stop_bot(&e.app, &bot.id).await.unwrap();

    // resume：子命令排最前面，guard 參數一樣要帶。
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, native_session_id, started_at, ended_at)
         VALUES (?,?,'stopped','idle','codex-previous','2026-09-07T00:00:00Z','2026-09-07T00:01:00Z')",
    )
    .bind(db::ulid())
    .bind(&bot.id)
    .execute(e.app.db())
    .await
    .unwrap();
    start_bot_with(&e.app, &bot.id, StartOpts { resume_native: true, ..Default::default() }).await.unwrap();
    let args = started_args(&e).pop().unwrap();
    assert_eq!(&args[..2], ["resume", "codex-previous"], "{args:?}");
    assert!(args.contains(&"--no-daemon".into()), "resumed Codex must not reuse a shared app server: {args:?}");
    assert!(args.windows(2).any(|w| w == ["-c", "tui.show_tooltips=false"]), "resumed Codex turn tips must stay off: {args:?}");
    assert!(args.windows(2).any(|w| w == ["-c", "allow_login_shell=false"]), "resumed Codex tool shells must not be login shells: {args:?}");
}

#[tokio::test]
async fn grok_start_passes_a_short_rules_file_pointer_without_truncating_the_rules() {
    let e = env().await;
    let bot = claude_bot(&e.app, &e.project_id, "grok-rules").await;
    let grok_home = scratch_dir("am-grok-rules-home");
    let env_json = serde_json::json!({ "GROK_HOME": grok_home.to_string_lossy() }).to_string();
    let persona = "Keep this configured persona intact: $(echo GROK-PERSONA)";
    sqlx::query("UPDATE bots SET kind='grok', persona=?, env_json=? WHERE id=?")
        .bind(persona)
        .bind(&env_json)
        .bind(&bot.id)
        .execute(e.app.db())
        .await
        .unwrap();

    start_bot(&e.app, &bot.id).await.unwrap();

    let args = started_args(&e).pop().unwrap();
    let at = args.iter().position(|a| a == "--rules").expect("Grok gets --rules");
    let pointer = &args[at + 1];
    assert!(pointer.contains("grok-rules.md"), "the argv carries a file pointer, not a truncated prompt: {pointer}");
    assert!(pointer.len() < 300, "the directive fits Herdr's command-line budget: {}", pointer.len());

    let rules_file = e.app.data_dir.join("bots").join(&bot.id).join("grok-rules.md");
    let full_rules = std::fs::read_to_string(&rules_file).expect("the full instructions are staged beside the bot");
    assert!(full_rules.contains("需要開子任務或平行工作時"), "base rules survive outside the shell argv");
    assert!(full_rules.contains(persona), "configured persona survives outside the shell argv");
}

/// #769：claude 母 bot 的完整指示以前整份塞進 `--append-system-prompt`，被 herdr 的 900 bytes 預算砍成「…（後略）」，
/// 只剩 AG Man 規則的前段，agent md 與 persona 一個字都沒帶到。現在走檔案。
#[tokio::test]
async fn claude_start_passes_the_full_persona_by_file_without_truncation() {
    let e = env().await;
    let bot = claude_bot(&e.app, &e.project_id, "claude-persona").await;
    let persona = format!("CLAUDE-PERSONA-START {} CLAUDE-PERSONA-END", "完整的人設內容".repeat(120));
    assert!(persona.len() > 1500);
    sqlx::query("UPDATE bots SET persona=? WHERE id=?").bind(&persona).bind(&bot.id).execute(e.app.db()).await.unwrap();

    start_bot(&e.app, &bot.id).await.unwrap();

    let args = started_args(&e).pop().unwrap();
    let at = args.iter().position(|a| a == "--append-system-prompt-file").expect("claude reads its persona from a file");
    assert!(args[at + 1].ends_with("persona.md"), "{args:?}");
    assert!(!args.iter().any(|a| a == "--append-system-prompt"), "the inline form must be gone: {args:?}");
    assert!(!args.iter().any(|a| a.ends_with("（後略）") || a.contains("後略")), "nothing in argv was truncated: {args:?}");
    assert!(args.iter().map(|a| a.len()).sum::<usize>() <= 900, "argv fits Herdr's command-line budget: {args:?}");

    let file = e.app.data_dir.join("bots").join(&bot.id).join("persona.md");
    let full = std::fs::read_to_string(&file).expect("the full persona is staged beside the bot");
    assert!(full.contains("需要開子任務或平行工作時"), "the AG Man rules survive");
    assert!(full.contains(&persona), "the configured persona survives in full");
}

/// #769：codex 母 bot 同理，`-c developer_instructions=<整份>` 改寫成 `<CODEX_HOME>/am-bot-<id>.config.toml`，argv 只帶 `-p`。
#[tokio::test]
async fn codex_start_passes_the_full_persona_by_profile_without_truncation() {
    let e = env().await;
    let bot = claude_bot(&e.app, &e.project_id, "codex-persona").await;
    let codex_home = e.dir.join("codex-home-persona");
    let persona = format!("CODEX-PERSONA-START {} CODEX-PERSONA-END", "完整的人設內容".repeat(120));
    let env_json = serde_json::json!({ "CODEX_HOME": codex_home.to_string_lossy() }).to_string();
    sqlx::query("UPDATE bots SET kind='codex', identity=NULL, persona=?, env_json=? WHERE id=?")
        .bind(&persona)
        .bind(&env_json)
        .bind(&bot.id)
        .execute(e.app.db())
        .await
        .unwrap();

    start_bot(&e.app, &bot.id).await.unwrap();

    let args = started_args(&e).pop().unwrap();
    let profile = format!("am-bot-{}", bot.id);
    assert!(args.windows(2).any(|w| w[0] == "-p" && w[1] == profile), "{args:?}");
    assert!(!args.iter().any(|a| a.contains("developer_instructions=")), "the inline form must be gone: {args:?}");
    assert!(!args.iter().any(|a| a.contains("後略")), "nothing in argv was truncated: {args:?}");

    let file = codex_home.join(format!("{profile}.config.toml"));
    let toml_text = std::fs::read_to_string(&file).expect("the profile is written under the bot's CODEX_HOME");
    assert!(toml_text.starts_with("developer_instructions = \""), "{toml_text}");
    let parsed: toml::Value = toml::from_str(&toml_text).expect("the profile is valid TOML");
    let instructions = parsed["developer_instructions"].as_str().unwrap();
    assert!(instructions.contains("需要開子任務或平行工作時"), "the AG Man rules survive");
    assert!(instructions.contains(&persona), "the configured persona survives in full");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        assert_eq!(std::fs::metadata(&file).unwrap().permissions().mode() & 0o777, 0o600, "the rules are private to the user");
    }
}

/// 自己指定了 codex profile 的 bot：不覆蓋它的 profile，指示照舊放 argv（會被截斷），並留一則 system 訊息。
#[tokio::test]
async fn codex_bot_with_its_own_profile_keeps_the_inline_instructions() {
    let e = env().await;
    let bot = claude_bot(&e.app, &e.project_id, "codex-own-profile").await;
    let codex_home = e.dir.join("codex-home-own-profile");
    let env_json = serde_json::json!({ "CODEX_HOME": codex_home.to_string_lossy() }).to_string();
    sqlx::query("UPDATE bots SET kind='codex', identity=NULL, args_json=?, env_json=? WHERE id=?")
        .bind(r#"["-p","x"]"#)
        .bind(&env_json)
        .bind(&bot.id)
        .execute(e.app.db())
        .await
        .unwrap();

    start_bot(&e.app, &bot.id).await.unwrap();

    let args = started_args(&e).pop().unwrap();
    assert!(args.iter().any(|a| a.starts_with("developer_instructions=")), "the old inline form stays: {args:?}");
    assert!(!args.iter().any(|a| a.starts_with("am-bot-")), "our profile is not injected: {args:?}");
    assert!(!codex_home.join(format!("am-bot-{}.config.toml", bot.id)).exists(), "nothing written to CODEX_HOME");
    let conv = db::conversation_id(e.app.db(), &bot.id).await.unwrap();
    let notes: Vec<String> = sqlx::query_scalar("SELECT content FROM messages WHERE conversation_id=? AND role='system'")
        .bind(&conv)
        .fetch_all(e.app.db())
        .await
        .unwrap();
    assert!(notes.iter().any(|n| n.contains("-p／--profile")), "the truncation is explained: {notes:?}");
}

#[tokio::test]
async fn a_corrupt_codex_profile_refuses_start_with_a_safe_actionable_system_message() {
    let e = env().await;
    let bot = claude_bot(&e.app, &e.project_id, "codex-corrupt-profile").await;
    let home = e.dir.join("codex-corrupt-home");
    std::fs::create_dir_all(&home).unwrap();
    let path = home.join(format!("am-bot-{}.config.toml", bot.id));
    let original = "developer_instructions = \"PROFILE_SOURCE_SENTINEL\"\n[hooks.state.\"broken\"\ntrusted_hash = \"TRUST_SOURCE_SENTINEL\"\n";
    std::fs::write(&path, original).unwrap();
    let env_json = serde_json::json!({"CODEX_HOME": home.to_string_lossy()}).to_string();
    sqlx::query("UPDATE bots SET kind='codex', identity=NULL, env_json=? WHERE id=?")
        .bind(env_json)
        .bind(&bot.id)
        .execute(e.app.db())
        .await
        .unwrap();
    let error = start_bot(&e.app, &bot.id).await.unwrap_err();
    assert!(
        started_args(&e).is_empty(),
        "a rejected profile must not launch an agent"
    );
    assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
    let conv = db::conversation_id(e.app.db(), &bot.id).await.unwrap();
    let messages: Vec<String> = sqlx::query_scalar(
        "SELECT content FROM messages WHERE conversation_id=? AND role='system'",
    )
    .bind(conv)
    .fetch_all(e.app.db())
    .await
    .unwrap();
    let message = messages
        .iter()
        .find(|s| s.contains("Codex profile"))
        .expect("actionable diagnostic must reach the user");
    let reason = match &error {
        LcError::Upstream(reason) => reason,
        _ => panic!("expected a safe profile Upstream failure"),
    };
    assert_eq!(message, reason);
    assert!(message.contains(path.to_str().unwrap()));
    assert!(message.contains("第 2 行"));
    assert!(message.contains("修好這個檔或刪掉它，就會重新產生"));
    for forbidden in original.lines().chain([
        "trusted_hash",
        "hooks.state",
        "PROFILE_SOURCE_SENTINEL",
        "TRUST_SOURCE_SENTINEL",
    ]) {
        assert!(
            !message.contains(forbidden),
            "source fragment leaked into the system diagnostic"
        );
        assert!(
            !reason.contains(forbidden),
            "source fragment leaked into the returned error"
        );
    }
}

#[test]
fn a_profile_flag_is_recognised_in_every_spelling() {
    let v = |a: &[&str]| a.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert!(super::has_profile_arg(&v(&["--yolo", "-p", "x"])));
    assert!(super::has_profile_arg(&v(&["--profile", "x"])));
    assert!(super::has_profile_arg(&v(&["--profile=x"])));
    assert!(!super::has_profile_arg(&v(&["--yolo", "-c", "tui.show_tooltips=false", "--model", "gpt-6.1-sol"])));
}

/// 寫不進 bot 目錄（沒有 shim_dir）時，claude／codex／grok 都回 `Failed`，呼叫端據此 fail closed，
/// 不會拿遭截斷的 argv 啟動。
#[tokio::test]
async fn a_claude_persona_that_cannot_be_staged_fails_closed() {
    let e = env().await;
    let bot = claude_bot(&e.app, &e.project_id, "unstageable").await;
    let project = db::project(e.app.db(), &e.project_id).await.unwrap().unwrap();
    let no_env = serde_json::json!({});
    let stage = |b: db::Bot| {
        let (app, project, no_env) = (e.app.clone(), project.clone(), no_env.clone());
        async move { super::stage_persona(&app, &b, &project, None, &no_env, "x", None, &[], None).await }
    };
    assert!(matches!(stage(bot.clone()).await, super::StagedPersona::Failed), "claude without a bot dir");
    let mut grok = bot.clone();
    grok.kind = "grok".into();
    assert!(matches!(stage(grok).await, super::StagedPersona::Failed), "grok without a bot dir");
    let mut agy = bot.clone();
    agy.kind = "agy".into();
    assert!(matches!(stage(agy).await, super::StagedPersona::NotNeeded), "agy has no persona argv");
    let mut codex = bot;
    codex.kind = "codex".into();
    let mut own = codex.clone();
    own.args_json = r#"["--profile","mine"]"#.into();
    assert!(matches!(stage(own).await, super::StagedPersona::KeptInline(_)));
    // 沒指定自己的 profile 時，CODEX_HOME 在這個測試行程的假家目錄底下，不是真的 ~/.codex。
    let home = super::codex_home_dir(&e.app, &project, &no_env).await.unwrap();
    assert!(home.ends_with("/.codex") && home.starts_with(crate::home::dir().unwrap().to_str().unwrap()), "{home}");
}

/// `[agents]` 設定 global（讀得到）＋這個專案一份（`project_md`，可以是不存在的檔）。
async fn configure_agent_md(e: &Env, global: &std::path::Path, project_md: &std::path::Path) {
    use crate::capabilities::Cfg;
    let (g, p) = (global.to_string_lossy().into_owned(), project_md.to_string_lossy().into_owned());
    let pid = e.project_id.clone();
    e.app
        .cfg()
        .update(move |c| {
            c.agents.instructions_file = Some(g);
            c.agents.projects.insert(pid, crate::config::AgentMdFiles::One(p));
            Ok(())
        })
        .await
        .unwrap();
}

fn first_pane_env(e: &Env) -> Value {
    let env_of = |method: &str| e.herdr.calls_to(method).into_iter().find_map(|p| p.get("env").cloned());
    env_of("tab.create").or_else(|| env_of("pane.split")).or_else(|| env_of("workspace.create")).expect("a pane was created with env")
}

async fn system_notes(e: &Env, bot_id: &str) -> Vec<String> {
    let conv = db::conversation_id(e.app.db(), bot_id).await.unwrap();
    sqlx::query_scalar("SELECT content FROM messages WHERE conversation_id=? AND role='system'")
        .bind(&conv)
        .fetch_all(e.app.db())
        .await
        .unwrap()
}

async fn agent_md_alerts(e: &Env, bot_id: &str) -> i64 {
    sqlx::query_scalar("SELECT COUNT(*) FROM supervisor_inbox WHERE kind='ops_alert' AND bot_id=? AND event_key LIKE 'ops_alert:daemon:agent_md_unreadable:%'")
        .bind(bot_id)
        .fetch_one(e.app.db())
        .await
        .unwrap()
}

/// #769 方案 A：設定的指示檔有一份讀不到（遠端主機睡著、ssh 逾時…）時，不關 CLI 自己的 CLAUDE.md／AGENTS.md，
/// 讀得到的那份照常注入，對話裡有 system 訊息、AGM inbox 有 ops_alert——不能開出一顆兩邊都沒有規則的 bot。
#[tokio::test]
async fn an_unreadable_agent_md_keeps_the_cli_docs_on_and_raises_an_alert() {
    let e = env().await;
    let global = e.dir.join("global-agent-md.md");
    std::fs::write(&global, "GLOBAL-RULES-MARKER\n").unwrap();
    configure_agent_md(&e, &global, &e.dir.join("missing-project-agent-md.md")).await;
    let bot = claude_bot(&e.app, &e.project_id, "md-unreadable").await;

    start_bot(&e.app, &bot.id).await.unwrap();

    let env = first_pane_env(&e);
    assert!(env.get("CLAUDE_CODE_DISABLE_CLAUDE_MDS").is_none(), "有一份讀不到：CLI 自己的 CLAUDE.md 照讀：{env}");
    assert_eq!(env["AM_KEEP_CLI_DOCS"], "1", "子 agent 的 shim 也要知道：{env}");
    let persona = std::fs::read_to_string(e.app.data_dir.join("bots").join(&bot.id).join("persona.md")).unwrap();
    assert!(persona.contains("GLOBAL-RULES-MARKER"), "讀得到的那份照常注入");
    let notes = system_notes(&e, &bot.id).await;
    assert!(notes.iter().any(|n| n.contains("讀不到") && n.contains("不關 CLI 自己的指示檔")), "{notes:?}");
    assert_eq!(agent_md_alerts(&e, &bot.id).await, 1, "AGM inbox 有一則 agent_md_unreadable");
    // 同一小時同一個問題再開一次：不重複推。
    stop_bot(&e.app, &bot.id).await.unwrap();
    start_bot(&e.app, &bot.id).await.unwrap();
    assert_eq!(agent_md_alerts(&e, &bot.id).await, 1, "同一小時同一個問題只推一則");
}

/// 每一份都讀到：照舊關掉 CLI 自己的指示檔，不推告警。
#[tokio::test]
async fn a_readable_agent_md_still_disables_the_cli_docs() {
    let e = env().await;
    let global = e.dir.join("global-agent-md-ok.md");
    let project = e.dir.join("project-agent-md-ok.md");
    std::fs::write(&global, "GLOBAL-OK\n").unwrap();
    std::fs::write(&project, "PROJECT-OK\n").unwrap();
    configure_agent_md(&e, &global, &project).await;
    let bot = claude_bot(&e.app, &e.project_id, "md-readable").await;

    start_bot(&e.app, &bot.id).await.unwrap();

    let env = first_pane_env(&e);
    assert_eq!(env["CLAUDE_CODE_DISABLE_CLAUDE_MDS"], "1", "{env}");
    assert!(env.get("AM_KEEP_CLI_DOCS").is_none(), "{env}");
    assert_eq!(agent_md_alerts(&e, &bot.id).await, 0);
    assert!(system_notes(&e, &bot.id).await.iter().all(|n| !n.contains("agent md")));
}

/// codex：讀不到時不帶 `-c project_doc_max_bytes=0`（AGENTS.md 照讀）；讀得到的 agent md 仍進 profile。
#[tokio::test]
async fn codex_with_an_unreadable_agent_md_does_not_cap_project_docs() {
    let e = env().await;
    let global = e.dir.join("global-agent-md-codex.md");
    std::fs::write(&global, "GLOBAL-CODEX-MARKER\n").unwrap();
    configure_agent_md(&e, &global, &e.dir.join("missing-project-agent-md-codex.md")).await;
    let bot = claude_bot(&e.app, &e.project_id, "md-codex").await;
    let codex_home = e.dir.join("codex-home-md");
    let env_json = serde_json::json!({ "CODEX_HOME": codex_home.to_string_lossy() }).to_string();
    sqlx::query("UPDATE bots SET kind='codex', identity=NULL, env_json=? WHERE id=?").bind(&env_json).bind(&bot.id).execute(e.app.db()).await.unwrap();

    start_bot(&e.app, &bot.id).await.unwrap();

    let args = started_args(&e).pop().unwrap();
    assert!(!args.iter().any(|a| a.contains("project_doc_max_bytes")), "{args:?}");
    let profile = std::fs::read_to_string(codex_home.join(format!("am-bot-{}.config.toml", bot.id))).unwrap();
    assert!(profile.contains("GLOBAL-CODEX-MARKER"), "讀得到的那份照常進 profile");
    assert_eq!(agent_md_alerts(&e, &bot.id).await, 1);
}

/// 2026-10-05：agy 只留 Gemini 3.8 Flash。已存在的 bot 設了拿掉的模型，啟動照常成功、argv 改用 3.8 medium（不讓它起不來）。
#[tokio::test]
async fn an_agy_bot_with_a_retired_model_starts_on_the_default_model() {
    let e = env().await;
    let bot = claude_bot(&e.app, &e.project_id, "agy-retired").await;
    sqlx::query("UPDATE bots SET kind='agy', model='gemini-3.1-pro-high', auto_approve=1 WHERE id=?").bind(&bot.id).execute(e.app.db()).await.unwrap();

    start_bot(&e.app, &bot.id).await.expect("拿掉的模型不能讓啟動失敗");

    let args = started_args(&e).pop().unwrap();
    assert!(args.windows(2).any(|w| w == ["--model", "gemini-3.8-flash-medium"]), "{args:?}");
    assert!(!args.iter().any(|a| a.contains("3.1-pro")), "{args:?}");
}

/// agy 啟動：`--dangerously-skip-permissions`（auto_approve）、`--model <slug>`、不帶 resume／persona 旗標；pane env 關掉自動更新；
/// 信任框預寫進**假 HOME** 的 `settings.json`、hook 與 statusLine 裝在假 HOME（絕不碰真的 `~/.gemini`）。
#[tokio::test]
async fn agy_start_pins_permissions_model_env_trust_and_hooks_in_the_fake_home() {
    let e = env().await;
    let bot = claude_bot(&e.app, &e.project_id, "agy-start").await;
    sqlx::query("UPDATE bots SET kind='agy', model='gemini-3.8-flash-high', auto_approve=1 WHERE id=?").bind(&bot.id).execute(e.app.db()).await.unwrap();

    start_bot(&e.app, &bot.id).await.unwrap();

    let args = started_args(&e).pop().unwrap();
    assert!(args.contains(&"--dangerously-skip-permissions".into()), "{args:?}");
    assert!(args.windows(2).any(|w| w == ["--model", "gemini-3.8-flash-high"]), "{args:?}");
    assert!(!args.iter().any(|a| a == "--rules" || a == "--resume" || a == "-c" || a == "--conversation"), "第一階段沒有 resume／persona 旗標：{args:?}");
    let started = e.herdr.calls_to("agent.start");
    assert_eq!(started.last().unwrap()["kind"], "agy");
    let env_of = |method: &str| e.herdr.calls_to(method).into_iter().find_map(|p| p.get("env").cloned());
    let pane_env = env_of("tab.create").or_else(|| env_of("pane.split")).or_else(|| env_of("workspace.create")).expect("a pane was created with env");
    assert_eq!(pane_env["AGY_CLI_DISABLE_AUTO_UPDATE"], "true", "agy 的自我更新每個 pane 都關掉：{pane_env}");

    let home = crate::home::dir().unwrap();
    let settings: Value = serde_json::from_str(&std::fs::read_to_string(crate::agy_support::settings_path(&home)).unwrap()).unwrap();
    let cwd = crate::trust::canonical(&e.repo.to_string_lossy());
    assert!(settings["trustedWorkspaces"].as_array().unwrap().iter().any(|v| v.as_str() == Some(cwd.as_str())), "{settings}");
    assert_eq!(settings["statusLine"]["type"], "command");
    let hooks: Value = serde_json::from_str(&std::fs::read_to_string(crate::agy_support::hooks_path(&home)).unwrap()).unwrap();
    assert_eq!(hooks["agents-manager"]["Stop"][0]["type"], "command");
    assert!(e.app.data_dir.join(crate::agy_support::DISPATCH_SH).is_file());
}

/// agy resume＝單一 argv `--conversation=<id>`，id 形狀不對（可能被拼進 argv）一律不接、沒 id 也不退到 `-c`；
/// fork 沒有旗標，維持 `unsupported_kind`。
#[test]
fn agy_resume_is_one_conversation_argv_and_fork_is_unsupported() {
    let id = "5e69519d-5998-496d-aa7a-b7df4383520a";
    assert_eq!(resume_args_by_kind("agy", id).unwrap(), vec![format!("--conversation={id}")]);
    assert_eq!(resume_args_by_kind("agy", &format!(" {id}\n")).unwrap(), vec![format!("--conversation={id}")]);
    for bad in ["", "  ", "a b", "x;rm -rf /", "../etc", &"a".repeat(65)] {
        assert_eq!(resume_args_by_kind("agy", bad), Err("no_session_id"), "{bad:?}");
    }
    assert_eq!(super::fork_args_by_kind("agy", id), Err("unsupported_kind"));
}

/// codex 0.160.0 的 resume 會恢復上次存的權限，除非明確覆寫（issue #778）：auto_approve 0／1 ×
/// resume／fork 四種組合都要把權限明講，關掉 auto_approve 的不能因為接回 Full Access 的對話變成 yolo。
#[tokio::test]
async fn codex_resume_and_fork_pin_permissions_to_auto_approve() {
    let e = env().await;
    let bot = claude_bot(&e.app, &e.project_id, "codex").await;
    sqlx::query("UPDATE bots SET kind='codex', identity=NULL WHERE id=?").bind(&bot.id).execute(e.app.db()).await.unwrap();
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, native_session_id, started_at, ended_at)
         VALUES (?,?,'stopped','idle','codex-previous','2026-09-07T00:00:00Z','2026-09-07T00:01:00Z')",
    )
    .bind(db::ulid())
    .bind(&bot.id)
    .execute(e.app.db())
    .await
    .unwrap();
    let sandbox = ["-c", "sandbox_mode=\"workspace-write\""];
    // First resume with Full Access, then turn auto_approve off and resume that same saved session again.
    for auto_approve in [1, 0] {
        sqlx::query("UPDATE bots SET auto_approve=? WHERE id=?").bind(auto_approve).bind(&bot.id).execute(e.app.db()).await.unwrap();
        let resume = StartOpts { resume_native: true, ..Default::default() };
        let fork = StartOpts { fork_session: Some("codex-source".into()), ..Default::default() };
        for (opts, head) in [(resume, ["resume", "codex-previous"]), (fork, ["fork", "codex-source"])] {
            start_bot_with(&e.app, &bot.id, opts).await.unwrap();
            let args = started_args(&e).pop().unwrap();
            stop_bot(&e.app, &bot.id).await.unwrap();
            assert_eq!(&args[..2], head, "{args:?}");
            let yolo = args.iter().filter(|a| *a == "--yolo").count();
            let pinned = args.windows(2).filter(|w| *w == sandbox).count();
            if auto_approve == 0 {
                assert_eq!((yolo, pinned), (0, 1), "auto_approve=0 must pin a non-yolo sandbox: {args:?}");
            } else {
                assert_eq!((yolo, pinned), (1, 0), "auto_approve=1 must stay yolo: {args:?}");
            }
        }
    }
}

#[test]
fn permission_args_follow_auto_approve() {
    use super::super::setup::permission_args;
    assert_eq!(permission_args("codex", true), vec!["--yolo"]);
    assert_eq!(permission_args("codex", false), vec!["-c", "sandbox_mode=\"workspace-write\""]);
    assert_eq!(permission_args("claude", true), vec!["--dangerously-skip-permissions"]);
    assert_eq!(permission_args("grok", true), vec!["--always-approve"]);
    // claude 的非 auto 權限由 settings 的 permissions.defaultMode 釘（#722）；grok 沒有 resume 恢復權限的問題。
    assert!(permission_args("claude", false).is_empty());
    assert!(permission_args("grok", false).is_empty());
    assert!(permission_args("gemini", true).is_empty());
}

/// `resume_native` 沒要求一定要接（`resume_required=false`）、接不回時照舊退回開新對話——但這件
/// 事以前只寫一行 log，聊天室裡看不出來，使用者會以為 bot 正常重啟了，其實脈絡已經斷了
/// （issue #92）。現在要在對話裡留一則看得到的系統訊息。
#[tokio::test]
async fn a_silent_fallback_to_a_new_conversation_leaves_a_visible_note() {
    let e = env().await;
    let pm = claude_bot(&e.app, &e.project_id, "pm").await;
    // 有過一段 run 但沒記到 session：native_resume_plan 判定 no_session_id，退回開新對話要留說明。
    // （#1001：全新 bot 第一次啟動沒有原本的對話，不留這則說明；那個情況見 a_brand_new_bot_first_start_does_not_warn_context_lost。）
    sqlx::query("INSERT INTO runs (id, bot_id, state, agent_status, started_at, ended_at) VALUES (?,?,'stopped','idle',?,?)")
        .bind(db::ulid())
        .bind(&pm.id)
        .bind("2026-10-05T00:00:00Z")
        .bind("2026-10-05T00:01:00Z")
        .execute(e.app.db())
        .await
        .unwrap();
    start_bot_with(&e.app, &pm.id, StartOpts { resume_native: true, ..Default::default() }).await.unwrap();
    assert!(!started_args(&e).pop().unwrap().contains(&"--resume".into()), "沒有上一段對話，不該帶 --resume");

    let conv = db::conversation_id(e.app.db(), &pm.id).await.unwrap();
    let note: String = sqlx::query_scalar(
        "SELECT content FROM messages WHERE conversation_id=? AND role='system' ORDER BY created_at DESC LIMIT 1",
    )
    .bind(&conv)
    .fetch_one(e.app.db())
    .await
    .unwrap();
    assert!(note.contains("接不回"), "{note}");
}

/// 這個 bot 自己身份的 `projects/` 底下的 transcript 路徑（檔還沒寫）：身份的 config dir 就是它所在的那一份，
/// 所以「換身份複製」判成同一份、不碰測試機器真正的 `~/.claude`。
async fn own_transcript(e: &crate::testing::Env, bot: &db::Bot, name: &str) -> std::path::PathBuf {
    let dir = e.dir.join("own-cfg");
    let d = dir.to_str().unwrap().to_string();
    e.app
        .cfg
        .update(|c| {
            if !c.identities.iter().any(|i| i.name == "own") {
                c.identities.push(crate::config::IdentityCfg {
                    name: "own".into(),
                    kind: "claude".into(),
                    host: None,
                    env: [("CLAUDE_CONFIG_DIR".to_string(), d.clone())].into(),
                    args: vec![],
                });
            }
            Ok(())
        })
        .await
        .unwrap();
    sqlx::query("UPDATE bots SET identity='own' WHERE id=?").bind(&bot.id).execute(e.app.db()).await.unwrap();
    let path = dir.join("projects/-own").join(format!("{name}.jsonl"));
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    path
}

/// #742：全新對話起的 claude run 記空基準（第一輪巡邏前的切換要採用）；`--resume` 接回的不記。
#[tokio::test]
async fn only_a_fresh_conversation_gets_an_empty_live_switch_baseline() {
    let e = env().await;
    let pm = claude_bot(&e.app, &e.project_id, "pm").await;
    let fresh = start_bot_with(&e.app, &pm.id, StartOpts::default()).await.unwrap();
    assert!(crate::claude_live::is_fresh(&fresh), "全新對話：空基準");

    let transcript = own_transcript(&e, &pm, "pm-742").await;
    std::fs::write(&transcript, "{}\n").unwrap();
    sqlx::query("UPDATE runs SET native_session_id='sid-742', transcript_path=? WHERE id=?")
        .bind(transcript.to_str().unwrap())
        .bind(&fresh)
        .execute(e.app.db())
        .await
        .unwrap();
    let resumed = restart_bot_with(&e.app, &pm.id, StartOpts { resume_native: true, ..Default::default() }).await.unwrap();
    assert!(started_args(&e).pop().unwrap().windows(2).any(|w| w == ["--resume", "sid-742"]));
    assert!(!crate::claude_live::is_fresh(&resumed), "接回的 run 畫面上可能有上個 session 的確認，第一次看到只當基準");
}

/// `?resume=native`（resume_required）：接不回就**整個不啟動**，回 `resumed:false`＋原因，不默默開新對話；
/// 接得回就帶 `--resume <sid>` 並記下 `resume_session_id`。反向：沒要求的照舊退回開新對話（上面那條）。
#[tokio::test]
async fn a_required_resume_refuses_instead_of_starting_fresh() {
    let e = env().await;
    let pm = claude_bot(&e.app, &e.project_id, "pm").await;
    let strict = StartOpts { resume_native: true, resume_required: true, ..Default::default() };
    let err = start_bot_with(&e.app, &pm.id, strict.clone()).await.unwrap_err();
    match err {
        LcError::Conflict(v) => {
            assert_eq!((v["reason"].as_str(), v["resumed"].as_bool(), v["resume_reason"].as_str()), (Some("cannot_resume"), Some(false), Some("no_session_id")))
        }
        other => panic!("expected 409, got {other:?}"),
    }
    assert!(started_args(&e).is_empty(), "nothing was started");
    assert!(db::active_run(e.app.db(), &pm.id).await.unwrap().is_none(), "no run row left behind");

    let transcript = own_transcript(&e, &pm, "pm").await;
    std::fs::write(&transcript, "{}\n").unwrap();
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, native_session_id, transcript_path, started_at, ended_at)
         VALUES (?,?,'stopped','idle','sid-pm',?,'2026-09-01T00:00:00Z','2026-09-01T00:01:00Z')",
    )
    .bind(db::ulid())
    .bind(&pm.id)
    .bind(transcript.to_str().unwrap())
    .execute(e.app.db())
    .await
    .unwrap();
    let run_id = start_bot_with(&e.app, &pm.id, strict.clone()).await.unwrap();
    assert!(started_args(&e).pop().unwrap().windows(2).any(|w| w == ["--resume", "sid-pm"]));
    let sid: Option<String> = sqlx::query_scalar("SELECT resume_session_id FROM runs WHERE id=?").bind(&run_id).fetch_one(e.app.db()).await.unwrap();
    assert_eq!(sid.as_deref(), Some("sid-pm"));

    // restart：停之前就判斷（看現在這個 run 的 session），接不回就連停都不停。
    sqlx::query("UPDATE runs SET native_session_id='sid-live' WHERE id=?").bind(&run_id).execute(e.app.db()).await.unwrap();
    restart_bot_with(&e.app, &pm.id, strict.clone()).await.unwrap();
    assert!(started_args(&e).pop().unwrap().windows(2).any(|w| w == ["--resume", "sid-live"]));
    std::fs::remove_file(&transcript).unwrap();
    let before = db::active_run(e.app.db(), &pm.id).await.unwrap().unwrap().id;
    sqlx::query("UPDATE runs SET native_session_id='sid-gone', transcript_path=? WHERE id=?")
        .bind(transcript.to_str().unwrap())
        .bind(&before)
        .execute(e.app.db())
        .await
        .unwrap();
    let err = restart_bot_with(&e.app, &pm.id, strict).await.unwrap_err();
    assert!(matches!(err, LcError::Conflict(ref v) if v["resume_reason"] == "transcript_missing"), "{err:?}");
    assert_eq!(db::active_run(e.app.db(), &pm.id).await.unwrap().map(|r| r.id), Some(before), "the running agent was not stopped");
    stop_bot(&e.app, &pm.id).await.unwrap();
}

/// #1123：預檢（`native_resume_plan`）過了、真的組參數時才接不回（兩次判斷之間對話檔不見了）：`resume_required`
/// 的約定是**不啟動**，回 409，不能悄悄開新對話。測試用 kind 預檢 hook 在兩次之間把檔案刪掉。
#[tokio::test]
async fn a_required_resume_that_breaks_after_the_precheck_does_not_start_fresh() {
    let e = env().await;
    let pm = claude_bot(&e.app, &e.project_id, "pm").await;
    let transcript = own_transcript(&e, &pm, "pm-1123").await;
    std::fs::write(&transcript, "{}\n").unwrap();
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, native_session_id, transcript_path, started_at, ended_at)
         VALUES (?,?,'stopped','idle','sid-pm',?,'2026-09-01T00:00:00Z','2026-09-01T00:01:00Z')",
    )
    .bind(db::ulid())
    .bind(&pm.id)
    .bind(transcript.to_str().unwrap())
    .execute(e.app.db())
    .await
    .unwrap();

    let gone = transcript.clone();
    e.app.kind_probe.set(std::sync::Arc::new(move |_host, kind, _probe| {
        let _ = std::fs::remove_file(&gone);
        Some(format!("/test/bin/{kind}"))
    }));
    let err = start_bot_with(&e.app, &pm.id, StartOpts { resume_native: true, resume_required: true, ..Default::default() }).await.unwrap_err();
    assert!(matches!(err, LcError::Conflict(ref v) if v["reason"] == "cannot_resume" && v["resume_reason"] == "transcript_missing"), "{err:?}");
    assert!(started_args(&e).is_empty(), "沒有 agent.start：{:?}", started_args(&e));
    assert!(db::active_run(e.app.db(), &pm.id).await.unwrap().is_none(), "不留 run 列");
    let conv = db::conversation_id(e.app.db(), &pm.id).await.unwrap();
    let notes: Vec<String> = sqlx::query_scalar("SELECT content FROM messages WHERE conversation_id=? AND role='system'")
        .bind(&conv)
        .fetch_all(e.app.db())
        .await
        .unwrap();
    assert!(!notes.iter().any(|n| n.contains(CONTEXT_LOST_PREFIX)), "不該說接不回又開了新對話：{notes:?}");
}

/// #1124：來源 bot 改了身份還沒重啟就 fork：來源那段對話的檔在舊身份的 `projects/` 底下，fork 出來的 bot 要先把它
/// 複製到自己的身份，`--resume <sid> --fork-session` 才找得到（以前直接啟動，claude 報 No conversation found 退出）。
#[tokio::test]
async fn a_fork_stages_the_source_transcript_into_the_forks_identity() {
    let e = env().await;
    let src = claude_bot(&e.app, &e.project_id, "src").await;
    let fork = claude_bot(&e.app, &e.project_id, "fork").await;
    let _ = own_transcript(&e, &fork, "unused").await;
    let old = e.dir.join("old-cfg/projects/-own/sid-src.jsonl");
    std::fs::create_dir_all(old.parent().unwrap()).unwrap();
    std::fs::write(&old, "{}\n").unwrap();
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, native_session_id, transcript_path, started_at, ended_at)
         VALUES (?,?,'stopped','idle','sid-src',?,'2026-09-01T00:00:00Z','2026-09-01T00:01:00Z')",
    )
    .bind(db::ulid())
    .bind(&src.id)
    .bind(old.to_str().unwrap())
    .execute(e.app.db())
    .await
    .unwrap();

    sqlx::query("UPDATE runs SET status_json=? WHERE bot_id=? AND native_session_id='sid-src'")
        .bind(serde_json::json!({"session_id": "sid-src", "model": {"id": "opus"},
            "context_window": {"used_percentage": 77, "context_window_size": 1000000}}).to_string())
        .bind(&src.id).execute(e.app.db()).await.unwrap();
    start_bot_with(&e.app, &fork.id, StartOpts { fork_session: Some("sid-src".into()), fork_source_bot: Some(src.id.clone()), ..Default::default() }).await.unwrap();
    let fork_run = db::active_run(e.app.db(), &fork.id).await.unwrap().unwrap();
    assert!(fork_run.status_json.is_none(), "a fork has a new session and must not inherit source context");
    assert!(fork_run.resume_session_id.is_none());
    let staged = e.dir.join("own-cfg/projects/-own/sid-src.jsonl");
    assert_eq!(std::fs::read_to_string(&staged).unwrap(), "{}\n", "來源對話要搬到 fork 出來的身份底下");
    assert!(started_args(&e).pop().unwrap().windows(3).any(|w| w == ["--resume", "sid-src", "--fork-session"]), "{:?}", started_args(&e));
}

#[tokio::test]
async fn fork_note_is_staged_for_all_three_provider_paths() {
    let e = env().await;
    let bot = claude_bot(&e.app, &e.project_id, "fork-stage").await;
    let project = db::project(e.app.db(), &e.project_id).await.unwrap().unwrap();
    let shim = e.dir.join("fork-stage/shim");
    std::fs::create_dir_all(&shim).unwrap();
    let home = e.dir.join("fork-codex-home");
    let env = serde_json::json!({"CODEX_HOME": home.to_string_lossy()});
    for kind in ["claude", "grok", "codex"] {
        let mut b = bot.clone(); b.kind = kind.into();
        let result = super::stage_persona(&e.app, &b, &project, Some(shim.to_str().unwrap()), &env, "new-agent", None, &[], Some("FORK-REMINDER-2043")).await;
        let super::StagedPersona::Staged(reference) = result else { panic!("{kind} staging failed") };
        let file = if kind == "codex" { home.join(format!("{reference}.config.toml")) } else { std::path::PathBuf::from(&reference) };
        let contents = std::fs::read_to_string(file).unwrap();
        assert!(contents.contains("FORK-REMINDER-2043"), "{kind}");
        assert!(contents.contains("new-agent"), "{kind}");
        let args = super::super::setup::persona_args(&b, "new-agent", None, Some(&reference));
        assert!(args.iter().any(|a| a.contains(&reference)), "{kind}: {args:?}");
    }
}

#[tokio::test]
async fn fork_identity_note_uses_source_and_its_children_only() {
    let e = env().await;
    crate::fork_ops::migrate(e.app.db()).await.unwrap();
    let src = claude_bot(&e.app, &e.project_id, "old-bot").await;
    let target = claude_bot(&e.app, &e.project_id, "new-bot").await;
    let child = claude_bot(&e.app, &e.project_id, "old-child").await;
    sqlx::query("UPDATE bots SET parent_bot_id=? WHERE id=?").bind(&src.id).bind(&child.id).execute(e.app.db()).await.unwrap();
    for (bot, agent) in [(&src, "old-agent"), (&child, "old-agent-child")] {
        sqlx::query("INSERT INTO runs (id,bot_id,state,agent_status,agent_name,started_at) VALUES (?,?,'stopped','idle',?,?)")
            .bind(db::ulid()).bind(&bot.id).bind(agent).bind(db::now()).execute(e.app.db()).await.unwrap();
    }
    crate::fork_ops::insert(e.app.db(), &crate::fork_ops::ForkOp {
        client_request_id: db::ulid(), source_bot_id: src.id.clone(), requested_name: String::new(),
        target_bot_id: target.id.clone(), session_id: "old-session".into(), name: target.name.clone(),
        state: "started".into(), run_id: None, start_error: None,
    }).await.unwrap();
    let note = super::fork_identity_note(e.app.db(), &target.id).await.unwrap().unwrap();
    for text in ["old-bot", "old-agent", "old-agent-child", "$AM_AGENT_NAME", "herdr agent list"] { assert!(note.contains(text), "{note}"); }
    assert!(super::fork_identity_note(e.app.db(), &src.id).await.unwrap().is_none());
    assert!(super::fork_identity_note(e.app.db(), &child.id).await.unwrap().is_none());
}

/// #1761：剛 fork 就換身份重啟，第一代還沒回報 session（遠端 hook 晚到）：接回原對話時改從 fork 的來源再分一次，
/// 不寫「接不回」、不開空白新對話；`resume_required` 也不擋。這顆 bot 一旦記到自己的 session 就不再重分。
#[tokio::test]
async fn a_fork_restarted_before_its_session_is_known_forks_again_from_the_source() {
    let e = env().await;
    let src = claude_bot(&e.app, &e.project_id, "src-1761").await;
    let fork = claude_bot(&e.app, &e.project_id, "fork-1761").await;
    let _ = own_transcript(&e, &fork, "unused").await;
    let old = e.dir.join("old-cfg/projects/-own/sid-src.jsonl");
    std::fs::create_dir_all(old.parent().unwrap()).unwrap();
    std::fs::write(&old, "{}\n").unwrap();
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, native_session_id, transcript_path, started_at, ended_at)
         VALUES (?,?,'stopped','idle','sid-src',?,'2026-09-01T00:00:00Z','2026-09-01T00:01:00Z')",
    )
    .bind(db::ulid())
    .bind(&src.id)
    .bind(old.to_str().unwrap())
    .execute(e.app.db())
    .await
    .unwrap();
    crate::fork_ops::migrate(e.app.db()).await.unwrap();
    let first_gen = db::ulid();
    crate::fork_ops::insert(
        e.app.db(),
        &crate::fork_ops::ForkOp {
            client_request_id: db::ulid(),
            source_bot_id: src.id.clone(),
            requested_name: String::new(),
            target_bot_id: fork.id.clone(),
            session_id: "sid-src".into(),
            name: fork.name.clone(),
            state: "started".into(),
            run_id: Some(first_gen.clone()),
            start_error: None,
        },
    )
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, started_at, ended_at)
         VALUES (?,?,'stopped','idle','2026-09-02T00:00:00Z','2026-09-02T00:00:20Z')",
    )
    .bind(&first_gen)
    .bind(&fork.id)
    .execute(e.app.db())
    .await
    .unwrap();

    start_bot_with(&e.app, &fork.id, StartOpts { resume_native: true, resume_required: true, ..Default::default() }).await.unwrap();
    assert!(started_args(&e).pop().unwrap().windows(3).any(|w| w == ["--resume", "sid-src", "--fork-session"]), "{:?}", started_args(&e));
    let conv = db::conversation_id(e.app.db(), &fork.id).await.unwrap();
    let notes: Vec<String> = sqlx::query_scalar("SELECT content FROM messages WHERE conversation_id=? AND role='system'")
        .bind(&conv)
        .fetch_all(e.app.db())
        .await
        .unwrap();
    assert!(!notes.iter().any(|n| n.contains(CONTEXT_LOST_PREFIX)), "不是接不回：{notes:?}");

    sqlx::query("UPDATE runs SET native_session_id='sid-fork' WHERE id=?").bind(&first_gen).execute(e.app.db()).await.unwrap();
    assert!(crate::lifecycle::start::unsettled_fork(&e.app, &fork.id).await.is_none(), "記到自己的 session 就不再從來源重分");
}

/// #1168：fork 只接來源 bot 自己的對話檔。兩顆 bot 的 run 用同一個 sid（不同檔）、時間先後任意：
/// 以前全 DB 挑最新的一筆，挑到別的 bot 的檔就複製過來、用錯的對話繼續。
async fn fork_stages_source_transcript(src_is_newer: bool) -> String {
    let e = env().await;
    let src = claude_bot(&e.app, &e.project_id, "src-shared").await;
    let other = claude_bot(&e.app, &e.project_id, "other-shared").await;
    let fork = claude_bot(&e.app, &e.project_id, "fork-shared").await;
    let _ = own_transcript(&e, &fork, "unused").await;
    let mine = e.dir.join("old-cfg/projects/-own/sid-shared.jsonl");
    let theirs = e.dir.join("other-cfg/projects/-other/sid-shared.jsonl");
    for (p, body) in [(&mine, "A"), (&theirs, "B")] {
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, body).unwrap();
    }
    let (mine_at, theirs_at) = if src_is_newer {
        ("2026-09-02T00:00:00Z", "2026-09-01T00:00:00Z")
    } else {
        ("2026-09-01T00:00:00Z", "2026-09-02T00:00:00Z")
    };
    for (bot, path, at) in [(&src.id, &mine, mine_at), (&other.id, &theirs, theirs_at)] {
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, native_session_id, transcript_path, started_at, ended_at)
             VALUES (?,?,'stopped','idle','sid-shared',?,?,?)",
        )
        .bind(db::ulid())
        .bind(bot.clone())
        .bind(path.to_str().unwrap())
        .bind(at)
        .bind(at)
        .execute(e.app.db())
        .await
        .unwrap();
    }
    let fork_opts = StartOpts { fork_session: Some("sid-shared".into()), fork_source_bot: Some(src.id.clone()), ..Default::default() };
    start_bot_with(&e.app, &fork.id, fork_opts).await.unwrap();
    std::fs::read_to_string(e.dir.join("own-cfg/projects/-own/sid-shared.jsonl")).unwrap()
}

#[tokio::test]
async fn a_fork_stages_its_own_source_transcript_even_when_another_bot_has_the_same_session_later() {
    assert_eq!(fork_stages_source_transcript(false).await, "A", "另一顆 bot 的同 sid 對話比較新，也不能被接過來");
}

#[tokio::test]
async fn a_fork_stages_its_own_source_transcript_even_when_another_bot_has_the_same_session_earlier() {
    assert_eq!(fork_stages_source_transcript(true).await, "A", "結果不能因為時間順序而變");
}

/// #1168：來源對話搬不過來就不啟動。以前只記 warn 照樣啟動，claude 接不到對話就直接退出，等於假裝 fork 成功。
#[tokio::test]
async fn a_fork_whose_source_transcript_cannot_be_staged_does_not_start() {
    let e = env().await;
    let src = claude_bot(&e.app, &e.project_id, "src-gone").await;
    let fork = claude_bot(&e.app, &e.project_id, "fork-gone").await;
    let _ = own_transcript(&e, &fork, "unused").await;
    let gone = e.dir.join("old-cfg/projects/-own/sid-gone.jsonl");
    sqlx::query(
        "INSERT INTO runs (id, bot_id, state, agent_status, native_session_id, transcript_path, started_at, ended_at)
         VALUES (?,?,'stopped','idle','sid-gone',?,'2026-09-01T00:00:00Z','2026-09-01T00:01:00Z')",
    )
    .bind(db::ulid())
    .bind(src.id.clone())
    .bind(gone.to_str().unwrap())
    .execute(e.app.db())
    .await
    .unwrap();
    let fork_opts = StartOpts { fork_session: Some("sid-gone".into()), fork_source_bot: Some(src.id.clone()), ..Default::default() };
    assert!(start_bot_with(&e.app, &fork.id, fork_opts).await.is_err(), "搬不過來就不啟動");
    assert!(!started_args(&e).iter().any(|a| a.iter().any(|x| x == "--fork-session")), "{:?}", started_args(&e));
}

/// #1265：來源 run 的對話檔路徑是 NULL、空字串、或指到不存在的檔：都沒有證據，fork 不能照樣啟動 `--fork-session`。
/// NULL／空字串是 `no_source_transcript`（沒有記到位置）；指到不存在的檔維持 `fork_transcript_unavailable`（#1168）。
#[tokio::test]
async fn a_fork_without_source_transcript_evidence_does_not_start() {
    for (label, path, want) in [
        ("null", None, "no_source_transcript"),
        ("empty", Some(""), "no_source_transcript"),
        ("missing", Some("/nonexistent/sid-x.jsonl"), "fork_transcript_unavailable"),
    ] {
        let e = env().await;
        let src = claude_bot(&e.app, &e.project_id, &format!("src-{label}")).await;
        let fork = claude_bot(&e.app, &e.project_id, &format!("fork-{label}")).await;
        let _ = own_transcript(&e, &fork, "unused").await;
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, native_session_id, transcript_path, started_at, ended_at)
             VALUES (?,?,'stopped','idle','sid-x',?,'2026-09-01T00:00:00Z','2026-09-01T00:01:00Z')",
        )
        .bind(db::ulid())
        .bind(&src.id)
        .bind(path)
        .execute(e.app.db())
        .await
        .unwrap();
        let fork_opts = StartOpts { fork_session: Some("sid-x".into()), fork_source_bot: Some(src.id.clone()), ..Default::default() };
        let err = start_bot_with(&e.app, &fork.id, fork_opts).await.unwrap_err();
        assert!(matches!(&err, LcError::Conflict(v) if v["reason"] == want), "{label}: {err:?}");
        assert!(started_args(&e).is_empty(), "{label}: 沒有 agent.start：{:?}", started_args(&e));
    }
}

/// No transcript → not resumed: `claude --resume` would exit with "No conversation found".
#[tokio::test]
async fn a_session_without_a_transcript_on_disk_is_not_resumed() {
    let e = env().await;
    let pm = claude_bot(&e.app, &e.project_id, "pm").await;
    let ended = |sid: &str, transcript: &str, at: &str| {
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, native_session_id, transcript_path, started_at, ended_at)
             VALUES (?,?,'stopped','idle',?,?,?,?)",
        )
        .bind(db::ulid())
        .bind(pm.id.clone())
        .bind(sid.to_string())
        .bind(transcript.to_string())
        .bind(at.to_string())
        .bind(at.to_string())
    };
    let missing = e.dir.join("never-written.jsonl");
    ended("sid-unwritten", missing.to_str().unwrap(), "2026-09-11T00:00:00Z").execute(e.app.db()).await.unwrap();
    start_bot_with(&e.app, &pm.id, StartOpts { resume_native: true, ..Default::default() }).await.unwrap();
    let args = started_args(&e).pop().unwrap();
    assert!(!args.contains(&"--resume".into()), "resumed a session that has no transcript: {args:?}");
    stop_bot(&e.app, &pm.id).await.unwrap();

    let written = own_transcript(&e, &pm, "written").await;
    std::fs::write(&written, "{}\n").unwrap();
    ended("sid-written", written.to_str().unwrap(), "2026-09-11T00:10:00Z").execute(e.app.db()).await.unwrap();
    start_bot_with(&e.app, &pm.id, StartOpts { resume_native: true, ..Default::default() }).await.unwrap();
    let args = started_args(&e).pop().unwrap();
    assert!(args.windows(2).any(|w| w == ["--resume", "sid-written"]), "{args:?}");
    stop_bot(&e.app, &pm.id).await.unwrap();
}

/// 換身份後重啟要接得回原對話：新舊身份的 `CLAUDE_CONFIG_DIR` 是同一份（symlink）就不用搬檔，
/// 不是同一份就要把 jsonl 複製過去。兩顆都用 `native_resume_plan` 直接驗，不用真的啟動一次 CLI。
mod cross_identity_transcript_tests {
    use super::*;
    use crate::config::LOCAL_HOST;
    use crate::lifecycle::native_resume_plan;
    use std::os::unix::fs::MetadataExt;

    fn write_jsonl(dir: &std::path::Path, cwd_key: &str, sid: &str) -> std::path::PathBuf {
        let cwd_dir = dir.join("projects").join(cwd_key);
        std::fs::create_dir_all(&cwd_dir).unwrap();
        let f = cwd_dir.join(format!("{sid}.jsonl"));
        std::fs::write(&f, "{}\n").unwrap();
        f
    }

    async fn set_identity_dir(app: &std::sync::Arc<crate::state::App>, name: &str, dir: &std::path::Path) {
        app.cfg
            .update(|c| {
                c.identities.push(crate::config::IdentityCfg {
                    name: name.into(),
                    kind: "claude".into(),
                    host: None,
                    env: [("CLAUDE_CONFIG_DIR".to_string(), dir.to_str().unwrap().to_string())].into(),
                    args: vec![],
                });
                Ok(())
            })
            .await
            .unwrap();
    }

    /// (a) 新身份的 `projects/` 是舊身份那份的 symlink → 不搬檔，直接接得回。
    #[tokio::test]
    async fn symlinked_projects_dir_needs_no_copy() {
        let e = env().await;
        let pm = claude_bot(&e.app, &e.project_id, "pm").await;

        let old_dir = e.dir.join("cc-old");
        let transcript = write_jsonl(&old_dir, "-Users-me-project-x", "sid-same");

        // 新身份：自己的目錄下 `projects` 是指到舊身份那份 `projects` 的 symlink（同一份東西）。
        let new_dir = e.dir.join("cc-symlinked");
        std::fs::create_dir_all(&new_dir).unwrap();
        std::os::unix::fs::symlink(old_dir.join("projects"), new_dir.join("projects")).unwrap();
        set_identity_dir(&e.app, "cc-sym", &new_dir).await;
        sqlx::query("UPDATE bots SET identity='cc-sym' WHERE id=?").bind(&pm.id).execute(e.app.db()).await.unwrap();

        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, native_session_id, transcript_path, started_at, ended_at)
             VALUES (?,?,'stopped','idle','sid-same',?,'2026-09-17T00:00:00Z','2026-09-17T00:01:00Z')",
        )
        .bind(db::ulid())
        .bind(&pm.id)
        .bind(transcript.to_str().unwrap())
        .execute(e.app.db())
        .await
        .unwrap();

        let bot = db::bot(e.app.db(), &pm.id).await.unwrap().unwrap();
        let plan = native_resume_plan(&e.app, &bot, LOCAL_HOST, false, None).await.unwrap();
        let (sid, args) = plan.expect("resumable");
        assert_eq!(sid, "sid-same");
        assert!(args.windows(2).any(|w| w == ["--resume", "sid-same"]));

        // 沒有另外複製出一份：透過 symlink 看到的還是原本那個 inode。
        let via_new = new_dir.join("projects").join("-Users-me-project-x").join("sid-same.jsonl");
        assert_eq!(std::fs::metadata(&via_new).unwrap().ino(), std::fs::metadata(&transcript).unwrap().ino());
        assert_eq!(std::fs::read_dir(old_dir.join("projects").join("-Users-me-project-x")).unwrap().count(), 1, "沒有多出檔案");
    }

    /// (b) 新舊身份的 `projects/` 是不相干的兩個目錄 → 接回前先把 jsonl 複製過去，複製出的是獨立的檔案。
    #[tokio::test]
    async fn different_config_dirs_copy_the_transcript_before_resuming() {
        let e = env().await;
        let pm = claude_bot(&e.app, &e.project_id, "pm").await;

        let old_dir = e.dir.join("cc-old2");
        let transcript = write_jsonl(&old_dir, "-Users-me-project-y", "sid-move");

        let new_dir = e.dir.join("cc2-new");
        set_identity_dir(&e.app, "cc2", &new_dir).await;
        sqlx::query("UPDATE bots SET identity='cc2' WHERE id=?").bind(&pm.id).execute(e.app.db()).await.unwrap();

        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, native_session_id, transcript_path, started_at, ended_at)
             VALUES (?,?,'stopped','idle','sid-move',?,'2026-09-17T00:00:00Z','2026-09-17T00:01:00Z')",
        )
        .bind(db::ulid())
        .bind(&pm.id)
        .bind(transcript.to_str().unwrap())
        .execute(e.app.db())
        .await
        .unwrap();

        let bot = db::bot(e.app.db(), &pm.id).await.unwrap().unwrap();
        let plan = native_resume_plan(&e.app, &bot, LOCAL_HOST, false, None).await.unwrap();
        let (sid, args) = plan.expect("resumable after copy");
        assert_eq!(sid, "sid-move");
        assert!(args.windows(2).any(|w| w == ["--resume", "sid-move"]));

        let dest = new_dir.join("projects").join("-Users-me-project-y").join("sid-move.jsonl");
        assert!(dest.exists(), "jsonl 沒有被複製到新身份的 projects 目錄");
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), std::fs::read_to_string(&transcript).unwrap());
        assert_ne!(std::fs::metadata(&dest).unwrap().ino(), std::fs::metadata(&transcript).unwrap().ino(), "應該是獨立複製出的檔案，不是同一個 inode");
    }

    /// 來源檔不見了：不硬擋，退回開新對話（跟原本沒換身份時「transcript_missing」一致）。
    #[tokio::test]
    async fn missing_source_falls_back_to_a_new_conversation() {
        let e = env().await;
        let pm = claude_bot(&e.app, &e.project_id, "pm").await;
        let new_dir = e.dir.join("cc3-new");
        set_identity_dir(&e.app, "cc3", &new_dir).await;
        sqlx::query("UPDATE bots SET identity='cc3' WHERE id=?").bind(&pm.id).execute(e.app.db()).await.unwrap();

        let gone = e.dir.join("cc-old3/projects/-Users-me-project-z/sid-gone.jsonl");
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, native_session_id, transcript_path, started_at, ended_at)
             VALUES (?,?,'stopped','idle','sid-gone',?,'2026-09-17T00:00:00Z','2026-09-17T00:01:00Z')",
        )
        .bind(db::ulid())
        .bind(&pm.id)
        .bind(gone.to_str().unwrap())
        .execute(e.app.db())
        .await
        .unwrap();

        let bot = db::bot(e.app.db(), &pm.id).await.unwrap().unwrap();
        let plan = native_resume_plan(&e.app, &bot, LOCAL_HOST, false, None).await.unwrap();
        let failure = plan.unwrap_err();
        assert_eq!(failure.reason, "transcript_missing");
        assert_eq!(failure.session_id.as_deref(), Some("sid-gone"), "失敗通知需要綁定本次實際接回的 session");
    }

    // ---- 對抗式審查（資料安全）：以下測的都是「換身份複製 transcript」的失敗與邊界 ----

    use std::os::unix::fs::PermissionsExt;

    /// 把一段 run 記在 DB 裡，回新身份（`cc9`）的 config dir。
    async fn seed(e: &crate::testing::Env, bot: &db::Bot, kind: &str, sid: &str, transcript: &std::path::Path) -> std::path::PathBuf {
        let new_dir = e.dir.join(format!("new-{sid}"));
        set_identity_dir(&e.app, "cc9", &new_dir).await;
        sqlx::query("UPDATE bots SET identity='cc9', kind=? WHERE id=?").bind(kind).bind(&bot.id).execute(e.app.db()).await.unwrap();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, native_session_id, transcript_path, started_at, ended_at)
             VALUES (?,?,'stopped','idle',?,?,'2026-09-17T00:00:00Z','2026-09-17T00:01:00Z')",
        )
        .bind(db::ulid())
        .bind(&bot.id)
        .bind(sid)
        .bind(transcript.to_str().unwrap())
        .execute(e.app.db())
        .await
        .unwrap();
        new_dir
    }

    async fn plan_of(e: &crate::testing::Env, bot: &db::Bot) -> Result<(String, Vec<String>), &'static str> {
        let bot = db::bot(e.app.db(), &bot.id).await.unwrap().unwrap();
        native_resume_plan(&e.app, &bot, LOCAL_HOST, false, None).await.unwrap().map_err(|failure| failure.reason)
    }

    fn files_under(dir: &std::path::Path) -> Vec<String> {
        fn walk(d: &std::path::Path, out: &mut Vec<String>) {
            let Ok(rd) = std::fs::read_dir(d) else { return };
            for e in rd.flatten() {
                let p = e.path();
                if p.is_dir() { walk(&p, out) } else { out.push(p.to_string_lossy().into_owned()) }
            }
        }
        let mut out = vec![];
        walk(dir, &mut out);
        out.sort();
        out
    }

    /// codex／grok 的對話檔不在 claude 的 `projects/` 底下，也不歸 `CLAUDE_CONFIG_DIR` 管：以前照樣複製進
    /// `<claude 身份>/projects/<日>/rollout-….jsonl`，把整段 codex 對話複製到 claude 的目錄裡當垃圾專案。
    #[tokio::test]
    async fn a_codex_rollout_is_never_copied_into_a_claude_projects_dir() {
        let e = env().await;
        let bot = claude_bot(&e.app, &e.project_id, "cx").await;
        let rollout = e.dir.join("codex-home/sessions/2026/10/02/rollout-2026-10-02T00-00-00-sid-cx.jsonl");
        std::fs::create_dir_all(rollout.parent().unwrap()).unwrap();
        std::fs::write(&rollout, "{\"secret\":\"conversation\"}\n").unwrap();
        let new_dir = seed(&e, &bot, "codex", "sid-cx", &rollout).await;
        let _ = plan_of(&e, &bot).await;
        assert!(files_under(&new_dir).is_empty(), "codex 的對話被複製進 claude 的目錄：{:?}", files_under(&new_dir));
    }

    /// 來源路徑來自 hook payload 記下的 `transcript_path`：不是 `…/projects/<key>/<sid>.jsonl` 的形狀、或是符號連結
    /// （指到別的檔），都不能被「換身份」複製到另一個身份的目錄。
    #[tokio::test]
    async fn only_a_regular_jsonl_inside_a_projects_dir_is_ever_staged() {
        let e = env().await;
        let bot = claude_bot(&e.app, &e.project_id, "shape").await;
        let secret = e.dir.join("fake-home/.codex/auth.json");
        std::fs::create_dir_all(secret.parent().unwrap()).unwrap();
        std::fs::write(&secret, "TOKEN").unwrap();

        // 1. 形狀不對（不是 projects/<key>/<sid>.jsonl）。
        let new_dir = seed(&e, &bot, "claude", "sid-shape", &secret).await;
        assert_eq!(plan_of(&e, &bot).await, Err("transcript_missing"));
        assert!(files_under(&new_dir).is_empty(), "憑證被複製了：{:?}", files_under(&new_dir));

        // 2. 形狀對，但檔案是指到別處的符號連結。
        let linked = e.dir.join("old-link/projects/-k/sid-link.jsonl");
        std::fs::create_dir_all(linked.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&secret, &linked).unwrap();
        sqlx::query("INSERT INTO runs (id, bot_id, state, agent_status, native_session_id, transcript_path, started_at, ended_at) VALUES (?,?,'stopped','idle','sid-link',?,'2026-09-18T00:00:00Z','2026-09-18T00:01:00Z')")
            .bind(db::ulid()).bind(&bot.id).bind(linked.to_str().unwrap()).execute(e.app.db()).await.unwrap();
        assert_eq!(plan_of(&e, &bot).await, Err("transcript_missing"));
        assert!(files_under(&new_dir).is_empty(), "符號連結被跟著複製：{:?}", files_under(&new_dir));
    }

    /// transcript 是整段對話內容：複製出去的檔 0600、這次新建的目錄 0700，不管來源檔當初是什麼權限（umask 022 的 0644 很常見）。
    #[tokio::test]
    async fn the_copy_and_its_new_directories_are_private() {
        let e = env().await;
        let bot = claude_bot(&e.app, &e.project_id, "priv").await;
        let old_dir = e.dir.join("old-priv");
        let t = write_jsonl(&old_dir, "-Users-x-priv", "sid-priv");
        std::fs::set_permissions(&t, std::fs::Permissions::from_mode(0o644)).unwrap();
        let companion = old_dir.join("projects/-Users-x-priv/sid-priv/sub");
        std::fs::create_dir_all(&companion).unwrap();
        std::fs::write(companion.join("note.txt"), "x").unwrap();
        std::fs::set_permissions(companion.join("note.txt"), std::fs::Permissions::from_mode(0o644)).unwrap();
        let new_dir = seed(&e, &bot, "claude", "sid-priv", &t).await;
        plan_of(&e, &bot).await.expect("resumable");
        let mode = |p: std::path::PathBuf| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        let base = new_dir.join("projects/-Users-x-priv");
        assert_eq!(mode(base.join("sid-priv.jsonl")), 0o600, "複製出去的對話檔");
        assert_eq!(mode(base.clone()), 0o700, "新建的 cwd 目錄");
        assert_eq!(mode(new_dir.join("projects")), 0o700, "新建的 projects 目錄");
        assert_eq!(mode(base.join("sid-priv/sub/note.txt")), 0o600, "附屬目錄的檔");
        assert_eq!(mode(base.join("sid-priv/sub")), 0o700, "附屬目錄");
    }

    /// 目標已經有同名檔而且比來源**長**（來源是它的前綴）：那邊已經接著寫過了，不能被較短的來源蓋回去。
    #[tokio::test]
    async fn a_destination_that_already_continued_the_session_is_not_truncated() {
        let e = env().await;
        let bot = claude_bot(&e.app, &e.project_id, "cont").await;
        let old_dir = e.dir.join("old-cont");
        let t = write_jsonl(&old_dir, "-Users-x-cont", "sid-cont");
        std::fs::write(&t, "{\"a\":1}\n").unwrap();
        let new_dir = seed(&e, &bot, "claude", "sid-cont", &t).await;
        let dest = new_dir.join("projects/-Users-x-cont/sid-cont.jsonl");
        std::fs::create_dir_all(dest.parent().unwrap()).unwrap();
        std::fs::write(&dest, "{\"a\":1}\n{\"b\":2}\n").unwrap();
        plan_of(&e, &bot).await.expect("resumable");
        assert_eq!(std::fs::read_to_string(&dest).unwrap(), "{\"a\":1}\n{\"b\":2}\n", "新身份那邊多出來的回合被蓋掉了");
    }

    /// 兩邊各自長出不同的內容（不是前綴關係）：照 run 記的來源為準，但被換掉的那份留在旁邊，不是直接消失。
    #[tokio::test]
    async fn a_diverged_destination_is_set_aside_not_destroyed() {
        let e = env().await;
        let bot = claude_bot(&e.app, &e.project_id, "div").await;
        let old_dir = e.dir.join("old-div");
        let t = write_jsonl(&old_dir, "-Users-x-div", "sid-div");
        std::fs::write(&t, "{\"a\":1}\n{\"src\":true}\n").unwrap();
        let new_dir = seed(&e, &bot, "claude", "sid-div", &t).await;
        let dest_dir = new_dir.join("projects/-Users-x-div");
        std::fs::create_dir_all(&dest_dir).unwrap();
        std::fs::write(dest_dir.join("sid-div.jsonl"), "{\"a\":1}\n{\"dest\":true}\n").unwrap();
        plan_of(&e, &bot).await.expect("resumable");
        assert_eq!(std::fs::read_to_string(dest_dir.join("sid-div.jsonl")).unwrap(), "{\"a\":1}\n{\"src\":true}\n");
        let aside: Vec<_> = files_under(&dest_dir).into_iter().filter(|f| !f.ends_with("/sid-div.jsonl")).collect();
        assert_eq!(aside.len(), 1, "被換掉的那份要留下來：{aside:?}");
        assert!(!aside[0].ends_with(".jsonl"), "備份不能也叫 .jsonl，不然 CLI 會把它當成另一段 session：{aside:?}");
        assert_eq!(std::fs::read_to_string(&aside[0]).unwrap(), "{\"a\":1}\n{\"dest\":true}\n");
    }

    /// 同一顆 bot 同時兩個（以上）接回請求：不會留下暫存檔、結果是完整的來源內容。
    #[tokio::test]
    async fn concurrent_resumes_leave_one_complete_copy_and_no_debris() {
        let e = env().await;
        let bot = claude_bot(&e.app, &e.project_id, "race").await;
        let old_dir = e.dir.join("old-race");
        let t = write_jsonl(&old_dir, "-Users-x-race", "sid-race");
        let body = "{\"turn\":\"x\"}\n".repeat(20_000);
        std::fs::write(&t, &body).unwrap();
        let new_dir = seed(&e, &bot, "claude", "sid-race", &t).await;
        let results = futures::future::join_all((0..6).map(|_| plan_of(&e, &bot))).await;
        assert!(results.iter().all(|r| r.is_ok()), "{results:?}");
        let files = files_under(&new_dir);
        assert_eq!(files.len(), 1, "只該有最後那一個檔，沒有 .tmp／備份：{files:?}");
        assert_eq!(std::fs::read_to_string(&files[0]).unwrap(), body);
    }
}

/// issue #95：換身份後 transcript 只搬本機，遠端主機的對話接不回來。這裡測遠端那條路：純函式
/// 部分（組 script、解析輸出）直接驗內容；連線失敗時的 fail-closed 用一個刻意連不上的假 host
/// 驗證（沒有可重用的 live-SSH 測試環境，這是能不碰任何真實遠端主機驗到的最大範圍）。
mod remote_cross_identity_transcript_tests {
    use super::*;
    use crate::lifecycle::native_resume_plan;
    use crate::lifecycle::start::{parse_stage_output, remote_stage_script, stage_cross_identity_transcript_remote};
    use std::time::Duration;

    /// `remote_stage_script` 只是拼字串，真正的守衛（`[ -f ... ]`／`mkdir -p ... ||`／
    /// `cp ... ||`）活在產生出來的 shell 語法裡——只驗「文字裡有沒有出現某個標記」測不到
    /// 那些守衛是不是真的接對了地方（q4queue／is5859 今晚踩到同一類問題：trigger 測得到欄位
    /// 變化，測不到 WHERE 子句擋不擋得住）。這裡直接把腳本丟給本機 `/bin/sh` 跑：語法跟
    /// `ssh_exec` 遠端執行的是同一顆直譯器，用本機暫存目錄冒充「舊／新身份的 projects/」，
    /// 不連任何真實遠端主機。
    fn run_script_locally(script: &str) -> String {
        let out = std::process::Command::new("/bin/sh").arg("-c").arg(script).output().expect("run script locally");
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    fn run_with_diagnostics(script: &str, path: Option<&str>) -> (String, diagnostics::Handle) {
        let handle = diagnostics::Handle::new();
        let result = diagnostics::run(script, &handle, path)
            .unwrap_or_else(|error| panic!("test diagnostic instrumentation refused: {}", error.summary()));
        (String::from_utf8_lossy(&result.stdout).into_owned(), handle)
    }

    mod diagnostics {
        use super::remote_stage_script;

        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub(super) enum Code {
            Missing(u8),
            Same(u8),
            StatSource,
            StatSame,
            StatFallback,
            NlinkPre,
            NlinkSame,
            CanonOld,
            CanonNew,
            CanonCompare,
        }

        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub(super) enum StdoutKind {
            Empty,
            Same,
            Staged,
            Missing,
            MkdirFailed,
            CopyFailed,
            Other,
        }

        pub(super) type Handle = crate::lifecycle::diag_support::Recorder<Code, StdoutKind>;
        use crate::lifecycle::diag_support::RECORD_LIMIT;

        fn stdout_kind(stdout: &[u8]) -> StdoutKind {
            match stdout.split(|byte| *byte == b'\n').next().unwrap_or_default() {
                b"AM_SAME" => StdoutKind::Same,
                b"AM_STAGED" => StdoutKind::Staged,
                b"AM_MISSING" => StdoutKind::Missing,
                b"AM_MKDIR_FAILED" => StdoutKind::MkdirFailed,
                b"AM_COPY_FAILED" => StdoutKind::CopyFailed,
                b"" => StdoutKind::Empty,
                _ => StdoutKind::Other,
            }
        }

        fn code_from_marker(line: &[u8]) -> Option<Code> {
            Some(match line {
                b"AM_DIAG:H:H01" => Code::Missing(1),
                b"AM_DIAG:H:H02" => Code::Missing(2),
                b"AM_DIAG:H:H03" => Code::Missing(3),
                b"AM_DIAG:H:H04" => Code::Same(4),
                b"AM_DIAG:H:H05" => Code::Missing(5),
                b"AM_DIAG:H:H06" => Code::Missing(6),
                b"AM_DIAG:H:H07" => Code::Missing(7),
                b"AM_DIAG:H:H08" => Code::Missing(8),
                b"AM_DIAG:H:H09" => Code::Missing(9),
                b"AM_DIAG:H:H10" => Code::Missing(10),
                b"AM_DIAG:H:H11" => Code::Missing(11),
                b"AM_DIAG:H:H12" => Code::Missing(12),
                b"AM_DIAG:H:H13" => Code::Missing(13),
                b"AM_DIAG:H:H14" => Code::Missing(14),
                b"AM_DIAG:H:H15" => Code::Missing(15),
                b"AM_DIAG:H:H16" => Code::Missing(16),
                b"AM_DIAG:H:H17" => Code::Missing(17),
                b"AM_DIAG:H:H18" => Code::Missing(18),
                b"AM_DIAG:H:H19" => Code::Missing(19),
                b"AM_DIAG:H:H20" => Code::Same(20),
                b"AM_DIAG:H:STAT_SOURCE" => Code::StatSource,
                b"AM_DIAG:H:STAT_SAME" => Code::StatSame,
                b"AM_DIAG:H:STAT_FALLBACK" => Code::StatFallback,
                b"AM_DIAG:H:NLINK_PRE" => Code::NlinkPre,
                b"AM_DIAG:H:NLINK_SAME" => Code::NlinkSame,
                b"AM_DIAG:H:CANON_OLD" => Code::CanonOld,
                b"AM_DIAG:H:CANON_NEW" => Code::CanonNew,
                b"AM_DIAG:H:CANON_COMPARE" => Code::CanonCompare,
                _ => return None,
            })
        }

        #[derive(Debug, Clone, Copy, PartialEq, Eq)]
        pub(super) enum Site {
            MissingDefinition,
            Missing(u8),
            SameEarly,
            SameLate,
            FdStat,
            SameFunction,
            StatSource,
            StatSame,
            StatCompare,
            NlinkGnu,
            NlinkBsd,
            CanonOld,
            CanonNew,
        }

        pub(super) type InstrumentError = crate::lifecycle::diag_support::InstrumentError<Site>;
        use crate::lifecycle::diag_support::replace_count;

        fn instrument(mut script: String) -> Result<String, InstrumentError> {
            let missing_count = script.matches("am_missing").count();
            if missing_count != 19 {
                return Err(InstrumentError { site: Site::MissingDefinition, expected: 19, found: missing_count });
            }
            script = replace_count(
                &script,
                "am_missing() { printf 'AM_MISSING\\n'; exit 0; }",
                "am_missing() { printf 'AM_MISSING\\n'; exit 0; }",
                1,
                Site::MissingDefinition,
            )?;

            let missing = [
                ("case \"$K\" in ''|.|..|*/*|*[[:cntrl:]]*) am_missing;; esac", "case \"$K\" in ''|.|..|*/*|*[[:cntrl:]]*) { printf 'AM_DIAG:H:H01\\n' >&9; am_missing; };; esac", 1),
                ("case \"$N\" in ''|.|..|*/*|*[[:cntrl:]]*) am_missing;; esac", "case \"$N\" in ''|.|..|*/*|*[[:cntrl:]]*) { printf 'AM_DIAG:H:H02\\n' >&9; am_missing; };; esac", 2),
                ("case \"$N\" in *.jsonl) ;; *) am_missing;; esac", "case \"$N\" in *.jsonl) ;; *) { printf 'AM_DIAG:H:H03\\n' >&9; am_missing; };; esac", 3),
                ("[ ! -L \"$OLD\" ] && [ -d \"$OLD\" ] || am_missing", "[ ! -L \"$OLD\" ] && [ -d \"$OLD\" ] || { printf 'AM_DIAG:H:H05\\n' >&9; am_missing; }", 5),
                ("cd -- \"$OLD\" 2>/dev/null || am_missing", "cd -- \"$OLD\" 2>/dev/null || { printf 'AM_DIAG:H:H06\\n' >&9; am_missing; }", 6),
                ("[ ! -L \"$OLD\" ] && am_same \"$OLD\" . || am_missing", "[ ! -L \"$OLD\" ] && am_same \"$OLD\" . || { printf 'AM_DIAG:H:H07\\n' >&9; am_missing; }", 7),
                ("[ ! -L \"$K\" ] && [ -d \"$K\" ] || am_missing", "[ ! -L \"$K\" ] && [ -d \"$K\" ] || { printf 'AM_DIAG:H:H08\\n' >&9; am_missing; }", 8),
                ("cd -- \"$K\" 2>/dev/null || am_missing", "cd -- \"$K\" 2>/dev/null || { printf 'AM_DIAG:H:H09\\n' >&9; am_missing; }", 9),
                ("[ ! -L \"$OLD/$K\" ] && am_same \"$OLD/$K\" . || am_missing", "[ ! -L \"$OLD/$K\" ] && am_same \"$OLD/$K\" . || { printf 'AM_DIAG:H:H10\\n' >&9; am_missing; }", 10),
                ("[ ! -L \"$N\" ] && [ -f \"$N\" ] || am_missing", "[ ! -L \"$N\" ] && [ -f \"$N\" ] || { printf 'AM_DIAG:H:H11\\n' >&9; am_missing; }", 11),
                ("exec 3< \"$N\" || am_missing", "exec 3< \"$N\" || { printf 'AM_DIAG:H:H12\\n' >&9; am_missing; }", 12),
                ("am_same \"$N\" /dev/fd/3 && am_singlelink /dev/fd/3 || am_missing", "am_same \"$N\" /dev/fd/3 && am_singlelink /dev/fd/3 || { printf 'AM_DIAG:H:H13\\n' >&9; am_missing; }", 13),
                ("exec 5< . || am_missing", "exec 5< . || { printf 'AM_DIAG:H:H14\\n' >&9; am_missing; }", 14),
                ("am_same \"$SRC_DIR\" /dev/fd/5 || am_missing", "am_same \"$SRC_DIR\" /dev/fd/5 || { printf 'AM_DIAG:H:H15\\n' >&9; am_missing; }", 15),
                ("NEW_REAL=$(cd -- \"$NEW\" 2>/dev/null && pwd -P) || am_missing", "NEW_REAL=$(cd -- \"$NEW\" 2>/dev/null && pwd -P) || { printf 'AM_DIAG:H:CANON_NEW\\n' >&9; printf 'AM_DIAG:H:H16\\n' >&9; am_missing; }", 16),
                ("[ ! -L \"$DEST\" ] && [ -f \"$DEST\" ] || am_missing", "[ ! -L \"$DEST\" ] && [ -f \"$DEST\" ] || { printf 'AM_DIAG:H:H17\\n' >&9; am_missing; }", 17),
                ("exec 4< \"$DEST\" || am_missing", "exec 4< \"$DEST\" || { printf 'AM_DIAG:H:H18\\n' >&9; am_missing; }", 18),
                ("am_same \"$DEST\" /dev/fd/4 && am_singlelink /dev/fd/4 || am_missing", "am_same \"$DEST\" /dev/fd/4 && am_singlelink /dev/fd/4 || { printf 'AM_DIAG:H:H19\\n' >&9; am_missing; }", 19),
            ];
            for (needle, replacement, number) in missing {
                script = replace_count(&script, needle, replacement, 1, Site::Missing(number))?;
            }

            script = replace_count(
                &script,
                "    printf 'AM_SAME\\n'; exit 0\n  fi\nfi\n[ ! -L \"$OLD\" ]",
                "    printf 'AM_DIAG:H:H04\\n' >&9; printf 'AM_SAME\\n'; exit 0\n  fi\nfi\n[ ! -L \"$OLD\" ]",
                1,
                Site::SameEarly,
            )?;
            script = replace_count(
                &script,
                "[ \"$NEW_REAL\" = \"$OLD_REAL\" ] && { printf 'AM_SAME\\n'; exit 0; }",
                "if [ \"$NEW_REAL\" = \"$OLD_REAL\" ]; then printf 'AM_DIAG:H:H20\\n' >&9; printf 'AM_SAME\\n'; exit 0; else printf 'AM_DIAG:H:CANON_COMPARE\\n' >&9; fi",
                2,
                Site::SameLate,
            )?;

            script = replace_count(
                &script,
                "AM_FD_PY\n}",
                "AM_FD_PY\n  rc=$?\n  if [ \"$rc\" -ne 0 ]; then case \"$1\" in nlink1) printf 'AM_DIAG:H:NLINK_PRE\\n' >&9 ;; *) printf 'AM_DIAG:H:STAT_FALLBACK\\n' >&9 ;; esac; fi\n  return \"$rc\"\n}",
                1,
                Site::FdStat,
            )?;
            script = replace_count(
                &script,
                "if stat -c %Y \"$1\" >/dev/null 2>&1; then [ \"$1\" -ef \"$2\" ]; return; fi",
                "if stat -c %Y \"$1\" >/dev/null 2>&1; then [ \"$1\" -ef \"$2\" ]; rc=$?; if [ \"$rc\" -ne 0 ]; then printf 'AM_DIAG:H:STAT_SAME\\n' >&9; fi; return \"$rc\"; fi",
                1,
                Site::SameFunction,
            )?;
            script = replace_count(
                &script,
                "a=$(stat -L -f '%d %i' \"$1\" 2>/dev/null) || return 1;",
                "a=$(stat -L -f '%d %i' \"$1\" 2>/dev/null) || { printf 'AM_DIAG:H:STAT_SOURCE\\n' >&9; return 1; };",
                1,
                Site::StatSource,
            )?;
            script = replace_count(
                &script,
                "b=$(stat -L -f '%d %i' \"$2\" 2>/dev/null) || return 1;",
                "b=$(stat -L -f '%d %i' \"$2\" 2>/dev/null) || { printf 'AM_DIAG:H:STAT_SAME\\n' >&9; return 1; };",
                1,
                Site::StatSame,
            )?;
            script = replace_count(
                &script,
                "[ \"$a\" = \"$b\" ]",
                "[ \"$a\" = \"$b\" ]; rc=$?; if [ \"$rc\" -ne 0 ]; then printf 'AM_DIAG:H:STAT_SAME\\n' >&9; fi; return \"$rc\"",
                1,
                Site::StatCompare,
            )?;
            script = replace_count(
                &script,
                "l=$(stat -L -c %h \"$1\" 2>/dev/null) || return 1; [ \"$l\" = 1 ]; return;",
                "l=$(stat -L -c %h \"$1\" 2>/dev/null) || { printf 'AM_DIAG:H:NLINK_SAME\\n' >&9; return 1; }; [ \"$l\" = 1 ]; rc=$?; if [ \"$rc\" -ne 0 ]; then printf 'AM_DIAG:H:NLINK_SAME\\n' >&9; fi; return \"$rc\";",
                1,
                Site::NlinkGnu,
            )?;
            script = replace_count(
                &script,
                "l=$(stat -L -f %l \"$1\" 2>/dev/null) || return 1\n  [ \"$l\" = 1 ]",
                "l=$(stat -L -f %l \"$1\" 2>/dev/null) || { printf 'AM_DIAG:H:NLINK_SAME\\n' >&9; return 1; }\n  [ \"$l\" = 1 ]; rc=$?; if [ \"$rc\" -ne 0 ]; then printf 'AM_DIAG:H:NLINK_SAME\\n' >&9; fi; return \"$rc\"",
                1,
                Site::NlinkBsd,
            )?;
            script = replace_count(
                &script,
                "o=$(cd -- \"$OLD\" 2>/dev/null && pwd -P) || o=",
                "o=$(cd -- \"$OLD\" 2>/dev/null && pwd -P) || { printf 'AM_DIAG:H:CANON_OLD\\n' >&9; o=; }",
                1,
                Site::CanonOld,
            )?;
            script = replace_count(
                &script,
                "n=$(cd -- \"$NEW\" 2>/dev/null && pwd -P) || n=",
                "n=$(cd -- \"$NEW\" 2>/dev/null && pwd -P) || { printf 'AM_DIAG:H:CANON_NEW\\n' >&9; n=; }",
                1,
                Site::CanonNew,
            )?;
            Ok(format!("exec 9>&2\n{script}"))
        }

        pub(super) struct Run {
            pub(super) stdout: Vec<u8>,
            pub(super) status: Option<i32>,
        }

        pub(super) fn run(script: &str, handle: &Handle, path: Option<&str>) -> Result<Run, InstrumentError> {
            let copy = instrument(script.to_owned())?;
            let mut command = std::process::Command::new("/bin/sh");
            command.arg("-c").arg(copy);
            if let Some(path) = path {
                command.env("PATH", path);
            }
            let output = command.output().map_err(|_| InstrumentError { site: Site::MissingDefinition, expected: 1, found: 0 })?;
            let status = output.status.code();
            handle.record_output(status, stdout_kind(&output.stdout), &output.stderr, code_from_marker);
            Ok(Run { stdout: output.stdout, status })
        }

        fn run_original(script: &str, path: Option<&str>) -> (Vec<u8>, Option<i32>) {
            let mut command = std::process::Command::new("/bin/sh");
            command.arg("-c").arg(script);
            if let Some(path) = path {
                command.env("PATH", path);
            }
            match command.output() {
                Ok(output) => (output.stdout, output.status.code()),
                Err(_) => (Vec::new(), None),
            }
        }

        #[test]
        fn instrumented_staging_classifies_real_guards_without_changing_stdout_or_exit() {
            let base = super::tmp();
            let old = base.join("old");
            std::fs::create_dir_all(old.join("k")).unwrap();
            let target = base.join("private-fixture");
            std::fs::write(&target, b"fixture").unwrap();
            std::os::unix::fs::symlink(&target, old.join("k/sid.jsonl")).unwrap();
            let new = base.join("new");
            let missing_script = remote_stage_script(&old.to_string_lossy(), &new.to_string_lossy(), "k", "sid.jsonl", None);
            let original = run_original(&missing_script, None);
            let handle = Handle::new();
            let instrumented = run(&missing_script, &handle, None).unwrap();
            assert!(original.0 == instrumented.stdout, "H diagnostics changed missing-case stdout");
            assert_eq!(original.1, instrumented.status);
            assert!(handle.contains(Code::Missing(11)), "{}", handle.summary());
            assert!(matches!(stdout_kind(&instrumented.stdout), StdoutKind::Missing));

            // The same-root case needs a regular transcript, not the prior rejection fixture.
            std::fs::remove_file(old.join("k/sid.jsonl")).unwrap();
            std::fs::write(old.join("k/sid.jsonl"), b"fixture").unwrap();
            let link = base.join("same");
            std::os::unix::fs::symlink(&old, &link).unwrap();
            let same_script = remote_stage_script(&old.to_string_lossy(), &link.to_string_lossy(), "k", "sid.jsonl", None);
            let original = run_original(&same_script, None);
            let handle = Handle::new();
            let instrumented = run(&same_script, &handle, None).unwrap();
            assert!(original.0 == instrumented.stdout, "H diagnostics changed same-case stdout");
            assert_eq!(original.1, instrumented.status);
            assert!(handle.contains(Code::Same(4)), "{}", handle.summary());
        }

        #[test]
        fn an_actual_same_file_identity_failure_gets_a_fixed_classification() {
            let make_case = || {
                let base = super::tmp();
                let old = base.join("old");
                std::fs::create_dir_all(old.join("k")).unwrap();
                let source = old.join("k/sid.jsonl");
                std::fs::write(&source, b"fixture").unwrap();
                let secret = base.join("other-file");
                std::fs::write(&secret, b"other fixture").unwrap();
                let shim_dir = base.join("bin");
                std::fs::create_dir_all(&shim_dir).unwrap();
                let source_q = crate::hosts::sh_quote(&source.to_string_lossy());
                let secret_q = crate::hosts::sh_quote(&secret.to_string_lossy());
                let shim = shim_dir.join("stat");
                std::fs::write(
                    &shim,
                    format!(
                        "#!/bin/sh\nif [ \"$1\" = \"-c\" ] && [ \"$2\" = \"%Y\" ]; then\n  if [ \"$3\" = sid.jsonl ]; then rm -f {source_q}; ln -s {secret_q} {source_q}; fi\n  printf '0\\n'; exit 0\nfi\nexec /usr/bin/stat \"$@\"\n"
                    ),
                )
                .unwrap();
                std::fs::set_permissions(&shim, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
                let new = base.join("new");
                let script = remote_stage_script(&old.to_string_lossy(), &new.to_string_lossy(), "k", "sid.jsonl", None);
                let path = format!("{}:{}", shim_dir.display(), std::env::var("PATH").unwrap_or_default());
                (base, script, path)
            };

            let (original_base, original_script, original_path) = make_case();
            let original = run_original(&original_script, Some(&original_path));
            let (instrumented_base, instrumented_script, instrumented_path) = make_case();
            let handle = Handle::new();
            let instrumented = run(&instrumented_script, &handle, Some(&instrumented_path)).unwrap();
            assert!(original.0 == instrumented.stdout, "H diagnostics changed same-file rejection stdout");
            assert_eq!(original.1, instrumented.status);
            assert!(handle.contains(Code::Missing(13)), "{}", handle.summary());
            assert!(handle.contains(Code::StatSame), "{}", handle.summary());
            let _ = std::fs::remove_dir_all(original_base);
            let _ = std::fs::remove_dir_all(instrumented_base);
        }

        #[test]
        fn diagnostics_drop_unknown_stderr_bound_records_and_reject_changed_generators() {
            let handle = Handle::new();
            handle.record_output(
                Some(0),
                stdout_kind(b"AM_MISSING\n"),
                b"AM_DIAG:H:H11\nUNKNOWN_SENTINEL_PATH_PAYLOAD\n",
                code_from_marker,
            );
            assert!(handle.contains(Code::Missing(11)));
            assert!(!handle.summary().contains("UNKNOWN_SENTINEL_PATH_PAYLOAD"));
            for _ in 0..RECORD_LIMIT + 2 {
                handle.record_code(Code::Missing(11), StdoutKind::Empty);
            }
            assert_eq!(handle.length_and_overflow(), (RECORD_LIMIT, 3));

            let base = super::tmp();
            let script = remote_stage_script("/tmp/old", "/tmp/new", "k", "sid.jsonl", None);
            let missing_callsite = script.replace("cd -- \"$OLD\" 2>/dev/null || am_missing", "cd -- \"$OLD\" 2>/dev/null || true");
            assert!(instrument(missing_callsite).is_err(), "missing required H guard was accepted");
            let duplicate_callsite = format!("{script}\ncd -- \"$OLD\" 2>/dev/null || am_missing\n");
            assert!(instrument(duplicate_callsite).is_err(), "duplicate H guard was accepted");
            let _ = std::fs::remove_dir_all(base);
        }
    }

    fn tmp() -> std::path::PathBuf {
        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-remote-stage-{}", db::ulid())));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn symlinked_projects_dirs_report_same_and_copy_nothing() {
        let base = tmp();
        let old = base.join("old-projects");
        std::fs::create_dir_all(old.join("cwd-key")).unwrap();
        std::fs::write(old.join("cwd-key").join("sid.jsonl"), "orig").unwrap();
        let new = base.join("new-projects");
        std::os::unix::fs::symlink(&old, &new).unwrap();

        let script = remote_stage_script(&old.to_string_lossy(), &new.to_string_lossy(), "cwd-key", "sid.jsonl", None);
        let out = run_script_locally(&script);
        assert!(out.contains("AM_SAME"), "{out}");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn different_dirs_copy_the_transcript_and_its_companion() {
        let base = tmp();
        let old = base.join("old-projects");
        std::fs::create_dir_all(old.join("cwd-key")).unwrap();
        std::fs::write(old.join("cwd-key").join("sid.jsonl"), "hello").unwrap();
        std::fs::create_dir_all(old.join("cwd-key").join("sid")).unwrap();
        std::fs::write(old.join("cwd-key").join("sid").join("extra.txt"), "companion").unwrap();
        let new = base.join("new-projects"); // 還不存在，要靠 mkdir -p 建出來

        let script = remote_stage_script(&old.to_string_lossy(), &new.to_string_lossy(), "cwd-key", "sid.jsonl", Some("sid"));
        let out = run_script_locally(&script);
        assert!(out.contains("AM_STAGED"), "{out}");
        assert_eq!(std::fs::read_to_string(new.join("cwd-key").join("sid.jsonl")).unwrap(), "hello");
        assert_eq!(std::fs::read_to_string(new.join("cwd-key").join("sid").join("extra.txt")).unwrap(), "companion");

        let _ = std::fs::remove_dir_all(&base);
    }

    // ---- 對抗式審查（資料安全）：遠端 script 的暫存檔／權限／覆蓋規則要跟本機版一致 ----

    fn mode(p: &std::path::Path) -> u32 {
        use std::os::unix::fs::PermissionsExt;
        std::fs::metadata(p).unwrap().permissions().mode() & 0o777
    }

    fn names(dir: &std::path::Path) -> Vec<String> {
        let mut v: Vec<String> = std::fs::read_dir(dir).unwrap().flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect();
        v.sort();
        v
    }

    /// 形狀不對的遠端來源（例如指到 `auth.json`）：連 ssh 都不碰就退回開新對話；host 刻意是連不上的假名，
    /// 要是有去連會是另一種失敗訊息，但一樣是 `transcript_missing`，所以另外確認 script 根本沒被組出來。
    #[tokio::test]
    async fn a_remote_source_of_the_wrong_shape_is_refused_before_any_ssh() {
        assert!(!crate::lifecycle::transcript_stage::has_claude_transcript_shape("/home/u/.codex/auth.json"));
        assert!(!crate::lifecycle::transcript_stage::has_claude_transcript_shape("/home/u/.claude/projects/k/sid.txt"));
        assert!(!crate::lifecycle::transcript_stage::has_claude_transcript_shape("/home/u/.claude/other/k/sid.jsonl"));
        assert!(!crate::lifecycle::transcript_stage::has_claude_transcript_shape("/home/u/.claude/projects/../auth.jsonl"), "remote staging must reject a traversal cwd component");
        assert!(crate::lifecycle::transcript_stage::has_claude_transcript_shape("/home/u/.claude-cc1/projects/-Users-x/sid.jsonl"));
    }

    #[test]
    fn the_remote_copy_is_private_and_leaves_no_temp_file() {
        use std::os::unix::fs::PermissionsExt;
        let base = tmp();
        let old = base.join("old-projects");
        std::fs::create_dir_all(old.join("k/sid/sub")).unwrap();
        std::fs::write(old.join("k/sid.jsonl"), "hello").unwrap();
        std::fs::write(old.join("k/sid/sub/n.txt"), "c").unwrap();
        for f in ["k/sid.jsonl", "k/sid/sub/n.txt"] {
            std::fs::set_permissions(old.join(f), std::fs::Permissions::from_mode(0o644)).unwrap();
        }
        let new = base.join("new-projects");
        let script = remote_stage_script(&old.to_string_lossy(), &new.to_string_lossy(), "k", "sid.jsonl", Some("sid"));
        assert!(run_script_locally(&script).contains("AM_STAGED"));
        assert_eq!(mode(&new.join("k/sid.jsonl")), 0o600);
        assert_eq!(mode(&new.join("k")), 0o700);
        assert_eq!(mode(&new.join("k/sid/sub/n.txt")), 0o600);
        assert_eq!(names(&new.join("k")), vec!["sid".to_string(), "sid.jsonl".to_string()], "不能留下暫存檔");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn the_remote_copy_never_truncates_a_destination_that_already_continued() {
        let base = tmp();
        let old = base.join("old-projects");
        std::fs::create_dir_all(old.join("k")).unwrap();
        std::fs::write(old.join("k/sid.jsonl"), "{\"a\":1}\n").unwrap();
        let new = base.join("new-projects");
        std::fs::create_dir_all(new.join("k")).unwrap();
        std::fs::write(new.join("k/sid.jsonl"), "{\"a\":1}\n{\"b\":2}\n").unwrap();
        let script = remote_stage_script(&old.to_string_lossy(), &new.to_string_lossy(), "k", "sid.jsonl", None);
        assert!(run_script_locally(&script).contains("AM_STAGED"));
        assert_eq!(std::fs::read_to_string(new.join("k/sid.jsonl")).unwrap(), "{\"a\":1}\n{\"b\":2}\n");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_diverged_remote_destination_is_set_aside_not_destroyed() {
        let base = tmp();
        let old = base.join("old-projects");
        std::fs::create_dir_all(old.join("k")).unwrap();
        std::fs::write(old.join("k/sid.jsonl"), "{\"a\":1}\n{\"src\":1}\n").unwrap();
        let new = base.join("new-projects");
        std::fs::create_dir_all(new.join("k")).unwrap();
        std::fs::write(new.join("k/sid.jsonl"), "{\"a\":1}\n{\"dest\":1}\n").unwrap();
        let script = remote_stage_script(&old.to_string_lossy(), &new.to_string_lossy(), "k", "sid.jsonl", None);
        assert!(run_script_locally(&script).contains("AM_STAGED"));
        assert_eq!(std::fs::read_to_string(new.join("k/sid.jsonl")).unwrap(), "{\"a\":1}\n{\"src\":1}\n");
        let aside: Vec<String> = names(&new.join("k")).into_iter().filter(|n| n != "sid.jsonl").collect();
        assert_eq!(aside.len(), 1, "{aside:?}");
        assert!(!aside[0].ends_with(".jsonl"), "{aside:?}");
        assert_eq!(std::fs::read_to_string(new.join("k").join(&aside[0])).unwrap(), "{\"a\":1}\n{\"dest\":1}\n");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// 假的 BSD `stat`：不認 `-c`（逼腳本走 macOS 那條路），`-f` 照 macOS 的行為——對 `/dev/fd/N` 回 devfs 的裝置編號
    /// （2026-10-04 mac_builder 實測：檔案 dev 16777234、`/dev/fd/3` dev 1318745794，inode 相同）。
    fn bsd_stat_dir() -> std::path::PathBuf {
        let dir = tmp().join("bsd-bin");
        std::fs::create_dir_all(&dir).unwrap();
        let shim = dir.join("stat");
        std::fs::write(
            &shim,
            r#"#!/bin/sh
fmt=; path=
while [ $# -gt 0 ]; do
  case "$1" in -c) exit 1;; -L) ;; -f) fmt=$2; shift;; *) path=$1;; esac
  shift
done
gfmt=$(printf '%s' "$fmt" | sed -e 's/%l/%h/g' -e 's/%z/%s/g')
case "$path" in
  /dev/fd/*) out=$(/usr/bin/stat -L -c "$gfmt" "$path") || exit 1; printf '%s\n' "$out" | sed 's/^[0-9]* /1318745794 /' ;;
  *) /usr/bin/stat -L -c "$gfmt" "$path" ;;
esac
"#,
        )
        .unwrap();
        std::fs::set_permissions(&shim, std::os::unix::fs::PermissionsExt::from_mode(0o755)).unwrap();
        dir
    }

    /// 2026-10-04：macOS 上 `/dev/fd/N` 的裝置編號是 devfs 的，以前比對永遠失敗 → `AM_MISSING` → 每次重啟都開新對話。
    #[test]
    fn a_bsd_host_stages_the_transcript_instead_of_calling_it_missing() {
        let base = tmp();
        let old = base.join("old-projects");
        std::fs::create_dir_all(old.join("k")).unwrap();
        std::fs::write(old.join("k/sid.jsonl"), "{\"a\":1}\n").unwrap();
        let new = base.join("new-projects");
        let script = remote_stage_script(&old.to_string_lossy(), &new.to_string_lossy(), "k", "sid.jsonl", None);
        let path = format!("{}:{}", bsd_stat_dir().display(), std::env::var("PATH").unwrap_or_default());
        let (out, diag) = run_with_diagnostics(&script, Some(&path));
        assert!(out.contains("AM_STAGED"), "BSD stat staging output; {}", diag.summary());
        assert!(std::fs::read_to_string(new.join("k/sid.jsonl")).unwrap() == "{\"a\":1}\n", "BSD fixture content changed");
        // 同一個 projects（console-rpa 的情形：身份沒換）回 AM_SAME。
        let same = remote_stage_script(&old.to_string_lossy(), &old.to_string_lossy(), "k", "sid.jsonl", None);
        let path = format!("{}:{}", bsd_stat_dir().display(), std::env::var("PATH").unwrap_or_default());
        let (out, diag) = run_with_diagnostics(&same, Some(&path));
        assert!(out.contains("AM_SAME"), "BSD same-path output; {}", diag.summary());
        let _ = std::fs::remove_dir_all(&base);
    }

    /// 2026-10-13 ops-web（mac_builder）：`~/.claude-cc1/projects` 是指向 `~/.claude/projects` 的 symlink。從 cc1 換回 cc0 時
    /// 來源那邊是 symlink，以前回 AM_MISSING、開新對話。兩個方向都要回 AM_SAME，而且不留下任何暫存檔。
    #[test]
    fn identities_sharing_one_projects_dir_through_a_symlink_are_the_same_place_both_ways() {
        let base = tmp();
        let real = base.join("real-projects");
        std::fs::create_dir_all(real.join("k")).unwrap();
        std::fs::write(real.join("k/sid.jsonl"), "{\"a\":1}\n").unwrap();
        let link = base.join("linked-projects");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        for (old, new) in [(&link, &real), (&real, &link)] {
            let script = remote_stage_script(&old.to_string_lossy(), &new.to_string_lossy(), "k", "sid.jsonl", None);
            let (out, diag) = run_with_diagnostics(&script, None);
            assert!(out.contains("AM_SAME"), "shared projects identity output; {}", diag.summary());
        }
        let names: Vec<_> = std::fs::read_dir(real.join("k")).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names.len(), 1, "不該留下暫存檔或備份：{names:?}");
        // 共用目錄裡那個檔本身是 symlink 時照舊不認。
        let secret = base.join("secret");
        std::fs::write(&secret, "TOKEN").unwrap();
        std::os::unix::fs::symlink(&secret, real.join("k/evil.jsonl")).unwrap();
        let script = remote_stage_script(&link.to_string_lossy(), &real.to_string_lossy(), "k", "evil.jsonl", None);
        let (out, diag) = run_with_diagnostics(&script, None);
        assert!(out.contains("AM_MISSING"), "shared symlink-file rejection; {}", diag.summary());
        assert!(diag.contains(diagnostics::Code::Missing(5)), "{}", diag.summary());
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_symlinked_remote_source_is_reported_missing_and_copies_nothing() {
        let base = tmp();
        let old = base.join("old-projects");
        std::fs::create_dir_all(old.join("k")).unwrap();
        let secret = base.join("secret");
        std::fs::write(&secret, "TOKEN").unwrap();
        std::os::unix::fs::symlink(&secret, old.join("k/sid.jsonl")).unwrap();
        let new = base.join("new-projects");
        let script = remote_stage_script(&old.to_string_lossy(), &new.to_string_lossy(), "k", "sid.jsonl", None);
        let (out, diag) = run_with_diagnostics(&script, None);
        assert!(out.contains("AM_MISSING"), "symlinked source rejection; {}", diag.summary());
        assert!(diag.contains(diagnostics::Code::Missing(11)), "{}", diag.summary());
        assert!(!new.exists(), "符號連結被跟著複製");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_remote_projects_symlink_cannot_import_a_transcript_from_another_tree() {
        let base = tmp();
        let real = base.join("private-projects");
        std::fs::create_dir_all(real.join("k")).unwrap();
        std::fs::write(real.join("k/sid.jsonl"), "private transcript").unwrap();
        let old = base.join("old-projects");
        std::os::unix::fs::symlink(&real, &old).unwrap();
        let new = base.join("new-projects");
        let script = remote_stage_script(&old.to_string_lossy(), &new.to_string_lossy(), "k", "sid.jsonl", None);
        let (out, diag) = run_with_diagnostics(&script, None);
        assert!(out.contains("AM_MISSING"), "projects symlink rejection; {}", diag.summary());
        assert!(diag.contains(diagnostics::Code::Missing(5)), "{}", diag.summary());
        assert!(!new.exists(), "transcript outside the owned projects tree was copied");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_remote_destination_cwd_symlink_cannot_write_into_another_tree() {
        let base = tmp();
        let old = base.join("old-projects");
        std::fs::create_dir_all(old.join("k")).unwrap();
        std::fs::write(old.join("k/sid.jsonl"), "private transcript").unwrap();
        let outside = base.join("outside");
        std::fs::create_dir(&outside).unwrap();
        let new = base.join("new-projects");
        std::fs::create_dir(&new).unwrap();
        std::os::unix::fs::symlink(&outside, new.join("k")).unwrap();
        let script = remote_stage_script(&old.to_string_lossy(), &new.to_string_lossy(), "k", "sid.jsonl", None);
        let (out, diag) = run_with_diagnostics(&script, None);
        assert!(out.contains("AM_MKDIR_FAILED") || out.contains("AM_MISSING"), "destination symlink rejection; {}", diag.summary());
        assert!(!outside.join("sid.jsonl").exists(), "destination cwd symlink was followed");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_remote_hard_linked_source_is_not_treated_as_a_transcript() {
        let base = tmp();
        let old = base.join("old-projects");
        std::fs::create_dir_all(old.join("k")).unwrap();
        let secret = base.join("credential");
        std::fs::write(&secret, "sensitive value").unwrap();
        std::fs::hard_link(&secret, old.join("k/sid.jsonl")).unwrap();
        let new = base.join("new-projects");
        let script = remote_stage_script(&old.to_string_lossy(), &new.to_string_lossy(), "k", "sid.jsonl", None);
        let (out, diag) = run_with_diagnostics(&script, None);
        assert!(out.contains("AM_MISSING"), "hard-linked source rejection; {}", diag.summary());
        assert!(diag.contains(diagnostics::Code::Missing(13)), "{}", diag.summary());
        assert!(diag.contains(diagnostics::Code::NlinkSame), "{}", diag.summary());
        assert!(!new.exists(), "hard-linked credential was copied as a transcript");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn a_source_swapped_to_a_symlink_at_copy_time_is_not_read() {
        let base = tmp();
        let old = base.join("old-projects");
        std::fs::create_dir_all(old.join("k")).unwrap();
        let src = old.join("k/sid.jsonl");
        std::fs::write(&src, "ordinary transcript").unwrap();
        let secret = base.join("credential");
        std::fs::write(&secret, "sensitive value").unwrap();
        let new = base.join("new-projects");
        let script = remote_stage_script(&old.to_string_lossy(), &new.to_string_lossy(), "k", "sid.jsonl", None);
        let injected = format!(
            "cat() {{ rm -f {src}; ln -s {secret} {src}; command cat \"$@\"; }}\n{script}",
            src = crate::hosts::sh_quote(&src.to_string_lossy()),
            secret = crate::hosts::sh_quote(&secret.to_string_lossy()),
        );
        let (out, diag) = run_with_diagnostics(&injected, None);
        assert!(out.contains("AM_STAGED"), "copy-time symlink swap staging; {}", diag.summary());
        assert!(std::fs::read_to_string(new.join("k/sid.jsonl")).unwrap() == "ordinary transcript", "TOCTOU swap must keep reading the opened fd");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// `[ -f "$SRC" ]` 那道守衛真的擋住了：來源檔不存在時不會往下跑 `mkdir`／`cp`，
    /// 只印 `AM_MISSING`，也不會建出目的目錄。
    #[test]
    fn a_missing_source_file_reports_missing_and_creates_nothing() {
        let base = tmp();
        let old = base.join("old-projects"); // 連目錄都沒建，模擬來源徹底不存在
        let new = base.join("new-projects");

        let script = remote_stage_script(&old.to_string_lossy(), &new.to_string_lossy(), "cwd-key", "sid.jsonl", None);
        let (out, diag) = run_with_diagnostics(&script, None);
        assert!(out.contains("AM_MISSING"), "missing source output; {}", diag.summary());
        assert!(diag.contains(diagnostics::Code::Missing(5)), "{}", diag.summary());
        assert!(!new.exists(), "missing source must not create destination");

        let _ = std::fs::remove_dir_all(&base);
    }

    /// `mkdir -p ... ||` 那道守衛真的擋住了：目的路徑的上一層其實是一個檔案（不是目錄），
    /// `mkdir -p` 一定失敗，腳本要印 `AM_MKDIR_FAILED`，不能繼續往下跑 `cp` 假裝成功。
    #[test]
    fn an_unmakeable_destination_reports_mkdir_failed() {
        let base = tmp();
        let old = base.join("old-projects");
        std::fs::create_dir_all(old.join("cwd-key")).unwrap();
        std::fs::write(old.join("cwd-key").join("sid.jsonl"), "hello").unwrap();
        let blocker = base.join("blocker");
        std::fs::write(&blocker, "this is a file, not a directory").unwrap();
        let new = blocker.join("projects"); // 上一層是檔案，底下建不出任何東西

        let script = remote_stage_script(&old.to_string_lossy(), &new.to_string_lossy(), "cwd-key", "sid.jsonl", None);
        let out = run_script_locally(&script);
        assert!(out.contains("AM_MKDIR_FAILED"), "{out}");

        let _ = std::fs::remove_dir_all(&base);
    }

    /// Claude 的 projects 目錄名是 `-Users-…`。`cd "$K"` 會把前導 `-` 當成選項，搬檔被判成不存在。
    #[test]
    fn a_project_dir_whose_name_starts_with_a_dash_is_still_staged() {
        let base = tmp();
        let cwd = "-Users-me-project";
        let old = base.join("old-projects");
        std::fs::create_dir_all(old.join(cwd)).unwrap();
        std::fs::write(old.join(cwd).join("sid.jsonl"), "hello").unwrap();
        let new = base.join("new-projects");
        let script = remote_stage_script(&old.to_string_lossy(), &new.to_string_lossy(), cwd, "sid.jsonl", None);
        let out = run_script_locally(&script);
        assert!(out.contains("AM_STAGED"), "{out}");
        assert_eq!(std::fs::read_to_string(new.join(cwd).join("sid.jsonl")).unwrap(), "hello");
        let _ = std::fs::remove_dir_all(&base);
    }

    /// 目錄名帶著 shell 特殊字元（`$(...)`、單引號）不能被當成指令執行——每段路徑各自
    /// `sh_quote`，不是把值原樣黏進雙引號字串裡。用一個會在展開時建出檔案的 `$(...)` 當
    /// canary：quoting 對了就不會被展開，那個檔案就不會出現。
    #[test]
    fn shell_metacharacters_in_a_directory_name_are_not_executed() {
        let base = tmp();
        let weird_cwd = "it's-$(printf pwned)-cwd";
        let old = base.join("old-projects");
        std::fs::create_dir_all(old.join(&weird_cwd)).unwrap();
        std::fs::write(old.join(&weird_cwd).join("sid.jsonl"), "hello").unwrap();
        let new = base.join("new-projects");

        let script = remote_stage_script(&old.to_string_lossy(), &new.to_string_lossy(), &weird_cwd, "sid.jsonl", None);
        let out = run_script_locally(&script);
        assert!(out.contains("AM_STAGED"), "{out}");
        assert_eq!(std::fs::read_to_string(new.join(&weird_cwd).join("sid.jsonl")).unwrap(), "hello");

        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn parse_stage_output_only_trusts_the_success_markers() {
        assert_eq!(parse_stage_output("AM_SAME\n"), Ok(()));
        assert_eq!(parse_stage_output("AM_STAGED\n"), Ok(()));
        assert_eq!(parse_stage_output("AM_MISSING\n"), Err("transcript_missing"));
        assert_eq!(parse_stage_output("AM_MKDIR_FAILED\n"), Err("transcript_missing"));
        assert_eq!(parse_stage_output("AM_COPY_FAILED\n"), Err("transcript_missing"));
        assert_eq!(parse_stage_output(""), Err("transcript_missing"), "ssh 連上了但腳本什麼都沒印，不能當成功");
        assert_eq!(parse_stage_output("Permission denied (publickey)\n"), Err("transcript_missing"), "ssh 本身報錯的輸出，不能被誤判成任何一個 AM_* 標記");
    }

    /// 主機根本沒連線（`app.hosts` 裡沒有這個名字）：連 ssh 都沒機會跑，直接 fail closed。
    #[tokio::test]
    async fn an_unconfigured_host_fails_closed_without_touching_ssh() {
        let e = env().await;
        let pm = claude_bot(&e.app, &e.project_id, "pm").await;
        let transcript = e.dir.join("remote-pm.jsonl");
        std::fs::write(&transcript, "{}\n").unwrap();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, native_session_id, transcript_path, started_at, ended_at)
             VALUES (?,?,'stopped','idle','sid-remote-1',?,'2026-09-01T00:00:00Z','2026-09-01T00:01:00Z')",
        )
        .bind(db::ulid())
        .bind(&pm.id)
        .bind(transcript.to_str().unwrap())
        .execute(e.app.db())
        .await
        .unwrap();

        let bot = db::bot(e.app.db(), &pm.id).await.unwrap().unwrap();
        let plan = native_resume_plan(&e.app, &bot, "no-such-host", false, None).await.unwrap();
        assert_eq!(plan.map_err(|failure| failure.reason), Err("transcript_missing"));
    }

    #[tokio::test]
    async fn remote_transcript_staging_skips_unreadable_home_and_retries_with_remote_home() {
        let e = env().await;
        let pm = claude_bot(&e.app, &e.project_id, "pm").await;
        let host = format!("resume-home-616-{}", db::ulid().to_ascii_lowercase());
        let conn = e.app.hosts.insert_remote_for_test(crate::config::HostCfg { daemon_url: None,
            shared_session: false,
            name: host.clone(),
            ssh: "unused".into(),
            ssh_port: 22,
            ssh_opts: vec![],
            herdr_session: "agents-manager".into(),
            remote_path: String::new(),
        }).await;
        let transcript = e.dir.join("projects/-k/remote-home-resume.jsonl");
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        std::fs::write(&transcript, "{}\n").unwrap();
        let calls = std::sync::Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let calls2 = calls.clone();
        crate::hosts::set_ssh_fake(&host, move |script| {
            calls2.lock().unwrap().push(script.to_string());
            Err(anyhow::anyhow!("injected remote HOME read failure"))
        });

        assert_eq!(stage_cross_identity_transcript_remote(&e.app, &pm, &host, transcript.to_str().unwrap()).await, Err("transcript_missing"));
        assert_eq!(calls.lock().unwrap().len(), 1, "do not run a transcript-copy script with unknown identity HOME");

        *conn.remote_home.lock().await = Some("/home/remote-resume".into());
        let calls2 = calls.clone();
        crate::hosts::set_ssh_fake(&host, move |script| {
            calls2.lock().unwrap().push(script.to_string());
            Ok("AM_SAME\n".into())
        });
        assert_eq!(stage_cross_identity_transcript_remote(&e.app, &pm, &host, transcript.to_str().unwrap()).await, Ok(()));
        assert!(calls.lock().unwrap().last().unwrap().contains("/home/remote-resume/.claude/projects"), "retry uses the recovered remote HOME");
    }

    /// 主機有設定，但連不上（沒有東西在聽那個 port，connection refused）：ssh_exec 真的失敗，
    /// 一樣要 fail closed，不能假裝已經搬過去——這是能不碰真實遠端主機驗到的最大範圍
    /// （issue #95 明講：沒有可重用的 live-SSH 測試環境時，用刻意連不上的假 host 驗證）。
    #[tokio::test]
    async fn an_unreachable_host_fails_closed_instead_of_pretending_to_stage() {
        let e = env().await;
        let pm = claude_bot(&e.app, &e.project_id, "pm").await;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener); // 沒有人在聽這個 port 了：接下來連過去是 connection refused，快速失敗。

        let host_cfg = crate::config::HostCfg { daemon_url: None,
            shared_session: false,
            name: "unreachable-box".into(),
            ssh: "127.0.0.1".into(),
            ssh_port: port,
            ssh_opts: vec!["-o".into(), "ConnectTimeout=2".into()],
            herdr_session: "agents-manager".into(),
            remote_path: String::new(),
        };
        e.app.hosts.apply_config(&e.app, &[host_cfg]).await;

        let transcript = e.dir.join("remote-pm-2.jsonl");
        std::fs::write(&transcript, "{}\n").unwrap();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, native_session_id, transcript_path, started_at, ended_at)
             VALUES (?,?,'stopped','idle','sid-remote-2',?,'2026-09-01T00:00:00Z','2026-09-01T00:01:00Z')",
        )
        .bind(db::ulid())
        .bind(&pm.id)
        .bind(transcript.to_str().unwrap())
        .execute(e.app.db())
        .await
        .unwrap();

        let bot = db::bot(e.app.db(), &pm.id).await.unwrap().unwrap();
        let plan = tokio::time::timeout(Duration::from_secs(20), native_resume_plan(&e.app, &bot, "unreachable-box", false, None))
            .await
            .expect("連不上要快速失敗，不能卡住整個重啟流程")
            .unwrap();
        assert_eq!(plan.map_err(|failure| failure.reason), Err("transcript_missing"), "連不上遠端主機，不能假裝已經搬過去");

        e.app.hosts.remove(&e.app, "unreachable-box").await;
    }
}

/// 2026-09-10 23:02: restart repeatedly beside a tight reconcile loop; every restart must
/// return the run it started, alive (no adoption in a stop/start gap).
#[tokio::test]
async fn restarts_racing_a_reconcile_loop_always_come_back() {
    let e = env().await;
    let pm = claude_bot(&e.app, &e.project_id, "pm").await;
    start_bot(&e.app, &pm.id).await.unwrap();

    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let (app2, stop2) = (e.app.clone(), stop.clone());
    let looper = tokio::spawn(async move {
        let mut rounds = 0u32;
        while !stop2.load(std::sync::atomic::Ordering::SeqCst) {
            let _ = crate::reconcile::reconcile_host(&app2, crate::config::LOCAL_HOST).await;
            rounds += 1;
            tokio::task::yield_now().await;
        }
        rounds
    });

    for i in 0..15 {
        let started = match crate::lifecycle::restart_bot_with(&e.app, &pm.id, StartOpts::default()).await {
            Ok(id) => id,
            Err(e) => panic!("restart {i} was refused while a reconcile was running: {e:?}"),
        };
        let active = db::active_run(e.app.db(), &pm.id).await.unwrap().expect("a run after the restart");
        assert_eq!(active.id, started, "restart {i}: the active run is the one the restart started, not an adopted leftover");
    }
    stop.store(true, std::sync::atomic::Ordering::SeqCst);
    let rounds = looper.await.unwrap();
    assert!(rounds > 0, "the reconcile loop actually ran alongside the restarts");

    let run = db::active_run(e.app.db(), &pm.id).await.unwrap().unwrap();
    let bot = db::bot(e.app.db(), &pm.id).await.unwrap().unwrap();
    assert!(crate::lifecycle::run_alive(&e.app, &run, &bot).await, "the surviving run has a live pane and agent");
    let live: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM runs WHERE bot_id=? AND state IN ('starting','running','stopping')")
        .bind(&pm.id)
        .fetch_one(e.app.db())
        .await
        .unwrap();
    assert_eq!(live, 1, "exactly one live run");
}
