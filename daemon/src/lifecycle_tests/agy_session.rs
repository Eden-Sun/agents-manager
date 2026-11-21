#![allow(unused_imports)]
//! #1862 L2-1：從 am-lifecycle 搬來、需要真 daemon App／DB 的測試。

mod tests {
    use crate::lifecycle_tests::prelude::*;
    use crate::lifecycle::agy_session::*;
    use crate::lifecycle::grok_transcript::{sync_locked, Synced};
    use crate::testing as tt;
    fn step(i: u64, ty: &str, content: &str, tokens: Option<i64>) -> String {
        let mut v = serde_json::json!({"step_index": i, "source": if ty == "USER_INPUT" { "USER_EXPLICIT" } else { "MODEL" }, "type": ty, "status": "DONE", "content": content});
        if let Some(n) = tokens {
            v["input_tokens"] = n.into();
        }
        v.to_string()
    }
    fn user(i: u64, text: &str) -> String {
        step(i, "USER_INPUT", &format!("<USER_REQUEST>\n{text}\n</USER_REQUEST>\n<ADDITIONAL_METADATA>x</ADDITIONAL_METADATA>"), None)
    }
    struct Child {
        env: tt::Env,
        run_id: String,
        turn_id: String,
        file: std::path::PathBuf,
    }
    const REMOTE_HOME: &str = "/Users/me";
    /// 一顆沒有 hook 的 agy 子 agent：adopted、`inject_hooks = 0`、已經綁了對話（pane 環境的探測不在這裡測）、有一筆在飛的回合。
    async fn agy_child(sid: &str, transcript: &str) -> Child {
        agy_child_on(tt::env().await, None, sid, transcript).await
    }
    /// `host`＝`None` 是本機（對話檔寫進 HOME）；遠端只建專案、bot、run，對話檔在那台（測試用假 ssh 回）。
    async fn agy_child_on(env: tt::Env, host: Option<&str>, sid: &str, transcript: &str) -> Child {
        let app = env.app.clone();
        let project_id = match host {
            None => env.project_id.clone(),
            Some(h) => {
                let id = db::ulid();
                sqlx::query("INSERT INTO projects (id, path, label, host, created_at) VALUES (?,?,?,?,?)")
                    .bind(&id)
                    .bind(format!("{REMOTE_HOME}/project/wt"))
                    .bind("remote")
                    .bind(h)
                    .bind(db::now())
                    .execute(&app.db)
                    .await
                    .unwrap();
                id
            }
        };
        let bot_id = db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, model, args_json, autostart, inject_hooks, managed_by, hook_token, created_at)
             VALUES (?,?,'a1','agy','gemini-3.8-flash-medium','[]',0,0,'child','tok',?)",
        )
        .bind(&bot_id)
        .bind(&project_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let file = match host {
            None => {
                let file = crate::agy_support::transcript_path(&crate::home::dir().unwrap(), sid);
                std::fs::create_dir_all(file.parent().unwrap()).unwrap();
                std::fs::write(&file, transcript).unwrap();
                file
            }
            Some(_) => crate::agy_support::transcript_path(Path::new(REMOTE_HOME), sid),
        };
        let run_id = db::ulid();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, workspace_id, pane_id, adopted, agent_name, herdr_session, native_session_id, started_at)
             VALUES (?,?,'running','idle','ws-1',?,1,'parent-a1','test',?,?)",
        )
        .bind(&run_id)
        .bind(&bot_id)
        .bind(format!("pane-{bot_id}"))
        .bind(sid)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let conv = db::conversation_id(&app.db, &bot_id).await.unwrap();
        let turn_id = db::ulid();
        sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, prompt_text, created_at) VALUES (?,?,?,'web','in_flight','ok','say PONG',?)")
            .bind(&turn_id)
            .bind(&conv)
            .bind(&run_id)
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();
        sqlx::query("INSERT INTO messages (id, conversation_id, turn_id, role, content, source, created_at) VALUES (?,?,?,'user','say PONG','web',?)")
            .bind(db::ulid())
            .bind(&conv)
            .bind(&turn_id)
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();
        Child { env, run_id, turn_id, file }
    }
    async fn replies(c: &Child) -> Vec<(String, String)> {
        sqlx::query_as("SELECT content, source FROM messages WHERE turn_id = ? AND role = 'assistant'").bind(&c.turn_id).fetch_all(&c.env.app.db).await.unwrap()
    }
    /// 2026-10-04 a1：子 agent 沒有 hook，回覆以前永遠是整個 pane 畫面（terminal_fallback）。現在讀對話檔：回覆是 `PLANNER_RESPONSE` 的原文。
    #[tokio::test]
    async fn a_hookless_agy_child_gets_its_reply_from_the_transcript_not_the_pane() {
        let sid = format!("c-{}", db::ulid());
        let text = [user(0, "say PONG"), step(1, "PLANNER_RESPONSE", "PONG", Some(11824))].join("\n");
        let c = agy_child(&sid, &text).await;
        let app = c.env.app.clone();
        assert_eq!(sync_locked(&app, &c.run_id).await.unwrap(), Synced::Read { imported: 1, pending: None });
        assert_eq!(replies(&c).await, [("PONG".to_string(), "transcript".to_string())]);
        let t: db::Turn = sqlx::query_as("SELECT * FROM turns WHERE id = ?").bind(&c.turn_id).fetch_one(&app.db).await.unwrap();
        assert_eq!((t.status.as_str(), t.native_session_id.as_deref(), t.native_turn_id.as_deref()), ("completed", Some(sid.as_str()), Some("p0")));
        let r = db::run(&app.db, &c.run_id).await.unwrap().unwrap();
        let st: serde_json::Value = serde_json::from_str(r.status_json.as_deref().expect("status_json")).unwrap();
        assert_eq!(st["model"]["id"], "gemini-3.8-flash-medium");
        assert_eq!(st["context_window"]["total_input_tokens"], 11824);
        assert_eq!(r.transcript_path.as_deref(), c.file.to_str());
        assert_eq!(sync_locked(&app, &c.run_id).await.unwrap(), Synced::Read { imported: 0, pending: None });
        assert_eq!(replies(&c).await.len(), 1);
    }
    #[tokio::test]
    async fn an_answer_still_being_worked_on_is_pending_and_nothing_is_recorded_yet() {
        let sid = format!("c-{}", db::ulid());
        let text = [user(0, "say PONG"), step(1, "PLANNER_RESPONSE", "let me check", None), step(2, "RUN_COMMAND", "ls", None)].join("\n");
        let c = agy_child(&sid, &text).await;
        let got = sync_locked(&c.env.app, &c.run_id).await.unwrap();
        assert_eq!(got, Synced::Read { imported: 0, pending: Some("say PONG".into()) });
        assert!(replies(&c).await.is_empty(), "旁白不是最終回覆");
        let done = format!("{text}\n{}", step(3, "PLANNER_RESPONSE", "PONG", None));
        std::fs::write(&c.file, done).unwrap();
        assert_eq!(sync_locked(&c.env.app, &c.run_id).await.unwrap(), Synced::Read { imported: 1, pending: None });
        assert_eq!(replies(&c).await[0].0, "PONG");
    }
    #[tokio::test]
    async fn a_daemon_started_agy_bot_with_hooks_and_a_run_with_no_conversation_stay_on_the_old_paths() {
        let c = agy_child("c-none", "").await;
        std::fs::remove_file(&c.file).unwrap();
        assert_eq!(sync_locked(&c.env.app, &c.run_id).await.unwrap(), Synced::Unavailable, "沒有紀錄檔：照舊看畫面");
        sqlx::query("UPDATE bots SET inject_hooks = 1 WHERE id = (SELECT bot_id FROM runs WHERE id = ?)").bind(&c.run_id).execute(&c.env.app.db).await.unwrap();
        std::fs::create_dir_all(c.file.parent().unwrap()).unwrap();
        std::fs::write(&c.file, [user(0, "x"), step(1, "PLANNER_RESPONSE", "y", None)].join("\n")).unwrap();
        assert_eq!(sync_locked(&c.env.app, &c.run_id).await.unwrap(), Synced::Unavailable, "有 hook 的 bot 由 hook 收，不重複記");
    }
    /// 遠端（mac_builder）的 agy 子 agent（issue #1788）：pane 裡的行程開著兩段對話，只有一段有紀錄檔 → 綁到有紀錄檔那段；
    /// `/proc` 換成那台的 `lsof`，紀錄檔的時間與尾巴都經 ssh 從那台讀，路徑是那台家目錄底下的。
    #[tokio::test]
    async fn a_remote_agy_child_binds_the_open_conversation_that_has_a_transcript_over_ssh() {
        let e = tt::env().await;
        let sock = tt::track(std::env::temp_dir().join(format!("am-agy-remote-{}", db::ulid())));
        std::fs::create_dir_all(&sock).unwrap();
        let host = format!("mac-builder-{}", db::ulid().to_ascii_lowercase());
        let conn = e
            .app
            .hosts
            .insert_remote_with_client_for_test(
                crate::config::HostCfg { name: host.clone(), ssh: "unused".into(), herdr_session: "test".into(), ..Default::default() },
                crate::herdr::HerdrClient::new(sock.join("herdr.sock")),
            )
            .await;
        conn.connected.store(true, std::sync::atomic::Ordering::SeqCst);
        *conn.remote_home.lock().await = Some(REMOTE_HOME.into());

        let sid = format!("c-{}", db::ulid());
        let other = format!("d-{}", db::ulid());
        let text = [user(0, "say PONG"), step(1, "PLANNER_RESPONSE", "PONG", Some(11824))].join("\n");
        let c = agy_child_on(e, Some(&host), &sid, &text).await;
        let pane = db::run(&c.env.app.db, &c.run_id).await.unwrap().unwrap().pane_id.expect("pane");
        let seen: std::sync::Arc<std::sync::Mutex<Vec<String>>> = Default::default();
        {
            let (seen, sid, other, pane, text) = (seen.clone(), sid.clone(), other.clone(), pane.clone(), text.clone());
            crate::hosts::set_ssh_fake(&host, move |script| {
                seen.lock().unwrap().push(script.to_string());
                Ok(if script.contains("ps -Awwo") {
                    format!("---AM-ENV---\n  901 agy HERDR_PANE_ID={pane}\n")
                } else if script.contains("lsof") {
                    format!("p901\nn{REMOTE_HOME}/.zshrc\nn{REMOTE_HOME}/.gemini/antigravity-cli/conversations/{other}.db\nn{REMOTE_HOME}/.gemini/antigravity-cli/conversations/{sid}.db\n")
                } else if script.contains("date -r") {
                    format!("{sid} 1700000000\n")
                } else if script.contains("tail -c") {
                    text.clone()
                } else {
                    String::new()
                })
            });
        }

        let app = c.env.app.clone();
        assert_eq!(sync_locked(&app, &c.run_id).await.unwrap(), Synced::Read { imported: 1, pending: None });
        assert_eq!(replies(&c).await, [("PONG".to_string(), "transcript".to_string())]);
        let r = db::run(&app.db, &c.run_id).await.unwrap().unwrap();
        assert_eq!(r.native_session_id.as_deref(), Some(sid.as_str()));
        assert_eq!(r.transcript_path.as_deref(), c.file.to_str());
        assert!(c.file.starts_with(REMOTE_HOME), "{:?}", c.file);
        let scripts = seen.lock().unwrap().clone();
        assert!(scripts.iter().any(|s| s.contains("lsof") && s.contains("-p 901")), "{scripts:?}");
    }
}
