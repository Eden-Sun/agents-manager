//! grok 的全域 hook 檔（SPEC §12）：`<GROK_HOME>/hooks/agents-manager[-<實例>].json` 指向這顆 daemon 資料目錄裡的 dispatcher。
//!
//! 檔案是共用的（grok 合併 `hooks/*.json`、使用者也可能在裡面加自己的 hook），所以：
//! * **只換我們那一項**：只認本 dispatcher 的完整路徑；舊路徑需由 dispatcher 標頭證明歸屬。使用者自己的 hook 與 `hooks` 以外的鍵原樣保留。
//! * **原子寫入**：同目錄的暫存檔寫好再 `rename`，grok 的 hook loader 讀不到寫一半的檔。權限照原檔（但群組／其他人寫得進去的收回 0600：
//!   那等於誰都能改寫一個會被執行的指令）；新檔 0600。
//! * **自癒**：bot 啟動時裝／修；daemon 開機時也檢查一次——檔案在、而且我們那一項不是指到這顆
//!   daemon 的 dispatcher（別顆 daemon 或測試寫的、腳本被刪了）就換掉。沒有檔就不建：沒在用 grok 的機器不該被裝上 hook。

use std::path::{Path, PathBuf};

use serde_json::{json, Value};

use super::setup::{grok_hooks_file, grok_home, local_grok_dispatch_sh, GROK_DISPATCH_SH};
use crate::state::App;

const EVENTS: [&str; 2] = ["SessionStart", "Stop"];

fn is_ours(command: &str, dispatcher: &str, proven_legacy: &std::collections::HashSet<String>) -> bool {
    let command = command.trim();
    command == dispatcher || proven_legacy.contains(command)
}

fn our_hook(dispatcher: &str) -> Value {
    json!({"type": "command", "command": dispatcher, "timeout": 5})
}

/// 既有的 hook 檔內容（沒有或讀不懂就是 `None`）合併進我們這顆 daemon 的 dispatcher，回要寫的 JSON 文字。
pub fn hooks_json_merged(existing: Option<&str>, dispatcher: &str) -> String {
    hooks_json_merged_with_legacy(existing, dispatcher, &std::collections::HashSet::new())
}

fn hooks_json_merged_with_legacy(existing: Option<&str>, dispatcher: &str, proven_legacy: &std::collections::HashSet<String>) -> String {
    let mut root: Value = existing.and_then(|t| serde_json::from_str(t).ok()).filter(Value::is_object).unwrap_or_else(|| json!({}));
    if !root.get("hooks").is_some_and(Value::is_object) {
        root["hooks"] = json!({});
    }
    for event in EVENTS {
        let groups = root["hooks"][event].take();
        let mut groups: Vec<Value> = groups.as_array().cloned().unwrap_or_default();
        let mut have_exact = false;
        for g in groups.iter_mut() {
            let Some(hooks) = g.get_mut("hooks").and_then(Value::as_array_mut) else { continue };
            let old = std::mem::take(hooks);
            let mut kept = Vec::with_capacity(old.len());
            for mut hook in old {
                let Some(command) = hook.get("command").and_then(Value::as_str) else {
                    kept.push(hook);
                    continue;
                };
                if !is_ours(command, dispatcher, proven_legacy) {
                    kept.push(hook);
                    continue;
                }
                // 已證實屬於 daemon 的留第一份（順序不動才冪等），並修回完整 entry。
                if !have_exact {
                    hook = our_hook(dispatcher);
                    kept.push(hook);
                    have_exact = true;
                }
            }
            *hooks = kept;
        }
        // 只剩空殼的群組（我們拿掉之後沒東西了）整個丟掉；使用者的群組有別的 hook 就留著。
        groups.retain(|g| g.get("hooks").and_then(Value::as_array).is_none_or(|h| !h.is_empty()));
        if !have_exact {
            groups.push(json!({"hooks": [our_hook(dispatcher)]}));
        }
        root["hooks"][event] = Value::Array(groups);
    }
    serde_json::to_string_pretty(&root).unwrap_or_default()
}

/// 舊資料目錄的 entry 只有在其腳本仍帶著 daemon 標頭時才能證明 ownership；只看 basename 會刪掉使用者同名 hook。
fn proven_legacy_dispatchers(existing: Option<&str>, dispatcher: &str) -> std::collections::HashSet<String> {
    let Some(existing) = existing else { return Default::default() };
    let Ok(root) = serde_json::from_str::<Value>(existing) else { return Default::default() };
    let mut owned = std::collections::HashSet::new();
    let Some(events) = root.get("hooks").and_then(Value::as_object) else { return owned };
    for groups in events.values().filter_map(Value::as_array) {
        for group in groups {
            for hook in group.get("hooks").and_then(Value::as_array).into_iter().flatten() {
                let Some(command) = hook.get("command").and_then(Value::as_str).map(str::trim) else { continue };
                if command == dispatcher || Path::new(command).file_name().and_then(|n| n.to_str()) != Some(GROK_DISPATCH_SH) {
                    continue;
                }
                // 跟 hook 檔同一條：不跟著 symlink 讀，硬連結與過大檔也不算證明。
                let Ok(Some(text)) = read_hook_file(Path::new(command)) else { continue };
                if is_local_daemon_dispatcher(&text) {
                    owned.insert(command.to_string());
                }
            }
        }
    }
    owned
}

/// A basename or generic comment is user-controlled. Recognize the actual local dispatcher
/// shape before retiring an old path; remote wrappers and unrelated user scripts stay untouched.
fn is_local_daemon_dispatcher(text: &str) -> bool {
    let lines: Vec<&str> = text.lines().collect();
    lines.len() == 5
        && lines[0] == "#!/bin/sh"
        && lines[1]
            == "# agents-manager grok dispatcher (SPEC §12). Rewritten by the daemon on every grok bot start; no-op outside daemon panes."
        && lines[2] == "[ -n \"$AM_BOT_ID\" ] && [ -n \"$AM_HOOK_TOKEN\" ] || exit 0"
        && (lines[3] == "[ -z \"${AM_INSTANCE:-}\" ] || exit 0"
            || (lines[3].starts_with("[ \"${AM_INSTANCE:-}\" = ") && lines[3].ends_with(" ] || exit 0")))
        && lines[4].starts_with("exec ")
        && lines[4].contains(" hook grok --bot \"$AM_BOT_ID\" --port \"${AM_PORT:-7788}\" --data-dir ")
}

/// 同目錄暫存檔 → `rename`。`mode` 是新檔／換檔後的權限。
fn write_atomic(path: &Path, content: &str, mode: u32) -> anyhow::Result<()> {
    use std::io::Write as _;
    use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
    let dir = path.parent().ok_or_else(|| anyhow::anyhow!("{} has no parent", path.display()))?;
    crate::private_files::create_private_dir(dir)?;
    let tmp = dir.join(format!(".{}.tmp-{}", path.file_name().and_then(|n| n.to_str()).unwrap_or("hook"), crate::db::ulid()));
    let result = (|| -> std::io::Result<()> {
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(mode).open(&tmp)?;
        f.write_all(content.as_bytes())?;
        f.sync_all()?;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(mode))?;
        std::fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    Ok(result?)
}

/// 換檔後的權限：照原檔，但別人（群組／其他）寫得進去的一律收回 0600；新檔 0600。
fn mode_for(path: &Path) -> u32 {
    use std::os::unix::fs::PermissionsExt as _;
    match std::fs::metadata(path).map(|m| m.permissions().mode() & 0o7777) {
        Ok(m) if m & 0o022 == 0 => m & 0o777,
        _ => 0o600,
    }
}

fn dispatcher_text(app: &App) -> String {
    local_grok_dispatch_sh(&app.exe.to_string_lossy(), &app.data_dir.to_string_lossy(), app.instance().as_deref())
}

fn read_hook_file(path: &Path) -> anyhow::Result<Option<String>> {
    use std::io::Read as _;
    use std::os::unix::fs::MetadataExt as _;
    use std::os::unix::fs::OpenOptionsExt as _;
    let file = match std::fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW).open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.nlink() != 1 {
        anyhow::bail!("{} is not a private regular hook file", path.display());
    }
    let mut bytes = Vec::new();
    file.take(262_145).read_to_end(&mut bytes)?;
    if bytes.len() > 262_144 {
        anyhow::bail!("{} exceeds the hook file size limit", path.display());
    }
    Ok(Some(String::from_utf8(bytes)?))
}

/// dispatcher 腳本與 hook 檔都對了就不動。回「有沒有改到東西」。
fn ensure(app: &App, hooks_path: &Path) -> anyhow::Result<bool> {
    let dispatcher = app.data_dir.join(GROK_DISPATCH_SH);
    let want_script = dispatcher_text(app);
    use std::os::unix::fs::PermissionsExt as _;
    let mode_ok = std::fs::symlink_metadata(&dispatcher).is_ok_and(|m| {
        m.file_type().is_file() && (m.permissions().mode() & 0o7777) == 0o700
    });
    let script_ok = mode_ok && read_hook_file(&dispatcher).ok().flatten().as_deref() == Some(want_script.as_str());
    if !script_ok {
        write_atomic(&dispatcher, &want_script, 0o700)?;
    }
    let existing = read_hook_file(hooks_path)?;
    let dispatcher = dispatcher.to_string_lossy().into_owned();
    let legacy = proven_legacy_dispatchers(existing.as_deref(), &dispatcher);
    let merged = hooks_json_merged_with_legacy(existing.as_deref(), &dispatcher, &legacy);
    let same = existing.as_deref().and_then(|t| serde_json::from_str::<Value>(t).ok()) == serde_json::from_str::<Value>(&merged).ok();
    let mut changed = !script_ok;
    if !same {
        write_atomic(hooks_path, &merged, mode_for(hooks_path))?;
        changed = true;
    } else if let Ok(meta) = std::fs::metadata(hooks_path) {
        let want_mode = mode_for(hooks_path);
        if meta.permissions().mode() & 0o7777 != want_mode {
            std::fs::set_permissions(hooks_path, std::fs::Permissions::from_mode(want_mode))?;
            changed = true;
        }
    }
    Ok(changed)
}

fn hooks_path(app: &App, grok_home: &str) -> PathBuf {
    PathBuf::from(grok_home).join("hooks").join(grok_hooks_file(app.instance().as_deref()))
}

pub(super) fn selected_grok_home(env: &Value, home: Option<&Path>) -> Option<String> {
    let explicit = env.get("GROK_HOME").and_then(Value::as_str).map(str::trim).filter(|s| !s.is_empty());
    if explicit.is_some() {
        return Some(grok_home(env, ""));
    }
    home.map(|h| grok_home(&Value::Null, &h.to_string_lossy()))
}

/// grok bot 啟動（本機）：裝／修這顆 bot 的 `GROK_HOME`（身分可以指到別的目錄）底下的 hook 檔。
pub fn install_local(app: &App, env: &Value) -> anyhow::Result<bool> {
    let root = selected_grok_home(env, crate::home::dir().as_deref()).ok_or_else(|| anyhow::anyhow!("no home dir or GROK_HOME"))?;
    let path = hooks_path(app, &root);
    let changed = ensure(app, &path)?;
    if changed {
        tracing::info!(dispatcher = %app.data_dir.join(GROK_DISPATCH_SH).display(), hooks = %path.display(), "grok hook installed");
    }
    Ok(changed)
}

/// daemon 開機：預設位置（`grok_home` 沒給就看行程的 `GROK_HOME`，再退 `~/.grok`）的 hook 檔**存在**才檢查／修；不存在不建。
pub fn heal_at_startup(app: &App, grok_home_override: Option<&str>) -> anyhow::Result<bool> {
    let env = match grok_home_override.map(str::to_string).or_else(|| std::env::var("GROK_HOME").ok()) {
        Some(g) => json!({"GROK_HOME": g}),
        None => json!({}),
    };
    let Some(root) = selected_grok_home(&env, crate::home::dir().as_deref()) else { return Ok(false) };
    let path = hooks_path(app, &root);
    if !path.is_file() {
        return Ok(false);
    }
    ensure(app, &path)
}
