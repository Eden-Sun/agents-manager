
    use super::*;

    fn alive(pid: &str) -> bool {
        std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", pid])
            .output()
            .map(|o| !String::from_utf8_lossy(&o.stdout).trim().is_empty())
            .unwrap_or(false)
    }

    #[tokio::test]
    async fn a_hung_command_times_out_and_is_killed() {
        let pidfile = crate::testing::track(std::env::temp_dir().join(format!("am-local-sh-{}.pid", std::process::id())));
        let started = std::time::Instant::now();
        let err = output_within(&format!("echo $$ > {}; exec sleep 60", pidfile.display()), Duration::from_millis(500))
            .await
            .unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
        assert!(started.elapsed() < Duration::from_secs(20), "逾時要準時回，不是等指令跑完");
        let pid = std::fs::read_to_string(&pidfile).unwrap().trim().to_string();
        let _ = std::fs::remove_file(&pidfile);
        assert!(crate::testing::eventually!(!alive(&pid)), "逾時後行程還活著（沒有 kill_on_drop）");
    }

    #[tokio::test]
    async fn a_quick_command_returns_its_output() {
        let o = output("echo hi").await.unwrap();
        assert!(o.status.success());
        assert_eq!(String::from_utf8_lossy(&o.stdout).trim(), "hi");
    }
