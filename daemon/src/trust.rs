//! Pre-trusting a workspace directory so an unattended claude/codex pane does not stall on the
//! first-run "do you trust this folder?" dialog (claude's cursor starts on `No, exit`, so it quits
//! → `agent_not_running`). `--dangerously-skip-permissions` does not suppress it (verified).
//!
//! We write the record the dialog would: claude `hasTrustDialogAccepted` in `.claude.json`, codex
//! `trust_level = "trusted"` in `config.toml`, grok `[folders."<path>"] trusted = true` in
//! `trusted_folders.toml` (grok had no gate until 1.0.34 added "Do you trust the contents of this
//! directory?", 2026-09-17). Gotchas: the file must be the one the *bot's identity* reads
//! (`CLAUDE_CONFIG_DIR` / `CODEX_HOME`), and the path must be physical (`/tmp` → `/private/tmp`),
//! see [`canonical`]. These files belong to the CLIs: amend in place via temp file + `rename`,
//! and leave untouched when already trusted.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::config::expand_home;
use crate::db;
use crate::state::App;

const CLAUDE_KEY: &str = "hasTrustDialogAccepted";
/// Second first-run dialog (2026-09-08): external CLAUDE.md `@imports`, cursor on *No*. Same fix.
const CLAUDE_EXTERNAL_KEYS: [&str; 2] = ["hasClaudeMdExternalIncludesApproved", "hasClaudeMdExternalIncludesWarningShown"];
const CODEX_KEY: &str = "trust_level";
const CODEX_TRUSTED: &str = "trusted";

/// Falls back to the input when the directory does not exist (a bad key is inert).
pub fn canonical(path: &str) -> String {
    match std::fs::canonicalize(path) {
        Ok(p) => p.to_string_lossy().into_owned(),
        Err(_) => path.to_string(),
    }
}

/// `env` = identity ∪ bot env, `$HOME`-expanded. `None` for kinds without a gate.
pub fn store_path(kind: &str, env: &BTreeMap<String, String>, home: &str) -> Option<PathBuf> {
    let var = |k: &str| {
        env.get(k).map(|s| s.trim()).filter(|s| !s.is_empty()).map(|s| expand_home(s, home))
    };
    match kind {
        // Unset → `~/.claude.json`, *not* `~/.claude/.claude.json` (unlike settings.json).
        "claude" => Some(match var("CLAUDE_CONFIG_DIR") {
            Some(dir) => PathBuf::from(dir).join(".claude.json"),
            None => PathBuf::from(home).join(".claude.json"),
        }),
        "codex" => {
            let dir = var("CODEX_HOME").unwrap_or_else(|| format!("{home}/.codex"));
            Some(PathBuf::from(dir).join("config.toml"))
        }
        "grok" => {
            let dir = var("GROK_HOME").unwrap_or_else(|| format!("{home}/.grok"));
            Some(PathBuf::from(dir).join("trusted_folders.toml"))
        }
        _ => None,
    }
}

/// `None` when nothing changed. Unparseable or wrong-shaped files are an error, never overwritten.
pub fn claude_merge(existing: &str, paths: &[String]) -> Result<Option<String>> {
    let mut root: Value = if existing.trim().is_empty() {
        json!({})
    } else {
        serde_json::from_str(existing).context("`.claude.json` is not valid JSON")?
    };
    let Some(obj) = root.as_object_mut() else { bail!("`.claude.json` is not a JSON object") };

    let projects = obj.entry("projects").or_insert_with(|| json!({}));
    let Some(projects) = projects.as_object_mut() else { bail!("`projects` in `.claude.json` is not an object") };

    let mut changed = false;
    for p in paths {
        let entry = projects.entry(p.as_str()).or_insert_with(|| json!({}));
        let Some(entry) = entry.as_object_mut() else {
            bail!("`projects[{p}]` in `.claude.json` is not an object");
        };
        for key in std::iter::once(CLAUDE_KEY).chain(CLAUDE_EXTERNAL_KEYS) {
            if entry.get(key) != Some(&Value::Bool(true)) {
                entry.insert(key.into(), json!(true));
                changed = true;
            }
        }
    }
    if !changed {
        return Ok(None);
    }
    // Match Claude Code's own formatting so the file does not flip between writers.
    Ok(Some(format!("{}\n", serde_json::to_string_pretty(&root)?)))
}

/// `toml_edit` keeps the rest of the file byte-identical. `None` when already trusted.
pub fn codex_merge(existing: &str, paths: &[String]) -> Result<Option<String>> {
    let mut doc: toml_edit::DocumentMut = if existing.trim().is_empty() {
        toml_edit::DocumentMut::new()
    } else {
        existing.parse().context("codex `config.toml` is not valid TOML")?
    };

    let projects = doc.entry("projects").or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()));
    let Some(projects) = projects.as_table_mut() else { bail!("`projects` in codex `config.toml` is not a table") };
    // Renders the children as `[projects."/path"]` instead of emitting a bare `[projects]`.
    projects.set_implicit(true);

    let mut changed = false;
    for p in paths {
        let entry = projects.entry(p).or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()));
        let Some(entry) = entry.as_table_mut() else {
            bail!("`projects.{p}` in codex `config.toml` is not a table");
        };
        if entry.get(CODEX_KEY).and_then(|v| v.as_str()) != Some(CODEX_TRUSTED) {
            entry[CODEX_KEY] = toml_edit::value(CODEX_TRUSTED);
            changed = true;
        }
    }
    if !changed {
        return Ok(None);
    }
    Ok(Some(doc.to_string()))
}

/// grok 1.0.34 `trusted_folders.toml`：`[folders."/path"] trusted = true, decided_at = <epoch 秒>`，
/// 跟 grok 自己按 `y` 寫的一樣。`None` when already trusted.
pub fn grok_merge(existing: &str, paths: &[String], now_secs: i64) -> Result<Option<String>> {
    let mut doc: toml_edit::DocumentMut = if existing.trim().is_empty() {
        toml_edit::DocumentMut::new()
    } else {
        existing.parse().context("grok `trusted_folders.toml` is not valid TOML")?
    };
    let folders = doc.entry("folders").or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()));
    let Some(folders) = folders.as_table_mut() else { bail!("`folders` in grok `trusted_folders.toml` is not a table") };
    folders.set_implicit(true);

    let mut changed = false;
    for p in paths {
        let entry = folders.entry(p).or_insert_with(|| toml_edit::Item::Table(toml_edit::Table::new()));
        let Some(entry) = entry.as_table_mut() else {
            bail!("`folders.{p}` in grok `trusted_folders.toml` is not a table");
        };
        if entry.get("trusted").and_then(|v| v.as_bool()) != Some(true) {
            entry["trusted"] = toml_edit::value(true);
            entry["decided_at"] = toml_edit::value(now_secs);
            changed = true;
        }
    }
    if !changed {
        return Ok(None);
    }
    Ok(Some(doc.to_string()))
}

/// Temp file + fsync + `rename`: a crash must not corrupt the user's own agent-CLI state files.
fn write_atomic(path: &Path, text: &str) -> Result<()> {
    use std::io::Write;

    let dir = path.parent().ok_or_else(|| anyhow!("{} has no parent directory", path.display()))?;
    let name = path.file_name().and_then(|s| s.to_str()).unwrap_or("trust");
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".{name}.am-trust.{}.tmp", std::process::id()));

    let res = (|| -> Result<()> {
        let mut f = std::fs::File::create(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.sync_all()?;
        drop(f);
        // `.claude.json` is 0600; a rename would otherwise hand it our umask instead.
        if let Ok(md) = std::fs::metadata(path) {
            let _ = std::fs::set_permissions(&tmp, md.permissions());
        }
        std::fs::rename(&tmp, path)?;
        Ok(())
    })();
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res.with_context(|| format!("writing {}", path.display()))
}

/// No-op (file not rewritten) when already trusted.
pub fn mark_trusted(kind: &str, store: &Path, paths: &[String]) -> Result<bool> {
    let existing = match std::fs::read_to_string(store) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", store.display())),
    };
    let next = match kind {
        "claude" => claude_merge(&existing, paths)?,
        "codex" => codex_merge(&existing, paths)?,
        "grok" => {
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
            grok_merge(&existing, paths, now)?
        }
        _ => None,
    };
    let Some(next) = next else { return Ok(false) };
    write_atomic(store, &next)?;
    tracing::info!(kind, store = %store.display(), ?paths, "pre-trusted agent workspace directories");
    Ok(true)
}

/// Identity env then bot env (`lifecycle::pane_env` minus daemon vars, which name no config dir).
async fn config_env(app: &Arc<App>, bot: &db::Bot, home: &str) -> BTreeMap<String, String> {
    let mut env: BTreeMap<String, String> = BTreeMap::new();
    // `identity_for_host`, not `cfg.identities`: shell-discovered `ccN` (SPEC §16) aren't in
    // config.toml, which once sent cc2's record to the wrong file (2026-09-08).
    if let Some(name) = bot.identity.as_deref().filter(|s| !s.is_empty()) {
        if let Some(id) = crate::tools::identity_for_host(app, crate::config::LOCAL_HOST, name).await {
            for (k, v) in &id.env {
                env.insert(k.clone(), expand_home(v, home));
            }
        }
    }
    for (k, v) in bot.env() {
        env.insert(k, expand_home(&v, home));
    }
    env
}

/// Best effort: returns one error per store rather than refusing the start.
/// **Local host only** — remote bots would need this written over ssh.
pub async fn pretrust_bots(app: &Arc<App>, bots: &[db::Bot]) -> Vec<String> {
    let Some(home) = dirs::home_dir() else {
        return vec!["no home directory; cannot pre-trust the working directory".into()];
    };
    let home = home.to_string_lossy().into_owned();

    // One write per file, not per bot: the CLIs write these files too, so minimise the race.
    let mut jobs: BTreeMap<PathBuf, (String, BTreeSet<String>)> = BTreeMap::new();
    for b in bots {
        let Some(cwd) = b.cwd.as_deref().map(str::trim).filter(|s| !s.is_empty()) else { continue };
        let env = config_env(app, b, &home).await;
        let Some(store) = store_path(&b.kind, &env, &home) else { continue };
        jobs.entry(store).or_insert_with(|| (b.kind.clone(), BTreeSet::new())).1.insert(canonical(cwd));
    }

    let mut errors = Vec::new();
    for (store, (kind, paths)) in jobs {
        let paths: Vec<String> = paths.into_iter().collect();
        if let Err(e) = mark_trusted(&kind, &store, &paths) {
            errors.push(format!("{}: {e:#}", store.display()));
        }
    }
    errors
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(pairs: &[(&str, &str)]) -> BTreeMap<String, String> {
        pairs.iter().map(|(k, v)| (k.to_string(), v.to_string())).collect()
    }

    #[test]
    fn claude_store_follows_the_identity_config_dir() {
        assert_eq!(
            store_path("claude", &env(&[]), "/home/u").unwrap(),
            PathBuf::from("/home/u/.claude.json")
        );
        assert_eq!(
            store_path("claude", &env(&[("CLAUDE_CONFIG_DIR", "$HOME/.claude-ccompany")]), "/home/u").unwrap(),
            PathBuf::from("/home/u/.claude-ccompany/.claude.json")
        );
        assert_eq!(
            store_path("claude", &env(&[("CLAUDE_CONFIG_DIR", "   ")]), "/home/u").unwrap(),
            PathBuf::from("/home/u/.claude.json")
        );
    }

    #[test]
    fn codex_store_follows_codex_home_and_other_kinds_have_no_gate() {
        assert_eq!(
            store_path("codex", &env(&[]), "/home/u").unwrap(),
            PathBuf::from("/home/u/.codex/config.toml")
        );
        assert_eq!(
            store_path("codex", &env(&[("CODEX_HOME", "~/alt")]), "/home/u").unwrap(),
            PathBuf::from("/home/u/alt/config.toml")
        );
        assert!(store_path("shell", &env(&[]), "/home/u").is_none());
    }

    #[test]
    fn grok_merge_writes_what_grok_writes_and_leaves_a_trusted_folder_alone() {
        let existing = "[folders.\"/Users/m4p/project/agents-manager\"]\ntrusted = true\ndecided_at = 1789389148\n";
        let out = grok_merge(existing, &["/tmp/rt".into()], 42).unwrap().expect("new folder is written");
        let doc: toml_edit::DocumentMut = out.parse().unwrap();
        assert_eq!(doc["folders"]["/tmp/rt"]["trusted"].as_bool(), Some(true));
        assert_eq!(doc["folders"]["/tmp/rt"]["decided_at"].as_integer(), Some(42));
        assert_eq!(doc["folders"]["/Users/m4p/project/agents-manager"]["decided_at"].as_integer(), Some(1789389148));
        assert!(!out.contains("[folders]\n"), "no bare [folders] header: {out}");
        assert!(grok_merge(&out, &["/tmp/rt".into()], 99).unwrap().is_none(), "already trusted");
        assert!(grok_merge("folders = 3", &["/tmp/rt".into()], 1).is_err());
    }

    #[test]
    fn grok_store_follows_grok_home() {
        let mut env = BTreeMap::new();
        assert_eq!(store_path("grok", &env, "/h"), Some(PathBuf::from("/h/.grok/trusted_folders.toml")));
        env.insert("GROK_HOME".into(), "/x/g2".into());
        assert_eq!(store_path("grok", &env, "/h"), Some(PathBuf::from("/x/g2/trusted_folders.toml")));
    }

    #[test]
    fn claude_merge_keeps_every_other_field() {
        let before = r#"{
  "numStartups": 447,
  "oauthAccount": {"accountUuid": "abc", "emailAddress": "u@example.com"},
  "tipsHistory": {"new-user-warmup": 8},
  "projects": {
    "/home/u": {"hasTrustDialogAccepted": true, "lastCost": 1.25, "mcpServers": {}},
    "/home/u/other": {"allowedTools": ["Bash"]}
  },
  "autoUpdates": false
}"#;
        let after = claude_merge(before, &["/data/proj/main".into()]).unwrap().unwrap();
        let a: Value = serde_json::from_str(&after).unwrap();
        let b: Value = serde_json::from_str(before).unwrap();

        for (k, v) in b.as_object().unwrap() {
            if k == "projects" {
                continue;
            }
            assert_eq!(a.get(k), Some(v), "top-level `{k}` was lost or changed");
        }
        let bp = b["projects"].as_object().unwrap();
        for (k, v) in bp {
            assert_eq!(&a["projects"][k], v, "project `{k}` was lost or changed");
        }
        assert_eq!(a["projects"]["/home/u"]["lastCost"], json!(1.25));
        assert_eq!(a["projects"]["/home/u/other"]["allowedTools"], json!(["Bash"]));
        assert_eq!(a["projects"]["/data/proj/main"][CLAUDE_KEY], json!(true));
        assert_eq!(a["projects"].as_object().unwrap().len(), 3);
        assert!(after.ends_with("}\n"));
    }

    #[test]
    fn claude_merge_adds_projects_and_leaves_a_trusted_path_alone() {
        let out = claude_merge(r#"{"numStartups": 1}"#, &["/w".into()]).unwrap().unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["numStartups"], json!(1));
        assert_eq!(v["projects"]["/w"][CLAUDE_KEY], json!(true));

        let out = claude_merge("", &["/w".into()]).unwrap().unwrap();
        assert_eq!(serde_json::from_str::<Value>(&out).unwrap()["projects"]["/w"][CLAUDE_KEY], json!(true));

        assert!(claude_merge(&out, &["/w".into()]).unwrap().is_none());
        for k in CLAUDE_EXTERNAL_KEYS {
            assert_eq!(serde_json::from_str::<Value>(&out).unwrap()["projects"]["/w"][k], json!(true));
        }

        let out = claude_merge(r#"{"projects":{"/w":{"lastCost":3}}}"#, &["/w".into()]).unwrap().unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["projects"]["/w"]["lastCost"], json!(3));
        assert_eq!(v["projects"]["/w"][CLAUDE_KEY], json!(true));
    }

    #[test]
    fn claude_merge_refuses_a_file_it_does_not_understand() {
        assert!(claude_merge("not json", &["/w".into()]).is_err());
        assert!(claude_merge("[1,2]", &["/w".into()]).is_err());
        assert!(claude_merge(r#"{"projects": 7}"#, &["/w".into()]).is_err());
        assert!(claude_merge(r#"{"projects": {"/w": 7}}"#, &["/w".into()]).is_err());
    }

    #[test]
    fn codex_merge_keeps_comments_and_other_tables() {
        let before = r#"# my codex config
model = "gpt-5"

[tui]
theme = "dark"

[projects."/home/u/project"]
trust_level = "trusted"
"#;
        let out = codex_merge(before, &["/data/proj/dev-1".into()]).unwrap().unwrap();
        assert!(out.starts_with("# my codex config\n"), "comment lost:\n{out}");
        assert!(out.contains("[tui]\ntheme = \"dark\""), "table lost:\n{out}");
        assert!(out.contains("[projects.\"/home/u/project\"]"), "existing project lost:\n{out}");
        assert!(out.contains("[projects.\"/data/proj/dev-1\"]"), "new project missing:\n{out}");
        let v: toml::Value = toml::from_str(&out).unwrap();
        assert_eq!(v["model"].as_str(), Some("gpt-5"));
        assert_eq!(v["projects"]["/home/u/project"]["trust_level"].as_str(), Some("trusted"));
        assert_eq!(v["projects"]["/data/proj/dev-1"]["trust_level"].as_str(), Some("trusted"));

        assert!(codex_merge(&out, &["/data/proj/dev-1".into()]).unwrap().is_none());
        let fresh = codex_merge("", &["/w".into()]).unwrap().unwrap();
        assert!(fresh.contains("[projects.\"/w\"]"), "{fresh}");
        assert!(!fresh.contains("\n[projects]\n"), "bare [projects] header:\n{fresh}");
        assert!(codex_merge("nope = ", &["/w".into()]).is_err());
    }

    #[test]
    fn canonical_resolves_symlinks_and_survives_a_missing_directory() {
        let dir = std::env::temp_dir().join(format!("am-trust-canon-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("real")).unwrap();
        let link = dir.join("link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(dir.join("real"), &link).unwrap();

        let got = canonical(&link.to_string_lossy());
        let want = std::fs::canonicalize(dir.join("real")).unwrap();
        assert_eq!(got, want.to_string_lossy());
        assert!(!got.contains("/link"), "symlink not resolved: {got}");

        assert_eq!(canonical("/no/such/dir/anywhere"), "/no/such/dir/anywhere");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mark_trusted_is_atomic_and_idempotent_on_disk() {
        let dir = std::env::temp_dir().join(format!("am-trust-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let store = dir.join(".claude.json");
        std::fs::write(&store, r#"{"numStartups": 9, "projects": {"/keep": {"lastCost": 2}}}"#).unwrap();

        assert!(mark_trusted("claude", &store, &["/w1".into(), "/w2".into()]).unwrap());
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&store).unwrap()).unwrap();
        assert_eq!(v["numStartups"], json!(9));
        assert_eq!(v["projects"]["/keep"]["lastCost"], json!(2));
        assert_eq!(v["projects"]["/w1"][CLAUDE_KEY], json!(true));
        assert_eq!(v["projects"]["/w2"][CLAUDE_KEY], json!(true));

        let bytes = std::fs::read(&store).unwrap();
        assert!(!mark_trusted("claude", &store, &["/w1".into(), "/w2".into()]).unwrap());
        assert_eq!(std::fs::read(&store).unwrap(), bytes);
        let strays: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(strays.is_empty(), "temp files left: {strays:?}");

        let none = dir.join("shell-nothing");
        assert!(!mark_trusted("shell", &none, &["/w1".into()]).unwrap());
        assert!(!none.exists());

        let cx = dir.join("sub").join("config.toml");
        assert!(mark_trusted("codex", &cx, &["/w1".into()]).unwrap());
        let v: toml::Value = toml::from_str(&std::fs::read_to_string(&cx).unwrap()).unwrap();
        assert_eq!(v["projects"]["/w1"]["trust_level"].as_str(), Some("trusted"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mark_trusted_keeps_the_original_file_mode() {
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let dir = std::env::temp_dir().join(format!("am-trust-mode-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            let store = dir.join(".claude.json");
            std::fs::write(&store, "{}").unwrap();
            std::fs::set_permissions(&store, std::fs::Permissions::from_mode(0o600)).unwrap();

            assert!(mark_trusted("claude", &store, &["/w".into()]).unwrap());
            let mode = std::fs::metadata(&store).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "mode changed to {mode:o}");
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}

