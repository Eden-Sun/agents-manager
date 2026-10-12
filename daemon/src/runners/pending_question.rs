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
    if bot.kind != "claude" {
        return null();
    }
    let host = db::bot_host(&app.db, &id).await.map_err(up)?;
    let log = if host == crate::config::LOCAL_HOST {
        let Some(path) = run.transcript_path.clone().filter(|p| !p.trim().is_empty()) else { return null() };
        if !crate::app_ports_p5::local_transcript_allowed(&app, &bot, &path).await {
            return null();
        }
        tokio::task::spawn_blocking(move || read_tail(std::path::Path::new(&path), TAIL_BYTES)).await.ok().flatten()
    } else {
        crate::runners::pending_question_remote::tail(&app, &bot, &run, &host).await
    };
    let entry = tokio::task::spawn_blocking(move || log.and_then(|log| pending_ask_entry(&log))).await.ok().flatten();
    let Some((tool_use_id, input)) = entry else { return null() };
    let jev_options = crate::judge::ask_option::hint_for(&app, &bot, &run.id, &tool_use_id, &input).await;
    Ok(Json(json!({"questions": input.get("questions").cloned(), "jev_options": jev_options})))
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

    /// 遠端主機上一顆 claude bot，run 記了 session 與 transcript 路徑；`reply` 是遠端 script 的回應。
    async fn rig(recorded_path: Option<&str>, reply: impl Fn() -> anyhow::Result<String> + Send + Sync + 'static) -> Rig {
        let env = tt::env().await;
        let app = env.app.clone();
        let host = format!("m4p-pq-{}", db::ulid());
        let conn = app
            .hosts
            .insert_remote_for_test(crate::config::HostCfg {
                shared_session: false,
                name: host.clone(),
                ssh: "unused".into(),
                ssh_port: 22,
                ssh_opts: vec![],
                herdr_session: "am-test".into(),
                remote_path: String::new(),
            })
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
        sqlx::query("UPDATE runs SET native_session_id=?, transcript_path=? WHERE id=?").bind(SID).bind(recorded_path).bind(&run_id).execute(&app.db).await.unwrap();
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

    /// 路徑不在這顆 bot 的 config root 底下（別的身分、`..`、不是 jsonl、檔名跟 session 對不上）→ null，而且不發遠端讀取。
    #[tokio::test]
    async fn a_recorded_path_outside_the_bots_roots_is_null_and_never_read() {
        for forged in [
            "/etc/passwd".to_string(),
            format!("/Users/m4p/.claude-other/projects/-work/{SID}.jsonl"),
            format!("{ROOT}/projects/../../.claude-other/projects/-work/{SID}.jsonl"),
            format!("{ROOT}/projects/-work/{SID}.txt"),
            format!("{ROOT}/projects/-work/00000000-0000-4000-8000-000000000000.jsonl"),
            format!("{ROOT}/other/{SID}.jsonl"),
        ] {
            let rig = rig(Some(&forged), || Ok(found(&ask_line("toolu_1")))).await;
            let body = get(&rig).await;
            assert_eq!(body["questions"], Value::Null, "{forged}");
            assert_eq!(rig.reads(), 0, "{forged}: 不能發出遠端讀取");
        }
    }

    /// run 沒記 session（`native_session_id` 與路徑都空）：沒有檔可找，null、不讀。
    #[tokio::test]
    async fn a_run_without_a_recorded_session_is_null_and_never_read() {
        let rig = rig(None, || Ok(found(&ask_line("toolu_1")))).await;
        sqlx::query("UPDATE runs SET native_session_id=NULL WHERE id=?").bind(&rig.run_id).execute(&rig.env.app.db).await.unwrap();
        assert_eq!(get(&rig).await["questions"], Value::Null);
        assert_eq!(rig.reads(), 0);
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

    /// #1475 的 `jev_options` 對遠端 bot 一併生效：同一支 `hint_for`（開關、專案名單、名額、mask 照舊）。
    #[tokio::test]
    async fn a_remote_bot_gets_jev_options_through_the_same_gate() {
        let rig = rig(Some(&recorded()), || Ok(found(&ask_line("toolu_1")))).await;
        let app = rig.env.app.clone();
        // 假 Jev：第一個選項 0.9、其餘 0.05。
        let route = axum::routing::post(|axum::Json(body): axum::Json<Value>| async move {
            let mut answers = serde_json::Map::new();
            for k in body["questions"].as_object().unwrap().keys() {
                answers.insert(k.clone(), json!({"type": "noul", "noul": if k.ends_with("_a") { 0.9 } else { 0.05 }}));
            }
            axum::Json(json!({"model": "jev-1.13.0", "answers": answers, "usage": {"input_tokens": 10}}))
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
        let second = get(&rig).await;
        assert_eq!(second["jev_options"]["questions"], json!([{"index": 0, "scores": [0.9, 0.05], "suggested": 0}]));
        assert_eq!(rig.reads(), 1, "題目是節流過的同一份，沒有為了 Jev 多讀一次遠端");
        std::fs::remove_dir_all(&dir).ok();
    }
}
