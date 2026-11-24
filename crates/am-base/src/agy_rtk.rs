use serde_json::{json, Value};
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

pub const ENABLE_FILE: &str = "agy-rtk";
pub const REWRITE_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_COMMAND: usize = 64 * 1024;

pub fn rtk_path_from(data_dir: &Path) -> Option<PathBuf> {
    let text = std::fs::read_to_string(data_dir.join(ENABLE_FILE)).ok()?;
    let p = PathBuf::from(text.trim());
    (p.is_absolute() && p.is_file()).then_some(p)
}

pub fn shell_command(payload: &Value) -> Option<String> {
    let call = payload.get("toolCall").or_else(|| payload.get("tool_call"))?;
    if call.get("name").and_then(Value::as_str) != Some("run_command") {
        return None;
    }
    let command = call.get("args").and_then(|a| a.get("CommandLine")).and_then(Value::as_str)?;
    (!command.trim().is_empty() && command.len() <= MAX_COMMAND).then(|| command.to_string())
}

pub fn run_rtk_rewrite(rtk: &Path, command: &str, timeout: Duration) -> Option<String> {
    let mut child = Command::new(rtk)
        .arg("rewrite")
        .arg(command)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .ok()?;
    let mut stdout = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout.by_ref().take(MAX_COMMAND as u64 + 1).read_to_end(&mut buf);
        buf
    });
    let deadline = Instant::now() + timeout;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if Instant::now() < deadline => std::thread::sleep(Duration::from_millis(10)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return None;
            }
        }
    }
    let out = String::from_utf8(reader.join().ok()?).ok()?;
    let out = out.trim_end_matches(['\n', '\r']);
    (!out.trim().is_empty() && out.len() <= MAX_COMMAND).then(|| out.to_string())
}

pub fn decision(command: &str, rewritten: &str) -> Option<String> {
    (rewritten != command).then(|| json!({"decision": "allow", "overwrite": {"CommandLine": rewritten}}).to_string())
}

pub fn rewrite_decision(payload: &Value, rewrite: impl FnOnce(&str) -> Option<String>) -> Option<String> {
    let command = shell_command(payload)?;
    let rewritten = rewrite(&command)?;
    decision(&command, &rewritten)
}

pub fn pre_tool_use_decision(data_dir: &Path, stdin_text: &str) -> Option<String> {
    pre_tool_use_decision_with(data_dir, stdin_text, REWRITE_TIMEOUT)
}

fn pre_tool_use_decision_with(data_dir: &Path, stdin_text: &str, timeout: Duration) -> Option<String> {
    let rtk = rtk_path_from(data_dir)?;
    let payload: Value = serde_json::from_str(stdin_text.trim_start_matches('\u{feff}')).ok()?;
    rewrite_decision(&payload, |c| run_rtk_rewrite(&rtk, c, timeout))
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Dir(PathBuf);
    impl Dir {
        fn new() -> Self {
            let p = std::env::temp_dir().join(format!("am-agy-rtk-{}-{:?}", std::process::id(), std::thread::current().id()).replace(['(', ')', ' '], "")); // scratch-guard: allow Dir 的 Drop 會刪掉這個目錄
            let _ = std::fs::remove_dir_all(&p);
            std::fs::create_dir_all(&p).unwrap();
            Dir(p)
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn payload(name: &str, command: &str) -> Value {
        json!({"hookEventName": "PreToolUse", "toolCall": {"name": name, "args": {"CommandLine": command, "Cwd": "/tmp"}}})
    }

    fn fake_rtk(dir: &Path, body: &str) -> PathBuf {
        let p = dir.join("rtk");
        crate::test_dirs::write_exec(&p, format!("#!/bin/sh\n{body}\n"));
        p
    }

    #[test]
    fn a_changed_command_becomes_an_overwrite_and_the_rest_is_left_alone() {
        let out = rewrite_decision(&payload("run_command", "git status"), |_| Some("rtk git status".into())).unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v, json!({"decision": "allow", "overwrite": {"CommandLine": "rtk git status"}}));
        assert_eq!(rewrite_decision(&payload("run_command", "git status"), |_| Some("git status".into())), None);
        assert_eq!(rewrite_decision(&payload("run_command", "ls"), |_| None), None);
        assert_eq!(rewrite_decision(&payload("view_file", "git status"), |_| panic!("not a shell tool")), None);
        assert_eq!(rewrite_decision(&json!({"toolCall": {"name": "run_command", "args": {}}}), |_| panic!("no command")), None);
        assert_eq!(rewrite_decision(&payload("run_command", "  "), |_| panic!("blank")), None);
    }

    #[test]
    fn the_real_rtk_runner_prints_the_rewrite_and_fails_open() {
        let d = Dir::new();
        let ok = fake_rtk(d.path(), "[ \"$1\" = rewrite ] && printf 'rtk %s\\n' \"$2\" && exit 3");
        assert_eq!(run_rtk_rewrite(&ok, "git status", Duration::from_secs(2)).as_deref(), Some("rtk git status"));
        let none = fake_rtk(d.path(), "exit 1");
        assert_eq!(run_rtk_rewrite(&none, "ls", Duration::from_secs(2)), None);
        let slow = fake_rtk(d.path(), "sleep 5; echo rtk x");
        let t = Instant::now();
        assert_eq!(run_rtk_rewrite(&slow, "x", Duration::from_millis(200)), None);
        assert!(t.elapsed() < Duration::from_secs(2), "逾時要很快放行");
        assert_eq!(run_rtk_rewrite(Path::new("/nonexistent/rtk"), "x", Duration::from_secs(1)), None);
    }

    #[test]
    fn only_an_enabled_host_with_an_existing_rtk_rewrites() {
        let d = Dir::new();
        let text = payload("run_command", "git status").to_string();
        assert_eq!(pre_tool_use_decision(d.path(), &text), None, "沒有啟用檔：不改寫");
        let rtk = fake_rtk(d.path(), "printf 'rtk %s\\n' \"$2\"");
        std::fs::write(d.path().join(ENABLE_FILE), rtk.to_string_lossy().as_bytes()).unwrap();
        let out = pre_tool_use_decision_with(d.path(), &text, Duration::from_secs(10)).unwrap();
        assert!(out.contains("rtk git status"));
        std::fs::write(d.path().join(ENABLE_FILE), "relative/rtk").unwrap();
        assert_eq!(pre_tool_use_decision(d.path(), &text), None, "相對路徑不收");
        std::fs::write(d.path().join(ENABLE_FILE), "/nonexistent/rtk").unwrap();
        assert_eq!(pre_tool_use_decision(d.path(), &text), None, "rtk 不在：放行");
        assert_eq!(pre_tool_use_decision(d.path(), "not json"), None);
    }
}
