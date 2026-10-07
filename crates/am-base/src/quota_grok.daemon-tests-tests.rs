
    use super::*;
    use std::sync::Arc;
    use crate::runners::quota_grok::refresh_grok;
    use serde_json::json;

    /// 剛開的 pane 的 shell 還沒好：`agent.start` 回 `agent_pane_busy` 不能讓整輪探測失敗，等一下重試；
    /// 一直 busy 就在上限後照實回錯（不無限等）。
    #[tokio::test]
    async fn agent_start_waits_for_the_new_panes_shell_instead_of_failing_the_probe() {
        let e = crate::testing::env().await;
        let (_ws, pane) = e.app.herdr.workspace_create("/tmp/p", "probe", json!({})).await.unwrap();
        e.herdr.fail_next("agent.start", crate::testing::Fault::RefuseWith("agent_pane_busy"));
        e.herdr.fail_next("agent.start", crate::testing::Fault::RefuseWith("agent_pane_busy"));
        let started = start_when_shell_ready(&e.app.herdr, "amquotatest", &pane.pane_id).await.expect("busy twice, then the shell is ready");
        assert_eq!(started.pane_id, pane.pane_id);
        assert_eq!(e.herdr.calls_to("agent.start").len(), 3);

        let before = e.herdr.calls_to("agent.start").len();
        for _ in 0..SHELL_READY_ATTEMPTS {
            e.herdr.fail_next("agent.start", crate::testing::Fault::RefuseWith("agent_pane_busy"));
        }
        let err = start_when_shell_ready(&e.app.herdr, "amquotatest2", &pane.pane_id).await.unwrap_err();
        assert!(pane_busy(&err), "有上限：一直 busy 就照實回錯，{err:#}");
        assert_eq!(e.herdr.calls_to("agent.start").len() - before, SHELL_READY_ATTEMPTS as usize, "有上限：忙碌回應到原本上限就照實回錯");
        // 別種錯誤不重試。
        e.herdr.fail_next("agent.start", crate::testing::Fault::RefuseWith("invalid_agent_argument"));
        let before = e.herdr.calls_to("agent.start").len();
        assert!(start_when_shell_ready(&e.app.herdr, "amquotatest3", &pane.pane_id).await.is_err());
        assert_eq!(e.herdr.calls_to("agent.start").len(), before + 1);
    }

    #[tokio::test]
    async fn agent_start_keeps_the_previous_twelve_attempt_shell_ready_window() {
        let e = crate::testing::env().await;
        let (_ws, pane) = e.app.herdr.workspace_create("/tmp/p", "probe", json!({})).await.unwrap();
        for _ in 0..10 {
            e.herdr.fail_next("agent.start", crate::testing::Fault::RefuseWith("agent_pane_busy"));
        }

        let started = start_when_shell_ready(&e.app.herdr, "amquotatest-late", &pane.pane_id)
            .await
            .expect("a shell becoming ready after ten transient busy responses should still start");

        assert_eq!(started.pane_id, pane.pane_id);
        assert_eq!(e.herdr.calls_to("agent.start").len(), 11, "the previous 12-attempt limit includes the eventual success");
    }

    #[test]
    fn a_logged_out_or_recently_failed_host_is_not_probed() {
        assert!(should_probe_grok(None, false));
        assert!(should_probe_grok(Some(true), false));
        assert!(!should_probe_grok(Some(false), false));
        assert!(!should_probe_grok(None, true));

        let key = format!("test-{}/grok", crate::db::ulid());
        assert!(!cooling_down(&key));
        park(&key, Duration::from_secs(60));
        assert!(cooling_down(&key));
        park(&key, Duration::from_millis(0));
        std::thread::sleep(Duration::from_millis(5));
        assert!(!cooling_down(&key), "an expired park clears itself");
    }

    #[tokio::test]
    async fn an_unreadable_remote_home_skips_grok_probe_and_recovers_next_poll() {
        use std::sync::atomic::Ordering;

        let app = crate::testing::env().await.app.clone();
        let host = format!("grok-home-618-{}", crate::db::ulid().to_ascii_lowercase());
        let conn = app.hosts.insert_remote_for_test(crate::config::HostCfg {
            shared_session: false,
            name: host.clone(),
            ssh: "unused".into(),
            ssh_port: 22,
            ssh_opts: vec![],
            herdr_session: "agents-manager".into(),
            remote_path: String::new(),
        }).await;
        conn.connected.store(true, Ordering::SeqCst);
        app.tools.lock().await.insert(host.clone(), crate::tools::HostTools {
            tools: [("grok".into(), crate::tools::ToolInfo { installed: true, path: Some("/usr/bin/grok".into()), version: None, logged_in: Some(true) })].into(),
            identities: Default::default(),
            shell_identities: vec![],
            utc_offset_secs: None,
            herdr_cli: None,
            checked_at: crate::db::now(),
        });
        let ssh_calls = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let ssh_calls2 = ssh_calls.clone();
        crate::hosts::set_ssh_fake(&host, move |script| {
            ssh_calls2.lock().unwrap().push(script.to_string());
            Err(anyhow!("injected remote HOME read failure"))
        });
        let socket = conn.client.socket_path().to_path_buf();
        let herdr = crate::testing::MockHerdr::start(socket.clone());

        let key = crate::quota::quota_key(&host, "grok");
        let err = refresh_grok(&app, &host).await.expect_err("unreadable remote HOME must stop the Grok probe");
        assert!(err.to_string().contains("HOME"), "retain the HOME failure reason: {err:#}");
        assert!(herdr.calls_to("workspace.create").is_empty(), "do not create a workspace at `/tmp` or daemon HOME");
        assert!(!app.quotas.lock().await.contains_key(&key), "no quota observation is published");
        assert_eq!(ssh_calls.lock().unwrap().len(), 1, "only the failed HOME read is allowed");

        *conn.remote_home.lock().await = Some("/home/remote-grok".into());
        herdr.set_screen("*", SCREEN);
        assert!(refresh_grok(&app, &host).await.unwrap(), "the next poll retries naturally");
        let creates = herdr.calls_to("workspace.create");
        assert_eq!(creates.len(), 1, "one recovered Grok workspace is created");
        assert_eq!(creates[0]["cwd"], "/home/remote-grok", "workspace uses the remote HOME");
        assert!(app.quotas.lock().await.contains_key(&key), "the recovered quota is published");

        drop(herdr);
        let _ = std::fs::remove_file(socket);
    }

    const SCREEN: &str = "\
  /private/tmp                                                        1.5K / 500K
     ◆ session_start  [hooks: 2]
        ┌──────────────────────────────────────────────────── [✗] ─┐
        │  Context usage  Usage limit  Session info                │
        │──────────────────────────────────────────────────────────│
        │  Weekly limit (SuperGrok)                                │
        │                                                          │
        │  ████░░░░░░░░░░░░░░░░░░░░░░░░░░  14%                     │
        │  Resets: September 12, 16:28                             │
        │                                                          │
        │           Tab switch  |  ↑/↓ scroll  |  Esc close        │
        └──────────────────────────────────────────────────────────┘";

    fn at(s: &str) -> DateTime<Local> {
        DateTime::parse_from_rfc3339(s).unwrap().with_timezone(&Local)
    }

    /// 2026-09-28：herdr 在啟動當下把名字拿掉只算「名字不見」，不算探測失敗；其他錯照舊是失敗。
    #[test]
    fn a_dropped_agent_name_is_not_a_failed_probe() {
        let lost: anyhow::Error = crate::herdr::HerdrError { code: "agent_name_not_found".into(), message: "named agent amquota no longer owns the target terminal".into() }.into();
        assert!(name_lost(&lost));
        let other: anyhow::Error = crate::herdr::HerdrError { code: "pane_not_found".into(), message: "x".into() }.into();
        assert!(!name_lost(&other));
        assert!(!name_lost(&anyhow::anyhow!("socket closed")));
    }

    #[test]
    fn the_probe_pane_is_ready_once_grok_is_up_on_it() {
        let pane = |agent: Option<&str>, st: Option<AgentStatus>| crate::herdr::PaneInfo {
            pane_id: "w1:p1".into(),
            workspace_id: "w1".into(),
            tab_id: "w1:t1".into(),
            cwd: None,
            foreground_cwd: None,
            agent: agent.map(String::from),
            agent_status: st,
            revision: 0,
            scroll: None,
        };
        assert!(grok_pane_ready(Some(&pane(Some("grok"), Some(AgentStatus::Idle)))));
        assert!(grok_pane_ready(Some(&pane(Some("grok"), Some(AgentStatus::Blocked)))));
        assert!(!grok_pane_ready(Some(&pane(None, None))), "還在 shell");
        assert!(!grok_pane_ready(Some(&pane(Some("grok"), Some(AgentStatus::Unknown)))), "還沒畫完");
        assert!(!grok_pane_ready(Some(&pane(Some("claude"), Some(AgentStatus::Idle)))));
        assert!(!grok_pane_ready(None));
    }

    #[test]
    fn weekly_limit_lands_on_seven_day() {
        let q = parse_grok_usage(SCREEN, at("2026-09-06T12:00:00+08:00")).unwrap();
        assert_eq!(q.seven_day.as_ref().unwrap().used_pct, 14.0);
        assert!(q.five_hour.is_none());
        assert_eq!(q.plan.as_deref(), Some("SuperGrok"));
        assert_eq!(q.source, "grok-usage");
        assert!(q.seven_day.unwrap().resets_at.unwrap().starts_with("2026-09-12"));
    }

    #[test]
    fn a_reset_already_past_belongs_to_next_year() {
        // Asked in late December about a reset rendered as "January 3".
        let r = parse_reset(" January 3, 09:00", at("2026-12-28T10:00:00+08:00")).unwrap();
        assert!(r.starts_with("2027-01-03"), "{r}");
        // Same date, asked in January: this year.
        let r = parse_reset(" January 3, 09:00", at("2026-01-02T10:00:00+08:00")).unwrap();
        assert!(r.starts_with("2026-01-03"), "{r}");
    }

    #[test]
    fn an_explicit_year_is_honoured() {
        let r = parse_reset(" September 12, 2027 16:28", at("2026-09-06T12:00:00+08:00")).unwrap();
        assert!(r.starts_with("2027-09-12"), "{r}");
    }

    const REMOTE_SCREEN: &str = "Weekly limit (SuperGrok)\n  ████░░  14%\n  Resets: September 12, 2027 16:28";

    #[test]
    fn a_remote_grok_reset_uses_the_host_offset_not_the_daemon_timezone() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-09-06T12:00:00Z").unwrap().with_timezone(&chrono::Utc);
        for (offset_secs, expected) in [(0, "2027-09-12T16:28:00.000Z"), (13 * 3600, "2027-09-12T03:28:00.000Z")] {
            let quota = parse_grok_usage_remote(REMOTE_SCREEN, now, Some(offset_secs)).unwrap();
            assert_eq!(quota.seven_day.unwrap().resets_at.as_deref(), Some(expected));
        }
    }

    #[test]
    fn a_remote_grok_without_a_detected_offset_keeps_usage_but_does_not_guess_reset_time() {
        let now = chrono::DateTime::parse_from_rfc3339("2026-09-06T12:00:00Z").unwrap().with_timezone(&chrono::Utc);
        let quota = parse_grok_usage_remote(REMOTE_SCREEN, now, None).unwrap();
        let weekly = quota.seven_day.unwrap();
        assert_eq!(weekly.used_pct, 14.0);
        assert_eq!(weekly.resets_at, None);
    }

    /// The probe reads the offset detected for that host (#629), not the daemon's zone.
    #[tokio::test]
    async fn the_probe_parses_a_remote_screen_with_that_hosts_offset() {
        let env = crate::testing::env().await;
        let app = env.app.clone();
        let tools = |offset| crate::tools::HostTools {
            tools: Default::default(),
            identities: Default::default(),
            shell_identities: vec![],
            utc_offset_secs: offset,
            herdr_cli: None,
            checked_at: crate::db::now(),
        };
        app.tools.lock().await.insert("r13".into(), tools(Some(13 * 3600)));
        app.tools.lock().await.insert("rnone".into(), tools(None));
        let now = chrono::DateTime::parse_from_rfc3339("2026-09-06T12:00:00Z").unwrap().with_timezone(&chrono::Utc);
        let reset = |q: Option<Quota>| q.unwrap().seven_day.unwrap().resets_at;
        assert_eq!(reset(parse_probe_screen(&app, "r13", REMOTE_SCREEN, now).await).as_deref(), Some("2027-09-12T03:28:00.000Z"));
        assert_eq!(reset(parse_probe_screen(&app, "rnone", REMOTE_SCREEN, now).await), None);
        assert_eq!(reset(parse_probe_screen(&app, "unknown", REMOTE_SCREEN, now).await), None, "never detected = no offset");
        let local = Local.from_local_datetime(&NaiveDate::from_ymd_opt(2027, 9, 12).unwrap().and_hms_opt(16, 28, 0).unwrap()).earliest().unwrap();
        assert_eq!(reset(parse_probe_screen(&app, LOCAL_HOST, REMOTE_SCREEN, now).await), Some(crate::db::iso_at(local.with_timezone(&chrono::Utc))));
    }

    #[test]
    fn an_hourly_row_would_land_on_five_hour() {
        let s = "  2-hour limit (SuperGrok)\n  ██░░  7%\n  Resets: September 6, 18:00\n\
                 \n  Weekly limit (SuperGrok)\n  ████░░  40%\n  Resets: September 12, 16:28";
        let q = parse_grok_usage(s, at("2026-09-06T12:00:00+08:00")).unwrap();
        assert_eq!(q.five_hour.unwrap().used_pct, 7.0);
        assert_eq!(q.seven_day.unwrap().used_pct, 40.0);
    }

    #[test]
    fn a_screen_without_a_bar_row_is_not_a_quota() {
        assert!(parse_grok_usage("Context usage  Usage limit\n  Loading…", Local::now()).is_none());
        // The context-usage tab has a percentage but no limit header.
        assert!(parse_grok_usage("  Context: ████░░  62%", Local::now()).is_none());
    }
