//! Quick git for the chat header (2026-09-08): `GET /api/projects/:id/git` for the
//! `+N −M · ↑a ↓b` chip, and `POST …/git/{commit,push,pull}` behind its three buttons.
//!
//! Everything runs through `team_git::sh` on the project's host (local or ssh), on the
//! project's own checkout — no worktrees, no branches: this is the user's "commit what the
//! agents just did and push it" gesture, nothing more. Not a git repo → `{"git": false}` and
//! the UI hides the chip.

use crate::db;
use crate::lifecycle::LcError;
use crate::state::App;
use crate::team_git::{git, GIT_TIMEOUT, PUSH_TIMEOUT};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;

#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize)]
pub struct GitSummary {
    pub git: bool,
    pub branch: Option<String>,
    pub upstream: Option<String>,
    pub ahead: i64,
    pub behind: i64,
    /// Tracked files with changes (staged or not).
    pub changed: i64,
    pub untracked: i64,
    pub insertions: i64,
    pub deletions: i64,
}

async fn project(app: &Arc<App>, id: &str) -> Result<db::Project, LcError> {
    db::project(&app.db, id)
        .await
        .map_err(|e| LcError::Upstream(e.to_string()))?
        .filter(|p| p.deleted_at.is_none())
        .ok_or_else(|| LcError::NotFound("project".into()))
}

/// `git status --porcelain=v2 --branch` + `git diff --shortstat HEAD` → [`GitSummary`].
pub fn parse_summary(status: &str, shortstat: &str) -> GitSummary {
    let mut s = GitSummary { git: true, ..Default::default() };
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("# branch.head ") {
            s.branch = Some(rest.trim().to_string()).filter(|b| b != "(detached)");
        } else if let Some(rest) = line.strip_prefix("# branch.upstream ") {
            s.upstream = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("# branch.ab ") {
            // `+3 -1`
            for tok in rest.split_whitespace() {
                if let Some(n) = tok.strip_prefix('+') {
                    s.ahead = n.parse().unwrap_or(0);
                } else if let Some(n) = tok.strip_prefix('-') {
                    s.behind = n.parse().unwrap_or(0);
                }
            }
        } else if line.starts_with("1 ") || line.starts_with("2 ") || line.starts_with("u ") {
            s.changed += 1;
        } else if line.starts_with("? ") {
            s.untracked += 1;
        }
    }
    // ` 3 files changed, 12 insertions(+), 4 deletions(-)`
    for part in shortstat.split(',') {
        let part = part.trim();
        let n: i64 = part.split_whitespace().next().and_then(|x| x.parse().ok()).unwrap_or(0);
        if part.contains("insertion") {
            s.insertions = n;
        } else if part.contains("deletion") {
            s.deletions = n;
        }
    }
    s
}

pub async fn summary(app: &Arc<App>, id: &str) -> Result<GitSummary, LcError> {
    let p = project(app, id).await?;
    let st = git(app, &p.host, &p.path, &["status", "--porcelain=v2", "--branch"], GIT_TIMEOUT)
        .await
        .map_err(|e| LcError::Upstream(e.to_string()))?;
    if !st.ok() {
        // Not a repository (or git missing): the chip simply does not show.
        return Ok(GitSummary::default());
    }
    // `HEAD` fails on an unborn branch; an empty shortstat then just means zero.
    let stat = git(app, &p.host, &p.path, &["diff", "--shortstat", "HEAD"], GIT_TIMEOUT)
        .await
        .map(|o| if o.ok() { o.stdout } else { String::new() })
        .unwrap_or_default();
    Ok(parse_summary(&st.stdout, &stat))
}

#[derive(Debug, Deserialize)]
pub struct CommitBody {
    pub message: String,
}

fn done(op: &str, out: crate::team_git::Out) -> Result<Value, LcError> {
    if out.ok() {
        Ok(json!({"ok": true, "output": out.trimmed()}))
    } else {
        Err(LcError::Conflict(json!({"error": "conflict", "reason": format!("git_{op}_failed"), "output": out.message()})))
    }
}

/// `git add -A && git commit -m <message>`. Nothing to commit → 409 `nothing_to_commit`.
pub async fn commit(app: &Arc<App>, id: &str, message: &str) -> Result<Value, LcError> {
    let msg = message.trim();
    if msg.is_empty() {
        return Err(LcError::Bad("commit message is empty".into()));
    }
    let p = project(app, id).await?;
    let st = git(app, &p.host, &p.path, &["status", "--porcelain"], GIT_TIMEOUT).await.map_err(|e| LcError::Upstream(e.to_string()))?;
    if st.stdout.trim().is_empty() {
        return Err(LcError::conflict("nothing_to_commit", json!({})));
    }
    let add = git(app, &p.host, &p.path, &["add", "-A"], GIT_TIMEOUT).await.map_err(|e| LcError::Upstream(e.to_string()))?;
    if !add.ok() {
        return done("add", add);
    }
    let out = git(app, &p.host, &p.path, &["commit", "-m", msg], GIT_TIMEOUT).await.map_err(|e| LcError::Upstream(e.to_string()))?;
    done("commit", out)
}

/// `git push`; a branch with no upstream gets `-u origin HEAD` so the first push just works.
pub async fn push(app: &Arc<App>, id: &str) -> Result<Value, LcError> {
    let p = project(app, id).await?;
    let has_upstream = git(app, &p.host, &p.path, &["rev-parse", "--abbrev-ref", "@{upstream}"], GIT_TIMEOUT)
        .await
        .map(|o| o.ok())
        .unwrap_or(false);
    let args: &[&str] = if has_upstream { &["push"] } else { &["push", "-u", "origin", "HEAD"] };
    let out = git(app, &p.host, &p.path, args, PUSH_TIMEOUT).await.map_err(|e| LcError::Upstream(e.to_string()))?;
    done("push", out)
}

/// `git pull --rebase --no-autostash` — the repo rule (CLAUDE.md): never stash other agents' work.
pub async fn pull(app: &Arc<App>, id: &str) -> Result<Value, LcError> {
    let p = project(app, id).await?;
    let out = git(app, &p.host, &p.path, &["pull", "--rebase", "--no-autostash"], PUSH_TIMEOUT).await.map_err(|e| LcError::Upstream(e.to_string()))?;
    done("pull", out)
}

#[cfg(test)]
mod tests {
    use super::parse_summary;

    #[test]
    fn parses_status_and_shortstat() {
        let st = "# branch.oid abc\n# branch.head main\n# branch.upstream origin/main\n# branch.ab +2 -1\n1 .M N... 100644 100644 100644 x y web/a.ts\n2 R. N... 100644 100644 100644 x y R100 b.ts\tc.ts\n? new.txt\n";
        let s = parse_summary(st, " 2 files changed, 10 insertions(+), 3 deletions(-)\n");
        assert!(s.git);
        assert_eq!(s.branch.as_deref(), Some("main"));
        assert_eq!(s.upstream.as_deref(), Some("origin/main"));
        assert_eq!((s.ahead, s.behind), (2, 1));
        assert_eq!((s.changed, s.untracked), (2, 1));
        assert_eq!((s.insertions, s.deletions), (10, 3));
    }

    #[test]
    fn detached_and_clean() {
        let s = parse_summary("# branch.oid abc\n# branch.head (detached)\n", "");
        assert_eq!(s.branch, None);
        assert_eq!((s.changed, s.insertions, s.deletions), (0, 0, 0));
    }
}
