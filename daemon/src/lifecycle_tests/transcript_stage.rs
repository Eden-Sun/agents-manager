#![allow(unused_imports)]
mod tests {
    use crate::lifecycle::transcript_stage::*;
use crate::lifecycle::transcript_stage::test_seams::*;
use std::ffi::OsStr;
    use crate::lifecycle::*;
    use crate::lifecycle_tests::prelude::*;

    fn root(tag: &str) -> PathBuf {
        let p = crate::testing::track(std::env::temp_dir().join(format!("am-transcript-stage-{tag}-{}", crate::db::ulid())));
        std::fs::create_dir_all(&p).unwrap();
        p
    }

    fn source(root: &Path) -> PathBuf {
        let dir = root.join("projects/key");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("sid.jsonl");
        std::fs::write(&path, "private conversation").unwrap();
        path
    }

    fn stage(src: &Path, config: &Path) -> std::io::Result<Staged> {
        stage_file(src, &config.join("projects/key"), OsStr::new("sid.jsonl"))
    }

    #[test]
    fn staging_creates_a_missing_identity_root_privately() {
        use std::os::unix::fs::PermissionsExt as _;
        let base = root("missing-identity-root");
        let src = source(&base.join("old"));
        let config = base.join("new-identity");
        assert_eq!(stage(&src, &config).unwrap(), Staged::Copied);
        let dest = config.join("projects/key/sid.jsonl");
        assert_eq!(std::fs::read_to_string(dest).unwrap(), "private conversation");
        assert_eq!(std::fs::metadata(&config).unwrap().permissions().mode() & 0o777, 0o700);
    }

    #[test]
    fn prefix_comparison_does_not_advance_the_open_source() {
        use std::io::Read as _;
        let base = root("prefix-position");
        let src = source(&base.join("old"));
        let dest = base.join("long-destination.jsonl");
        std::fs::write(&dest, "private conversation and later turns").unwrap();
        let mut a = crate::transcript_read::open_regular(&src).unwrap();
        let b = crate::transcript_read::open_regular(&dest).unwrap();
        assert!(is_prefix_of(&a, &b).unwrap());
        let mut actual = String::new();
        a.read_to_string(&mut actual).unwrap();
        assert_eq!(actual, "private conversation");
    }

    #[test]
    fn a_projects_root_symlink_is_not_a_transcript_source() {
        let base = root("projects-link");
        let real = base.join("real");
        let transcript = source(&real);
        let alias = base.join("alias");
        std::fs::create_dir_all(&alias).unwrap();
        std::os::unix::fs::symlink(real.join("projects"), alias.join("projects")).unwrap();
        assert!(stage(&alias.join("projects/key/sid.jsonl"), &base.join("new")).is_err());
        assert!(!base.join("new/projects/key/sid.jsonl").exists());
        assert_eq!(std::fs::read_to_string(&transcript).unwrap(), "private conversation");
    }

    #[test]
    fn a_source_cwd_symlink_is_not_followed() {
        let base = root("source-cwd-link");
        let outside = source(&base.join("outside"));
        let old = base.join("old/projects");
        std::fs::create_dir_all(&old).unwrap();
        std::os::unix::fs::symlink(outside.parent().unwrap(), old.join("key")).unwrap();
        assert!(stage(&old.join("key/sid.jsonl"), &base.join("new")).is_err());
        assert!(!base.join("new/projects/key/sid.jsonl").exists());
    }

    #[test]
    fn a_destination_cwd_symlink_is_rejected_without_writing_through_it() {
        let base = root("destination-cwd-link");
        let transcript = source(&base.join("old"));
        let outside = base.join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        let projects = base.join("new/projects");
        std::fs::create_dir_all(&projects).unwrap();
        std::os::unix::fs::symlink(&outside, projects.join("key")).unwrap();
        assert!(stage(&transcript, &base.join("new")).is_err());
        assert!(!outside.join("sid.jsonl").exists());
    }

    #[test]
    fn hard_linked_sources_and_destinations_are_rejected() {
        let base = root("hard-links");
        let src = source(&base.join("old"));
        let secret = base.join("secret");
        std::fs::write(&secret, "private conversation").unwrap();
        let linked_src = base.join("linked/projects/key/sid.jsonl");
        std::fs::create_dir_all(linked_src.parent().unwrap()).unwrap();
        std::fs::hard_link(&secret, &linked_src).unwrap();
        assert!(stage(&linked_src, &base.join("source-reject")).is_err());
        assert!(!base.join("source-reject/projects/key/sid.jsonl").exists());

        let linked_dest = base.join("new/projects/key/sid.jsonl");
        std::fs::create_dir_all(linked_dest.parent().unwrap()).unwrap();
        std::fs::hard_link(&secret, &linked_dest).unwrap();
        assert!(stage(&src, &base.join("new")).is_err());
        assert_eq!(std::fs::read_to_string(&secret).unwrap(), "private conversation");
        assert_eq!(std::fs::read_to_string(linked_dest).unwrap(), "private conversation");
    }

    #[test]
    fn concurrent_first_stagers_both_accept_the_winning_copy() {
        let base = root("concurrent-first-stage");
        let src = source(&base.join("old"));
        let dest_dir = base.join("new/projects/key");
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(2));

        let results = std::thread::scope(|scope| {
            let threads = (0..2)
                .map(|_| {
                    let barrier = barrier.clone();
                    let src = src.clone();
                    let dest_dir = dest_dir.clone();
                    scope.spawn(move || {
                        stage_file_after_missing_check(
                            &src,
                            &dest_dir,
                            OsStr::new("sid.jsonl"),
                            || {
                                barrier.wait();
                            },
                        )
                    })
                })
                .collect::<Vec<_>>();
            threads
                .into_iter()
                .map(|thread| thread.join().unwrap())
                .collect::<Vec<_>>()
        });

        assert!(
            results.iter().all(Result::is_ok),
            "concurrent staging results: {results:?}"
        );
        let final_file = dest_dir.join("sid.jsonl");
        assert_eq!(
            std::fs::read_to_string(&final_file).unwrap(),
            "private conversation"
        );
        assert_eq!(
            std::fs::read_dir(&dest_dir).unwrap().count(),
            1,
            "only the completed transcript should remain"
        );
    }
}
