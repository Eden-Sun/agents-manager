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
    let mut cmd = tokio::process::Command::new("/bin/sh");
    cmd.arg("-c").arg(script).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped()).kill_on_drop(true);
    unsafe {
        cmd.pre_exec(|| {
            if libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = cmd.spawn()?;
    let mut group = KillGroupOnDrop(child.id().map(|pid| pid as i32));
    match tokio::time::timeout(limit, child.wait_with_output()).await {
        Ok(r) => {
            group.0 = None;
            r
        }
        Err(_) => Err(std::io::Error::new(std::io::ErrorKind::TimedOut, format!("local sh timed out after {limit:?}"))),
    }
}

/// 逾時或 future 被丟掉（還沒跑完）時殺整個行程群組；正常結束前會先清成 `None`。
struct KillGroupOnDrop(Option<i32>);

impl Drop for KillGroupOnDrop {
    fn drop(&mut self) {
        if let Some(pgid) = self.0 {
            unsafe { libc::killpg(pgid, libc::SIGKILL) };
        }
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
