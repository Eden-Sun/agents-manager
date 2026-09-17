//! 任務的交付（D2、D8）：推 main 或開 PR。
//!
//! 推 main 只做 **fast-forward**：先 `fetch`，`origin/main` 必須是 HEAD 的祖先才推，否則停下來問人
//! （D8：非 fast-forward、rebase 衝突、驗證沒過都算失敗，**不**自動改開 PR、不 force）。
//! 「整樹驗證通過」這一關由呼叫端把關（任務要先有 `verified` 事件），這裡只管 git 的事實。
//!
//! MVP 只支援本機專案，所以直接跑本機 `git`，不經主機路由——也因此可以用暫存 repo 單元測試。

use std::path::Path;
use tokio::process::Command;

#[derive(Debug, Clone, PartialEq)]
pub struct Failure {
    /// 機器碼：`dirty_worktree` | `fetch_failed` | `not_fast_forward` | `nothing_to_deliver` | `push_failed` | `pr_failed`
    pub code: &'static str,
    pub detail: String,
}

fn fail(code: &'static str, detail: impl Into<String>) -> Failure {
    Failure { code, detail: detail.into() }
}

async fn git(dir: &Path, args: &[&str]) -> Result<(bool, String), Failure> {
    let out = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .output()
        .await
        .map_err(|e| fail("push_failed", format!("git 無法執行：{e}")))?;
    let mut text = String::from_utf8_lossy(&out.stdout).trim().to_string();
    let err = String::from_utf8_lossy(&out.stderr).trim().to_string();
    if !err.is_empty() {
        if !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&err);
    }
    Ok((out.status.success(), text))
}

/// 這個目錄現在的 HEAD（完整 sha）。不是 git 工作樹就回 `None`。
pub async fn head_sha(dir: &Path) -> Option<String> {
    match git(dir, &["rev-parse", "--verify", "HEAD"]).await {
        Ok((true, out)) => out.lines().next().map(str::trim).filter(|s| is_full_sha(s)).map(str::to_string),
        _ => None,
    }
}

/// 在 `dir` 這個 repo 裡把一個（可能是縮寫的）sha 解成完整的 commit sha。找不到或不是 commit 回 `None`。
pub async fn resolve_commit(dir: &Path, sha: &str) -> Option<String> {
    if !(7..=40).contains(&sha.len()) || !sha.chars().all(|c| c.is_ascii_hexdigit()) {
        return None;
    }
    let spec = format!("{}^{{commit}}", sha.to_ascii_lowercase());
    match git(dir, &["rev-parse", "--verify", "--quiet", &spec]).await {
        Ok((true, out)) => out.lines().next().map(str::trim).filter(|s| is_full_sha(s)).map(str::to_string),
        _ => None,
    }
}

fn is_full_sha(s: &str) -> bool {
    s.len() == 40 && s.chars().all(|c| c.is_ascii_hexdigit())
}

/// 兩個目錄是不是同一個 repo（主工作樹與它的 `git worktree` 共用一份 common dir）。
///
/// 交付只收這個任務所屬專案的工作樹：隨便指一個存在的絕對路徑就放行的話，別的 repo 的 HEAD
/// 也能被推上這個專案的 origin（review3 c1 M9）。任一邊讀不到就當不是。
pub async fn same_repo(a: &Path, b: &Path) -> bool {
    async fn common(dir: &Path) -> Option<std::path::PathBuf> {
        let (ok, out) = git(dir, &["rev-parse", "--path-format=absolute", "--git-common-dir"]).await.ok()?;
        let line = out.lines().next().map(str::trim).filter(|l| ok && !l.is_empty())?;
        std::fs::canonicalize(line).ok()
    }
    match (common(a).await, common(b).await) {
        (Some(x), Some(y)) => x == y,
        _ => false,
    }
}

/// 推上去的結果。`already_in_base` = HEAD 早就在 `<remote>/<base>` 裡——先前那次交付其實成功了，
/// 只是回應在路上斷掉、`delivered` 事件沒寫成（review3 c1 M11）。
#[derive(Debug, Clone, PartialEq)]
pub struct Pushed {
    pub sha: String,
    pub already_in_base: bool,
}

/// PR 的結果。`existing` = 這條分支本來就有 PR（重試時 `gh pr create` 會失敗，但事情早就做完了）。
#[derive(Debug, Clone, PartialEq)]
pub struct Opened {
    pub url: String,
    pub sha: String,
    pub existing: bool,
}

/// `a` 是不是 `b` 的祖先（`a` 已經在 `b` 裡）。
async fn is_ancestor(dir: &Path, a: &str, b: &str) -> bool {
    matches!(git(dir, &["merge-base", "--is-ancestor", a, b]).await, Ok((true, _)))
}

/// 共同的前置檢查：工作樹乾淨、fetch 得到。回傳 (HEAD sha, `<remote>/<base>` sha)。
///
/// 「有沒有東西要交」不在這裡判：重試時 HEAD 已經在 base 裡是**成功**的證據，不是錯誤，
/// 由呼叫端配合「先前有沒有試過」決定（review3 c1 M11）。
async fn preflight(dir: &Path, remote: &str, base: &str) -> Result<(String, String), Failure> {
    let (_, porcelain) = git(dir, &["status", "--porcelain"]).await?;
    if !porcelain.trim().is_empty() {
        return Err(fail("dirty_worktree", format!("worktree 還有未提交的改動：\n{porcelain}")));
    }
    let (ok, out) = git(dir, &["fetch", remote, base]).await?;
    if !ok {
        return Err(fail("fetch_failed", out));
    }
    let remote_ref = format!("{remote}/{base}");
    let (_, head) = git(dir, &["rev-parse", "HEAD"]).await?;
    let (ok, upstream) = git(dir, &["rev-parse", &remote_ref]).await?;
    if !ok {
        return Err(fail("fetch_failed", upstream));
    }
    Ok((head, upstream))
}

/// fast-forward 推到 `<remote>/<base>`。
///
/// `attempted_before` = 這個 commit 先前已經試過一次交付（呼叫端記的 `delivery_attempt`）。HEAD 已經在
/// base 裡時它決定這是「上一次其實推成功了」還是「根本沒東西可交」：以前一律回 `nothing_to_deliver`，
/// 逾時重試就把一筆已經在 main 上的交付報成失敗、任務停在「等你決定」（review3 c1 M11）。
pub async fn push_main(dir: &Path, remote: &str, base: &str, attempted_before: bool) -> Result<Pushed, Failure> {
    let (head, upstream) = preflight(dir, remote, base).await?;
    if is_ancestor(dir, &head, &upstream).await {
        return if attempted_before {
            Ok(Pushed { sha: head, already_in_base: true })
        } else {
            Err(fail("nothing_to_deliver", format!("HEAD（{head}）已經在 {remote}/{base} 裡，沒有新的 commit 要交")))
        };
    }
    if !is_ancestor(dir, &upstream, &head).await {
        return Err(fail(
            "not_fast_forward",
            format!("{remote}/{base}（{upstream}）已經往前走，HEAD（{head}）不是它的後代；要先 rebase 並重新驗證"),
        ));
    }
    let refspec = format!("HEAD:refs/heads/{base}");
    let (ok, out) = git(dir, &["push", remote, &refspec]).await?;
    if !ok {
        return Err(fail("push_failed", out));
    }
    Ok(Pushed { sha: head, already_in_base: false })
}

/// 推一條 `mission/<id>` 分支並用 `gh` 開 PR。成功回 PR 網址。
pub async fn open_pr(dir: &Path, remote: &str, base: &str, branch: &str, title: &str, body: &str) -> Result<Opened, Failure> {
    open_pr_with(Path::new("gh"), dir, remote, base, branch, title, body).await
}

/// 這條分支現在有沒有 PR。`None` = 沒有（`gh pr view` 找不到就是非 0）。
async fn pr_url(gh: &Path, dir: &Path, branch: &str) -> Option<String> {
    let out = Command::new(gh)
        .current_dir(dir)
        .args(["pr", "view", branch, "--json", "url", "--jq", ".url"])
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    String::from_utf8_lossy(&out.stdout).trim().lines().last().map(str::to_string).filter(|u| u.starts_with("http"))
}

/// `gh` 的路徑可換，測試才餵得進假的 `gh`（開 PR 不能真的打 GitHub）。
pub(crate) async fn open_pr_with(
    gh: &Path,
    dir: &Path,
    remote: &str,
    base: &str,
    branch: &str,
    title: &str,
    body: &str,
) -> Result<Opened, Failure> {
    let (head, upstream) = preflight(dir, remote, base).await?;
    let in_base = is_ancestor(dir, &head, &upstream).await;
    if !in_base {
        let refspec = format!("HEAD:refs/heads/{branch}");
        let (ok, out) = git(dir, &["push", remote, &refspec]).await?;
        if !ok {
            return Err(fail("push_failed", out));
        }
    }
    // 先看有沒有 PR：重試時 `gh pr create` 會因為「已經有一個」失敗，但事情早就做完了（review3 c1 M11）。
    if let Some(url) = pr_url(gh, dir, branch).await {
        return Ok(Opened { url, sha: head, existing: true });
    }
    if in_base {
        return Err(fail("nothing_to_deliver", format!("HEAD（{head}）已經在 {remote}/{base} 裡，也沒有開著的 PR")));
    }
    let out = Command::new(gh)
        .current_dir(dir)
        .args(["pr", "create", "--base", base, "--head", branch, "--title", title, "--body", body])
        .output()
        .await
        .map_err(|e| fail("pr_failed", format!("gh 無法執行：{e}")))?;
    if !out.status.success() {
        return Err(fail("pr_failed", String::from_utf8_lossy(&out.stderr).trim().to_string()));
    }
    let url = String::from_utf8_lossy(&out.stdout).trim().lines().last().unwrap_or_default().to_string();
    Ok(Opened { url, sha: head, existing: false })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command as StdCommand;

    fn sh(dir: &Path, args: &[&str]) {
        let st = StdCommand::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@t")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@t")
            .status()
            .unwrap();
        assert!(st.success(), "git {args:?}");
    }

    fn commit(dir: &Path, file: &str) {
        std::fs::write(dir.join(file), file).unwrap();
        sh(dir, &["add", file]);
        sh(dir, &["commit", "-q", "-m", file]);
    }

    /// 測試結束就刪掉的暫存目錄（不另加依賴）。
    struct Tmp(std::path::PathBuf);
    impl Tmp {
        fn path(&self) -> &Path {
            &self.0
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// bare origin（main 上一個 commit）＋ 兩個 clone。
    fn fixture() -> (Tmp, std::path::PathBuf, std::path::PathBuf) {
        let root = Tmp(std::env::temp_dir().join(format!("am-mission-deliver-{}", crate::db::ulid())));
        std::fs::create_dir_all(root.path()).unwrap();
        let origin = root.path().join("origin.git");
        let seed = root.path().join("seed");
        StdCommand::new("git").args(["init", "-q", "--bare", "-b", "main"]).arg(&origin).status().unwrap();
        StdCommand::new("git").args(["clone", "-q"]).arg(&origin).arg(&seed).status().unwrap();
        sh(&seed, &["checkout", "-q", "-b", "main"]);
        commit(&seed, "a");
        sh(&seed, &["push", "-q", "origin", "main"]);
        let work = root.path().join("work");
        StdCommand::new("git").args(["clone", "-q", "-b", "main"]).arg(&origin).arg(&work).status().unwrap();
        (root, seed, work)
    }

    #[tokio::test]
    async fn a_fast_forward_pushes_and_returns_the_sha() {
        let (_root, _seed, work) = fixture();
        commit(&work, "b");
        let out = push_main(&work, "origin", "main", false).await.expect("ff push");
        assert!(!out.already_in_base);
        let (_, remote) = git(&work, &["ls-remote", "origin", "refs/heads/main"]).await.unwrap();
        assert!(remote.starts_with(&out.sha));
    }

    /// 逾時重試：push 成功但沒記下來，再交付一次不能報成失敗（review3 c1 M11）。
    #[tokio::test]
    async fn a_second_delivery_of_a_commit_that_is_already_in_main_succeeds_when_it_was_tried_before() {
        let (_root, seed, work) = fixture();
        commit(&work, "b");
        let first = push_main(&work, "origin", "main", false).await.expect("ff push");
        // 同一個 commit 再交付一次（第一次已經留下 delivery_attempt）。
        let again = push_main(&work, "origin", "main", true).await.expect("重試要回成功");
        assert_eq!((again.sha.as_str(), again.already_in_base), (first.sha.as_str(), true));
        // 別人又推了一顆上去（先接上我們那一顆），我們的 commit 仍在 main 裡：一樣算已經交付。
        sh(&seed, &["pull", "-q", "--ff-only", "origin", "main"]);
        commit(&seed, "theirs");
        sh(&seed, &["push", "-q", "origin", "main"]);
        assert!(push_main(&work, "origin", "main", true).await.expect("仍在 main 裡").already_in_base);
        // 沒試過的那次維持 `nothing_to_deliver`：那是「執行者根本沒 commit」。
        assert_eq!(push_main(&work, "origin", "main", false).await.unwrap_err().code, "nothing_to_deliver");
    }

    /// 假的 `gh`：第一次開 PR，第二次 `pr create` 會說已經有了——要回原本那條 PR，不是 `pr_failed`。
    #[tokio::test]
    async fn a_retry_returns_the_pull_request_that_already_exists() {
        let (root, _seed, work) = fixture();
        commit(&work, "b");
        let gh = root.path().join("gh");
        std::fs::write(
            &gh,
            "#!/bin/sh\ndir=$(dirname \"$0\")\ncase \"$2\" in\n  view) [ -f \"$dir/pr.url\" ] && cat \"$dir/pr.url\" && exit 0; echo 'no pull requests found' >&2; exit 1;;\n  create) [ -f \"$dir/pr.url\" ] && { echo 'a pull request for branch already exists' >&2; exit 1; }; echo https://example.invalid/pull/7 > \"$dir/pr.url\"; cat \"$dir/pr.url\";;\nesac\n",
        )
        .unwrap();
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::set_permissions(&gh, std::fs::Permissions::from_mode(0o755)).unwrap();

        let open = |title: &'static str| {
            let (gh, work) = (gh.clone(), work.clone());
            async move { open_pr_with(&gh, &work, "origin", "main", "mission/x", title, "body").await }
        };
        let first = open("第一次").await.expect("開 PR");
        assert_eq!((first.url.as_str(), first.existing), ("https://example.invalid/pull/7", false));
        let again = open("重試").await.expect("重試要回原本那條 PR");
        assert_eq!((again.url.as_str(), again.existing), ("https://example.invalid/pull/7", true));
    }

    #[tokio::test]
    async fn a_diverged_main_is_not_pushed() {
        let (_root, seed, work) = fixture();
        commit(&seed, "someone-else");
        sh(&seed, &["push", "-q", "origin", "main"]);
        commit(&work, "mine");
        let err = push_main(&work, "origin", "main", false).await.unwrap_err();
        assert_eq!(err.code, "not_fast_forward");
        let (_, remote) = git(&work, &["ls-remote", "origin", "refs/heads/main"]).await.unwrap();
        let (_, mine) = git(&work, &["rev-parse", "HEAD"]).await.unwrap();
        assert!(!remote.starts_with(&mine), "不能被推上去");
    }

    /// 交付關卡用的 git 事實：HEAD、縮寫 sha 解成完整的、兩個目錄是不是同一個 repo（review3 c1 M9）。
    #[tokio::test]
    async fn the_gate_can_tell_which_commit_and_which_repo() {
        let (root, seed, work) = fixture();
        commit(&work, "b");
        let head = head_sha(&work).await.expect("HEAD");
        assert_eq!(head.len(), 40);
        assert_eq!(resolve_commit(&work, &head[..10]).await.as_deref(), Some(head.as_str()));
        assert_eq!(resolve_commit(&work, "not-a-sha").await, None);
        assert_eq!(resolve_commit(&work, "0000000000").await, None, "不存在的 commit");
        assert!(head_sha(root.path()).await.is_none(), "不是 git 工作樹");

        let wt = root.path().join("wt");
        sh(&work, &["worktree", "add", "-q", "--detach", wt.to_str().unwrap()]);
        assert!(same_repo(&work, &wt).await, "worktree 跟主工作樹是同一個 repo");
        assert!(!same_repo(&work, &seed).await, "同一個 origin 的另一份 clone 不算");
        assert!(!same_repo(&work, root.path()).await);
    }

    #[tokio::test]
    async fn a_dirty_worktree_or_nothing_new_is_refused() {
        let (_root, _seed, work) = fixture();
        assert_eq!(push_main(&work, "origin", "main", false).await.unwrap_err().code, "nothing_to_deliver");
        commit(&work, "b");
        std::fs::write(work.join("scratch"), "x").unwrap();
        assert_eq!(push_main(&work, "origin", "main", false).await.unwrap_err().code, "dirty_worktree");
    }
}
