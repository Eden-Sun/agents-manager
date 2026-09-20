//! 本機 `sh -c` 的共用執行器（#288）：`ps`／`kill` 這類小指令卡住時，不能讓輪詢與 API 跟著卡死。
//! 遠端 `ssh_exec` 早有 30 秒逾時；本機這一側原本直接 `.output().await`，沒逾時、丟掉 future 也不殺行程。

use std::process::{Output, Stdio};
use std::time::Duration;

/// 跟 `hosts::SSH_EXEC_TIMEOUT` 同一個量級：正常的 `ps` 是毫秒級，撐到這麼久就是卡住了。
const LOCAL_EXEC_TIMEOUT: Duration = Duration::from_secs(30);

/// 跑 `sh -c <script>`，stdin 接 /dev/null；逾時回 `TimedOut`，行程隨 future 丟掉而被殺（`kill_on_drop`）。
pub async fn output(script: &str) -> std::io::Result<Output> {
    output_within(script, LOCAL_EXEC_TIMEOUT).await
}

async fn output_within(script: &str, limit: Duration) -> std::io::Result<Output> {
    let child = tokio::process::Command::new("/bin/sh")
        .arg("-c")
        .arg(script)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    match tokio::time::timeout(limit, child.wait_with_output()).await {
        Ok(r) => r,
        Err(_) => Err(std::io::Error::new(std::io::ErrorKind::TimedOut, format!("local sh timed out after {limit:?}"))),
    }
}

#[cfg(test)]
mod tests {
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
        let pidfile = std::env::temp_dir().join(format!("am-local-sh-{}.pid", std::process::id()));
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
}
