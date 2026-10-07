
    use super::*;
    use crate::state::App;
    use std::sync::Arc;

    async fn app_for(dir: &std::path::Path) -> Arc<App> {
        let pool = crate::app_ports_p1::open(&dir.join("db.sqlite3")).await.unwrap();
        let cfg = crate::runners::app_ports_p2::load_config(dir.join("config.toml")).await.unwrap();
        let h = crate::herdr::HerdrClient::new(dir.join("herdr.sock"));
        App::new(
            pool,
            h.clone(),
            h,
            cfg,
            dir.to_path_buf(),
            dir.join("agents-managerd"),
            7799,
            "t".into(),
            "test".into(),
            false,
        )
    }

    #[tokio::test]
    async fn sh_local_preserves_exit_status_and_output() {
        let tmp = crate::testing::track(std::env::temp_dir().join(format!("am-git-sh-{}", crate::db::ulid())));
        std::fs::create_dir_all(&tmp).unwrap();
        let app = app_for(&tmp).await;

        for (script, expected) in [("printf 'ok\\n'", 0), ("exit 7", 7)] {
            let out = sh(&app, LOCAL_HOST, script, GIT_TIMEOUT).await.unwrap();
            assert_eq!(out.code, expected, "script: {script}");
            if expected == 0 {
                assert_eq!(out.stdout, "ok\n");
            }
        }

        let _ = std::fs::remove_dir_all(&tmp);
    }

    #[test]
    fn remote_wrapper_keeps_marker_after_script_exit() {
        for (script, expected, output) in [
            ("printf 'plain\\n'", 0, "plain"),
            ("printf 'ok\\n'; exit 0", 0, "ok"),
            ("printf 'bad\\n'; exit 7", 7, "bad"),
        ] {
            let wrapped = wrap_remote_script(script);
            let raw = std::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(&wrapped)
                .output()
                .unwrap();
            assert!(raw.status.success(), "wrapper must finish after script exit");
            let (body, code) = parse_remote_output(String::from_utf8_lossy(&raw.stdout).into_owned());
            assert_eq!(code, expected, "script: {script}");
            assert!(body.contains(output));
        }
    }
