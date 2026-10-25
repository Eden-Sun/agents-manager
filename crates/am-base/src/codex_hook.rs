//! Codex hooks.json dispatcher installation and trust evaluation.
//!
//! Provides atomic, isolated installation of the `agents-manager` Stop hook dispatcher
//! into Codex's `hooks.json`, and read-only evaluation of trust state from
//! base config and active profile config.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use anyhow::{anyhow, bail, Context, Result};
use crate::hosts::{sh_quote, HostConn};

pub const CODEX_HOOK_SH: &str = "codex-hook.sh";

/// Safe profile / config parse diagnostic that prevents leaking any tokens,
/// hashes, or file contents across boundaries.
#[derive(Debug)]
pub struct CodexConfigError(String);

impl std::fmt::Display for CodexConfigError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for CodexConfigError {}

fn config_error(path: &str, detail: &str) -> CodexConfigError {
    CodexConfigError(format!("Codex config {path}：{detail}"))
}

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
        Some(slug) => format!("[ \"${{AM_INSTANCE:-}}\" = {} ] || exit 0\n", sh_quote(slug)),
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
        Some(slug) => format!("[ \"${{AM_INSTANCE:-}}\" = {} ] || exit 0\n", sh_quote(slug)),
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
    crate::atomic_file::write(path, content.as_bytes(), crate::atomic_file::Mode::Executable)
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

    let mut doc: serde_json::Value = serde_json::from_str(existing)
        .map_err(|_| anyhow!("invalid JSON in hooks.json"))?;

    let root_obj = doc.as_object_mut()
        .ok_or_else(|| anyhow!("hooks.json root must be a JSON object"))?;

    if !root_obj.contains_key("hooks") {
        root_obj.insert("hooks".to_string(), serde_json::json!({
            "Stop": [ expected_group ]
        }));
        return Ok(HooksMergeOutcome {
            merged_json: Some(serde_json::to_string_pretty(&doc)?),
            group_index: 0,
        });
    }

    let hooks_val = root_obj.get_mut("hooks").unwrap();
    let hooks_obj = hooks_val.as_object_mut()
        .ok_or_else(|| anyhow!("hooks.json 'hooks' property must be an object"))?;

    if !hooks_obj.contains_key("Stop") {
        hooks_obj.insert("Stop".to_string(), serde_json::json!([ expected_group ]));
        return Ok(HooksMergeOutcome {
            merged_json: Some(serde_json::to_string_pretty(&doc)?),
            group_index: 0,
        });
    }

    let stop_val = hooks_obj.get_mut("Stop").unwrap();
    let stop_arr = stop_val.as_array_mut()
        .ok_or_else(|| anyhow!("hooks.json hooks.Stop must be an array"))?;

    let mut matched_group: Option<usize> = None;
    for (idx, group) in stop_arr.iter().enumerate() {
        let has_command = group.get("hooks")
            .and_then(|h| h.as_array())
            .map(|arr| arr.iter().any(|h| h.get("command").and_then(|c| c.as_str()) == Some(canonical_command)))
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

/// Evaluates trust state for the given key by merging base config and active profile config.
///
/// Follows Codex config_rules semantics:
/// - Base config and Profile config entries for `[hooks.state."<key>"]` are merged field-by-field.
/// - Fields in entry: `trusted_hash` (Option<String>), `enabled` (Option<bool>).
/// - If a table has fields with wrong types (e.g. `trusted_hash` not a string, or `enabled` not a bool),
///   it must return Err (fail closed), not silently fall back.
/// - Profile overrides Base per-field:
///   - If profile has `enabled = Some(false)`, the result is disabled (`false`).
///   - If profile has `enabled = Some(true)` and base has non-empty `trusted_hash`, the base hash is retained!
///   - If profile has an explicit empty `trusted_hash = ""`, it does NOT fall back to base hash (overrides to empty).
///   - If profile does not specify `trusted_hash`, the base `trusted_hash` is retained.
/// - Unreadable / unparseable TOML fails closed (`Err`), never falls back.
/// - Error messages never leak source text, hashes, or tokens.
pub fn evaluate_trust_state(
    base_toml: Option<&str>,
    profile_toml: Option<&str>,
    trust_key: &str,
    base_path_display: &str,
    profile_path_display: &str,
) -> Result<bool> {
    let parse_entry = |content: &str, path_display: &str| -> Result<Option<(Option<String>, Option<bool>)>> {
        let doc = toml_edit::ImDocument::parse(content).map_err(|error| {
            let byte = error.span().map(|s| s.start).unwrap_or(0).min(content.len());
            let line = content.as_bytes()[..byte].iter().filter(|&&b| b == b'\n').count() + 1;
            config_error(path_display, &format!("第 {line} 行無法解析"))
        })?;

        let entry = doc.get("hooks")
            .and_then(|h| h.get("state"))
            .and_then(|s| s.get(trust_key));

        let Some(item) = entry else {
            return Ok(None);
        };

        let table = item.as_table_like()
            .ok_or_else(|| config_error(path_display, "hooks.state entry 必須是 table"))?;

        let trusted_hash = match table.get("trusted_hash") {
            Some(v) => match v.as_str() {
                Some(s) => Some(s.to_string()),
                None => return Err(config_error(path_display, "trusted_hash 型別錯誤（必須是字串）").into()),
            },
            None => None,
        };

        let enabled = match table.get("enabled") {
            Some(v) => match v.as_bool() {
                Some(b) => Some(b),
                None => return Err(config_error(path_display, "enabled 型別錯誤（必須是布林）").into()),
            },
            None => None,
        };

        Ok(Some((trusted_hash, enabled)))
    };

    let base_entry = match base_toml {
        Some(text) => parse_entry(text, base_path_display)?,
        None => None,
    };

    let profile_entry = match profile_toml {
        Some(text) => parse_entry(text, profile_path_display)?,
        None => None,
    };

    let mut merged_hash: Option<String> = None;
    let mut merged_enabled: Option<bool> = None;

    if let Some((b_hash, b_en)) = base_entry {
        merged_hash = b_hash;
        merged_enabled = b_en;
    }

    if let Some((p_hash, p_en)) = profile_entry {
        if p_hash.is_some() {
            merged_hash = p_hash;
        }
        if p_en.is_some() {
            merged_enabled = p_en;
        }
    }

    let is_trusted = match (merged_hash, merged_enabled) {
        (Some(hash), enabled) => !hash.trim().is_empty() && enabled != Some(false),
        (None, _) => false,
    };

    Ok(is_trusted)
}

/// Validates that profile name contains only safe alphanumeric characters, underscores, and hyphens.
pub fn validate_profile_name(profile: &str) -> Result<&str> {
    let clean = profile.trim();
    if clean.is_empty() {
        bail!("profile name cannot be empty");
    }
    if !clean.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        bail!("unsafe profile name: contains characters outside [-a-zA-Z0-9_]");
    }
    Ok(clean)
}

/// Installs or updates the Stop hook dispatcher into local Codex `hooks.json`
/// and evaluates whether it is currently trusted.
pub fn install_local(
    codex_home: &Path,
    data_dir: &Path,
    instance: Option<&str>,
    profile: Option<&str>,
) -> Result<bool> {
    crate::private_files::create_private_dir(data_dir)?;
    crate::private_files::create_private_dir(codex_home)?;

    let dispatcher_path = data_dir.join(CODEX_HOOK_SH);
    let dispatcher_script = local_dispatch_sh(data_dir, instance);
    write_local_dispatcher(&dispatcher_path, &dispatcher_script)?;

    let hooks_json_path = codex_home.join("hooks.json");
    let canonical_command = sh_quote(&dispatcher_path.to_string_lossy());

    let target_group_index = AtomicUsize::new(0);
    crate::trust::update_file(&hooks_json_path, |existing| {
        let outcome = merge_hooks_json(existing, &canonical_command)?;
        target_group_index.store(outcome.group_index, Ordering::SeqCst);
        Ok(outcome.merged_json)
    })?;

    let final_group_index = target_group_index.load(Ordering::SeqCst);
    let abs_hooks_json = match std::fs::canonicalize(&hooks_json_path) {
        Ok(p) => p,
        Err(_) => hooks_json_path.clone(),
    };
    let trust_key = format!("{}:stop:{}:0", abs_hooks_json.to_string_lossy(), final_group_index);

    let base_config_path = codex_home.join("config.toml");
    let base_toml = match std::fs::read_to_string(&base_config_path) {
        Ok(s) => Some(s),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(_) => return Err(config_error(&base_config_path.to_string_lossy(), "讀取基礎設定失敗").into()),
    };

    let profile_toml = if let Some(prof) = profile {
        let prof_clean = validate_profile_name(prof)?;
        let prof_path = codex_home.join(format!("{prof_clean}.config.toml"));
        match std::fs::read_to_string(&prof_path) {
            Ok(s) => Some(s),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
            Err(_) => return Err(config_error(&prof_path.to_string_lossy(), "讀取 profile 設定失敗").into()),
        }
    } else {
        None
    };

    let base_disp = base_config_path.to_string_lossy();
    let prof_disp = profile.map(|p| format!("{p}.config.toml")).unwrap_or_default();
    evaluate_trust_state(
        base_toml.as_deref(),
        profile_toml.as_deref(),
        &trust_key,
        &base_disp,
        &prof_disp,
    )
}

/// Installs or updates the Stop hook dispatcher into remote Codex `hooks.json`
/// and evaluates whether it is currently trusted.
///
/// Implements `Box::pin` on the inner async body to keep stack frame small.
pub fn install_remote<'a>(
    conn: &'a HostConn,
    codex_home: &'a str,
    remote_root: &'a str,
    instance: Option<&'a str>,
    profile: Option<&'a str>,
) -> impl std::future::Future<Output = Result<bool>> + Send + 'a {
    Box::pin(install_remote_body(conn, codex_home, remote_root, instance, profile))
}

async fn install_remote_body(
    conn: &HostConn,
    codex_home: &str,
    remote_root: &str,
    instance: Option<&str>,
    profile: Option<&str>,
) -> Result<bool> {
    let clean_home = codex_home.trim_end_matches('/');
    let clean_root = remote_root.trim_end_matches('/');
    let home = conn.home().await?;
    let dispatcher_path = format!("{home}/{clean_root}/{CODEX_HOOK_SH}");
    let dispatcher_script = remote_dispatch_sh(clean_root, instance);

    let out = conn.ssh_exec(&write_remote_dispatcher_script(&dispatcher_path, &dispatcher_script)).await?;
    if !out.contains("AM_CODEX_DISPATCH_OK") {
        bail!("遠端安裝 codex dispatcher 腳本失敗");
    }

    let hooks_json_path = format!("{clean_home}/hooks.json");
    let canonical_command = sh_quote(&dispatcher_path);

    let target_group_index = AtomicUsize::new(0);
    crate::trust::update_remote_file(conn, &hooks_json_path, |existing| {
        let outcome = merge_hooks_json(existing, &canonical_command)?;
        target_group_index.store(outcome.group_index, Ordering::SeqCst);
        Ok(outcome.merged_json)
    }).await?;

    let final_group_index = target_group_index.load(Ordering::SeqCst);
    let trust_key = format!("{hooks_json_path}:stop:{final_group_index}:0");

    let base_path = format!("{clean_home}/config.toml");
    let base_toml = read_remote_optional_file(conn, &base_path).await?;

    let prof_toml = if let Some(prof) = profile {
        let prof_clean = validate_profile_name(prof)?;
        let prof_path = format!("{clean_home}/{prof_clean}.config.toml");
        read_remote_optional_file(conn, &prof_path).await?
    } else {
        None
    };

    evaluate_trust_state(
        base_toml.as_deref(),
        prof_toml.as_deref(),
        &trust_key,
        &base_path,
        profile.unwrap_or(""),
    )
}

/// Reads a remote file if present, returning `Ok(None)` ONLY on explicit missing (file does not exist).
/// Any transmission failure, permission issue, directory error, or base64 decoding failure returns Err
/// with a safe diagnostic that never leaks remote contents or tokens.
async fn read_remote_optional_file(conn: &HostConn, remote_path: &str) -> Result<Option<String>> {
    let f = sh_quote(remote_path);
    let script = format!(
        "if [ ! -e {f} ] && [ ! -L {f} ]; then\n\
           printf 'AM_FILE_MISSING\\n'\n\
         elif [ -f {f} ]; then\n\
           printf 'AM_FILE_B64_START\\n'\n\
           base64 < {f} 2>/dev/null || exit 1\n\
           printf 'AM_FILE_B64_END\\n'\n\
         else\n\
           exit 2\n\
         fi\n"
    );
    let out = conn.ssh_exec(&script).await.map_err(|_| anyhow!("遠端讀取檔案連線或執行失敗"))?;
    let trimmed = out.trim();
    if trimmed == "AM_FILE_MISSING" {
        return Ok(None);
    }
    let Some(start_pos) = out.find("AM_FILE_B64_START\n") else {
        bail!("遠端檔案讀取框架無效或無法存取");
    };
    let payload_start = start_pos + "AM_FILE_B64_START\n".len();
    let Some(end_pos) = out[payload_start..].find("AM_FILE_B64_END\n") else {
        bail!("遠端檔案讀取框架未正常結束或傳輸截斷");
    };
    let b64_str = out[payload_start..payload_start + end_pos].replace(['\n', '\r', ' '], "");
    use base64::Engine as _;
    let bytes = base64::engine::general_purpose::STANDARD.decode(b64_str)
        .map_err(|_| anyhow!("遠端檔案內容 base64 解碼失敗"))?;
    let text = String::from_utf8(bytes)
        .map_err(|_| anyhow!("遠端檔案內容不是有效 UTF-8"))?;
    Ok(Some(text))
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
        assert_eq!(v["hooks"]["SessionStart"][0]["hooks"][0]["command"], "bash '/home/ubuntu/.codex/herdr-agent-state.sh' session");
        assert_eq!(v["hooks"]["Stop"][0]["hooks"][0]["command"], "/bin/other-stop.sh");
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
        }).to_string();
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
        }).to_string();
        assert!(merge_hooks_json(&extra_fields, &cmd).is_err());
    }

    #[test]
    fn test_merge_hooks_json_invalid_json_fails() {
        assert!(merge_hooks_json("NOT_JSON", "'/opt/agm/codex-hook.sh'").is_err());
    }

    #[test]
    fn test_evaluate_trust_state_field_merge_and_precedence() {
        let key = "/home/user/.codex/hooks.json:stop:0:0";

        // 1. Base has trusted_hash and enabled=true -> true
        let base_ok = format!("[hooks.state.\"{key}\"]\ntrusted_hash = \"hash123\"\nenabled = true\n");
        assert!(evaluate_trust_state(Some(&base_ok), None, key, "config.toml", "").unwrap());

        // 2. Profile overrides enabled=false -> false
        let prof_disabled = format!("[hooks.state.\"{key}\"]\nenabled = false\n");
        assert!(!evaluate_trust_state(Some(&base_ok), Some(&prof_disabled), key, "config.toml", "profile.config.toml").unwrap());

        // 3. Base has trusted_hash (enabled omitted), Profile has enabled=true -> retains base hash -> true!
        let base_only_hash = format!("[hooks.state.\"{key}\"]\ntrusted_hash = \"hash123\"\n");
        let prof_enabled_only = format!("[hooks.state.\"{key}\"]\nenabled = true\n");
        assert!(evaluate_trust_state(Some(&base_only_hash), Some(&prof_enabled_only), key, "config.toml", "profile.config.toml").unwrap());

        // 4. Profile explicitly sets trusted_hash = "" -> overrides base hash to empty -> false
        let prof_empty_hash = format!("[hooks.state.\"{key}\"]\ntrusted_hash = \"\"\nenabled = true\n");
        assert!(!evaluate_trust_state(Some(&base_ok), Some(&prof_empty_hash), key, "config.toml", "profile.config.toml").unwrap());

        // 5. Wrong type in profile (e.g. enabled is a string) -> Err, not silent fallback
        let prof_wrong_type = format!("[hooks.state.\"{key}\"]\nenabled = \"true\"\n");
        assert!(evaluate_trust_state(Some(&base_ok), Some(&prof_wrong_type), key, "config.toml", "profile.config.toml").is_err());

        // 6. Wrong type in hash (e.g. integer) -> Err
        let prof_wrong_hash = format!("[hooks.state.\"{key}\"]\ntrusted_hash = 12345\n");
        assert!(evaluate_trust_state(Some(&base_ok), Some(&prof_wrong_hash), key, "config.toml", "profile.config.toml").is_err());
    }

    #[test]
    fn test_evaluate_trust_state_corrupt_toml_never_leaks_sentinel_values() {
        let sentinel_token = "SECRET_SENTINEL_TOKEN_12345";
        let sentinel_hash = "SECRET_TRUSTED_HASH_98765";
        let corrupt_toml = format!(
            "invalid_table_header [\nsecret = \"{sentinel_token}\"\n[hooks.state.\"key\"]\ntrusted_hash = \"{sentinel_hash}\"\n"
        );

        let err = evaluate_trust_state(Some(&corrupt_toml), None, "key", "config.toml", "").unwrap_err();
        let err_msg = err.to_string();
        assert!(!err_msg.contains(sentinel_token), "error message leaked secret token");
        assert!(!err_msg.contains(sentinel_hash), "error message leaked trusted hash");
        assert!(err_msg.contains("無法解析"));
    }

    #[test]
    fn test_validate_profile_name() {
        assert!(validate_profile_name("am-bot-123_abc").is_ok());
        assert!(validate_profile_name("").is_err());
        assert!(validate_profile_name("bot/with/slash").is_err());
        assert!(validate_profile_name("../traversal").is_err());
        assert!(validate_profile_name("has spaces").is_err());
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
        use std::process::{Command, Stdio};
        use std::io::Write as _;

        let dir = crate::test_dirs::scratch_dir("am-codex-disp-test");
        let disp = dir.join("codex-hook.sh");
        let fake_exe = dir.join("fake-daemon.sh");

        // Write fake daemon exe that dumps its argv and stdin
        std::fs::write(&fake_exe, "#!/bin/sh\nprintf '%s\\n' \"$*\" > \"$1.argv\"\ncat > \"$1.stdin\"\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&fake_exe, std::fs::Permissions::from_mode(0o700)).unwrap();
        }

        let script = local_dispatch_sh(&dir, Some("main-inst"));
        write_local_dispatcher(&disp, &script).unwrap();

        // Case 1: Missing AM_BOT_ID -> exit 0 without executing fake daemon
        let out = Command::new("/bin/sh")
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

        child.stdin.as_mut().unwrap().write_all(large_payload.as_bytes()).unwrap();
        let out = child.wait_with_output().unwrap();
        assert!(out.status.success());
        assert!(out.stdout.is_empty());

        let argv_content = std::fs::read_to_string(dir.join("hook.argv")).unwrap();
        assert!(argv_content.contains("hook codex --bot bot-1 --port 7788 --data-dir"));

        let stdin_content = std::fs::read_to_string(dir.join("hook.stdin")).unwrap();
        assert_eq!(stdin_content.len(), 200 * 1024);
    }

    #[test]
    fn test_install_local_end_to_end_in_scratch_dir() {
        let dir = crate::test_dirs::scratch_dir("am-codex-hook-local");
        let codex_home = dir.join(".codex");
        let data_dir = dir.join("data");

        // Initially untrusted because no config.toml exists
        let trusted1 = install_local(&codex_home, &data_dir, None, None).unwrap();
        assert!(!trusted1);

        // Verify dispatcher was written with 0700 permissions
        let disp = data_dir.join(CODEX_HOOK_SH);
        assert!(disp.exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::metadata(&disp).unwrap().permissions();
            assert_eq!(perms.mode() & 0777, 0700);

            // Verify directories are 0700
            assert_eq!(std::fs::metadata(&data_dir).unwrap().permissions().mode() & 0777, 0700);
            assert_eq!(std::fs::metadata(&codex_home).unwrap().permissions().mode() & 0777, 0700);
        }

        // Add trust entry in config.toml
        let abs_hooks = std::fs::canonicalize(codex_home.join("hooks.json")).unwrap();
        let trust_key = format!("{}:stop:0:0", abs_hooks.to_string_lossy());
        let config_toml = format!(
            "[hooks.state.\"{trust_key}\"]\ntrusted_hash = \"abc123hash\"\nenabled = true\n"
        );
        std::fs::write(codex_home.join("config.toml"), config_toml).unwrap();

        let trusted2 = install_local(&codex_home, &data_dir, None, None).unwrap();
        assert!(trusted2);
    }
}
