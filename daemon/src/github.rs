//! v4.0 — GitHub origin detection (`projects[].github`) and issue listing through `gh`.
//!
//! Origin: `git -C <path> remote get-url origin` on the project's host (local process, or
//! `ssh_exec_path` for a remote host), parsed for the usual GitHub URL shapes. Cached per
//! project until the next reconcile or `POST /projects/:id/github/refresh`.
//!
//! Issues: `gh issue list --repo owner/repo --json …` on the same host (the user has run
//! `gh auth login` there). Cached 2 min per (project, state, q, limit).

use crate::config::LOCAL_HOST;
use crate::db;
use crate::hosts::sh_quote;
use crate::lifecycle::LcError;
use crate::state::App;
use anyhow::{anyhow, Result};
use serde::Serialize;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub const ISSUES_TTL: Duration = Duration::from_secs(120);
const GIT_TIMEOUT: Duration = Duration::from_secs(15);
const GH_TIMEOUT: Duration = Duration::from_secs(40);
/// `body_excerpt` length in characters.
const EXCERPT_CHARS: usize = 300;

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct GithubInfo {
    pub owner: String,
    pub repo: String,
    pub url: String,
}

impl GithubInfo {
    pub fn slug(&self) -> String {
        format!("{}/{}", self.owner, self.repo)
    }
}

/// `git@github.com:owner/repo.git` / `https://github.com/owner/repo(.git)` /
/// `ssh://git@github.com/owner/repo` / `github.com/owner/repo` → (owner, repo).
pub fn parse_github_remote(url: &str) -> Option<GithubInfo> {
    let u = url.trim();
    let rest = if let Some(r) = u.strip_prefix("git@github.com:") {
        r
    } else {
        let no_scheme = u
            .strip_prefix("https://")
            .or_else(|| u.strip_prefix("http://"))
            .or_else(|| u.strip_prefix("ssh://"))
            .or_else(|| u.strip_prefix("git://"))
            .unwrap_or(u);
        // drop userinfo (`git@`, `user:token@`)
        let no_user = no_scheme.rsplit_once('@').map(|(_, h)| h).unwrap_or(no_scheme);
        let host_path = no_user.strip_prefix("github.com")?;
        host_path.strip_prefix('/').or_else(|| host_path.strip_prefix(':'))?
    };
    let mut parts = rest.trim_end_matches('/').splitn(3, '/');
    let owner = parts.next()?.trim();
    let repo = parts.next()?.trim().trim_end_matches(".git").trim();
    if owner.is_empty() || repo.is_empty() {
        return None;
    }
    Some(GithubInfo { owner: owner.into(), repo: repo.into(), url: format!("https://github.com/{owner}/{repo}") })
}

// ---------------------------------------------------------------- running commands on a host

/// Run a POSIX `sh` script on `host`; stdout on success.
async fn run_on_host(app: &Arc<App>, host: &str, script: &str, timeout: Duration) -> Result<String> {
    if host == LOCAL_HOST {
        let o = tokio::time::timeout(
            timeout,
            tokio::process::Command::new("/bin/sh").arg("-c").arg(script).stdin(std::process::Stdio::null()).output(),
        )
        .await
        .map_err(|_| anyhow!("command timed out"))??;
        if !o.status.success() {
            let err = String::from_utf8_lossy(&o.stderr).trim().to_string();
            let out = String::from_utf8_lossy(&o.stdout).trim().to_string();
            anyhow::bail!("{}", if err.is_empty() { out } else { err });
        }
        return Ok(String::from_utf8_lossy(&o.stdout).to_string());
    }
    let conn = app.hosts.get(host).await.ok_or_else(|| anyhow!("unknown host `{host}`"))?;
    conn.ssh_exec_path(script).await
}

/// PATH prefix so `gh` / `git` from Homebrew are found even from a launchd daemon.
/// Also kill color forcing: some agent / IDE shells export `CLICOLOR_FORCE=1`, and
/// `gh --json` then pretty-prints with ANSI — which breaks `serde_json`.
const PATH_FIX: &str = "export PATH=\"/opt/homebrew/bin:/usr/local/bin:$HOME/.local/bin:$PATH\"\n\
export NO_COLOR=1\nunset CLICOLOR_FORCE FORCE_COLOR CLICOLOR 2>/dev/null\n";

/// Strip CSI / OSC ANSI sequences so a colored `gh` dump is still parseable.
pub fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\u{1b}' {
            out.push(c);
            continue;
        }
        match chars.peek() {
            Some('[') => {
                chars.next();
                while let Some(x) = chars.next() {
                    if ('\x40'..='\x7e').contains(&x) {
                        break;
                    }
                }
            }
            Some(']') => {
                chars.next();
                while let Some(x) = chars.next() {
                    if x == '\u{7}' {
                        break;
                    }
                    if x == '\u{1b}' && matches!(chars.peek(), Some('\\')) {
                        chars.next();
                        break;
                    }
                }
            }
            Some(_) => {
                let _ = chars.next();
            }
            None => {}
        }
    }
    out
}
// ---------------------------------------------------------------- origin detection

pub async fn detect_project(app: &Arc<App>, p: &db::Project) -> Option<GithubInfo> {
    let script = format!("{PATH_FIX}git -C {} remote get-url origin 2>/dev/null", sh_quote(&p.path));
    let out = match run_on_host(app, &p.host, &script, GIT_TIMEOUT).await {
        Ok(o) => o,
        Err(e) => {
            tracing::debug!(project = %p.label, host = %p.host, error = %e, "git remote lookup failed");
            return None;
        }
    };
    let info = parse_github_remote(out.lines().next().unwrap_or(""));
    app.github.lock().await.insert(p.id.clone(), info.clone());
    info
}

/// Detect every live project on `host` (spawned; off the reconcile path).
pub fn spawn_detect_host(app: Arc<App>, host: String) {
    tokio::spawn(async move {
        let projects = db::live_projects(&app.db).await.unwrap_or_default();
        let mut changed = false;
        for p in projects.into_iter().filter(|p| p.host == host) {
            let before = app.github.lock().await.get(&p.id).cloned();
            let after = detect_project(&app, &p).await;
            if before.as_ref() != Some(&after) {
                changed = true;
            }
        }
        if changed {
            app.emit("project_changed", json!({})).await;
        }
    });
}

pub fn spawn_detect_all(app: Arc<App>) {
    spawn_detect_host(app, LOCAL_HOST.to_string());
}

/// The cached value for `GET /api/state` (`None` = unknown / not GitHub → `null`).
pub async fn cached(app: &Arc<App>, project_id: &str) -> Option<GithubInfo> {
    app.github.lock().await.get(project_id).cloned().flatten()
}

// ---------------------------------------------------------------- issues via gh

fn gh_error(e: impl std::fmt::Display) -> LcError {
    let msg = e.to_string();
    let hint = if msg.contains("not found") || msg.contains("command not found") || msg.contains("No such file") {
        "gh 未安裝（brew install gh）"
    } else if msg.contains("auth login") || msg.contains("not logged") || msg.contains("authentication") {
        "gh 未登入（gh auth login）"
    } else {
        "gh 未安裝或未登入，或指令失敗"
    };
    LcError::Upstream(format!("{hint}: {msg}"))
}

fn excerpt(body: &str) -> String {
    let flat: String = body.split_whitespace().collect::<Vec<_>>().join(" ");
    let mut s: String = flat.chars().take(EXCERPT_CHARS).collect();
    if flat.chars().count() > EXCERPT_CHARS {
        s.push('…');
    }
    s
}

fn labels(v: &Value) -> Vec<String> {
    v.get("labels")
        .and_then(|l| l.as_array())
        .map(|a| a.iter().filter_map(|x| x.get("name").and_then(|n| n.as_str()).map(String::from)).collect())
        .unwrap_or_default()
}

fn author(v: &Value) -> Option<String> {
    v.get("author").and_then(|a| a.get("login")).and_then(|l| l.as_str()).map(String::from)
}

pub fn issue_summary(v: &Value) -> Value {
    json!({
        "number": v.get("number").and_then(|n| n.as_i64()).unwrap_or(0),
        "title": v.get("title").and_then(|t| t.as_str()).unwrap_or(""),
        "state": v.get("state").and_then(|t| t.as_str()).unwrap_or(""),
        "labels": labels(v),
        "url": v.get("url").and_then(|t| t.as_str()).unwrap_or(""),
        "updated_at": v.get("updatedAt").and_then(|t| t.as_str()),
        "author": author(v),
        "body_excerpt": excerpt(v.get("body").and_then(|b| b.as_str()).unwrap_or("")),
    })
}

async fn project_with_github(app: &Arc<App>, project_id: &str) -> Result<(db::Project, GithubInfo), LcError> {
    let p = db::project(&app.db, project_id)
        .await
        .map_err(|e| LcError::Upstream(e.to_string()))?
        .filter(|p| p.deleted_at.is_none())
        .ok_or_else(|| LcError::NotFound("project".into()))?;
    let gh = match cached(app, &p.id).await {
        Some(g) => g,
        None => detect_project(app, &p).await.ok_or_else(|| LcError::Bad("project has no GitHub origin".into()))?,
    };
    Ok((p, gh))
}

/// `GET /api/projects/:id/issues`
pub async fn list_issues(
    app: &Arc<App>,
    project_id: &str,
    state: &str,
    limit: u32,
    q: Option<&str>,
    refresh: bool,
) -> Result<Value, LcError> {
    let state = match state {
        "open" | "closed" | "all" => state,
        _ => return Err(LcError::Bad("state must be open, closed or all".into())),
    };
    let limit = limit.clamp(1, 100);
    let q = q.map(str::trim).filter(|s| !s.is_empty());
    let (p, gh) = project_with_github(app, project_id).await?;
    let key = format!("{}|{state}|{limit}|{}", p.id, q.unwrap_or(""));
    if !refresh {
        if let Some((at, v)) = app.issues_cache.lock().await.get(&key) {
            if at.elapsed() < ISSUES_TTL {
                return Ok(v.clone());
            }
        }
    }
    let mut cmd = format!(
        "{PATH_FIX}gh issue list --repo {} --state {state} --limit {limit} --json number,title,state,labels,url,updatedAt,author,body",
        sh_quote(&gh.slug())
    );
    if let Some(q) = q {
        cmd.push_str(&format!(" --search {}", sh_quote(q)));
    }
    let out = run_on_host(app, &p.host, &cmd, GH_TIMEOUT).await.map_err(gh_error)?;
    let cleaned = strip_ansi(&out);
    let arr: Vec<Value> =
        serde_json::from_str(cleaned.trim()).map_err(|e| gh_error(format!("{e}: {}", cleaned.trim())))?;
    let v = json!({
        "project_id": p.id,
        "repo": gh.slug(),
        "source": "gh",
        "fetched_at": db::now(),
        "issues": arr.iter().map(issue_summary).collect::<Vec<_>>(),
    });
    app.issues_cache.lock().await.insert(key, (Instant::now(), v.clone()));
    Ok(v)
}

/// `GET /api/projects/:id/issues/:number` — full body, uncached.
pub async fn get_issue(app: &Arc<App>, project_id: &str, number: u64) -> Result<Value, LcError> {
    let (p, gh) = project_with_github(app, project_id).await?;
    let cmd = format!(
        "{PATH_FIX}gh issue view {number} --repo {} --json number,title,state,labels,url,updatedAt,author,body",
        sh_quote(&gh.slug())
    );
    let out = run_on_host(app, &p.host, &cmd, GH_TIMEOUT).await.map_err(gh_error)?;
    let cleaned = strip_ansi(&out);
    let v: Value =
        serde_json::from_str(cleaned.trim()).map_err(|e| gh_error(format!("{e}: {}", cleaned.trim())))?;    let mut issue = issue_summary(&v);
    if let Some(o) = issue.as_object_mut() {
        o.remove("body_excerpt");
        o.insert("body".into(), json!(v.get("body").and_then(|b| b.as_str()).unwrap_or("")));
    }
    Ok(json!({"project_id": p.id, "repo": gh.slug(), "issue": issue}))
}

/// `gh issue close` — the daemon's only write to GitHub outside `deliver=pr`.
///
/// **Never called on the daemon's own initiative.** `team::close_issue` is reached from one
/// explicit user action on a finished team, which is the whole point of the feature: the team
/// says the work is done, a human decides whether that closes the issue.
///
/// The issue's state is read first because `gh issue close` exits non-zero on an
/// already-closed issue, and an exit code alone cannot tell "someone else closed it" from
/// "the repo is unreachable". An issue that is already closed is reported, not failed — the
/// user's intent already holds.
pub async fn close_issue(
    app: &Arc<App>,
    project_id: &str,
    number: u64,
    comment: Option<&str>,
) -> Result<Value, LcError> {
    let (p, gh) = project_with_github(app, project_id).await?;
    let slug = gh.slug();
    let view = format!(
        "{PATH_FIX}gh issue view {number} --repo {} --json number,state,url,title",
        sh_quote(&slug)
    );
    let out = run_on_host(app, &p.host, &view, GH_TIMEOUT).await.map_err(gh_error)?;
    let cleaned = strip_ansi(&out);
    let before: Value =
        serde_json::from_str(cleaned.trim()).map_err(|e| gh_error(format!("{e}: {}", cleaned.trim())))?;
    let url = before.get("url").and_then(Value::as_str).unwrap_or("").to_string();
    let title = before.get("title").and_then(Value::as_str).unwrap_or("").to_string();
    let was_closed = before
        .get("state")
        .and_then(Value::as_str)
        .map(|s| s.eq_ignore_ascii_case("closed"))
        .unwrap_or(false);

    if !was_closed {
        let mut cmd = format!("{PATH_FIX}gh issue close {number} --repo {}", sh_quote(&slug));
        if let Some(c) = comment.map(str::trim).filter(|s| !s.is_empty()) {
            cmd.push_str(&format!(" --comment {}", sh_quote(c)));
        }
        run_on_host(app, &p.host, &cmd, GH_TIMEOUT).await.map_err(gh_error)?;
        // The IssuesBar reads a two-minute cache; a closed issue lingering in it looks like
        // the close silently failed.
        let prefix = format!("{}|", p.id);
        app.issues_cache.lock().await.retain(|k, _| !k.starts_with(&prefix));
    }
    Ok(json!({
        "project_id": p.id,
        "repo": slug,
        "number": number,
        "title": title,
        "url": url,
        "state": "CLOSED",
        "already_closed": was_closed,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_remote_shapes() {
        for u in [
            "git@github.com:Eden-Sun/powertech-hub.git",
            "https://github.com/Eden-Sun/powertech-hub",
            "https://github.com/Eden-Sun/powertech-hub.git",
            "ssh://git@github.com/Eden-Sun/powertech-hub",
            "ssh://git@github.com/Eden-Sun/powertech-hub.git\n",
            "git://github.com/Eden-Sun/powertech-hub.git",
        ] {
            let g = parse_github_remote(u).unwrap_or_else(|| panic!("{u}"));
            assert_eq!(g.owner, "Eden-Sun");
            assert_eq!(g.repo, "powertech-hub");
            assert_eq!(g.url, "https://github.com/Eden-Sun/powertech-hub");
        }
        assert!(parse_github_remote("git@gitlab.com:a/b.git").is_none());
        assert!(parse_github_remote("").is_none());
        assert!(parse_github_remote("https://github.com/only-owner").is_none());
    }

    #[test]
    fn excerpt_flattens_and_caps() {
        assert_eq!(excerpt("a\n\nb   c\n"), "a b c");
        let long = "x".repeat(400);
        let e = excerpt(&long);
        assert_eq!(e.chars().count(), 301);
        assert!(e.ends_with('…'));
    }

    #[test]
    fn summary_shape() {
        let v = json!({"number": 7, "title": "T", "state": "OPEN", "labels": [{"name": "bug"}], "url": "u",
                       "updatedAt": "2026-09-05T12:00:00Z", "author": {"login": "me"}, "body": "hello\nworld"});
        let s = issue_summary(&v);
        assert_eq!(s["number"], 7);
        assert_eq!(s["labels"], json!(["bug"]));
        assert_eq!(s["author"], "me");
        assert_eq!(s["body_excerpt"], "hello world");
    }

    #[test]
    fn strip_ansi_makes_colored_json_parseable() {
        let colored = "\u{1b}[1;37m[\u{1b}[m\n  \u{1b}[1;37m{\u{1b}[m\n    \u{1b}[1;34m\"number\"\u{1b}[m\u{1b}[1;37m:\u{1b}[m 21\n  \u{1b}[1;37m}\u{1b}[m\n\u{1b}[1;37m]\u{1b}[m";
        let cleaned = strip_ansi(colored);
        let v: Value = serde_json::from_str(&cleaned).expect(&cleaned);
        assert_eq!(v[0]["number"], 21);
    }
}
