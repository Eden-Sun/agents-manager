
    use super::*;
    use std::path::PathBuf;

    fn read_all(f: &mut std::fs::File) -> Vec<u8> {
        let mut v = Vec::new();
        std::io::Read::read_to_end(f, &mut v).unwrap();
        v
    }

    /// 先建、再 canonicalize：macOS 的 `$TMPDIR` 本身經過符號連結（`/var` → `/private/var`），`resolve()`
    /// 只 canonicalize 一次 `root`／`cwd`，這裡先把測試自己的 `base` 也校正成同一種拼法，兩邊字串比對才會
    /// 一致（跟 `outbox.rs` 的 `scratch()` 同一個理由）。
    fn scratch(tag: &str) -> PathBuf {
        let base = crate::testing::track(std::env::temp_dir().join(format!("am-local-image-{tag}-{}", crate::db::ulid())));
        std::fs::create_dir_all(&base).unwrap();
        std::fs::canonicalize(base).unwrap()
    }

    /// 專案裡的圖片（相對或絕對）放行；`..` 逃出去、符號連結指到外面、非圖片、不存在都擋。
    #[test]
    fn only_images_inside_the_project_resolve() {
        let base = scratch("resolve");
        let root = base.join("proj");
        std::fs::create_dir_all(root.join("docs/shots")).unwrap();
        std::fs::write(root.join("docs/shots/a.png"), b"png").unwrap();
        std::fs::write(root.join("notes.txt"), b"secret").unwrap();
        std::fs::write(base.join("outside.png"), b"png").unwrap();
        std::os::unix::fs::symlink(base.join("outside.png"), root.join("link.png")).unwrap();

        let ok = resolve(&root, None, "docs/shots/a.png").expect("relative inside");
        assert_eq!(ok.1, "image/png");
        let abs = root.join("docs/shots/a.png");
        assert!(resolve(&root, None, abs.to_str().unwrap()).is_some(), "absolute inside");
        assert!(resolve(&root, None, &format!("file://{}", abs.display())).is_some(), "file:// URL");
        assert!(resolve(&root, None, "../outside.png").is_none(), "escapes with ..");
        assert!(resolve(&root, None, "..%2Foutside.png").is_none(), "escapes with an encoded ..");
        assert!(resolve(&root, None, base.join("outside.png").to_str().unwrap()).is_none(), "absolute outside");
        assert!(resolve(&root, None, "link.png").is_none(), "symlink pointing outside");
        assert!(resolve(&root, None, "notes.txt").is_none(), "not an image");
        assert!(resolve(&root, None, "docs/shots/missing.png").is_none());
        assert!(resolve(&root, None, "docs").is_none(), "a directory");
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// review3 c4 L3：child 在自己的 worktree 截圖、回覆寫相對路徑，要讀 worktree 那張，不是主樹的同名舊圖；
    /// worktree 裡沒有才退回專案根目錄。bot 的工作目錄在專案外時照樣不放行。
    #[test]
    fn relative_paths_start_from_the_bots_working_dir() {
        let base = scratch("cwd");
        let root = base.join("proj");
        let wt = root.join(".claude/worktrees/child");
        std::fs::create_dir_all(root.join("docs")).unwrap();
        std::fs::create_dir_all(wt.join("docs")).unwrap();
        std::fs::write(root.join("docs/a.png"), b"old").unwrap();
        std::fs::write(wt.join("docs/a.png"), b"new").unwrap();
        std::fs::write(root.join("docs/only-root.png"), b"root").unwrap();
        let outside = base.join("elsewhere");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("b.png"), b"png").unwrap();

        let (mut got, _) = resolve(&root, Some(&wt), "docs/a.png").expect("worktree copy");
        assert_eq!(read_all(&mut got), b"new", "讀到的是 worktree 那份");
        let (mut got, _) = resolve(&root, None, "docs/a.png").unwrap();
        assert_eq!(read_all(&mut got), b"old", "沒有 cwd 就是專案根目錄");
        let (mut got, _) = resolve(&root, Some(&wt), "docs/only-root.png").expect("falls back to the project root");
        assert_eq!(read_all(&mut got), b"root");
        assert!(resolve(&root, Some(&outside), "b.png").is_none(), "工作目錄在專案外：範圍仍是專案目錄");
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// Markdown 渲染把網址編過：中文檔名、空白、`file://` 網址都是 `%XX`。
    #[test]
    fn percent_encoded_paths_resolve() {
        let base = scratch("pct");
        let root = base.join("proj");
        std::fs::create_dir_all(root.join("docs")).unwrap();
        std::fs::write(root.join("docs/截圖.png"), b"png").unwrap();
        std::fs::write(root.join("docs/my shot.png"), b"png").unwrap();
        std::fs::write(root.join("docs/100%20.png"), b"literal").unwrap();

        assert!(resolve(&root, None, "docs/%E6%88%AA%E5%9C%96.png").is_some(), "中文檔名");
        assert!(resolve(&root, None, "docs/my%20shot.png").is_some(), "空白");
        let abs = std::fs::canonicalize(root.join("docs")).unwrap().join("%E6%88%AA%E5%9C%96.png");
        assert!(resolve(&root, None, &format!("file://{}", abs.display())).is_some(), "file:// 網址");
        let (mut got, _) = resolve(&root, None, "docs/100%20.png").expect("字面檔名先試");
        assert_eq!(read_all(&mut got), b"literal");
        assert_eq!(percent_decode("a%2"), Some("a%2".into()), "不完整的 % 原樣保留");
        assert_eq!(percent_decode("%FF.png"), None, "不是 UTF-8 就放棄");
        std::fs::remove_dir_all(&base).unwrap();
    }

    /// issue #89：跟 outbox 同一個形狀——先讀一次成功，把同一個檔名換成指到界線外的符號連結，再讀一次
    /// 必須拿不到界線外的內容（一定是 `None`，不會安靜地跟著連結走）。
    #[test]
    fn a_file_swapped_for_a_symlink_after_being_read_once_is_refused_next_time() {
        let base = scratch("race");
        let root = base.join("proj");
        std::fs::create_dir_all(root.join("docs")).unwrap();
        std::fs::write(root.join("docs/a.png"), b"safe image").unwrap();

        let (mut got, _) = resolve(&root, None, "docs/a.png").expect("第一次正常讀到");
        assert_eq!(read_all(&mut got), b"safe image");

        let secret = base.join("host-secret.png");
        std::fs::write(&secret, b"host secret").unwrap();
        std::fs::remove_file(root.join("docs/a.png")).unwrap();
        std::os::unix::fs::symlink(&secret, root.join("docs/a.png")).unwrap();

        assert!(resolve(&root, None, "docs/a.png").is_none(), "換成符號連結之後不能再讀到任何內容");
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[tokio::test(flavor = "current_thread")]
    async fn macos_local_image_resolution_does_not_block_the_tokio_worker() {
        use std::thread;
        use std::time::Duration;

        let work = run_in_blocking_pool(|| {
            thread::sleep(Duration::from_millis(150));
            42
        });
        tokio::pin!(work);
        tokio::select! {
            biased;
            _ = tokio::time::sleep(Duration::from_millis(20)) => {}
            result = &mut work => panic!("blocking image resolution finished on the tokio worker: {result:?}"),
        }
        assert_eq!(work.await, Some(42));
    }
