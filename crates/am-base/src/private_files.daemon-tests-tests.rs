
    use super::*;
    use std::os::unix::fs::PermissionsExt as _;

    fn mode_of(p: &Path) -> u32 {
        std::fs::metadata(p).unwrap().permissions().mode() & 0o7777
    }

    #[test]
    fn a_private_dir_is_0700_even_when_it_already_existed_wide_open() {
        let root = crate::testing::track(std::env::temp_dir().join(format!("am-private-{}", crate::db::ulid())));
        let deep = root.join("bots").join("b1");
        create_private_dir(&deep).unwrap();
        assert_eq!(mode_of(&deep), 0o700, "新建的就是 0700");
        assert_eq!(mode_of(&root.join("bots")), 0o700, "中間層也是");

        std::fs::set_permissions(&deep, std::fs::Permissions::from_mode(0o755)).unwrap();
        create_private_dir(&deep).unwrap();
        assert_eq!(mode_of(&deep), 0o700, "舊版留下的 0755 要收回來");
        let _ = std::fs::remove_dir_all(&root);
    }

    /// 建檔當下就要是 0600：先寫再 chmod 的話，中間那一瞬是 umask 決定的。
    #[test]
    fn an_appended_file_is_created_0600() {
        use std::io::Write as _;
        let dir = crate::testing::track(std::env::temp_dir().join(format!("am-private-{}", crate::db::ulid())));
        create_private_dir(&dir).unwrap();
        let p = dir.join("spool.jsonl");
        append_private(&p).unwrap().write_all(b"a\n").unwrap();
        assert_eq!(mode_of(&p), 0o600);
        append_private(&p).unwrap().write_all(b"b\n").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "a\nb\n", "第二次是附加，不是覆蓋");
        let _ = std::fs::remove_dir_all(&dir);
    }
