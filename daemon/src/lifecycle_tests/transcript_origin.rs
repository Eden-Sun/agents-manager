#![allow(unused_imports)]
mod tests {
    use crate::lifecycle::transcript_origin::*;
    use crate::lifecycle::*;
    use crate::lifecycle_tests::prelude::*;
    use crate::db;
    use crate::testing as tt;

    /// 2026-09-19 真機實抓（claude 2.1.278）：使用者 prompt（`origin.kind: human`）→ 工具呼叫與結果 → 回覆 → 背景 shell 完成後
    /// claude 自己接的一回合（`queue-operation` → `origin.kind: task-notification`）→ 回覆。
    const SAMPLE: &str = include_str!("../../../crates/am-lifecycle/src/lifecycle/fixtures/claude_2.1.278_task_notification.jsonl");
    /// 上一則使用者 prompt 還留在畫面上（那一則已經記在上一回合底下）；自己起的那一回合沒有新的 `❯` 回音。
    const SCREEN_AFTER_A_NOTIFICATION_TURN: &str = "❯ 用 Bash 工具的 run_in_background 參數跑一個背景指令：sleep 20; echo done\n⏺ 已在背景啟動，20 秒後會完成。\n✻ Worked for 3s\n⏺ 背景任務已完成。\n──────\n❯\n";
    const SCREEN_TYPED_IN_THE_PANE: &str = "❯ 順便看一下 lint\n──────\n❯\n";

    #[test]
    fn the_sample_says_who_started_the_turn() {
        assert_eq!(starter_origin_kind(SAMPLE).as_deref(), Some("task-notification"), "最後一回合是背景 shell 完成的通知");
        let human_turn: Vec<&str> = SAMPLE.lines().take(6).collect();
        assert_eq!(starter_origin_kind(&human_turn.join("\n")).as_deref(), Some("human"));
        let tool_result = SAMPLE.lines().find(|l| l.contains("tool_result")).expect("樣本裡有一筆真的工具結果");
        assert_eq!(starter_origin_kind(&format!("{SAMPLE}{tool_result}\n")).as_deref(), Some("task-notification"));
        assert_eq!(starter_origin_kind("{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"hi\"}}\nnot json\n"), None);
        assert_eq!(starter_origin_kind(""), None);
    }

    async fn with_transcript(log: &str) -> (tt::Env, db::Run, String) {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "notified").await;
        let config_dir = env.dir.join("claude-config");
        sqlx::query("UPDATE bots SET env_json=? WHERE id=?")
            .bind(serde_json::json!({"CLAUDE_CONFIG_DIR":config_dir.to_string_lossy()}).to_string())
            .bind(&bot.id)
            .execute(app.db())
            .await
            .unwrap();
        let run_id = tt::fake_run(&app, &bot.id).await;
        let path = config_dir.join("projects/-test/session.jsonl");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, log).unwrap();
        sqlx::query("UPDATE runs SET transcript_path=? WHERE id=?").bind(path.to_string_lossy().to_string()).bind(&run_id).execute(app.db()).await.unwrap();
        env.herdr.set_screen(&format!("pane-{}", bot.id), SCREEN_AFTER_A_NOTIFICATION_TURN);
        let run = db::run(app.db(), &run_id).await.unwrap().unwrap();
        (env, run, bot.id)
    }

    async fn user_messages(env: &tt::Env, bot_id: &str) -> Vec<String> {
        let conv = db::conversation_id(env.app.db(), bot_id).await.unwrap();
        sqlx::query_scalar("SELECT content FROM messages WHERE conversation_id=? AND role='user' ORDER BY created_at")
            .bind(conv)
            .fetch_all(env.app.db())
            .await
            .unwrap()
    }

    async fn external_turns(env: &tt::Env, bot_id: &str) -> Vec<(String, String)> {
        let conv = db::conversation_id(env.app.db(), bot_id).await.unwrap();
        sqlx::query_as("SELECT origin, status FROM turns WHERE conversation_id=?").bind(conv).fetch_all(env.app.db()).await.unwrap()
    }

    /// 事故（2026-09-19 真機，#224）：claude 自己接的回合（背景 shell 完成的通知）開出外部回合時，畫面上最後一個 `❯` 回音是上一則
    /// 使用者 prompt——不能存成這一回合的 user 訊息。回合照開（回覆要有地方掛）。
    #[tokio::test]
    async fn a_turn_the_cli_starts_by_itself_does_not_borrow_the_previous_prompt() {
        let (env, run, bot_id) = with_transcript(SAMPLE).await;
        crate::lifecycle::begin_external_turn(&env.app, &run).await;

        assert_eq!(external_turns(&env, &bot_id).await, vec![("external".to_string(), "in_flight".to_string())], "回合照開");
        assert_eq!(user_messages(&env, &bot_id).await, Vec::<String>::new(), "不是使用者起頭的：沒有 user 訊息，不借上一則的 prompt");
    }

    /// 對照組：使用者直接在 pane 裡打字（transcript 最新的起點是 `human`）——回音照舊存成 user 訊息。
    #[tokio::test]
    async fn a_prompt_typed_in_the_pane_keeps_its_echo() {
        let typed = r#"{"type":"user","message":{"role":"user","content":"順便看一下 lint"},"origin":{"kind":"human"},"promptSource":"typed","turnOrigin":"human","timestamp":"2026-09-19T10:05:00.000Z"}"#;
        let (env, run, bot_id) = with_transcript(&format!("{SAMPLE}{typed}\n")).await;
        env.herdr.set_screen(&format!("pane-{bot_id}"), SCREEN_TYPED_IN_THE_PANE);
        crate::lifecycle::begin_external_turn(&env.app, &run).await;

        assert_eq!(user_messages(&env, &bot_id).await, vec!["順便看一下 lint".to_string()]);
    }

    /// 對照組：讀不到 transcript、或舊版沒有 `origin`——沒有證據，照舊當成使用者打的。
    #[tokio::test]
    async fn without_evidence_the_echo_is_kept_as_before() {
        let (env, run, bot_id) = with_transcript("").await;
        crate::lifecycle::begin_external_turn(&env.app, &run).await;
        assert_eq!(user_messages(&env, &bot_id).await.len(), 1, "沒有 origin：照舊");
        drop(env);

        let (env, mut run, bot_id) = with_transcript(SAMPLE).await;
        run.transcript_path = None;
        sqlx::query("UPDATE runs SET transcript_path=NULL WHERE id=?").bind(&run.id).execute(env.app.db()).await.unwrap();
        crate::lifecycle::begin_external_turn(&env.app, &run).await;
        assert_eq!(user_messages(&env, &bot_id).await.len(), 1, "run 沒有 transcript 路徑：照舊");
    }

    /// 2026-10-01 wits-ops-web（遠端，transcript 讀不到）：claude 自己接著做的一輪，畫面上最後一則回音是上一則已經回答過的
    /// prompt。沒有 transcript 可判斷時，回音跟上一則一樣、而且那一回合已收掉，就不再存；真的打了新的一句照存。
    #[tokio::test]
    async fn without_a_transcript_the_previous_answered_prompt_is_not_stored_again() {
        let (env, mut run, bot_id) = with_transcript("").await;
        run.transcript_path = None;
        sqlx::query("UPDATE runs SET transcript_path=NULL WHERE id=?").bind(&run.id).execute(env.app.db()).await.unwrap();
        let conv = db::conversation_id(env.app.db(), &bot_id).await.unwrap();
        let answered = db::ulid();
        sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at) VALUES (?,?,?,'web','in_flight','ok',?)")
            .bind(&answered).bind(&conv).bind(&run.id).bind(db::now()).execute(env.app.db()).await.unwrap();
        crate::lifecycle::insert_message(&env.app, &conv, Some(&answered), "user", "用 Bash 工具的 run_in_background 參數跑一個背景指令：sleep 20; echo done", "web", false, None).await.unwrap();
        sqlx::query("UPDATE turns SET status='completed' WHERE id=?").bind(&answered).execute(env.app.db()).await.unwrap();

        crate::lifecycle::begin_external_turn(&env.app, &run).await;
        assert_eq!(user_messages(&env, &bot_id).await.len(), 1, "畫面上的回音是上一則：不再存一次");

        sqlx::query("UPDATE turns SET status='completed' WHERE origin='external'").execute(env.app.db()).await.unwrap();
        env.herdr.set_screen(&format!("pane-{bot_id}"), SCREEN_TYPED_IN_THE_PANE);
        crate::lifecycle::begin_external_turn(&env.app, &run).await;
        assert_eq!(user_messages(&env, &bot_id).await.last().map(String::as_str), Some("順便看一下 lint"));
    }

    /// A remote run can record an absolute path that happens to exist locally; that local
    /// collision must not be used to classify the run as a CLI-started notification.
    #[tokio::test]
    async fn a_remote_transcript_path_does_not_control_prompt_classification() {
        let (env, run, bot_id) = with_transcript(SAMPLE).await;
        sqlx::query("UPDATE projects SET host='remote-test' WHERE id=?")
            .bind(&env.project_id)
            .execute(env.app.db())
            .await
            .unwrap();
        let bot = db::bot(env.app.db(), &bot_id).await.unwrap().unwrap();

        assert!(!started_by_the_cli_itself(&env.app, &bot, run.transcript_path.as_deref()).await,
            "遠端 transcript 的本機同路徑檔案不能控制 prompt 分類");
    }

    /// 只有 claude 的 transcript 讀得懂：codex／grok 的 run 就算有路徑也照舊。
    #[tokio::test]
    async fn only_claude_transcripts_are_read() {
        let (env, run, bot_id) = with_transcript(SAMPLE).await;
        let bot = db::bot(env.app.db(), &bot_id).await.unwrap().unwrap();
        assert!(started_by_the_cli_itself(&env.app, &bot, run.transcript_path.as_deref()).await);
        let mut codex = bot.clone();
        codex.kind = "codex".into();
        assert!(!started_by_the_cli_itself(&env.app, &codex, run.transcript_path.as_deref()).await);
        let mut grok = bot.clone();
        grok.kind = "grok".into();
        assert!(!started_by_the_cli_itself(&env.app, &grok, run.transcript_path.as_deref()).await);
        assert!(!started_by_the_cli_itself(&env.app, &bot, None).await);
        assert!(!started_by_the_cli_itself(&env.app, &bot, Some("/nonexistent/session.jsonl")).await);
    }
}
