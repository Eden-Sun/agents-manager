//! Pre-trusting a workspace directory, so an agent CLI does not stop on its "do you trust
//! this folder?" dialog.
//!
//! Both claude and codex gate the *first* interactive run in a directory they have never
//! seen behind a confirmation dialog. Nobody is sitting at a team member's pane to answer
//! it, and for claude the cursor even starts on `No, exit`, so the CLI eventually quits by
//! itself and `agent.wait` comes back `agent_not_running`:
//!
//! ```text
//!  Accessing workspace: /private/tmp/.../trusttest2
//!  Quick safety check: Is this a project you created or one you trust? …
//!  ❯ No, exit
//!    Yes, I trust this folder
//! ```
//!
//! A team's worktrees (SPEC-team §6.2) are brand-new directories under
//! `<data_dir>/teams/<team_id>/`, so this fired on *every* team, which is why creating a
//! team always failed. `--dangerously-skip-permissions` does not suppress it (verified);
//! headless `claude -p` never shows it, only the interactive mode a pane runs.
//!
//! The fix is to write, before the members start, exactly the record the dialog would have
//! written:
//!
//! * claude — `projects["<dir>"].hasTrustDialogAccepted = true` in `.claude.json`;
//! * codex  — `[projects."<dir>"] trust_level = "trusted"` in `config.toml`;
//! * grok   — nothing: it has no start-up directory gate (its `trusted_folders.toml` only
//!   governs *project-local* `.grok/hooks/`, and the daemon installs its hook in the user
//!   layer at `$GROK_HOME/hooks/`). Verified on grok 4.6, 2026-09-06.
//!
//! Two details that are easy to get wrong, both measured rather than assumed:
//!
//! * **which file** — the record has to land in the config the *bot's identity* will read.
//!   `CLAUDE_CONFIG_DIR` (how `cc1` is kept apart from `cc0`) moves `.claude.json` with it,
//!   and `CODEX_HOME` does the same for codex.
//! * **which spelling of the path** — the CLI compares against its own `getcwd()`, which is
//!   the *physical* path (`/tmp` is a symlink to `/private/tmp` on macOS). A record written
//!   under `/tmp/x` does not match a process whose cwd is `/private/tmp/x`; it is skipped
//!   and the dialog appears anyway. Everything here goes through [`canonical`].
//!
//! These files belong to the agent CLIs, not to us: they are read, amended in place and
//! written back through a temporary file plus `rename`, and left alone entirely when the
//! record is already there.

use anyhow::{anyhow, bail, Context, Result};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::config::expand_home;
use crate::db;
use crate::state::App;

/// claude's per-project flag inside `.claude.json`.
const CLAUDE_KEY: &str = "hasTrustDialogAccepted";
/// The *second* first-run dialog (2026-09-08): a CLAUDE.md that `@imports` a file outside the
/// cwd (the user's `~/.claude/RTK.md`) stops the TUI on 「Allow external CLAUDE.md file
/// imports?」, cursor on *No*. Same shape as the trust record, same file, same fix.
const CLAUDE_EXTERNAL_KEYS: [&str; 2] = ["hasClaudeMdExternalIncludesApproved", "hasClaudeMdExternalIncludesWarningShown"];
/// codex's per-project key inside `config.toml`.
const CODEX_KEY: &str = "trust_level";
const CODEX_TRUSTED: &str = "trusted";

/// The physical path, the way an agent CLI sees its own cwd. Falls back to the input when
/// the directory does not exist (nothing else useful to write, and a bad key is inert).
pub fn canonical(path: &str) -> String {
    match std::fs::canonicalize(path) {
        Ok(p) => p.to_string_lossy().into_owned(),
        Err(_) => path.to_string(),
    }
}

/// The file holding `kind`'s directory-trust records for a pane started with `env`
/// (identity ∪ bot env, already `$HOME`-expanded). `None` for kinds without a gate.
pub fn store_path(kind: &str, env: &BTreeMap<String, String>, home: &str) -> Option<PathBuf> {
    let var = |k: &str| {
        env.get(k).map(|s| s.trim()).filter(|s| !s.is_empty()).map(|s| expand_home(s, home))
    };
    match kind {
        // `CLAUDE_CONFIG_DIR` relocates the whole state directory, `.claude.json` included.
        // Unset, the file is `~/.claude.json` — *not* `~/.claude/.claude.json`, which is why
        // this cannot reuse the `${CLAUDE_CONFIG_DIR:-$HOME/.claude}` shape settings.json uses.
        "claude" => Some(match var("CLAUDE_CONFIG_DIR") {
            Some(dir) => PathBuf::from(dir).join(".claude.json"),
            None => PathBuf::from(home).join(".claude.json"),
        }),
        "codex" => {
            let dir = var("CODEX_HOME").unwrap_or_else(|| format!("{home}/.codex"));
            Some(PathBuf::from(dir).join("config.toml"))
        }
        // grok: no start-up trust gate.
        _ => None,
    }
}

/// `.claude.json` with `projects[p].hasTrustDialogAccepted = true` for every `p`, and every
/// other key of the document left exactly as it was. `None` when nothing needed changing.
///
/// An empty/absent file yields a fresh document; a file that is not JSON, or whose
/// `projects` is not an object, is an error rather than something to overwrite.
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
    // Claude Code writes this file two-space-pretty with a trailing newline; match it so the
    // shape does not flip back and forth between its writer and ours.
    Ok(Some(format!("{}\n", serde_json::to_string_pretty(&root)?)))
}

/// codex's `config.toml` with `[projects."<p>"] trust_level = "trusted"` for every `p`.
/// `toml_edit` keeps the rest of the file — comments, ordering, formatting — byte-identical.
/// `None` when every path was already trusted.
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

/// Replace `path`'s contents with `text` in one step: a sibling temp file, flushed to disk,
/// then `rename`d over the target. A crash mid-write leaves the original untouched, which
/// matters because these are the user's own agent-CLI state files.
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

/// Record `paths` as trusted in `store` for `kind`. A no-op when they already are — the
/// file is not rewritten, so a run that changes nothing leaves no trace at all.
pub fn mark_trusted(kind: &str, store: &Path, paths: &[String]) -> Result<bool> {
    let existing = match std::fs::read_to_string(store) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", store.display())),
    };
    let next = match kind {
        "claude" => claude_merge(&existing, paths)?,
        "codex" => codex_merge(&existing, paths)?,
        _ => None,
    };
    let Some(next) = next else { return Ok(false) };
    write_atomic(store, &next)?;
    tracing::info!(kind, store = %store.display(), ?paths, "pre-trusted agent workspace directories");
    Ok(true)
}

/// The pane env that decides *which* config file a bot reads: the identity's env then the
/// bot's own, `$HOME` expanded against the local home (`lifecycle::pane_env`, minus the
/// daemon's own variables, none of which name a config directory).
async fn config_env(app: &Arc<App>, bot: &db::Bot, home: &str) -> BTreeMap<String, String> {
    let mut env: BTreeMap<String, String> = BTreeMap::new();
    // `identity_for_host`, not `cfg.identities`: a shell-discovered `ccN` (SPEC §16) is not in
    // config.toml, and looking only there sent cc2's trust record to `~/.claude.json` while
    // its CLI read `~/.claude-cc2/.claude.json` — so the dialog came up anyway (2026-09-08).
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

/// Mark every member's cwd as already trusted, in whichever config file that member's
/// identity actually reads. Best effort: returns one message per store that could not be
/// updated, because a team that might still start is better than one refused outright.
///
/// **Local host only.** Teams are local-only in this stage (SPEC-team §13); a remote member
/// would need the same record written over ssh in the *remote* home, which is not done here.
pub async fn pretrust_members(app: &Arc<App>, members: &[db::Bot]) -> Vec<String> {
    let Some(home) = dirs::home_dir() else {
        return vec!["no home directory; cannot pre-trust the team worktrees".into()];
    };
    let home = home.to_string_lossy().into_owned();

    // One read-modify-write per file, not per member: four members on one identity share a
    // `.claude.json`, and rewriting it four times only widens the window against the agent
    // CLIs, which write this file themselves.
    let mut jobs: BTreeMap<PathBuf, (String, BTreeSet<String>)> = BTreeMap::new();
    for b in members {
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
        // Default account: the file sits beside the home, not inside `~/.claude`.
        assert_eq!(
            store_path("claude", &env(&[]), "/home/u").unwrap(),
            PathBuf::from("/home/u/.claude.json")
        );
        // `cc1`-style identity: `$HOME` expands against the host's home.
        assert_eq!(
            store_path("claude", &env(&[("CLAUDE_CONFIG_DIR", "$HOME/.claude-ccompany")]), "/home/u").unwrap(),
            PathBuf::from("/home/u/.claude-ccompany/.claude.json")
        );
        // Blank is not a config dir.
        assert_eq!(
            store_path("claude", &env(&[("CLAUDE_CONFIG_DIR", "   ")]), "/home/u").unwrap(),
            PathBuf::from("/home/u/.claude.json")
        );
    }

    #[test]
    fn codex_store_follows_codex_home_and_grok_has_no_gate() {
        assert_eq!(
            store_path("codex", &env(&[]), "/home/u").unwrap(),
            PathBuf::from("/home/u/.codex/config.toml")
        );
        assert_eq!(
            store_path("codex", &env(&[("CODEX_HOME", "~/alt")]), "/home/u").unwrap(),
            PathBuf::from("/home/u/alt/config.toml")
        );
        // grok has no start-up trust dialog, so there is nothing to write.
        assert!(store_path("grok", &env(&[]), "/home/u").is_none());
    }

    /// The whole point of amending rather than rewriting: a real `.claude.json` carries
    /// dozens of unrelated top-level keys and dozens of other projects, and none of them
    /// may be disturbed.
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
        let after = claude_merge(before, &["/data/teams/t1/main".into()]).unwrap().unwrap();
        let a: Value = serde_json::from_str(&after).unwrap();
        let b: Value = serde_json::from_str(before).unwrap();

        // Every pre-existing top-level key survives, untouched.
        for (k, v) in b.as_object().unwrap() {
            if k == "projects" {
                continue;
            }
            assert_eq!(a.get(k), Some(v), "top-level `{k}` was lost or changed");
        }
        // Every pre-existing project survives, untouched — including its other fields.
        let bp = b["projects"].as_object().unwrap();
        for (k, v) in bp {
            assert_eq!(&a["projects"][k], v, "project `{k}` was lost or changed");
        }
        assert_eq!(a["projects"]["/home/u"]["lastCost"], json!(1.25));
        assert_eq!(a["projects"]["/home/u/other"]["allowedTools"], json!(["Bash"]));
        // …and the new one is trusted.
        assert_eq!(a["projects"]["/data/teams/t1/main"][CLAUDE_KEY], json!(true));
        assert_eq!(a["projects"].as_object().unwrap().len(), 3);
        assert!(after.ends_with("}\n"));
    }

    #[test]
    fn claude_merge_adds_projects_and_leaves_a_trusted_path_alone() {
        // No `projects` at all yet.
        let out = claude_merge(r#"{"numStartups": 1}"#, &["/w".into()]).unwrap().unwrap();
        let v: Value = serde_json::from_str(&out).unwrap();
        assert_eq!(v["numStartups"], json!(1));
        assert_eq!(v["projects"]["/w"][CLAUDE_KEY], json!(true));

        // An entirely absent file is a fresh document, not a failure.
        let out = claude_merge("", &["/w".into()]).unwrap().unwrap();
        assert_eq!(serde_json::from_str::<Value>(&out).unwrap()["projects"]["/w"][CLAUDE_KEY], json!(true));

        // Already trusted → no rewrite at all, so the user's file is never even touched.
        assert!(claude_merge(&out, &["/w".into()]).unwrap().is_none());
        // The external-imports dialog is pre-answered in the same record.
        for k in CLAUDE_EXTERNAL_KEYS {
            assert_eq!(serde_json::from_str::<Value>(&out).unwrap()["projects"]["/w"][k], json!(true));
        }

        // A member's other project keys are preserved when only the flag is missing.
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
        let out = codex_merge(before, &["/data/teams/t1/dev-1".into()]).unwrap().unwrap();
        assert!(out.starts_with("# my codex config\n"), "comment lost:\n{out}");
        assert!(out.contains("[tui]\ntheme = \"dark\""), "table lost:\n{out}");
        assert!(out.contains("[projects.\"/home/u/project\"]"), "existing project lost:\n{out}");
        assert!(out.contains("[projects.\"/data/teams/t1/dev-1\"]"), "new project missing:\n{out}");
        // Parses back, with both projects trusted and `model` intact.
        let v: toml::Value = toml::from_str(&out).unwrap();
        assert_eq!(v["model"].as_str(), Some("gpt-5"));
        assert_eq!(v["projects"]["/home/u/project"]["trust_level"].as_str(), Some("trusted"));
        assert_eq!(v["projects"]["/data/teams/t1/dev-1"]["trust_level"].as_str(), Some("trusted"));

        // Already trusted → nothing to write.
        assert!(codex_merge(&out, &["/data/teams/t1/dev-1".into()]).unwrap().is_none());
        // Empty file → a fresh document.
        let fresh = codex_merge("", &["/w".into()]).unwrap().unwrap();
        assert!(fresh.contains("[projects.\"/w\"]"), "{fresh}");
        assert!(!fresh.contains("\n[projects]\n"), "bare [projects] header:\n{fresh}");
        assert!(codex_merge("nope = ", &["/w".into()]).is_err());
    }

    /// The measured rule: the CLI compares against its own `getcwd()`, so `/tmp/x` on macOS
    /// has to be written as `/private/tmp/x` or the record is simply not found.
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

        // A path that does not exist is passed through rather than dropped.
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

        // Second run: nothing changes, and the file is left byte-identical.
        let bytes = std::fs::read(&store).unwrap();
        assert!(!mark_trusted("claude", &store, &["/w1".into(), "/w2".into()]).unwrap());
        assert_eq!(std::fs::read(&store).unwrap(), bytes);
        // No temp file left behind.
        let strays: Vec<_> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.ends_with(".tmp"))
            .collect();
        assert!(strays.is_empty(), "temp files left: {strays:?}");

        // A kind without a gate writes nothing, not even a file.
        let none = dir.join("grok-nothing");
        assert!(!mark_trusted("grok", &none, &["/w1".into()]).unwrap());
        assert!(!none.exists());

        // A missing file is created (codex's `config.toml` may not exist yet).
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

