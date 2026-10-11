
    use super::*;

    fn alive(pid: &str) -> bool {
        std::process::Command::new("ps")
            .args(["-o", "stat=", "-p", pid])
            .output()
            .map(|o| !String::from_utf8_lossy(&o.stdout).trim().is_empty())
            .unwrap_or(false)
    }

    /// 逾時是要測的行為，但「shell 在逾時前有沒有跑到寫 pid 檔」是 fork/exec 的速度，機器忙時 500ms 不夠（#1273）。
    /// 所以沒寫到 pid 檔就放大逾時重來：寫到了才往下驗「逾時準時回、行程被收掉」，斷言本身不放寬。
    #[tokio::test]
    async fn a_hung_command_times_out_and_is_killed() {
        let pidfile = crate::testing::track(std::env::temp_dir().join(format!("am-local-sh-{}.pid", std::process::id())));
        let mut pid = None;
        for timeout_ms in [500u64, 2_000, 8_000, 30_000] {
            let _ = std::fs::remove_file(&pidfile);
            let started = std::time::Instant::now();
            let err = output_within(&format!("echo $$ > {}; exec sleep 60", pidfile.display()), Duration::from_millis(timeout_ms))
                .await
                .unwrap_err();
            assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
            assert!(started.elapsed() < Duration::from_millis(timeout_ms) + Duration::from_secs(20), "逾時要準時回，不是等指令跑完");
            if let Ok(p) = std::fs::read_to_string(&pidfile) {
                if p.ends_with('\n') && p.trim().parse::<u32>().is_ok() {
                    pid = Some(p.trim().to_string());
                    break;
                }
            }
        }
        let pid = pid.expect("四輪（最長 30 秒）shell 都沒跑到寫 pid 檔");
        let _ = std::fs::remove_file(&pidfile);
        assert!(crate::testing::eventually!(!alive(&pid)), "逾時後行程還活著（沒有 kill_on_drop）");
    }

    #[tokio::test]
    async fn a_quick_command_returns_its_output() {
        let o = output("echo hi").await.unwrap();
        assert!(o.status.success());
        assert_eq!(String::from_utf8_lossy(&o.stdout).trim(), "hi");
    }
