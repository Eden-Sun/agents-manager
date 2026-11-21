#![allow(unused_imports)]
//! #1862 L2-1：從 am-lifecycle 搬來、需要真 daemon App／DB 的測試。

mod tests {
    use crate::lifecycle_tests::prelude::*;
    use crate::lifecycle::delivery::test_seams::*;
    use crate::lifecycle::*;
    use crate::state::App;
    use std::time::Duration;
    #[tokio::test]
    async fn agy_trust_dialog_is_unready_and_composer_ready_yields_dialog_reason() {
        use crate::testing as tt;
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "agy-trust-test").await;
        let run_id = tt::fake_run(&app, &bot.id).await;
        let run = db::run(&app.db, &run_id).await.unwrap().unwrap();

        const PLAIN: &str = "\
Accessing workspace:\n/home/user/project\n\n\
Do you trust the contents of this project?\n\n\
> Yes, I trust this folder\n\
  No, exit\n";

        const ANSI: &str = "\
Accessing workspace:\n/home/user/project\n\n\
\x1b[1mDo you trust the contents of this project?\x1b[0m\n\n\
\x1b[36m> Yes, I trust this folder\x1b[0m\n\
\x1b[2m  No, exit\x1b[0m\n";

        const RULE: &str = "────────────────────────────────────────";
        let empty = format!(" Antigravity CLI 1.2.16\n Gemini 3.1 Pro\n\n{RULE}\n>\n{RULE}\n");
        let normal = format!(" Antigravity CLI 1.2.16\n Gemini 3.1 Pro\n\n{RULE}\n> hello\n{RULE}\n");

        assert_eq!(crate::lifecycle::delivery::box_state("agy", PLAIN), crate::lifecycle::delivery::BoxState::Unready);
        assert_eq!(crate::lifecycle::delivery::box_state_within("agy", PLAIN, 24), crate::lifecycle::delivery::BoxState::Unready);
        assert_eq!(crate::lifecycle::delivery::box_state("agy", ANSI), crate::lifecycle::delivery::BoxState::Unready);
        assert_eq!(crate::lifecycle::delivery::box_state_within("agy", ANSI, 24), crate::lifecycle::delivery::BoxState::Unready);

        assert_eq!(crate::lifecycle::delivery::box_state("agy", &normal), crate::lifecycle::delivery::BoxState::NonEmpty);
        assert_eq!(crate::lifecycle::delivery::box_state("agy", &empty), crate::lifecycle::delivery::BoxState::Empty);

        assert_eq!(
            crate::lifecycle::delivery::test_seams::composer_ready(&app, &run, "agy", PLAIN).await,
            Err(crate::lifecycle::delivery::Delivered::NotAttempted { reason: "dialog_open", retry: true }),
        );
        assert_eq!(
            crate::lifecycle::delivery::test_seams::composer_ready(&app, &run, "agy", ANSI).await,
            Err(crate::lifecycle::delivery::Delivered::NotAttempted { reason: "dialog_open", retry: true }),
        );
        assert_eq!(
            crate::lifecycle::delivery::test_seams::composer_ready(&app, &run, "agy", &normal).await,
            Err(crate::lifecycle::delivery::Delivered::NotAttempted { reason: "composer_busy", retry: true }),
        );
        assert_eq!(
            crate::lifecycle::delivery::test_seams::composer_ready(&app, &run, "agy", &empty).await,
            Ok(()),
        );
    }
    /// 要不要連真的 herdr：只有 `AM_TEST_REAL_HERDR=1` 才連。pane 裡本來就有 `HERDR_*`，不能當成同意（#1147）。
    fn real_herdr_target(opt_in: Option<&str>, sock: Option<String>, pane: Option<String>) -> Option<(String, String)> {
        if opt_in != Some("1") {
            return None;
        }
        Some((sock?, pane?))
    }
    /// 本機有 herdr socket 與 pane、而且明講 `AM_TEST_REAL_HERDR=1` 時，直接丟給真 herdr 的 RPC
    /// （CLI 的 clap 連字號也收，所以只有 socket 能證明）。手動驗證：
    /// `AM_TEST_REAL_HERDR=1 cargo test -p agents-managerd -- the_scan_source_is_one_real_herdr_accepts`。
    #[test]
    fn the_scan_source_is_one_real_herdr_accepts() {
        let Some((sock, pane)) = real_herdr_target(
            std::env::var("AM_TEST_REAL_HERDR").ok().as_deref(),
            std::env::var("HERDR_SOCKET_PATH").ok(),
            std::env::var("HERDR_PANE_ID").ok(),
        ) else {
            return;
        };
        use std::io::{BufRead, Write};
        let mut stream = std::os::unix::net::UnixStream::connect(&sock).expect("AM_TEST_REAL_HERDR=1 但連不上 HERDR_SOCKET_PATH");
        let limit = Some(std::time::Duration::from_secs(10));
        stream.set_read_timeout(limit).unwrap();
        stream.set_write_timeout(limit).unwrap();
        let req = json!({"id": "scan-source", "method": "pane.read",
            "params": {"pane_id": pane, "source": SCAN_SOURCE, "lines": 1, "format": "ansi"}});
        stream.write_all(format!("{req}\n").as_bytes()).unwrap();
        let mut line = String::new();
        std::io::BufReader::new(stream).read_line(&mut line).unwrap();
        let v: Value = serde_json::from_str(&line).unwrap();
        assert!(v.get("error").is_none(), "real herdr rejected source {SCAN_SOURCE}: {v}");
        assert!(v.get("result").is_some(), "{v}");
    }
    use crate::lifecycle::delivery::*;
    const RULE: &str = "─────────────────────────────────────────────";
    /// #1915：codex 把長段貼上摺成 `[Pasted Content N chars]`，框裡看不到原文。字還在框裡，要算「還在」，補按才會發生。
    /// 標籤是字元數：`codex_paste_text()` 含中文，1752 字、3552 位元組，寫成位元組數就對不上。
    #[test]
    fn a_codex_fold_of_our_text_still_counts_as_in_the_composer() {
        use crate::lifecycle::poller::folded_paste_tests::{codex_paste_text, CODEX_FOLDED_PASTE_SCREEN};
        assert!(composer_still_holds("codex", CODEX_FOLDED_PASTE_SCREEN, &codex_paste_text()), "多位元組，字元數");
        let as_bytes = CODEX_FOLDED_PASTE_SCREEN.replace("1752", "3552");
        assert!(!composer_still_holds("codex", &as_bytes, &codex_paste_text()), "3552 是位元組數，不是 codex 的數字");
        let other = CODEX_FOLDED_PASTE_SCREEN.replace("1752", "999");
        assert!(!composer_still_holds("codex", &other, &codex_paste_text()), "別人的摺疊、字數不同");
    }
    /// #1984 A：late proof 的「字還在框裡」也認得摺起來的我們那一則（與補按共用同一個 helper）。
    #[test]
    fn a_codex_fold_of_our_text_counts_as_still_in_the_box_for_the_late_proof_too() {
        use crate::lifecycle::poller::folded_paste_tests::{codex_paste_text, CODEX_FOLDED_PASTE_SCREEN};
        assert!(text_still_in_box("codex", CODEX_FOLDED_PASTE_SCREEN, &codex_paste_text()));
        let other = CODEX_FOLDED_PASTE_SCREEN.replace("1752", "999");
        assert!(!text_still_in_box("codex", &other, &codex_paste_text()), "別人的摺疊、字數不同");
    }
    /// #1915 ①：補按的上限照舊（`pressed_again` 只有一次）。補按之後標籤還在也不會再按，之後交給 unsent_draft。
    #[test]
    fn a_codex_fold_that_stays_after_the_enter_gets_exactly_one_more() {
        use crate::lifecycle::poller::folded_paste_tests::{codex_paste_text, CODEX_FOLDED_PASTE_SCREEN};
        let text = codex_paste_text();
        let mut pressed_again = false;
        let mut presses = 0;
        for _ in 0..SUBMIT_CHECKS {
            if may_press_retry_submit("codex", CODEX_FOLDED_PASTE_SCREEN, &text, false, pressed_again) {
                presses += 1;
                pressed_again = true;
            }
        }
        assert_eq!(presses, 1, "標籤一直在，也只補按一次");
    }
    fn user_entry(content: Value) -> String {
        json!({"type": "user", "message": {"role": "user", "content": content}}).to_string()
    }
    /// transcript 只看基準點之後新增的位元組；逐字比對，截半的 UTF-8／JSON 行不會誤判。
    #[test]
    fn the_transcript_is_read_from_the_baseline_offset_and_compared_byte_for_byte() {
        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-transcript-{}", db::ulid())));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.jsonl");
        let code = "修這段：\n\nfn main() {\n    println!(\"hi\");  \n}";
        std::fs::write(&path, format!("{}\n", user_entry(json!(code)))).unwrap();
        let offset = transcript_len(&path).unwrap();
        assert_eq!(transcript_hits_since(&path, offset, code).unwrap(), 0);
        let long: String = (0..2000).map(|i| format!("  line {i}\n")).collect();
        let appended = [
            user_entry(json!("修這段：\nfn main() {\nprintln!(\"hi\");\n}")),
            user_entry(json!([{"type": "tool_result", "content": code}])),
            json!({"type": "user", "isMeta": true, "message": {"content": code}}).to_string(),
            user_entry(json!(code)),
            user_entry(json!([{"type": "text", "text": long.clone()}])),
        ];
        let mut f = std::fs::OpenOptions::new().append(true).open(&path).unwrap();
        use std::io::Write as _;
        writeln!(f, "{}", appended.join("\n")).unwrap();
        assert_eq!(transcript_hits_since(&path, offset, code).unwrap(), 1);
        assert_eq!(transcript_hits_since(&path, offset, &code.replace("  \n", "\n")).unwrap(), 0, "行尾兩空白是內容");
        assert_eq!(transcript_hits_since(&path, offset, &long).unwrap(), 1, "2k 行");
        assert!(transcript_hits_since(&path, offset + 1, code).unwrap() <= 1);
        std::fs::write(&path, "").unwrap();
        assert!(transcript_hits_since(&path, offset, code).is_err());
        assert!(transcript_hits_since(&dir.join("missing.jsonl"), 0, code).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }
    /// #910：送達驗證數的是人打的行。compact 摘要、slash 指令回音不算——有它們在檔裡，真正送出的那句的計數不變。
    #[test]
    fn compact_summaries_and_command_echoes_do_not_change_the_delivery_count() {
        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-transcript-{}", db::ulid())));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.jsonl");
        std::fs::write(&path, "").unwrap();
        let summary = json!({"type": "user", "isCompactSummary": true, "isVisibleInTranscriptOnly": true,
            "message": {"role": "user", "content": "This session is being continued from a previous conversation. Summary: ..."}})
        .to_string();
        let echo = user_entry(json!("<command-name>/compact</command-name>\n<command-args></command-args>"));
        std::fs::write(&path, [summary.clone(), echo.clone(), user_entry(json!("送出的那句")), summary.clone()].join("\n") + "\n").unwrap();
        assert_eq!(transcript_hits_since(&path, 0, "送出的那句").unwrap(), 1);
        let same_text = json!({"type": "user", "isCompactSummary": true, "message": {"role": "user", "content": "送出的那句"}}).to_string();
        std::fs::write(&path, [same_text, user_entry(json!("送出的那句"))].join("\n") + "\n").unwrap();
        assert_eq!(transcript_hits_since(&path, 0, "送出的那句").unwrap(), 1);
        assert_eq!(transcript_user_text(&summary), None);
        assert_eq!(transcript_user_text(&echo), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
    /// issue #1771：claude 2.1.300 的請求紀錄（`api-request*`，blob 也帶 `message.content`）夾在中間，送達計數不變。
    #[test]
    fn api_request_records_do_not_change_the_delivery_count() {
        let log = include_str!("../../../crates/am-lifecycle/src/lifecycle/fixtures/claude_2.1.300_api_request.jsonl");
        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-transcript-{}", db::ulid())));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.jsonl");
        let blob = json!({"type": "api-request-blob", "hash": "h", "message": {"role": "user", "content": [{"type": "text", "text": "送出的那句"}]}}).to_string();
        std::fs::write(&path, [log.trim_end(), &user_entry(json!("送出的那句")), log.trim_end(), &blob].join("\n") + "\n").unwrap();
        assert_eq!(transcript_hits_since(&path, 0, "送出的那句").unwrap(), 1);
        assert!(log.lines().all(|l| transcript_user_text(l).is_none()));
        let _ = std::fs::remove_dir_all(&dir);
    }
    /// #218：claude 2.1.278 把**貼上事件**（herdr `agent.prompt`、人在終端貼上）包成 `<pasted_content id=…>` 才寫進
    /// transcript。真 transcript（2026-09-19 實測）：包起來的、20 字門檻下沒包的、字面標籤被 CLI 跳脫的，都認得是同一則；
    /// 只是被包含的一段不算。
    #[test]
    fn a_prompt_the_cli_wrapped_as_pasted_content_is_still_our_prompt() {
        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-transcript-{}", db::ulid())));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.jsonl");
        std::fs::write(&path, include_str!("../../../crates/am-lifecycle/src/lifecycle/fixtures/claude_2.1.278_pasted_content.jsonl")).unwrap();
        let hits = |text: &str| transcript_hits_since(&path, 0, text).unwrap();
        assert_eq!(hits("請只回覆 OK 兩個字母，不要多說任何其他的話，也不要使用任何工具。"), 1, "整段包起來");
        assert_eq!(hits("請只回覆 OK 兩個字母不要多說其他話"), 1, "19 字：CLI 沒包，照舊逐字");
        assert_eq!(hits("請只回覆 OK 兩個字母，不要多說其他話"), 1, "20 字：包起來");
        assert_eq!(hits("第一行：這是多行貼上測試。\n第二行：請不要使用任何工具。\n第三行：還是一樣。\n第四行：只回覆 OK 兩個字母。"), 1, "多行");
        assert_eq!(
            hits("這段文字裡有字面的 <pasted_content id=\"1234\">x</pasted_content id=\"1234\"> 標籤，請只回覆 OK 兩個字母。"),
            1,
            "字面標籤被 CLI 跳脫成 <\\"
        );
        assert_eq!(hits("請只回覆 OK 兩個字母"), 0, "只是被包含的一段不算");
        let _ = std::fs::remove_dir_all(&dir);
    }
    /// 只有錯誤訊息明確提到 format 才降級成純文字讀法。
    #[test]
    fn only_an_error_about_format_downgrades_the_read() {
        let e = |code: &str, msg: &str| anyhow::Error::new(HerdrError { code: code.into(), message: msg.into() });
        assert!(ansi_unsupported(&e("invalid_params", "unknown field `format`")));
        assert!(!ansi_unsupported(&e("invalid_params", "lines must be positive")));
        assert!(!ansi_unsupported(&e("unsupported", "pane is gone")));
    }
}

mod api_tests {
    use crate::lifecycle_tests::prelude::*;
    use crate::lifecycle::delivery::test_seams::*;
    use crate::lifecycle::*;
    use crate::state::App;
    use std::time::Duration;
    use crate::lifecycle::delivery::*;
    use crate::testing as tt;
    async fn idle_bot(env: &tt::Env, kind: &str) -> (String, String, String) {
        let app = &env.app;
        let bot_id = db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, created_at)
             VALUES (?,?,'api-bot',?,'[]',0,1,'tok',?)",
        )
        .bind(&bot_id)
        .bind(&env.project_id)
        .bind(kind)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let conv = db::conversation_id(&app.db, &bot_id).await.unwrap();
        let run_id = db::ulid();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, agent_name, herdr_session, pane_typed, started_at)
             VALUES (?,?,'running','idle','ws-1','pane-api','api-bot','test',1,?)",
        )
        .bind(&run_id)
        .bind(&bot_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        (bot_id, conv, run_id)
    }
    /// A local Claude run with a legacy untrusted DB path must wait for its own transcript rather
    /// than treating an arbitrary same-host JSONL file as delivery evidence.
    #[tokio::test]
    async fn delivery_does_not_select_an_out_of_root_transcript_as_proof() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot_id, _, run_id) = idle_bot(&env, "claude").await;
        let bot = db::bot(&app.db, &bot_id).await.unwrap().unwrap();
        let scratch = tt::track(std::env::temp_dir().join(format!("am-delivery-path-{}", db::ulid())));
        std::fs::create_dir_all(&scratch).unwrap();
        let outside = scratch.join("foreign.jsonl");
        std::fs::write(&outside, "{}\n").unwrap();
        sqlx::query("UPDATE runs SET native_session_id='session-guard', transcript_path=? WHERE id=?")
            .bind(outside.to_string_lossy())
            .bind(&run_id)
            .execute(&app.db)
            .await
            .unwrap();
        let run = db::run(&app.db, &run_id).await.unwrap().unwrap();
        env.herdr.live_pane("pane-api", tt::LivePane { width: Some(120), ..Default::default() });
        let client = crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"));

        let plan = plan_delivery(&app, &client, &run, &bot, "deliver this", true, false).await.unwrap();
        assert!(matches!(plan, Ok(Plan::Type { proof: Proof::EchoRow | Proof::Unverified, .. })
            | Err(Delivered::NotAttempted { reason: "transcript_not_ready", retry: true })),
            "out-of-root file cannot become a transcript proof: {plan:?}");
    }
    async fn turns(app: &Arc<App>, conv: &str) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM turns WHERE conversation_id = ?").bind(conv).fetch_one(&app.db).await.unwrap()
    }
    /// 框裡有字：回可重試的 409，而且**沒有**建立 turn；框清空後同一個 request id 再送就成功（第七輪 #2）。
    #[tokio::test]
    async fn a_busy_box_is_a_retryable_409_with_no_turn_and_the_retry_goes_through() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot_id, conv, _run) = idle_bot(&env, "claude").await;
        env.herdr.live_pane("pane-api", crate::testing::LivePane { composer: vec!["草稿".into()], width: Some(120), ..Default::default() });

        match prompt(&app, &bot_id, "Reply with PONG please", "crid-1").await {
            Err(LcError::Conflict(v)) => {
                assert_eq!(v.get("reason").and_then(Value::as_str), Some("composer_busy"));
                assert_eq!(v.get("retryable").and_then(Value::as_bool), Some(true));
            }
            other => panic!("expected a retryable 409, got {:?}", other.map(|o| o.delivery)),
        }
        assert_eq!(turns(&app, &conv).await, 0, "沒送出就沒有 in-flight unknown turn");

        env.herdr.live_pane("pane-api", crate::testing::LivePane { width: Some(120), ..Default::default() });
        let out = prompt(&app, &bot_id, "Reply with PONG please", "crid-1").await.unwrap();
        assert_eq!(out.delivery, "ok");
        assert_eq!(turns(&app, &conv).await, 1);
    }
    /// Codex 0.157 puts unanswered question answers back in the composer when a turn ends.
    /// If one appears after delivery preparation, the prompt must not be appended to it.
    #[tokio::test]
    async fn a_codex_answer_restored_after_preparation_is_left_in_the_composer() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot_id, conv, _run) = idle_bot(&env, "codex").await;
        env.herdr.live_pane("pane-api", crate::testing::LivePane { codex: true, width: Some(120), ..Default::default() });
        let live = env.herdr.live.clone();
        crate::lifecycle::race_point::arm("delivery_before_typing", &bot_id, move || async move {
            live.lock().unwrap().get_mut("pane-api").unwrap().composer = vec!["restored answer from Codex 0.157".into()];
        });

        match prompt(&app, &bot_id, "first line\nsecond line", "crid-codex-restored-answer").await {
            Err(LcError::Conflict(body)) => {
                assert_eq!(body["reason"], "composer_busy", "{body}");
                assert_eq!(body["retryable"], true, "{body}");
            }
            other => panic!("expected a retryable composer_busy, got {other:?}"),
        }
        assert_eq!(turns(&app, &conv).await, 0, "unattempted delivery withdraws its turn");
        assert_eq!(env.herdr.calls_to("pane.send_text").len(), 0, "no text is appended to Codex's restored answer");
        assert_eq!(env.herdr.pane("pane-api").unwrap().composer, ["restored answer from Codex 0.157"]);
    }
    /// codex 已知 session 但 rollout 還沒寫：直接送的 prompt 回可重試的 409，不建 turn（sol 第九輪 #1）。
    #[tokio::test]
    async fn a_codex_prompt_before_its_rollout_exists_is_a_retryable_409() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot_id, conv, run_id) = idle_bot(&env, "codex").await;
        let home = env.dir.join("codex-home-api");
        std::fs::create_dir_all(home.join("sessions")).unwrap();
        sqlx::query("UPDATE bots SET env_json = ? WHERE id = ?").bind(json!({"CODEX_HOME": home.to_str().unwrap()}).to_string()).bind(&bot_id).execute(&app.db).await.unwrap();
        sqlx::query("UPDATE runs SET native_session_id = 'sess-api' WHERE id = ?").bind(&run_id).execute(&app.db).await.unwrap();
        env.herdr.live_pane("pane-api", crate::testing::LivePane { width: Some(120), codex: true, ..Default::default() });
        match prompt(&app, &bot_id, "第一行\n第二行", "crid-codex").await {
            Err(LcError::Conflict(v)) => assert_eq!(v.get("reason").and_then(Value::as_str), Some("codex_log_not_ready")),
            other => panic!("expected 409, got {:?}", other.map(|o| o.delivery)),
        }
        assert_eq!(turns(&app, &conv).await, 0);
        assert_eq!(env.herdr.methods().iter().filter(|m| m.starts_with("pane.send")).count(), 0);
    }
    /// 直接送：herdr 拒絕 `format: ansi` 時退回純文字讀法照樣送出；讀不到畫面時回 409、不建 turn，不是 502。
    #[tokio::test]
    async fn direct_prompts_survive_a_herdr_without_styled_reads_and_unreadable_panes() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot_id, conv, _run) = idle_bot(&env, "claude").await;
        env.herdr.reject_ansi.store(true, std::sync::atomic::Ordering::SeqCst);
        env.herdr.live_pane("pane-api", crate::testing::LivePane { width: Some(120), ..Default::default() });
        let out = prompt(&app, &bot_id, "Reply with PONG please", "crid-plain").await.unwrap();
        assert_eq!(out.delivery, "ok");

        let env = tt::env().await;
        let app = env.app.clone();
        let (bot_id, conv2, _run) = idle_bot(&env, "grok").await;
        env.herdr.set_screen("pane-api", "__READ_ERROR__");
        match prompt(&app, &bot_id, "Reply with PONG please", "crid-unreadable").await {
            Err(LcError::Conflict(v)) => assert_eq!(v.get("reason").and_then(Value::as_str), Some("composer_unreadable")),
            other => panic!("expected a retryable 409, got {:?}", other.map(|o| o.delivery)),
        }
        assert_eq!(turns(&app, &conv2).await, 0);
        let _ = conv;
    }
    /// 證據檔在打字前讀不到（這裡是權限被拿掉）：直接送撤回剛建的 turn、回 409，pane 零寫入（sol 第十一輪）。
    #[cfg(unix)]
    #[tokio::test]
    async fn an_unreadable_evidence_file_withdraws_the_direct_turn() {
        use std::os::unix::fs::PermissionsExt;
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot_id, conv, run_id) = idle_bot(&env, "claude").await;
        let config_dir = env.dir.join("claude-config");
        sqlx::query("UPDATE bots SET env_json=? WHERE id=?")
            .bind(json!({"CLAUDE_CONFIG_DIR": config_dir.to_string_lossy()}).to_string())
            .bind(&bot_id)
            .execute(&app.db)
            .await
            .unwrap();
        let transcript_dir = config_dir.join("projects/-test");
        std::fs::create_dir_all(&transcript_dir).unwrap();
        let t = transcript_dir.join("locked.jsonl");
        std::fs::write(&t, "").unwrap();
        sqlx::query("UPDATE runs SET native_session_id = 's-locked', transcript_path = ? WHERE id = ?")
            .bind(t.to_str().unwrap())
            .bind(&run_id)
            .execute(&app.db)
            .await
            .unwrap();
        env.herdr.live_pane("pane-api", crate::testing::LivePane { width: Some(120), ..Default::default() });
        std::fs::set_permissions(&t, std::fs::Permissions::from_mode(0o000)).unwrap();
        let res = prompt(&app, &bot_id, "第一行\n第二行", "crid-locked").await;
        std::fs::set_permissions(&t, std::fs::Permissions::from_mode(0o644)).unwrap();
        match res {
            Err(LcError::Conflict(v)) => assert_eq!(v.get("reason").and_then(Value::as_str), Some("transcript_unreadable")),
            other => panic!("expected a retryable 409, got {:?}", other.map(|o| o.delivery)),
        }
        assert_eq!(turns(&app, &conv).await, 0, "the turn was withdrawn, not left unknown");
        assert_eq!(env.herdr.methods().iter().filter(|m| m.starts_with("pane.send")).count(), 0);
    }
    #[tokio::test]
    async fn a_grok_multi_line_prompt_is_sent_and_marked_unverified() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot_id, conv, _run) = idle_bot(&env, "grok").await;
        env.herdr.live_pane("pane-api", crate::testing::LivePane { width: Some(120), boxed: true, ..Default::default() });

        let out = prompt(&app, &bot_id, "第一行\n第二行", "crid-2").await.unwrap();
        assert_eq!(out.delivery, "unverified");
        assert_eq!(turns(&app, &conv).await, 1);
        let (delivery, verified): (String, i64) = sqlx::query_as("SELECT delivery, delivery_verified FROM turns WHERE id = ?")
            .bind(&out.turn_id)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!((delivery.as_str(), verified), ("ok", 0), "不是 unknown，而是帶標記的已送出");
        assert_eq!(env.herdr.methods().iter().filter(|m| *m == "pane.send_text").count(), 1);
        assert_eq!(prompt(&app, &bot_id, "第一行\n第二行", "crid-2").await.unwrap().delivery, "unverified");
    }

    /// 一顆 codex bot：session `sess-api` 的 rollout 存在但永遠不長（mock pane 不寫 codex rollout＝「rollout 還沒 flush」）。
    async fn codex_with_idle_rollout(env: &tt::Env) -> (String, String, crate::codex_history::Binding) {
        let (bot_id, conv, run_id) = idle_bot(env, "codex").await;
        let home = env.dir.join(format!("codex-home-{}", db::ulid()));
        let day = home.join("sessions/2026/10/01");
        std::fs::create_dir_all(&day).unwrap();
        std::fs::write(day.join("rollout-2026-10-01T00-00-00-sess-api.jsonl"), "").unwrap();
        sqlx::query("UPDATE bots SET env_json = ? WHERE id = ?").bind(json!({"CODEX_HOME": home.to_str().unwrap()}).to_string()).bind(&bot_id).execute(&env.app.db).await.unwrap();
        sqlx::query("UPDATE runs SET native_session_id = 'sess-api' WHERE id = ?").bind(&run_id).execute(&env.app.db).await.unwrap();
        env.herdr.live_pane("pane-api", crate::testing::LivePane { codex: true, width: Some(120), ..Default::default() });
        let binding = crate::codex_history::Binding { host: crate::config::LOCAL_HOST.into(), codex_home: Some(home), thread_id: "sess-api".into() };
        (bot_id, conv, binding)
    }
    async fn delivery_of(app: &Arc<App>, turn_id: &str) -> (String, i64) {
        sqlx::query_as("SELECT delivery, delivery_verified FROM turns WHERE id = ?").bind(turn_id).fetch_one(&app.db).await.unwrap()
    }
    /// 多行 prompt、rollout 還沒 flush：thread 歷史裡出現我們那則 → 判 Submitted（verified），不靠終端回音。
    #[tokio::test]
    async fn a_prompt_the_rollout_cannot_prove_yet_is_proven_by_the_thread_history() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot_id, _conv, b) = codex_with_idle_rollout(&env).await;
        let stub = crate::codex_history::StubSource::new();
        stub.push_user(&b, "t0", "u0", "old prompt");
        stub.push_final(&b, "t0", "a0", "old reply");
        let prompt_text = "第一行\n第二行\n\n  indented { \"json\": true }";
        stub.arrive_on_open(2, &b, "t1", "u1", prompt_text);
        app.codex_history.set(Some(Arc::new(stub.clone())));
        let before = crate::codex_history::counts();

        let out = prompt(&app, &bot_id, prompt_text, "crid-hist-hit").await.unwrap();
        assert_eq!(delivery_of(&app, &out.turn_id).await, ("ok".to_string(), 1), "結構化歷史是 positive proof");
        assert_eq!(env.herdr.calls_to("pane.send_text").len(), 1, "只送一次");
        assert!(crate::codex_history::counts().structured_hit > before.structured_hit);
    }
    /// 歷史讀不到（RPC error）：行為跟沒有這個模組一模一樣——證不出來就是 unknown，**不**重送、不收掉。
    #[tokio::test]
    async fn an_rpc_error_in_the_history_is_the_old_path_not_a_verdict() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot_id, conv, b) = codex_with_idle_rollout(&env).await;
        let stub = crate::codex_history::StubSource::new();
        stub.push_user(&b, "t0", "u0", "old prompt");
        stub.fail(true);
        app.codex_history.set(Some(Arc::new(stub.clone())));
        let with_error = prompt(&app, &bot_id, "第一行\n第二行", "crid-hist-err").await.unwrap();
        let with_error = (delivery_of(&app, &with_error.turn_id).await, env.herdr.calls_to("pane.send_text").len());

        let env2 = tt::env().await;
        let (bot2, conv2, _) = codex_with_idle_rollout(&env2).await;
        let baseline = prompt(&env2.app, &bot2, "第一行\n第二行", "crid-hist-off").await.unwrap();
        let baseline = (delivery_of(&env2.app, &baseline.turn_id).await, env2.herdr.calls_to("pane.send_text").len());

        assert_eq!(with_error, baseline, "RPC error 與關掉結構化路徑結果相同");
        assert_eq!(with_error.1, 1, "沒有因為歷史讀不到而重送");
        assert_eq!(turns(&app, &conv).await, 1);
        assert_eq!(turns(&env2.app, &conv2).await, 1);
    }
    /// 投影停住（歷史一直回空／沒有基準）：沒有 positive evidence，一樣只走舊路，也不會被後來才吐出的舊歷史騙到。
    #[tokio::test]
    async fn a_stalled_or_empty_history_proves_nothing() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot_id, _conv, b) = codex_with_idle_rollout(&env).await;
        let stub = crate::codex_history::StubSource::new();
        stub.push_user(&b, "t0", "u0", "第一行\n第二行");
        stub.stall(true);
        app.codex_history.set(Some(Arc::new(stub.clone())));
        let out = prompt(&app, &bot_id, "第一行\n第二行", "crid-hist-stall").await.unwrap();
        assert_eq!(delivery_of(&app, &out.turn_id).await.1, 0, "舊的同句歷史不能當成這一次的證據");
        assert_eq!(env.herdr.calls_to("pane.send_text").len(), 1);
    }
    /// 基準之前就有一模一樣的 prompt：不算；只有基準之後新出現的才算。
    #[tokio::test]
    async fn an_identical_older_prompt_is_not_new_evidence() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot_id, _conv, b) = codex_with_idle_rollout(&env).await;
        let stub = crate::codex_history::StubSource::new();
        stub.push_user(&b, "t0", "u0", "第一行\n第二行");
        stub.push_final(&b, "t0", "a0", "ok");
        app.codex_history.set(Some(Arc::new(stub.clone())));
        let out = prompt(&app, &bot_id, "第一行\n第二行", "crid-hist-old").await.unwrap();
        assert_eq!(delivery_of(&app, &out.turn_id).await.1, 0);
    }
    /// 另一顆 bot（別的 thread）出現同一句話，不會算成這顆的證據。
    #[tokio::test]
    async fn another_threads_identical_prompt_does_not_count() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (bot_id, _conv, b) = codex_with_idle_rollout(&env).await;
        let stub = crate::codex_history::StubSource::new();
        stub.push_user(&b, "t0", "u0", "old");
        let other = crate::codex_history::Binding { thread_id: "sess-other".into(), ..b.clone() };
        stub.arrive_on_open(2, &other, "t1", "u1", "第一行\n第二行");
        let other_home = crate::codex_history::Binding { codex_home: Some(std::path::PathBuf::from("/elsewhere")), ..b.clone() };
        stub.arrive_on_open(2, &other_home, "t1", "u2", "第一行\n第二行");
        let other_host = crate::codex_history::Binding { host: "mac_builder".into(), ..b.clone() };
        stub.arrive_on_open(2, &other_host, "t1", "u3", "第一行\n第二行");
        app.codex_history.set(Some(Arc::new(stub.clone())));
        let out = prompt(&app, &bot_id, "第一行\n第二行", "crid-hist-other").await.unwrap();
        assert_eq!(delivery_of(&app, &out.turn_id).await.1, 0);
    }
    /// 出貨的預設設定（沒寫 `[codex_history]`）：codex 的送達路徑完全不開 app-server（#749 審查：預設關）。
    #[tokio::test]
    async fn the_shipped_default_config_never_opens_the_history() {
        let env = tt::env().await;
        let app = env.app.clone();
        app.cfg.update(|c| { c.codex_history = crate::config::CodexHistoryCfg::default(); Ok(()) }).await.unwrap();
        let (bot_id, _conv, b) = codex_with_idle_rollout(&env).await;
        let stub = crate::codex_history::StubSource::new();
        stub.push_user(&b, "t0", "u0", "old prompt");
        app.codex_history.set(Some(Arc::new(stub.clone())));
        prompt(&app, &bot_id, "第一行\n第二行", "crid-hist-default").await.unwrap();
        assert_eq!(stub.opens.load(std::sync::atomic::Ordering::Relaxed), 0, "預設設定不能起 app-server");
    }
    /// 非 codex 的 bot 不碰 app-server；設定關掉也不碰。
    #[tokio::test]
    async fn only_an_enabled_codex_bot_opens_the_history() {
        let env = tt::env().await;
        let app = env.app.clone();
        let stub = crate::codex_history::StubSource::new();
        app.codex_history.set(Some(Arc::new(stub.clone())));
        let (claude_bot, _conv, _run) = idle_bot(&env, "claude").await;
        env.herdr.live_pane("pane-api", crate::testing::LivePane { width: Some(120), ..Default::default() });
        prompt(&app, &claude_bot, "Reply with PONG please", "crid-hist-claude").await.unwrap();
        assert_eq!(stub.opens.load(std::sync::atomic::Ordering::Relaxed), 0, "claude 不開");

        let env = tt::env().await;
        let app = env.app.clone();
        let (bot_id, _conv, _b) = codex_with_idle_rollout(&env).await;
        let stub = crate::codex_history::StubSource::new();
        app.codex_history.set(Some(Arc::new(stub.clone())));
        app.cfg.update(|c| { c.codex_history.enabled = false; Ok(()) }).await.unwrap();
        prompt(&app, &bot_id, "第一行\n第二行", "crid-hist-disabled").await.unwrap();
        assert_eq!(stub.opens.load(std::sync::atomic::Ordering::Relaxed), 0, "設定關掉就不開");
    }
}
