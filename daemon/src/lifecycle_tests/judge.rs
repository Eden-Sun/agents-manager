#![allow(unused_imports)]

mod source_mark_tests {
    use crate::judge::*;
    use serde_json::json;

    #[test]
    fn the_badge_names_jev_the_probability_and_that_it_only_suggests() {
        assert_eq!(badge(Some(0.874)), "🤖 Jev 建議（TypeSafe 型別判斷，機率 87%，只是建議，不會自動動作）");
        assert_eq!(badge(None), "🤖 Jev 建議（TypeSafe 型別判斷，只是建議，不會自動動作）");
        assert_eq!(pct(1.7), "100%");
        assert_eq!(pct(-0.2), "0%");
    }

    #[test]
    fn mark_adds_the_source_fields_and_keeps_the_rest() {
        let p = mark(json!({"action": "x", "probability": 0.93}), Some(0.93));
        assert_eq!((p["suggested_by"].as_str(), p["source_label"].as_str(), p["suggestion_only"].as_bool(), p["probability_pct"].as_i64()), (Some("jev"), Some("Jev 建議"), Some(true), Some(93)));
        assert_eq!(p["action"], "x");
        assert!(mark(json!({}), None).get("probability_pct").is_none());
    }
}


mod tests {
    use crate::lifecycle_tests::prelude::*;
    use crate::config::JudgeCfg;
    use crate::judge::*;
    use crate::judge::test_seams::*;
    use std::sync::Mutex;
    use crate::judge::*;
    use std::sync::Arc;
    use crate::state::App;

    fn cfg(enabled: bool, projects: &[&str]) -> JudgeCfg {
        JudgeCfg { enabled, projects: projects.iter().map(|s| s.to_string()).collect(), ..JudgeCfg::default() }
    }

    #[test]
    fn both_switches_must_be_on_and_the_fuse_holds() {
        assert_eq!(gate(&JudgeCfg::default(), "P1", "agents-manager", 0), Some(Skip::Disabled));
        assert_eq!(gate(&cfg(true, &[]), "P1", "agents-manager", 0), Some(Skip::ProjectNotListed));
        assert_eq!(gate(&cfg(false, &["P1"]), "P1", "agents-manager", 0), Some(Skip::Disabled));
        assert_eq!(gate(&cfg(true, &["other"]), "P1", "agents-manager", 0), Some(Skip::ProjectNotListed));
        assert_eq!(gate(&cfg(true, &["P1"]), "P1", "x", 0), None);
        assert_eq!(gate(&cfg(true, &["agents-manager"]), "P1", "agents-manager", 59), None);
        assert_eq!(gate(&cfg(true, &["agents-manager"]), "P1", "agents-manager", 60), Some(Skip::Fuse));
    }

    #[test]
    fn wildcard_projects_include_unlisted_projects_but_still_require_enabled() {
        assert_eq!(gate(&cfg(true, &["*"]), "P2", "new project", 0), None);
        assert_eq!(gate(&cfg(false, &["*"]), "P2", "new project", 0), Some(Skip::Disabled));
    }

    #[test]
    fn secrets_are_masked_and_layout_is_kept() {
        let m = |s: &str| mask(s);
        assert_eq!(m("  export GITHUB_TOKEN=ghp_abcdefghijklmnopqrstuvwxyz0123456789"), format!("  export GITHUB_TOKEN={REDACTED}"));
        assert_eq!(m("key sk-ant-REDACTED done"), format!("key {REDACTED} done"));
        assert_eq!(m("Authorization: Bearer abc.def.ghi"), format!("Authorization: Bearer {REDACTED}"));
        assert_eq!(m("db_password: hunter2 # x"), format!("db_password: {REDACTED} # x"));
        assert_eq!(m("DB_PASSWORD=hunter2"), format!("DB_PASSWORD={REDACTED}"));
        assert_eq!(m("mail someone@example.com now"), format!("mail {REDACTED} now"));
        assert_eq!(m("  gpt-5.6-luna xhigh · /Users/me/project/agents-manager"), "  gpt-5.6-luna xhigh · ~/project/agents-manager");
        assert_eq!(m("  gpt-6-luna max · /home/user/project/agents-manager"), "  gpt-6-luna max · ~/project/agents-manager");
        assert_eq!(m("cd:/home/user"), "cd:~");
        assert_eq!(m("see https://example.com/home/feed"), "see https://example.com/home/feed", "網址的 /home/ 不是家目錄");
        assert_eq!(m("token 9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"), format!("token {REDACTED}"));
        assert_eq!(m("a\n-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaA\n-----END OPENSSH PRIVATE KEY-----\nz"), format!("a\n{REDACTED}\nz"));
        let banner = "■ You've hit your usage limit. Upgrade to Pro (https://chatgpt.com/explore/pro), or try again at 3:22 AM.";
        assert_eq!(m(banner), banner);
        let code = r#"    low.contains("you hit your weekly limit") || low.contains("you've hit your weekly limit")"#;
        assert_eq!(m(code), code);
        assert_eq!(m("@mention and a_long_snake_case_identifier_that_is_not_a_secret_0123"), "@mention and a_long_snake_case_identifier_that_is_not_a_secret_0123");
    }

    /// issue #451：JSON／引號形式的 header。`"Bearer` 前面黏著雙引號，原本 `== "BEARER"` 不成立，
    /// 於是 `secret_next` 是 false；JWT 又因為有 `.` 被 `looks_random` 的字元集擋在門外，整條原樣送出去。
    ///
    /// 這裡的字串全是假的（header 是 `{"alg":"HS256"}`、payload 是 `{"sub":"1"}`，簽章隨便湊的）。
    #[test]
    fn json_shaped_bearer_and_bare_jwts_are_masked() {
        let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.abcdefghijklmnopqrstuvwxyz012345";
        let line = format!(r#"{{"Authorization": "Bearer {jwt}"}}"#);
        let out = mask(&line);
        assert!(!out.contains("eyJzdWIiOiIxIn0"), "JSON 形式的 Bearer 後面那條也要遮掉：{out}");
        assert!(!out.contains(jwt), "{out}");
        assert!(out.contains("Bearer"), "{out}");

        assert_eq!(mask(&format!("token={jwt}")), format!("token={REDACTED}"));
        assert_eq!(mask(jwt), REDACTED);
        assert_eq!(mask(&format!("  header: {jwt} ok")), format!("  header: {REDACTED} ok"));

        assert_eq!(mask(&format!("Authorization: Bearer {jwt}")), format!("Authorization: Bearer {REDACTED}"));

        let out = mask(r#"{"authorization": "abcdefghijklmnopqrstuvwxyz0123456789"}"#);
        assert!(!out.contains("abcdefghijklmnopqrstuvwxyz0123456789"), "{out}");
        assert!(mask(r#"cookie: "sess_abcdefghijklmnopqrstuvwxyz01234567""#).contains(REDACTED));
        assert_eq!(mask("Authorization: Token ghp_abcdefghijklmnopqrstuvwxyz0123"), format!("Authorization: Token {REDACTED}"));
    }

    /// issue #451 的跟進審核（i407）：四個反向缺口。字串全是捏造的。
    #[test]
    fn bare_schemes_query_strings_userinfo_and_jwe_are_masked_too() {
        assert_eq!(mask("Basic dXNlcjpwYXNzd29yZA=="), format!("Basic {REDACTED}"));
        assert_eq!(mask("Token ghp_abcdefghijklmnopqrstuvwxyz0123"), format!("Token {REDACTED}"));
        assert_eq!(mask("Digest username=admin"), format!("Digest {REDACTED}"));

        assert_eq!(
            mask("https://h.example.invalid/api?token=abcdef123456&page=2"),
            format!("https://h.example.invalid/api?token={REDACTED}&page=2")
        );
        assert_eq!(mask("curl 'https://x.invalid/v1?api_key=k-abc123&q=1'"), format!("curl 'https://x.invalid/v1?api_key={REDACTED}&q=1'"));
        assert_eq!(mask("https://x.invalid/p?key=abc&sig=def"), format!("https://x.invalid/p?key={REDACTED}&sig={REDACTED}"));
        let plain = "https://x.invalid/p?page=2&sort=name";
        assert_eq!(mask(plain), plain);

        assert_eq!(mask("http://u:p@localhost:8080/path"), REDACTED);
        assert_eq!(mask("mail someone@example.com now"), format!("mail {REDACTED} now"));

        let jwe = "eyJhbGciOiJSU0EtT0FFUCJ9.abcdefgh.ijklmnop.qrstuvwxyz012345.tag12345";
        assert_eq!(mask(jwe), REDACTED);
        assert_eq!(mask("eyAiYWxnIjoiSFMyNTYifQ.eyJzdWIiOiIxIn0.sig12345"), REDACTED);

        let jwt = "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxIn0.abcdefghijklmnopqrstuvwxyz012345";
        let out = mask(&format!(r#"{{"Authorization": "Bearer {jwt}"}}"#));
        assert!(out.ends_with(r#""}"#), "版面要留著：{out}");
        assert!(!out.contains(jwt), "{out}");
    }

    /// 遮罩只能吃 token，不能吃版面：有 `.` 的一般字串（版本、路徑、檔名、句子）照樣原樣通過。
    /// 這條是 [`looks_jwt`] 的反面——當初沒有把 `.` 加進 `looks_random` 的允許集合就是為了這些。
    #[test]
    fn dotted_words_that_are_not_jwts_survive() {
        for s in [
            "gpt-5.6-luna",
            "v1.22.333-rc.1",
            "daemon/src/judge.rs:346",
            "web/src/api/index.ts",
            "eyJhbGci.short.x",
            "api.github.com",
            "0.93",
        ] {
            assert_eq!(mask(s), s, "{s} 不是 JWT，不該被遮掉");
        }
    }

    #[test]
    fn only_the_tail_of_the_screen_leaves_the_machine() {
        let screen: String = (0..200).map(|i| format!("line {i}\n")).collect();
        let t = tail(&screen);
        assert!(t.starts_with("line 140"), "{}", &t[..20]);
        assert!(t.ends_with("line 199"));
        let wide: String = (0..60).map(|_| "x".repeat(500) + "\n").collect();
        assert_eq!(tail(&wide).chars().count(), TAIL_CHARS);
    }

    #[test]
    fn the_request_carries_no_bot_or_project_identity() {
        let body = request_body("jev-1.13.0", "grok", "You hit your weekly limit.", "screen", true);
        let state = body["state"].as_object().unwrap();
        let mut keys: Vec<_> = state.keys().cloned().collect();
        keys.sort();
        assert_eq!(keys, ["agent", "composer_idle", "needle", "screen"]);
        assert_eq!(body["model"], "jev-1.13.0");
    }

    #[cfg(unix)]
    #[test]
    fn a_key_file_others_can_read_is_refused() {
        use std::os::unix::fs::PermissionsExt;
        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-judge-{}", crate::db::ulid())));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("key");
        std::fs::write(&path, "k-test\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let err = read_key(path.to_str().unwrap()).unwrap_err().to_string();
        assert!(err.contains("chmod 600"), "{err}");
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(read_key(path.to_str().unwrap()).unwrap(), "k-test");
        assert!(read_key(dir.join("absent").to_str().unwrap()).unwrap_err().to_string().contains("unreadable"));
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 假的 Jev：數被打了幾次、記下收到的 body 與 Authorization。
    async fn fake_jev(status: u16) -> (String, Arc<std::sync::Mutex<Vec<(String, Value)>>>) {
        use axum::http::{HeaderMap, StatusCode};
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = seen.clone();
        let route = axum::routing::post(move |headers: HeaderMap, axum::Json(body): axum::Json<Value>| {
            let log = log.clone();
            async move {
                let auth = headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or_default().to_string();
                log.lock().unwrap().push((auth, body));
                (StatusCode::from_u16(status).unwrap(), axum::Json(json!({"model": "jev-1.13.0", "answers": {"is_live_ui": {"type": "noul", "noul": 0.07}}, "usage": {"input_tokens": 812}})))
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/systemone", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, axum::Router::new().route("/v1/systemone", route)).await.unwrap() });
        (url, seen)
    }

    async fn app_with(enabled: bool, projects: &[&str], endpoint: &str) -> (Arc<App>, std::path::PathBuf) {
        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-judge-{}", crate::db::ulid())));
        std::fs::create_dir_all(&dir).unwrap();
        let key = dir.join("key");
        std::fs::write(&key, "k-test-0001\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let db = crate::app_ports_p1::open(&dir.join("t.sqlite3")).await.unwrap();
        let store = crate::runners::app_ports_p2::load_config(dir.join("config.toml")).await.unwrap();
        let judge = JudgeCfg { key_file: key.to_string_lossy().into_owned(), endpoint: endpoint.into(), ..cfg(enabled, projects) };
        store
            .update(move |c| {
                c.judge = judge;
                Ok(())
            })
            .await
            .unwrap();
        let client = crate::herdr::HerdrClient::new(dir.join("absent.sock"));
        (App::new(db, client.clone(), client, store, dir.clone(), dir.join("daemon"), 7799, "test".into(), "test".into(), false), dir)
    }

    fn sample() -> Sample {
        Sample {
            bot_id: "B1".into(),
            run_id: "R1".into(),
            project_id: "P1".into(),
            kind: "grok".into(),
            matched_line: "low.contains(\"you hit your weekly limit\") // ghp_abcdefghijklmnopqrstuvwxyz0123456789".into(),
            screen: "  ◆ Run: rg weekly /Users/me/project\n    screen.rs:458: low.contains(\"you hit your weekly limit\")\n❯\n".into(),
        }
    }

    async fn rows(app: &impl crate::capabilities::Db) -> Vec<(String, Option<f64>, Option<String>, Option<String>, bool)> {
        sqlx::query_as("SELECT matched_line, jev_is_live_ui, error, cleared_at, composer_idle FROM judge_shadow").fetch_all(app.db()).await.unwrap()
    }

    /// #481：保險絲在**並行**時也要是真的上限。
    ///
    /// `shadow_limit_hit` 是 `tokio::spawn` 出去的，所以 K 顆 bot 同時撞限就是 K 個並行的
    /// `observe`。改成「先占位再問」之前，它們會在第一筆寫進去之前都讀到同一個數字、一起放行；
    /// 這條測試就是要釘住「不管同時來幾個，放行的就是 `max_per_hour` 個」。
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_asks_never_exceed_the_hourly_fuse() {
        let (url, seen) = fake_jev(200).await;
        let (app, dir) = app_with(true, &["P1"], &url).await;
        const CAP: u32 = 3;
        const CONCURRENT: usize = 12;
        app.cfg
            .update(|c| {
                c.judge.max_per_hour = CAP;
                Ok(())
            })
            .await
            .unwrap();

        let mut tasks = Vec::new();
        for i in 0..CONCURRENT {
            let app = app.clone();
            let mut s = sample();
            s.run_id = format!("R{i}");
            tasks.push(tokio::spawn(async move { observe(&app, s).await.is_ok() }));
        }
        let mut passed = 0usize;
        for t in tasks {
            if t.await.unwrap() {
                passed += 1;
            }
        }

        assert_eq!(passed as u32, CAP, "同時來 {CONCURRENT} 個，放行的必須剛好是上限 {CAP} 個");
        assert_eq!(seen.lock().unwrap().len() as u32, CAP, "真的送出去的次數也要等於上限");
        let written: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM judge_shadow").fetch_one(&app.db).await.unwrap();
        assert_eq!(written as u32, CAP, "帳本上就是那幾筆，沒有多寫也沒有少寫");
        let pending: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM judge_shadow WHERE error = 'pending'").fetch_one(&app.db).await.unwrap();
        assert_eq!(pending, 0, "問完了就要把占位列補上答案，不能留著 pending");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn an_answer_is_recorded_and_later_settled() {
        let (url, seen) = fake_jev(200).await;
        let (app, dir) = app_with(true, &["P1"], &url).await;
        observe(&app, sample()).await.unwrap();
        let got = rows(&app).await;
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].1, Some(0.07));
        assert!(got[0].2.is_none() && got[0].3.is_none() && got[0].4, "{got:?}");
        assert!(!got[0].0.contains("ghp_"), "命中行存進帳本前也要遮罩：{}", got[0].0);
        let calls = seen.lock().unwrap().clone();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].0, "Bearer k-test-0001");
        let sent = calls[0].1.to_string();
        assert!(!sent.contains("ghp_") && !sent.contains("/Users/me") && !sent.contains("B1") && !sent.contains("P1"), "{sent}");
        assert!(!app.cfg.get().await.judge.key_file.contains("k-test"), "設定裡只有路徑");

        note_cleared(&app.db, "other").await;
        assert!(rows(&app).await[0].3.is_none());
        note_cleared(&app.db, "B1").await;
        assert!(rows(&app).await[0].3.is_some());
        std::fs::remove_dir_all(&dir).ok();
    }

    /// issue #453：`cleared_at` 的語意是「**撞限**之後被成功回合清掉」，所以那句 UPDATE 只能蓋
    /// 撞限那一類、而且是近期的。以前沒有任何條件，於是一次成功回合會順手：
    /// (1) 把 `stuck_queued`（跟撞限無關）也蓋上時刻；(2) 把幾天前那筆「從未被清掉」追認成「剛剛清掉」。
    #[tokio::test]
    async fn only_recent_limit_hits_are_settled_by_a_clear() {
        let (app, dir) = app_with(false, &[], "http://127.0.0.1:1/x").await;
        let put = |id: &str, verdict: &str, at: String, line: &str| {
            let (app, id, verdict, line) = (app.clone(), id.to_string(), verdict.to_string(), line.to_string());
            async move {
                sqlx::query(
                    "INSERT INTO judge_shadow (id, at, bot_id, run_id, kind, matched_line, composer_idle, regex_verdict)
                     VALUES (?, ?, 'B1', 'R1', 'claude', ?, 1, ?)",
                )
                .bind(&id)
                .bind(&at)
                .bind(&line)
                .bind(&verdict)
                .execute(&app.db)
                .await
                .unwrap();
            }
        };
        let cleared = |id: &str| {
            let (app, id) = (app.clone(), id.to_string());
            async move {
                sqlx::query_scalar::<_, Option<String>>("SELECT cleared_at FROM judge_shadow WHERE id = ?")
                    .bind(&id)
                    .fetch_one(&app.db)
                    .await
                    .unwrap()
            }
        };
        let hours_ago = |h: i64| crate::db::iso_at(chrono::Utc::now() - chrono::Duration::hours(h));

        put("fresh", "limit_hit", hours_ago(1), "You've hit your usage limit").await;
        put("stale", "limit_hit", hours_ago(CLEAR_WINDOW_HOURS + 1), "You've hit your usage limit").await;
        put("stuck", "stuck_queued", hours_ago(1), "x").await;
        put("weekly", "limit_hit", hours_ago(24 * 3), "You hit your weekly limit.").await;
        put("weekly_old", "limit_hit", hours_ago(WEEKLY_CLEAR_WINDOW_HOURS + 1), "You hit your weekly limit.").await;

        note_cleared(&app.db, "B1").await;

        assert!(cleared("fresh").await.is_some(), "近期的撞限才是這次清掉的那一筆");
        assert!(cleared("stale").await.is_none(), "幾天前沒被清掉的撞限要維持 NULL，不是追認成剛剛清掉");
        assert!(cleared("stuck").await.is_none(), "stuck_queued 跟撞限無關，cleared_at 對它沒有意義");
        assert!(cleared("weekly").await.is_some(), "三天前的週限就是這次清掉的：週限本來就可能隔幾天才恢復");
        assert!(cleared("weekly_old").await.is_none(), "連週限的窗都過了：那是從未被清掉");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn nothing_leaves_the_machine_unless_both_switches_are_on() {
        let (url, seen) = fake_jev(200).await;
        for (enabled, projects) in [(false, vec!["P1"]), (true, vec![]), (true, vec!["someone-else"])] {
            let (app, dir) = app_with(enabled, &projects, &url).await;
            let _ = observe(&app, sample()).await;
            assert!(rows(&app).await.is_empty());
            std::fs::remove_dir_all(&dir).ok();
        }
        assert!(seen.lock().unwrap().is_empty(), "關著還是打了 API");
    }

    #[tokio::test]
    async fn a_failing_service_is_one_error_row_and_no_retry() {
        let (url, seen) = fake_jev(429).await;
        let (app, dir) = app_with(true, &["P1"], &url).await;
        observe(&app, sample()).await.unwrap();
        let got = rows(&app).await;
        assert_eq!((got[0].1, got[0].2.as_deref()), (None, Some("http 429")));
        assert_eq!(seen.lock().unwrap().len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[cfg(unix)]
    #[test]
    fn a_pasted_key_lands_in_a_600_file_and_nowhere_else() {
        use std::os::unix::fs::PermissionsExt;
        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-judge-{}", crate::db::ulid())));
        let path = dir.join("nested").join("api-key");
        let p = path.to_str().unwrap();
        assert!(key_status(p).unwrap_err().contains("unreadable"));
        write_key(p, "  k-test-0002\n").unwrap();
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(read_key(p).unwrap(), "k-test-0002");
        write_key(p, "k-test-0003").unwrap();
        assert!(write_key(p, "two words").is_err() && write_key(p, " ").is_err());
        assert_eq!(read_key(p).unwrap(), "k-test-0003");
        assert_eq!(std::fs::read_dir(path.parent().unwrap()).unwrap().count(), 1, "暫存檔沒收乾淨");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn the_settings_page_can_switch_it_on_only_with_a_key_and_never_reads_the_key_back() {
        use axum::extract::State;
        let (app, dir) = app_with(false, &[], "http://127.0.0.1:9/unused").await;
        std::fs::remove_file(dir.join("key")).unwrap();
        let put = |enabled, projects: Option<Vec<&str>>, token: Option<&str>| {
            let body = crate::runners::judge::SettingsBody { enabled, projects: projects.map(|p| p.iter().map(|s| s.to_string()).collect()), token: token.map(str::to_string) };
            crate::runners::judge::put_settings(State(app.clone()), axum::Json(body))
        };
        let refused = put(Some(true), None, None).await;
        assert!(matches!(refused, Err(crate::lifecycle::LcError::Conflict(ref v)) if v["error"] == "needs_key"), "沒 key 不給開");
        assert!(!app.cfg.get().await.judge.enabled);

        let saved = put(Some(true), Some(vec![" agents-manager ", "agents-manager", ""]), Some("k-test-0004")).await.unwrap().0;
        assert_eq!(saved["enabled"], true);
        assert_eq!(saved["projects"], json!(["agents-manager"]));
        assert_eq!(saved["key_present"], true);
        assert!(!saved.to_string().contains("k-test-0004"));
        assert!(!crate::runners::judge::get_settings(State(app.clone())).await.0.to_string().contains("k-test-0004"));
        assert!(!std::fs::read_to_string(dir.join("config.toml")).unwrap().contains("k-test-0004"), "key 不進 config.toml");

        let off = put(Some(false), None, Some("  ")).await.unwrap().0;
        assert_eq!((off["enabled"].clone(), off["key_present"].clone()), (json!(false), json!(true)));
        assert_eq!(read_key(dir.join("key").to_str().unwrap()).unwrap(), "k-test-0004");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[tokio::test]
    async fn a_failed_config_update_leaves_the_old_key_live_and_no_staged_file() {
        use axum::extract::State;
        let (app, dir) = app_with(false, &[], "http://127.0.0.1:9/unused").await;
        let key = dir.join("key");
        let keys = key.to_str().unwrap();
        assert_eq!(read_key(keys).unwrap(), "k-test-0001");
        std::fs::write(dir.join("config.toml"), "this is = = not toml").unwrap();
        let f = std::fs::OpenOptions::new().write(true).open(dir.join("config.toml")).unwrap();
        f.set_modified(std::time::SystemTime::now() + std::time::Duration::from_secs(60)).unwrap();
        let body = crate::runners::judge::SettingsBody { enabled: Some(true), projects: Some(vec!["p".into()]), token: Some("k-test-0009".into()) };
        let err = crate::runners::judge::put_settings(State(app.clone()), axum::Json(body)).await;
        assert!(matches!(err, Err(crate::lifecycle::LcError::Upstream(_))), "config 失敗要回錯");
        assert_eq!(read_key(keys).unwrap(), "k-test-0001", "config 失敗不可讓新 key 生效");
        assert_eq!(std::fs::read_dir(&dir).unwrap().filter(|e| e.as_ref().unwrap().file_name().to_string_lossy().ends_with(".tmp")).count(), 0, "暫存 key 要清掉");
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn a_staged_key_is_inert_until_published() {
        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-judge-{}", crate::db::ulid())));
        let path = dir.join("api-key");
        let p = path.to_str().unwrap();
        write_key(p, "k-old").unwrap();
        drop(stage_key(p, "k-new").unwrap());
        assert_eq!(read_key(p).unwrap(), "k-old");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1, "沒 publish 的暫存檔要刪");
        assert!(stage_key(p, "two words").is_err());
        let st = stage_key(p, "k-new").unwrap();
        assert_eq!(read_key(p).unwrap(), "k-old");
        assert_eq!(read_key(&st.staged_path().to_string_lossy()).unwrap(), "k-new");
        st.publish().unwrap();
        assert_eq!(read_key(p).unwrap(), "k-new");
        std::fs::remove_dir_all(&dir).ok();
    }

}

pub mod ask_option {
mod tests {
    use crate::lifecycle_tests::prelude::*;
    use crate::judge::ask_option::*;
    use std::sync::Mutex;
    use crate::db;
    use crate::state::App;
    use crate::testing as tt;
    use axum::extract::{Path, State};
    use axum::http::StatusCode;
    use std::collections::HashMap;

    #[derive(Clone)]
    enum Mode {
        /// 依 key 回分數，沒列的 0.05。
        Scores(HashMap<String, f64>),
        Status(u16),
        Gate(Arc<tokio::sync::Notify>, Arc<tokio::sync::Notify>),
    }

    async fn fake_jev(mode: Mode) -> (String, Arc<Mutex<Vec<Value>>>) {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let log = seen.clone();
        let route = axum::routing::post(move |axum::Json(body): axum::Json<Value>| {
            let (log, mode) = (log.clone(), mode.clone());
            async move {
                log.lock().unwrap().push(body.clone());
                match mode {
                    Mode::Gate(entered, release) => {
                        entered.notify_one();
                        release.notified().await;
                        (StatusCode::OK, axum::Json(json!({"answers": {"q1_a": {"noul": 0.95}, "q1_b": {"noul": 0.05}}})))
                    }
                    Mode::Status(code) => (StatusCode::from_u16(code).unwrap(), axum::Json(json!({"error": "no"}))),
                    Mode::Scores(by_key) => {
                        let mut answers = serde_json::Map::new();
                        for k in body["questions"].as_object().unwrap().keys() {
                            answers.insert(k.clone(), json!({"type": "noul", "noul": by_key.get(k).copied().unwrap_or(0.05)}));
                        }
                        (StatusCode::OK, axum::Json(json!({"model": "jev-1.13.0", "answers": answers, "usage": {"input_tokens": 321}})))
                    }
                }
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/systemone", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, axum::Router::new().route("/v1/systemone", route)).await.unwrap() });
        (url, seen)
    }

    struct Rig {
        env: tt::Env,
        dir: std::path::PathBuf,
        seen: Arc<Mutex<Vec<Value>>>,
        bot: db::Bot,
        run_id: String,
        transcript: std::path::PathBuf,
    }

    impl Drop for Rig {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.dir).ok();
        }
    }

    fn scores(pairs: &[(&str, f64)]) -> Mode {
        Mode::Scores(pairs.iter().map(|(k, v)| (k.to_string(), *v)).collect())
    }

    /// 開著、專案在名單、有 key、一顆 claude bot 有 run 與 transcript、一則在飛的使用者 prompt。
    async fn stand(mode: Mode, enabled: bool, listed: bool) -> Rig {
        let env = tt::env().await;
        let app = env.app.clone();
        let (url, seen) = fake_jev(mode).await;
        let dir = tt::track(std::env::temp_dir().join(format!("am-judge-ask-option-{}", db::ulid())));
        std::fs::create_dir_all(&dir).unwrap();
        let key = dir.join("key");
        std::fs::write(&key, "k-test\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let pid = if listed { env.project_id.clone() } else { "not-this-project".into() };
        let key_s = key.to_string_lossy().into_owned();
        app.cfg
            .update(move |c| {
                c.judge.enabled = enabled;
                c.judge.projects = vec![pid];
                c.judge.key_file = key_s;
                c.judge.endpoint = url;
                c.judge.timeout_ms = 2000;
                Ok(())
            })
            .await
            .unwrap();
        let bot = tt::claude_bot(&app, &env.project_id, "asker").await;
        let cfg_dir = dir.join("claude");
        sqlx::query("UPDATE bots SET env_json=? WHERE id=?")
            .bind(json!({"CLAUDE_CONFIG_DIR": cfg_dir.to_string_lossy()}).to_string())
            .bind(&bot.id)
            .execute(&app.db)
            .await
            .unwrap();
        let run_id = tt::fake_run(&app, &bot.id).await;
        let transcript = cfg_dir.join("projects/-work/s.jsonl");
        std::fs::create_dir_all(transcript.parent().unwrap()).unwrap();
        sqlx::query("UPDATE runs SET transcript_path=? WHERE id=?").bind(transcript.to_string_lossy()).bind(&run_id).execute(&app.db).await.unwrap();
        let conv = db::conversation_id(&app.db, &bot.id).await.unwrap();
        sqlx::query("INSERT INTO turns (id, conversation_id, run_id, origin, status, delivery, prompt_text, created_at) VALUES (?,?,?,'web','in_flight','ok',?,?)")
            .bind(db::ulid())
            .bind(&conv)
            .bind(&run_id)
            .bind("部署 prod console，連線一律走內網，不要開公網。 GITHUB_TOKEN=ghp_abcdefghijklmnopqrstuvwxyz0123456789")
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();
        let bot = db::bot(&app.db, &bot.id).await.unwrap().unwrap();
        Rig { env, dir, seen, bot, run_id, transcript }
    }

    fn two_options() -> Value {
        json!([{"question": "連線方式？", "header": "連線", "multiSelect": false,
                "options": [{"label": "內網 http", "description": "流量留在 VPC"}, {"label": "公網 https", "description": "拿掉白名單"}]}])
    }

    fn write_ask(rig: &Rig, id: &str, questions: Value) {
        let line = json!({"type": "assistant", "message": {"content": [{"type": "tool_use", "id": id, "name": "AskUserQuestion", "input": {"questions": questions}}]}});
        std::fs::write(&rig.transcript, format!("{line}\n")).unwrap();
    }

    async fn get(rig: &Rig) -> Value {
        let axum::Json(body) = crate::runners::pending_question::get_pending_question(State(rig.env.app.clone()), Path(rig.bot.id.clone())).await.unwrap();
        body
    }

    async fn ledger(app: &Arc<App>) -> Vec<(String, Option<String>, Option<f64>)> {
        sqlx::query_as("SELECT matched_line, error, jev_is_live_ui FROM judge_shadow WHERE regex_verdict='ask_option' ORDER BY at, id")
            .fetch_all(&app.db)
            .await
            .unwrap()
    }

    /// 等答案落帳那則事件（不靠 sleep）。
    async fn hint_event(rx: &mut tokio::sync::broadcast::Receiver<crate::state::WsEvent>) -> Value {
        tokio::time::timeout(std::time::Duration::from_secs(10), async {
            loop {
                let ev = rx.recv().await.unwrap();
                if ev.kind == EVENT {
                    return ev.data;
                }
            }
        })
        .await
        .expect("答案落帳後要發事件")
    }

    #[tokio::test]
    async fn a_stored_hint_is_not_served_to_a_later_turn() {
        let rig = stand(scores(&[("q1_a", 0.95)]), true, true).await;
        write_ask(&rig, "toolu_1", two_options());
        assert!(ask(&rig.env.app, &rig.bot.id, &rig.run_id, "toolu_1", &json!({"questions": two_options()})).await.unwrap());
        let conv = db::conversation_id(&rig.env.app.db, &rig.bot.id).await.unwrap();
        sqlx::query("UPDATE turns SET status='completed'").execute(&rig.env.app.db).await.unwrap();
        sqlx::query("INSERT INTO turns (id,conversation_id,run_id,origin,status,delivery,prompt_text,created_at) VALUES (?,?,?,'web','in_flight','ok','LATER PROMPT','9999')")
            .bind(db::ulid()).bind(conv).bind(&rig.run_id).execute(&rig.env.app.db).await.unwrap();
        assert_eq!(get(&rig).await["jev_options"], Value::Null);
        assert_eq!(rig.seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_background_question_cannot_follow_a_later_turn_before_send() {
        let rig = stand(scores(&[("q1_a", 0.95)]), true, true).await;
        write_ask(&rig, "toolu_1", two_options());
        let gate = rig.env.app.judge_fuse.lock().await;
        assert_eq!(get(&rig).await["jev_options"], Value::Null);
        rig.env.app.background_tasks.close();
        sqlx::query("UPDATE turns SET status='completed'").execute(&rig.env.app.db).await.unwrap();
        let conv = db::conversation_id(&rig.env.app.db, &rig.bot.id).await.unwrap();
        sqlx::query("INSERT INTO turns (id,conversation_id,run_id,origin,status,delivery,prompt_text,created_at) VALUES (?,?,?,'web','in_flight','ok','LATER PROMPT','9999')")
            .bind(db::ulid()).bind(conv).bind(&rig.run_id).execute(&rig.env.app.db).await.unwrap();
        drop(gate);
        rig.env.app.background_tasks.wait().await;
        assert!(rig.seen.lock().unwrap().is_empty(), "later turn must not be sent for the old question");
        assert!(matches!(lookup(&rig.env.app, &rig.run_id, "toolu_1").await.unwrap(), Lookup::Never | Lookup::NoHint));
        for row in ledger(&rig.env.app).await { assert_eq!(row.1.as_deref(), Some("stale")); }
    }

    #[tokio::test]
    async fn an_old_run_never_borrows_the_new_runs_prompt() {
        let rig = stand(scores(&[("q1_a", 0.95)]), true, true).await;
        write_ask(&rig, "toolu_1", two_options());
        sqlx::query("UPDATE runs SET state='exited', ended_at='ended' WHERE id=?").bind(&rig.run_id).execute(&rig.env.app.db).await.unwrap();
        let next = tt::fake_run(&rig.env.app, &rig.bot.id).await;
        let conv = db::conversation_id(&rig.env.app.db, &rig.bot.id).await.unwrap();
        sqlx::query("INSERT INTO turns (id,conversation_id,run_id,origin,status,delivery,prompt_text,created_at) VALUES (?,?,?,'web','in_flight','ok','R2 PROMPT','9999')")
            .bind(db::ulid()).bind(conv).bind(next).execute(&rig.env.app.db).await.unwrap();
        assert!(!ask(&rig.env.app, &rig.bot.id, &rig.run_id, "toolu_1", &json!({"questions": two_options()})).await.unwrap());
        assert!(rig.seen.lock().unwrap().is_empty());
        assert!(ledger(&rig.env.app).await.is_empty());
    }

    #[tokio::test]
    async fn stale_questions_never_publish_after_the_http_gate() {
        for boundary in ["answered", "turn", "run", "handoff"] {
            let entered = Arc::new(tokio::sync::Notify::new());
            let release = Arc::new(tokio::sync::Notify::new());
            let rig = stand(Mode::Gate(entered.clone(), release.clone()), true, true).await;
            write_ask(&rig, "toolu_1", two_options());
            let mut rx = rig.env.app.subscribe();
            let app = rig.env.app.clone();
            let bot = rig.bot.id.clone();
            let run = rig.run_id.clone();
            let task = tokio::spawn(Box::pin(async move {
                ask(&app, &bot, &run, "toolu_1", &json!({"questions": two_options()})).await.unwrap()
            }));
            entered.notified().await;
            match boundary {
                "answered" => {
                    let mut log = std::fs::read_to_string(&rig.transcript).unwrap();
                    log.push_str(&json!({"message":{"content":[{"type":"tool_result","tool_use_id":"toolu_1"}]}}).to_string());
                    std::fs::write(&rig.transcript, log).unwrap();
                }
                "turn" => {
                    sqlx::query("UPDATE turns SET status='completed'").execute(&rig.env.app.db).await.unwrap();
                    let conv = db::conversation_id(&rig.env.app.db, &rig.bot.id).await.unwrap();
                    sqlx::query("INSERT INTO turns (id,conversation_id,run_id,origin,status,delivery,prompt_text,created_at) VALUES (?,?,?,'web','in_flight','ok','NEW PROMPT','9999')")
                        .bind(db::ulid()).bind(conv).bind(&rig.run_id).execute(&rig.env.app.db).await.unwrap();
                }
                "run" => {
                    sqlx::query("UPDATE runs SET state='exited', ended_at='ended' WHERE id=?").bind(&rig.run_id).execute(&rig.env.app.db).await.unwrap();
                    tt::fake_run(&rig.env.app, &rig.bot.id).await;
                }
                "handoff" => { sqlx::query("UPDATE projects SET handed_off_to='remote' WHERE id=?").bind(&rig.env.project_id).execute(&rig.env.app.db).await.unwrap(); }
                _ => unreachable!(),
            }
            release.notify_one();
            assert!(!task.await.unwrap(), "{boundary}: stale answer must be rejected");
            assert_eq!(lookup(&rig.env.app, &rig.run_id, "toolu_1").await.unwrap(), Lookup::NoHint, "{boundary}");
            assert_eq!(ledger(&rig.env.app).await[0].1.as_deref(), Some("stale"), "{boundary}");
            assert!(rig.seen.lock().unwrap()[0]["state"]["user_prompt"].as_str().unwrap().contains("連線一律走內網"));
            while let Ok(event) = rx.try_recv() { assert_ne!(event.kind, EVENT, "{boundary}"); }
        }
    }

    /// 有明顯建議：第一次讀不等 Jev（`jev_options` 為 null、題目照給）、答案回來發事件、之後的讀帶出建議選項與分數；
    /// 同一個 `tool_use` 不再問；請求是一個、每個選項一題；prompt 與題目已遮罩、不帶 bot 識別；不按任何鍵。
    #[tokio::test]
    async fn a_clear_favourite_is_served_by_the_api_after_one_background_request() {
        let rig = stand(scores(&[("q1_a", 0.91), ("q1_b", 0.08)]), true, true).await;
        let mut rx = rig.env.app.subscribe();
        write_ask(&rig, "toolu_1", two_options());

        let first = get(&rig).await;
        assert_eq!(first["questions"][0]["question"], "連線方式？", "題目照常顯示，不等 Jev");
        assert_eq!(first["jev_options"], Value::Null, "第一次讀的時候答案還沒回來");
        let ev = hint_event(&mut rx).await;
        assert_eq!(ev, json!({"bot_id": rig.bot.id, "run_id": rig.run_id, "tool_use_id": "toolu_1"}));

        let second = get(&rig).await;
        assert_eq!(second["questions"], first["questions"]);
        let hint = &second["jev_options"];
        assert_eq!((hint["suggested_by"].as_str(), hint["source_label"].as_str(), hint["suggestion_only"].as_bool()), (Some("jev"), Some("Jev 建議"), Some(true)));
        assert!(hint["note"].as_str().unwrap().contains("只是建議，不會自動動作"));
        assert_eq!(hint["threshold"], 0.7);
        assert_eq!(hint["margin"], 0.15);
        assert_eq!(hint["model"], "jev-1.13.0");
        assert_eq!(hint["questions"], json!([{"index": 0, "scores": [0.91, 0.08], "suggested": 0}]));
        let _ = get(&rig).await;

        let sent = rig.seen.lock().unwrap().clone();
        assert_eq!(sent.len(), 1, "同一個 tool_use 只問一次");
        let mut keys: Vec<_> = sent[0]["questions"].as_object().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(keys, ["q1_a", "q1_b"], "一個請求、每個選項一題");
        let text = sent[0].to_string();
        assert!(text.contains("連線一律走內網") && text.contains("流量留在 VPC"), "prompt 與選項說明有送出：{text}");
        assert!(!text.contains("ghp_abcdefghijklmnopqrstuvwxyz0123456789"), "prompt 走既有的遮罩");
        assert!(!text.contains(&rig.bot.id) && !text.contains(&rig.env.project_id), "不送 bot／專案識別");

        let rows = ledger(&rig.env.app).await;
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].1, None, "沒有錯誤");
        assert_eq!(rows[0].2, None, "不寫進 jev_is_live_ui");
        let stored: Value = serde_json::from_str(&rows[0].0).unwrap();
        assert_eq!(stored["tool_use_id"], "toolu_1");
        assert_eq!(stored["questions"][0]["suggested"], 0);
        assert!(!rows[0].0.contains("內網") && !rows[0].0.contains("連線方式"), "帳本不存題目與選項原文");

        assert!(rig.env.herdr.calls_to("pane.send_keys").is_empty() && rig.env.herdr.calls_to("pane.send_text").is_empty());
        let turns: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM turns").fetch_one(&rig.env.app.db).await.unwrap();
        assert_eq!(turns, 1, "沒有替使用者送出任何東西");
    }

    /// 分數接近或低於門檻：分數照存、不標建議。
    #[tokio::test]
    async fn close_or_low_scores_carry_no_suggestion() {
        for (a, b) in [(0.85, 0.80), (0.6, 0.1), (0.7, 0.56)] {
            let rig = stand(scores(&[("q1_a", a), ("q1_b", b)]), true, true).await;
            write_ask(&rig, "toolu_1", two_options());
            assert!(ask(&rig.env.app, &rig.bot.id, &rig.run_id, "toolu_1", &json!({"questions": two_options()})).await.unwrap());
            let body = get(&rig).await;
            assert_eq!(body["jev_options"]["questions"], json!([{"index": 0, "scores": [a, b], "suggested": Value::Null}]), "{a}/{b}");
            assert_eq!(body["questions"][0]["options"].as_array().unwrap().len(), 2);
        }
    }

    /// Jev 失敗（5xx、答案缺題）：問題照常顯示、`jev_options` 是 null、帳本記 error、不重試。
    #[tokio::test]
    async fn a_failing_jev_leaves_the_question_untouched_and_is_not_retried() {
        let rig = stand(Mode::Status(503), true, true).await;
        write_ask(&rig, "toolu_1", two_options());
        let input = json!({"questions": two_options()});
        assert!(!ask(&rig.env.app, &rig.bot.id, &rig.run_id, "toolu_1", &input).await.unwrap());
        let body = get(&rig).await;
        assert_eq!(body["questions"][0]["question"], "連線方式？");
        assert_eq!(body["jev_options"], Value::Null);
        let rows = ledger(&rig.env.app).await;
        assert_eq!(rows.len(), 1);
        assert!(rows[0].1.as_deref().is_some_and(|e| e.contains("503")), "{:?}", rows[0]);
        let _ = get(&rig).await;
        assert!(!ask(&rig.env.app, &rig.bot.id, &rig.run_id, "toolu_1", &input).await.unwrap());
        assert_eq!(rig.seen.lock().unwrap().len(), 1);
        assert_eq!(ledger(&rig.env.app).await.len(), 1);
    }

    /// 關著、專案不在名單、專案已移交：零請求、零帳本，題目照常顯示。
    #[tokio::test]
    async fn disabled_unlisted_and_handed_off_projects_send_nothing() {
        let input = json!({"questions": two_options()});
        for case in ["disabled", "unlisted", "handed_off"] {
            let rig = stand(scores(&[("q1_a", 0.95)]), case != "disabled", case != "unlisted").await;
            if case == "handed_off" {
                sqlx::query("UPDATE projects SET handed_off_to='remote' WHERE id=?").bind(&rig.env.project_id).execute(&rig.env.app.db).await.unwrap();
            }
            write_ask(&rig, "toolu_1", two_options());
            let body = get(&rig).await;
            assert_eq!(body["questions"][0]["question"], "連線方式？", "{case}");
            assert_eq!(body["jev_options"], Value::Null, "{case}");
            assert!(!ask(&rig.env.app, &rig.bot.id, &rig.run_id, "toolu_1", &input).await.unwrap(), "{case}");
            assert!(rig.seen.lock().unwrap().is_empty(), "{case}: 不發請求");
            assert!(ledger(&rig.env.app).await.is_empty(), "{case}: 不寫帳本");
        }
    }

    /// 每小時名額（`reserve_slot` 的保險絲）用完：不問、不寫帳本、題目照常顯示。
    #[tokio::test]
    async fn an_exhausted_hourly_fuse_means_no_request() {
        let rig = stand(scores(&[("q1_a", 0.95)]), true, true).await;
        rig.env.app.cfg.update(|c| {
            c.judge.max_per_hour = 1;
            Ok(())
        }).await.unwrap();
        sqlx::query("INSERT INTO judge_shadow (id, at, bot_id, run_id, kind, matched_line, composer_idle, regex_verdict, error) VALUES (?,?,?,?,?,?,0,'limit_hit','boom')")
            .bind(db::ulid())
            .bind(db::now())
            .bind(&rig.bot.id)
            .bind("other-run")
            .bind("claude")
            .bind("limit")
            .execute(&rig.env.app.db)
            .await
            .unwrap();
        write_ask(&rig, "toolu_1", two_options());
        let input = json!({"questions": two_options()});
        assert!(!ask(&rig.env.app, &rig.bot.id, &rig.run_id, "toolu_1", &input).await.unwrap());
        assert!(rig.seen.lock().unwrap().is_empty());
        assert!(ledger(&rig.env.app).await.is_empty());
    }

    /// 多選題不問；單選題與多選題同一次提問時，只問單選那題（題號保留原位置）。
    #[tokio::test]
    async fn multi_select_questions_are_not_asked_and_do_not_shift_the_numbering() {
        let rig = stand(scores(&[("q2_b", 0.9)]), true, true).await;
        let only_multi = json!([{"question": "要哪些？", "multiSelect": true, "options": [{"label": "A"}, {"label": "B"}]}]);
        write_ask(&rig, "toolu_m", only_multi.clone());
        assert_eq!(get(&rig).await["jev_options"], Value::Null);
        assert!(rig.seen.lock().unwrap().is_empty(), "只有多選題：不發請求");

        let mixed = json!([
            {"question": "要哪些？", "multiSelect": true, "options": [{"label": "A"}, {"label": "B"}]},
            {"question": "連線方式？", "multiSelect": false, "options": [{"label": "內網"}, {"label": "公網"}]},
        ]);
        write_ask(&rig, "toolu_x", mixed.clone());
        assert!(ask(&rig.env.app, &rig.bot.id, &rig.run_id, "toolu_x", &json!({"questions": mixed})).await.unwrap());
        let body = get(&rig).await;
        assert_eq!(body["jev_options"]["questions"], json!([{"index": 1, "scores": [0.05, 0.9], "suggested": 1}]));
        let sent = rig.seen.lock().unwrap().clone();
        assert_eq!(sent.len(), 1);
        let mut keys: Vec<_> = sent[0]["questions"].as_object().unwrap().keys().cloned().collect();
        keys.sort();
        assert_eq!(keys, ["q2_a", "q2_b"]);
    }

    /// 同一題換一次提問（另一個 tool_use）要重問；上一次的建議不能掛到新的這一次。
    #[tokio::test]
    async fn a_new_tool_use_is_asked_again() {
        let rig = stand(scores(&[("q1_a", 0.9)]), true, true).await;
        write_ask(&rig, "toolu_1", two_options());
        let input = json!({"questions": two_options()});
        assert!(ask(&rig.env.app, &rig.bot.id, &rig.run_id, "toolu_1", &input).await.unwrap());
        write_ask(&rig, "toolu_2", two_options());
        assert_eq!(lookup(&rig.env.app, &rig.run_id, "toolu_2").await.unwrap(), Lookup::Never, "新的提問還沒有答案，上一次的不能掛過來");
        assert!(ask(&rig.env.app, &rig.bot.id, &rig.run_id, "toolu_2", &input).await.unwrap());
        assert_eq!(rig.seen.lock().unwrap().len(), 2);
        assert_eq!(ledger(&rig.env.app).await.len(), 2);
    }

    /// 沒有使用者 prompt 可依據：不問（Jev 沒有東西可判斷，白送一次外送）。
    #[tokio::test]
    async fn no_prompt_to_go_by_means_no_request() {
        let rig = stand(scores(&[("q1_a", 0.9)]), true, true).await;
        sqlx::query("DELETE FROM turns").execute(&rig.env.app.db).await.unwrap();
        write_ask(&rig, "toolu_1", two_options());
        let input = json!({"questions": two_options()});
        assert!(!ask(&rig.env.app, &rig.bot.id, &rig.run_id, "toolu_1", &input).await.unwrap());
        assert!(rig.seen.lock().unwrap().is_empty());
        assert!(ledger(&rig.env.app).await.is_empty());
    }
}
}

pub mod report {
    pub mod tests {
        use crate::lifecycle_tests::prelude::*;
        use crate::judge::report::*;
        use crate::runners::judge::shadow_settled;
        use crate::testing as tt;

    pub async fn fake_jev(status: u16, claims: f64, asks: f64, wait: bool) -> (String, Arc<std::sync::Mutex<Vec<Value>>>, Arc<tokio::sync::Notify>) {
        use axum::http::StatusCode;
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = seen.clone();
        let go = Arc::new(tokio::sync::Notify::new());
        let gate = go.clone();
        let route = axum::routing::post(move |axum::Json(body): axum::Json<Value>| {
            let log = log.clone();
            let gate = gate.clone();
            async move {
                log.lock().unwrap().push(body);
                if wait {
                    gate.notified().await;
                }
                (
                    StatusCode::from_u16(status).unwrap(),
                    axum::Json(json!({
                        "model": "jev-1.13.0",
                        "answers": {
                            "claims_verified": {"type": "noul", "noul": claims},
                            "asks_parent_action": {"type": "noul", "noul": asks}
                        },
                        "usage": {"input_tokens": 1400}
                    })),
                )
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/systemone", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, axum::Router::new().route("/v1/systemone", route)).await.unwrap() });
        (url, seen, go)
    }

    struct World {
        env: tt::Env,
        bot_id: String,
        parent_id: String,
        assignment_id: String,
        seen: Arc<std::sync::Mutex<Vec<Value>>>,
    }

    impl World {
        fn app(&self) -> &Arc<App> {
            &self.env.app
        }
    }

    pub async fn enable(app: &Arc<App>, project_id: &str, endpoint: &str, key: &std::path::Path) {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(key, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let project_id = project_id.to_string();
        let endpoint = endpoint.to_string();
        let key_path = key.to_string_lossy().into_owned();
        app.cfg
            .update(move |c| {
                c.judge.enabled = true;
                c.judge.projects = vec![project_id];
                c.judge.key_file = key_path;
                c.judge.endpoint = endpoint;
                Ok(())
            })
            .await
            .unwrap();
    }

    async fn world(status: u16, claims: f64, asks: f64) -> World {
        let env = tt::env().await;
        let (url, seen, _go) = fake_jev(status, claims, asks, false).await;
        let key = env.dir.join("jev-key");
        std::fs::write(&key, "k-test\n").unwrap();
        enable(&env.app, &env.project_id, &url, &key).await;
        let parent = tt::claude_bot(&env.app, &env.project_id, "parent").await;
        let child = tt::claude_bot(&env.app, &env.project_id, "child").await;
        sqlx::query("UPDATE bots SET parent_bot_id = ?, managed_by = 'child' WHERE id = ?")
            .bind(&parent.id)
            .bind(&child.id)
            .execute(&env.app.db)
            .await
            .unwrap();
        let a = crate::supervisor::store::insert_assignment(&env.app.db, None, &child.id, "crid-report", "把功能做完", &[], None, true)
            .await
            .unwrap();
        let settled = crate::supervisor::store::settle_and_notify(
            &env.app.db,
            &a.id,
            "completed",
            true,
            Some("cargo test 12 通過。commit abcdef1。"),
            None,
            "assignment_completed:test",
            "assignment_completed",
            &json!({"bot_id": child.id}),
        )
        .await
        .unwrap();
        assert!(settled.moved, "測試前置：交辦要先收到 awaiting_review");
        World { env, bot_id: child.id, parent_id: parent.id, assignment_id: a.id, seen }
    }

    async fn status_of(app: &Arc<App>, id: &str) -> String {
        sqlx::query_scalar("SELECT status FROM supervisor_assignments WHERE id = ?").bind(id).fetch_one(&app.db).await.unwrap()
    }

    async fn flags(app: &Arc<App>, id: &str) -> Vec<String> {
        sqlx::query_scalar("SELECT payload_json FROM supervisor_inbox WHERE kind = 'judge_report_evidence' AND assignment_id = ?")
            .bind(id)
            .fetch_all(&app.db)
            .await
            .unwrap()
    }

    async fn ledger(app: &Arc<App>, id: &str) -> (Option<f64>, Option<f64>, Option<String>, String) {
        sqlx::query_as(
            "SELECT claims_verified, asks_parent_action, error, regex_verdict FROM judge_shadow WHERE assignment_id = ?",
        )
        .bind(id)
        .fetch_one(&app.db)
        .await
        .unwrap()
    }

    async fn parent_notes(app: &Arc<App>, parent_id: &str) -> Vec<String> {
        let conv = db::conversation_id(&app.db, parent_id).await.unwrap();
        sqlx::query_scalar("SELECT content FROM messages WHERE conversation_id = ? AND role = 'system' ORDER BY created_at, rowid")
            .bind(conv)
            .fetch_all(&app.db)
            .await
            .unwrap()
    }

    fn sample(w: &World, report: &str) -> Sample {
        Sample { assignment_id: w.assignment_id.clone(), bot_id: w.bot_id.clone(), turn_id: None, report: report.into() }
    }

    #[tokio::test]
    async fn a_report_that_claims_a_passing_check_is_recorded_and_not_flagged() {
        let w = world(200, 0.96, 0.21).await;
        let before = status_of(w.app(), &w.assignment_id).await;
        let flag = observe(w.app(), &sample(&w, "cargo test 12 passed. commit abcdef1.")).await.unwrap();
        assert_eq!(flag, Some(Flag { missing_verification: false, needs_parent: false }));
        assert!(flags(w.app(), &w.assignment_id).await.is_empty(), "有證據、也不用人動手：不標旗");
        assert!(parent_notes(w.app(), &w.parent_id).await.is_empty());
        let (claims, asks, err, verdict) = ledger(w.app(), &w.assignment_id).await;
        assert_eq!((claims, asks, err.as_deref(), verdict.as_str()), (Some(0.96), Some(0.21), None, VERDICT));
        assert_eq!(status_of(w.app(), &w.assignment_id).await, before, "帳本寫了也不能改交辦狀態");
        let sent = w.seen.lock().unwrap()[0].to_string();
        assert!(!sent.contains("ghp_"));
        assert_eq!(w.seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_report_without_a_verification_claim_flags_the_parent_and_does_not_block() {
        let w = world(200, 0.08, 0.30).await;
        let before = status_of(w.app(), &w.assignment_id).await;
        let report = "做完了，還沒跑測試。token ghp_abcdefghijklmnopqrstuvwxyz0123456789 不要送出去。";
        let flag = observe(w.app(), &sample(&w, report)).await.unwrap().unwrap();
        assert!(flag.missing_verification && !flag.needs_parent);
        assert_eq!(status_of(w.app(), &w.assignment_id).await, before);
        let inbox = flags(w.app(), &w.assignment_id).await;
        assert_eq!(inbox.len(), 1);
        assert!(inbox[0].contains("missing_verification"), "{}", inbox[0]);
        assert!(inbox[0].contains("不阻擋"), "{}", inbox[0]);
        assert!(!inbox[0].contains("ghp_"), "旗標內文不帶回報原文：{}", inbox[0]);
        let notes = parent_notes(w.app(), &w.parent_id).await;
        assert_eq!(notes.len(), 1, "父 bot 聊天室要有一則系統旗標");
        assert!(notes[0].contains(&w.assignment_id) && !notes[0].contains("ghp_"));
        assert!(notes[0].starts_with("🤖 Jev 建議") && notes[0].contains("只是建議，不會自動動作"), "聊天室系統訊息要標明是 Jev 的建議：{}", notes[0]);
        let p: Value = serde_json::from_str(&inbox[0]).unwrap();
        assert_eq!((p["suggested_by"].as_str(), p["suggestion_only"].as_bool()), (Some("jev"), Some(true)));
        assert!(p["action"].as_str().unwrap().starts_with("🤖 Jev 建議") && p["action"].as_str().unwrap().contains("8%"), "{}", p["action"]);
        let sent = w.seen.lock().unwrap()[0].to_string();
        assert!(!sent.contains("ghp_"), "送出去之前要遮罩：{sent}");
        assert!(sent.contains("claims_verified") && sent.contains("asks_parent_action"));
        assert!(observe(w.app(), &sample(&w, report)).await.unwrap().is_none());
        assert_eq!(w.seen.lock().unwrap().len(), 1);
        assert_eq!(flags(w.app(), &w.assignment_id).await.len(), 1);
    }

    #[tokio::test]
    async fn a_verified_report_that_asks_the_parent_to_act_is_flagged_too() {
        let w = world(200, 0.97, 0.91).await;
        let flag = observe(w.app(), &sample(&w, "tests passed. please restart the daemon.")).await.unwrap().unwrap();
        assert!(!flag.missing_verification && flag.needs_parent);
        let inbox = flags(w.app(), &w.assignment_id).await;
        assert!(inbox[0].contains("needs_parent_action") && !inbox[0].contains("missing_verification"));
        assert_eq!(status_of(w.app(), &w.assignment_id).await, "awaiting_review");
    }

    #[tokio::test]
    async fn a_failing_jev_writes_the_ledger_and_raises_nothing() {
        let w = world(500, 0.0, 0.0).await;
        let before = status_of(w.app(), &w.assignment_id).await;
        let flag = observe(w.app(), &sample(&w, "做完了。")).await.unwrap();
        assert!(flag.is_none(), "失敗不標旗");
        assert!(flags(w.app(), &w.assignment_id).await.is_empty());
        assert!(parent_notes(w.app(), &w.parent_id).await.is_empty());
        let (claims, asks, err, _) = ledger(w.app(), &w.assignment_id).await;
        assert!(claims.is_none() && asks.is_none());
        assert_eq!(err.as_deref(), Some("http 500"));
        assert_eq!(status_of(w.app(), &w.assignment_id).await, before);
        assert_eq!(w.seen.lock().unwrap().len(), 1, "不重試");
    }

    #[tokio::test]
    async fn shadow_settled_returns_before_jev_answers_and_a_disabled_judge_never_calls() {
        let env = tt::env().await;
        let app = env.app.clone();
        let (url, seen, go) = fake_jev(200, 0.1, 0.1, true).await;
        let key = env.dir.join("jev-key");
        std::fs::write(&key, "k-test\n").unwrap();
        enable(&app, &env.project_id, &url, &key).await;
        app.cfg.update(|c| { c.judge.timeout_ms = 60_000; Ok(()) }).await.unwrap();
        let child = tt::claude_bot(&app, &env.project_id, "child").await;
        shadow_settled(&app, "A-fail", &child.id, None, "failed", Some("做完了")).await;
        shadow_settled(&app, "A-scraped", &child.id, None, "completed_fallback", Some("把功能做完\n5-hour limit reached")).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(seen.lock().unwrap().is_empty(), "失敗回合與 completed_fallback 都不問");

        tokio::time::timeout(Duration::from_secs(10), shadow_settled(&app, "A-hang", &child.id, None, "completed", Some("還沒跑測試")))
            .await
            .expect("不能在 tick 裡等 Jev");
        let n: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM supervisor_inbox WHERE kind = 'judge_report_evidence'")
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(n, 0);
        go.notify_one();
        let got = tt::eventually!(sqlx::query_scalar::<_, Option<String>>("SELECT error FROM judge_shadow WHERE assignment_id = 'A-hang'")
            .fetch_optional(&app.db)
            .await
            .unwrap()
            .is_some_and(|e| e.as_deref() != Some("pending")));
        assert!(got, "背景任務問完要寫帳本");
        app.cfg.update(|c| { c.judge.enabled = false; Ok(()) }).await.unwrap();
        let calls = seen.lock().unwrap().len();
        shadow_settled(&app, "A-off", &child.id, None, "completed", Some("還沒跑測試")).await;
        tokio::time::sleep(Duration::from_millis(40)).await;
        assert_eq!(seen.lock().unwrap().len(), calls, "關掉之後不再打 API");
    }

    #[tokio::test]
    async fn note_cleared_does_not_stamp_a_report_row() {
        let w = world(200, 0.96, 0.1).await;
        observe(w.app(), &sample(&w, "tests passed, 4 ok.")).await.unwrap();
        crate::judge::note_cleared(&w.app().db, &w.bot_id).await;
        let cleared: Option<String> = sqlx::query_scalar("SELECT cleared_at FROM judge_shadow WHERE assignment_id = ?")
            .bind(&w.assignment_id)
            .fetch_one(&w.app().db)
            .await
            .unwrap();
        assert!(cleared.is_none(), "report_evidence 的 cleared_at 沒有「被清掉」這件事");
    }
}
}

pub mod stuck {
mod tests {
    use crate::lifecycle_tests::prelude::*;
    use crate::judge::stuck::*;
    use std::sync::Arc;
    use crate::db;
    use crate::runners::judge::{spawn_sweep_task, sweep_once, SweepGuard, MAX_PER_ROUND, SEEN_COOLDOWN, SWEEPING};
    use crate::state::App;
    use crate::testing as tt;

    /// `SWEEPING` 是行程全域旗標：直接拿 `SweepGuard` 的測試一次只准一條（照 `idle_sleep.rs` 的 `SWEEP_TEST_LOCK`，issue #947）。
    static SWEEP_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[tokio::test]
    async fn a_slow_background_sweep_is_cancelled_and_joined_on_shutdown() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let _serial = SWEEP_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        SWEEPING.store(false, Ordering::SeqCst);

        struct DropMark(Arc<AtomicBool>);
        impl Drop for DropMark {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let env = tt::env().await;
        let app = env.app.clone();
        let guard = SweepGuard::take().expect("test owns the single sweep slot");
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let dropped = Arc::new(AtomicBool::new(false));
        let round_dropped = dropped.clone();
        spawn_sweep_task(&app, guard, async move {
            let _mark = DropMark(round_dropped);
            let _ = started_tx.send(());
            std::future::pending::<()>().await;
        });
        started_rx.await.unwrap();

        app.shutdown.cancel();
        app.background_tasks.close();
        let joined = tokio::time::timeout(std::time::Duration::from_secs(5), app.background_tasks.wait()).await;
        assert!(joined.is_ok(), "daemon shutdown must join the in-flight Jev probe");
        assert!(dropped.load(Ordering::SeqCst), "shutdown must drop the slow sweep future");
        assert!(!SWEEPING.load(Ordering::SeqCst), "cancelling a sweep must release its concurrency guard");
    }

    /// 假 Jev：回 `blocked_by_dialog` 的機率，記下收到的 body。
    async fn fake_jev(p: f64) -> (String, Arc<std::sync::Mutex<Vec<Value>>>) {
        use axum::http::StatusCode;
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = seen.clone();
        let route = axum::routing::post(move |axum::Json(body): axum::Json<Value>| {
            let log = log.clone();
            async move {
                log.lock().unwrap().push(body);
                (StatusCode::OK, axum::Json(json!({"model": "jev-1.13.0", "answers": {"blocked_by_dialog": {"type": "noul", "noul": p}}, "usage": {"input_tokens": 700}})))
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/systemone", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, axum::Router::new().route("/v1/systemone", route)).await.unwrap() });
        (url, seen)
    }

    async fn setup(p: f64, enabled: bool) -> (tt::Env, Stuck, Arc<std::sync::Mutex<Vec<Value>>>, std::path::PathBuf) {
        let env = tt::env().await;
        let app = env.app.clone();
        let (url, seen) = fake_jev(p).await;
        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-judge-stuck-{}", db::ulid())));
        std::fs::create_dir_all(&dir).unwrap();
        let key = dir.join("key");
        std::fs::write(&key, "k-test\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let pid = env.project_id.clone();
        app.cfg
            .update(move |c| {
                c.judge.enabled = enabled;
                c.judge.projects = vec![pid];
                c.judge.key_file = key.to_string_lossy().into_owned();
                c.judge.endpoint = url;
                Ok(())
            })
            .await
            .unwrap();
        let bot = tt::claude_bot(&app, &env.project_id, "stuck").await;
        let run_id = tt::fake_run(&app, &bot.id).await;
        let conv = db::conversation_id(&app.db, &bot.id).await.unwrap();
        let turn_id = db::ulid();
        sqlx::query("INSERT INTO turns (id, conversation_id, origin, status, delivery, prompt_text, created_at) VALUES (?,?,'web','queued','pending','派工','2026-09-22T15:00:00.000Z')")
            .bind(&turn_id)
            .bind(&conv)
            .execute(&app.db)
            .await
            .unwrap();
        let stuck = Stuck { bot_id: bot.id.clone(), run_id, pane_id: format!("pane-{}", bot.id), turn_id, waited_secs: 1800 };
        (env, stuck, seen, dir)
    }

    async fn inbox_kinds(app: &Arc<App>) -> Vec<String> {
        crate::supervisor::store::pending_inbox(&app.db).await.unwrap().into_iter().map(|e| e.kind).collect()
    }

    /// 2.1.278 的 Auto mode 框：排了半小時的 prompt、pane idle、輸入列被框占著 → 問 Jev、記帳本、推通知；不按鍵。
    #[tokio::test]
    async fn a_queued_prompt_behind_an_unknown_dialog_raises_one_inbox_event_and_presses_nothing() {
        let (env, s, seen, dir) = setup(0.93, true).await;
        let app = env.app.clone();
        env.herdr.set_screen(&s.pane_id, crate::tui_prompts::screens::AUTO_MODE);
        let cands = candidates(&app, STUCK_AFTER_SECS).await.unwrap();
        assert_eq!(cands.iter().map(|c| c.turn_id.as_str()).collect::<Vec<_>>(), [s.turn_id.as_str()], "候選就是這一筆");
        assert_eq!(inspect(&app, &s).await.unwrap(), Some(0.93));
        let sent = seen.lock().unwrap().clone();
        assert_eq!(sent.len(), 1);
        assert!(sent[0]["state"]["screen"].as_str().unwrap().contains("keep bypass permissions"));
        assert!(!sent[0].to_string().contains(&s.bot_id), "不送 bot 識別");
        assert_eq!(inbox_kinds(&app).await, ["judge_stuck_screen"]);
        let ev = crate::supervisor::store::pending_inbox(&app.db).await.unwrap().remove(0);
        let p: Value = serde_json::from_str(&ev.payload_json).unwrap();
        assert_eq!((p["suggested_by"].as_str(), p["source_label"].as_str(), p["suggestion_only"].as_bool(), p["probability_pct"].as_i64()), (Some("jev"), Some("Jev 建議"), Some(true), Some(93)));
        let action = p["action"].as_str().unwrap();
        assert!(action.starts_with("🤖 Jev 建議") && action.contains("93%") && action.contains("只是建議，不會自動動作"), "{action}");
        assert!(env.herdr.calls_to("pane.send_keys").is_empty(), "只通知，不按鍵");
        assert_eq!(inspect(&app, &s).await.unwrap(), None);
        assert_eq!(seen.lock().unwrap().len(), 1);
        assert_eq!(inbox_kinds(&app).await.len(), 1);
        let rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM judge_shadow WHERE regex_verdict='stuck_queued' AND jev_is_live_ui=0.93").fetch_one(&app.db).await.unwrap();
        assert_eq!(rows, 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// #1467：遠端主機（mac_builder）上的專案走同一輪背景掃描：候選的 SQL 不分主機，畫面從那台主機自己的 herdr 讀
    /// （`herdr_for_run`），不是本機那顆；一樣問一次、推一則通知、不按鍵。
    #[tokio::test]
    async fn a_remote_projects_queued_prompt_is_read_from_that_hosts_own_herdr() {
        let (env, s, seen, dir) = setup(0.93, true).await;
        let app = env.app.clone();
        let sock = env.dir.join("data").join("herdr-mac_builder.sock");
        let remote = tt::MockHerdr::start(sock.clone());
        remote.set_screen(&s.pane_id, crate::tui_prompts::screens::AUTO_MODE);
        let cfg = crate::config::HostCfg { daemon_url: None,
            shared_session: false,
            name: "mac_builder".into(),
            ssh: "mac_builder@100.64.0.2".into(),
            ssh_port: 22,
            ssh_opts: vec![],
            herdr_session: "test".into(),
            remote_path: String::new(),
        };
        app.hosts.insert_remote_with_client_for_test(cfg, crate::herdr::HerdrClient::new(sock)).await;
        sqlx::query("UPDATE projects SET host='mac_builder' WHERE id=?").bind(&env.project_id).execute(&app.db).await.unwrap();

        sweep_once(&app).await;

        assert_eq!(seen.lock().unwrap().len(), 1, "遠端專案的卡住畫面照樣問一次");
        assert_eq!(inbox_kinds(&app).await, ["judge_stuck_screen"]);
        assert!(!remote.calls_to("pane.read").is_empty(), "畫面是從遠端那台的 herdr 讀的");
        assert!(env.herdr.calls_to("pane.read").is_empty(), "不是本機那顆");
        assert!(remote.calls_to("pane.send_keys").is_empty() && env.herdr.calls_to("pane.send_keys").is_empty(), "只通知，不按鍵");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// 低機率只記帳本；輸入列空著（沒有框）連問都不問；關著什麼都不做。
    #[tokio::test]
    async fn low_probability_idle_composer_and_disabled_are_all_quiet() {
        let (env, s, seen, dir) = setup(0.2, true).await;
        let app = env.app.clone();
        env.herdr.set_screen(&s.pane_id, crate::tui_prompts::screens::AUTO_MODE);
        assert_eq!(inspect(&app, &s).await.unwrap(), Some(0.2));
        assert!(inbox_kinds(&app).await.is_empty(), "低於門檻不推通知");
        let idle = "⏺ 做完了。\n────────\n❯\n────────\n  x | agents-manager | Opus 5 | 5h:96%\n";
        env.herdr.set_screen(&s.pane_id, idle);
        assert_eq!(inspect(&app, &s).await.unwrap(), None);
        assert_eq!(seen.lock().unwrap().len(), 1, "輸入列空著不問");
        std::fs::remove_dir_all(&dir).ok();

        let (env, s, seen, dir) = setup(0.99, false).await;
        env.herdr.set_screen(&s.pane_id, crate::tui_prompts::screens::AUTO_MODE);
        sweep_once(&env.app).await;
        assert!(seen.lock().unwrap().is_empty() && inbox_kinds(&env.app).await.is_empty(), "關著零呼叫");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// #480：一輪最多**看** `MAX_PER_ROUND` 顆，而且下一輪要輪到後面的（不能餓死第 11 顆）。
    ///
    /// 一個對話只能有一筆 queued（`turns.conversation_id` 上的唯一索引），所以候選數等於
    /// 「有 queued turn 的 bot 數」——要撐出上限就得真的多開幾顆 bot。
    #[tokio::test]
    async fn a_round_is_bounded_and_the_next_round_moves_on() {
        let (env, _s, _seen, dir) = setup(0.99, true).await;
        let app = env.app.clone();
        let total = MAX_PER_ROUND + 5;
        for i in 0..total {
            let bot = tt::claude_bot(&app, &env.project_id, &format!("stuck-{i}")).await;
            tt::fake_run(&app, &bot.id).await;
            let conv = db::conversation_id(&app.db, &bot.id).await.unwrap();
            sqlx::query("INSERT INTO turns (id, conversation_id, origin, status, delivery, prompt_text, created_at) VALUES (?,?,'web','queued','pending','派工','2026-09-22T15:00:00.000Z')")
                .bind(db::ulid())
                .bind(&conv)
                .execute(&app.db)
                .await
                .unwrap();
        }
        let all = candidates(&app, STUCK_AFTER_SECS).await.unwrap();
        assert!(all.len() > MAX_PER_ROUND, "撈到的要比每輪上限多，否則輪不動：{}", all.len());

        let pick = |app: Arc<App>| async move {
            let now = std::time::Instant::now();
            let mut seen = app.judge_stuck_seen.lock().await;
            seen.retain(|_, t| now.duration_since(*t) < SEEN_COOLDOWN);
            let cands = candidates(&app, STUCK_AFTER_SECS).await.unwrap();
            let picked: Vec<String> =
                cands.into_iter().filter(|c| !seen.contains_key(&c.turn_id)).take(MAX_PER_ROUND).map(|c| c.turn_id).collect();
            for t in &picked {
                seen.insert(t.clone(), now);
            }
            picked
        };
        let first = pick(app.clone()).await;
        assert_eq!(first.len(), MAX_PER_ROUND, "一輪要有上限");
        let second = pick(app.clone()).await;
        assert!(!second.is_empty(), "第二輪要輪到後面的，不能被最舊那幾顆餓死");
        assert!(second.iter().all(|t| !first.contains(t)), "第二輪不該又是同一批");
        std::fs::remove_dir_all(&dir).ok();
    }

    /// #480：同時只准一輪在跑；而且 **panic 也要放掉**——用「最後一行 `store(false)`」寫法時，
    /// 一次 panic 就會讓 sweep 從此靜靜不再跑（tokio 不會因為 task panic 收掉行程）。
    #[test]
    fn the_sweep_guard_is_released_even_when_the_round_panics() {
        use std::sync::atomic::Ordering;
        let _serial = SWEEP_TEST_LOCK.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        SWEEPING.store(false, Ordering::SeqCst);
        assert!(!SWEEPING.load(Ordering::SeqCst), "起點是沒人在跑");
        {
            let _g = SweepGuard::take().expect("第一個搶得到");
            assert!(SweepGuard::take().is_none(), "第二個搶不到——那一拍就跳過");
        }
        assert!(!SWEEPING.load(Ordering::SeqCst), "正常結束要放掉");

        let hit = std::panic::catch_unwind(|| {
            let _g = SweepGuard::take().expect("panic 那一輪也拿得到");
            panic!("boom");
        });
        assert!(hit.is_err(), "這裡就是要它 panic");
        assert!(!SWEEPING.load(Ordering::SeqCst), "panic 之後也要放掉，否則 sweep 從此不再跑");
        assert!(SweepGuard::take().is_some(), "下一輪拿得到");
        SWEEPING.store(false, Ordering::SeqCst);
    }
    }
}

pub mod collision {
mod tests {
    use crate::lifecycle_tests::prelude::*;
    use crate::judge::collision::*;
    use std::collections::HashSet;
    use std::sync::Mutex;
    use std::time::Instant;
    use crate::judge::collision::test_seams::*;
    use crate::state::App;
    use std::sync::Arc;
    use crate::db;
    use crate::release_triage::ledger::{IssueRef, Row, Status};
    use crate::release_triage::{Bucket, Entry};
    use crate::testing as tt;
    use axum::http::StatusCode;

    #[derive(Clone, Copy)]
    enum Mode {
        Noul(f64),
        Hang,
        Status(u16),
    }

    async fn fake_jev(mode: Mode) -> (String, Arc<std::sync::Mutex<Vec<Value>>>, Arc<tokio::sync::Notify>) {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let log = seen.clone();
        let request_received = Arc::new(tokio::sync::Notify::new());
        let notify = request_received.clone();
        let route = axum::routing::post(move |axum::Json(body): axum::Json<Value>| {
            let log = log.clone();
            let notify = notify.clone();
            let mode = match mode {
                Mode::Noul(p) => Mode::Noul(p),
                Mode::Hang => Mode::Hang,
                Mode::Status(c) => Mode::Status(c),
            };
            async move {
                log.lock().unwrap().push(body);
                notify.notify_one();
                match mode {
                    Mode::Hang => {
                        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
                        (StatusCode::OK, axum::Json(json!({})))
                    }
                    Mode::Status(code) => (StatusCode::from_u16(code).unwrap(), axum::Json(json!({"error": "no"}))),
                    Mode::Noul(p) => (
                        StatusCode::OK,
                        axum::Json(json!({"model": "jev-1.13.0", "answers": {"same_work": {"type": "noul", "noul": p}}, "usage": {"input_tokens": 400}})),
                    ),
                }
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/systemone", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, axum::Router::new().route("/v1/systemone", route)).await.unwrap() });
        (url, seen, request_received)
    }

    async fn gated_parallel_jev() -> (String, Arc<std::sync::Mutex<Vec<Value>>>, Arc<tokio::sync::Semaphore>, Arc<tokio::sync::Semaphore>) {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let arrived = Arc::new(tokio::sync::Semaphore::new(0));
        let release = Arc::new(tokio::sync::Semaphore::new(0));
        let (log, arrived_in, release_in) = (seen.clone(), arrived.clone(), release.clone());
        let route = axum::routing::post(move |axum::Json(body): axum::Json<Value>| {
            log.lock().unwrap().push(body);
            arrived_in.add_permits(1);
            let release = release_in.clone();
            async move {
                release.acquire().await.unwrap().forget();
                (
                    StatusCode::OK,
                    axum::Json(json!({"model": "jev-1.13.0", "answers": {"same_work": {"type": "noul", "noul": 0.8}}, "usage": {"input_tokens": 400}})),
                )
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/systemone", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, axum::Router::new().route("/v1/systemone", route)).await.unwrap() });
        (url, seen, arrived, release)
    }

    struct Rig {
        env: tt::Env,
        dir: std::path::PathBuf,
        seen: Arc<std::sync::Mutex<Vec<Value>>>,
        request_received: Arc<tokio::sync::Notify>,
    }

    async fn stand(mode: Mode, enabled: bool, listed: bool) -> Rig {
        let env = tt::env().await;
        let (url, seen, request_received) = fake_jev(mode).await;
        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-judge-collision-{}", db::ulid())));
        std::fs::create_dir_all(&dir).unwrap();
        let key = dir.join("key");
        std::fs::write(&key, "k-test\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let pid = if listed { env.project_id.clone() } else { "not-this-project".into() };
        let key_s = key.to_string_lossy().into_owned();
        env.app
            .cfg
            .update(move |c| {
                c.judge.enabled = enabled;
                c.judge.projects = vec![pid];
                c.judge.key_file = key_s;
                c.judge.endpoint = url;
                c.judge.timeout_ms = 300;
                Ok(())
            })
            .await
            .unwrap();
        Rig { env, dir, seen, request_received }
    }

    impl Drop for Rig {
        fn drop(&mut self) {
            std::fs::remove_dir_all(&self.dir).ok();
        }
    }

    fn write_branch(dir: &std::path::Path, branch: &str) {
        let git = dir.join(".git");
        std::fs::create_dir_all(&git).unwrap();
        std::fs::write(git.join("HEAD"), format!("ref: refs/heads/{branch}\n")).unwrap();
    }

    async fn child(app: &Arc<App>, project: &str, parent: &str, name: &str, cwd: &str, title: &str) -> db::Bot {
        let bot = tt::claude_bot(app, project, name).await;
        sqlx::query("UPDATE bots SET parent_bot_id=?, cwd=?, managed_by='child' WHERE id=?")
            .bind(parent)
            .bind(cwd)
            .bind(&bot.id)
            .execute(&app.db)
            .await
            .unwrap();
        let run = tt::fake_run(app, &bot.id).await;
        sqlx::query("UPDATE runs SET agent_title=? WHERE id=?").bind(title).bind(&run).execute(&app.db).await.unwrap();
        db::bot(&app.db, &bot.id).await.unwrap().unwrap()
    }

    async fn shadow_rows(app: &Arc<App>) -> Vec<(Option<f64>, Option<f64>, Option<String>, String, String)> {
        sqlx::query_as("SELECT jev_is_live_ui, jev_same_work, error, regex_verdict, matched_line FROM judge_shadow WHERE regex_verdict='same_work' ORDER BY at, id")
            .fetch_all(&app.db)
            .await
            .unwrap()
    }

    struct AskedRaceGateGuard;

    impl AskedRaceGateGuard {
        fn install(pair: &str, parties: usize) -> Self {
            *asked_race_gate().get_or_init(|| std::sync::Mutex::new(None)).lock().unwrap() = Some(AskedRaceGate {
                pair: pair.to_string(),
                barrier: Arc::new(tokio::sync::Barrier::new(parties)),
            });
            Self
        }
    }

    impl Drop for AskedRaceGateGuard {
        fn drop(&mut self) {
            *asked_race_gate().get_or_init(|| std::sync::Mutex::new(None)).lock().unwrap() = None;
        }
    }

    async fn inbox(app: &Arc<App>) -> Vec<crate::supervisor::store::InboxEvent> {
        crate::supervisor::store::pending_inbox(&app.db).await.unwrap().into_iter().filter(|e| e.kind == "judge_same_work").collect()
    }

    async fn status_of(app: &Arc<App>, id: &str) -> String {
        sqlx::query_scalar("SELECT status FROM supervisor_assignments WHERE id=?").bind(id).fetch_one(&app.db).await.unwrap()
    }

    #[tokio::test]
    async fn same_pair_claim_race_asks_once_and_pushes_one_hint() {
        let rig = stand(Mode::Noul(0.8), true, true).await;
        let app = rig.env.app.clone();
        let caller = tt::claude_bot(&app, &rig.env.project_id, "caller").await;
        let other = tt::claude_bot(&app, &rig.env.project_id, "other").await;
        let existing = crate::supervisor::store::insert_assignment(
            &app.db,
            None,
            &other.id,
            "existing-1499",
            "修正同一個 Jev 撞題檢查的競態 #1499",
            &[],
            None,
            true,
        )
        .await
        .unwrap();
        let pair = format!("claim:{}:1499|asg:{}", caller.id, existing.id);
        let _gate = AskedRaceGateGuard::install(&pair, 2);
        let ask = || ClaimAsk {
            project_id: rig.env.project_id.clone(),
            caller: caller.id.clone(),
            issue: Some(1499),
            title: "修正同一個 Jev 撞題檢查的競態 #1499".into(),
            child: "candidate".into(),
            child_bot_id: None,
            claim_id: None,
            branch: None,
        };

        let first_ask = ask();
        let second_ask = ask();
        let (first, second) = tokio::join!(check_claim(&app, &first_ask), check_claim(&app, &second_ask));
        first.unwrap();
        second.unwrap();

        assert_eq!(rig.seen.lock().unwrap().len(), 1, "同一 pair 只送一個 Jev HTTP request");
        assert_eq!(shadow_rows(&app).await.len(), 1, "同一 pair 只占一列帳本");
        assert_eq!(inbox(&app).await.len(), 1, "同一 pair 只推一則通知");
    }

    #[tokio::test]
    async fn different_pairs_reach_jev_while_the_other_request_is_pending() {
        let rig = stand(Mode::Noul(0.8), true, true).await;
        let app = rig.env.app.clone();
        let (url, seen, arrived, release) = gated_parallel_jev().await;
        app.cfg.update(move |c| {
            c.judge.endpoint = url;
            c.judge.timeout_ms = 10_000;
            Ok(())
        })
        .await
        .unwrap();
        let caller = tt::claude_bot(&app, &rig.env.project_id, "caller").await;
        let other = tt::claude_bot(&app, &rig.env.project_id, "other").await;
        crate::supervisor::store::insert_assignment(&app.db, None, &other.id, "existing-1499", "修正撞題並行認領問題 #1499", &[], None, true).await.unwrap();
        let ask = |issue| ClaimAsk {
            project_id: rig.env.project_id.clone(),
            caller: caller.id.clone(),
            issue: Some(issue),
            title: format!("檢查獨立工作 {issue}"),
            child: "candidate".into(),
            child_bot_id: None,
            claim_id: None,
            branch: None,
        };
        let first_app = app.clone();
        let first_ask = ask(14991);
        let first = tokio::spawn(async move { check_claim(&first_app, &first_ask).await });
        let second_app = app.clone();
        let second_ask = ask(14992);
        let second = tokio::spawn(async move { check_claim(&second_app, &second_ask).await });

        arrived.acquire_many(2).await.unwrap().forget();
        assert_eq!(seen.lock().unwrap().len(), 2, "不同 pair 都能在前一個 Jev 回覆前送出 request");
        release.add_permits(2);
        first.await.unwrap().unwrap();
        second.await.unwrap().unwrap();
        assert_eq!(shadow_rows(&app).await.len(), 2, "不同 pair 各自留一列帳本");
        assert_eq!(inbox(&app).await.len(), 2, "不同 pair 各自推提示");
    }

    #[tokio::test]
    async fn overlap_at_or_above_half_hints_and_records_without_blocking() {
        let rig = stand(Mode::Noul(0.72), true, true).await;
        let app = rig.env.app.clone();
        let parent = tt::claude_bot(&app, &rig.env.project_id, "parent").await;
        let wt = rig.dir.join("wt");
        std::fs::create_dir_all(&wt).unwrap();
        write_branch(&wt, "fix/other-g557");
        let existing = child(&app, &rig.env.project_id, &parent.id, "sib", wt.to_str().unwrap(), "卡住的畫面判斷").await;
        crate::supervisor::store::insert_assignment(
            &app.db,
            None,
            &existing.id,
            "old-557",
            "實作 #557：派工時比對同專案在跑的工作 daemon/src/judge.rs",
            &[],
            None,
            true,
        )
        .await
        .unwrap();
        let top = tt::claude_bot(&app, &rig.env.project_id, "top").await;
        let top_run = tt::fake_run(&app, &top.id).await;
        sqlx::query("UPDATE runs SET state='stopped', agent_title=? WHERE id=?")
            .bind("不要比對這個 #999")
            .bind(&top_run)
            .execute(&app.db)
            .await
            .unwrap();

        let newbie = tt::claude_bot(&app, &rig.env.project_id, "newbie").await;
        sqlx::query("UPDATE bots SET parent_bot_id=? WHERE id=?").bind(&parent.id).bind(&newbie.id).execute(&app.db).await.unwrap();
        let own_run = tt::fake_run(&app, &newbie.id).await;
        sqlx::query("UPDATE runs SET agent_title=? WHERE id=?").bind("自己上一件 SELF-TITLE").bind(&own_run).execute(&app.db).await.unwrap();
        crate::supervisor::store::insert_assignment(&app.db, None, &newbie.id, "own-old", "SELF-ASSIGNMENT 還沒驗收", &[], None, true).await.unwrap();
        let a = crate::supervisor::store::insert_assignment(
            &app.db,
            None,
            &newbie.id,
            "new-557",
            "開票與派工的撞題提示，跟 #557 是同一件事，改 daemon/src/judge/collision.rs Bearer super-secret-token",
            &[],
            None,
            true,
        )
        .await
        .unwrap();

        check_assignment(&app, &a.id).await.unwrap();
        assert_eq!(status_of(&app, &a.id).await, "queued", "shadow 不改交辦狀態");
        assert!(rig.env.herdr.calls_to("pane.send_keys").is_empty(), "不按鍵");

        let sent = rig.seen.lock().unwrap().clone();
        assert!(!sent.is_empty());
        let blob = serde_json::to_string(&sent).unwrap();
        assert!(blob.contains(SAME_WORK_QUESTION));
        assert!(blob.contains(SAME_WORK_FOCUS));
        assert!(!blob.contains("\"collision\""), "不問四級 Score");
        assert!(!blob.contains(&newbie.id) && !blob.contains(&rig.env.project_id), "不送 bot／專案 id");
        assert!(!blob.contains("super-secret-token"), "交辦正文要遮罩");
        assert!(blob.contains("daemon/src/judge.rs") || blob.contains("collision.rs"));
        assert!(!blob.contains("#999") && !blob.contains("不要比對"), "頂層 bot 的標題不在比對裡");
        assert!(!blob.contains("SELF-TITLE") && !blob.contains("SELF-ASSIGNMENT"), "候選那顆自己的工作不比");
        let sources: HashSet<String> = shadow_rows(&app)
            .await
            .into_iter()
            .filter_map(|(_, _, _, _, line)| serde_json::from_str::<Value>(&line).ok())
            .filter_map(|v| v.get("source").and_then(|s| s.as_str()).map(str::to_string))
            .collect();
        assert!(sources.contains("claimed_issue"), "{sources:?}");
        assert!(sources.contains("child_title"), "{sources:?}");
        assert!(sources.contains("worktree_branch"), "{sources:?}");

        let rows = shadow_rows(&app).await;
        assert!(rows.iter().all(|(live, p, err, verdict, _)| {
            live.is_none() && *p == Some(0.72) && err.is_none() && verdict == "same_work"
        }));
        let cleared: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM judge_shadow WHERE regex_verdict='same_work' AND cleared_at IS NOT NULL")
            .fetch_one(&app.db)
            .await
            .unwrap();
        assert_eq!(cleared, 0, "cleared_at 只服務撞限，撞題列維持 NULL 給事後對帳");

        let hints = inbox(&app).await;
        assert!(!hints.is_empty());
        assert!(hints.iter().all(|e| e.payload_json.contains(HINT_NOISE) && e.payload_json.contains("不阻擋")));
        assert!(hints.iter().all(|e| {
            let p: Value = serde_json::from_str(&e.payload_json).unwrap();
            let action = p["action"].as_str().unwrap_or_default();
            p["suggested_by"] == "jev" && p["suggestion_only"] == true && p["probability_pct"] == 72 && action.starts_with("🤖 Jev 建議") && action.contains("72%")
        }));
        assert!(hints.iter().any(|e| e.bot_id.as_deref() == Some(parent.id.as_str())), "提示掛在父 bot");

        let n = sent.len();
        check_assignment(&app, &a.id).await.unwrap();
        assert_eq!(rig.seen.lock().unwrap().len(), n, "同一對不再問");
        assert_eq!(inbox(&app).await.len(), hints.len());
    }

    /// #1467：專案在遠端主機（mac_builder）時，派工與認領兩個入口跟本機專案一樣問、一樣記帳本、一樣推提示——daemon 這一側
    /// 不看主機。（`agm issue claim` 從遠端 pane 送不進這支路由是另一回事：遠端沒有 `AM_PORT`，見 SPEC §4.3c。）
    #[tokio::test]
    async fn a_remote_hosts_project_is_compared_by_assignment_and_by_claim_like_a_local_one() {
        let rig = stand(Mode::Noul(0.8), true, true).await;
        let app = rig.env.app.clone();
        sqlx::query("UPDATE projects SET host='mac_builder' WHERE id=?").bind(&rig.env.project_id).execute(&app.db).await.unwrap();
        let bot = tt::claude_bot(&app, &rig.env.project_id, "ops-web").await;
        let other = tt::claude_bot(&app, &rig.env.project_id, "ops-master").await;
        crate::supervisor::store::insert_assignment(&app.db, None, &other.id, "old", "修 sidebar 的捲動 #1467", &[], None, true).await.unwrap();

        let a = crate::supervisor::store::insert_assignment(&app.db, None, &bot.id, "new", "修 sidebar 的捲動殘影 #1467", &[], None, true).await.unwrap();
        check_assignment(&app, &a.id).await.unwrap();
        assert_eq!(shadow_rows(&app).await.len(), 1, "派工入口問了");
        assert_eq!(inbox(&app).await.len(), 1);

        let ask = ClaimAsk {
            project_id: rig.env.project_id.clone(),
            caller: bot.id.clone(),
            issue: Some(1467),
            title: "修 sidebar 的捲動殘影".into(),
            child: "ops-web".into(),
            child_bot_id: None,
            claim_id: None,
            branch: None,
        };
        check_claim(&app, &ask).await.unwrap();
        let rows = shadow_rows(&app).await;
        assert_eq!(rows.len(), 2, "認領入口也問了：{rows:?}");
        assert!(rows.iter().all(|r| r.1 == Some(0.8) && r.2.is_none()));
    }

    #[tokio::test]
    async fn running_top_level_bot_titles_and_branches_are_claim_candidates_only_in_the_same_project() {
        let rig = stand(Mode::Noul(0.8), true, true).await;
        let app = rig.env.app.clone();
        let caller = tt::claude_bot(&app, &rig.env.project_id, "caller").await;

        let active = tt::claude_bot(&app, &rig.env.project_id, "active-top-level").await;
        let wt = rig.dir.join("top-level-wt");
        std::fs::create_dir_all(&wt).unwrap();
        write_branch(&wt, "fix/lx1468");
        sqlx::query("UPDATE bots SET cwd=? WHERE id=?").bind(wt.to_str().unwrap()).bind(&active.id).execute(&app.db).await.unwrap();
        let active_run = tt::fake_run(&app, &active.id).await;
        sqlx::query("UPDATE runs SET agent_title=? WHERE id=?")
            .bind("Top-level active title #1468")
            .bind(&active_run)
            .execute(&app.db)
            .await
            .unwrap();

        let idle = tt::claude_bot(&app, &rig.env.project_id, "idle-top-level").await;
        let idle_run = tt::fake_run(&app, &idle.id).await;
        sqlx::query("UPDATE runs SET state='stopped', agent_title=? WHERE id=?")
            .bind("Idle top-level title should stay out")
            .bind(&idle_run)
            .execute(&app.db)
            .await
            .unwrap();

        let other_project = db::ulid();
        sqlx::query("INSERT INTO projects (id,path,label,host,created_at) VALUES (?,'/tmp/other-project','other-project','local',?)")
            .bind(&other_project)
            .bind(db::now())
            .execute(&app.db)
            .await
            .unwrap();
        let foreign = tt::claude_bot(&app, &other_project, "foreign-top-level").await;
        let foreign_run = tt::fake_run(&app, &foreign.id).await;
        sqlx::query("UPDATE runs SET agent_title=? WHERE id=?")
            .bind("Foreign top-level title should stay out")
            .bind(&foreign_run)
            .execute(&app.db)
            .await
            .unwrap();

        let ask = ClaimAsk {
            project_id: rig.env.project_id.clone(),
            caller: caller.id.clone(),
            issue: Some(1468),
            title: "Candidate issue #1468 title".into(),
            child: caller.name.clone(),
            child_bot_id: None,
            claim_id: None,
            branch: None,
        };
        check_claim(&app, &ask).await.unwrap();

        let sent = rig.seen.lock().unwrap().clone();
        let blob = serde_json::to_string(&sent).unwrap();
        assert!(blob.contains("Top-level active title #1468"), "在跑的同專案頂層標題要作為候選：{blob}");
        assert!(blob.contains("fix/lx1468"), "在跑的同專案頂層分支要作為候選：{blob}");
        assert!(!blob.contains("Idle top-level title should stay out"), "閒置頂層 bot 不納入候選");
        assert!(!blob.contains("Foreign top-level title should stay out"), "其他專案頂層 bot 不納入候選");

        let sources: HashSet<String> = shadow_rows(&app)
            .await
            .into_iter()
            .filter_map(|(_, _, _, _, line)| serde_json::from_str::<Value>(&line).ok())
            .filter_map(|v| v.get("source").and_then(|s| s.as_str()).map(str::to_string))
            .collect();
        assert!(sources.contains("top_level_title"), "{sources:?}");
        assert!(sources.contains("top_level_branch"), "{sources:?}");
    }

    #[tokio::test]
    async fn a_remote_claim_spool_event_reaches_the_collision_check_for_its_project() {
        let rig = stand(Mode::Noul(0.8), true, true).await;
        let app = rig.env.app.clone();
        sqlx::query("UPDATE projects SET host='mac_builder' WHERE id=?").bind(&rig.env.project_id).execute(&app.db).await.unwrap();
        let caller = tt::claude_bot(&app, &rig.env.project_id, "remote-claimer").await;
        let existing = tt::claude_bot(&app, &rig.env.project_id, "remote-top-level").await;
        let run = tt::fake_run(&app, &existing.id).await;
        sqlx::query("UPDATE runs SET agent_title=? WHERE id=?")
            .bind("Remote top-level active #1468")
            .bind(&run)
            .execute(&app.db)
            .await
            .unwrap();

        let notified = rig.request_received.notified();
        let body = crate::hookrecv::HookBody {
            bot_id: caller.id.clone(),
            provider: "claude".into(),
            payload: json!({
                "hook_event_name": "AmClaimCollisionCheck",
                "issue": 1468,
                "title": "遠端認領工作 #1468",
                "child": "remote-claimer",
                "branch": "fix/lx1468",
            }),
            received_at: Some("2026-10-12T00:00:00.000Z".into()),
            truncated: false,
            run_id: None,
        };
        crate::hookrecv::process(&app, &body).await.unwrap();
        notified.await;

        let sent = rig.seen.lock().unwrap().clone();
        let blob = serde_json::to_string(&sent).unwrap();
        assert!(blob.contains("遠端認領工作 #1468"), "事件的標題送給 Jev：{blob}");
        assert!(blob.contains("Remote top-level active #1468"), "同專案活躍頂層候選送給 Jev：{blob}");
        assert!(!blob.contains(&caller.id), "不送 bot id");
        assert!(!blob.contains(&rig.env.project_id), "不送 project id");
    }

    #[tokio::test]
    async fn half_hints_and_just_under_only_records() {
        let rig = stand(Mode::Noul(0.5), true, true).await;
        let app = rig.env.app.clone();
        let bot = tt::claude_bot(&app, &rig.env.project_id, "a").await;
        let other = tt::claude_bot(&app, &rig.env.project_id, "b").await;
        crate::supervisor::store::insert_assignment(&app.db, None, &other.id, "old", "修 sidebar 的捲動", &[], None, true).await.unwrap();
        let a = crate::supervisor::store::insert_assignment(&app.db, None, &bot.id, "new", "修 sidebar 的捲動殘影", &[], None, true).await.unwrap();
        check_assignment(&app, &a.id).await.unwrap();
        assert_eq!(status_of(&app, &a.id).await, "queued");
        assert_eq!(shadow_rows(&app).await.len(), 1);
        assert_eq!(shadow_rows(&app).await[0].1, Some(0.5));
        assert_eq!(inbox(&app).await.len(), 1, "0.5 要提示");

        let rig = stand(Mode::Noul(0.49), true, true).await;
        let app = rig.env.app.clone();
        let bot = tt::claude_bot(&app, &rig.env.project_id, "a").await;
        let other = tt::claude_bot(&app, &rig.env.project_id, "b").await;
        crate::supervisor::store::insert_assignment(&app.db, None, &other.id, "old", "修 sidebar 的捲動", &[], None, true).await.unwrap();
        let a = crate::supervisor::store::insert_assignment(&app.db, None, &bot.id, "new", "修 sidebar 的捲動殘影", &[], None, true).await.unwrap();
        check_assignment(&app, &a.id).await.unwrap();
        assert_eq!(status_of(&app, &a.id).await, "queued");
        assert_eq!(shadow_rows(&app).await[0].1, Some(0.49));
        assert!(inbox(&app).await.is_empty(), "0.49 只記帳");
    }

    #[tokio::test]
    async fn timeout_and_http_failure_record_an_error_and_do_not_hint_or_block() {
        for mode in [Mode::Hang, Mode::Status(500)] {
            let rig = stand(mode, true, true).await;
            let app = rig.env.app.clone();
            let bot = tt::claude_bot(&app, &rig.env.project_id, "a").await;
            let other = tt::claude_bot(&app, &rig.env.project_id, "b").await;
            crate::supervisor::store::insert_assignment(&app.db, None, &other.id, "old", "修 quota 橫幅", &[], None, true).await.unwrap();
            let a = crate::supervisor::store::insert_assignment(&app.db, None, &bot.id, "new", "修 quota 橫幅的誤判", &[], None, true).await.unwrap();
            check_assignment(&app, &a.id).await.unwrap();
            assert_eq!(status_of(&app, &a.id).await, "queued");
            let rows = shadow_rows(&app).await;
            assert_eq!(rows.len(), 1);
            assert!(rows[0].0.is_none() && rows[0].1.is_none());
            let err = rows[0].2.as_deref().unwrap_or("");
            assert!(err.contains("request failed") || err.contains("http 500"), "{err}");
            assert!(inbox(&app).await.is_empty(), "失敗不推提示");
        }
    }

    #[tokio::test]
    async fn disabled_unlisted_missing_key_and_notices_skip_quietly() {
        let rig = stand(Mode::Noul(0.99), false, true).await;
        let app = rig.env.app.clone();
        let bot = tt::claude_bot(&app, &rig.env.project_id, "a").await;
        let other = tt::claude_bot(&app, &rig.env.project_id, "b").await;
        crate::supervisor::store::insert_assignment(&app.db, None, &other.id, "old", "做 A", &[], None, true).await.unwrap();
        let a = crate::supervisor::store::insert_assignment(&app.db, None, &bot.id, "new", "做 A 的另一面", &[], None, true).await.unwrap();
        check_assignment(&app, &a.id).await.unwrap();
        assert!(rig.seen.lock().unwrap().is_empty());
        assert!(shadow_rows(&app).await.is_empty());

        let rig = stand(Mode::Noul(0.99), true, false).await;
        let app = rig.env.app.clone();
        let bot = tt::claude_bot(&app, &rig.env.project_id, "a").await;
        let other = tt::claude_bot(&app, &rig.env.project_id, "b").await;
        crate::supervisor::store::insert_assignment(&app.db, None, &other.id, "old", "做 A", &[], None, true).await.unwrap();
        let a = crate::supervisor::store::insert_assignment(&app.db, None, &bot.id, "new", "做 A 的另一面", &[], None, true).await.unwrap();
        check_assignment(&app, &a.id).await.unwrap();
        assert!(rig.seen.lock().unwrap().is_empty() && shadow_rows(&app).await.is_empty(), "專案不在名單");

        let rig = stand(Mode::Noul(0.99), true, true).await;
        let app = rig.env.app.clone();
        app.cfg.update(|c| { c.judge.key_file = "/tmp/am-judge-no-such-key".into(); Ok(()) }).await.unwrap();
        let bot = tt::claude_bot(&app, &rig.env.project_id, "a").await;
        let other = tt::claude_bot(&app, &rig.env.project_id, "b").await;
        crate::supervisor::store::insert_assignment(&app.db, None, &other.id, "old", "做 A", &[], None, true).await.unwrap();
        let a = crate::supervisor::store::insert_assignment(&app.db, None, &bot.id, "new", "做 A 的另一面", &[], None, true).await.unwrap();
        check_assignment(&app, &a.id).await.unwrap();
        assert!(rig.seen.lock().unwrap().is_empty() && shadow_rows(&app).await.is_empty(), "沒有 key 不寫帳");

        let rig = stand(Mode::Noul(0.99), true, true).await;
        let app = rig.env.app.clone();
        let bot = tt::claude_bot(&app, &rig.env.project_id, "a").await;
        let other = tt::claude_bot(&app, &rig.env.project_id, "b").await;
        crate::supervisor::store::insert_assignment(&app.db, None, &other.id, "old", "做 A", &[], None, true).await.unwrap();
        let note = crate::supervisor::store::insert_assignment(&app.db, None, &bot.id, "note", "收到，進 idle", &[], None, false).await.unwrap();
        check_assignment(&app, &note.id).await.unwrap();
        assert!(rig.seen.lock().unwrap().is_empty(), "通知不問");
    }

    #[tokio::test]
    async fn schedule_returns_while_jev_is_still_hanging_and_the_assignment_stays_queued() {
        let rig = stand(Mode::Hang, true, true).await;
        let app = rig.env.app.clone();
        app.cfg.update(|c| { c.judge.timeout_ms = 60_000; Ok(()) }).await.unwrap();
        let bot = tt::claude_bot(&app, &rig.env.project_id, "a").await;
        let other = tt::claude_bot(&app, &rig.env.project_id, "b").await;
        crate::supervisor::store::insert_assignment(&app.db, None, &other.id, "old", "做 A", &[], None, true).await.unwrap();
        let a = crate::supervisor::store::insert_assignment(&app.db, None, &bot.id, "new", "做 A 的另一面", &[], None, true).await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(10), schedule_assignment(&app, &a.id)).await.expect("schedule 不能等 Jev 回來");
        assert_eq!(status_of(&app, &a.id).await, "queued");
        assert!(shadow_rows(&app).await.is_empty() || shadow_rows(&app).await[0].2.as_deref() == Some("pending") || shadow_rows(&app).await[0].1.is_none());
    }

    /// 真的走 `supervisor::assign`，Jev 卡住、逾時 60 秒：派工要在 10 秒內回來（不拿牆鐘毫秒數當斷言，issue #948）。
    #[tokio::test]
    async fn assign_returns_while_jev_hangs() {
        let rig = stand(Mode::Hang, true, true).await;
        let app = rig.env.app.clone();
        app.cfg.update(|c| { c.judge.timeout_ms = 60_000; Ok(()) }).await.unwrap();
        let bot = tt::claude_bot(&app, &rig.env.project_id, "a").await;
        let other = tt::claude_bot(&app, &rig.env.project_id, "b").await;
        crate::supervisor::store::insert_assignment(&app.db, None, &other.id, "old", "修 quota 橫幅", &[], None, true).await.unwrap();
        let manager = tt::claude_bot(&app, &rig.env.project_id, "agm").await;
        crate::supervisor::store::get_or_init(&app.db).await.unwrap();
        sqlx::query("UPDATE supervisors SET bot_id=? WHERE id=?")
            .bind(&manager.id)
            .bind(crate::supervisor::store::SUPERVISOR_ID)
            .execute(&app.db)
            .await
            .unwrap();
        let out = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            crate::supervisor::assign(&app, &bot.id, "修 quota 橫幅的誤判", "crid-948", None, &[], None, true, None, None, Default::default()),
        )
        .await
        .expect("派工不能等 Jev")
        .unwrap();
        assert!(out["ownership_conflicts"].as_array().is_some_and(|v| v.is_empty()), "撞題不併進 ownership_conflicts");
    }

    /// 真的走 `supervisor::assign`：派工照常回來、交辦照常建，撞題的問答在背景才落帳。
    #[tokio::test]
    async fn assign_returns_before_jev_answers_and_the_pair_lands_in_the_ledger() {
        let rig = stand(Mode::Hang, true, true).await;
        let app = rig.env.app.clone();
        app.cfg.update(|c| { c.judge.timeout_ms = 1500; Ok(()) }).await.unwrap();
        let bot = tt::claude_bot(&app, &rig.env.project_id, "a").await;
        let other = tt::claude_bot(&app, &rig.env.project_id, "b").await;
        crate::supervisor::store::insert_assignment(&app.db, None, &other.id, "old", "修 quota 橫幅", &[], None, true).await.unwrap();
        let manager = tt::claude_bot(&app, &rig.env.project_id, "agm").await;
        crate::supervisor::store::get_or_init(&app.db).await.unwrap();
        sqlx::query("UPDATE supervisors SET bot_id=? WHERE id=?")
            .bind(&manager.id)
            .bind(crate::supervisor::store::SUPERVISOR_ID)
            .execute(&app.db)
            .await
            .unwrap();
        let out = crate::supervisor::assign(&app, &bot.id, "修 quota 橫幅的誤判", "crid-557", None, &[], None, true, None, None, Default::default())
            .await
            .unwrap();
        assert!(out["ownership_conflicts"].as_array().is_some_and(|v| v.is_empty()), "撞題不併進 ownership_conflicts");
        let deadline = Instant::now() + std::time::Duration::from_secs(30);
        let row = loop {
            let rows = shadow_rows(&app).await;
            if rows.first().is_some_and(|r| r.2.as_deref() != Some("pending")) {
                break rows[0].clone();
            }
            assert!(Instant::now() < deadline, "背景 task 沒有落帳：{rows:?}");
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        };
        assert!(row.1.is_none() && row.2.as_deref().unwrap_or("").contains("request failed"), "{row:?}");
        let id = out["id"].as_str().unwrap();
        let status = status_of(&app, id).await;
        assert!(status != "failed" && status != "cancelled", "逾時不影響交辦：{status}");
        assert!(inbox(&app).await.is_empty());
    }

    /// #568：真的走 `supervisor::assign`，派送當場就 `dispatch_failed`（整段是終端控制序列，清完什麼都不剩）。
    /// 那筆已經停在 `awaiting_review`、不會開始了：不能再被當成「有人要開始做的工作」去問 Jev、推提示。
    #[tokio::test]
    async fn an_assignment_whose_dispatch_failed_on_the_spot_is_not_asked_or_hinted() {
        let rig = stand(Mode::Noul(0.99), true, true).await;
        let app = rig.env.app.clone();
        let bot = tt::claude_bot(&app, &rig.env.project_id, "a").await;
        let other = tt::claude_bot(&app, &rig.env.project_id, "b").await;
        crate::supervisor::store::insert_assignment(&app.db, None, &other.id, "old", "修 quota 橫幅", &[], None, true).await.unwrap();
        let manager = tt::claude_bot(&app, &rig.env.project_id, "agm").await;
        crate::supervisor::store::get_or_init(&app.db).await.unwrap();
        sqlx::query("UPDATE supervisors SET bot_id=? WHERE id=?")
            .bind(&manager.id)
            .bind(crate::supervisor::store::SUPERVISOR_ID)
            .execute(&app.db)
            .await
            .unwrap();
        let out = crate::supervisor::assign(&app, &bot.id, "\u{1b}[2J\u{1b}[31m", "crid-568", None, &[], None, true, None, None, Default::default())
            .await
            .unwrap();
        let id = out["id"].as_str().unwrap().to_string();
        let a = crate::supervisor::store::assignment(&app.db, &id).await.unwrap().unwrap();
        assert_eq!((a.status.as_str(), a.turn_status.as_deref()), ("awaiting_review", Some("dispatch_failed")), "前提：派送當場失敗");
        check_assignment(&app, &id).await.unwrap();
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        assert!(rig.seen.lock().unwrap().is_empty(), "不會開始的交辦不問 Jev");
        assert!(shadow_rows(&app).await.is_empty(), "也不占帳本");
        assert!(inbox(&app).await.is_empty(), "不推撞題提示");
    }

    /// 假 Jev：收到請求就卡住，直到 `release` 放行才回 `p`。
    async fn gated_jev(p: f64) -> (String, Arc<std::sync::Mutex<Vec<Value>>>, Arc<tokio::sync::Semaphore>) {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let gate = Arc::new(tokio::sync::Semaphore::new(0));
        let (log, g) = (seen.clone(), gate.clone());
        let route = axum::routing::post(move |axum::Json(body): axum::Json<Value>| {
            let (log, g) = (log.clone(), g.clone());
            async move {
                log.lock().unwrap().push(body);
                g.acquire().await.unwrap().forget();
                axum::Json(json!({"model": "jev-1.13.0", "answers": {"same_work": {"type": "noul", "noul": p}}, "usage": {"input_tokens": 400}}))
            }
        });
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/v1/systemone", listener.local_addr().unwrap());
        tokio::spawn(async move { axum::serve(listener, axum::Router::new().route("/v1/systemone", route)).await.unwrap() });
        (url, seen, gate)
    }

    /// #568：Jev 還在回的時候交辦被取消／收掉，答案回來就不能再推提示；帳本留下這次觀察，但標成過時。
    /// 對照組：在途之間的正常推進（送達、等額度）不是過時，照樣提示——只看 `updated_at` 變沒變的話這兩條會被誤殺。
    #[tokio::test]
    async fn a_lifecycle_change_while_jev_is_answering_suppresses_the_hint_and_marks_the_row_stale() {
        use crate::supervisor::assignment_state::AssignmentState as S;
        for (to, stale) in [
            (S::Cancelled, true),
            (S::AwaitingReview, true),
            (S::Completed, true),
            (S::Superseded, true),
            (S::Blocked, true),
            (S::Delivered, false),
            (S::QuotaBlocked, false),
        ] {
            let rig = stand(Mode::Noul(0.0), true, true).await;
            let app = rig.env.app.clone();
            let (url, seen, gate) = gated_jev(0.93).await;
            app.cfg.update(move |c| { c.judge.endpoint = url; c.judge.timeout_ms = 10_000; Ok(()) }).await.unwrap();
            let bot = tt::claude_bot(&app, &rig.env.project_id, "a").await;
            let other = tt::claude_bot(&app, &rig.env.project_id, "b").await;
            crate::supervisor::store::insert_assignment(&app.db, None, &other.id, "old", "修 sidebar 的捲動", &[], None, true).await.unwrap();
            let a = crate::supervisor::store::insert_assignment(&app.db, None, &bot.id, "new", "修 sidebar 的捲動殘影", &[], None, true).await.unwrap();

            let task = {
                let (app, id) = (app.clone(), a.id.clone());
                tokio::spawn(async move { check_assignment(&app, &id).await })
            };
            let deadline = Instant::now() + std::time::Duration::from_secs(5);
            while seen.lock().unwrap().is_empty() {
                assert!(Instant::now() < deadline, "Jev 沒被問到");
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            let moved = crate::supervisor::assignment_state::set_status(&app.db, &a.id, S::Queued, to, "test").await.unwrap();
            assert!(matches!(moved, crate::supervisor::assignment_state::Outcome::Applied), "{to:?}");
            gate.add_permits(16);
            task.await.unwrap().unwrap();

            let rows: Vec<(Option<f64>, String)> = sqlx::query_as("SELECT jev_same_work, matched_line FROM judge_shadow WHERE regex_verdict='same_work'")
                .fetch_all(&app.db)
                .await
                .unwrap();
            assert_eq!(rows.len(), 1, "{to:?}");
            assert_eq!(rows[0].0, Some(0.93), "{to:?}：觀察照記");
            let line: Value = serde_json::from_str(&rows[0].1).unwrap();
            if stale {
                assert!(inbox(&app).await.is_empty(), "{to:?}：已經不會開始的交辦不推提示");
                assert_eq!(line["stale"].as_str(), Some(to.as_str()), "{to:?}：帳本分得出這筆是過時的觀察");
            } else {
                assert_eq!(inbox(&app).await.len(), 1, "{to:?}：在途的正常推進照樣提示");
                assert!(line.get("stale").is_none(), "{to:?}");
            }
        }
    }

    /// 答案回來之後只重讀狀態、不帶世代戳記，靠的是這個性質：離開可開工的狀態就回不來。
    /// 狀態機哪天多了一條回頭邊（例如 `blocked → queued`），這裡要先紅，逼人補上真的世代比對。
    #[test]
    fn once_out_of_the_startable_states_an_assignment_never_comes_back() {
        for from in crate::supervisor::assignment_state::ALL {
            if STARTABLE.contains(&from) {
                continue;
            }
            for to in STARTABLE {
                assert!(!crate::supervisor::assignment_state::allowed(from, to), "{from} → {to}");
            }
        }
    }


    #[tokio::test]
    async fn a_burst_asks_at_most_fifteen_and_keeps_the_shared_issue() {
        let rig = stand(Mode::Noul(0.2), true, true).await;
        let app = rig.env.app.clone();
        let bot = tt::claude_bot(&app, &rig.env.project_id, "cand").await;
        for i in 0..16 {
            let other = tt::claude_bot(&app, &rig.env.project_id, &format!("o{i}")).await;
            crate::supervisor::store::insert_assignment(
                &app.db,
                None,
                &other.id,
                &format!("old-{i}"),
                &format!("unrelated padding fix number {i} in web/src/pad{i}.tsx"),
                &[],
                None,
                true,
            )
            .await
            .unwrap();
        }
        let shared_bot = tt::claude_bot(&app, &rig.env.project_id, "shared").await;
        crate::supervisor::store::insert_assignment(
            &app.db,
            None,
            &shared_bot.id,
            "old-shared",
            "SHARED-557 跟 #557 是同一張票 daemon/src/judge.rs",
            &[],
            None,
            true,
        )
        .await
        .unwrap();
        let a = crate::supervisor::store::insert_assignment(
            &app.db,
            None,
            &bot.id,
            "new",
            "候選也是 #557，改 daemon/src/judge/collision.rs",
            &[],
            None,
            true,
        )
        .await
        .unwrap();
        check_assignment(&app, &a.id).await.unwrap();
        let sent = rig.seen.lock().unwrap().clone();
        assert_eq!(sent.len(), MAX_PAIRS, "一次最多 15 對");
        let blob = serde_json::to_string(&sent).unwrap();
        assert!(blob.contains("SHARED-557"), "票號重疊的那張要排進名額");
        assert_eq!(status_of(&app, &a.id).await, "queued");
        assert!(inbox(&app).await.is_empty(), "0.2 不提示");
    }

    #[tokio::test]
    async fn an_opened_issue_is_compared_the_same_way() {
        let rig = stand(Mode::Noul(0.81), true, true).await;
        let app = rig.env.app.clone();
        let pid = rig.env.project_id.clone();
        app.cfg.update(move |c| { c.release_triage.repo = Some("owner/proj".into()); c.release_triage.project_ids = vec![pid]; Ok(()) }).await.unwrap();
        let parent = tt::claude_bot(&app, &rig.env.project_id, "parent").await;
        let wt = rig.dir.join("wt2");
        std::fs::create_dir_all(&wt).unwrap();
        write_branch(&wt, "fix/quota-g404");
        child(&app, &rig.env.project_id, &parent.id, "sib", wt.to_str().unwrap(), "舊快照蓋掉額度").await;
        let before = Vec::new();
        let after = sample_row(404);
        let cands = opened_candidates(&before, &after);
        assert_eq!(cands.len(), 1);
        assert_eq!(cands[0].key, "issue:404");
        assert!(cands[0].summary.contains("screen.rs") || cands[0].touches.iter().any(|p| p.contains("screen.rs")));
        check_opened(&app, &before, &after).await.unwrap();
        let sent = rig.seen.lock().unwrap().clone();
        assert!(!sent.is_empty());
        let blob = serde_json::to_string(&sent).unwrap();
        assert!(blob.contains("quota banner") || blob.contains("claude 2.1.277"));
        assert!(blob.contains("fix/quota-g404") || blob.contains("issue #404") || blob.contains("#404"));
        assert!(!inbox(&app).await.is_empty());
        assert!(shadow_rows(&app).await.iter().any(|r| r.1 == Some(0.81)));
        let n = sent.len();
        check_opened(&app, &after.issues, &after).await.unwrap();
        assert_eq!(rig.seen.lock().unwrap().len(), n);
    }

    /// 名稱不當參照：專案 label／路徑末段剛好等於 repo 名也不算「同專案」，只有 `project_ids` 明講的才是；
    /// 沒設就不問。
    #[tokio::test]
    async fn an_issue_is_only_compared_with_the_configured_project_ids_not_with_a_same_named_project() {
        let rig = stand(Mode::Noul(0.81), true, true).await;
        let app = rig.env.app.clone();
        let label: String = sqlx::query_scalar("SELECT label FROM projects WHERE id = ?").bind(&rig.env.project_id).fetch_one(&app.db).await.unwrap();
        let repo = format!("owner/{label}");
        app.cfg.update(move |c| { c.release_triage.repo = Some(repo); Ok(()) }).await.unwrap();
        let parent = tt::claude_bot(&app, &rig.env.project_id, "parent").await;
        child(&app, &rig.env.project_id, &parent.id, "sib", rig.dir.to_str().unwrap(), "舊快照蓋掉額度").await;
        check_opened(&app, &[], &sample_row(404)).await.unwrap();
        assert!(rig.seen.lock().unwrap().is_empty(), "label 對得上 repo 名、但沒設 project_ids：不問");
        assert!(shadow_rows(&app).await.is_empty() && inbox(&app).await.is_empty());

        app.cfg.update(|c| { c.release_triage.project_ids = vec!["no-such-project".into()]; Ok(()) }).await.unwrap();
        check_opened(&app, &[], &sample_row(404)).await.unwrap();
        assert!(rig.seen.lock().unwrap().is_empty(), "設的專案 ID 不存在：不問，也不退回用名字猜");
    }

    fn sample_row(number: i64) -> Row {
        let entry = Entry {
            id: "e1".into(),
            text: "The limit line is matched inside a diff".into(),
            bucket: Bucket::Kept,
            categories: vec![],
            rules: vec![],
        };
        let marker = crate::release_triage::issue::marker("claude", "2.1.277", &["e1".into()]);
        Row {
            kind: "claude".into(),
            version: "2.1.277".into(),
            status: Status::Published,
            assigned_bot_id: None,
            dispatch_gen: 0,
            entries: vec![entry],
            verdicts: Some(json!({"issues": [{
                "entry_ids": ["e1"],
                "triage": "adopt",
                "title": "quota banner is parsed inside source",
                "goal": "stop matching the limit line inside printed source in daemon/src/lifecycle/screen.rs",
                "suggestion": "tighten the matcher",
                "acceptance": "a diff containing the line is not a hit"
            }]})),
            issues: vec![IssueRef {
                marker,
                entry_ids: vec!["e1".into()],
                number,
                url: format!("https://example.test/{number}"),
                created_at: "2026-09-25T00:00:00.000Z".into(),
                comment: false,
            }],
            dispatched_at: None,
            attempts: 1,
            publish_error: None,
            created_at: "2026-09-25T00:00:00.000Z".into(),
            updated_at: "2026-09-25T00:00:00.000Z".into(),
        }
    }

    /// #1330：認領時的撞題。候選＝呼叫端專案裡、`child` 以外的在跑工作。
    /// sibling 的工作目錄放在呼叫端的 `dir`（`Rig` 的暫存目錄，`Rig::drop` 會刪），不碰系統暫存目錄（#1387）。
    async fn claim_setup(app: &Arc<App>, project: &str, dir: &std::path::Path, sibling_title: &str) -> (db::Bot, db::Bot) {
        let parent = tt::claude_bot(app, project, "parent").await;
        let wt = dir.join("claim-sib");
        std::fs::create_dir_all(&wt).unwrap();
        child(app, project, &parent.id, "sib", wt.to_str().unwrap(), sibling_title).await;
        let newbie = tt::claude_bot(app, project, "newbie").await;
        sqlx::query("UPDATE bots SET parent_bot_id=? WHERE id=?").bind(&parent.id).bind(&newbie.id).execute(&app.db).await.unwrap();
        (parent, newbie)
    }

    fn claim_ask(project: &str, caller: &str, child_bot_id: Option<&str>) -> ClaimAsk {
        ClaimAsk {
            project_id: project.into(),
            caller: caller.into(),
            issue: Some(1330),
            title: "修卡住的畫面判斷".into(),
            child: "newbie".into(),
            child_bot_id: child_bot_id.map(String::from),
            claim_id: Some("claim-1330".into()),
            branch: Some("fix/newbie-g1330".into()),
        }
    }

    #[tokio::test]
    async fn a_claim_at_or_above_half_hints_once_per_pair_and_never_blocks() {
        let rig = stand(Mode::Noul(0.72), true, true).await;
        let app = rig.env.app.clone();
        let (parent, newbie) = claim_setup(&app, &rig.env.project_id, &rig.dir, "卡住的畫面判斷 #1330").await;
        let ask = || claim_ask(&rig.env.project_id, &parent.id, Some(&newbie.id));
        check_claim(&app, &ask()).await.unwrap();
        let rows = shadow_rows(&app).await;
        assert_eq!(rows.len(), 1, "{rows:?}");
        assert_eq!(rows[0].1, Some(0.72));
        assert_eq!(inbox(&app).await.len(), 1, "≥0.5 推一則提示");
        check_claim(&app, &ask()).await.unwrap();
        assert_eq!(rig.seen.lock().unwrap().len(), 1, "同一對只問一次");
        assert_eq!(shadow_rows(&app).await.len(), 1);
    }

    #[tokio::test]
    async fn a_claim_below_half_only_lands_in_the_ledger() {
        let rig = stand(Mode::Noul(0.3), true, true).await;
        let app = rig.env.app.clone();
        let (parent, newbie) = claim_setup(&app, &rig.env.project_id, &rig.dir, "卡住的畫面判斷 #1330").await;
        check_claim(&app, &claim_ask(&rig.env.project_id, &parent.id, Some(&newbie.id))).await.unwrap();
        assert_eq!(shadow_rows(&app).await.len(), 1, "帳本照寫");
        assert!(inbox(&app).await.is_empty(), "<0.5 不推提示");
    }

    #[tokio::test]
    async fn a_claim_asks_nothing_when_jev_is_off_or_the_project_is_not_listed() {
        for (enabled, listed) in [(false, true), (true, false)] {
            let rig = stand(Mode::Noul(0.9), enabled, listed).await;
            let app = rig.env.app.clone();
            let (parent, newbie) = claim_setup(&app, &rig.env.project_id, &rig.dir, "卡住的畫面判斷 #1330").await;
            check_claim(&app, &claim_ask(&rig.env.project_id, &parent.id, Some(&newbie.id))).await.unwrap();
            assert!(rig.seen.lock().unwrap().is_empty(), "enabled={enabled} listed={listed}：不問");
            assert!(shadow_rows(&app).await.is_empty(), "enabled={enabled} listed={listed}：不寫帳本");
        }
    }

    #[tokio::test]
    async fn a_claim_never_compares_the_child_with_its_own_running_work() {
        let rig = stand(Mode::Noul(0.9), true, true).await;
        let app = rig.env.app.clone();
        let (parent, newbie) = claim_setup(&app, &rig.env.project_id, &rig.dir, "別的工作 #77").await;
        let own_run = tt::fake_run(&app, &newbie.id).await;
        sqlx::query("UPDATE runs SET agent_title=? WHERE id=?").bind("修卡住的畫面判斷 自己的 SELF-1330").bind(&own_run).execute(&app.db).await.unwrap();
        crate::supervisor::store::insert_assignment(&app.db, None, &newbie.id, "own-1330", "SELF-ASSIGNMENT 還沒驗收", &[], None, true).await.unwrap();
        check_claim(&app, &claim_ask(&rig.env.project_id, &parent.id, Some(&newbie.id))).await.unwrap();
        let asked = rig.seen.lock().unwrap().clone();
        assert_eq!(asked.len(), 1, "只問 sib，不問 child 自己的卡：{asked:?}");
        let text = asked[0].to_string();
        assert!(!text.contains("SELF-1330") && !text.contains("SELF-ASSIGNMENT"), "{text}");
    }

    #[tokio::test]
    async fn a_claim_whose_child_is_not_a_bot_here_is_recorded_under_the_caller() {
        let rig = stand(Mode::Noul(0.9), true, true).await;
        let app = rig.env.app.clone();
        let (parent, _newbie) = claim_setup(&app, &rig.env.project_id, &rig.dir, "卡住的畫面判斷 #1330").await;
        check_claim(&app, &claim_ask(&rig.env.project_id, &parent.id, Some("ghost-child-id"))).await.unwrap();
        let bot: String = sqlx::query_scalar("SELECT bot_id FROM judge_shadow WHERE regex_verdict='same_work' LIMIT 1").fetch_one(&app.db).await.unwrap();
        assert_eq!(bot, parent.id, "對不上 bot 時記在呼叫端名下");
        assert_eq!(rig.seen.lock().unwrap().len(), 1);
    }

    /// 名稱不當參照：child 只認 bot ID。bot 改名後，帶 ID 的認領照樣把它自己的卡排除；
    /// 只有名字（沒帶 ID）時，即使專案裡剛好有同名的 bot 也不當成 child——記在呼叫端名下，那顆同名 bot 的卡照常拿來比。
    #[tokio::test]
    async fn a_claim_finds_its_child_by_bot_id_never_by_name() {
        let rig = stand(Mode::Noul(0.9), true, true).await;
        let app = rig.env.app.clone();
        let (parent, newbie) = claim_setup(&app, &rig.env.project_id, &rig.dir, "別的工作 #77").await;
        let own_run = tt::fake_run(&app, &newbie.id).await;
        sqlx::query("UPDATE runs SET agent_title=? WHERE id=?").bind("修卡住的畫面判斷 自己的 SELF-1330").bind(&own_run).execute(&app.db).await.unwrap();
        sqlx::query("UPDATE bots SET name='renamed-newbie' WHERE id=?").bind(&newbie.id).execute(&app.db).await.unwrap();

        check_claim(&app, &claim_ask(&rig.env.project_id, &parent.id, Some(&newbie.id))).await.unwrap();
        let text = rig.seen.lock().unwrap().iter().map(|v| v.to_string()).collect::<String>();
        assert!(!text.contains("SELF-1330"), "改名後仍靠 ID 認得 child，自己的卡不比：{text}");
        let bot: String = sqlx::query_scalar("SELECT bot_id FROM judge_shadow WHERE regex_verdict='same_work' LIMIT 1").fetch_one(&app.db).await.unwrap();
        assert_eq!(bot, newbie.id);

        let impostor = tt::claude_bot(&app, &rig.env.project_id, "newbie").await;
        let impostor_run = tt::fake_run(&app, &impostor.id).await;
        sqlx::query("UPDATE runs SET agent_title=? WHERE id=?").bind("修卡住的畫面判斷 同名者 NAMESAKE-1330").bind(&impostor_run).execute(&app.db).await.unwrap();
        let mut ask = claim_ask(&rig.env.project_id, &parent.id, None);
        ask.claim_id = Some("claim-by-name-only".into());
        check_claim(&app, &ask).await.unwrap();
        let text = rig.seen.lock().unwrap().iter().map(|v| v.to_string()).collect::<String>();
        assert!(text.contains("NAMESAKE-1330"), "沒帶 ID 不用名字認 child，同名那顆的卡照常比：{text}");
        let bots: Vec<String> = sqlx::query_scalar("SELECT bot_id FROM judge_shadow WHERE regex_verdict='same_work' AND run_id LIKE 'claim:claim-by-name-only%'").fetch_all(&app.db).await.unwrap();
        assert!(!bots.is_empty() && bots.iter().all(|b| *b == parent.id), "記在呼叫端名下：{bots:?}");
    }

    /// #1421：移交出去的專案（`projects.handed_off_to`）不問撞題：不呼叫 Jev、不寫帳本、不推提示。
    /// 認領路徑與派工路徑共用 `check_candidate`，兩邊都要擋。
    #[tokio::test]
    async fn a_handed_off_project_is_not_asked_about_a_claim() {
        let rig = stand(Mode::Noul(0.9), true, true).await;
        let app = rig.env.app.clone();
        let (parent, newbie) = claim_setup(&app, &rig.env.project_id, &rig.dir, "卡住的畫面判斷 #1330").await;
        sqlx::query("UPDATE projects SET handed_off_to='remote' WHERE id=?").bind(&rig.env.project_id).execute(&app.db).await.unwrap();
        check_claim(&app, &claim_ask(&rig.env.project_id, &parent.id, Some(&newbie.id))).await.unwrap();
        assert!(rig.seen.lock().unwrap().is_empty(), "移交中的專案不問 Jev");
        assert!(shadow_rows(&app).await.is_empty(), "不寫帳本");
        assert!(inbox(&app).await.is_empty(), "不推提示");
    }

    #[tokio::test]
    async fn a_handed_off_project_is_not_asked_about_an_assignment() {
        let rig = stand(Mode::Noul(0.9), true, true).await;
        let app = rig.env.app.clone();
        let bot = tt::claude_bot(&app, &rig.env.project_id, "a").await;
        let other = tt::claude_bot(&app, &rig.env.project_id, "b").await;
        crate::supervisor::store::insert_assignment(&app.db, None, &other.id, "old", "修 sidebar 的捲動", &[], None, true).await.unwrap();
        let a = crate::supervisor::store::insert_assignment(&app.db, None, &bot.id, "new", "修 sidebar 的捲動殘影", &[], None, true).await.unwrap();
        sqlx::query("UPDATE projects SET handed_off_to='remote' WHERE id=?").bind(&rig.env.project_id).execute(&app.db).await.unwrap();
        check_assignment(&app, &a.id).await.unwrap();
        assert!(rig.seen.lock().unwrap().is_empty(), "移交中的專案不問 Jev");
        assert!(shadow_rows(&app).await.is_empty(), "不寫帳本");
        assert!(inbox(&app).await.is_empty(), "不推提示");
        assert_eq!(status_of(&app, &a.id).await, "queued", "交辦狀態不動");
    }
}

}
