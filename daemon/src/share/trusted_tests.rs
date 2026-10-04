//! 信任分享（`share_profile = "trusted"`，使用者 2026-10-04）與擁有者回合的回覆（SPEC §20）。

use super::*;

/// 一顆信任分享的 bot（照 API 那條路：先記 `shared_bots`、再指 cwd），資料夾是一個既有的暫存目錄。
async fn trusted_bot(app: &Arc<App>, project_id: &str, name: &str) -> (db::Bot, std::path::PathBuf) {
    let b = tt::claude_bot(app, project_id, name).await;
    let dir = tt::scratch_dir("am-share-trusted");
    let folder = folder::ShareFolderIn::Existing { path: dir.to_string_lossy().into_owned() };
    let (ws, made) = admin::reserve_share_bot(app, &b.id, store::PROFILE_TRUSTED, &folder, false).await.unwrap();
    admin::finish_restricted(app, &b.id, &ws, made, true).await;
    (db::bot(&app.db, &b.id).await.unwrap().unwrap(), std::fs::canonicalize(dir).unwrap())
}

#[tokio::test]
async fn a_trusted_share_bot_needs_an_explicit_confirm_and_only_an_existing_folder() {
    let e = tt::env().await;
    let pid = e.project_id.clone();
    e.app
        .cfg
        .update(|c| {
            c.projects.push(crate::config::ProjectCfg {
                id: Some(pid.clone()),
                path: e.repo.to_string_lossy().into_owned(),
                label: "proj".into(),
                host: "local".into(),
                bots: vec![],
                handed_off_to: None,
            });
            Ok(())
        })
        .await
        .unwrap();
    let base = serve(crate::api::router(e.app.clone())).await;
    let c = client();
    let create = |body: Value| c.post(format!("{base}/api/projects/{pid}/bots")).header("X-AM-Token", "test-token").json(&body).send();
    let site = tt::scratch_dir("am-share-trusted-site");
    let existing = json!({"kind": "existing", "path": site.to_string_lossy()});

    let res = create(json!({"name": "t1", "kind": "claude", "share_profile": "trusted", "share_folder": existing})).await.unwrap();
    assert_eq!(res.status(), 400, "沒有 confirm_trusted 不能建");
    assert_eq!(res.json::<Value>().await.unwrap()["reason"], "confirm_trusted_required");
    let res = create(json!({"name": "t1", "kind": "claude", "share_profile": "trusted", "confirm_trusted": false, "share_folder": existing})).await.unwrap();
    assert_eq!(res.status(), 400, "false 也不行");
    let res = create(json!({"name": "t1", "kind": "claude", "share_profile": "restricted", "confirm_trusted": true})).await.unwrap();
    assert_eq!(res.status(), 400, "confirm_trusted 只給信任分享");
    let res = create(json!({"name": "t1", "kind": "claude", "share_profile": "trusted", "confirm_trusted": true, "share_folder": {"kind": "new", "name": "t1"}})).await.unwrap();
    assert_eq!(res.status(), 400, "信任分享只用既有資料夾");
    let res = create(json!({"name": "t1", "kind": "codex", "share_profile": "trusted", "confirm_trusted": true, "share_folder": existing})).await.unwrap();
    assert_eq!(res.status(), 409);
    let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM shared_bots").fetch_one(&e.app.db).await.unwrap();
    assert_eq!(n, 0, "被拒絕的一顆都沒記下來");

    let v: Value = create(json!({"name": "t1", "kind": "claude", "share_profile": "trusted", "confirm_trusted": true, "share_folder": existing}))
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let id = v["bot_id"].as_str().unwrap_or_else(|| panic!("帶了 confirm 就建得起來：{v}")).to_string();
    let profile: String = sqlx::query_scalar("SELECT profile FROM shared_bots WHERE bot_id = ?").bind(&id).fetch_one(&e.app.db).await.unwrap();
    assert_eq!(profile, "trusted");
    let bot = db::bot(&e.app.db, &id).await.unwrap().unwrap();
    let ws = std::fs::canonicalize(&site).unwrap().to_string_lossy().into_owned();
    assert_eq!(bot.cwd.as_deref(), Some(ws.as_str()), "cwd 是選的資料夾");
    assert_eq!(bot.auto_approve, 1, "權限照 bot 設定（預設 auto-approve），不像受限的強制關掉");
    assert_eq!(bot.inject_hooks, 1);
    let st: Value = c.get(format!("{base}/api/state")).header("X-AM-Token", "test-token").send().await.unwrap().json().await.unwrap();
    let bots = st["projects"].as_array().unwrap().iter().flat_map(|p| p["bots"].as_array().unwrap().iter()).collect::<Vec<_>>();
    assert_eq!(bots.iter().find(|b| b["id"] == json!(id)).unwrap()["share_profile"], "trusted");

    // 分享開得起來，跟受限的一樣。
    set_base(&e.app).await;
    let r: Value = c.post(format!("{base}/api/bots/{id}/share")).header("X-AM-Token", "test-token").json(&json!({"enabled": true})).send().await.unwrap().json().await.unwrap();
    assert_eq!(r["enabled"], true, "{r}");
    assert!(r["url"].as_str().is_some_and(|u| u.contains("/s/")), "{r}");
}

#[tokio::test]
async fn a_trusted_share_bot_starts_as_an_ordinary_bot_without_the_cage() {
    let e = tt::env().await;
    let (b, dir) = trusted_bot(&e.app, &e.project_id, "ops").await;
    crate::lifecycle::start_bot(&e.app, &b.id).await.unwrap();
    let (args, pane) = started(&e);
    for cage in ["--restricted", "--tools", "--strict-mcp-config", "dontAsk"] {
        assert!(!args.contains(&cage.to_string()), "信任分享不套籠子：{cage} 在 {args:?}");
    }
    assert!(args.contains(&"--dangerously-skip-permissions".to_string()), "auto_approve 照 bot 設定、Bash 等工具全開：{args:?}");
    assert_eq!(pane["cwd"], json!(dir.to_string_lossy()), "cwd 是選的資料夾");
    assert!(dir.join("inbox").is_dir(), "上傳的 inbox 一樣在");
    assert!(crate::outbox::dir_for(&e.app.data_dir, &b.id).unwrap().join(crate::outbox::SHARE_KEEP_MARK).exists(), "outbox 不給 gc 清");
    // 受限 bot 的「hook token 只准打自己的 hook」不套在信任分享上；AGM 不能刪、不能停則一樣。
    assert!(!refuses_bot_principal(&e.app.db, &b.id).await);
    assert_eq!(guards_from_bot_principal(&e.app.db, "DELETE", &format!("/api/bots/{}", b.id)).await, Some(b.id.clone()));
    assert_eq!(guards_from_bot_principal(&e.app.db, "POST", &format!("/api/bots/{}/stop", b.id)).await, Some(b.id.clone()));
    let r = restricted_bot(&e.app, &e.project_id, "pub").await;
    assert!(refuses_bot_principal(&e.app.db, &r.id).await, "受限的照舊關著");
}

#[tokio::test]
async fn a_trusted_share_link_stays_inside_the_portal_api_surface() {
    let e = tt::env().await;
    let (b, _) = trusted_bot(&e.app, &e.project_id, "portal-boundary").await;
    let token = shared(&e.app, &b.id).await;
    let base = serve(portal::router(e.app.clone())).await;
    let c = client();

    let info: Value = c
        .get(format!("{base}/s/{token}/api/info"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(info["bot_name"], b.name);
    assert_eq!(info.as_object().unwrap().len(), 2, "分享頁只拿 bot_name 與 status，不拿 profile 或主 UI 狀態：{info}");

    let state = c.get(format!("{base}/s/{token}/api/state")).send().await.unwrap();
    assert_eq!(state.status(), reqwest::StatusCode::NOT_FOUND, "trusted 連結不能進主 UI API");
    let accept = c
        .post(format!("{base}/s/{token}/api/bots/{}/suggestion/accept", b.id))
        .json(&json!({"suggestion": "stale", "client_request_id": "share-attempt"}))
        .send()
        .await
        .unwrap();
    assert_eq!(accept.status(), reqwest::StatusCode::NOT_FOUND, "trusted 連結不能直接呼叫 accept 路由");
}

#[tokio::test]
async fn a_trusted_share_bot_is_kept_out_of_idle_sleep_and_outbox_expiry() {
    let e = tt::env().await;
    let (b, _) = trusted_bot(&e.app, &e.project_id, "desk").await;
    assert!(store::is_share_bot(&e.app.db, &b.id).await.unwrap());
    assert!(!store::is_caged(&e.app.db, &b.id).await.unwrap());
    tt::fake_run(&e.app, &b.id).await;
    let c = crate::supervisor::idle_sleep::candidates(&e.app).await.unwrap().into_iter().find(|c| c.bot_id == b.id).expect("在名單裡");
    assert!(c.is_share_bot, "閒置再久也不收");
    // outbox 列表：不過期（`ttl_secs:null`、`kept`）。
    let out = crate::outbox::dir_for(&e.app.data_dir, &b.id).unwrap();
    std::fs::create_dir_all(&out).unwrap();
    std::fs::write(out.join("card.png"), b"\x89PNG\r\n").unwrap();
    assert!(out.join(crate::outbox::SHARE_KEEP_MARK).exists(), "建立時就放好不清的標記");
    let base = serve(crate::api::router(e.app.clone())).await;
    let v: Value = client().get(format!("{base}/api/bots/{}/outbox", b.id)).header("X-AM-Token", "test-token").send().await.unwrap().json().await.unwrap();
    assert_eq!(v["kept"], true, "{v}");
    assert_eq!(v["ttl_secs"], Value::Null, "{v}");
}

#[tokio::test]
async fn the_bots_reply_to_an_owner_turn_stays_off_the_share_page_unless_flagged() {
    let e = tt::env().await;
    let (b, _) = trusted_bot(&e.app, &e.project_id, "ops").await;
    let token = shared(&e.app, &b.id).await;
    assert_eq!(SHARE_SENDER, "share", "VISIBLE_SQL 寫死的字要跟哨符一致");
    add_message(&e.app, &b.id, "assistant", "開場白：你好", None).await;
    add_turn_message(&e.app, &b.id, Some("t1"), "user", &format!("{}幫我跑測試", portal::SHARE_PREFIX), Some(SHARE_SENDER)).await;
    add_turn_message(&e.app, &b.id, Some("t1"), "assistant", "測試都過了", None).await;
    add_turn_message(&e.app, &b.id, Some("t2"), "user", "後台：記得別動 main", None).await;
    add_turn_message(&e.app, &b.id, Some("t2"), "assistant", "ok", None).await;
    let flagged = add_turn_message(&e.app, &b.id, Some("t3"), "user", "後台：跟他說今天會晚點回", None).await;
    store::mark_reply_visible(&e.app.db, &flagged).await.unwrap();
    add_turn_message(&e.app, &b.id, Some("t3"), "assistant", "今天會晚一點回覆你", None).await;
    // 沒有 turn 的回覆：看它前面最近一則 user（擁有者的）→ 不給。
    add_message(&e.app, &b.id, "user", "後台：再確認一次", None).await;
    add_message(&e.app, &b.id, "assistant", "確認了", None).await;
    let base = serve(portal::router(e.app.clone())).await;
    let v: Value = client().get(format!("{base}/s/{token}/api/messages")).send().await.unwrap().json().await.unwrap();
    let texts: Vec<&str> = v["messages"].as_array().unwrap().iter().map(|m| m["text"].as_str().unwrap()).collect();
    assert_eq!(texts, vec!["開場白：你好", "幫我跑測試", "測試都過了", "今天會晚一點回覆你"], "{v}");
}

/// 舊 DB 的 `shared_bots` 只收 'restricted'：開機時換成新定義、資料原樣搬過去，之後收得下 'trusted'；再跑一次不動。
#[tokio::test]
async fn the_old_shared_bots_table_is_rebuilt_to_accept_trusted() {
    let e = tt::env().await;
    let db = &e.app.db;
    sqlx::query("DROP TABLE shared_bots").execute(db).await.unwrap();
    sqlx::query(
        "CREATE TABLE shared_bots (bot_id TEXT PRIMARY KEY, profile TEXT NOT NULL CHECK (profile IN ('restricted')), workspace TEXT NOT NULL, created_at TEXT NOT NULL)",
    )
    .execute(db)
    .await
    .unwrap();
    sqlx::query("INSERT INTO shared_bots VALUES ('B-old', 'restricted', '/tmp/ws-old', '2026-10-03T00:00:00Z')").execute(db).await.unwrap();
    assert!(sqlx::query("INSERT INTO shared_bots VALUES ('B-t', 'trusted', '/tmp/ws', 'x')").execute(db).await.is_err(), "舊表收不下 trusted");
    store::migrate(db).await.unwrap();
    store::migrate(db).await.unwrap();
    let row: (String, String) = sqlx::query_as("SELECT profile, workspace FROM shared_bots WHERE bot_id = 'B-old'").fetch_one(db).await.unwrap();
    assert_eq!(row, ("restricted".into(), "/tmp/ws-old".into()));
    store::insert_share_bot(db, "B-t", store::PROFILE_TRUSTED, "/tmp/ws").await.unwrap();
    let left: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM sqlite_master WHERE name = 'shared_bots_v1'").fetch_one(db).await.unwrap();
    assert_eq!(left, 0, "暫存的舊表收掉了");
}
