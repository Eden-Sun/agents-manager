//! 任務的交付（D2、D8）：推 main 或開 PR。
//!
//! 推 main 只做 **fast-forward**：先 `fetch`，`origin/main` 必須是 HEAD 的祖先才推，否則停下來問人
//! （D8：非 fast-forward、rebase 衝突、驗證沒過都算失敗，**不**自動改開 PR、不 force）。
//! 「整樹驗證通過」這一關由呼叫端把關（任務要先有 `verified` 事件），這裡只管 git 的事實。
//!
//! MVP 只支援本機專案（team 的 worktree helper 同樣本機限定，`team.rs` 對遠端直接拒絕），
//! 所以直接跑本機 `git`，不經 `team_git` 的主機路由——也因此可以用暫存 repo 單元測試。

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

/// 共同的前置檢查：工作樹乾淨、fetch 得到、真的有東西要交。回傳 (HEAD sha, origin/<base> sha)。
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
    if head == upstream {
        return Err(fail("nothing_to_deliver", format!("HEAD 就是 {remote_ref}（{head}），沒有新的 commit")));
    }
    Ok((head, upstream))
}

/// fast-forward 推到 `<remote>/<base>`。成功回推上去的 sha。
pub async fn push_main(dir: &Path, remote: &str, base: &str) -> Result<String, Failure> {
    let (head, upstream) = preflight(dir, remote, base).await?;
    let (ancestor, _) = git(dir, &["merge-base", "--is-ancestor", &upstream, "HEAD"]).await?;
    if !ancestor {
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
    Ok(head)
}

/// 推一條 `mission/<id>` 分支並用 `gh` 開 PR。成功回 PR 網址。
pub async fn open_pr(dir: &Path, remote: &str, base: &str, branch: &str, title: &str, body: &str) -> Result<String, Failure> {
    preflight(dir, remote, base).await?;
    let refspec = format!("HEAD:refs/heads/{branch}");
    let (ok, out) = git(dir, &["push", remote, &refspec]).await?;
    if !ok {
        return Err(fail("push_failed", out));
    }
    let out = Command::new("gh")
        .current_dir(dir)
        .args(["pr", "create", "--base", base, "--head", branch, "--title", title, "--body", body])
        .output()
        .await
        .map_err(|e| fail("pr_failed", format!("gh 無法執行：{e}")))?;
    if !out.status.success() {
        return Err(fail("pr_failed", String::from_utf8_lossy(&out.stderr).trim().to_string()));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().lines().last().unwrap_or_default().to_string())
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

    /// 測試結束就刪掉的暫存目錄（同 team_git 測試的做法，不另加依賴）。
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
        let sha = push_main(&work, "origin", "main").await.expect("ff push");
        let (_, remote) = git(&work, &["ls-remote", "origin", "refs/heads/main"]).await.unwrap();
        assert!(remote.starts_with(&sha));
    }

    #[tokio::test]
    async fn a_diverged_main_is_not_pushed() {
        let (_root, seed, work) = fixture();
        commit(&seed, "someone-else");
        sh(&seed, &["push", "-q", "origin", "main"]);
        commit(&work, "mine");
        let err = push_main(&work, "origin", "main").await.unwrap_err();
        assert_eq!(err.code, "not_fast_forward");
        let (_, remote) = git(&work, &["ls-remote", "origin", "refs/heads/main"]).await.unwrap();
        let (_, mine) = git(&work, &["rev-parse", "HEAD"]).await.unwrap();
        assert!(!remote.starts_with(&mine), "不能被推上去");
    }

    #[tokio::test]
    async fn a_dirty_worktree_or_nothing_new_is_refused() {
        let (_root, _seed, work) = fixture();
        assert_eq!(push_main(&work, "origin", "main").await.unwrap_err().code, "nothing_to_deliver");
        commit(&work, "b");
        std::fs::write(work.join("scratch"), "x").unwrap();
        assert_eq!(push_main(&work, "origin", "main").await.unwrap_err().code, "dirty_worktree");
    }
}
