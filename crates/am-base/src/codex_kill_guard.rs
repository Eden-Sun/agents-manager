//! Codex 的廣域 kill 守門（SPEC §4.4c，使用者 2026-11-28）：`hooks.json` 的 `PreToolUse` 多一組 `{"matcher":"Bash",…}`，
//! 本機指向 `agents-managerd hook codex`、遠端指向遠端根目錄的 `kill-guard.sh codex`。
//!
//! codex 對未信任的 hook 不執行，所以這一組跟 Stop hook 在**同一次**使用者確認裡寫入（`codex_hook::install_and_trust_*`），
//! 同樣先寫 `trusted_hash`、後寫 handler。信任鍵是 `<hooks.json>:pre_tool_use:<組>:0`，hash 的身分含 matcher
//! （codex 0.172 `hooks/src/engine/discovery.rs` 的 `hook_hash`）。
//!
//! 守門失效一律擋：codex 只把 exit 0＋決策 JSON 或 exit 2＋stderr 當成擋下，其他退出碼、逾時都是**放行**，
//! 所以指令外層包 `|| { echo <提示> >&2; exit 2; }`。不在 AG Man pane（沒有 `AM_BOT_ID`，例如使用者自己開的 codex）直接放行。

use crate::codex_hook::{recorded_trusted_hash, trust_edit, StopHookStatus};
use crate::hosts::{sh_quote, HostConn};
use anyhow::{anyhow, bail, Result};
use std::path::Path;

/// codex 的 shell 工具（`exec_command`／`shell`）在 hook 裡的 `tool_name` 一律是 `Bash`（`HookToolName::bash()`）。
pub const MATCHER: &str = "Bash";
const TIMEOUT_SECS: u64 = 5;
const EVENT: &str = "PreToolUse";
const EVENT_KEY: &str = "pre_tool_use";

/// 給 `agm health` 與設定頁的一句話。
pub const NOT_EFFECTIVE: &str = "codex kill 守門未生效";

fn fail_closed(inner: &str) -> String {
    format!(
        "[ -z \"${{AM_BOT_ID:-}}\" ] || {inner} || {{ echo {} >&2; exit 2; }}",
        sh_quote(crate::kill_guard::KILL_GUARD_UNAVAILABLE_MSG)
    )
}

/// 本機 `PreToolUse` 指令：pane 的 `$AM_DAEMON_EXE`（daemon 換版瞬間不存在＝擋下並提示重試）。
pub fn local_command(data_dir: &Path) -> String {
    fail_closed(&format!("\"$AM_DAEMON_EXE\" hook codex --bot \"$AM_BOT_ID\" --data-dir {}", sh_quote(&data_dir.to_string_lossy())))
}

/// 遠端 `PreToolUse` 指令：`<遠端根>/kill-guard.sh codex`（`install_remote_hook` 裝的同一支）。
pub fn remote_command(kill_guard_sh: &str) -> String {
    fail_closed(&format!("{} codex", sh_quote(kill_guard_sh)))
}

fn canonical_group(command: &str) -> serde_json::Value {
    serde_json::json!({"matcher": MATCHER, "hooks": [{"type": "command", "command": command, "timeout": TIMEOUT_SECS}]})
}

/// codex 記在 `[hooks.state."<key>"].trusted_hash` 的值：鍵排序緊湊 JSON 的 sha256，身分含 `matcher`。
pub fn trust_hash(command: &str) -> String {
    use sha2::{Digest, Sha256};
    let identity = serde_json::json!({
        "event_name": EVENT_KEY,
        "matcher": MATCHER,
        "hooks": [{"async": false, "command": command, "timeout": TIMEOUT_SECS, "type": "command"}],
    });
    let digest = Sha256::digest(serde_json::to_vec(&identity).unwrap_or_default());
    format!("sha256:{}", digest.iter().map(|b| format!("{b:02x}")).collect::<String>())
}

pub fn state_key(hooks_path: &str, group_index: usize) -> String {
    format!("{hooks_path}:{EVENT_KEY}:{group_index}:0")
}

/// 把我們的組併進 `hooks.json`：已有且形狀完全相符＝`(None, 位置)`；沒有＝接在 `PreToolUse` 尾端（不動別組的位置）。
/// 同指令的組被改過或有兩組：`Err`，不動檔案。
pub fn merge(existing: &str, command: &str) -> Result<(Option<String>, usize)> {
    let mut doc: serde_json::Value = if existing.trim().is_empty() {
        serde_json::json!({})
    } else {
        serde_json::from_str(existing).map_err(|_| anyhow!("invalid JSON in hooks.json"))?
    };
    let root = doc.as_object_mut().ok_or_else(|| anyhow!("hooks.json root must be an object"))?;
    let hooks = root.entry("hooks").or_insert_with(|| serde_json::json!({})).as_object_mut().ok_or_else(|| anyhow!("hooks must be an object"))?;
    let groups = hooks.entry(EVENT).or_insert_with(|| serde_json::json!([])).as_array_mut().ok_or_else(|| anyhow!("hooks.{EVENT} must be an array"))?;
    let expected = canonical_group(command);
    let ours: Vec<usize> = groups
        .iter()
        .enumerate()
        .filter(|(_, g)| g.get("hooks").and_then(|h| h.as_array()).is_some_and(|hs| hs.iter().any(|h| h.get("command").and_then(|c| c.as_str()) == Some(command))))
        .map(|(i, _)| i)
        .collect();
    match ours.as_slice() {
        [] => {
            groups.push(expected);
            let index = groups.len() - 1;
            Ok((Some(serde_json::to_string_pretty(&doc)?), index))
        }
        [i] if groups[*i] == expected => Ok((None, *i)),
        _ => bail!("ambiguous or modified {EVENT} kill-guard handler"),
    }
}

/// 兩個檔案的文字 → 狀態。`hooks.json` 讀不懂或我們的組被改過：`Err`。
pub fn status_in(hooks_text: &str, config_text: &str, hooks_path: &str, command: &str) -> Result<StopHookStatus> {
    let (merged, index) = merge(hooks_text, command)?;
    if merged.is_some() {
        return Ok(StopHookStatus::NotInstalled);
    }
    Ok(match recorded_trusted_hash(config_text, &state_key(hooks_path, index)) {
        None => StopHookStatus::Untrusted,
        Some(h) if h == trust_hash(command) => StopHookStatus::Trusted,
        Some(_) => StopHookStatus::HashMismatch,
    })
}

pub fn local_status(codex_home: &Path, data_dir: &Path) -> Result<StopHookStatus> {
    let hooks = codex_home.join("hooks.json");
    let hooks_text = std::fs::read_to_string(&hooks).unwrap_or_default();
    let config_text = std::fs::read_to_string(codex_home.join("config.toml")).unwrap_or_default();
    status_in(&hooks_text, &config_text, &hooks.to_string_lossy(), &local_command(data_dir))
}

pub async fn remote_status(conn: &HostConn, codex_home: &str, kill_guard_sh: &str) -> Result<StopHookStatus> {
    let home = codex_home.trim_end_matches('/');
    let hooks = format!("{home}/hooks.json");
    let hooks_text = crate::trust::read_remote_text(conn, &hooks).await?;
    let config_text = crate::trust::read_remote_text(conn, &format!("{home}/config.toml")).await?;
    status_in(&hooks_text, &config_text, &hooks, &remote_command(kill_guard_sh))
}

/// 使用者確認後（本機）：先寫信任、後寫 handler，寫前由呼叫端備份 config.toml；寫後讀回不是 trusted 就報錯。
pub fn install_and_trust_local(codex_home: &Path, data_dir: &Path) -> Result<()> {
    let hooks = codex_home.join("hooks.json");
    let config = codex_home.join("config.toml");
    let command = local_command(data_dir);
    let (_, index) = merge(&std::fs::read_to_string(&hooks).unwrap_or_default(), &command)?;
    let key = state_key(&hooks.to_string_lossy(), index);
    let hash = trust_hash(&command);
    crate::trust::update_file(&config, |cur| trust_edit(cur, &key, &hash))?;
    crate::trust::update_file(&hooks, |cur| {
        let (merged, now) = merge(cur, &command)?;
        if now != index {
            bail!("hook positions changed during install");
        }
        Ok(merged)
    })?;
    if local_status(codex_home, data_dir)? != StopHookStatus::Trusted {
        bail!("Codex kill guard is not trusted after the write (check config.toml for a conflicting entry)");
    }
    Ok(())
}

/// 遠端版的 [`install_and_trust_local`]；`kill-guard.sh` 也在這裡補寫（信任可能早於這台第一次啟動 bot）。
pub async fn install_and_trust_remote(conn: &HostConn, codex_home: &str, kill_guard_sh: &str) -> Result<()> {
    let out = conn.ssh_exec(&crate::codex_hook::write_remote_dispatcher_script(kill_guard_sh, &crate::kill_guard::remote_kill_guard_sh())).await?;
    if !out.contains("AM_CODEX_DISPATCH_OK") {
        bail!("remote kill-guard.sh installation failed");
    }
    let home = codex_home.trim_end_matches('/');
    let hooks = format!("{home}/hooks.json");
    let config = format!("{home}/config.toml");
    let command = remote_command(kill_guard_sh);
    let (_, index) = merge(&crate::trust::read_remote_text(conn, &hooks).await?, &command)?;
    let key = state_key(&hooks, index);
    let hash = trust_hash(&command);
    crate::trust::update_remote_file(conn, &config, |cur| trust_edit(cur, &key, &hash)).await?;
    crate::trust::update_remote_file(conn, &hooks, |cur| {
        let (merged, now) = merge(cur, &command)?;
        if now != index {
            bail!("hook positions changed during install");
        }
        Ok(merged)
    })
    .await?;
    if remote_status(conn, codex_home, kill_guard_sh).await? != StopHookStatus::Trusted {
        bail!("Codex kill guard is not trusted after the write (check config.toml for a conflicting entry)");
    }
    Ok(())
}

/// 最近一次看到的各 (主機, CODEX_HOME) 守門狀態：bot 啟動與設定頁讀取時更新，`agm health` 讀這份（health 不另跑 ssh）。
fn seen() -> &'static std::sync::Mutex<std::collections::BTreeMap<(String, String), String>> {
    static SEEN: std::sync::OnceLock<std::sync::Mutex<std::collections::BTreeMap<(String, String), String>>> = std::sync::OnceLock::new();
    SEEN.get_or_init(Default::default)
}

pub fn status_label(s: StopHookStatus) -> &'static str {
    match s {
        StopHookStatus::NotInstalled => "not_installed",
        StopHookStatus::Untrusted => "untrusted",
        StopHookStatus::HashMismatch => "hash_mismatch",
        StopHookStatus::Trusted => "trusted",
    }
}

pub fn record(host: &str, codex_home: &str, status: &str) {
    seen().lock().unwrap_or_else(|e| e.into_inner()).insert((host.to_string(), codex_home.to_string()), status.to_string());
}

fn note(host: &str, codex_home: &str, label: &str) {
    record(host, codex_home, label);
    if label != "trusted" {
        tracing::warn!(host, codex_home, status = label, "{NOT_EFFECTIVE}: trust the hooks in Settings → Codex hook trust");
    }
}

/// bot 啟動時（本機）讀一次狀態，記給 `agm health`；沒生效只 warn，不擋啟動。
pub fn observe_local(host: &str, codex_home: &str, data_dir: &Path) {
    note(host, codex_home, local_status(Path::new(codex_home), data_dir).map(status_label).unwrap_or("unavailable"));
}

/// 遠端版的 [`observe_local`]，`budget` 內讀不完記成 `unavailable`。
pub async fn observe_remote(conn: &HostConn, host: &str, codex_home: &str, remote_root: &str, budget: std::time::Duration) {
    let probe = async {
        let guard = format!("{}/{}/{}", conn.home().await?, remote_root.trim_end_matches('/'), crate::kill_guard::REMOTE_KILL_GUARD_SH);
        remote_status(conn, codex_home, &guard).await
    };
    let label = match tokio::time::timeout(budget, probe).await {
        Ok(Ok(s)) => status_label(s),
        _ => "unavailable",
    };
    note(host, codex_home, label);
}

/// `agm health` 的 `codex_kill_guard` 段：沒有生效的帳號逐一列出，附 [`NOT_EFFECTIVE`]。
pub fn health_json() -> serde_json::Value {
    let map = seen().lock().unwrap_or_else(|e| e.into_inner());
    let not_effective: Vec<serde_json::Value> = map
        .iter()
        .filter(|(_, s)| s.as_str() != "trusted")
        .map(|((host, home), s)| serde_json::json!({"host": host, "codex_home": home, "status": s, "message": NOT_EFFECTIVE}))
        .collect();
    serde_json::json!({
        "status": if not_effective.is_empty() { "ok" } else { "not_effective" },
        "checked": map.len(),
        "not_effective": not_effective,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trust_hash_identity_includes_the_bash_matcher() {
        let identity = serde_json::json!({
            "event_name": "pre_tool_use",
            "hooks": [{"async": false, "command": "x", "timeout": 5, "type": "command"}],
            "matcher": "Bash",
        });
        assert_eq!(serde_json::to_string(&identity).unwrap(), r#"{"event_name":"pre_tool_use","hooks":[{"async":false,"command":"x","timeout":5,"type":"command"}],"matcher":"Bash"}"#);
        assert_ne!(trust_hash("x"), crate::codex_hook::stop_hook_trust_hash("x"));
    }

    /// Real Codex 0.172.0 `hooks/list` `currentHash` for [`local_command`] with data dir `/d` (2026-11-28 live verification).
    #[test]
    fn trust_hash_matches_codex_0_172_current_hash() {
        assert_eq!(
            trust_hash("[ -z \"${AM_BOT_ID:-}\" ] || \"$AM_DAEMON_EXE\" hook codex --bot \"$AM_BOT_ID\" --data-dir '/d' || { echo 'x' >&2; exit 2; }"),
            "sha256:795ea87b66936f6ee6a63247053e9c62e98b7ef201d9c0095ed48f16853c3163"
        );
    }

    #[test]
    fn merge_appends_after_user_groups_and_is_idempotent() {
        let cmd = local_command(Path::new("/d"));
        let existing = serde_json::json!({"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"user"}]}],"Stop":[]}}).to_string();
        let (merged, idx) = merge(&existing, &cmd).unwrap();
        assert_eq!(idx, 1);
        let merged = merged.unwrap();
        let v: serde_json::Value = serde_json::from_str(&merged).unwrap();
        assert_eq!(v["hooks"]["PreToolUse"][0]["hooks"][0]["command"], "user");
        assert_eq!(v["hooks"]["PreToolUse"][1]["matcher"], "Bash");
        assert_eq!(merge(&merged, &cmd).unwrap(), (None, 1));
        let edited = merged.replace("\"timeout\": 5", "\"timeout\": 9");
        assert!(merge(&edited, &cmd).is_err(), "a group of ours someone changed is reported, not rewritten");
    }

    #[test]
    fn status_covers_every_state() {
        let cmd = remote_command("/h/.config/agents-manager/kill-guard.sh");
        assert_eq!(status_in("", "", "/c/hooks.json", &cmd).unwrap(), StopHookStatus::NotInstalled);
        let (hooks, idx) = merge("", &cmd).unwrap();
        let hooks = hooks.unwrap();
        assert_eq!(status_in(&hooks, "", "/c/hooks.json", &cmd).unwrap(), StopHookStatus::Untrusted);
        let key = state_key("/c/hooks.json", idx);
        assert_eq!(key, "/c/hooks.json:pre_tool_use:0:0");
        let stale = format!("[hooks.state.{key:?}]\ntrusted_hash = \"sha256:stale\"\n");
        assert_eq!(status_in(&hooks, &stale, "/c/hooks.json", &cmd).unwrap(), StopHookStatus::HashMismatch);
        let good = trust_edit("", &key, &trust_hash(&cmd)).unwrap().unwrap();
        assert_eq!(status_in(&hooks, &good, "/c/hooks.json", &cmd).unwrap(), StopHookStatus::Trusted);
    }

    fn run_sh(command: &str, env: &[(&str, &str)], stdin: &str) -> std::process::Output {
        use std::io::Write as _;
        let mut c = std::process::Command::new("sh");
        c.arg("-c").arg(command).env_remove("AM_BOT_ID").env_remove("AM_DAEMON_EXE");
        for (k, v) in env {
            c.env(k, v);
        }
        let mut child = c.stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::piped()).stderr(std::process::Stdio::piped()).spawn().unwrap();
        child.stdin.take().unwrap().write_all(stdin.as_bytes()).unwrap();
        child.wait_with_output().unwrap()
    }

    #[test]
    fn the_command_fails_closed_only_inside_ag_man_panes() {
        let local = local_command(Path::new("/no/such/data"));
        let outside = run_sh(&local, &[], "{}");
        assert!(outside.status.success() && outside.stdout.is_empty(), "the user's own codex is never blocked");
        let broken = run_sh(&local, &[("AM_BOT_ID", "b1"), ("AM_DAEMON_EXE", "/no/such/agents-managerd")], "{}");
        assert_eq!(broken.status.code(), Some(2), "codex blocks only on exit 2 with stderr");
        assert!(String::from_utf8_lossy(&broken.stderr).contains(crate::kill_guard::KILL_GUARD_UNAVAILABLE_MSG));
        let remote = run_sh(&remote_command("/no/such/kill-guard.sh"), &[("AM_BOT_ID", "b1")], "{}");
        assert_eq!(remote.status.code(), Some(2));
    }

    #[test]
    fn the_remote_guard_denies_a_codex_payload_in_the_shape_codex_parses() {
        let dir = crate::test_dirs::scratch_dir("am-codex-kill-guard-remote");
        let guard = dir.join("kill-guard.sh");
        std::fs::write(&guard, crate::kill_guard::remote_kill_guard_sh()).unwrap();
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&guard, std::fs::Permissions::from_mode(0o700)).unwrap();
        }
        let payload = codex_payload("kill -9 -1");
        let out = run_sh(&remote_command(&guard.to_string_lossy()), &[("AM_BOT_ID", "b1")], &payload);
        assert!(out.status.success());
        let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(v["hookSpecificOutput"]["hookEventName"], "PreToolUse");
        assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "deny");
        assert!(!v["hookSpecificOutput"]["permissionDecisionReason"].as_str().unwrap().is_empty());
        let allow = payload.replace("kill -9 -1", "ls -la");
        let out = run_sh(&remote_command(&guard.to_string_lossy()), &[("AM_BOT_ID", "b1")], &allow);
        assert!(out.status.success() && out.stdout.is_empty());
    }

    fn codex_payload(command: &str) -> String {
        serde_json::json!({
            "session_id": "s", "turn_id": "t", "transcript_path": null, "cwd": "/w",
            "hook_event_name": "PreToolUse", "model": "m", "permission_mode": "default",
            "tool_name": "Bash", "tool_use_id": "call_1", "tool_input": {"command": command},
        })
        .to_string()
    }

    #[test]
    fn the_local_hook_reads_a_codex_payload_and_denies_in_hook_specific_output() {
        use crate::kill_guard::{guard_logged, Guard};
        let Guard::Deny(out) = guard_logged("codex", "", &codex_payload("kill -9 -1"), &mut |_| {}) else { panic!("kill -9 -1 must be denied") };
        let v: serde_json::Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["hookSpecificOutput"]["hookEventName"], "PreToolUse");
        assert_eq!(v["hookSpecificOutput"]["permissionDecision"], "deny");
        assert!(v["hookSpecificOutput"]["permissionDecisionReason"].as_str().is_some_and(|r| !r.trim().is_empty()));
        assert_eq!(guard_logged("codex", "", &codex_payload("lsof -ti :3917 | xargs kill"), &mut |_| {}), Guard::Pass);
        assert!(matches!(guard_logged("codex", "", &codex_payload("pkill -f next"), &mut |_| {}), Guard::Deny(_)));
    }

    #[test]
    fn install_and_trust_local_writes_the_exact_hash_and_keeps_user_bytes() {
        let dir = crate::test_dirs::scratch_dir("am-codex-kill-guard-trust");
        let home = dir.join("codex");
        let data = dir.join("data");
        std::fs::create_dir_all(&home).unwrap();
        let config = "# user\n[projects.\"/x\"]\ntrust_level = \"trusted\"\n";
        std::fs::write(home.join("config.toml"), config).unwrap();
        assert_eq!(local_status(&home, &data).unwrap(), StopHookStatus::NotInstalled);
        install_and_trust_local(&home, &data).unwrap();
        assert_eq!(local_status(&home, &data).unwrap(), StopHookStatus::Trusted);
        let after = std::fs::read_to_string(home.join("config.toml")).unwrap();
        assert!(after.starts_with(config));
        assert!(after.contains(&trust_hash(&local_command(&data))));
        install_and_trust_local(&home, &data).unwrap();
        let v: serde_json::Value = serde_json::from_slice(&std::fs::read(home.join("hooks.json")).unwrap()).unwrap();
        assert_eq!(v["hooks"]["PreToolUse"].as_array().unwrap().len(), 1, "a second trust does not add a second group");
    }

    #[test]
    fn health_lists_accounts_where_the_guard_is_not_effective() {
        record("hz-test-host", "/a", "trusted");
        record("hz-test-host", "/b", "untrusted");
        let v = health_json();
        assert_eq!(v["status"], "not_effective");
        let rows = v["not_effective"].as_array().unwrap();
        let row = rows.iter().find(|r| r["host"] == "hz-test-host").unwrap();
        assert_eq!(row["codex_home"], "/b");
        assert_eq!(row["message"], NOT_EFFECTIVE);
        assert!(!rows.iter().any(|r| r["host"] == "hz-test-host" && r["codex_home"] == "/a"));
    }
}
