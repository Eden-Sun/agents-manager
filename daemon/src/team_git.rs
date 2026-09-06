//! Every git / `gh` command a team runs (SPEC-team §6, appendix C).
//!
//! Two rules shape this module, and both come from §6.1:
//!
//! 1. **The user's checkout is never written to.** The only commands aimed at
//!    `<project.path>` are `rev-parse`, `branch`, `worktree add|remove|prune` and
//!    `remote get-url` — none of which touch the main working tree. Every command that
//!    changes files runs with `-C <worktree>`, and every worktree lives under
//!    `<data_dir>/teams/<team_id>/`, outside the repo (§6.2).
//! 2. **Merging is the daemon's job, not the LLM's.** `merge_task` below is the only path
//!    that integrates work, and it is plain `git merge --no-ff`; a conflict is aborted and
//!    handed back to the worker who wrote the code (§6.1 #4).
//!
//! `push` appears exactly once, in [`deliver_pr`], which the scheduler only calls for
//! `deliver = "pr"` in the `finishing` phase (§12 #1: `branch` is the default precisely so
//! that nothing reaches origin unasked).
//!
//! Everything goes through [`sh`], the same local-process / `ssh_exec_path` split
//! `github.rs` uses, so the remote-host stage is a matter of passing another host name.

use crate::config::LOCAL_HOST;
use crate::hosts::sh_quote;
use crate::state::App;
use anyhow::{anyhow, Result};
use std::sync::Arc;
use std::time::Duration;

pub const GIT_TIMEOUT: Duration = Duration::from_secs(60);
/// `worktree add` writes out a whole checkout; a large repo needs more than a minute.
pub const WORKTREE_TIMEOUT: Duration = Duration::from_secs(300);
pub const PUSH_TIMEOUT: Duration = Duration::from_secs(180);

/// Marker `sh` appends so a remote shell can report the exit status through stdout.
const RC: &str = "__am_rc=";

#[derive(Debug, Clone)]
pub struct Out {
    pub code: i32,
    pub stdout: String,
    pub stderr: String,
}

impl Out {
    pub fn ok(&self) -> bool {
        self.code == 0
    }
    pub fn trimmed(&self) -> String {
        self.stdout.trim().to_string()
    }
    /// stderr when there is any, else stdout — what a human wants to read about a failure.
    pub fn message(&self) -> String {
        let e = self.stderr.trim();
        if e.is_empty() {
            self.stdout.trim().to_string()
        } else {
            e.to_string()
        }
    }
}

/// PATH prefix so git / gh from Homebrew are found even under launchd, colour forcing off
/// (the same fix `github.rs` needs for `gh --json`).
const PATH_FIX: &str = "export PATH=\"/opt/homebrew/bin:/usr/local/bin:$HOME/.local/bin:$PATH\"\n\
export NO_COLOR=1\nunset CLICOLOR_FORCE FORCE_COLOR CLICOLOR 2>/dev/null\n";

/// Run a POSIX `sh` script on `host` and return its exit code as data rather than an error:
/// a merge conflict is a normal outcome here, not a failure to report upwards.
pub async fn sh(app: &Arc<App>, host: &str, script: &str, timeout: Duration) -> Result<Out> {
    let full = format!("{PATH_FIX}{script}");
    if host == LOCAL_HOST {
        let o = tokio::time::timeout(
            timeout,
            tokio::process::Command::new("/bin/sh")
                .arg("-c")
                .arg(&full)
                .stdin(std::process::Stdio::null())
                .output(),
        )
        .await
        .map_err(|_| anyhow!("git command timed out after {}s", timeout.as_secs()))??;
        return Ok(Out {
            code: o.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&o.stdout).to_string(),
            stderr: String::from_utf8_lossy(&o.stderr).to_string(),
        });
    }
    // Remote: ssh_exec_path already fails the whole call on a non-zero status, so the status
    // is carried back in stdout instead.
    let conn = app.hosts.get(host).await.ok_or_else(|| anyhow!("unknown host `{host}`"))?;
    let wrapped = format!("{{ {full}\n}} 2>&1; printf '\\n{RC}%s\\n' \"$?\"");
    let raw = conn.ssh_exec_path(&wrapped).await?;
    let (body, code) = match raw.rsplit_once(RC) {
        Some((b, c)) => (b.to_string(), c.trim().parse::<i32>().unwrap_or(-1)),
        None => (raw, -1),
    };
    Ok(Out { code, stdout: body, stderr: String::new() })
}

/// One `git -C <dir> …` invocation. `args` are shell-quoted here, never by the caller.
pub async fn git(app: &Arc<App>, host: &str, dir: &str, args: &[&str], timeout: Duration) -> Result<Out> {
    let mut script = format!("git -C {}", sh_quote(dir));
    for a in args {
        script.push(' ');
        script.push_str(&sh_quote(a));
    }
    sh(app, host, &script, timeout).await
}

/// `git …` and its stdout, or an error carrying git's own message.
pub async fn git_ok(app: &Arc<App>, host: &str, dir: &str, args: &[&str], timeout: Duration) -> Result<String> {
    let o = git(app, host, dir, args, timeout).await?;
    if !o.ok() {
        return Err(anyhow!("git {}: {}", args.join(" "), o.message()));
    }
    Ok(o.trimmed())
}

// ---------------------------------------------------------------- repo-level (read-only on the user's checkout)

/// Appendix C step 1. A team can only be built on a git working tree (§1.2 non-goal).
pub async fn is_inside_work_tree(app: &Arc<App>, host: &str, path: &str) -> bool {
    matches!(
        git(app, host, path, &["rev-parse", "--is-inside-work-tree"], GIT_TIMEOUT).await,
        Ok(o) if o.ok() && o.trimmed() == "true"
    )
}

/// Appendix C step 2: resolve `base` to the commit the whole team starts from.
pub async fn resolve_commit(app: &Arc<App>, host: &str, repo: &str, base: &str) -> Result<String> {
    let spec = format!("{base}^{{commit}}");
    git_ok(app, host, repo, &["rev-parse", "--verify", "--quiet", &spec], GIT_TIMEOUT)
        .await
        .map_err(|_| anyhow!("cannot resolve `{base}` to a commit in this repository"))
}

pub async fn create_branch(app: &Arc<App>, host: &str, repo: &str, branch: &str, sha: &str) -> Result<()> {
    git_ok(app, host, repo, &["branch", branch, sha], GIT_TIMEOUT).await.map(|_| ())
}

/// `git worktree add`. `detach = true` for workers / the reviewer (§6.2: only `main/` may
/// hold the integration branch, because git refuses the same branch in two worktrees).
pub async fn worktree_add(
    app: &Arc<App>,
    host: &str,
    repo: &str,
    path: &str,
    rev: &str,
    detach: bool,
) -> Result<()> {
    let mut args: Vec<&str> = vec!["worktree", "add"];
    if detach {
        args.push("--detach");
    }
    args.push(path);
    args.push(rev);
    git_ok(app, host, repo, &args, WORKTREE_TIMEOUT).await.map(|_| ())
}

/// §6.5, and the order matters: `remove` (which drops `.git/worktrees/<name>/`) then
/// `prune` then the directory. Deleting the directory first leaves an orphan registration
/// that makes `git worktree list` report `prunable` for ever.
pub async fn worktree_remove(app: &Arc<App>, host: &str, repo: &str, path: &str) -> Out {
    match git(app, host, repo, &["worktree", "remove", "--force", path], GIT_TIMEOUT).await {
        Ok(o) if o.ok() => o,
        // A locked worktree needs `--force` twice (documented git behaviour).
        _ => git(app, host, repo, &["worktree", "remove", "--force", "--force", path], GIT_TIMEOUT)
            .await
            .unwrap_or(Out { code: -1, stdout: String::new(), stderr: "worktree remove failed".into() }),
    }
}

pub async fn worktree_prune(app: &Arc<App>, host: &str, repo: &str) {
    let _ = git(app, host, repo, &["worktree", "prune"], GIT_TIMEOUT).await;
}

/// `git worktree list --porcelain`, parsed down to the checkout paths.
pub async fn worktree_paths(app: &Arc<App>, host: &str, repo: &str) -> Vec<String> {
    let Ok(o) = git(app, host, repo, &["worktree", "list", "--porcelain"], GIT_TIMEOUT).await else {
        return Vec::new();
    };
    o.stdout
        .lines()
        .filter_map(|l| l.strip_prefix("worktree ").map(|p| p.trim().to_string()))
        .collect()
}

// ---------------------------------------------------------------- inside a worktree

/// Appendix C, dispatch: the task branch is cut from the integration branch **at dispatch
/// time**, so a later task automatically contains everything merged before it (§6.2).
pub async fn checkout_task_branch(
    app: &Arc<App>,
    host: &str,
    wt: &str,
    branch: &str,
    from: &str,
) -> Result<()> {
    git_ok(app, host, wt, &["checkout", "-b", branch, from], GIT_TIMEOUT).await.map(|_| ())
}

/// Appendix C, review: the reviewer's worktree is parked on the branch under review,
/// detached so it never owns it.
pub async fn checkout_detach(app: &Arc<App>, host: &str, wt: &str, rev: &str) -> Result<()> {
    git_ok(app, host, wt, &["checkout", "--detach", rev], GIT_TIMEOUT).await.map(|_| ())
}

pub async fn status_porcelain(app: &Arc<App>, host: &str, wt: &str) -> Result<String> {
    git_ok(app, host, wt, &["status", "--porcelain"], GIT_TIMEOUT).await
}

/// Appendix C, report: a worker that reported with a dirty tree gets it committed for it,
/// so nothing it wrote is lost between `report` and the merge.
pub async fn commit_all(app: &Arc<App>, host: &str, wt: &str, msg: &str) -> Result<bool> {
    if status_porcelain(app, host, wt).await?.trim().is_empty() {
        return Ok(false);
    }
    git_ok(app, host, wt, &["add", "-A"], GIT_TIMEOUT).await?;
    let o = git(app, host, wt, &["commit", "--no-verify", "-m", msg], GIT_TIMEOUT).await?;
    if !o.ok() {
        return Err(anyhow!("git commit: {}", o.message()));
    }
    Ok(true)
}

/// `git rev-list --count <from>..HEAD` — zero means the worker reported without committing
/// anything, which appendix C turns into a repair prompt rather than an empty merge.
pub async fn commits_ahead(app: &Arc<App>, host: &str, wt: &str, from: &str) -> Result<i64> {
    let range = format!("{from}..HEAD");
    let s = git_ok(app, host, wt, &["rev-list", "--count", &range], GIT_TIMEOUT).await?;
    Ok(s.trim().parse::<i64>().unwrap_or(0))
}

pub async fn head_sha(app: &Arc<App>, host: &str, wt: &str) -> Result<String> {
    git_ok(app, host, wt, &["rev-parse", "HEAD"], GIT_TIMEOUT).await
}

#[derive(Debug, Clone, PartialEq)]
pub enum MergeOutcome {
    /// Fast, deterministic, daemon-owned: the integration branch now contains the task.
    Merged { sha: String },
    /// `merge --abort` has already run; the integration worktree is back where it was.
    Conflict { files: Vec<String>, message: String },
}

/// SPEC-team §6.1 #3 / appendix C: **the daemon merges, never the LLM**.
///
/// The integration worktree must be clean first — a dirty `main/` means somebody edited the
/// integration branch by hand and the caller pauses with `integration_dirty` rather than
/// sweeping it into a merge commit.
pub async fn merge_task(app: &Arc<App>, host: &str, main_wt: &str, branch: &str) -> Result<MergeOutcome> {
    let o = git(app, host, main_wt, &["merge", "--no-ff", "--no-edit", branch], GIT_TIMEOUT).await?;
    if o.ok() {
        return Ok(MergeOutcome::Merged { sha: head_sha(app, host, main_wt).await.unwrap_or_default() });
    }
    let files = git(app, host, main_wt, &["diff", "--name-only", "--diff-filter=U"], GIT_TIMEOUT)
        .await
        .map(|x| x.stdout.lines().map(|l| l.trim().to_string()).filter(|l| !l.is_empty()).collect())
        .unwrap_or_default();
    // Always leave the integration worktree usable, whatever went wrong.
    let _ = git(app, host, main_wt, &["merge", "--abort"], GIT_TIMEOUT).await;
    Ok(MergeOutcome::Conflict { files, message: o.message() })
}

// ---------------------------------------------------------------- deliver (§6.4)

/// The **only** `git push` in the whole feature, and only for `deliver = "pr"`.
#[allow(clippy::too_many_arguments)]
pub async fn deliver_pr(
    app: &Arc<App>,
    host: &str,
    main_wt: &str,
    team_root: &str,
    branch: &str,
    repo_slug: &str,
    base_branch: &str,
    title: &str,
    body: &str,
) -> Result<String> {
    let push = git(app, host, main_wt, &["push", "-u", "origin", branch], PUSH_TIMEOUT).await?;
    if !push.ok() {
        return Err(anyhow!("git push: {}", push.message()));
    }
    // The body goes through a file, not the command line: a PM summary can be long and can
    // contain anything at all.
    //
    // The file lives at the **team root**, not `<main_wt>/.git/`: in a linked worktree
    // `.git` is a *file* pointing at the real git dir, so `cat > <wt>/.git/…` fails with
    // "not a directory" — and since the push has already happened by then, every `pr`
    // delivery would push and then fail, and every resume would push again.
    let body_path = format!("{}/pr-body.txt", team_root.trim_end_matches('/'));
    let script = format!(
        "cat > {} <<'AM_TEAM_PR_BODY_EOF'\n{}\nAM_TEAM_PR_BODY_EOF\n\
         gh pr create --repo {} --head {} --base {} --title {} --body-file {}\n\
         rc=$?; rm -f {}; exit $rc",
        sh_quote(&body_path),
        body.replace("\r\n", "\n"),
        sh_quote(repo_slug),
        sh_quote(branch),
        sh_quote(base_branch),
        sh_quote(title),
        sh_quote(&body_path),
        sh_quote(&body_path),
    );
    let o = sh(app, host, &script, PUSH_TIMEOUT).await?;
    if !o.ok() {
        return Err(anyhow!("gh pr create: {}", o.message()));
    }
    let url = o
        .stdout
        .lines()
        .map(str::trim)
        .find(|l| l.starts_with("https://"))
        .unwrap_or("")
        .to_string();
    Ok(url)
}

/// Which remote branch a PR should target. `base_ref` is whatever the user asked for
/// (`HEAD` by default), so it is mapped back to a branch name the remote knows.
pub async fn remote_base_branch(app: &Arc<App>, host: &str, repo: &str, base_ref: &str) -> String {
    let cleaned = base_ref.trim();
    if !cleaned.is_empty() && cleaned != "HEAD" && !cleaned.contains('~') && !cleaned.contains('^') {
        return cleaned.trim_start_matches("origin/").to_string();
    }
    // `HEAD` → the branch the user's checkout is actually on, else the remote's default.
    if let Ok(b) = git_ok(app, host, repo, &["symbolic-ref", "--short", "-q", "HEAD"], GIT_TIMEOUT).await {
        if !b.trim().is_empty() {
            return b.trim().to_string();
        }
    }
    if let Ok(b) = git_ok(app, host, repo, &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"], GIT_TIMEOUT).await
    {
        if let Some(s) = b.trim().strip_prefix("origin/") {
            return s.to_string();
        }
    }
    "main".into()
}

// ---------------------------------------------------------------- files (§6.2)

/// Write one file on the project's host, creating its parent directory.
pub async fn put_file(app: &Arc<App>, host: &str, path: &str, content: &str) -> Result<()> {
    if host == LOCAL_HOST {
        let p = std::path::Path::new(path);
        if let Some(dir) = p.parent() {
            tokio::fs::create_dir_all(dir).await?;
        }
        tokio::fs::write(p, content.as_bytes()).await?;
        return Ok(());
    }
    let conn = app.hosts.get(host).await.ok_or_else(|| anyhow!("unknown host `{host}`"))?;
    conn.ssh_put(path, content.as_bytes()).await
}

/// §6.2: the copy of `ISSUE.md` / `TEAM.md` that lives *inside* a member's cwd, next to a
/// `.gitignore` of `*` so the worktree's `git status` stays clean and `git add -A` on a task
/// branch cannot commit it. Nothing to do with `attach.rs`, whose `.gitignore` only covers
/// `<project.path>/.agents-manager/attachments/`.
pub async fn write_team_docs(app: &Arc<App>, host: &str, wt: &str, issue_md: &str, team_md: &str) -> Result<()> {
    let dir = format!("{}/.agents-manager/team", wt.trim_end_matches('/'));
    put_file(app, host, &format!("{dir}/.gitignore"), "*\n").await?;
    put_file(app, host, &format!("{dir}/ISSUE.md"), issue_md).await?;
    put_file(app, host, &format!("{dir}/TEAM.md"), team_md).await?;
    Ok(())
}

/// Remove the team root once every worktree registration is gone (§6.5, last step).
pub async fn remove_dir(app: &Arc<App>, host: &str, path: &str) {
    if host == LOCAL_HOST {
        let _ = tokio::fs::remove_dir_all(path).await;
        return;
    }
    let _ = sh(app, host, &format!("rm -rf {}", sh_quote(path)), GIT_TIMEOUT).await;
}

// ---------------------------------------------------------------- tests

#[cfg(test)]
pub mod testing {
    //! A throw-away git repository, so the git tests never touch the repo they run in.
    use std::path::PathBuf;
    use std::process::Command;

    pub fn run(dir: &std::path::Path, args: &[&str]) -> String {
        let o = Command::new("git")
            .args(args)
            .current_dir(dir)
            .env("GIT_AUTHOR_NAME", "am-test")
            .env("GIT_AUTHOR_EMAIL", "am-test@example.invalid")
            .env("GIT_COMMITTER_NAME", "am-test")
            .env("GIT_COMMITTER_EMAIL", "am-test@example.invalid")
            .output()
            .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
        assert!(o.status.success(), "git {args:?} failed: {}", String::from_utf8_lossy(&o.stderr));
        String::from_utf8_lossy(&o.stdout).trim().to_string()
    }

    /// A repo with one commit on `main` and a `README.md`.
    pub fn init_repo(dir: &PathBuf) {
        std::fs::create_dir_all(dir).unwrap();
        run(dir, &["init", "--initial-branch=main", "-q"]);
        run(dir, &["config", "user.name", "am-test"]);
        run(dir, &["config", "user.email", "am-test@example.invalid"]);
        run(dir, &["config", "commit.gpgsign", "false"]);
        std::fs::write(dir.join("README.md"), "base\n").unwrap();
        run(dir, &["add", "-A"]);
        run(dir, &["commit", "-q", "-m", "base"]);
    }
}

#[cfg(test)]
mod tests {
    use super::testing::*;
    use super::*;

    async fn app_for(dir: &std::path::Path) -> Arc<App> {
        let pool = crate::db::open(&dir.join("db.sqlite3")).await.unwrap();
        let cfg = crate::config::ConfigStore::load(dir.join("config.toml")).await.unwrap();
        let h = crate::herdr::HerdrClient::new(dir.join("herdr.sock"));
        App::new(
            pool,
            h.clone(),
            h,
            cfg,
            dir.to_path_buf(),
            dir.join("agents-managerd"),
            7799,
            "t".into(),
            "test".into(),
        )
    }

    /// The whole appendix C happy path in a temporary repo: worktrees, a task branch, a
    /// commit, a merge — then the T8 assertion that the user's checkout never moved.
    #[tokio::test]
    async fn worktrees_branch_and_merge_leave_the_main_checkout_untouched() {
        let tmp = std::env::temp_dir().join(format!("am-git-{}", crate::db::ulid()));
        let repo = tmp.join("repo");
        let root = tmp.join("data/teams/t1");
        init_repo(&repo);
        let app = app_for(&tmp).await;
        let (h, r) = (LOCAL_HOST, repo.to_string_lossy().to_string());

        assert!(is_inside_work_tree(&app, h, &r).await);
        assert!(!is_inside_work_tree(&app, h, &tmp.to_string_lossy()).await);
        let base = resolve_commit(&app, h, &r, "HEAD").await.unwrap();
        assert_eq!(base.len(), 40);
        assert!(resolve_commit(&app, h, &r, "no-such-ref").await.is_err());

        let head_before = run(&repo, &["rev-parse", "HEAD"]);
        let before: Vec<_> = std::fs::read_dir(&repo).unwrap().map(|e| e.unwrap().file_name()).collect();

        create_branch(&app, h, &r, "team/i1-aaa", &base).await.unwrap();
        let main_wt = root.join("main").to_string_lossy().to_string();
        let dev_wt = root.join("dev-1").to_string_lossy().to_string();
        worktree_add(&app, h, &r, &main_wt, "team/i1-aaa", false).await.unwrap();
        worktree_add(&app, h, &r, &dev_wt, &base, true).await.unwrap();
        write_team_docs(&app, h, &dev_wt, "# issue", "# team").await.unwrap();

        // The ignored doc copy does not dirty the worktree (§6.2).
        assert_eq!(status_porcelain(&app, h, &dev_wt).await.unwrap().trim(), "");

        checkout_task_branch(&app, h, &dev_wt, "team/i1-aaa-t1-dev-1", "team/i1-aaa").await.unwrap();
        std::fs::write(root.join("dev-1/a.txt"), "from dev-1\n").unwrap();
        assert!(!status_porcelain(&app, h, &dev_wt).await.unwrap().is_empty());
        assert!(commit_all(&app, h, &dev_wt, "wip(dev-1): uncommitted at report").await.unwrap());
        assert!(!commit_all(&app, h, &dev_wt, "again").await.unwrap(), "a clean tree commits nothing");
        assert_eq!(commits_ahead(&app, h, &dev_wt, "team/i1-aaa").await.unwrap(), 1);

        // Review parks the reviewer on the task branch, detached.
        let rev_wt = root.join("reviewer").to_string_lossy().to_string();
        worktree_add(&app, h, &r, &rev_wt, &base, true).await.unwrap();
        checkout_detach(&app, h, &rev_wt, "team/i1-aaa-t1-dev-1").await.unwrap();
        assert!(root.join("reviewer/a.txt").exists());

        // The daemon merges.
        match merge_task(&app, h, &main_wt, "team/i1-aaa-t1-dev-1").await.unwrap() {
            MergeOutcome::Merged { sha } => assert_eq!(sha.len(), 40),
            other => panic!("expected a clean merge, got {other:?}"),
        }
        assert!(root.join("main/a.txt").exists());

        // T8: the user's checkout did not move and gained no files.
        assert_eq!(run(&repo, &["rev-parse", "HEAD"]), head_before);
        assert_eq!(run(&repo, &["status", "--porcelain"]), "");
        let after: Vec<_> = std::fs::read_dir(&repo).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(before, after, "nothing new appeared in <project.path>");
        // …and git knows about all three team worktrees.
        let listed = worktree_paths(&app, h, &r).await;
        assert_eq!(listed.len(), 4, "main checkout + 3 team worktrees: {listed:?}");

        // T9 cleanup order: remove → prune → rmdir leaves no orphan registration.
        for wt in [&main_wt, &dev_wt, &rev_wt] {
            assert!(worktree_remove(&app, h, &r, wt).await.ok(), "remove {wt}");
        }
        worktree_prune(&app, h, &r).await;
        remove_dir(&app, h, &root.to_string_lossy()).await;
        assert_eq!(worktree_paths(&app, h, &r).await.len(), 1, "only the main checkout is left");
        assert!(!run(&repo, &["worktree", "list"]).contains("prunable"));
        // Branches survive on purpose (§6.5).
        assert!(run(&repo, &["branch", "--list"]).contains("team/i1-aaa"));

        let _ = std::fs::remove_dir_all(&tmp);
    }

    /// T5's first half: a real conflict is detected, aborted, and reported with the file
    /// list — the integration worktree is left clean enough to merge again afterwards.
    #[tokio::test]
    async fn a_conflicting_merge_is_aborted_and_named() {
        let tmp = std::env::temp_dir().join(format!("am-git-conflict-{}", crate::db::ulid()));
        let repo = tmp.join("repo");
        let root = tmp.join("data/teams/t1");
        init_repo(&repo);
        let app = app_for(&tmp).await;
        let (h, r) = (LOCAL_HOST, repo.to_string_lossy().to_string());
        let base = resolve_commit(&app, h, &r, "HEAD").await.unwrap();
        create_branch(&app, h, &r, "team/i1-aaa", &base).await.unwrap();
        let main_wt = root.join("main").to_string_lossy().to_string();
        let a_wt = root.join("dev-1").to_string_lossy().to_string();
        let b_wt = root.join("dev-2").to_string_lossy().to_string();
        worktree_add(&app, h, &r, &main_wt, "team/i1-aaa", false).await.unwrap();
        worktree_add(&app, h, &r, &a_wt, &base, true).await.unwrap();
        worktree_add(&app, h, &r, &b_wt, &base, true).await.unwrap();

        // Two workers touch the same line of the same file.
        checkout_task_branch(&app, h, &a_wt, "team/i1-aaa-t1-dev-1", "team/i1-aaa").await.unwrap();
        std::fs::write(root.join("dev-1/README.md"), "dev-1 was here\n").unwrap();
        commit_all(&app, h, &a_wt, "t1").await.unwrap();
        checkout_task_branch(&app, h, &b_wt, "team/i1-aaa-t2-dev-2", "team/i1-aaa").await.unwrap();
        std::fs::write(root.join("dev-2/README.md"), "dev-2 was here\n").unwrap();
        commit_all(&app, h, &b_wt, "t2").await.unwrap();

        assert!(matches!(
            merge_task(&app, h, &main_wt, "team/i1-aaa-t1-dev-1").await.unwrap(),
            MergeOutcome::Merged { .. }
        ));
        let conflict = merge_task(&app, h, &main_wt, "team/i1-aaa-t2-dev-2").await.unwrap();
        match &conflict {
            MergeOutcome::Conflict { files, .. } => assert_eq!(files, &vec!["README.md".to_string()]),
            other => panic!("expected a conflict, got {other:?}"),
        }
        // `merge --abort` already ran: the integration worktree is clean and mergeable again.
        assert_eq!(status_porcelain(&app, h, &main_wt).await.unwrap().trim(), "");

        // The worker rebases in its own worktree and the second merge succeeds — T5's shape.
        run(&root.join("dev-2"), &["rebase", "-X", "theirs", "team/i1-aaa"]);
        assert!(matches!(
            merge_task(&app, h, &main_wt, "team/i1-aaa-t2-dev-2").await.unwrap(),
            MergeOutcome::Merged { .. }
        ));
        assert_eq!(run(&repo, &["status", "--porcelain"]), "", "the user's checkout stayed clean");
        let _ = std::fs::remove_dir_all(&tmp);
    }
}
