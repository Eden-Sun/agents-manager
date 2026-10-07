    use super::*;
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
    fn secrets_are_masked_and_layout_is_kept() {
        let m = |s: &str| mask(s);
        assert_eq!(m("  export GITHUB_TOKEN=ghp_abcdefghijklmnopqrstuvwxyz0123456789"), format!("  export GITHUB_TOKEN={REDACTED}"));
        assert_eq!(m("key sk-ant-REDACTED done"), format!("key {REDACTED} done"));
        assert_eq!(m("Authorization: Bearer abc.def.ghi"), format!("Authorization: Bearer {REDACTED}"));
        assert_eq!(m("db_password: hunter2 # x"), format!("db_password: {REDACTED} # x"));
        assert_eq!(m("DB_PASSWORD=hunter2"), format!("DB_PASSWORD={REDACTED}"));
        assert_eq!(m("mail someone@example.com now"), format!("mail {REDACTED} now"));
        assert_eq!(m("  gpt-5.6-luna xhigh · /Users/m4p/project/agents-manager"), "  gpt-5.6-luna xhigh · ~/project/agents-manager");
        assert_eq!(m("  gpt-6-luna max · /home/ubuntu/project/agents-manager"), "  gpt-6-luna max · ~/project/agents-manager");
        assert_eq!(m("cd:/home/ubuntu"), "cd:~");
        assert_eq!(m("see https://example.com/home/feed"), "see https://example.com/home/feed", "網址的 /home/ 不是家目錄");
        assert_eq!(m("token 9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08"), format!("token {REDACTED}"));
        assert_eq!(m("a\n-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaA\n-----END OPENSSH PRIVATE KEY-----\nz"), format!("a\n{REDACTED}\nz"));
        // 撞限橫幅與一般程式碼原樣通過——遮掉了 Jev 就沒東西可判。
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
        // 認證方式本身留著：遮掉版面等於把 Jev 要判的東西一起拿走。
        assert!(out.contains("Bearer"), "{out}");

        // 沒有 Bearer、單獨出現的 JWT 也要遮（log、curl 的 -H 拆行、程式碼字面值都會這樣）。
        assert_eq!(mask(&format!("token={jwt}")), format!("token={REDACTED}"));
        assert_eq!(mask(jwt), REDACTED);
        assert_eq!(mask(&format!("  header: {jwt} ok")), format!("  header: {REDACTED} ok"));

        // 空白分隔的那一版（既有行為）不能退步。
        assert_eq!(mask(&format!("Authorization: Bearer {jwt}")), format!("Authorization: Bearer {REDACTED}"));

        // 名字認得出來、值直接接在後面時也遮：`"Authorization":` 去掉包裹標點才對得上。
        let out = mask(r#"{"authorization": "abcdefghijklmnopqrstuvwxyz0123456789"}"#);
        assert!(!out.contains("abcdefghijklmnopqrstuvwxyz0123456789"), "{out}");
        assert!(mask(r#"cookie: "sess_abcdefghijklmnopqrstuvwxyz01234567""#).contains(REDACTED));
        // `Token <值>`（GitHub 那種寫法）：方式留著、值遮掉。
        assert_eq!(mask("Authorization: Token ghp_abcdefghijklmnopqrstuvwxyz0123"), format!("Authorization: Token {REDACTED}"));
    }

    /// issue #451 的跟進審核（i407）：四個反向缺口。字串全是捏造的。
    #[test]
    fn bare_schemes_query_strings_userinfo_and_jwe_are_masked_too() {
        // 1) 裸寫的 Basic／Token／Digest：以前只有 `BEARER` 點得起 `secret_next`，
        //    所以 `Authorization: Basic X` 有遮（靠前面那個字接力），單獨一行的 `Basic X` 沒遮。
        assert_eq!(mask("Basic dXNlcjpwYXNzd29yZA=="), format!("Basic {REDACTED}"));
        assert_eq!(mask("Token ghp_abcdefghijklmnopqrstuvwxyz0123"), format!("Token {REDACTED}"));
        assert_eq!(mask("Digest username=admin"), format!("Digest {REDACTED}"));

        // 2) query string：整串 URL 是同一個片段，第一個冒號是 scheme 的，名字比對不到；
        //    `/`／`?`／`.` 又讓 looks_random 回 false。只換值、其餘原樣。
        assert_eq!(
            mask("https://h.example.invalid/api?token=abcdef123456&page=2"),
            format!("https://h.example.invalid/api?token={REDACTED}&page=2")
        );
        assert_eq!(mask("curl 'https://x.invalid/v1?api_key=k-abc123&q=1'"), format!("curl 'https://x.invalid/v1?api_key={REDACTED}&q=1'"));
        assert_eq!(mask("https://x.invalid/p?key=abc&sig=def"), format!("https://x.invalid/p?key={REDACTED}&sig={REDACTED}"));
        // 沒有憑據參數的 URL 一個字都不要動。
        let plain = "https://x.invalid/p?page=2&sort=name";
        assert_eq!(mask(plain), plain);

        // 3) `user:pass@`：以前要網域含 `.` 才遮。
        assert_eq!(mask("http://u:p@localhost:8080/path"), REDACTED);
        assert_eq!(mask("mail someone@example.com now"), format!("mail {REDACTED} now"));

        // 4) JWE compact 是五段。
        let jwe = "eyJhbGciOiJSU0EtT0FFUCJ9.abcdefgh.ijklmnop.qrstuvwxyz012345.tag12345";
        assert_eq!(mask(jwe), REDACTED);
        // `{ "alg"…`（大括號後有空白）的 header 是 `eyA` 開頭。
        assert_eq!(mask("eyAiYWxnIjoiSFMyNTYifQ.eyJzdWIiOiIxIn0.sig12345"), REDACTED);

        // 收尾標點留著：以前整個片段被換掉，JSON 的 `"}` 會不見。
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
            screen: "  ◆ Run: rg weekly /Users/m4p/project\n    screen.rs:458: low.contains(\"you hit your weekly limit\")\n❯\n".into(),
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
        assert!(!sent.contains("ghp_") && !sent.contains("/Users/m4p") && !sent.contains("B1") && !sent.contains("P1"), "{sent}");
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
        // 週限：撞了之後可能好幾天才有下一次成功回合，用 5 小時窗那個下界會把它整批漏掉（#453 跟進審核）。
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
        // 覆寫舊的；壞輸入不動既有的檔。
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

        // 空 token＝不動現有的 key；只關開關。
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
        // config.toml 被外面換成壞檔：ConfigStore::update 會在重讀時失敗
        std::fs::write(dir.join("config.toml"), "this is = = not toml").unwrap();
        // mtime 一定要跟 store 記的不同，否則不會重讀（時間解析度粗時會假綠／偶發紅）
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
