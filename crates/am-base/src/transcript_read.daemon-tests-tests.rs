
    use super::*;
    use crate::testing as tt;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn tmp(tag: &str) -> PathBuf {
        static N: AtomicU64 = AtomicU64::new(0);
        let d = tt::track(std::env::temp_dir().join(format!("am-test-tx-{tag}-{}-{}", std::process::id(), N.fetch_add(1, Ordering::SeqCst))));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// root 底下放一條指到外面的符號連結：字面上在 root 裡、實際上在別處，不收。
    #[test]
    fn a_symlink_out_of_the_root_is_not_inside_it() {
        let root = tmp("symroot");
        let outside = tmp("symout");
        std::fs::write(outside.join("s.jsonl"), "{}\n").unwrap();
        std::fs::create_dir_all(root.join("projects")).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("projects/-work")).unwrap();
        std::os::unix::fs::symlink(outside.join("s.jsonl"), root.join("projects/link.jsonl")).unwrap();
        let roots = vec![root.join("projects")];
        for p in ["projects/-work/s.jsonl", "projects/link.jsonl", "projects/-work/new.jsonl"] {
            assert!(!path_within_roots(&root.join(p).to_string_lossy(), &roots), "{p} 解開符號連結後在 root 外面");
        }
        std::fs::create_dir_all(root.join("projects/-real")).unwrap();
        assert!(path_within_roots(&root.join("projects/-real/not-yet-written.jsonl").to_string_lossy(), &roots), "還沒寫出來的檔、目錄在 root 裡：收");
    }

    #[test]
    fn a_symlinked_projects_root_cannot_expand_the_trusted_boundary() {
        let base = tmp("root-link");
        let config = base.join(".claude-own");
        let other = base.join(".claude-other");
        std::fs::create_dir_all(other.join("projects/-work")).unwrap();
        std::fs::write(other.join("projects/-work/foreign.jsonl"), "secret\n").unwrap();
        std::fs::create_dir_all(&config).unwrap();
        std::os::unix::fs::symlink(other.join("projects"), config.join("projects")).unwrap();
        let forged = config.join("projects/-work/foreign.jsonl");
        assert!(!path_within_roots(&forged.to_string_lossy(), &[config.join("projects")]), "projects 根 symlink 不能擴張 trusted root");
    }

    #[test]
    fn a_symlinked_sessions_root_cannot_expand_promote_identity_lookup() {
        let base = tmp("session-root-link");
        let config = base.join(".claude-own");
        let other = base.join(".claude-other");
        std::fs::create_dir_all(other.join("sessions")).unwrap();
        std::fs::write(other.join("sessions/123.json"), r#"{"pid":123,"sessionId":"foreign","cwd":"/work"}"#).unwrap();
        std::fs::create_dir_all(&config).unwrap();
        std::os::unix::fs::symlink(other.join("sessions"), config.join("sessions")).unwrap();
        assert!(open_regular(&config.join("sessions/123.json")).is_err(), "promote must not read session metadata through another identity's root");
    }

    #[test]
    fn transcript_reader_refuses_final_symlinks_and_hard_links() {
        let dir = tmp("aliases");
        let source = dir.join("source.jsonl");
        std::fs::write(&source, "private transcript\n").unwrap();
        let symlink = dir.join("symlink.jsonl");
        std::os::unix::fs::symlink(&source, &symlink).unwrap();
        let hardlink = dir.join("hardlink.jsonl");
        std::fs::hard_link(&source, &hardlink).unwrap();
        assert!(open_regular(&symlink).is_err(), "一般檔讀取不能跟最終 symlink");
        assert!(open_regular(&hardlink).is_err(), "transcript 不能以 hard link 跨身分別名進來");
    }

    /// 裝置檔（`/dev/zero`）的長度是 0、讀不完：不是一般檔就不讀。尾端讀取半行、壞 JSON 照舊由解析端跳過。
    #[test]
    fn a_device_is_not_read_and_a_cut_first_line_is_tolerated() {
        assert_eq!(read_tail(std::path::Path::new("/dev/zero"), 4096), None);
        let dir = tmp("tail");
        let p = dir.join("t.jsonl");
        let body = format!("{}\n{{\"a\":1}}\n{{\"b\":", "x".repeat(10_000));
        std::fs::write(&p, &body).unwrap();
        let got = read_tail(&p, 64).unwrap();
        assert!(got.len() <= 64 && got.ends_with("{\"b\":"));
    }


    /// 單顆 bot 的假來源：每個事實都由測試給定，證明授權規則不需要 `App`／DB。
    struct Fake {
        kind: &'static str,
        local: Option<bool>,
        claude: Option<PathBuf>,
        codex: Option<PathBuf>,
        home: Option<PathBuf>,
    }

    impl TranscriptRoots for Fake {
        fn kind(&self) -> &str {
            self.kind
        }
        fn is_local(&self) -> impl Future<Output = Option<bool>> + Send + '_ {
            async move { self.local }
        }
        fn claude_config_dir(&self) -> impl Future<Output = Option<String>> + Send + '_ {
            async move { self.claude.as_ref().map(|p| p.to_string_lossy().into_owned()) }
        }
        fn codex_home(&self) -> impl Future<Output = Option<PathBuf>> + Send + '_ {
            async move { self.codex.clone() }
        }
        fn user_home(&self) -> Option<PathBuf> {
            self.home.clone()
        }
    }

    fn fake(kind: &'static str) -> Fake {
        Fake { kind, local: Some(true), claude: None, codex: None, home: None }
    }

    #[tokio::test]
    async fn roots_follow_the_kind_and_a_missing_fact_means_no_root() {
        let base = tmp("roots");
        let mut f = fake("claude");
        assert!(trusted_roots_for(&f).await.is_empty(), "claude 設定目錄查不到就沒有 root");
        f.claude = Some(base.join(".claude-own"));
        assert_eq!(trusted_roots_for(&f).await, vec![base.join(".claude-own/projects")]);
        let mut f = fake("codex");
        f.codex = Some(base.join(".codex"));
        assert_eq!(trusted_roots_for(&f).await, vec![base.join(".codex/sessions")]);
        let mut f = fake("agy");
        assert!(trusted_roots_for(&f).await.is_empty());
        f.home = Some(base.clone());
        assert_eq!(trusted_roots_for(&f).await, vec![base.join(".gemini/antigravity-cli/brain")]);
        assert!(trusted_roots_for(&fake("grok")).await.is_empty(), "grok 沒有 transcript root");
    }

    #[tokio::test]
    async fn a_local_bot_reads_only_under_its_own_root_and_an_unknown_host_reads_nothing() {
        let cfg = tmp("auth-cfg");
        let other = tmp("auth-other");
        std::fs::create_dir_all(cfg.join("projects/-w")).unwrap();
        std::fs::create_dir_all(other.join("projects/-w")).unwrap();
        let own = cfg.join("projects/-w/s.jsonl");
        let foreign = other.join("projects/-w/s.jsonl");
        let (own_s, foreign_s) = (own.to_string_lossy().into_owned(), foreign.to_string_lossy().into_owned());

        let mut f = fake("claude");
        f.claude = Some(cfg.clone());
        assert!(transcript_allowed_for(&f, &own_s).await && local_transcript_allowed_for(&f, &own_s).await);
        assert!(!transcript_allowed_for(&f, &foreign_s).await && !local_transcript_allowed_for(&f, &foreign_s).await);

        f.local = None;
        assert!(!transcript_allowed_for(&f, &own_s).await, "主機查不到：沒有依據，不收");
        assert!(!local_transcript_allowed_for(&f, &own_s).await);
    }

    /// 遠端 bot 的檔案在那台：能留下路徑（只擋形狀），但這台 daemon 絕不開它。
    #[tokio::test]
    async fn a_remote_bot_keeps_a_well_shaped_path_but_is_never_opened_locally() {
        let mut f = fake("claude");
        f.local = Some(false);
        for ok in ["/remote/home/.claude/projects/-w/s.jsonl"] {
            assert!(transcript_allowed_for(&f, ok).await, "{ok}");
            assert!(!local_transcript_allowed_for(&f, ok).await, "{ok}");
        }
        for bad in ["relative/s.jsonl", "/a/../b/s.jsonl", "/a/b/s.txt", "/a/b\u{7}/s.jsonl"] {
            assert!(!transcript_allowed_for(&f, bad).await, "{bad:?}");
        }
    }
