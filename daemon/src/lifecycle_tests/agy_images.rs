#![allow(unused_imports)]
//! #2215：agy 產圖收集的整合測試（直接送出路徑拍快照 → 回合結束收圖 → outbox 列出與下載）。

mod agy_images_tests {
    use crate::lifecycle_tests::prelude::*;
    use crate::lifecycle::agy_images_collect;
    use crate::lifecycle::prompt::{prompt, queue_for_next_turn, RelaySrc};
    use crate::lifecycle::turn_controller;
    use crate::testing as tt;
    use std::collections::HashMap;
    use std::path::PathBuf;

    const PNG: &[u8] = b"\x89PNG\r\n\x1a\nmockup-bytes";

    struct Agy {
        env: tt::Env,
        bot_id: String,
        brain: PathBuf,
    }

    /// 正在跑、閒置、已綁對話的 agy bot；brain 目錄裡先放一張舊圖（回合開始前就在的）。
    async fn running_agy(sid: &str) -> Agy {
        let env = tt::env().await;
        let app = env.app.clone();
        let bot_id = db::ulid();
        sqlx::query(
            "INSERT INTO bots (id, project_id, name, kind, args_json, autostart, inject_hooks, hook_token, created_at)
             VALUES (?,?,'agy-images','agy','[]',0,1,'tok',?)",
        )
        .bind(&bot_id)
        .bind(&env.project_id)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        let run_id = db::ulid();
        sqlx::query(
            "INSERT INTO runs (id, bot_id, state, agent_status, herdr_session, pane_id, pane_typed, native_session_id, started_at)
             VALUES (?,?,'running','idle','test','pane-agy-images',1,?,?)",
        )
        .bind(&run_id)
        .bind(&bot_id)
        .bind(sid)
        .bind(db::now())
        .execute(&app.db)
        .await
        .unwrap();
        env.herdr.set_agent("agy-images", "pane-agy-images", false);
        env.herdr.live_pane("pane-agy-images", tt::LivePane { width: Some(120), agy: true, no_spinner: true, ..Default::default() });
        let brain = crate::home::dir().unwrap().join(".gemini").join("antigravity-cli").join("brain").join(sid);
        std::fs::create_dir_all(&brain).unwrap();
        std::fs::write(brain.join("old.png"), b"\x89PNG\r\n\x1a\nold-render").unwrap();
        Agy { env, bot_id, brain }
    }

    #[tokio::test]
    async fn a_direct_prompt_to_a_running_idle_agy_bot_snapshots_and_collects_its_new_image() {
        let sid = format!("img-{}", db::ulid());
        let c = running_agy(&sid).await;
        let app = c.env.app.clone();

        let out = prompt(&app, &c.bot_id, "做一張 mockup", "agy-img-direct").await.expect("直接送出");
        let (before, state): (Option<String>, String) = sqlx::query_as("SELECT before_json, state FROM agy_image_marks WHERE turn_id = ?")
            .bind(&out.turn_id)
            .fetch_optional(&app.db)
            .await
            .unwrap()
            .expect("直接送出的回合要拍快照");
        assert_eq!(state, "open");
        assert!(before.unwrap().contains("old.png"), "回合前就在的舊圖進快照");

        std::fs::write(c.brain.join("mockup.png"), PNG).unwrap();
        turn_controller::set_status(&app.db, &out.turn_id, "in_flight", "completed", "agy images test").await.unwrap();
        agy_images_collect::collect_due(&app).await;

        let state: String = sqlx::query_scalar("SELECT state FROM agy_image_marks WHERE turn_id = ?").bind(&out.turn_id).fetch_one(&app.db).await.unwrap();
        assert_eq!(state, "done");
        let (msg_id, source): (String, String) = sqlx::query_as("SELECT id, source FROM messages WHERE turn_id = ? AND role = 'assistant'")
            .bind(&out.turn_id)
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(source, "system");
        let ready: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM attachments WHERE message_id = ? AND state = 'ready' AND mime = 'image/png'").bind(&msg_id).fetch_one(&app.db).await.unwrap();
        assert_eq!(ready, 1, "只收新產的 mockup.png，舊圖不收");

        let resp = crate::outbox::list_for(&app, c.bot_id.clone()).await.unwrap();
        let body: serde_json::Value = serde_json::from_slice(&axum::body::to_bytes(resp.into_body(), usize::MAX).await.unwrap()).unwrap();
        let entry = body["files"].as_array().unwrap().iter().find(|f| f["source"] == "agy").expect("產圖在 outbox 清單裡");
        let name = entry["name"].as_str().unwrap().to_string();
        assert!(name.starts_with("agy-images/") && name.ends_with(".png"), "{name}");
        let left = entry["remaining_secs"].as_u64().unwrap();
        assert!(left > 50 * 60 && left <= 3600, "跟 outbox 一樣保留 1 小時：{left}");

        let dl = crate::outbox::file_for(&app, c.bot_id.clone(), HashMap::from([("path".to_string(), name.clone())])).await.unwrap();
        let bytes = axum::body::to_bytes(dl.into_body(), usize::MAX).await.unwrap();
        assert_eq!(bytes.as_ref(), PNG);
    }

    #[tokio::test]
    async fn a_prompt_queued_for_the_next_turn_takes_the_snapshot_too() {
        let sid = format!("img-{}", db::ulid());
        let c = running_agy(&sid).await;
        let app = c.env.app.clone();
        let conv = db::conversation_id(&app.db, &c.bot_id).await.unwrap();
        let out = queue_for_next_turn(&app, &conv, &c.bot_id, "排隊的一句", "排隊的一句", "agy-img-queued", RelaySrc::default()).await.expect("排進佇列");
        let (before, state): (Option<String>, String) = sqlx::query_as("SELECT before_json, state FROM agy_image_marks WHERE turn_id = ?")
            .bind(&out.turn_id)
            .fetch_optional(&app.db)
            .await
            .unwrap()
            .expect("排隊的回合也要拍快照");
        assert_eq!(state, "open");
        assert!(before.is_some_and(|b| b.contains("old.png")));
    }
}
