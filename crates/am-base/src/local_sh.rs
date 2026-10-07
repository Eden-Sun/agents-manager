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

pub async fn output_within(script: &str, limit: Duration) -> std::io::Result<Output> {
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

/// 丟掉 `Child` 不會 wait：行程結束後在 daemon 存活期間留 zombie（#287）。另起 thread 等它，結束就收掉。
pub fn reap_in_background(mut child: std::process::Child) {
    std::thread::spawn(move || {
        if let Err(error) = child.wait() {
            tracing::warn!(?error, "failed waiting for spawned child");
        }
    });
}
