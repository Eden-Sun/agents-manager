//! Opt-in Codex Stop dispatcher installation; trust remains user-owned.
use crate::hosts::{sh_quote, HostConn};
use anyhow::{anyhow, bail, Context, Result};
use std::path::Path;
use std::sync::Arc;
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

/// Remote status for one Codex home (reads both files over the host's cached connection).
pub async fn remote_stop_hook_status(conn: &HostConn, codex_home: &str, command: &str) -> Result<StopHookStatus> {
    let home = codex_home.trim_end_matches('/');
    let hooks = format!("{home}/hooks.json");
    let hooks_text = crate::trust::read_remote_text(conn, &hooks).await?;
    let config_text = crate::trust::read_remote_text(conn, &format!("{home}/config.toml")).await?;
    Ok(stop_hook_status_in(&hooks_text, &config_text, &hooks, command)?.0)
}

/// AG Man 自己的 canonical Stop group 在 `hooks.json` 裡的位置；沒有＝`None`。
/// 使用者改過（形狀不符、歧義）：`Err`，呼叫端不動檔案。
fn canonical_group_present(existing: &str, command: &str) -> Result<Option<usize>> {
    let merged = merge_hooks_json(existing, command)?;
    Ok(if merged.merged_json.is_some() { None } else { Some(merged.group_index) })
}

/// 拿掉第 `idx` 組（那一組必須是 canonical）。在陣列尾端就整組刪；在中間就只清空它的 `hooks`，
/// 讓後面各組的 group index（信任鍵 `…:stop:<組>:0` 的一部分）不位移。
fn without_canonical_group(existing: &str, idx: usize) -> Result<String> {
    let mut doc: serde_json::Value = serde_json::from_str(existing).map_err(|_| anyhow!("invalid JSON in hooks.json"))?;
    let stop = doc
        .get_mut("hooks")
        .and_then(|h| h.get_mut("Stop"))
        .and_then(serde_json::Value::as_array_mut)
        .ok_or_else(|| anyhow!("hooks.json hooks.Stop must be an array"))?;
    if idx + 1 == stop.len() {
        stop.remove(idx);
    } else {
        stop[idx]["hooks"] = serde_json::json!([]);
    }
    Ok(serde_json::to_string_pretty(&doc)?)
}

/// 刪掉我們在 `[hooks.state]` 裡那一鍵（其他鍵與其餘內容不動）。沒有＝`None`。
pub fn trust_remove_edit(config_text: &str, key: &str) -> Result<Option<String>> {
    use toml_edit::{DocumentMut, Item};
    let mut doc: DocumentMut = config_text.parse().context("config.toml is not valid TOML")?;
    let removed = doc
        .get_mut("hooks")
        .and_then(Item::as_table_mut)
        .and_then(|h| h.get_mut("state"))
        .and_then(Item::as_table_mut)
        .and_then(|state| state.remove(key))
        .is_some();
    Ok(if removed { Some(doc.to_string()) } else { None })
}

/// 同一台主機、同一個 CODEX_HOME 的 hook 寫入（安裝並信任、移除）都在這把鎖底下做。
pub fn home_lock(host: &str, codex_home: &str) -> Arc<tokio::sync::Mutex<()>> {
    static LOCKS: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<String, Arc<tokio::sync::Mutex<()>>>>> = std::sync::OnceLock::new();
    let mut map = LOCKS.get_or_init(Default::default).lock().unwrap_or_else(|e| e.into_inner());
    map.entry(format!("{host}\n{codex_home}")).or_insert_with(|| Arc::new(tokio::sync::Mutex::new(()))).clone()
}

/// 寫入前的備份：`<檔名>.am-bak-<epoch 秒>`，權限 0600。
fn backup_local(path: &Path) -> Result<()> {
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let name = path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let backup = path.with_file_name(format!("{name}.am-bak-{stamp}"));
    std::fs::copy(path, &backup).with_context(|| format!("backing up {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&backup, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// 移除 AG Man 自己的 Stop handler（只認 canonical 那一組）。先拿掉 handler，再拿掉它的信任項（剩下的信任無害）。
/// 兩個檔案各自寫前備份；寫後讀回，我們的 handler 還在就報錯。回 `false` 表示沒有我們的項目可移。
pub fn remove_local(codex_home: &Path, command: &str) -> Result<bool> {
    let hooks = codex_home.join("hooks.json");
    let Ok(existing) = std::fs::read_to_string(&hooks) else { return Ok(false) };
    let Some(idx) = canonical_group_present(&existing, command)? else { return Ok(false) };
    let key = stop_hook_state_key(&hooks.to_string_lossy(), idx);
    backup_local(&hooks)?;
    crate::trust::update_file(&hooks, |cur| match canonical_group_present(cur, command)? {
        Some(i) => Ok(Some(without_canonical_group(cur, i)?)),
        None => Ok(None),
    })?;
    if canonical_group_present(&std::fs::read_to_string(&hooks)?, command)?.is_some() {
        bail!("Codex Stop handler is still registered after the removal write");
    }
    let config = codex_home.join("config.toml");
    if let Ok(text) = std::fs::read_to_string(&config) {
        if trust_remove_edit(&text, &key)?.is_some() {
            backup_local(&config)?;
            crate::trust::update_file(&config, |cur| trust_remove_edit(cur, &key))?;
        }
    }
    Ok(true)
}

/// 遠端版的 [`remove_local`]。
pub async fn remove_remote(conn: &HostConn, codex_home: &str, command: &str) -> Result<bool> {
    let home = codex_home.trim_end_matches('/');
    let hooks = format!("{home}/hooks.json");
    let existing = crate::trust::read_remote_text(conn, &hooks).await?;
    let Some(idx) = canonical_group_present(&existing, command)? else { return Ok(false) };
    let key = stop_hook_state_key(&hooks, idx);
    let backup = |path: &str| {
        let q = sh_quote(path);
        format!("set -e\ncp -p {q} {q}.am-bak-$(date +%s)\nprintf 'AM_BACKUP_OK\\n'\n")
    };
    if !conn.ssh_exec(&backup(&hooks)).await?.contains("AM_BACKUP_OK") {
        bail!("could not back up the remote hooks.json");
    }
    crate::trust::update_remote_file(conn, &hooks, |cur| match canonical_group_present(cur, command)? {
        Some(i) => Ok(Some(without_canonical_group(cur, i)?)),
        None => Ok(None),
    })
    .await?;
    if canonical_group_present(&crate::trust::read_remote_text(conn, &hooks).await?, command)?.is_some() {
        bail!("Codex Stop handler is still registered after the removal write");
    }
    let config = format!("{home}/config.toml");
    if trust_remove_edit(&crate::trust::read_remote_text(conn, &config).await?, &key)?.is_some() {
        if !conn.ssh_exec(&backup(&config)).await?.contains("AM_BACKUP_OK") {
            bail!("could not back up the remote config.toml");
        }
        crate::trust::update_remote_file(conn, &config, |cur| trust_remove_edit(cur, &key)).await?;
    }
    Ok(true)
}

/// 讓本機這個 CODEX_HOME 與設定一致（#1989）。`keep_trusted=true`（`stop_hook=true`）：已信任就保留並回 `true`；
/// 未信任／雜湊不符的殘留就移除；`keep_trusted=false`（`stop_hook=false`）：一律移除我們的項目。
pub fn reconcile_local(codex_home: &Path, command: &str, keep_trusted: bool) -> Result<bool> {
    match local_stop_hook_status(codex_home, command)? {
        StopHookStatus::NotInstalled => Ok(false),
        StopHookStatus::Trusted if keep_trusted => Ok(true),
        _ => {
            remove_local(codex_home, command)?;
            Ok(false)
        }
    }
}

/// 遠端版的 [`reconcile_local`]。
pub async fn reconcile_remote(conn: &HostConn, codex_home: &str, command: &str, keep_trusted: bool) -> Result<bool> {
    match remote_stop_hook_status(conn, codex_home, command).await? {
        StopHookStatus::NotInstalled => Ok(false),
        StopHookStatus::Trusted if keep_trusted => Ok(true),
        _ => {
            remove_remote(conn, codex_home, command).await?;
            Ok(false)
        }
    }
}

/// 寫入 dispatcher 腳本，回傳 Codex 會不會執行我們的 Stop handler（唯讀）。**不註冊 hook**：
/// 註冊只在使用者確認信任時（[`install_and_trust_local`]），所以這裡不會製造未信任的項目（#1989）。
pub fn install_local(codex_home: &Path, data_dir: &Path, instance: Option<&str>) -> Result<bool> {
    crate::private_files::create_private_dir(data_dir)?;
    let dispatcher = data_dir.join(CODEX_HOOK_SH);
    write_local_dispatcher(&dispatcher, &local_dispatch_sh(data_dir, instance))?;
    let command = sh_quote(&dispatcher.to_string_lossy());
    Ok(local_stop_hook_status(codex_home, &command)? == StopHookStatus::Trusted)
}

/// 使用者確認後：註冊 AG Man 自己的 Stop group，並信任**這一項**的精確 hash。
/// 順序是先寫信任、後寫 handler：任何時刻 codex 看到的 handler 都已經被信任，或根本還不存在；
/// 中途失敗最多留下一筆沒有對應 handler 的信任（無害），不會留下未信任的 handler。
pub fn install_and_trust_local(codex_home: &Path, data_dir: &Path, instance: Option<&str>) -> Result<()> {
    crate::private_files::create_private_dir(data_dir)?;
    crate::private_files::create_private_dir(codex_home)?;
    let dispatcher = data_dir.join(CODEX_HOOK_SH);
    write_local_dispatcher(&dispatcher, &local_dispatch_sh(data_dir, instance))?;
    let hooks = codex_home.join("hooks.json");
    let command = sh_quote(&dispatcher.to_string_lossy());
    let existing = std::fs::read_to_string(&hooks).unwrap_or_default();
    let merged = merge_hooks_json(&existing, &command)?;
    let key = stop_hook_state_key(&hooks.to_string_lossy(), merged.group_index);
    let hash = stop_hook_trust_hash(&command);
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
    crate::trust::update_file(&hooks, |existing| Ok(merge_hooks_json(existing, &command)?.merged_json))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&hooks, std::fs::Permissions::from_mode(0o600))?;
    }
    if local_stop_hook_status(codex_home, &command)? != StopHookStatus::Trusted {
        bail!("Codex Stop hook is not trusted after the write (check config.toml for a conflicting entry)");
    }
    Ok(())
}

/// 寫入 dispatcher 腳本，回傳遠端 Codex 會不會執行我們的 Stop handler（唯讀，不註冊）。見 [`install_local`]。
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
        let out = conn.ssh_exec(&write_remote_dispatcher_script(&dispatcher, &script)).await?;
        if !out.contains("AM_CODEX_DISPATCH_OK") {
            bail!("remote Codex dispatcher installation failed");
        }
        let command = sh_quote(&dispatcher);
        Ok(remote_stop_hook_status(conn, codex_home, &command).await? == StopHookStatus::Trusted)
    })
}

/// 遠端版的 [`install_and_trust_local`]：同樣先寫信任、後寫 handler；寫前備份 config.toml。
pub async fn install_and_trust_remote(conn: &HostConn, codex_home: &str, remote_root: &str, instance: Option<&str>) -> Result<()> {
    let root = remote_root.trim_end_matches('/');
    let home = conn.home().await?;
    let dispatcher = format!("{home}/{root}/{CODEX_HOOK_SH}");
    let script = remote_dispatch_sh(root, instance);
    let out = conn.ssh_exec(&write_remote_dispatcher_script(&dispatcher, &script)).await?;
    if !out.contains("AM_CODEX_DISPATCH_OK") {
        bail!("remote Codex dispatcher installation failed");
    }
    let home_dir = codex_home.trim_end_matches('/');
    let hooks = format!("{home_dir}/hooks.json");
    let config = format!("{home_dir}/config.toml");
    let command = sh_quote(&dispatcher);
    let existing = crate::trust::read_remote_text(conn, &hooks).await?;
    let merged = merge_hooks_json(&existing, &command)?;
    let key = stop_hook_state_key(&hooks, merged.group_index);
    let hash = stop_hook_trust_hash(&command);
    let q = sh_quote(&config);
    conn.ssh_exec(&format!("[ -f {q} ] && cp -p {q} {q}.am-bak-$(date +%s) || true")).await?;
    crate::trust::update_remote_file(conn, &config, |existing| trust_edit(existing, &key, &hash)).await?;
    crate::trust::update_remote_file(conn, &hooks, |existing| Ok(merge_hooks_json(existing, &command)?.merged_json)).await?;
    conn.ssh_exec(&format!("chmod 600 {}", sh_quote(&hooks))).await?;
    if remote_stop_hook_status(conn, codex_home, &command).await? != StopHookStatus::Trusted {
        bail!("Codex Stop hook is not trusted after the write (check config.toml for a conflicting entry)");
    }
    Ok(())
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
    fn install_never_registers_an_untrusted_stop_group() {
        let dir = crate::test_dirs::scratch_dir("am-codex-stop-install");
        let home = dir.join("codex");
        let data = dir.join("data");
        std::fs::create_dir_all(&home).unwrap();
        let config = "# user-owned\n[hooks.state.existing]\ntrusted_hash = 'fixture-only'\n";
        std::fs::write(home.join("config.toml"), config).unwrap();
        let initial = serde_json::json!({"hooks":{"Stop":[{"hooks":[{"type":"command","command":"user-hook"}]}]}});
        std::fs::write(home.join("hooks.json"), initial.to_string()).unwrap();
        assert!(!install_local(&home, &data, None).unwrap(), "install alone never reports trusted");
        assert_eq!(std::fs::read(home.join("hooks.json")).unwrap(), initial.to_string().into_bytes(), "no handler is registered before the user trusts it");
        assert_eq!(std::fs::read_to_string(home.join("config.toml")).unwrap(), config);
        assert!(data.join(CODEX_HOOK_SH).exists(), "the dispatcher script is still written");
    }

    #[test]
    fn install_and_trust_registers_only_our_group_and_trusts_exactly_that_handler() {
        let dir = crate::test_dirs::scratch_dir("am-codex-stop-trust-all");
        let home = dir.join("codex");
        let data = dir.join("data");
        std::fs::create_dir_all(&home).unwrap();
        let config = "# user-owned\n[projects.\"/x\"]\ntrust_level = \"trusted\"\n";
        std::fs::write(home.join("config.toml"), config).unwrap();
        let initial = serde_json::json!({"hooks":{"Stop":[{"hooks":[{"type":"command","command":"user-hook"}]}]}});
        std::fs::write(home.join("hooks.json"), initial.to_string()).unwrap();
        install_and_trust_local(&home, &data, None).unwrap();
        assert!(install_local(&home, &data, None).unwrap(), "the trusted handler reads as trusted");
        let merged: serde_json::Value = serde_json::from_slice(&std::fs::read(home.join("hooks.json")).unwrap()).unwrap();
        assert_eq!(merged["hooks"]["Stop"][0], initial["hooks"]["Stop"][0], "the user's own Stop group is untouched");
        assert_eq!(merged["hooks"]["Stop"].as_array().unwrap().len(), 2);
        let after = std::fs::read_to_string(home.join("config.toml")).unwrap();
        assert!(after.starts_with(config), "everything the user wrote stays byte-identical");
    }

    #[test]
    fn a_failed_install_and_trust_never_leaves_an_untrusted_handler_behind() {
        let dir = crate::test_dirs::scratch_dir("am-codex-stop-order");
        let home = dir.join("codex");
        let data = dir.join("data");
        std::fs::create_dir_all(home.join("hooks.json")).unwrap(); // hooks.json cannot be written: the handler write must fail
        assert!(install_and_trust_local(&home, &data, None).is_err());
        let command = sh_quote(&data.join(CODEX_HOOK_SH).to_string_lossy());
        let hooks = home.join("hooks.json");
        assert_eq!(
            local_stop_hook_status(&home, &command).unwrap(),
            StopHookStatus::NotInstalled,
            "no untrusted handler is left registered"
        );
        let _ = hooks;
    }

    #[test]
    fn install_local_reports_trust_only_for_the_exact_user_recorded_hash() {
        let dir = crate::test_dirs::scratch_dir("am-codex-stop-exact-hash");
        let home = dir.join("codex");
        let data = dir.join("data");
        let command = sh_quote(&data.join(CODEX_HOOK_SH).to_string_lossy());
        let key = format!("{}:stop:0:0", home.join("hooks.json").display());
        std::fs::create_dir_all(&home).unwrap();
        std::fs::write(home.join("config.toml"), format!("[hooks.state.{key:?}]\ntrusted_hash=\"sha256:stale\"\n")).unwrap();
        assert!(!install_local(&home, &data, None).unwrap(), "wrong hash is untrusted");
        std::fs::remove_file(home.join("config.toml")).unwrap();
        install_and_trust_local(&home, &data, None).unwrap();
        assert!(install_local(&home, &data, None).unwrap(), "the exact recorded hash is trusted");
        std::fs::write(home.join("config.toml"), "not = [valid").unwrap();
        assert!(!install_local(&home, &data, None).unwrap(), "unparseable config is untrusted");
    }

    fn ours_and_user_groups(command: &str) -> serde_json::Value {
        serde_json::json!({"hooks":{"Stop":[
            {"hooks":[{"type":"command","command":"user-first","timeout":5}]},
            {"hooks":[{"type":"command","command":command,"timeout":5}]},
            {"hooks":[{"type":"command","command":"user-last","timeout":5}]}
        ]}})
    }

    #[test]
    fn removing_our_middle_group_keeps_later_groups_and_their_trust_keys() {
        let dir = crate::test_dirs::scratch_dir("am-codex-stop-remove-mid");
        let home = dir.join("codex");
        std::fs::create_dir_all(&home).unwrap();
        let data = dir.join("data");
        let command = sh_quote(&data.join(CODEX_HOOK_SH).to_string_lossy());
        let hooks = home.join("hooks.json");
        std::fs::write(&hooks, ours_and_user_groups(&command).to_string()).unwrap();
        let config = format!(
            "[projects.\"/x\"]\ntrust_level = \"trusted\"\n[hooks.state.{:?}]\ntrusted_hash={:?}\n[hooks.state.{:?}]\ntrusted_hash=\"sha256:user-last\"\n",
            stop_hook_state_key(&hooks.to_string_lossy(), 1),
            stop_hook_trust_hash(&command),
            stop_hook_state_key(&hooks.to_string_lossy(), 2),
        );
        std::fs::write(home.join("config.toml"), &config).unwrap();
        assert!(!reconcile_local(&home, &command, false).unwrap(), "disabled: our trusted handler is removed");
        let after: serde_json::Value = serde_json::from_slice(&std::fs::read(&hooks).unwrap()).unwrap();
        let stop = after["hooks"]["Stop"].as_array().unwrap();
        assert_eq!(stop.len(), 3, "a middle group is emptied, not deleted, so later indices stay put");
        assert_eq!(stop[0]["hooks"][0]["command"], "user-first");
        assert!(stop[1]["hooks"].as_array().unwrap().is_empty());
        assert_eq!(stop[2]["hooks"][0]["command"], "user-last");
        let cfg = std::fs::read_to_string(home.join("config.toml")).unwrap();
        assert!(!cfg.contains(&stop_hook_state_key(&hooks.to_string_lossy(), 1)), "our trust entry is removed");
        assert!(cfg.contains("sha256:user-last"), "the later group's trust key is not moved or removed");
        assert!(cfg.starts_with("[projects.\"/x\"]\ntrust_level = \"trusted\"\n"), "the user's own table survives: {cfg:?}");
    }

    #[test]
    fn removing_our_last_group_deletes_it() {
        let dir = crate::test_dirs::scratch_dir("am-codex-stop-remove-last");
        let home = dir.join("codex");
        std::fs::create_dir_all(&home).unwrap();
        let data = dir.join("data");
        let command = sh_quote(&data.join(CODEX_HOOK_SH).to_string_lossy());
        let initial = serde_json::json!({"hooks":{"Stop":[{"hooks":[{"type":"command","command":"user-only","timeout":5}]},{"hooks":[{"type":"command","command":command,"timeout":5}]}]}});
        std::fs::write(home.join("hooks.json"), initial.to_string()).unwrap();
        reconcile_local(&home, &command, false).unwrap();
        let after: serde_json::Value = serde_json::from_slice(&std::fs::read(home.join("hooks.json")).unwrap()).unwrap();
        assert_eq!(after["hooks"]["Stop"].as_array().unwrap().len(), 1);
        assert_eq!(after["hooks"]["Stop"][0]["hooks"][0]["command"], "user-only");
    }

    #[test]
    fn an_untrusted_leftover_of_ours_is_removed_when_not_approved() {
        let dir = crate::test_dirs::scratch_dir("am-codex-stop-leftover");
        let home = dir.join("codex");
        std::fs::create_dir_all(&home).unwrap();
        let data = dir.join("data");
        let command = sh_quote(&data.join(CODEX_HOOK_SH).to_string_lossy());
        std::fs::write(home.join("hooks.json"), ours_and_user_groups(&command).to_string()).unwrap();
        assert!(!reconcile_local(&home, &command, true).unwrap());
        let after = std::fs::read_to_string(home.join("hooks.json")).unwrap();
        assert!(!after.contains(&command), "the incident's untrusted handler is gone");
        assert!(after.contains("user-first") && after.contains("user-last"));
    }

    #[test]
    fn a_trusted_handler_is_kept_when_enabled() {
        let dir = crate::test_dirs::scratch_dir("am-codex-stop-keep");
        let home = dir.join("codex");
        std::fs::create_dir_all(&home).unwrap();
        let data = dir.join("data");
        let command = sh_quote(&data.join(CODEX_HOOK_SH).to_string_lossy());
        install_and_trust_local(&home, &data, None).unwrap();
        let before = std::fs::read(home.join("hooks.json")).unwrap();
        assert!(reconcile_local(&home, &command, true).unwrap());
        assert_eq!(std::fs::read(home.join("hooks.json")).unwrap(), before);
    }

    #[test]
    fn a_non_canonical_group_with_our_command_is_never_touched() {
        let dir = crate::test_dirs::scratch_dir("am-codex-stop-noncanon");
        let home = dir.join("codex");
        std::fs::create_dir_all(&home).unwrap();
        let data = dir.join("data");
        let command = sh_quote(&data.join(CODEX_HOOK_SH).to_string_lossy());
        let edited = serde_json::json!({"hooks":{"Stop":[{"matcher":"x","hooks":[{"type":"command","command":command,"timeout":5}]}]}}).to_string();
        std::fs::write(home.join("hooks.json"), &edited).unwrap();
        assert!(reconcile_local(&home, &command, false).is_err(), "someone else changed our group: report, do not edit");
        assert_eq!(std::fs::read_to_string(home.join("hooks.json")).unwrap(), edited);
    }

}
