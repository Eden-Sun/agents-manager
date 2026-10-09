
    use super::*;

    /// 主機睡著／tailscale 斷線：背景巡邏（每 10 分鐘）讀磁碟版本不能對連不上的主機各等一趟 30 秒 ssh 逾時
    /// （每個 kind、每台主機各一次，串行），直接說連不上。
    #[tokio::test]
    async fn reading_the_disk_version_of_a_down_host_does_not_dial_ssh() {
        let host = "changelog-asleep";
        let env = crate::testing::env().await;
        let cfg = crate::config::HostCfg { name: host.into(), ssh: host.into(), ssh_port: 22, ssh_opts: vec![], herdr_session: "agents-manager".into(), remote_path: String::new(), shared_session: false };
        let conn = env.app.hosts.insert_remote_for_test(cfg).await;
        assert!(!conn.is_connected());
        let calls = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let seen = calls.clone();
        crate::hosts::set_ssh_fake(host, move |_| {
            seen.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok("2.1.5 (Claude Code)\n".into())
        });
        crate::hosts::set_ssh_delay(host, Duration::from_secs(60));
        let started = std::time::Instant::now();
        let err = installed_version(&env.app, host, "claude").await.unwrap_err();
        assert!(err.to_string().contains("未連線"), "{err:#}");
        assert!(started.elapsed() < Duration::from_secs(20), "連不上的主機不能讓巡邏等：{:?}", started.elapsed());
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 0, "不該打任何 ssh");
    }

    const MD: &str = "# Changelog\n\n## 2.1.5\n\n- fixed a\n- fixed b\n\n## 2.1.4\n\n- thing\n\n## Unreleased\n\nnope\n\n## 2.1.3\n\n- old\n";

    #[test]
    fn splits_versions_and_skips_non_version_headings() {
        let s = parse_changelog(MD);
        assert_eq!(s.iter().map(|x| x.version.as_str()).collect::<Vec<_>>(), ["2.1.5", "2.1.4", "2.1.3"]);
        assert_eq!(s[0].body, "- fixed a\n- fixed b");
    }

    #[test]
    fn picks_the_range_newest_first() {
        let s = parse_changelog(MD);
        let p = pick_sections(&s, Some("2.1.3"), "2.1.5 (Claude Code)");
        assert_eq!(p.iter().map(|x| x.version.as_str()).collect::<Vec<_>>(), ["2.1.5", "2.1.4"]);
        let only = pick_sections(&s, None, "2.1.4");
        assert_eq!(only.iter().map(|x| x.version.as_str()).collect::<Vec<_>>(), ["2.1.4"]);
        assert!(pick_sections(&s, Some("2.1.5"), "2.1.5").is_empty());
        assert!(pick_sections(&s, None, "9.9.9").is_empty());
    }

    #[test]
    fn codex_releases_become_changelog_markdown() {
        let json = r#"[
          {"tag_name":"rust-v0.154.0","prerelease":false,"draft":false,"body":"New Features\n\n- thing"},
          {"tag_name":"rust-v0.154.0-alpha.6","prerelease":true,"draft":false,"body":"nope"},
          {"tag_name":"rust-v0.153.4","prerelease":false,"draft":false,"body":"- older"}
        ]"#;
        let md = codex_releases_to_md(json).unwrap();
        let s = parse_changelog(&md);
        assert_eq!(s.iter().map(|x| x.version.as_str()).collect::<Vec<_>>(), ["0.154.0", "0.153.4"]);
        let p = pick_sections(&s, Some("0.153.4"), "0.154.0");
        assert_eq!(p.len(), 1);
        assert!(p[0].body.contains("- thing"));
        assert!(codex_releases_to_md("[]").is_err());
    }

    #[test]
    fn version_parsing_tolerates_suffixes() {
        assert_eq!(parse_version("2.1.0 (Claude Code)"), Some(vec![2, 1, 0]));
        assert_eq!(parse_version("v1.0"), Some(vec![1, 0]));
        assert_eq!(parse_version("nope"), None);
    }

    /// herdr 的 CHANGELOG 是 Keep a Changelog 格式：`## [x.y.z] - date`，不是 Claude Code 那種
    /// 裸 `## x.y.z`。issue #66 整理 herdr 版本差異要吃得下這個格式，不然段落永遠抓不到。
    const HERDR_MD: &str = "# Changelog\n\n\
        ## Unreleased\n\n## [0.9.1] - 2026-09-16\n\n### Added\n- machine 遠端指令轉發\n\n\
        ## [0.9.0] - 2026-09-07\n\n### Changed\n- endpoint generation 1\n\n\
        ## [0.8.2] - 2026-08-01\n\n- 基準版\n";

    #[test]
    fn parses_keep_a_changelog_bracket_headings() {
        let s = parse_changelog(HERDR_MD);
        assert_eq!(s.iter().map(|x| x.version.as_str()).collect::<Vec<_>>(), ["0.9.1", "0.9.0", "0.8.2"], "帶中括號與日期的標題要能解析出版本號");
        assert!(s[0].body.contains("machine 遠端指令轉發"));
        let p = pick_sections(&s, Some("0.8.2"), "0.9.1");
        assert_eq!(p.iter().map(|x| x.version.as_str()).collect::<Vec<_>>(), ["0.9.1", "0.9.0"]);
    }

    /// `GET /api/changelog?kind=herdr`：herdr 有自己的 CHANGELOG 來源（Keep a Changelog），快取命中就不上網；
    /// 帶了 `to`（新版還沒裝）就不探磁碟。
    #[tokio::test]
    async fn herdr_lookup_reads_its_own_changelog_feed() {
        let e = crate::testing::env().await;
        e.app.changelog.seed("herdr", "# Changelog\n\n## Unreleased\n\n\
            ## [0.9.3] - 2026-09-29\n\n### Fixed\n- codex idle 判斷\n\n\
            ## [0.9.2] - 2026-09-24\n\n### Removed\n- `pane.graphics.*`\n\n\
            ## [0.9.1] - 2026-09-16\n\n- 基準\n").await;
        let r = lookup(&e.app, "local", "herdr", Some("0.9.1"), Some("0.9.3")).await;
        assert!(r.found, "{:?}", r.error);
        assert_eq!(r.sections.iter().map(|x| x.version.as_str()).collect::<Vec<_>>(), ["0.9.3", "0.9.2"]);
        assert_eq!(r.installed_version.as_deref(), Some("0.9.3"));
        assert_eq!(r.source_url, HERDR_CHANGELOG_URL);
        assert!(lookup(&e.app, "local", "grok", None, Some("1.0.0")).await.error.unwrap().contains("沒有 changelog 來源"));
    }
