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

/// `None` when already trusted (or the kind has no gate).
fn merged(kind: &str, existing: &str, paths: &[String]) -> Result<Option<String>> {
    match kind {
        "claude" => claude_merge(existing, paths),
        "codex" => codex_merge(existing, paths),
        "grok" => {
            let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
            grok_merge(existing, paths, now)
        }
        _ => Ok(None),
    }
}

/// No-op (file not rewritten) when already trusted.
pub fn mark_trusted(kind: &str, store: &Path, paths: &[String]) -> Result<bool> {
    let existing = match std::fs::read_to_string(store) {
        Ok(s) => s,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => String::new(),
        Err(e) => return Err(e).with_context(|| format!("reading {}", store.display())),
    };
    let Some(next) = merged(kind, &existing, paths)? else { return Ok(false) };
    write_atomic(store, &next)?;
    tracing::info!(kind, store = %store.display(), ?paths, "pre-trusted agent workspace directories");
    Ok(true)
}

/// Identity env then bot env (`lifecycle::pane_env` minus daemon vars, which name no config dir).
async fn config_env(app: &Arc<App>, bot: &db::Bot, host: &str, home: &str) -> BTreeMap<String, String> {
    let mut env: BTreeMap<String, String> = BTreeMap::new();
    // `identity_for_host`, not `cfg.identities`: shell-discovered `ccN` (SPEC §16) aren't in
    // config.toml, which once sent cc2's record to the wrong file (2026-09-08).
    if let Some(name) = bot.identity.as_deref().filter(|s| !s.is_empty()) {
        if let Some(id) = crate::tools::identity_for_host(app, host, name).await {
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
/// Local host; remote bots go through [`pretrust_bots_remote`].
pub async fn pretrust_bots(app: &Arc<App>, bots: &[db::Bot]) -> Vec<String> {
    let Some(home) = dirs::home_dir() else {
        return vec!["no home directory; cannot pre-trust the working directory".into()];
    };
    let home = home.to_string_lossy().into_owned();

    // One write per file, not per bot: the CLIs write these files too, so minimise the race.
    let mut jobs: BTreeMap<PathBuf, (String, BTreeSet<String>)> = BTreeMap::new();
    for b in bots {
        let Some(cwd) = b.cwd.as_deref().map(str::trim).filter(|s| !s.is_empty()) else { continue };
        let env = config_env(app, b, crate::config::LOCAL_HOST, &home).await;
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

/// `start_inner` 在開 pane 之前呼叫：本機寫檔，遠端經 ssh（#407）。best effort，回傳每個失敗的說明。
pub async fn pretrust_for_start(app: &Arc<App>, bot: &db::Bot, host: &str, cwd: &str) -> Vec<String> {
    let mut b = bot.clone();
    b.cwd = Some(cwd.to_string());
    if host == crate::config::LOCAL_HOST {
        pretrust_bots(app, std::slice::from_ref(&b)).await
    } else {
        pretrust_bots_remote(app, host, std::slice::from_ref(&b)).await
    }
}

/// 整段遠端預先信任的**總**預算。這段在 `start_inner` 裡 await，擋在開 pane 前面：主機沒死透（TCP 收下但不回話）
/// 時每一趟 ssh 都會慢慢等到 `SSH_EXEC_TIMEOUT`（30 秒），累起來每次遠端啟動要多等好幾分鐘。
/// 所以整段包一個上限，超時只記 warn、照樣啟動——最差就是回到原本那個 trust 提示（#407 review）。
const REMOTE_BUDGET: std::time::Duration = std::time::Duration::from_secs(8);

/// 讀到寫之間被 CLI 改掉時重讀幾次。只給真的 race 用（`AM_TRUST_CHANGED`）：ssh 失敗是直接回錯，不在這裡重試。
const REMOTE_RACE_ATTEMPTS: usize = 2;

/// 同 [`pretrust_bots`]，檔案在遠端：經 ssh 讀、在這裡合併（規則只有一份）、再經 ssh 寫回（#407）。
pub async fn pretrust_bots_remote(app: &Arc<App>, host: &str, bots: &[db::Bot]) -> Vec<String> {
    pretrust_bots_remote_within(app, host, bots, REMOTE_BUDGET).await
}

/// `budget` 拆出來是為了測得到上限：假 ssh 掛住時，呼叫端必須在上限內拿回警告。
/// 逾時會把整個 future 丟掉，`ssh_exec` 的子行程是 `kill_on_drop`，所以不會留下跑著的 ssh。
async fn pretrust_bots_remote_within(app: &Arc<App>, host: &str, bots: &[db::Bot], budget: std::time::Duration) -> Vec<String> {
    match tokio::time::timeout(budget, remote_jobs(app, host, bots)).await {
        Ok(errors) => errors,
        Err(_) => vec![format!("{host}: pre-trusting the working directory took longer than {}s; left to the trust dialog", budget.as_secs())],
    }
}

async fn remote_jobs(app: &Arc<App>, host: &str, bots: &[db::Bot]) -> Vec<String> {
    let Some(conn) = app.hosts.get(host).await else { return vec![format!("unknown host `{host}`")] };
    let home = match conn.home().await {
        Ok(h) => h,
        Err(e) => return vec![format!("{host}: cannot resolve the remote home: {e:#}")],
    };
    let mut jobs: BTreeMap<PathBuf, (String, BTreeSet<String>)> = BTreeMap::new();
    for b in bots {
        let Some(cwd) = b.cwd.as_deref().map(str::trim).filter(|s| !s.is_empty()) else { continue };
        let env = config_env(app, b, host, &home).await;
        let Some(store) = store_path(&b.kind, &env, &home) else { continue };
        jobs.entry(store).or_insert_with(|| (b.kind.clone(), BTreeSet::new())).1.insert(cwd.to_string());
    }
    let mut errors = Vec::new();
    for (store, (kind, paths)) in jobs {
        let paths: Vec<String> = paths.into_iter().collect();
        let store = store.to_string_lossy().into_owned();
        if let Err(e) = mark_trusted_remote(&conn, &kind, &store, &paths).await {
            errors.push(format!("{host}:{store}: {e:#}"));
        }
    }
    errors
}

/// 讀的那一趟同時把每個工作目錄的**遠端**真實路徑（`pwd -P`）帶回來：CLI 寫的鍵是它自己 `cwd` 看到的路徑，
/// worktree、symlink、`/tmp`→`/private/tmp` 都會讓字面路徑變成一個沒用的鍵，claude 照樣問（#407 review）。
/// 本機那半用 [`canonical`]；這裡不能用，那是 daemon 這台的檔案系統。目錄還不存在就照字面留著（跟 [`canonical`] 一樣）。
fn remote_read_script(store_q: &str, paths: &[String]) -> String {
    use crate::hosts::sh_quote;
    let mut s = String::new();
    for path in paths {
        let q = sh_quote(path);
        s.push_str(&format!("P={q}\nC=$(cd -- \"$P\" 2>/dev/null && pwd -P)\n[ -n \"$C\" ] || C=$P\nprintf 'AM_P=%s\\n' \"$C\"\n"));
    }
    s.push_str(&format!(
        "F={store_q}\nif [ -f \"$F\" ]; then printf 'AM_SUM=%s\\n' \"$(cksum < \"$F\")\"; cat \"$F\"; else printf 'AM_SUM=missing\\n'; fi\n"
    ));
    s
}

/// `(遠端真實路徑, cksum, 檔案內容)`。缺了哪一段都當成錯：把「讀不完整」當成「檔案是空的」會整個蓋掉使用者的檔。
fn parse_remote_read(out: &str, want: usize) -> Result<(Vec<String>, String, String)> {
    let mut paths = Vec::new();
    let mut rest = out;
    loop {
        let (line, tail) = rest.split_once('\n').unwrap_or((rest, ""));
        if let Some(p) = line.strip_prefix("AM_P=") {
            paths.push(p.to_string());
            rest = tail;
            continue;
        }
        let Some(sum) = line.strip_prefix("AM_SUM=") else {
            bail!("unexpected reply while reading the trust store: {}", out.trim());
        };
        if paths.len() != want {
            bail!("expected {want} remote paths, got {}: {}", paths.len(), out.trim());
        }
        return Ok((paths, sum.to_string(), tail.to_string()));
    }
}

/// 讀 → 合併 → 寫回，寫之前比對 `cksum`：CLI 自己在中間改過檔（claude 常寫 `.claude.json`）就放棄這次、重讀再合併，
/// 不拿舊內容蓋掉。暫存檔＋`mv`，權限照原檔（新檔 0600，跟 `.claude.json` 一樣）；`mv` 之前死掉由 `trap` 收掉暫存檔。
/// 已經信任的在第一趟（只讀）就回 `Ok(false)`，不會有第二趟 ssh。
async fn mark_trusted_remote(conn: &crate::hosts::HostConn, kind: &str, store: &str, paths: &[String]) -> Result<bool> {
    use crate::hosts::sh_quote;
    let f = sh_quote(store);
    for _ in 0..REMOTE_RACE_ATTEMPTS {
        let out = conn.ssh_exec(&remote_read_script(&f, paths)).await?;
        let (real_paths, sum, existing) = parse_remote_read(&out, paths.len()).with_context(|| format!("reading {store}"))?;
        // `pwd -P` 之後才去重：兩顆 bot 可能用不同的 symlink 指到同一個目錄。
        let real_paths: Vec<String> = real_paths.into_iter().collect::<BTreeSet<_>>().into_iter().collect();
        let Some(next) = merged(kind, &existing, &real_paths)? else { return Ok(false) };
        let body = next.strip_suffix('\n').unwrap_or(&next);
        let mut delim = String::from("AM_TRUST_EOF");
        while body.contains(&delim) {
            delim.push('_');
        }
        let write = format!(
            "set -e\nF={f}\nD=$(dirname \"$F\")\nmkdir -p \"$D\"\ncur=missing\nif [ -f \"$F\" ]; then cur=$(cksum < \"$F\"); fi\n\
             if [ \"$cur\" != {sum} ]; then printf 'AM_TRUST_CHANGED\\n'; exit 0; fi\n\
             T=\"$D/.$(basename \"$F\").am-trust.$$.tmp\"\ntrap 'rm -f \"$T\"' EXIT\numask 077\ncat > \"$T\" <<'{delim}'\n{body}\n{delim}\n\
             if [ -f \"$F\" ]; then chmod \"$(stat -c %a \"$F\" 2>/dev/null || stat -f %Lp \"$F\")\" \"$T\" 2>/dev/null || true; fi\n\
             mv -f \"$T\" \"$F\"\nprintf 'AM_TRUST_OK\\n'\n",
            sum = sh_quote(&sum),
        );
        let out = conn.ssh_exec(&write).await?;
        if out.contains("AM_TRUST_OK") {
            tracing::info!(host = %conn.name, kind, store, paths = ?real_paths, "pre-trusted agent workspace directories on the remote");
            return Ok(true);
        }
        if !out.contains("AM_TRUST_CHANGED") {
            bail!("writing {store} did not confirm: {}", out.trim());
        }
    }
    bail!("{store} kept changing while pre-trusting it")
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


/// #407：遠端也要預先信任。ssh 換成「在本機 `/bin/sh` 跑那段腳本」，暫存目錄當遠端家目錄——腳本真的被執行，
/// 讀寫的是真的檔案，不是只比對字串。
#[cfg(test)]
mod remote_tests {
    use super::*;
    use crate::testing as tt;
    use std::io::Write;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const CWD: &str = "/home/ubuntu/zz-proj";

    fn run_sh(script: &str) -> Result<String> {
        let mut child = std::process::Command::new("/bin/sh")
            .arg("-s")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()?;
        child.stdin.take().unwrap().write_all(script.as_bytes())?;
        let out = child.wait_with_output()?;
        if !out.status.success() {
            bail!("sh failed: {}", String::from_utf8_lossy(&out.stderr));
        }
        Ok(String::from_utf8_lossy(&out.stdout).into_owned())
    }

    /// 遠端主機＋兩個只在那台的 claude 身分（各自的 `CLAUDE_CONFIG_DIR`）。ssh 假貨以主機名為鍵、是全域的：
    /// 每個測試用自己的主機名，平行跑才不會互蓋。
    async fn remote_env(host: &'static str) -> (tt::Env, PathBuf) {
        let env = tt::env().await;
        let home = env.dir.join("remote-home");
        std::fs::create_dir_all(&home).unwrap();
        let cfg = crate::config::HostCfg {
            name: host.into(),
            ssh: host.into(),
            ssh_port: 22,
            ssh_opts: vec![],
            herdr_session: "agents-manager".into(),
            remote_path: String::new(),
        };
        let conn = env.app.hosts.insert_remote_for_test(cfg).await;
        *conn.remote_home.lock().await = Some(home.to_string_lossy().into_owned());
        env.app
            .cfg
            .update(|c| {
                for n in ["ra", "rb"] {
                    c.identities.push(crate::config::IdentityCfg {
                        name: n.into(),
                        kind: "claude".into(),
                        host: Some(host.into()),
                        env: [("CLAUDE_CONFIG_DIR".to_string(), format!("$HOME/.claude-{n}"))].into(),
                        args: vec![],
                    });
                }
                Ok(())
            })
            .await
            .unwrap();
        (env, home)
    }

    async fn bot_on(env: &tt::Env, identity: &str) -> db::Bot {
        let bot = tt::claude_bot(&env.app, &env.project_id, "remote").await;
        switch(env, &bot.id, identity).await
    }

    async fn switch(env: &tt::Env, bot_id: &str, identity: &str) -> db::Bot {
        let bot = db::bot(&env.app.db, bot_id).await.unwrap().unwrap();
        sqlx::query("UPDATE bots SET identity = ? WHERE id = ?").bind(identity).bind(&bot.id).execute(&env.app.db).await.unwrap();
        db::bot(&env.app.db, &bot.id).await.unwrap().unwrap()
    }

    fn trusted(store: &Path) -> Value {
        let v: Value = serde_json::from_str(&std::fs::read_to_string(store).unwrap()).unwrap();
        v["projects"][CWD][CLAUDE_KEY].clone()
    }

    /// 換身分＝換到一個從沒用過（目錄都還沒有）的設定目錄：照樣寫進**那個**目錄的 `.claude.json`。
    #[tokio::test]
    async fn switching_a_remote_bot_to_a_never_used_identity_pre_trusts_its_config_dir() {
        const HOST: &str = "trustbox-switch";
        let (env, home) = remote_env(HOST).await;
        crate::hosts::set_ssh_fake(HOST, run_sh);

        let bot = bot_on(&env, "ra").await;
        assert!(pretrust_for_start(&env.app, &bot, HOST, CWD).await.is_empty());
        assert_eq!(trusted(&home.join(".claude-ra/.claude.json")), json!(true));

        let bot = switch(&env, &bot.id, "rb").await;
        assert!(!home.join(".claude-rb").exists(), "前提：B 的目錄還不存在");
        let errs = pretrust_for_start(&env.app, &bot, HOST, CWD).await;
        assert!(errs.is_empty(), "{errs:?}");
        assert_eq!(trusted(&home.join(".claude-rb/.claude.json")), json!(true), "換過去的身分也信任了");
        assert!(!home.join(".claude.json").exists(), "沒寫到預設帳號的檔");
    }

    /// 既有的 `.claude.json` 其他欄位原樣；已經信任時不重寫。
    #[tokio::test]
    async fn remote_pre_trust_keeps_the_rest_of_the_file_and_skips_when_already_trusted() {
        const HOST: &str = "trustbox-keep";
        let (env, home) = remote_env(HOST).await;
        let writes = Arc::new(AtomicUsize::new(0));
        let w = writes.clone();
        crate::hosts::set_ssh_fake(HOST, move |script| {
            if script.contains("AM_TRUST_OK") {
                w.fetch_add(1, Ordering::SeqCst);
            }
            run_sh(script)
        });
        let store = home.join(".claude-ra/.claude.json");
        std::fs::create_dir_all(store.parent().unwrap()).unwrap();
        std::fs::write(&store, r#"{"hasCompletedOnboarding":true,"oauthAccount":{"x":1},"projects":{"/other":{"allowedTools":["Bash"]}}}"#).unwrap();

        let bot = bot_on(&env, "ra").await;
        assert!(pretrust_for_start(&env.app, &bot, HOST, CWD).await.is_empty());
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&store).unwrap()).unwrap();
        assert_eq!(v["hasCompletedOnboarding"], json!(true));
        assert_eq!(v["oauthAccount"], json!({"x": 1}));
        assert_eq!(v["projects"]["/other"], json!({"allowedTools": ["Bash"]}));
        assert_eq!(v["projects"][CWD][CLAUDE_KEY], json!(true));
        assert_eq!(writes.load(Ordering::SeqCst), 1);

        assert!(pretrust_for_start(&env.app, &bot, HOST, CWD).await.is_empty());
        assert_eq!(writes.load(Ordering::SeqCst), 1, "已經信任了就不再寫");
    }

    /// 讀和寫之間 claude 自己改了檔（它常寫 `.claude.json`）：不能拿舊內容蓋掉，重讀再合併。
    #[tokio::test]
    async fn remote_pre_trust_does_not_clobber_a_concurrent_write() {
        const HOST: &str = "trustbox-race";
        let (env, home) = remote_env(HOST).await;
        let store = home.join(".claude-ra/.claude.json");
        std::fs::create_dir_all(store.parent().unwrap()).unwrap();
        std::fs::write(&store, r#"{"numStartups":1}"#).unwrap();
        let raced = Arc::new(AtomicUsize::new(0));
        let (r, st) = (raced.clone(), store.clone());
        crate::hosts::set_ssh_fake(HOST, move |script| {
            if script.contains("AM_TRUST_OK") && r.fetch_add(1, Ordering::SeqCst) == 0 {
                std::fs::write(&st, r#"{"numStartups":2,"tipsHistory":{"a":1}}"#).unwrap();
            }
            run_sh(script)
        });

        let bot = bot_on(&env, "ra").await;
        let errs = pretrust_for_start(&env.app, &bot, HOST, CWD).await;
        assert!(errs.is_empty(), "{errs:?}");
        let v: Value = serde_json::from_str(&std::fs::read_to_string(&store).unwrap()).unwrap();
        assert_eq!(v["numStartups"], json!(2), "中途寫進去的留著");
        assert_eq!(v["tipsHistory"], json!({"a": 1}));
        assert_eq!(v["projects"][CWD][CLAUDE_KEY], json!(true));
    }

    /// #407 review (1)：主機沒死透時每趟 ssh 都會等滿 `SSH_EXEC_TIMEOUT`，這段又擋在開 pane 前面。
    /// 整段有總上限，超過就只回警告——`start_inner` 照樣往下走。
    #[tokio::test]
    async fn a_hung_remote_gives_the_start_a_warning_within_the_budget() {
        const HOST: &str = "trustbox-hang";
        let (env, home) = remote_env(HOST).await;
        // 收下連線卻不回話：每趟 ssh 都會慢慢等到 `SSH_EXEC_TIMEOUT`（30 秒）。
        crate::hosts::set_ssh_delay(HOST, std::time::Duration::from_secs(30));
        crate::hosts::set_ssh_fake(HOST, run_sh);
        let bot = bot_on(&env, "ra").await;
        let mut b = bot.clone();
        b.cwd = Some(CWD.to_string());

        let budget = std::time::Duration::from_millis(300);
        let t0 = std::time::Instant::now();
        let errs = pretrust_bots_remote_within(&env.app, HOST, std::slice::from_ref(&b), budget).await;
        let took = t0.elapsed();

        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(errs[0].contains("took longer than"), "{errs:?}");
        assert!(took < std::time::Duration::from_secs(5), "沒有在上限內回來：{took:?}");
        assert!(!home.join(".claude-ra").exists(), "逾時不會留半個檔");
    }

    /// #407 review (1)：已經信任的只花一趟 ssh（讀），不會再發第二趟。
    #[tokio::test]
    async fn an_already_trusted_remote_store_costs_exactly_one_ssh_round_trip() {
        const HOST: &str = "trustbox-oneshot";
        let (env, home) = remote_env(HOST).await;
        let calls = Arc::new(AtomicUsize::new(0));
        let c = calls.clone();
        crate::hosts::set_ssh_fake(HOST, move |script| {
            c.fetch_add(1, Ordering::SeqCst);
            run_sh(script)
        });
        let bot = bot_on(&env, "ra").await;
        assert!(pretrust_for_start(&env.app, &bot, HOST, CWD).await.is_empty());
        assert_eq!(calls.swap(0, Ordering::SeqCst), 2, "第一次是讀 + 寫");
        assert_eq!(trusted(&home.join(".claude-ra/.claude.json")), json!(true));

        assert!(pretrust_for_start(&env.app, &bot, HOST, CWD).await.is_empty());
        assert_eq!(calls.load(Ordering::SeqCst), 1, "已經信任：只讀一趟，不再寫");
    }

    /// #407 review (2)：鍵要用**遠端**的 `pwd -P`。symlink 的工作目錄（worktree、`/tmp`→`/private/tmp`）
    /// 照字面寫下去只會多一個沒用的鍵，claude 起來照樣問。
    #[tokio::test]
    async fn the_recorded_key_is_the_remote_physical_path_not_the_literal_cwd() {
        const HOST: &str = "trustbox-symlink";
        let (env, home) = remote_env(HOST).await;
        crate::hosts::set_ssh_fake(HOST, run_sh);

        let real = env.dir.join("real-proj");
        std::fs::create_dir_all(&real).unwrap();
        let link = env.dir.join("link-proj");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let physical = std::fs::canonicalize(&real).unwrap().to_string_lossy().into_owned();
        assert_ne!(physical, link.to_string_lossy(), "前提：兩個路徑不一樣");

        let bot = bot_on(&env, "ra").await;
        let errs = pretrust_for_start(&env.app, &bot, HOST, &link.to_string_lossy()).await;
        assert!(errs.is_empty(), "{errs:?}");

        let v: Value = serde_json::from_str(&std::fs::read_to_string(home.join(".claude-ra/.claude.json")).unwrap()).unwrap();
        assert_eq!(v["projects"][&physical][CLAUDE_KEY], json!(true), "寫的是 pwd -P 的結果：{v}");
        assert!(v["projects"].get(link.to_string_lossy().as_ref()).is_none(), "不留字面路徑那個沒用的鍵：{v}");
        assert_eq!(v["projects"].as_object().unwrap().len(), 1);
    }

    /// 目錄還不存在（bot 的 cwd 還沒建出來）就照字面留著，跟本機的 [`canonical`] 一樣，不是錯。
    #[tokio::test]
    async fn a_remote_cwd_that_does_not_exist_yet_is_recorded_verbatim() {
        const HOST: &str = "trustbox-nodir";
        let (env, home) = remote_env(HOST).await;
        crate::hosts::set_ssh_fake(HOST, run_sh);
        let bot = bot_on(&env, "ra").await;
        assert!(pretrust_for_start(&env.app, &bot, HOST, "/no/such/dir/anywhere").await.is_empty());
        let v: Value = serde_json::from_str(&std::fs::read_to_string(home.join(".claude-ra/.claude.json")).unwrap()).unwrap();
        assert_eq!(v["projects"]["/no/such/dir/anywhere"][CLAUDE_KEY], json!(true));
    }

    /// #407 review (3)：`mv` 之前掛掉不能把 `.am-trust.*.tmp` 留在使用者的設定目錄裡。
    #[tokio::test]
    async fn a_write_that_dies_before_the_rename_leaves_no_temp_file() {
        const HOST: &str = "trustbox-trap";
        let (env, home) = remote_env(HOST).await;
        let dir = home.join(".claude-ra");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join(".claude.json"), "{}").unwrap();
        // `mv` 那一行換成 `exit 1`：cat 已經寫好暫存檔，接著就死了。
        crate::hosts::set_ssh_fake(HOST, |script| run_sh(&script.replace("mv -f \"$T\" \"$F\"", "exit 1")));

        let bot = bot_on(&env, "ra").await;
        let errs = pretrust_for_start(&env.app, &bot, HOST, CWD).await;
        assert_eq!(errs.len(), 1, "寫失敗要回警告：{errs:?}");
        let strays: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.contains("am-trust"))
            .collect();
        assert!(strays.is_empty(), "暫存檔沒被 trap 收掉：{strays:?}");
        assert_eq!(std::fs::read_to_string(dir.join(".claude.json")).unwrap(), "{}", "原檔沒被動到");
    }

    /// 讀回來缺一段（ssh 中途斷、遠端 shell 吐別的）不能當成「檔案是空的」而整個蓋掉。
    #[test]
    fn a_truncated_remote_read_is_an_error_not_an_empty_file() {
        let (paths, sum, body) = parse_remote_read("AM_P=/a\nAM_SUM=missing\n", 1).unwrap();
        assert_eq!((paths, sum.as_str(), body.as_str()), (vec!["/a".to_string()], "missing", ""));
        let (_, sum, body) = parse_remote_read("AM_P=/a\nAM_SUM=1 2\n{\"x\":1}", 1).unwrap();
        assert_eq!((sum.as_str(), body.as_str()), ("1 2", "{\"x\":1}"));
        assert!(parse_remote_read("AM_SUM=missing\n", 1).is_err(), "少了路徑");
        assert!(parse_remote_read("AM_P=/a\n", 1).is_err(), "少了 cksum");
        assert!(parse_remote_read("", 1).is_err());
    }

    /// ssh 失敗是警告，不擋啟動（跟本機一樣 best effort），也不會寫出半個檔。
    #[tokio::test]
    async fn an_unreachable_remote_is_a_warning_not_a_failure() {
        const HOST: &str = "trustbox-down";
        let (env, home) = remote_env(HOST).await;
        crate::hosts::set_ssh_fake(HOST, |_| bail!("ssh: connect to host: Connection refused"));
        let bot = bot_on(&env, "ra").await;
        let errs = pretrust_for_start(&env.app, &bot, HOST, CWD).await;
        assert_eq!(errs.len(), 1, "{errs:?}");
        assert!(errs[0].contains("Connection refused"), "{errs:?}");
        assert!(!home.join(".claude-ra").exists());
    }
}
