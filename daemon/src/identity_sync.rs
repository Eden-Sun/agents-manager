use crate::api::RequestPrincipal;
use crate::config::IdentityCfg;
use crate::lc_error::{LcError, LcResult};
use crate::state::App;
use crate::{hosts, projection};
use axum::extract::{Extension, Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

const SYNC_TIMEOUT: Duration = Duration::from_secs(30);
const SYNCABLE: [&str; 7] = ["cc0", "cc1", "cc2", "cc3", "cc4", "cc5", "cc6"];

static HOST_LOCKS: OnceLock<Mutex<BTreeMap<String, Arc<tokio::sync::Mutex<()>>>>> = OnceLock::new();

fn host_lock(host: &str) -> Arc<tokio::sync::Mutex<()>> {
    HOST_LOCKS
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .entry(host.to_string())
        .or_default()
        .clone()
}

const PROBE: &str = r#"printf 'AM_SYNC home %s\n' "$HOME"
case "${SHELL:-}" in */zsh) ba_rc=.zshrc; ba_sh=zsh ;; *) ba_rc=.bashrc; ba_sh=bash ;; esac
printf 'AM_SYNC shell %s\n' "$ba_sh"
if [ -f "$HOME/$ba_rc" ] && [ ! -L "$HOME/$ba_rc" ]; then
  grep -E '^[[:space:]]*(alias[[:space:]]+)?cc[0-6]=' "$HOME/$ba_rc" 2>/dev/null | sed 's/^/AM_ALIAS /'
fi
printf 'AM_SYNC end\n'
"#;

const HELPER: &str = r#"import json, os, re, shutil, subprocess, sys, tempfile, uuid
def out(obj):
    print(json.dumps(obj))
    sys.exit(0)
home = os.environ.get("HOME", "")
if not os.path.isabs(home):
    out({"error": "no_home"})
shell = os.path.basename(os.environ.get("SHELL", ""))
if shell not in ("zsh", "bash"):
    out({"error": "unknown_shell"})
rc = os.path.join(home, ".zshrc" if shell == "zsh" else ".bashrc")
if os.path.islink(rc):
    out({"error": "symlink"})
existed = os.path.exists(rc)
text = ""
if existed:
    if not os.path.isfile(rc):
        out({"error": "not_a_file"})
    with open(rc, encoding="utf-8") as f:
        text = f.read()
req = json.loads(sys.stdin.read())
results = []
to_add = []
for it in req.get("items", []):
    name = it["name"]
    created = []
    if it.get("alias_line"):
        if re.search(r"^\s*(alias\s+)?" + re.escape(name) + r"=", text, re.M):
            results.append({"name": name, "status": "skipped", "reason": "already_exists", "created_dirs": []})
            continue
        to_add.append(it["alias_line"])
    for d in it.get("dirs", []):
        if not d.startswith("$HOME/"):
            out({"error": "bad_dir"})
        rel = d[len("$HOME/"):]
        parts = [seg for seg in rel.split("/") if seg]
        if not parts or any(seg in (".", "..") for seg in parts):
            out({"error": "bad_dir"})
        p = os.path.abspath(os.path.join(home, *parts))
        if os.path.commonpath([os.path.abspath(home), p]) != os.path.abspath(home) or p == os.path.abspath(home):
            out({"error": "bad_dir"})
        if os.path.islink(p):
            out({"error": "symlink"})
        if not os.path.isdir(p):
            os.makedirs(p, mode=0o700)
            created.append(d)
    results.append({"name": name, "status": "created", "reason": None, "created_dirs": created})
backup = None
if to_add:
    if existed:
        bdir = os.path.join(home, ".config", "agents-manager", "shell-alias-backups")
        os.makedirs(bdir, mode=0o700, exist_ok=True)
        backup = os.path.join(bdir, shell + "rc-" + uuid.uuid4().hex + ".bak")
        fd = os.open(backup, os.O_WRONLY | os.O_CREAT | os.O_EXCL, 0o600)
        with os.fdopen(fd, "w", encoding="utf-8") as f:
            f.write(text)
    mode = (os.stat(rc).st_mode & 0o777) if existed else 0o600
    new = text + ("" if text == "" or text.endswith("\n") else "\n") + "".join(l + "\n" for l in to_add)
    checker = shutil.which(shell)
    if checker is None:
        out({"error": "syntax_check_unavailable"})
    fd, tmp = tempfile.mkstemp(dir=home, prefix=".am-rc-")
    with os.fdopen(fd, "w", encoding="utf-8") as f:
        f.write(new)
    os.chmod(tmp, mode)
    if subprocess.run([checker, "-n", tmp], capture_output=True).returncode != 0:
        os.unlink(tmp)
        out({"error": "syntax_check_failed"})
    os.replace(tmp, rc)
out({"backup_path": backup, "results": results})
"#;

#[derive(Debug, Clone)]
struct Candidate {
    name: String,
    kind: String,
    source: &'static str,
    env: BTreeMap<String, String>,
    args: Vec<String>,
}

fn is_syncable(name: &str) -> bool {
    SYNCABLE.contains(&name)
}

pub fn normalize_home(value: &str, home: &str) -> String {
    if home.is_empty() {
        return value.to_string();
    }
    if value == home {
        return "$HOME".into();
    }
    if let Some(rest) = value.strip_prefix(&format!("{home}/")) {
        return format!("$HOME/{rest}");
    }
    if let Some(rest) = value.strip_prefix("~/") {
        return format!("$HOME/{rest}");
    }
    value.to_string()
}

fn normalize_env(env: &BTreeMap<String, String>, home: &str) -> BTreeMap<String, String> {
    env.iter().map(|(k, v)| (k.clone(), normalize_home(v, home))).collect()
}

pub fn alias_line(name: &str, env: &BTreeMap<String, String>) -> String {
    match env.get("CLAUDE_CONFIG_DIR") {
        Some(dir) => format!("alias {name}='CLAUDE_CONFIG_DIR={dir} claude'"),
        None => format!("alias {name}='claude'"),
    }
}

fn local_home() -> String {
    std::env::var("HOME").unwrap_or_default()
}

async fn local_candidates(app: &Arc<App>) -> Vec<Candidate> {
    let home = local_home();
    let cfg = app.cfg.get().await;
    let shell: Vec<IdentityCfg> = app.tools.lock().await.get("local").map(|t| t.shell_identities.clone()).unwrap_or_default();
    let mut out = Vec::new();
    for s in shell.iter().filter(|s| is_syncable(&s.name)) {
        if cfg.identities.iter().any(|c| c.name == s.name && c.host_or_local() == "local") {
            continue;
        }
        out.push(Candidate { name: s.name.clone(), kind: s.kind.clone(), source: "shell", env: normalize_env(&s.env, &home), args: vec![] });
    }
    for c in cfg.identities.iter().filter(|c| c.host.as_deref() == Some("local")) {
        out.push(Candidate { name: c.name.clone(), kind: c.kind.clone(), source: "config", env: normalize_env(&c.env, &home), args: c.args.clone() });
    }
    out
}

async fn ensure_known_host(app: &Arc<App>, host: &str) -> LcResult<hosts::HostFence> {
    if host == "local" {
        return Err(LcError::BadValue(json!({"error": "bad_request", "reason": "cannot_sync_local", "host": host})));
    }
    if app.hosts.get(host).await.is_none() {
        return Err(LcError::conflict("unknown_host", json!({"host": host})));
    }
    app.hosts.fence(host).await.ok_or_else(|| LcError::conflict("host_offline", json!({"host": host})))
}

fn require_user(principal: &RequestPrincipal) -> LcResult<()> {
    if *principal != RequestPrincipal::User {
        return Err(LcError::Forbidden(json!({"error": "forbidden", "reason": "user_only"})));
    }
    Ok(())
}

async fn probe_remote(fence: &hosts::HostFence) -> LcResult<(String, String, Vec<IdentityCfg>)> {
    let out = fence
        .conn()
        .ssh_exec_path_timeout(PROBE, SYNC_TIMEOUT)
        .await
        .map_err(|e| LcError::Upstream(format!("{e:#}")))?;
    let home = out.lines().find_map(|l| l.strip_prefix("AM_SYNC home ")).unwrap_or("").trim().to_string();
    let shell = out.lines().find_map(|l| l.strip_prefix("AM_SYNC shell ")).unwrap_or("bash").trim().to_string();
    if !home.starts_with('/') {
        return Err(LcError::Upstream("remote HOME is not an absolute path".into()));
    }
    let aliases = crate::tools::parse_shell_identities(&out);
    Ok((home, shell, aliases))
}

fn rc_name(shell: &str) -> &'static str {
    if shell == "zsh" {
        ".zshrc"
    } else {
        ".bashrc"
    }
}

fn dirs_of(env: &BTreeMap<String, String>) -> Vec<String> {
    env.values().filter(|v| v.starts_with("$HOME/")).cloned().collect()
}

pub async fn sync_plan(
    State(app): State<Arc<App>>,
    Path(host): Path<String>,
    Extension(principal): Extension<RequestPrincipal>,
) -> LcResult<Response> {
    require_user(&principal)?;
    let fence = ensure_known_host(&app, &host).await?;
    let (remote_home, shell, remote_aliases) = probe_remote(&fence).await?;
    let cfg = app.cfg.get().await;
    let remote_config: Vec<&IdentityCfg> = cfg.identities.iter().filter(|c| c.host.as_deref() == Some(host.as_str())).collect();
    let local = local_candidates(&app).await;
    let mut items = Vec::new();
    let mut already = 0usize;
    for c in local {
        let (status, can_apply, note, remote_env) = match c.source {
            "shell" => {
                let same_config = remote_config.iter().find(|r| r.name == c.name);
                let remote = remote_aliases.iter().find(|r| r.name == c.name);
                match (same_config, remote) {
                    (Some(_), _) => ("different", false, Some("遠端已有同名的設定身份，不覆寫".to_string()), None),
                    (None, None) => ("missing", true, None, None),
                    (None, Some(r)) => {
                        let remote_env = normalize_env(&r.env, &remote_home);
                        if remote_env == c.env {
                            already += 1;
                            continue;
                        }
                        ("different", false, Some("遠端已有同名的 alias 但目錄不同，不覆寫".to_string()), Some(remote_env))
                    }
                }
            }
            _ => {
                let same = remote_config.iter().find(|r| r.name == c.name);
                match same {
                    None => ("missing", true, None, None),
                    Some(r) => {
                        let remote_env = normalize_env(&r.env, &remote_home);
                        if r.kind == c.kind && remote_env == c.env && r.args == c.args {
                            already += 1;
                            continue;
                        }
                        ("different", false, Some("遠端已有同名的設定身份但內容不同，不覆寫".to_string()), Some(remote_env))
                    }
                }
            }
        };
        let alias = (c.source == "shell").then(|| alias_line(&c.name, &c.env));
        items.push(json!({
            "name": c.name,
            "kind": c.kind,
            "source": c.source,
            "status": status,
            "can_apply": can_apply,
            "local_env": c.env,
            "remote_env": remote_env,
            "alias_line": alias,
            "note": note,
        }));
    }
    Ok((
        StatusCode::OK,
        Json(json!({
            "host": host,
            "remote_shell": shell,
            "remote_rc": format!("~/{}", rc_name(&shell)),
            "already_synced": already,
            "items": items,
        })),
    )
        .into_response())
}

#[derive(Deserialize)]
pub struct SyncBody {
    pub names: Vec<String>,
    #[serde(default)]
    pub dry_run: bool,
}

pub async fn apply_sync(
    State(app): State<Arc<App>>,
    Path(host): Path<String>,
    Extension(principal): Extension<RequestPrincipal>,
    Json(body): Json<SyncBody>,
) -> LcResult<Response> {
    require_user(&principal)?;
    if body.names.is_empty() {
        return Err(LcError::BadValue(json!({"error": "bad_request", "reason": "names_required"})));
    }
    let _guard = host_lock(&host).lock_owned().await;
    let fence = ensure_known_host(&app, &host).await?;
    let (remote_home, shell, remote_aliases) = probe_remote(&fence).await?;
    let cfg = app.cfg.get().await;
    let remote_config_names: Vec<String> = cfg.identities.iter().filter(|c| c.host.as_deref() == Some(host.as_str())).map(|c| c.name.clone()).collect();
    let local = local_candidates(&app).await;
    let mut results = Vec::new();
    let mut writes = Vec::new();
    let mut config_adds: Vec<Candidate> = Vec::new();
    let mut seen = BTreeSet::new();
    for name in &body.names {
        if !seen.insert(name.as_str()) {
            continue;
        }
        let Some(c) = local.iter().find(|c| &c.name == name) else {
            results.push(json!({"name": name, "status": "skipped", "reason": "not_found_on_local", "created_dirs": []}));
            continue;
        };
        let taken = remote_config_names.contains(&c.name) || (c.source == "shell" && remote_aliases.iter().any(|r| r.name == c.name));
        if taken {
            results.push(json!({"name": name, "kind": c.kind, "source": c.source, "status": "skipped", "reason": "already_exists", "created_dirs": []}));
            continue;
        }
        if body.dry_run {
            let alias = (c.source == "shell").then(|| alias_line(&c.name, &c.env));
            results.push(json!({"name": name, "kind": c.kind, "source": c.source, "status": "would_create", "reason": null, "message": null, "alias_line": alias, "created_dirs": dirs_of(&c.env)}));
            continue;
        }
        let alias = (c.source == "shell").then(|| alias_line(&c.name, &c.env));
        writes.push(json!({"name": c.name, "alias_line": alias, "dirs": dirs_of(&c.env)}));
        if c.source == "config" {
            config_adds.push(c.clone());
        }
    }
    let mut backup_path: Option<String> = None;
    let mut created_names: Vec<(String, String, &'static str, Option<String>)> = Vec::new();
    if !writes.is_empty() {
        let req = serde_json::to_vec(&json!({"items": writes})).map_err(|e| LcError::Upstream(e.to_string()))?;
        let script = format!("python3 -I -c {}", hosts::sh_quote(HELPER));
        let out = fence
            .conn()
            .ssh_exec_stdin(&script, &req, SYNC_TIMEOUT)
            .await
            .map_err(|e| LcError::Upstream(format!("{e:#}")))?;
        let parsed: Value = serde_json::from_str(out.trim()).map_err(|_| LcError::Upstream("identity sync helper output was not JSON".into()))?;
        if let Some(err) = parsed.get("error").and_then(Value::as_str) {
            return Err(LcError::conflict("identity_sync_failed", json!({"host": host, "reason": err})));
        }
        backup_path = parsed.get("backup_path").and_then(Value::as_str).map(str::to_string);
        for r in parsed.get("results").and_then(Value::as_array).cloned().unwrap_or_default() {
            let name = r.get("name").and_then(Value::as_str).unwrap_or("").to_string();
            let status = r.get("status").and_then(Value::as_str).unwrap_or("skipped");
            let c = local.iter().find(|c| c.name == name);
            if status == "created" {
                if let Some(c) = c {
                    created_names.push((c.name.clone(), c.kind.clone(), c.source, c.env.get("CLAUDE_CONFIG_DIR").cloned()));
                }
            }
            results.push(json!({
                "name": name,
                "kind": c.map(|c| c.kind.clone()),
                "source": c.map(|c| c.source),
                "status": status,
                "reason": r.get("reason").cloned().unwrap_or(Value::Null),
                "message": null,
                "alias_line": c.filter(|c| c.source == "shell").map(|c| alias_line(&c.name, &c.env)),
                "created_dirs": r.get("created_dirs").cloned().unwrap_or(json!([])),
            }));
        }
    }
    let mut added_config = false;
    for c in &config_adds {
        let (name, kind, env, args) = (c.name.clone(), c.kind.clone(), c.env.clone(), c.args.clone());
        let target = host.clone();
        projection::update_and_project(&app.cfg, &app.db, move |f| {
            if !f.identities.iter().any(|i| i.name == name && i.host.as_deref() == Some(target.as_str())) {
                f.identities.push(IdentityCfg { name, kind, host: Some(target), env, args });
            }
            Ok(())
        })
        .await
        .map_err(|e| LcError::conflict("config.toml was not changed", json!({"reason": "identity_sync_config_failed", "message": e.to_string()})))?;
        added_config = true;
        if c.kind == "agy" {
            crate::tools::ensure_agy_shadow_home(&fence, &c.env, &remote_home).await.map_err(|e| LcError::Upstream(format!("{e:#}")))?;
        }
    }
    if !body.dry_run && (!created_names.is_empty() || added_config) {
        {
            let mut tools = app.tools.lock().await;
            let ht = tools.entry(host.clone()).or_insert_with(|| crate::tools::HostTools {
                tools: Default::default(),
                identities: Default::default(),
                shell_identities: Vec::new(),
                utc_offset_secs: None,
                herdr_cli: None,
                checked_at: crate::db::now(),
            });
            for (name, kind, source, config_dir) in &created_names {
                if *source == "shell" && !ht.shell_identities.iter().any(|i| i.name == *name) {
                    let mut env = BTreeMap::new();
                    if let Some(dir) = config_dir {
                        env.insert("CLAUDE_CONFIG_DIR".to_string(), dir.clone());
                    }
                    ht.shell_identities.push(IdentityCfg { name: name.clone(), kind: kind.clone(), host: None, env, args: vec![] });
                }
                ht.identities.insert(
                    name.clone(),
                    crate::tools::IdentityInfo {
                        name: name.clone(),
                        kind: kind.clone(),
                        logged_in: Some(false),
                        reason: Some("新建於遠端，尚未登入".into()),
                        account: None,
                        plan: None,
                        source: if *source == "shell" { crate::tools::SOURCE_SHELL } else { crate::tools::SOURCE_CONFIG },
                        config_dir: config_dir.clone(),
                        checked_at: None,
                        basis: None,
                    },
                );
            }
        }
        {
            let mut cache = app.models_cache.lock().await;
            for (name, _, _, _) in &created_names {
                cache.retain(|k, _| k.splitn(3, '/').nth(2) != Some(name.as_str()));
            }
        }
        if added_config {
            app.emit("identities_changed", json!({})).await;
        }
        crate::state::emit_host_changed(&app, &fence).await;
        crate::tools::spawn_detect(app.clone(), host.clone());
    }
    Ok((
        StatusCode::OK,
        Json(json!({"host": host, "dry_run": body.dry_run, "backup_path": backup_path, "shell": shell, "results": results})),
    )
        .into_response())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn home_paths_are_written_relative_to_home() {
        assert_eq!(normalize_home("/home/ubuntu/.claude-cc2", "/home/ubuntu"), "$HOME/.claude-cc2");
        assert_eq!(normalize_home("/home/ubuntu", "/home/ubuntu"), "$HOME");
        assert_eq!(normalize_home("~/.claude-cc3", "/home/ubuntu"), "$HOME/.claude-cc3");
        assert_eq!(normalize_home("/home/ubuntux/.x", "/home/ubuntu"), "/home/ubuntux/.x");
        assert_eq!(normalize_home("$HOME/.claude-cc1", "/home/ubuntu"), "$HOME/.claude-cc1");
    }

    #[test]
    fn an_alias_line_round_trips_through_the_probe_parser() {
        let mut env = BTreeMap::new();
        env.insert("CLAUDE_CONFIG_DIR".to_string(), "$HOME/.claude-cc2".to_string());
        let line = alias_line("cc2", &env);
        assert_eq!(line, "alias cc2='CLAUDE_CONFIG_DIR=$HOME/.claude-cc2 claude'");
        let parsed = crate::tools::parse_shell_identities(&format!("AM_ALIAS {line}\n"));
        assert_eq!(parsed.len(), 1);
        assert_eq!(parsed[0].name, "cc2");
        assert_eq!(parsed[0].env.get("CLAUDE_CONFIG_DIR").map(String::as_str), Some("$HOME/.claude-cc2"));
        assert_eq!(alias_line("cc0", &BTreeMap::new()), "alias cc0='claude'");
    }

    #[test]
    fn only_the_seven_alias_names_are_syncable() {
        assert!(is_syncable("cc0") && is_syncable("cc6"));
        assert!(!is_syncable("cc7") && !is_syncable("lead") && !is_syncable("cc"));
    }

    fn run_helper(home: &std::path::Path, shell: &str, input: &str) -> String {
        use std::io::Write;
        let script = format!("python3 -I -c {}", hosts::sh_quote(HELPER));
        let mut child = std::process::Command::new("/bin/sh")
            .arg("-c")
            .arg(script)
            .env_clear()
            .env("HOME", home)
            .env("SHELL", shell)
            .env("PATH", "/usr/bin:/bin")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child.stdin.take().unwrap().write_all(input.as_bytes()).unwrap();
        let out = child.wait_with_output().unwrap();
        String::from_utf8(out.stdout).unwrap()
    }

    fn scratch(tag: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("am-identity-sync-{tag}-{}", crate::db::ulid()));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn helper_creates_the_alias_and_its_directory_once_in_a_fake_home() {
        let home = scratch("create");
        let input = json!({"items": [{"name": "cc2", "alias_line": "alias cc2='CLAUDE_CONFIG_DIR=$HOME/.claude-cc2 claude'", "dirs": ["$HOME/.claude-cc2"]}]}).to_string();
        let first: Value = serde_json::from_str(run_helper(&home, "/bin/bash", &input).trim()).unwrap();
        assert!(first.get("error").is_none(), "{first}");
        assert!(first["backup_path"].is_null(), "no backup when the rc did not exist: {first}");
        assert_eq!(first["results"][0]["status"], "created");
        assert!(home.join(".claude-cc2").is_dir());
        let rc = std::fs::read_to_string(home.join(".bashrc")).unwrap();
        assert!(rc.contains("alias cc2='CLAUDE_CONFIG_DIR=$HOME/.claude-cc2 claude'"), "{rc}");
        let again: Value = serde_json::from_str(run_helper(&home, "/bin/bash", &input).trim()).unwrap();
        assert_eq!(again["results"][0]["status"], "skipped");
        assert_eq!(again["results"][0]["reason"], "already_exists");
        assert_eq!(std::fs::read_to_string(home.join(".bashrc")).unwrap().matches("alias cc2=").count(), 1);
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn helper_backs_up_an_existing_rc_before_appending_and_never_writes_credentials() {
        let home = scratch("backup");
        std::fs::write(home.join(".zshrc"), "export FOO=1\n").unwrap();
        std::fs::create_dir_all(home.join(".claude-cc1")).unwrap();
        std::fs::write(home.join(".claude-cc1/.credentials.json"), "secret").unwrap();
        let input = json!({"items": [{"name": "cc3", "alias_line": "alias cc3='claude'", "dirs": []}]}).to_string();
        let out: Value = serde_json::from_str(run_helper(&home, "/bin/zsh", &input).trim()).unwrap();
        let backup = out["backup_path"].as_str().expect("a backup path");
        assert_eq!(std::fs::read_to_string(backup).unwrap(), "export FOO=1\n");
        let rc = std::fs::read_to_string(home.join(".zshrc")).unwrap();
        assert!(rc.starts_with("export FOO=1\n") && rc.contains("alias cc3='claude'"), "{rc}");
        assert!(!home.join(".claude-cc3/.credentials.json").exists());
        std::fs::remove_dir_all(&home).ok();
    }

    #[test]
    fn helper_rejects_parent_directory_traversal() {
        let home = scratch("traversal");
        let input = json!({"items": [{"name": "cc2", "alias_line": "alias cc2='claude'", "dirs": ["$HOME/../escape"]}]}).to_string();
        let out: Value = serde_json::from_str(run_helper(&home, "/bin/bash", &input).trim()).unwrap();
        assert_eq!(out["error"], "bad_dir");
        std::fs::remove_dir_all(&home).ok();
    }
}
