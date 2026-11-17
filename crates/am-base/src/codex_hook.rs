//! Opt-in Codex Stop dispatcher installation; trust remains user-owned.
use crate::hosts::{sh_quote, HostConn};
use anyhow::{anyhow, bail, Context, Result};
use std::path::Path;
pub const CODEX_HOOK_SH: &str = "codex-hook.sh";

/// Script body for the local `codex-hook.sh` dispatcher.
///
/// Guards against non-daemon execution:
/// - Requires `AM_BOT_ID` and `AM_HOOK_TOKEN`.
/// - Requires empty `AM_CHILD_OF` (child processes are no-op).
/// - Requires non-empty `AM_DAEMON_EXE` and `AM_PORT`.
/// - Enforces `AM_INSTANCE` match.
///
/// On match, passes stdin straight to `exec "$AM_DAEMON_EXE" hook codex --bot "$AM_BOT_ID" --port "$AM_PORT" --data-dir <sh_quote(data_dir)>` without 二次截斷 or no-op cat.
pub fn local_dispatch_sh(data_dir: &Path, instance: Option<&str>) -> String {
    let gate = match instance {
        Some(slug) => format!(
            "[ \"${{AM_INSTANCE:-}}\" = {} ] || exit 0\n",
            sh_quote(slug)
        ),
        None => "[ -z \"${AM_INSTANCE:-}\" ] || exit 0\n".to_string(),
    };
    let data_dir_quoted = sh_quote(&data_dir.to_string_lossy());
    format!(
        "#!/bin/sh\n\
         # agents-manager codex dispatcher (local). Installed by the daemon; no-op outside daemon panes.\n\
         [ -n \"$AM_BOT_ID\" ] && [ -n \"$AM_HOOK_TOKEN\" ] || exit 0\n\
         [ -z \"${{AM_CHILD_OF:-}}\" ] || exit 0\n\
         [ -n \"$AM_DAEMON_EXE\" ] && [ -n \"$AM_PORT\" ] || exit 0\n\
         {gate}\
         exec \"$AM_DAEMON_EXE\" hook codex --bot \"$AM_BOT_ID\" --port \"$AM_PORT\" --data-dir {data_dir_quoted}\n",
        gate = gate,
        data_dir_quoted = data_dir_quoted,
    )
}

/// Script body for the remote `codex-hook.sh` dispatcher.
///
/// On match, passes stdin straight to `exec "$H" codex "$AM_BOT_ID" -` without 二次截斷 or no-op cat.
pub fn remote_dispatch_sh(remote_root: &str, instance: Option<&str>) -> String {
    let root = remote_root.trim_end_matches('/');
    let gate = match instance {
        Some(slug) => format!(
            "[ \"${{AM_INSTANCE:-}}\" = {} ] || exit 0\n",
            sh_quote(slug)
        ),
        None => "[ -z \"${AM_INSTANCE:-}\" ] || exit 0\n".to_string(),
    };
    format!(
        "#!/bin/sh\n\
         # agents-manager codex dispatcher (remote). Installed by the daemon; no-op outside daemon panes.\n\
         [ -n \"$AM_BOT_ID\" ] && [ -n \"$AM_HOOK_TOKEN\" ] || exit 0\n\
         [ -z \"${{AM_CHILD_OF:-}}\" ] || exit 0\n\
         {gate}\
         H=\"$HOME/{root}/bots/$AM_BOT_ID/hook.sh\"\n\
         [ -x \"$H\" ] || exit 0\n\
         exec \"$H\" codex \"$AM_BOT_ID\" -\n",
        gate = gate,
        root = root,
    )
}

/// Helper to write an executable dispatcher atomically with mode 0700.
fn write_local_dispatcher(path: &Path, content: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        crate::private_files::create_private_dir(parent)?;
    }
    crate::atomic_file::write(
        path,
        content.as_bytes(),
        crate::atomic_file::Mode::Exact(0o700),
    )
    .with_context(|| format!("writing local codex dispatcher {}", path.display()))
}

/// Shell script snippet to write the remote dispatcher script atomically (0700).
fn write_remote_dispatcher_script(path: &str, text: &str) -> String {
    let mut delim = String::from("AM_CODEX_DISPATCH_EOF");
    while text.contains(&delim) {
        delim.push('_');
    }
    format!(
        "set -e\numask 077\nW={w}\nmkdir -p \"$(dirname \"$W\")\"\nWT=$(mktemp \"$W.tmp.XXXXXX\")\ntrap 'rm -f \"$WT\"' EXIT\ncat > \"$WT\" <<'{delim}'\n{text}{delim}\nchmod 700 \"$WT\"\nmv -f \"$WT\" \"$W\"\nprintf 'AM_CODEX_DISPATCH_OK\\n'\n",
        w = sh_quote(path),
    )
}

/// Result of merging the canonical Stop dispatcher hook into `hooks.json`.
#[derive(Debug, PartialEq, Eq)]
pub struct HooksMergeOutcome {
    /// Merged JSON text (or `None` if content was already identical and unchanged).
    pub merged_json: Option<String>,
    /// Zero-based group index of the matched or appended Stop hook group.
    pub group_index: usize,
}

/// Constructs the canonical Stop group Value:
/// `{ "hooks": [ { "type": "command", "command": "<canonical_command>", "timeout": 5 } ] }`
fn canonical_stop_group(canonical_command: &str) -> serde_json::Value {
    serde_json::json!({
        "hooks": [
            {
                "type": "command",
                "command": canonical_command,
                "timeout": 5
            }
        ]
    })
}

/// Validates or appends the canonical Stop hook handler into `hooks.json` text.
///
/// Actual `hooks.json` structure:
/// `{ "hooks": { "SessionStart": [ ... ], "Stop": [ ... ] } }`
///
/// Rules:
/// - If existing is empty, creates a new object: `{ "hooks": { "Stop": [ canonical_group ] } }`.
/// - If existing is invalid JSON, returns Err (never overwrite unparseable files).
/// - Preserves all other top-level keys (e.g. description, herdr), events under "hooks", handler groups, and ordering.
/// - If a group contains a hook with our command:
///   - It must match the canonical group Value EXACTLY (no extra fields like async/matcher, no multiple hooks).
///   - If matched, returns `group_index` without modification (`merged_json: None`).
///   - If there is ambiguity (multiple matching groups) or the group does not match the canonical Value exactly, returns Err.
/// - If not present, appends the canonical group at the end of `hooks.Stop` (preserving existing group indices).
pub fn merge_hooks_json(existing: &str, canonical_command: &str) -> Result<HooksMergeOutcome> {
    let trimmed = existing.trim();
    let expected_group = canonical_stop_group(canonical_command);

    if trimmed.is_empty() {
        let val = serde_json::json!({
            "hooks": {
                "Stop": [ expected_group ]
            }
        });
        return Ok(HooksMergeOutcome {
            merged_json: Some(serde_json::to_string_pretty(&val)?),
            group_index: 0,
        });
    }

    let mut doc: serde_json::Value =
        serde_json::from_str(existing).map_err(|_| anyhow!("invalid JSON in hooks.json"))?;

    let root_obj = doc
        .as_object_mut()
        .ok_or_else(|| anyhow!("hooks.json root must be a JSON object"))?;

    if !root_obj.contains_key("hooks") {
        root_obj.insert(
            "hooks".to_string(),
            serde_json::json!({
                "Stop": [ expected_group ]
            }),
        );
        return Ok(HooksMergeOutcome {
            merged_json: Some(serde_json::to_string_pretty(&doc)?),
            group_index: 0,
        });
    }

    let hooks_val = root_obj.get_mut("hooks").unwrap();
    let hooks_obj = hooks_val
        .as_object_mut()
        .ok_or_else(|| anyhow!("hooks.json 'hooks' property must be an object"))?;

    if !hooks_obj.contains_key("Stop") {
        hooks_obj.insert("Stop".to_string(), serde_json::json!([expected_group]));
        return Ok(HooksMergeOutcome {
            merged_json: Some(serde_json::to_string_pretty(&doc)?),
            group_index: 0,
        });
    }

    let stop_val = hooks_obj.get_mut("Stop").unwrap();
    let stop_arr = stop_val
        .as_array_mut()
        .ok_or_else(|| anyhow!("hooks.json hooks.Stop must be an array"))?;

    let mut matched_group: Option<usize> = None;
    for (idx, group) in stop_arr.iter().enumerate() {
        let has_command = group
            .get("hooks")
            .and_then(|h| h.as_array())
            .map(|arr| {
                arr.iter()
                    .any(|h| h.get("command").and_then(|c| c.as_str()) == Some(canonical_command))
            })
            .unwrap_or(false);

        if has_command {
            if matched_group.is_some() {
                bail!("ambiguous match: multiple Stop groups with command {canonical_command:?}");
            }
            if group != &expected_group {
                bail!("existing Stop group with command {canonical_command:?} has non-canonical shape or extra fields");
            }
            matched_group = Some(idx);
        }
    }

    if let Some(idx) = matched_group {
        Ok(HooksMergeOutcome {
            merged_json: None,
            group_index: idx,
        })
    } else {
        let group_idx = stop_arr.len();
        stop_arr.push(expected_group);
        Ok(HooksMergeOutcome {
            merged_json: Some(serde_json::to_string_pretty(&doc)?),
            group_index: group_idx,
        })
    }
}
/// Hash Codex 0.169 stores in `[hooks.state."<hooks.json>:stop:<group>:0"].trusted_hash` for our canonical Stop handler:
/// sha256 of the key-sorted compact JSON of the normalized identity (verified against `hooks/list` `currentHash`, #1989).
pub fn stop_hook_trust_hash(command: &str) -> String {
    use sha2::{Digest, Sha256};
    // serde_json::Value objects are BTreeMap-ordered, so serialization is key-sorted.
    let identity = serde_json::json!({
        "event_name": "stop",
        "hooks": [{"async": false, "command": command, "timeout": 5, "type": "command"}],
    });
    let digest = Sha256::digest(serde_json::to_vec(&identity).unwrap_or_default());
    format!("sha256:{}", digest.iter().map(|b| format!("{b:02x}")).collect::<String>())
}

/// Where Codex records trust for our Stop handler.
pub fn stop_hook_state_key(hooks_path: &str, group_index: usize) -> String {
    format!("{hooks_path}:stop:{group_index}:0")
}

/// `[hooks.state."<key>"].trusted_hash` as written by the user's own review; unparseable config reads as absent.
fn recorded_trusted_hash(config_text: &str, key: &str) -> Option<String> {
    let doc = config_text.parse::<toml::Table>().ok()?;
    doc.get("hooks")?.get("state")?.get(key)?.get("trusted_hash")?.as_str().map(str::to_owned)
}

/// Whether Codex will run our Stop handler (#1989).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StopHookStatus {
    /// Our group is not in `hooks.json` yet.
    NotInstalled,
    /// Installed, no trust recorded.
    Untrusted,
    /// Trust recorded for this slot but for a different handler (command changed, or user edited it).
    HashMismatch,
    Trusted,
}

/// Pure status from the two files' text. `Err` when `hooks.json` is unparseable or our group is ambiguous.
pub fn stop_hook_status_in(hooks_text: &str, config_text: &str, hooks_path: &str, command: &str) -> Result<(StopHookStatus, Option<usize>)> {
    let merged = merge_hooks_json(hooks_text, command)?;
    if merged.merged_json.is_some() {
        return Ok((StopHookStatus::NotInstalled, None));
    }
    let key = stop_hook_state_key(hooks_path, merged.group_index);
    let status = match recorded_trusted_hash(config_text, &key) {
        None => StopHookStatus::Untrusted,
        Some(h) if h == stop_hook_trust_hash(command) => StopHookStatus::Trusted,
        Some(_) => StopHookStatus::HashMismatch,
    };
    Ok((status, Some(merged.group_index)))
}

/// Set only `[hooks.state."<key>"].trusted_hash`, keeping every other byte of the user's config. `None` when already correct.
pub fn trust_edit(config_text: &str, key: &str, hash: &str) -> Result<Option<String>> {
    use toml_edit::{DocumentMut, Item, Table};
    let mut doc: DocumentMut = config_text.parse().context("config.toml is not valid TOML")?;
    fn sub<'a>(parent: &'a mut Table, name: &str) -> Result<&'a mut Table> {
        if !parent.contains_key(name) {
            let mut t = Table::new();
            t.set_implicit(true);
            parent.insert(name, Item::Table(t));
        }
        parent.get_mut(name).and_then(Item::as_table_mut).ok_or_else(|| anyhow!("`{name}` in config.toml is not a table"))
    }
    let state = sub(sub(doc.as_table_mut(), "hooks")?, "state")?;
    let entry = sub(state, key)?;
    if entry.get("trusted_hash").and_then(Item::as_str) == Some(hash) {
        return Ok(None);
    }
    entry.set_implicit(false);
    entry["trusted_hash"] = toml_edit::value(hash);
    Ok(Some(doc.to_string()))
}

/// Local status for one Codex home.
pub fn local_stop_hook_status(codex_home: &Path, command: &str) -> Result<StopHookStatus> {
    let hooks = codex_home.join("hooks.json");
    let hooks_text = std::fs::read_to_string(&hooks).unwrap_or_default();
    let config_text = std::fs::read_to_string(codex_home.join("config.toml")).unwrap_or_default();
    Ok(stop_hook_status_in(&hooks_text, &config_text, &hooks.to_string_lossy(), command)?.0)
}

/// User-confirmed trust (local): back up `config.toml`, then write only our one key.
pub fn local_trust_stop_hook(codex_home: &Path, command: &str) -> Result<()> {
    let hooks = codex_home.join("hooks.json");
    let hooks_text = std::fs::read_to_string(&hooks).unwrap_or_default();
    let (status, idx) = stop_hook_status_in(&hooks_text, "", &hooks.to_string_lossy(), command)?;
    let Some(idx) = idx else { bail!("the Stop hook is not installed yet ({status:?})") };
    let key = stop_hook_state_key(&hooks.to_string_lossy(), idx);
    let hash = stop_hook_trust_hash(command);
    let config = codex_home.join("config.toml");
    if config.exists() {
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
        let backup = codex_home.join(format!("config.toml.am-bak-{stamp}"));
        std::fs::copy(&config, &backup)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&backup, std::fs::Permissions::from_mode(0o600))?;
        }
    }
    crate::trust::update_file(&config, |existing| trust_edit(existing, &key, &hash))?;
    Ok(())
}

/// Remote status for one Codex home (reads both files over the host's cached connection).
pub async fn remote_stop_hook_status(conn: &HostConn, codex_home: &str, command: &str) -> Result<StopHookStatus> {
    let home = codex_home.trim_end_matches('/');
    let hooks = format!("{home}/hooks.json");
    let hooks_text = crate::trust::read_remote_text(conn, &hooks).await?;
    let config_text = crate::trust::read_remote_text(conn, &format!("{home}/config.toml")).await?;
    Ok(stop_hook_status_in(&hooks_text, &config_text, &hooks, command)?.0)
}

/// User-confirmed trust (remote): backup then merge one key.
pub async fn remote_trust_stop_hook(conn: &HostConn, codex_home: &str, command: &str) -> Result<()> {
    let home = codex_home.trim_end_matches('/');
    let hooks = format!("{home}/hooks.json");
    let hooks_text = crate::trust::read_remote_text(conn, &hooks).await?;
    let (status, idx) = stop_hook_status_in(&hooks_text, "", &hooks, command)?;
    let Some(idx) = idx else { bail!("the Stop hook is not installed yet ({status:?})") };
    let key = stop_hook_state_key(&hooks, idx);
    let hash = stop_hook_trust_hash(command);
    let config = format!("{home}/config.toml");
    let q = sh_quote(&config);
    conn.ssh_exec(&format!("[ -f {q} ] && cp -p {q} {q}.am-bak-$(date +%s) || true")).await?;
    crate::trust::update_remote_file(conn, &config, |existing| trust_edit(existing, &key, &hash)).await?;
    Ok(())
}

/// Install the dispatcher and append its Stop group without writing trust. Returns whether Codex will already run it (user-trusted).
pub fn install_local(codex_home: &Path, data_dir: &Path, instance: Option<&str>) -> Result<bool> {
    crate::private_files::create_private_dir(data_dir)?;
    crate::private_files::create_private_dir(codex_home)?;
    let dispatcher = data_dir.join(CODEX_HOOK_SH);
    write_local_dispatcher(&dispatcher, &local_dispatch_sh(data_dir, instance))?;
    let hooks = codex_home.join("hooks.json");
    let command = sh_quote(&dispatcher.to_string_lossy());
    crate::trust::update_file(&hooks, |existing| {
        Ok(merge_hooks_json(existing, &command)?.merged_json)
    })?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&hooks, std::fs::Permissions::from_mode(0o600))?;
    }
    let config_text = std::fs::read_to_string(codex_home.join("config.toml")).unwrap_or_default();
    let installed = std::fs::read_to_string(&hooks)?;
    Ok(stop_hook_status_in(&installed, &config_text, &hooks.to_string_lossy(), &command)?.0 == StopHookStatus::Trusted)
}

/// Remote installation only; never writes trust. Returns whether the user already trusts the handler (one extra read of the remote `config.toml`).
pub fn install_remote<'a>(
    conn: &'a HostConn,
    codex_home: &'a str,
    remote_root: &'a str,
    instance: Option<&'a str>,
) -> impl std::future::Future<Output = Result<bool>> + Send + 'a {
    Box::pin(async move {
        let root = remote_root.trim_end_matches('/');
        let home = conn.home().await?;
        let dispatcher = format!("{home}/{root}/{CODEX_HOOK_SH}");
        let script = remote_dispatch_sh(root, instance);
        let out = conn
            .ssh_exec(&write_remote_dispatcher_script(&dispatcher, &script))
            .await?;
        if !out.contains("AM_CODEX_DISPATCH_OK") {
            bail!("remote Codex dispatcher installation failed");
        }
        let hooks = format!("{}/hooks.json", codex_home.trim_end_matches('/'));
        let command = sh_quote(&dispatcher);
        crate::trust::update_remote_file(conn, &hooks, |existing| {
            Ok(merge_hooks_json(existing, &command)?.merged_json)
        })
        .await?;
        conn.ssh_exec(&format!("chmod 600 {}", sh_quote(&hooks)))
            .await?;
        let installed = crate::trust::read_remote_text(conn, &hooks).await?;
        let config_text = crate::trust::read_remote_text(conn, &format!("{}/config.toml", codex_home.trim_end_matches('/'))).await?;
        Ok(stop_hook_status_in(&installed, &config_text, &hooks, &command)?.0 == StopHookStatus::Trusted)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn test_merge_hooks_json_empty_and_append_nested_shape() {
        let dispatcher = sh_quote("/opt/agm/codex hook.sh");
        let outcome = merge_hooks_json("", &dispatcher).unwrap();
        assert_eq!(outcome.group_index, 0);
        let merged = outcome.merged_json.expect("new file should be written");
        let v: serde_json::Value = serde_json::from_str(&merged).unwrap();
        assert_eq!(v["hooks"]["Stop"][0]["hooks"][0]["command"], dispatcher);
        assert_eq!(v["hooks"]["Stop"][0]["hooks"][0]["timeout"], 5);

        // Second time should be idempotent and return merged_json: None
        let outcome2 = merge_hooks_json(&merged, &dispatcher).unwrap();
        assert_eq!(outcome2.group_index, 0);
        assert!(outcome2.merged_json.is_none());
    }

    #[test]
    fn test_merge_hooks_json_preserves_existing_keys_and_groups() {
        let existing = serde_json::json!({
            "description": "User custom config",
            "herdr": { "version": 1 },
            "hooks": {
                "SessionStart": [
                    {
                        "hooks": [
                            {
                                "type": "command",
                                "command": "bash '/home/ubuntu/.codex/herdr-agent-state.sh' session",
                                "timeout": 10
                            }
                        ]
                    }
                ],
                "Stop": [
                    {
                        "hooks": [
                            {
                                "type": "command",
                                "command": "/bin/other-stop.sh",
                                "timeout": 10
                            }
                        ]
                    }
                ]
            }
        }).to_string();

        let dispatcher = sh_quote("/opt/agm/codex-hook.sh");
        let outcome = merge_hooks_json(&existing, &dispatcher).unwrap();
        assert_eq!(outcome.group_index, 1); // Appended after existing Stop[0]
        let merged = outcome.merged_json.unwrap();
        let v: serde_json::Value = serde_json::from_str(&merged).unwrap();
        assert_eq!(v["description"], "User custom config");
        assert_eq!(v["herdr"]["version"], 1);
        assert_eq!(
            v["hooks"]["SessionStart"][0]["hooks"][0]["command"],
            "bash '/home/ubuntu/.codex/herdr-agent-state.sh' session"
        );
        assert_eq!(
            v["hooks"]["Stop"][0]["hooks"][0]["command"],
            "/bin/other-stop.sh"
        );
        assert_eq!(v["hooks"]["Stop"][1]["hooks"][0]["command"], dispatcher);
    }

    #[test]
    fn test_merge_hooks_json_non_canonical_or_ambiguous_fails() {
        let cmd = sh_quote("/opt/agm/codex-hook.sh");
        let non_canonical = serde_json::json!({
            "hooks": {
                "Stop": [
                    {
                        "hooks": [
                            {
                                "type": "command",
                                "command": cmd,
                                "timeout": 10 // Not canonical 5
                            }
                        ]
                    }
                ]
            }
        })
        .to_string();
        assert!(merge_hooks_json(&non_canonical, &cmd).is_err());

        let extra_fields = serde_json::json!({
            "hooks": {
                "Stop": [
                    {
                        "hooks": [
                            {
                                "type": "command",
                                "command": cmd,
                                "timeout": 5,
                                "async": true // extra field!
                            }
                        ]
                    }
                ]
            }
        })
        .to_string();
        assert!(merge_hooks_json(&extra_fields, &cmd).is_err());
    }

    #[test]
    fn test_merge_hooks_json_invalid_json_fails() {
        assert!(merge_hooks_json("NOT_JSON", "'/opt/agm/codex-hook.sh'").is_err());
    }

    #[test]
    fn test_local_dispatch_script_gates_and_exec() {
        let data_dir = Path::new("/var/run/agents manager");
        let script = local_dispatch_sh(data_dir, Some("inst-slug"));
        assert!(script.contains("[ -n \"$AM_BOT_ID\" ] && [ -n \"$AM_HOOK_TOKEN\" ] || exit 0"));
        assert!(script.contains("[ -z \"${AM_CHILD_OF:-}\" ] || exit 0"));
        assert!(script.contains("[ -n \"$AM_DAEMON_EXE\" ] && [ -n \"$AM_PORT\" ] || exit 0"));
        assert!(script.contains("[ \"${AM_INSTANCE:-}\" = 'inst-slug' ] || exit 0"));
        assert!(script.contains("exec \"$AM_DAEMON_EXE\" hook codex --bot \"$AM_BOT_ID\" --port \"$AM_PORT\" --data-dir '/var/run/agents manager'"));
        // Ensure no PAYLOAD= or head secondary cap or no-op cat
        assert!(!script.contains("PAYLOAD"));
        assert!(!script.contains("head -c"));
        assert!(!script.contains("cat >/dev/null"));
    }

    #[test]
    fn test_remote_dispatch_script_gates_and_exec() {
        let script = remote_dispatch_sh(".agents-manager/", None);
        assert!(script.contains("[ -n \"$AM_BOT_ID\" ] && [ -n \"$AM_HOOK_TOKEN\" ] || exit 0"));
        assert!(script.contains("[ -z \"${AM_CHILD_OF:-}\" ] || exit 0"));
        assert!(script.contains("[ -z \"${AM_INSTANCE:-}\" ] || exit 0"));
        assert!(script.contains("H=\"$HOME/.agents-manager/bots/$AM_BOT_ID/hook.sh\""));
        assert!(script.contains("exec \"$H\" codex \"$AM_BOT_ID\" -"));
        assert!(!script.contains("PAYLOAD"));
        assert!(!script.contains("head -c"));
        assert!(!script.contains("cat >/dev/null"));
    }

    #[test]
    fn test_local_dispatcher_execution_and_gates() {
        use std::io::Write as _;
        use std::process::{Command, Stdio};

        let dir = crate::test_dirs::scratch_dir("am-codex-disp-test");
        let disp = dir.join("codex-hook.sh");
        let fake_exe = dir.join("fake-daemon.sh");

        // Write fake daemon exe that dumps its argv and stdin
        std::fs::write(
            &fake_exe,
            "#!/bin/sh\nprintf '%s\\n' \"$*\" > \"$1.argv\"\ncat > \"$1.stdin\"\n",
        )
        .unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake_exe, std::fs::Permissions::from_mode(0o700)).unwrap();
        }

        let script = local_dispatch_sh(&dir, Some("main-inst"));
        write_local_dispatcher(&disp, &script).unwrap();

        // Case 1: Missing AM_BOT_ID -> exit 0 without executing fake daemon
        let out = Command::new("/bin/sh")
            .current_dir(&dir)
            .arg(&disp)
            .env_clear()
            .env("AM_HOOK_TOKEN", "token")
            .output()
            .unwrap();
        assert!(out.status.success());
        assert!(out.stdout.is_empty());
        assert!(!dir.join("hook.argv").exists());

        // Case 2: AM_CHILD_OF is set -> exit 0 without executing
        let out = Command::new("/bin/sh")
            .current_dir(&dir)
            .arg(&disp)
            .env_clear()
            .env("AM_BOT_ID", "bot-1")
            .env("AM_HOOK_TOKEN", "token")
            .env("AM_CHILD_OF", "parent-bot")
            .output()
            .unwrap();
        assert!(out.status.success());
        assert!(!dir.join("hook.argv").exists());

        // Case 3: Instance mismatch -> exit 0 without executing
        let out = Command::new("/bin/sh")
            .current_dir(&dir)
            .arg(&disp)
            .env_clear()
            .env("AM_BOT_ID", "bot-1")
            .env("AM_HOOK_TOKEN", "token")
            .env("AM_INSTANCE", "other-inst")
            .output()
            .unwrap();
        assert!(out.status.success());
        assert!(!dir.join("hook.argv").exists());

        // Case 4: Matches guards -> execs fake daemon with full stdin and argv
        let large_payload = "X".repeat(200 * 1024); // 200 KiB stdin (>128 KiB limit)
        let mut child = Command::new("/bin/sh")
            .current_dir(&dir)
            .arg(&disp)
            .env_clear()
            .env("AM_BOT_ID", "bot-1")
            .env("AM_HOOK_TOKEN", "token")
            .env("AM_INSTANCE", "main-inst")
            .env("AM_DAEMON_EXE", &fake_exe)
            .env("AM_PORT", "7788")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();

        child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(large_payload.as_bytes())
            .unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success());
        assert!(out.stdout.is_empty());

        let argv_content = std::fs::read_to_string(dir.join("hook.argv")).unwrap();
        assert!(argv_content.contains("hook codex --bot bot-1 --port 7788 --data-dir"));

        let stdin_content = std::fs::read_to_string(dir.join("hook.stdin")).unwrap();
        assert_eq!(stdin_content.len(), 200 * 1024);
    }

    #[test]
    fn installation_preserves_config_and_existing_groups_without_creating_trust() {
        let dir = crate::test_dirs::scratch_dir("am-codex-stop-install");
        let home = dir.join("codex");
        let data = dir.join("data");
        std::fs::create_dir_all(&home).unwrap();
        let config = "# user-owned\n[hooks.state.existing]\ntrusted_hash = 'fixture-only'\n";
        std::fs::write(home.join("config.toml"), config).unwrap();
        let initial = serde_json::json!({"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"herdr"}]}],"Stop":[{"hooks":[{"type":"command","command":"user-hook"}]}]}});
        std::fs::write(home.join("hooks.json"), initial.to_string()).unwrap();
        install_local(&home, &data, None).unwrap();
        let first = std::fs::read(home.join("hooks.json")).unwrap();
        install_local(&home, &data, None).unwrap();
        assert_eq!(std::fs::read(home.join("hooks.json")).unwrap(), first);
        assert_eq!(
            std::fs::read_to_string(home.join("config.toml")).unwrap(),
            config
        );
        let merged: serde_json::Value = serde_json::from_slice(&first).unwrap();
        assert_eq!(
            merged["hooks"]["SessionStart"],
            initial["hooks"]["SessionStart"]
        );
        assert_eq!(merged["hooks"]["Stop"][0], initial["hooks"]["Stop"][0]);
        assert_eq!(merged["hooks"]["Stop"].as_array().unwrap().len(), 2);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            for (path, mode) in [
                (home.clone(), 0o700),
                (data.clone(), 0o700),
                (home.join("hooks.json"), 0o600),
                (data.join(CODEX_HOOK_SH), 0o700),
            ] {
                assert_eq!(
                    std::fs::metadata(path).unwrap().permissions().mode() & 0o777,
                    mode
                );
            }
        }
    }
    #[test]
    fn native_shell_fixture_uses_the_production_builders() {
        println!(
            "C89_BASE_FIXTURE={}",
            serde_json::json!({
                "local_dispatch":local_dispatch_sh(Path::new("/__AM_DATA__"),None),
                "remote_dispatch":remote_dispatch_sh(".config/agents-manager",None),
                "remote_write":write_remote_dispatcher_script("/__AM_ROOT__/installed/codex-hook.sh","#!/bin/sh\nexit 0\n"),
            })
        );
    }

    /// Real Codex 0.169.0 `hooks/list` `currentHash` for `{"type":"command","command":"/tmp/claude-1000/stopexp.YmY3/collect.sh","timeout":5}` (#1989 live verification).
    #[test]
    fn stop_hook_trust_hash_matches_codex_0_169_current_hash() {
        assert_eq!(
            stop_hook_trust_hash("/tmp/claude-1000/stopexp.YmY3/collect.sh"),
            "sha256:1c7ccd958319119de4fa7b5568141442f18cff574b79306ad7d89f620dae251f"
        );
    }

    #[test]
    fn install_local_reports_trust_only_for_the_exact_user_recorded_hash() {
        let dir = crate::test_dirs::scratch_dir("am-codex-stop-trust");
        let home = dir.join("codex");
        let data = dir.join("data");
        assert!(!install_local(&home, &data, None).unwrap(), "fresh install is untrusted");
        let command = sh_quote(&data.join(CODEX_HOOK_SH).to_string_lossy());
        let key = format!("{}:stop:0:0", home.join("hooks.json").display());
        std::fs::write(home.join("config.toml"), format!("[hooks.state.{key:?}]\ntrusted_hash=\"sha256:stale\"\n")).unwrap();
        assert!(!install_local(&home, &data, None).unwrap(), "wrong hash is untrusted");
        let good = format!("[hooks.state.{key:?}]\ntrusted_hash={:?}\n", stop_hook_trust_hash(&command));
        std::fs::write(home.join("config.toml"), &good).unwrap();
        assert!(install_local(&home, &data, None).unwrap());
        assert_eq!(std::fs::read_to_string(home.join("config.toml")).unwrap(), good, "trust file is never written");
        std::fs::write(home.join("config.toml"), "not = [valid").unwrap();
        assert!(!install_local(&home, &data, None).unwrap(), "unparseable config is untrusted");
    }
}
