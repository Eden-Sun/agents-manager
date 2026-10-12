
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

    /// 逾時（或 future 被丟掉）只殺直接子行程 sh 的話，它底下的孫行程（`&` 的背景工作）會留著。這裡保留 sh 本體（不 `exec`），
    /// 用 abort 丟掉 future 觸發收尾：跟逾時走同一條路，而且不靠時間。
    #[tokio::test]
    async fn a_dropped_run_takes_its_grandchildren_with_it() {
        let pidfile = crate::testing::track(std::env::temp_dir().join(format!("am-local-sh-grandchild-{}.pid", std::process::id())));
        let _ = std::fs::remove_file(&pidfile);
        let script = format!("sleep 60 & echo $! > {}; wait", pidfile.display());
        let run = tokio::spawn(async move { output_within(&script, Duration::from_secs(120)).await });
        let grandchild = loop {
            if let Ok(p) = std::fs::read_to_string(&pidfile) {
                if p.ends_with('\n') && p.trim().parse::<u32>().is_ok() {
                    break p.trim().to_string();
                }
            }
            tokio::task::yield_now().await;
        };
        run.abort();
        let _ = run.await;
        let _ = std::fs::remove_file(&pidfile);
        // 還在跑才算活著；殘留的 zombie 已經不在執行了。
        let still_running = |pid: &str| {
            std::process::Command::new("ps")
                .args(["-o", "stat=", "-p", pid])
                .output()
                .map(|o| {
                    let stat = String::from_utf8_lossy(&o.stdout).trim().to_string();
                    !stat.is_empty() && !stat.starts_with('Z')
                })
                .unwrap_or(false)
        };
        let gone = crate::testing::eventually!(!still_running(&grandchild));
        if !gone {
            let _ = std::process::Command::new("kill").args(["-9", &grandchild]).status();
        }
        assert!(gone, "run 被丟掉之後，sh 底下的孫行程 {grandchild} 還活著");
    }
