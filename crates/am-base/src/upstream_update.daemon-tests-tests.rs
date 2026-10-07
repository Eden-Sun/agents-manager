
    use super::*;
    use std::sync::Mutex as StdMutex;

    fn ok(v: &str) -> Result<String, String> {
        Ok(v.to_string())
    }

    fn disk(host: &str, line: &str) -> (String, Result<String, String>) {
        (host.to_string(), Ok(line.to_string()))
    }

    #[test]
    fn claude_pending_from_requires_a_source_and_target_pair() {
        let without_source = claude_pending_text(None, "2.1.284");
        assert_eq!(claude_pending_from(&without_source), None);

        let with_source = claude_pending_text(Some("2.1.281"), "2.1.284");
        assert_eq!(claude_pending_from(&with_source).as_deref(), Some("2.1.281"));
    }

    // ── 純函式 ──

    /// 使用者 2026-09-28 那一刻：npm 2.1.283、磁碟 2.1.281。
    #[test]
    fn npm_newer_than_disk_is_an_update() {
        let s = build_status("claude", &ok("2.1.283"), &[disk("local", "2.1.281 (Claude Code)")], None);
        assert!(s.has_update && s.hosts[0].behind);
        assert_eq!(s.latest_version.as_deref(), Some("2.1.283"));
        assert_eq!(s.hosts[0].installed_version.as_deref(), Some("2.1.281"));
        assert!(should_notify(&s, None));
        let t = notice_text(&s);
        assert!(t.contains("2.1.283") && t.contains("local：2.1.281 → 2.1.283") && t.contains("裝到每台都相同"),
            "{t}"
        );
        assert!(
            t.contains("claude install 2.1.283"),
            "提示要給出指定版本的安裝方式：{t}"
        );
        assert!(
            !t.contains("背景更新") && !t.contains("claude update"),
            "停用自動更新時不可叫人繼續等：{t}"
        );
    }

    #[test]
    fn same_or_older_upstream_is_not_an_update() {
        let same = build_status("claude", &ok("2.1.283"), &[disk("local", "2.1.283 (Claude Code)")], None);
        assert!(!same.has_update && !should_notify(&same, None));
        // 磁碟比 npm 新（npm 還沒同步、或裝了別的通道）：不叫人去裝。
        let older = build_status("claude", &ok("2.1.281"), &[disk("local", "2.1.283 (Claude Code)")], None);
        assert!(!older.has_update && !should_notify(&older, None));
    }

    #[test]
    fn claude_host_version_mismatch_uses_one_fleet_target_and_lists_every_host() {
        let disks = [
            disk("local", "2.1.284"),
            disk("m4p", "2.1.281"),
            ("offline".into(), Err("timeout".into())),
        ];
        let s = build_status("claude", &ok("2.1.283"), &disks, None);
        assert!(s.has_update, "版本不一致也要持續提示");
        assert_eq!(s.latest_version.as_deref(), Some("2.1.283"));
        assert_eq!(
            s.target_version.as_deref(),
            Some("2.1.284"),
            "最高已安裝版成為共同目標"
        );
        assert_eq!(
            s.hosts.iter().map(|h| h.behind).collect::<Vec<_>>(),
            [false, true, true]
        );
        let text = notice_text(&s);
        assert!(text.contains("local：2.1.284 → 2.1.284"), "{text}");
        assert!(text.contains("m4p：2.1.281 → 2.1.284"), "{text}");
        assert!(text.contains("offline：timeout → 2.1.284"), "{text}");
        assert!(text.contains("claude install 2.1.284"), "{text}");
    }

    #[test]
    fn versions_compare_numerically_not_as_strings() {
        assert!(build_status("codex", &ok("0.9.10"), &[disk("local", "codex-cli 0.9.9")], None).has_update);
        assert!(!build_status("codex", &ok("0.9.9"), &[disk("local", "codex-cli 0.9.10")], None).has_update);
    }

    /// 只要有一台落後就算；通知列出落後的那幾台。
    #[test]
    fn any_host_behind_is_an_update_and_the_notice_names_it() {
        let s = build_status("codex", &ok("0.157.0"), &[disk("local", "codex-cli 0.157.0"), disk("m2", "codex-cli 0.156.1")], None);
        assert!(s.has_update && !s.hosts[0].behind && s.hosts[1].behind);
        let t = notice_text(&s);
        assert!(t.contains("m2 磁碟上是 0.156.1") && !t.contains("local") && t.contains("需先安裝"), "{t}");
    }

    #[test]
    fn dedup_notifies_each_upstream_version_once() {
        let s = build_status("claude", &ok("2.1.283"), &[disk("local", "2.1.281")], None);
        assert!(should_notify(&s, None), "還沒通知過");
        assert!(should_notify(&s, Some("2.1.282")), "上次通知的是更舊的版本");
        assert!(!should_notify(&s, Some("2.1.283")), "這一版已經通知過");
        assert!(!should_notify(&s, Some("2.1.284")), "npm latest 被退回舊版，不重報");
    }

    /// 抓不到上游不是「沒有新版」：`latest_version` 空、`error` 講原因，不會被當成有更新或沒更新而默默帶過。
    #[test]
    fn an_unreachable_upstream_is_an_explicit_error() {
        let s = build_status("claude", &Err("連不上 npm registry：timeout".into()), &[disk("local", "2.1.281")], None);
        assert!(s.latest_version.is_none() && !s.has_update && !should_notify(&s, None));
        assert_eq!(s.error.as_deref(), Some("連不上 npm registry：timeout"));
        let t = error_text(&s);
        assert!(t.contains("claude") && t.contains("npm registry") && t.contains("timeout"), "{t}");
    }

    #[test]
    fn npm_latest_reads_the_version_field() {
        assert_eq!(npm_latest(r#"{"name":"@anthropic-ai/claude-code","version":"2.1.283","dist":{}}"#).unwrap(), "2.1.283");
        assert!(npm_latest(r#"{"name":"x"}"#).is_err());
        assert!(npm_latest("<html>rate limited</html>").is_err());
        assert!(npm_latest(r#"{"version":"2.2.0-beta.1"}"#).is_err(), "預發布不拿來比");
    }

    /// codex 走 releases：沿用 `codex_releases_to_md`（丟草稿／預發布），取最大的正式版，不是第一個。
    #[test]
    fn codex_latest_is_the_highest_stable_release() {
        let json = r#"[
          {"tag_name":"rust-v0.157.0-alpha.2","prerelease":true,"draft":false,"body":"nope"},
          {"tag_name":"rust-v0.156.1","prerelease":false,"draft":false,"body":"- older"},
          {"tag_name":"rust-v0.157.0","prerelease":false,"draft":false,"body":"- newest"},
          {"tag_name":"rust-v0.99.0","prerelease":false,"draft":false,"body":"- ancient"}
        ]"#;
        let md = changelog::codex_releases_to_md(json).unwrap();
        assert_eq!(codex_latest(&md).unwrap(), "0.157.0");
        assert!(codex_latest("").is_err());
    }

    /// herdr 走 GitHub releases：草稿、預發布、帶後綴的 tag 都不算，取數值最大的正式版（不是第一個、不是字串最大）。
    #[test]
    fn herdr_latest_is_the_highest_stable_release() {
        let json = r#"[
          {"tag_name":"v1.0.0","prerelease":false,"draft":true,"body":"草稿"},
          {"tag_name":"v0.10.0-rc.1","prerelease":true,"draft":false,"body":"預發布"},
          {"tag_name":"v0.9.3","prerelease":false,"draft":false,"body":"- codex idle"},
          {"tag_name":"v0.9.10","prerelease":false,"draft":false,"body":"- 數值比較"},
          {"tag_name":"v0.9.2","prerelease":false,"draft":false,"body":"- events_lost"}
        ]"#;
        assert_eq!(herdr_latest(json).unwrap(), "0.9.10");
        assert!(herdr_latest("[]").is_err());
        assert!(herdr_latest(r#"{"message":"API rate limit exceeded"}"#).is_err());
    }

    /// herdr 的磁碟版本是 `herdr 0.9.1` 這種形狀；目標就是上游最新版，落後的主機列在通知裡，並講明會重啟 herdr server。
    #[test]
    fn herdr_behind_upstream_names_the_host_and_warns_about_the_restart() {
        let s = build_status("herdr", &ok("0.9.3"), &[disk("local", "herdr 0.9.1"), disk("m4p", "herdr 0.9.3")], None);
        assert!(s.has_update && s.hosts[0].behind && !s.hosts[1].behind);
        assert_eq!(s.target_version.as_deref(), Some("0.9.3"));
        assert_eq!(s.hosts[0].installed_version.as_deref(), Some("0.9.1"));
        assert_eq!(s.source_url, "https://github.com/herdrdev/herdr/releases");
        let t = notice_text(&s);
        assert!(t.contains("herdr 上游有新版 0.9.3") && t.contains("local 是 0.9.1") && !t.contains("m4p") && t.contains("重啟 herdr server"), "{t}");
        let same = build_status("herdr", &ok("0.9.3"), &[disk("local", "herdr 0.9.3")], None);
        assert!(!same.has_update && !should_notify(&same, None));
        let err = build_status("herdr", &Err("GitHub releases 回 HTTP 403".into()), &[disk("local", "herdr 0.9.1")], None);
        assert!(error_text(&err).contains("GitHub releases"), "{}", error_text(&err));
    }

    // ── 整輪：假的上游與磁碟 ──

    struct Fake {
        upstream: StdMutex<HashMap<String, Result<String, String>>>,
        disk: StdMutex<HashMap<String, String>>,
        calls: StdMutex<usize>,
    }

    impl Fake {
        fn new(claude: Result<&str, &str>, codex: Result<&str, &str>, claude_disk: &str, codex_disk: &str) -> Self {
            // herdr 預設跟上游同版（沒有更新），要測 herdr 的用 set_upstream／set_disk 改。
            let up = [("claude", claude), ("codex", codex), ("herdr", Ok("0.9.3")), ("grok", Ok("1.0.46"))]
                .into_iter()
                .map(|(k, r)| (k.to_string(), r.map(str::to_string).map_err(str::to_string)))
                .collect();
            let d = [
                ("claude".to_string(), claude_disk.to_string()),
                ("codex".to_string(), codex_disk.to_string()),
                ("herdr".to_string(), "herdr 0.9.3".to_string()),
                ("grok".to_string(), "grok 1.0.46 (2765805b9442)".to_string()),
            ]
            .into_iter()
            .collect();
            Fake { upstream: StdMutex::new(up), disk: StdMutex::new(d), calls: StdMutex::new(0) }
        }
        fn set_disk(&self, kind: &str, v: &str) {
            self.disk.lock().unwrap().insert(kind.into(), v.into());
        }
        fn set_upstream(&self, kind: &str, r: Result<&str, &str>) {
            self.upstream.lock().unwrap().insert(kind.into(), r.map(str::to_string).map_err(str::to_string));
        }
    }

    impl Sources for Fake {
        fn upstream<'a>(&'a self, kind: &'a str) -> BoxFuture<'a, Result<String>> {
            *self.calls.lock().unwrap() += 1;
            let r = self.upstream.lock().unwrap().get(kind).cloned().unwrap();
            Box::pin(async move { r.map_err(|e| anyhow!(e)) })
        }
        fn hosts<'a>(&'a self, _kind: &'a str) -> BoxFuture<'a, Vec<String>> {
            Box::pin(async { vec!["local".to_string()] })
        }
        fn installed<'a>(&'a self, _host: &'a str, kind: &'a str) -> BoxFuture<'a, Result<String>> {
            let v = self.disk.lock().unwrap().get(kind).cloned().unwrap();
            Box::pin(async move { Ok(v) })
        }
    }

    fn of<'a>(evs: &'a [Value], kind: &str) -> Option<&'a Value> {
        evs.iter().find(|e| e["kind"] == kind)
    }

    #[tokio::test]
    async fn claude_via_npm_notifies_once_then_clears_when_the_disk_catches_up() {
        let e = crate::testing::env().await;
        let w = Watch::default();
        let last = e.dir.join(LAST_FILE);
        let src = Fake::new(Ok("2.1.283"), Ok("0.157.0"), "2.1.281 (Claude Code)", "codex-cli 0.157.0");

        let evs = tick(&e.app, &w, &src, &last).await;
        let c = of(&evs, "claude").expect("第一輪要推");
        assert_eq!(c["notify"], "update");
        assert!(c["text"].as_str().unwrap().contains("2.1.283"));
        assert_eq!(c["notified_version"], "2.1.283");
        // codex 同版：推快照但不通知、沒有要顯示的字。
        assert!(of(&evs, "codex").unwrap()["notify"].is_null());
        assert!(of(&evs, "codex").unwrap()["text"].is_null());

        // 同一版第二輪：什麼都不推（快照沒變、已通知過）。
        assert!(tick(&e.app, &w, &src, &last).await.is_empty(), "同一版不重複通知");
        // daemon 重啟（新的 Watch）也不重報：去重記在檔案。
        let w2 = Watch::default();
        let evs = tick(&e.app, &w2, &src, &last).await;
        assert!(evs.iter().all(|e| e["notify"].is_null()), "重啟後同一版不重報：{evs:?}");

        // claude 自己下載好了：快照改成沒有更新，推一次（web 收掉），不通知。
        src.set_disk("claude", "2.1.283 (Claude Code)");
        let evs = tick(&e.app, &w2, &src, &last).await;
        let c = of(&evs, "claude").unwrap();
        assert_eq!(c["has_update"], false);
        assert!(c["notify"].is_null());
        assert_eq!(w2.snapshot().await.len(), 4);
    }

    // ── grok（issue #761）：正式版是 x.ai 放在 storage.googleapis.com 的 `cli/stable` 純文字，磁碟版本是 `grok --version` ──

    #[test]
    fn grok_latest_reads_the_stable_pointer_and_refuses_anything_else() {
        assert_eq!(grok_latest("1.0.47\n").unwrap(), "1.0.47");
        assert_eq!(grok_latest("  v1.0.47  ").unwrap(), "1.0.47");
        // 預發布、HTML 錯誤頁、空內容都不能被當成版本（也不能默默變成「沒有新版」）。
        for bad in ["", "1.0.47-alpha.2", "<!doctype html><title>404</title>", "latest", "1.0.47+build5"] {
            assert!(grok_latest(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_grok_host_behind_the_stable_release_is_an_update_with_the_official_command() {
        let s = build_status("grok", &ok("1.0.47"), &[disk("local", "grok 1.0.46 (2765805b9442)"), disk("m4p", "grok 1.0.47 (abcdef012345)")], None);
        assert!(s.has_update && s.hosts[0].behind && !s.hosts[1].behind);
        assert_eq!(s.hosts[0].installed_version.as_deref(), Some("1.0.46"), "`(commit)` 後綴不是版本的一部分");
        assert_eq!(s.target_version.as_deref(), Some("1.0.47"));
        let t = notice_text(&s);
        assert!(t.contains("grok") && t.contains("1.0.47") && t.contains("local 磁碟上是 1.0.46") && !t.contains("m4p"), "{t}");
        assert!(t.contains("grok update"), "官方的升級指令要寫出來（只提示，不一鍵安裝）：{t}");
        let up_to_date = build_status("grok", &ok("1.0.46"), &[disk("local", "grok 1.0.46 (2765805b9442)")], None);
        assert!(!up_to_date.has_update && !should_notify(&up_to_date, None));
        let err = build_status("grok", &Err("GCS 回 HTTP 503".into()), &[disk("local", "grok 1.0.46")], None);
        assert!(!err.has_update && error_text(&err).contains("GCS 回 HTTP 503") && !error_text(&err).contains("npm"));
    }

    #[tokio::test]
    async fn grok_notifies_each_new_stable_once_and_clears_when_the_host_catches_up() {
        let e = crate::testing::env().await;
        let w = Watch::default();
        let last = e.dir.join(LAST_FILE);
        let src = Fake::new(Ok("2.1.283"), Ok("0.157.0"), "2.1.283 (Claude Code)", "codex-cli 0.157.0");
        src.set_upstream("grok", Ok("1.0.47"));

        let evs = tick(&e.app, &w, &src, &last).await;
        let g = of(&evs, "grok").expect("grok 落後要推");
        assert_eq!((g["notify"].as_str(), g["has_update"].as_bool()), (Some("update"), Some(true)));
        assert!(g["text"].as_str().unwrap().contains("grok update"));
        assert!(tick(&e.app, &w, &src, &last).await.is_empty(), "同一版不重複通知");
        src.set_disk("grok", "grok 1.0.47 (abcdef012345)");
        let evs = tick(&e.app, &w, &src, &last).await;
        let g = of(&evs, "grok").unwrap();
        assert_eq!(g["has_update"], false);
        assert!(g["notify"].is_null(), "裝好了只收掉提示，不再通知");
    }

    #[tokio::test]
    async fn codex_via_releases_notifies_a_newer_release_and_each_later_one() {
        let e = crate::testing::env().await;
        let w = Watch::default();
        let last = e.dir.join(LAST_FILE);
        let src = Fake::new(Ok("2.1.281"), Ok("0.157.0"), "2.1.281", "codex-cli 0.156.1");
        let evs = tick(&e.app, &w, &src, &last).await;
        let c = of(&evs, "codex").unwrap();
        assert_eq!(c["notify"], "update");
        assert!(c["text"].as_str().unwrap().contains("需先安裝"));
        // 上游又出了下一版：TTL 內用快取，不會馬上看到——清掉快取模擬過期。
        src.set_upstream("codex", Ok("0.158.0"));
        w.upstream.lock().await.clear();
        let evs = tick(&e.app, &w, &src, &last).await;
        assert_eq!(of(&evs, "codex").unwrap()["notify"], "update", "新的一版要再通知");
        assert_eq!(of(&evs, "codex").unwrap()["latest_version"], "0.158.0");
    }

    /// herdr 跟 claude／codex 同一套：同一個上游版本只通知一次，事件帶 `kind:"herdr"` 與 `target_version`。
    #[tokio::test]
    async fn herdr_notifies_each_upstream_version_once() {
        let e = crate::testing::env().await;
        let w = Watch::default();
        let last = e.dir.join(LAST_FILE);
        let src = Fake::new(Ok("2.1.281"), Ok("0.157.0"), "2.1.281", "codex-cli 0.157.0");
        src.set_disk("herdr", "herdr 0.9.1");
        let evs = tick(&e.app, &w, &src, &last).await;
        let h = of(&evs, "herdr").expect("herdr 要推");
        assert_eq!(h["notify"], "update");
        assert_eq!(h["target_version"], "0.9.3");
        assert_eq!(h["hosts"][0]["behind"], true);
        assert!(h["text"].as_str().unwrap().contains("herdr 上游有新版 0.9.3"));
        assert!(of(&tick(&e.app, &w, &src, &last).await, "herdr").is_none(), "同一版不重複通知");
        assert_eq!(load_last(&last).get("herdr").map(String::as_str), Some("0.9.3"));
    }

    /// TTL：一輪內或下一輪都不重打上游；失敗不快取，下一輪再試。
    #[tokio::test]
    async fn upstream_results_are_cached_but_failures_are_retried() {
        let e = crate::testing::env().await;
        let w = Watch::default();
        let last = e.dir.join(LAST_FILE);
        let src = Fake::new(Ok("2.1.283"), Err("GitHub HTTP 403 rate limit"), "2.1.283", "codex-cli 0.157.0");
        tick(&e.app, &w, &src, &last).await;
        assert_eq!(*src.calls.lock().unwrap(), 4, "claude、codex、herdr、grok 各問一次");
        tick(&e.app, &w, &src, &last).await;
        assert_eq!(*src.calls.lock().unwrap(), 5, "claude、herdr、grok 用快取，只有失敗的 codex 重問");
    }

    /// 抓不到要講：從正常變成抓不到推一次 `error`，一直抓不到不刷屏，恢復後沒事。
    #[tokio::test]
    async fn an_unreachable_upstream_is_announced_once_not_silently_skipped() {
        let e = crate::testing::env().await;
        let w = Watch::default();
        let last = e.dir.join(LAST_FILE);
        let src = Fake::new(Err("連不上 npm registry：dns error"), Ok("0.157.0"), "2.1.281", "codex-cli 0.157.0");
        let evs = tick(&e.app, &w, &src, &last).await;
        let c = of(&evs, "claude").unwrap();
        assert_eq!(c["notify"], "error");
        assert!(c["text"].as_str().unwrap().contains("dns error"));
        assert!(c["latest_version"].is_null() && c["has_update"] == false);
        // 下一輪的嘗試時間一定不同（毫秒精度）：時間戳變了不算快照變了。
        tokio::time::sleep(Duration::from_millis(5)).await;
        assert!(of(&tick(&e.app, &w, &src, &last).await, "claude").is_none(), "持續抓不到不重複推");
        src.set_upstream("claude", Ok("2.1.283"));
        let c = of(&tick(&e.app, &w, &src, &last).await, "claude").cloned().unwrap();
        assert_eq!(c["notify"], "update", "恢復之後照常通知新版");
        assert!(c["error"].is_null());
    }

    /// codex 沒有 run 帶「需安裝」時，安裝 API 靠這個核對：那台落後才給目標，沒落後、沒新版都是 None。
    #[tokio::test]
    async fn behind_target_is_only_given_for_a_host_that_is_behind() {
        let e = crate::testing::env().await;
        set_snapshot_for_test(&e.app.upstream_watch, build_status(
            "codex-behind-test",
            &Ok("0.159.0".into()),
            &[("bt-old".into(), Ok("codex-cli 0.157.1".into())), ("bt-new".into(), Ok("codex-cli 0.159.0".into()))],
            None,
        ))
        .await;
        assert_eq!(behind_target_for_host(&e.app, "codex-behind-test", "bt-old").await.as_deref(), Some("0.159.0"));
        assert_eq!(behind_target_for_host(&e.app, "codex-behind-test", "bt-new").await, None, "已經是新版");
        assert_eq!(behind_target_for_host(&e.app, "codex-behind-test", "bt-missing").await, None);
        assert_eq!(behind_target_for_host(&e.app, "no-such-kind", "bt-old").await, None);
    }
