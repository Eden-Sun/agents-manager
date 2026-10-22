//! `pending_question` runner 與 API 處理器。

use crate::db;
use crate::lc_error::LcError;
use crate::pending_question::{pending_ask_entry, read_tail, TAIL_BYTES};
use crate::state::App;
use axum::extract::{Path, State};
use axum::Json;
use serde_json::{json, Value};
use std::sync::Arc;

/// `GET /api/bots/{id}/pending-question`：`200 {"questions": [...] | null, "jev_options": {...} | null}`。
///
/// `jev_options` 是 Jev 對各選項的建議（issue #1475，SPEC §4.3c 第五個場景）：沒開、沒問過、還在問、失敗都是 `null`，
/// 題目照常回；第一次讀到某個提問時丟一個背景 task 去問，答案落帳後發 WS `pending_question_hint`。
///
/// 本機 bot 讀這台的 transcript；遠端主機的 claude bot 經 §12.5b 那條 `sh`／ssh 只讀檔尾（issue #1476，
/// `pending_question_remote`：有逾時、有節流、路徑只認這顆 bot 自己的 config root）。非 claude、沒有 transcript、
/// 讀不到、沒在等題目都回 `null`，不是錯誤——網頁拿不到就照舊用畫面。bot 或 run 不存在 404。
pub async fn get_pending_question(State(app): State<Arc<App>>, Path(id): Path<String>) -> Result<Json<Value>, LcError> {
    let up = |e: anyhow::Error| LcError::Upstream(e.to_string());
    let bot = db::bot(&app.db, &id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("bot".into()))?;
    let run = db::active_run(&app.db, &id).await.map_err(up)?.ok_or_else(|| LcError::NotFound("run".into()))?;
    let null = || Ok(Json(json!({"questions": null, "jev_options": null})));
    if bot.kind == "codex" {
        return codex_async(&app, &bot, &run).await;
    }
    if bot.kind != "claude" {
        return null();
    }
    let host = db::bot_host(&app.db, &id).await.map_err(up)?;
    // `fresh`：這次的檔尾是現讀的。現讀的檔尾已經證明提問還在，`hint_for` 的有效性檢查就不再讀一次；快取的檔尾不算證據。
    let (log, fresh) = if host == crate::config::LOCAL_HOST {
        let Some(path) = run.transcript_path.clone().filter(|p| !p.trim().is_empty()) else { return null() };
        if !crate::app_ports_p5::local_transcript_allowed(&app, &bot, &path).await {
            return null();
        }
        let log = tokio::task::spawn_blocking(move || read_tail(std::path::Path::new(&path), TAIL_BYTES)).await.ok().flatten();
        (log, true)
    } else {
        crate::runners::pending_question_remote::tail(&app, &bot, &run, &host).await
    };
    let entry = tokio::task::spawn_blocking(move || log.and_then(|log| pending_ask_entry(&log))).await.ok().flatten();
    let Some((tool_use_id, input)) = entry else { return null() };
    let jev_options = crate::judge::ask_option::hint_for(&app, &bot, &run.id, &tool_use_id, &input, fresh).await;
    Ok(Json(json!({"questions": input.get("questions").cloned(), "jev_options": jev_options})))
}

/// codex 的非同步提問（`request_user_input_async`，issue #1879）：畫面上排著提問時，題目與選項從 rollout 讀，
/// 形狀同 claude 的 `questions`（`header`／`multiSelect` 固定空與 `false`，另帶 `call_id`／`index`）；`async_screen` 是畫面狀態
/// （`expanded`：已按 shift+← 展開；`count`：畫面上排著幾題），網頁作答前靠它與重讀畫面對帳。
/// 畫面上沒有提問（含讀不到畫面）`questions` 與 `async_screen` 都是 `null`；找不到 rollout 或遠端讀取失敗時只有 `async_screen`。
async fn codex_async(app: &Arc<App>, bot: &db::Bot, run: &db::Run) -> Result<Json<Value>, LcError> {
    let Some(Some(snap)) = crate::runners::codex_async_question::snapshot(app, bot, run).await else {
        return Ok(Json(json!({"questions": null, "jev_options": null, "async_screen": null})));
    };
    let questions: Vec<Value> = snap
        .questions
        .iter()
        .map(|q| {
            let options: Vec<Value> = q.options.iter().map(|o| json!({"label": o, "description": ""})).collect();
            json!({"question": q.title, "header": "", "multiSelect": false, "options": options, "call_id": q.call_id, "index": q.index})
        })
        .collect();
    Ok(Json(json!({
        "questions": if questions.is_empty() { Value::Null } else { Value::Array(questions) },
        "jev_options": null,
        "async_screen": {"expanded": snap.screen.expanded, "count": snap.screen.count},
    })))
}

#[cfg(test)]
mod path_guard_tests {
    use super::*;

    /// The read endpoint must reject a legacy or poisoned DB path outside this bot's projects
    /// tree, even when that path points to a valid JSONL file on the daemon host.
    #[tokio::test]
    async fn a_pending_question_is_not_read_from_outside_the_bots_projects_dir() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let bot = crate::testing::claude_bot(&app, &env.project_id, "pending-path-guard").await;
        let run_id = crate::testing::fake_run(&app, &bot.id).await;
        let scratch = crate::testing::track(std::env::temp_dir().join(format!("am-pending-path-{}", db::ulid())));
        std::fs::create_dir_all(&scratch).unwrap();
        let outside = scratch.join("foreign.jsonl");
        std::fs::write(&outside, json!({"type":"assistant","message":{"content":[{"type":"tool_use","id":"foreign-question","name":"AskUserQuestion","input":{"questions":[{"question":"FOREIGN-QUESTION-7741","options":[{"label":"secret"}]}]}}]}}).to_string()).unwrap();
        sqlx::query("UPDATE runs SET transcript_path=? WHERE id=?")
            .bind(outside.to_string_lossy())
            .bind(&run_id)
            .execute(&app.db)
            .await
            .unwrap();

        let Json(body) = get_pending_question(State(app), Path(bot.id)).await.unwrap();
        assert_eq!(body["questions"], Value::Null, "未驗證的舊路徑不能讓任意本機 JSONL 內容出現在 API 回應");
    }
}

/// codex 非同步提問（issue #1879）：題目從 rollout 讀，畫面是「排著幾題」的真相。
#[cfg(test)]
mod codex_async_tests {
    use super::*;
    use crate::testing as tt;

    const SESSION: &str = "01a15bc3-38d3-78d2-8277-ce884f555ebc";
    const COLLAPSED: &str = "• Queued follow-up inputs\n  ? 1 question · 7s\n    shift+← to answer\n› Ask Codex to do anything\n";

    fn ask(call_id: &str, questions: Value) -> String {
        json!({"type": "response_item", "payload": {"type": "function_call", "name": "request_user_input_async", "call_id": call_id,
            "arguments": json!({"questions": questions}).to_string()}})
        .to_string()
    }

    fn reply(call_id: &str, index: usize) -> String {
        let items = json!([{"answer": "x", "question": "q", "questionItemId": json!(["request_user_input_async", call_id, index]).to_string()}]);
        json!({"type": "response_item", "payload": {"type": "message", "role": "user",
            "content": [{"type": "input_text", "text": format!("<send_user_message_question_reply>\n{items}\n</send_user_message_question_reply>")}]}})
        .to_string()
    }

    /// 一顆本機 codex bot：自己的 CODEX_HOME 底下有這個 session 的 rollout（synthetic，形狀照 codex 0.163.0 真機）。
    /// `live`：herdr 回報綁在這顆 pane 上的 session（`None`＝herdr 沒回報）；DB 記的 session 是 `db_session`。
    async fn rig_with(files: &[(&str, Vec<String>)], screen: &str, live: Option<&str>, db_session: &str) -> (Arc<App>, String, tt::Env) {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot = tt::claude_bot(&app, &env.project_id, "async-api").await;
        let home = tt::track(std::env::temp_dir().join(format!("am-codex-async-{}", db::ulid())));
        let dir = home.join("sessions/2026/10/21");
        std::fs::create_dir_all(&dir).unwrap();
        for (session, rollout) in files {
            std::fs::write(dir.join(format!("rollout-2026-10-21T00-00-00-{session}.jsonl")), rollout.join("\n")).unwrap();
        }
        sqlx::query("UPDATE bots SET kind='codex', env_json=? WHERE id=?")
            .bind(json!({"CODEX_HOME": home.to_string_lossy()}).to_string())
            .bind(&bot.id)
            .execute(&app.db)
            .await
            .unwrap();
        let run_id = tt::fake_run(&app, &bot.id).await;
        sqlx::query("UPDATE runs SET pane_id='pane-async-api', agent_status='blocked', native_session_id=? WHERE id=?")
            .bind(db_session)
            .bind(&run_id)
            .execute(&app.db)
            .await
            .unwrap();
        env.herdr.set_screen("pane-async-api", screen);
        if let Some(live) = live {
            env.herdr.agents.lock().unwrap().push(json!({"name": "async-api", "agent": "codex", "agent_status": "blocked",
                "workspace_id": "w0", "tab_id": "w0:t0", "pane_id": "pane-async-api", "agent_session": {"kind": "id", "value": live}}));
        }
        (app, bot.id, env)
    }

    /// herdr 回報了 live session、DB 也記同一個（常態）。
    async fn rig(rollout: &[String], screen: &str) -> (Arc<App>, String, tt::Env) {
        rig_with(&[(SESSION, rollout.to_vec())], screen, Some(SESSION), SESSION).await
    }

    #[tokio::test]
    async fn the_collapsed_prompt_gets_its_question_and_options_from_the_rollout() {
        let log = [ask("c1", json!([{"title": "「智選 hub 的 menu 最下方」是哪個選單？", "options": ["左側清單", "bot 的「⋯」選單"]}]))];
        let (app, bot, env) = rig(&log, COLLAPSED).await;
        let Json(body) = get_pending_question(State(app), Path(bot)).await.unwrap();
        assert_eq!(body["questions"][0]["question"], "「智選 hub 的 menu 最下方」是哪個選單？");
        assert_eq!(body["questions"][0]["options"][1]["label"], "bot 的「⋯」選單");
        assert_eq!(body["questions"][0]["call_id"], "c1");
        assert_eq!(body["async_screen"], json!({"expanded": false, "count": 1}));
        assert!(env.herdr.calls_to("pane.send_keys").is_empty() && env.herdr.calls_to("pane.send_text").is_empty(), "只讀、不按鍵");
    }

    /// Codex acknowledges enqueueing immediately; only a user reply answers the question.
    #[tokio::test]
    async fn accepted_function_output_does_not_answer_an_async_question() {
        let mut log = vec![
            ask("accepted-call", json!([{"title":"Which account?", "options":["Current", "Another"]}])),
            json!({"type":"response_item", "payload":{
                "type":"function_call_output", "call_id":"accepted-call", "output":r#"{"accepted":true}"#
            }}).to_string(),
        ];
        let pending = crate::codex_async_question::pending_async_questions(&log.join("\n"));
        assert_eq!(pending.len(), 1, "accepted only acknowledges enqueueing");
        assert_eq!(pending[0].call_id, "accepted-call");
        assert_eq!(pending[0].options, ["Current", "Another"]);
        let (app, bot, env) = rig(&log, COLLAPSED).await;
        let Json(body) = get_pending_question(State(app.clone()), Path(bot.clone())).await.unwrap();
        assert_eq!(body["questions"][0]["question"], "Which account?");
        assert_eq!(body["questions"][0]["options"][1]["label"], "Another");
        log.push(reply("accepted-call", 0));
        assert!(crate::codex_async_question::pending_async_questions(&log.join("\n")).is_empty(), "only the user reply resolves it");
        let (answered_app, answered_bot, _answered_env) = rig(&log, COLLAPSED).await;
        let Json(answered) = get_pending_question(State(answered_app), Path(answered_bot)).await.unwrap();
        assert_eq!(answered["questions"], Value::Null);
        assert!(env.herdr.calls_to("pane.send_keys").is_empty() && env.herdr.calls_to("pane.send_text").is_empty());
    }

    /// 答過的題目不再出現；畫面題數與沒作答的題數一致時全部都是。
    #[tokio::test]
    async fn answered_questions_are_not_returned() {
        let log = [ask("done", json!([{"title": "答過的？"}])), reply("done", 0), ask("new", json!([{"title": "現在這題？"}]))];
        let (app, bot, _env) = rig(&log, COLLAPSED).await;
        let Json(body) = get_pending_question(State(app), Path(bot)).await.unwrap();
        let qs = body["questions"].as_array().unwrap();
        assert_eq!(qs.len(), 1);
        assert_eq!(qs[0]["question"], "現在這題？");
    }

    /// 審查反例（P2）：同一個 call 問了 Q1、Q2，使用者在 TUI 切到 Q2 按 `^]` 跳過，畫面剩 1 題（Q1）；rollout 兩題都沒有 reply。
    /// 兩題原文都在對話紀錄裡，分不出剩哪一題：降級成只有題數，絕不回 Q2。
    #[tokio::test]
    async fn a_skipped_question_degrades_instead_of_guessing_the_last_one() {
        let log = [ask("c1", json!([{"title": "Pick a size?", "options": ["S", "M"]}, {"title": "Your name?"}]))];
        let screen = format!("• Pick a size?\n  • S\n  • M\n  Your name?\n{COLLAPSED}");
        let (app, bot, _env) = rig(&log, &screen).await;
        let Json(body) = get_pending_question(State(app), Path(bot)).await.unwrap();
        assert_eq!(body["questions"], Value::Null);
        assert_eq!(body["async_screen"], json!({"expanded": false, "count": 1}));
    }

    /// 複審反例：Q1 待答但已捲出畫面，Q2 剛跳過、原文還在畫面上；畫面剩 1 題。沒作答 2 題 ≠ 畫面 1 題＝有跳過歧義：降級，
    /// 對話紀錄裡看得到 Q2 不是證據，不可回 Q2。
    #[tokio::test]
    async fn a_visible_skipped_question_does_not_make_it_the_pending_one() {
        let log = [ask("c1", json!([{"title": "Q1?"}])), ask("c2", json!([{"title": "Q2?"}]))];
        let screen = format!("• Q2?\n{COLLAPSED}");
        let (app, bot, _env) = rig(&log, &screen).await;
        let Json(body) = get_pending_question(State(app), Path(bot)).await.unwrap();
        assert_eq!(body["questions"], Value::Null);
        assert_eq!(body["async_screen"], json!({"expanded": false, "count": 1}));
    }

    /// 審查反例（P3）：/new 或 resume 後 DB 的 native_session_id 還是舊的。以 herdr 當下綁的 session 為準：讀新 session 的 rollout，
    /// 舊 session 檔裡沒作答的題目不會冒出來。
    #[tokio::test]
    async fn the_live_session_wins_over_a_stale_db_session() {
        const OLD: &str = "01a15bc3-0000-0000-0000-000000000001";
        let files = [(OLD, vec![ask("c-old", json!([{"title": "舊 session 的題目？"}]))]), (SESSION, vec![ask("c-new", json!([{"title": "新 session 的題目？"}]))])];
        let (app, bot, _env) = rig_with(&files, COLLAPSED, Some(SESSION), OLD).await;
        let Json(body) = get_pending_question(State(app), Path(bot)).await.unwrap();
        assert_eq!(body["questions"][0]["question"], "新 session 的題目？");
        assert_eq!(body["questions"].as_array().unwrap().len(), 1);
    }

    /// herdr 沒回報 session、只剩 DB 的（可能是舊的）：題目原文必須在畫面上看得到才認，否則降級。
    #[tokio::test]
    async fn without_a_live_session_the_question_must_be_visible_on_screen() {
        let log = [ask("c1", json!([{"title": "舊 session 的題目？"}]))];
        let (app, bot, _env) = rig_with(&[(SESSION, log.to_vec())], COLLAPSED, None, SESSION).await;
        let Json(body) = get_pending_question(State(app), Path(bot)).await.unwrap();
        assert_eq!(body["questions"], Value::Null, "畫面上沒有這題的原文：可能是 /new 之前的舊題");
        assert_eq!(body["async_screen"], json!({"expanded": false, "count": 1}));

        let screen = format!("• 舊 session 的題目？\n{COLLAPSED}");
        let (app, bot, _env) = rig_with(&[(SESSION, log.to_vec())], &screen, None, SESSION).await;
        let Json(body) = get_pending_question(State(app), Path(bot)).await.unwrap();
        assert_eq!(body["questions"][0]["question"], "舊 session 的題目？");
    }

    /// 展開後（自由輸入題）：面板裡的題目原文對得上才回，網頁拿它當「這題」。
    #[tokio::test]
    async fn an_expanded_panel_returns_the_question_it_shows() {
        let log = [ask("c1", json!([{"title": "Your name?"}])), ask("c2", json!([{"title": "別題？"}]))];
        let screen = "• 別題？\n• Queued follow-up inputs\n  Your name?\n  Type your answer\n  enter submit   ^] skip   shift+→ main prompt\n";
        let (app, bot, _env) = rig(&log, screen).await;
        let Json(body) = get_pending_question(State(app), Path(bot)).await.unwrap();
        let qs = body["questions"].as_array().unwrap();
        assert_eq!(qs.len(), 1);
        assert_eq!(qs[0]["question"], "Your name?");
        assert_eq!(body["async_screen"], json!({"expanded": true, "count": 1}));
    }

    /// 複審 P2：面板上是「Full Name?」，rollout 裡舊的待答題是「Name?」——子字串不算同題，questions 是 null（降級）。
    #[tokio::test]
    async fn a_question_that_is_only_a_substring_of_the_panel_title_is_not_returned() {
        let log = [ask("c1", json!([{"title": "Name?"}]))];
        let screen = "• Queued follow-up inputs\n  Full Name?\n  Type your answer\n  enter submit   ^] skip   shift+→ main prompt\n";
        let (app, bot, _env) = rig(&log, screen).await;
        let Json(body) = get_pending_question(State(app), Path(bot)).await.unwrap();
        assert_eq!(body["questions"], Value::Null);
        assert_eq!(body["async_screen"], json!({"expanded": true, "count": 1}));
    }

    /// 畫面上沒有提問（作答了、跳過了）：什麼都不回，就算 rollout 裡還有沒對到 reply 的題目。
    #[tokio::test]
    async fn no_prompt_on_screen_means_no_question() {
        let (app, bot, _env) = rig(&[ask("c1", json!([{"title": "Q?"}]))], "› Ask Codex to do anything\n").await;
        let Json(body) = get_pending_question(State(app), Path(bot)).await.unwrap();
        assert_eq!((body["questions"].clone(), body["async_screen"].clone()), (Value::Null, Value::Null));
    }

    /// 找不到 rollout：畫面狀態照給，`questions` 是 `null`。
    #[tokio::test]
    async fn without_a_rollout_only_the_screen_state_comes_back() {
        let (app, bot, _env) = rig_with(&[], COLLAPSED, None, "no-such-session").await;
        let Json(body) = get_pending_question(State(app), Path(bot)).await.unwrap();
        assert_eq!(body["questions"], Value::Null);
        assert_eq!(body["async_screen"], json!({"expanded": false, "count": 1}));
    }

    struct RemoteCodex {
        env: tt::Env,
        bot: String,
        host: String,
        reads: Arc<std::sync::atomic::AtomicUsize>,
        log: Arc<std::sync::Mutex<String>>,
    }

    impl RemoteCodex {
        async fn get(&self) -> Value {
            get_pending_question(State(self.env.app.clone()), Path(self.bot.clone())).await.unwrap().0
        }
        fn reads(&self) -> usize { self.reads.load(std::sync::atomic::Ordering::SeqCst) }
    }

    async fn remote_codex(log: &str) -> RemoteCodex {
        let (app, bot, env) = rig(&[], COLLAPSED).await;
        let host = format!("async-remote-{}", db::ulid());
        let conn = app.hosts.insert_remote_with_client_for_test(crate::config::HostCfg {
            shared_session: false, name: host.clone(), ssh: "unused".into(), ssh_port: 22,
            ssh_opts: vec![], herdr_session: "test".into(), remote_path: String::new(),
        }, crate::herdr::HerdrClient::new(env.dir.join("data/herdr.sock"))).await;
        conn.connected.store(true, std::sync::atomic::Ordering::SeqCst);
        sqlx::query("UPDATE projects SET host=? WHERE id=?").bind(&host).bind(&env.project_id).execute(&app.db).await.unwrap();
        sqlx::query("UPDATE bots SET env_json=? WHERE id=?").bind(json!({"CODEX_HOME":"/Users/m4p/.codex"}).to_string()).bind(&bot).execute(&app.db).await.unwrap();
        let current_bot = db::bot(&app.db, &bot).await.unwrap().unwrap();
        sqlx::query("UPDATE runs SET runtime_identity='', launch_rev=? WHERE bot_id=?")
            .bind(crate::launch_rev::of(&current_bot)).bind(&bot).execute(&app.db).await.unwrap();
        let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let log = Arc::new(std::sync::Mutex::new(log.to_string()));
        let (seen, output) = (reads.clone(), log.clone());
        crate::hosts::set_ssh_fake(&host, move |script| {
            assert!(script.contains("python3 -c") && script.contains("/Users/m4p/.codex") && script.contains("8388608"));
            seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let text = output.lock().unwrap().clone();
            if text == "ERROR" { anyhow::bail!("offline") }
            Ok(text)
        });
        RemoteCodex { env, bot, host, reads, log }
    }

    #[tokio::test]
    async fn remote_codex_api_reads_options_and_single_flights_polling_for_twenty_seconds() {
        let remote = remote_codex(&ask("remote", json!([{"title":"Remote question?", "options":["A", "B"]}]))).await;
        crate::hosts::set_ssh_delay(&remote.host, std::time::Duration::from_millis(40));
        let (a, b) = tokio::join!(remote.get(), remote.get());
        assert_eq!(a, b);
        assert_eq!(a["questions"][0]["question"], "Remote question?");
        assert_eq!(a["questions"][0]["options"][1]["label"], "B");
        *remote.log.lock().unwrap() = "ERROR".into();
        let run = db::active_run(&remote.env.app.db, &remote.bot).await.unwrap().unwrap();
        super::super::codex_async_question_remote::age_cache(&run.id, std::time::Duration::from_secs(15));
        assert_eq!(remote.get().await, a);
        assert_eq!(remote.reads(), 1);
        super::super::codex_async_question_remote::age_cache(&run.id, std::time::Duration::from_secs(21));
        assert_eq!(remote.get().await["questions"], Value::Null);
        assert_eq!(remote.reads(), 2, "cache expires after twenty seconds");
        assert!(remote.env.herdr.calls_to("pane.send_keys").is_empty());
        assert!(remote.env.herdr.calls_to("pane.send_text").is_empty());
    }

    #[tokio::test]
    async fn changing_identity_or_env_while_running_never_reads_the_new_directory() {
        let remote = remote_codex(&ask("old", json!([{"title":"Old account question?"}]))).await;
        let app = &remote.env.app;
        app.cfg.update(|cfg| {
            cfg.identities = [
                ("old-account", "/Users/m4p/.codex"),
                ("new-account", "/Users/m4p/.codex-new"),
            ].into_iter().map(|(name, home)| crate::config::IdentityCfg {
                name: name.into(), kind: "codex".into(), host: Some(remote.host.clone()),
                env: [("CODEX_HOME".into(), home.into())].into(), args: vec![],
            }).collect();
            Ok(())
        }).await.unwrap();
        sqlx::query("UPDATE bots SET identity='old-account', env_json='{}' WHERE id=?").bind(&remote.bot).execute(&app.db).await.unwrap();
        let bot = db::bot(&app.db, &remote.bot).await.unwrap().unwrap();
        let run = db::active_run(&app.db, &remote.bot).await.unwrap().unwrap();
        sqlx::query("UPDATE runs SET runtime_identity='old-account', launch_rev=? WHERE id=?")
            .bind(crate::launch_rev::of(&bot)).bind(&run.id).execute(&app.db).await.unwrap();
        // No warm cache: the first read happens after the setting is changed.
        sqlx::query("UPDATE bots SET identity='new-account' WHERE id=?").bind(&remote.bot).execute(&app.db).await.unwrap();
        assert_eq!(remote.get().await["questions"][0]["question"], "Old account question?");
        assert_eq!(remote.reads(), 1);
        super::super::codex_async_question_remote::age_cache(&run.id, std::time::Duration::from_secs(21));
        sqlx::query("UPDATE bots SET env_json=? WHERE id=?")
            .bind(json!({"CODEX_HOME":"/Users/m4p/changed-env"}).to_string()).bind(&remote.bot).execute(&app.db).await.unwrap();
        let body = remote.get().await;
        assert_eq!(body["questions"], Value::Null, "changed env cannot be proven to be the launch env");
        assert_eq!(body["async_screen"]["count"], 1);
        assert_eq!(remote.reads(), 1, "do not read the new configured account or env directory");
    }

    #[tokio::test]
    async fn remote_codex_failure_and_timeout_are_cached_and_keep_screen_count() {
        let failed = remote_codex("ERROR").await;
        for _ in 0..2 {
            let body = failed.get().await;
            assert_eq!(body["questions"], Value::Null);
            assert_eq!(body["async_screen"]["count"], 1);
        }
        assert_eq!(failed.reads(), 1);
        let slow = remote_codex("not reached").await;
        crate::hosts::set_ssh_delay(&slow.host, std::time::Duration::from_secs(30));
        let at = std::time::Instant::now();
        assert_eq!(slow.get().await["questions"], Value::Null);
        assert!(at.elapsed() < std::time::Duration::from_secs(3));
        let at = std::time::Instant::now();
        assert_eq!(slow.get().await["async_screen"]["count"], 1);
        assert!(at.elapsed() < std::time::Duration::from_millis(500), "timeout must be cached");
        assert_eq!(slow.reads(), 0);
    }

    #[tokio::test]
    async fn remote_codex_keeps_skip_and_reply_reconciliation_and_live_session_isolation() {
        let remote = remote_codex(&[
            ask("first", json!([{"title":"Skipped?"}])),
            ask("second", json!([{"title":"Still queued?", "options":["Yes"]}])),
        ].join("\n")).await;
        assert_eq!(remote.get().await["questions"], Value::Null, "two unresolved calls for one queue item must degrade");
        remote.env.herdr.set_screen("pane-async-api", "• Queued follow-up inputs\n  Still queued?\n  Type your answer\n  enter submit   ^] skip   shift+→ main prompt\n");
        assert_eq!(remote.get().await["questions"][0]["question"], "Still queued?");
        assert_eq!(remote.reads(), 1);
        remote.env.herdr.agents.lock().unwrap()[0]["agent_session"]["value"] = json!("new-session");
        *remote.log.lock().unwrap() = [ask("third", json!([{"title":"New question?"}])), reply("third", 0)].join("\n");
        remote.env.herdr.set_screen("pane-async-api", COLLAPSED);
        assert_eq!(remote.get().await["questions"], Value::Null, "answered question is not pending in new session");
        assert_eq!(remote.reads(), 2, "new live session must not reuse old session cache");
    }

    #[tokio::test]
    async fn remote_codex_discards_a_tail_when_the_session_changes_during_ssh() {
        let remote = remote_codex(&ask("old", json!([{"title":"Old question?"}]))).await;
        let agents = remote.env.herdr.agents.clone();
        let reads = remote.reads.clone();
        let log = remote.log.lock().unwrap().clone();
        crate::hosts::set_ssh_fake(&remote.host, move |_| {
            reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            agents.lock().unwrap()[0]["agent_session"]["value"] = json!("new-session");
            Ok(log.clone())
        });
        let body = remote.get().await;
        assert_eq!(body["questions"], Value::Null);
        assert_eq!(body["async_screen"]["count"], 1);
        assert_eq!(remote.reads(), 1);
    }

    #[tokio::test]
    async fn remote_codex_losing_a_live_binding_during_ssh_cannot_relax_to_db_fallback() {
        let remote = remote_codex(&ask("old", json!([{"title":"Old question?"}]))).await;
        let agents = remote.env.herdr.agents.clone();
        let log = remote.log.lock().unwrap().clone();
        crate::hosts::set_ssh_fake(&remote.host, move |_| {
            agents.lock().unwrap().clear();
            Ok(log.clone())
        });
        let body = remote.get().await;
        assert_eq!(body["questions"], Value::Null);
        assert_eq!(body["async_screen"]["count"], 1);
    }

    #[tokio::test]
    async fn remote_codex_db_session_fallback_still_requires_the_title_on_screen() {
        let remote = remote_codex(&ask("fallback", json!([{"title":"Fallback question?"}]))).await;
        remote.env.herdr.agents.lock().unwrap().clear();
        assert_eq!(remote.get().await["questions"], Value::Null);
        remote.env.herdr.set_screen("pane-async-api", &format!("Fallback question?\n{COLLAPSED}"));
        assert_eq!(remote.get().await["questions"][0]["question"], "Fallback question?");
        assert_eq!(remote.reads(), 1);
    }

    #[tokio::test]
    async fn remote_codex_observation_does_not_block_patrol_or_hold_bot_lock() {
        let remote = remote_codex("ERROR").await;
        crate::hosts::set_ssh_delay(&remote.host, std::time::Duration::from_secs(30));
        let app = &remote.env.app;
        let bot = db::bot(&app.db, &remote.bot).await.unwrap().unwrap();
        let run = db::active_run(&app.db, &remote.bot).await.unwrap().unwrap();
        let at = std::time::Instant::now();
        super::super::codex_async_question::observe(app, &run).await;
        super::super::codex_async_question::observe(app, &run).await;
        assert!(at.elapsed() < std::time::Duration::from_millis(500));
        let lock = app.bot_lock(&bot.id).await;
        assert!(lock.try_lock().is_ok());
        tokio::time::timeout(std::time::Duration::from_secs(4), async {
            loop {
                let state = crate::api::state_json(app).await.unwrap();
                if state.to_string().contains("codex 有 1 題非同步提問") { break; }
                tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            }
        }).await.unwrap();
        assert!(remote.env.herdr.calls_to("pane.send_keys").is_empty());
    }

}

/// 遠端主機的 claude bot（issue #1476）：對話檔在那台機器上，經 §12.5b 的 `sh`／ssh 只讀檔尾。
/// 遠端一律是 `set_ssh_fake`，不碰真的 ssh 與真的 `~/.claude`。
#[cfg(test)]
mod remote_tests {
    use super::*;
    use crate::lifecycle::claude_child_log::LOG_MARK;
    use crate::testing as tt;
    use std::sync::Mutex;

    const SID: &str = "99b1f189-867f-4f13-87d7-fb9464ef4041";
    const ROOT: &str = "/Users/m4p/.claude-x";

    fn ask_line(id: &str) -> String {
        json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "id": id, "name": "AskUserQuestion", "input": {"questions": [
            {"question": "連線方式？", "header": "連線", "multiSelect": false,
             "options": [{"label": "內網 http", "description": "流量留在 VPC"}, {"label": "公網 https", "description": "拿掉白名單"}]}]}}]}})
        .to_string()
    }

    struct Rig {
        env: tt::Env,
        bot: db::Bot,
        run_id: String,
        /// 每一次送到遠端主機的 script。
        scripts: Arc<Mutex<Vec<String>>>,
    }

    impl Rig {
        fn reads(&self) -> usize {
            self.scripts.lock().unwrap().iter().filter(|s| s.contains(LOG_MARK)).count()
        }
    }

    /// 遠端主機上一顆 claude bot，run 記了 session 與 transcript 路徑，herdr 在它的 pane 上報的 live session 是 `SID`。
    async fn rig(recorded_path: Option<&str>, reply: impl Fn() -> anyhow::Result<String> + Send + Sync + 'static) -> Rig {
        rig_with(recorded_path, Some(SID), reply).await
    }

    /// 同 [`rig`]，但 herdr 報的 live session 由呼叫端決定；`None`＝這顆 pane 上 herdr 沒有 claude agent。
    async fn rig_with(recorded_path: Option<&str>, live: Option<&str>, reply: impl Fn() -> anyhow::Result<String> + Send + Sync + 'static) -> Rig {
        let env = tt::env().await;
        let app = env.app.clone();
        let host = format!("m4p-pq-{}", db::ulid());
        // 遠端的 herdr 接到同一顆 mock：#1503 起遠端讀取要先問 live session（herdr），所以 host 的 client 得真的能問。
        let conn = app
            .hosts
            .insert_remote_with_client_for_test(
                crate::config::HostCfg {
                    shared_session: false,
                    name: host.clone(),
                    ssh: "unused".into(),
                    ssh_port: 22,
                    ssh_opts: vec![],
                    herdr_session: "am-test".into(),
                    remote_path: String::new(),
                },
                crate::herdr::HerdrClient::new(env.dir.join("data").join("herdr.sock")),
            )
            .await;
        conn.connected.store(true, std::sync::atomic::Ordering::SeqCst);
        let project = db::ulid();
        sqlx::query("INSERT INTO projects (id, path, label, host, created_at) VALUES (?, ?, 'remote', ?, ?)")
            .bind(&project)
            .bind(format!("/remote/{project}"))
            .bind(&host)
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();
        let bot = tt::claude_bot(&app, &project, "remote-asker").await;
        sqlx::query("UPDATE bots SET env_json=? WHERE id=?").bind(json!({"CLAUDE_CONFIG_DIR": ROOT}).to_string()).bind(&bot.id).execute(&app.db).await.unwrap();
        let run_id = tt::fake_run(&app, &bot.id).await;
        sqlx::query("UPDATE runs SET native_session_id=?, transcript_path=?, herdr_session='am-test' WHERE id=?").bind(SID).bind(recorded_path).bind(&run_id).execute(&app.db).await.unwrap();
        // 這顆 run 的 pane 上，herdr 報的 live Claude session（`herdr_session` 以它為準；沒有就是 None）。
        if let Some(live) = live {
            env.herdr.set_unnamed_agent(&format!("pane-{}", bot.id), Some(live));
        }
        let scripts = Arc::new(Mutex::new(Vec::<String>::new()));
        let seen = scripts.clone();
        crate::hosts::set_ssh_fake(&host, move |script| {
            seen.lock().unwrap().push(script.to_string());
            if script.contains(LOG_MARK) {
                return reply();
            }
            Ok("/Users/m4p".to_string())
        });
        let bot = db::bot(&app.db, &bot.id).await.unwrap().unwrap();
        Rig { env, bot, run_id, scripts }
    }

    fn found(body: &str) -> String {
        format!("{LOG_MARK}\n{ROOT}/projects/-work/{SID}.jsonl\n{body}\n")
    }

    async fn get(rig: &Rig) -> Value {
        let Json(body) = get_pending_question(State(rig.env.app.clone()), Path(rig.bot.id.clone())).await.unwrap();
        body
    }

    fn recorded() -> String {
        format!("{ROOT}/projects/-work/{SID}.jsonl")
    }

    /// 遠端 bot 有還沒被回答的 AskUserQuestion → API 帶出題目與選項；只讀檔尾（同本機的 TAIL_BYTES）、檔案由這顆 bot 的 root 組出來。
    #[tokio::test]
    async fn a_remote_bots_unanswered_question_is_read_over_the_existing_shell() {
        let rig = rig(Some(&recorded()), || Ok(found(&ask_line("toolu_1")))).await;
        let body = get(&rig).await;
        assert_eq!(body["questions"][0]["question"], "連線方式？");
        assert_eq!(body["questions"][0]["options"][1]["label"], "公網 https");
        assert_eq!(body["jev_options"], Value::Null, "Jev 沒開");
        let scripts = rig.scripts.lock().unwrap().clone();
        let read = scripts.iter().find(|s| s.contains(LOG_MARK)).expect("有一次遠端讀取");
        assert!(read.contains(ROOT) && read.contains(SID) && read.contains(&format!(" {TAIL_BYTES} ")) && read.contains("os.O_NOFOLLOW"), "{read}");
    }

    /// 連續請求不會每次都打遠端（網頁每 5 秒輪詢、兩個視窗各一份）。
    #[tokio::test]
    async fn consecutive_requests_do_not_hit_the_remote_host_each_time() {
        let rig = rig(Some(&recorded()), || Ok(found(&ask_line("toolu_1")))).await;
        for _ in 0..3 {
            assert_eq!(get(&rig).await["questions"][0]["question"], "連線方式？");
        }
        assert_eq!(rig.reads(), 1);
    }

    /// 記的路徑不在這顆 bot 的 config root 底下、而且檔名就是 live session（別的身分、`..`、不是 jsonl、不在 `projects/` 下）→ null，而且不發遠端讀取。
    #[tokio::test]
    async fn a_recorded_path_outside_the_roots_that_names_the_live_session_is_null_and_never_read() {
        for forged in [
            format!("/Users/m4p/.claude-other/projects/-work/{SID}.jsonl"),
            format!("{ROOT}/projects/../../.claude-other/projects/-work/{SID}.jsonl"),
            format!("{ROOT}/projects/-work/{SID}.txt"),
            format!("{ROOT}/other/{SID}.jsonl"),
        ] {
            let rig = rig(Some(&forged), || Ok(found(&ask_line("toolu_1")))).await;
            let body = get(&rig).await;
            assert_eq!(body["questions"], Value::Null, "{forged}");
            assert_eq!(rig.reads(), 0, "{forged}: 不能發出遠端讀取");
            assert!(rig.scripts.lock().unwrap().iter().all(|s| !s.contains(&forged)), "{forged}: 偽造路徑不能出現在遠端指令裡");
        }
    }

    /// 記的路徑指向別的 session（`/etc/passwd`、另一支 uuid）：它的 stem 不是 live session，所以當成過期或偽造而忽略——
    /// 路徑從頭到尾不進遠端指令；讀取只限 roots 底下 live session 的那一支檔（#1503：以 live session 為準）。
    #[tokio::test]
    async fn a_recorded_path_naming_another_session_is_ignored_and_only_the_live_session_under_the_roots_is_read() {
        for forged in [
            "/etc/passwd".to_string(),
            format!("{ROOT}/projects/-work/00000000-0000-4000-8000-000000000000.jsonl"),
        ] {
            let rig = rig(Some(&forged), || Ok(found(&ask_line("toolu_1")))).await;
            let body = get(&rig).await;
            assert_eq!(body["questions"][0]["question"], "連線方式？", "{forged}: 題目來自 live session 的檔");
            assert_eq!(rig.reads(), 1, "{forged}");
            let scripts = rig.scripts.lock().unwrap().clone();
            assert!(scripts.iter().all(|s| !s.contains(&forged)), "{forged}: 偽造路徑不能出現在遠端指令裡");
            let read = scripts.iter().find(|s| s.contains(LOG_MARK)).expect("有一次遠端讀取");
            assert!(read.contains(ROOT) && read.contains(SID), "{forged}: 讀取只限 roots 底下 live session 的檔\n{read}");
        }
    }

    /// run 沒記 session（`native_session_id` 與路徑都空），herdr 這顆 pane 上也沒有 live session：沒有檔可找，null、不讀。
    #[tokio::test]
    async fn a_run_with_no_recorded_or_live_session_is_null_and_never_read() {
        let rig = rig_with(None, None, || Ok(found(&ask_line("toolu_1")))).await;
        sqlx::query("UPDATE runs SET native_session_id=NULL WHERE id=?").bind(&rig.run_id).execute(&rig.env.app.db).await.unwrap();
        assert_eq!(get(&rig).await["questions"], Value::Null);
        assert_eq!(rig.reads(), 0);
    }

    /// #1503 的新行為：run 沒記 session，但 herdr 報了 live session → 讀 roots 底下那一支檔，不再要求先記過 session。
    #[tokio::test]
    async fn a_run_without_a_recorded_session_reads_the_live_session_file_under_the_roots() {
        let rig = rig_with(None, Some(SID), || Ok(found(&ask_line("toolu_1")))).await;
        sqlx::query("UPDATE runs SET native_session_id=NULL WHERE id=?").bind(&rig.run_id).execute(&rig.env.app.db).await.unwrap();
        assert_eq!(get(&rig).await["questions"][0]["question"], "連線方式？");
        assert_eq!(rig.reads(), 1);
        let scripts = rig.scripts.lock().unwrap().clone();
        let read = scripts.iter().find(|s| s.contains(LOG_MARK)).expect("有一次遠端讀取");
        assert!(read.contains(ROOT) && read.contains(SID), "{read}");
    }

    /// herdr 報的 session id 會被拿去組路徑與 glob，必須是 session id 的形狀（英數、`-`、`_`，≤64）。
    /// 不合形狀的一律當成沒有 live session：null、不讀（即使 run 自己記了一個合法的 session）。
    #[tokio::test]
    async fn a_live_session_that_is_not_a_plain_session_id_is_null_and_never_read() {
        for bad in ["../../etc/passwd", "*", "99b1*", "99b1f189?", "99b1 f189", "99b1\nf189", "../", "a/b"] {
            let rig = rig_with(Some(&recorded()), Some(bad), || Ok(found(&ask_line("toolu_1")))).await;
            let body = get(&rig).await;
            assert_eq!(body["questions"], Value::Null, "{bad:?}");
            assert_eq!(rig.reads(), 0, "{bad:?}: 不能發出遠端讀取");
        }
    }

    /// 遠端讀取失敗（ssh 斷線）或讀不到檔：null，不是錯誤；失敗的結果也節流，不會每個請求都重試。
    #[tokio::test]
    async fn a_failing_or_empty_remote_read_is_null_and_not_retried() {
        let rig = rig(Some(&recorded()), || anyhow::bail!("ssh: connection reset")).await;
        for _ in 0..3 {
            let body = get(&rig).await;
            assert_eq!(body["questions"], Value::Null);
            assert_eq!(body["jev_options"], Value::Null);
        }
        assert_eq!(rig.reads(), 1);

        let missing = rig_missing().await;
        assert_eq!(get(&missing).await["questions"], Value::Null, "檔案不在（script 什麼都沒印）");
    }

    async fn rig_missing() -> Rig {
        rig(Some(&recorded()), || Ok(String::new())).await
    }

    /// 答完的提問不再是「在等」：檔尾有對應的 tool_result → null。
    #[tokio::test]
    async fn an_answered_question_on_the_remote_is_not_pending() {
        let answered = format!(
            "{}\n{}",
            ask_line("toolu_1"),
            json!({"type": "user", "message": {"content": [{"type": "tool_result", "tool_use_id": "toolu_1", "content": "User answered"}]}})
        );
        let rig = rig(Some(&recorded()), move || Ok(found(&answered))).await;
        assert_eq!(get(&rig).await["questions"], Value::Null);
    }

    /// 本機 bot 的路徑沒變：沒記 transcript 照舊 null（沒有走到遠端那條）。
    #[tokio::test]
    async fn a_local_bot_never_takes_the_remote_path() {
        let env = tt::env().await;
        let bot = tt::claude_bot(&env.app, &env.project_id, "local-asker").await;
        let _ = tt::fake_run(&env.app, &bot.id).await;
        let Json(body) = get_pending_question(State(env.app.clone()), Path(bot.id)).await.unwrap();
        assert_eq!(body["questions"], Value::Null);
    }

    /// 打開 Jev：假的 systemone 端點（每次被叫到先跑 `on_request`，好在 Jev 回答前改檔尾）、專案進名單、key 在暫存目錄、
    /// 這顆 bot 的 run 有一則 turn。回傳的目錄由測試自己刪。
    async fn enable_jev(rig: &Rig, on_request: impl Fn() + Clone + Send + Sync + 'static) -> std::path::PathBuf {
        let app = rig.env.app.clone();
        // 假 Jev：第一個選項 0.9、其餘 0.05。
        let route = axum::routing::post(move |axum::Json(body): axum::Json<Value>| {
            let on_request = on_request.clone();
            async move {
                on_request();
                let mut answers = serde_json::Map::new();
                for k in body["questions"].as_object().unwrap().keys() {
                    answers.insert(k.clone(), json!({"type": "noul", "noul": if k.ends_with("_a") { 0.9 } else { 0.05 }}));
                }
                axum::Json(json!({"model": "jev-1.13.0", "answers": answers, "usage": {"input_tokens": 10}}))
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/systemone", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, axum::Router::new().route("/v1/systemone", route)).await.unwrap() });
        let dir = tt::track(std::env::temp_dir().join(format!("am-pq-remote-jev-{}", db::ulid())));
        std::fs::create_dir_all(&dir).unwrap();
        let key = dir.join("key");
        std::fs::write(&key, "k-test\n").unwrap();
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let project = db::bot(&app.db, &rig.bot.id).await.unwrap().unwrap().project_id;
        let key_s = key.to_string_lossy().into_owned();
        app.cfg
            .update(move |c| {
                c.judge.enabled = true;
                c.judge.projects = vec![project];
                c.judge.key_file = key_s;
                c.judge.endpoint = url;
                Ok(())
            })
            .await
            .unwrap();
        let conv = db::conversation_id(&app.db, &rig.bot.id).await.unwrap();
        sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, prompt_text, created_at) VALUES (?,?,?,'web','in_flight','ok',?,?)")
            .bind(db::ulid())
            .bind(&conv)
            .bind(&rig.run_id)
            .bind("連線一律走內網")
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();
        dir
    }

    async fn stale_rows(app: &Arc<App>) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM judge_shadow WHERE error = 'stale'").fetch_one(&app.db).await.unwrap()
    }

    /// #1475 的 `jev_options` 對遠端 bot 一併生效：同一支 `hint_for`（開關、專案名單、名額、mask 照舊）。
    /// 一題的遠端讀取：GET 讀一次檔尾，那次是新鮮的，有效性檢查就用它作證、不再讀；背景問 Jev 的兩段（投 Jev 前、投 Jev 後）各讀一次。
    #[tokio::test]
    async fn a_remote_bot_gets_jev_options_through_the_same_gate() {
        let transcript = Arc::new(Mutex::new(ask_line("toolu_1")));
        let reply = transcript.clone();
        let rig = rig(Some(&recorded()), move || Ok(found(&reply.lock().unwrap()))).await;
        let _clock = crate::runners::pending_question_remote::fixed_clock(&rig.run_id);
        let app = rig.env.app.clone();
        let dir = enable_jev(&rig, || {}).await;

        let mut rx = app.subscribe();
        let first = get(&rig).await;
        assert_eq!(first["questions"][0]["question"], "連線方式？");
        assert_eq!(first["jev_options"], Value::Null, "第一次讀不等 Jev");
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                if rx.recv().await.unwrap().kind == "pending_question_hint" {
                    break;
                }
            }
        })
        .await
        .expect("答案落帳後要發事件");
        assert_eq!(rig.reads(), 3, "GET 的一次讀取（新鮮）＋背景問 Jev 投 Jev 前、投 Jev 後各一次；讀建議前的有效性檢查不再另讀");
        let second = get(&rig).await;
        assert_eq!(second["jev_options"]["questions"], json!([{"index": 0, "scores": [0.9, 0.05], "suggested": 0}]));
        assert_eq!(rig.reads(), 4, "已有建議的後續輪詢：檔尾走快取，讀建議前的有效性檢查另讀一次新鮮的");
        transcript.lock().unwrap().push_str(&format!("\n{}", json!({"message":{"content":[{"type":"tool_result","tool_use_id":"toolu_1"}]}})));
        assert_eq!(get(&rig).await["jev_options"], Value::Null, "檔尾快取仍在，也不能把已回答的提問建議帶出去");
        assert_eq!(rig.reads(), 5, "讀建議前再次讀新鮮的，確認提問已回答");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 投 Jev 的那段等待中提問被答掉：投 Jev 之後的檢查要現讀（不沿用投 Jev 之前那次），所以不寫出建議、不發事件。
    #[tokio::test]
    async fn a_question_answered_while_jev_is_thinking_gets_no_hint() {
        let transcript = Arc::new(Mutex::new(ask_line("toolu_1")));
        let reply = transcript.clone();
        let rig = rig(Some(&recorded()), move || Ok(found(&reply.lock().unwrap()))).await;
        let _clock = crate::runners::pending_question_remote::fixed_clock(&rig.run_id);
        let app = rig.env.app.clone();
        let answered = transcript.clone();
        let dir = enable_jev(&rig, move || {
            answered.lock().unwrap().push_str(&format!("\n{}", json!({"message":{"content":[{"type":"tool_result","tool_use_id":"toolu_1"}]}})));
        })
        .await;

        let mut rx = app.subscribe();
        assert_eq!(get(&rig).await["questions"][0]["question"], "連線方式？");
        // 背景任務在投 Jev 之後的檢查寫下 stale 才算跑完；等它，不靠 sleep。
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            while stale_rows(&app).await == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("背景任務要寫下 stale");
        while let Ok(ev) = rx.try_recv() {
            assert!(ev.kind != "pending_question_hint", "已答的提問不發建議");
        }
        assert_eq!(get(&rig).await["jev_options"], Value::Null, "投 Jev 期間被答掉，建議不能出去");
        std::fs::remove_dir_all(&dir).ok();
    }
}

impl crate::judge::ask_option::PendingQuestion for App {
    async fn question_pending(&self, bot: &db::Bot, run: &db::Run, tool: &str) -> bool {
        let Ok(host) = db::bot_host(&self.db, &bot.id).await else { return false };
        let log = if host == crate::config::LOCAL_HOST {
            let Some(path) = run.transcript_path.as_ref() else { return false };
            if !crate::app_ports_p5::local_transcript_allowed(&self.shared(), bot, path).await { return false; }
            let path = path.clone();
            tokio::task::spawn_blocking(move || read_tail(std::path::Path::new(&path), TAIL_BYTES)).await.ok().flatten()
        } else {
            crate::runners::pending_question_remote::bounded(
                crate::runners::pending_question_remote::REMOTE_READ_TIMEOUT,
                crate::lifecycle::claude_child_log::pending_tail(&self.shared(), bot, run, &host, TAIL_BYTES),
            ).await
        };
        log.and_then(|log| pending_ask_entry(&log)).is_some_and(|(id, _)| id == tool)
    }
}
