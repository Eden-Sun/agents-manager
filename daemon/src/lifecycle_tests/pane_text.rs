#![allow(unused_imports)]
//! #1862 L2-1：從 am-lifecycle 搬來、需要真 daemon App／DB 的測試。

mod delivery_tests {
    use crate::lifecycle_tests::prelude::*;
    use crate::lifecycle::pane_text::claude_strips;
    use crate::lifecycle::*;
    use crate::testing as tt;
    struct Fixture {
        env: tt::Env,
        bot_id: String,
        pane: String,
    }
    /// 一顆閒著、會回話的 claude pane：transcript 檔就是送達證據；`strips` 是這顆 CLI 按 Enter 時會清掉的字。
    async fn fixture(strips: fn(char) -> bool) -> Fixture {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "clean-send").await;
        let transcript = env.dir.join(format!("session-{}.jsonl", db::ulid()));
        std::fs::write(&transcript, "").unwrap();
        let pane = format!("pane-{}", bot.id);
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, pane_id, herdr_session, native_session_id, transcript_path, started_at)
             VALUES (?,?,'running','idle',?,'test','sess-clean',?,?)",
        )
        .bind(db::ulid())
        .bind(&bot.id)
        .bind(&pane)
        .bind(transcript.to_str().unwrap())
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        env.herdr.live_pane(&pane, tt::LivePane { width: Some(120), transcript_file: Some(transcript), strips_on_enter: Some(strips), ..Default::default() });
        Fixture { env, bot_id: bot.id, pane }
    }
    async fn stored(f: &Fixture, turn_id: &str) -> (String, String) {
        sqlx::query_as("SELECT t.prompt_text, m.content FROM turns t JOIN messages m ON m.turn_id = t.id AND m.role = 'user' WHERE t.id = ?")
            .bind(turn_id)
            .fetch_one(&f.env.app.db)
            .await
            .unwrap()
    }
    /// 從網頁貼來的零寬空白＋從 log 貼來的色碼：打進 pane 的、`prompt_text` 都是清過的字，泡泡是原文，
    /// CLI 一按 Enter 就收下（沒有 review），transcript 裡逐字相同——送達證據成立。
    #[tokio::test]
    async fn a_prompt_with_invisible_characters_and_colour_codes_goes_in_clean_and_is_proven() {
        let f = fixture(claude_strips).await;
        let dirty = "看這段\u{200b}：\u{1b}[31m紅字\u{1b}[0m 完\u{feff}";
        let out = prompt(&f.env.app, &f.bot_id, dirty, "crid-clean").await.unwrap();
        assert_eq!(out.delivery, "ok", "{out:?}");
        assert_eq!(stored(&f, &out.turn_id).await, ("看這段：紅字 完".to_string(), dirty.to_string()), "prompt_text 清過、泡泡原文");
        let p = f.env.herdr.pane(&f.pane).unwrap();
        assert_eq!((p.transcript.as_slice(), p.notice.as_deref()), (&["❯ 看這段：紅字 完".to_string()][..], None), "CLI 沒有要 review");
    }
    /// 保險：CLI 還會清掉一個我們表上沒有的字（這裡假設下一版把 U+2800 盲文空白也當成隱形字元）——畫面出現 review 提示，
    /// 框裡是清過的字。不再按 Enter（再按送出去的就不是記下的那一段），送達記成不明，交給使用者看。
    #[tokio::test]
    async fn a_review_prompt_from_the_cli_is_not_pressed_through() {
        fn future_cli(c: char) -> bool {
            c == '\u{2800}' || claude_strips(c)
        }
        let f = fixture(future_cli).await;
        let out = prompt(&f.env.app, &f.bot_id, "盲文空白\u{2800}在這", "crid-review").await.unwrap();
        assert_ne!(out.delivery, "ok", "{out:?}");
        let p = f.env.herdr.pane(&f.pane).unwrap();
        assert!(p.transcript.is_empty(), "沒有被按出去：{:?}", p.transcript);
        assert_eq!(p.composer, vec!["盲文空白在這".to_string()], "清過的字還在框裡");
        assert!(p.notice.as_deref().is_some_and(|n| n.contains("review and press Enter to send")));
    }
    /// 另外兩條存 `prompt_text` 的路也存清過的字：對方回合中時 AGM 的派工排進佇列（`queue_for_next_turn`，flush 與 stall
    /// 重送都讀它），以及 bot 沒在跑時送出、等它起來（`start_send`）。泡泡照樣是原文。
    #[tokio::test]
    async fn the_queued_and_the_waiting_for_start_paths_store_the_clean_text_too() {
        let f = fixture(claude_strips).await;
        let app = f.env.app.clone();
        let run: String = sqlx::query_scalar("SELECT id FROM runs WHERE bot_id=?").bind(&f.bot_id).fetch_one(&app.db).await.unwrap();
        let conv = db::conversation_id(&app.db, &f.bot_id).await.unwrap();
        sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, created_at) VALUES (?,?,?,'web','in_flight','ok',?)")
            .bind(db::ulid())
            .bind(&conv)
            .bind(&run)
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();
        let dirty = "派工\u{200b}內容\u{1b}[1m";
        let queued = prompt_relayed_queueable(&app, &f.bot_id, dirty, "agm-clean", Some(crate::agent_relay::DAEMON_SENDER), QueueClass::Relay).await.unwrap();
        assert_eq!(queued.delivery, "queued");
        assert_eq!(stored(&f, &queued.turn_id).await, ("派工內容".to_string(), dirty.to_string()));

        let idle = tt::claude_bot(&app, &f.env.project_id, "not-running").await;
        let waiting = crate::lifecycle::start_send::prompt_starting(&app, &idle.id, dirty, "crid-waiting", &[], crate::lifecycle::RelaySrc::default()).await.unwrap();
        assert_eq!(stored(&f, &waiting.turn_id).await, ("派工內容".to_string(), dirty.to_string()));
    }
    /// 整段都是隱形字元：清完什麼都不剩，回 400，不打一個空的 Enter 進去。
    #[tokio::test]
    async fn a_prompt_that_is_all_invisible_is_refused() {
        let f = fixture(claude_strips).await;
        let err = prompt(&f.env.app, &f.bot_id, "\u{200b}\u{feff}", "crid-empty").await.unwrap_err();
        assert!(matches!(err, LcError::Bad(_)), "{err:?}");
        assert!(!f.env.herdr.methods().iter().any(|m| m == "pane.send_text"), "一個字都沒打");
    }
}
